"""Pure helpers for disjoint intervals of unpriced dashboard history.

Inputs map reasons to legacy ``[start, end]`` pairs or lists of such pairs.
Outputs always use nested lists of float bounds, with ``None`` for an open end.
Negative timestamps and zero-length evidence are preserved; malformed, reversed,
boolean, or nonfinite bounds are ignored. Windows use half-open intervals.
"""

from collections.abc import Mapping
import math
from typing import Any


Interval = list[float | None]
Intervals = dict[str, list[Interval]]
_STORAGE_REASONS = frozenset({"suspended_memory", "fallback_storage", "fallback_tax"})
_COMPUTE_REASONS = frozenset({"prior_compute", "fallback_compute"})


def _finite(value: Any) -> bool:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False


def _bounds(raw: Any) -> Interval | None:
    if not isinstance(raw, (list, tuple)) or len(raw) != 2:
        return None
    start, end = raw
    if not _finite(start) or (end is not None and (not _finite(end) or end < start)):
        return None
    return [float(start), None if end is None else float(end)]


def normalize_unpriced_intervals(raw: Any) -> Intervals:
    """Return fresh sorted intervals per reason, merging only overlap or touching bounds.

    Accept legacy pairs and nested lists/tuples. Invalid entries do not discard
    valid siblings. Empty reasons are omitted; no input objects are mutated.
    """
    normalized: Intervals = {}
    if not isinstance(raw, Mapping):
        return normalized
    for reason, value in raw.items():
        if not isinstance(reason, str):
            continue
        legacy = _bounds(value)
        candidates = [legacy] if legacy is not None else (
            [_bounds(pair) for pair in value] if isinstance(value, (list, tuple)) else [])
        ordered = sorted((pair for pair in candidates if pair is not None), key=lambda pair: pair[0])
        merged: list[Interval] = []
        for start, end in ordered:
            if not merged or (merged[-1][1] is not None and start > merged[-1][1]):
                merged.append([start, end])
            elif merged[-1][1] is None or end is None:
                merged[-1][1] = None
            else:
                merged[-1][1] = max(merged[-1][1], end)
        if merged:
            normalized[reason] = merged
    return normalized


def add_unpriced_interval(intervals: dict, reason: str, start: float, end: float | None) -> None:
    """Add evidence and normalize only ``reason`` in place; repeated adds are idempotent.

    An invalid addition leaves existing valid evidence intact. An open interval
    stays open until the caller explicitly closes its bound before normalization.
    """
    current = normalize_unpriced_intervals({reason: intervals.get(reason)}).get(reason, [])
    updated = normalize_unpriced_intervals({reason: current + [[start, end]]})
    if reason in updated:
        intervals[reason] = updated[reason]
    else:
        intervals.pop(reason, None)


def history_components(raw: Any) -> tuple[bool, bool]:
    """Return (compute, storage) evidence flags; unrecognized reasons affect both."""
    reasons = normalize_unpriced_intervals(raw)
    return (any(reason not in _STORAGE_REASONS for reason in reasons),
            any(reason not in _COMPUTE_REASONS for reason in reasons))


def history_in_window(raw: Any, start: float, end: float, include_storage: bool = True,
                      *, include_tax: bool | None = None) -> bool:
    """Check actual disjoint overlap with ``[start, end)`` in the selected mode.

    Compute-only mode excludes storage-only reasons. Empty/reversed windows and
    zero-length intervals do not overlap. Nonfinite window bounds raise ValueError.
    """
    if not _finite(start) or not _finite(end):
        raise ValueError("window bounds must be finite numbers")
    if end <= start:
        return False
    return any(left < end and (right is None or (right > start and right > left))
               for reason, ranges in normalize_unpriced_intervals(raw).items()
               if ((include_storage if include_tax is None else include_tax) if reason == "fallback_tax"
                   else include_storage or reason not in _STORAGE_REASONS)
               for left, right in ranges)


def compact_unpriced_intervals(raw: Any, now: float, retention: float) -> tuple[Intervals, bool, bool]:
    """Return (recent intervals, compute ever, storage ever), flagging before pruning.

    Drop closed intervals ending at or before ``now - retention``. Keep open and
    straddling intervals intact. ``now`` must be finite; retention must also be
    finite and nonnegative. Callers persist lifetime flags across compactions.
    """
    if not _finite(now) or not _finite(retention) or retention < 0:
        raise ValueError("now must be finite and retention must be finite and nonnegative")
    normalized = normalize_unpriced_intervals(raw)
    compute_ever, storage_ever = history_components(normalized)
    cutoff = now - retention
    recent = {}
    for reason, ranges in normalized.items():
        kept = [pair for pair in ranges if pair[1] is None or pair[1] > cutoff]
        if kept:
            recent[reason] = kept
    return recent, compute_ever, storage_ever
