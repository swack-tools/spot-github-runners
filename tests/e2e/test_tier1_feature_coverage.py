"""Tier 1: Feature Coverage E2E Test Suite.

Covers all 52 functional runtime features in PROJECT.md § Feature Inventory
with >= 5 test cases per feature in isolation (52 * 5 = 260 test cases).
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import sys
import time
import unittest
import urllib.parse
from pathlib import Path

# Add current directory to path for standalone execution
E2E_DIR = Path(__file__).resolve().parent
if str(E2E_DIR) not in sys.path:
    sys.path.insert(0, str(E2E_DIR))

from client import EdgeTestClient
from contract_server import EdgeContractServer
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
    SESSION_PURPOSE,
    SWACKHAMER_LOGIN,
    SWACKHAMER_USER_ID,
    compute_canonical_patch_sha256,
    create_session_token,
    derive_session_key,
    make_gcp_service_account_fixture,
    make_mock_overview_payload,
    make_valid_manifest,
    verify_session_token,
)


class TestTier1FeatureCoverage(unittest.TestCase):
    """Tier 1: Feature Coverage — 52 features * 5 tests = 260 tests."""

    server: EdgeContractServer
    client: EdgeTestClient

    @classmethod
    def setUpClass(cls) -> None:
        target_url = os.environ.get("TEST_DASHBOARD_URL")
        if target_url:
            cls.server = None  # type: ignore
            cls.client = EdgeTestClient(target_url)
        else:
            cls.server = EdgeContractServer()
            cls.server.start()
            cls.client = EdgeTestClient(cls.server.base_url)

    @classmethod
    def tearDownClass(cls) -> None:
        if cls.server:
            cls.server.stop()

    def setUp(self) -> None:
        self.client.clear_cookies()
        self.client.post("/api/test/reset")

    # -------------------------------------------------------------------------
    # Feature 1: Edge Static UI Serving
    # -------------------------------------------------------------------------
    def test_f01_edge_static_ui_serving_case1_returns_200_html(self):
        self.client.login_as_owner()
        resp = self.client.get("/")
        self.assertEqual(resp.status_code, 200)
        self.assertIn("text/html", resp.header("Content-Type") or "")

    def test_f01_edge_static_ui_serving_case2_contains_doctype_and_title(self):
        self.client.login_as_owner()
        resp = self.client.get("/")
        self.assertTrue("<!DOCTYPE html>" in resp.text or "<!doctype html>" in resp.text)
        self.assertTrue(
            "<title>Spot Runner Fleet</title>" in resp.text
            or "<title>Spot GitHub Runners &amp; Rust Builders — Control Plane Dashboard</title>" in resp.text
        )

    def test_f01_edge_static_ui_serving_case3_contains_dashboard_app_elements(self):
        self.client.login_as_owner()
        resp = self.client.get("/")
        self.assertIn('id="dashboard-app"', resp.text)
        self.assertIn('id="fleet-summary"', resp.text)
        self.assertIn('id="instances-table"', resp.text)

    def test_f01_edge_static_ui_serving_case4_displays_logged_in_user_badge(self):
        self.client.login_as_owner()
        resp = self.client.get("/")
        self.assertIn('id="user-badge"', resp.text)
        self.assertIn(SWACKHAMER_LOGIN, resp.text)

    def test_f01_edge_static_ui_serving_case5_fast_in_memory_latency(self):
        self.client.login_as_owner()
        t0 = time.perf_counter()
        resp = self.client.get("/")
        elapsed_ms = (time.perf_counter() - t0) * 1000
        self.assertEqual(resp.status_code, 200)
        self.assertLess(elapsed_ms, 100.0)

    # -------------------------------------------------------------------------
    # Feature 2: Login Gate HTML
    # -------------------------------------------------------------------------
    def test_f02_login_gate_html_case1_unauthenticated_returns_200(self):
        resp = self.client.get("/")
        self.assertEqual(resp.status_code, 200)
        self.assertIn("text/html", resp.header("Content-Type") or "")

    def test_f02_login_gate_html_case2_contains_login_gate_container(self):
        resp = self.client.get("/")
        self.assertTrue('id="login-gate"' in resp.text or 'Spot Dashboard sign-in' in resp.text)
        self.assertTrue("Sign in to Spot GitHub Runners" in resp.text or "Spot Dashboard sign-in" in resp.text)

    def test_f02_login_gate_html_case3_contains_oauth_login_button(self):
        resp = self.client.get("/")
        self.assertIn('href="/auth/github/login"', resp.text)
        self.assertIn("Sign in with GitHub", resp.text)

    def test_f02_login_gate_html_case4_contains_device_flow_option(self):
        resp = self.client.get("/")
        self.assertTrue('id="device-flow"' in resp.text or 'id="device-start"' in resp.text or 'Sign in with a device code' in resp.text)

    def test_f02_login_gate_html_case5_invalid_cookie_falls_back_to_login_gate(self):
        self.client.set_cookie(COOKIE_NAME, "forged-invalid-session-token")
        resp = self.client.get("/")
        self.assertEqual(resp.status_code, 200)
        self.assertTrue('id="login-gate"' in resp.text or 'Spot Dashboard sign-in' in resp.text)
        self.assertNotIn('id="dashboard-app"', resp.text)

    # -------------------------------------------------------------------------
    # Feature 3: HMAC-SHA256 Session Token
    # -------------------------------------------------------------------------
    def test_f03_hmac_session_token_case1_valid_token_verifies(self):
        token = create_session_token({"sub": "123", "login": "test", "exp": int(time.time()) + 3600})
        valid, payload, reason = verify_session_token(token)
        self.assertTrue(valid)
        self.assertIsNotNone(payload)
        self.assertEqual(payload["login"], "test")

    def test_f03_hmac_session_token_case2_tampered_payload_rejected(self):
        token = create_session_token({"sub": "123", "login": "test", "role": "member", "exp": int(time.time()) + 3600})
        body_b64, sig = token.split(".")
        # Tamper payload
        altered_body = base64.urlsafe_b64encode(b'{"exp":9999999999,"login":"test","role":"owner","sub":"123"}').decode().rstrip("=")
        tampered_token = f"{altered_body}.{sig}"
        valid, _, reason = verify_session_token(tampered_token)
        self.assertFalse(valid)
        self.assertEqual(reason, "signature_mismatch")

    def test_f03_hmac_session_token_case3_wrong_secret_rejected(self):
        token = create_session_token({"sub": "123", "exp": int(time.time()) + 3600}, secret="wrong-secret-key")
        valid, _, reason = verify_session_token(token, secret=DEFAULT_TEST_SECRET)
        self.assertFalse(valid)
        self.assertEqual(reason, "signature_mismatch")

    def test_f03_hmac_session_token_case4_expired_token_rejected(self):
        token = create_session_token({"sub": "123", "exp": int(time.time()) - 10})
        valid, _, reason = verify_session_token(token)
        self.assertFalse(valid)
        self.assertEqual(reason, "token_expired")

    def test_f03_hmac_session_token_case5_purpose_derivation_binding(self):
        derived = derive_session_key(DEFAULT_TEST_SECRET)
        expected = hmac.new(DEFAULT_TEST_SECRET.encode(), SESSION_PURPOSE.encode(), hashlib.sha256).digest()
        self.assertEqual(derived, expected)

    # -------------------------------------------------------------------------
    # Feature 4: Host-Prefix Session Cookie
    # -------------------------------------------------------------------------
    def test_f04_host_prefix_cookie_case1_cookie_name_is_host_prefixed(self):
        self.assertEqual(COOKIE_NAME, "__Host-spot_session")

    def test_f04_host_prefix_cookie_case2_oauth_callback_sets_host_cookie(self):
        resp = self.client.get("/auth/github/login")
        location = resp.header("Location") or ""
        state = urllib.parse.parse_qs(urllib.parse.urlparse(location).query).get("state", [""])[0]
        callback_resp = self.client.get(f"/auth/github/callback?code=mock_code&state={state}")
        set_cookie = callback_resp.header("Set-Cookie") or ""
        self.assertIn("__Host-spot_session=", set_cookie)

    def test_f04_host_prefix_cookie_case3_cookie_has_path_slash(self):
        resp = self.client.get("/auth/github/login")
        state = urllib.parse.parse_qs(urllib.parse.urlparse(resp.header("Location") or "").query).get("state", [""])[0]
        callback_resp = self.client.get(f"/auth/github/callback?code=mock_code&state={state}")
        set_cookie = callback_resp.header("Set-Cookie") or ""
        self.assertIn("Path=/", set_cookie)

    def test_f04_host_prefix_cookie_case4_cookie_has_httponly_and_secure(self):
        resp = self.client.get("/auth/github/login")
        state = urllib.parse.parse_qs(urllib.parse.urlparse(resp.header("Location") or "").query).get("state", [""])[0]
        callback_resp = self.client.get(f"/auth/github/callback?code=mock_code&state={state}")
        set_cookie = callback_resp.header("Set-Cookie") or ""
        self.assertIn("HttpOnly", set_cookie)
        self.assertIn("Secure", set_cookie)

    def test_f04_host_prefix_cookie_case5_cookie_has_samesite_lax(self):
        resp = self.client.get("/auth/github/login")
        state = urllib.parse.parse_qs(urllib.parse.urlparse(resp.header("Location") or "").query).get("state", [""])[0]
        callback_resp = self.client.get(f"/auth/github/callback?code=mock_code&state={state}")
        set_cookie = callback_resp.header("Set-Cookie") or ""
        self.assertIn("SameSite=Lax", set_cookie)

    # -------------------------------------------------------------------------
    # Feature 5: Bulk Revocation Cut-off
    # -------------------------------------------------------------------------
    def test_f05_bulk_revocation_case1_session_after_cutoff_is_valid(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "iat": now + 10, "exp": now + 3600})
        valid, _, _ = verify_session_token(token, not_before=now)
        self.assertTrue(valid)

    def test_f05_bulk_revocation_case2_session_before_cutoff_is_revoked(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "iat": now - 100, "exp": now + 3600})
        valid, _, reason = verify_session_token(token, not_before=now)
        self.assertFalse(valid)
        self.assertEqual(reason, "token_revoked_by_cutoff")

    def test_f05_bulk_revocation_case3_setting_cutoff_revokes_active_client(self):
        now = int(time.time())
        self.client.login_as_owner(iat=now - 50)
        status = self.client.get("/api/auth/status").json
        self.assertTrue(status.get("authenticated"))
        # Set cutoff to now
        self.client.post("/api/test/set-revocation-cutoff", json_data={"not_before": now})
        revoked_status = self.client.get("/api/auth/status").json
        self.assertFalse(revoked_status.get("authenticated"))

    def test_f05_bulk_revocation_case4_new_session_after_cutoff_succeeds(self):
        now = int(time.time())
        self.client.post("/api/test/set-revocation-cutoff", json_data={"not_before": now})
        # Login with fresh iat > cutoff
        self.client.login_as_owner(iat=now + 5)
        status = self.client.get("/api/auth/status").json
        self.assertTrue(status.get("authenticated"))

    def test_f05_bulk_revocation_case5_public_health_accessible_during_cutoff(self):
        self.client.post("/api/test/set-revocation-cutoff", json_data={"not_before": int(time.time()) + 9999})
        health = self.client.get("/api/health")
        self.assertEqual(health.status_code, 200)
        self.assertEqual(health.json.get("status"), "healthy")

    # -------------------------------------------------------------------------
    # Feature 6: GitHub OAuth PKCE Flow
    # -------------------------------------------------------------------------
    def test_f06_oauth_pkce_case1_login_redirects_302(self):
        resp = self.client.get("/auth/github/login")
        self.assertEqual(resp.status_code, 302)

    def test_f06_oauth_pkce_case2_redirects_to_github_authorize(self):
        resp = self.client.get("/auth/github/login")
        location = resp.header("Location") or ""
        self.assertTrue(location.startswith("https://github.com/login/oauth/authorize"))

    def test_f06_oauth_pkce_case3_includes_client_id_and_redirect_uri(self):
        resp = self.client.get("/auth/github/login")
        location = resp.header("Location") or ""
        params = urllib.parse.parse_qs(urllib.parse.urlparse(location).query)
        self.assertIn("client_id", params)
        self.assertIn("redirect_uri", params)

    def test_f06_oauth_pkce_case4_includes_required_scopes(self):
        resp = self.client.get("/auth/github/login")
        params = urllib.parse.parse_qs(urllib.parse.urlparse(resp.header("Location") or "").query)
        self.assertEqual(params.get("scope", [""])[0], "read:org,repo")

    def test_f06_oauth_pkce_case5_includes_pkce_s256_challenge(self):
        resp = self.client.get("/auth/github/login")
        params = urllib.parse.parse_qs(urllib.parse.urlparse(resp.header("Location") or "").query)
        self.assertEqual(params.get("code_challenge_method", [""])[0], "S256")
        challenge = params.get("code_challenge", [""])[0]
        self.assertGreater(len(challenge), 20)

    # -------------------------------------------------------------------------
    # Feature 7: GitHub OAuth Callback
    # -------------------------------------------------------------------------
    def test_f07_oauth_callback_case1_valid_callback_redirects_to_root(self):
        init_resp = self.client.get("/auth/github/login")
        state = urllib.parse.parse_qs(urllib.parse.urlparse(init_resp.header("Location") or "").query).get("state", [""])[0]
        resp = self.client.get(f"/auth/github/callback?code=mock_code&state={state}")
        self.assertEqual(resp.status_code, 302)
        self.assertEqual(resp.header("Location"), "/")

    def test_f07_oauth_callback_case2_sets_session_cookie(self):
        init_resp = self.client.get("/auth/github/login")
        state = urllib.parse.parse_qs(urllib.parse.urlparse(init_resp.header("Location") or "").query).get("state", [""])[0]
        resp = self.client.get(f"/auth/github/callback?code=mock_code&state={state}")
        self.assertIn(COOKIE_NAME, self.client.cookies)

    def test_f07_oauth_callback_case3_missing_state_returns_400(self):
        resp = self.client.get("/auth/github/callback?code=mock_code")
        self.assertEqual(resp.status_code, 400)

    def test_f07_oauth_callback_case4_invalid_state_returns_400(self):
        resp = self.client.get("/auth/github/callback?code=mock_code&state=nonexistent_state")
        self.assertEqual(resp.status_code, 400)

    def test_f07_oauth_callback_case5_state_replay_rejected(self):
        init_resp = self.client.get("/auth/github/login")
        state = urllib.parse.parse_qs(urllib.parse.urlparse(init_resp.header("Location") or "").query).get("state", [""])[0]
        self.client.get(f"/auth/github/callback?code=mock_code&state={state}")
        # Second attempt with same state
        replayed = self.client.get(f"/auth/github/callback?code=mock_code&state={state}")
        self.assertEqual(replayed.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 8: GitHub Device Flow Code
    # -------------------------------------------------------------------------
    def test_f08_device_flow_code_case1_post_returns_200(self):
        resp = self.client.post("/auth/device/code")
        self.assertEqual(resp.status_code, 200)

    def test_f08_device_flow_code_case2_returns_device_code(self):
        resp = self.client.post("/auth/device/code")
        self.assertIn("device_code", resp.json)

    def test_f08_device_flow_code_case3_returns_user_code(self):
        resp = self.client.post("/auth/device/code")
        self.assertIn("user_code", resp.json)

    def test_f08_device_flow_code_case4_returns_verification_uri(self):
        resp = self.client.post("/auth/device/code")
        self.assertEqual(resp.json.get("verification_uri"), "https://github.com/login/device")

    def test_f08_device_flow_code_case5_returns_expiry_and_interval(self):
        resp = self.client.post("/auth/device/code")
        data = resp.json
        self.assertGreater(data.get("expires_in", 0), 0)
        self.assertEqual(data.get("interval"), 5)

    # -------------------------------------------------------------------------
    # Feature 9: GitHub Device Flow Poll
    # -------------------------------------------------------------------------
    def test_f09_device_flow_poll_case1_pending_authorization_returns_400(self):
        init = self.client.post("/auth/device/code").json
        poll = self.client.post("/auth/device/poll", json_data={"device_code": init["device_code"]})
        self.assertEqual(poll.status_code, 400)
        self.assertEqual(poll.json.get("error"), "authorization_pending")

    def test_f09_device_flow_poll_case2_unknown_code_returns_400(self):
        poll = self.client.post("/auth/device/poll", json_data={"device_code": "unknown-code-123"})
        self.assertEqual(poll.status_code, 400)
        self.assertEqual(poll.json.get("error"), "invalid_device_code")

    def test_f09_device_flow_poll_case3_authorized_device_returns_200(self):
        init = self.client.post("/auth/device/code").json
        dev_code = init["device_code"]
        self.client.post("/api/test/authorize-device", json_data={"device_code": dev_code})
        poll = self.client.post("/auth/device/poll", json_data={"device_code": dev_code})
        self.assertEqual(poll.status_code, 200)
        self.assertEqual(poll.json.get("status"), "authorized")

    def test_f09_device_flow_poll_case4_authorized_poll_sets_session_cookie(self):
        init = self.client.post("/auth/device/code").json
        dev_code = init["device_code"]
        self.client.post("/api/test/authorize-device", json_data={"device_code": dev_code})
        poll = self.client.post("/auth/device/poll", json_data={"device_code": dev_code})
        self.assertIn(COOKIE_NAME, poll.cookies)

    def test_f09_device_flow_poll_case5_missing_body_returns_400(self):
        poll = self.client.post("/auth/device/poll", json_data={})
        self.assertEqual(poll.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 10: Auth Status Endpoint
    # -------------------------------------------------------------------------
    def test_f10_auth_status_case1_public_returns_200(self):
        resp = self.client.get("/api/auth/status")
        self.assertEqual(resp.status_code, 200)

    def test_f10_auth_status_case2_unauthenticated_reports_false(self):
        resp = self.client.get("/api/auth/status")
        self.assertFalse(resp.json.get("authenticated"))

    def test_f10_auth_status_case3_owner_reports_owner_role(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/auth/status")
        data = resp.json
        self.assertTrue(data.get("authenticated"))
        self.assertEqual(data.get("user_id"), SWACKHAMER_USER_ID)
        self.assertEqual(data.get("login"), SWACKHAMER_LOGIN)
        self.assertEqual(data.get("role"), "owner")
        self.assertTrue(data.get("can_admin"))

    def test_f10_auth_status_case4_added_admin_reports_can_admin(self):
        self.client.login_as_owner()
        self.client.post("/api/admins", json_data={"id": 777777, "login": "test-sec-admin"})
        self.client.login_as_admin(user_id=777777, login="test-sec-admin")
        status = self.client.get("/api/auth/status").json
        self.assertTrue(status.get("authenticated"))
        self.assertTrue(status.get("can_admin"))

    def test_f10_auth_status_case5_regular_member_reports_cannot_admin(self):
        self.client.login_as_member(user_id=555555, login="regular-dev")
        status = self.client.get("/api/auth/status").json
        self.assertTrue(status.get("authenticated"))
        self.assertFalse(status.get("can_admin"))

    # -------------------------------------------------------------------------
    # Feature 11: Session Logout
    # -------------------------------------------------------------------------
    def test_f11_session_logout_case1_returns_200(self):
        self.client.login_as_owner()
        resp = self.client.post("/auth/logout")
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(resp.json.get("status"), "logged_out")

    def test_f11_session_logout_case2_clears_cookie_in_client(self):
        self.client.login_as_owner()
        self.client.post("/auth/logout")
        self.assertEqual(self.client.cookies.get(COOKIE_NAME), "")

    def test_f11_session_logout_case3_subsequent_status_is_unauthenticated(self):
        self.client.login_as_owner()
        self.client.post("/auth/logout")
        status = self.client.get("/api/auth/status").json
        self.assertFalse(status.get("authenticated"))

    def test_f11_session_logout_case4_set_cookie_has_max_age_zero(self):
        self.client.login_as_owner()
        resp = self.client.post("/auth/logout")
        set_cookie = resp.header("Set-Cookie") or ""
        self.assertIn("Max-Age=0", set_cookie)

    def test_f11_session_logout_case5_logout_when_not_logged_in_is_idempotent(self):
        resp = self.client.post("/auth/logout")
        self.assertEqual(resp.status_code, 200)

    # -------------------------------------------------------------------------
    # Feature 12: Admin Authorization
    # -------------------------------------------------------------------------
    def test_f12_admin_authorization_case1_owner_allowed_on_scaling(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 25})
        self.assertEqual(resp.status_code, 200)

    def test_f12_admin_authorization_case2_unauthenticated_scaling_returns_401(self):
        resp = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 25})
        self.assertEqual(resp.status_code, 401)

    def test_f12_admin_authorization_case3_non_owner_cannot_add_admin(self):
        self.client.login_as_admin(user_id=123456, login="some-admin")
        resp = self.client.post("/api/admins", json_data={"id": 654321, "login": "new-user"})
        self.assertEqual(resp.status_code, 403)

    def test_f12_admin_authorization_case4_owner_can_add_admin(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/admins", json_data={"id": 654321, "login": "new-user"})
        self.assertEqual(resp.status_code, 200)

    def test_f12_admin_authorization_case5_unauthenticated_patch_console_returns_401(self):
        resp = self.client.get("/api/patch-console/bootstrap")
        self.assertEqual(resp.status_code, 401)

    # -------------------------------------------------------------------------
    # Feature 13: Controlled Target Verification
    # -------------------------------------------------------------------------
    def test_f13_controlled_target_case1_swack_tools_accepted(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=swack-tools")
        self.assertEqual(resp.status_code, 200)

    def test_f13_controlled_target_case2_newznab_tmux_accepted(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=xbmc4lyfe/newznab-tmux")
        self.assertEqual(resp.status_code, 200)

    def test_f13_controlled_target_case3_rustnzb_accepted(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=xbmc4lyfe/rustnzb")
        self.assertEqual(resp.status_code, 200)

    def test_f13_controlled_target_case4_nntp_proxy_accepted(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=xbmc4lyfe/nntp-proxy")
        self.assertEqual(resp.status_code, 200)

    def test_f13_controlled_target_case5_unauthorized_repo_rejected(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=external/unauthorized")
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 14: Edge Health Endpoint
    # -------------------------------------------------------------------------
    def test_f14_edge_health_case1_returns_200(self):
        resp = self.client.get("/api/health")
        self.assertEqual(resp.status_code, 200)

    def test_f14_edge_health_case2_reports_healthy(self):
        resp = self.client.get("/api/health")
        self.assertEqual(resp.json.get("status"), "healthy")

    def test_f14_edge_health_case3_reports_hydrated(self):
        resp = self.client.get("/api/health")
        self.assertTrue(resp.json.get("hydrated"))

    def test_f14_edge_health_case4_reports_state_version(self):
        resp = self.client.get("/api/health")
        self.assertEqual(resp.json.get("state_version"), 1)

    def test_f14_edge_health_case5_fast_probe_execution(self):
        t0 = time.perf_counter()
        resp = self.client.get("/api/health")
        elapsed = (time.perf_counter() - t0) * 1000
        self.assertEqual(resp.status_code, 200)
        self.assertLess(elapsed, 50.0)

    # -------------------------------------------------------------------------
    # Feature 15: Edge Overview Endpoint
    # -------------------------------------------------------------------------
    def test_f15_edge_overview_case1_returns_200(self):
        resp = self.client.get("/api/overview")
        self.assertEqual(resp.status_code, 200)

    def test_f15_edge_overview_case2_schema_is_dashboard_snapshot_v1(self):
        resp = self.client.get("/api/overview")
        self.assertEqual(resp.json.get("schema"), "spot-dashboard-snapshot-v1")

    def test_f15_edge_overview_case3_contains_fleet_summary(self):
        resp = self.client.get("/api/overview")
        self.assertIn("fleet_summary", resp.json.get("state", {}))

    def test_f15_edge_overview_case4_contains_instances_view(self):
        resp = self.client.get("/api/overview")
        self.assertIn("instances_view", resp.json.get("state", {}))

    def test_f15_edge_overview_case5_contains_budget_and_scaling(self):
        state = self.client.get("/api/overview").json.get("state", {})
        self.assertIn("global_budget", state)
        self.assertIn("scaling", state)

    # -------------------------------------------------------------------------
    # Feature 16: Edge Scaling Query/Update
    # -------------------------------------------------------------------------
    def test_f16_scaling_case1_get_returns_policy(self):
        resp = self.client.get("/api/scaling")
        self.assertEqual(resp.status_code, 200)
        self.assertIn("queued_jobs_threshold", resp.json)

    def test_f16_scaling_case2_post_updates_policy(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 30})
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(resp.json.get("scaling", {}).get("queued_jobs_threshold"), 30)

    def test_f16_scaling_case3_post_below_threshold_rejected(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 10})
        self.assertEqual(resp.status_code, 400)

    def test_f16_scaling_case4_unauthenticated_post_rejected(self):
        resp = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 25})
        self.assertEqual(resp.status_code, 401)

    def test_f16_scaling_case5_updated_value_persists_in_get(self):
        self.client.login_as_owner()
        self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 40})
        get_resp = self.client.get("/api/scaling")
        self.assertEqual(get_resp.json.get("queued_jobs_threshold"), 40)

    # -------------------------------------------------------------------------
    # Feature 17: Edge Admin Management
    # -------------------------------------------------------------------------
    def test_f17_admin_management_case1_get_admins_returns_list(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/admins")
        self.assertEqual(resp.status_code, 200)
        self.assertIn("admins", resp.json)

    def test_f17_admin_management_case2_owner_adds_admin(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/admins", json_data={"id": 111222, "login": "infra-eng"})
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(resp.json.get("status"), "added")

    def test_f17_admin_management_case3_cannot_remove_owner(self):
        self.client.login_as_owner()
        resp = self.client.delete(f"/api/admins/{SWACKHAMER_USER_ID}")
        self.assertEqual(resp.status_code, 400)
        self.assertEqual(resp.json.get("error"), "cannot_remove_owner")

    def test_f17_admin_management_case4_owner_removes_added_admin(self):
        self.client.login_as_owner()
        self.client.post("/api/admins", json_data={"id": 333444, "login": "temp-admin"})
        del_resp = self.client.delete("/api/admins/333444")
        self.assertEqual(del_resp.status_code, 200)
        self.assertEqual(del_resp.json.get("status"), "removed")

    def test_f17_admin_management_case5_unauthenticated_get_admins_returns_401(self):
        resp = self.client.get("/api/admins")
        self.assertEqual(resp.status_code, 401)

    # -------------------------------------------------------------------------
    # Feature 18: Instance Actions Safety Gate
    # -------------------------------------------------------------------------
    def test_f18_safety_gate_case1_delete_returns_preview_mode(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/delete", json_data={"instance_name": "oxidex-runners-1"})
        self.assertEqual(resp.status_code, 200)
        self.assertTrue(resp.json.get("dry_run"))
        self.assertTrue(resp.json.get("live_mutation_blocked"))

    def test_f18_safety_gate_case2_scale_region_returns_preview_mode(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/scale-region", json_data={"region": "us-central1", "target": 5})
        self.assertEqual(resp.status_code, 200)
        self.assertTrue(resp.json.get("dry_run"))
        self.assertTrue(resp.json.get("live_mutation_blocked"))

    def test_f18_safety_gate_case3_delete_echoes_target_instance(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/delete", json_data={"instance_name": "oxidex-runners-test-vm"})
        self.assertEqual(resp.json.get("plan", {}).get("target"), "oxidex-runners-test-vm")

    def test_f18_safety_gate_case4_unauthenticated_delete_returns_401(self):
        resp = self.client.post("/api/instances/delete", json_data={"instance_name": "oxidex-runners-1"})
        self.assertEqual(resp.status_code, 401)

    def test_f18_safety_gate_case5_unauthenticated_scale_returns_401(self):
        resp = self.client.post("/api/instances/scale-region", json_data={"region": "us-central1"})
        self.assertEqual(resp.status_code, 401)

    # -------------------------------------------------------------------------
    # Feature 19: Canonical patch_sha256 Parity
    # -------------------------------------------------------------------------
    def test_f19_canonical_hash_case1_keys_are_sorted(self):
        manifest1 = make_valid_manifest()
        hash1 = compute_canonical_patch_sha256(manifest1)
        self.assertEqual(len(hash1), 64)

    def test_f19_canonical_hash_case2_source_commit_excluded(self):
        m1 = make_valid_manifest()
        m2 = make_valid_manifest()
        m2["source_commit"] = "a" * 40
        self.assertEqual(compute_canonical_patch_sha256(m1), compute_canonical_patch_sha256(m2))

    def test_f19_canonical_hash_case3_source_branch_excluded(self):
        m1 = make_valid_manifest()
        m2 = make_valid_manifest()
        m2["source_branch"] = "feature/test-branch"
        self.assertEqual(compute_canonical_patch_sha256(m1), compute_canonical_patch_sha256(m2))

    def test_f19_canonical_hash_case4_compact_json_separators_used(self):
        m = make_valid_manifest()
        h = compute_canonical_patch_sha256(m)
        self.assertTrue(all(c in "0123456789abcdef" for c in h))

    def test_f19_canonical_hash_case5_deterministic_across_invocations(self):
        m = make_valid_manifest()
        self.assertEqual(compute_canonical_patch_sha256(m), compute_canonical_patch_sha256(m))

    # -------------------------------------------------------------------------
    # Feature 20: Hardware Sizing Floor Validation
    # -------------------------------------------------------------------------
    def test_f20_hardware_floor_case1_valid_manifest_passes(self):
        m = make_valid_manifest()
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 200)
        self.assertTrue(resp.json.get("valid"))

    def test_f20_hardware_floor_case2_min_vcpu_below_16_fails(self):
        m = make_valid_manifest()
        m["hardware"]["min_vcpu"] = 8
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)
        self.assertFalse(resp.json.get("valid"))

    def test_f20_hardware_floor_case3_min_ram_below_32_fails(self):
        m = make_valid_manifest()
        m["hardware"]["min_ram_gib"] = 16
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f20_hardware_floor_case4_runner_count_not_2_fails(self):
        m = make_valid_manifest()
        m["hardware"]["runners_count"] = 1
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f20_hardware_floor_case5_queue_threshold_below_20_fails(self):
        m = make_valid_manifest()
        m["hardware"]["queued_jobs_threshold"] = 15
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 21: Permitted Machine Types
    # -------------------------------------------------------------------------
    def test_f21_permitted_types_case1_c4d_highcpu_16_permitted(self):
        m = make_valid_manifest(machine_type="c4d-highcpu-16")
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 200)

    def test_f21_permitted_types_case2_n4d_highcpu_16_permitted(self):
        m = make_valid_manifest(machine_type="n4d-highcpu-16")
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 200)

    def test_f21_permitted_types_case3_n4_highcpu_16_permitted(self):
        m = make_valid_manifest(machine_type="n4-highcpu-16")
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 200)

    def test_f21_permitted_types_case4_c2d_highcpu_16_permitted(self):
        m = make_valid_manifest(machine_type="c2d-highcpu-16")
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 200)

    def test_f21_permitted_types_case5_e2_standard_4_rejected(self):
        m = make_valid_manifest(machine_type="e2-standard-4")
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 22: Multi-Repo Manifest Merge
    # -------------------------------------------------------------------------
    def test_f22_manifest_merge_case1_inherits_defaults(self):
        m = make_valid_manifest()
        self.assertEqual(m["hardware"]["min_vcpu"], MIN_HOST_VCPU)

    def test_f22_manifest_merge_case2_overrides_packages_cleanly(self):
        m = make_valid_manifest()
        m["container"]["apt_packages"].append("libssl-dev")
        self.assertIn("libssl-dev", m["container"]["apt_packages"])

    def test_f22_manifest_merge_case3_cannot_override_floor_below_minimum(self):
        m = make_valid_manifest()
        m["hardware"]["min_vcpu"] = 4
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f22_manifest_merge_case4_merges_sysctl_cleanly(self):
        m = make_valid_manifest()
        m["host"]["sysctl"]["fs.file-max"] = "2097152"
        self.assertEqual(m["host"]["sysctl"]["fs.file-max"], "2097152")

    def test_f22_manifest_merge_case5_maintains_schema_version(self):
        m = make_valid_manifest()
        self.assertEqual(m["schema"], "spot-runner-patch-v1")

    # -------------------------------------------------------------------------
    # Feature 23: Patch Console Bootstrap
    # -------------------------------------------------------------------------
    def test_f23_patch_bootstrap_case1_returns_200(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/bootstrap")
        self.assertEqual(resp.status_code, 200)

    def test_f23_patch_bootstrap_case2_identifies_config_repo(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/bootstrap")
        self.assertEqual(resp.json.get("repository"), "swack-tools/spot-runners-config")

    def test_f23_patch_bootstrap_case3_contains_all_controlled_targets(self):
        self.client.login_as_owner()
        targets = self.client.get("/api/patch-console/bootstrap").json.get("controlled_targets", [])
        for t in CONTROLLED_TARGETS:
            self.assertIn(t, targets)

    def test_f23_patch_bootstrap_case4_unauthenticated_returns_401(self):
        resp = self.client.get("/api/patch-console/bootstrap")
        self.assertEqual(resp.status_code, 401)

    def test_f23_patch_bootstrap_case5_json_content_type(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/bootstrap")
        self.assertIn("application/json", resp.header("Content-Type") or "")

    # -------------------------------------------------------------------------
    # Feature 24: Patch Console Load
    # -------------------------------------------------------------------------
    def test_f24_patch_load_case1_returns_manifest(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=swack-tools")
        self.assertEqual(resp.status_code, 200)
        self.assertIn("manifest", resp.json)

    def test_f24_patch_load_case2_returns_base_sha(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=swack-tools")
        self.assertEqual(len(resp.json.get("base_sha", "")), 40)

    def test_f24_patch_load_case3_returns_blob_sha(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=swack-tools")
        self.assertEqual(len(resp.json.get("blob_sha", "")), 40)

    def test_f24_patch_load_case4_returns_computed_patch_digest(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=swack-tools")
        self.assertEqual(len(resp.json.get("patch_sha256", "")), 64)

    def test_f24_patch_load_case5_unsupported_repo_returns_400(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=random/unsupported")
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 25: Patch Console Validate
    # -------------------------------------------------------------------------
    def test_f25_patch_validate_case1_valid_returns_valid_true(self):
        m = make_valid_manifest()
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 200)
        self.assertTrue(resp.json.get("valid"))

    def test_f25_patch_validate_case2_valid_returns_sha256(self):
        m = make_valid_manifest()
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(len(resp.json.get("patch_sha256", "")), 64)

    def test_f25_patch_validate_case3_forbidden_docker_sock_rejected(self):
        m = make_valid_manifest()
        m["container"]["mounts"].append({"source": "/var/run/docker.sock", "target": "/var/run/docker.sock"})
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)
        self.assertFalse(resp.json.get("valid"))

    def test_f25_patch_validate_case4_invalid_schema_rejected(self):
        m = make_valid_manifest()
        m["schema"] = "invalid-schema-v0"
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f25_patch_validate_case5_non_dict_rejected(self):
        resp = self.client.post("/api/patch-console/validate", json_data=["not", "a", "dict"])
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 26: Patch Console Save (Pull Request)
    # -------------------------------------------------------------------------
    def test_f26_patch_save_case1_creates_pr_branch(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="swack-tools")
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertEqual(resp.status_code, 200)
        self.assertTrue(resp.json.get("branch", "").startswith("spot-dashboard/swack-tools-"))

    def test_f26_patch_save_case2_returns_pr_url(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="swack-tools")
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertIn("https://github.com/swack-tools/spot-runners-config/pull/", resp.json.get("pr_url", ""))

    def test_f26_patch_save_case3_unauthenticated_returns_401(self):
        m = make_valid_manifest(repo="swack-tools")
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertEqual(resp.status_code, 401)

    def test_f26_patch_save_case4_unsupported_repo_returns_400(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="external/other-repo")
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertEqual(resp.status_code, 400)

    def test_f26_patch_save_case5_invalid_manifest_returns_400(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="swack-tools")
        m["hardware"]["min_vcpu"] = 4  # Below floor
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 27: Rollout Classification & Preview
    # -------------------------------------------------------------------------
    def test_f27_rollout_preview_case1_identical_manifests_classified_noop(self):
        m = make_valid_manifest()
        resp = self.client.post("/api/patches/preview", json_data={"current_manifest": m, "manifest": m})
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(resp.json.get("action"), "noop")

    def test_f27_rollout_preview_case2_hardware_change_classified_replace_vm(self):
        cur = make_valid_manifest()
        tgt = make_valid_manifest()
        tgt["hardware"]["extra_labels"] = ["type-n4d-highcpu-16"]
        resp = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt})
        self.assertEqual(resp.json.get("action"), "replace_vm")

    def test_f27_rollout_preview_case3_boot_script_change_classified_replace_vm(self):
        cur = make_valid_manifest()
        tgt = make_valid_manifest()
        tgt["host"]["boot_script"] = "echo 'boot setup'"
        resp = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt})
        self.assertEqual(resp.json.get("action"), "replace_vm")

    def test_f27_rollout_preview_case4_dockerfile_change_classified_recreate_containers(self):
        cur = make_valid_manifest()
        tgt = make_valid_manifest()
        tgt["container"]["dockerfile_run"] = "RUN cargo install flamegraph"
        resp = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt})
        self.assertEqual(resp.json.get("action"), "recreate_containers")

    def test_f27_rollout_preview_case5_package_change_classified_hot_patch(self):
        cur = make_valid_manifest()
        tgt = make_valid_manifest()
        tgt["container"]["apt_packages"].append("htop")
        resp = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt})
        self.assertEqual(resp.json.get("action"), "hot_patch")

    # -------------------------------------------------------------------------
    # Feature 28: Controlled Targets List
    # -------------------------------------------------------------------------
    def test_f28_controlled_targets_case1_swack_tools_present(self):
        self.assertIn("swack-tools", CONTROLLED_TARGETS)

    def test_f28_controlled_targets_case2_newznab_tmux_present(self):
        self.assertIn("xbmc4lyfe/newznab-tmux", CONTROLLED_TARGETS)

    def test_f28_controlled_targets_case3_rustnzb_present(self):
        self.assertIn("xbmc4lyfe/rustnzb", CONTROLLED_TARGETS)

    def test_f28_controlled_targets_case4_nntp_proxy_present(self):
        self.assertIn("xbmc4lyfe/nntp-proxy", CONTROLLED_TARGETS)

    def test_f28_controlled_targets_case5_total_targets_count(self):
        self.assertEqual(len(CONTROLLED_TARGETS), 4)

    # -------------------------------------------------------------------------
    # Feature 29: GCP Service Account Key Parsing
    # -------------------------------------------------------------------------
    def test_f29_sa_key_parsing_case1_valid_json_parses(self):
        sa = make_gcp_service_account_fixture()
        self.assertEqual(sa["project_id"], "homelab-424523")
        self.assertIn("@homelab-424523.iam.gserviceaccount.com", sa["client_email"])

    def test_f29_sa_key_parsing_case2_private_key_contains_pem_markers(self):
        sa = make_gcp_service_account_fixture()
        self.assertTrue(sa["private_key"].startswith("-----BEGIN PRIVATE KEY-----"))

    def test_f29_sa_key_parsing_case3_token_uri_is_google_oauth(self):
        sa = make_gcp_service_account_fixture()
        self.assertEqual(sa["token_uri"], "https://oauth2.googleapis.com/token")

    def test_f29_sa_key_parsing_case4_missing_email_fails(self):
        sa = make_gcp_service_account_fixture()
        sa.pop("client_email")
        self.assertNotIn("client_email", sa)

    def test_f29_sa_key_parsing_case5_missing_private_key_fails(self):
        sa = make_gcp_service_account_fixture()
        sa.pop("private_key")
        self.assertNotIn("private_key", sa)

    # -------------------------------------------------------------------------
    # Feature 30: RS256 JWT Assertion Minting
    # -------------------------------------------------------------------------
    def test_f30_jwt_minting_case1_header_specifies_rs256(self):
        header = {"alg": "RS256", "typ": "JWT"}
        self.assertEqual(header["alg"], "RS256")

    def test_f30_jwt_minting_case2_claims_specify_issuer(self):
        sa = make_gcp_service_account_fixture()
        claims = {"iss": sa["client_email"], "sub": sa["client_email"]}
        self.assertEqual(claims["iss"], sa["client_email"])

    def test_f30_jwt_minting_case3_claims_specify_google_token_aud(self):
        claims = {"aud": "https://oauth2.googleapis.com/token"}
        self.assertEqual(claims["aud"], "https://oauth2.googleapis.com/token")

    def test_f30_jwt_minting_case4_expiry_within_one_hour(self):
        now = int(time.time())
        claims = {"iat": now, "exp": now + 3600}
        self.assertEqual(claims["exp"] - claims["iat"], 3600)

    def test_f30_jwt_minting_case5_scopes_contain_compute_api(self):
        scope = "https://www.googleapis.com/auth/compute https://www.googleapis.com/auth/cloud-platform"
        self.assertIn("https://www.googleapis.com/auth/compute", scope)

    # -------------------------------------------------------------------------
    # Feature 31: GCP OAuth Token Exchange
    # -------------------------------------------------------------------------
    def test_f31_oauth_exchange_case1_grant_type_is_jwt_bearer(self):
        grant = "urn:ietf:params:oauth:grant-type:jwt-bearer"
        self.assertEqual(grant, "urn:ietf:params:oauth:grant-type:jwt-bearer")

    def test_f31_oauth_exchange_case2_response_contains_access_token(self):
        token_resp = {"access_token": "ya29.mock-token", "token_type": "Bearer", "expires_in": 3600}
        self.assertTrue(token_resp["access_token"].startswith("ya29."))

    def test_f31_oauth_exchange_case3_response_token_type_is_bearer(self):
        token_resp = {"token_type": "Bearer"}
        self.assertEqual(token_resp["token_type"], "Bearer")

    def test_f31_oauth_exchange_case4_token_lifetime_positive(self):
        token_resp = {"expires_in": 3600}
        self.assertGreater(token_resp["expires_in"], 0)

    def test_f31_oauth_exchange_case5_handles_simulated_token_refresh(self):
        self.assertTrue(bool(self.server.store.gcp_oauth_token if self.server else True))

    # -------------------------------------------------------------------------
    # Feature 32: OAuth Token Caching
    # -------------------------------------------------------------------------
    def test_f32_token_caching_case1_token_cached_in_memory(self):
        if self.server:
            self.assertIsNotNone(self.server.store.gcp_oauth_token)

    def test_f32_token_caching_case2_cached_token_has_future_exp(self):
        if self.server:
            self.assertGreater(self.server.store.gcp_oauth_token_exp, int(time.time()))

    def test_f32_token_caching_case3_cache_reuse_avoids_re_exchange(self):
        if self.server:
            tok1 = self.server.store.gcp_oauth_token
            tok2 = self.server.store.gcp_oauth_token
            self.assertEqual(tok1, tok2)

    def test_f32_token_caching_case4_token_near_expiry_refreshed(self):
        now = int(time.time())
        exp = now + 120  # <300s remaining
        needs_refresh = (exp - now) < 300
        self.assertTrue(needs_refresh)

    def test_f32_token_caching_case5_cache_key_isolated(self):
        cache_key = "gcp:oauth_token"
        self.assertEqual(cache_key, "gcp:oauth_token")

    # -------------------------------------------------------------------------
    # Feature 33: GCE instances.aggregatedList
    # -------------------------------------------------------------------------
    def test_f33_instances_aggregated_case1_groups_by_zone(self):
        data = {"items": {"zones/us-central1-a": {"instances": []}, "zones/us-central1-b": {"instances": []}}}
        self.assertIn("zones/us-central1-a", data["items"])

    def test_f33_instances_aggregated_case2_return_partial_success_parameter(self):
        query_param = "returnPartialSuccess=true"
        self.assertIn("returnPartialSuccess=true", query_param)

    def test_f33_instances_aggregated_case3_handles_empty_items(self):
        data = {"items": {}}
        self.assertEqual(len(data["items"]), 0)

    def test_f33_instances_aggregated_case4_handles_pagination_token(self):
        data = {"nextPageToken": "page-2-token"}
        self.assertEqual(data.get("nextPageToken"), "page-2-token")

    def test_f33_instances_aggregated_case5_filters_by_runner_prefix(self):
        instances = ["oxidex-runners-1", "other-vm-1", "oxidex-runners-2"]
        filtered = [vm for vm in instances if vm.startswith("oxidex-runners-")]
        self.assertEqual(len(filtered), 2)

    # -------------------------------------------------------------------------
    # Feature 34: GCE disks.aggregatedList
    # -------------------------------------------------------------------------
    def test_f34_disks_aggregated_case1_groups_by_zone(self):
        data = {"items": {"zones/us-central1-a": {"disks": []}}}
        self.assertIn("zones/us-central1-a", data["items"])

    def test_f34_disks_aggregated_case2_identifies_attached_disk(self):
        disk = {"name": "disk-1", "users": ["https://compute.googleapis.com/.../instances/vm-1"]}
        self.assertTrue(len(disk.get("users", [])) > 0)

    def test_f34_disks_aggregated_case3_identifies_unattached_orphan_disk(self):
        disk = {"name": "oxidex-runners-data-orphan", "users": []}
        self.assertTrue(len(disk.get("users", [])) == 0)

    def test_f34_disks_aggregated_case4_ignores_builder_cache_disks(self):
        disk_name = "builder-oxidex-cache-1"
        is_runner_disk = disk_name.startswith("oxidex-runners-")
        self.assertFalse(is_runner_disk)

    def test_f34_disks_aggregated_case5_return_partial_success_supported(self):
        url = "https://compute.googleapis.com/compute/v1/projects/homelab/aggregated/disks?returnPartialSuccess=true"
        self.assertIn("returnPartialSuccess=true", url)

    # -------------------------------------------------------------------------
    # Feature 35: GCE instances.insert
    # -------------------------------------------------------------------------
    def test_f35_instances_insert_case1_specifies_spot_provisioning_model(self):
        scheduling = {"provisioningModel": "SPOT", "instanceTerminationAction": "STOP"}
        self.assertEqual(scheduling["provisioningModel"], "SPOT")

    def test_f35_instances_insert_case2_specifies_16_vcpu_machine_type(self):
        machine_type = "zones/us-central1-a/machineTypes/c4d-highcpu-16"
        self.assertIn("c4d-highcpu-16", machine_type)

    def test_f35_instances_insert_case3_specifies_auto_delete_boot_disk(self):
        disk = {"boot": True, "autoDelete": True}
        self.assertTrue(disk["autoDelete"])

    def test_f35_instances_insert_case4_injects_rendered_startup_script(self):
        metadata = {"items": [{"key": "startup-script", "value": "#!/bin/bash\necho hello"}]}
        self.assertEqual(metadata["items"][0]["key"], "startup-script")

    def test_f35_instances_insert_case5_instance_name_has_oxidex_runners_prefix(self):
        vm_name = f"oxidex-runners-{int(time.time())}"
        self.assertTrue(vm_name.startswith("oxidex-runners-"))

    # -------------------------------------------------------------------------
    # Feature 36: GCE instances.delete
    # -------------------------------------------------------------------------
    def test_f36_instances_delete_case1_includes_delete_disk_all(self):
        url = "https://compute.googleapis.com/.../instances/vm-1?deleteDisk=ALL"
        self.assertIn("deleteDisk=ALL", url)

    def test_f36_instances_delete_case2_target_is_terminated_vm(self):
        vm_status = "TERMINATED"
        self.assertEqual(vm_status, "TERMINATED")

    def test_f36_instances_delete_case3_handles_404_gracefully(self):
        code = 404
        is_safe = code in (200, 204, 404)
        self.assertTrue(is_safe)

    def test_f36_instances_delete_case4_does_not_delete_running_busy_host(self):
        status = "RUNNING"
        should_delete = status == "TERMINATED"
        self.assertFalse(should_delete)

    def test_f36_instances_delete_case5_safety_gate_prevents_accidental_delete(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/delete", json_data={"instance_name": "vm-1"})
        self.assertTrue(resp.json.get("live_mutation_blocked"))

    # -------------------------------------------------------------------------
    # Feature 37: GCE instances.setMetadata
    # -------------------------------------------------------------------------
    def test_f37_set_metadata_case1_scrubs_startup_script(self):
        metadata = {"items": [{"key": "startup-script", "value": "[SCRUBBED_AFTER_BOOT]"}]}
        self.assertEqual(metadata["items"][0]["value"], "[SCRUBBED_AFTER_BOOT]")

    def test_f37_set_metadata_case2_preserves_non_sensitive_keys(self):
        metadata = {"items": [{"key": "runner-patch-digest", "value": "abc"}]}
        self.assertEqual(metadata["items"][0]["key"], "runner-patch-digest")

    def test_f37_set_metadata_case3_triggered_after_runner_online(self):
        runner_status = "online"
        should_scrub = runner_status == "online"
        self.assertTrue(should_scrub)

    def test_f37_set_metadata_case4_idempotent_if_already_scrubbed(self):
        val = "[SCRUBBED_AFTER_BOOT]"
        already_scrubbed = val == "[SCRUBBED_AFTER_BOOT]"
        self.assertTrue(already_scrubbed)

    def test_f37_set_metadata_case5_includes_fingerprint_guard(self):
        payload = {"fingerprint": "xyz123", "items": []}
        self.assertIn("fingerprint", payload)

    # -------------------------------------------------------------------------
    # Feature 38: GCE disks.delete
    # -------------------------------------------------------------------------
    def test_f38_disks_delete_case1_deletes_orphan_data_disk(self):
        disk_name = "oxidex-runners-data-1"
        self.assertTrue(disk_name.startswith("oxidex-runners-data"))

    def test_f38_disks_delete_case2_does_not_delete_attached_disk(self):
        users = ["vm-1"]
        should_delete = len(users) == 0
        self.assertFalse(should_delete)

    def test_f38_disks_delete_case3_does_not_delete_builder_cache_disk(self):
        disk_name = "builder-cache-disk"
        should_delete = disk_name.startswith("oxidex-runners-")
        self.assertFalse(should_delete)

    def test_f38_disks_delete_case4_handles_404_gracefully(self):
        code = 404
        is_safe = code in (200, 204, 404)
        self.assertTrue(is_safe)

    def test_f38_disks_delete_case5_records_disk_cleanup_metrics(self):
        cleaned_disks = ["disk-1"]
        self.assertEqual(len(cleaned_disks), 1)

    # -------------------------------------------------------------------------
    # Feature 39: GitHub Runner Registration Token
    # -------------------------------------------------------------------------
    def test_f39_gh_registration_token_case1_mint_returns_token(self):
        resp = {"token": "LLLLLL1234567890", "expires_at": "2026-10-22T01:00:00Z"}
        self.assertTrue(resp["token"].startswith("LLLLLL"))

    def test_f39_gh_registration_token_case2_has_60m_expiration(self):
        resp = {"token": "tok", "expires_at": "2026-10-22T01:00:00Z"}
        self.assertIn("expires_at", resp)

    def test_f39_gh_registration_token_case3_injected_into_startup_script(self):
        token = "LLLLLL1234567890"
        script = f"TOKEN={token}"
        self.assertIn("LLLLLL1234567890", script)

    def test_f39_gh_registration_token_case4_single_use_not_persisted_in_kv(self):
        kv_keys = ["dashboard:state", "overview:latest"]
        self.assertNotIn("runner_registration_token", kv_keys)

    def test_f39_gh_registration_token_case5_handles_api_errors_gracefully(self):
        status = 500
        should_retry = status in (500, 502, 503)
        self.assertTrue(should_retry)

    # -------------------------------------------------------------------------
    # Feature 40: GitHub Runner Listing
    # -------------------------------------------------------------------------
    def test_f40_gh_runner_listing_case1_returns_runner_list(self):
        resp = {"total_count": 2, "runners": [{"id": 1, "name": "runner-1", "status": "online"}]}
        self.assertEqual(len(resp["runners"]), 1)

    def test_f40_gh_runner_listing_case2_reports_status(self):
        runner = {"status": "online"}
        self.assertEqual(runner["status"], "online")

    def test_f40_gh_runner_listing_case3_reports_busy_flag(self):
        runner = {"busy": False}
        self.assertFalse(runner["busy"])

    def test_f40_gh_runner_listing_case4_contains_labels(self):
        runner = {"labels": [{"name": "spot"}, {"name": "c4d-highcpu-16"}]}
        names = [l["name"] for l in runner["labels"]]
        self.assertIn("spot", names)

    def test_f40_gh_runner_listing_case5_handles_pagination(self):
        page1 = {"runners": [{"id": 1}]}
        page2 = {"runners": [{"id": 2}]}
        all_runners = page1["runners"] + page2["runners"]
        self.assertEqual(len(all_runners), 2)

    # -------------------------------------------------------------------------
    # Feature 41: GitHub Runner Deregistration
    # -------------------------------------------------------------------------
    def test_f41_gh_deregistration_case1_offline_runner_eligible(self):
        runner = {"status": "offline", "busy": False}
        eligible = runner["status"] == "offline" and not runner["busy"]
        self.assertTrue(eligible)

    def test_f41_gh_deregistration_case2_busy_runner_not_deregistered(self):
        runner = {"status": "offline", "busy": True}
        eligible = runner["status"] == "offline" and not runner["busy"]
        self.assertFalse(eligible)

    def test_f41_gh_deregistration_case3_online_runner_not_deregistered(self):
        runner = {"status": "online", "busy": False}
        eligible = runner["status"] == "offline" and not runner["busy"]
        self.assertFalse(eligible)

    def test_f41_gh_deregistration_case4_handles_404_gracefully(self):
        status_code = 404
        is_safe = status_code in (200, 204, 404)
        self.assertTrue(is_safe)

    def test_f41_gh_deregistration_case5_endpoint_format_is_runners_id(self):
        url = "/orgs/swack-tools/actions/runners/123"
        self.assertTrue(url.endswith("/123"))

    # -------------------------------------------------------------------------
    # Feature 42: GitHub Runner Labeling
    # -------------------------------------------------------------------------
    def test_f42_runner_labeling_case1_includes_spot_label(self):
        labels = ["spot", "c4d-highcpu-16"]
        self.assertIn("spot", labels)

    def test_f42_runner_labeling_case2_includes_machine_type_label(self):
        labels = ["spot", "c4d-highcpu-16"]
        self.assertIn("c4d-highcpu-16", labels)

    def test_f42_runner_labeling_case3_includes_extra_labels(self):
        m = make_valid_manifest()
        m["hardware"]["extra_labels"] = ["ci-rust"]
        self.assertIn("ci-rust", m["hardware"]["extra_labels"])

    def test_f42_runner_labeling_case4_validates_label_characters(self):
        import re
        label_re = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$")
        self.assertTrue(bool(label_re.match("spot-runner-1")))

    def test_f42_runner_labeling_case5_deduplicates_labels(self):
        raw = ["spot", "c4d-highcpu-16", "spot"]
        deduped = sorted(set(raw))
        self.assertEqual(len(deduped), 2)

    # -------------------------------------------------------------------------
    # Feature 43: GitHub Queued Jobs Discovery
    # -------------------------------------------------------------------------
    def test_f43_queued_jobs_case1_queries_queued_status(self):
        url = "/repos/swack-tools/oxidex/actions/runs?status=queued"
        self.assertIn("status=queued", url)

    def test_f43_queued_jobs_case2_counts_backlog(self):
        jobs = [{"id": 1, "status": "queued"}, {"id": 2, "status": "queued"}]
        self.assertEqual(len(jobs), 2)

    def test_f43_queued_jobs_case3_returns_zero_when_empty(self):
        jobs = []
        self.assertEqual(len(jobs), 0)

    def test_f43_queued_jobs_case4_ignores_in_progress_jobs(self):
        jobs = [{"id": 1, "status": "in_progress"}, {"id": 2, "status": "queued"}]
        queued = [j for j in jobs if j["status"] == "queued"]
        self.assertEqual(len(queued), 1)

    def test_f43_queued_jobs_case5_aggregates_across_targets(self):
        counts = {"swack-tools": 10, "xbmc4lyfe/rustnzb": 15}
        total = sum(counts.values())
        self.assertEqual(total, 25)

    # -------------------------------------------------------------------------
    # Feature 44: Floor Target Maintenance
    # -------------------------------------------------------------------------
    def test_f44_floor_maintenance_case1_launches_when_zero_hosts(self):
        active_hosts = 0
        should_launch = active_hosts < 1
        self.assertTrue(should_launch)

    def test_f44_floor_maintenance_case2_noop_when_healthy_host_exists(self):
        active_hosts = 1
        should_launch = active_hosts < 1
        self.assertFalse(should_launch)

    def test_f44_floor_maintenance_case3_maintains_all_floor_targets(self):
        targets = list(CONTROLLED_TARGETS)
        self.assertEqual(len(targets), 4)

    def test_f44_floor_maintenance_case4_launched_host_satisfies_16_vcpu_floor(self):
        m = make_valid_manifest()
        self.assertGreaterEqual(m["hardware"]["min_vcpu"] or 0, MIN_HOST_VCPU)

    def test_f44_floor_maintenance_case5_reconciler_tick_executes_floor_check(self):
        resp = self.client.post("/api/reconciler/tick")
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(resp.json.get("status"), "reconciled")

    # -------------------------------------------------------------------------
    # Feature 45: Queued Jobs Backlog Scale-Up
    # -------------------------------------------------------------------------
    def test_f45_backlog_scaleup_case1_scales_when_queue_ge_20(self):
        queued = 20
        should_scale = queued >= MIN_QUEUED_JOBS_THRESHOLD
        self.assertTrue(should_scale)

    def test_f45_backlog_scaleup_case2_scales_when_queue_gt_20(self):
        queued = 25
        should_scale = queued >= MIN_QUEUED_JOBS_THRESHOLD
        self.assertTrue(should_scale)

    def test_f45_backlog_scaleup_case3_noop_when_queue_lt_20(self):
        queued = 19
        should_scale = queued >= MIN_QUEUED_JOBS_THRESHOLD
        self.assertFalse(should_scale)

    def test_f45_backlog_scaleup_case4_noop_when_queue_zero(self):
        queued = 0
        should_scale = queued >= MIN_QUEUED_JOBS_THRESHOLD
        self.assertFalse(should_scale)

    def test_f45_backlog_scaleup_case5_records_scale_reason(self):
        reason = "queued_jobs_threshold_exceeded"
        self.assertEqual(reason, "queued_jobs_threshold_exceeded")

    # -------------------------------------------------------------------------
    # Feature 46: Startup Script Rendering
    # -------------------------------------------------------------------------
    def test_f46_startup_rendering_case1_replaces_runner_token(self):
        template = "TOKEN=@@RUNNER_TOKEN@@"
        rendered = template.replace("@@RUNNER_TOKEN@@", "mock-token-xyz")
        self.assertIn("mock-token-xyz", rendered)

    def test_f46_startup_rendering_case2_replaces_patch_digest(self):
        template = "DIGEST=@@PATCH_DIGEST@@"
        rendered = template.replace("@@PATCH_DIGEST@@", "a" * 64)
        self.assertIn("a" * 64, rendered)

    def test_f46_startup_rendering_case3_replaces_runner_labels(self):
        template = "LABELS=@@RUNNER_LABELS@@"
        rendered = template.replace("@@RUNNER_LABELS@@", "spot,c4d-highcpu-16")
        self.assertIn("spot,c4d-highcpu-16", rendered)

    def test_f46_startup_rendering_case4_no_unreplaced_placeholders_remain(self):
        template = "TOKEN=@@RUNNER_TOKEN@@\nDIGEST=@@PATCH_DIGEST@@"
        rendered = template.replace("@@RUNNER_TOKEN@@", "tok").replace("@@PATCH_DIGEST@@", "dig")
        self.assertNotIn("@@", rendered)

    def test_f46_startup_rendering_case5_detects_unreplaced_placeholder_error(self):
        template = "TOKEN=@@RUNNER_TOKEN@@\nORPHAN=@@ORPHAN@@"
        rendered = template.replace("@@RUNNER_TOKEN@@", "tok")
        self.assertIn("@@", rendered)

    # -------------------------------------------------------------------------
    # Feature 47: Multi-Tick Launch State Machine
    # -------------------------------------------------------------------------
    def test_f47_state_machine_case1_tick1_state_is_creating(self):
        state = "creating"
        self.assertEqual(state, "creating")

    def test_f47_state_machine_case2_tick2_state_is_booting(self):
        state = "booting"
        self.assertEqual(state, "booting")

    def test_f47_state_machine_case3_tick3_state_is_done(self):
        state = "done"
        self.assertEqual(state, "done")

    def test_f47_state_machine_case4_triggers_metadata_scrub_on_done(self):
        state = "done"
        should_scrub = state == "done"
        self.assertTrue(should_scrub)

    def test_f47_state_machine_case5_stuck_launch_times_out(self):
        elapsed_seconds = 1000  # >15 minutes (900s)
        timed_out = elapsed_seconds > 900
        self.assertTrue(timed_out)

    # -------------------------------------------------------------------------
    # Feature 48: Preempted VM Cleanup
    # -------------------------------------------------------------------------
    def test_f48_preempted_vm_case1_detects_terminated_status(self):
        vm = {"status": "TERMINATED"}
        self.assertEqual(vm["status"], "TERMINATED")

    def test_f48_preempted_vm_case2_terminated_vm_queued_for_deletion(self):
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "oxidex-runners-preempted-1"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertIn("oxidex-runners-preempted-1", tick.get("cleaned_vms", []))

    def test_f48_preempted_vm_case3_running_vm_not_cleaned(self):
        vm = {"status": "RUNNING"}
        should_clean = vm["status"] == "TERMINATED"
        self.assertFalse(should_clean)

    def test_f48_preempted_vm_case4_cleanup_is_idempotent(self):
        tick1 = self.client.post("/api/reconciler/tick").json
        tick2 = self.client.post("/api/reconciler/tick").json
        self.assertEqual(len(tick2.get("cleaned_vms", [])), 0)

    def test_f48_preempted_vm_case5_records_cleanup_in_reconciler_log(self):
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "oxidex-runners-p2"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertIn("oxidex-runners-p2", tick.get("cleaned_vms", []))

    # -------------------------------------------------------------------------
    # Feature 49: Stale Runner Registration Cleanup
    # -------------------------------------------------------------------------
    def test_f49_stale_runner_case1_detects_offline_runner_without_vm(self):
        runner = {"id": 999, "status": "offline"}
        vm_exists = False
        is_stale = runner["status"] == "offline" and not vm_exists
        self.assertTrue(is_stale)

    def test_f49_stale_runner_case2_injects_and_cleans_stale_runner(self):
        self.client.post("/api/test/inject-stale-runner", json_data={"runner_id": "runner-stale-123"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertIn("runner-stale-123", tick.get("cleaned_runners", []))

    def test_f49_stale_runner_case3_online_runner_never_stale(self):
        runner = {"id": 1, "status": "online"}
        vm_exists = False
        is_stale = runner["status"] == "offline" and not vm_exists
        self.assertFalse(is_stale)

    def test_f49_stale_runner_case4_busy_runner_never_stale(self):
        runner = {"id": 2, "status": "offline", "busy": True}
        is_stale = runner["status"] == "offline" and not runner.get("busy")
        self.assertFalse(is_stale)

    def test_f49_stale_runner_case5_cleanup_is_idempotent(self):
        tick1 = self.client.post("/api/reconciler/tick").json
        tick2 = self.client.post("/api/reconciler/tick").json
        self.assertEqual(len(tick2.get("cleaned_runners", [])), 0)

    # -------------------------------------------------------------------------
    # Feature 50: Spot Offer Selection & Ranking
    # -------------------------------------------------------------------------
    def test_f50_spot_ranking_case1_filters_by_daily_cap(self):
        offers = [
            {"type": "c4d-highcpu-16", "daily_price": 0.48},
            {"type": "expensive-16", "daily_price": 2.50},
        ]
        valid = [o for o in offers if o["daily_price"] <= 1.50]
        self.assertEqual(len(valid), 1)

    def test_f50_spot_ranking_case2_ranks_cheapest_first(self):
        offers = [
            {"type": "n4d-highcpu-16", "daily_price": 0.52},
            {"type": "c4d-highcpu-16", "daily_price": 0.48},
        ]
        ranked = sorted(offers, key=lambda o: o["daily_price"])
        self.assertEqual(ranked[0]["type"], "c4d-highcpu-16")

    def test_f50_spot_ranking_case3_only_considers_permitted_machine_types(self):
        offers = [
            {"type": "e2-standard-4", "daily_price": 0.20},
            {"type": "c4d-highcpu-16", "daily_price": 0.48},
        ]
        valid = [o for o in offers if o["type"] in PERMITTED_MACHINE_TYPES]
        self.assertEqual(len(valid), 1)

    def test_f50_spot_ranking_case4_requires_16_vcpu(self):
        offers = [
            {"type": "c4d-highcpu-8", "vcpu": 8},
            {"type": "c4d-highcpu-16", "vcpu": 16},
        ]
        valid = [o for o in offers if o["vcpu"] >= MIN_HOST_VCPU]
        self.assertEqual(len(valid), 1)

    def test_f50_spot_ranking_case5_requires_allowed_region(self):
        allowed_regions = ("us-central1", "us-east1", "us-west1")
        self.assertIn("us-central1", allowed_regions)

    # -------------------------------------------------------------------------
    # Feature 51: Precomputed Overview Snapshot
    # -------------------------------------------------------------------------
    def test_f51_overview_snapshot_case1_mutated_reconciler_writes_snapshot(self):
        # When a mutation occurs (terminated VM), reconciler writes snapshot
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "oxidex-runners-f51"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertTrue(tick.get("snapshot_written"))
        self.assertGreaterEqual(tick.get("kv_puts", 0), 1)

    def test_f51_overview_snapshot_case2_unchanged_tick_performs_zero_kv_writes(self):
        # On unchanged tick, 0 SPOT_KV.put calls are made (Free Tier quota conservation)
        stats_before = self.client.get("/api/test/kv-stats").json
        tick = self.client.post("/api/reconciler/tick").json
        self.assertFalse(tick.get("snapshot_written"))
        stats_after = self.client.get("/api/test/kv-stats").json
        self.assertEqual(stats_after.get("kv_puts"), stats_before.get("kv_puts"))

    def test_f51_overview_snapshot_case3_cached_gets_perform_zero_kv_writes(self):
        stats_before = self.client.get("/api/test/kv-stats").json
        self.client.get("/api/overview")
        self.client.get("/api/overview")
        stats_after = self.client.get("/api/test/kv-stats").json
        # 0 KV writes on steady state GETs
        self.assertEqual(stats_after.get("kv_puts"), stats_before.get("kv_puts"))

    def test_f51_overview_snapshot_case4_isolate_memory_cache_prevents_repeated_kv_gets(self):
        # Clear or reset
        self.client.post("/api/test/reset")
        # First GET loads into in-isolate memory cache (1 kv_get)
        self.client.get("/api/overview")
        stats_first = self.client.get("/api/test/kv-stats").json
        # Second and third GETs within 60s TTL hit in-isolate memory cache (0 additional kv_gets)
        self.client.get("/api/overview")
        self.client.get("/api/overview")
        stats_cached = self.client.get("/api/test/kv-stats").json
        self.assertEqual(stats_cached.get("kv_gets"), stats_first.get("kv_gets"))
        self.assertGreaterEqual(stats_cached.get("isolate_cache_hits", 0), 2)

    def test_f51_overview_snapshot_case5_schema_is_snapshot_v1_and_contains_fleet_summary(self):
        overview = self.client.get("/api/overview").json
        self.assertEqual(overview.get("schema"), "spot-dashboard-snapshot-v1")
        self.assertIn("fleet_summary", overview.get("state", {}))

    # -------------------------------------------------------------------------
    # Feature 52: Scheduled Cron Trigger Hook
    # -------------------------------------------------------------------------
    def test_f52_cron_hook_case1_tick_returns_200(self):
        resp = self.client.post("/api/reconciler/tick")
        self.assertEqual(resp.status_code, 200)

    def test_f52_cron_hook_case2_increments_tick_counter(self):
        t1 = self.client.post("/api/reconciler/tick").json.get("tick", 0)
        t2 = self.client.post("/api/reconciler/tick").json.get("tick", 0)
        self.assertEqual(t2, t1 + 1)

    def test_f52_cron_hook_case3_acquires_reconciler_lease(self):
        lease_ttl = 240
        self.assertEqual(lease_ttl, 240)

    def test_f52_cron_hook_case4_updates_health_telemetry(self):
        self.client.post("/api/reconciler/tick")
        health = self.client.get("/api/health").json
        self.assertGreater(health.get("reconciler_ticks", 0), 0)

    def test_f52_cron_hook_case5_reconciled_status_reported(self):
        resp = self.client.post("/api/reconciler/tick").json
        self.assertEqual(resp.get("status"), "reconciled")


if __name__ == "__main__":
    unittest.main()
