"""Read-only Compute Engine inventory of the runner hosts and dedicated builders the dashboard shows.

The dashboard's instance list comes only from this listing: :func:`gce_fleet_source` lists every instance in the
project once (one aggregated call, plus one aggregated disk listing and a cached machine-type lookup per shape),
keeps the ``oxidex-runners-*`` and ``builder-*`` VMs, and returns one normalized observation per VM. Nothing here
creates, changes, or deletes a cloud resource.
"""

from __future__ import annotations

from datetime import datetime
import os
import re
from typing import Any, Callable, Mapping

from .runner_fleet import gce_metadata_items, runner_deployment_from_gce_item

ENV_FLEET_INVENTORY = "DASHBOARD_FLEET_INVENTORY"
# Runner hosts (runner_manager) and dedicated Rust builders (render_builder_startup). Control VMs
# (``spot-control-*``) and anything else in the project are never part of the fleet view.
FLEET_NAME_PREFIXES = ("oxidex-runners-", "builder-")
# Dedicated builders run one build container capped at 16 vCPU / 28 GiB (see AGENTS.md).
BUILDER_CONTAINER_VCPU = 16
BUILDER_CONTAINER_RAM_GIB = 28.0
DEFAULT_DISK_TYPE = "pd-balanced"

_MACHINE_VCPU_RE = re.compile(r"-(\d+)$")


def gce_epoch(value: Any) -> float | None:
    """Epoch seconds of a GCE RFC 3339 timestamp (``2026-10-18T09:12:33.123-07:00``), or None."""
    text = str(value or "").strip()
    if not text or len(text) > 40:
        return None
    try:
        parsed = datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.timestamp()


def fleet_role(name: str) -> str | None:
    """``cicd_runner`` or ``rust_builder`` for a fleet VM name, None for anything else."""
    if name.startswith("builder-"):
        return "rust_builder"
    if name.startswith("oxidex-runners-"):
        return "cicd_runner"
    return None


_CUSTOM_MACHINE_VCPU_RE = re.compile(r"custom-(\d+)-\d+(?:-ext)?$")


def _vcpu_from_machine_type(machine_type: str) -> int:
    """vCPUs encoded in a machine type name (0 when unknown): ``n4-standard-16`` -> 16, ``n2-custom-32-112640`` -> 32."""
    if "custom" in machine_type:
        custom = _CUSTOM_MACHINE_VCPU_RE.search(machine_type)
        return int(custom.group(1)) if custom else 0
    match = _MACHINE_VCPU_RE.search(machine_type)
    return int(match.group(1)) if match else 0


def observe_fleet_items(
    items: list[dict],
    *,
    disk_types: Mapping[str, str] | None = None,
    shapes: Mapping[tuple[str, str], Mapping[str, Any]] | None = None,
) -> list[dict[str, Any]]:
    """Normalize a Compute Engine aggregated instance listing into fleet observations.

    Each observation carries the VM's identity (``instance_id``, ``name``, ``zone``, ``region``), its GCE
    ``status`` verbatim, its shape and disks, and its lifetime timestamps as epoch seconds. Shapes come from
    ``shapes[(zone, machine_type)]`` (``{"vcpu", "ram_gib"}``); when unknown, vCPU is read from the machine type
    name and ``specs_known`` is False. Disk types come from ``disk_types[selfLink]``.
    """
    disk_types = disk_types or {}
    shapes = shapes or {}
    observed: list[dict[str, Any]] = []
    for item in items:
        if not isinstance(item, dict) or not isinstance(item.get("name"), str):
            continue
        name = item["name"]
        role = fleet_role(name)
        if role is None:
            continue
        instance_id = str(item.get("id", "") or "").strip()
        if not instance_id:
            continue
        zone = str(item.get("zone", "")).rsplit("/", 1)[-1]
        machine_type = str(item.get("machineType", "")).rsplit("/", 1)[-1]
        shape = shapes.get((zone, machine_type)) or {}
        vcpu = shape.get("vcpu")
        ram_gib = shape.get("ram_gib")
        specs_known = isinstance(vcpu, int) and isinstance(ram_gib, (int, float))
        if not specs_known:
            vcpu, ram_gib = _vcpu_from_machine_type(machine_type), 0.0
        boot_disk_type, boot_disk_gb = DEFAULT_DISK_TYPE, 0
        data_disk_type, data_disk_gb = DEFAULT_DISK_TYPE, 0
        for disk in item.get("disks", []) if isinstance(item.get("disks"), list) else []:
            if not isinstance(disk, dict):
                continue
            size_raw = str(disk.get("diskSizeGb", "") or "")
            size = int(size_raw) if size_raw.isdigit() else 0
            dtype = disk_types.get(str(disk.get("source", "")), "") or DEFAULT_DISK_TYPE
            if disk.get("boot"):
                boot_disk_type, boot_disk_gb = dtype, size
            else:
                # Several data disks: report the type of the first and their total size.
                if data_disk_gb == 0:
                    data_disk_type = dtype
                data_disk_gb += size
        scheduling = item.get("scheduling") if isinstance(item.get("scheduling"), dict) else {}
        spot = (str(scheduling.get("provisioningModel", "")).upper() == "SPOT"
                or bool(scheduling.get("preemptible", False)))
        deployment = runner_deployment_from_gce_item(item)
        metadata = gce_metadata_items(item)
        if role == "rust_builder":
            runners_count, runner_vcpu, runner_ram_gib = 1, BUILDER_CONTAINER_VCPU, BUILDER_CONTAINER_RAM_GIB
            runner_limits_known = True
        else:
            # Instance metadata records the listener count only; per-job limits depend on the tier and admission
            # (listeners are not memory capacity), so they are unknown here rather than guessed.
            runners_count = int(deployment.get("runners_count", 0) or 0)
            runner_vcpu, runner_ram_gib, runner_limits_known = 0, 0.0, False
        runners_count_known = role == "rust_builder" or "runners_count" in deployment
        observed.append({
            "instance_id": instance_id,
            "name": name,
            "role": role,
            "status": str(item.get("status", "UNKNOWN") or "UNKNOWN").upper(),
            "zone": zone,
            "region": zone.rsplit("-", 1)[0] if "-" in zone else zone,
            "machine_type": machine_type,
            "cpu_platform": str(item.get("cpuPlatform", "") or ""),
            "vcpu": int(vcpu or 0),
            "ram_gib": float(ram_gib or 0.0),
            "specs_known": specs_known,
            "spot": spot,
            "boot_disk_type": boot_disk_type,
            "boot_disk_gb": boot_disk_gb,
            "data_disk_type": data_disk_type,
            "data_disk_gb": data_disk_gb,
            "runners_count": runners_count,
            "runner_vcpu": runner_vcpu,
            "runner_ram_gib": runner_ram_gib,
            "runner_limits_known": runner_limits_known,
            "runners_count_known": runners_count_known,
            "runner_label": str(deployment.get("runner_label", "") or ""),
            "target_label": str((item.get("labels") or {}).get("spot-target", "")
                                if isinstance(item.get("labels"), dict) else ""),
            "created_epoch": gce_epoch(item.get("creationTimestamp")),
            "last_start_epoch": gce_epoch(item.get("lastStartTimestamp")),
            "last_stop_epoch": gce_epoch(item.get("lastStopTimestamp")),
            "patch_target": metadata.get("spot-runner-patch-target", ""),
        })
    return observed


def gce_fleet_source(project: str) -> Callable[[], list[dict[str, Any]]]:
    """A callable that lists the live fleet from Compute Engine (gcloud credentials or the metadata server).

    It raises when any zone is unreachable: a partial listing must never make a VM look deleted.
    """
    shape_cache: dict[tuple[str, str], dict[str, Any]] = {}
    disk_listing = {"ok_before": False}

    def source() -> list[dict[str, Any]]:
        import asyncio

        from . import compute_api

        async def listing() -> list[dict[str, Any]]:
            auth = compute_api.get_auth(project)
            api = compute_api.AsyncGoogleApi(auth.token, 4)
            try:
                items, unreachables = await compute_api.list_instances(
                    api, auth.project, instance_fields=compute_api.FLEET_INSTANCE_LIST_FIELDS)
                if unreachables:
                    raise RuntimeError(f"zones unreachable: {', '.join(unreachables[:5])}")
                fleet_items = [i for i in items if isinstance(i, dict)
                               and str(i.get("name", "")).startswith(FLEET_NAME_PREFIXES)]
                disk_types: dict[str, str] = {}
                try:
                    if fleet_items:
                        disk_types = await compute_api.list_disk_types(api, auth.project)
                        disk_listing["ok_before"] = True
                except Exception as exc:
                    if disk_listing["ok_before"]:
                        # Transient: fail the whole listing so known disk types are not replaced by defaults.
                        raise RuntimeError(f"disk listing failed: {type(exc).__name__}: {exc}") from exc
                    # Never worked (for example no compute.disks.list): disk types fall back to the default.
                wanted = {(str(i.get("zone", "")).rsplit("/", 1)[-1], str(i.get("machineType", "")).rsplit("/", 1)[-1])
                          for i in fleet_items}
                for zone, machine_type in wanted - shape_cache.keys():
                    if not zone or not machine_type:
                        continue
                    try:
                        shape_cache[(zone, machine_type)] = await compute_api.machine_type_capacity(
                            api, auth.project, zone, machine_type)
                    except Exception:  # noqa: BLE001 - reported below, retried next time
                        pass
                missing = sorted(f"{mt} in {zone}" for zone, mt in wanted - shape_cache.keys() if zone and mt)
                if missing:
                    # Unknown RAM would price as free: fail the refresh (records stay as they were) instead.
                    raise RuntimeError(f"machine type capacity unavailable: {', '.join(missing[:5])}")
                return observe_fleet_items(fleet_items, disk_types=disk_types, shapes=shape_cache)
            finally:
                while not api.connections.empty():
                    _reader, writer = api.connections.get_nowait()
                    writer.close()

        return asyncio.run(listing())

    return source


def fleet_source_from_env(project: str, env: Mapping[str, str] | None = None) -> Callable[[], list[dict[str, Any]]] | None:
    """The live fleet source when ``DASHBOARD_FLEET_INVENTORY=gce`` (set on the control VM), else None."""
    env = os.environ if env is None else env
    return gce_fleet_source(project) if str(env.get(ENV_FLEET_INVENTORY, "") or "").strip().lower() == "gce" else None
