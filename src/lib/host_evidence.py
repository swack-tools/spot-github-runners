"""Standalone root sampler; no credentials, job payloads or journal text leave the host."""
import hashlib
import json
import re
import subprocess
import time
from pathlib import Path


def classify_events(journal, docker, boot_id, timestamp):
    events = []
    for row in journal:
        message = row.get('MESSAGE','')
        if not isinstance(message,str) or not re.search(r'(out of memory|oom-kill|Killed process)',message,re.I):
            continue
        try:
            observed = int(row['__REALTIME_TIMESTAMP']) / 1e6
        except (KeyError,ValueError,TypeError):
            continue
        if not timestamp - 1800 <= observed <= timestamp:
            continue
        kind = 'container' if re.search(r'(cgroup|CONSTRAINT_MEMCG)',message,re.I) else 'host'
        event_id = hashlib.sha256(f'{boot_id}:{observed}:{message}'.encode()).hexdigest()
        events.append(dict(id=event_id,kind=kind,at=observed))
    for row in docker:
        if row.get('Action') != 'oom':
            continue
        try:
            observed = int(row['timeNano']) / 1e9
        except (KeyError,ValueError,TypeError):
            continue
        if timestamp - 1800 <= observed <= timestamp:
            event_id = hashlib.sha256(f"{boot_id}:{observed}:{row.get('id')}:docker-oom".encode()).hexdigest()
            events.append(dict(id=event_id,kind='container',at=observed))
    return sorted(events,key=lambda row:row['at'])


def command_json_lines(command):
    raw = subprocess.run(command,check=True,capture_output=True,text=True,timeout=30).stdout
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
    memory = {line.split(':')[0]:int(line.split()[1])/1024/1024
              for line in Path('/proc/meminfo').read_text().splitlines() if line.startswith(('MemTotal:','MemAvailable:'))}
    return dict(observed_at=timestamp,boot_id=boot,ram_gib=memory['MemTotal'],
                used_ram_gib=memory['MemTotal']-memory['MemAvailable'],
                events=classify_events(journal,docker,boot,timestamp),complete=True)


if __name__ == '__main__':
    print(json.dumps(sample()))
