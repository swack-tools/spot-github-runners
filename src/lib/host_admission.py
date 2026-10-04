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


def builders_running():
    try:
        result=subprocess.run(['/usr/bin/docker','ps','-q','--filter','label=oxidex.remote-build=true'],
                              check=True,capture_output=True,text=True,timeout=10)
        return bool(result.stdout.strip())
    except (OSError,subprocess.SubprocessError):
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
                    pool = LeasePool(ledger/'leases.json',config['work_gib'],alive=owner_alive)
                    if message['action'] == 'release':
                        if kind!='builder' or not builders_running():
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
    else:
        raise SystemExit('Unknown admission command')
