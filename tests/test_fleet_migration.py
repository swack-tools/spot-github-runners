import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
from dataclasses import asdict
from lib.runner_fleet import GceVm,GitHubRunner
from lib.fleet_migration import begin_migration, reconcile_migration, expected_slots


def vm(name,cpu,ram,id):
    return GceVm('oxidex-runners-'+name,'z','RUNNING',f'n2-standard-{cpu}',True,cpu,ram,instance_id=id)


class MigrationTests(unittest.TestCase):
    def test_completion_retains_unresolved_ooms_on_retained_or_unrelated_hosts(self):
        import json
        old,new=vm('old',32,128,'1'),vm('retained',16,64,'2')
        for kind in ('runner','builder','host'):
            with self.subTest(kind=kind),tempfile.TemporaryDirectory() as folder:
                path=Path(folder)/'state.json';state={}
                begin_migration([old,new],('large',),state,path,'shrink')
                state['migration'].update(pending_tiers=[],phase='drain')
                state['pending_oom']={'z/1':'runner','z/2':kind,'other/99':'builder'}
                state['seen_oom_ids']=['unresolved-on-retained']
                reconcile_migration([new],[],SimpleNamespace(),state,path)
                saved=json.loads(path.read_text())
                self.assertEqual(saved.get('pending_oom'),{'z/2':kind,'other/99':'builder'})
                self.assertNotIn('migration',saved)
                self.assertEqual(saved['seen_oom_ids'],['unresolved-on-retained'])

    def test_invalid_tier_receipt_cannot_be_marked_imported(self):
        import json
        from lib.spot_runner_autoscaler import import_deployment_receipt
        host=vm('a',16,64,'1')
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'state.json';receipt=Path(folder)/'receipt.json'
            receipt.write_text(json.dumps({**asdict(host),'tier':'large','expected_runner_count':100}))
            state={'host_operation':{'kind':'launch','vm':asdict(host),'deployment_receipt':str(receipt)}}
            with self.assertRaises(RuntimeError):import_deployment_receipt(state,path)
            self.assertFalse(state['host_operation'].get('receipt_imported',False))

    def test_changed_attached_disk_blocks_tier_delete(self):
        from lib.spot_runner_autoscaler import delete_drained_vm
        from dataclasses import replace
        import json
        host=replace(vm('a',8,16,'1'),disks=({'source':'old','diskId':'10'},))
        with patch('lib.spot_runner_autoscaler.subprocess.run',return_value=SimpleNamespace(stdout=json.dumps({'id':'1','disks':[{'source':'new'}]}))) as provider:
            with self.assertRaisesRegex(RuntimeError,'disk'):
                delete_drained_vm(host,SimpleNamespace(project='p',scaling_mode='tiered'))
        self.assertEqual(provider.call_count,1)

    def test_dynamic_readiness_falls_back_only_for_legacy_receipts(self):
        host=vm('a',16,64,'1')
        self.assertEqual(expected_slots(host,{}),8)
        self.assertEqual(expected_slots(host,{'host_deployments':{'z/1':{'expected_runner_count':20}}}),20)

    def test_persisted_source_waits_for_two_replacement_health_windows(self):
        old,new=vm('old',32,128,'1'),vm('new',8,16,'2')
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'state.json'; state={}
            begin_migration([old],('starting',),state,path,'migration')
            state['migration'].update(pending_tiers=[],replacement_ids=['2'],phase='healthy')
            state['host_deployments']={'z/2':{'expected_runner_count':8}}
            runners=[GitHubRunner(i,f'gha-{new.name}-{i}','online',False,('spot','X64')) for i in range(1,9)]
            args=SimpleNamespace(org='org',remote_builder=False,maximum_hosts=5,scaling_mode='tiered')
            for tick in (1000,1300):
                state['latest_resources']=[dict(instance_id=identity,zone='z',observed_at=tick,cpu_fraction=.1,memory_fraction=.2,peak_cpu_cores=.1,peak_ram_gib=.5,
                    io_complete=True,io_pressure_peaks=dict(full_avg10=0,full_avg60=0,some_avg10=0,some_avg60=0)) for identity in ('1','2')]
                state['latest_workload']={'complete':True}
                state['host_health']={'z/2':{'complete':True,'observed_at':tick,'events':[]}}
                with patch('lib.fleet_migration.time.time',return_value=tick), patch('lib.spot_runner_autoscaler.begin_host_operation') as drain:
                    reconcile_migration([old,new],runners,args,state,path)
                    self.assertEqual(drain.call_count,0 if tick==1000 else 1)
                    if tick==1300:self.assertEqual(drain.call_args.args[1].instance_id,'1')

    def test_identity_change_and_five_host_limit_cannot_authorize_delete_or_launch(self):
        old=vm('old',32,128,'1')
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'state.json';state={}
            begin_migration([old],('starting',),state,path,'migration')
            args=SimpleNamespace(maximum_hosts=5)
            with patch('lib.spot_runner_autoscaler.scale_up') as launch:
                reconcile_migration([vm('old',32,128,'changed')],[],args,state,path)
                launch.assert_not_called()
            with patch('lib.spot_runner_autoscaler.scale_up') as launch:
                reconcile_migration([old]+[vm(str(i),16,64,str(i+2)) for i in range(4)],[],args,state,path)
                launch.assert_not_called()

    def test_launch_intent_is_linked_before_provider_submission(self):
        from lib.spot_runner_autoscaler import begin_host_operation
        host=vm('new',8,16,'')
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'state.json'
            state={'migration':{'pending_tiers':['starting']}}
            begin_host_operation('launch',host,[],[],state,path)
            self.assertEqual(state['migration']['awaiting_launch']['name'],host.name)

    def test_awaiting_launch_reused_identity_holds_without_accepting_replacement(self):
        old,new=vm('old',16,64,'1'),vm('new',8,16,'different')
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'state.json';state={}
            begin_migration([old],('starting',),state,path,'migration')
            state['migration']['awaiting_launch']={**asdict(new),'instance_id':'recorded'}
            with patch('lib.spot_runner_autoscaler.scale_up',side_effect=AssertionError('launch')):
                reconcile_migration([old,new],[],SimpleNamespace(maximum_hosts=5),state,path)
            self.assertEqual(state['migration']['pending_tiers'],['starting'])
            self.assertEqual(state['migration']['replacement_ids'],[])

    def test_failed_tier_canary_switches_to_drain_instead_of_force_delete(self):
        from lib.spot_runner_autoscaler import abandon_launch
        host=vm('new',8,16,'1')
        state={'host_operation':{'kind':'launch','vm':asdict(host),'abandon':'partial registration'}}
        with tempfile.TemporaryDirectory() as folder:
            with patch('lib.spot_runner_autoscaler.list_gcloud_vms',return_value=[host]),\
                 patch('lib.spot_runner_autoscaler.list_github_runners',return_value=[]),\
                 patch('lib.spot_runner_autoscaler.subprocess.run',side_effect=AssertionError('delete')):
                self.assertFalse(abandon_launch(SimpleNamespace(project='p',org='o',scaling_mode='tiered'),state,Path(folder)/'state.json'))
        self.assertEqual(state['host_operation']['kind'],'drain')

    def test_failed_tier_launch_cannot_adopt_a_reused_identity(self):
        from lib.spot_runner_autoscaler import abandon_launch
        host=vm('new',8,16,'1');changed=vm('new',8,16,'2')
        state={'host_operation':{'kind':'launch','vm':asdict(host),'abandon':'failed'}}
        with tempfile.TemporaryDirectory() as folder:
            with patch('lib.spot_runner_autoscaler.list_gcloud_vms',return_value=[changed]),\
                 patch('lib.spot_runner_autoscaler.subprocess.run',side_effect=AssertionError('delete')):
                self.assertFalse(abandon_launch(SimpleNamespace(project='p',org='o',scaling_mode='tiered'),state,Path(folder)/'state.json'))
        self.assertEqual(state['host_operation']['vm']['instance_id'],'1')

    def test_cpu_overlap_boundary_uses_safe_drain_first_below_five_hosts(self):
        hosts=[vm('a',32,128,'1'),vm('b',32,128,'2'),vm('c',16,64,'3')]
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'state.json';state={}
            begin_migration(hosts,('bulky','bulky','expanded'),state,path,'shrink')
            state['latest_workload']={'complete':True}
            state['latest_resources']=[dict(peak_cpu_cores=1,peak_ram_gib=2,
                io_complete=True,io_pressure_peaks=dict(full_avg10=0,full_avg60=0,some_avg10=0,some_avg60=0))]*3
            with patch('lib.spot_runner_autoscaler.begin_host_operation') as drain,\
                 patch('lib.spot_runner_autoscaler.check_running_prices',side_effect=AssertionError('launch pricing')):
                reconcile_migration(hosts,[],SimpleNamespace(maximum_hosts=5),state,path)
                self.assertEqual(drain.call_args.args[1].instance_id,'3')
