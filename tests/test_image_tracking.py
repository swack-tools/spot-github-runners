"""Dashboard image rollout (SPOT_DASHBOARD_IMAGE_TRACK=main) and the control VM restart policy.

Registry, GitHub, gcloud, KV, and HTTP all go through in-memory fakes; nothing touches GHCR, GitHub, GCP, or
Cloudflare.
"""
from __future__ import annotations

import base64
import hashlib
import io
import json
import tempfile
import unittest
import unittest.mock
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import test_control_vm as tcv
from lib import control_vm as cv
from lib import image_tracking as it
from lib.https_transport import HttpResponse, TransportError

REPO = cv.DEFAULT_DASHBOARD_IMAGE_REPO
D_A, D_B, D_C = ("sha256:" + c * 64 for c in "abc")
IMG_A, IMG_B, IMG_C = (f"{REPO}@{d}" for d in (D_A, D_B, D_C))
SHA_A, SHA_B, SHA_C = (c * 40 for c in "123")


def tearDownModule():
    tcv._CONTROL_STATE_DIR.cleanup()  # imported for its harness; idempotent with its own tearDownModule


# --------------------------------------------------------------------------------------------
# Pure decision
# --------------------------------------------------------------------------------------------

def facts(**overrides):
    values = dict(transition_active=False, active_ready=True, cooling=False, active_digest=D_A,
                  active_revision=SHA_A, candidate_digest=D_B, candidate_revision=SHA_B, failed_digests=(),
                  workflow="success", ancestry="ahead", repository=REPO)
    values.update(overrides)
    return values


class DecisionTests(unittest.TestCase):
    def test_newer_digest_rolls(self):
        decision = it.decide(**facts())
        self.assertTrue(decision.roll)
        self.assertEqual((decision.image, decision.revision, decision.from_revision), (IMG_B, SHA_B, SHA_A))

    def test_every_guard_refuses(self):
        cases = {
            "a transition is in progress": dict(transition_active=True),
            "not healthy": dict(active_ready=False),
            "cooldown": dict(cooling=True),
            "could not be resolved": dict(candidate_digest=None),
            "active image is main": dict(candidate_digest=D_A),
            "aborted earlier": dict(failed_digests=[D_B]),
            "commit is unknown": dict(active_revision=None),
            "has no org.opencontainers.image.revision": dict(candidate_revision=None),
            "is failure": dict(workflow="failure"),
            "is pending": dict(workflow="pending"),
            "is missing": dict(workflow="missing"),
            "older than the active image": dict(ancestry="behind"),
            "the same commit": dict(ancestry="identical"),
            "not a descendant": dict(ancestry="diverged"),
            "of unknown order": dict(ancestry=None),
        }
        for reason, override in cases.items():
            decision = it.decide(**facts(**override))
            self.assertFalse(decision.roll, override)
            self.assertIn(reason, decision.reason, override)
            self.assertIsNone(decision.image)

    def test_guard_order_puts_gates_first(self):
        # A closed gate wins over everything else, so no network fact is needed while it is closed.
        decision = it.decide(**facts(transition_active=True, cooling=True, candidate_digest=None))
        self.assertIn("transition", decision.reason)

    def test_parse_image(self):
        self.assertEqual(it.parse_image(IMG_A, REPO), (D_A, None))
        self.assertEqual(it.parse_image(f"{REPO}:{SHA_A}", REPO), (None, SHA_A))
        for other in (f"{REPO}:main", "ghcr.io/other/image@" + D_A, f"docker.io/x/y@{D_A}", ""):
            self.assertEqual(it.parse_image(other, REPO), (None, None), other)

    def test_record_failed_is_bounded(self):
        cache = {}
        for i in range(it.FAILED_DIGESTS_KEPT + 5):
            it.record_failed(cache, f"sha256:{i:064d}")
        it.record_failed(cache, f"sha256:{0:064d}")
        self.assertEqual(len(cache["failed"]), it.FAILED_DIGESTS_KEPT)
        self.assertEqual(cache["failed"][-1], f"sha256:{0:064d}")


# --------------------------------------------------------------------------------------------
# Registry and GitHub clients
# --------------------------------------------------------------------------------------------

class FakeRegistryHttp:
    def __init__(self):
        self.calls = []
        config_a = json.dumps({"config": {"Labels": {it.REVISION_LABEL: SHA_A}}}).encode()
        self.blobs = {"sha256:" + hashlib.sha256(config_a).hexdigest(): config_a}
        config_digest = next(iter(self.blobs))
        manifest = json.dumps({"mediaType": "application/vnd.oci.image.manifest.v1+json",
                               "config": {"digest": config_digest}}).encode()
        self.manifest_digest = "sha256:" + hashlib.sha256(manifest).hexdigest()
        index = json.dumps({"mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [
            {"digest": "sha256:" + "f" * 64, "platform": {"os": "unknown", "architecture": "unknown"}},
            {"digest": self.manifest_digest, "platform": {"os": "linux", "architecture": "amd64"}}]}).encode()
        self.index_digest = "sha256:" + hashlib.sha256(index).hexdigest()
        self.manifests = {self.manifest_digest: manifest, self.index_digest: index, "main": index}
        self.lie_about_digest = False

    def __call__(self, method, url, headers, body, timeout):
        self.calls.append((url, dict(headers)))
        if url.startswith("https://ghcr.io/token?"):
            expected = base64.b64encode(b"x-access-token:pat").decode()
            assert headers["Authorization"] == f"Basic {expected}"
            assert "scope=repository%3Aswack-tools%2Fspot-dashboard%3Apull" in url
            return HttpResponse(200, {}, b'{"token": "registry-token"}')
        if url.startswith("https://ghcr.io/v2/swack-tools/spot-dashboard/manifests/"):
            assert headers["Authorization"] == "Bearer registry-token"
            ref = url.rsplit("/", 1)[1]
            body = self.manifests[ref]
            digest = "sha256:" + hashlib.sha256(body).hexdigest()
            return HttpResponse(200, {"docker-content-digest": ("sha256:" + "0" * 64) if self.lie_about_digest
                                      else digest}, body)
        if url.startswith("https://ghcr.io/v2/swack-tools/spot-dashboard/blobs/"):
            digest = url.rsplit("/", 1)[1]
            return HttpResponse(307, {"location": f"https://pkg-containers.githubusercontent.com/blob/{digest}"}, b"")
        if url.startswith("https://pkg-containers.githubusercontent.com/blob/"):
            assert "Authorization" not in headers, "the registry token must never follow a redirect"
            return HttpResponse(200, {}, self.blobs[url.rsplit("/", 1)[1]])
        raise AssertionError(url)


class RegistryClientTests(unittest.TestCase):
    def test_resolve_and_revision_through_an_index_and_a_redirect(self):
        http = FakeRegistryHttp()
        client = it.RegistryClient(REPO, "pat", transport=http)
        self.assertEqual(client.resolve("main"), http.index_digest)
        self.assertEqual(client.revision(http.index_digest), SHA_A)
        self.assertEqual(sum(1 for url, _ in http.calls if "/token?" in url), 1)  # token cached

    def test_digests_are_verified(self):
        http = FakeRegistryHttp()
        http.lie_about_digest = True
        with self.assertRaisesRegex(it.TrackingError, "differs from its bytes"):
            it.RegistryClient(REPO, "pat", transport=http).resolve("main")
        http = FakeRegistryHttp()
        http.manifests[D_C] = b"{}"  # bytes that don't hash to the requested digest
        with self.assertRaisesRegex(it.TrackingError, "does not hash"):
            it.RegistryClient(REPO, "pat", transport=http).manifest(D_C)

    def test_missing_label_and_failures(self):
        http = FakeRegistryHttp()
        unlabeled = json.dumps({"config": {"Labels": {}}}).encode()
        config_digest = "sha256:" + hashlib.sha256(unlabeled).hexdigest()
        http.blobs[config_digest] = unlabeled
        manifest = json.dumps({"config": {"digest": config_digest}}).encode()
        digest = "sha256:" + hashlib.sha256(manifest).hexdigest()
        http.manifests[digest] = manifest
        self.assertIsNone(it.RegistryClient(REPO, "pat", transport=http).revision(digest))

        def down(*a):
            raise TransportError("boom")
        with self.assertRaises(it.TrackingError):
            it.RegistryClient(REPO, "pat", transport=down).resolve("main")
        with self.assertRaises(ValueError):
            it.RegistryClient("docker.io/x/y", "pat")


class GitHubClientTests(unittest.TestCase):
    def client(self, runs=None, compare=None, status=200):
        calls = []

        def http(method, url, headers, body, timeout):
            calls.append(url)
            assert headers["Authorization"] == "Bearer pat"
            if "/actions/workflows/dashboard-image.yml/runs?" in url:
                return HttpResponse(status, {}, json.dumps({"workflow_runs": runs or []}).encode())
            if "/compare/" in url:
                return HttpResponse(status, {}, json.dumps({"status": compare}).encode())
            raise AssertionError(url)
        return it.GitHubClient("swack-tools/spot-github-runners", "pat", transport=http), calls

    def run(self, conclusion, status="completed", branch="main", sha=SHA_B):
        return {"head_sha": sha, "head_branch": branch, "status": status, "conclusion": conclusion}

    def test_workflow_verdicts(self):
        cases = [([self.run("success")], "success"), ([self.run("failure"), self.run("success")], "success"),
                 ([self.run(None, status="in_progress")], "pending"), ([self.run("failure")], "failure"),
                 ([], "missing"), ([self.run("success", branch="feature")], "missing"),
                 ([self.run("success", sha=SHA_C)], "missing")]
        for runs, verdict in cases:
            client, calls = self.client(runs=runs)
            self.assertEqual(client.workflow_verdict(SHA_B), verdict, runs)
            self.assertIn(f"head_sha={SHA_B}", calls[0])
            self.assertIn("branch=main", calls[0])
        client, _ = self.client(status=500)
        with self.assertRaises(it.TrackingError):
            client.workflow_verdict(SHA_B)
        with self.assertRaises(it.TrackingError):
            client.workflow_verdict("main")

    def test_compare(self):
        for status in ("ahead", "behind", "identical", "diverged"):
            client, calls = self.client(compare=status)
            self.assertEqual(client.compare(SHA_A, SHA_B), status)
            self.assertIn(f"/compare/{SHA_A}...{SHA_B}", calls[0])
        client, _ = self.client(compare="weird")
        with self.assertRaises(it.TrackingError):
            client.compare(SHA_A, SHA_B)


# --------------------------------------------------------------------------------------------
# Manager integration
# --------------------------------------------------------------------------------------------

class FakeRegistry:
    def __init__(self):
        self.tags = {"main": D_B}
        self.revisions = {D_A: SHA_A, D_B: SHA_B, D_C: SHA_C}
        self.calls = []

    def resolve(self, tag):
        self.calls.append(("resolve", tag))
        return self.tags[tag]

    def revision(self, digest):
        self.calls.append(("revision", digest))
        return self.revisions.get(digest)


class FakeGitHub:
    def __init__(self):
        self.verdicts = {SHA_B: "success", SHA_C: "success"}
        self.order = {(SHA_A, SHA_B): "ahead", (SHA_A, SHA_C): "ahead", (SHA_B, SHA_C): "ahead"}
        self.calls = []

    def workflow_verdict(self, sha):
        self.calls.append(("workflow", sha))
        return self.verdicts.get(sha, "missing")

    def compare(self, base, head):
        self.calls.append(("compare", base, head))
        return self.order.get((base, head), "behind")

    def set_status(self, sha, state, description, *, target_url=""):
        self.calls.append(("status", sha, state, description))
        if getattr(self, "status_error", None):
            raise it.TrackingError(self.status_error)


class WatchdogGcloud(tcv.FakeGcloud):
    """FakeGcloud plus instance metadata (dashboard-image, add-metadata), reset, and set-scheduling."""

    def __init__(self, events):
        super().__init__(events)
        self.images: dict[str, str] = {}
        self.metadata_rc = 0
        self.reset_rc = 0
        self.metadata_scripts: list[str] = []
        self.runs: dict[str, str] = {}  # guest attribute spot-dashboard/run by VM name

    def __call__(self, argv):
        verb = argv[2:4]
        if verb == ["instances", "get-guest-attributes"]:
            self.calls.append(list(argv))
            assert "--query-path=spot-dashboard/" in argv
            if argv[4] not in self.runs:
                return SimpleNamespace(returncode=1, stdout="", stderr="ERROR: The resource was not found")
            rows = [{"namespace": "spot-dashboard", "key": "run", "value": self.runs[argv[4]]}]
            return SimpleNamespace(returncode=0, stdout=json.dumps(rows), stderr="")
        if verb == ["instances", "add-metadata"]:
            self.calls.append(list(argv))
            items = dict(item.split("=", 1) for item in
                         next(a for a in argv if a.startswith("--metadata="))[len("--metadata="):].split(","))
            script = next(a.split("=", 2)[2] for a in argv if a.startswith("--metadata-from-file=startup-script="))
            self.metadata_scripts.append(Path(script).read_text(encoding="utf-8"))
            self.events.append(("set-image", argv[4], items["dashboard-image"]))
            if self.metadata_rc == 0:
                self.images[argv[4]] = items["dashboard-image"]
            return SimpleNamespace(returncode=self.metadata_rc, stdout="", stderr="boom" if self.metadata_rc else "")
        if verb == ["instances", "reset"]:
            self.calls.append(list(argv))
            self.events.append(("reset", argv[4]))
            return SimpleNamespace(returncode=self.reset_rc, stdout="", stderr="")
        if verb == ["instances", "set-scheduling"]:
            self.calls.append(list(argv))
            self.events.append(("set-scheduling", argv[4]))
            assert "--no-restart-on-failure" in argv and "--restart-on-failure" not in argv
            self.vms[argv[4]].setdefault("scheduling", {})["automaticRestart"] = False
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        if verb == ["instances", "describe"] and "--format=json(id,metadata.items)" in argv:
            self.calls.append(list(argv))
            row = self.vms.get(argv[4])
            if row is None:
                return SimpleNamespace(returncode=1, stdout="", stderr="was not found")
            items = [{"key": "dashboard-image", "value": self.images.get(argv[4], "")}]
            return SimpleNamespace(returncode=0, stdout=json.dumps({"id": row["id"], "metadata": {"items": items}}),
                                   stderr="")
        result = super().__call__(argv)
        if verb == ["instances", "create"] and result.returncode == 0:
            metadata = next(a for a in argv if a.startswith("--metadata="))[len("--metadata="):]
            self.images[argv[4]] = dict(item.split("=", 1) for item in metadata.split(","))["dashboard-image"]
        return result

    def create_image(self):
        create = [c for c in self.calls if c[2:4] == ["instances", "create"]][-1]
        metadata = next(a for a in create if a.startswith("--metadata="))[len("--metadata="):]
        return dict(item.split("=", 1) for item in metadata.split(","))["dashboard-image"]


class TrackingHarness(tcv.Harness):
    def __init__(self, **overrides):
        values = dict(offers=tcv.STANDARD_OFFERS, provisioning="standard", regions=("us-central1",),
                      image_track="main", image_track_token="pat")
        values.update(overrides)
        super().__init__(**values)
        self.gcloud = WatchdogGcloud(self.events)
        self.registry = FakeRegistry()
        self.github = FakeGitHub()

    def manager(self):
        tracker = it.ImageTracker(REPO, self.registry, self.github, lambda: self.clock[0])
        return cv.ControlVmManager(self.config, self.state, self.save, gcloud=self.gcloud, kv=self.kv,
                                   requester=self.http, offer_source=lambda: list(self.offers),
                                   clock=lambda: self.clock[0], image_tracker=tracker)

    def active(self, image=IMG_A, slot="a"):
        self.gcloud.add(slot, machine="e2-micro", provisioning="STANDARD")
        self.gcloud.images[f"spot-control-{slot}"] = image
        self.kv.set_origin(slot)
        self.http.healthy(slot, version=7)

    @property
    def image(self):
        return self.state.get(cv.STATE_KEY, {}).get("image", {})

    def swapped(self, image=IMG_B, slot="a", version=7):
        """The VM's image watcher replaced the dashboard container, which now reports ``image``. It starts frozen:
        the freeze marker the update set lives on the boot disk."""
        self.http.health[slot] = (503, {"status": "frozen", "hydrated": True, "frozen": True, "state_version": version,
                                        "slot": slot, "kv_writer": False, "image": image})

    def reported(self, outcome="done", slot="a"):
        """The startup run (or the watcher) reports the update the controller last requested."""
        expect = self.transition["expect_run"]
        detail = expect["image"] if outcome == "done" else str(int(self.clock[0]))
        self.gcloud.runs[f"spot-control-{slot}"] = f"{outcome} {expect['id']} {detail}"

    def finish(self):
        """Drive a started rollout to completion."""
        self.swapped()
        self.clock[0] += 180
        self.tick()


class RolloutTests(unittest.TestCase):
    def test_newer_main_digest_updates_the_image_in_place(self):
        h = TrackingHarness()
        h.active()
        plan = h.tick()
        self.assertEqual((plan.action, plan.target_slot), ("update_image", "a"))
        self.assertIn("image rollout", plan.reason)
        t = h.transition
        self.assertEqual((t["mode"], t["step"], t["dashboard_image"], t["from_image"]),
                         (cv.MODE_IMAGE, "wait_image", IMG_B, IMG_A))
        self.assertEqual(t["image_rollout"], {"image": IMG_B, "digest": D_B, "revision": SHA_B,
                                              "from_revision": SHA_A})
        self.assertEqual(t["new_vm"]["instance_id"], "id-a")
        self.assertEqual(h.gcloud.images["spot-control-a"], IMG_B)
        # The refreshed startup script installs the image watcher on the next boot of a VM that lacks it.
        self.assertEqual(h.gcloud.metadata_scripts, [cv.render_control_startup()])
        self.assertIn("install_image_watcher", h.gcloud.metadata_scripts[0])
        self.assertEqual(h.image["active"]["image"], IMG_A)  # recorded from a's metadata
        self.assertEqual(h.tick().action, "update_image")  # resumes; the old container still serves
        self.assertIsNotNone(h.transition)
        h.finish()
        self.assertIsNone(h.transition)
        # No second VM, cutover, or reset: drain (freeze), one metadata write, and release by importing the frozen
        # snapshot into the new container.
        self.assertEqual(h.mutations(), [("freeze", "a"), ("set-image", "spot-control-a", IMG_B), ("import", "a")])
        self.assertEqual(sorted(h.gcloud.vms), ["spot-control-a"])
        self.assertEqual(h.kv.origin_slot(), "a")
        self.assertEqual((h.image["active"]["image"], h.image["active"]["slot"], h.image["active"]["instance_id"]),
                         (IMG_B, "a", "id-a"))
        self.assertEqual(h.state[cv.STATE_KEY]["history"][-1]["mode"], cv.MODE_IMAGE)
        # Up to date afterwards, and the cooldown keeps the registry quiet meanwhile.
        h.registry.calls.clear()
        self.assertIn("cooldown", h.tick().reason)
        self.assertEqual(h.registry.calls, [])
        h.clock[0] += cv.ROTATION_COOLDOWN_SECONDS
        plan = h.tick()
        self.assertEqual(plan.action, "noop")
        self.assertIn("active image is main", plan.reason)

    def test_same_digest_needs_no_github_call(self):
        h = TrackingHarness()
        h.active()
        h.registry.tags["main"] = D_A
        plan = h.tick()
        self.assertEqual(plan.action, "noop")
        self.assertIn("active image is main", plan.reason)
        self.assertEqual(h.github.calls, [])
        self.assertEqual(h.mutations(), [])

    def test_older_or_identical_commit_never_rolls(self):
        for order, words in (("behind", "older than the active image"), ("identical", "the same commit"),
                             ("diverged", "not a descendant")):
            h = TrackingHarness()
            h.active()
            h.github.order[(SHA_A, SHA_B)] = order
            plan = h.tick()
            self.assertEqual(plan.action, "noop", order)
            self.assertIn(words, plan.reason)
            self.assertIsNone(h.transition)
            self.assertEqual(h.mutations(), [])
            # The order of two commits is final: no second compare call on the next check.
            before = len(h.github.calls)
            h.tick()
            self.assertEqual(len(h.github.calls), before)

    def test_failed_or_pending_workflow_waits_and_is_checked_again(self):
        h = TrackingHarness()
        h.active()
        h.github.verdicts[SHA_B] = "pending"
        self.assertIn("is pending", h.tick().reason)
        h.github.verdicts[SHA_B] = "failure"
        self.assertIn("is pending", h.tick().reason)  # cached inside the recheck interval
        h.clock[0] += it.VERDICT_RECHECK_SECONDS
        self.assertIn("is failure", h.tick().reason)
        self.assertIsNone(h.transition)
        h.github.verdicts[SHA_B] = "success"  # a re-run succeeded
        h.clock[0] += it.VERDICT_RECHECK_SECONDS
        self.assertEqual(h.tick().action, "update_image")

    def test_unhealthy_new_image_rolls_back_and_backs_off_that_digest(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.health["a"] = (503, {"status": "starting", "hydrated": False, "frozen": True, "state_version": 7,
                                    "slot": "a", "image": IMG_B})  # swapped, but the new image never gets ready
        h.clock[0] += cv.IMAGE_SWAP_GRACE_SECONDS
        h.tick()
        self.assertNotIn(("reset", "spot-control-a"), h.events, "the container did swap; a reset would not help")
        self.assertIn(f"reports image {IMG_B}", h.transition["error"])
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.tick()  # timed out: pin the previous image again; the watcher swaps back
        self.assertIn("to release it", h.transition["error"])
        self.assertNotIn(("import", "a"), h.events, "the failed image is never released")
        h.swapped(image=IMG_A)
        h.clock[0] += 180
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.mutations(), [("freeze", "a"), ("set-image", "spot-control-a", IMG_B),
                                         ("set-image", "spot-control-a", IMG_A), ("import", "a")])
        self.assertEqual(h.gcloud.images["spot-control-a"], IMG_A)
        self.assertEqual(h.image["failed"], [D_B])
        self.assertEqual(h.image["active"]["image"], IMG_A)
        self.assertEqual(sorted(h.gcloud.vms), ["spot-control-a"])  # never deleted
        self.assertEqual(h.kv.origin_slot(), "a")
        h.http.healthy("a", version=7, image=IMG_A)
        h.clock[0] += cv.ROTATION_COOLDOWN_SECONDS
        plan = h.tick()
        self.assertEqual(plan.action, "noop")
        self.assertIn("aborted earlier", plan.reason)
        h.registry.tags["main"] = D_C
        plan = h.tick()
        self.assertEqual(plan.action, "update_image")
        self.assertEqual(h.transition["dashboard_image"], IMG_C)
    def test_a_vm_without_the_image_watcher_is_reset_once(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.clock[0] += cv.IMAGE_SWAP_GRACE_SECONDS - 1
        h.tick()
        self.assertNotIn(("reset", "spot-control-a"), h.events)
        h.clock[0] += 1
        h.tick()  # the old container (no image in its health) still serves: no watcher on this VM
        self.assertEqual(h.events.count(("reset", "spot-control-a")), 1)
        h.http.unhealthy("a")  # rebooting
        h.clock[0] += 180
        h.tick()
        self.assertEqual(h.events.count(("reset", "spot-control-a")), 1, "never a second reset")
        h.finish()
        self.assertIsNone(h.transition)
        self.assertEqual(h.image["active"]["image"], IMG_B)

    def test_a_restored_container_behind_the_frozen_state_is_raised_by_the_import(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.gcloud.reset_rc = 0
        h.swapped(image=IMG_A, version=3)  # the old image came back behind the frozen version
        h.http.versions["a"] = 3
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.http.versions["a"], 7)
        self.assertEqual(h.mutations()[-1], ("import", "a"))
    def test_a_rollback_never_releases_the_failed_image_and_holds_for_an_operator(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.import_status = 500
        h.swapped()  # the new image runs, but its release keeps failing
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.tick()  # rollback: pin the previous image (still frozen, so no refreeze is needed)
        self.assertEqual(h.mutations()[-1], ("set-image", "spot-control-a", IMG_A))
        self.assertEqual(h.events.count(("freeze", "a")), 1)
        for _ in range(2):
            h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
            with self.assertLogs("spot_runner_autoscaler.control_vm", "ERROR"):
                h.tick()  # the watcher never swapped back: reachable, so never ended into a recovery
        self.assertIn("operator action needed", h.transition["error"])
        self.assertIn("still runs", h.transition["error"])
        h.swapped(image=IMG_A)
        h.http.import_status = 200
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.mutations()[-1], ("import", "a"))
    def test_an_unanswered_release_means_no_reset_and_a_refreeze_before_rollback(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.import_response_lost = True  # the release lands; its answer is lost
        h.swapped()
        h.clock[0] += cv.IMAGE_SWAP_GRACE_SECONDS
        h.tick()
        self.assertNotIn(("reset", "spot-control-a"), h.events)
        h.http.import_response_lost = False
        h.http.health["a"] = (503, dict(h.http.health["a"][1], status="starting", frozen=False))  # released, not ready
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.tick()
        mutations = h.mutations()
        self.assertLess(mutations.index(("freeze", "a"), 1), mutations.index(("set-image", "spot-control-a", IMG_A)))
    def test_a_refreeze_never_replaces_a_newer_snapshot(self):
        from lib.edge_state import build_snapshot
        h = TrackingHarness()
        manager = h.manager()
        t = {}
        self.assertTrue(manager._record_freeze(t, {"state_version": 7, "snapshot": build_snapshot({}, 7, "a", now=5)}))
        self.assertTrue(manager._record_freeze(t, {"state_version": 3, "snapshot": build_snapshot({}, 3, "a", now=6)}))
        self.assertEqual((t["frozen_version"], t["frozen_snapshot"]["state_version"]), (7, 7))
        self.assertFalse(manager._record_freeze({}, {"state_version": 7}))  # no snapshot: not a usable freeze

    def test_a_rollback_never_swaps_a_released_container_without_a_confirmed_refreeze(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.import_response_lost = True  # the release lands; its answer is lost
        h.swapped()
        h.clock[0] += 180
        h.tick()
        h.http.freeze_status = 503
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        for _ in range(cv.STEP_RETRY_LIMIT + 2):
            h.tick()
            h.clock[0] += 180
        self.assertEqual(h.transition["step"], "abort")
        self.assertNotIn(("set-image", "spot-control-a", IMG_A), h.events)
        h.http.freeze_status = 200
        h.tick()
        self.assertIn(("set-image", "spot-control-a", IMG_A), h.events)

    def test_an_unreachable_vm_ends_the_rollback_only_after_kv_holds_the_frozen_state(self):
        from lib.edge_state import KV_STATE_KEY, parse_snapshot
        h = TrackingHarness()
        h.active()
        h.tick()  # frozen at state_version 7
        h.kv.store.pop(KV_STATE_KEY, None)
        del h.http.health["a"]  # unreachable from here on
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.tick()  # rollback: pin the previous image, wait for the VM
        self.assertEqual(h.transition["step"], "abort")
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        with self.assertLogs("spot_runner_autoscaler.control_vm", "ERROR"):
            h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(parse_snapshot(h.kv.store[KV_STATE_KEY])["state_version"], 7)

    def test_a_vanished_vm_ends_the_rollback_only_after_kv_holds_the_frozen_state(self):
        from lib.edge_state import KV_STATE_KEY, parse_snapshot
        h = TrackingHarness()
        h.active()
        h.tick()
        h.kv.store.pop(KV_STATE_KEY, None)
        h.gcloud.vms.pop("spot-control-a")
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(parse_snapshot(h.kv.store[KV_STATE_KEY])["state_version"], 7)

    def test_a_container_is_released_only_once_its_startup_run_settled(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.gcloud.runs["spot-control-a"] = f"running {int(h.clock[0])}"
        h.swapped()
        h.clock[0] += 180
        h.tick()
        self.assertIn("a startup run is in progress", h.transition["error"])
        self.assertNotIn(("import", "a"), h.events)
        h.gcloud.runs["spot-control-a"] = f"done some-earlier-update {IMG_B}"  # an older request's report
        h.clock[0] += 180
        h.tick()
        self.assertIn("to report update", h.transition["error"])
        h.reported()
        h.clock[0] += 180
        h.tick()
        self.assertIsNone(h.transition)
        self.assertIn(("import", "a"), h.events)

    def test_a_rollback_waits_for_a_pending_startup_run(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.gcloud.runs["spot-control-a"] = f"running {int(h.clock[0])}"  # still pulling the new image
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.gcloud.runs["spot-control-a"] = f"running {int(h.clock[0])}"
        h.tick()  # rollback: the old container still serves, but the run may yet replace it
        self.assertEqual(h.transition["step"], "abort")
        self.assertIn("a startup run is in progress", h.transition["error"])
        self.assertNotIn(("import", "a"), h.events)
        h.reported()  # it followed the restored image
        h.clock[0] += 180
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.mutations()[-1], ("import", "a"))

    def test_a_rollback_never_releases_a_failed_image_that_a_late_run_started(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        forward = h.transition["expect_run"]["id"]
        h.gcloud.runs["spot-control-a"] = f"done {forward} {IMG_B}"  # a late run started the failed image after all
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.tick()  # rollback; the old image's health was observed, but the restore has not been applied
        self.assertEqual(h.transition["step"], "abort")
        self.assertIn("-restore", h.transition["error"])
        self.assertNotIn(("import", "a"), h.events)
        h.reported()  # the restore was applied
        h.swapped(image=IMG_B)  # but a fresh probe still shows B
        h.clock[0] += 180
        h.tick()
        self.assertNotIn(("import", "a"), h.events)
        h.swapped(image=IMG_A)
        h.clock[0] += 180
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.mutations()[-1], ("import", "a"))

    def test_an_oversized_snapshot_refuses_the_update_and_releases_the_dashboard(self):
        h = TrackingHarness()
        h.active()
        with unittest.mock.patch.object(cv, "IMAGE_SNAPSHOT_MAX_BYTES", 10):
            h.tick()
        self.assertIsNone(h.transition)
        self.assertNotIn("set-image", [e[0] for e in h.events])
        self.assertEqual(h.mutations(), [("freeze", "a"), ("unfreeze", "a")])
        self.assertEqual(h.image["failed"], [D_B])

    def test_an_oversized_refusal_retries_a_lost_unfreeze_without_capturing(self):
        h = TrackingHarness()
        h.active()
        h.http.unfreeze_status = None  # the rollback's unfreeze lands; its answer is lost
        with unittest.mock.patch.object(cv, "IMAGE_SNAPSHOT_MAX_BYTES", 10):
            h.tick()
            self.assertEqual(h.transition["step"], "abort")
            h.http.unfreeze_status = 200
            h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.events.count(("freeze", "a")), 1)
        self.assertEqual(h.events.count(("unfreeze", "a")), 2)

    def test_a_rollback_before_any_metadata_write_needs_no_run_report(self):
        h = TrackingHarness()
        h.active()
        h.http.freeze_status = None  # the freeze may have landed; its answers are lost
        h.gcloud.runs["spot-control-a"] = f"running {int(h.clock[0])}"  # would block a release after a write
        for _ in range(cv.STEP_RETRY_LIMIT):
            h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.mutations()[-1], ("unfreeze", "a"))

    def test_kv_at_an_equal_version_with_other_state_is_overwritten(self):
        from lib.edge_state import KV_STATE_KEY, build_snapshot, parse_snapshot
        h = TrackingHarness()
        manager = h.manager()
        frozen = build_snapshot({"records": ["done"]}, 7, "a", now=5)
        h.kv.store[KV_STATE_KEY] = json.dumps(build_snapshot({}, 7, "a", now=4))
        self.assertIsNone(manager._publish_snapshot(frozen))
        self.assertEqual(parse_snapshot(h.kv.store[KV_STATE_KEY])["state"], {"records": ["done"]})
        h.kv.store[KV_STATE_KEY] = json.dumps(build_snapshot({}, 8, "a", now=6))  # newer: left alone
        self.assertIsNone(manager._publish_snapshot(frozen))
        self.assertEqual(parse_snapshot(h.kv.store[KV_STATE_KEY])["state_version"], 8)

    def test_an_oversized_refreeze_is_kept_in_kv_and_imported_from_there(self):
        from lib.edge_state import KV_STATE_KEY, build_snapshot
        h = TrackingHarness()
        manager = h.manager()
        t = {}
        self.assertTrue(manager._record_freeze(t, {"state_version": 7, "snapshot": build_snapshot({}, 7, "a", now=5)}))
        big = build_snapshot({"blob": "x" * 64}, 9, "a", now=6)
        with unittest.mock.patch.object(cv, "IMAGE_SNAPSHOT_MAX_BYTES", 100):
            self.assertTrue(manager._record_freeze(t, {"state_version": 9, "snapshot": big}))
        self.assertEqual((t["snapshot_in_kv"], t["frozen_version"], t["frozen_snapshot"]["state_version"]), (9, 9, 7))
        self.assertEqual(json.loads(h.kv.store[KV_STATE_KEY])["state_version"], 9)
        h.http.versions["a"] = 0
        h.swapped(version=0)  # the restored container, frozen
        ok, detail = manager._release(t, "a")
        self.assertTrue(ok, detail)
        self.assertEqual(h.http.versions["a"], 9)  # imported the KV copy, not the older one in the state

    def test_a_retried_release_refreezes_first(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.import_response_lost = True  # the import lands; its answer is lost
        h.swapped()
        h.clock[0] += 180
        h.tick()
        h.http.import_response_lost = False
        h.clock[0] += 180
        h.tick()
        imports = [i for i, e in enumerate(h.events) if e == ("import", "a")]
        freezes = [i for i, e in enumerate(h.events) if e == ("freeze", "a")]
        self.assertEqual(len(imports), 2)
        self.assertTrue(any(imports[0] < f < imports[1] for f in freezes), h.events)
        self.assertIsNone(h.transition)

    def test_a_failed_import_into_a_stale_container_keeps_the_original_snapshot(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        original = h.transition["frozen_snapshot"]
        h.http.import_status = 500  # a definite failure: still frozen
        h.swapped()
        h.clock[0] += 180
        h.tick()
        h.http.import_status = 200
        h.clock[0] += 180
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual([e for e in h.events if e == ("freeze", "a")], [("freeze", "a")])  # no refreeze
        posted = [json.loads(c[3]) for c in h.http.calls if c[1].endswith("/api/control/import")]
        self.assertEqual(posted[-1], original)

    def test_a_lost_refreeze_answer_is_never_taken_as_no_release(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.import_response_lost = True  # the release lands
        h.swapped()
        h.clock[0] += 180
        h.tick()
        h.http.import_response_lost = False
        h.http.freeze_status = None  # the retry's refreeze lands (frozen again), but its snapshot is lost
        h.clock[0] += 180
        h.tick()
        self.assertTrue(h.transition["capture_pending"])
        h.http.health["a"] = (503, dict(h.http.health["a"][1], status="frozen", frozen=True))
        h.clock[0] += 180
        h.tick()  # frozen now, but that proves nothing: no import of the original snapshot over its state
        self.assertEqual(h.events.count(("import", "a")), 1)
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.tick()  # rollback: frozen, but a refreeze is unconfirmed, so no shortcut and no swap back yet
        self.assertNotIn(("set-image", "spot-control-a", IMG_A), h.events)
        h.http.freeze_status = 200
        h.tick()
        self.assertIn(("set-image", "spot-control-a", IMG_A), h.events)

    def test_a_newer_local_snapshot_replaces_the_kv_selector(self):
        from lib.edge_state import build_snapshot
        h = TrackingHarness()
        manager = h.manager()
        t = {"snapshot_in_kv": 9, "frozen_version": 9, "frozen_snapshot": build_snapshot({}, 7, "a", now=5)}
        self.assertTrue(manager._record_freeze(t, {"state_version": 10, "snapshot": build_snapshot({}, 10, "a", now=6)}))
        self.assertNotIn("snapshot_in_kv", t)
        self.assertEqual(t["frozen_snapshot"]["state_version"], 10)

    def test_preservation_never_overwrites_a_snapshot_kept_in_kv(self):
        from lib.edge_state import KV_STATE_KEY, build_snapshot
        h = TrackingHarness()
        manager = h.manager()
        h.kv.store[KV_STATE_KEY] = json.dumps(build_snapshot({"new": 1}, 9, "a", now=6))
        t = {"snapshot_in_kv": 9, "snapshot_in_kv_digest": cv.snapshot_digest(build_snapshot({"new": 1}, 9, "a", now=1)),
             "frozen_snapshot": build_snapshot({"old": 1}, 9, "a", now=5)}
        self.assertIsNone(manager._preserve_frozen_snapshot(t))
        self.assertEqual(json.loads(h.kv.store[KV_STATE_KEY])["state"], {"new": 1})
        h.kv.store[KV_STATE_KEY] = json.dumps(build_snapshot({}, 8, "a", now=7))
        self.assertIn("does not (yet) hold", manager._preserve_frozen_snapshot(t))

    def test_a_vanished_vm_without_a_captured_snapshot_holds_unless_kv_has_the_known_state(self):
        from lib.edge_state import KV_STATE_KEY, build_snapshot
        for kv_version, holds in ((3, True), (7, False)):
            h = TrackingHarness()
            h.active()
            h.http.freeze_status = None  # the freeze may have landed; its snapshot never arrives
            h.tick()
            h.kv.store[KV_STATE_KEY] = json.dumps(build_snapshot({}, kv_version, "a", now=1))
            h.gcloud.vms.pop("spot-control-a")
            with self.assertLogs("spot_runner_autoscaler.control_vm", "ERROR"):
                h.tick()
            if holds:
                self.assertIn("operator action needed", h.transition["error"])
            else:
                self.assertIsNone(h.transition)

    def test_a_target_that_starts_writable_is_frozen_and_captured_before_the_import(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.healthy("a", version=8, image=IMG_B)  # writable: its freeze handling regressed; took a change
        h.http.versions["a"] = 8
        h.clock[0] += 180
        h.tick()
        freezes = [i for i, e in enumerate(h.events) if e == ("freeze", "a")]
        imports = [i for i, e in enumerate(h.events) if e == ("import", "a")]
        self.assertEqual(len(freezes), 2)
        self.assertLess(freezes[1], imports[0])
        posted = [json.loads(c[3]) for c in h.http.calls if c[1].endswith("/api/control/import")]
        self.assertEqual(posted[-1]["state_version"], 8)  # its own current state, not the older frozen one

    def test_a_rollback_drains_a_pending_capture_before_the_swap_back(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.healthy("a", version=8, image=IMG_B)  # started writable
        h.http.versions["a"] = 8
        h.http.freeze_status = 409  # busy: the capture can't drain it yet
        h.clock[0] += 180
        h.tick()
        self.assertTrue(h.transition["capture_pending"])
        self.assertFalse(h.transition.get("release_attempted"))
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.tick()  # rollback: no swap back before a confirmed drain
        self.assertNotIn(("set-image", "spot-control-a", IMG_A), h.events)
        h.http.freeze_status = 200
        h.tick()
        self.assertIn(("set-image", "spot-control-a", IMG_A), h.events)
        self.assertEqual(h.transition["frozen_snapshot"]["state_version"], 8)  # what it accepted is kept

    def test_a_target_that_starts_writable_behind_the_frozen_state_holds(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.healthy("a", version=3, image=IMG_B)
        h.http.versions["a"] = 3
        h.clock[0] += 180
        h.tick()
        self.assertIn("operator action needed", h.transition["error"])
        self.assertNotIn(("import", "a"), h.events)

    def test_a_rollback_holds_a_container_that_diverged_below_the_frozen_state(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.healthy("a", version=3, image=IMG_B)  # started writable, behind the frozen 7
        h.http.versions["a"] = 3
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        for _ in range(2):
            with self.assertLogs("spot_runner_autoscaler.control_vm", "ERROR"):
                h.tick()
        self.assertIn("operator action needed", h.transition["error"])
        self.assertNotIn(("set-image", "spot-control-a", IMG_A), h.events)
        self.assertEqual(h.transition["frozen_snapshot"]["state_version"], 7)

    def test_a_release_imports_only_the_exact_snapshot_kept_in_kv(self):
        from lib.edge_state import KV_STATE_KEY, build_snapshot
        h = TrackingHarness()
        manager = h.manager()
        kept = build_snapshot({"records": ["done"]}, 9, "a", now=6)
        t = {"snapshot_in_kv": 9, "snapshot_in_kv_digest": cv.snapshot_digest(kept), "frozen_version": 9,
             "frozen_snapshot": build_snapshot({}, 7, "a", now=5)}
        h.kv.store[KV_STATE_KEY] = json.dumps(build_snapshot({}, 9, "a", now=7))  # same version, other state
        h.swapped(version=0)
        ok, detail = manager._release(t, "a")
        self.assertFalse(ok)
        self.assertIn("does not (yet) hold", detail)
        h.kv.store[KV_STATE_KEY] = json.dumps(kept)
        self.assertTrue(manager._release(t, "a")[0])

    def test_a_stale_running_report_does_not_block_forever(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.gcloud.runs["spot-control-a"] = f"running {int(h.clock[0]) - cv.STARTUP_RUN_STALE_SECONDS - 1}"
        h.swapped()
        h.clock[0] += 180
        h.tick()
        self.assertIsNone(h.transition)

    def test_a_vanished_vm_preserves_the_snapshot_even_after_a_release(self):
        from lib.edge_state import KV_STATE_KEY, parse_snapshot
        h = TrackingHarness()
        h.active()
        h.tick()
        h.kv.store.pop(KV_STATE_KEY, None)
        h.swapped()
        h.http.health["a"] = (503, {"status": "starting", "hydrated": True, "frozen": False, "state_version": 7,
                                    "slot": "a", "image": IMG_B})  # the import lands, but it never gets healthy
        h.clock[0] += 180
        h.tick()
        self.assertTrue(h.transition["released"])
        h.gcloud.vms.pop("spot-control-a")
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(parse_snapshot(h.kv.store[KV_STATE_KEY])["state_version"], 7)

    def test_an_unreachable_vm_that_cannot_be_refrozen_is_fenced_by_stop(self):
        from lib.edge_state import KV_STATE_KEY, parse_snapshot
        h = TrackingHarness()
        h.active()
        h.tick()
        h.kv.store.pop(KV_STATE_KEY, None)
        h.http.import_response_lost = True  # the release lands; its answer is lost
        h.swapped()
        h.clock[0] += 180
        h.tick()
        del h.http.health["a"]  # then the dashboard became unreachable
        h.http.freeze_status = None
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.tick()  # rollback: the refreeze can't reach it, so it waits
        self.assertEqual(h.transition["step"], "abort")
        self.assertNotIn(("stop", "spot-control-a"), h.events)
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        with self.assertLogs("spot_runner_autoscaler.control_vm", "ERROR"):
            h.tick()
        self.assertIn(("stop", "spot-control-a"), h.events)
        h.gcloud.status("a", "TERMINATED")
        with self.assertLogs("spot_runner_autoscaler.control_vm", "ERROR"):
            h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.gcloud.images["spot-control-a"], IMG_A)
        self.assertEqual(parse_snapshot(h.kv.store[KV_STATE_KEY])["state_version"], 7)

    def test_no_reset_while_a_startup_run_is_pulling(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.gcloud.runs["spot-control-a"] = f"running {int(h.clock[0])}"  # a slow pull on a VM with the watcher
        h.clock[0] += cv.IMAGE_SWAP_GRACE_SECONDS
        h.tick()
        self.assertNotIn(("reset", "spot-control-a"), h.events)

    def test_a_failed_fencing_stop_is_retried(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.import_response_lost = True
        h.swapped()
        h.clock[0] += 180
        h.tick()
        del h.http.health["a"]
        h.http.freeze_status = None
        h.gcloud.stop_rc = 1
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        h.tick()  # rollback begins; the refreeze can't reach it
        h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
        with self.assertLogs("spot_runner_autoscaler.control_vm", "ERROR"):
            h.tick()  # the first stop fails
        h.tick()  # inside the retry interval: no second stop yet
        self.assertEqual(h.events.count(("stop", "spot-control-a")), 1)
        h.gcloud.stop_rc = 0
        h.clock[0] += cv.STOP_RETRY_SECONDS
        with self.assertLogs("spot_runner_autoscaler.control_vm", "ERROR"):
            h.tick()
        self.assertEqual(h.events.count(("stop", "spot-control-a")), 2)

    def test_a_reset_that_reports_failure_is_never_repeated(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.gcloud.reset_rc = 1  # it may still have landed
        h.clock[0] += cv.IMAGE_SWAP_GRACE_SECONDS
        h.tick()
        h.clock[0] += 180
        h.tick()
        self.assertEqual(h.events.count(("reset", "spot-control-a")), 1)
        self.assertIsNotNone(h.transition["reset_at"])

    def test_a_new_container_behind_the_frozen_state_is_raised_by_the_import(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.swapped(version=3)
        h.http.versions["a"] = 3  # came up without changes the freeze drained
        h.clock[0] += 180
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.http.versions["a"], 7)  # the frozen snapshot, imported
        self.assertEqual(h.image["active"]["image"], IMG_B)

    def test_a_failed_import_is_never_success(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.http.import_status = 500
        h.swapped()
        h.clock[0] += 180
        h.tick()
        self.assertIn("import into slot a returned 500", h.transition["error"])
        self.assertEqual(h.transition["step"], "wait_image")
    def test_metadata_write_failures_abort_without_a_swap(self):
        h = TrackingHarness()
        h.active()
        h.gcloud.metadata_rc = 1
        for _ in range(cv.STEP_RETRY_LIMIT - 1):
            h.tick()
            self.assertEqual(h.transition["step"], "set_image")
        h.tick()  # the limit: abort; restoring the previous image also fails, so the rollback keeps trying
        self.assertEqual(h.transition["step"], "abort")
        self.assertIn("could not restore", h.transition["error"])
        h.gcloud.metadata_rc = 0
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.gcloud.images["spot-control-a"], IMG_A)
        self.assertEqual(h.image["failed"], [D_B])

    def test_a_vm_preempted_during_the_update_ends_it_at_once(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.gcloud.status("a", "TERMINATED")
        h.clock[0] += 180
        h.tick()  # no 20-minute wait: roll back now, start the VM, and unfreeze it once it is back
        self.assertEqual(h.transition["step"], "abort")
        self.assertEqual(h.gcloud.images["spot-control-a"], IMG_A)
        self.assertIn(("start", "spot-control-a"), h.events)
        h.gcloud.status("a", "RUNNING")
        h.swapped(image=IMG_A)  # booted the restored image, still frozen
        h.clock[0] += 180
        h.tick()
        self.assertIsNone(h.transition)
        self.assertEqual(h.mutations()[-1], ("import", "a"))
        self.assertEqual(h.image["failed"], [D_B])
        self.assertEqual(h.tick().action, "noop")

    def test_a_dashboard_that_does_not_freeze_is_never_updated(self):
        h = TrackingHarness()
        h.active()
        h.http.freeze_status = 503
        for _ in range(cv.STEP_RETRY_LIMIT):
            h.tick()
        self.assertIsNone(h.transition)
        self.assertNotIn("set-image", [e[0] for e in h.events])
        self.assertEqual(h.mutations()[-1], ("unfreeze", "a"))  # in case a freeze landed without its answer
        self.assertEqual(h.image["failed"], [D_B])

    def test_every_managed_key_is_written_so_cleared_settings_clear(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        call = next(c for c in h.gcloud.calls if c[2:4] == ["instances", "add-metadata"])
        items = dict(item.split("=", 1) for item in
                     next(a for a in call if a.startswith("--metadata="))[len("--metadata="):].split(","))
        self.assertEqual(list(items), list(cv.METADATA_KEYS))
        self.assertEqual(items["dashboard-image"], IMG_B)
        for key in ("dashboard-config-repo", "dashboard-controlled-targets", "dashboard-global-admins",
                    "dashboard-session-not-before", "dashboard-source-ref"):
            self.assertEqual(items[key], "", key)  # unset here: cleared on the VM rather than left as it was

    def test_a_vm_recreated_during_the_update_is_held(self):
        h = TrackingHarness()
        h.active()
        h.tick()
        h.gcloud.recreate("a", "id-new")
        h.clock[0] += cv.IMAGE_SWAP_GRACE_SECONDS
        h.tick()
        self.assertIn("held", h.transition["error"])
        self.assertNotIn(("reset", "spot-control-a"), h.events)

    def test_a_stopped_vm_is_not_updated(self):
        h = TrackingHarness()
        h.active()
        manager = h.manager()
        obs = manager.observe()
        plan = manager.plan(obs)
        self.assertEqual(plan.action, "update_image")
        h.gcloud.status("a", "TERMINATED")
        manager.execute(plan, manager.observe())
        self.assertIsNone(h.transition)
        self.assertNotIn("set-image", [e[0] for e in h.events])
        self.assertEqual(h.image["failed"], [D_B])

    def test_no_rollout_while_a_transition_runs_or_the_other_slot_exists(self):
        h = TrackingHarness()
        h.active()
        h.state[cv.STATE_KEY] = {"transition": {"id": "t", "mode": cv.MODE_RECOVER, "step": "wait_health",
                                                "old_slot": "a", "new_slot": "b", "new_vm": {"name": "spot-control-b"},
                                                "old_vm": {"name": "spot-control-a", "zone": "us-central1-a",
                                                           "instance_id": "id-a"}}}
        self.assertTrue(h.tick().resume)
        self.assertEqual(h.registry.calls, [])
        h = TrackingHarness()
        h.active()
        h.gcloud.add("b", machine="e2-micro", provisioning="STANDARD")
        plan = h.tick()
        self.assertEqual(plan.action, "noop")
        self.assertIn("not healthy, or the other slot is occupied", plan.reason)
        self.assertEqual(h.registry.calls, [])

    def test_unhealthy_active_vm_never_rolls(self):
        h = TrackingHarness()
        h.active()
        h.http.unhealthy("a")
        self.assertEqual(h.tick().action, "wait")
        self.assertEqual(h.registry.calls, [])

    def test_active_image_of_unknown_commit_never_rolls(self):
        h = TrackingHarness()
        h.active(image="ghcr.io/other/dashboard@" + D_A)
        plan = h.tick()
        self.assertIn("commit is unknown", plan.reason)
        self.assertIsNone(h.transition)

    def test_recovery_recreates_the_recorded_image_not_the_configured_one(self):
        h = TrackingHarness()
        h.active(image=IMG_B)
        h.registry.tags["main"] = D_B
        h.tick()  # records a's image (main already)
        self.assertEqual(h.image["active"]["image"], IMG_B)
        h.gcloud.vms.pop("spot-control-a")
        h.tick()
        h.clock[0] += cv.START_GRACE_SECONDS
        plan = h.tick()
        self.assertEqual(plan.action, "recreate_other_slot")
        self.assertEqual(h.transition["dashboard_image"], IMG_B)
        self.assertNotIn("image_rollout", h.transition)
        self.assertEqual(h.gcloud.create_image(), IMG_B)  # SPOT_DASHBOARD_IMAGE is IMG_A

    def recover_from(self, h, image):
        h.active(image=image)
        h.registry.tags.update({"main": D_B, SHA_B: D_B})
        h.tick()  # records a's image
        self.assertEqual(h.image["active"]["image"], image)
        h.gcloud.vms.pop("spot-control-a")
        h.tick()
        h.clock[0] += cv.START_GRACE_SECONDS
        return h.tick()

    def test_recovery_keeps_a_recorded_commit_tag_on_the_standard_vm(self):
        h = TrackingHarness()
        tag = f"{REPO}:{SHA_B}"
        self.assertEqual(self.recover_from(h, tag).action, "recreate_other_slot")
        self.assertEqual(h.transition["dashboard_image"], tag)
        self.assertEqual(h.gcloud.create_image(), tag)  # never the configured IMG_A

    def test_recovery_resolves_a_recorded_commit_tag_for_spot(self):
        h = TrackingHarness(provisioning="spot", offers=tcv.OFFERS)
        h.gcloud = WatchdogGcloud(h.events)
        tag = f"{REPO}:{SHA_B}"
        h.active = lambda image=IMG_A, slot="a": (h.gcloud.add(slot, provisioning="SPOT"),
                                                  h.gcloud.images.__setitem__(f"spot-control-{slot}", image),
                                                  h.kv.set_origin(slot), h.http.healthy(slot, version=7))
        self.assertEqual(self.recover_from(h, tag).action, "recreate_other_slot")
        self.assertEqual(h.transition["dashboard_image"], IMG_B)
        self.assertEqual(h.image["active"]["resolved_image"], IMG_B)
        self.assertEqual(h.gcloud.create_image(), IMG_B)

    def test_unresolvable_commit_tag_on_spot_waits_instead_of_downgrading(self):
        h = TrackingHarness(provisioning="spot", offers=tcv.OFFERS)
        tag = f"{REPO}:{SHA_B}"
        h.gcloud.add("a", provisioning="SPOT")
        h.gcloud.images["spot-control-a"] = tag
        h.kv.set_origin("a")
        h.http.healthy("a", version=7)
        h.state[cv.STATE_KEY] = {"image": {"active": {"slot": "a", "instance_id": "id-a", "image": tag}}}

        def down(tag):
            raise it.TrackingError("ghcr down")
        h.registry.resolve = down
        h.gcloud.vms.pop("spot-control-a")
        h.tick()
        h.clock[0] += cv.START_GRACE_SECONDS
        h.tick()
        # The transition keeps the commit tag, which a Spot create refuses: it waits, never creating IMG_A.
        self.assertEqual(h.transition["dashboard_image"], tag)
        self.assertIn("pinned by digest", h.transition["error"])
        self.assertFalse([c for c in h.gcloud.calls if c[2:4] == ["instances", "create"]])

    def test_no_rollout_from_a_standard_vm_when_spot_is_configured(self):
        h = TrackingHarness(provisioning="spot", offers=tcv.OFFERS)
        h.active()  # standard e2-micro serving; :main is newer
        with self.assertLogs("spot_runner_autoscaler.control_vm", "WARNING") as logs:
            plan = h.tick()
        self.assertEqual(plan.action, "noop")
        self.assertIn("run rotate to move back to Spot", plan.reason)  # the planner's explicit-rotate reason stays
        self.assertIn("image rollout held", plan.reason)
        self.assertIn("an explicit rotate is required", "\n".join(logs.output))
        self.assertIsNone(h.transition)
        self.assertEqual(h.registry.calls, [])
        self.assertEqual(h.mutations(), [])

    def test_no_rollout_from_a_spot_vm_when_standard_is_configured(self):
        h = TrackingHarness()
        h.gcloud.add("a", machine="e2-small", provisioning="SPOT")
        h.gcloud.images["spot-control-a"] = IMG_A
        h.kv.set_origin("a")
        h.http.healthy("a", version=7)
        h.state[cv.STATE_KEY] = {"last_transition_end_at": h.clock[0] - 10}  # inside the cooldown: a noop
        plan = h.tick()
        self.assertEqual(plan.action, "noop")
        self.assertIn("image rollout held", plan.reason)
        self.assertIsNone(h.transition)
        self.assertEqual(h.registry.calls, [])
        # After the cooldown the planner's own mode switch runs, never an image rollout.
        h.clock[0] += cv.ROTATION_COOLDOWN_SECONDS
        plan = h.tick()
        self.assertEqual(plan.action, "rotate")
        self.assertIn("SPOT_CONTROL_PROVISIONING=standard", plan.reason)
        self.assertNotIn("image_rollout", h.transition)
        self.assertNotEqual(h.transition["dashboard_image"], IMG_B)

    def test_unknown_model_holds_a_standard_rollout(self):
        h = TrackingHarness()
        h.gcloud.add("a", machine="e2-micro")  # no scheduling in the inventory row
        h.gcloud.images["spot-control-a"] = IMG_A
        h.kv.set_origin("a")
        h.http.healthy("a", version=7)
        plan = h.tick()
        self.assertEqual(plan.action, "noop")
        self.assertIn("image rollout held", plan.reason)
        self.assertIsNone(h.transition)

    def test_a_resumed_rollout_creates_its_pinned_image_on_any_controller(self):
        h = tcv.Harness(offers=tcv.STANDARD_OFFERS, provisioning="standard", regions=("us-central1",))
        h.gcloud.add("a", machine="e2-micro", provisioning="STANDARD")
        h.kv.set_origin("a")
        h.http.healthy("a", version=7)
        h.state[cv.STATE_KEY] = {"transition": {
            "id": "t1", "mode": cv.MODE_PRICE, "old_slot": "a", "new_slot": "b", "step": "create",
            "old_vm": {"name": "spot-control-a", "zone": "us-central1-a", "instance_id": "id-a"},
            "new_vm": {"name": "spot-control-b", "zone": "", "machine_type": ""}, "offer": None,
            "provisioning": "standard", "excluded_zones": [], "failed_offers": [], "failed_offers_at": {},
            "started_at": h.clock[0], "step_started_at": h.clock[0], "updated_at": h.clock[0], "attempts": 0,
            "create_failures": 0, "freeze_requested": False, "frozen_version": None, "import_skipped": False,
            "cutover_attempted": False, "cutover_at": None, "abort_reason": None, "error": None, "reason": "test",
            "dashboard_image": IMG_C, "image_rollout": {"image": IMG_C, "digest": D_C}}}
        h.tick()  # tracking is off on this controller, and its SPOT_DASHBOARD_IMAGE is IMG_A
        create = [c for c in h.gcloud.calls if c[2:4] == ["instances", "create"]][-1]
        self.assertIn(f"dashboard-image={IMG_C}", next(a for a in create if a.startswith("--metadata=")))

    def test_missing_token_or_bad_settings(self):
        h = TrackingHarness(image_track_token="")
        h.active()
        manager = cv.ControlVmManager(h.config, h.state, h.save, gcloud=h.gcloud, kv=h.kv, requester=h.http,
                                      offer_source=lambda: list(h.offers), clock=lambda: h.clock[0])
        plan = manager.tick(apply=True)
        self.assertEqual(plan.action, "noop")
        self.assertIn("SPOT_DASHBOARD_IMAGE_TRACK_TOKEN is not set", plan.reason)
        self.assertIn("must be empty or main", " ".join(cv.ControlConfig(project="p", image_track="latest")
                                                         .runtime_problems()))
        self.assertIn("only swack-tools/spot-github-runners publishes", " ".join(
            cv.ControlConfig(project="p", image_track="main", dashboard_repo="me/fork").runtime_problems()))
        env_config = cv.ControlConfig.from_env("p", {"SPOT_DASHBOARD_IMAGE_TRACK": " Main ",
                                                     "SPOT_DASHBOARD_IMAGE_TRACK_TOKEN": "pat"})
        self.assertTrue(env_config.tracking)
        self.assertNotIn("pat", repr(env_config))

    def test_registry_failure_changes_nothing(self):
        h2 = TrackingHarness()
        h2.active()
        h2.registry.resolve = lambda tag: (_ for _ in ()).throw(it.TrackingError("ghcr down"))
        plan = h2.tick()
        self.assertEqual(plan.action, "noop")
        self.assertIn("ghcr down", plan.reason)
        self.assertEqual(h2.mutations(), [])

    def test_preview_rolls_nothing_and_saves_nothing(self):
        h = TrackingHarness()
        h.active()
        plan = h.tick(apply=False)
        self.assertEqual(plan.action, "update_image")
        self.assertEqual(h.mutations(), [])
        self.assertEqual(h.state, {})
        self.assertEqual(h.saves, 0)


class ImageHealthTests(unittest.TestCase):
    def test_probe_reads_the_reported_image(self):
        h = tcv.Harness()
        h.http.healthy("a", version=4, image=IMG_B)
        self.assertEqual(cv.probe_health(h.http, h.config, "a").image, IMG_B)
        h.http.healthy("a", version=4)
        self.assertIsNone(cv.probe_health(h.http, h.config, "a").image)
        h.http.health["a"] = (200, {"status": "ok", "hydrated": True, "frozen": False, "image": 5})
        self.assertIsNone(cv.probe_health(h.http, h.config, "a").image)

    def test_an_image_transition_resumes_as_update_image(self):
        plan = cv.plan_next(vms=[], origin=None, origin_known=True, health={}, offers=None,
                            control_state={"transition": {"mode": cv.MODE_IMAGE, "step": "wait_image",
                                                          "old_slot": None, "new_slot": "a"}},
                            now=0, max_daily_usd=cv.DEFAULT_MAX_DAILY_USD)
        self.assertEqual((plan.action, plan.target_slot, plan.resume), ("update_image", "a", True))


class RestartPolicyTests(unittest.TestCase):
    def test_create_command_keeps_automatic_restart_off(self):
        config = tcv.config(provisioning="standard", regions=("us-central1",))
        command = cv.create_command(config, "a", tcv.micro(), "/tmp/s.sh")
        self.assertIn("--no-restart-on-failure", command)
        self.assertNotIn("--restart-on-failure", command)
        spot = cv.create_command(tcv.config(), "a", tcv.offer(), "/tmp/s.sh")
        self.assertFalse([a for a in spot if "restart-on-failure" in a])  # Spot VMs never restart by themselves

    def test_inventory_reads_automatic_restart(self):
        self.assertIn("scheduling.automaticRestart", " ".join(cv.list_command("p")))
        base = {"name": "spot-control-a", "zone": "z/us-central1-a", "status": "RUNNING",
                "labels": {"spot-role": "control", "spot-control-slot": "a"}}
        for scheduling, expected in (({"automaticRestart": True}, True), ({"automaticRestart": False}, False),
                                     ({}, None), (None, None), ({"automaticRestart": "true"}, None)):
            row = dict(base, scheduling=scheduling) if scheduling is not None else dict(base)
            self.assertIs(cv.parse_control_vms([row])[0].automatic_restart, expected, scheduling)

    def harness_with_auto_restart(self):
        h = TrackingHarness(image_track="")
        h.gcloud.add("a", machine="e2-micro", provisioning="STANDARD")
        h.gcloud.vms["spot-control-a"]["scheduling"]["automaticRestart"] = True
        h.kv.set_origin("a")
        h.http.healthy("a")
        return h

    def test_automatic_restart_is_turned_off_and_a_preview_is_not_idle(self):
        h = self.harness_with_auto_restart()
        h.gcloud.vms["spot-control-a"]["scheduling"]["automaticRestart"] = False
        h.tick()  # settle the state: from here on a preview of a healthy VM is idle
        manager = h.manager()
        self.assertTrue(manager.preview_is_idle(manager.tick(apply=False), h.state))
        h.gcloud.vms["spot-control-a"]["scheduling"]["automaticRestart"] = True
        h.events.clear()
        manager = h.manager()
        plan = manager.tick(apply=False)
        self.assertEqual(h.gcloud.mutating(), [])
        self.assertEqual(manager.preview_actions, ["turn off automatic restart on spot-control-a"])
        self.assertFalse(manager.preview_is_idle(plan, h.state), "a pending set-scheduling is not idle")
        h.tick()
        self.assertEqual(h.events, [("set-scheduling", "spot-control-a")])
        self.assertFalse(h.gcloud.vms["spot-control-a"]["scheduling"]["automaticRestart"])
        manager = h.manager()
        plan = manager.tick(apply=False)
        self.assertEqual(manager.preview_actions, [])
        h.tick()
        self.assertEqual(h.events.count(("set-scheduling", "spot-control-a")), 1)

    def test_recovery_fence_waits_while_a_stopped_old_vm_could_restart_by_itself(self):
        h = tcv.ResumeFromPersistedStepTests.setup_rotation(None, "cutover", mode=cv.MODE_RECOVER, kv_caught_up=True,
                                                            freeze_requested=True, frozen_version=7)
        h.gcloud = WatchdogGcloud(h.events)
        h.gcloud.add("a", status="TERMINATED", machine="e2-micro", provisioning="STANDARD")
        h.gcloud.vms["spot-control-a"]["scheduling"]["automaticRestart"] = True
        tcv.ResumeFromPersistedStepTests.with_new_vm(None, h)
        h.tick()
        self.assertNotIn(("origin", "b"), h.events, "a crash-TERMINATED VM with automatic restart is no fence")
        self.assertIn(("set-scheduling", "spot-control-a"), h.events)
        self.assertIn("automatic restart is on", h.transition["error"])
        self.assertEqual(h.transition["step"], "cutover")
        h.tick()  # the inventory now confirms automaticRestart false while it is still TERMINATED
        self.assertIn(("origin", "b"), h.events)

    def test_rollback_fence_waits_for_a_stopped_replacement_to_lose_automatic_restart(self):
        h = tcv.ResumeFromPersistedStepTests.rollback_setup(tcv.RollbackFenceTests())
        h.gcloud = WatchdogGcloud(h.events)
        h.gcloud.add("a", machine="e2-medium")
        tcv.ResumeFromPersistedStepTests.with_new_vm(None, h, status="TERMINATED", healthy=False)
        h.gcloud.vms["spot-control-b"]["scheduling"] = {"provisioningModel": "STANDARD", "automaticRestart": True}
        h.http.unhealthy("b")
        h.clock[0] += cv.REPLACEMENT_READY_GRACE_SECONDS
        h.state[cv.STATE_KEY]["transition"].update(step="abort", abort_reason="test")
        h.tick()
        self.assertNotIn(("unfreeze", "a"), h.events)
        self.assertNotIn(("origin", "a"), h.events)
        self.assertIn(("set-scheduling", "spot-control-b"), h.events)
        h.tick()
        self.assertIn(("origin", "a"), h.events)



# --------------------------------------------------------------------------------------------
# Aborted rollouts: recorded reason, warning, retry, and GitHub commit statuses
# --------------------------------------------------------------------------------------------

def abort_rollout(h):
    """Start the in-place rollout to :main (D_B) and let it time out unhealthy, then complete the rollback."""
    h.tick()
    fail_update(h)


def fail_update(h, slot="a"):
    """The update in progress never gets healthy: it times out, the watcher swaps back, and the rollback ends."""
    h.http.health[slot] = (503, {"status": "starting", "hydrated": False, "frozen": True, "state_version": 7,
                                 "slot": slot, "image": h.transition["dashboard_image"]})
    h.clock[0] += cv.IMAGE_UPDATE_TIMEOUT_SECONDS
    h.tick()  # timed out: the previous image is pinned again
    h.swapped(image=h.transition["from_image"], slot=slot)
    h.tick()  # the restored container is released; the rollback ends
    h.http.healthy(slot, version=7, image=h.image["active"]["image"])


class AbortVisibilityTests(unittest.TestCase):
    def test_the_abort_reason_is_kept_named_in_the_plan_and_logged_every_check(self):
        h = TrackingHarness()
        h.active()
        abort_rollout(h)
        failure = h.image["failures"][D_B]
        self.assertEqual(failure["revision"], SHA_B)
        self.assertIn(f"healthy within {cv.IMAGE_UPDATE_TIMEOUT_SECONDS}s", failure["reason"])
        self.assertEqual(failure["at"], h.clock[0])
        h.clock[0] += cv.ROTATION_COOLDOWN_SECONDS
        with self.assertLogs("spot_runner_autoscaler.control_vm", "WARNING") as logs:
            plan = h.tick()
        self.assertEqual(plan.action, "noop")
        self.assertIn("healthy within", plan.reason)
        self.assertIn("retry-image --apply", plan.reason)
        self.assertTrue(any("image rollout held" in line for line in logs.output))
        # A preview reports the same, and changes nothing.
        with self.assertLogs("spot_runner_autoscaler.control_vm", "WARNING"):
            self.assertIn("healthy within", h.tick(apply=False).reason)
        # The status output carries it too.
        manager = h.manager()
        self.assertIn(D_B, manager.status(manager.observe())["image"]["failures"])

    def test_the_abort_is_reported_during_the_cooldown_too(self):
        h = TrackingHarness()
        h.active()
        abort_rollout(h)
        h.registry.calls.clear()
        with self.assertLogs("spot_runner_autoscaler.control_vm", "WARNING") as logs:
            plan = h.tick()
        self.assertIn("cooldown", plan.reason)
        self.assertIn("healthy within", plan.reason)
        self.assertTrue(any("image rollout held" in line for line in logs.output))
        self.assertEqual(h.registry.calls, [])  # still no registry call while the cooldown holds

    def test_a_retry_never_waives_the_cooldown_for_another_digest(self):
        h = TrackingHarness()
        h.active()
        abort_rollout(h)
        it.retry_failed(h.image, D_B, at=h.clock[0])
        h.registry.tags["main"] = D_C  # :main moved before the retried digest could roll
        plan = h.tick()
        self.assertEqual(plan.action, "noop")
        self.assertIn("doesn't cover", plan.reason)
        self.assertIsNone(h.transition)
        h.clock[0] += cv.ROTATION_COOLDOWN_SECONDS
        plan = h.tick()  # after the cooldown the new digest rolls as usual, and uses the retry up
        self.assertEqual(plan.action, "update_image")
        self.assertEqual(h.transition["dashboard_image"], IMG_C)
        self.assertNotIn("retry", h.image)

    def test_failures_stay_bounded_to_the_backed_off_digests(self):
        cache = {}
        for i in range(it.FAILED_DIGESTS_KEPT + 3):
            it.record_failed(cache, f"sha256:{i:064d}", reason=f"r{i}", at=i)
        self.assertEqual(set(cache["failures"]), set(cache["failed"]))
        self.assertEqual(it.failure_of(cache, cache["failed"][-1])["reason"], f"r{it.FAILED_DIGESTS_KEPT + 2}")
        self.assertEqual(it.failure_of(cache, "sha256:" + "f" * 64), {})
        it.record_failed({"failed": [D_A], "failures": "junk"}, D_B)  # a malformed section never raises

    def test_retry_lifts_the_back_off_and_skips_the_abort_cooldown_once(self):
        h = TrackingHarness()
        h.active()
        abort_rollout(h)
        self.assertIn("cooldown", h.tick().reason)
        digest, failure = cv.retry_image_target(h.state[cv.STATE_KEY], None)
        self.assertEqual(digest, D_B)
        self.assertIn("healthy within", failure["reason"])
        it.retry_failed(h.image, D_B, at=h.clock[0])
        self.assertEqual(h.image["failed"], [])
        self.assertNotIn(D_B, h.image["failures"])
        plan = h.tick()  # no cooldown wait for the abort before the retry
        self.assertEqual(plan.action, "update_image")
        self.assertEqual(h.transition["dashboard_image"], IMG_B)
        self.assertNotIn("retry", h.image)  # used up once its rollout begins
        # Aborting again backs it off again, and that abort's cooldown applies.
        fail_update(h)
        self.assertEqual(h.image["failed"], [D_B])
        self.assertIn("cooldown", h.tick().reason)

    def test_a_retry_never_skips_a_completed_transitions_cooldown(self):
        h = TrackingHarness()
        h.active()
        h.state.setdefault(cv.STATE_KEY, {})["last_transition_end_at"] = h.clock[0] - 60
        h.state[cv.STATE_KEY]["image"] = {"failed": [D_B], "retry": {"digest": D_C, "at": h.clock[0]}}
        self.assertIn("cooldown", h.tick().reason)

    def test_retry_target_selection(self):
        failures = {"image": {"failed": [D_A, D_B], "main": {"digest": D_B}}}
        self.assertEqual(cv.retry_image_target(failures, None)[0], D_B)  # the :main one
        self.assertEqual(cv.retry_image_target(failures, D_A)[0], D_A)
        self.assertEqual(cv.retry_image_target({"image": {"failed": [D_C]}}, None)[0], D_C)  # the only one
        for cstate, digest, words in (({}, None, "no image rollout is backed off"),
                                      ({"image": {"failed": [D_A, D_B]}}, None, "name one with --digest"),
                                      (failures, D_C, "is not backed off"),
                                      (failures, "sha256:nope", "must be sha256")):
            with self.assertRaises(ValueError) as caught:
                cv.retry_image_target(cstate, digest)
            self.assertIn(words, str(caught.exception))
        with self.assertRaises(ValueError):
            it.retry_failed({"failed": []}, D_A, at=1)


class RetryImageCliTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.dir = Path(directory.name)
        self.path = cv.project_control_paths("homelab-424523", self.dir)[0]
        self.cstate = {"image": {"failed": [D_B], "main": {"digest": D_B},
                                 "failures": {D_B: {"reason": "slot b not healthy after 1200s", "revision": SHA_B}}},
                       "last_abort_at": 5.0}
        self.path.write_text(json.dumps({cv.STATE_KEY: self.cstate}))

    def cli(self, *argv):
        out = io.StringIO()
        with redirect_stdout(out), patch("lib.environment.load_env"), \
                patch.dict("os.environ", {cv.ENV_CONTROL_STATE_URI: ""}):
            code = cv.main(["retry-image", *argv, "--control-state-dir", str(self.dir),
                            "--state-file", str(self.dir / "autoscaler.json")])
        return code, out.getvalue()

    def test_preview_changes_nothing_and_apply_lifts_the_back_off(self):
        before = self.path.read_text()
        code, out = self.cli()
        self.assertEqual(code, 0)
        self.assertIn(f"Would lift the back-off of {D_B}", out)
        self.assertIn("slot b not healthy after 1200s", out)
        self.assertEqual(self.path.read_text(), before)
        code, out = self.cli("--apply")
        self.assertEqual(code, 0, out)
        self.assertIn(f"Lifted the back-off of {D_B}", out)
        image = json.loads(self.path.read_text())[cv.STATE_KEY]["image"]
        self.assertEqual(image["failed"], [])
        self.assertEqual(image["retry"]["digest"], D_B)
        self.assertEqual(image["retry"]["previous"]["reason"], "slot b not healthy after 1200s")
        code, out = self.cli("--apply")
        self.assertEqual(code, 1)
        self.assertIn("Nothing to retry: no image rollout is backed off", out)

    def test_an_unknown_digest_is_refused_without_a_write(self):
        before = self.path.read_text()
        code, out = self.cli("--apply", "--digest", D_C)
        self.assertEqual(code, 1)
        self.assertIn("is not backed off", out)
        self.assertEqual(self.path.read_text(), before)


class RolloutStatusTests(unittest.TestCase):
    def statuses(self, h):
        return [c[1:] for c in h.github.calls if c[0] == "status"]

    def test_a_rollout_reports_pending_then_success_on_its_commit(self):
        h = TrackingHarness(rollout_status="1")
        h.active()
        h.tick()
        self.assertEqual([s[:2] for s in self.statuses(h)], [(SHA_B, "pending")])
        h.finish()
        self.assertEqual([s[:2] for s in self.statuses(h)], [(SHA_B, "pending"), (SHA_B, "success")])
        self.assertIn("spot-control-a", self.statuses(h)[-1][2])

    def test_an_aborted_rollout_reports_failure_with_the_reason(self):
        h = TrackingHarness(rollout_status="1")
        h.active()
        abort_rollout(h)
        sha, state, description = self.statuses(h)[-1]
        self.assertEqual((sha, state), (SHA_B, "failure"))
        self.assertIn("healthy within", description)

    def test_off_by_default_and_a_reporting_failure_changes_nothing(self):
        h = TrackingHarness()
        h.active()
        abort_rollout(h)
        self.assertEqual(self.statuses(h), [])
        h = TrackingHarness(rollout_status="1")
        h.active()
        h.github.status_error = "HTTP 403"
        with self.assertLogs("spot_runner_autoscaler.control_vm", "WARNING") as logs:
            h.tick()
        self.assertEqual(h.transition["step"], "wait_image")
        self.assertTrue(any("could not report the image rollout" in line for line in logs.output))
        h.github.status_error = None
        h.finish()
        self.assertIsNone(h.transition)

    def test_a_recovery_reports_nothing(self):
        h = TrackingHarness(rollout_status="1")
        h.manager()._report_rollout({"image_rollout": None}, "pending", "x")
        self.assertEqual(self.statuses(h), [])

    def test_settings(self):
        config = tcv.config(image_track="main", image_track_token="pat", rollout_status="yes")
        self.assertTrue(any(it.ENV_ROLLOUT_STATUS in p for p in config.tracking_problems()))
        self.assertFalse(tcv.config(image_track="", rollout_status="1").reports_rollouts)
        self.assertTrue(tcv.config(image_track="main", rollout_status="1").reports_rollouts)
        env = {it.ENV_IMAGE_TRACK: "main", it.ENV_ROLLOUT_STATUS: " 1 "}
        self.assertTrue(cv.ControlConfig.from_env("homelab-424523", env).reports_rollouts)


class SetStatusTests(unittest.TestCase):
    def test_posts_a_commit_status_and_needs_201(self):
        calls = []

        def http(method, url, headers, body, timeout):
            calls.append((method, url, headers, json.loads(body)))
            return HttpResponse(201, {}, b"{}")
        client = it.GitHubClient("swack-tools/spot-github-runners", "pat", transport=http)
        client.set_status(SHA_B, "failure", "x" * 200, target_url="https://spot.swacktech.com")
        method, url, headers, body = calls[0]
        self.assertEqual((method, url), ("POST", f"https://api.github.com/repos/swack-tools/spot-github-runners/"
                                                 f"statuses/{SHA_B}"))
        self.assertEqual(headers["Authorization"], "Bearer pat")
        self.assertEqual((body["state"], body["context"], body["target_url"]),
                         ("failure", it.ROLLOUT_STATUS_CONTEXT, "https://spot.swacktech.com"))
        self.assertEqual(len(body["description"]), it.STATUS_DESCRIPTION_MAX)
        client.set_status(SHA_B, "success", "ok", target_url="http://insecure")
        self.assertNotIn("target_url", calls[1][3])

        def forbidden(method, url, headers, body, timeout):
            return HttpResponse(403, {}, b"{}")
        client = it.GitHubClient("swack-tools/spot-github-runners", "pat", transport=forbidden)
        for args in ((SHA_B, "success", "ok"), ("main", "success", "ok"), (SHA_B, "done", "ok")):
            with self.assertRaises(it.TrackingError):
                client.set_status(*args)


if __name__ == "__main__":
    unittest.main()
