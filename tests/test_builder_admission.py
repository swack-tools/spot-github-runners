import importlib.util
import base64
import json
import signal
import unittest
from pathlib import Path
from unittest.mock import patch,mock_open,MagicMock
from types import SimpleNamespace


class BuilderAdmissionTests(unittest.TestCase):
    def arguments(self,payload=None):
        args=['run','--label=oxidex.remote-build=true','-v',
              '/mnt/runner-data/remote-build/sources/cache:/src','-v',
              '/mnt/runner-data/remote-build/targets/cache:/target',
              'oxidex-remote-builder:1.97.1','cargo','build']
        if payload is not None:
            args[-2:]=['python3','/src/tools/remote-build/container_job.py',
                       base64.urlsafe_b64encode(json.dumps(payload).encode()).decode()]
        return args

    def module(self):
        spec=importlib.util.spec_from_file_location('docker_admission',Path(__file__).resolve().parents[1]/'builder/docker-admission.py')
        module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
        return module

    def test_full_host_returns_retryable_without_starting_docker(self):
        module=self.module()
        admission=SimpleNamespace(acquire_build=lambda:(_ for _ in ()).throw(SystemExit(75)),release_build=lambda:None)
        with patch.object(module.subprocess,'run',side_effect=AssertionError('started without capacity')):
            self.assertEqual(module.run_build(self.arguments(),admission),75)

    def test_builder_result_propagates_and_lease_always_released(self):
        module=self.module();events=[]
        admission=SimpleNamespace(acquire_build=lambda:events.append('acquire'),release_build=lambda:events.append('release'))
        child=MagicMock();child.poll.return_value=137;child.returncode=137
        with patch('builtins.open',mock_open()),patch.object(module.fcntl,'flock'),\
             patch.object(module,'container_id',return_value=None),\
             patch.object(module.subprocess,'run',return_value=SimpleNamespace(returncode=0)),\
             patch.object(module.subprocess,'Popen',return_value=child) as docker:
            self.assertEqual(module.run_build(self.arguments(),admission),137)
        self.assertEqual(events,['acquire','release'])
        self.assertEqual(docker.call_args.args[0][0],'/usr/bin/docker')

    def test_payload_identity_and_mounts_are_validated(self):
        module=self.module();run='run-'+'a'*32
        self.assertEqual(module.build_identity(self.arguments({'run_id':run})),('cache',run,'oxidex-run-'+run))
        for args in (self.arguments({'run_id':'../other'}),
                     [*self.arguments(),'--name=unrelated'],
                     [v.replace('/sources/cache','/sources/other') for v in self.arguments()]):
            with self.assertRaises(ValueError):module.build_identity(args)

    def test_unknown_inventory_keeps_lease_and_locks_until_absent(self):
        module=self.module();events=[]
        admission=SimpleNamespace(acquire_build=lambda:events.append('acquire'),release_build=lambda:events.append('release'))
        child=MagicMock();child.poll.return_value=0;child.returncode=0
        def inventory(name):
            events.append('inventory')
            if events.count('inventory')==2:
                raise RuntimeError('daemon inaccessible')
            self.assertNotIn('release',events)
            return None
        with patch('builtins.open',mock_open()),patch.object(module.fcntl,'flock'),\
             patch.object(module,'container_id',side_effect=inventory),\
             patch.object(module.time,'sleep',return_value=None),\
             patch.object(module.subprocess,'run',return_value=SimpleNamespace(returncode=0)),\
             patch.object(module.subprocess,'Popen',return_value=child):
            self.assertEqual(module.run_build(self.arguments(),admission),0)
        self.assertEqual(events,['acquire','inventory','inventory','inventory','release'])

    def test_signal_stops_exact_container_and_reaps_cli_before_release(self):
        module=self.module();events=[];handlers={}
        child=MagicMock();child.poll.side_effect=[None,None,143];child.returncode=143
        def wait(timeout):
            if not events:
                events.append('signal');handlers[signal.SIGTERM](signal.SIGTERM,None)
                raise module.subprocess.TimeoutExpired('docker',1)
            events.append('reaped');return 143
        child.wait.side_effect=wait
        admission=SimpleNamespace(acquire_build=lambda:None,release_build=lambda:events.append('release'))
        def register(signum,handler):handlers[signum]=handler;return signal.SIG_DFL
        def stop(*args):events.append('stop');return True
        with patch('builtins.open',mock_open()),patch.object(module.fcntl,'flock'),\
             patch.object(module.signal,'signal',side_effect=register),\
             patch.object(module,'container_id',return_value=None),\
             patch.object(module,'stop_owned',side_effect=stop),\
             patch.object(module.subprocess,'run',return_value=SimpleNamespace(returncode=0)),\
             patch.object(module.subprocess,'Popen',return_value=child):
            self.assertEqual(module.run_build(self.arguments(),admission),143)
        self.assertEqual(events,['signal','stop','reaped','release'])
        child.terminate.assert_called_once()

    def test_cancel_during_create_never_starts_container(self):
        module=self.module();handlers={}
        admission=SimpleNamespace(acquire_build=lambda:None,release_build=lambda:None)
        def register(signum,handler):handlers[signum]=handler;return signal.SIG_DFL
        def create(*args,**kwargs):
            self.assertEqual(args[0][1],'create')
            handlers[signal.SIGTERM](signal.SIGTERM,None)
        with patch('builtins.open',mock_open()),patch.object(module.fcntl,'flock'),\
             patch.object(module.signal,'signal',side_effect=register),\
             patch.object(module,'container_id',side_effect=[None,'a'*12,None]),\
             patch.object(module,'stop_owned',return_value=True) as stop,\
             patch.object(module.subprocess,'run',side_effect=create),\
             patch.object(module.subprocess,'Popen',side_effect=AssertionError('started cancelled container')):
            self.assertEqual(module.run_build(self.arguments(),admission),143)
        stop.assert_called_once()

    def test_release_failure_preserves_completed_build_exit(self):
        module=self.module()
        admission=SimpleNamespace(acquire_build=lambda:None,release_build=lambda:(_ for _ in ()).throw(OSError('socket restarted')))
        child=MagicMock();child.poll.return_value=0;child.returncode=0
        with patch('builtins.open',mock_open()),patch.object(module.fcntl,'flock'),\
             patch.object(module,'container_id',return_value=None),\
             patch.object(module.subprocess,'run',return_value=SimpleNamespace(returncode=0)),\
             patch.object(module.subprocess,'Popen',return_value=child):
            self.assertEqual(module.run_build(self.arguments(),admission),0)

    def test_stop_refuses_other_container_identity(self):
        module=self.module()
        with patch.object(module,'container_id',return_value='a'*12),\
             patch.object(module.subprocess,'check_output',return_value=json.dumps([{'Name':'/other'}])),\
             patch.object(module.subprocess,'run',side_effect=AssertionError('stopped other build')):
            with self.assertRaises(RuntimeError):module.stop_owned('oxidex-run-run-'+'a'*32,'cache','run-'+'a'*32)
