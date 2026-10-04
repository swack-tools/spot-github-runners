"""Root-owned aggregate memory admission for runner hooks and remote builds.

Installed standalone on hosts. Clients cannot supply their own budget or
release another container's lease: Unix peer credentials determine ownership.
"""
import fcntl
import json
import math
import os
import re
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path

BUILD_ROOT = Path('/mnt/runner-data/remote-build')
LOCK_ROOT = Path('/run')

def admission_budget(tier, observed_overhead_gib, workload_peak_gib, *, slots=None):
    slots = tier.initial_slots if slots is None else slots
    if not 1 <= slots <= tier.maximum_slots or not 0 <= observed_overhead_gib < tier.ram_gib:
        raise ValueError('Invalid host overhead or runner slots')
    if not math.isfinite(workload_peak_gib) or workload_peak_gib <= 0:
        raise ValueError('Invalid workload memory reservation')
    reserve = max(2.,observed_overhead_gib) + .25 * slots
    work = tier.ram_gib - reserve
    if work < workload_peak_gib:
        raise ValueError('Workload cannot fit after host/listener reserve')
    return dict(reserve_gib=reserve,work_gib=work,
                concurrency=min(slots,int(work//workload_peak_gib)))


class LeasePool:
    def __init__(self,path,limit_gib,*,alive):
        self.path = Path(path)
        if not math.isfinite(limit_gib) or limit_gib <= 0:
            raise ValueError('Invalid aggregate memory budget')
        self.limit = limit_gib
        self.alive = alive

    def _change(self,owner,amount):
        self.path.parent.mkdir(parents=True,exist_ok=True)
        # Lock a separate inode: the data file is atomically replaced.
        fd = os.open(str(self.path)+'.lock',os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW,0o600)
        with os.fdopen(fd,'r+') as lock:
            fcntl.flock(lock,fcntl.LOCK_EX)
            rows = json.loads(self.path.read_text()) if self.path.exists() else {}
            rows = {key:value for key,value in rows.items() if self.alive(key)}
            accepted = True
            if amount is None:
                rows.pop(owner,None)
            elif owner in rows:
                accepted = rows[owner] == amount
            elif sum(rows.values()) + amount <= self.limit:
                rows[owner] = amount
            else:
                accepted = False
            temporary = self.path.with_suffix('.tmp')
            with temporary.open('w') as target:
                json.dump(rows,target)
                target.flush()
                os.fsync(target.fileno())
            temporary.replace(self.path)
            return accepted

    def acquire(self,owner,amount):
        if not math.isfinite(amount) or not 0 < amount <= self.limit:
            raise ValueError('Request exceeds host memory budget')
        return self._change(owner,amount)

    def release(self,owner):
        self._change(owner,None)


def process_identity(pid):
    stat = Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()
    return f'host:{pid}:{stat[19]}'


def docker_identity(container):
    row = json.loads(subprocess.run(['docker','inspect',container],check=True,capture_output=True,text=True,timeout=10).stdout)[0]
    if not row['State']['Running']:
        raise ValueError('Container is not running')
    return f"container:{row['Id']}:{row['State']['StartedAt']}", row


def owner_alive(owner):
    try:
        kind,identity,start = owner.split(':',2)
        if kind == 'host':
            if process_identity(int(identity)) == owner:
                return True
            return builders_running()  # PID reuse cannot free a surviving build.
        row=json.loads(subprocess.run(['/usr/bin/docker','inspect',identity],
            check=True,capture_output=True,text=True,timeout=10).stdout)[0]
        return (row['State']['Running'] and row['Id']==identity
                and row['State']['StartedAt']==start)
    except (OSError,ValueError,KeyError,IndexError,subprocess.SubprocessError):
        if owner.startswith('host:'):
            return builders_running()
        try:
            rows=subprocess.run(['/usr/bin/docker','ps','-a','--no-trunc','--format','{{.ID}}'],
                check=True,capture_output=True,text=True,timeout=10).stdout.splitlines()
            # Only a complete successful ID inventory can prove absence.
            return owner.split(':',2)[1] in rows
        except (OSError,ValueError,subprocess.SubprocessError):
            return True  # Unknown state cannot reclaim a live runner lease.


def builder_cache_identity(record):
    """Recognize only the root adapter's exact name, labels and bind mounts."""
    config=record.get('Config') or {}
    if not isinstance(config,dict):return None
    labels=config.get('Labels') or {}
    if not isinstance(labels,dict):return None
    project=labels.get('oxidex.cache-project','')
    run=labels.get('oxidex.run-id','')
    if (not isinstance(project,str) or not re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}',project)
        or not isinstance(run,str) or not re.fullmatch(r'run-[0-9a-f]{32}',run)
        or labels.get('oxidex.remote-build')!='true'
        or record.get('Name')!='/oxidex-run-'+run):
        return None
    mounts=record.get('Mounts',[])
    if not isinstance(mounts,list) or any(not isinstance(mount,dict) for mount in mounts):
        return None
    for destination,folder in (('/src','sources'),('/target','targets')):
        matching=[mount for mount in mounts if mount.get('Destination')==destination]
        if (len(matching)!=1 or matching[0].get('Type')!='bind'
            or matching[0].get('Source')!=str(BUILD_ROOT/folder/project)):
            return None
    return project,run


def stopped_builder(record):
    state=record.get('State') or {}
    if not isinstance(state,dict):return False
    return (state.get('Running') is False and state.get('Paused') is False
            and state.get('Restarting') is False and state.get('Pid')==0
            and state.get('Status') in ('created','exited','dead'))


def reconcile_stopped_builders():
    """Called under global SH/EX; never force-remove a container or take a wait lock."""
    result=subprocess.run(['/usr/bin/docker','ps','-aq','--no-trunc',
                           '--filter','label=oxidex.remote-build=true'],
                          check=True,capture_output=True,text=True,timeout=10)
    ids=result.stdout.split()
    if len(set(ids))!=len(ids) or any(not re.fullmatch(r'[0-9a-f]{64}',value) for value in ids):
        raise ValueError('Incomplete builder container inventory')
    remaining=False
    for identity in ids:
        def inspect():
            rows=json.loads(subprocess.run(['/usr/bin/docker','inspect',identity],
                check=True,capture_output=True,text=True,timeout=10).stdout)
            if not isinstance(rows,list) or len(rows)!=1 or not isinstance(rows[0],dict) or rows[0].get('Id')!=identity:
                raise ValueError('Builder identity changed')
            return rows[0]
        record=inspect()
        cache=builder_cache_identity(record)
        if cache is None or not stopped_builder(record):
            remaining=True
            continue
        with (LOCK_ROOT/f'oxidex-cache-active-{cache[0]}.lock').open('a') as active:
            try:
                fcntl.flock(active,fcntl.LOCK_EX|fcntl.LOCK_NB)
            except BlockingIOError:
                remaining=True
                continue
            # A start can complete between the first inspect and lock acquire.
            # No protocol client survives this exclusive lock, but recheck state
            # and identity before rm, which also refuses a now-running container.
            current=inspect()
            if builder_cache_identity(current)!=cache or not stopped_builder(current):
                remaining=True
                continue
            subprocess.run(['/usr/bin/docker','rm',identity],check=True,
                           capture_output=True,text=True,timeout=10)
    return remaining


def builders_running(*,global_lock_held=False):
    try:
        if global_lock_held:
            # Only the root drain CLI calls this, under its inherited global EX.
            return reconcile_stopped_builders()
        with (LOCK_ROOT/'oxidex-build.lock').open('a') as global_lock:
            fcntl.flock(global_lock,fcntl.LOCK_SH|fcntl.LOCK_NB)
            return reconcile_stopped_builders()
    except (OSError,ValueError,KeyError,TypeError,subprocess.SubprocessError):
        return True  # Unknown container state cannot reclaim memory.


def memory_envelope(config,total_gib,used_gib,container_usage):
    working=0.
    for usage in container_usage:
        match=re.fullmatch(r'\s*([0-9.]+)\s*(B|kB|MB|GB|TB|KiB|MiB|GiB|TiB)\s*',usage.split('/')[0])
        if not match:raise ValueError('Unrecognized Docker memory usage')
        scales={'B':1,'kB':1000,'MB':1000**2,'GB':1000**3,'TB':1000**4,
                'KiB':1024,'MiB':1024**2,'GiB':1024**3,'TiB':1024**4}
        working+=float(match[1])*scales[match[2]]/1024**3
    reserve=max(2,used_gib-working)+.25*len(config['runner_names'])
    return max(0,min(config['work_gib'],total_gib-reserve))


def observed_envelope(config):
    memory={line.split(':')[0]:int(line.split()[1])/1024/1024
            for line in Path('/proc/meminfo').read_text().splitlines() if line.startswith(('MemTotal:','MemAvailable:'))}
    result=subprocess.run(['/usr/bin/docker','stats','--no-stream','--format','{{json .}}'],
                           check=True,capture_output=True,text=True,timeout=15)
    usage=[json.loads(line)['MemUsage'] for line in result.stdout.splitlines() if line.strip()]
    return memory_envelope(config,memory['MemTotal'],memory['MemTotal']-memory['MemAvailable'],usage)


def peer_owner(pid,uid,config):
    if uid == 0:
        return process_identity(pid),'builder'
    if uid != 1001:
        raise ValueError('Unauthorized admission peer')
    cgroup = Path(f'/proc/{pid}/cgroup').read_text()
    ids = re.findall(r'(?:docker[-/])([a-f0-9]{64})(?:\.scope)?',cgroup)
    if len(set(ids)) != 1:
        raise ValueError('Runner must belong to a Docker cgroup')
    owner,row = docker_identity(ids[0])
    if row['Name'].lstrip('/') not in config['runner_names']:
        raise ValueError('Container is outside this runner deployment')
    return owner,'runner'


def request(action,kind='runner'):
    with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
        client.settimeout(30)
        client.connect('/run/gha-admission/socket')
        client.sendall((json.dumps(dict(action=action,kind=kind))+'\n').encode())
        client.shutdown(socket.SHUT_WR)
        response = json.loads(client.recv(4096))
    if 'error' in response:
        raise RuntimeError(response['error'])
    return response['accepted']


def acquire_build():
    if not request('acquire','builder'):
        raise SystemExit(75)


def release_build():
    request('release','builder')


def serve():
    if os.geteuid() != 0:
        raise RuntimeError('Admission service must run as root')
    directory = Path('/run/gha-admission')
    directory.mkdir(mode=0o755,exist_ok=True)
    ledger = Path('/var/lib/gha-admission')
    ledger.mkdir(mode=0o700,exist_ok=True)
    path = directory/'socket'
    path.unlink(missing_ok=True)
    with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as server:
        server.bind(str(path))
        os.chown(path,0,1001)
        path.chmod(0o660)
        server.listen(128)
        envelope=0
        measured_at=0
        while True:
            client,_ = server.accept()
            with client:
                try:
                    client.settimeout(5)
                    pid,uid,_ = struct.unpack('3i',client.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))
                    config = json.loads(Path('/etc/gha-admission.json').read_text())
                    owner,kind = peer_owner(pid,uid,config)
                    raw = client.recv(4096)
                    message = json.loads(raw)
                    if message.get('kind','runner') != kind:
                        raise ValueError('Admission kind does not match peer identity')
                    limit=config['work_gib']
                    if message['action']=='acquire':
                        if time.time()-measured_at>=60:
                            try:
                                envelope=observed_envelope(config)
                                measured_at=time.time()
                            except (OSError,ValueError,KeyError,subprocess.SubprocessError):
                                envelope=0
                        limit=min(limit,envelope)
                    # A late stopped create may appear after its lease was
                    # pruned on an earlier empty inventory. Always reconcile
                    # refreshes, even when no dead host owner remains to query.
                    builders_present=builders_running()
                    pool = LeasePool(ledger/'leases.json',config['work_gib'],alive=owner_alive)
                    if message['action'] == 'release':
                        if kind!='builder' or not builders_present:
                            pool.release(owner)
                        accepted = True
                    elif message['action'] == 'acquire':
                        if limit<config[kind+'_gib'] or (kind == 'builder' and Path('/run/oxidex-build-draining').exists()):
                            accepted = False
                        else:
                            pool.limit=limit
                            accepted = pool.acquire(owner,config[kind+'_gib'])
                    else:
                        raise ValueError('Unknown admission action')
                    answer = dict(accepted=accepted)
                except (OSError,ValueError,KeyError,RuntimeError,subprocess.SubprocessError) as exc:
                    answer = dict(error=str(exc)[:200])
                try:
                    client.sendall(json.dumps(answer).encode())
                except OSError:
                    # The disconnected caller can retry; its lease stays in
                    # the ledger until refreshed against actual owner state.
                    pass


if __name__ == '__main__':
    action = sys.argv[1]
    if action == 'serve':
        serve()
    elif action == 'acquire':
        # A GitHub-assigned job is already busy during its pre-job hook. Let it
        # finish admission even after isolation; drain waits for busy to clear.
        while True:
            try:
                if request('acquire'):
                    break
            except OSError:
                # A root-controlled service restart must not fail an assigned job.
                pass
            time.sleep(5)
    elif action == 'release':
        request('release')
    elif action == 'reconcile-builders':
        if os.geteuid()!=0 or sys.argv[2:]!=['--global-lock-held']:
            raise SystemExit('Root drain requires its held global lock')
        raise SystemExit(75 if builders_running(global_lock_held=True) else 0)
    else:
        raise SystemExit('Unknown admission command')
