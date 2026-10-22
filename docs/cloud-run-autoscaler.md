# Serverless edge reconciler

The runner fleet controller operates as a 100% serverless edge reconciler inside the Cloudflare Worker
control plane (`cloudflare/src/reconciler.rs`). On a 5-minute scheduled cron trigger (`#[event(scheduled)]`),
the reconciler queries the Google Compute Engine and GitHub Actions APIs, evaluates runner demand, manages
Spot VM lifecycles, and commits consolidated state to Cloudflare Workers KV (`SPOT_KV`).

<!-- Cloud Run, Cloud Storage, Cloud Scheduler, and Cloudflare are product names. -->
<!-- vale Google.WordList = NO -->
All legacy components, including Cloud Run jobs, Python control VMs, workstation LaunchAgents, and Cloud Storage
state buckets, have been fully decommissioned.

## Decommissioned legacy components

The table below summarizes the retirement of legacy infrastructure and its pure-Rust edge replacement:

| Component | Status | Modern replacement |
| --- | --- | --- |
| GCP Cloud Run job (`spot-runner-autoscaler`) | Decommissioned | Cloudflare Worker scheduled event trigger (`#[event(scheduled)]`) running every 5 minutes |
| GCP Cloud Scheduler job | Decommissioned | Native Cloudflare Worker Cron Triggers configured in `cloudflare/wrangler.toml` (`*/5 * * * *`) |
| Cloud Storage state bucket | Decommissioned | Consolidated JSON document in Cloudflare Workers KV under key `dashboard:state` |
| Python container image (`autoscaler/Dockerfile`) | Decommissioned | Pure-Rust WebAssembly Worker binary compiled with `worker-build --release` |
| Workstation LaunchAgent | Decommissioned | Autonomous edge reconciliation requiring zero workstation execution |
| Python control VMs (`spot-control-*`) | Decommissioned | Serverless edge control plane serving dashboard UI and REST APIs |
<!-- vale Google.WordList = YES -->

## Scheduled cron reconciler design

The edge reconciler executes in Cloudflare's global edge network every 5 minutes:

- **Direct REST API orchestration**: the Worker communicates directly with Google Compute Engine REST APIs
  (`instances.aggregatedList`, `instances.insert`, `instances.delete`) and GitHub Actions REST APIs (`generate-jitconfig`,
  runner registration and deregistration).
- **Service account authentication**: uses an embedded RS256 JWT assertion exchange to obtain Google OAuth 2.0 access
  tokens, caching tokens in Wasm isolate memory for `55m` with zero KV operations.
- **Diff-gated reconciliation**: computes a canonical SHA-256 state fingerprint across active instances and runners.
  On unchanged ticks, the reconciler skips writing to `SPOT_KV`, ensuring write operations stay below 50 writes per day
  (well within Cloudflare's free tier quota of 1,000 writes per day).
- **Floor target maintenance**: ensures at least one 16-vCPU Spot runner host is active for each target in
  `controlled_targets`.
- **Demand scale-up**: launches additional 16-vCPU Spot runner hosts when queued jobs reach or exceed 20 (`queued_jobs >= 20`).
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
  reconciler ticks, and active instance counts.
- **Tail logs**: run `wrangler tail` in the `cloudflare/` directory to stream real-time execution logs for both
  HTTP requests and scheduled cron events.
- **Instance safety gates**: mutating endpoints like `/api/instances/delete` and `/api/instances/scale-region` operate
  in preview-only mode, returning execution plans without modifying live cloud infrastructure.
