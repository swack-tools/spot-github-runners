import unittest
from lib.fleet_evidence import record_resources, workload_signals
from lib.runner_fleet import GceVm


class EvidenceTests(unittest.TestCase):
    def test_absolute_demand_and_missing_identity_reset_window(self):
        vm = GceVm('oxidex-runners-a','z','RUNNING','n4-standard-32',True,32,128,instance_id='1')
        state = {}
        rows = record_resources(state,[vm],[(.25,.10)],1000)
        self.assertEqual(rows[0]['cpu_cores'],8)
        self.assertAlmostEqual(rows[0]['peak_ram_gib'],12.8)
        self.assertIsNone(record_resources(state,[vm],None,1300))
        self.assertEqual(state['resource_history'],{})
        self.assertIsNone(record_resources(state,[vm],[(float('nan'),.1)],1600))

    def test_changed_vm_id_cannot_reuse_memory_peak(self):
        old = GceVm('oxidex-runners-a','z','RUNNING','n2-standard-4',True,4,16,instance_id='1')
        new = GceVm('oxidex-runners-a','z','RUNNING','n2-standard-4',True,4,16,instance_id='2')
        state = {}
        record_resources(state,[old],[(.25,.9)],1000)
        rows = record_resources(state,[new],[(.25,.1)],1300)
        self.assertEqual(rows[0]['cpu_cores'],1)
        self.assertEqual(rows[0]['peak_ram_gib'],1.6)
        self.assertEqual(rows[0]['windows'],1)

    def test_only_compatible_queues_and_comparable_successes_drive_timing(self):
        jobs = [dict(id=1, status='queued', labels=['self-hosted','Linux','ARM64'], created_at='1970-01-01T00:00:00Z')]
        labels = [{'self-hosted','Linux','X64','spot'}]
        self.assertFalse(workload_signals(jobs,labels,1000)['pending'])
        jobs.append(dict(id=2,status='queued',labels=['self-hosted','Linux','X64','spot'],created_at='1970-01-01T00:00:00Z'))
        result = workload_signals(jobs,labels,1000)
        self.assertTrue(result['pending'])
        self.assertTrue(result['queue_delayed'])
        jobs[-1]['runner_group_id'] = 123
        self.assertFalse(workload_signals(jobs,labels,1000,group_ids={10})['pending'])
