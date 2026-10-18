"""Repricing preserves historical trust and failed inventory writes remain atomic."""

import copy
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from lib.dashboard_server import (DashboardStore, _live_instance_record, _mark_instance_deleted,
                                  _retained_pricing, _unpriced_history_in_window)
from lib.fleet_inventory import FleetListing


class PricingHistoryTests(unittest.TestCase):
    now = 1800000000.0

    def setUp(self):
        self.live = patch("lib.dashboard_models.fetch_live_spot_pricing", return_value={})
        self.live.start()
        self.addCleanup(self.live.stop)

    def observation(self, **changes):
        disk = {"source": "z/builder-price-cache", "id": "123", "type": "pd-balanced", "gb": 100,
                "created_epoch": self.now - 7200, "auto_delete": False, "boot": False}
        obs = {"instance_id": "price", "name": "builder-price", "role": "rust_builder", "status": "RUNNING",
               "zone": "us-south1-b", "region": "us-south1", "machine_type": "n4-highcpu-16", "vcpu": 16,
               "ram_gib": 32.0, "spot": True, "specs_known": True, "boot_disk_type": "pd-balanced",
               "boot_disk_gb": 0, "data_disk_type": "pd-balanced", "data_disk_gb": 100,
               "data_disks": {"pd-balanced": 100}, "data_disks_attached_gb": 100, "disks_detail": [disk],
               "created_epoch": self.now - 7200, "last_start_epoch": self.now - 7200}
        return dict(obs, **changes)

    def store(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        store = DashboardStore(state_path=Path(temp.name) / "state.json")
        store.start()
        self.addCleanup(store.close)
        return store

    def test_retained_disks_preserve_unknown_rate_and_live_catalog_completeness(self):
        obs = self.observation()
        disk = dict(obs["disks_detail"][0], type="pd-not-catalogued")
        obs.update(data_disk_type=disk["type"], data_disks={disk["type"]: 100}, disks_detail=[disk])
        record = _live_instance_record(obs, project="p", now=self.now)
        retained = _retained_pricing(record, project="p", tax_rate_percent=0)
        self.assertFalse(retained["complete"])
        self.assertIn(disk["type"], retained["disk_rate_unknown"])
        deleted = _mark_instance_deleted(record, self.now + 60, disks_pending=True, project="p")
        self.assertFalse(deleted["pricing_complete"])
        trusted = _retained_pricing(record, project="p", tax_rate_percent=0,
                                    live_pricing={"disk_monthly_gib": {disk["type"]: 0.2}})
        self.assertIsNot(trusted.get("complete"), False)
        self.assertFalse(trusted.get("disk_rate_unknown"))
        self.assertGreater(trusted["disk_hourly_usd"], 0)

    def test_compute_fallback_remains_unknown_after_shape_gets_trusted_rate(self):
        record = _live_instance_record(self.observation(machine_type="unknown-standard-16"), project="p", now=self.now)
        self.assertFalse(record["pricing_complete"])
        trusted = _live_instance_record(self.observation(), project="p", now=self.now + 3600,
                                        previous=json.loads(json.dumps(record)))
        self.assertTrue(trusted["pricing_complete"])
        self.assertFalse(trusted["lifetime_cost_complete"])
        self.assertIn("fallback_compute", trusted["unpriced_history_intervals"])
        self.assertTrue(_unpriced_history_in_window(trusted, self.now - 86400, self.now + 3600))
        self.assertFalse(_unpriced_history_in_window(trusted, self.now + 3600, self.now + 7200))
        again = _live_instance_record(self.observation(), project="p", now=self.now + 33 * 86400,
                                      previous=json.loads(json.dumps(trusted)))
        self.assertFalse(again["lifetime_cost_complete"])
        self.assertFalse(_unpriced_history_in_window(again, self.now + 2 * 86400, self.now + 33 * 86400))

    def test_disk_catalog_recovery_keeps_storage_only_history_uncertain(self):
        obs = self.observation()
        disk = dict(obs["disks_detail"][0], type="pd-not-catalogued")
        obs.update(data_disk_type=disk["type"], data_disks={disk["type"]: 100}, disks_detail=[disk])
        record = _live_instance_record(obs, project="p", now=self.now)
        store = self.store()
        with patch("lib.dashboard_server.fetch_live_spot_pricing", return_value={"disk_monthly_gib": {disk["type"]: 0.2}}), \
             patch.object(store, "_clock", return_value=self.now + 3600):
            self.assertTrue(store._refresh_instances_pricing([record], live=True))
        self.assertTrue(record["pricing_complete"])
        self.assertFalse(record["lifetime_cost_complete"])
        self.assertIn("fallback_storage", record["unpriced_history_intervals"])
        self.assertTrue(_unpriced_history_in_window(record, self.now - 86400, self.now + 3600))
        self.assertFalse(_unpriced_history_in_window(record, self.now - 86400, self.now + 3600, include_storage=False))

    def test_trust_only_change_preserves_history_without_splitting_equal_rates(self):
        from lib.dashboard_server import _mark_pricing_completeness, _pricing_rates

        record = _live_instance_record(self.observation(), project="p", now=self.now)
        rates = _pricing_rates(record["pricing"])
        record["pricing"].update(complete=False, family_rate_unknown=True)
        _mark_pricing_completeness(record)
        before_runs = copy.deepcopy(record["prior_runs"])
        store = self.store()
        with patch("lib.dashboard_server.fetch_live_spot_pricing", return_value={}),              patch.object(store, "_clock", return_value=self.now + 3600):
            self.assertTrue(store._refresh_instances_pricing([record], live=True))
        self.assertEqual(_pricing_rates(record["pricing"]), rates)
        self.assertEqual(record["prior_runs"], before_runs)
        self.assertTrue(record["pricing_complete"])
        self.assertFalse(record["lifetime_cost_complete"])
        self.assertIn("fallback_compute", record["unpriced_history_intervals"])

    def test_retained_disk_catalog_recovery_keeps_pending_history_incomplete(self):
        obs = self.observation()
        disk = dict(obs["disks_detail"][0], type="pd-not-catalogued")
        obs.update(data_disk_type=disk["type"], data_disks={disk["type"]: 100}, disks_detail=[disk])
        record = _mark_instance_deleted(_live_instance_record(obs, project="p", now=self.now),
                                        self.now + 60, disks_pending=True, project="p")
        store = self.store()
        with patch("lib.dashboard_server.fetch_live_spot_pricing", return_value={"disk_monthly_gib": {disk["type"]: 0.2}}),              patch.object(store, "_clock", return_value=self.now + 3600):
            self.assertTrue(store._refresh_instances_pricing([record], live=True))
        self.assertTrue(record["disk_liability_pending"])
        self.assertTrue(record["pricing_complete"])
        self.assertFalse(record["lifetime_cost_complete"])
        self.assertIn("fallback_storage", record["unpriced_history_intervals"])

    def test_recoverable_disk_debt_does_not_hide_an_independent_unknown_rate(self):
        from lib.dashboard_server import _remember_incomplete_pricing

        cache = {"source": "z/cache", "type": "unsupported-cache", "gb": 100}
        pending = {"source": "z/new", "type": "pd-balanced", "gb": 100, "billing_deferred_since": 0.0}
        previous = {"pricing_complete": False, "deferred_disk_sources": [pending["source"]],
                    "disks_detail": [cache, pending]}
        record = {}
        _remember_incomplete_pricing(record, previous,
            {"complete": False, "disk_rate_unknown": ["unsupported-cache"]}, 0, 0, 0, 3600)
        self.assertEqual(record["unpriced_history_intervals"]["fallback_storage"], [0, 3600])
        self.assertTrue(record["unpriced_lifetime_history"])
        # Debt for the only unknown disk remains recoverable, without manufacturing a permanent history hold.
        previous["disks_detail"] = [dict(pending, type="unsupported-cache")]
        recovered = {}
        _remember_incomplete_pricing(recovered, previous,
            {"complete": False, "disk_rate_unknown": ["unsupported-cache"]}, 0, 0, 0, 3600)
        self.assertFalse(recovered.get("unpriced_history_intervals"))

    def test_failed_preserved_detached_repricing_does_not_mutate_nested_state(self):
        store = self.store()
        obs = self.observation(instance_id="disk-123", kind="disk", status="DETACHED", machine_type="", vcpu=0, ram_gib=0)
        store.fleet_source = lambda: [obs]
        with patch.object(store, "_clock", return_value=self.now):
            self.assertTrue(store.refresh_fleet_inventory())
        before = copy.deepcopy(store.state)
        previous_version = store.state_version()
        denied = FleetListing([])
        denied.disks_listed = False
        store.fleet_source = lambda: denied
        with patch.object(store, "_clock", return_value=self.now + 3600), \
             patch("lib.dashboard_server.fetch_live_spot_pricing", return_value={"disk_monthly_gib": {"pd-balanced": 0.3}}), \
             patch.object(store, "_persist", side_effect=OSError("read-only filesystem")):
            self.assertFalse(store.refresh_fleet_inventory())
        self.assertEqual(store.state_version(), previous_version)
        self.assertEqual(store.state, before)
        self.assertEqual(store.state["instances"][0]["prior_runs"], before["instances"][0]["prior_runs"])
        self.assertEqual(store.state["instances"][0]["disk_segments"], before["instances"][0]["disk_segments"])
