"""Dashboard image rollout (SPOT_DASHBOARD_IMAGE_TRACK=main).

Registry, GitHub, and HTTP all go through in-memory fakes; nothing touches GHCR, GitHub, GCP, or Cloudflare.
"""
from __future__ import annotations

import base64
import hashlib
import json
import unittest
from types import SimpleNamespace

from lib import image_tracking as it
from lib.https_transport import HttpResponse, TransportError

REPO = "ghcr.io/swack-tools/spot-dashboard"
D_A, D_B, D_C = ("sha256:" + c * 64 for c in "abc")
IMG_A, IMG_B, IMG_C = (f"{REPO}@{d}" for d in (D_A, D_B, D_C))
SHA_A, SHA_B, SHA_C = (c * 40 for c in "123")


def tearDownModule():
    pass


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
# Tracker evaluation
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


class ImageTrackerTests(unittest.TestCase):
    def test_evaluate_rolls_when_healthy(self):
        registry = FakeRegistry()
        github = FakeGitHub()
        tracker = it.ImageTracker(REPO, registry, github, lambda: 1000.0)
        cache = {}
        decision = tracker.evaluate(cache, active_image=IMG_A, transition_active=False, active_ready=True, cooling=False)
        self.assertTrue(decision.roll)
        self.assertEqual(decision.image, IMG_B)

    def test_active_facts_resolves(self):
        registry = FakeRegistry()
        github = FakeGitHub()
        tracker = it.ImageTracker(REPO, registry, github, lambda: 1000.0)
        cache = {}
        digest, rev = tracker.active_facts(cache, IMG_A)
        self.assertEqual((digest, rev), (D_A, SHA_A))


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
