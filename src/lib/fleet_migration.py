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
from .storage_selection import observed_data_storage
from .oom_obligations import obligations_for,pending_kinds,remember_oom,clear_oom

LOG=logging.getLogger('spot_runner_autoscaler')


def data_storage_status(vm,args):
    device=getattr(args,'disk_device_name',None) or 'runner-data-'+vm.name.removeprefix('oxidex-runners-')
    return observed_data_storage(vm.disks,device,getattr(args,'disk_size_gb',100))


def expected_slots(vm,state):
    return int(state.get('host_deployments',{}).get(f'{vm.zone}/{vm.instance_id}',{}).get('expected_runner_count',8))


def topology_for(vms):
    names=[tier.name if (tier:=tier_for_shape(vm.vcpu,vm.ram_gib)) else 'legacy' for vm in vms]
    rank={'legacy':0,'bulky':.5,'large':1,'expanded':2,'starting':3,'small':4}
    return tuple(sorted(names,key=rank.__getitem__))


def begin_migration(vms,target,state,state_path,reason,slots=None,force_ids=(),slot_counts=(),io_pressure_sources=(),staged_bad_id=None):
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
    if staged_bad_id is not None:
        if (reason!='storage-pressure canary replacement' or list(target)!=['large','large']
                or len(sources)!=2 or staged_bad_id not in {row['instance_id'] for row in sources}):
            raise ValueError('Staging requires exact two-source storage replacement')
        state['migration'].update(storage_staged_bad_id=staged_bad_id,spend_ceiling='15')
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
                    or pending_kinds(state,key) & {'host','unknown','container'}):
                return False
        except (KeyError,TypeError,ValueError):
            return False
    destinations=[row for row in resources or [] if row.get('instance_id') not in allowed]
    # Source stalls are the explicit reason for replacement. They cannot veto
    # retirement once every destination has healthy IO; missing source evidence
    # still holds because measured demand and pressure must remain complete.
    return io_status(resources)!='incomplete' and io_status(destinations)=='healthy'


def retarget_unhealthy_storage_canary(vms,args,state,state_path):
    """Safely replace the exact stalled two-32-vCPU canary with one or two measured-fit 16/64 hosts."""
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
                or not fits(('large','large'),resources)):
            return False
        for vm in managed:
            key=f'{vm.zone}/{vm.instance_id}';health=state.get('host_health',{}).get(key,{})
            if (health.get('instance_id')!=vm.instance_id or health.get('complete') is not True
                    or not health.get('boot_id') or not 0<=timestamp-health['observed_at']<=450
                    or health.get('boot_oom_complete') is not True or health.get('boot_host_oom_ids')!=[]
                    or any(event.get('kind') in ('host','unknown') for event in health.get('events',[]))
                    or pending_kinds(state,key) & {'host','unknown','container'}):
                return False
        bad=next(vm for vm in managed if vm.instance_id==bad_id);key=f'{bad.zone}/{bad.instance_id}'
        history=state.get('io_history',{}).get(key,{})
        windows=history.get('windows',[])
        storage=data_storage_status(bad,args)
        sustained=(history.get('boot_id')==state['host_health'][key]['boot_id'] and len(windows)>=2
                   and 300<=windows[-1]['observed_at']-windows[-2]['observed_at']<=450
                   and 0<=timestamp-windows[-1]['observed_at']<=450
                   and all(io_status([dict(io_complete=True,io_pressure_peaks=row)])=='pressured' for row in windows[-2:]))
        if storage=='incomplete' or not (storage=='incompatible' or sustained):return False
    except (KeyError,TypeError,ValueError,OverflowError):
        return False
    candidate=copy.deepcopy(state)
    candidate['migration_history']=(candidate.get('migration_history',[])+[
        dict(retargeted_at=timestamp,reason='sustained storage pressure',migration=copy.deepcopy(migration))])[-3:]
    candidate.pop('migration')
    target=('large',) if fits(('large',),resources) else ('large','large')
    begin_migration(managed,target,candidate,state_path,'storage-pressure canary replacement',
                    slots=TIERS['large'].initial_slots,io_pressure_sources=identities,
                    staged_bad_id=bad_id if len(target)==2 else None)
    # Existing job trackers hold references into other state subtrees.
    state['migration']=candidate['migration']
    state['migration_history']=candidate['migration_history']
    LOG.info('Retargeted exact storage canary: preserve 32-vCPU sources until %s validates',target)
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
    if migration.get('storage_staged_bad_id'):
        if not migration['pending_tiers'] and len(set(migration['replacement_ids']))!=2:
            LOG.error('Staged migration holds: final replacement cardinality is incomplete')
            return True
        for identity in list(migration['replacement_ids']):
            if identity in by_id:continue
            receipt=next((row for key,row in state.get('host_deployments',{}).items() if key.endswith('/'+identity)),{})
            if receipt.get('tier')!='large':return True
            migration['replacement_ids'].remove(identity)
            migration['pending_tiers'].append('large')
            migration.setdefault('pending_slots',[]).append(receipt.get('expected_runner_count') or TIERS['large'].initial_slots)
            controller.write_scale_state(state_path,state)
        paused=migration.get('paused_retirement')
        if paused:
            source=by_id.get(paused['vm']['instance_id'])
            if source is None or staged_retirement_safe(vms,runners,args,state,state_path,paused):
                state['host_operation']=copy.deepcopy(paused)
                migration.pop('paused_retirement')
                controller.write_scale_state(state_path,state)
                return True
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
            if migration.get('storage_staged_bad_id'):
                bad_id=migration['storage_staged_bad_id']
                if not staged_retirement_safe(vms,runners,args,state,state_path,
                        dict(vm=next(row for row in migration['sources'] if row['instance_id']==bad_id),
                             staged_retirement=dict(started_at=migration['started_at'],stage='early'))):
                    LOG.info('Storage staging holds: first canary or surviving capacity is not proven')
                    return True
                migration['early_retirement_ids']=[bad_id]
            else:
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
            if migration.get('storage_staged_bad_id'):
                migration['spend_ceiling']='15'
            previous_ceiling=Decimal(migration.get('spend_ceiling','15'))
            if ('spend_ceiling' not in migration or
                    ('overlap_baseline_daily_usd' not in migration and current>=previous_ceiling>=Decimal('15'))):
                allowance=Decimal(0)
                if current>=Decimal('15') and not migration.get('storage_staged_bad_id'):
                    retiring=sum((totals.get(row['name'],Decimal(0)) for row in migration['sources']),Decimal(0))
                    allowance=min(retiring,sum((TIERS[name].max_daily_usd for name in migration['pending_tiers']),Decimal(0)))
                    migration['overlap_baseline_daily_usd']=str(current)
                    migration['overlap_allowance_daily_usd']=str(allowance)
                migration['spend_ceiling']='15' if migration.get('storage_staged_bad_id') else str(max(Decimal('15'),current+allowance))
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
        if not replacements_healthy(fleet,runners,args,state,state_path):
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
            if (migration.get('storage_staged_bad_id') and migration['pending_tiers']
                    and record['instance_id'] not in migration.get('early_retirement_ids',[])):continue
            vm=by_id.get(record['instance_id'])
            if not vm:continue
            assigned=controller.host_runners(vm,runners,fleet)
            metadata={}
            if migration.get('storage_staged_bad_id'):
                stage='early' if migration['pending_tiers'] else 'final'
                metadata['staged_retirement']=dict(started_at=migration['started_at'],stage=stage,
                    survivor_ids=[item.instance_id for item in fleet if item.instance_id!=vm.instance_id])
                proposal=dict(vm=record,staged_retirement=metadata['staged_retirement'])
                if not staged_retirement_safe(vms,runners,args,state,state_path,proposal):return True
                vm=vm_from_record(record)
            controller.begin_host_operation('drain',vm,runners,vms,state,state_path,**metadata,
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

def replacements_healthy(fleet,runners,args,state,state_path):
    from . import spot_runner_autoscaler as controller
    migration=state['migration']
    by_id={vm.instance_id:vm for vm in fleet}
    rows={f"{row['zone']}/{row['instance_id']}":row for row in state.get('latest_resources') or []}
    healthy=bool(migration['replacement_ids'])
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
            return False
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
        good=(ready and sample and sample.get('instance_id')==vm.instance_id
              and health.get('instance_id')==vm.instance_id and health.get('boot_id')
              and 0<=observed-sample.get('observed_at',0)<=450
              and health.get('boot_oom_complete') is True and health.get('boot_host_oom_ids')==[]
              and not pending_kinds(state,key)
              and sample['cpu_fraction']<.75 and sample['memory_fraction']<.75
              and io_status([sample])=='healthy'
              and data_storage_status(vm,args)=='healthy'
              and health.get('complete') and not health.get('events')
              and 0<=observed-health.get('observed_at',0)<=450)
        if good and getattr(args,'remote_builder',False):
            good=controller.builder_ready(vm,args)
        tick=sample['observed_at'] if sample else 0
        previous=checks.get(key,{})
        if previous.get('boot_id')!=health.get('boot_id'):previous={}
        count=previous.get('count',0)
        if not good:count=0
        elif tick-previous.get('at',-1e20)>=300:
            count=count+1 if tick-previous.get('at',tick)<=450 else 1
            checks[key]=dict(count=count,at=tick,boot_id=health.get('boot_id'))
        if not good:checks[key]=dict(count=0,at=tick,boot_id=health.get('boot_id'))
        healthy=healthy and count>=2
    controller.write_scale_state(state_path,state)
    return healthy


def staged_retirement_safe(vms,runners,args,state,state_path,operation):
    """Prove current survivors can carry all demand before scoped source retirement."""
    from . import spot_runner_autoscaler as controller
    migration=state.get('migration',{})
    scope=operation.get('staged_retirement',{})
    record=operation.get('vm',{})
    bad_id=migration.get('storage_staged_bad_id')
    sources=migration.get('sources',[])
    if (not bad_id or migration.get('reason')!='storage-pressure canary replacement'
            or migration.get('target')!=['large','large'] or len(sources)!=2
            or scope.get('started_at')!=migration.get('started_at')
            or scope.get('stage') not in ('early','final')
            or not any(row==record for row in sources)
            or (scope['stage']=='early' and record.get('instance_id')!=bad_id)
            or (scope['stage']=='final' and record.get('instance_id')==bad_id)):
        return False
    managed=[vm for vm in vms if vm.spot and vm.name.startswith('oxidex-runners-')]
    by_id={vm.instance_id:vm for vm in managed}
    if (len(managed)>min(5,args.maximum_hosts) or sum(vm.vcpu or 80 for vm in managed)>80
            or sum(expected_slots(vm,state) for vm in managed)>100 or any(not vm.running for vm in managed)):
        return False
    for saved in sources:
        current=by_id.get(saved['instance_id'])
        reused=next((vm for vm in managed if vm.name==saved['name'] and vm.zone==saved['zone']),None)
        if reused and reused.instance_id!=saved['instance_id']:return False
        if current and (current.name!=saved['name'] or current.zone!=saved['zone']
                        or list(current.disks)!=list(saved.get('disks',[]))
                        or not current.disks or any(not disk.get('diskId') for disk in current.disks)):
            return False
    if record['instance_id'] not in by_id:return False
    replacement_ids=migration.get('replacement_ids',[])
    if len(set(replacement_ids))!=len(replacement_ids):return False
    if (scope['stage']=='final' or any(row['instance_id'] not in by_id for row in sources)):
        if len(replacement_ids)!=2 or migration.get('pending_tiers'):return False
    if any(not row.get('_cleanup_complete') for row in state.get('pending_vm_recovery',{}).values()):return False
    survivors=[vm for vm in managed if vm.instance_id!=record['instance_id']]
    topology=topology_for(survivors)
    resources=state.get('latest_resources') or []
    timestamp=time.time()
    try:
        if (not survivors or 'legacy' in topology
                or state.get('latest_workload',{}).get('complete') is not True
                or {(row['zone'],row['instance_id']) for row in resources}!={(vm.zone,vm.instance_id) for vm in managed}
                or len(resources)!=len(managed)
                or any(row.get('windows',0)<1 or not 0<=timestamp-row['observed_at']<=450
                       or any(not math.isfinite(row[name]) or row[name]<0 for name in ('peak_cpu_cores','peak_ram_gib'))
                       for row in resources)
                or not reservation_fits(topology,getattr(args,'tier_job_memory_gib',15))
                or not fits(topology,resources) or not retirement_io_safe(migration,resources,state)
                or io_status([row for row in resources if row['instance_id']!=record['instance_id']])!='healthy'):
            return False
        if scope['stage']=='final' and (bad_id in by_id or migration.get('pending_tiers')
                                       or not fits(migration['target'],resources)):
            return False
        for identity in migration['replacement_ids']:
            vm=by_id.get(identity)
            if not vm:return False
            receipt=state.get('host_deployments',{}).get(f'{vm.zone}/{identity}',{})
            if ((vm.vcpu,vm.ram_gib)!=(16,64) or receipt.get('tier')!='large'
                    or receipt.get('runner_memory_gib',0)<15):return False
        if not migration['replacement_ids']:return False
        return replacements_healthy(managed,runners,args,state,state_path)
    except (KeyError,TypeError,ValueError,OverflowError):
        return False


def guard_staged_retirement(vms,runners,args,state,state_path):
    """Refresh proof at the lifecycle barrier, pausing only for lost survivors."""
    from . import spot_runner_autoscaler as controller
    operation=state.get('host_operation',{})
    migration=state.get('migration',{})
    if not operation.get('staged_retirement'):
        return not (migration.get('storage_staged_bad_id') and
                    operation.get('vm',{}).get('instance_id') in {row['instance_id'] for row in migration.get('sources',[])})
    if not migration.get('storage_staged_bad_id'):return False
    try:
        current_vms=controller.list_gcloud_vms(args.project,name_prefix='oxidex-runners-')
        fresh=controller.list_github_runners(args.org)
        from .tier_controller import evidence_for
        evidence_for(current_vms,fresh,args,state)
        if staged_retirement_safe(current_vms,fresh,args,state,state_path,operation):return True
    except (RuntimeError,OSError,ValueError,KeyError,subprocess.SubprocessError) as exc:
        LOG.warning('Staged retirement proof unavailable: %s',controller.error_detail(exc))
        return False
    # A real survivor loss must not monopolize the scheduling barrier and
    # prevent normal identity-scoped loss cleanup / replacement. Persist the
    # original isolation/drain receipt before releasing only its active slot.
    scope=operation['staged_retirement']
    alive={vm.instance_id for vm in current_vms if vm.running}
    source_id=operation['vm']['instance_id']
    if (scope.get('started_at')==migration.get('started_at') and source_id in alive
            and set(scope.get('survivor_ids',[]))-alive):
        saved=migration.get('paused_retirement')
        if saved and saved!=operation:return False
        migration['paused_retirement']=copy.deepcopy(operation)
        controller.write_scale_state(state_path,state)
        state.pop('host_operation')
        controller.write_scale_state(state_path,state)
        LOG.warning('Paused exact staged retirement while missing survivors recover: %s',source_id)
    return False
