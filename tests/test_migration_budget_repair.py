import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from lib.fleet_migration import begin_migration, reconcile_migration
from lib.runner_fleet import GceVm, GitHubRunner


class MigrationBudgetRepairTests(unittest.TestCase):
    def fixture(self, path, workload='runner', busy=False):
        old=GceVm('oxidex-runners-old','z','RUNNING','n2-standard-32',True,32,128,instance_id='1')
        new=GceVm('oxidex-runners-new','z','RUNNING','n2-standard-32',True,32,128,instance_id='2')
        state={}
        begin_migration([old],('bulky',),state,path,'replacement',force_ids=['1'])
        state['migration'].update(pending_tiers=[],pending_slots=[],replacement_ids=['2'],phase='healthy')
        state['host_deployments']={'z/2':{'tier':'bulky','expected_runner_count':16}}
        state['latest_resources']=[dict(zone='z',instance_id='2',observed_at=1000,
            cpu_fraction=.1,memory_fraction=.06,peak_cpu_cores=3,peak_ram_gib=7.7)]
        state['host_health']={'z/2':dict(instance_id='2',observed_at=1000,complete=True,
            events=[dict(id='oom1',kind='container',workload=workload,at=999)])}
        state['pending_oom']={'z/2':workload}
        runners=[GitHubRunner(i,f'gha-{new.name}-{i}','online',busy if i==1 else False,()) for i in range(1,17)]
        return old,new,state,runners,SimpleNamespace(remote_builder=False)

    def test_idle_identified_cap_oom_repairs_without_advancing_health(self):
        for kind in ('runner','builder'):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as folder:
                path=Path(folder)/'state.json'
                old,new,state,runners,args=self.fixture(path,kind)
                with patch('lib.fleet_migration.time.time',return_value=1000), \
                     patch('lib.spot_runner_autoscaler.adjust_admission',return_value=True) as adjust, \
                     patch('lib.spot_runner_autoscaler.begin_host_operation') as drain:
                    reconcile_migration([old,new],runners,args,state,path)
                    self.assertIsNotNone(adjust.call_args,'identified idle replacement OOM must reach budget repair')
                    self.assertEqual(adjust.call_args.args,(new,args,kind))
                    self.assertEqual(state['migration']['healthy_checks']['z/2']['count'],0)
                    self.assertNotIn('z/2',state['pending_oom'])
                    drain.assert_not_called()
                    reconcile_migration([old,new],runners,args,state,path)
                    self.assertEqual(adjust.call_count,1,'same OOM must not repeatedly double the cap')

    def test_busy_or_incomplete_or_unattributed_windows_cannot_change_caps(self):
        variants=('busy','missing_identity','stale','high_memory','incomplete','unattributed','host_oom')
        for variant in variants:
            with self.subTest(variant=variant),tempfile.TemporaryDirectory() as folder:
                path=Path(folder)/'state.json'
                old,new,state,runners,args=self.fixture(path,busy=variant=='busy')
                health=state['host_health']['z/2'];sample=state['latest_resources'][0]
                if variant=='missing_identity':health['instance_id']='another'
                if variant=='stale':sample['observed_at']=500
                if variant=='high_memory':sample['memory_fraction']=.8
                if variant=='incomplete':health['complete']=False
                if variant=='unattributed':health['events'][0]['workload']='unknown'
                if variant=='host_oom':health['events'].append(dict(id='global',kind='host',workload='unknown',at=999))
                with patch('lib.fleet_migration.time.time',return_value=1000), \
                     patch('lib.spot_runner_autoscaler.adjust_admission') as adjust:
                    reconcile_migration([old,new],runners,args,state,path)
                    adjust.assert_not_called()

    def test_failed_idle_adjustment_retries_without_clearing_oom(self):
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'state.json'
            old,new,state,runners,args=self.fixture(path)
            with patch('lib.fleet_migration.time.time',return_value=1000), \
                 patch('lib.spot_runner_autoscaler.adjust_admission',return_value=False) as adjust:
                reconcile_migration([old,new],runners,args,state,path)
                reconcile_migration([old,new],runners,args,state,path)
                self.assertEqual(adjust.call_count,2)
                self.assertEqual(state['pending_oom']['z/2'],'runner')

    def test_repaired_replacement_still_needs_two_new_event_free_windows(self):
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'state.json'
            old,new,state,runners,args=self.fixture(path)
            with patch('lib.fleet_migration.time.time',return_value=1000), \
                 patch('lib.spot_runner_autoscaler.adjust_admission',return_value=True):
                reconcile_migration([old,new],runners,args,state,path)
            state['latest_workload']={'complete':True}
            state['latest_resources'].append(dict(zone='z',instance_id='1',observed_at=3000,
                cpu_fraction=.1,memory_fraction=.06,peak_cpu_cores=3,peak_ram_gib=7.7))
            for tick in (3000,3300):
                for sample in state['latest_resources']:sample['observed_at']=tick
                state['host_health']['z/2'].update(observed_at=tick,events=[])
                with patch('lib.fleet_migration.time.time',return_value=tick), \
                     patch('lib.spot_runner_autoscaler.begin_host_operation') as drain:
                    reconcile_migration([old,new],runners,args,state,path)
                    self.assertEqual(state['migration']['healthy_checks']['z/2']['count'],1 if tick==3000 else 2)
                    self.assertEqual(drain.call_count,0 if tick==3000 else 1)
