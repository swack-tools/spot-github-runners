"""Static checks for the Cloudflare Worker, its wrangler config, the dashboard image, and its dockerignore."""
from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WRANGLER = ROOT / "cloudflare" / "wrangler.toml"
DOCKERFILE = ROOT / "dashboard" / "Dockerfile"
DOCKERIGNORE = ROOT / ".dockerignore"


class WranglerConfigTests(unittest.TestCase):
    def setUp(self) -> None:
        self.text = WRANGLER.read_text(encoding="utf-8")

    def test_no_account_ids_or_secrets(self) -> None:
        self.assertIsNone(re.search(r"\b[0-9a-f]{32}\b", self.text), "wrangler.toml must not contain 32-hex ids")
        self.assertNotRegex(self.text, r"(?m)^\s*account_id\s*=")
        for secret in ("SESSION_SECRET", "CF_ACCESS_CLIENT_ID", "CF_ACCESS_CLIENT_SECRET"):
            self.assertNotRegex(self.text, rf"(?m)^\s*{secret}\s*=", f"{secret} must be a wrangler secret")
            self.assertIn(f"wrangler secret put {secret}", self.text)

    def test_route_kv_and_vars(self) -> None:
        self.assertRegex(self.text, r'(?m)^name = "spot-dashboard-edge"$')
        self.assertRegex(self.text, r'(?m)^main = "build/worker/shim\.mjs"$')
        self.assertIn('command = "worker-build --release"', self.text)
        self.assertIn('{ pattern = "spot.swacktech.com", custom_domain = true }', self.text)
        self.assertIn('binding = "SPOT_KV", id = "REPLACE_WITH_SPOT_KV_NAMESPACE_ID"', self.text)
        # The Worker only authenticates; the control VM decides which accounts may sign in (owner plus the
        # administrators added on the Admins page), so no identity is pinned here.
        self.assertNotRegex(self.text, r'(?m)^ALLOWED_ADMIN_(LOGIN|ID)\s*=')
        # Optional bulk sign-out cut-off: empty (none) or Unix seconds, read by worker.js sessionNotBefore().
        self.assertRegex(self.text, r'(?m)^SESSION_NOT_BEFORE = "([0-9]{1,15})?"$')
        date = re.search(r'(?m)^compatibility_date = "(\d{4}-\d{2}-\d{2})"$', self.text)
        self.assertIsNotNone(date)
        self.assertLessEqual(date.group(1), "2026-08-01")


class DockerfileTests(unittest.TestCase):
    def setUp(self) -> None:
        self.text = DOCKERFILE.read_text(encoding="utf-8")

    def test_pinned_base_and_non_root(self) -> None:
        self.assertRegex(self.text, r"(?m)^FROM python:3\.13\.\d+-slim-bookworm(@sha256:[0-9a-f]{64})?$")
        self.assertRegex(self.text, r"(?m)^USER 10001:10001$")
        self.assertIn("--uid 10001", self.text)
        self.assertIn("--gid 10001", self.text)
        self.assertNotRegex(self.text, r"(?m)^USER (root|0)\b")

    def test_copied_files_are_normalized_for_the_runtime_user(self) -> None:
        # A checkout under umask 077 must still produce readable, root-owned, non-writable app files.
        normalize = "RUN chown -R root:root /app \\\n    && chmod -R u=rwX,go=rX /app"
        self.assertIn(normalize, self.text)
        self.assertLess(self.text.rindex("COPY --chown=root:root"), self.text.index(normalize))
        self.assertLess(self.text.index(normalize), self.text.index("USER 10001:10001"))

    def test_runtime_settings(self) -> None:
        self.assertRegex(self.text, r"(?m)^WORKDIR /app$")
        self.assertIn("PYTHONPATH=/app/src", self.text)
        self.assertIn("PYTHONDONTWRITEBYTECODE=1", self.text)
        self.assertIn('ENTRYPOINT ["python3", "-B", "/app/src/web_dashboard.py"]', self.text)
        self.assertNotRegex(self.text, r"(?m)^CMD\b")
        self.assertIn("http://127.0.0.1:8787/api/health", self.text)
        self.assertRegex(self.text, r"(?m)^HEALTHCHECK ")

    def test_binds_nothing_publicly_and_carries_no_secrets(self) -> None:
        self.assertNotIn("0.0.0.0", self.text)
        self.assertNotRegex(self.text, r"(?m)^EXPOSE\b")
        self.assertNotRegex(self.text, r"(?im)^(ENV|ARG)\b.*(SECRET|TOKEN|PASSWORD)")

    def test_dockerignore_excludes_sensitive_paths(self) -> None:
        lines = set(DOCKERIGNORE.read_text(encoding="utf-8").split())
        self.assertIn("*", lines)
        for pattern in ("**/.git", "**/.env", "**/*.pem", "**/.wrangler", "**/__pycache__", "**/.pytest_cache"):
            self.assertIn(pattern, lines)
        for allowed in ("!src/", "!cloud-init/", "!runner/", "!builder/"):
            self.assertIn(allowed, lines)


if __name__ == "__main__":
    unittest.main()
