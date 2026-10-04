import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from lib import spot_runner_autoscaler as controller
from lib.runner_fleet import GceVm


class TierControllerTests(unittest.TestCase):
    def test_dry_run_cannot_mutate_live_state_or_provider(self):
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/'state.json';path.write_text('{"idle_loops":0}')
            args=controller.args_parser().parse_args(['--scaling-mode','tiered','--dry-run','--once',
                '--state-file',str(path),'--migrate-oversized'])
            with patch.object(controller,'list_gcloud_vms',return_value=[]),patch.object(controller,'list_github_runners',return_value=[]),\
                 patch('lib.job_recovery.JobRecovery._scan',return_value=({},{})),\
                 patch.object(controller,'discover_tier_offers',return_value=[]),\
                 patch.object(controller,'write_scale_state',side_effect=AssertionError('state mutation')),\
                 patch.object(controller,'launch_offer',side_effect=AssertionError('provider mutation')):
                self.assertEqual(controller._run_controller(args),0)
            self.assertEqual(path.read_text(),'{"idle_loops":0}')

    def test_tier_launch_rejects_64_core_recovery_before_any_provider_call(self):
        from lib.machine_profiles import CpuOffer
        args=controller.args_parser().parse_args(['--scaling-mode','tiered'])
        args.launch_tier='large'
        offer=CpuOffer('us-west4','n2-standard-64',64,256,.05,'Intel Ice Lake',7)
        with patch.object(controller.subprocess,'run',side_effect=AssertionError('external side effect')):
            with self.assertRaisesRegex(RuntimeError,'forbidden'):
                controller.launch_offer(offer,args,{},Path('/tmp/never-used-tier-state'))
