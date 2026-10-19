"""Autoscaler Cloud Run job deploy: preview by default, exact IAM, secrets as file mounts, idempotent --apply.

The runner is always a fake; nothing touches GCP or GitHub.
"""
from __future__ import annotations

import io
import json
import shlex
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from types import SimpleNamespace

from lib import autoscaler_deploy as deploy
from lib import control_watchdog_deploy as wd

PROJECT = "homelab-424523"
NUMBER = "330135650610"
IMAGE = f"us-central1-docker.pkg.dev/{PROJECT}/spot-control/spot-runner-autoscaler@sha256:" + "c" * 64
HOST_SA = f"spot-runner-host@{PROJECT}.iam.gserviceaccount.com"
RUNTIME_SA = f"spot-runner-autoscaler@{PROJECT}.iam.gserviceaccount.com"
INVOKER_SA = f"spot-autoscaler-invoker@{PROJECT}.iam.gserviceaccount.com"
ARGS = ("--scaling-mode", "tiered", "--maximum-hosts", "5", "--max-daily-usd", "10", "--interval-seconds", "300",
        "--minimum-runners", "25", "--project", PROJECT, "--org", "swack-tools", "--remote-builder",
        "--job-repository", "swack-tools/oxidex", "--job-repository", "swack-tools/oxidex-mcp")
BASE_ROLE = ["compute.instances.create", "compute.instances.setMetadata", "compute.projects.get",
             "compute.projects.setCommonInstanceMetadata", "compute.instances.osAdminLogin",
             "compute.instances.osLogin", "compute.instances.setIamPolicy", "compute.disks.create",
             "resourcemanager.projects.get", "resourcemanager.projects.list", "compute.untestable.permission",
             "compute.images.create", "compute.images.delete", "compute.images.useReadOnly", "compute.images.get",
             "compute.snapshots.delete", "compute.instanceTemplates.create", "compute.subnetworks.use",
             "compute.subnetworks.useExternalIp", "backupdr.backupPlans.useForComputeDisk", "backupdr.operations.get"]
TESTABLE = [{"name": p, "customRolesSupportLevel": "SUPPORTED"} for p in BASE_ROLE
            if p not in ("resourcemanager.projects.list", "compute.untestable.permission")]
TESTABLE.append({"name": "resourcemanager.projects.list", "customRolesSupportLevel": "NOT_SUPPORTED"})


def config(**overrides):
    values = dict(project=PROJECT, project_number=NUMBER, image=IMAGE, autoscaler_args=ARGS)
    values.update(overrides)
    return deploy.AutoscalerDeployConfig(**values)


def commands(steps):
    out = []
    for step in steps:
        out += [list(c) for c in (*step.run, *step.create, *step.update)]
    return out


def flag(command, name):
    return next(arg.split("=", 1)[1] for arg in command if arg.startswith(name + "="))


class FakeGcloud:
    """Answers describe calls (existing or not) and records every command."""

    def __init__(self, existing=()):
        self.existing = set(existing)
        self.calls = []

    def __call__(self, argv):
        argv = list(argv)
        self.calls.append(argv)
        ok = SimpleNamespace(returncode=0, stdout="", stderr="")
        if argv[:3] == ["gcloud", "projects", "describe"]:
            return SimpleNamespace(returncode=0, stdout=NUMBER + "\n", stderr="")
        if argv[:4] == ["gcloud", "iam", "roles", "describe"] and argv[4] == deploy.BASE_COMPUTE_ROLE:
            return SimpleNamespace(returncode=0, stdout=json.dumps({"includedPermissions": BASE_ROLE}), stderr="")
        if argv[:3] == ["gcloud", "iam", "list-testable-permissions"]:
            return SimpleNamespace(returncode=0, stdout=json.dumps(TESTABLE), stderr="")
        if argv[:4] == ["gcloud", "secrets", "versions", "describe"]:
            return SimpleNamespace(returncode=0, stdout="ENABLED\n", stderr="")
        if "describe" in argv[:5]:
            name = next((a for a in argv[3:] if not a.startswith("-") and a != "describe"), "")
            return ok if name in self.existing or argv[2] == "buckets" else SimpleNamespace(
                returncode=1, stdout="", stderr="NOT_FOUND")
        return ok


class ConfigTests(unittest.TestCase):
    def test_state_and_lease_share_the_watchdog_bucket_and_floor_prefix(self):
        c = config()
        watchdog = wd.DeployConfig(project=PROJECT, control_vm_service_account="x")
        self.assertEqual(c.floor_state_uri, watchdog.floor_state_uri)
        self.assertEqual(c.state_uri,
                         "gs://homelab-424523-spot-control-state/autoscaler/state-swack-tools-a7245ac03c.json")
        self.assertNotEqual(config(autoscaler_args=ARGS + ("--target", "swack-tools/oxidex")).state_uri, c.state_uri)

    def test_the_target_resolves_like_the_runtime_parser_even_abbreviated(self):
        repo = config(autoscaler_args=ARGS + ("--tar", "other-owner/repo"))
        self.assertEqual(repo.target, "other-owner/repo")
        self.assertNotEqual(repo.state_uri, config().state_uri)
        self.assertEqual(config(autoscaler_args=("--scaling-mode", "tiered")).target, "swack-tools")

    def test_job_args_add_the_dedicated_host_account_and_nothing_per_execution(self):
        args = config().job_args()
        self.assertEqual(args[-1], f"--host-service-account={HOST_SA}")
        # Each flag with a value is one item: gcloud's --args refuses a list holding one item twice.
        self.assertIn("--job-repository=swack-tools/oxidex", args)
        self.assertIn("--remote-builder", args)
        self.assertEqual(len(args), len(set(args)))
        from lib.spot_runner_autoscaler import args_parser
        self.assertEqual(vars(args_parser().parse_args(args)),
                         vars(args_parser().parse_args(list(ARGS) + ["--host-service-account", HOST_SA])))
        for refused in ("--once", "--dry-run", "--state-file", "--log-file", "--github-user"):
            self.assertNotIn(refused, args)

    def test_job_env_holds_paths_and_uris_never_a_token(self):
        env = config(env={"GCP_BILLING_EXPORT_TABLE": "p.d.t"}).job_env()
        self.assertEqual(env["SPOT_AUTOSCALER_STATE_URI"], config().state_uri)
        self.assertEqual(env["SPOT_RUNNER_FLOOR_STATE_URI"], config().floor_state_uri)
        self.assertEqual(env["SPOT_AUTOSCALER_GITHUB_TOKEN_FILE"], "/secrets/autoscaler-github/token")
        self.assertEqual(env["SPOT_AUTOSCALER_SSH_KEY_FILE"], "/secrets/autoscaler-ssh/key")
        self.assertEqual(env["SPOT_GCP_TOKEN_SOURCE"], "metadata")
        self.assertEqual(env["GCP_BILLING_EXPORT_TABLE"], "p.d.t")
        self.assertFalse({"GH_TOKEN", "GITHUB_TOKEN"} & set(env))


class ValidateTests(unittest.TestCase):
    def test_the_lifted_launch_agent_flags_pass(self):
        self.assertEqual(deploy.validate(config()), [])

    def test_problems_stop_the_deploy(self):
        cases = {
            "pinned by digest": config(image=IMAGE.split("@")[0] + ":latest"),
            "autoscaler-args-file": config(autoscaler_args=()),
            "not --project": config(autoscaler_args=ARGS + ("--project", "other-project")),
            "interval-seconds": config(autoscaler_args=ARGS + ("--interval-seconds", "600")),
            "host-service-account": config(autoscaler_args=ARGS + ("--host-service-account",
                                                                   f"other-host@{PROJECT}.iam.gserviceaccount.com")),
            "tiered": config(autoscaler_args=ARGS + ("--scaling-mode", "resource")),
            "--once": config(autoscaler_args=ARGS + ("--once",)),
            "'|'": config(autoscaler_args=ARGS + ("--label", "a|b")),
            "two different secrets": config(ssh_secret=deploy.DEFAULT_GITHUB_SECRET),
            "--memory": config(memory="lots"),
            "--task-timeout": config(task_timeout=60),
            "appear twice": config(autoscaler_args=ARGS + ("--remote-builder",)),
            "schedule must be": config(schedule="*/10 * * * *"),
        }
        for needle, c in cases.items():
            with self.subTest(needle=needle), redirect_stderr(io.StringIO()):
                problems = deploy.validate(c)
                self.assertTrue(any(needle in p for p in problems), problems)


class RoleTests(unittest.TestCase):
    def test_the_custom_role_changes_only_hosts_and_never_project_metadata(self):
        support = {row["name"]: row["customRolesSupportLevel"] for row in TESTABLE}
        permissions = deploy.role_permissions(BASE_ROLE, support)
        self.assertEqual(permissions, ("backupdr.operations.get", "compute.disks.create", "compute.images.get",
                                       "compute.images.useReadOnly", "compute.instances.create",
                                       "compute.instances.setMetadata", "compute.projects.get",
                                       "compute.subnetworks.use", "compute.subnetworks.useExternalIp",
                                       "resourcemanager.projects.get"))

    def test_the_compute_binding_uses_the_custom_role_under_the_runner_host_condition(self):
        steps = deploy.build_steps(config())
        binding = next(c for c in commands(steps) if c[:3] == ["gcloud", "projects", "add-iam-policy-binding"]
                       and f"--member=serviceAccount:{RUNTIME_SA}" in c and "spotRunnerHostAdmin" in " ".join(c))
        self.assertEqual(flag(binding, "--role"), f"projects/{PROJECT}/roles/spotRunnerHostAdmin")
        self.assertIn("--condition-from-file=@FILE:hosts-condition.json", binding)
        condition = json.loads(next(s for s in steps if "hosts-condition.json" in s.files)
                               .files["hosts-condition.json"])
        self.assertEqual(condition["expression"], wd.FLOOR_CONDITION_EXPRESSION)
        # The predefined role, which can write project metadata, is never bound.
        self.assertFalse(any(f"--role={deploy.BASE_COMPUTE_ROLE}" in c for c in commands(steps)))


class StepsTests(unittest.TestCase):
    def test_a_switch_of_the_condition_setting_removes_the_other_binding(self):
        for condition, removed in ((True, "--condition=None"),
                                   (False, "--condition-from-file=@FILE:hosts-condition.json")):
            with self.subTest(condition=condition):
                steps = deploy.build_steps(config(compute_condition=condition))
                step = next(s for s in steps if s.remove)
                (command,) = step.remove
                self.assertEqual(command[:3], ("gcloud", "projects", "remove-iam-policy-binding"))
                self.assertIn(f"--role=projects/{PROJECT}/roles/spotRunnerHostAdmin", command)
                self.assertIn(removed, command)
                self.assertIn("hosts-condition.json", step.files)

    def test_secrets_no_longer_mounted_lose_the_runtime_accessor(self):
        steps = deploy.build_steps(config(ssh_secret="new-ssh-key"))
        labels = [c for c in commands(steps) if c[:3] == ["gcloud", "secrets", "update"]]
        self.assertEqual(sorted(c[3] for c in labels), ["new-ssh-key", "spot-floor-github-swack-tools"])
        self.assertTrue(all("--update-labels=spot-runner-autoscaler=mounted" in c for c in labels))
        ((listing, keep, prune),) = next(s for s in steps if s.prune).prune
        self.assertIn("--filter=labels.spot-runner-autoscaler=mounted", listing)
        self.assertEqual(keep, ("new-ssh-key", "spot-floor-github-swack-tools"))
        self.assertIn(f"--member=serviceAccount:{RUNTIME_SA}", prune[0])
        self.assertIn("--role=roles/secretmanager.secretAccessor", prune[0])

    def test_apply_revokes_a_retired_secret_and_keeps_the_current_ones(self):
        runner = FakeGcloud()
        calls = []

        def listing(argv):
            calls.append(list(argv))
            if list(argv[:3]) == ["gcloud", "secrets", "list"]:
                return SimpleNamespace(returncode=0, stdout="spot-floor-github-swack-tools\nold-ssh-key\n", stderr="")
            return runner(argv)
        out = io.StringIO()
        with redirect_stdout(out):
            wd.execute(config(), deploy.build_steps(config()), listing, out=lambda line: None)
        removed = [c for c in calls if c[:3] == ["gcloud", "secrets", "remove-iam-policy-binding"]]
        self.assertEqual([c[3] for c in removed], ["old-ssh-key"])

    def test_iam_bindings_are_exactly_the_documented_ones(self):
        steps = deploy.build_steps(config())
        granted = sorted((flag(c, "--member").split(":", 1)[1], flag(c, "--role")) for c in commands(steps)
                         if "add-iam-policy-binding" in c)
        documented = sorted((member.split(":", 1)[1], role.split(" ", 1)[0])
                            for _, member, role, _ in deploy.iam_bindings(config()))
        self.assertEqual(granted, documented)
        act_as = [c for c in commands(steps) if "--role=roles/iam.serviceAccountUser" in c]
        self.assertEqual(len(act_as), 1)
        self.assertEqual(act_as[0][4], HOST_SA)  # only on the host account, never project-wide

    def test_the_job_runs_one_task_without_retries_and_mounts_secrets_as_files(self):
        job = next(c for c in commands(deploy.build_steps(config())) if c[:4] == ["gcloud", "run", "jobs", "deploy"])
        for expected in ("--tasks=1", "--parallelism=1", "--max-retries=0", "--task-timeout=3600s", "--cpu=1",
                         "--memory=2Gi", f"--service-account={RUNTIME_SA}", f"--image={IMAGE}"):
            self.assertIn(expected, job)
        self.assertEqual(flag(job, "--set-secrets"),
                         "/secrets/autoscaler-github/token=spot-floor-github-swack-tools:latest,"
                         "/secrets/autoscaler-ssh/key=spot-runner-autoscaler-ssh-key:latest")
        args = flag(job, "--args")
        self.assertTrue(args.startswith("^|^"))
        self.assertEqual(args[3:].split("|"), config().job_args())
        env = dict(item.split("=", 1) for item in flag(job, "--set-env-vars")[3:].split("|"))
        self.assertEqual(env, config().job_env())

    def test_the_scheduler_starts_executions_through_the_admin_api_with_oauth(self):
        steps = deploy.build_steps(config())
        create = next(c for c in commands(steps) if c[:4] == ["gcloud", "scheduler", "jobs", "create"])
        self.assertEqual(flag(create, "--uri"), f"https://run.googleapis.com/v2/projects/{PROJECT}/locations/"
                                                "us-central1/jobs/spot-runner-autoscaler:run")
        self.assertEqual(flag(create, "--schedule"), "*/5 * * * *")
        self.assertEqual(flag(create, "--oauth-service-account-email"), INVOKER_SA)
        self.assertIn("--max-retry-attempts=0", create)
        self.assertFalse(any(a.startswith("--oidc") for a in create))
        self.assertFalse(any(c[:3] == ["gcloud", "scheduler", "jobs"]
                             for c in commands(deploy.build_steps(config(skip_scheduler=True)))))

    def test_without_an_image_only_the_infrastructure_is_deployed(self):
        cmds = commands(deploy.build_steps(config(image="", autoscaler_args=())))
        self.assertFalse(any(c[:3] == ["gcloud", "run", "jobs"] for c in cmds))
        self.assertFalse(any(c[:2] == ["gcloud", "scheduler"] for c in cmds))


class CliTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.args_file = Path(directory.name) / "autoscaler.args"
        lines = ["# lifted from the LaunchAgent"]
        for arg in ARGS:  # one flag per line, its value (if any) on the same line
            if arg.startswith("--"):
                lines.append(shlex.quote(arg))
            else:
                lines[-1] += " " + shlex.quote(arg)
        self.args_file.write_text("\n".join(lines) + "\n\n")

    def run_main(self, argv, runner):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = deploy.main(argv, runner=runner, env={})
        return code, out.getvalue(), err.getvalue()

    def test_the_args_file_reads_shell_words_and_skips_comments(self):
        self.assertEqual(deploy.args_from_file(str(self.args_file)), ARGS)

    def test_preview_is_the_default_and_runs_nothing(self):
        runner = FakeGcloud()
        code, out, err = self.run_main(["--image", IMAGE, "--autoscaler-args-file", str(self.args_file),
                                        "--project-number", NUMBER], runner)
        self.assertEqual((code, err), (0, ""))
        self.assertEqual(runner.calls, [])
        self.assertIn("Runner autoscaler deploy (preview): nothing below has run.", out)
        self.assertIn("gcloud run jobs deploy spot-runner-autoscaler", out)
        self.assertIn("SPOT_AUTOSCALER_DRY_RUN=1", out)

    def test_a_refused_config_runs_nothing(self):
        runner = FakeGcloud()
        code, _, err = self.run_main(["--image", IMAGE, "--apply"], runner)
        self.assertEqual(code, 1)
        self.assertIn("--autoscaler-args-file", err)
        self.assertEqual(runner.calls, [])

    def test_apply_creates_what_is_missing_then_updates_on_the_next_run(self):
        argv = ["--image", IMAGE, "--autoscaler-args-file", str(self.args_file), "--apply"]
        first = FakeGcloud()
        code, out, err = self.run_main(argv, first)
        self.assertEqual((code, err), (0, ""), err)
        self.assertIn("Deployed.", out)
        self.assertIn(["gcloud", "iam", "service-accounts", "create", "spot-runner-host", f"--project={PROJECT}",
                       "--display-name=Runner hosts (Ops Agent metrics and logs only)"], first.calls)
        role_create = next(c for c in first.calls if c[:4] == ["gcloud", "iam", "roles", "create"])
        role_path = Path(flag(role_create, "--file"))
        self.assertFalse(role_path.exists())  # temporary files are gone after the deploy
        self.assertTrue(any(c[:4] == ["gcloud", "scheduler", "jobs", "create"] for c in first.calls))
        again = FakeGcloud(existing={"spot-runner-autoscaler", "spot-runner-host", "spot-autoscaler-invoker",
                                     "spotRunnerHostAdmin", RUNTIME_SA, HOST_SA, INVOKER_SA})
        code, _, err = self.run_main(argv, again)
        self.assertEqual((code, err), (0, ""), err)
        self.assertFalse(any(c[:4] == ["gcloud", "iam", "service-accounts", "create"] for c in again.calls))
        self.assertTrue(any(c[:4] == ["gcloud", "iam", "roles", "update"] for c in again.calls))
        self.assertTrue(any(c[:4] == ["gcloud", "scheduler", "jobs", "update"] for c in again.calls))

    def test_apply_writes_the_computed_permissions_into_the_role_file(self):
        seen = {}
        runner = FakeGcloud()

        def capture(argv):
            if list(argv[:4]) == ["gcloud", "iam", "roles", "create"]:
                seen["role"] = json.loads(Path(flag(argv, "--file")).read_text())
            return runner(argv)
        code, _, err = self.run_main(["--apply"], capture)
        self.assertEqual((code, err), (0, ""), err)
        self.assertNotIn("compute.projects.setCommonInstanceMetadata", seen["role"]["includedPermissions"])
        self.assertIn("compute.instances.create", seen["role"]["includedPermissions"])


if __name__ == "__main__":
    unittest.main()
