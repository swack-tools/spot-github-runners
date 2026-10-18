"""Shared disk role attribution must not depend on cross-role instance ordering."""

import itertools
import unittest
from unittest.mock import AsyncMock, patch

from lib import compute_api
from lib.fleet_inventory import gce_fleet_source, observe_detached_disks, observe_fleet_items


class SharedDiskBillingRoleTests(unittest.TestCase):
    def _items(self, disk_name, *, ids=("1", "2"), names=("builder-a", "oxidex-runners-a"), boot=False):
        source = "projects/p/zones/us-south1-a/disks/" + disk_name
        return [{"id": identity, "name": name, "zone": "zones/us-south1-a", "status": "RUNNING",
                 "machineType": "machineTypes/n4-highcpu-16",
                 "disks": [{"source": source, "type": "PERSISTENT", "diskSizeGb": "100",
                            "boot": boot, "autoDelete": False, "deviceName": "runner-data"}]}
                for identity, name in zip(ids, names)]

    def _catalog(self, items, *, named=True):
        source = items[0]["disks"][0]["source"]
        detail = {"id": "500", "type": "pd-ssd", "sizeGb": "100",
                  "creationTimestamp": "2026-10-18T00:00:00Z"}
        if named:
            detail["name"] = source.rsplit("/", 1)[-1]
        return {source: detail}

    def _assert_single_charge(self, observations, *, boot=False):
        self.assertEqual(sum(d["billing_owner"] for o in observations for d in o["disks_detail"]), 1)
        self.assertEqual(sum(o["boot_disk_billed_gb"] + sum(o["data_disks"].values())
                             for o in observations), 100)
        for obs in observations:
            self.assertEqual(obs["boot_disk_gb"] if boot else obs["data_disk_gb"], 100)
            self.assertEqual(obs["shared_attachments"], obs["disks_detail"])
            self.assertEqual(obs["disks_detail"][0]["gb"], 100)
            self.assertFalse(obs["disks_detail"][0]["auto_delete"])

    def test_explicit_role_survives_swapped_ids_and_listing_order(self):
        cases = [("runner-data", "cicd_runner"), ("runner-data-cache", "cicd_runner"),
                 ("oxidex-runners-cache", "cicd_runner"), ("newznab-spot-runners-cache", "cicd_runner"),
                 ("spot-runners-cache", "cicd_runner"), ("oxidex-spot-runners-cache", "cicd_runner"),
                 ("builder-cache", "rust_builder")]
        for (name, role), ids, named, boot in itertools.product(
                cases, [("1", "2"), ("2", "1")], [True, False], [True, False]):
            items = self._items(name, ids=ids, boot=boot)
            for ordered in (items, list(reversed(items))):
                with self.subTest(name=name, ids=ids, named=named, boot=boot, first=ordered[0]["id"]):
                    obs = observe_fleet_items(ordered, disk_types=self._catalog(items, named=named))
                    self._assert_single_charge(obs, boot=boot)
                    owner = next(o for o in obs if o["disks_detail"][0]["billing_owner"])
                    self.assertEqual(owner["role"], role)
                    self.assertTrue(all(o.get("storage_billing_ambiguous") is False for o in obs))

    def test_custom_cross_role_disk_flags_every_participant(self):
        # deviceName is an attachment alias, not canonical disk-role evidence.
        for disk_name in ("custom-cache", "runner-database"):
            items = self._items(disk_name, ids=("3", "1", "2"),
                                names=("builder-a", "oxidex-runners-a", "builder-b"))
            for ordered in itertools.permutations(items):
                obs = observe_fleet_items(list(ordered), disk_types=self._catalog(items))
                self._assert_single_charge(obs)
                self.assertTrue(all(o.get("storage_billing_ambiguous") is True for o in obs))

    def test_explicit_role_selects_stable_owner_within_that_role(self):
        items = self._items("runner-data", ids=("1", "3", "2"),
                            names=("builder-a", "oxidex-runners-b", "oxidex-runners-a"))
        for ordered in itertools.permutations(items):
            obs = observe_fleet_items(list(ordered), disk_types=self._catalog(items))
            self._assert_single_charge(obs)
            owner = next(o for o in obs if o["disks_detail"][0]["billing_owner"])
            self.assertEqual(owner["instance_id"], "2")

    def test_same_role_keeps_existing_id_order_ownership(self):
        for names in (("builder-a", "builder-b"), ("oxidex-runners-a", "oxidex-runners-b")):
            items = self._items("custom-cache", ids=("2", "1"), names=names)
            for ordered in (items, list(reversed(items))):
                obs = observe_fleet_items(ordered, disk_types=self._catalog(items))
                self._assert_single_charge(obs)
                self.assertEqual(obs[0]["instance_id"], "1")
                self.assertTrue(obs[0]["disks_detail"][0]["billing_owner"])
                self.assertTrue(all(o.get("storage_billing_ambiguous") is False for o in obs))

    def test_ambiguity_does_not_spread_to_unrelated_vm(self):
        items = self._items("custom-cache")
        unrelated = self._items("builder-private", ids=("3",), names=("builder-private",))[0]
        obs = observe_fleet_items(items + [unrelated], disk_types=self._catalog(items))
        self.assertEqual([o.get("storage_billing_ambiguous") for o in obs], [True, True, False])

    def test_denied_catalog_uses_source_role_and_flags_custom_disk(self):
        for name, ambiguous in (("runner-data", False), ("custom-cache", True)):
            obs = observe_fleet_items(self._items(name), disk_types_known=False)
            self._assert_single_charge(obs)
            self.assertEqual([o.get("storage_billing_ambiguous") for o in obs], [ambiguous, ambiguous])
            if not ambiguous:
                self.assertEqual(next(o["role"] for o in obs if o["disks_detail"][0]["billing_owner"]),
                                 "cicd_runner")

    def test_fleet_source_propagates_ambiguity_and_attachment_sources(self):
        items = self._items("custom-cache")
        catalog = self._catalog(items)
        with patch.object(compute_api, "get_auth", return_value=compute_api.Auth("t", "p")), \
                patch.object(compute_api, "list_instances", AsyncMock(return_value=(items, []))), \
                patch.object(compute_api, "list_disk_types", AsyncMock(return_value=catalog)), \
                patch.object(compute_api, "machine_type_capacity",
                             AsyncMock(return_value={"vcpu": 16, "ram_gib": 32.0})):
            obs = gce_fleet_source("p")()
        self.assertEqual(len(obs), 2)
        self._assert_single_charge(obs)
        self.assertTrue(all(o.get("storage_billing_ambiguous") is True for o in obs))
        self.assertTrue(all(o["specs_known"] for o in obs))
        for o in obs:
            self.assertEqual(o["disks_detail"][0]["source"], items[0]["disks"][0]["source"])


class SplitSnapshotDiskBillingRoleTests(unittest.TestCase):
    def catalog(self, name, users, *, include_name=True):
        source = "projects/p/zones/us-south1-a/disks/" + name
        detail = {"id": "501", "type": "pd-ssd", "sizeGb": "100", "zone": "zones/us-south1-a",
                  "users": ["projects/p/zones/us-south1-a/instances/" + user for user in users]}
        if include_name:
            detail["name"] = name
        return {source: detail}

    def assert_one_disk_charge(self, observations):
        self.assertEqual(len(observations), 1)
        self.assertEqual(observations[0]["instance_id"], "disk-501")
        self.assertEqual(observations[0]["kind"], "disk")
        self.assertEqual(sum(sum(o["data_disks"].values()) for o in observations), 100)
        self.assertEqual(sum(d["gb"] for o in observations for d in o["disks_detail"]), 100)

    def test_unnamed_mixed_users_flag_ambiguity_independent_of_order(self):
        users = ("builder-a", "oxidex-runners-a", "builder-b")
        for ordered, include_name in itertools.product(itertools.permutations(users), (True, False)):
            with self.subTest(users=ordered, include_name=include_name):
                catalog = self.catalog("custom-cache", ordered, include_name=include_name)
                obs = observe_detached_disks(catalog)
                self.assert_one_disk_charge(obs)
                self.assertTrue(obs[0].get("storage_billing_ambiguous"))
                self.assertEqual(obs[0]["role"], "cicd_runner")  # Stable numerical carrier, not role evidence.

    def test_explicit_name_or_source_role_wins_for_mixed_users(self):
        cases = (("runner-data", "cicd_runner"), ("runner-data-cache", "cicd_runner"),
                 ("oxidex-runners-cache", "cicd_runner"), ("builder-cache", "rust_builder"))
        for (name, role), ordered, include_name in itertools.product(
                cases, itertools.permutations(("builder-a", "oxidex-runners-a")), (True, False)):
            with self.subTest(name=name, users=ordered, include_name=include_name):
                obs = observe_detached_disks(self.catalog(name, ordered, include_name=include_name))
                self.assert_one_disk_charge(obs)
                self.assertEqual(obs[0]["role"], role)
                self.assertIs(obs[0].get("storage_billing_ambiguous"), False)

    def test_single_role_users_remain_unambiguous(self):
        for users, role in ((("builder-a", "builder-b"), "rust_builder"),
                            (("oxidex-runners-a", "spot-runners-b"), "cicd_runner")):
            for ordered in itertools.permutations(users):
                with self.subTest(users=ordered):
                    obs = observe_detached_disks(self.catalog("custom-cache", ordered))
                    self.assert_one_disk_charge(obs)
                    self.assertEqual(obs[0]["role"], role)
                    self.assertIs(obs[0].get("storage_billing_ambiguous"), False)

    def test_attached_source_suppresses_split_snapshot_charge(self):
        catalog = self.catalog("custom-cache", ("builder-a", "oxidex-runners-a"))
        self.assert_one_disk_charge(observe_detached_disks(catalog))
        self.assertEqual(observe_detached_disks(catalog, attached_sources=frozenset(catalog)), [])


if __name__ == "__main__":
    unittest.main()
