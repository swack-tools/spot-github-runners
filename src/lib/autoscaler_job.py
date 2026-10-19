"""Cloud Run job entrypoint of the runner autoscaler (``src/spot_runner_autoscaler_job.py``).

Cloud Scheduler starts one execution of the ``spot-runner-autoscaler`` Cloud Run job every 5 minutes. Each
execution runs exactly one ``spot_runner_autoscaler.py --once`` check with the container arguments the deploy pinned
(``src/spot_runner_autoscaler_deploy.py``), then exits:

* State: ``SPOT_AUTOSCALER_STATE_URI`` (the controller state object, see ``autoscaler_state_store``) and
  ``SPOT_RUNNER_FLOOR_STATE_URI`` (the per-target lease shared with the runner floor keeper) must both be set. The
  lease makes an execution that starts while another one still runs skip its check, and the state's generation
  preconditions make a write after another controller's write fail closed.
* GitHub: the token is a Secret Manager secret mounted as a file (``SPOT_AUTOSCALER_GITHUB_TOKEN_FILE``), never a
  service env var. It becomes ``GH_TOKEN`` for this process and its ``gh`` children only.
* SSH: a dedicated, persistent key (a Secret Manager secret mounted at ``SPOT_AUTOSCALER_SSH_KEY_FILE``) is copied
  to ``~/.ssh/google_compute_engine`` with mode 0600. ``gcloud compute ssh`` then reuses it: it can't write project
  metadata, so it adds the key once to each runner host's own metadata (the conditional compute role allows that on
  ``oxidex-runners-*`` only) and never again, instead of a new key on every execution.
* ``SPOT_AUTOSCALER_DRY_RUN=1`` (``gcloud run jobs execute ... --update-env-vars``) runs the read-only
  ``--dry-run`` check instead: no lease, no state write, no provider change.

The flags that only make sense on a workstation (``--install-launch-agent``, ``--github-user``, ``--state-file``,
``--log-file``, ``--once``, ``--dry-run``, ``--manage-control-vm``) are refused in the container arguments: the
entrypoint sets the ones it needs, and the control VM stays with the watchdog. Standard library only.
"""
from __future__ import annotations

import logging
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Callable, Mapping, Sequence

from .autoscaler_state_store import ENV_STATE_URI, parse_uri

LOG = logging.getLogger("spot_runner_autoscaler.job")

ENV_DRY_RUN = "SPOT_AUTOSCALER_DRY_RUN"
ENV_GITHUB_TOKEN_FILE = "SPOT_AUTOSCALER_GITHUB_TOKEN_FILE"
ENV_SSH_KEY_FILE = "SPOT_AUTOSCALER_SSH_KEY_FILE"
ENV_FLOOR_STATE_URI = "SPOT_RUNNER_FLOOR_STATE_URI"
STATE_DIR = Path("/tmp/spot-runner-autoscaler")
GITHUB_SECRET_ROOT = "/secrets/autoscaler-github"
SSH_SECRET_ROOT = "/secrets/autoscaler-ssh"
_TRUE = ("1", "true", "yes", "on")
# Workstation-only flags and the flags this entrypoint sets itself: (argparse dest, value that means "not given").
_REFUSED = {
    "install_launch_agent": ("--install-launch-agent", False),
    "github_user": ("--github-user", None),
    "once": ("--once", False),
    "dry_run": ("--dry-run", False),
    "manage_control_vm": ("--manage-control-vm", False),
}
_PRIVATE_KEY_RE = re.compile(rb"^-----BEGIN OPENSSH PRIVATE KEY-----\r?\n[A-Za-z0-9+/=\r\n]+"
                             rb"-----END OPENSSH PRIVATE KEY-----\r?\n?$")


class JobConfigError(RuntimeError):
    pass


def _truthy(value: str | None) -> bool:
    return str(value or "").strip().lower() in _TRUE


def autoscaler_argv(args: Sequence[str], env: Mapping[str, str]) -> list[str]:
    """The autoscaler's argv for this execution: the pinned container args plus the flags the job always sets."""
    from .spot_runner_autoscaler import args_parser

    parser = args_parser()
    try:
        parsed = parser.parse_args(list(args))
    except SystemExit as exc:
        raise JobConfigError(f"the container arguments are not valid autoscaler flags (exit {exc.code})") from exc
    defaults = parser.parse_args([])
    refused = [flag for dest, (flag, unset) in _REFUSED.items() if getattr(parsed, dest) != unset]
    for dest, flag in (("state_file", "--state-file"), ("log_file", "--log-file")):
        if getattr(parsed, dest) != getattr(defaults, dest):
            refused.append(flag)
    if refused:
        raise JobConfigError(f"the Cloud Run job sets or refuses {', '.join(sorted(refused))}; remove them from the "
                             "container arguments")
    if parsed.scaling_mode != "tiered":
        raise JobConfigError("the Cloud Run job runs --scaling-mode tiered only")
    argv = [*args, "--once", "--state-file", str(STATE_DIR / "state.json"),
            "--log-file", str(STATE_DIR / "autoscaler.log")]
    if _truthy(env.get(ENV_DRY_RUN)):
        argv.append("--dry-run")
    return argv


def check_environment(env: Mapping[str, str]) -> None:
    """Refuse to run without durable state, the shared lease, or with a token in a plain env var."""
    try:
        parse_uri(str(env.get(ENV_STATE_URI, "") or ""))
    except ValueError as exc:
        raise JobConfigError(str(exc)) from exc
    if not str(env.get(ENV_FLOOR_STATE_URI, "") or "").strip().startswith("gs://"):
        raise JobConfigError(f"{ENV_FLOOR_STATE_URI} must name the runner floor state (gs://...): the per-target "
                             "lease keeps two executions, and the floor keeper, from acting at once")
    for name in ("GH_TOKEN", "GITHUB_TOKEN"):
        if env.get(name):
            raise JobConfigError(f"{name} is set in the job's environment; the GitHub token comes only from the "
                                 f"secret file {ENV_GITHUB_TOKEN_FILE}")


def read_github_token(env: Mapping[str, str]) -> str:
    path = str(env.get(ENV_GITHUB_TOKEN_FILE, "") or "").strip()
    if not path:
        raise JobConfigError(f"{ENV_GITHUB_TOKEN_FILE} is not set")
    try:
        token = Path(path).read_text(encoding="utf-8").strip()
    except OSError as exc:
        raise JobConfigError(f"could not read the GitHub token file {path}: {exc.strerror}") from exc
    if not token or any(c.isspace() for c in token):
        raise JobConfigError(f"the GitHub token file {path} does not hold one token")
    return token


def install_ssh_key(env: Mapping[str, str], home: Path,
                    runner: Callable[..., subprocess.CompletedProcess] | None = None) -> Path:
    """Copy the mounted private key to gcloud's default key path (0600) and write its public key next to it."""
    source = str(env.get(ENV_SSH_KEY_FILE, "") or "").strip()
    if not source:
        raise JobConfigError(f"{ENV_SSH_KEY_FILE} is not set")
    try:
        key = Path(source).read_bytes()
    except OSError as exc:
        raise JobConfigError(f"could not read the SSH key file {source}: {exc.strerror}") from exc
    if not _PRIVATE_KEY_RE.match(key):
        raise JobConfigError(f"the SSH key file {source} is not an unencrypted OpenSSH private key")
    ssh_dir = home / ".ssh"
    ssh_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(ssh_dir, 0o700)
    private = ssh_dir / "google_compute_engine"
    descriptor = os.open(private, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(key if key.endswith(b"\n") else key + b"\n")
    os.chmod(private, 0o600)
    result = (runner or subprocess.run)(["ssh-keygen", "-y", "-f", str(private)], check=False, capture_output=True, text=True,
                    timeout=30)
    public = (result.stdout or "").strip()
    if result.returncode != 0 or not public.startswith("ssh-"):
        raise JobConfigError(f"ssh-keygen could not read the SSH key: {(result.stderr or '').strip()[:200]}")
    (ssh_dir / "google_compute_engine.pub").write_text(public + "\n", encoding="utf-8")
    return private


def main(argv: Sequence[str] | None = None, *, env: Mapping[str, str] | None = None) -> int:
    from .log_format import configure_logging

    configure_logging()
    env = os.environ if env is None else env
    args = list(sys.argv[1:] if argv is None else argv)
    try:
        check_environment(env)
        final = autoscaler_argv(args, env)
        token = read_github_token(env)
        install_ssh_key(env, Path(str(env.get("HOME") or Path.home())))
    except JobConfigError as exc:
        LOG.error("Autoscaler job refused to start: %s", exc)
        return 2
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    # The token is visible to this process and its children (gh reads GH_TOKEN), never to the job's spec.
    os.environ["GH_TOKEN"] = token
    os.environ.setdefault("GH_CONFIG_DIR", "/tmp/gh-config")
    os.environ.setdefault("GH_PROMPT_DISABLED", "1")
    os.environ.setdefault("GH_NO_UPDATE_NOTIFIER", "1")
    LOG.info("Autoscaler job execution: one %s check", "dry-run" if "--dry-run" in final else "live")
    from .spot_runner_autoscaler import main as autoscaler_main

    return autoscaler_main(final)
