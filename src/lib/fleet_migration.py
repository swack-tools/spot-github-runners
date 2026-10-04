"""Persist one tier replacement; existing identity-guarded drain owns deletion."""
import logging
import time
from collections import Counter
from dataclasses import asdict
from decimal import Decimal
from .runner_tiers import TIERS,tier_for_shape,validate_topology
from .tier_policy import fits

LOG=logging.getLogger('spot_runner_autoscaler')


def expected_slots(vm,state):
    return int(state.get('host_deployments',{}).get(f'{vm.zone}/{vm.instance_id}',{}).get('expected_runner_count',8))


def topology_for(vms):
    names=[tier.name if (tier:=tier_for_shape(vm.vcpu,vm.ram_gib)) else 'legacy' for vm in vms]
    rank={'legacy':0,'bulky':.5,'large':1,'expanded':2,'starting':3,'small':4}
    return tuple(sorted(names,key=rank.__getitem__))


def begin_migration(vms,target,state,state_path,reason,slots=None,force_ids=()):
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
    state['migration']=dict(target=list(target),sources=sources,retained_ids=retained,
        pending_tiers=list(remaining.elements()),replacement_ids=[],phase='launch',reason=reason,
        started_at=time.time(),slots=slots)
    write_scale_state(state_path,state)


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
        migration.pop('awaiting_launch')
        controller.write_scale_state(state_path,state)
    if migration['pending_tiers']:
        # Count provisioning/stopped managed identities too; the hard ceiling
        # cannot be bypassed by an incomplete running-only inventory.
        managed=[vm for vm in vms if vm.spot and vm.name.startswith('oxidex-runners-')]
        tier=TIERS[migration['pending_tiers'][0]]
        slots=migration.get('slots') or tier.initial_slots
        overlap_blocked=(len(managed)>=min(5,args.maximum_hosts)
            or sum(vm.vcpu or 80 for vm in managed)+tier.vcpu>80
            or sum(expected_slots(vm,state) for vm in managed)+slots>100)
        if overlap_blocked:
            remaining=[vm for vm in fleet if vm.instance_id not in {row['instance_id'] for row in migration['sources']}]
            topology=topology_for(remaining)
            if (state.get('latest_workload',{}).get('complete') is not True
                    or not remaining or 'legacy' in topology
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
            if 'spend_ceiling' not in migration:
                migration['spend_ceiling']=str(max(Decimal('15'),current))
                controller.write_scale_state(state_path,state)
            offers=controller.discover_tier_offers(args.project,tier,args.disk_size_gb)
            offers=[offer for offer in offers if current+offer.total_daily_usd<Decimal(migration['spend_ceiling'])]
            slots=migration.get('slots') or tier.initial_slots
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
                refresh=lambda: [row for row in controller.discover_tier_offers(args.project,tier,args.disk_size_gb)
                                  if current+row.total_daily_usd<Decimal(migration['spend_ceiling'])])
            if state.get('host_operation',{}).get('kind')=='launch':
                migration['awaiting_launch']=state['host_operation']['vm']
                controller.write_scale_state(state_path,state)
            return True
    if not migration['pending_tiers'] and migration['phase']!='drain':
        migration['phase']='healthy'
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
                    migration['replacement_ids'].remove(identity)
                    controller.write_scale_state(state_path,state)
                return True
            key=f'{vm.zone}/{vm.instance_id}'
            sample=rows.get(key)
            health=state.get('host_health',{}).get(key,{})
            assigned=controller.host_runners(vm,runners,fleet)
            names={runner.name for runner in assigned if runner.online}
            ready={f'gha-{vm.name}-{i}' for i in range(1,expected_slots(vm,state)+1)}<=names
            good=(ready and sample and sample['cpu_fraction']<.75 and sample['memory_fraction']<.75
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
                or observed!=identities or not fits(migration['target'],resources)):
            LOG.info('Migration keeps source: fresh complete workload/peak demand does not fit target')
            return True
        migration['phase']='drain'
    if migration['phase'] in ('drain','drain-first'):
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
        state.pop('pending_oom',None)
        state.pop('migration')
        controller.write_scale_state(state_path,state)
        LOG.info('Tier migration completed: %s',state['desired_topology'])
    return True
