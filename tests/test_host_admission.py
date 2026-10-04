import tempfile
import unittest
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor
from lib.host_admission import LeasePool, admission_budget
from lib.runner_tiers import TIERS


class AdmissionTests(unittest.TestCase):
    def test_parallel_requests_never_exceed_memory_budget(self):
        with tempfile.TemporaryDirectory() as folder:
            pool = LeasePool(Path(folder)/'leases.json', 6, alive=lambda identity:True)
            with ThreadPoolExecutor(max_workers=8) as workers:
                results = list(workers.map(lambda i:pool.acquire(str(i),2), range(8)))
            self.assertEqual(sum(results),3)
            self.assertFalse(pool.acquire('0',3))
            pool.release('0')
            self.assertTrue(pool.acquire('new',2))

    def test_dead_identity_reclaimed_and_oversized_request_rejected(self):
        with tempfile.TemporaryDirectory() as folder:
            alive = {'old'}
            pool = LeasePool(Path(folder)/'leases.json',4,alive=lambda key:key in alive)
            self.assertTrue(pool.acquire('old',4))
            alive.clear()
            alive.add('new')
            self.assertTrue(pool.acquire('new',4))
            with self.assertRaises(ValueError):pool.acquire('new',5)
            with self.assertRaises(ValueError):pool.acquire('new',float('nan'))

    def test_listener_and_measured_overhead_leave_work_headroom(self):
        budget = admission_budget(TIERS['starting'],3,2,slots=8)
        self.assertEqual(budget['work_gib'],11)
        self.assertEqual(budget['concurrency'],5)
        self.assertLessEqual(budget['work_gib']+budget['reserve_gib'],16)
        with self.assertRaises(ValueError):admission_budget(TIERS['starting'],15,2,slots=8)
