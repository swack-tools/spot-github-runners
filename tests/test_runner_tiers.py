import unittest
from lib.runner_tiers import TIERS, next_topology, tier_for_shape, validate_topology


class TierTopologyTests(unittest.TestCase):
    def test_growth_and_reverse_stop_at_five_large_hosts(self):
        stages = [('large',), ('large', 'small'), ('large', 'expanded'),
                  ('large', 'large'), ('large', 'large', 'small')]
        for before, after in zip(stages, stages[1:]):
            self.assertEqual(next_topology(before, 'up', 'cpu'), after)
            self.assertEqual(next_topology(after, 'down', 'low'), before)
        self.assertEqual(next_topology(('large',)*5, 'up', 'cpu'), ('large',)*5)
        self.assertEqual(next_topology(('large',)*5, 'down', 'low'), ('large',)*4+('expanded',))
        with self.assertRaises(ValueError):
            validate_topology(('large',)*6)

    def test_first_host_and_memory_upgrade_preserve_cpu_shape(self):
        self.assertEqual(next_topology((), 'up', 'empty'), ('starting',))
        self.assertEqual(next_topology(('starting',), 'up', 'memory'), ('expanded',))
        self.assertEqual(next_topology(('starting',), 'up', 'cpu'), ('large',))
        self.assertEqual(next_topology(('expanded',), 'down', 'low'), ('starting',))
        self.assertEqual(next_topology(('starting',), 'down', 'low'), ('small',))
        self.assertEqual(next_topology(('small',), 'down', 'low'), ('small',))

    def test_forbidden_shape_cannot_receive_tier_or_price_authorization(self):
        self.assertIsNone(tier_for_shape(32,128))
        for name, limit in [('small','.60'), ('starting','1.25'), ('expanded','1.25'), ('large','3')]:
            self.assertFalse(TIERS[name].affordable(limit))
            self.assertFalse(TIERS[name].affordable('NaN'))
        self.assertTrue(TIERS['expanded'].affordable('1.24'))
