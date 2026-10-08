import unittest
from unittest.mock import patch
from src.lib import runner_manager as manager
from src.lib.runner_assets import STARTUP_SCRIPT
from pathlib import Path

class StrictPoolStartup(unittest.TestCase):
    def args(self,count=2):
        return manager.args_parser().parse_args(['--max-cost','.1','--heavy-runner-pool',str(count),'--cpu','32','--ram','128'])
    def test_render_fixed_members_and_caps(self):
        args=self.args()
        with patch.object(manager,'configure_storage'),patch.object(manager,'configure_cpu_platform'):manager.select_sizing(args)
        script=manager.render_startup(args,'synthetic-token','restricted-oxidex','synthetic-deployment')
        self.assertEqual((args.runner_count,args.runner_cpu,args.runner_memory_gb,args.admission_work_gib),(2,8,32,64))
        self.assertIn("config.update(role='runner',heavy_runners=names)",script)
        self.assertIn('STRICT_HEAVY_POOL=1',script)
        self.assertIn('RUNNER_CPU_LIMIT=\'8\'',script)
        self.assertIn('--memory-swap="32g"',script)
        self.assertIn('spot-heavy32',script)
        self.assertNotIn('{{',script)
    def test_insufficient_host_refused(self):
        args=self.args();args.cpu=16
        with self.assertRaises(RuntimeError):manager.strict_pool_settings(args)
        args.cpu=32;args.ram=64
        with self.assertRaises(RuntimeError):manager.strict_pool_settings(args)
    def test_no_builder_or_tier_override(self):
        args=self.args();args.remote_builder=True
        with self.assertRaises(RuntimeError):manager.strict_pool_settings(args)
        args.remote_builder=False;args.tier='bulky'
        with self.assertRaises(RuntimeError):manager.strict_pool_settings(args)
    def test_actual_group_membership_required(self):
        import json
        args=self.args();args.runner_count=2
        rows=[{'name':f'{args.runner_prefix}-{args.instance_name}-{n}','id':n}for n in (1,2)]
        with patch.object(manager,'run',return_value='\n'.join(json.dumps(r)for r in rows)):manager.verify_strict_runner_membership(args)
        for invalid in ([],rows[:1],[rows[0],dict(rows[0])]):
            with patch.object(manager,'run',return_value='\n'.join(json.dumps(r)for r in invalid)),self.assertRaises(RuntimeError):manager.verify_strict_runner_membership(args)

    def test_canonical_startup_parity(self):
        self.assertEqual(STARTUP_SCRIPT,(Path(manager.ROOT)/'cloud-init/startup.sh').read_text())

if __name__=='__main__':unittest.main()
