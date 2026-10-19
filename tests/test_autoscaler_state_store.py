"""Autoscaler controller state in Cloud Storage: generation-checked writes that fail closed and fence mutations.

A fake Cloud Storage client stands in for the JSON API; nothing touches GCP.
"""
from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from lib import autoscaler_state_store as store_module
from lib import spot_runner_autoscaler as controller
from lib.autoscaler_state_store import AutoscalerStateError, GcsStateStore
from lib.gcs_control_state import GcsAmbiguous, GcsError, GcsObject, GcsPreconditionFailed, ObjectInfo

URI = "gs://homelab-424523-spot-control-state/autoscaler/state-swack-tools-a7245ac03c.json"
OBJECT = "autoscaler/state-swack-tools-a7245ac03c.json"
RECEIPT = "oxidex-runners-uswest1-a-grks-tn3q3cvuy4y-" + "0" * 32 + "-deployment.json"


class FakeGcs:
    """Objects with generations; ``put`` honors ifGenerationMatch like the JSON API."""

    def __init__(self):
        self.objects: dict[str, tuple[bytes, int]] = {}
        self.next_generation = 100
        self.puts: list[tuple[str, int]] = []
        self.ambiguous: list[bool] = []  # per put: True = raise GcsAmbiguous after (or instead of) writing
        self.ambiguous_lands: list[bool] = []
        self.fail_put: Exception | None = None

    def write(self, name, data):
        self.next_generation += 1
        self.objects[name] = (data, self.next_generation)
        return self.next_generation

    def get(self, name, *, generation=None):
        if name not in self.objects:
            return None
        data, gen = self.objects[name]
        return GcsObject(data, gen)

    def stat(self, name):
        return ObjectInfo(self.objects[name][1]) if name in self.objects else None

    def put(self, name, data, *, if_generation_match):
        self.puts.append((name, if_generation_match))
        if self.fail_put is not None:
            raise self.fail_put
        current = self.objects.get(name, (b"", 0))[1]
        if current != if_generation_match:
            raise GcsPreconditionFailed(f"{name} is not at generation {if_generation_match}")
        if self.ambiguous and self.ambiguous.pop(0):
            if self.ambiguous_lands.pop(0):
                self.write(name, data)
            raise GcsAmbiguous("connection reset")
        return ObjectInfo(self.write(name, data))


def make_store(fake=None, read_only=False):
    fake = fake or FakeGcs()
    return GcsStateStore(URI, client=fake, read_only=read_only), fake


class UriTests(unittest.TestCase):
    def test_parses_a_json_object_uri(self):
        self.assertEqual(store_module.parse_uri(URI), ("homelab-424523-spot-control-state", OBJECT))

    def test_refuses_anything_else(self):
        for uri in ("", "gs://bucket", "gs://bucket/", "gs://bucket/state", "s3://bucket/state.json",
                    "gs://bucket/../state.json", "gs://bucket/a b.json", "/tmp/state.json"):
            with self.subTest(uri=uri), self.assertRaises(ValueError):
                store_module.parse_uri(uri)


class StoreTests(unittest.TestCase):
    def test_missing_object_loads_as_none_and_the_first_write_creates_it_only_if_absent(self):
        store, fake = make_store()
        self.assertIsNone(store.load())
        store.save(b'{"a": 1}')
        self.assertEqual(fake.puts, [(OBJECT, 0)])
        store.save(b'{"a": 2}')
        self.assertEqual(fake.puts[-1], (OBJECT, 101))
        self.assertEqual(fake.objects[OBJECT], (b'{"a": 2}', 102))

    def test_a_write_before_any_read_fails_closed(self):
        store, _ = make_store()
        with self.assertRaises(AutoscalerStateError):
            store.save(b"{}")
        with self.assertRaises(AutoscalerStateError):
            store.check()

    def test_another_writer_makes_this_store_fail_for_good(self):
        store, fake = make_store()
        fake.write(OBJECT, b'{"v": 1}')
        self.assertEqual(store.load(), b'{"v": 1}')
        fake.write(OBJECT, b'{"v": "other controller"}')
        with self.assertRaisesRegex(AutoscalerStateError, "another controller"):
            store.save(b'{"v": 2}')
        self.assertIn("another controller", store.failure)
        with self.assertRaisesRegex(AutoscalerStateError, "Fleet mutation held"):
            store.check()
        # Sticky: no later write is even attempted, so nothing overwrites the other controller's state.
        attempts = len(fake.puts)
        with self.assertRaises(AutoscalerStateError):
            store.save(b'{"v": 3}')
        self.assertEqual(len(fake.puts), attempts)
        self.assertEqual(fake.objects[OBJECT][0], b'{"v": "other controller"}')

    def test_an_ambiguous_write_that_landed_counts_as_written(self):
        store, fake = make_store()
        store.load()
        fake.ambiguous, fake.ambiguous_lands = [True], [True]
        store.save(b'{"ok": true}')
        self.assertIsNone(store.failure)
        store.save(b'{"ok": 2}')  # chained on the generation the read-back proved
        self.assertEqual(fake.objects[OBJECT][0], b'{"ok": 2}')

    def test_an_ambiguous_write_that_did_not_land_is_retried(self):
        store, fake = make_store()
        store.load()
        fake.ambiguous, fake.ambiguous_lands = [True, False], [False, False]
        store.save(b'{"ok": true}')
        self.assertEqual(fake.objects[OBJECT][0], b'{"ok": true}')
        self.assertEqual(len(fake.puts), 2)

    def test_writes_that_stay_unconfirmed_fail_closed(self):
        store, fake = make_store()
        store.load()
        fake.ambiguous = [True] * store_module.WRITE_ATTEMPTS
        fake.ambiguous_lands = [False] * store_module.WRITE_ATTEMPTS
        with self.assertRaisesRegex(AutoscalerStateError, "not confirmed"):
            store.save(b"{}")
        self.assertTrue(store.failure)

    def test_a_definitive_write_error_fails_closed(self):
        store, fake = make_store()
        store.load()
        fake.fail_put = GcsError("HTTP 403")
        with self.assertRaises(AutoscalerStateError):
            store.save(b"{}")
        self.assertIn("403", store.failure)

    def test_a_read_error_raises_instead_of_looking_empty(self):
        class Broken(FakeGcs):
            def get(self, name, *, generation=None):
                raise GcsError("HTTP 500")
        store, _ = make_store(Broken())
        with self.assertRaises(AutoscalerStateError):
            store.load()

    def test_a_read_only_store_never_writes(self):
        store, fake = make_store(read_only=True)
        store.load()
        with self.assertRaisesRegex(AutoscalerStateError, "read-only"):
            store.save(b"{}")
        self.assertEqual(fake.puts, [])


class ReceiptTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.dir = Path(directory.name)

    def test_receipts_are_mirrored_once_and_restored_when_missing(self):
        store, fake = make_store()
        receipt = self.dir / RECEIPT
        receipt.write_text('{"name": "vm"}')
        (self.dir / "state.json").write_text("{}")
        (self.dir / "other-deployment.json").write_text("{}")  # not a receipt name: never uploaded
        store.mirror_once(self.dir)
        store.mirror_once(self.dir)
        names = [name for name, _ in fake.puts]
        self.assertEqual(names, [store.receipt_object(RECEIPT)])
        self.assertEqual(store.receipt_object(RECEIPT),
                         "autoscaler/state-swack-tools-a7245ac03c/receipts/" + RECEIPT)
        receipt.unlink()
        self.assertTrue(store.restore_receipt(receipt))
        self.assertEqual(receipt.read_text(), '{"name": "vm"}')

    def test_restore_of_an_unknown_or_unsafe_name_does_nothing(self):
        store, _ = make_store()
        self.assertFalse(store.restore_receipt(self.dir / RECEIPT))
        self.assertFalse(store.restore_receipt(self.dir / "../escape-deployment.json"))
        self.assertFalse((self.dir / RECEIPT).exists())

    def test_a_read_only_store_does_not_mirror(self):
        store, fake = make_store(read_only=True)
        (self.dir / RECEIPT).write_text("{}")
        store.mirror_once(self.dir)
        self.assertEqual(fake.puts, [])

    def test_stop_mirror_copies_what_the_thread_has_not(self):
        store, fake = make_store()
        store.start_mirror(self.dir, interval=3600)
        (self.dir / RECEIPT).write_text("{}")
        store.stop_mirror(self.dir)
        self.assertEqual([name for name, _ in fake.puts], [store.receipt_object(RECEIPT)])


class AutoscalerIntegrationTests(unittest.TestCase):
    """read_scale_state / write_scale_state / the mutation fence with SPOT_AUTOSCALER_STATE_URI set."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name) / "state.json"
        self.fake = FakeGcs()
        self.addCleanup(store_module.reset)

    def configure(self, read_only=False):
        return store_module.configure({store_module.ENV_STATE_URI: URI}, read_only=read_only,
                                      factory=lambda uri, read_only: GcsStateStore(uri, client=self.fake,
                                                                                   read_only=read_only))

    def test_unset_uri_keeps_the_local_file(self):
        self.assertIsNone(store_module.configure({}))
        controller.write_scale_state(self.path, {"idle_loops": 3})
        self.assertEqual(controller.read_scale_state(self.path)["idle_loops"], 3)
        self.assertEqual(self.fake.puts, [])

    def test_state_round_trips_through_the_object(self):
        self.fake.write(OBJECT, json.dumps({"idle_loops": 4, "job_tracking": {"x": 1}}).encode())
        self.configure()
        state = controller.read_scale_state(self.path)
        self.assertEqual((state["idle_loops"], state["job_tracking"]), (4, {"x": 1}))
        state["idle_loops"] = 5
        controller.write_scale_state(self.path, state)
        self.assertEqual(json.loads(self.fake.objects[OBJECT][0]), state)
        self.assertEqual(json.loads(self.path.read_text()), state)  # the local copy too

    def test_a_missing_object_stops_a_live_check_but_not_a_dry_run(self):
        self.configure()
        with self.assertRaisesRegex(AutoscalerStateError, "seed it"):
            controller.read_scale_state(self.path)
        self.configure(read_only=True)
        self.assertEqual(controller.read_scale_state(self.path)["idle_loops"], 0)

    def test_an_unreadable_object_is_never_replaced_by_an_empty_state(self):
        self.fake.write(OBJECT, b"not json")
        self.configure()
        with self.assertRaisesRegex(AutoscalerStateError, "not JSON"):
            controller.read_scale_state(self.path)
        self.fake.write(OBJECT, b"[1, 2]")
        with self.assertRaisesRegex(AutoscalerStateError, "not a JSON object"):
            controller.read_scale_state(self.path)

    def test_a_conflicting_write_raises_from_write_scale_state(self):
        self.fake.write(OBJECT, b"{}")
        self.configure()
        state = controller.read_scale_state(self.path)
        self.fake.write(OBJECT, b'{"other": true}')
        with self.assertRaises(AutoscalerStateError):
            controller.write_scale_state(self.path, state)

    def test_a_missing_receipt_is_restored_from_the_mirror_before_import(self):
        store = SimpleNamespace(restored=[], restore_receipt=lambda path: store.restored.append(path) or False)
        receipt = self.path.parent / RECEIPT
        state = {"host_operation": {"kind": "launch", "deployment_receipt": str(receipt), "vm": {}}}
        with patch.object(store_module, "_STORE", store):
            self.assertFalse(controller.import_deployment_receipt(state, self.path))
        self.assertEqual(store.restored, [receipt])

    def test_the_floor_lease_guard_also_proves_the_state_is_durable(self):
        self.fake.write(OBJECT, b"{}")
        store = self.configure()
        store.load()
        calls = []
        holder = SimpleNamespace(acquire=lambda: True, pending_operation=lambda: None, start=lambda: None,
                                 guard=lambda: calls.append("lease"), release=lambda: calls.append("release"),
                                 orphaned=None, lost=None)
        args = controller.args_parser().parse_args(["--state-file", str(self.path), "--once"])
        with patch.dict("os.environ", {"SPOT_RUNNER_FLOOR_STATE_URI": "gs://bucket/runner-floor"}), \
                patch("lib.runner_floor.AutoscalerTargetLease.for_target", return_value=holder), \
                patch("lib.runner_floor.install_mutation_fence") as fence:
            self.assertIs(controller.acquire_floor_lease(args), holder)
            try:
                controller._FLOOR_LEASE_GUARD()
                self.assertEqual(calls, ["lease"])
                fence.assert_called_with(controller._FLOOR_LEASE_GUARD)
                store.failure = "changed since generation 7"
                with self.assertRaisesRegex(AutoscalerStateError, "Fleet mutation held"):
                    controller._FLOOR_LEASE_GUARD()
                with self.assertRaises(AutoscalerStateError):
                    controller.guard_inventory_fresh({"runner_inventory_observed_at": controller.time.time()})
            finally:
                controller.release_floor_lease(holder)
        self.assertIsNone(controller._FLOOR_LEASE_GUARD)

    def test_live_cloud_storage_state_needs_the_floor_lease(self):
        self.configure()
        args = controller.args_parser().parse_args(["--state-file", str(self.path), "--once"])
        with patch.dict("os.environ", {"SPOT_RUNNER_FLOOR_STATE_URI": ""}), \
                patch.object(controller, "_run_controller", side_effect=AssertionError("must not run")):
            with self.assertRaisesRegex(RuntimeError, "SPOT_RUNNER_FLOOR_STATE_URI"):
                controller.run_controller(args)

    def test_a_check_whose_state_stopped_being_durable_exits_nonzero(self):
        store = self.configure()
        args = controller.args_parser().parse_args(["--state-file", str(self.path), "--once"])

        def check(_args):
            store.failure = "write failed: HTTP 503"
            return 0
        with patch.dict("os.environ", {"SPOT_RUNNER_FLOOR_STATE_URI": "gs://bucket/runner-floor"}), \
                patch.object(controller, "acquire_floor_lease", return_value=None), \
                patch.object(controller, "_run_controller", side_effect=check):
            self.assertEqual(controller.run_controller(args), 1)


if __name__ == "__main__":
    unittest.main()
