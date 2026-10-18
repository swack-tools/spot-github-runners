"""replace-unmanaged: preflight refusals, a read-only preview, and the make-before-break ordering (the managed floor
host is online before the old host is cordoned, drained, deregistered, and deleted), with every rollback path.

Fakes only: the in-memory Cloud Storage server, fake GitHub and Compute Engine, and fake cordon/drain calls.
"""
from __future__ import annotations

import argparse
import io
import json
import os
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from lib import runner_floor as rf
from lib import runner_floor_replace as rp
from lib.runner_fleet import GitHubRunner
from test_runner_floor import NOW, PROJECT, TARGETS_TEXT, World, host

OLD = "newznab-spot-runners-asia"
OLD_LABELS = ("self-hosted", "Linux", "X64", "ubuntu-latest", "spot", "n4-highcpu-16", "asia-east2")
TARGET = "xbmc4lyfe/newznab-tmux"


def old_host(instance_id="42", status="RUNNING"):
    return rf.Host(OLD, "asia-east2-a", status, instance_id, "n4-highcpu-16", True, "", False, "",
                   ({"source": f"https://x/disks/{OLD}", "boot": True},))


def old_runners(busy=False):
    return [GitHubRunner(100 + i, f"newznab-asia-{i}", "online", busy, OLD_LABELS) for i in range(1, 5)]


class PreflightTests(unittest.TestCase):
    def check(self, *, t=None, old=None, names=None, runners=None, hosts=(), record=None, accept=True):
        target = t or World().config.target(TARGET)
        return rp.preflight(target, old_host() if old is None else old,
                            [r.name for r in old_runners()] if names is None else names,
                            old_runners() if runners is None else runners, list(hosts), record or {},
                            accept_dropped=accept, instance=OLD)

    def test_the_newznab_replacement_passes(self):
        out = self.check()
        self.assertEqual(out.problems, [])
        self.assertEqual(out.required_labels, ["spot", "ubuntu-latest"])
        self.assertEqual(out.dropped_labels, ["asia-east2", "n4-highcpu-16"])
        self.assertEqual(len(out.old_runners), 4)
        self.assertTrue(out.steps[0].startswith("1. ensure the managed floor host"))
        self.assertIn("cordon", out.steps[2])
        self.assertIn("delete", out.steps[5])

    def test_refusals(self):
        world = World()
        cases = {
            "repository targets only": dict(t=world.config.target("Appz4Fun")),
            "managed host": dict(old=rf.Host("oxidex-runners-x", "asia-east2-a", "RUNNING", "1", "n4-highcpu-16")),
            "--old-runner": dict(names=[]),
            "is not registered": dict(names=["nope"]),
            "--accept-dropped-labels": dict(accept=False),
        }
        for why, kwargs in cases.items():
            with self.subTest(why=why):
                self.assertTrue(any(why in p for p in self.check(**kwargs).problems), self.check(**kwargs).problems)
        no_extra = World("xbmc4lyfe/newznab-tmux:secret=s,account=xbmc4lyfe").config.target(TARGET)
        problems = self.check(t=no_extra).problems
        self.assertTrue(any("labels=ubuntu-latest" in p for p in problems), problems)
        missing = rp.preflight(world.config.target(TARGET), None, ["x"], [], [], {}, accept_dropped=True,
                               instance=OLD)
        self.assertTrue(any("was not found" in p for p in missing.problems))
        busy_elsewhere = {"replacement": {"old": {"name": "other-vm"}, "phase": "draining"}}
        self.assertTrue(any("another replacement" in p for p in self.check(record=busy_elsewhere).problems))

    def test_named_runners_must_belong_to_the_old_vm(self):
        stranger = GitHubRunner(999, "laptop-runner", "online", False, ("self-hosted", "spot", "ubuntu-latest"))
        out = self.check(names=["laptop-runner"], runners=[*old_runners(), stranger])
        self.assertTrue(any("doesn't look like a runner of" in p for p in out.problems), out.problems)

    def test_candidates_help_name_the_old_runners(self):
        out = self.check(names=[])
        self.assertEqual(out.candidates, [f"newznab-asia-{i}" for i in range(1, 5)])


class ReplaceWorld(World):
    def __init__(self, text=TARGETS_TEXT):
        super().__init__(text)
        self.hosts = [old_host()]
        self.runners[TARGET] = old_runners()
        self.target = self.config.target(TARGET)
        self.store_ = rf.TargetStore(self.client, self.location, PROJECT, self.target, wall=lambda: self.clock[0])
        self.store_.load()
        self.lease_ = self.lease(self.store_.lock_name, owner="replace:mac:1")
        assert self.lease_.acquire()
        self.store_.record["replacement"] = {
            "old": {"name": OLD, "zone": "asia-east2-a", "instance_id": "42"},
            "old_runner_names": [r.name for r in old_runners()], "required_labels": ["spot", "ubuntu-latest"],
            "phase": "ensure_managed", "started_at": NOW}
        self.store_.save(self.lease_)
        self.cordon_result = ("placed", {})
        self.drain_results: list[bool] = []
        self.out: list[str] = []

    def cordon(self, vm, args, token, names, guard):
        guard()
        self.log.append(("cordon", vm.name, vm.instance_id, tuple(names)))
        return self.cordon_result

    def drain(self, operation, vm, args, state, save, assigned, guard):
        guard()
        self.log.append(("drain", vm.name, tuple(r.name for r in assigned)))
        done = self.drain_results.pop(0) if self.drain_results else False
        if done == "deregister":
            operation.setdefault("deregistered_runner_ids", []).extend(r.runner_id for r in assigned)
            self.runners[TARGET] = [r for r in self.runners[TARGET] if not r.name.startswith("newznab-asia-")]
            return False
        return done

    def release(self, vm, args, token, guard):
        self.log.append(("release_cordon", vm.name, token))
        return True

    def replacement(self, **flags):
        args = argparse.Namespace(project=PROJECT, instance=OLD, label_strip_drain=False, max_wait_seconds=10_000,
                                  poll_seconds=30, **flags)
        return rp.Replacement(args, self.config, self.target, store=self.store_, lease=self.lease_,
                              inventory=self.provider, github=self.github, provider=self.provider,
                              drain=self.drain, cordon=self.cordon, release_cordon=self.release,
                              clock=lambda: self.clock[0], sleep=lambda s: self.clock.__setitem__(0, self.clock[0] + s),
                              out=self.out.append)

    def managed_name(self):
        op = self.store_.record.get("operation") or {}
        return op.get("name") or self.store_.record["host"]["name"]

    def bring_managed_online(self, rep):
        self.assertIsNone(rep.step())  # launch
        name = self.managed_name()
        self.bootstrap(name)
        self.assertIsNone(rep.step())  # finish_launch
        self.assertIsNone(rep.step())  # verified
        return name

    def assertIsNone(self, value):
        assert value is None, value

    def kinds(self):
        return [entry[0] if entry[0] in ("cordon", "drain", "release_cordon") else entry[1] for entry in self.log]


class ReplacementTests(unittest.TestCase):
    def test_make_before_break_order(self):
        world = ReplaceWorld()
        rep = world.replacement()
        name = world.bring_managed_online(rep)
        self.assertEqual(world.store_.record["replacement"]["phase"], "verified")
        self.assertEqual(world.store_.record["replacement"]["managed"]["name"], name)
        kinds = world.kinds()
        self.assertNotIn("cordon", kinds)  # nothing touched the old host before the managed host was ready
        self.assertIsNone(rep.step())  # draining; original labels recorded
        self.assertEqual(world.store_.record["replacement"]["original_labels"]["newznab-asia-1"],
                         ["asia-east2", "n4-highcpu-16", "spot", "ubuntu-latest"])
        self.assertIsNone(rep.step())  # cordon placed -> mode cordon
        world.drain_results = [False, "deregister", True]
        for _ in range(3):
            self.assertIsNone(rep.step())
        self.assertEqual(world.store_.record["replacement"]["phase"], "deleting")
        self.assertIsNone(rep.step())  # delete the same instance
        self.assertEqual(rep.step(), 0)  # gone: done
        kinds = world.kinds()
        self.assertLess(kinds.index("record_no_patch"), kinds.index("cordon"))
        self.assertLess(kinds.index("cordon"), kinds.index("drain"))
        self.assertLess(kinds.index("drain"), kinds.index("delete_instance"))
        deletes = [e for e in world.log if e[1] == "delete_instance"]
        self.assertEqual(deletes, [("provider", "delete_instance", OLD, "42", "keep")])  # only auto-delete disks
        self.assertEqual(world.store_.record["replacement"]["phase"], "done")
        # The managed floor host was never touched by the retirement.
        self.assertTrue(any(h.name == name and h.running for h in world.hosts))

    def test_managed_host_lost_before_deregistration_aborts_and_restores(self):
        world = ReplaceWorld()
        rep = world.replacement()
        name = world.bring_managed_online(rep)
        rep.step()  # draining
        rep.step()  # cordon placed
        # drain_runners strips the custom labels...
        world.runners[TARGET] = [GitHubRunner(r.runner_id, r.name, r.status, r.busy, ("self-hosted",))
                                 if r.name.startswith("newznab-asia-") else r for r in world.runners[TARGET]]
        # ...then the managed host is preempted.
        world.hosts = [h for h in world.hosts if h.name != name]
        self.assertEqual(rep.step(), 1)
        self.assertEqual(world.store_.record["replacement"]["phase"], "aborted")
        restored = [e for e in world.log if e[1] == "set_labels"]
        self.assertEqual(len(restored), 4)
        self.assertEqual(restored[0][3], ("asia-east2", "n4-highcpu-16", "spot", "ubuntu-latest"))
        self.assertIn(("release_cordon", OLD, "replace-42"), world.log)
        kinds = world.kinds()
        self.assertLess(kinds.index("release_cordon"), kinds.index("set_labels"))  # the cordon goes first
        self.assertFalse([e for e in world.log if e[1] == "delete_instance"])

    def test_an_unconfirmed_cordon_release_keeps_the_labels_off_and_retries(self):
        world = ReplaceWorld()
        rep = world.replacement()
        name = world.bring_managed_online(rep)
        rep.step()
        rep.step()
        world.hosts = [h for h in world.hosts if h.name != name]
        released = [False]
        world.release = lambda vm, args, token, guard: (world.log.append(("release_cordon", vm.name, token)),
                                                        released[0])[1]
        rep.release_cordon = world.release
        self.assertIsNone(rep.step())
        self.assertEqual(world.store_.record["replacement"]["phase"], "aborting")
        self.assertFalse([e for e in world.log if e[1] == "set_labels"])
        released[0] = True
        self.assertEqual(rep.step(), 1)
        self.assertEqual(world.store_.record["replacement"]["phase"], "aborted")
        self.assertEqual(len([e for e in world.log if e[1] == "set_labels"]), 0)  # labels were never stripped here

    def test_managed_host_lost_after_deregistration_waits_and_never_deletes(self):
        world = ReplaceWorld()
        rep = world.replacement()
        name = world.bring_managed_online(rep)
        rep.step()
        rep.step()
        world.drain_results = ["deregister"]
        rep.step()
        world.hosts = [h for h in world.hosts if h.name != name]
        self.assertIsNone(rep.step())
        self.assertEqual(world.store_.record["replacement"]["phase"], "draining")
        self.assertFalse([e for e in world.log if e[1] in ("delete_instance", "set_labels")])

    def test_an_uncordonable_host_holds_unless_label_strip_is_allowed(self):
        world = ReplaceWorld()
        world.cordon_result = ("unsupported", {"reason": "runner container has no read-only host cordon mount"})
        rep = world.replacement()
        world.bring_managed_online(rep)
        rep.step()
        self.assertEqual(rep.step(), 1)
        self.assertEqual(world.store_.record["replacement"]["phase"], "held")
        self.assertFalse([e for e in world.log if e[1] in ("DELETE", "delete_runner", "delete_instance")])

        world = ReplaceWorld()
        world.cordon_result = ("unsupported", {"reason": "no hook"})
        rep = world.replacement()
        rep.args.label_strip_drain = True
        world.bring_managed_online(rep)
        rep.step()  # draining
        rep.step()  # mode label_strip
        self.assertEqual(world.store_.record["replacement"]["mode"], "label_strip")
        rep.step()  # strip labels
        stripped = [e for e in world.log if e[1] == "DELETE" and e[2].endswith("/labels")]
        self.assertEqual(len(stripped), 4)
        rep.step()  # idle since now
        rep.step()  # not idle long enough
        self.assertFalse([e for e in world.log if e[1] == "delete_runner"])
        world.clock[0] += rp.IDLE_CONFIRM_SECONDS
        rep.step()  # deregister after fresh idle reads
        self.assertEqual(len([e for e in world.log if e[1] == "delete_runner"]), 4)
        rep.step()  # fresh inventory proves them gone -> deleting
        self.assertEqual(world.store_.record["replacement"]["phase"], "deleting")

    def test_busy_old_runners_reset_the_idle_window(self):
        world = ReplaceWorld()
        rep = world.replacement()
        rep.args.label_strip_drain = True
        world.cordon_result = ("unsupported", {})
        world.bring_managed_online(rep)
        for _ in range(4):
            rep.step()  # draining, mode, strip, idle_since
        world.runners[TARGET] = [GitHubRunner(r.runner_id, r.name, r.status, True, r.labels)
                                 if r.name == "newznab-asia-2" else r for r in world.runners[TARGET]]
        world.clock[0] += rp.IDLE_CONFIRM_SECONDS
        rep.step()
        self.assertIsNone(world.store_.record["replacement"]["idle_since"])
        self.assertFalse([e for e in world.log if e[1] == "delete_runner"])

    def test_a_recreated_old_vm_stops_everything(self):
        world = ReplaceWorld()
        world.hosts = [old_host(instance_id="43")]
        self.assertEqual(world.replacement().step(), 1)
        self.assertFalse([e for e in world.log if e[0] == "provider" and e[1] != "offers"])

    def test_delete_waits_for_the_registrations_to_disappear(self):
        world = ReplaceWorld()
        rep = world.replacement()
        world.bring_managed_online(rep)
        world.store_.record["replacement"]["phase"] = "deleting"
        self.assertIsNone(rep.step())
        self.assertFalse([e for e in world.log if e[1] == "delete_instance"])
        self.assertIn("waiting for", world.out[-1])

    def test_every_action_runs_behind_the_lease(self):
        world = ReplaceWorld()
        rep = world.replacement()
        world.bring_managed_online(rep)
        world.server.objects.pop(world.lease_.name)  # broken while we slept
        world.clock[0] += 200
        self.assertEqual(rep.run(), rp.EXIT_RESUMABLE)
        self.assertNotIn("cordon", world.kinds())


class ResumeTests(unittest.TestCase):
    def test_a_recorded_replacement_resumes_after_its_runners_are_gone(self):
        world = ReplaceWorld()
        world.store_.record["replacement"]["phase"] = "deleting"
        world.store_.save(world.lease_)
        world.lease_.release()
        world.runners[TARGET] = []  # already deregistered: a fresh preflight would refuse
        args = argparse.Namespace(project=PROJECT, target=TARGET, instance=OLD, zone="asia-east2-a",
                                  old_runner=[r.name for r in old_runners()], accept_dropped_labels=True,
                                  label_strip_drain=False, github_user="", apply=False, max_wait_seconds=1,
                                  poll_seconds=1)

        class Tokens:
            def __init__(self, *a, **k):
                pass

            def __call__(self, target):
                return "gho_x"
        out = io.StringIO()
        with patch.object(rf, "TokenSource", Tokens), patch.object(rf, "GitHubClient", lambda token, **k: world.github), \
                patch.object(rf, "ComputeInventory", lambda project: world.provider), \
                patch.object(rp, "GcsClient", lambda bucket: world.client), redirect_stdout(out):
            self.assertEqual(rp.run_replace(args, world.config), 0)
        self.assertIn("resume phase deleting", out.getvalue())
        self.assertNotIn("Refused", out.getvalue())


class PreviewTests(unittest.TestCase):
    def test_the_preview_changes_nothing(self):
        world = World()
        world.hosts = [old_host()]
        world.runners[TARGET] = old_runners()
        args = argparse.Namespace(project=PROJECT, target=TARGET, instance=OLD, zone="asia-east2-a",
                                  old_runner=[r.name for r in old_runners()], accept_dropped_labels=True,
                                  label_strip_drain=False, github_user="", apply=False, max_wait_seconds=1,
                                  poll_seconds=1)

        class Tokens:
            def __init__(self, *a, **k):
                pass

            def __call__(self, target):
                return "gho_x"
        out = io.StringIO()
        with patch.object(rf, "TokenSource", Tokens), patch.object(rf, "GitHubClient", lambda token, **k: world.github), \
                patch.object(rf, "ComputeInventory", lambda project: world.provider), \
                patch.object(rp, "GcsClient", lambda bucket: world.client), patch.dict(os.environ, {}, clear=False), \
                redirect_stdout(out):
            os.environ.pop("GH_TOKEN", None)
            code = rp.run_replace(args, world.config)
            self.assertNotIn("GH_TOKEN", os.environ)
        self.assertEqual(code, 0, out.getvalue())
        self.assertIn("Preview only; nothing changed", out.getvalue())
        report = json.loads(out.getvalue().split("\nPreview")[0])
        self.assertEqual(report["dropped_labels"], ["asia-east2", "n4-highcpu-16"])
        self.assertEqual(world.server.writes("POST") + world.server.writes("DELETE"), [])
        self.assertFalse([e for e in world.log if e[0] == "provider" or e[1] in ("registration_token", "DELETE")])
        args.accept_dropped_labels = False
        with patch.object(rf, "TokenSource", Tokens), patch.object(rf, "GitHubClient", lambda token, **k: world.github), \
                patch.object(rf, "ComputeInventory", lambda project: world.provider), \
                patch.object(rp, "GcsClient", lambda bucket: world.client), redirect_stdout(io.StringIO()):
            self.assertEqual(rp.run_replace(args, world.config), 1)


if __name__ == "__main__":
    unittest.main()
