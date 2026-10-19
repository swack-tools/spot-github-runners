"""Cloud Run job entrypoint of the autoscaler: argv, refused flags, credentials as files, and the host account.

Nothing here runs gcloud, gh, ssh or the autoscaler itself.
"""
from __future__ import annotations

import io
import os
import stat
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import test_launch_failover as failover
from lib import autoscaler_job as job
from lib import spot_runner_autoscaler as controller

BASE = ["--scaling-mode", "tiered", "--project", "homelab-424523", "--org", "swack-tools"]
HOST_SA = "spot-runner-host@homelab-424523.iam.gserviceaccount.com"
ENV = {"SPOT_AUTOSCALER_STATE_URI": "gs://bucket/autoscaler/state-x.json",
       "SPOT_RUNNER_FLOOR_STATE_URI": "gs://bucket/runner-floor"}
KEY = (b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmU=\n"
       b"-----END OPENSSH PRIVATE KEY-----\n")


class ArgvTests(unittest.TestCase):
    def test_the_job_adds_once_and_its_own_state_and_log_paths(self):
        argv = job.autoscaler_argv(BASE, {})
        self.assertEqual(argv[:len(BASE)], BASE)
        self.assertEqual(argv[len(BASE):], ["--once", "--state-file", str(job.STATE_DIR / "state.json"),
                                            "--log-file", str(job.STATE_DIR / "autoscaler.log")])
        parsed = controller.args_parser().parse_args(argv)
        self.assertTrue(parsed.once)
        self.assertFalse(parsed.dry_run)

    def test_dry_run_comes_only_from_the_execution_override(self):
        self.assertIn("--dry-run", job.autoscaler_argv(BASE, {job.ENV_DRY_RUN: "1"}))
        self.assertNotIn("--dry-run", job.autoscaler_argv(BASE, {job.ENV_DRY_RUN: "0"}))

    def test_workstation_and_entrypoint_flags_are_refused_even_abbreviated(self):
        for extra in (["--install-launch-agent"], ["--github-user", "swackhamer"], ["--state-file", "/x.json"],
                      ["--state-f", "/x.json"], ["--log-file", "/x.log"], ["--once"], ["--dry-run"],
                      ["--manage-control-vm"]):
            with self.subTest(extra=extra), self.assertRaises(job.JobConfigError):
                job.autoscaler_argv(BASE + extra, {})

    def test_only_tiered_mode_runs(self):
        with self.assertRaisesRegex(job.JobConfigError, "tiered"):
            job.autoscaler_argv(["--scaling-mode", "resource"], {})
        with self.assertRaises(job.JobConfigError), redirect_stderr(io.StringIO()):
            job.autoscaler_argv(["--no-such-flag"], {})


class EnvironmentTests(unittest.TestCase):
    def test_durable_state_and_the_floor_lease_are_required(self):
        job.check_environment(ENV)
        for missing in ENV:
            with self.subTest(missing=missing), self.assertRaises(job.JobConfigError):
                job.check_environment({k: v for k, v in ENV.items() if k != missing})

    def test_a_token_in_a_plain_env_var_is_refused(self):
        for name in ("GH_TOKEN", "GITHUB_TOKEN"):
            with self.subTest(name=name), self.assertRaisesRegex(job.JobConfigError, "secret file"):
                job.check_environment({**ENV, name: "gho_x"})


class CredentialTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.dir = Path(directory.name)

    def test_the_github_token_comes_from_its_file(self):
        token = self.dir / "token"
        token.write_text("gho_abc123\n")
        self.assertEqual(job.read_github_token({job.ENV_GITHUB_TOKEN_FILE: str(token)}), "gho_abc123")
        for content in ("", "two tokens\n"):
            token.write_text(content)
            with self.subTest(content=content), self.assertRaises(job.JobConfigError):
                job.read_github_token({job.ENV_GITHUB_TOKEN_FILE: str(token)})
        with self.assertRaises(job.JobConfigError):
            job.read_github_token({})

    def test_the_ssh_key_is_installed_owner_only_with_its_public_key(self):
        source = self.dir / "mounted-key"
        source.write_bytes(KEY)
        home = self.dir / "home"
        calls = []

        def keygen(argv, **kwargs):
            calls.append(argv)
            return SimpleNamespace(returncode=0, stdout="ssh-ed25519 AAAAC3Nza spot-runner-autoscaler\n", stderr="")
        private = job.install_ssh_key({job.ENV_SSH_KEY_FILE: str(source)}, home, runner=keygen)
        self.assertEqual(private, home / ".ssh" / "google_compute_engine")
        self.assertEqual(private.read_bytes(), KEY)
        self.assertEqual(stat.S_IMODE(private.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE((home / ".ssh").stat().st_mode), 0o700)
        self.assertEqual((home / ".ssh" / "google_compute_engine.pub").read_text(),
                         "ssh-ed25519 AAAAC3Nza spot-runner-autoscaler\n")
        self.assertEqual(calls, [["ssh-keygen", "-y", "-f", str(private)]])

    def test_anything_but_an_unencrypted_openssh_key_is_refused(self):
        source = self.dir / "mounted-key"
        for content in (b"", b"-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----\n",
                        b"not a key"):
            source.write_bytes(content)
            with self.subTest(content=content), self.assertRaises(job.JobConfigError):
                job.install_ssh_key({job.ENV_SSH_KEY_FILE: str(source)}, self.dir / "home",
                                    runner=lambda *a, **k: self.fail("ssh-keygen must not run"))


class MainTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.dir = Path(directory.name)
        (self.dir / "token").write_text("gho_abc123")
        (self.dir / "key").write_bytes(KEY)
        self.env = {**ENV, job.ENV_GITHUB_TOKEN_FILE: str(self.dir / "token"),
                    job.ENV_SSH_KEY_FILE: str(self.dir / "key"), "HOME": str(self.dir / "home")}
        patcher = patch.object(job, "STATE_DIR", self.dir / "state")
        patcher.start()
        self.addCleanup(patcher.stop)
        saved = {name: os.environ.get(name) for name in ("GH_TOKEN", "GH_CONFIG_DIR", "GH_PROMPT_DISABLED",
                                                         "GH_NO_UPDATE_NOTIFIER")}

        def restore():
            for name, value in saved.items():
                if value is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = value
        self.addCleanup(restore)

    def keygen(self):
        return patch.object(job.subprocess, "run", return_value=SimpleNamespace(
            returncode=0, stdout="ssh-ed25519 AAAA\n", stderr=""))

    def test_a_refused_configuration_exits_2_before_the_autoscaler_starts(self):
        with patch.object(controller, "main", side_effect=AssertionError("must not run")):
            self.assertEqual(job.main(BASE + ["--once"], env=self.env), 2)
            self.assertEqual(job.main(BASE, env={}), 2)
        self.assertNotEqual(os.environ.get("GH_TOKEN"), "gho_abc123")

    def test_one_check_runs_with_the_token_for_gh_only_in_this_process(self):
        seen = {}

        def autoscaler(argv):
            seen["argv"], seen["token"] = argv, os.environ.get("GH_TOKEN")
            return 0
        with self.keygen(), patch.object(controller, "main", side_effect=autoscaler):
            self.assertEqual(job.main(BASE, env=self.env), 0)
        self.assertEqual(seen["token"], "gho_abc123")
        self.assertIn("--once", seen["argv"])
        self.assertTrue((self.dir / "state").is_dir())
        self.assertTrue((self.dir / "home" / ".ssh" / "google_compute_engine").exists())


class HostServiceAccountTests(unittest.TestCase):
    """--host-service-account: new hosts run as the dedicated account, never the default Compute Engine one."""

    setUp = failover.LaunchFailoverTests.setUp

    def launch(self, *extra):
        args = controller.args_parser().parse_args(["--state-file", str(self.path), *extra])
        controller.launch_offer(failover.candidate("us-west4-a"), args, {}, self.path)
        return self.cloud.commands[-1]

    def test_the_autoscaler_passes_the_account_to_the_deployment_manager(self):
        command = self.launch("--host-service-account", HOST_SA)
        self.assertEqual(command[command.index("--host-service-account") + 1], HOST_SA)
        self.assertNotIn("--host-service-account", self.launch())

    def test_the_deployment_manager_creates_the_vm_as_that_account_with_two_scopes(self):
        from lib import runner_manager as manager
        create = self.create_command(manager, ["--host-service-account", HOST_SA])
        self.assertIn(f"--service-account={HOST_SA}", create)
        self.assertIn("--scopes=logging-write,monitoring-write", create)
        default = self.create_command(manager, [])
        self.assertFalse(any(flag.startswith(("--service-account", "--scopes")) for flag in default))

    def create_command(self, manager, extra):
        args = manager.args_parser().parse_args(['--max-cost', '.2', '--instance-type', 'n2-standard-32',
                                                 '--ram', '128', '--instance-name', 'test-vm', *extra])
        manager.select_sizing(args)
        results = [SimpleNamespace(returncode=1, stdout=''), SimpleNamespace(returncode=1, stdout='')]
        calls = []

        def cloud(command, **kwargs):
            calls.append(command)
            return '[]' if command[:3] == ['compute', 'disks', 'list'] else ''
        with patch('lib.runner_manager.subprocess.run', side_effect=results), \
                patch('lib.runner_manager.gcloud', side_effect=cloud):
            manager.apply_startup(args, 'zone-a', 'safe-test-script')
        return next(c for c in calls if c[:3] == ['compute', 'instances', 'create'])

    def test_a_malformed_account_is_refused(self):
        with self.assertRaisesRegex(ValueError, "service account"):
            controller.main(["--host-service-account", "not-an-email", "--once"])


if __name__ == "__main__":
    unittest.main()
