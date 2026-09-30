"""Maintain an online floor of cost-capped GitHub Actions runners."""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import subprocess
import sys
import time
from decimal import Decimal
from pathlib import Path
from tempfile import NamedTemporaryFile

from .autoscaling_policy import affordable_offers
from .compute_api import get_auth
from .environment import load_env
from .fleet_logging import lifecycle_log, observe_vms
from .price_guard import check_running_prices
from .runner_labels import DEFAULT_RUNNER_LABELS, counts_toward_capacity
from .runner_fleet import GceVm, GitHubRunner, list_gcloud_vms, list_github_runners
from .spot_finder import find

ROOT = Path(__file__).resolve().parents[2]
LOG = logging.getLogger("spot_runner_autoscaler")
DOCKER_LABEL = "docker"


def count_online_runners(org: str, label: str) -> int:
    return sum(runner.online and runner_is_managed(runner)
               and counts_toward_capacity(runner.labels, label)
               for runner in list_github_runners(org))


def runner_is_managed(runner: GitHubRunner) -> bool:
    """Return whether a runner belongs to this GCP x64 fleet."""
    current_shape = bool({"n2-standard-32", "e2-standard-32", "c2d-standard-32"}
                         .intersection(runner.labels))
    legacy_toronto = {
        "oxidex-spot-7x27", "northamerica-northeast2"
    }.issubset(runner.labels)
    return (current_shape or legacy_toronto) and "X64" in runner.labels


def docker_pool_hosts(runners: list[GitHubRunner] | tuple[GitHubRunner, ...]) -> set[str]:
    """Return host keys for online Docker runners without the Spot label."""
    hosts = set()
    for runner in runners:
        if not runner.online or DOCKER_LABEL not in runner.labels or "spot" in runner.labels:
            continue
        # Runner names end in the per-host numeric slot, such as -1 or -2.
        host, separator, slot = runner.name.rpartition("-")
        if separator and slot.isdecimal():
            hosts.add(host)
        else:
            hosts.add(runner.name)
    return hosts


def docker_pool_state(org: str) -> tuple[int, int]:
    """Return all online Docker runners and their distinct hosts."""
    runners = list_github_runners(org)
    docker_runners = [runner for runner in runners
                      if runner.online and DOCKER_LABEL in runner.labels and "spot" not in runner.labels]
    return len(docker_runners), len(docker_pool_hosts(docker_runners))


def docker_pool_satisfied(runner_count: int, host_count: int,
                          minimum_runners: int = 2, minimum_hosts: int = 2) -> bool:
    """Check the Docker runner count and host-diversity floors."""
    return runner_count >= minimum_runners and host_count >= minimum_hosts


def maximum_runner_name_length(prefix: str, instance_name: str, runner_count: int) -> int:
    """Estimate the longest runner name when GCE's hostname matches its VM name."""
    return max(len(f"{prefix}-{instance_name}-{index}") for index in range(1, runner_count + 1))


def managed_online_runners(runners: list[GitHubRunner] | tuple[GitHubRunner, ...], label: str):
    """Return online GCP fleet runners in either worker pool."""
    return [runner for runner in runners if runner.online and runner_is_managed(runner)
            and (counts_toward_capacity(runner.labels, label) or DOCKER_LABEL in runner.labels)]


def spot_instances(vms: list[GceVm] | tuple[GceVm, ...]) -> list[GceVm]:
    """Return running Spot VMs created for this runner fleet."""
    return [vm for vm in vms if vm.running and vm.spot and vm.name.startswith("oxidex-runners-")]


def utilization_counts(runners: list[GitHubRunner] | tuple[GitHubRunner, ...], label: str) -> tuple[int, int]:
    """Return online and busy runner counts for both GCP worker pools."""
    online = managed_online_runners(runners, label)
    return len(online), sum(runner.busy for runner in online)


def utilization_counts_with_draining(
    runners: list[GitHubRunner] | tuple[GitHubRunner, ...], label: str,
    draining_vm_names: set[str], vms: list[GceVm] | tuple[GceVm, ...],
) -> tuple[int, int]:
    """Count online runners after their custom labels have been cleared."""
    counted = {runner.runner_id: runner for runner in managed_online_runners(runners, label)}
    for runner in runners:
        if not runner.online:
            continue
        matches = [vm.name for vm in vms if vm.name in runner.name]
        if matches and max(matches, key=len) in draining_vm_names:
            counted[runner.runner_id] = runner
    return len(counted), sum(runner.busy for runner in counted.values())


def host_runners(vm: GceVm, runners: list[GitHubRunner] | tuple[GitHubRunner, ...],
                 vms: list[GceVm] | tuple[GceVm, ...]):
    """Find runners assigned to the longest matching VM name."""
    names = [candidate.name for candidate in vms]
    assigned = []
    for runner in runners:
        matches = [name for name in names if name in runner.name]
        if matches and max(matches, key=len) == vm.name:
            assigned.append(runner)
    return assigned


def host_utilization(vms: list[GceVm] | tuple[GceVm, ...],
                     runners: list[GitHubRunner] | tuple[GitHubRunner, ...]) -> dict[str, tuple[int, int, int]]:
    """Return busy, online, and assigned runner counts for every Spot VM."""
    fleet = spot_instances(vms)
    usage = {}
    for vm in fleet:
        assigned = host_runners(vm, runners, fleet)
        online = [runner for runner in assigned if runner.online]
        usage[vm.name] = (sum(runner.busy for runner in online), len(online), len(assigned))
    return usage


def removable_spot_vms(vms: list[GceVm], runners: list[GitHubRunner],
                       docker_floor: int, docker_host_floor: int,
                       draining_vm_names: set[str] | None = None,
                       draining_runner_ids: set[int] | None = None) -> list[GceVm]:
    """Choose VMs with at most half of their online runners busy."""
    candidates = []
    fleet = spot_instances(vms)
    draining_vm_names = draining_vm_names or set()
    draining_runner_ids = draining_runner_ids or set()
    potential = []
    for vm in fleet:
        if vm.name in draining_vm_names:
            continue
        assigned = host_runners(vm, runners, fleet)
        if (not assigned or any(not runner.online for runner in assigned)
                or 2 * sum(runner.busy for runner in assigned) > len(assigned)):
            continue
        potential.append(vm)
    potential.sort(key=lambda vm: any(
        DOCKER_LABEL in runner.labels and "spot" not in runner.labels
        for runner in host_runners(vm, runners, fleet)))
    removed_ids = set(draining_runner_ids)
    for vm in potential:
        host = host_runners(vm, runners, fleet)
        proposed_removed = removed_ids | {runner.runner_id for runner in host}
        remaining_docker = [runner for runner in runners
                            if runner.online and runner.runner_id not in proposed_removed
                            and DOCKER_LABEL in runner.labels and "spot" not in runner.labels]
        if not docker_pool_satisfied(len(remaining_docker), len(docker_pool_hosts(remaining_docker)),
                                     docker_floor, docker_host_floor):
            continue
        candidates.append(vm)
        removed_ids = proposed_removed
    return candidates


def clear_custom_runner_labels(runners: list[GitHubRunner], org: str) -> None:
    """Remove custom labels so new label-specific jobs stop targeting runners."""
    for runner in runners:
        if not any(label not in DEFAULT_RUNNER_LABELS for label in runner.labels):
            continue
        subprocess.run(
            ["gh", "api", "-X", "DELETE",
             f"orgs/{org}/actions/runners/{runner.runner_id}/labels"],
            check=True, text=True, capture_output=True, timeout=60,
        )


def begin_vm_drain(vm: GceVm, runners: list[GitHubRunner], vms: list[GceVm],
                   org: str, state: dict, reason: str) -> bool:
    """Clear custom labels and persist the host's drain request."""
    if not vm.running or not vm.spot or not vm.name.startswith("oxidex-runners-"):
        return False
    assigned = host_runners(vm, runners, spot_instances(vms))
    if any(not runner.online for runner in assigned):
        return False
    if not assigned and reason != "price":
        return False
    clear_custom_runner_labels(assigned, org)
    state.setdefault("draining_vms", {})[vm.name] = {
        "zone": vm.zone,
        "reason": reason,
        "runner_ids": [runner.runner_id for runner in assigned],
    }
    return True


def delete_drained_vm(vm: GceVm, project: str, org: str,
                      runners: list[GitHubRunner], vms: list[GceVm]) -> None:
    """Unregister idle runners, then delete their Spot VM and attached disk."""
    if not vm.running or not vm.spot or not vm.name.startswith("oxidex-runners-"):
        raise RuntimeError(f"Refusing to delete unmanaged Spot VM {vm.name}")
    assigned = host_runners(vm, runners, spot_instances(vms))
    if any(not runner.online or runner.busy for runner in assigned):
        raise RuntimeError(f"Refusing to delete {vm.name}: a runner is offline or busy")
    for runner in assigned:
        subprocess.run(
            ["gh", "api", "-X", "DELETE",
             f"orgs/{org}/actions/runners/{runner.runner_id}"],
            check=True, text=True, capture_output=True, timeout=60,
        )
    subprocess.run(
        ["gcloud", "compute", "instances", "delete", vm.name,
         f"--zone={vm.zone}", f"--project={project}", "--delete-disks=all", "--quiet"],
        check=True, text=True, capture_output=True, timeout=180,
    )
    LOG.info("Deleted drained Spot VM %s in %s and its attached disk", vm.name, vm.zone)


def advance_vm_drains(vms: list[GceVm], runners: list[GitHubRunner],
                      args: argparse.Namespace, state: dict) -> tuple[set[str], set[int]]:
    """Delete drained VMs after their registered runners finish their jobs."""
    removed_vms: set[str] = set()
    removed_runner_ids: set[int] = set()
    fleet = spot_instances(vms)
    drains = state.setdefault("draining_vms", {})
    for name, drain in list(drains.items()):
        vm = next((candidate for candidate in fleet if candidate.name == name), None)
        if vm is None:
            drains.pop(name, None)
            continue
        assigned = host_runners(vm, runners, fleet)
        if any(not runner.online for runner in assigned):
            LOG.warning("Drain waits for offline runners on %s; job state is unknown", name)
            continue
        try:
            custom_labels = [runner for runner in assigned
                             if any(label not in DEFAULT_RUNNER_LABELS for label in runner.labels)]
            if custom_labels:
                clear_custom_runner_labels(custom_labels, args.org)
                LOG.info("Cleared custom labels from %s; checking job state next loop", name)
                continue
            if any(runner.busy for runner in assigned):
                LOG.info("Drain waits for %d active jobs on %s", sum(r.busy for r in assigned), name)
                continue
            if not assigned and not drain.get("runner_ids") and drain.get("reason") != "price":
                LOG.warning("Drain waits for runner inventory on %s", name)
                continue
            delete_drained_vm(vm, args.project, args.org, runners, vms)
        except (OSError, subprocess.SubprocessError, RuntimeError) as exc:
            LOG.error("Could not finish draining %s; retrying next loop: %s", name, exc)
            continue
        removed_vms.add(name)
        removed_runner_ids.update(drain.get("runner_ids", []))
        removed_runner_ids.update(runner.runner_id for runner in assigned)
        drains.pop(name, None)
    return removed_vms, removed_runner_ids


def read_scale_state(path: Path) -> dict:
    """Read loop counters that must survive LaunchAgent restarts."""
    try:
        state = json.loads(path.read_text())
    except FileNotFoundError:
        return {"idle_loops": 0, "scale_up_active": False, "draining_vms": {}}
    except (json.JSONDecodeError, OSError) as exc:
        LOG.warning("Ignoring unreadable autoscaler state at %s: %s", path, exc)
        return {"idle_loops": 0, "scale_up_active": False, "draining_vms": {}}
    return {"idle_loops": int(state.get("idle_loops", 0)),
            "scale_up_active": bool(state.get("scale_up_active", False)),
            "price_replacement_target": int(state.get("price_replacement_target", 0)),
            "observed_vms": state.get("observed_vms", {}),
            "draining_vms": state.get("draining_vms", {})}


def write_scale_state(path: Path, state: dict) -> None:
    """Write autoscaler loop counters atomically."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with NamedTemporaryFile("w", dir=path.parent, delete=False) as temp:
        json.dump(state, temp)
        temp.flush()
        os.fsync(temp.fileno())
        temporary_path = Path(temp.name)
    temporary_path.replace(path)


def scale_down(vms: list[GceVm], runners: list[GitHubRunner], args: argparse.Namespace,
               state: dict) -> int:
    """Halve the Spot fleet by draining hosts with at most half busy runners."""
    fleet = spot_instances(vms)
    target = max(1, len(fleet) // 2)
    drains = state.setdefault("draining_vms", {})
    existing = {name for name, item in drains.items() if item.get("reason") == "scale-down"}
    draining_runner_ids = {runner_id for item in drains.values()
                           for runner_id in item.get("runner_ids", [])}
    count = max(0, len(fleet) - target - len(existing))
    candidates = removable_spot_vms(vms, runners, args.minimum_docker_runners,
                                    args.minimum_docker_hosts, existing, draining_runner_ids)
    if len(candidates) < count:
        LOG.info("Scale-down waits: %d hosts are at most half busy; %d are needed",
                 len(candidates), count)
    draining = 0
    for vm in candidates[:count]:
        LOG.info("Starting drain for Spot VM %s", vm.name)
        try:
            if begin_vm_drain(vm, runners, vms, args.org, state, "scale-down"):
                draining += 1
        except (OSError, subprocess.SubprocessError) as exc:
            LOG.error("Could not start draining %s: %s", vm.name, exc)
            break
    return draining


def scale_up(additional_hosts: int, offers, args: argparse.Namespace) -> int:
    """Launch the requested host count, trying affordable offers in order."""
    launched = 0
    for _ in range(additional_hosts):
        for offer in offers:
            try:
                launch_offer(offer, args)
                launched += 1
                break
            except (RuntimeError, OSError) as exc:
                LOG.warning("Could not deploy %s in %s: %s", offer.machine_type, offer.region, exc)
        else:
            break
    return launched


def reconcile_prices(vms, runners, args, state, state_path) -> bool:
    """Evict overpriced VMs and retry replacements across loop restarts."""
    fleet = spot_instances(vms)
    overpriced = check_running_prices(fleet, args.project, args.max_daily_usd)
    if overpriced:
        state["price_replacement_target"] = max(
            state.get("price_replacement_target", 0), len(fleet))
        state["idle_loops"] = 0
        write_scale_state(state_path, state)
    for vm in overpriced:
        if vm.name in state.setdefault("draining_vms", {}):
            continue
        try:
            if begin_vm_drain(vm, runners, fleet, args.org, state, "price"):
                LOG.warning("Price exceeds cap; cleared custom labels on %s and will wait for its jobs",
                            vm.name)
            else:
                LOG.warning("Price exceeds cap, but runner state is unsafe to drain on %s; retrying next loop",
                            vm.name)
        except (OSError, subprocess.SubprocessError) as exc:
            LOG.error("Could not start price drain for %s; retrying next loop: %s", vm.name, exc)
            continue
    target = state.get("price_replacement_target", 0)
    if not target:
        return False
    write_scale_state(state_path, state)
    remaining = spot_instances(list_gcloud_vms(args.project, name_prefix="oxidex-runners-"))
    price_drains = {name for name, drain in state.get("draining_vms", {}).items()
                    if drain.get("reason") == "price"}
    if {vm.name for vm in remaining} & (price_drains | {vm.name for vm in overpriced}):
        return True
    missing = max(0, target - len(remaining))
    if missing:
        LOG.info("Finding affordable replacements for %d price evictions", missing)
        offers = discover_offers(args.project, args.max_daily_usd)
        launched = scale_up(missing, offers, args) if offers else 0
        if launched < missing:
            LOG.warning("Replacement capacity is pending; retrying next loop")
            return True
    state["price_replacement_target"] = 0
    state["idle_loops"] = 0
    write_scale_state(state_path, state)
    return True


def scaling_decision(online: int, busy: int, vm_count: int, idle_loops: int,
                     scale_up_active: bool, target_workers: int) -> tuple[str, int, int, bool]:
    """Choose one scale action from utilization and the persisted loop state."""
    if scale_up_active and online > target_workers:
        return "hold", 0, 0, False
    if scale_up_active or (online > 0 and online <= target_workers and busy * 2 > online):
        target_vms = max(1, vm_count * 2)
        return "up", max(0, target_vms - vm_count), 0, True
    if vm_count > 1 and online > 0 and busy * 2 < online:
        loops = idle_loops + 1
        if loops >= 2:
            target_vms = max(1, vm_count // 2)
            return "down", vm_count - target_vms, 0, False
        return "wait", 0, loops, False
    return "wait", 0, 0, False


def discover_offers(project: str, max_daily_usd: Decimal, top: int = 500):
    auth = get_auth(project)
    offers, _, _, errors = asyncio.run(
        find(auth, min_vcpu=32, min_ram=128, top=top, concurrency=200,
             billing_api_key=os.environ.get("GOOGLE_CLOUD_API_KEY"))
    )
    for error in errors[:5]:
        LOG.warning("Spot scan warning: %s", error)
    return eligible_offers(offers, max_daily_usd)


def eligible_offers(offers, max_daily_usd: Decimal):
    """Keep affordable offers with exactly 32 vCPU and 128 GiB of RAM."""
    exact_shapes = [offer for offer in offers if offer.vcpu == 32 and offer.ram_gib == 128]
    return affordable_offers(exact_shapes, max_daily_usd)


def runner_capacity(vcpu: int) -> int:
    if vcpu < 4:
        return 0
    return vcpu // 4


def launch_offer(offer, args: argparse.Namespace) -> None:
    """Try each zone until the manager confirms a complete runner deployment."""
    hourly_limit = args.max_daily_usd / Decimal(24)
    result = subprocess.run(
        ["gcloud", "compute", "zones", "list", f"--project={args.project}",
         f"--filter=name~'^{offer.region}-' AND status=UP", "--format=value(name)"],
        check=True, text=True, capture_output=True,
    )
    zones = result.stdout.split()
    if not zones:
        raise RuntimeError(f"No active Compute Engine zones found in {offer.region}")
    for zone in zones:
        letter = zone.rsplit("-", 1)[-1]
        suffix = f"{offer.region.replace('-', '')}-{letter}-{int(time.time())}"
        instance_name = f"oxidex-runners-{suffix}"
        runner_prefix = "gha"
        longest_name = maximum_runner_name_length(runner_prefix, instance_name, 8)
        if longest_name > 64:
            raise RuntimeError(f"Generated GitHub runner name would be {longest_name} characters (maximum 64)")
        command = [
            sys.executable, str(ROOT / "src" / "spot_runner_manager.py"),
            "--max-cost", str(hourly_limit), "--instance-type", offer.machine_type,
            "--region", offer.region, "--zone", zone,
            "--cpu", str(offer.vcpu), "--ram", str(int(offer.ram_gib)),
            "--instance-name", instance_name,
            "--disk-device-name", f"runner-data-{suffix}",
            "--disk-size-gb", str(args.disk_size_gb),
            "--runner-prefix", runner_prefix,
            "--runner-label", args.label, "--apply",
        ]
        LOG.info("Launching Spot VM %s: %s in %s at $%.4f/day compute",
                 instance_name, offer.machine_type, zone, offer.hourly_usd * 24)
        result = subprocess.run(command, check=False)
        if result.returncode == 0:
            LOG.info("Spot VM deployment completed: %s in %s; manager confirmed runner registration",
                     instance_name, zone)
            return
        LOG.warning("Deployment in %s failed; trying another zone", zone)
    raise RuntimeError(f"No zone accepted {offer.machine_type} in {offer.region}")


def install_launch_agent(args: argparse.Namespace) -> Path:
    """Install a per-user LaunchAgent that restarts the autoscaler after login."""
    import plistlib

    home = Path.home()
    launch_agents = home / "Library" / "LaunchAgents"
    launch_agents.mkdir(parents=True, exist_ok=True)
    plist_path = launch_agents / "com.swacktools.spot-runner-autoscaler.plist"
    plist = {
        "Label": "com.swacktools.spot-runner-autoscaler",
        "ProgramArguments": [sys.executable, str(ROOT / "src" / "spot_runner_autoscaler.py"),
                             "--max-daily-usd", str(args.max_daily_usd),
                             "--project", args.project, "--org", args.org,
                             "--interval-seconds", str(args.interval_seconds),
                             "--minimum-runners", str(args.minimum_runners),
                             "--minimum-docker-runners", str(args.minimum_docker_runners),
                             "--minimum-docker-hosts", str(args.minimum_docker_hosts),
                             "--disk-size-gb", str(args.disk_size_gb),
                             "--label", args.label, "--state-file", str(args.state_file.expanduser()),
                             "--log-file", str(args.log_file.expanduser())],
        "WorkingDirectory": str(ROOT),
        "RunAtLoad": True,
        "KeepAlive": True,
        "EnvironmentVariables": {
            "PATH": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        },
        "StandardOutPath": str(home / "Library" / "Logs" / "spot-runner-autoscaler.log"),
        "StandardErrorPath": str(home / "Library" / "Logs" / "spot-runner-autoscaler.log"),
    }
    plist_path.write_bytes(plistlib.dumps(plist))
    subprocess.run(["launchctl", "bootstrap", f"gui/{os.getuid()}", str(plist_path)], check=False)
    return plist_path


def args_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--org", default="swack-tools")
    parser.add_argument("--project", default="homelab-424523")
    parser.add_argument("--minimum-runners", type=int, default=25)
    parser.add_argument("--minimum-docker-runners", type=int, default=2)
    parser.add_argument("--minimum-docker-hosts", type=int, default=2)
    parser.add_argument("--label", default="spot")
    parser.add_argument("--max-daily-usd", type=Decimal, default=Decimal("5"))
    parser.add_argument("--interval-seconds", type=int, default=300)
    parser.add_argument("--disk-size-gb", type=int, default=100)
    parser.add_argument("--log-file", type=Path, default=Path.home() / "spot-ghrunner.log")
    parser.add_argument("--state-file", type=Path,
                        default=Path.home() / "Library" / "Application Support"
                        / "Spot Runner Autoscaler" / "state.json")
    parser.add_argument("--install-launch-agent", action="store_true")
    parser.add_argument("--once", action="store_true", help="Check and scale once, then exit")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = args_parser().parse_args(argv)
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    if (args.minimum_runners < 1 or args.minimum_docker_runners < 1
            or args.minimum_docker_hosts < 1 or args.interval_seconds < 1
            or args.disk_size_gb < 1):
        raise ValueError("runner floors, interval, and disk size must be positive")
    if args.max_daily_usd <= 0:
        raise ValueError("max daily VM cost must be positive")
    load_env(ROOT / ".env")
    if args.install_launch_agent:
        print(f"Installed LaunchAgent at {install_launch_agent(args)}")
        return 0

    with lifecycle_log(args.log_file):
        LOG.info("Checking fleet every %d seconds; compute cap $%s/day per VM",
                 args.interval_seconds, args.max_daily_usd)
        return run_controller(args)


def run_controller(args: argparse.Namespace) -> int:
    """Run fleet checks until interrupted or a single check completes."""
    state_path = args.state_file.expanduser()
    state = read_scale_state(state_path)
    while True:
        runners = list_github_runners(args.org)
        vms = list_gcloud_vms(args.project, name_prefix="oxidex-runners-")
        observe_vms(vms, state)
        removed_vm_names, removed_runner_ids = advance_vm_drains(vms, runners, args, state)
        if removed_vm_names:
            vms = [vm for vm in vms if vm.name not in removed_vm_names]
            runners = [runner for runner in runners if runner.runner_id not in removed_runner_ids]
        write_scale_state(state_path, state)
        fleet_vms = spot_instances(vms)
        for vm_name, (host_busy, host_online, host_assigned) in host_utilization(
                fleet_vms, runners).items():
            LOG.info("Spot VM %s utilization: %d/%d online runners busy (%d assigned)",
                     vm_name, host_busy, host_online, host_assigned)
        current, busy = utilization_counts_with_draining(
            runners, args.label, set(state.get("draining_vms", {})), fleet_vms)
        if reconcile_prices(vms, runners, args, state, state_path):
            if args.once:
                return 0
            time.sleep(args.interval_seconds)
            continue
        docker_runners = [runner for runner in runners
                          if runner.online and DOCKER_LABEL in runner.labels
                          and "spot" not in runner.labels]
        docker_count = len(docker_runners)
        docker_hosts = len(docker_pool_hosts(docker_runners))
        LOG.info("Managed GCP runners: %d online, %d busy; %d Spot VMs",
                 current, busy, len(fleet_vms))
        LOG.info("Online Docker runners: %d across %d hosts; targets %d runners across %d hosts",
                 docker_count, docker_hosts, args.minimum_docker_runners, args.minimum_docker_hosts)
        action, host_count, idle_loops, scale_up_active = scaling_decision(
            current, busy, len(fleet_vms), state["idle_loops"],
            state["scale_up_active"], args.minimum_runners)
        if state["scale_up_active"] and not scale_up_active and action == "hold":
            LOG.info("Scale-up complete: online capacity is above %d runners", args.minimum_runners)
        state["scale_up_active"] = scale_up_active

        docker_short = not docker_pool_satisfied(
            docker_count, docker_hosts, args.minimum_docker_runners, args.minimum_docker_hosts)
        if action == "up":
            state["idle_loops"] = idle_loops
            if host_count:
                LOG.info("High usage started a doubling step: launching %d hosts", host_count)
                offers = discover_offers(args.project, args.max_daily_usd)
                launched = scale_up(host_count, offers, args) if offers else 0
                if not launched:
                    LOG.error("Could not launch Spot capacity below $%s/day", args.max_daily_usd)
        elif docker_short:
            state["idle_loops"] = 0
            LOG.warning("Docker runner floor is short; launching one replacement host")
            offers = discover_offers(args.project, args.max_daily_usd)
            if offers and not scale_up(1, offers, args):
                LOG.error("Could not restore the Docker runner floor")
        elif action == "down":
            state["idle_loops"] = idle_loops
            LOG.info("Low usage persisted for two checks; draining up to %d Spot hosts", host_count)
            draining = scale_down(vms, runners, args, state)
            LOG.info("Scale-down step started draining %d hosts", draining)
        else:
            state["idle_loops"] = idle_loops
            if action == "wait" and idle_loops:
                LOG.info("Low usage observed in loop %d of 2", idle_loops)
        write_scale_state(state_path, state)
        if args.once:
            return 0
        time.sleep(args.interval_seconds)


if __name__ == "__main__":
    raise SystemExit(main())
