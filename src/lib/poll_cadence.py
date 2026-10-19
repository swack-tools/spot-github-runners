"""Schedule tier observations without adding post-observation work to their wait."""
import math


def tier_poll_delay(state, interval, started_at, finished_at):
    rows = state.get('latest_resources')
    if not isinstance(rows, list) or not rows:
        return interval
    stamps = [row.get('observed_at') if isinstance(row, dict) else None for row in rows]
    if any(isinstance(stamp, bool) or not isinstance(stamp, (int, float))
           or not math.isfinite(stamp) for stamp in stamps):
        return interval
    observed_at = stamps[0]
    if any(stamp != observed_at for stamp in stamps) or not started_at <= observed_at <= finished_at:
        return interval  # Missing/newly stale evidence and clock reversals keep the full wait.
    # The next iteration polls jobs before sampling, so genuine samples remain
    # at least interval apart. Sleeping after health/price work otherwise makes
    # six five-minute observations age out of the unchanged 30-minute history.
    return max(0, interval - (finished_at - observed_at))


# A one-shot check (the Cloud Run job) waits at most this long for the tier sampling boundary.
ONCE_SAMPLE_MAX_WAIT_SECONDS = 120


def once_sample_wait(state, interval, now):
    """Seconds a ``--once`` tier check waits so its sample lands at least ``interval`` after the last accepted one.

    The tier policy ignores a sample taken less than 300 s after its last one and restarts its streak after 450 s.
    A scheduler that starts a check every 5 minutes doesn't keep samples 300 s apart, because the work before the
    sample varies; waiting for the boundary keeps every interval between 300 s and the longest pre-sample work.
    """
    policy = state.get('tier_policy')
    previous = policy.get('timestamp') if isinstance(policy, dict) else None
    if isinstance(previous, bool) or not isinstance(previous, (int, float)) or not math.isfinite(previous):
        return 0
    wait = previous + interval - now
    return wait if 0 < wait <= ONCE_SAMPLE_MAX_WAIT_SECONDS else 0
