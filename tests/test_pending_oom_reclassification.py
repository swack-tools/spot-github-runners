import unittest
from unittest.mock import patch

from lib import spot_runner_autoscaler as controller
from lib.runner_fleet import GceVm
from lib.tier_controller import evidence_for


class PendingOomReclassificationTests(unittest.TestCase):
    def fixture(self):
        vm=GceVm('oxidex-runners-new','z','RUNNING','n2-standard-32',True,32,128,instance_id='2')
        state={'pending_oom':{'z/2':'host'},'seen_oom_ids':['runner-oom'],
               'host_health':{'z/2':{'boot_id':'same-boot'}}}
        observed={'instance_id':'2','boot_id':'same-boot','observed_at':1000,'complete':True,
                  'boot_oom_complete':True,'boot_host_oom_ids':[],
                  'events':[{'id':'runner-oom','kind':'container','workload':'runner','at':999}]}
        return vm,state,observed,controller.args_parser().parse_args(['--scaling-mode','tiered'])

    def test_complete_same_boot_telemetry_retires_false_legacy_host_classification(self):
        vm,state,observed,args=self.fixture()
        with patch('lib.tier_controller.time.time',return_value=1000), \
             patch.object(controller,'read_utilization',return_value=[(.1,.06)]), \
             patch('lib.tier_controller.read_host_events',return_value=observed):
            evidence_for([vm],[],args,state)
        self.assertEqual(state['pending_oom']['z/2'],'runner')

    def test_actual_historical_global_or_unproven_history_retains_host_classification(self):
        for variant in ('historical_global','changed_boot','incomplete_boot','wrong_identity','fresh_global'):
            with self.subTest(variant=variant):
                vm,state,observed,args=self.fixture()
                if variant=='historical_global':observed['boot_host_oom_ids']=['real-old-global']
                if variant=='changed_boot':observed['boot_id']='new-boot'
                if variant=='incomplete_boot':observed['boot_oom_complete']=False
                if variant=='wrong_identity':observed['instance_id']='different'
                if variant=='fresh_global':observed['events'].append({'id':'new-global','kind':'host','workload':'unknown','at':999})
                with patch('lib.tier_controller.time.time',return_value=1000), \
                     patch.object(controller,'read_utilization',return_value=[(.1,.06)]), \
                     patch('lib.tier_controller.read_host_events',return_value=observed):
                    evidence_for([vm],[],args,state)
                self.assertEqual(state['pending_oom']['z/2'],'host')
