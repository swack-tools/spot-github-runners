"""Select an idle remote-builder host using complete Monitoring windows."""
import json
import math
import shlex
import subprocess
from types import SimpleNamespace
from .resource_metrics import read_utilization


def rank_workers(rows):
    eligible = [(vm, sample) for vm, sample in rows if sample is not None
                and len(sample) == 2 and all(math.isfinite(v) and 0 <= v < .75 for v in sample)]
    return sorted(eligible, key=lambda row: (max(row[1]), sum(row[1]), row[0].name))


def select_worker(project):
    inventory = json.loads(subprocess.check_output([
        'gcloud', 'compute', 'instances', 'list', '--project='+project,
        '--format=json(name,id,zone,status)'], text=True))
    observations = []
    for row in inventory:
        if row.get('status') != 'RUNNING' or not row['name'].startswith(('oxidex-runners-', 'oxidex-buildbench-')):
            continue
        vm = SimpleNamespace(name=row['name'], instance_id=str(row['id']), zone=row['zone'].rsplit('/',1)[-1])
        sample = read_utilization(project, [vm])
        observations.append((vm, sample[0] if sample else None))
    # Probe only candidates with trustworthy low utilization, best first.
    probe = ('test -f /etc/oxidex-remote-build.json && '
             'test ! -e /run/oxidex-build-draining && '
             'test -z "$(docker ps -q --filter label=oxidex.remote-build=true)" && '
             'flock -n -x /run/oxidex-build.lock true')
    for vm, sample in rank_workers(observations):
        try:
            result = subprocess.run(['gcloud','compute','ssh',vm.name,'--zone='+vm.zone,
            '--project='+project,'--quiet','--command=sudo sh -c '+shlex.quote(probe)],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)
        except subprocess.TimeoutExpired:
            continue
        if result.returncode == 0:
            print(f'Selected {vm.name} ({vm.zone}): CPU {sample[0]:.1%}, memory {sample[1]:.1%}', flush=True)
            return vm, sample
    raise RuntimeError('No available remote builder with complete CPU/memory metrics below 75%; retry later')
