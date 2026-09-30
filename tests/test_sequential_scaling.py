"""Offline proofs of the persisted, one-host fleet operation barrier."""
import subprocess
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from lib.runner_fleet import GceVm, GitHubRunner
from lib import spot_runner_autoscaler as controller


class SequentialScalingTests(unittest.TestCase):
    def setUp(self):
        external = patch('subprocess.run', side_effect=AssertionError('Unexpected external command in offline test'))
        external.start()
        self.addCleanup(external.stop)
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / 'state.json'
        self.args = controller.args_parser().parse_args(['--state-file', str(self.path)])
        self.vm = GceVm('oxidex-runners-test', 'asia-south2-a', 'RUNNING', 'n2-standard-32', True, instance_id='123')
        self.other = replace(self.vm, name='oxidex-runners-other', instance_id='456')
        self.runners = [GitHubRunner(i, f'gha-{self.vm.name}-{i}', 'online', False, ('spot', 'X64', 'n2-standard-32')) for i in range(1, 9)]

    def test_high_demand_adds_one_and_sticky_old_state_does_not_override_idle_load(self):
        self.assertEqual(controller.scaling_decision(24, 14, 3, 0, False, 25), ('up', 1, 0, False))
        self.assertEqual(controller.scaling_decision(24, 2, 3, 0, True, 25), ('wait', 0, 1, False))
        self.assertEqual(controller.scaling_decision(48, 10, 6, 1, True, 25), ('down', 1, 0, False))

    def test_partial_registration_and_restart_block_additional_host_actions(self):
        state = {}
        controller.begin_host_operation('launch', self.vm, self.runners, [self.vm], state, self.path)
        restarted = controller.read_scale_state(self.path)
        with patch('lib.spot_runner_autoscaler.subprocess.run') as command:
            self.assertTrue(controller.reconcile_host_operation([self.vm], self.runners[:7], self.args, restarted, self.path))
            self.assertEqual(controller.scale_up(5, ['offer'], self.args, restarted, self.path), 0)
            self.assertEqual(controller.scale_down([self.vm, self.other], self.runners, self.args, restarted, self.path), 0)
            command.assert_not_called()
        self.assertIn('host_operation', restarted)
        self.assertTrue(controller.reconcile_host_operation([self.vm], self.runners, self.args, restarted, self.path))
        self.assertNotIn('host_operation', restarted)

    def test_failed_deregistration_blocks_second_delete_and_retries_only_stale_idle_ids(self):
        state = {}
        controller.begin_host_operation('delete', self.vm, self.runners, [self.vm], state, self.path)
        restarted = controller.read_scale_state(self.path)
        offline = [replace(runner, status='offline') for runner in self.runners]
        with patch('lib.spot_runner_autoscaler.subprocess.run', side_effect=subprocess.CalledProcessError(1, ['gh'])):
            self.assertTrue(controller.reconcile_host_operation([self.other], offline, self.args, restarted, self.path))
        with patch('lib.spot_runner_autoscaler.terminate_spot_vm') as delete:
            self.assertEqual(controller.scale_down([self.other], [], self.args, restarted, self.path), 0)
            delete.assert_not_called()
        with patch('lib.spot_runner_autoscaler.subprocess.run') as command:
            self.assertTrue(controller.reconcile_host_operation([self.other], [replace(offline[0], busy=True)], self.args, restarted, self.path))
            command.assert_not_called()
        with patch('lib.spot_runner_autoscaler.subprocess.run') as command:
            controller.reconcile_host_operation([self.other], offline, self.args, restarted, self.path)
            self.assertEqual(command.call_count, 8)
        self.assertIn('host_operation', restarted)  # DELETE receipts still need an absent-record observation.
        controller.reconcile_host_operation([self.other], [], self.args, restarted, self.path)
        self.assertNotIn('host_operation', restarted)

    def test_failed_launch_with_surviving_vm_never_tries_another_zone(self):
        offer = SimpleNamespace(region='asia-south2', machine_type='n2-standard-32', vcpu=32, ram_gib=128, hourly_usd=.1)
        state = {}
        def command(args, **kwargs):
            if args[:3] == ['gcloud', 'compute', 'zones']:
                return SimpleNamespace(returncode=0, stdout='asia-south2-a\nasia-south2-b\n')
            persisted = controller.read_scale_state(self.path)
            self.assertEqual(persisted['host_operation']['kind'], 'launch')
            return SimpleNamespace(returncode=1, stdout='', stderr='runner registration failed')
        def inventory(*args, **kwargs):
            operation = state['host_operation']
            return [replace(self.vm, name=operation['vm']['name'])]
        with patch('lib.spot_runner_autoscaler.subprocess.run', side_effect=command) as external, \
                patch('lib.spot_runner_autoscaler.list_gcloud_vms', side_effect=inventory):
            self.assertEqual(controller.scale_up(3, [offer, offer], self.args, state, self.path), 0)
            self.assertEqual(external.call_count, 2)
            self.assertIn('host_operation', state)

    def test_price_replacement_deficit_restores_only_one_and_keeps_target(self):
        state = {'price_replacement_target': 3}
        with patch('lib.spot_runner_autoscaler.check_running_prices', return_value=[]), \
                patch('lib.spot_runner_autoscaler.discover_offers', return_value=['offer']), \
                patch('lib.spot_runner_autoscaler.scale_up', return_value=1) as launch:
            self.assertTrue(controller.reconcile_prices([self.vm], [], self.args, state, self.path))
        self.assertEqual(launch.call_args.args[0], 1)
        self.assertEqual(state['price_replacement_target'], 3)

    def test_multiple_overpriced_hosts_delete_only_one_before_any_launch(self):
        state = {}
        with patch('lib.spot_runner_autoscaler.check_running_prices', return_value=[self.vm, self.other]), \
                patch('lib.spot_runner_autoscaler.delete_overpriced_vm') as delete, \
                patch('lib.spot_runner_autoscaler.scale_up') as launch:
            controller.reconcile_prices([self.vm, self.other], self.runners, self.args, state, self.path)
        delete.assert_called_once_with(self.vm, self.args.project)
        launch.assert_not_called()
        self.assertEqual(state['price_replacement_target'], 2)

    def test_registration_captures_instance_identity_and_requires_exact_names(self):
        state = {}
        controller.begin_host_operation('launch', replace(self.vm, instance_id=''), [], [], state, self.path)
        bogus = [replace(runner, name=runner.name.replace('gha-', 'unrelated-')) for runner in self.runners]
        controller.reconcile_host_operation([self.vm], bogus, self.args, state, self.path)
        self.assertEqual(controller.read_scale_state(self.path)['host_operation']['vm']['instance_id'], '123')
        controller.reconcile_host_operation([replace(self.vm, instance_id='999')], self.runners, self.args, state, self.path)
        self.assertIn('host_operation', state)

    def test_uncertain_launch_absence_never_permits_further_launch_after_restart(self):
        state = {}
        controller.begin_host_operation('launch', replace(self.vm, instance_id=''), [], [], state, self.path)
        restarted = controller.read_scale_state(self.path)
        controller.reconcile_host_operation([], [], self.args, restarted, self.path)
        with patch('lib.spot_runner_autoscaler.launch_offer') as launch:
            self.assertEqual(controller.scale_up(1, ['offer'], self.args, restarted, self.path), 0)
            launch.assert_not_called()

    def test_failed_launch_with_orphan_disk_blocks_zone_and_offer_failover(self):
        offer = SimpleNamespace(region='asia-south2', machine_type='n2-standard-32', vcpu=32, ram_gib=128, hourly_usd=.1)
        state = {}
        def command(args, **kwargs):
            if args[:3] == ['gcloud', 'compute', 'zones']:
                return SimpleNamespace(returncode=0, stdout='asia-south2-a\nasia-south2-b\n')
            if args[:3] == ['gcloud', 'compute', 'disks']:
                import json
                name = state['host_operation']['vm']['name'].removeprefix('oxidex-runners-')
                return SimpleNamespace(returncode=0, stdout=json.dumps([{'name': f'runner-data-{name}'}]))
            return SimpleNamespace(returncode=1, stdout='', stderr='capacity unavailable')
        with patch('lib.spot_runner_autoscaler.subprocess.run', side_effect=command) as external, \
                patch('lib.spot_runner_autoscaler.list_gcloud_vms', return_value=[]):
            self.assertEqual(controller.scale_up(3, [offer, offer], self.args, state, self.path), 0)
            self.assertEqual(external.call_count, 3)
            self.assertIn('host_operation', state)

    def test_recovery_operation_metadata_is_persisted_before_cleanup(self):
        from dataclasses import asdict
        from lib.vm_recovery import reconcile_lost_vms
        state = {'pending_vm_recovery': {'lost': asdict(self.vm)}}
        def cleanup(*args):
            saved = controller.read_scale_state(self.path)
            self.assertEqual(saved['host_operation']['recovery_key'], 'lost')
            raise subprocess.TimeoutExpired('gcloud', 180)
        with patch('lib.vm_recovery.cleanup_lost_vm', side_effect=cleanup):
            reconcile_lost_vms([], [], self.args, state, self.path)
        self.assertIn('host_operation', controller.read_scale_state(self.path))


if __name__ == '__main__':
    unittest.main()
