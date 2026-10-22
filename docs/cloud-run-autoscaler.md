# Serverless edge reconciler

<!-- Cloud Run, Cloud Storage, Cloud Scheduler, and Cloud Logging are product names, not the old "Cloud" shorthand for GCP. -->
<!-- vale Google.WordList = NO -->

The runner autoscaler and fleet management infrastructure has migrated from legacy GCP Cloud Run jobs
and Cloud Storage state buckets to a 100% serverless edge control plane hosted on Cloudflare Workers
(`cloudflare/src/reconciler.rs`).

This page details the completed architectural migration, edge reconciliation lifecycle, KV state persistence,
and quota conservation.

## Architectural migration summary

The table below summarizes the retirement of legacy components and their pure-Rust replacements:

| Legacy component | Status | Modern replacement |
| --- | --- | --- |
| GCP Cloud Run job (`spot-runner-autoscaler`) | Decommissioned | Cloudflare Worker scheduled event trigger (`#[event(scheduled)]`) running every 5 minutes |
| GCP Cloud Scheduler job | Paused and backed up | Native Cloudflare Worker Cron Triggers configured in `cloudflare/wrangler.toml` |
| Cloud Storage state bucket (`gs://<project>-spot-control-state`) | Decommissioned | Consolidated JSON document in Cloudflare Workers KV under key `dashboard:state` |
| Python container image (`autoscaler/Dockerfile`) | Deleted | Pure-Rust WebAssembly Worker binary compiled with `worker-build --release` |
| Workstation LaunchAgent (`com.swacktools.spot-runner-autoscaler`) | Decommissioned | Autonomous edge reconciliation requiring zero workstation execution |

## Scheduled cron reconciler design

The edge reconciler executes in Cloudflare's global edge network every 5 minutes:

- **Direct REST API orchestration**: the Worker communicates directly with Google Compute Engine REST APIs
  (`instances.aggregatedList`, `instances.insert`, `instances.delete`) and GitHub Actions REST APIs (`generate-jitconfig`,
  runner registration/deregistration).
- **Service account authentication**: uses an embedded RS256 JWT assertion exchange to obtain Google OAuth 2.0 access
  tokens, caching tokens in Wasm isolate memory for `55m` with zero KV operations.
- **Diff-gated reconciliation**: computes a canonical SHA-256 state fingerprint across active instances and runners.
  On unchanged ticks, the reconciler skips writing to `SPOT_KV`, ensuring write operations stay below 50 writes per day
  (well within Cloudflare's free tier quota of 1,000 writes per day).
- **Coarse heartbeat**: performs a full state snapshot update at least once every 30 to 60 minutes to refresh telemetry
  and timestamps even when fleet topology is static.

## Edge state and KV persistence

All control plane state is consolidated into a single document in `SPOT_KV`:

- **Key identifier**: `dashboard:state` formatted with schema `spot-dashboard-snapshot-v1`.
- **In-isolate memory cache**: steady-state read traffic (`/api/overview`, `/api/health`, `/api/scaling`) is served
  from a thread-local in-memory cache with a `60s` TTL, generating 0 KV writes and at most 1 KV read per minute
  per isolate.
- **Atomic state updates**: when scale adjustments or administrator mutations occur, the Worker updates the KV key
  and immediately refreshes the local in-isolate cache.

## Operational safety and monitoring

- **Health probe**: `curl https://spot.swacktech.com/api/health` reports the latest edge reconciliation timestamp,
  GCP connection status, and active instance counts.
- **Tail logs**: run `wrangler tail` in the `cloudflare/` directory to stream real-time execution logs for both
  HTTP requests and scheduled cron events.
- **Instance safety gates**: mutating endpoints like `/api/instances/delete` and `/api/instances/scale-region` operate
  in preview-only mode, returning execution plans without modifying live cloud infrastructure.

<!-- vale Google.WordList = YES -->
