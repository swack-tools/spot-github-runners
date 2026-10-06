import json
import tempfile
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
from lib import runner_fleet, spot_runner_autoscaler as controller, tier_controller, fleet_migration

class HeavyRouteContinuityTests(unittest.TestCase):
    def setUp(self):
        self.vm=runner_fleet.GceVm('oxidex-runners-host','z','RUNNING','n4-standard-16',True,16,64,instance_id='721')
        self.runner=runner_fleet.GitHubRunner(1,'gha-oxidex-runners-host-1','online',False,('spot-heavy32',))
    def test_live_route_is_remembered_when_label_disappears(self):
        state={}
        self.assertEqual(controller.protected_heavy_routes([self.vm],[self.runner],state),[self.vm])
        self.assertEqual(controller.protected_heavy_routes([self.vm],[],state),[self.vm])
        lost=runner_fleet.GceVm(self.vm.name,'z','TERMINATED','n4-standard-16',True,16,64,instance_id='721')
        self.assertEqual(controller.protected_heavy_routes([lost],[],state),[])
    def test_root_planned_protection_holds_before_first_label_observation(self):
        state={'protected_heavy_routes':{'721':{'name':self.vm.name,'zone':'z'}}}
        self.assertEqual(controller.protected_heavy_routes([self.vm],[],state),[self.vm])
    def test_unrelated_runner_does_not_protect_host(self):
        other=runner_fleet.GitHubRunner(2,'unrelated','online',False,('spot-heavy32',))
        state={}
        self.assertEqual(controller.protected_heavy_routes([self.vm],[other],state),[])
        self.assertNotIn('protected_heavy_routes',state)
    def test_delete_is_held_before_any_deployment_or_drain_effect(self):
        state={'host_operation':{'kind':'delete','vm':{'instance_id':'721'}}}
        with patch.object(controller,'import_deployment_receipt') as effect, patch.object(controller,'write_scale_state'):
            self.assertTrue(controller.reconcile_host_operation([self.vm],[self.runner],None,state,None))
            effect.assert_not_called()
    def test_migration_retirement_retries_are_protected(self):
        operation={'vm':{'instance_id':'721'}}
        for guard in (fleet_migration.retirement_safe,fleet_migration.staged_retirement_safe):
            self.assertFalse(guard([self.vm],[self.runner],None,{},None,operation))
    def test_tier_hold_preserves_pending_migration_and_collects_metrics(self):
        state={'migration':{'started_at':123,'target':['expanded']}}
        with tempfile.TemporaryDirectory() as root, patch.object(tier_controller,'evidence_for',return_value=({}, {}, time.time())) as observed, patch.object(tier_controller,'reconcile_migration') as mutate:
            tier_controller.check_tiers([self.vm],[self.runner],None,state,Path(root)/'state.json')
            observed.assert_called_once()
            mutate.assert_not_called()
            self.assertEqual(state['migration'],{'started_at':123,'target':['expanded']})
    def test_stale_and_future_inventory_are_not_absence_permission(self):
        for value in (time.time()-121,time.time()+10,'invalid'):
            state={'runner_inventory_observed_at':value,'host_operation':{'kind':'delete','vm':{'instance_id':'721'}}}
            with patch.object(controller,'import_deployment_receipt') as effect:
                self.assertTrue(controller.reconcile_host_operation([self.vm],[],None,state,None))
                effect.assert_not_called()
    def test_resource_and_legacy_modes_do_not_bypass_route_hold(self):
        from contextlib import ExitStack
        for mode in ('resource','legacy'):
            args=controller.args_parser().parse_args(['--once','--scaling-mode',mode])
            state={}
            with ExitStack() as stack:
                for name,value in (('read_scale_state',state),('load_cpu_platform_map',{}),
                                   ('list_github_runners',[self.runner]),('list_gcloud_vms',[self.vm]),
                                   ('reconcile_host_operation',False),('reconcile_lost_vms',False)):
                    stack.enter_context(patch.object(controller,name,return_value=value))
                for name in ('write_scale_state','observe_vms','reconcile_deletion_identities','log_hourly_spend'):
                    stack.enter_context(patch.object(controller,name))
                stack.enter_context(patch.object(controller,'JobRecovery'))
                price=stack.enter_context(patch.object(controller,'reconcile_prices'))
                observed=stack.enter_context(patch.object(tier_controller,'evidence_for',return_value=({}, {}, time.time())))
                self.assertEqual(controller._run_controller(args),0)
                price.assert_not_called();observed.assert_called_once()
    def test_inventory_requires_total_pages_labels_and_no_diagnostics(self):
        item={'id':1,'name':'r','status':'online','busy':False,'labels':[{'name':'spot-heavy32'}]}
        valid=[{'total_count':1,'runners':[item]}]
        cases=[([1],''),([{'total_count':2,'runners':[item]}],''),([{'total_count':1,'runners':[dict(item,labels=None)]}],''),(valid,'partial warning')]
        for pages,stderr in cases:
            with patch.object(runner_fleet.subprocess,'run',return_value=SimpleNamespace(stdout=json.dumps(pages),stderr=stderr)):
                with self.assertRaises(RuntimeError):runner_fleet.list_github_runners()
        with patch.object(runner_fleet.subprocess,'run',return_value=SimpleNamespace(stdout=json.dumps(valid),stderr='')):
            self.assertEqual(runner_fleet.list_github_runners()[0].labels,('spot-heavy32',))
    def test_provider_partial_diagnostic_holds_and_strict_flag_is_set(self):
        with patch.object(runner_fleet.subprocess,'run',return_value=SimpleNamespace(stdout='[]',stderr='partial zones')) as cli:
            with self.assertRaises(RuntimeError):runner_fleet._command_json(['gcloud','compute','instances','list'])
            self.assertEqual(cli.call_args.kwargs['env']['CLOUDSDK_COMPUTE_ALLOW_PARTIAL_ERROR'],'false')
    def test_malformed_protection_and_unknown_or_reused_host_cannot_retire(self):
        self.assertEqual(controller.protected_heavy_routes([self.vm],[],{'protected_heavy_routes':None}),[self.vm])
        other=runner_fleet.GceVm(self.vm.name,'z','UNKNOWN','n4-standard-16',True,instance_id=None)
        state={'protected_heavy_routes':{'721':{'name':self.vm.name,'zone':'z'}}}
        self.assertEqual(controller.protected_heavy_routes([other],[],state),[other])
    def test_duplicate_and_changed_total_pages_hold(self):
        item={'id':1,'name':'r','status':'online','busy':False,'labels':[]}
        pages=[{'total_count':2,'runners':[item,item]}]
        with patch.object(runner_fleet.subprocess,'run',return_value=SimpleNamespace(stdout=json.dumps(pages),stderr='')):
            with self.assertRaises(RuntimeError):runner_fleet.list_github_runners()

if __name__=='__main__':unittest.main()
