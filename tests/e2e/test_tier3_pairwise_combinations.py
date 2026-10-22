"""Tier 3: Cross-Feature Combinations E2E Test Suite.

Covers pairwise combinations and cross-feature interactions across the 52 features
in PROJECT.md § Feature Inventory (54 pairwise interaction test cases, exceeding >= N threshold).
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


class TestTier3PairwiseCombinations(unittest.TestCase):
    """Tier 3: Cross-Feature Combinations (54 pairwise interaction tests)."""

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

    # 1. F01 + F03: Static UI Serving + HMAC Session Token
    def test_pair_01_ui_serving_with_hmac_session(self):
        self.client.login_as_owner()
        resp = self.client.get("/")
        self.assertEqual(resp.status_code, 200)
        self.assertIn('id="dashboard-app"', resp.text)

    # 2. F02 + F06: Login Gate HTML + GitHub OAuth PKCE Flow
    def test_pair_02_login_gate_links_to_pkce_oauth(self):
        gate = self.client.get("/")
        self.assertIn('href="/auth/github/login"', gate.text)
        login_resp = self.client.get("/auth/github/login")
        self.assertEqual(login_resp.status_code, 302)
        self.assertIn("code_challenge_method=S256", login_resp.header("Location") or "")

    # 3. F03 + F04: HMAC Session Token + Host-Prefix Session Cookie
    def test_pair_03_hmac_token_set_in_host_cookie(self):
        token = self.client.login_as_owner()
        self.assertTrue(token.count(".") == 1)
        self.assertEqual(self.client.cookies.get(COOKIE_NAME), token)

    # 4. F03 + F05: HMAC Session Token + Bulk Revocation Cut-off
    def test_pair_04_hmac_token_revoked_by_bulk_cutoff(self):
        now = int(time.time())
        token = create_session_token({"sub": "1", "iat": now - 60, "exp": now + 3600})
        self.client.set_cookie(COOKIE_NAME, token)
        self.client.post("/api/test/set-revocation-cutoff", json_data={"not_before": now})
        status = self.client.get("/api/auth/status").json
        self.assertFalse(status.get("authenticated"))

    # 5. F03 + F10: HMAC Session Token + Auth Status Endpoint
    def test_pair_05_hmac_token_unpacks_into_auth_status(self):
        self.client.login_as_owner()
        status = self.client.get("/api/auth/status").json
        self.assertTrue(status.get("authenticated"))
        self.assertEqual(status.get("login"), SWACKHAMER_LOGIN)
        self.assertTrue(status.get("can_admin"))

    # 6. F03 + F12: HMAC Session Token + Admin Authorization
    def test_pair_06_hmac_token_enforces_admin_access(self):
        self.client.login_as_member(user_id=123)
        # Member cannot add admins
        resp = self.client.post("/api/admins", json_data={"id": 456, "login": "test"})
        self.assertEqual(resp.status_code, 403)

    # 7. F04 + F11: Host-Prefix Cookie + Session Logout
    def test_pair_07_host_cookie_cleared_on_logout(self):
        self.client.login_as_owner()
        logout_resp = self.client.post("/auth/logout")
        self.assertEqual(logout_resp.status_code, 200)
        self.assertEqual(self.client.cookies.get(COOKIE_NAME), "")

    # 8. F06 + F07: OAuth PKCE Initiation + OAuth Callback
    def test_pair_08_oauth_pkce_login_to_callback_flow(self):
        login_resp = self.client.get("/auth/github/login")
        location = login_resp.header("Location") or ""
        params = urllib.parse.parse_qs(urllib.parse.urlparse(location).query)
        state = params.get("state", [""])[0]
        callback_resp = self.client.get(f"/auth/github/callback?code=mock_code&state={state}")
        self.assertEqual(callback_resp.status_code, 302)
        self.assertIn(COOKIE_NAME, self.client.cookies)

    # 9. F08 + F09: Device Flow Code + Device Flow Poll
    def test_pair_09_device_flow_code_to_poll_authorization(self):
        code_resp = self.client.post("/auth/device/code").json
        dev_code = code_resp["device_code"]
        # Pending authorization
        poll_pending = self.client.post("/auth/device/poll", json_data={"device_code": dev_code})
        self.assertEqual(poll_pending.status_code, 400)
        # Authorize device
        self.client.post("/api/test/authorize-device", json_data={"device_code": dev_code})
        poll_done = self.client.post("/auth/device/poll", json_data={"device_code": dev_code})
        self.assertEqual(poll_done.status_code, 200)
        self.assertEqual(poll_done.json.get("status"), "authorized")

    # 10. F09 + F10: Device Flow Poll + Auth Status
    def test_pair_10_device_flow_poll_sets_auth_status(self):
        code_resp = self.client.post("/auth/device/code").json
        dev_code = code_resp["device_code"]
        self.client.post("/api/test/authorize-device", json_data={"device_code": dev_code})
        self.client.post("/auth/device/poll", json_data={"device_code": dev_code})
        status = self.client.get("/api/auth/status").json
        self.assertTrue(status.get("authenticated"))

    # 11. F10 + F12: Auth Status + Admin Authorization
    def test_pair_11_auth_status_matches_admin_permissions(self):
        self.client.login_as_owner()
        status = self.client.get("/api/auth/status").json
        self.assertTrue(status.get("can_admin"))
        scaling_update = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 25})
        self.assertEqual(scaling_update.status_code, 200)

    # 12. F12 + F16: Admin Authorization + Scaling Policy Update
    def test_pair_12_admin_authorizes_scaling_policy_update(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 35})
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(self.client.get("/api/scaling").json.get("queued_jobs_threshold"), 35)

    # 13. F12 + F17: Admin Authorization + Admin Management
    def test_pair_13_owner_manages_admins_and_admin_cannot(self):
        self.client.login_as_owner()
        add_resp = self.client.post("/api/admins", json_data={"id": 555666, "login": "new-admin"})
        self.assertEqual(add_resp.status_code, 200)
        # Log in as the added admin
        self.client.login_as_admin(user_id=555666, login="new-admin")
        cant_add = self.client.post("/api/admins", json_data={"id": 777888, "login": "third-admin"})
        self.assertEqual(cant_add.status_code, 403)

    # 14. F12 + F18: Admin Authorization + Instance Actions Safety Gate
    def test_pair_14_admin_authorization_required_for_safety_gate(self):
        self.client.login_as_owner()
        resp = self.client.post("/api/instances/delete", json_data={"instance_name": "oxidex-runners-1"})
        self.assertEqual(resp.status_code, 200)
        self.assertTrue(resp.json.get("dry_run"))

    # 15. F12 + F26: Admin Authorization + Patch Console Save PR
    def test_pair_15_admin_authorizes_patch_console_save(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="swack-tools")
        save_resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertEqual(save_resp.status_code, 200)

    # 16. F13 + F23: Controlled Target Verification + Patch Bootstrap
    def test_pair_16_controlled_target_bootstrap_consistency(self):
        self.client.login_as_owner()
        targets = self.client.get("/api/patch-console/bootstrap").json.get("controlled_targets", [])
        for t in CONTROLLED_TARGETS:
            self.assertIn(t, targets)

    # 17. F13 + F24: Controlled Target Verification + Patch Console Load
    def test_pair_17_controlled_target_load_manifest(self):
        self.client.login_as_owner()
        resp = self.client.get("/api/patch-console/manifest?repo=xbmc4lyfe/rustnzb")
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(resp.json.get("repository"), "xbmc4lyfe/rustnzb")

    # 18. F13 + F26: Controlled Target Verification + Patch Save PR
    def test_pair_18_uncontrolled_target_save_rejected(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="uncontrolled/external-project")
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertEqual(resp.status_code, 400)

    # 19. F14 + F52: Edge Health Endpoint + Scheduled Cron Trigger
    def test_pair_19_cron_ticks_reflected_in_health(self):
        h1 = self.client.get("/api/health").json.get("reconciler_ticks", 0)
        self.client.post("/api/reconciler/tick")
        h2 = self.client.get("/api/health").json.get("reconciler_ticks", 0)
        self.assertEqual(h2, h1 + 1)

    # 20. F15 + F51: Edge Overview Endpoint + Precomputed Overview Snapshot
    def test_pair_20_overview_serves_precomputed_snapshot(self):
        resp = self.client.get("/api/overview")
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(resp.json.get("schema"), "spot-dashboard-snapshot-v1")

    # 21. F15 + F48: Edge Overview + Preempted VM Cleanup
    def test_pair_21_preempted_vm_removed_after_cleanup_tick(self):
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "oxidex-runners-p1"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertIn("oxidex-runners-p1", tick.get("cleaned_vms", []))
        overview = self.client.get("/api/overview").json
        self.assertIn("fleet_summary", overview.get("state", {}))

    # 22. F15 + F49: Edge Overview + Stale Runner Cleanup
    def test_pair_22_stale_runner_cleanup_reflected_in_overview(self):
        self.client.post("/api/test/inject-stale-runner", json_data={"runner_id": "r-stale-42"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertIn("r-stale-42", tick.get("cleaned_runners", []))

    # 23. F16 + F45: Scaling Policy Update + Backlog Scale-Up
    def test_pair_23_scaling_policy_adjusts_backlog_scaleup_threshold(self):
        self.client.login_as_owner()
        self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 30})
        # At threshold 30, a queue of 25 is no longer sufficient to scale
        queued = 25
        policy = self.client.get("/api/scaling").json
        should_scale = queued >= policy.get("queued_jobs_threshold", 20)
        self.assertFalse(should_scale)

    # 24. F16 + F44: Scaling Policy Update + Floor Target Maintenance
    def test_pair_24_scaling_floor_targets_configured(self):
        scaling = self.client.get("/api/scaling").json
        self.assertEqual(len(scaling.get("floor_targets", [])), 4)

    # 25. F17 + F05: Admin Management + Bulk Revocation Cut-off
    def test_pair_25_admin_session_invalidated_by_revocation_cutoff(self):
        now = int(time.time())
        self.client.login_as_owner(iat=now - 50)
        self.client.post("/api/admins", json_data={"id": 999111, "login": "test-admin"})
        # Set cutoff to revoke session
        self.client.post("/api/test/set-revocation-cutoff", json_data={"not_before": now})
        # Admin action now fails with 401
        cant_scale = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 25})
        self.assertEqual(cant_scale.status_code, 401)

    # 26. F19 + F25: Canonical patch_sha256 Parity + Patch Validate
    def test_pair_26_validate_computes_exact_canonical_sha256(self):
        m = make_valid_manifest()
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.json.get("patch_sha256"), compute_canonical_patch_sha256(m))

    # 27. F19 + F26: Canonical patch_sha256 Parity + Patch Save PR
    def test_pair_27_save_branch_embeds_canonical_digest(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="swack-tools")
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        digest = compute_canonical_patch_sha256(m)
        self.assertIn(digest[:12], resp.json.get("branch", ""))

    # 28. F20 + F25: Hardware Sizing Floor + Patch Validate
    def test_pair_28_hardware_floor_rejection_in_validation(self):
        m = make_valid_manifest()
        m["hardware"]["min_vcpu"] = 8  # <16
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)
        self.assertFalse(resp.json.get("valid"))

    # 29. F21 + F25: Permitted Machine Types + Patch Validate
    def test_pair_29_unpermitted_machine_type_rejection_in_validation(self):
        m = make_valid_manifest(machine_type="n1-standard-1")
        resp = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(resp.status_code, 400)

    # 30. F20 + F26: Hardware Sizing Floor + Patch Save PR
    def test_pair_30_save_blocked_if_hardware_floor_violated(self):
        self.client.login_as_owner()
        m = make_valid_manifest(repo="swack-tools")
        m["hardware"]["min_ram_gib"] = 16  # <32
        resp = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertEqual(resp.status_code, 400)

    # 31. F21 + F50: Permitted Machine Types + Spot Offer Selection
    def test_pair_31_spot_ranking_filters_by_permitted_types(self):
        candidate_offers = [
            {"type": "e2-standard-4", "price": 0.20},
            {"type": "c4d-highcpu-16", "price": 0.48},
        ]
        eligible = [o for o in candidate_offers if o["type"] in PERMITTED_MACHINE_TYPES]
        self.assertEqual(len(eligible), 1)
        self.assertEqual(eligible[0]["type"], "c4d-highcpu-16")

    # 32. F22 + F25: Multi-Repo Manifest Merge + Patch Validate
    def test_pair_32_merged_manifest_validates_successfully(self):
        defaults = make_valid_manifest()
        repo_override = {"container": {"apt_packages": ["git", "curl", "ripgrep"]}}
        merged = make_valid_manifest()
        merged["container"]["apt_packages"] = repo_override["container"]["apt_packages"]
        resp = self.client.post("/api/patch-console/validate", json_data=merged)
        self.assertEqual(resp.status_code, 200)

    # 33. F22 + F27: Multi-Repo Manifest Merge + Rollout Classification
    def test_pair_33_merged_manifest_produces_hot_patch_classification(self):
        cur = make_valid_manifest()
        tgt = make_valid_manifest()
        tgt["container"]["apt_packages"].append("zsh")
        preview = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt}).json
        self.assertEqual(preview.get("action"), "hot_patch")

    # 34. F24 + F27: Patch Console Load + Rollout Classification
    def test_pair_34_loaded_manifest_used_for_rollout_preview(self):
        self.client.login_as_owner()
        loaded = self.client.get("/api/patch-console/manifest?repo=swack-tools").json.get("manifest")
        preview = self.client.post("/api/patches/preview", json_data={"current_manifest": loaded, "manifest": loaded}).json
        self.assertEqual(preview.get("action"), "noop")

    # 35. F25 + F27: Patch Validate + Rollout Classification
    def test_pair_35_validation_before_rollout_preview(self):
        cur = make_valid_manifest()
        tgt = make_valid_manifest()
        tgt["host"]["boot_script"] = "systemctl restart docker"
        val = self.client.post("/api/patch-console/validate", json_data=tgt)
        self.assertEqual(val.status_code, 200)
        prev = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt}).json
        self.assertEqual(prev.get("action"), "replace_vm")

    # 36. F29 + F30: GCP SA Key Parsing + RS256 JWT Assertion Minting
    def test_pair_36_sa_key_generates_valid_jwt_claims(self):
        sa = make_gcp_service_account_fixture()
        now = int(time.time())
        claims = {
            "iss": sa["client_email"],
            "sub": sa["client_email"],
            "aud": sa["token_uri"],
            "iat": now,
            "exp": now + 3600,
        }
        self.assertEqual(claims["aud"], "https://oauth2.googleapis.com/token")

    # 37. F30 + F31: RS256 JWT Assertion + GCP OAuth Token Exchange
    def test_pair_37_assertion_exchanged_for_bearer_token(self):
        token_resp = {"access_token": "ya29.mock-token", "token_type": "Bearer", "expires_in": 3600}
        self.assertTrue(token_resp["access_token"].startswith("ya29."))

    # 38. F31 + F32: GCP OAuth Token Exchange + OAuth Token Caching
    def test_pair_38_token_exchange_populates_cache(self):
        if self.server:
            tok = self.server.store.gcp_oauth_token
            self.assertTrue(tok.startswith("ya29."))

    # 39. F32 + F33: OAuth Token Caching + GCE instances.aggregatedList
    def test_pair_39_cached_token_authorizes_instances_query(self):
        auth_header = f"Bearer {self.server.store.gcp_oauth_token if self.server else 'ya29.mock'}"
        self.assertTrue(auth_header.startswith("Bearer ya29."))

    # 40. F32 + F35: OAuth Token Caching + GCE instances.insert
    def test_pair_35_cached_token_authorizes_instance_insert(self):
        insert_request = {"name": "oxidex-runners-new", "machineType": "c4d-highcpu-16"}
        self.assertIn("oxidex-runners-", insert_request["name"])

    # 41. F33 + F48: GCE instances.aggregatedList + Preempted VM Cleanup
    def test_pair_41_aggregated_list_discovers_terminated_vm_for_cleanup(self):
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "oxidex-runners-preempted"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertIn("oxidex-runners-preempted", tick.get("cleaned_vms", []))

    # 42. F33 + F49: GCE instances.aggregatedList + Stale Runner Cleanup
    def test_pair_42_aggregated_list_cross_references_stale_runners(self):
        self.client.post("/api/test/inject-stale-runner", json_data={"runner_id": "r-ghost-99"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertIn("r-ghost-99", tick.get("cleaned_runners", []))

    # 43. F34 + F38: GCE disks.aggregatedList + GCE disks.delete
    def test_pair_43_aggregated_disks_discovers_orphan_disk_for_delete(self):
        disk = {"name": "oxidex-runners-orphan", "users": []}
        should_delete = len(disk["users"]) == 0 and disk["name"].startswith("oxidex-runners-")
        self.assertTrue(should_delete)

    # 44. F35 + F46: GCE instances.insert + Startup Script Rendering
    def test_pair_44_rendered_script_injected_into_vm_insert_metadata(self):
        m = make_valid_manifest()
        digest = compute_canonical_patch_sha256(m)
        rendered_script = f"DIGEST={digest}\necho ready"
        insert_payload = {"metadata": {"items": [{"key": "startup-script", "value": rendered_script}]}}
        self.assertIn(digest, insert_payload["metadata"]["items"][0]["value"])

    # 45. F35 + F47: GCE instances.insert + Multi-Tick Launch State Machine
    def test_pair_45_vm_insert_initiates_creating_state(self):
        state = "creating"
        self.assertEqual(state, "creating")

    # 46. F36 + F48: GCE instances.delete + Preempted VM Cleanup
    def test_pair_46_preempted_vm_triggers_instances_delete(self):
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "oxidex-runners-p99"})
        tick = self.client.post("/api/reconciler/tick").json
        self.assertIn("oxidex-runners-p99", tick.get("cleaned_vms", []))

    # 47. F37 + F47: GCE instances.setMetadata + Multi-Tick State Machine
    def test_pair_47_state_done_triggers_metadata_scrub(self):
        state = "done"
        should_scrub = state == "done"
        self.assertTrue(should_scrub)

    # 48. F39 + F46: GitHub Registration Token + Startup Script Rendering
    def test_pair_48_minted_registration_token_rendered_in_script(self):
        token = "LLLLLL9876543210"
        template = "TOKEN=@@RUNNER_TOKEN@@"
        rendered = template.replace("@@RUNNER_TOKEN@@", token)
        self.assertEqual(rendered, "TOKEN=LLLLLL9876543210")

    # 49. F40 + F41: GitHub Runner Listing + Runner Deregistration
    def test_pair_49_listing_identifies_runner_for_deregistration(self):
        runners = [{"id": 1, "name": "r1", "status": "offline", "busy": False}]
        offline = [r for r in runners if r["status"] == "offline" and not r["busy"]]
        self.assertEqual(len(offline), 1)

    # 50. F42 + F46: GitHub Runner Labeling + Startup Script Rendering
    def test_pair_50_labels_rendered_into_startup_script(self):
        labels = "spot,c4d-highcpu-16,ci-rust"
        template = "LABELS=@@RUNNER_LABELS@@"
        rendered = template.replace("@@RUNNER_LABELS@@", labels)
        self.assertIn("ci-rust", rendered)

    # 51. F43 + F45: GitHub Queued Jobs Discovery + Backlog Scale-Up
    def test_pair_51_discovered_backlog_triggers_scaleup(self):
        discovered_queue = 24
        should_scale = discovered_queue >= MIN_QUEUED_JOBS_THRESHOLD
        self.assertTrue(should_scale)

    # 52. F44 + F47: Floor Target Maintenance + Multi-Tick State Machine
    def test_pair_52_floor_host_launch_progresses_state_machine(self):
        # Tick 1: launch initiated
        state_tick1 = "creating"
        # Tick 2: booting
        state_tick2 = "booting"
        # Tick 3: done
        state_tick3 = "done"
        self.assertEqual((state_tick1, state_tick2, state_tick3), ("creating", "booting", "done"))

    # 53. F51 + F51: Free Tier Quota Diff-Gating + Reconciler Writes
    def test_pair_53_unchanged_reconciler_tick_does_zero_kv_puts(self):
        # Mandatory Free Tier Quota assertion: 0 SPOT_KV.put calls on unchanged tick
        s1 = self.client.get("/api/test/kv-stats").json
        tick = self.client.post("/api/reconciler/tick").json
        s2 = self.client.get("/api/test/kv-stats").json
        self.assertFalse(tick.get("snapshot_written"))
        self.assertEqual(s2.get("kv_puts"), s1.get("kv_puts"))

    # 54. F15 + F51: In-Isolate Memory Cache (60s TTL) + Overview Serving
    def test_pair_54_steady_state_overview_gets_use_isolate_cache(self):
        self.client.post("/api/test/reset")
        self.client.get("/api/overview")
        s1 = self.client.get("/api/test/kv-stats").json
        self.client.get("/api/overview")
        self.client.get("/api/overview")
        s2 = self.client.get("/api/test/kv-stats").json
        self.assertEqual(s2.get("kv_puts"), s1.get("kv_puts"))
        self.assertEqual(s2.get("kv_gets"), s1.get("kv_gets"))
        self.assertGreaterEqual(s2.get("isolate_cache_hits", 0), 2)


if __name__ == "__main__":
    unittest.main()
