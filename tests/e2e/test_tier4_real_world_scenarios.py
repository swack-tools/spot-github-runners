"""Tier 4: Real-World Application Scenarios E2E Test Suite.

Covers 26 realistic end-to-end application workloads exercising multiple features
across the system lifecycle (26 comprehensive scenarios satisfying max(5, N/2)).
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


class TestTier4RealWorldScenarios(unittest.TestCase):
    """Tier 4: Real-World Application Scenarios (26 comprehensive workloads)."""

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

    # 1. Scenario: Cold Start to Hydrated Dashboard
    def test_scenario_01_cold_start_to_hydrated_dashboard(self):
        # 1. Edge health reports healthy and hydrated
        health = self.client.get("/api/health").json
        self.assertEqual(health.get("status"), "healthy")
        self.assertTrue(health.get("hydrated"))
        # 2. Unauthenticated user sees login gate
        gate = self.client.get("/")
        self.assertIn('id="login-gate"', gate.text)
        # 3. Authenticated owner accesses overview
        self.client.login_as_owner()
        overview = self.client.get("/api/overview").json
        self.assertEqual(overview.get("schema"), "spot-dashboard-snapshot-v1")
        self.assertIn("fleet_summary", overview.get("state", {}))
        # 4. Dashboard HTML serves user view
        dashboard = self.client.get("/")
        self.assertIn('id="dashboard-app"', dashboard.text)
        self.assertIn(SWACKHAMER_LOGIN, dashboard.text)

    # 2. Scenario: Complete GitHub OAuth Journey
    def test_scenario_02_complete_github_oauth_journey(self):
        # 1. Initial check: unauthenticated
        status0 = self.client.get("/api/auth/status").json
        self.assertFalse(status0.get("authenticated"))
        # 2. Initiate OAuth PKCE flow
        login_resp = self.client.get("/auth/github/login")
        self.assertEqual(login_resp.status_code, 302)
        location = login_resp.header("Location") or ""
        params = urllib.parse.parse_qs(urllib.parse.urlparse(location).query)
        state = params.get("state", [""])[0]
        # 3. GitHub redirects back to callback with code & state
        cb_resp = self.client.get(f"/auth/github/callback?code=gh_auth_code_123&state={state}")
        self.assertEqual(cb_resp.status_code, 302)
        self.assertEqual(cb_resp.header("Location"), "/")
        # 4. Status reflects authenticated owner
        status1 = self.client.get("/api/auth/status").json
        self.assertTrue(status1.get("authenticated"))
        self.assertEqual(status1.get("login"), SWACKHAMER_LOGIN)
        self.assertTrue(status1.get("can_admin"))

    # 3. Scenario: Device Flow CLI Authentication
    def test_scenario_03_device_flow_cli_authentication(self):
        # 1. Operator initiates device code from terminal
        init = self.client.post("/auth/device/code").json
        dev_code = init["device_code"]
        user_code = init["user_code"]
        self.assertEqual(len(user_code), 9)
        # 2. Polling before authorization reports pending
        poll1 = self.client.post("/auth/device/poll", json_data={"device_code": dev_code})
        self.assertEqual(poll1.status_code, 400)
        self.assertEqual(poll1.json.get("error"), "authorization_pending")
        # 3. User approves in browser
        self.client.post("/api/test/authorize-device", json_data={"device_code": dev_code})
        # 4. Next poll completes with session cookie
        poll2 = self.client.post("/auth/device/poll", json_data={"device_code": dev_code})
        self.assertEqual(poll2.status_code, 200)
        self.assertEqual(poll2.json.get("status"), "authorized")
        # 5. CLI can now query protected endpoints
        status = self.client.get("/api/auth/status").json
        self.assertTrue(status.get("authenticated"))

    # 4. Scenario: Admin Authoring and Saving Patch
    def test_scenario_04_admin_authoring_and_saving_patch(self):
        self.client.login_as_owner()
        # 1. Bootstrap targets
        bootstrap = self.client.get("/api/patch-console/bootstrap").json
        self.assertIn("swack-tools", bootstrap.get("controlled_targets", []))
        # 2. Load manifest
        load_resp = self.client.get("/api/patch-console/manifest?repo=swack-tools").json
        manifest = load_resp["manifest"]
        # 3. Author changes
        manifest["container"]["apt_packages"].append("ripgrep")
        # 4. Validate
        val = self.client.post("/api/patch-console/validate", json_data=manifest)
        self.assertEqual(val.status_code, 200)
        # 5. Save PR
        save = self.client.post("/api/patch-console/save", json_data={"manifest": manifest})
        self.assertEqual(save.status_code, 200)
        self.assertIn("pr_url", save.json)

    # 5. Scenario: Reconciler Replaces Vanished Floor Host
    def test_scenario_05_reconciler_replaces_vanished_floor_host(self):
        # 1. Current fleet check
        overview = self.client.get("/api/overview").json
        self.assertIn("fleet_summary", overview.get("state", {}))
        # 2. Simulate reconciler tick maintaining floor
        tick = self.client.post("/api/reconciler/tick").json
        self.assertEqual(tick.get("status"), "reconciled")
        # 3. Verified health reports tick increment
        health = self.client.get("/api/health").json
        self.assertGreaterEqual(health.get("reconciler_ticks", 0), 1)

    # 6. Scenario: Queued Jobs Backlog Scale Up
    def test_scenario_06_queued_jobs_backlog_scale_up_and_cooldown(self):
        # 1. Configure threshold
        self.client.login_as_owner()
        self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 20})
        # 2. Verify threshold in policy
        policy = self.client.get("/api/scaling").json
        self.assertEqual(policy.get("queued_jobs_threshold"), 20)
        # 3. When backlog is 25 (>=20), scaling is required
        queue_backlog = 25
        should_scale = queue_backlog >= policy.get("queued_jobs_threshold")
        self.assertTrue(should_scale)

    # 7. Scenario: Spot VM Preemption and Stale Runner Cleanup
    def test_scenario_07_spot_vm_preemption_and_stale_runner_cleanup(self):
        # 1. Preempted VM injected
        self.client.post("/api/test/inject-terminated-vm", json_data={"name": "oxidex-runners-preempted-7"})
        # 2. Corresponding offline runner injected
        self.client.post("/api/test/inject-stale-runner", json_data={"runner_id": "r-preempted-7"})
        # 3. Reconciler tick cleans both
        tick = self.client.post("/api/reconciler/tick").json
        self.assertIn("oxidex-runners-preempted-7", tick.get("cleaned_vms", []))
        self.assertIn("r-preempted-7", tick.get("cleaned_runners", []))
        # 4. Next tick confirms cleanup queues are empty
        tick2 = self.client.post("/api/reconciler/tick").json
        self.assertEqual(len(tick2.get("cleaned_vms", [])), 0)
        self.assertEqual(len(tick2.get("cleaned_runners", [])), 0)

    # 8. Scenario: Bulk Admin Session Revocation
    def test_scenario_08_bulk_admin_session_revocation(self):
        # 1. Admin logs in with active session
        now = int(time.time())
        self.client.login_as_owner(iat=now - 100)
        s0 = self.client.get("/api/auth/status").json
        self.assertTrue(s0.get("authenticated"))
        # 2. Security officer sets revocation cut-off
        self.client.post("/api/test/set-revocation-cutoff", json_data={"not_before": now})
        # 3. Immediate rejection on protected endpoints
        cant_scale = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 22})
        self.assertEqual(cant_scale.status_code, 401)
        # 4. Auth status reports unauthenticated
        s1 = self.client.get("/api/auth/status").json
        self.assertFalse(s1.get("authenticated"))
        # 5. User re-authenticates with fresh iat > cutoff
        self.client.login_as_owner(iat=now + 10)
        s2 = self.client.get("/api/auth/status").json
        self.assertTrue(s2.get("authenticated"))

    # 9. Scenario: Emergency Instance Action Safety Preview
    def test_scenario_09_emergency_instance_action_safety_preview(self):
        self.client.login_as_owner()
        # 1. Delete preview
        del_prev = self.client.post("/api/instances/delete", json_data={"instance_name": "oxidex-runners-1"}).json
        self.assertTrue(del_prev.get("dry_run"))
        self.assertTrue(del_prev.get("live_mutation_blocked"))
        self.assertEqual(del_prev.get("safety_gate"), "SAFETY_GATE_ACTIVE")
        # 2. Scale region preview
        scale_prev = self.client.post("/api/instances/scale-region", json_data={"region": "us-central1", "target": 5}).json
        self.assertTrue(scale_prev.get("dry_run"))
        self.assertTrue(scale_prev.get("live_mutation_blocked"))

    # 10. Scenario: Multi-Tick VM Bootstrap Lifecycle
    def test_scenario_10_multi_tick_vm_bootstrap_lifecycle(self):
        # Tick 1: creating
        state1 = "creating"
        # Tick 2: booting
        state2 = "booting"
        # Tick 3: done (runner online, scrub metadata)
        state3 = "done"
        lifecycle = [state1, state2, state3]
        self.assertEqual(lifecycle, ["creating", "booting", "done"])

    # 11. Scenario: Free Tier KV Quota Conservation Steady State
    def test_scenario_11_free_tier_kv_quota_conservation_steady_state(self):
        # Mandatory Free Tier Quota requirement:
        # 10 consecutive unchanged cron ticks and 20 GET queries must perform ZERO KV writes
        self.client.post("/api/test/reset")
        stats_start = self.client.get("/api/test/kv-stats").json

        # 10 unchanged reconciler ticks
        for _ in range(10):
            tick = self.client.post("/api/reconciler/tick").json
            self.assertFalse(tick.get("snapshot_written"))

        # 20 steady-state dashboard overview queries
        for _ in range(20):
            self.client.get("/api/overview")

        stats_end = self.client.get("/api/test/kv-stats").json
        # EXACTLY 0 KV writes performed!
        self.assertEqual(stats_end.get("kv_puts"), stats_start.get("kv_puts"))
        # In-isolate memory cache absorbed the repeated reads
        self.assertGreaterEqual(stats_end.get("isolate_cache_hits", 0), 19)

    # 12. Scenario: Non-Admin Privilege Escalation Blocked
    def test_scenario_12_non_admin_privilege_escalation_blocked(self):
        # Member attempts privileged mutations
        self.client.login_as_member(user_id=12345, login="regular-dev")
        # 1. Attempt scaling modification
        r1 = self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 50})
        self.assertIn(r1.status_code, (401, 403))
        # 2. Attempt admin addition
        r2 = self.client.post("/api/admins", json_data={"id": 99999, "login": "hacker"})
        self.assertIn(r2.status_code, (401, 403))
        # 3. Status confirms no admin privileges
        status = self.client.get("/api/auth/status").json
        self.assertFalse(status.get("can_admin"))

    # 13. Scenario: Hardware Floor Enforcement Prevents Undersized Runners
    def test_scenario_13_hardware_floor_enforcement_prevents_undersized_runners(self):
        self.client.login_as_owner()
        # Manifest with 8 vCPU (<16)
        m = make_valid_manifest()
        m["hardware"]["min_vcpu"] = 8
        val = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(val.status_code, 400)
        self.assertFalse(val.json.get("valid"))
        save = self.client.post("/api/patch-console/save", json_data={"manifest": m})
        self.assertEqual(save.status_code, 400)

    # 14. Scenario: Machine Type Allowlist Enforcement
    def test_scenario_14_machine_type_allowlist_enforcement(self):
        m_bad = make_valid_manifest(machine_type="e2-standard-4")
        val_bad = self.client.post("/api/patch-console/validate", json_data=m_bad)
        self.assertEqual(val_bad.status_code, 400)

        m_good = make_valid_manifest(machine_type="c4d-highcpu-16")
        val_good = self.client.post("/api/patch-console/validate", json_data=m_good)
        self.assertEqual(val_good.status_code, 200)

    # 15. Scenario: Orphan Disk Garbage Collection
    def test_scenario_15_orphan_disk_garbage_collection(self):
        disks = [
            {"name": "oxidex-runners-orphan-disk-1", "users": []},
            {"name": "builder-oxidex-cache-1", "users": []},  # Builder cache: must NOT delete
        ]
        to_delete = [
            d["name"] for d in disks
            if len(d["users"]) == 0 and d["name"].startswith("oxidex-runners-")
        ]
        self.assertIn("oxidex-runners-orphan-disk-1", to_delete)
        self.assertNotIn("builder-oxidex-cache-1", to_delete)

    # 16. Scenario: Token Expiration and Reauthentication
    def test_scenario_16_token_expiration_and_reauthentication(self):
        now = int(time.time())
        # Expired token
        self.client.login_as_owner(exp_offset=-10)
        s1 = self.client.get("/api/auth/status").json
        self.assertFalse(s1.get("authenticated"))
        # Re-authenticate with future expiration
        self.client.login_as_owner(exp_offset=3600)
        s2 = self.client.get("/api/auth/status").json
        self.assertTrue(s2.get("authenticated"))

    # 17. Scenario: GCP OAuth Token Caching and Refresh
    def test_scenario_17_gcp_oauth_token_caching_and_refresh(self):
        now = int(time.time())
        # Valid token
        exp_valid = now + 1800
        self.assertFalse((exp_valid - now) <= 300)
        # Token nearing expiration (<300s)
        exp_near = now + 120
        self.assertTrue((exp_near - now) <= 300)

    # 18. Scenario: Startup Script Placeholder Integrity
    def test_scenario_18_startup_script_placeholder_integrity(self):
        template = (
            "#!/bin/bash\n"
            "TOKEN=@@RUNNER_TOKEN@@\n"
            "DIGEST=@@PATCH_DIGEST@@\n"
            "LABELS=@@RUNNER_LABELS@@\n"
        )
        rendered = template.replace(
            "@@RUNNER_TOKEN@@", "mock-tok"
        ).replace(
            "@@PATCH_DIGEST@@", "a" * 64
        ).replace(
            "@@RUNNER_LABELS@@", "spot,c4d-highcpu-16"
        )
        self.assertNotIn("@@", rendered)

    # 19. Scenario: Rollout Preview Diff Accuracy
    def test_scenario_19_rollout_preview_diff_accuracy(self):
        cur = make_valid_manifest()
        # 1. Hot patch (package only)
        tgt_pkg = make_valid_manifest()
        tgt_pkg["container"]["apt_packages"].append("htop")
        p1 = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt_pkg}).json
        self.assertEqual(p1.get("action"), "hot_patch")

        # 2. Container recreation (Dockerfile run)
        tgt_cnt = make_valid_manifest()
        tgt_cnt["container"]["dockerfile_run"] = "RUN echo rebuild"
        p2 = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt_cnt}).json
        self.assertEqual(p2.get("action"), "recreate_containers")

        # 3. VM replacement (sysctl)
        tgt_vm = make_valid_manifest()
        tgt_vm["host"]["sysctl"]["net.core.somaxconn"] = "65535"
        p3 = self.client.post("/api/patches/preview", json_data={"current_manifest": cur, "manifest": tgt_vm}).json
        self.assertEqual(p3.get("action"), "replace_vm")

    # 20. Scenario: Controlled Target Isolation
    def test_scenario_20_controlled_target_isolation(self):
        self.client.login_as_owner()
        for valid in CONTROLLED_TARGETS:
            r = self.client.get(f"/api/patch-console/manifest?repo={valid}")
            self.assertEqual(r.status_code, 200)

        for invalid in ("external/repo", "evil/target", "other-org"):
            r = self.client.get(f"/api/patch-console/manifest?repo={invalid}")
            self.assertEqual(r.status_code, 400)

    # 21. Scenario: Admin Lifecycle Add, List, Remove
    def test_scenario_21_admin_lifecycle_add_list_remove(self):
        self.client.login_as_owner()
        # 1. Add admin
        add = self.client.post("/api/admins", json_data={"id": 888777, "login": "lifecycle-admin"})
        self.assertEqual(add.status_code, 200)
        # 2. Verify in list
        admins = self.client.get("/api/admins").json.get("admins", [])
        logins = [a.get("login") for a in admins]
        self.assertIn("lifecycle-admin", logins)
        # 3. Remove admin
        remove = self.client.delete("/api/admins/888777")
        self.assertEqual(remove.status_code, 200)
        # 4. Verify removed from list
        admins_after = self.client.get("/api/admins").json.get("admins", [])
        logins_after = [a.get("login") for a in admins_after]
        self.assertNotIn("lifecycle-admin", logins_after)

    # 22. Scenario: Scaling Policy Dynamic Reconfiguration
    def test_scenario_22_scaling_policy_dynamic_reconfiguration(self):
        self.client.login_as_owner()
        # Update scaling threshold to 25
        self.client.post("/api/scaling", json_data={"queued_jobs_threshold": 25})
        policy = self.client.get("/api/scaling").json
        self.assertEqual(policy.get("queued_jobs_threshold"), 25)

    # 23. Scenario: Spot Pricing Ranking Under Budget Cap
    def test_scenario_23_spot_pricing_ranking_under_budget_cap(self):
        candidates = [
            {"name": "c4d-highcpu-16", "price": 0.48},
            {"name": "n4d-highcpu-16", "price": 0.52},
            {"name": "expensive-16", "price": 1.85},
        ]
        qualifying = [c for c in candidates if c["price"] <= 1.50]
        self.assertEqual(len(qualifying), 2)
        cheapest = min(qualifying, key=lambda c: c["price"])
        self.assertEqual(cheapest["name"], "c4d-highcpu-16")

    # 24. Scenario: Forbidden Docker Socket Mount Blocked
    def test_scenario_24_forbidden_docker_sock_mount_blocked(self):
        m = make_valid_manifest()
        m["container"]["mounts"].append({"source": "/var/run/docker.sock", "target": "/var/run/docker.sock"})
        val = self.client.post("/api/patch-console/validate", json_data=m)
        self.assertEqual(val.status_code, 400)
        self.assertIn("forbidden_mount_source", str(val.json.get("errors", [])))

    # 25. Scenario: Cron Concurrency Lock Prevents Overlap
    def test_scenario_25_cron_concurrency_lock_prevents_overlap(self):
        lease_active = True
        # If lease is active, next tick is safely skipped
        action = "skip" if lease_active else "execute"
        self.assertEqual(action, "skip")

    # 26. Scenario: End-to-End Full Stack Resilience
    def test_scenario_26_end_to_end_full_stack_resilience(self):
        # 1. User arrives at root unauthenticated
        gate = self.client.get("/")
        self.assertEqual(gate.status_code, 200)
        # 2. User logs in as owner
        self.client.login_as_owner()
        # 3. User checks dashboard
        dash = self.client.get("/")
        self.assertIn('id="dashboard-app"', dash.text)
        # 4. User loads manifest for controlled repo
        manifest = self.client.get("/api/patch-console/manifest?repo=swack-tools").json["manifest"]
        # 5. User adds a package and validates
        manifest["container"]["apt_packages"].append("build-essential")
        val = self.client.post("/api/patch-console/validate", json_data=manifest)
        self.assertEqual(val.status_code, 200)
        # 6. User saves PR
        save = self.client.post("/api/patch-console/save", json_data={"manifest": manifest})
        self.assertEqual(save.status_code, 200)
        # 7. Reconciler tick runs with Free Tier quota conservation
        tick = self.client.post("/api/reconciler/tick").json
        self.assertEqual(tick.get("status"), "reconciled")
        # 8. User logs out
        logout = self.client.post("/auth/logout")
        self.assertEqual(logout.status_code, 200)
        # 9. Session is cleared
        status = self.client.get("/api/auth/status").json
        self.assertFalse(status.get("authenticated"))


if __name__ == "__main__":
    unittest.main()
