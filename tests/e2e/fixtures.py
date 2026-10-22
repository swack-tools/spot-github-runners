"""Authoritative test fixtures, test vectors, and contract helpers for E2E testing."""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import time
from typing import Any

# Interface Contract 2: Session Token Specification
SESSION_PURPOSE = "spot-dashboard/session/v1"
COOKIE_NAME = "__Host-spot_session"
DEFAULT_TEST_SECRET = "test-e2e-secret-key-purpose-derived-session-auth-token-32b"
SWACKHAMER_USER_ID = 619624
SWACKHAMER_LOGIN = "swackhamer"

# Hardware floor constraints
MIN_HOST_VCPU = 16
MIN_HOST_RAM_GIB = 32
REQUIRED_RUNNERS_COUNT = 2
REQUIRED_RUNNER_VCPU = 7
REQUIRED_RUNNER_RAM_GIB = 14
REQUIRED_HOST_OVERHEAD_GIB = 4
MIN_QUEUED_JOBS_THRESHOLD = 20

PERMITTED_MACHINE_TYPES = (
    "c4d-highcpu-16",
    "n4d-highcpu-16",
    "n4-highcpu-16",
    "c2d-highcpu-16",
)

CONTROLLED_TARGETS = (
    "swack-tools",
    "xbmc4lyfe/newznab-tmux",
    "xbmc4lyfe/rustnzb",
    "xbmc4lyfe/nntp-proxy",
)

FORBIDDEN_MOUNTS = (
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/run/gha-admission",
    "/var/lib/gha-admission",
    "/etc",
    "/root",
    "/proc",
    "/sys",
    "/dev",
)


def derive_session_key(secret: str) -> bytes:
    """Derive session HMAC signing key using purpose label: HMAC-SHA256(secret, purpose)."""
    return hmac.new(
        secret.encode("utf-8"),
        SESSION_PURPOSE.encode("utf-8"),
        hashlib.sha256,
    ).digest()


def create_session_token(
    payload: dict[str, Any],
    secret: str = DEFAULT_TEST_SECRET,
) -> str:
    """Create a signed session token according to Interface Contract 2:

    Format: base64url_nopad(compact_sorted_json) + "." + hex(HMAC-SHA256(derived_key, body_ascii))
    """
    compact_json = json.dumps(payload, sort_keys=True, separators=(",", ":"))
    body_b64 = base64.urlsafe_b64encode(compact_json.encode("utf-8")).decode("ascii").rstrip("=")
    derived_key = derive_session_key(secret)
    sig_hex = hmac.new(derived_key, body_b64.encode("ascii"), hashlib.sha256).hexdigest()
    return f"{body_b64}.{sig_hex}"


def verify_session_token(
    token: str,
    secret: str = DEFAULT_TEST_SECRET,
    not_before: int = 0,
    current_time: int | None = None,
) -> tuple[bool, dict[str, Any] | None, str]:
    """Verify session token against derived key, expiration, and bulk revocation cut-off."""
    if not token or "." not in token:
        return False, None, "malformed_token_format"
    
    parts = token.split(".")
    if len(parts) != 2:
        return False, None, "invalid_token_segments"
    
    body_b64, sig_hex = parts
    derived_key = derive_session_key(secret)
    expected_sig = hmac.new(derived_key, body_b64.encode("ascii"), hashlib.sha256).hexdigest()
    
    if not hmac.compare_digest(sig_hex, expected_sig):
        return False, None, "signature_mismatch"
    
    # Add back padding
    padded_b64 = body_b64 + "=" * (-len(body_b64) % 4)
    try:
        raw_json = base64.urlsafe_b64decode(padded_b64.encode("ascii")).decode("utf-8")
        payload = json.loads(raw_json)
    except Exception:
        return False, None, "invalid_json_body"
    
    now = int(time.time()) if current_time is None else current_time
    
    # Check expiration
    exp = payload.get("exp")
    if exp is not None and now > exp:
        return False, payload, "token_expired"
    
    # Check bulk revocation cut-off (iat < not_before)
    iat = payload.get("iat", 0)
    if iat < not_before:
        return False, payload, "token_revoked_by_cutoff"
    
    return True, payload, "valid"


def compute_canonical_patch_sha256(manifest: dict[str, Any]) -> str:
    """Compute canonical patch_sha256 parity digest matching src/lib/runner_patch.py."""
    import copy
    payload = copy.deepcopy(manifest)
    payload.pop("source_commit", None)
    payload.pop("source_branch", None)
    hw = payload.get("hardware")
    if isinstance(hw, dict):
        for opt_key in (
            "runners_count",
            "runner_vcpu",
            "runner_ram_gib",
            "host_overhead_gib",
            "queued_jobs_threshold",
        ):
            if hw.get(opt_key) is None:
                hw.pop(opt_key, None)
    raw = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return hashlib.sha256(raw).hexdigest()


def make_valid_manifest(
    repo: str = "swack-tools/oxidex",
    machine_type: str = "c4d-highcpu-16",
    queued_jobs_threshold: int = 20,
) -> dict[str, Any]:
    """Create a fully valid runner patch manifest satisfying all hardware sizing floor rules."""
    return {
        "schema": "spot-runner-patch-v1",
        "repository": repo,
        "hardware": {
            "arch": "x86_64",
            "isa": ["avx2"],
            "extra_labels": [f"type-{machine_type}"],
            "min_vcpu": MIN_HOST_VCPU,
            "min_ram_gib": MIN_HOST_RAM_GIB,
            "runners_count": REQUIRED_RUNNERS_COUNT,
            "runner_vcpu": REQUIRED_RUNNER_VCPU,
            "runner_ram_gib": REQUIRED_RUNNER_RAM_GIB,
            "host_overhead_gib": REQUIRED_HOST_OVERHEAD_GIB,
            "queued_jobs_threshold": queued_jobs_threshold,
            "qualify_command": "",
        },
        "host": {
            "apt_packages": ["git", "curl"],
            "sysctl": {"net.core.somaxconn": "4096"},
            "files": [],
            "boot_mkdirs": [],
            "boot_script": "",
        },
        "container": {
            "apt_packages": ["build-essential", "pkg-config"],
            "sysctl": {},
            "mounts": [],
            "files": [],
            "dockerfile_run": "",
        },
    }


def make_mock_overview_payload(
    project: str = "homelab-424523",
    total_runners: int = 4,
    active_instances: int = 2,
) -> dict[str, Any]:
    """Create an OverviewPayload snapshot matching Interface Contract 1."""
    now = int(time.time())
    return {
        "schema": "spot-dashboard-snapshot-v1",
        "state_version": 1,
        "writer_slot": "edge",
        "written_at": now,
        "state": {
            "project": project,
            "org": "swack-tools",
            "updated_at": now,
            "global_budget": {
                "daily_cap_usd": 25.0,
                "current_daily_spend_usd": 3.84,
                "spot_discount_pct": 68.5,
            },
            "profiles": {
                "default": {
                    "machine_types": list(PERMITTED_MACHINE_TYPES),
                    "target_runners": total_runners,
                }
            },
            "scaling": {
                "enabled": True,
                "queued_jobs_threshold": 20,
                "idle_runner_scale_down_minutes": 15,
                "floor_targets": list(CONTROLLED_TARGETS),
            },
            "instances_view": [
                {
                    "name": f"oxidex-runners-{i}",
                    "zone": "us-central1-a",
                    "machine_type": "c4d-highcpu-16",
                    "status": "RUNNING",
                    "vcpu": 16,
                    "ram_gib": 32,
                    "runners_count": 2,
                    "created_at": now - 3600,
                }
                for i in range(1, active_instances + 1)
            ],
            "fleet_summary": {
                "total_instances": active_instances,
                "total_runners": total_runners,
                "online_runners": total_runners,
                "busy_runners": 1,
                "queued_jobs": 0,
            },
            "fleet_inventory": {
                "by_zone": {"us-central1-a": active_instances},
                "by_machine_type": {"c4d-highcpu-16": active_instances},
            },
            "rust_builds": [],
        },
    }


def make_gcp_service_account_fixture() -> dict[str, Any]:
    """Mock Service Account JSON key."""
    return {
        "type": "service_account",
        "project_id": "homelab-424523",
        "private_key_id": "abc1234567890abcdef1234567890abcdef12",
        "private_key": "-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC3testfakekey1234\n-----END PRIVATE KEY-----\n",
        "client_email": "spot-fleet-controller@homelab-424523.iam.gserviceaccount.com",
        "client_id": "109876543210987654321",
        "auth_uri": "https://accounts.google.com/o/oauth2/auth",
        "token_uri": "https://oauth2.googleapis.com/token",
        "auth_provider_x509_cert_url": "https://www.googleapis.com/oauth2/v1/certs",
        "client_x509_cert_url": "https://www.googleapis.com/robot/v1/metadata/x509/spot-fleet-controller%40homelab-424523.iam.gserviceaccount.com",
    }
