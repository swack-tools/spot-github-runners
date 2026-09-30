"""Verify price eviction without changing cloud resources."""

import json
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from lib.price_guard import check_running_prices
from lib.runner_fleet import GceVm, GitHubRunner
from lib.spot_runner_autoscaler import (
    advance_vm_drains,
    read_scale_state,
    reconcile_prices,
    utilization_counts,
)


class PriceGuardTests(unittest.TestCase):
    def setUp(self):
        self.vm = GceVm("oxidex-runners-test", "asia-south2-a", "RUNNING", "n2-standard-32", True)
        self.args = SimpleNamespace(project="project", org="org", max_daily_usd=Decimal("5"))
        self.shape = SimpleNamespace(stdout=json.dumps({"guestCpus": 32, "memoryMb": 131072}))

    @patch("lib.price_guard.subprocess.run")
    @patch("lib.price_guard.spot_rates")
    def test_price_comparison_and_fresh_lookup_on_each_check(self, rates, command):
        command.return_value = self.shape
        rates.side_effect = [(Decimal("0.005"), Decimal(0)),
                             (Decimal("0.01"), Decimal(0))]
        self.assertEqual(check_running_prices([self.vm], "project", Decimal("3.84")), [])
        self.assertEqual(check_running_prices([self.vm], "project", Decimal("5")), [self.vm])
        self.assertEqual(rates.call_count, 2)

    @patch("lib.price_guard.subprocess.run")
    @patch("lib.price_guard.spot_rates", side_effect=RuntimeError("API unavailable"))
    def test_missing_price_never_marks_a_vm_for_eviction(self, rates, command):
        with self.assertLogs("spot_runner_autoscaler", level="ERROR"):
            self.assertEqual(check_running_prices([self.vm], "project", Decimal("5")), [])
        command.assert_not_called()

    def test_alternative_supported_replacement_families_count_as_capacity(self):
        runners = [GitHubRunner(index, f"gha-replacement-{index}", "online", False,
                               ("spot", "X64", shape))
                   for index, shape in enumerate(("e2-standard-32", "c2d-standard-32"))]
        self.assertEqual(utilization_counts(runners, "spot"), (2, 0))

    def test_busy_price_eviction_clears_labels_and_waits_before_deleting(self):
        runner = GitHubRunner(1, "gha-oxidex-runners-test-1", "online", True,
                              ("self-hosted", "Linux", "X64", "spot", "n2-standard-32"))
        state = {"idle_loops": 1, "scale_up_active": False, "draining_vms": {}}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            with patch("lib.spot_runner_autoscaler.check_running_prices", return_value=[self.vm]), \
                    patch("lib.spot_runner_autoscaler.subprocess.run") as command, \
                    patch("lib.spot_runner_autoscaler.list_gcloud_vms", return_value=[self.vm]):
                self.assertTrue(reconcile_prices([self.vm], [runner], self.args, state, path))
                self.assertTrue(command.call_args.args[0][-1].endswith("/labels"))
            recovered = read_scale_state(path)
            self.assertEqual(recovered["price_replacement_target"], 1)
            self.assertEqual(recovered["draining_vms"][self.vm.name]["reason"], "price")

            idle_runner = GitHubRunner(1, runner.name, "online", False,
                                       ("self-hosted", "Linux", "X64"))
            with patch("lib.spot_runner_autoscaler.subprocess.run") as command:
                removed, runner_ids = advance_vm_drains(
                    [self.vm], [idle_runner], self.args, recovered)
            self.assertEqual(removed, {self.vm.name})
            self.assertEqual(runner_ids, {runner.runner_id})
            self.assertTrue(command.call_args_list[0].args[0][4].endswith("/1"))
            self.assertIn("--delete-disks=all", command.call_args_list[1].args[0])

            with patch("lib.spot_runner_autoscaler.check_running_prices", return_value=[]), \
                    patch("lib.spot_runner_autoscaler.list_gcloud_vms", return_value=[]), \
                    patch("lib.spot_runner_autoscaler.discover_offers", return_value=["offer"]), \
                    patch("lib.spot_runner_autoscaler.scale_up", return_value=1) as launch:
                self.assertTrue(reconcile_prices([], [], self.args, recovered, path))
                launch.assert_called_once_with(1, ["offer"], self.args)
            self.assertEqual(read_scale_state(path)["price_replacement_target"], 0)

    def test_drain_keeps_vm_running_while_any_runner_is_busy(self):
        runner = GitHubRunner(1, "gha-oxidex-runners-test-1", "online", True,
                              ("self-hosted", "Linux", "X64"))
        state = {"draining_vms": {self.vm.name: {
            "zone": self.vm.zone,
            "reason": "scale-down",
            "runner_ids": [runner.runner_id],
        }}}
        with patch("lib.spot_runner_autoscaler.subprocess.run") as command:
            removed, runner_ids = advance_vm_drains([self.vm], [runner], self.args, state)

        self.assertEqual(removed, set())
        self.assertEqual(runner_ids, set())
        self.assertEqual(command.call_count, 0)
        self.assertIn(self.vm.name, state["draining_vms"])


if __name__ == "__main__":
    unittest.main()
