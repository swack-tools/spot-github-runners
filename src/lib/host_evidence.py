"""Standalone root sampler; no credentials, job payloads or journal text leave the host."""
import hashlib
import json
import re
import subprocess
import time
import math
from pathlib import Path


def validate_io_pressure(values):
    keys={f'{scope}_{name}' for scope in ('some','full') for name in ('avg10','avg60','avg300','total')}
    if not isinstance(values,dict) or set(values)!=keys:
        raise ValueError('Incomplete IO pressure evidence')
    for key,value in values.items():
        if isinstance(value,bool) or not isinstance(value,(int,float)):
            raise ValueError('Invalid IO pressure number')
        if key.endswith('_total'):
            if not isinstance(value,int) or not 0<=value<2**64:
                raise ValueError('Invalid IO pressure counter')
        elif not 0<=value<=100 or not math.isfinite(value):
            raise ValueError('IO pressure percentage outside bounds')
    return values


def parse_io_pressure(text):
    """Parse the two kernel PSI lines, bounded independently of OOM telemetry."""
    if not isinstance(text,str) or len(text)>1024:
        raise ValueError('IO pressure sample exceeds bounded size')
    values={};scopes=set()
    for line in text.splitlines():
        parts=line.split()
        if len(parts)!=5 or parts[0] not in ('some','full') or parts[0] in scopes:
            raise ValueError('Malformed IO pressure line')
        scope=parts[0];scopes.add(scope)
        for item in parts[1:]:
            key,separator,value=item.partition('=')
            name=scope+'_'+key
            if not separator or name in values:
                raise ValueError('Malformed IO pressure field')
            values[name]=int(value) if key=='total' else float(value)
    return validate_io_pressure(values)


def classify_events(journal, docker, boot_id, timestamp, *, window_start=None):
    events = []
    window_start=timestamp-1800 if window_start is None else window_start
    for row in journal:
        message = row.get('MESSAGE','')
        if not isinstance(message,str):
            continue
        # "invoked oom-killer" starts the diagnostic dump for either a global
        # or a cgroup OOM. Only the actual decision/kill identifies its scope.
        decision=re.search(r'\boom-kill:\s*constraint=(CONSTRAINT_[A-Z_]+)\b',message,re.I)
        if decision:
            kind={'CONSTRAINT_NONE':'host','CONSTRAINT_MEMCG':'container'}.get(decision[1].upper(),'unknown')
        elif re.search(r'\bMemory cgroup out of memory:\s*Killed process\b',message,re.I):
            kind='container'
        elif re.search(r'\bOut of memory:\s*Killed process\b',message,re.I):
            kind='host'
        else:
            continue
        try:
            observed = int(row['__REALTIME_TIMESTAMP']) / 1e6
        except (KeyError,ValueError,TypeError):
            continue
        if not window_start <= observed <= timestamp:
            continue
        event_id = hashlib.sha256(f'{boot_id}:{observed}:{message}'.encode()).hexdigest()
        events.append(dict(id=event_id,kind=kind,workload='unknown',at=observed))
    for row in docker:
        if row.get('Action') != 'oom':
            continue
        try:
            observed = int(row['timeNano']) / 1e9
        except (KeyError,ValueError,TypeError):
            continue
        if window_start <= observed <= timestamp:
            event_id = hashlib.sha256(f"{boot_id}:{observed}:{row.get('id')}:docker-oom".encode()).hexdigest()
            attributes=row.get('Actor',{}).get('Attributes',{})
            workload=('builder' if attributes.get('oxidex.remote-build')=='true' else
                      'runner' if attributes.get('name','').startswith('gha-oxidex-runners-') else 'unknown')
            events.append(dict(id=event_id,kind='container',workload=workload,at=observed))
    return sorted(events,key=lambda row:row['at'])


def command_json_lines(command, *, empty_grep=False):
    result=subprocess.run(command,check=not empty_grep,capture_output=True,text=True,timeout=30)
    if empty_grep:
        if result.returncode==1 and not result.stdout and not result.stderr:
            return []  # journalctl --grep uses 1 for a confirmed empty match.
        if result.returncode!=0:
            raise subprocess.CalledProcessError(result.returncode,command,output=result.stdout,stderr=result.stderr)
        if result.stderr:
            raise RuntimeError('Boot OOM journal query emitted diagnostics')
    raw=result.stdout
    if len(raw) > 4*1024*1024:
        raise RuntimeError('Host evidence exceeds bounded window')
    return [json.loads(line) for line in raw.splitlines() if line.strip()]


def sample():
    timestamp = time.time()
    boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
    journal = command_json_lines(['journalctl','-k','--since',f'@{int(timestamp-1800)}',
                                  '--no-pager','-o','json','-n','2000'])
    docker = command_json_lines(['docker','events','--since',str(int(timestamp-1800)),
                                 '--until',str(int(timestamp)), '--filter','event=oom','--format','{{json .}}'])
    # Legacy controller state stores only a pending kind. A complete current-
    # boot decision history distinguishes false context from older real globals.
    globals_journal=command_json_lines(['journalctl','-k','-b','0','--no-pager','--quiet','-o','json','-n','2000',
        '--grep=^(oom-kill:constraint=CONSTRAINT_NONE|Out of memory: Killed process)'],empty_grep=True)
    global_events=classify_events(globals_journal,[],boot,timestamp,window_start=0)
    memory = {line.split(':')[0]:int(line.split()[1])/1024/1024
              for line in Path('/proc/meminfo').read_text().splitlines() if line.startswith(('MemTotal:','MemAvailable:'))}
    try:
        io_pressure=parse_io_pressure(Path('/proc/pressure/io').read_text())
    except (OSError,ValueError):
        io_pressure=None  # Kernels without PSI still provide valid OOM evidence.
    return dict(observed_at=timestamp,boot_id=boot,ram_gib=memory['MemTotal'],
                used_ram_gib=memory['MemTotal']-memory['MemAvailable'],
                io_pressure=io_pressure,io_complete=io_pressure is not None,
                events=classify_events(journal,docker,boot,timestamp),complete=len(journal)<2000,
                boot_oom_complete=(len(globals_journal)<2000 and len(global_events)==len(globals_journal)
                                   and all(event['kind']=='host' for event in global_events)),
                boot_host_oom_ids=sorted(event['id'] for event in global_events if event['kind']=='host'))


if __name__ == '__main__':
    print(json.dumps(sample()))
