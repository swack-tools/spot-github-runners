"""Idempotent deployment of the runner autoscaler as a Cloud Run job (``src/spot_runner_autoscaler_deploy.py``).

The autoscaler (``spot_runner_autoscaler.py --scaling-mode tiered``) used to run as a macOS LaunchAgent loop. Here
it is a Cloud Run **job**: Cloud Scheduler starts one execution every 5 minutes through the Cloud Run Admin API, and
each execution runs one ``--once`` check (``src/spot_runner_autoscaler_job.py``), then exits. A job, not a service:

* A check that launches a host waits for its runners to register, up to 40 minutes with the remote builder. A job
  task may run for hours; a Cloud Scheduler HTTP call to a service gives up after 30 minutes.
* Jobs use instance-based billing, cheaper per vCPU-second than a request-billed service, with their own free tier.
* A timed-out or stopped execution loses its whole container, so no orphaned ``gcloud`` child keeps acting.

Two executions never act at once: an execution takes the target's runner floor lease (``SPOT_RUNNER_FLOOR_STATE_URI``,
shared with the floor keeper in the watchdog) before it reads its state, and one that starts while another still
holds it skips its check. The controller state lives in one Cloud Storage object (``SPOT_AUTOSCALER_STATE_URI``) that
every write updates with a generation precondition (see ``autoscaler_state_store``).

This module creates or updates, in order:

1. The APIs it needs, and a check that the state bucket exists (``control_watchdog_deploy.py`` creates it).
2. Three service accounts: the job's runtime account, the scheduler's invoker account, and the runner host account
   that new hosts run as (Ops Agent metrics and logs only), so the job never acts as the default Compute Engine
   account.
3. A check that both secrets have an enabled version: the GitHub token and the autoscaler's SSH private key (it
   never writes a secret value).
4. IAM, each binding on the narrowest resource that works (see :func:`iam_bindings`).
5. With ``--image`` and ``--autoscaler-args-file``: the Cloud Run job (one task, no retries, secrets mounted as
   files), its invoker binding, and, unless ``--skip-scheduler``, the Cloud Scheduler job.

Without ``--apply`` it prints every command as a reviewable shell script and runs nothing. With it, every resource
is described first and created only when missing; bindings and updates are idempotent. The image comes from the
``autoscaler`` job of ``.github/workflows/control-watchdog-image.yml``, pushed with the watchdog's publisher account.

Standard library only.
"""
from __future__ import annotations

import argparse
import json
import re
import shlex
import sys
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Mapping, Sequence

from . import control_watchdog_deploy as wd
from .autoscaler_job import ENV_DRY_RUN, ENV_GITHUB_TOKEN_FILE, ENV_SSH_KEY_FILE, GITHUB_SECRET_ROOT, \
    SSH_SECRET_ROOT, JobConfigError, autoscaler_argv
from .autoscaler_state_store import ENV_STATE_URI
from .target_config import DEFAULT_ORG, target_label_value

DEFAULT_REGION = wd.DEFAULT_REGION
# Every 5 minutes, the tiered policy's sampling interval (the autoscaler refuses any other interval in tiered mode).
DEFAULT_SCHEDULE = "*/5 * * * *"
DEFAULT_GITHUB_SECRET = "spot-floor-github-swack-tools"
DEFAULT_SSH_SECRET = "spot-runner-autoscaler-ssh-key"
STATE_PREFIX = "autoscaler"
# A launch with the remote builder waits up to 2400 s for registration plus 120 s, after the offer search (up to
# 600 s); the rest of the check fits in what is left of an hour.
TASK_TIMEOUT_SECONDS = 3600
GITHUB_TOKEN_PATH = f"{GITHUB_SECRET_ROOT}/token"
SSH_KEY_PATH = f"{SSH_SECRET_ROOT}/key"
APIS = ("run.googleapis.com", "cloudscheduler.googleapis.com", "secretmanager.googleapis.com",
        "storage.googleapis.com", "iam.googleapis.com", "compute.googleapis.com", "cloudbilling.googleapis.com",
        "monitoring.googleapis.com", "logging.googleapis.com")
PASSTHROUGH_ENV = ("GCP_BILLING_EXPORT_TABLE",)

ROLE_MONITORING_VIEWER = "roles/monitoring.viewer"
ROLE_METRIC_WRITER = "roles/monitoring.metricWriter"
ROLE_LOG_WRITER = "roles/logging.logWriter"
HOSTS_CONDITION_TITLE = "spot-runner-autoscaler-hosts"
SECRET_LABEL = "spot-runner-autoscaler"
# The compute role is roles/compute.instanceAdmin.v1 without the permissions that reach past runner hosts. The IAM
# condition can only narrow instances and disks; the project itself always passes it, and the base role's
# compute.projects.setCommonInstanceMetadata would let gcloud compute ssh put the autoscaler's key (or anyone with
# this account a startup script) into project metadata, which every VM in the project trusts. Without it gcloud
# falls back to the runner host's own metadata. The base role is read through the watchdog deploy's
# read_role_permissions, with this module's narrower role_permissions.
BASE_COMPUTE_ROLE = wd.BASE_COMPUTE_ROLE
ROLE_ID = "spotRunnerHostAdmin"
# Resource types the role may change; the IAM condition limits them to oxidex-runners-* and runner-data*.
HOST_RESOURCES = ("instances", "disks")
EXCLUDED_PERMISSIONS = wd.EXCLUDED_PERMISSIONS

_MEMORY_RE = re.compile(r"^[1-9][0-9]*(Mi|Gi)$")
_CPU_RE = re.compile(r"^[1-8]$")


@dataclass(frozen=True)
class AutoscalerDeployConfig:
    project: str
    region: str = DEFAULT_REGION
    bucket: str = ""
    runtime_account: str = "spot-runner-autoscaler"
    invoker_account: str = "spot-autoscaler-invoker"
    host_account: str = "spot-runner-host"
    repository: str = "spot-control"
    image_name: str = "spot-runner-autoscaler"
    job: str = "spot-runner-autoscaler"
    scheduler_job: str = "spot-runner-autoscaler"
    schedule: str = DEFAULT_SCHEDULE
    image: str = ""
    project_number: str = ""
    github_repo: str = wd.DEFAULT_GITHUB_REPO
    github_secret: str = DEFAULT_GITHUB_SECRET
    ssh_secret: str = DEFAULT_SSH_SECRET
    autoscaler_args: tuple[str, ...] = ()
    cpu: str = "1"
    memory: str = "2Gi"
    task_timeout: int = TASK_TIMEOUT_SECONDS
    compute_condition: bool = True
    skip_scheduler: bool = False
    env: Mapping[str, str] = field(default_factory=dict)
    # The custom role's permissions; read from the base role at --apply time (see role_permissions).
    role_permissions: tuple[str, ...] = ()

    @property
    def compute_role(self) -> str:
        return f"projects/{self.project}/roles/{ROLE_ID}"

    @property
    def state_bucket(self) -> str:
        return self.bucket or f"{self.project.replace(':', '-').replace('.', '-')}-spot-control-state"

    def email(self, account: str) -> str:
        return wd.DeployConfig(project=self.project, control_vm_service_account="").email(account)

    @property
    def runtime_email(self) -> str:
        return self.email(self.runtime_account)

    @property
    def invoker_email(self) -> str:
        return self.email(self.invoker_account)

    @property
    def host_email(self) -> str:
        return self.email(self.host_account)

    @property
    def image_repo(self) -> str:
        return f"{self.region}-docker.pkg.dev/{self.project.replace(':', '/')}/{self.repository}/{self.image_name}"

    @property
    def target(self) -> str:
        """The GitHub target the job manages (--target, else --org), parsed by the autoscaler's own parser, so an
        abbreviated flag resolves exactly as it will at run time (unparsable flags fail validation instead)."""
        import contextlib
        import io

        from .spot_runner_autoscaler import args_parser

        try:
            with contextlib.redirect_stderr(io.StringIO()):
                parsed = args_parser().parse_args(list(self.autoscaler_args))
        except SystemExit:
            return DEFAULT_ORG
        return parsed.target or parsed.org or DEFAULT_ORG

    @property
    def state_uri(self) -> str:
        return f"gs://{self.state_bucket}/{STATE_PREFIX}/state-{target_label_value(self.target)}.json"

    @property
    def floor_state_uri(self) -> str:
        # The same prefix the watchdog's runner floor keeper uses: both take one lease per target there.
        return f"gs://{self.state_bucket}/{wd.FLOOR_STATE_PREFIX}"

    @property
    def run_uri(self) -> str:
        return f"https://run.googleapis.com/v2/projects/{self.project}/locations/{self.region}/jobs/{self.job}:run"

    def job_args(self) -> list[str]:
        """The container args: the operator's autoscaler flags plus the dedicated host account."""
        args = list(self.autoscaler_args)
        if not any(a == "--host-service-account" or a.startswith("--host-service-account=") for a in args):
            args += ["--host-service-account", self.host_email]
        return args

    def job_env(self) -> dict[str, str]:
        env = {key: str(self.env[key]) for key in PASSTHROUGH_ENV if str(self.env.get(key, "") or "").strip()}
        env.update({
            ENV_STATE_URI: self.state_uri,
            "SPOT_RUNNER_FLOOR_STATE_URI": self.floor_state_uri,
            ENV_GITHUB_TOKEN_FILE: GITHUB_TOKEN_PATH,
            ENV_SSH_KEY_FILE: SSH_KEY_PATH,
            "SPOT_GCP_TOKEN_SOURCE": "metadata",
            "CLOUDSDK_CORE_DISABLE_PROMPTS": "1",
            "CLOUDSDK_COMPONENT_MANAGER_DISABLE_UPDATE_CHECK": "true",
            "CLOUDSDK_CONFIG": "/tmp/gcloud-config",
        })
        return env


def validate(config: AutoscalerDeployConfig) -> list[str]:
    """Every problem that must stop the deploy before any command runs."""
    problems = []
    try:
        wd.cv.project_control_paths(config.project, ".")
    except ValueError as exc:
        problems.append(str(exc))
    if not wd._REGION_RE.match(config.region):
        problems.append(f"invalid region {config.region!r}")
    if not wd._BUCKET_RE.match(config.state_bucket):
        problems.append(f"invalid bucket name {config.state_bucket!r}")
    for account in (config.runtime_account, config.invoker_account, config.host_account):
        if not wd._NAME_RE.match(account):
            problems.append(f"service account name {account!r} must be 6-30 lower-case letters, digits or hyphens")
    for name in (config.job, config.scheduler_job, config.repository, config.image_name):
        if not wd._JOB_RE.match(name):
            problems.append(f"invalid resource name {name!r}")
    for secret in (config.github_secret, config.ssh_secret):
        if not wd._SECRET_RE.match(secret):
            problems.append(f"invalid secret name {secret!r}")
    if config.github_secret == config.ssh_secret:
        problems.append("the GitHub token and the SSH key need two different secrets")
    if not wd._GH_REPO_RE.match(config.github_repo):
        problems.append("--github-repo must be owner/repo")
    if config.schedule != DEFAULT_SCHEDULE:
        problems.append(f"the schedule must be {DEFAULT_SCHEDULE!r}: the tier policy counts samples 300-450 s apart")
    if config.project_number and not re.match(r"^[0-9]{6,20}$", config.project_number):
        problems.append("--project-number must be numeric")
    if config.image and not re.match(rf"^{re.escape(config.image_repo)}@sha256:[0-9a-f]{{64}}$", config.image):
        problems.append(f"--image must be {config.image_repo}@sha256:<64 hex> (pinned by digest)")
    if not _CPU_RE.match(config.cpu):
        problems.append("--cpu must be a whole number of vCPUs from 1 to 8")
    if not _MEMORY_RE.match(config.memory):
        problems.append("--memory must look like 2Gi or 1536Mi")
    if not 600 <= config.task_timeout <= 86400:
        problems.append("--task-timeout must be 600 to 86400 seconds")
    if config.image and not config.autoscaler_args:
        problems.append("--image needs --autoscaler-args-file (the autoscaler flags the job runs with)")
    if config.autoscaler_args:
        problems.extend(validate_args(config))
    return problems


def validate_args(config: AutoscalerDeployConfig) -> list[str]:
    problems = []
    args = config.job_args()
    for arg in args:
        if any(c in arg for c in "|\n\r") or not arg:
            problems.append(f"autoscaler argument {arg!r} is empty or holds '|' or a newline")
    try:
        autoscaler_argv(args, {})
    except JobConfigError as exc:
        problems.append(str(exc))
        return problems
    from .spot_runner_autoscaler import args_parser

    parsed = args_parser().parse_args(args)
    if parsed.project != config.project:
        problems.append(f"the autoscaler flags manage project {parsed.project!r}, not --project {config.project!r}")
    if parsed.host_service_account != config.host_email:
        problems.append(f"--host-service-account in the autoscaler flags must be {config.host_email}")
    if parsed.interval_seconds != 300:
        problems.append("--interval-seconds must be 300 (the scheduler starts a check every 5 minutes)")
    return problems


def role_permissions(base: Sequence[str], support: Mapping[str, str] | None = None) -> tuple[str, ...]:
    """The base role's permissions that the autoscaler needs, and that a project custom role can hold
    (``support``: permission -> customRolesSupportLevel, from list-testable-permissions):

    * every permission on instances and disks (the binding's condition narrows those to runner hosts), except OS
      Login and IAM policy writes;
    * on every other Compute Engine resource type only reads and uses (``get*``, ``list*``, ``use*``: a subnetwork,
      an external IP, a public image), so images, snapshots, templates and instance groups stay untouched; no
      project metadata write;
    * on other services only reads (``get*``, ``list*``).
    """
    keep = []
    for permission in base:
        if permission in EXCLUDED_PERMISSIONS or permission.endswith(".setIamPolicy"):
            continue
        service, _, rest = permission.partition(".")
        resource, _, verb = rest.rpartition(".")
        if service == "compute" and resource in HOST_RESOURCES:
            pass
        elif not verb.startswith(("get", "list") if service != "compute" else ("get", "list", "use")):
            continue
        # A permission a project-level custom role can't hold (not testable on the project, or NOT_SUPPORTED, such
        # as resourcemanager.projects.list) would make the whole role definition fail.
        if support is not None and support.get(permission, "NOT_SUPPORTED") == "NOT_SUPPORTED":
            continue
        keep.append(permission)
    return tuple(sorted(set(keep)))


def role_file(config: AutoscalerDeployConfig) -> str:
    """The custom role definition (``gcloud iam roles create --file``), as JSON (a YAML subset)."""
    permissions = list(config.role_permissions) or [
        f"<computed at --apply time from {BASE_COMPUTE_ROLE}: instance and disk permissions except "
        f"{', '.join(EXCLUDED_PERMISSIONS[1:])} and *.setIamPolicy; get*, list* and use* on other compute "
        "resources; get* and list* on other services>"]
    return json.dumps({"title": "Spot runner host admin",
                       "description": f"{BASE_COMPUTE_ROLE}: instances and disks; reads and uses only on the rest",
                       "stage": "GA", "includedPermissions": permissions}, indent=2)


def read_role_permissions(config: AutoscalerDeployConfig, runner: wd.Runner) -> tuple[str, ...]:
    """Read the base role and the project's custom-role support levels; raises DeployError on failure."""
    return wd.read_role_permissions(config.project, runner, APIS, select=role_permissions)


def hosts_condition() -> dict[str, str]:
    return {"expression": wd.FLOOR_CONDITION_EXPRESSION, "title": HOSTS_CONDITION_TITLE,
            "description": "Runner hosts oxidex-runners-* and their runner-data disks"}


def iam_bindings(config: AutoscalerDeployConfig) -> list[tuple[str, str, str, str]]:
    """(resource, member, role, why) of every IAM binding the deploy adds; the documentation lists the same."""
    runtime = f"serviceAccount:{config.runtime_email}"
    host = f"serviceAccount:{config.host_email}"
    return [
        (f"project {config.project}" + (" (condition: instances oxidex-runners-*, disks oxidex-runners-* and "
                                        "runner-data*)" if config.compute_condition else ""), runtime,
         f"{config.compute_role} ({BASE_COMPUTE_ROLE}'s instance and disk permissions; reads and uses elsewhere)",
         "list, create, start, stop, label, set metadata on (drain markers and the SSH key), and delete runner "
         "hosts and their disks; list zones, machine types and CPU platforms"),
        (f"project {config.project}", runtime, ROLE_MONITORING_VIEWER,
         "read the runner hosts' Ops Agent memory and CPU metrics for the tier policy"),
        (f"service account {config.host_email}", runtime, wd.ROLE_ACT_AS,
         "create runner hosts that run as that account (iam.serviceAccounts.actAs), and no other"),
        (f"bucket gs://{config.state_bucket}", runtime, wd.ROLE_OBJECTS,
         "read and conditionally write the autoscaler state, its receipts, and the runner floor leases"),
        (f"secret {config.github_secret}", runtime, wd.ROLE_SECRET,
         f"read the GitHub token, mounted at {GITHUB_TOKEN_PATH}; no other secret"),
        (f"secret {config.ssh_secret}", runtime, wd.ROLE_SECRET,
         f"read the autoscaler's SSH private key, mounted at {SSH_KEY_PATH}; no other secret"),
        (f"project {config.project}", host, ROLE_METRIC_WRITER, "the hosts' Ops Agent writes metrics"),
        (f"project {config.project}", host, ROLE_LOG_WRITER, "the hosts' Ops Agent writes logs"),
        (f"Cloud Run job {config.job}", f"serviceAccount:{config.invoker_email}", wd.ROLE_INVOKER,
         "Cloud Scheduler starts executions of this one job (run.jobs.run); nobody else can"),
    ]


def build_steps(config: AutoscalerDeployConfig) -> list[wd.Step]:
    c = wd._c
    p = f"--project={config.project}"
    region = f"--region={config.region}"
    location = f"--location={config.region}"
    bucket = f"gs://{config.state_bucket}"
    runtime = f"serviceAccount:{config.runtime_email}"
    host = f"serviceAccount:{config.host_email}"
    steps = [wd.Step("Enable the APIs", run=(c("gcloud", "services", "enable", *APIS, p),))]
    steps.append(wd.Step(f"State bucket {bucket} exists (control_watchdog_deploy.py creates it)",
                         run=(c("gcloud", "storage", "buckets", "describe", bucket, p, "--format=value(name)"),)))
    for account, display in ((config.runtime_account, "Runner autoscaler (Cloud Run job)"),
                             (config.invoker_account, "Runner autoscaler scheduler invoker"),
                             (config.host_account, "Runner hosts (Ops Agent metrics and logs only)")):
        steps.append(wd.Step(
            f"Service account {config.email(account)}",
            check=c("gcloud", "iam", "service-accounts", "describe", config.email(account), p, "--format=value(email)"),
            create=(c("gcloud", "iam", "service-accounts", "create", account, p, f"--display-name={display}"),)))
    hints = {
        config.github_secret: ("the GitHub token the autoscaler's gh calls use",
                               f"gh auth token --user <account> | gcloud secrets create {config.github_secret} "
                               f"--data-file=- --replication-policy=automatic {p}"),
        config.ssh_secret: ("the autoscaler's SSH private key",
                            "ssh-keygen -t ed25519 -N '' -C spot-runner-autoscaler -f <scratch>/key && gcloud secrets "
                            f"create {config.ssh_secret} --data-file=<scratch>/key --replication-policy=automatic {p} "
                            "&& rm <scratch>/key <scratch>/key.pub"),
    }
    steps.append(wd.Step(
        "Secrets the job mounts (each needs an enabled version; this deploy never writes secret values)",
        require=tuple((c("gcloud", "secrets", "versions", "describe", "latest", f"--secret={secret}", p,
                         "--format=value(state)"),
                       f"secret {secret} ({what}) has no enabled version; create it: {how}")
                      for secret, (what, how) in sorted(hints.items()))))
    role_files = {"runner-host-role.json": role_file(config)}
    steps.append(wd.Step(
        f"Custom role {config.compute_role} (changes instances and disks only; no project metadata, OS Login, IAM)",
        files=role_files,
        check=c("gcloud", "iam", "roles", "describe", ROLE_ID, p, "--format=value(name)"),
        create=(c("gcloud", "iam", "roles", "create", ROLE_ID, p, "--file=@FILE:runner-host-role.json",
                  "--format=none"),),
        update=(c("gcloud", "iam", "roles", "update", ROLE_ID, p, "--file=@FILE:runner-host-role.json",
                  "--format=none", "--quiet"),)))
    compute_args = (["--condition-from-file=@FILE:hosts-condition.json"] if config.compute_condition
                    else ["--condition=None"])
    # The condition file is also needed to remove the conditional binding after a switch to --no-compute-condition.
    files = {"hosts-condition.json": json.dumps(hosts_condition(), indent=2)}
    steps.append(wd.Step("Runtime and host service account IAM (narrowest resource for each role)", files=files, run=(
        c("gcloud", "projects", "add-iam-policy-binding", config.project, f"--member={runtime}",
          f"--role={config.compute_role}", *compute_args, "--format=none"),
        c("gcloud", "projects", "add-iam-policy-binding", config.project, f"--member={runtime}",
          f"--role={ROLE_MONITORING_VIEWER}", "--condition=None", "--format=none"),
        c("gcloud", "iam", "service-accounts", "add-iam-policy-binding", config.host_email, p, f"--member={runtime}",
          f"--role={wd.ROLE_ACT_AS}", "--format=none"),
        c("gcloud", "storage", "buckets", "add-iam-policy-binding", bucket, f"--member={runtime}",
          f"--role={wd.ROLE_OBJECTS}", "--format=none"),
        *(c("gcloud", "secrets", "add-iam-policy-binding", secret, p, f"--member={runtime}",
            f"--role={wd.ROLE_SECRET}", "--format=none") for secret in sorted({config.github_secret,
                                                                                 config.ssh_secret})),
        c("gcloud", "projects", "add-iam-policy-binding", config.project, f"--member={host}",
          f"--role={ROLE_METRIC_WRITER}", "--condition=None", "--format=none"),
        c("gcloud", "projects", "add-iam-policy-binding", config.project, f"--member={host}",
          f"--role={ROLE_LOG_WRITER}", "--condition=None", "--format=none"),
        # Labels mark the secrets this deploy grants access to, so a later deploy finds and revokes stale ones.
        *(c("gcloud", "secrets", "update", secret, p, f"--update-labels={SECRET_LABEL}=mounted", "--format=none")
          for secret in sorted({config.github_secret, config.ssh_secret})),
    )))
    # Grants a previous deploy made that this configuration doesn't: the compute binding of the other condition
    # setting (add-iam-policy-binding never replaces it), and the accessor on labeled secrets no longer mounted.
    other = ["--condition=None"] if config.compute_condition else ["--condition-from-file=@FILE:hosts-condition.json"]
    keep = tuple(sorted({config.github_secret, config.ssh_secret}))
    steps.append(wd.Step(
        "Remove grants that are no longer configured (absent ones are skipped)", files=files,
        remove=(c("gcloud", "projects", "remove-iam-policy-binding", config.project, f"--member={runtime}",
                  f"--role={config.compute_role}", *other, "--format=none"),),
        prune=((c("gcloud", "secrets", "list", p, f"--filter=labels.{SECRET_LABEL}=mounted",
                  "--format=value(name.basename())"), keep,
                (c("gcloud", "secrets", "remove-iam-policy-binding", wd.NAME_ARG, p, f"--member={runtime}",
                   f"--role={wd.ROLE_SECRET}", "--format=none"),
                 c("gcloud", "secrets", "update", wd.NAME_ARG, p, f"--remove-labels={SECRET_LABEL}",
                   "--format=none"))),)))
    if not config.image:
        return steps
    env_arg = "--set-env-vars=^|^" + "|".join(f"{k}={v}" for k, v in sorted(config.job_env().items()))
    secrets_arg = (f"--set-secrets={GITHUB_TOKEN_PATH}={config.github_secret}:latest,"
                   f"{SSH_KEY_PATH}={config.ssh_secret}:latest")
    steps.append(wd.Step(f"Cloud Run job {config.job} (one --once check per execution)", run=(
        c("gcloud", "run", "jobs", "deploy", config.job, p, region, f"--image={config.image}",
          f"--service-account={config.runtime_email}", "--tasks=1", "--parallelism=1", "--max-retries=0",
          f"--task-timeout={config.task_timeout}s", f"--cpu={config.cpu}", f"--memory={config.memory}",
          env_arg, secrets_arg, "--args=^|^" + "|".join(config.job_args()),
          "--labels=spot-role=runner-autoscaler", "--quiet"),
        c("gcloud", "run", "jobs", "add-iam-policy-binding", config.job, p, region,
          f"--member=serviceAccount:{config.invoker_email}", f"--role={wd.ROLE_INVOKER}", "--format=none"),
    )))
    if config.skip_scheduler:
        return steps
    scheduler = (p, location, f"--schedule={config.schedule}", "--time-zone=Etc/UTC", f"--uri={config.run_uri}",
                 "--http-method=POST", f"--oauth-service-account-email={config.invoker_email}",
                 "--oauth-token-scope=https://www.googleapis.com/auth/cloud-platform", "--attempt-deadline=60s",
                 "--max-retry-attempts=0")
    steps.append(wd.Step(
        f"Cloud Scheduler job {config.scheduler_job} ({config.schedule}, OAuth as {config.invoker_email})",
        check=c("gcloud", "scheduler", "jobs", "describe", config.scheduler_job, location, p, "--format=value(name)"),
        create=(c("gcloud", "scheduler", "jobs", "create", "http", config.scheduler_job, *scheduler,
                  "--description=Starts one runner autoscaler check"),),
        update=(c("gcloud", "scheduler", "jobs", "update", "http", config.scheduler_job, *scheduler),)))
    return steps


def footer(config: AutoscalerDeployConfig) -> list[str]:
    p = f"--project={config.project}"
    region = f"--region={config.region}"
    lines = [""]
    if not config.image:
        lines += ["# The Cloud Run job and the scheduler need --image and --autoscaler-args-file: build the image",
                  "# first (the autoscaler job of the Control watchdog image workflow), then run this again."]
    lines += ["# GitHub repository variables for the image workflow (set them yourself with gh; the watchdog's",
              "# WATCHDOG_* variables are reused):"]
    for name, value in (("AUTOSCALER_GH_VERSION", "<x.y.z>"), ("AUTOSCALER_GH_SHA256", "<sha256 of the tarball>")):
        lines.append(f"#   gh variable set {name} --repo {config.github_repo} --body '{value}'")
    lines += ["# One read-only check (no lease, no state write, no provider change):",
              f"#   gcloud run jobs execute {config.job} {p} {region} --update-env-vars={ENV_DRY_RUN}=1 --wait",
              f"# State object: {config.state_uri}"]
    return lines


def args_from_file(path: str) -> tuple[str, ...]:
    """Autoscaler flags from a file: shell words per line, ``#`` comments and blank lines ignored."""
    try:
        text = Path(path).expanduser().read_text(encoding="utf-8")
    except OSError as exc:
        raise wd.DeployError(f"could not read --autoscaler-args-file {path}: {exc.strerror}") from exc
    args: list[str] = []
    for number, line in enumerate(text.splitlines(), 1):
        try:
            args += shlex.split(line, comments=True)
        except ValueError as exc:
            raise wd.DeployError(f"{path}:{number}: {exc}") from exc
    return tuple(args)


def args_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Create or update the runner autoscaler Cloud Run job and its Cloud Scheduler trigger. Prints "
                    "the commands as a reviewable script unless --apply is given.")
    parser.add_argument("--project", default=wd.cv.DEFAULT_PROJECT)
    parser.add_argument("--region", default=DEFAULT_REGION, help="Region of the job, scheduler and image repository")
    parser.add_argument("--bucket", default="", help="State bucket (default: <project>-spot-control-state)")
    parser.add_argument("--image", default="",
                        help="Autoscaler image pinned by digest (<region>-docker.pkg.dev/<project>/<repo>/"
                             "spot-runner-autoscaler@sha256:...); without it only the infrastructure is deployed")
    parser.add_argument("--autoscaler-args-file", default="",
                        help="File with the autoscaler flags the job runs with (shell words per line, # comments)")
    parser.add_argument("--github-secret", default=DEFAULT_GITHUB_SECRET,
                        help="Secret Manager secret with the GitHub token (default: %(default)s)")
    parser.add_argument("--ssh-secret", default=DEFAULT_SSH_SECRET,
                        help="Secret Manager secret with the autoscaler's SSH private key (default: %(default)s)")
    parser.add_argument("--cpu", default="1", help="vCPUs per execution (default: %(default)s)")
    parser.add_argument("--memory", default="2Gi", help="Memory per execution (default: %(default)s)")
    parser.add_argument("--task-timeout", type=int, default=TASK_TIMEOUT_SECONDS,
                        help="Seconds before an execution is stopped (default: %(default)s)")
    parser.add_argument("--github-repo", default=wd.DEFAULT_GITHUB_REPO)
    parser.add_argument("--project-number", default="", help="Skip the project-number lookup")
    parser.add_argument("--no-compute-condition", dest="compute_condition", action="store_false",
                        help="Grant the compute role without the oxidex-runners-* IAM condition")
    parser.add_argument("--skip-scheduler", action="store_true",
                        help="Deploy the job but not the scheduler (run executions by hand first)")
    parser.add_argument("--apply", action="store_true", help="Run the commands (default: print them only)")
    return parser


def main(argv: Sequence[str] | None = None, *, runner: wd.Runner = wd.default_runner,
         env: Mapping[str, str] | None = None) -> int:
    import os

    args = args_parser().parse_args(argv)
    env = os.environ if env is None else env
    try:
        config = AutoscalerDeployConfig(
            project=args.project, region=args.region, bucket=args.bucket, image=args.image,
            project_number=args.project_number, github_repo=args.github_repo, github_secret=args.github_secret,
            ssh_secret=args.ssh_secret, cpu=args.cpu, memory=args.memory,
            task_timeout=args.task_timeout, compute_condition=args.compute_condition,
            skip_scheduler=args.skip_scheduler,
            autoscaler_args=args_from_file(args.autoscaler_args_file) if args.autoscaler_args_file else (),
            env={key: str(env.get(key, "") or "") for key in PASSTHROUGH_ENV})
        problems = validate(config)
        if problems:
            raise wd.DeployError("refusing to deploy:\n  - " + "\n  - ".join(problems))
        if args.apply:
            config = replace(config, role_permissions=read_role_permissions(config, runner))
        steps = build_steps(config)
        if not args.apply:
            sys.stdout.write(wd.render(config, steps, title="Runner autoscaler deploy (preview)",
                                       footer=footer(config)))
            return 0
        wd.execute(config, steps, runner)
    except wd.DeployError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    print(f"Deployed. Job: {config.job} in {config.region}; state: {config.state_uri}.")
    if not config.image:
        print("Build the autoscaler image, then run again with --image=<digest> --autoscaler-args-file=<file>.")
    return 0
