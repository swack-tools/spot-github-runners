"""Persist one tier replacement; existing identity-guarded drain owns deletion."""
import logging
import copy
import math
import subprocess
import time
from collections import Counter
from dataclasses import asdict
from decimal import Decimal
from .runner_tiers import TIERS,tier_for_shape,validate_topology
from .tier_policy import fits,reservation_fits
from .fleet_evidence import io_status
from .oom_obligations import obligations_for,pending_kinds,remember_oom,clear_oom

LOG=logging.getLogger('spot_runner_autoscaler')


def expected_slots(vm,state):
    return int(state.get('host_deployments',{}).get(f'{vm.zone}/{vm.instance_id}',{}).get('expected_runner_count',8))


def topology_for(vms):
    names=[tier.name if (tier:=tier_for_shape(vm.vcpu,vm.ram_gib)) else 'legacy' for vm in vms]
    rank={'legacy':0,'bulky':.5,'large':1,'expanded':2,'starting':3,'small':4}
    return tuple(sorted(names,key=rank.__getitem__))


def begin_migration(vms,target,state,state_path,reason,slots=None,force_ids=(),slot_counts=(),io_pressure_sources=()):
    from .spot_runner_autoscaler import write_scale_state
    validate_topology(target)
    if state.get('migration') or state.get('host_operation'):
        raise RuntimeError('Another lifecycle transaction is active')
    remaining=Counter(target)
    sources=[]
    retained=[]
    for vm in vms:
        tier=tier_for_shape(vm.vcpu,vm.ram_gib)
        if tier and remaining[tier.name] and vm.instance_id not in force_ids:
            remaining[tier.name]-=1
            retained.append(vm.instance_id)
        else:
            if not vm.instance_id:raise ValueError('Migration requires provider instance identity')
            sources.append(asdict(vm))
    pending_tiers=list(remaining.elements())
    recorded_slots={}
    for name,count in slot_counts:
        if name not in TIERS or not isinstance(count,int) or not 1<=count<=TIERS[name].maximum_slots:
            raise ValueError('Recovery receipt contains unsafe runner density')
        recorded_slots.setdefault(name,[]).append(count)
    pending_slots=[recorded_slots[name].pop(0) if recorded_slots.get(name)
                   else slots or TIERS[name].initial_slots for name in pending_tiers]
    if io_pressure_sources and (reason!='storage-pressure canary replacement'
                               or set(io_pressure_sources)!={row['instance_id'] for row in sources}):
        raise ValueError('Storage relief exemption requires exact retiring sources')
    state['migration']=dict(target=list(target),sources=sources,retained_ids=retained,
        pending_tiers=pending_tiers,pending_slots=pending_slots,replacement_ids=[],phase='launch',reason=reason,
        started_at=time.time(),slots=slots)
    if io_pressure_sources:
        state['migration']['io_pressure_source_ids']=sorted(io_pressure_sources)
    write_scale_state(state_path,state)


def retirement_io_safe(migration,resources,state):
    if migration.get('reason')!='storage-pressure canary replacement':
        return io_status(resources)=='healthy'
    allowed=set(migration.get('io_pressure_source_ids',[]))
    if not allowed or allowed!={row['instance_id'] for row in migration['sources']}:
        return False
    for row in resources or []:
        key=f"{row.get('zone')}/{row.get('instance_id')}";health=state.get('host_health',{}).get(key,{})
        try:
            if (health.get('instance_id')!=row['instance_id'] or health.get('complete') is not True
                    or not health.get('boot_id') or not 0<=time.time()-health['observed_at']<=450
                    or not 0<=time.time()-row['observed_at']<=450
                    or health.get('boot_oom_complete') is not True or health.get('boot_host_oom_ids')!=[]
                    or any(event.get('kind') in ('host','unknown') for event in health.get('events',[]))
                    or 'host' in pending_kinds(state,key)):
                return False
        except (KeyError,TypeError,ValueError):
            return False
    destinations=[row for row in resources or [] if row.get('instance_id') not in allowed]
    # Source stalls are the explicit reason for replacement. They cannot veto
    # retirement once every destination has healthy IO; missing source evidence
    # still holds because measured demand and pressure must remain complete.
    return io_status(resources)!='incomplete' and io_status(destinations)=='healthy'


def retarget_unhealthy_storage_canary(vms,args,state,state_path):
    """Safely replace the exact stalled two-32-vCPU canary with one 16/64 host."""
    migration=state.get('migration',{})
    if (migration.get('phase')!='healthy' or migration.get('pending_tiers')
            or migration.get('awaiting_launch') or state.get('host_operation')
            or len(migration.get('sources',[]))!=1 or len(migration.get('replacement_ids',[]))!=1
            or migration.get('target')!=['bulky']):
        return False
    managed=[vm for vm in vms if vm.spot and vm.name.startswith('oxidex-runners-')]
    recorded=migration['sources'][0];source_id=recorded['instance_id'];bad_id=migration['replacement_ids'][0]
    identities={source_id,bad_id}
    if (len(managed)!=2 or len(identities)!=2 or {vm.instance_id for vm in managed}!=identities
            or any(not vm.running or (vm.vcpu,vm.ram_gib)!=(32,128) for vm in managed)):
        return False
    source=next(vm for vm in managed if vm.instance_id==source_id)
    if (source.name!=recorded['name'] or source.zone!=recorded['zone']
            or tuple(source.disks)!=tuple(recorded.get('disks',[]))
            or not source.disks or any(not disk.get('diskId') for disk in source.disks)):
        return False
    if (any(f'{vm.zone}/{vm.name}' in state.get('intentional_deletions',[]) for vm in managed)
            or identities & set(state.get('intentional_deletion_ids',{}).values())):
        return False
    job_memory=getattr(args,'tier_job_memory_gib',15)
    if (not isinstance(job_memory,(int,float)) or isinstance(job_memory,bool) or job_memory<15
            or not reservation_fits(('large',),job_memory)
            or state.get('latest_workload',{}).get('complete') is not True):
        return False
    timestamp=time.time();resources=state.get('latest_resources') or []
    try:
        if ({(row['zone'],row['instance_id']) for row in resources}!={(vm.zone,vm.instance_id) for vm in managed}
                or len(resources)!=2 or io_status(resources)=='incomplete'
                or any(row['windows']<6 or not 0<=timestamp-row['observed_at']<=450
                       or any(not math.isfinite(row[name]) or row[name]<0 for name in ('peak_cpu_cores','peak_ram_gib'))
                       for row in resources)
                or not fits(('large',),resources)):
            return False
        for vm in managed:
            key=f'{vm.zone}/{vm.instance_id}';health=state.get('host_health',{}).get(key,{})
            if (health.get('instance_id')!=vm.instance_id or health.get('complete') is not True
                    or not health.get('boot_id') or not 0<=timestamp-health['observed_at']<=450
                    or health.get('boot_oom_complete') is not True or health.get('boot_host_oom_ids')!=[]
                    or any(event.get('kind') in ('host','unknown') for event in health.get('events',[]))
                    or 'host' in pending_kinds(state,key)):
                return False
        bad=next(vm for vm in managed if vm.instance_id==bad_id);key=f'{bad.zone}/{bad.instance_id}'
        history=state.get('io_history',{}).get(key,{})
        windows=history.get('windows',[])
        if (history.get('boot_id')!=state['host_health'][key]['boot_id'] or len(windows)<2
                or not 300<=windows[-1]['observed_at']-windows[-2]['observed_at']<=450
                or not 0<=timestamp-windows[-1]['observed_at']<=450
                or any(io_status([dict(io_complete=True,io_pressure_peaks=row)])!='pressured' for row in windows[-2:])):
            return False
    except (KeyError,TypeError,ValueError,OverflowError):
        return False
    candidate=copy.deepcopy(state)
    candidate['migration_history']=(candidate.get('migration_history',[])+[
        dict(retargeted_at=timestamp,reason='sustained storage pressure',migration=copy.deepcopy(migration))])[-3:]
    candidate.pop('migration')
    begin_migration(managed,('large',),candidate,state_path,'storage-pressure canary replacement',
                    slots=TIERS['large'].initial_slots,io_pressure_sources=identities)
    # Existing job trackers hold references into other state subtrees.
    state['migration']=candidate['migration']
    state['migration_history']=candidate['migration_history']
    LOG.info('Retargeted exact stalled storage canary: preserve both 32-vCPU sources until a healthy 16/64 replacement')
    return True


def reconcile_migration(vms,runners,args,state,state_path):
    from . import spot_runner_autoscaler as controller
    from .fleet_logging import vm_from_record
    migration=state.get('migration')
    if not migration:return False
    if state.get('host_operation'):return True
    fleet=controller.spot_instances(vms)
    by_id={vm.instance_id:vm for vm in fleet}
    for record in migration['sources']:
        reused=next((vm for vm in vms if vm.name==record['name'] and vm.zone==record['zone']),None)
        if reused and reused.instance_id!=record['instance_id']:
            LOG.error('Migration holds: source name has a different provider identity')
            return True
    # Recovery cleans a real lost identity before replacement can resume.
    if any(not row.get('_cleanup_complete') for row in state.get('pending_vm_recovery',{}).values()):
        return True
    if migration.get('awaiting_launch'):
        pending=migration['awaiting_launch']
        vm=next((vm for vm in fleet if vm.name==pending['name'] and vm.zone==pending['zone']),None)
        if vm:
            if pending.get('instance_id') and vm.instance_id!=pending['instance_id']:
                LOG.error('Migration holds: replacement provider identity changed')
                return True
            tier=tier_for_shape(vm.vcpu,vm.ram_gib)
            if not tier or tier.name!=migration['pending_tiers'][0]:
                raise RuntimeError('Replacement shape differs from requested tier')
            migration['replacement_ids'].append(vm.instance_id)
            migration['pending_tiers'].pop(0)
            if migration.get('pending_slots'):migration['pending_slots'].pop(0)
        migration.pop('awaiting_launch')
        controller.write_scale_state(state_path,state)
    if migration['pending_tiers']:
        # Count provisioning/stopped managed identities too; the hard ceiling
        # cannot be bypassed by an incomplete running-only inventory.
        managed=[vm for vm in vms if vm.spot and vm.name.startswith('oxidex-runners-')]
        tier=TIERS[migration['pending_tiers'][0]]
        slots=(migration.get('pending_slots') or [migration.get('slots') or tier.initial_slots])[0]
        overlap_blocked=(len(managed)>=min(5,args.maximum_hosts)
            or sum(vm.vcpu or 80 for vm in managed)+tier.vcpu>80
            or sum(expected_slots(vm,state) for vm in managed)+slots>100)
        if overlap_blocked:
            remaining=[vm for vm in fleet if vm.instance_id not in {row['instance_id'] for row in migration['sources']}]
            topology=topology_for(remaining)
            if (state.get('latest_workload',{}).get('complete') is not True
                    or not remaining or 'legacy' in topology
                    or io_status(state.get('latest_resources'))!='healthy'
                    or not fits(topology,state.get('latest_resources'))):
                LOG.info('Migration holds at overlap ceiling: no safe remaining capacity')
                return True
            migration['phase']='drain-first'
        else:
            tier=TIERS[migration['pending_tiers'][0]]
            totals={}
            controller.check_running_prices(fleet,args.project,Decimal('999'),totals=totals)
            if len(totals)!=len(fleet):
                LOG.info('Migration launch holds: incomplete current all-disk pricing')
                return True
            current=sum(totals.values(),Decimal(0))
            previous_ceiling=Decimal(migration.get('spend_ceiling','15'))
            if ('spend_ceiling' not in migration or
                    ('overlap_baseline_daily_usd' not in migration and current>=previous_ceiling>=Decimal('15'))):
                allowance=Decimal(0)
                if current>=Decimal('15'):
                    retiring=sum((totals.get(row['name'],Decimal(0)) for row in migration['sources']),Decimal(0))
                    allowance=min(retiring,sum((TIERS[name].max_daily_usd for name in migration['pending_tiers']),Decimal(0)))
                    migration['overlap_baseline_daily_usd']=str(current)
                    migration['overlap_allowance_daily_usd']=str(allowance)
                migration['spend_ceiling']=str(max(Decimal('15'),current+allowance))
                controller.write_scale_state(state_path,state)
            offers=controller.discover_tier_offers(args.project,tier,args.disk_size_gb,getattr(args,'tier_job_memory_gib',15))
            offers=[offer for offer in offers if current+offer.total_daily_usd<Decimal(migration['spend_ceiling'])]
            slots=(migration.get('pending_slots') or [migration.get('slots') or tier.initial_slots])[0]
            if (sum(vm.vcpu or 80 for vm in managed)+tier.vcpu>80
                    or sum(expected_slots(vm,state) for vm in managed)+slots>100):
                LOG.info('Migration holds: overlapping CPU/slot ceiling would be exceeded')
                return True
            if not offers:
                LOG.info('Migration holds: no qualifying %s offer within disk and overlap budget',tier.name)
                return True
            args.launch_tier=tier.name
            args.launch_slots=slots
            controller.scale_up(1,offers,args,state,state_path,
                refresh=lambda: [row for row in controller.discover_tier_offers(args.project,tier,args.disk_size_gb,getattr(args,'tier_job_memory_gib',15))
                                  if current+row.total_daily_usd<Decimal(migration['spend_ceiling'])])
            if state.get('host_operation',{}).get('kind')=='launch':
                migration['awaiting_launch']=state['host_operation']['vm']
                controller.write_scale_state(state_path,state)
            return True
    if not migration['pending_tiers'] and migration['phase']!='drain':
        migration['phase']='healthy'
        if retarget_unhealthy_storage_canary(vms,args,state,state_path):
            return True
        rows={f"{row['zone']}/{row['instance_id']}":row for row in state.get('latest_resources') or []}
        healthy=True
        checks=migration.setdefault('healthy_checks',{})
        for identity in migration['replacement_ids']:
            vm=by_id.get(identity)
            if not vm:
                # Lost canary: restore its allowed tier, never delete the source.
                receipt=next((row for key,row in state.get('host_deployments',{}).items() if key.endswith('/'+identity)),{})
                if receipt.get('tier'):
                    migration['pending_tiers'].append(receipt['tier'])
                    counts=migration.setdefault('pending_slots',[
                        migration.get('slots') or TIERS[name].initial_slots for name in migration['pending_tiers'][:-1]])
                    counts.append(receipt.get('expected_runner_count') or TIERS[receipt['tier']].initial_slots)
                    migration['replacement_ids'].remove(identity)
                    controller.write_scale_state(state_path,state)
                return True
            key=f'{vm.zone}/{vm.instance_id}'
            sample=rows.get(key)
            health=state.get('host_health',{}).get(key,{})
            assigned=controller.host_runners(vm,runners,fleet)
            names={runner.name for runner in assigned if runner.online}
            ready={f'gha-{vm.name}-{i}' for i in range(1,expected_slots(vm,state)+1)}<=names
            observed=time.time()
            # Replacement validation owns the lifecycle barrier, so the normal
            # budget policy cannot repair a constrained container here. Repair
            # only identified containers with fresh host headroom. The remote
            # adjustment owns per-container lease checks and partial outcomes.
            if (ready and sample and sample.get('instance_id')==vm.instance_id
                    and health.get('instance_id')==vm.instance_id and health.get('complete') is True
                    and 0<=observed-sample.get('observed_at',0)<=450
                    and 0<=observed-health.get('observed_at',0)<=450
                    and sample['memory_fraction']<.75
                    and 'host' not in pending_kinds(state,key)
                    and not any(event.get('kind')=='host' for event in health.get('events',[]))):
                repaired=set(migration.get('oom_budget_repairs',[]))
                for kind in ('runner','builder'):
                    ids={event['id'] for event in health.get('events',[])
                         if event.get('kind')=='container' and event.get('workload')==kind
                         and event.get('id') and event['id'] not in repaired}
                    if ids:remember_oom(state,key,kind,health.get('boot_id'),ids)
                    # Already-seen attributed events can leave the bounded
                    # telemetry window before busy siblings release their leases.
                    ids|=set(obligations_for(state,key).get(kind,{}).get('event_ids',[]))-repaired
                    if not ids:continue
                    try:
                        outcome=controller.adjust_admission(vm,args,kind)
                    except (OSError,ValueError,RuntimeError,subprocess.SubprocessError) as exc:
                        LOG.warning('Replacement budget repair holds for %s: %s',vm.name,controller.error_detail(exc))
                        continue
                    if outcome is True:
                        repaired.update(ids)
                        migration['oom_budget_repairs']=sorted(repaired)
                        clear_oom(state,key,kind)
                        LOG.info('Repaired idle %s memory cap on replacement %s; fresh healthy windows still required',kind,vm.name)
            good=(ready and sample and not pending_kinds(state,key)
                  and sample['cpu_fraction']<.75 and sample['memory_fraction']<.75
                  and io_status([sample])=='healthy'
                  and health.get('complete') and not health.get('events')
                  and time.time()-health.get('observed_at',0)<=450)
            if good and getattr(args,'remote_builder',False):
                good=controller.builder_ready(vm,args)
            tick=sample['observed_at'] if sample else 0
            previous=checks.get(key,{})
            count=previous.get('count',0)
            if not good:count=0
            elif tick-previous.get('at',-1e20)>=300:
                count=count+1 if tick-previous.get('at',tick)<=450 else 1
                checks[key]=dict(count=count,at=tick)
            if not good:checks[key]=dict(count=0,at=tick)
            healthy=healthy and count>=2
        controller.write_scale_state(state_path,state)
        if not healthy:
            LOG.info('Migration waits for two healthy complete replacement windows')
            return True
        resources=state.get('latest_resources') or []
        identities={(vm.zone,vm.instance_id) for vm in fleet}
        observed={(row.get('zone'),row.get('instance_id')) for row in resources}
        if (state.get('latest_workload',{}).get('complete') is not True
                or observed!=identities or not retirement_io_safe(migration,resources,state) or not fits(migration['target'],resources)):
            LOG.info('Migration keeps source: fresh complete workload/peak demand does not fit target')
            return True
        migration['phase']='drain'
    if migration['phase'] in ('drain','drain-first'):
        if any(row['instance_id'] in by_id for row in migration['sources']) and not retirement_io_safe(migration,state.get('latest_resources'),state):
            LOG.info('Migration retirement holds: incomplete or pressured IO evidence')
            return True
        for record in migration['sources']:
            vm=by_id.get(record['instance_id'])
            if not vm:continue
            assigned=controller.host_runners(vm,runners,fleet)
            controller.begin_host_operation('drain',vm,runners,vms,state,state_path,
                runner_names={str(row.runner_id):row.name for row in assigned},
                drain_mode='isolated_group',drain_group_name=f'spot-drain-{vm.name}-{vm.instance_id}')
            return True
        if migration['pending_tiers']:
            migration['phase']='launch'
            controller.write_scale_state(state_path,state)
            return True
        state.setdefault('tier_policy',{})['changed_at']=time.time()
        state['desired_topology']=migration['target']
        state['migration_completed_at']=time.time()
        present_ids={vm.instance_id for vm in vms}
        for source in migration['sources']:
            if source['instance_id'] not in present_ids:
                clear_oom(state,f"{source['zone']}/{source['instance_id']}")
        state.pop('migration')
        controller.write_scale_state(state_path,state)
        LOG.info('Tier migration completed: %s',state['desired_topology'])
    return True
