"""Contract-compliant HTTP Reference/Mock Server for E2E testing of the Rust Edge Worker.

Implements all HTTP API routes, session verification, state store, reconciler simulation,
and safety gates according to PROJECT.md § Interface Contracts.
"""

from __future__ import annotations

import base64
import copy
import hashlib
import hmac
import html as html_lib
import http.server
import json
import re
import socketserver
import threading
import time
import urllib.parse
from typing import Any

from fixtures import (
    COOKIE_NAME,
    CONTROLLED_TARGETS,
    DEFAULT_TEST_SECRET,
    FORBIDDEN_MOUNTS,
    MIN_HOST_RAM_GIB,
    MIN_HOST_VCPU,
    MIN_QUEUED_JOBS_THRESHOLD,
    PERMITTED_MACHINE_TYPES,
    REQUIRED_HOST_OVERHEAD_GIB,
    REQUIRED_RUNNERS_COUNT,
    REQUIRED_RUNNER_RAM_GIB,
    REQUIRED_RUNNER_VCPU,
    SWACKHAMER_LOGIN,
    SWACKHAMER_USER_ID,
    compute_canonical_patch_sha256,
    create_session_token,
    make_mock_overview_payload,
    make_valid_manifest,
    verify_session_token,
)


class EdgeStateStore:
    """Thread-safe simulated SPOT_KV and control plane state store."""

    def __init__(self, secret: str = DEFAULT_TEST_SECRET):
        self.lock = threading.Lock()
        self.secret = secret
        self.reset()

    def reset(self) -> None:
        with self.lock:
            self.session_not_before = 0
            self.admins = {
                str(SWACKHAMER_USER_ID): {
                    "id": SWACKHAMER_USER_ID,
                    "login": SWACKHAMER_LOGIN,
                    "role": "owner",
                    "added_at": int(time.time()),
                }
            }
            self.scaling_policy = {
                "enabled": True,
                "queued_jobs_threshold": 20,
                "idle_runner_scale_down_minutes": 15,
                "floor_targets": list(CONTROLLED_TARGETS),
            }
            self.overview = make_mock_overview_payload()
            self.manifests = {
                target: make_valid_manifest(repo=target)
                for target in CONTROLLED_TARGETS
            }
            self.device_codes: dict[str, dict[str, Any]] = {}
            self.oauth_states: dict[str, dict[str, Any]] = {}
            self.gcp_oauth_token = "ya29.mock-cached-gcp-oauth-token-valid-3600"
            self.gcp_oauth_token_exp = int(time.time()) + 3600
            self.reconciler_lock: dict[str, Any] | None = None
            self.terminated_vms: list[str] = []
            self.stale_runners: list[str] = []
            self.reconciler_tick_count = 0
            self.kv_puts = 0
            self.kv_gets = 0
            self.isolate_cache_hits = 0
            self.isolate_cache_expires_at = 0.0
            self.cached_state_json: str | None = None
            self.last_put_hash: str | None = None


class EdgeContractHandler(http.server.BaseHTTPRequestHandler):
    """HTTP request handler implementing the Edge Worker's API and UI contract."""

    store: EdgeStateStore
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: Any) -> None:
        # Suppress noisy standard HTTP access logs during test runs
        pass

    def _parse_cookies(self) -> dict[str, str]:
        cookies: dict[str, str] = {}
        raw = self.headers.get("Cookie")
        if raw:
            for part in raw.split(";"):
                part = part.strip()
                if "=" in part:
                    k, v = part.split("=", 1)
                    cookies[k.strip()] = v.strip()
        return cookies

    def _get_authenticated_user(self) -> tuple[bool, dict[str, Any] | None, str]:
        cookies = self._parse_cookies()
        token = cookies.get(COOKIE_NAME)
        if not token:
            return False, None, "missing_session_cookie"
        valid, payload, reason = verify_session_token(
            token,
            secret=self.store.secret,
            not_before=self.store.session_not_before,
        )
        return valid, payload, reason

    def _is_admin(self, user: dict[str, Any] | None) -> bool:
        if not user:
            return False
        user_id = str(user.get("sub", ""))
        return user_id == str(SWACKHAMER_USER_ID) or user_id in self.store.admins

    def _send_json(self, status_code: int, data: Any, extra_headers: list[tuple[str, str]] | None = None) -> None:
        body = json.dumps(data, indent=2).encode("utf-8")
        self.send_response(status_code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.send_header("Cache-Control", "no-store, no-cache, must-revalidate")
        if extra_headers:
            for k, v in extra_headers:
                self.send_header(k, v)
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def _send_html(self, status_code: int, html: str, extra_headers: list[tuple[str, str]] | None = None) -> None:
        body = html.encode("utf-8")
        self.send_response(status_code)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        if extra_headers:
            for k, v in extra_headers:
                self.send_header(k, v)
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def _read_json_body(self) -> dict[str, Any]:
        length = int(self.headers.get("Content-Length", 0))
        if length <= 0:
            return {}
        raw = self.rfile.read(length)
        return json.loads(raw.decode("utf-8"))

    def do_GET(self) -> None:
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path
        query = urllib.parse.parse_qs(parsed.query)

        # 1. Dashboard UI / Login Gate
        if path == "/":
            authenticated, user, _ = self._get_authenticated_user()
            if authenticated and user:
                html = (
                    "<!DOCTYPE html>\n"
                    "<html lang=\"en\">\n"
                    "<head><title>Spot Runner Fleet</title></head>\n"
                    "<body>\n"
                    "<div id=\"dashboard-app\">\n"
                    "  <h1>Spot Runner Fleet Dashboard</h1>\n"
                    f"  <p id=\"user-badge\">Logged in as {html_lib.escape(str(user.get('login', '')))}</p>\n"
                    "  <div id=\"fleet-summary\">Summary</div>\n"
                    "  <div id=\"instances-table\">Instances</div>\n"
                    "  <div id=\"scaling-config\">Scaling</div>\n"
                    "  <div id=\"patch-console\">Patch Console</div>\n"
                    "</div>\n"
                    "</body></html>"
                )
            else:
                html = (
                    "<!DOCTYPE html>\n"
                    "<html lang=\"en\">\n"
                    "<head><title>Spot Runners - Sign In</title></head>\n"
                    "<body>\n"
                    "<div id=\"login-gate\">\n"
                    "  <h2>Sign in to Spot GitHub Runners</h2>\n"
                    "  <a id=\"login-btn\" href=\"/auth/github/login\">Sign in with GitHub</a>\n"
                    "  <div id=\"device-flow\">Device Flow</div>\n"
                    "</div>\n"
                    "</body></html>"
                )
            self._send_html(200, html)
            return

        # 2. OAuth Start
        if path == "/auth/github/login":
            state = hashlib.sha256(f"state-{time.time()}".encode()).hexdigest()[:32]
            code_verifier = "e2e-pkce-code-verifier-long-string-high-entropy-12345678"
            code_challenge = base64.urlsafe_b64encode(
                hashlib.sha256(code_verifier.encode()).digest()
            ).decode("ascii").rstrip("=")
            with self.store.lock:
                self.store.oauth_states[state] = {
                    "code_verifier": code_verifier,
                    "created_at": time.time(),
                }
            gh_url = (
                "https://github.com/login/oauth/authorize?"
                f"client_id=mock-spot-client-id&"
                f"redirect_uri=https%3A%2F%2Fspot.swacktech.com%2Fauth%2Fgithub%2Fcallback&"
                f"scope=read%3Aorg%2Crepo&"
                f"state={state}&"
                f"code_challenge={code_challenge}&"
                f"code_challenge_method=S256"
            )
            self.send_response(302)
            self.send_header("Location", gh_url)
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()
            return

        # 3. OAuth Callback
        if path == "/auth/github/callback":
            code = query.get("code", [""])[0]
            state = query.get("state", [""])[0]
            with self.store.lock:
                stored = self.store.oauth_states.pop(state, None)
            if not stored or not code:
                self._send_json(400, {"error": "invalid_or_expired_oauth_state"})
                return
            # Mint authenticated session for swackhamer
            payload = {
                "sub": str(SWACKHAMER_USER_ID),
                "login": SWACKHAMER_LOGIN,
                "role": "owner",
                "iat": int(time.time()),
                "exp": int(time.time()) + 604800,
            }
            token = create_session_token(payload, secret=self.store.secret)
            cookie_hdr = (
                f"{COOKIE_NAME}={token}; Path=/; Max-Age=604800; HttpOnly; Secure; SameSite=Lax"
            )
            self.send_response(302)
            self.send_header("Location", "/")
            self.send_header("Set-Cookie", cookie_hdr)
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()
            return

        # 4. Auth Status
        if path == "/api/auth/status":
            authenticated, user, reason = self._get_authenticated_user()
            if not authenticated or not user:
                self._send_json(200, {"authenticated": False, "reason": reason})
                return
            user_id = str(user.get("sub", ""))
            is_admin = user_id == str(SWACKHAMER_USER_ID) or user_id in self.store.admins
            self._send_json(200, {
                "authenticated": True,
                "user_id": int(user_id) if user_id.isdigit() else user_id,
                "login": user.get("login"),
                "role": user.get("role", "member"),
                "can_admin": is_admin,
            })
            return

        # 5. Health Probe
        if path == "/api/health":
            with self.store.lock:
                self._send_json(200, {
                    "status": "healthy",
                    "hydrated": True,
                    "state_version": 1,
                    "edge_worker": "spot-dashboard-edge",
                    "reconciler_ticks": self.store.reconciler_tick_count,
                })
            return

        # 6. Edge Overview (with 60s in-isolate memory cache fronting SPOT_KV)
        if path == "/api/overview":
            with self.store.lock:
                now = time.time()
                if self.store.cached_state_json is not None and now < self.store.isolate_cache_expires_at:
                    self.store.isolate_cache_hits += 1
                    overview = json.loads(self.store.cached_state_json)
                else:
                    self.store.kv_gets += 1
                    overview = copy.deepcopy(self.store.overview)
                    self.store.cached_state_json = json.dumps(overview)
                    self.store.isolate_cache_expires_at = now + 60.0
            self._send_json(200, overview)
            return

        # 7. Scaling Policy Query
        if path == "/api/scaling":
            with self.store.lock:
                scaling = copy.deepcopy(self.store.scaling_policy)
            self._send_json(200, scaling)
            return

        # 8. Admin Management - List
        if path == "/api/admins":
            authenticated, user, _ = self._get_authenticated_user()
            if not authenticated:
                self._send_json(401, {"error": "unauthorized"})
                return
            with self.store.lock:
                admin_list = list(self.store.admins.values())
            self._send_json(200, {"admins": admin_list})
            return

        # 9. Patch Console Bootstrap
        if path == "/api/patch-console/bootstrap":
            authenticated, user, _ = self._get_authenticated_user()
            if not authenticated:
                self._send_json(401, {"error": "unauthorized"})
                return
            self._send_json(200, {
                "repository": "swack-tools/spot-runners-config",
                "controlled_targets": list(CONTROLLED_TARGETS),
            })
            return

        # 10. Patch Console Load
        if path == "/api/patch-console/manifest":
            authenticated, user, _ = self._get_authenticated_user()
            if not authenticated:
                self._send_json(401, {"error": "unauthorized"})
                return
            repo = query.get("repo", [""])[0]
            if not repo or repo not in CONTROLLED_TARGETS:
                self._send_json(400, {"error": "unsupported_or_missing_repository", "repo": repo})
                return
            with self.store.lock:
                manifest = self.store.manifests.get(repo, make_valid_manifest(repo=repo))
            digest = compute_canonical_patch_sha256(manifest)
            self._send_json(200, {
                "repository": repo,
                "manifest": manifest,
                "base_sha": "a" * 40,
                "blob_sha": "b" * 40,
                "patch_sha256": digest,
            })
        # Test Helper: KV Stats & Cache telemetry
        if path == "/api/test/kv-stats":
            with self.store.lock:
                self._send_json(200, {
                    "kv_puts": self.store.kv_puts,
                    "kv_gets": self.store.kv_gets,
                    "isolate_cache_hits": self.store.isolate_cache_hits,
                })
            return

        self._send_json(404, {"error": "not_found", "path": path})

    def do_POST(self) -> None:
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path

        # 1. Device Flow Code
        if path == "/auth/device/code":
            device_code = hashlib.sha256(f"dev-{time.time()}".encode()).hexdigest()
            user_code = "WDJB-MJHT"
            with self.store.lock:
                self.store.device_codes[device_code] = {
                    "user_code": user_code,
                    "authorized": False,
                    "created_at": time.time(),
                    "expires_at": time.time() + 900,
                }
            self._send_json(200, {
                "device_code": device_code,
                "user_code": user_code,
                "verification_uri": "https://github.com/login/device",
                "expires_in": 900,
                "interval": 5,
            })
            return

        # 2. Device Flow Poll
        if path == "/auth/device/poll":
            data = self._read_json_body()
            device_code = data.get("device_code", "")
            with self.store.lock:
                dev = self.store.device_codes.get(device_code)
                if not dev:
                    self._send_json(400, {"error": "invalid_device_code"})
                    return
                if not dev["authorized"]:
                    self._send_json(400, {"error": "authorization_pending"})
                    return
                # Authorized -> Issue session
                payload = {
                    "sub": str(SWACKHAMER_USER_ID),
                    "login": SWACKHAMER_LOGIN,
                    "role": "owner",
                    "iat": int(time.time()),
                    "exp": int(time.time()) + 604800,
                }
                token = create_session_token(payload, secret=self.store.secret)
                cookie_hdr = (
                    f"{COOKIE_NAME}={token}; Path=/; Max-Age=604800; HttpOnly; Secure; SameSite=Lax"
                )
                self._send_json(200, {
                    "status": "authorized",
                    "access_token": "ghu_mock_token_12345",
                }, extra_headers=[("Set-Cookie", cookie_hdr)])
            return

        # 3. Session Logout
        if path == "/auth/logout":
            cookie_hdr = (
                f"{COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax; "
                "Expires=Thu, 01 Jan 1970 00:00:00 GMT"
            )
            self._send_json(200, {"status": "logged_out"}, extra_headers=[("Set-Cookie", cookie_hdr)])
            return

        # 4. Scaling Policy Update
        if path == "/api/scaling":
            authenticated, user, _ = self._get_authenticated_user()
            if not authenticated:
                self._send_json(401, {"error": "unauthorized"})
                return
            if not self._is_admin(user):
                self._send_json(403, {"error": "forbidden", "message": "Admin privileges required"})
                return
            data = self._read_json_body()
            # Validate queue threshold floor (>=20)
            threshold = data.get("queued_jobs_threshold")
            if threshold is not None and threshold < MIN_QUEUED_JOBS_THRESHOLD:
                self._send_json(400, {
                    "error": "invalid_queued_jobs_threshold",
                    "message": f"queued_jobs_threshold must be >= {MIN_QUEUED_JOBS_THRESHOLD}",
                })
                return
            with self.store.lock:
                self.store.scaling_policy.update(data)
                updated = copy.deepcopy(self.store.scaling_policy)
            self._send_json(200, {"status": "updated", "scaling": updated})
            return

        # 5. Admin Management - Add
        if path == "/api/admins":
            authenticated, user, _ = self._get_authenticated_user()
            if not authenticated:
                self._send_json(401, {"error": "unauthorized"})
                return
            if str(user.get("sub")) != str(SWACKHAMER_USER_ID):
                self._send_json(403, {"error": "forbidden", "message": "Only owner can manage admins"})
                return
            data = self._read_json_body()
            admin_id = str(data.get("id", ""))
            login = data.get("login", "")
            if not admin_id or not login:
                self._send_json(400, {"error": "missing_id_or_login"})
                return
            with self.store.lock:
                if len(self.store.admins) >= 50 and admin_id not in self.store.admins:
                    self._send_json(400, {"error": "admin_capacity_exceeded", "limit": 50})
                    return
                admin_obj = {
                    "id": int(admin_id) if admin_id.isdigit() else admin_id,
                    "login": login,
                    "role": data.get("role", "admin"),
                    "added_at": int(time.time()),
                }
                self.store.admins[admin_id] = admin_obj
            self._send_json(200, {"status": "added", "admin": admin_obj})
            return

        # 6. Instance Actions Safety Gates (Delete & Scale Region)
        if path in ("/api/instances/delete", "/api/instances/scale-region"):
            authenticated, user, _ = self._get_authenticated_user()
            if not authenticated:
                self._send_json(401, {"error": "unauthorized"})
                return
            data = self._read_json_body()
            action = "delete" if "delete" in path else "scale-region"
            self._send_json(200, {
                "action": action,
                "dry_run": True,
                "live_mutation_blocked": True,
                "safety_gate": "SAFETY_GATE_ACTIVE",
                "message": f"Action '{action}' simulated in preview mode. No live GCP VMs mutated.",
                "request_payload": data,
                "plan": {
                    "target": data.get("instance_name") or data.get("region"),
                    "estimated_impact": "preview_only",
                },
            })
            return

        # 7. Patch Console Validate
        if path == "/api/patch-console/validate":
            manifest = self._read_json_body()
            errors = self._validate_manifest(manifest)
            if errors:
                self._send_json(400, {"valid": False, "errors": errors})
                return
            digest = compute_canonical_patch_sha256(manifest)
            self._send_json(200, {"valid": True, "patch_sha256": digest})
            return

        # 8. Patch Console Save (Pull Request)
        if path == "/api/patch-console/save":
            authenticated, user, _ = self._get_authenticated_user()
            if not authenticated:
                self._send_json(401, {"error": "unauthorized"})
                return
            data = self._read_json_body()
            manifest = data.get("manifest")
            if not manifest:
                self._send_json(400, {"error": "missing_manifest"})
                return
            repo = manifest.get("repository", "")
            if repo not in CONTROLLED_TARGETS:
                self._send_json(400, {"error": "unsupported_repository", "repo": repo})
                return
            errors = self._validate_manifest(manifest)
            if errors:
                self._send_json(400, {"valid": False, "errors": errors})
                return
            digest = compute_canonical_patch_sha256(manifest)
            slug = repo.replace("/", "-")
            branch = f"spot-dashboard/{slug}-{digest[:12]}"
            with self.store.lock:
                self.store.manifests[repo] = copy.deepcopy(manifest)
            self._send_json(200, {
                "status": "pr_created",
                "repository": "swack-tools/spot-runners-config",
                "branch": branch,
                "target_repo": repo,
                "patch_sha256": digest,
                "pr_url": f"https://github.com/swack-tools/spot-runners-config/pull/123",
            })
            return

        # 9. Rollout Classification Preview
        if path == "/api/patches/preview":
            data = self._read_json_body()
            target_manifest = data.get("manifest", {})
            current_manifest = data.get("current_manifest", {})
            classification = self._classify_rollout(current_manifest, target_manifest)
            self._send_json(200, classification)
            return

        # 10. Scheduled Reconciler Simulation Hook
        if path == "/api/reconciler/tick":
            with self.store.lock:
                self.store.reconciler_tick_count += 1
                tick = self.store.reconciler_tick_count
                # Floor maintenance: verify all floor targets exist
                floor_targets = self.store.scaling_policy.get("floor_targets", [])
                launched_hosts: list[str] = []
                cleaned_vms: list[str] = []
                cleaned_runners: list[str] = []
                # Clean any terminated VMs
                if self.store.terminated_vms:
                    cleaned_vms = list(self.store.terminated_vms)
                    self.store.terminated_vms.clear()

                # Clean stale runners
                if self.store.stale_runners:
                    cleaned_runners = list(self.store.stale_runners)
                    self.store.stale_runners.clear()

                # Diff-gated KV writes: Only write to SPOT_KV if state actually changed
                has_mutations = bool(cleaned_vms or cleaned_runners or launched_hosts)
                if has_mutations:
                    self.store.overview["written_at"] = int(time.time())
                    self.store.overview["state"]["updated_at"] = int(time.time())
                    self.store.kv_puts += 1
                    self.store.cached_state_json = None  # invalidate isolate cache
                    snapshot_written = True
                else:
                    snapshot_written = False  # 0 SPOT_KV.put calls on unchanged tick!

            self._send_json(200, {
                "tick": tick,
                "status": "reconciled",
                "launched_hosts": launched_hosts,
                "cleaned_vms": cleaned_vms,
                "cleaned_runners": cleaned_runners,
                "snapshot_written": snapshot_written,
                "kv_puts": self.store.kv_puts,
            })
            return

        # Test Helper: Authorize Device Code
        if path == "/api/test/authorize-device":
            data = self._read_json_body()
            code = data.get("device_code")
            with self.store.lock:
                if code in self.store.device_codes:
                    self.store.device_codes[code]["authorized"] = True
                    self._send_json(200, {"status": "authorized"})
                    return
            self._send_json(404, {"error": "device_code_not_found"})
            return

        # Test Helper: Set Session Cut-Off
        if path == "/api/test/set-revocation-cutoff":
            data = self._read_json_body()
            cutoff = data.get("not_before", int(time.time()))
            with self.store.lock:
                self.store.session_not_before = cutoff
            self._send_json(200, {"status": "cutoff_updated", "not_before": cutoff})
            return

        # Test Helper: Inject Terminated VM
        if path == "/api/test/inject-terminated-vm":
            data = self._read_json_body()
            vm_name = data.get("name", "oxidex-runners-preempted")
            with self.store.lock:
                self.store.terminated_vms.append(vm_name)
            self._send_json(200, {"status": "injected", "vm": vm_name})
            return

        # Test Helper: Inject Stale Runner
        if path == "/api/test/inject-stale-runner":
            data = self._read_json_body()
            runner_id = data.get("runner_id", "runner-stale-999")
            with self.store.lock:
                self.store.stale_runners.append(runner_id)
            self._send_json(200, {"status": "injected", "runner_id": runner_id})
            return

        # Test Helper: Reset Store
        if path == "/api/test/reset":
            self.store.reset()
            self._send_json(200, {"status": "reset"})
            return

        self._send_json(404, {"error": "not_found", "path": path})

    def do_DELETE(self) -> None:
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path

        # Admin Management - Remove Admin
        if path.startswith("/api/admins/"):
            authenticated, user, _ = self._get_authenticated_user()
            if not authenticated:
                self._send_json(401, {"error": "unauthorized"})
                return
            if str(user.get("sub")) != str(SWACKHAMER_USER_ID):
                self._send_json(403, {"error": "forbidden", "message": "Only owner can manage admins"})
                return
            admin_id = path.removeprefix("/api/admins/")
            if str(admin_id) == str(SWACKHAMER_USER_ID):
                self._send_json(400, {"error": "cannot_remove_owner"})
                return
            with self.store.lock:
                removed = self.store.admins.pop(admin_id, None)
            if not removed:
                self._send_json(404, {"error": "admin_not_found", "id": admin_id})
                return
            self._send_json(200, {"status": "removed", "id": admin_id})
            return

        self._send_json(404, {"error": "not_found", "path": path})

    def _validate_manifest(self, manifest: dict[str, Any]) -> list[str]:
        errors: list[str] = []
        if not isinstance(manifest, dict):
            return ["manifest_must_be_dict"]

        schema = manifest.get("schema")
        if schema != "spot-runner-patch-v1":
            errors.append(f"invalid_schema: expected spot-runner-patch-v1, got {schema}")

        hw = manifest.get("hardware", {})
        if not isinstance(hw, dict):
            errors.append("hardware_must_be_dict")
            return errors

        # Hardware sizing floor checks
        min_vcpu = hw.get("min_vcpu")
        if min_vcpu is not None and min_vcpu < MIN_HOST_VCPU:
            errors.append(f"min_vcpu_below_floor: {min_vcpu} < {MIN_HOST_VCPU}")

        min_ram = hw.get("min_ram_gib")
        if min_ram is not None and min_ram < MIN_HOST_RAM_GIB:
            errors.append(f"min_ram_gib_below_floor: {min_ram} < {MIN_HOST_RAM_GIB}")

        runners_count = hw.get("runners_count")
        if runners_count is not None and runners_count != REQUIRED_RUNNERS_COUNT:
            errors.append(f"runners_count_must_be_2: got {runners_count}")

        runner_vcpu = hw.get("runner_vcpu")
        if runner_vcpu is not None and runner_vcpu != REQUIRED_RUNNER_VCPU:
            errors.append(f"runner_vcpu_must_be_7: got {runner_vcpu}")

        runner_ram = hw.get("runner_ram_gib")
        if runner_ram is not None and runner_ram != REQUIRED_RUNNER_RAM_GIB:
            errors.append(f"runner_ram_gib_must_be_14: got {runner_ram}")

        overhead = hw.get("host_overhead_gib")
        if overhead is not None and overhead != REQUIRED_HOST_OVERHEAD_GIB:
            errors.append(f"host_overhead_gib_must_be_4: got {overhead}")

        threshold = hw.get("queued_jobs_threshold")
        if threshold is not None and threshold < MIN_QUEUED_JOBS_THRESHOLD:
            errors.append(f"queued_jobs_threshold_below_floor: {threshold} < {MIN_QUEUED_JOBS_THRESHOLD}")

        # Permitted machine types check
        labels = hw.get("extra_labels", [])
        for lbl in labels:
            if lbl.startswith("type-"):
                mtype = lbl.removeprefix("type-")
                if mtype not in PERMITTED_MACHINE_TYPES:
                    errors.append(f"unpermitted_machine_type: {mtype}")

        # Check forbidden mounts
        container = manifest.get("container", {})
        if isinstance(container, dict):
            mounts = container.get("mounts", [])
            for mount in mounts:
                source = mount.get("source", "") if isinstance(mount, dict) else ""
                for forbidden in FORBIDDEN_MOUNTS:
                    if source == forbidden or source.startswith(forbidden + "/"):
                        errors.append(f"forbidden_mount_source: {source}")

        return errors

    def _classify_rollout(
        self,
        current: dict[str, Any],
        target: dict[str, Any],
    ) -> dict[str, Any]:
        """Classify changes into noop, hot_patch, recreate_containers, replace_vm."""
        current_sha = compute_canonical_patch_sha256(current) if current else ""
        target_sha = compute_canonical_patch_sha256(target) if target else ""

        if current_sha == target_sha:
            return {"action": "noop", "description": "Manifests are identical"}

        cur_hw = current.get("hardware", {})
        tgt_hw = target.get("hardware", {})
        cur_host = current.get("host", {})
        tgt_host = target.get("host", {})
        cur_cnt = current.get("container", {})
        tgt_cnt = target.get("container", {})

        # VM replacement needed if hardware or host boot changes
        if (
            cur_hw != tgt_hw
            or cur_host.get("boot_script") != tgt_host.get("boot_script")
            or cur_host.get("sysctl") != tgt_host.get("sysctl")
        ):
            return {
                "action": "replace_vm",
                "description": "Hardware sizing, ISA, sysctl, or boot script modified",
            }

        # Container recreation needed if dockerfile or mounts changed
        if (
            cur_cnt.get("dockerfile_run") != tgt_cnt.get("dockerfile_run")
            or cur_cnt.get("mounts") != tgt_cnt.get("mounts")
        ):
            return {
                "action": "recreate_containers",
                "description": "Container Dockerfile or mounts modified",
            }

        # Otherwise hot-patching apt or files
        return {
            "action": "hot_patch",
            "description": "In-place package installation or file updates",
        }


class EdgeContractServer:
    """Manager for the background HTTP contract test server."""

    def __init__(self, port: int = 0, secret: str = DEFAULT_TEST_SECRET):
        self.store = EdgeStateStore(secret=secret)
        
        class CustomHandler(EdgeContractHandler):
            store = self.store

        self.server = socketserver.ThreadingTCPServer(("127.0.0.1", port), CustomHandler)
        self.server.daemon_threads = True
        self.server.allow_reuse_address = True
        self.port = self.server.server_address[1]
        self.base_url = f"http://127.0.0.1:{self.port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def start(self) -> None:
        self.thread.start()

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2.0)
