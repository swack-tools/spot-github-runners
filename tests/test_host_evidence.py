import unittest
from lib.host_evidence import classify_events


class HostEvidenceTests(unittest.TestCase):
    def test_kernel_and_cgroup_oom_are_distinct_and_exit137_is_not_proof(self):
        journal = [{ '__REALTIME_TIMESTAMP':'1000000000','MESSAGE':'Out of memory: Killed process 42 (cargo)'},
                   {'__REALTIME_TIMESTAMP':'1001000000','MESSAGE':'Memory cgroup out of memory: Killed process 43'},
                   {'__REALTIME_TIMESTAMP':'1002000000','MESSAGE':'cargo exited 137'}]
        events = [{'Action':'oom','id':'abc','timeNano':1003000000000},
                  {'Action':'die','id':'def','Actor':{'Attributes':{'exitCode':'137'}},'timeNano':1004000000000}]
        result = classify_events(journal,events,'boot1',1005)
        self.assertEqual(len(result),3)
        self.assertEqual([row['kind'] for row in result],['host','container','container'])
        self.assertNotEqual(result[0]['id'],classify_events(journal,[], 'boot2',1005)[0]['id'])

    def test_old_and_future_events_do_not_trigger_capacity(self):
        rows = [{'__REALTIME_TIMESTAMP':'1000000000','MESSAGE':'Out of memory: Killed process 42'}]
        self.assertEqual(classify_events(rows,[],'boot',999),[])
        self.assertEqual(classify_events(rows,[],'boot',3000),[])
