import importlib.util
import unittest
from pathlib import Path
from unittest.mock import patch
from types import SimpleNamespace


class BuilderAdmissionTests(unittest.TestCase):
    def module(self):
        spec=importlib.util.spec_from_file_location('docker_admission',Path(__file__).resolve().parents[1]/'builder/docker-admission.py')
        module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
        return module

    def test_full_host_returns_retryable_without_starting_docker(self):
        module=self.module()
        admission=SimpleNamespace(acquire_build=lambda:(_ for _ in ()).throw(SystemExit(75)),release_build=lambda:None)
        with patch.object(module.subprocess,'run',side_effect=AssertionError('started without capacity')):
            self.assertEqual(module.run_build(['run','--label=oxidex.remote-build=true'],admission),75)

    def test_builder_result_propagates_and_lease_always_released(self):
        module=self.module();events=[]
        admission=SimpleNamespace(acquire_build=lambda:events.append('acquire'),release_build=lambda:events.append('release'))
        with patch.object(module.subprocess,'run',return_value=SimpleNamespace(returncode=137)) as docker:
            self.assertEqual(module.run_build(['run','--label=oxidex.remote-build=true'],admission),137)
        self.assertEqual(events,['acquire','release'])
        self.assertEqual(docker.call_args.args[0][0],'/usr/bin/docker')
