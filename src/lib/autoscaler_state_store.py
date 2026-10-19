"""Durable autoscaler controller state in Google Cloud Storage (``SPOT_AUTOSCALER_STATE_URI``).

On a workstation the autoscaler keeps its controller state in a local JSON file (``--state-file``). A Cloud Run job
execution has no disk that outlives it, so with ``SPOT_AUTOSCALER_STATE_URI=gs://<bucket>/<path>.json`` the same
JSON document lives in one Cloud Storage object instead:

* ``read_scale_state`` loads the object and remembers its generation. A missing object stops a live check (seed it
  from the workstation's state file first); only a ``--dry-run`` starts from an empty state.
* Every ``write_scale_state`` still writes the local file, then uploads the same bytes with
  ``ifGenerationMatch=<the generation it loaded or last wrote>``. The autoscaler persists its intent before every
  provider change, so a durable write before each change keeps its crash recovery working across executions.
* A failed or conflicting upload (another writer changed the object, or the outcome stays unknown after a
  read-back) marks the store failed for the rest of the process. :func:`check` then raises, and the autoscaler
  hooks it into its mutation fence: no mutating child process (``gcloud``, ``gh``, ``ssh``, the deployment manager)
  starts after the state stopped being durable, whichever ``except`` clause caught the write error.
* Deployment receipts (``<instance>-<uuid>-deployment.json``, written next to the state file by the deployment
  manager while a launch runs) are uploaded to ``<object path without .json>/receipts/`` by the deployment manager
  itself, right after it writes them and before it waits for the bootstrap (:func:`save_receipt_from_env`). An
  upload that fails fails the launch, whose VM the autoscaler then removes, so a launch never continues with its
  only receipt on an execution's disk. A missing local receipt is restored on demand.

The object holds exactly the JSON the local file holds, so ``gcloud storage cp`` moves state between a workstation
and Cloud Storage in either direction. Standard library only.
"""
from __future__ import annotations

import logging
import os
import re
import threading
from pathlib import Path
from typing import Callable, Mapping

from .gcs_control_state import GcsAmbiguous, GcsClient, GcsError, GcsPreconditionFailed

LOG = logging.getLogger("spot_runner_autoscaler.state_store")

ENV_STATE_URI = "SPOT_AUTOSCALER_STATE_URI"
WRITE_ATTEMPTS = 3

_URI_RE = re.compile(r"^gs://([a-z0-9][a-z0-9_.-]{1,220}[a-z0-9])/((?:[A-Za-z0-9_.-]+/)*[A-Za-z0-9_.-]+\.json)$")
_RECEIPT_NAME_RE = re.compile(r"^[a-z0-9-]+-[0-9a-f]{32}-deployment\.json$")


class AutoscalerStateError(RuntimeError):
    """The Cloud Storage state can't be read, or a write is not proven durable; stop before any provider change."""


def parse_uri(uri: str) -> tuple[str, str]:
    """(bucket, object name) of ``gs://<bucket>/<path>.json``; raises ValueError otherwise."""
    match = _URI_RE.match(str(uri or "").strip())
    if not match or any(part.strip(".") == "" for part in match.group(2).split("/")):
        raise ValueError(f"{ENV_STATE_URI} must be gs://<bucket>/<path>.json, not {uri!r}")
    return match.group(1), match.group(2)


class GcsStateStore:
    """The autoscaler's state object; writes are conditioned on the generation this process last saw."""

    def __init__(self, uri: str, *, client: GcsClient | None = None, read_only: bool = False) -> None:
        self.bucket, self.object_name = parse_uri(uri)
        self.uri = f"gs://{self.bucket}/{self.object_name}"
        self.client = client or GcsClient(self.bucket)
        self.read_only = read_only
        self.generation: int | None = None  # None: not loaded yet; 0: the object does not exist
        self.failure: str | None = None
        self._lock = threading.Lock()

    # State ------------------------------------------------------------------------------------

    def load(self) -> bytes | None:
        """The object's bytes, or None when it does not exist. Raises AutoscalerStateError when it can't be read."""
        try:
            obj = self.client.get(self.object_name)
        except GcsError as exc:
            raise AutoscalerStateError(f"could not read autoscaler state {self.uri}: {exc}") from exc
        with self._lock:
            self.generation = obj.generation if obj is not None else 0
        return obj.data if obj is not None else None

    def check(self) -> None:
        """Raise when an earlier write failed: the object no longer reflects what this process did."""
        if self.failure:
            raise AutoscalerStateError(f"Fleet mutation held: autoscaler state {self.uri} is not durable "
                                       f"({self.failure}); this execution stops changing providers")

    def _fail(self, reason: str) -> AutoscalerStateError:
        self.failure = self.failure or reason
        LOG.error("Autoscaler state %s: %s; no further provider change runs in this process", self.uri, reason)
        return AutoscalerStateError(f"autoscaler state {self.uri}: {reason}")

    def save(self, data: bytes) -> None:
        """Upload ``data`` over exactly the generation this process last loaded or wrote."""
        with self._lock:
            if self.read_only:
                raise AutoscalerStateError(f"autoscaler state {self.uri} was opened read-only (--dry-run)")
            self.check()
            if self.generation is None:
                raise self._fail("written before it was read")
            for attempt in range(WRITE_ATTEMPTS):
                try:
                    info = self.client.put(self.object_name, data, if_generation_match=self.generation)
                except GcsPreconditionFailed:
                    raise self._fail(f"changed since generation {self.generation}: another controller wrote it")
                except GcsAmbiguous as exc:
                    settled = self._settle(data)
                    if settled == "written":
                        return
                    if settled == "unchanged" and attempt + 1 < WRITE_ATTEMPTS:
                        LOG.warning("Autoscaler state write was not confirmed (%s); retrying", exc)
                        continue
                    raise self._fail(f"write not confirmed: {exc}")
                except GcsError as exc:
                    raise self._fail(f"write failed: {exc}")
                self.generation = info.generation
                return

    def _settle(self, data: bytes) -> str:
        """After an ambiguous write: "written" (the object holds ``data``), "unchanged", or "conflict"."""
        try:
            obj = self.client.get(self.object_name)
        except GcsError:
            return "conflict"
        current = obj.generation if obj is not None else 0
        if obj is not None and obj.data == data and current != self.generation:
            self.generation = current
            return "written"
        return "unchanged" if current == self.generation else "conflict"

    # Deployment receipts ----------------------------------------------------------------------

    def receipt_object(self, name: str) -> str:
        return f"{self.object_name[:-len('.json')]}/receipts/{name}"

    def upload_receipt(self, path: Path) -> None:
        """Copy one receipt file to Cloud Storage (overwriting it), proven by reading it back; raises otherwise."""
        if self.read_only:
            raise AutoscalerStateError(f"autoscaler state {self.uri} was opened read-only (--dry-run)")
        if not _RECEIPT_NAME_RE.match(path.name):
            raise AutoscalerStateError(f"{path.name} is not a deployment receipt name")
        data = path.read_bytes()
        name = self.receipt_object(path.name)
        last = ""
        for _attempt in range(WRITE_ATTEMPTS):
            try:
                current = self.client.stat(name)
                self.client.put(name, data, if_generation_match=current.generation if current else 0)
                return
            except GcsAmbiguous as exc:
                last = str(exc)
                try:
                    stored = self.client.get(name)
                except GcsError:
                    stored = None
                if stored is not None and stored.data == data:
                    return
            except GcsError as exc:
                last = str(exc)
        raise AutoscalerStateError(f"could not save deployment receipt {path.name} to {self.uri}: {last}")

    def restore_receipt(self, path: Path) -> bool:
        """Download the mirrored copy of a receipt that is missing locally; True when ``path`` now exists."""
        if path.exists():
            return True
        if not _RECEIPT_NAME_RE.match(path.name):
            return False
        try:
            obj = self.client.get(self.receipt_object(path.name))
        except GcsError as exc:
            LOG.warning("Could not restore deployment receipt %s from %s: %s", path.name, self.uri, exc)
            return False
        if obj is None:
            return False
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_name(path.name + ".restore")
        temporary.write_bytes(obj.data)
        temporary.replace(path)
        LOG.info("Restored deployment receipt %s from %s", path.name, self.uri)
        return True



# --------------------------------------------------------------------------------------------
# Process-wide store
# --------------------------------------------------------------------------------------------

_STORE: GcsStateStore | None = None


def configure(env: Mapping[str, str] | None = None, *, read_only: bool = False,
              factory: Callable[..., GcsStateStore] = GcsStateStore) -> GcsStateStore | None:
    """Open the store named by ``SPOT_AUTOSCALER_STATE_URI`` (None and no store when unset)."""
    global _STORE
    env = os.environ if env is None else env
    uri = str(env.get(ENV_STATE_URI, "") or "").strip()
    _STORE = factory(uri, read_only=read_only) if uri else None
    return _STORE


def active() -> GcsStateStore | None:
    return _STORE


def reset() -> None:
    global _STORE
    _STORE = None


def check() -> None:
    if _STORE is not None:
        _STORE.check()


def save_receipt_from_env(path: Path, env: Mapping[str, str] | None = None,
                          factory: Callable[..., GcsStateStore] | None = None) -> bool:
    """Called by the deployment manager after it writes a receipt: with ``SPOT_AUTOSCALER_STATE_URI`` in its
    environment (inherited from the autoscaler), upload the receipt before anything else happens. Raises when the
    upload is not proven; False when no Cloud Storage state is configured."""
    env = os.environ if env is None else env
    uri = str(env.get(ENV_STATE_URI, "") or "").strip()
    if not uri:
        return False
    (factory or GcsStateStore)(uri).upload_receipt(Path(path))
    return True
