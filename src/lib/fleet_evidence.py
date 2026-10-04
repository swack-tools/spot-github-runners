"""Bounded absolute resource history and compatible CI timing observations."""
import math
import statistics
import base64
import json
import shlex
import subprocess
from pathlib import Path
from datetime import datetime


def epoch(value):
    return datetime.fromisoformat(value.replace('Z', '+00:00')).timestamp()


def record_resources(state, vms, samples, timestamp):
    identities = [f'{vm.zone}/{vm.instance_id}' for vm in vms]
    valid = (samples is not None and len(samples) == len(vms) and bool(vms)
             and all(vm.instance_id and vm.vcpu and vm.ram_gib for vm in vms))
    try:
        valid = valid and all(len(pair) == 2 and all(math.isfinite(v) and 0 <= v <= 1
                                                   for v in pair) for pair in samples)
    except (TypeError, ValueError):
        valid = False
    if not valid:
        state['resource_history'] = {}
        return None
    history = state.setdefault('resource_history', {})
    for key in list(history):
        if key not in identities:
            del history[key]
    observations = []
    for vm, key, (cpu, ram) in zip(vms, identities, samples):
        windows = history.setdefault(key, [])
        if windows and (timestamp < windows[-1]['observed_at']
                        or timestamp - windows[-1]['observed_at'] > 450):
            windows.clear()
        if not windows or timestamp - windows[-1]['observed_at'] >= 300:
            windows.append(dict(observed_at=timestamp,cpu_cores=cpu*vm.vcpu,used_ram_gib=ram*vm.ram_gib))
        windows[:] = [row for row in windows if timestamp - row['observed_at'] <= 1800][-7:]
        observations.append(dict(instance_id=vm.instance_id, zone=vm.zone,
                                 observed_at=timestamp,cpu_cores=cpu*vm.vcpu,
                                 used_ram_gib=ram*vm.ram_gib,
                                 peak_cpu_cores=max(row['cpu_cores'] for row in windows),
                                 peak_ram_gib=max(row['used_ram_gib'] for row in windows),
                                 windows=len(windows),cpu_fraction=cpu,memory_fraction=ram))
    return observations


def workload_signals(jobs, runner_labels, timestamp, *, group_ids=None):
    result = dict(pending=False,queue_delayed=False,duration_regression=False)
    completions = {}
    for job in jobs:
        labels = set(job.get('labels') or [])
        if (not labels or not any(labels <= set(available) for available in runner_labels)
                or (job.get('runner_group_id') and group_ids is not None
                    and job['runner_group_id'] not in group_ids)):
            continue
        try:
            if job.get('status') in ('queued','waiting','pending'):
                result['pending'] = True
                created = job.get('created_at') or job.get('run_created_at')
                if created and timestamp - epoch(created) > 120:
                    result['queue_delayed'] = True
            elif job.get('conclusion') == 'success' and job.get('completed_at') and job.get('started_at'):
                if timestamp - epoch(job['completed_at']) > 86400:
                    continue
                key = (job.get('repository'),job.get('workflow_id'),job.get('name'),tuple(sorted(labels)))
                duration = epoch(job['completed_at']) - epoch(job['started_at'])
                if 0 < duration <= 86400:
                    completions.setdefault(key,[]).append((epoch(job['completed_at']),duration))
        except (ValueError, TypeError, OverflowError):
            continue
    for values in completions.values():
        values.sort()
        if len(values) >= 10:
            baseline = statistics.median(row[1] for row in values[:-5])
            if statistics.median(row[1] for row in values[-5:]) >= baseline * 1.25:
                result['duration_regression'] = True
    return result


def remember_jobs(data, repository, runs, jobs, observed_at):
    history = data.setdefault('capacity_jobs', {})
    for job in jobs.values():
        run = runs.get(job['run_id'], {})
        history[f"{repository}/{job['id']}/{job.get('run_attempt',1)}"] = {
            **{key:job.get(key) for key in ('id','name','status','conclusion','labels','runner_group_id',
                                         'runner_name','created_at','started_at','completed_at')},
            'repository':repository, 'workflow_id':run.get('workflow_id'),
            'run_created_at':run.get('created_at'), 'observed_at':observed_at}
    now = epoch(observed_at)
    for key, job in list(history.items()):
        if now - epoch(job['observed_at']) > 86400:
            del history[key]
    if len(history) > 5000:
        keep = sorted(history,key=lambda key:history[key]['observed_at'],reverse=True)[:5000]
        data['capacity_jobs'] = {key:history[key] for key in keep}


def read_host_events(project, vm, cursor=None):
    # Execute the same bounded sampler on legacy hosts without rewriting their
    # bootstrap or restarting runner containers. New hosts install this file.
    source = Path(__file__).with_name('host_evidence.py').read_bytes()
    program = 'import base64;exec(compile(base64.b64decode(' + repr(base64.b64encode(source).decode()) + "),'host-evidence','exec'))"
    result = subprocess.run(['gcloud','compute','ssh',vm.name,f'--zone={vm.zone}',
                              f'--project={project}','--quiet',
                              '--command=sudo python3 -c '+shlex.quote(program)],
                             check=True,text=True,capture_output=True,timeout=60)
    data = json.loads(result.stdout)
    data['instance_id'] = vm.instance_id
    return data
