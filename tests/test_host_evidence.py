import unittest
import subprocess
from types import SimpleNamespace
from unittest.mock import patch
from lib.host_evidence import classify_events, sample


class HostEvidenceTests(unittest.TestCase):
    def test_real_cgroup_kernel_sequence_never_promotes_context_to_host_oom(self):
        messages = [
            'python3 invoked oom-killer: gfp_mask=0xcc0(GFP_KERNEL), order=0, oom_score_adj=500',
            'CPU: 7 PID: 41071 Comm: python3 Not tainted 6.1.0-39-cloud-amd64 #1 Debian',
            'oom-kill:constraint=CONSTRAINT_MEMCG,nodemask=(null),cpuset=docker-abc.scope,mems_allowed=0,oom_memcg=/system.slice/docker-abc.scope,task_memcg=/system.slice/docker-abc.scope,task=python3,pid=41071,uid=1001',
            'Memory cgroup out of memory: Killed process 41071 (python3) total-vm:2553896kB, anon-rss:1919192kB, file-rss:0kB, shmem-rss:0kB, UID:1001 pgtables:6000kB oom_score_adj:500',
            'oom_reaper: reaped process 41071 (python3), now anon-rss:0kB, file-rss:0kB, shmem-rss:0kB',
        ]
        journal = [{'__REALTIME_TIMESTAMP':str((1000+i)*1000000),'MESSAGE':message}
                   for i,message in enumerate(messages)]
        result = classify_events(journal, [], 'boot', 1010)
        self.assertEqual([row['kind'] for row in result], ['container','container'])

    def test_explicit_global_kernel_decision_survives_context_filter(self):
        journal = [{'__REALTIME_TIMESTAMP':'1000000000','MESSAGE':
                    'oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),cpuset=docker-abc.scope,mems_allowed=0,global_oom,task_memcg=/system.slice/docker-abc.scope,task=rustc,pid=4242,uid=1001'}]
        self.assertEqual(classify_events(journal, [], 'boot', 1010)[0]['kind'], 'host')

    def test_sampler_retains_true_boot_global_oom_older_than_recent_window(self):
        global_row={'__REALTIME_TIMESTAMP':'1000000000','MESSAGE':
                    'oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),global_oom,task=rustc,pid=42,uid=1001'}
        with patch('lib.host_evidence.time.time',return_value=10000), \
             patch('lib.host_evidence.Path.read_text',side_effect=['boot','MemTotal: 131072000 kB\nMemAvailable: 123000000 kB\n']), \
             patch('lib.host_evidence.command_json_lines',side_effect=[[],[],[global_row]]):
            observed=sample()
        self.assertEqual(observed['events'],[])
        self.assertIn('boot_oom_complete',observed)
        self.assertTrue(observed['boot_oom_complete'])
        self.assertEqual(len(observed['boot_host_oom_ids']),1)

    def test_unparseable_boot_global_row_cannot_prove_absence(self):
        row={'__REALTIME_TIMESTAMP':'invalid','MESSAGE':'Out of memory: Killed process 42 (rustc)'}
        with patch('lib.host_evidence.time.time',return_value=10000), \
             patch('lib.host_evidence.Path.read_text',side_effect=['boot','MemTotal: 131072000 kB\nMemAvailable: 123000000 kB\n']), \
             patch('lib.host_evidence.command_json_lines',side_effect=[[],[],[row]]):
            observed=sample()
        self.assertFalse(observed['boot_oom_complete'])

    def test_empty_boot_grep_exit_one_proves_no_match(self):
        def run(command,**kwargs):
            result=SimpleNamespace(returncode=1 if any(arg.startswith('--grep=') for arg in command) else 0,
                                   stdout='',stderr='')
            if kwargs.get('check') and result.returncode:
                raise subprocess.CalledProcessError(result.returncode,command,output=result.stdout,stderr=result.stderr)
            return result
        with patch('lib.host_evidence.time.time',return_value=10000), \
             patch('lib.host_evidence.Path.read_text',side_effect=['boot','MemTotal: 131072000 kB\nMemAvailable: 123000000 kB\n']), \
             patch('lib.host_evidence.subprocess.run',side_effect=run):
            try:observed=sample()
            except subprocess.CalledProcessError:self.fail('confirmed empty journal grep exit 1 must be accepted')
        self.assertTrue(observed['boot_oom_complete'])
        self.assertEqual(observed['boot_host_oom_ids'],[])

    def test_boot_grep_permission_failure_cannot_prove_no_match(self):
        def run(command,**kwargs):
            result=SimpleNamespace(returncode=1 if any(arg.startswith('--grep=') for arg in command) else 0,
                                   stdout='',stderr='Permission denied' if any(arg.startswith('--grep=') for arg in command) else '')
            if kwargs.get('check') and result.returncode:
                raise subprocess.CalledProcessError(result.returncode,command,output=result.stdout,stderr=result.stderr)
            return result
        with patch('lib.host_evidence.time.time',return_value=10000), \
             patch('lib.host_evidence.Path.read_text',return_value='boot'), \
             patch('lib.host_evidence.subprocess.run',side_effect=run):
            with self.assertRaises(subprocess.CalledProcessError):sample()

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
