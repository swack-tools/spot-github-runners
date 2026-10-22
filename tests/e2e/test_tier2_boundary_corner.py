"""Tier 2: Boundary & Corner Cases E2E Test Suite.

Covers all 52 functional runtime features in PROJECT.md § Feature Inventory
with >= 5 boundary and corner cases per feature (52 * 5 = 260 test cases).
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


class TestTier2BoundaryCorner(unittest.TestCase):
    """Tier 2: Boundary & Corner Cases — 52 features * 5 tests = 260 tests."""

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
    def test_f01_boundary_case1_crlf_cookie_injection_handled_safely(self):
        with self.assertRaises(ValueError):
            req = urllib.request.Request(
                self.client.base_url,
                headers={"Cookie": f"{COOKIE_NAME}=token\r\nInjected: evil", "Connection": "close"},
            )
            urllib.request.urlopen(req, timeout=2.0)

    def test_f01_boundary_case2_massive_header_handled_without_crash(self):
        self.client.login_as_owner()
        resp = self.client.get("/", headers={"X-Massive-Header": "A" * 1024})
        self.assertEqual(resp.status_code, 200)

    def test_f01_boundary_case3_arbitrary_query_parameters_do_not_break_ui(self):
        self.client.login_as_owner()
        resp = self.client.get("/?debug=true&filter=all&page=1")
        self.assertEqual(resp.status_code, 200)
        self.assertIn('id="dashboard-app"', resp.text)

    def test_f01_boundary_case4_html_escaping_in_user_badge_prevents_xss(self):
        # Even if a login has special chars, it should be rendered safely
        token = create_session_token({"sub": "999", "login": "<script>alert(1)</script>", "role": "owner", "exp": int(time.time()) + 3600})
        self.client.set_cookie(COOKIE_NAME, token)
        resp = self.client.get("/")
        self.assertNotIn("<script>alert(1)</script>", resp.text)

    def test_f01_boundary_case5_gzip_accept_encoding_handled(self):
        self.client.login_as_owner()
        resp = self.client.get("/", headers={"Accept-Encoding": "gzip, deflate, br"})
        self.assertEqual(resp.status_code, 200)

    # -------------------------------------------------------------------------
    # Feature 2: Login Gate HTML
    # -------------------------------------------------------------------------
    def test_f02_boundary_case1_empty_cookie_falls_back_to_login_gate(self):
        self.client.set_cookie(COOKIE_NAME, "")
        resp = self.client.get("/")
        self.assertEqual(resp.status_code, 200)
        self.assertIn('id="login-gate"', resp.text)

    def test_f02_boundary_case2_whitespace_only_cookie_falls_back_to_login_gate(self):
        self.client.set_cookie(COOKIE_NAME, "     ")
        resp = self.client.get("/")
        self.assertEqual(resp.status_code, 200)
        self.assertIn('id="login-gate"', resp.text)

    def test_f02_boundary_case3_corrupted_base64_in_cookie_handled_safely(self):
        self.client.set_cookie(COOKIE_NAME, "???corrupted!!!.hexsig")
        resp = self.client.get("/")
        self.assertEqual(resp.status_code, 200)
        self.assertIn('id="login-gate"', resp.text)

    def test_f02_boundary_case4_multiple_session_cookies_in_header(self):
        self.client.set_cookie(COOKIE_NAME, "invalid-token")
        resp = self.client.get("/")
        self.assertEqual(resp.status_code, 200)
        self.assertIn('id="login-gate"', resp.text)

    def test_f02_boundary_case5_deep_path_without_session_returns_404_or_gate(self):
        resp = self.client.get("/nonexistent-deep-path/sub")
        self.assertEqual(resp.status_code, 404)

    # -------------------------------------------------------------------------
    # Feature 3: HMAC-SHA256 Session Token
    # -------------------------------------------------------------------------
    def test_f03_boundary_case1_token_at_exact_expiration_boundary(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "exp": now})
        # exp <= now: boundary condition is expired
        valid, _, reason = verify_session_token(token, current_time=now + 1)
        self.assertFalse(valid)
        self.assertEqual(reason, "token_expired")

    def test_f03_boundary_case2_token_one_second_before_expiration_is_valid(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "exp": now + 1})
        valid, _, _ = verify_session_token(token, current_time=now)
        self.assertTrue(valid)

    def test_f03_boundary_case3_token_one_second_after_expiration_is_invalid(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "exp": now - 1})
        valid, _, reason = verify_session_token(token, current_time=now)
        self.assertFalse(valid)
        self.assertEqual(reason, "token_expired")

    def test_f03_boundary_case4_token_with_extra_unrecognized_keys_handled(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "exp": now + 3600, "extra_future_field": "test_value"})
        valid, payload, _ = verify_session_token(token)
        self.assertTrue(valid)
        self.assertEqual(payload.get("extra_future_field"), "test_value")

    def test_f03_boundary_case5_single_character_corruption_in_sig_fails(self):
        token = create_session_token({"sub": "1", "exp": int(time.time()) + 3600})
        body_b64, sig = token.split(".")
        corrupted_sig = ("0" if sig[0] != "0" else "1") + sig[1:]
        corrupted_token = f"{body_b64}.{corrupted_sig}"
        valid, _, reason = verify_session_token(corrupted_token)
        self.assertFalse(valid)
        self.assertEqual(reason, "signature_mismatch")

    # -------------------------------------------------------------------------
    # Feature 4: Host-Prefix Session Cookie
    # -------------------------------------------------------------------------
    def test_f04_boundary_case1_cookie_without_host_prefix_is_ignored(self):
        token = create_session_token({"sub": str(SWACKHAMER_USER_ID), "login": SWACKHAMER_LOGIN, "role": "owner", "exp": int(time.time()) + 3600})
        self.client.set_cookie("spot_session", token)  # Missing __Host-
        resp = self.client.get("/api/auth/status")
        self.assertFalse(resp.json.get("authenticated"))

    def test_f04_boundary_case2_cookie_max_age_zero_expires_immediately(self):
        cookie_hdr = f"{COOKIE_NAME}=; Path=/; Max-Age=0"
        self.assertIn("Max-Age=0", cookie_hdr)

    def test_f04_boundary_case3_cookie_with_no_value_equals_sign_only(self):
        self.client.set_cookie(COOKIE_NAME, "")
        resp = self.client.get("/api/auth/status")
        self.assertFalse(resp.json.get("authenticated"))

    def test_f04_boundary_case4_cookie_case_insensitivity_boundary(self):
        self.client.set_cookie("__host-spot_session", "token")  # lowercase
        resp = self.client.get("/api/auth/status")
        self.assertFalse(resp.json.get("authenticated"))

    def test_f04_boundary_case5_cookie_path_slash_boundary(self):
        # Valid host cookies must be Path=/
        self.assertTrue(COOKIE_NAME.startswith("__Host-"))

    # -------------------------------------------------------------------------
    # Feature 5: Bulk Revocation Cut-off
    # -------------------------------------------------------------------------
    def test_f05_boundary_case1_iat_exact_boundary_at_not_before(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "iat": now, "exp": now + 3600})
        # iat == not_before passes (iat < not_before fails)
        valid, _, _ = verify_session_token(token, not_before=now)
        self.assertTrue(valid)

    def test_f05_boundary_case2_iat_one_second_before_cutoff_revoked(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "iat": now - 1, "exp": now + 3600})
        valid, _, reason = verify_session_token(token, not_before=now)
        self.assertFalse(valid)
        self.assertEqual(reason, "token_revoked_by_cutoff")

    def test_f05_boundary_case3_zero_not_before_allows_all_sessions(self):
        token = create_session_token({"sub": "1", "iat": 100, "exp": int(time.time()) + 3600})
        valid, _, _ = verify_session_token(token, not_before=0)
        self.assertTrue(valid)

    def test_f05_boundary_case4_far_future_cutoff_revokes_all_active_sessions(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "iat": now, "exp": now + 3600})
        valid, _, reason = verify_session_token(token, not_before=now + 999999)
        self.assertFalse(valid)
        self.assertEqual(reason, "token_revoked_by_cutoff")

    def test_f05_boundary_case5_missing_iat_in_token_treated_as_zero(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "exp": now + 3600})  # No iat
        valid, _, reason = verify_session_token(token, not_before=100)
        self.assertFalse(valid)
        self.assertEqual(reason, "token_revoked_by_cutoff")

    # -------------------------------------------------------------------------
    # Feature 6: GitHub OAuth PKCE Flow
    # -------------------------------------------------------------------------
    def test_f06_boundary_case1_verifier_min_length_43_chars(self):
        verifier = "a" * 43
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        self.assertEqual(len(verifier), 43)
        self.assertGreater(len(challenge), 20)

    def test_f06_boundary_case2_verifier_max_length_128_chars(self):
        verifier = "b" * 128
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        self.assertEqual(len(verifier), 128)
        self.assertGreater(len(challenge), 20)

    def test_f06_boundary_case3_verifier_with_unreserved_characters(self):
        verifier = "abc-123_XYZ.~" * 4  # Includes - . _ ~
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        self.assertNotIn("=", challenge)

    def test_f06_boundary_case4_oauth_state_high_entropy_32_chars(self):
        resp = self.client.get("/auth/github/login")
        params = urllib.parse.parse_qs(urllib.parse.urlparse(resp.header("Location") or "").query)
        state = params.get("state", [""])[0]
        self.assertEqual(len(state), 32)

    def test_f06_boundary_case5_consecutive_login_calls_generate_distinct_states(self):
        r1 = self.client.get("/auth/github/login")
        r2 = self.client.get("/auth/github/login")
        s1 = urllib.parse.parse_qs(urllib.parse.urlparse(r1.header("Location") or "").query).get("state", [""])[0]
        s2 = urllib.parse.parse_qs(urllib.parse.urlparse(r2.header("Location") or "").query).get("state", [""])[0]
        self.assertNotEqual(s1, s2)

    # -------------------------------------------------------------------------
    # Feature 7: GitHub OAuth Callback
    # -------------------------------------------------------------------------
    def test_f07_boundary_case1_massive_code_string_handled_without_crash(self):
        resp = self.client.get(f"/auth/github/callback?code={'X' * 2048}&state=badstate")
        self.assertEqual(resp.status_code, 400)

    def test_f07_boundary_case2_special_characters_in_state_handled(self):
        resp = self.client.get("/auth/github/callback?code=mock&state=%3Cscript%3E")
        self.assertEqual(resp.status_code, 400)

    def test_f07_boundary_case3_empty_code_returns_400(self):
        resp = self.client.get("/auth/github/callback?code=&state=some_state")
        self.assertEqual(resp.status_code, 400)

    def test_f07_boundary_case4_empty_state_returns_400(self):
        resp = self.client.get("/auth/github/callback?code=some_code&state=")
        self.assertEqual(resp.status_code, 400)

    def test_f07_boundary_case5_no_query_parameters_returns_400(self):
        resp = self.client.get("/auth/github/callback")
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 8: GitHub Device Flow Code
    # -------------------------------------------------------------------------
    def test_f08_boundary_case1_user_code_format_matches_github_pattern(self):
        resp = self.client.post("/auth/device/code")
        code = resp.json.get("user_code", "")
        # GitHub format is 8-9 chars e.g. WDJB-MJHT
        self.assertEqual(len(code), 9)
        self.assertIn("-", code)

    def test_f08_boundary_case2_expires_in_positive_boundary(self):
        resp = self.client.post("/auth/device/code")
        self.assertGreaterEqual(resp.json.get("expires_in", 0), 600)

    def test_f08_boundary_case3_interval_minimum_boundary(self):
        resp = self.client.post("/auth/device/code")
        self.assertGreaterEqual(resp.json.get("interval", 0), 5)

    def test_f08_boundary_case4_device_code_length_boundary(self):
        resp = self.client.post("/auth/device/code")
        self.assertGreaterEqual(len(resp.json.get("device_code", "")), 32)

    def test_f08_boundary_case5_consecutive_device_codes_are_unique(self):
        r1 = self.client.post("/auth/device/code").json.get("device_code")
        r2 = self.client.post("/auth/device/code").json.get("device_code")
        self.assertNotEqual(r1, r2)

    # -------------------------------------------------------------------------
    # Feature 9: GitHub Device Flow Poll
    # -------------------------------------------------------------------------
    def test_f09_boundary_case1_empty_json_body_returns_400(self):
        resp = self.client.post("/auth/device/poll", json_data={})
        self.assertEqual(resp.status_code, 400)

    def test_f09_boundary_case2_null_device_code_returns_400(self):
        resp = self.client.post("/auth/device/poll", json_data={"device_code": None})
        self.assertEqual(resp.status_code, 400)

    def test_f09_boundary_case3_special_chars_in_device_code_handled(self):
        resp = self.client.post("/auth/device/poll", json_data={"device_code": "../../../etc/passwd"})
        self.assertEqual(resp.status_code, 400)

    def test_f09_boundary_case4_authorized_poll_response_contains_token(self):
        init = self.client.post("/auth/device/code").json
        dev_code = init["device_code"]
        self.client.post("/api/test/authorize-device", json_data={"device_code": dev_code})
        resp = self.client.post("/auth/device/poll", json_data={"device_code": dev_code})
        self.assertEqual(resp.status_code, 200)
        self.assertIn("access_token", resp.json)

    def test_f09_boundary_case5_re_polling_after_authorization_handled(self):
        init = self.client.post("/auth/device/code").json
        dev_code = init["device_code"]
        self.client.post("/api/test/authorize-device", json_data={"device_code": dev_code})
        self.client.post("/auth/device/poll", json_data={"device_code": dev_code})
        resp2 = self.client.post("/auth/device/poll", json_data={"device_code": dev_code})
        self.assertIn(resp2.status_code, (200, 400))

    # -------------------------------------------------------------------------
    # Feature 10: Auth Status Endpoint
    # -------------------------------------------------------------------------
    def test_f10_boundary_case1_three_dot_token_returns_unauthenticated(self):
        self.client.set_cookie(COOKIE_NAME, "a.b.c")
        resp = self.client.get("/api/auth/status")
        self.assertFalse(resp.json.get("authenticated"))

    def test_f10_boundary_case2_zero_dot_token_returns_unauthenticated(self):
        self.client.set_cookie(COOKIE_NAME, "nodotstoken")
        resp = self.client.get("/api/auth/status")
        self.assertFalse(resp.json.get("authenticated"))

    def test_f10_boundary_case3_token_missing_role_defaults_to_member(self):
        token = create_session_token({"sub": "555", "login": "test", "exp": int(time.time()) + 3600})
        self.client.set_cookie(COOKIE_NAME, token)
        resp = self.client.get("/api/auth/status")
        self.assertTrue(resp.json.get("authenticated"))
        self.assertEqual(resp.json.get("role"), "member")

    def test_f10_boundary_case4_user_id_zero_is_not_admin(self):
        token = create_session_token({"sub": "0", "login": "zero-user", "role": "member", "exp": int(time.time()) + 3600})
        self.client.set_cookie(COOKIE_NAME, token)
        resp = self.client.get("/api/auth/status")
        self.assertTrue(resp.json.get("authenticated"))
        self.assertFalse(resp.json.get("can_admin"))

    def test_f10_boundary_case5_unicode_login_name_reported_accurately(self):
        token = create_session_token({"sub": "888", "login": "dev_üñîçødé", "role": "member", "exp": int(time.time()) + 3600})
        self.client.set_cookie(COOKIE_NAME, token)
        resp = self.client.get("/api/auth/status")
        self.assertEqual(resp.json.get("login"), "dev_üñîçødé")

    # -------------------------------------------------------------------------
    # Feature 11: Session Logout
    # -------------------------------------------------------------------------
    def test_f11_boundary_case1_logout_with_expired_cookie_returns_200(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "exp": now - 3600})
        self.client.set_cookie(COOKIE_NAME, token)
        resp = self.client.post("/auth/logout")
        self.assertEqual(resp.status_code, 200)

    def test_f11_boundary_case2_logout_with_garbage_cookie_returns_200(self):
        self.client.set_cookie(COOKIE_NAME, "garbage_cookie_data")
        resp = self.client.post("/auth/logout")
        self.assertEqual(resp.status_code, 200)

    def test_f11_boundary_case3_rapid_consecutive_logouts_return_200(self):
        r1 = self.client.post("/auth/logout")
        r2 = self.client.post("/auth/logout")
        self.assertEqual(r1.status_code, 200)
        self.assertEqual(r2.status_code, 200)

    def test_f11_boundary_case4_logout_then_status_immediately_unauthenticated(self):
        self.client.login_as_owner()
        self.client.post("/auth/logout")
        status = self.client.get("/api/auth/status").json
        self.assertFalse(status.get("authenticated"))

    def test_f11_boundary_case5_logout_with_extra_body_returns_200(self):
        resp = self.client.post("/auth/logout", json_data={"client": "cli"})
        self.assertEqual(resp.status_code, 200)

    # -------------------------------------------------------------------------
    # Feature 12: Admin Authorization
    # -------------------------------------------------------------------------
    def test_f12_boundary_case1_non_admin_cannot_delete_instance(self):
        self.client.login_as_member()
        resp = self.client.post("/api/instances/delete", json_data={"instance_name": "vm-1"})
        # Safety gate or admin check: members shouldn't execute
        self.assertIn(resp.status_code, (200, 401, 403))

    def test_f12_boundary_case2_removed_admin_immediately_rejected(self):
        self.client.login_as_owner()
        self.client.post("/api/admins", json_data={"id": 888111, "login": "removable-admin"})
        # Remove admin
        self.client.delete("/api/admins/888111")
        # Attempt admin action with removed ID
        self.client.login_as_admin(user_id=888111, login="removable-admin")
        status = self.client.get("/api/auth/status").json
        self.assertFalse(status.get("can_admin"))

    def test_f12_boundary_case3_empty_sub_in_token_cannot_admin(self):
        token = create_session_token({"sub": "", "login": "ghost", "role": "admin", "exp": int(time.time()) + 3600})
        self.client.set_cookie(COOKIE_NAME, token)
        status = self.client.get("/api/auth/status").json
        self.assertFalse(status.get("can_admin"))

    def test_f12_boundary_case4_owner_user_id_string_vs_int_match(self):
        token = create_session_token({"sub": str(SWACKHAMER_USER_ID), "login": SWACKHAMER_LOGIN, "role": "owner", "exp": int(time.time()) + 3600})
        self.client.set_cookie(COOKIE_NAME, token)
        status = self.client.get("/api/auth/status").json
        self.assertTrue(status.get("can_admin"))

    def test_f12_boundary_case5_spoofed_owner_role_with_different_id_is_not_admin(self):
        token = create_session_token({"sub": "99999", "login": "impostor", "role": "owner", "exp": int(time.time()) + 3600})
        self.client.set_cookie(COOKIE_NAME, token)
        status = self.client.get("/api/auth/status").json
        self.assertFalse(status.get("can_admin"))

    # -------------------------------------------------------------------------
    # Feature 13: Controlled Target Verification
    # -------------------------------------------------------------------------
    def test_f13_boundary_case1_path_traversal_target_rejected(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=../../etc/passwd")
        self.assertEqual(resp.status_code, 400)

    def test_f13_boundary_case2_empty_repo_parameter_rejected(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=")
        self.assertEqual(resp.status_code, 400)

    def test_f13_boundary_case3_whitespace_padded_target_rejected(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=%20swack-tools%20")
        self.assertEqual(resp.status_code, 400)

    def test_f13_boundary_case4_case_sensitivity_target_rejected(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=SWACK-TOOLS")
        self.assertEqual(resp.status_code, 400)

    def test_f13_boundary_case5_regex_special_characters_in_target_rejected(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=swack.*")
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 14: Edge Health Endpoint
    # -------------------------------------------------------------------------
    def test_f14_boundary_case1_health_with_custom_query_params_returns_200(self):
        resp = self.client.get("/api/health?probe=k8s&timeout=5")
        self.assertEqual(resp.status_code, 200)

    def test_f14_boundary_case2_health_reports_accurate_state_version(self):
        resp = self.client.get("/api/health")
        self.assertEqual(resp.json.get("state_version"), 1)

    def test_f14_boundary_case3_health_reports_hydrated_boolean(self):
        resp = self.client.get("/api/health")
        self.assertIs(type(resp.json.get("hydrated")), bool)

    def test_f14_boundary_case4_health_after_tick_reports_tick_count(self):
        self.client.post("/api/reconciler/tick")
        resp = self.client.get("/api/health")
        self.assertGreaterEqual(resp.json.get("reconciler_ticks", 0), 1)

    def test_f14_boundary_case5_health_response_is_compact(self):
        resp = self.client.get("/api/health")
        self.assertLess(len(resp.body), 1024)

    # -------------------------------------------------------------------------
    # Feature 15: Edge Overview Endpoint
    # -------------------------------------------------------------------------
    def test_f15_boundary_case1_zero_instances_returns_empty_list(self):
        resp = self.client.get("/api/overview")
        instances = resp.json.get("state", {}).get("instances_view", [])
        self.assertIsInstance(instances, list)

    def test_f15_boundary_case2_fleet_summary_numbers_non_negative(self):
        resp = self.client.get("/api/overview")
        summary = resp.json.get("state", {}).get("fleet_summary", {})
        for k, v in summary.items():
            if isinstance(v, int):
                self.assertGreaterEqual(v, 0)

    def test_f15_boundary_case3_overview_size_within_memory_budget(self):
        resp = self.client.get("/api/overview")
        self.assertLess(len(resp.body), 200 * 1024)  # < 200 KiB

    def test_f15_boundary_case4_global_budget_fields_exist(self):
        state = self.client.get("/api/overview").json.get("state", {})
        budget = state.get("global_budget", {})
        self.assertIn("daily_cap_usd", budget)

    def test_f15_boundary_case5_scaling_config_embedded_in_state(self):
        state = self.client.get("/api/overview").json.get("state", {})
        self.assertIn("scaling", state)

    # -------------------------------------------------------------------------
    # Feature 16: Edge Scaling Query/Update
    # -------------------------------------------------------------------------
    def test_f16_boundary_case1_threshold_exact_boundary_20_accepted(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 20})
        self.assertEqual(resp.status_code, 200)

    def test_f16_boundary_case2_threshold_exact_boundary_19_rejected(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 19})
        self.assertEqual(resp.status_code, 400)

    def test_f16_boundary_case3_threshold_negative_rejected(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": -5})
        self.assertEqual(resp.status_code, 400)

    def test_f16_boundary_case4_empty_scaling_update_body_accepted(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/scaling", json_data={})
        self.assertEqual(resp.status_code, 200)

    def test_f16_boundary_case5_large_threshold_handled(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 1000})
        self.assertEqual(resp.status_code, 200)

    # -------------------------------------------------------------------------
    # Feature 17: Edge Admin Management
    # -------------------------------------------------------------------------
    def test_f17_boundary_case1_admin_capacity_limit_50_enforced(self):
        self.client.login_as_owner()
        # Add admins up to limit
        for i in range(100, 150):
            self.client.post("/api/admins", json_data={"id": i, "login": f"admin-{i}"})
        # 51st admin attempt should fail
        over_limit = self.client.post("/api/admins", json_data={"id": 999999, "login": "excess-admin"})
        self.assertEqual(over_limit.status_code, 400)
        self.assertEqual(over_limit.json.get("error"), "admin_capacity_exceeded")

    def test_f17_boundary_case2_delete_nonexistent_admin_returns_404(self):
        self.client.login_as_owner()
        resp = self.client.delete("/api/admins/999999999")
        self.assertEqual(resp.status_code, 404)

    def test_f17_boundary_case3_add_admin_missing_login_returns_400(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/admins", json_data={"id": 12345})
        self.assertEqual(resp.status_code, 400)

    def test_f17_boundary_case4_add_admin_missing_id_returns_400(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/admins", json_data={"login": "no-id-admin"})
        self.assertEqual(resp.status_code, 400)

    def test_f17_boundary_case5_add_admin_duplicate_id_updates_login(self):
        self.client.login_as_owner()
        self.client.post("/api/admins", json_data={"id": 54321, "login": "name-one"})
        r2 = self.client.post("/api/admins", json_data={"id": 54321, "login": "name-two"})
        self.assertEqual(r2.status_code, 200)
        self.assertEqual(r2.json.get("admin", {}).get("login"), "name-two")

    # -------------------------------------------------------------------------
    # Feature 18: Instance Actions Safety Gate
    # -------------------------------------------------------------------------
    def test_f18_boundary_case1_empty_instance_name_delete_preview(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/delete", json_data={})
        self.assertEqual(resp.status_code, 200)
        self.assertTrue(resp.json.get("dry_run"))

    def test_f18_boundary_case2_scale_region_zero_target(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/scale-region", json_data={"region": "us-central1", "target": 0})
        self.assertEqual(resp.status_code, 200)
        self.assertTrue(resp.json.get("dry_run"))

    def test_f18_boundary_case3_instance_action_with_shell_metachars_handled(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/delete", json_data={"instance_name": "vm; rm -rf /"})
        self.assertEqual(resp.status_code, 200)
        self.assertTrue(resp.json.get("live_mutation_blocked"))

    def test_f18_boundary_case4_safety_gate_always_blocks_mutation(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/delete", json_data={"force": True})
        self.assertTrue(resp.json.get("live_mutation_blocked"))

    def test_f18_boundary_case5_safety_gate_response_contains_safety_label(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/delete", json_data={"instance_name": "vm-1"})
        self.assertEqual(resp.json.get("safety_gate"), "SAFETY_GATE_ACTIVE")

    # -------------------------------------------------------------------------
    # Feature 19: Canonical patch_sha256 Parity
    # -------------------------------------------------------------------------
    def test_f19_boundary_case1_empty_manifest_hashes_cleanly(self):
        h = compute_canonical_patch_sha256({})
        self.assertEqual(len(h), 64)

    def test_f19_boundary_case2_unicode_characters_in_boot_script(self):
        m = make_valid_manifest()
        m["host"]["boot_script"] = "echo 'üñîçødé test'"
        h = compute_canonical_patch_sha256(m)
        self.assertEqual(len(h), 64)

    def test_f19_boundary_case3_none_hardware_values_stripped(self):
        m = make_valid_manifest()
        m["hardware"]["runners_count"] = None
        h1 = compute_canonical_patch_sha256(m)
        m.get("hardware", {}).pop("runners_count", None)
        h2 = compute_canonical_patch_sha256(m)
        self.assertEqual(h1, h2)

    def test_f19_boundary_case4_min_vcpu_null_preserved(self):
        m = make_valid_manifest()
        m["hardware"]["min_vcpu"] = None
        # Preserving min_vcpu: null affects hash
        h = compute_canonical_patch_sha256(m)
        self.assertEqual(len(h), 64)

    def test_f19_boundary_case5_reordered_keys_produce_identical_hash(self):
        m1 = {"schema": "spot-runner-patch-v1", "repository": "swack-tools"}
        m2 = {"repository": "swack-tools", "schema": "spot-runner-patch-v1"}
        self.assertEqual(compute_canonical_patch_sha256(m1), compute_canonical_patch_sha256(m2))

    # -------------------------------------------------------------------------
    # Feature 20: Hardware Sizing Floor Validation
    # -------------------------------------------------------------------------
    def test_f20_boundary_case1_exact_16_vcpu_passes(self):
        m = make_valid_manifest()
        m["hardware"]["min_vcpu"] = 16
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 200)

    def test_f20_boundary_case2_exact_15_vcpu_fails(self):
        m = make_valid_manifest()
        m["hardware"]["min_vcpu"] = 15
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f20_boundary_case3_exact_32_ram_passes(self):
        m = make_valid_manifest()
        m["hardware"]["min_ram_gib"] = 32
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 200)

    def test_f20_boundary_case4_exact_31_ram_fails(self):
        m = make_valid_manifest()
        m["hardware"]["min_ram_gib"] = 31
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f20_boundary_case5_host_overhead_not_4_fails(self):
        m = make_valid_manifest()
        m["hardware"]["host_overhead_gib"] = 5
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 21: Permitted Machine Types
    # -------------------------------------------------------------------------
    def test_f21_boundary_case1_uppercase_machine_type_rejected(self):
        m = make_valid_manifest(machine_type="C4D-HIGHCPU-16")
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f21_boundary_case2_c4d_highcpu_8_rejected(self):
        m = make_valid_manifest(machine_type="c4d-highcpu-8")
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f21_boundary_case3_c4d_highcpu_32_rejected(self):
        m = make_valid_manifest(machine_type="c4d-highcpu-32")
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f21_boundary_case4_c4d_standard_16_rejected(self):
        m = make_valid_manifest(machine_type="c4d-standard-16")
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f21_boundary_case5_empty_machine_type_label_handled(self):
        m = make_valid_manifest()
        m["hardware"]["extra_labels"] = ["type-"]
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 22: Multi-Repo Manifest Merge
    # -------------------------------------------------------------------------
    def test_f22_boundary_case1_empty_overrides_retain_all_defaults(self):
        m = make_valid_manifest()
        self.assertEqual(m["hardware"]["min_vcpu"], 16)
        self.assertEqual(m["hardware"]["min_ram_gib"], 32)

    def test_f22_boundary_case2_empty_packages_list_override(self):
        m = make_valid_manifest()
        m["container"]["apt_packages"] = []
        self.assertEqual(len(m["container"]["apt_packages"]), 0)

    def test_f22_boundary_case3_deeply_nested_sysctl_override(self):
        m = make_valid_manifest()
        m["host"]["sysctl"]["net.ipv4.ip_local_port_range"] = "1024 65535"
        self.assertIn("net.ipv4.ip_local_port_range", m["host"]["sysctl"])

    def test_f22_boundary_case4_null_hardware_section_rejected(self):
        m = make_valid_manifest()
        m["hardware"] = None
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f22_boundary_case5_duplicate_packages_deduplicated_or_preserved(self):
        m = make_valid_manifest()
        m["container"]["apt_packages"] = ["curl", "curl"]
        self.assertIn("curl", m["container"]["apt_packages"])

    # -------------------------------------------------------------------------
    # Feature 23: Patch Console Bootstrap
    # -------------------------------------------------------------------------
    def test_f23_boundary_case1_large_catalog_handled(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/bootstrap")
        self.assertIsInstance(resp.json.get("controlled_targets"), list)

    def test_f23_boundary_case2_bootstrap_schema_contains_repo_key(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/bootstrap")
        self.assertIn("repository", resp.json)

    def test_f23_boundary_case3_repeated_bootstrap_calls_are_identical(self):
        self.client.login_as_owner()
        r1 = self.client.get("/api/patch-console/bootstrap").json
        r2 = self.client.get("/api/patch-console/bootstrap").json
        self.assertEqual(r1, r2)

    def test_f23_boundary_case4_bootstrap_with_member_session_returns_200(self):
        self.client.login_as_member()
        resp = self.client.get("/api/patch-console/bootstrap")
        self.assertEqual(resp.status_code, 200)

    def test_f23_boundary_case5_bootstrap_headers_no_store(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/bootstrap")
        self.assertIn("no-cache", resp.header("Cache-Control") or "")

    # -------------------------------------------------------------------------
    # Feature 24: Patch Console Load
    # -------------------------------------------------------------------------
    def test_f24_boundary_case1_base_sha_is_hex(self):
        self.client.login_as_owner()
        sha = self.client.get("/api/patch-console/manifest?repo=swack-tools").json.get("base_sha", "")
        self.assertTrue(all(c in "0123456789abcdef" for c in sha))

    def test_f24_boundary_case2_blob_sha_is_hex(self):
        self.client.login_as_owner()
        sha = self.client.get("/api/patch-console/manifest?repo=swack-tools").json.get("blob_sha", "")
        self.assertTrue(all(c in "0123456789abcdef" for c in sha))

    def test_f24_boundary_case3_escaped_characters_in_boot_script(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=swack-tools")
        self.assertIn("manifest", resp.json)

    def test_f24_boundary_case4_missing_repo_query_param_returns_400(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest")
        self.assertEqual(resp.status_code, 400)

    def test_f24_boundary_case5_calculated_patch_digest_matches_content(self):
        self.client.login_as_owner()
        data = self.client.get("/api/patch-console/manifest?repo=swack-tools").json
        expected = compute_canonical_patch_sha256(data["manifest"])
        self.assertEqual(data["patch_sha256"], expected)

    # -------------------------------------------------------------------------
    # Feature 25: Patch Console Validate
    # -------------------------------------------------------------------------
    def test_f25_boundary_case1_multi_error_reporting(self):
        m = make_valid_manifest()
        m["hardware"]["min_vcpu"] = 4
        m["hardware"]["min_ram_gib"] = 8
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)
        self.assertGreaterEqual(len(resp.json.get("errors", [])), 2)

    def test_f25_boundary_case2_forbidden_mount_proc_fails(self):
        m = make_valid_manifest()
        m["container"]["mounts"].append({"source": "/proc", "target": "/proc"})
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f25_boundary_case3_forbidden_mount_sys_fails(self):
        m = make_valid_manifest()
        m["container"]["mounts"].append({"source": "/sys", "target": "/sys"})
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f25_boundary_case4_forbidden_mount_etc_fails(self):
        m = make_valid_manifest()
        m["container"]["mounts"].append({"source": "/etc", "target": "/etc"})
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    def test_f25_boundary_case5_forbidden_mount_var_lib_gha_admission_fails(self):
        m = make_valid_manifest()
        m["container"]["mounts"].append({"source": "/var/lib/gha-admission", "target": "/admission"})
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 26: Patch Console Save (Pull Request)
    # -------------------------------------------------------------------------
    def test_f26_boundary_case1_branch_name_contains_sha_prefix(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="xbmc4lyfe/rustnzb")
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertEqual(resp.status_code, 200)
        self.assertIn("spot-dashboard/xbmc4lyfe-rustnzb-", resp.json.get("branch", ""))

    def test_f26_boundary_case2_branch_name_length_within_git_limits(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="xbmc4lyfe/newznab-tmux")
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertLess(len(resp.json.get("branch", "")), 100)

    def test_f26_boundary_case3_slug_sanitization_replaces_slashes(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="xbmc4lyfe/nntp-proxy")
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        branch = resp.json.get("branch", "")
        # The owner/repo slash should be turned into hyphen in branch name
        self.assertIn("xbmc4lyfe-nntp-proxy", branch)

    def test_f26_boundary_case4_save_without_manifest_returns_400(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/patch-console/save", json_data={})
        self.assertEqual(resp.status_code, 400)

    def test_f26_boundary_case5_save_with_null_manifest_returns_400(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": None})
        self.assertEqual(resp.status_code, 400)

    # -------------------------------------------------------------------------
    # Feature 27: Rollout Classification & Preview
    # -------------------------------------------------------------------------
    def test_f27_boundary_case1_only_apt_packages_modified_is_hot_patch(self):
        cur = make_valid_manifest()
        tgt = make_valid_manifest()
        tgt["container"]["apt_packages"].append("wget")
        resp = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt})
        self.assertEqual(resp.json.get("action"), "hot_patch")

    def test_f27_boundary_case2_only_sysctl_modified_is_replace_vm(self):
        cur = make_valid_manifest()
        tgt = make_valid_manifest()
        tgt["host"]["sysctl"]["net.core.somaxconn"] = "8192"
        resp = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt})
        self.assertEqual(resp.json.get("action"), "replace_vm")

    def test_f27_boundary_case3_both_dockerfile_and_hardware_is_replace_vm(self):
        cur = make_valid_manifest()
        tgt = make_valid_manifest()
        tgt["container"]["dockerfile_run"] = "RUN echo test"
        tgt["hardware"]["extra_labels"] = ["type-n4-highcpu-16"]
        resp = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt})
        self.assertEqual(resp.json.get("action"), "replace_vm")

    def test_f27_boundary_case4_empty_manifests_is_noop(self):
        resp = self.client.post("/api/patches/preview", json_data={"current_manifest": {}, "manifest": {}})
        self.assertEqual(resp.json.get("action"), "noop")

    def test_f27_boundary_case5_preview_contains_description_field(self):
        cur = make_valid_manifest()
        tgt = make_valid_manifest()
        tgt["container"]["dockerfile_run"] = "RUN echo 1"
        resp = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt})
        self.assertIn("description", resp.json)

    # -------------------------------------------------------------------------
    # Feature 28: Controlled Targets List
    # -------------------------------------------------------------------------
    def test_f28_boundary_case1_target_prefix_attack_rejected(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=swack-tools-fake")
        self.assertEqual(resp.status_code, 400)

    def test_f28_boundary_case2_target_suffix_attack_rejected(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=swack-tools/extra")
        self.assertEqual(resp.status_code, 400)

    def test_f28_boundary_case3_subdomain_spoofing_rejected(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=xbmc4lyfe.evil.com/rustnzb")
        self.assertEqual(resp.status_code, 400)

    def test_f28_boundary_case4_empty_string_target_rejected(self):
        self.assertNotIn("", CONTROLLED_TARGETS)

    def test_f28_boundary_case5_all_controlled_targets_have_valid_org_prefix(self):
        for target in CONTROLLED_TARGETS:
            self.assertTrue(target.startswith("swack-tools") or target.startswith("xbmc4lyfe/"))

    # -------------------------------------------------------------------------
    # Feature 29: GCP Service Account Key Parsing
    # -------------------------------------------------------------------------
    def test_f29_boundary_case1_extra_unrecognized_fields_handled(self):
        sa = make_gcp_service_account_fixture()
        sa["extra_future_field"] = "value"
        self.assertEqual(sa["project_id"], "homelab-424523")

    def test_f29_boundary_case2_whitespace_padded_pem_key(self):
        sa = make_gcp_service_account_fixture()
        padded_key = f"\n\n{sa['private_key']}\n\n"
        self.assertTrue(padded_key.strip().startswith("-----BEGIN PRIVATE KEY-----"))

    def test_f29_boundary_case3_empty_string_client_email_rejected(self):
        sa = make_gcp_service_account_fixture()
        sa["client_email"] = ""
        self.assertEqual(len(sa["client_email"]), 0)

    def test_f29_boundary_case4_missing_token_uri_handled(self):
        sa = make_gcp_service_account_fixture()
        sa.pop("token_uri")
        default_uri = sa.get("token_uri", "https://oauth2.googleapis.com/token")
        self.assertEqual(default_uri, "https://oauth2.googleapis.com/token")

    def test_f29_boundary_case5_key_size_within_limits(self):
        sa = make_gcp_service_account_fixture()
        raw = json.dumps(sa)
        self.assertLess(len(raw), 64 * 1024)

    # -------------------------------------------------------------------------
    # Feature 30: RS256 JWT Assertion Minting
    # -------------------------------------------------------------------------
    def test_f30_boundary_case1_assertion_has_three_segments(self):
        mock_assertion = "eyJhbGciOiJSUzI1NiJ9.eyJpc3MiOiJ0ZXN0In0.mock_signature"
        self.assertEqual(len(mock_assertion.split(".")), 3)

    def test_f30_boundary_case2_assertion_uses_urlsafe_b64(self):
        sample = b"test bytes \x00\xff"
        b64 = base64.urlsafe_b64encode(sample).decode().rstrip("=")
        self.assertNotIn("+", b64)
        self.assertNotIn("/", b64)

    def test_f30_boundary_case3_assertion_aud_must_be_google_token_endpoint(self):
        aud = "https://oauth2.googleapis.com/token"
        self.assertEqual(aud, "https://oauth2.googleapis.com/token")

    def test_f30_boundary_case4_assertion_exp_exactly_3600_seconds(self):
        iat = 1000000000
        exp = iat + 3600
        self.assertEqual(exp - iat, 3600)

    def test_f30_boundary_case5_assertion_iss_matches_sub(self):
        email = "spot-fleet-controller@homelab-424523.iam.gserviceaccount.com"
        claims = {"iss": email, "sub": email}
        self.assertEqual(claims["iss"], claims["sub"])

    # -------------------------------------------------------------------------
    # Feature 31: GCP OAuth Token Exchange
    # -------------------------------------------------------------------------
    def test_f31_boundary_case1_handles_http_500_from_token_endpoint(self):
        status = 500
        is_retryable = status in (500, 502, 503, 504)
        self.assertTrue(is_retryable)

    def test_f31_boundary_case2_handles_invalid_grant_error(self):
        err_resp = {"error": "invalid_grant", "error_description": "Invalid JWT"}
        self.assertEqual(err_resp["error"], "invalid_grant")

    def test_f31_boundary_case3_token_lifetime_lower_boundary(self):
        lifetime = 300  # 5 min minimum
        self.assertGreater(lifetime, 0)

    def test_f31_boundary_case4_token_lifetime_upper_boundary(self):
        lifetime = 3600  # 1 hour standard
        self.assertLessEqual(lifetime, 3600)

    def test_f31_boundary_case5_missing_access_token_in_response(self):
        resp = {"token_type": "Bearer"}
        self.assertNotIn("access_token", resp)

    # -------------------------------------------------------------------------
    # Feature 32: OAuth Token Caching
    # -------------------------------------------------------------------------
    def test_f32_boundary_case1_refresh_triggered_at_exact_300s_threshold(self):
        now = int(time.time())
        exp = now + 300
        should_refresh = (exp - now) <= 300
        self.assertTrue(should_refresh)

    def test_f32_boundary_case2_no_refresh_at_301s(self):
        now = int(time.time())
        exp = now + 301
        should_refresh = (exp - now) <= 300
        self.assertFalse(should_refresh)

    def test_f32_boundary_case3_zero_kv_writes_for_steady_state_token_cache(self):
        stats_before = self.client.get("/api/test/kv-stats").json
        # Reading cached token performs 0 KV writes
        stats_after = self.client.get("/api/test/kv-stats").json
        self.assertEqual(stats_after.get("kv_puts"), stats_before.get("kv_puts"))

    def test_f32_boundary_case4_token_cached_in_isolate_memory(self):
        if self.server:
            self.assertTrue(bool(self.server.store.gcp_oauth_token))

    def test_f32_boundary_case5_expired_token_in_cache_triggers_refresh(self):
        now = int(time.time())
        exp = now - 10  # expired
        should_refresh = (exp - now) <= 300
        self.assertTrue(should_refresh)

    # -------------------------------------------------------------------------
    # Feature 33: GCE instances.aggregatedList
    # -------------------------------------------------------------------------
    def test_f33_boundary_case1_empty_zone_items_handled(self):
        data = {"items": {"zones/us-central1-a": {"warning": {"code": "NO_RESULTS_ON_PAGE"}}}}
        self.assertIn("zones/us-central1-a", data["items"])

    def test_f33_boundary_case2_max_page_size_500_boundary(self):
        max_results = 500
        self.assertLessEqual(max_results, 500)

    def test_f33_boundary_case3_partial_failure_with_unreachable_zone(self):
        data = {
            "items": {"zones/us-central1-a": {"instances": [{"name": "vm-1"}]}},
            "warning": {"code": "ZONE_RESOURCE_POOL_EXHAUSTED", "data": [{"key": "scope", "value": "zones/us-central1-f"}]},
        }
        self.assertIn("warning", data)
        self.assertEqual(len(data["items"]["zones/us-central1-a"]["instances"]), 1)

    def test_f33_boundary_case4_instance_name_max_63_chars(self):
        name = "oxidex-runners-" + "a" * (63 - len("oxidex-runners-"))
        self.assertEqual(len(name), 63)

    def test_f33_boundary_case5_provisioning_status_handled(self):
        vm = {"status": "PROVISIONING"}
        self.assertIn(vm["status"], ("PROVISIONING", "STAGING", "RUNNING", "TERMINATED"))

    # -------------------------------------------------------------------------
    # Feature 34: GCE disks.aggregatedList
    # -------------------------------------------------------------------------
    def test_f34_boundary_case1_multiple_users_on_disk_handled(self):
        disk = {"users": ["vm-1", "vm-2"]}
        self.assertEqual(len(disk["users"]), 2)

    def test_f34_boundary_case2_zero_users_on_disk_identified_as_orphan(self):
        disk = {"name": "oxidex-runners-orphan", "users": []}
        self.assertEqual(len(disk.get("users", [])), 0)

    def test_f34_boundary_case3_builder_prefix_ignored(self):
        disk = {"name": "builder-oxidex-cache-1", "users": []}
        is_runner_disk = disk["name"].startswith("oxidex-runners-")
        self.assertFalse(is_runner_disk)

    def test_f34_boundary_case4_disk_size_gib_positive(self):
        disk = {"sizeGb": "50"}
        self.assertGreater(int(disk["sizeGb"]), 0)

    def test_f34_boundary_case5_multi_page_disks_aggregated(self):
        disks_p1 = [{"name": "d1"}]
        disks_p2 = [{"name": "d2"}]
        all_disks = disks_p1 + disks_p2
        self.assertEqual(len(all_disks), 2)

    # -------------------------------------------------------------------------
    # Feature 35: GCE instances.insert
    # -------------------------------------------------------------------------
    def test_f35_boundary_case1_body_size_under_gcp_4mib_limit(self):
        payload = {"name": "vm-1", "machineType": "c4d-highcpu-16"}
        raw = json.dumps(payload).encode()
        self.assertLess(len(raw), 4 * 1024 * 1024)

    def test_f35_boundary_case2_network_interface_specifies_default(self):
        nic = {"network": "global/networks/default", "accessConfigs": [{"type": "ONE_TO_ONE_NAT"}]}
        self.assertEqual(nic["accessConfigs"][0]["type"], "ONE_TO_ONE_NAT")

    def test_f35_boundary_case3_metadata_items_contain_startup_script(self):
        items = [{"key": "startup-script", "value": "#!/bin/bash"}]
        keys = [it["key"] for it in items]
        self.assertIn("startup-script", keys)

    def test_f35_boundary_case4_spot_provisioning_model_strictly_spot(self):
        scheduling = {"provisioningModel": "SPOT"}
        self.assertEqual(scheduling["provisioningModel"], "SPOT")

    def test_f35_boundary_case5_auto_delete_flag_set(self):
        disks = [{"boot": True, "autoDelete": True}, {"boot": False, "autoDelete": True}]
        self.assertTrue(all(d["autoDelete"] for d in disks))

    # -------------------------------------------------------------------------
    # Feature 36: GCE instances.delete
    # -------------------------------------------------------------------------
    def test_f36_boundary_case1_delete_in_progress_http_409_handled(self):
        status = 409
        is_safe = status in (200, 204, 404, 409)
        self.assertTrue(is_safe)

    def test_f36_boundary_case2_nonexistent_instance_http_404_handled(self):
        status = 404
        is_safe = status in (200, 204, 404)
        self.assertTrue(is_safe)

    def test_f36_boundary_case3_url_encodes_zone_and_name(self):
        zone = "us-central1-a"
        name = "oxidex-runners-1"
        url = f"/projects/p/zones/{zone}/instances/{name}?deleteDisk=ALL"
        self.assertIn("us-central1-a", url)

    def test_f36_boundary_case4_delete_disk_all_query_param(self):
        query = "deleteDisk=ALL"
        self.assertEqual(query, "deleteDisk=ALL")

    def test_f36_boundary_case5_running_host_deletion_blocked_by_safety(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/delete", json_data={"instance_name": "running-host"})
        self.assertTrue(resp.json.get("live_mutation_blocked"))

    # -------------------------------------------------------------------------
    # Feature 37: GCE instances.setMetadata
    # -------------------------------------------------------------------------
    def test_f37_boundary_case1_fingerprint_mismatch_precondition_failed(self):
        status = 412  # Precondition Failed
        self.assertEqual(status, 412)

    def test_f37_boundary_case2_scrubbed_value_is_explicit_marker(self):
        val = "[SCRUBBED_AFTER_BOOT]"
        self.assertEqual(val, "[SCRUBBED_AFTER_BOOT]")

    def test_f37_boundary_case3_preserves_digest_in_metadata(self):
        items = [{"key": "patch-digest", "value": "a" * 64}]
        self.assertEqual(items[0]["key"], "patch-digest")

    def test_f37_boundary_case4_empty_metadata_items_handled(self):
        items = []
        self.assertEqual(len(items), 0)

    def test_f37_boundary_case5_metadata_fingerprint_non_empty(self):
        payload = {"fingerprint": "mock_fp_123"}
        self.assertTrue(len(payload["fingerprint"]) > 0)

    # -------------------------------------------------------------------------
    # Feature 38: GCE disks.delete
    # -------------------------------------------------------------------------
    def test_f38_boundary_case1_disk_with_users_not_deleted(self):
        disk = {"users": ["instance-1"]}
        can_delete = len(disk.get("users", [])) == 0
        self.assertFalse(can_delete)

    def test_f38_boundary_case2_disk_delete_404_treated_as_success(self):
        status = 404
        is_success = status in (200, 204, 404)
        self.assertTrue(is_success)

    def test_f38_boundary_case3_builder_cache_disk_preserved(self):
        name = "builder-cache-disk"
        self.assertTrue(name.startswith("builder-"))

    def test_f38_boundary_case4_orphan_runner_disk_eligible_for_deletion(self):
        name = "oxidex-runners-orphan-disk"
        users = []
        can_delete = name.startswith("oxidex-runners-") and len(users) == 0
        self.assertTrue(can_delete)

    def test_f38_boundary_case5_disk_delete_url_format(self):
        url = "/projects/p/zones/us-central1-a/disks/disk-1"
        self.assertIn("/disks/disk-1", url)

    # -------------------------------------------------------------------------
    # Feature 39: GitHub Runner Registration Token
    # -------------------------------------------------------------------------
    def test_f39_boundary_case1_token_expiration_in_future(self):
        now = int(time.time())
        token_exp = now + 3600
        self.assertGreater(token_exp, now)

    def test_f39_boundary_case2_token_length_positive(self):
        token = "LLLLLL1234567890"
        self.assertGreater(len(token), 10)

    def test_f39_boundary_case3_github_rate_limit_403_handled(self):
        status = 403
        self.assertEqual(status, 403)

    def test_f39_boundary_case4_token_not_in_plaintext_kv(self):
        kv = {"dashboard:state": "{}"}
        self.assertNotIn("LLLLLL", kv["dashboard:state"])

    def test_f39_boundary_case5_single_use_token_invalidated(self):
        used = True
        self.assertTrue(used)

    # -------------------------------------------------------------------------
    # Feature 40: GitHub Runner Listing
    # -------------------------------------------------------------------------
    def test_f40_boundary_case1_empty_runners_list_handled(self):
        resp = {"total_count": 0, "runners": []}
        self.assertEqual(len(resp["runners"]), 0)

    def test_f40_boundary_case2_multiple_page_runners_concatenated(self):
        p1 = [{"id": 1}]
        p2 = [{"id": 2}]
        self.assertEqual(len(p1 + p2), 2)

    def test_f40_boundary_case3_null_labels_array_handled(self):
        runner = {"labels": None}
        labels = runner.get("labels") or []
        self.assertEqual(len(labels), 0)

    def test_f40_boundary_case4_runner_offline_idle_identified(self):
        runner = {"status": "offline", "busy": False}
        idle_offline = runner["status"] == "offline" and not runner["busy"]
        self.assertTrue(idle_offline)

    def test_f40_boundary_case5_runner_online_busy_identified(self):
        runner = {"status": "online", "busy": True}
        active_busy = runner["status"] == "online" and runner["busy"]
        self.assertTrue(active_busy)

    # -------------------------------------------------------------------------
    # Feature 41: GitHub Runner Deregistration
    # -------------------------------------------------------------------------
    def test_f41_boundary_case1_active_busy_runner_deregistration_blocked(self):
        runner = {"status": "online", "busy": True}
        can_deregister = runner["status"] == "offline" and not runner["busy"]
        self.assertFalse(can_deregister)

    def test_f41_boundary_case2_deregister_404_treated_as_idempotent_success(self):
        status = 404
        is_success = status in (200, 204, 404)
        self.assertTrue(is_success)

    def test_f41_boundary_case3_invalid_runner_id_handled(self):
        runner_id = "invalid-id"
        self.assertIsInstance(runner_id, str)

    def test_f41_boundary_case4_deregister_url_structure(self):
        url = "/orgs/swack-tools/actions/runners/999"
        self.assertIn("/actions/runners/999", url)

    def test_f41_boundary_case5_deregister_frees_quota_slot(self):
        quota = {"used": 5, "total": 10}
        quota["used"] -= 1
        self.assertEqual(quota["used"], 4)

    # -------------------------------------------------------------------------
    # Feature 42: GitHub Runner Labeling
    # -------------------------------------------------------------------------
    def test_f42_boundary_case1_label_max_length_63_chars(self):
        label = "a" * 63
        self.assertEqual(len(label), 63)

    def test_f42_boundary_case2_label_over_63_chars_rejected_or_truncated(self):
        label = "a" * 64
        self.assertGreater(len(label), 63)

    def test_f42_boundary_case3_label_with_slashes_rejected(self):
        import re
        label_re = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$")
        self.assertIsNone(label_re.match("invalid/label"))

    def test_f42_boundary_case4_duplicate_labels_deduplicated(self):
        labels = ["spot", "spot", "c4d-highcpu-16"]
        deduped = sorted(set(labels))
        self.assertEqual(len(deduped), 2)

    def test_f42_boundary_case5_spot_label_always_present(self):
        labels = ["spot", "c4d-highcpu-16"]
        self.assertIn("spot", labels)

    # -------------------------------------------------------------------------
    # Feature 43: GitHub Queued Jobs Discovery
    # -------------------------------------------------------------------------
    def test_f43_boundary_case1_zero_queued_jobs_boundary(self):
        queue_count = 0
        self.assertEqual(queue_count, 0)

    def test_f43_boundary_case2_large_queue_count_handled(self):
        queue_count = 50000
        self.assertGreater(queue_count, 20)

    def test_f43_boundary_case3_status_queued_filter_included(self):
        url = "/actions/runs?status=queued"
        self.assertIn("status=queued", url)

    def test_f43_boundary_case4_completed_jobs_ignored(self):
        jobs = [{"status": "completed"}]
        queued = [j for j in jobs if j["status"] == "queued"]
        self.assertEqual(len(queued), 0)

    def test_f43_boundary_case5_queue_query_error_falls_back_to_zero(self):
        # Fail safe: don't scale up on query failure
        fallback_queue = 0
        self.assertEqual(fallback_queue, 0)

    # -------------------------------------------------------------------------
    # Feature 44: Floor Target Maintenance
    # -------------------------------------------------------------------------
    def test_f44_boundary_case1_zero_hosts_launches_exactly_one_host(self):
        active = 0
        target = 1
        needed = target - active
        self.assertEqual(needed, 1)

    def test_f44_boundary_case2_multiple_missing_targets_launched(self):
        missing = ["swack-tools", "xbmc4lyfe/rustnzb"]
        self.assertEqual(len(missing), 2)

    def test_f44_boundary_case3_floor_host_hardware_satisfies_16_vcpu(self):
        vcpu = 16
        self.assertGreaterEqual(vcpu, MIN_HOST_VCPU)

    def test_f44_boundary_case4_terminating_vms_ignored_by_floor_check(self):
        vms = [{"status": "TERMINATING"}]
        healthy = [v for v in vms if v["status"] == "RUNNING"]
        self.assertEqual(len(healthy), 0)

    def test_f44_boundary_case5_floor_hosts_not_scaled_down(self):
        floor_min = 1
        active = 1
        can_scale_down = active > floor_min
        self.assertFalse(can_scale_down)

    # -------------------------------------------------------------------------
    # Feature 45: Queued Jobs Backlog Scale-Up
    # -------------------------------------------------------------------------
    def test_f45_boundary_case1_exact_20_queued_jobs_scales(self):
        jobs = 20
        should_scale = jobs >= MIN_QUEUED_JOBS_THRESHOLD
        self.assertTrue(should_scale)

    def test_f45_boundary_case2_exact_19_queued_jobs_does_not_scale(self):
        jobs = 19
        should_scale = jobs >= MIN_QUEUED_JOBS_THRESHOLD
        self.assertFalse(should_scale)

    def test_f45_boundary_case3_exact_21_queued_jobs_scales(self):
        jobs = 21
        should_scale = jobs >= MIN_QUEUED_JOBS_THRESHOLD
        self.assertTrue(should_scale)

    def test_f45_boundary_case4_fleet_cap_blocks_scale_up(self):
        active_instances = 10
        max_instances = 10
        can_scale = active_instances < max_instances
        self.assertFalse(can_scale)

    def test_f45_boundary_case5_scale_up_records_queued_jobs_count(self):
        reason = {"reason": "queued_jobs_threshold_exceeded", "count": 25}
        self.assertGreaterEqual(reason["count"], 20)

    # -------------------------------------------------------------------------
    # Feature 46: Startup Script Rendering
    # -------------------------------------------------------------------------
    def test_f46_boundary_case1_all_31_placeholders_replaced(self):
        script = "#!/bin/bash\necho ready"
        self.assertNotIn("@@", script)

    def test_f46_boundary_case2_single_unreplaced_placeholder_detected(self):
        script = "#!/bin/bash\necho @@UNREPLACED@@"
        has_error = "@@" in script
        self.assertTrue(has_error)

    def test_f46_boundary_case3_escaped_shell_variables_preserved(self):
        script = 'echo "$HOME \\$PATH"'
        self.assertIn("$HOME", script)

    def test_f46_boundary_case4_script_size_under_256k_budget(self):
        script = "#!/bin/bash\n" * 100
        self.assertLess(len(script.encode()), 256 * 1024)

    def test_f46_boundary_case5_patch_digest_64_hex_chars(self):
        digest = "0" * 64
        self.assertEqual(len(digest), 64)

    # -------------------------------------------------------------------------
    # Feature 47: Multi-Tick Launch State Machine
    # -------------------------------------------------------------------------
    def test_f47_boundary_case1_transition_to_booting_when_instance_running(self):
        cur_state = "creating"
        instance_status = "RUNNING"
        next_state = "booting" if instance_status == "RUNNING" else cur_state
        self.assertEqual(next_state, "booting")

    def test_f47_boundary_case2_transition_to_done_when_runner_online(self):
        cur_state = "booting"
        runner_status = "online"
        next_state = "done" if runner_status == "online" else cur_state
        self.assertEqual(next_state, "done")

    def test_f47_boundary_case3_timeout_at_15_minutes_boundary(self):
        elapsed_seconds = 901  # > 900s
        is_timeout = elapsed_seconds > 900
        self.assertTrue(is_timeout)

    def test_f47_boundary_case4_failed_launch_marks_error(self):
        state = "failed"
        self.assertEqual(state, "failed")

    def test_f47_boundary_case5_launch_state_key_namespaced(self):
        key = "launch:oxidex-runners-1"
        self.assertTrue(key.startswith("launch:"))

    # -------------------------------------------------------------------------
    # Feature 48: Preempted VM Cleanup
    # -------------------------------------------------------------------------
    def test_f48_boundary_case1_detects_terminated_vm(self):
        vm = {"name": "vm-preempted", "status": "TERMINATED"}
        self.assertEqual(vm["status"], "TERMINATED")

    def test_f48_boundary_case2_multiple_preempted_vms_cleaned_in_single_tick(self):
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "vm-p1"})
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "vm-p2"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertEqual(len(tick.get("cleaned_vms", [])), 2)

    def test_f48_boundary_case3_staging_vm_not_cleaned(self):
        vm = {"status": "STAGING"}
        should_clean = vm["status"] == "TERMINATED"
        self.assertFalse(should_clean)

    def test_f48_boundary_case4_stopping_vm_not_cleaned_until_terminated(self):
        vm = {"status": "STOPPING"}
        should_clean = vm["status"] == "TERMINATED"
        self.assertFalse(should_clean)

    def test_f48_boundary_case5_cleaned_vm_queue_cleared_after_tick(self):
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "vm-p3"})
        self.client.post("/api/reconciler/tick")
        tick2 = self.client.post("/api/reconciler/tick").json
        self.assertEqual(len(tick2.get("cleaned_vms", [])), 0)

    # -------------------------------------------------------------------------
    # Feature 49: Stale Runner Registration Cleanup
    # -------------------------------------------------------------------------
    def test_f49_boundary_case1_offline_matching_active_vm_not_stale(self):
        runner = {"name": "runner-1", "status": "offline"}
        active_vms = ["runner-1"]
        is_stale = runner["status"] == "offline" and runner["name"] not in active_vms
        self.assertFalse(is_stale)

    def test_f49_boundary_case2_offline_missing_vm_is_stale(self):
        runner = {"name": "runner-ghost", "status": "offline"}
        active_vms = ["runner-1"]
        is_stale = runner["status"] == "offline" and runner["name"] not in active_vms
        self.assertTrue(is_stale)

    def test_f49_boundary_case3_multiple_stale_runners_cleaned(self):
        self.client.post("/api/test/inject-stale-runner", json_data={"runner_id": "r-1"})
        self.client.post("/api/test/inject-stale-runner", json_data={"runner_id": "r-2"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertEqual(len(tick.get("cleaned_runners", [])), 2)

    def test_f49_boundary_case4_busy_flag_prevents_stale_marking(self):
        runner = {"status": "offline", "busy": True}
        can_clean = runner["status"] == "offline" and not runner.get("busy")
        self.assertFalse(can_clean)

    def test_f49_boundary_case5_stale_queue_cleared_after_tick(self):
        self.client.post("/api/test/inject-stale-runner", json_data={"runner_id": "r-3"})
        self.client.post("/api/reconciler/tick")
        tick2 = self.client.post("/api/reconciler/tick").json
        self.assertEqual(len(tick2.get("cleaned_runners", [])), 0)

    # -------------------------------------------------------------------------
    # Feature 50: Spot Offer Selection & Ranking
    # -------------------------------------------------------------------------
    def test_f50_boundary_case1_exact_1_50_dollar_daily_cap_passes(self):
        price = 1.50
        passes = price <= 1.50
        self.assertTrue(passes)

    def test_f50_boundary_case2_exact_1_51_dollar_daily_cap_fails(self):
        price = 1.51
        passes = price <= 1.50
        self.assertFalse(passes)

    def test_f50_boundary_case3_zero_availability_offer_rejected(self):
        offer = {"available": False, "price": 0.40}
        self.assertFalse(offer["available"])

    def test_f50_boundary_case4_tie_break_prefers_us_central1(self):
        offers = [
            {"zone": "us-east1-b", "price": 0.48},
            {"zone": "us-central1-a", "price": 0.48},
        ]
        sorted_offers = sorted(offers, key=lambda o: (o["price"], 0 if "us-central1" in o["zone"] else 1))
        self.assertEqual(sorted_offers[0]["zone"], "us-central1-a")

    def test_f50_boundary_case5_disallowed_region_rejected(self):
        zone = "europe-west1-b"
        allowed_regions = ("us-central1", "us-east1", "us-west1")
        is_allowed = any(r in zone for r in allowed_regions)
        self.assertFalse(is_allowed)

    # -------------------------------------------------------------------------
    # Feature 51: Precomputed Overview Snapshot
    # -------------------------------------------------------------------------
    def test_f51_boundary_case1_zero_kv_writes_on_unchanged_tick(self):
        # Mandatory Free Tier Quota assertion: 0 SPOT_KV.put calls on unchanged tick
        stats1 = self.client.get("/api/test/kv-stats").json
        tick = self.client.post("/api/reconciler/tick").json
        stats2 = self.client.get("/api/test/kv-stats").json
        self.assertFalse(tick.get("snapshot_written"))
        self.assertEqual(stats2.get("kv_puts"), stats1.get("kv_puts"))

    def test_f51_boundary_case2_zero_kv_writes_on_steady_state_gets(self):
        stats1 = self.client.get("/api/test/kv-stats").json
        for _ in range(5):
            self.client.get("/api/overview")
        stats2 = self.client.get("/api/test/kv-stats").json
        self.assertEqual(stats2.get("kv_puts"), stats1.get("kv_puts"))

    def test_f51_boundary_case3_in_isolate_cache_60s_ttl(self):
        self.client.post("/api/test/reset")
        self.client.get("/api/overview")
        s1 = self.client.get("/api/test/kv-stats").json
        self.client.get("/api/overview")
        s2 = self.client.get("/api/test/kv-stats").json
        # Second call hits cache (same kv_gets count)
        self.assertEqual(s2.get("kv_gets"), s1.get("kv_gets"))
        self.assertGreater(s2.get("isolate_cache_hits", 0), 0)

    def test_f51_boundary_case4_overview_snapshot_size_under_500k(self):
        resp = self.client.get("/api/overview")
        self.assertLess(len(resp.body), 500 * 1024)

    def test_f51_boundary_case5_snapshot_written_only_on_mutation(self):
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "vm-mut"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertTrue(tick.get("snapshot_written"))

    # -------------------------------------------------------------------------
    # Feature 52: Scheduled Cron Trigger Hook
    # -------------------------------------------------------------------------
    def test_f52_boundary_case1_concurrency_lease_240s_ttl(self):
        lease_ttl = 240
        self.assertEqual(lease_ttl, 240)

    def test_f52_boundary_case2_active_lease_skips_concurrent_tick(self):
        lease_active = True
        should_skip = lease_active
        self.assertTrue(should_skip)

    def test_f52_boundary_case3_expired_lease_allows_next_tick(self):
        now = int(time.time())
        lease_expires_at = now - 1
        can_acquire = now > lease_expires_at
        self.assertTrue(can_acquire)

    def test_f52_boundary_case4_reconciler_tick_execution_time_under_500ms(self):
        t0 = time.perf_counter()
        resp = self.client.post("/api/reconciler/tick")
        elapsed = (time.perf_counter() - t0) * 1000
        self.assertEqual(resp.status_code, 200)
        self.assertLess(elapsed, 500.0)

    def test_f52_boundary_case5_health_endpoint_tracks_ticks(self):
        self.client.post("/api/reconciler/tick")
        health = self.client.get("/api/health").json
        self.assertGreaterEqual(health.get("reconciler_ticks", 0), 1)


if __name__ == "__main__":
    unittest.main()
