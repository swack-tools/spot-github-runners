# Project: 100% Rust Serverless Cloudflare Worker Migration

## Architecture
Migration of the Spot GitHub Runners control plane from Python/VM-based architecture (`spot-control-a`/`b`, Cloudflare Tunnels, Cloud Run watchdog jobs, and JS reverse-proxy Worker) to a 100% pure Rust serverless Cloudflare Worker (`cloudflare/`, compiled to `wasm32-unknown-unknown` with `workers-rs`).

### Core Subsystems & Data Flow
1. **Edge Web UI & Auth Subsystem (`cloudflare/src/ui.rs`, `cloudflare/src/auth.rs`)**:
   - Single-page dashboard HTML/CSS/JS embedded directly in Worker memory (<200 KiB) and served at edge in <15 ms.
   - Session authentication: Purpose-derived HMAC-SHA256 (`spot-dashboard/session/v1`) using `__Host-spot_session` cookie.
   - GitHub OAuth: Web code flow with PKCE S256, device flow (RFC 8628), logout, and auth status.
   - Admin authorization: Owner `swackhamer` (ID 619624), added admins (up to 50), and target org/repo permission verification.
2. **Edge State Subsystem (`cloudflare/src/state.rs`) & KV Quota Conservation**:
   - **Single Consolidated KV Document (`dashboard:state`)**: Consolidates all dashboard state, precomputed `/api/overview` payload, admins, scaling profiles, and pricing into a single KV JSON document. Eliminates `origin:active`, `dashboard:overview`, `gcp:token`, etc.
   - **In-Isolate Wasm Memory Cache (`thread_local!`) + Cache API**: Fronts `SPOT_KV` with in-memory caching (`thread_local! { static STATE_CACHE: RefCell<Option<(u64, DashboardBundle)>> }`) with 60s TTL, updated immediately on any local write. Steady-state HTTP traffic performs 0 KV writes and at most 1 KV read per 60s per isolate.
   - **GCP Token in Memory**: 55-minute GCP Service Account OAuth token cached in Wasm isolate memory (0 KV writes/reads).
   - Fast REST APIs (`/api/health`, `/api/overview`, `/api/scaling`, `/api/admins`).
   - Instance action safety gates (`/api/instances/delete`, `/api/instances/scale-region`) enforcing lifecycle plan previews without live VM mutation.
3. **Rust Patch Console Subsystem (`cloudflare/src/patch.rs`, `cloudflare/src/patch_console.rs`)**:
   - Hardware sizing floor validator enforcing minimum 16 vCPU, 32 GiB RAM, 2 runners @ 7 vCPU / 14 GiB RAM, 4 GiB host overhead, and `queued_jobs_threshold >= 20`.
   - Exact canonical sorted compact JSON hashing for `patch_sha256` matching `src/lib/runner_patch.py`.
   - Multi-repo config management for `swack-tools/spot-runners-config` opening pull requests on branch `spot-dashboard/<slug>-<digest>`.
4. **Rust GCP Fleet Controller & Reconciler Subsystem (`cloudflare/src/gcp.rs`, `cloudflare/src/github.rs`, `cloudflare/src/reconciler.rs`)**:
   - Authenticates to GCP via Service Account JSON key minting RS256 JWT assertion and exchanging for OAuth token. Caches token in Wasm isolate memory.
   - Interacts with GCP Compute Engine APIs (`instances.aggregatedList`, `disks.aggregatedList`, `instances.insert`, `instances.delete`, `instances.setMetadata`, `disks.delete`).
   - Interacts with GitHub Actions APIs (minting registration tokens, listing runners, removing offline/stale runners, detecting queued jobs).
   - **Diff-Gated + Rate-Limited KV Writes in `#[event(scheduled)]`**: Compares a canonical state fingerprint against cached state. Unchanged/no-op cron ticks perform **0 `SPOT_KV.put` calls** (< 50 writes/day steady state, preserving the 1,000 writes/day Free Tier quota).
   - Scheduled event trigger (`#[event(scheduled)]`): runs every 5 minutes to maintain floor targets, scale up when `queued_jobs >= 20`, clean up `TERMINATED` VMs, render `cloud-init/startup.sh`, and update the consolidated snapshot in `dashboard:state` only on state changes or coarse heartbeat (>=30-60 min).
5. **Preserved Invariants & Decommissioning**:
   - Host-side runner scripts (`cloud-init/startup.sh`, `runner/`), dedicated builder assets, host admission, and runtime updates remain intact.
   - 15 obsolete control-VM, watchdog, and JS worker files removed.
   - Test proof contract floors (`MIN_MODULES = 82`, `MIN_CASES = 854`) in `tools/infra_python_test_proof.py` preserved.
   - Git review gates: SSH signing with `swackhamer`, PR review loop, and squash merge.

## Feature Inventory
| # | Feature | Description | Milestone | Source |
|---|---------|-------------|-----------|--------|
| 1 | Edge Static UI Serving | Serve single-page dashboard HTML from Worker memory in <15 ms | M1 | Survey R1 |
| 2 | Login Gate HTML | Serve minimal sign-in gate when unauthenticated | M1 | Survey R1 |
| 3 | HMAC-SHA256 Session Token | Signs and verifies session tokens bound to `spot-dashboard/session/v1` matching test vectors | M1 | Survey R1 |
| 4 | Host-Prefix Session Cookie | Cookie `__Host-spot_session` with Secure, Path=/, SameSite=Lax, HttpOnly | M1 | Survey R1 |
| 5 | Bulk Revocation Cut-off | Reject sessions issued prior to `SESSION_NOT_BEFORE` timestamp | M1 | Survey R1 |
| 6 | GitHub OAuth PKCE Flow | Web sign-in redirect with PKCE S256 code challenge and signed state | M1 | Survey R1 |
| 7 | GitHub OAuth Callback | Code exchange with PKCE verifier, state check, and session issuance | M1 | Survey R1 |
| 8 | GitHub Device Flow Code | RFC 8628 device flow initiation returning user code and verification URI | M1 | Survey R1 |
| 9 | GitHub Device Flow Poll | Poll access token using opaque device handle enforcing minimum polling interval | M1 | Survey R1 |
| 10 | Auth Status Endpoint | `/api/auth/status` public route reporting user identity and capabilities | M1 | Survey R1 |
| 11 | Session Logout | `/auth/logout` clearing session cookie | M1 | Survey R1 |
| 12 | Admin Authorization | Enforce owner (`swackhamer`, 619624), added admins, and per-target permission checks | M1 | Survey R1 |
| 13 | Controlled Target Verification | Check admin rights on controlled orgs and repositories | M1 | Survey R1 |
| 14 | Edge Health Endpoint | `/api/health` fast probe returning hydration and reconciliation state | M1 | Survey R1 |
| 15 | Edge Overview Endpoint | `/api/overview` serving precomputed payload from in-memory cache / `dashboard:state` in <15 ms | M1 | Survey R1 |
| 16 | Edge Scaling Query/Update | `/api/scaling` read and update auto-scaling configuration | M1 | Survey R1 |
| 17 | Edge Admin Management | `/api/admins` list, add, and remove administrators in KV state | M1 | Survey R1 |
| 18 | Instance Actions Safety Gate | `/api/instances/delete` and `/api/instances/scale-region` returning plan previews without VM mutation | M1 | Survey R1 |
| 19 | Canonical `patch_sha256` Parity | Exact canonical sorted compact JSON SHA-256 matching Python `runner_patch.py` | M2 | Survey R2 |
| 20 | Hardware Sizing Floor Validation | Enforce >=16 vCPU, >=32 GiB RAM, 2 runners @ 7 vCPU / 14 GiB, 4 GiB overhead, queue >=20 | M2 | Survey R2 |
| 21 | Permitted Machine Types | Restrict runner hosts to `c4d-highcpu-16`, `n4d-highcpu-16`, `n4-highcpu-16`, `c2d-highcpu-16` | M2 | Survey R2 |
| 22 | Multi-Repo Manifest Merge | Merge `defaults.yaml` with `repos/<owner>/<repo>.yaml` | M2 | Survey R2 |
| 23 | Patch Console Bootstrap | `/api/patch-console/bootstrap` listing repository manifests in `swack-tools/spot-runners-config` | M2 | Survey R2 |
| 24 | Patch Console Load | `/api/patch-console/manifest` loading manifest content, base SHA, blob SHA, patch digest | M2 | Survey R2 |
| 25 | Patch Console Validate | `/api/patch-console/validate` validating manifest against defaults and constraints | M2 | Survey R2 |
| 26 | Patch Console Save (Pull Request) | `/api/patch-console/save` creating idempotent branch `spot-dashboard/<slug>-<digest>` and opening PR | M2 | Survey R2 |
| 27 | Rollout Classification & Preview | `/api/patches/preview` classifying hosts into `noop`, `hot_patch`, `recreate_containers`, `replace_vm` | M2 | Survey R2 |
| 28 | Controlled Targets List | Target repository list (`swack-tools`, `xbmc4lyfe/newznab-tmux`, `xbmc4lyfe/rustnzb`, `xbmc4lyfe/nntp-proxy`) | M2 | Survey R2 |
| 29 | GCP Service Account Key Parsing | Parse credentials JSON to extract client email, private key PKCS#8 PEM, and token URI | M3 | Survey R3 |
| 30 | RS256 JWT Assertion Minting | Sign RS256 JWT assertion with RSA private key targeting Google token endpoint | M3 | Survey R3 |
| 31 | GCP OAuth Token Exchange | Exchange RS256 JWT for OAuth2 access token at `https://oauth2.googleapis.com/token` | M3 | Survey R3 |
| 32 | OAuth Token Memory Caching | Cache GCP access token in Wasm isolate memory (zero KV reads/writes) | M3 | Survey R3 + Update |
| 33 | GCE `instances.aggregatedList` | Paginated query of all VM instances across all zones with `returnPartialSuccess=true` | M3 | Survey R3 |
| 34 | GCE `disks.aggregatedList` | Paginated query of persistent disks across all zones to identify orphan disks | M3 | Survey R3 |
| 35 | GCE `instances.insert` | Create Spot runner VM in target zone with 16-vCPU Spot shape, disks, network, and startup script | M3 | Survey R3 |
| 36 | GCE `instances.delete` | Delete VM instances and attached disks | M3 | Survey R3 |
| 37 | GCE `instances.setMetadata` | Scrub `startup-script` metadata containing one-time registration token once online | M3 | Survey R3 |
| 38 | GCE `disks.delete` | Delete orphan unattached runner data disks | M3 | Survey R3 |
| 39 | GitHub Runner Registration Token | Mint single-use runner registration token from GitHub Actions API | M3 | Survey R3 |
| 40 | GitHub Runner Listing | Paginated retrieval of runners registered to target org or repo | M3 | Survey R3 |
| 41 | GitHub Runner Deregistration | Remove offline, idle runner registrations | M3 | Survey R3 |
| 42 | GitHub Runner Labeling | Apply runner labels (`spot`, machine type, region, extra labels) | M3 | Survey R3 |
| 43 | GitHub Queued Jobs Discovery | Query active workflow runs and jobs to count queued backlog | M3 | Survey R3 |
| 44 | Floor Target Maintenance | Ensure at least 1 managed Spot host exists per target in `SPOT_RUNNER_FLOOR_TARGETS` | M3 | Survey R3 |
| 45 | Queued Jobs Backlog Scale-Up | Trigger scale-up of additional Spot runner hosts only when target `queued_jobs >= 20` | M3 | Survey R3 |
| 46 | Startup Script Rendering | Render `cloud-init/startup.sh` replacing 31 placeholders + injecting patch script | M3 | Survey R3 |
| 47 | Multi-Tick Launch State Machine | Track launch lifecycle (`creating` -> `booting` -> `done`) across cron ticks in memory/KV | M3 | Survey R3 |
| 48 | Preempted VM Cleanup | Detect GCE instances in `TERMINATED` status and delete them | M3 | Survey R3 |
| 49 | Stale Runner Registration Cleanup | Delete offline, idle runner registrations belonging to terminated or missing VMs | M3 | Survey R3 |
| 50 | Spot Offer Selection & Ranking | Rank qualifying 16-vCPU Spot offers by daily cost below $1.50/day in allowed regions | M3 | Survey R3 |
| 51 | Diff-Gated KV Snapshot Updates | Skip `SPOT_KV.put` on unchanged cron ticks (< 50 writes/day steady state) | M3 | Survey R3 + Update |
| 52 | Scheduled Cron Trigger Hook | Worker `#[event(scheduled)]` entrypoint executing reconciliation loop every 5 minutes | M3 | Survey R3 |
| 53 | Decommission Control-VM Core | Remove `src/control_vm.py`, `src/lib/control_vm.py`, `cloud-init/control-vm-startup.sh` | M4 | Survey R4 |
| 54 | Decommission Watchdog Modules | Remove `src/control_watchdog_deploy.py`, `src/lib/control_watchdog_deploy.py`, `src/control_watchdog_server.py`, `src/lib/control_watchdog_server.py`, `watchdog/Dockerfile` | M4 | Survey R4 |
| 55 | Decommission CI Workflows | Remove `.github/workflows/dashboard-image.yml`, `.github/workflows/control-watchdog-image.yml` | M4 | Survey R4 |
| 56 | Decommission Legacy Edge JS | Remove `cloudflare/worker.js`, `cloudflare/worker.test.mjs` | M4 | Survey R4 |
| 57 | Decommission Obsolete Tests | Remove `tests/test_control_vm.py`, `tests/test_control_watchdog_deploy.py`, `tests/test_control_watchdog_server.py` | M4 | Survey R4 |
| 58 | Decouple Downstream Python Imports | Decouple `runner_floor.py`, `gcs_control_state.py`, `spot_runner_autoscaler.py`, `test_runner_floor.py`, `test_runner_patch_metadata.py`, `test_gcs_control_state.py` | M4 | Survey R4 |
| 59 | Rust Worker Gitignore Config | Allowlist `cloudflare/Cargo.toml`, `cloudflare/Cargo.lock`, `cloudflare/src/`, `!*.rs`, `!*.toml`; ignore `/target/` | M4 | Survey R4 |
| 60 | Documentation & Nav Update | Rewrite `docs/edge-dashboard.md` for 100% Rust Worker, update `mkdocs.yml` nav, pass `vale` | M4 | Survey R4 |
| 61 | Test Proof Floor Maintenance | Verify 126 remaining test modules and ~2,276 tests satisfy floors in `tools/infra_python_test_proof.py` and `PRODUCER_SHA` | M4 | Survey R4 |
| 62 | Preserved Assets Parity | Verify runner templates, dedicated builder assets, host admission, and runtime updates remain intact | M4 | Survey R4 |
| 63 | End-to-End Test Suite Verification | Validate 100% pass across E2E test suite (Tiers 1-4) published by E2E Testing Track | M5 | Survey R1-R4 |
| 64 | Adversarial Hardening (Tier 5) | White-box adversarial testing and edge case fuzzing | M5 | Survey R1-R4 |
| 65 | Free Tier Resource Budget Gate | Verify compressed wasm bundle size < 3 MiB and CPU execution < 10 ms | M5 | Survey R1 |
| 66 | Git Commit Signing & Identity | Commits authored and SSH-signed as `swackhamer` using `/Users/allen/.ssh/id_es25519_swackhamer` | M5 | Survey R5 |
| 67 | Pull Request Review & Squash Merge | Feature branch, PR opened, independent review loop until approval, squash-merge with pinned SHA | M5 | Survey R5 |
| 68 | In-Isolate Memory Cache (`thread_local!`) | Front `SPOT_KV.get` with 60s in-memory cache to ensure steady-state GETs do 0 KV writes and minimal reads | M1 | User Update |
| 69 | Rate-Limited KV Write Budget | Strict diff gating ensuring steady-state operations use <50 KV writes/day | M3 | User Update |
| 70 | Fast-Boot Golden Image & `sourceInstanceTemplate` Support | Optional `source_image` / `golden_image_family` and `sourceInstanceTemplate` in patch manifests; `skip_serializing_if = "Option::is_none"` preserving canonical `patch_sha256` parity | M2, M3 | User Additive Update |
| 71 | GCE `instances.insert` Template & Golden Image Boot | `instances.insert?sourceInstanceTemplate=...` and golden boot disk override with machineType (>=16 vCPU/32 GiB), zone, labels, metadata | M3 | User Additive Update |
| 72 | GitHub Runner `generate-jitconfig` Support | Mint ephemeral single-use configs via GitHub `generate-jitconfig` for 2 runner slots (7 vCPU/14 GiB each) with registration-token fallback | M3 | User Additive Update |

## Milestones
| # | Name | Scope | Dependencies | Status |
|---|------|-------|-------------|--------|
| M1 | Rust Worker UI, Auth & Edge State | Single-page UI serving (<200 KiB embedded in Worker memory), HMAC-SHA256 session tokens with purpose labels and test vectors, GitHub OAuth (code flow + PKCE S256 + device flow RFC 8628), admin authorization, single consolidated KV document (`dashboard:state`), in-isolate memory caching (`thread_local!`) fronting `SPOT_KV`, and sub-15ms REST API edge serving (`/api/overview`, `/api/scaling`, `/api/admins`, `/api/health`, instance safety gates). | None | DONE |
| M2 | Rust Patch Console & Multi-Repo Templating | Hardware sizing floor validation (16 vCPU / 32 GiB, 2 runners @ 7 vCPU / 14 GiB, 4 GiB overhead, queue threshold >= 20), canonical sorted compact JSON `patch_sha256` hashing with parity against `src/lib/runner_patch.py`, optional fast-boot golden image (`source_image` / `golden_image_family`) and `sourceInstanceTemplate` support (omitted via `skip_serializing_if` to guarantee 100% digest parity), and multi-repo patch console PR workflow (`/api/patch-console/*`) targeting `swack-tools/spot-runners-config`. | M1 | DONE |
| M3 | Rust GCP Fleet Controller & Scheduled Reconciler | GCP Service Account RS256 JWT auth + token caching in Wasm memory (0 KV ops), GCE REST compute client (`instances.aggregatedList`, `disks.aggregatedList`, `instances.insert` with optional `?sourceInstanceTemplate=...` and golden image override, `instances.delete`, `instances.setMetadata`, `disks.delete`), GitHub Actions REST client (2-slot `generate-jitconfig` minting with registration-token fallback, listing, stale runner cleanup, queue check), and scheduled cron trigger (`#[event(scheduled)]`) running multi-tick reconciler, floor targets, scaling, startup rendering, and diff-gated KV writes (<50 writes/day; 0 writes on unchanged ticks). | M1, M2 | DONE |
| M4 | Decommission Obsolete Python Control Modules & Update Invariants | Remove 15 obsolete files (`src/control_vm.py`, `src/lib/control_vm.py`, `cloud-init/control-vm-startup.sh`, watchdog modules, workflows, legacy `worker.js`, obsolete test files), decouple downstream Python references (`runner_floor.py`, `gcs_control_state.py`, `spot_runner_autoscaler.py`, test files), update `.gitignore`, rewrite `docs/edge-dashboard.md`, verify `vale`, and maintain test proof contracts (`infra_python_test_proof.py`, `PRODUCER_SHA`). | M3 | DONE |
| M5 | Dual-Track Final Milestone: E2E Verification & Git Review Gates | Pass 100% of E2E test suite (Tiers 1-4) published by E2E Testing Track (including KV write-budget assertions: 0 writes on unchanged ticks, 0 writes on cached GETs), execute Phase 2 adversarial coverage hardening (Tier 5), verify bundle size < 3 MiB (`worker-build --release`), verify full repository test suite, and execute Git review loop (SSH-signed commits as `swackhamer`, PR, independent review, squash merge). | M1, M2, M3, M4, E2E Testing Track | DONE |

## Interface Contracts

### 1. Edge State Single Consolidated Document (`SPOT_KV` key `dashboard:state`)
- All dashboard state, precomputed overview, admins, scaling policies, and pricing are stored in a **single KV key** (`dashboard:state`).
- Structure:
  ```json
  {
    "schema": "spot-dashboard-snapshot-v1",
    "state_version": 1,
    "writer_slot": "edge",
    "written_at": 1792000000,
    "state_fingerprint": "a1b2c3d4...",
    "state": {
      "project": "homelab-424523",
      "org": "swack-tools",
      "updated_at": 1792000000,
      "global_budget": { ... },
      "profiles": { ... },
      "scaling": { ... },
      "admins": { ... },
      "instances_view": [ ... ],
      "fleet_summary": { ... },
      "fleet_inventory": { ... },
      "rust_builds": [ ... ]
    }
  }
  ```
- **In-Isolate Wasm Memory Cache**:
  ```rust
  thread_local! {
      static STATE_CACHE: RefCell<Option<(u64, DashboardBundle)>> = RefCell::new(None);
      static GCP_TOKEN_CACHE: RefCell<Option<(u64, String)>> = RefCell::new(None);
  }
  ```
  - State cache has 60s TTL; hits avoid `SPOT_KV.get`.
  - GCP token cache has 55-minute TTL; stored in memory only (0 KV writes/reads).
- **Diff-Gated KV Writes**:
  - `#[event(scheduled)]` calculates `state_fingerprint = sha256(canonical_state)`.
  - If `state_fingerprint == cached_fingerprint` and `now - written_at < 1800s`, **skip `SPOT_KV.put` completely** (0 KV writes).

### 2. Session Token Wire Format
- Purpose label: `spot-dashboard/session/v1`
- Derived key: `HMAC-SHA256(secret, "spot-dashboard/session/v1")`
- Format: `base64url_nopad(compact_sorted_json) + "." + hex(HMAC-SHA256(derived_key, base64url_body_ascii))`
- Cookie header: `__Host-spot_session=<token>; Path=/; Max-Age=604800; HttpOnly; Secure; SameSite=Lax`

### 3. Canonical Patch Hashing Format (`patch_sha256`)
- Schema: `spot-runner-patch-v1`
- Canonical dictionary:
  - Excludes `source_commit`, `source_branch`.
  - Removes optional `hardware` keys if `None`: `"runners_count"`, `"runner_vcpu"`, `"runner_ram_gib"`, `"host_overhead_gib"`, `"queued_jobs_threshold"`. Preserves `min_vcpu: null`, `min_ram_gib: null`.
- Output: `SHA-256(json.dumps(dict, sort_keys=True, separators=(',', ':')).encode('utf-8')).hexdigest()`.

### 4. GCP REST Compute API Contract
- Aggregated instances: `GET https://compute.googleapis.com/compute/v1/projects/{project}/aggregated/instances?returnPartialSuccess=true&maxResults=500`
- Insert instance: `POST https://compute.googleapis.com/compute/v1/projects/{project}/zones/{zone}/instances` with Spot provisioning model and rendered `startup-script`.
  - Supports optional query parameter `?sourceInstanceTemplate=projects/{project}/global/instanceTemplates/{template}`.
  - When golden image or instance template is specified, overrides `machineType` (>= 16 vCPU / 32 GiB), `zone`, labels, and `startup-script` metadata.
- Delete instance: `DELETE https://compute.googleapis.com/compute/v1/projects/{project}/zones/{zone}/instances/{instance}?deleteDisk=ALL`

### 5. Fast-Boot Golden Image & `sourceInstanceTemplate` Contracts
- Manifest schema optional fields:
  - `source_image: Option<String>`
  - `golden_image_family: Option<String>`
  - `source_instance_template: Option<String>`
- Serialization rule: All optional fields MUST use `#[serde(skip_serializing_if = "Option::is_none")]`.
- Canonical hashing parity: When these fields are `None`, the resulting canonical dictionary is identical to legacy manifests, producing 100% identical `patch_sha256` digests matching `runner_patch.py`.

### 6. GitHub Runner JIT Config (`generate-jitconfig`) Contract
- Endpoint: `POST https://api.github.com/orgs/{org}/actions/runners/generate-jitconfig` (or `/repos/{owner}/{repo}/actions/runners/generate-jitconfig`)
- Payload:
  ```json
  {
    "name": "runner-slot-1",
    "runner_group_id": 10,
    "labels": ["self-hosted", "Linux", "X64", "spot", "c4d-highcpu-16", "us-central1"],
    "work_folder": "_work"
  }
  ```
- Response: contains base64-encoded `encoded_jit_config`.
- Multi-runner slots: Controller mints 2 distinct JIT configs (one per 7-vCPU / 14-GiB runner slot).
- Injected into VM startup metadata: `runner_1_jitconfig` and `runner_2_jitconfig`.
- Graceful Fallback: If `generate-jitconfig` returns 404/403 or is disabled, controller falls back cleanly to minting GitHub runner registration token (`POST .../actions/runners/registration-token`).

## Code Layout
- `cloudflare/`:
  - `Cargo.toml`: Rust package definition (`spot-dashboard-edge`, `crate-type = ["cdylib", "rlib"]`).
  - `wrangler.toml`: Cloudflare Worker configuration (`compatibility_date = "2026-06-01"`, `SPOT_KV` binding, cron trigger `triggers = { crons = ["*/5 * * * *"] }`).
  - `src/`:
    - `lib.rs`: Worker router, entrypoints (`#[event(fetch)]`, `#[event(scheduled)]`).
    - `ui.rs`: Static dashboard HTML and login gate rendering.
    - `auth.rs`: HMAC session signing/verification, OAuth code/PKCE/device flows, admin verification.
    - `state.rs`: Consolidated `dashboard:state` management, `thread_local!` cache, REST APIs.
    - `patch.rs`: Canonical `patch_sha256` digest computation, hardware sizing floor validation.
    - `patch_console.rs`: Multi-repo config PR creation on `swack-tools/spot-runners-config`.
    - `gcp.rs`: Service Account RS256 JWT assertion minting, in-memory OAuth2 token caching, Compute Engine REST client.
    - `github.rs`: GitHub Actions REST client (runner registration tokens, runner listing, job backlog).
    - `reconciler.rs`: Multi-tick reconciliation loop, floor maintenance, scale-up, diff-gated snapshot updates.
- `src/lib/`:
  - `runner_floor.py`: Floor keeper logic decoupled from `control_vm`.
  - `gcs_control_state.py`: GCS client and lease primitives decoupled from `control_vm`.
  - `spot_runner_autoscaler.py`: Autoscaler decoupled from `control_vm`.
- `tests/`:
  - Unit tests for Python components, test proof verification (`test_infra_python_test_proof.py`).
  - E2E tests (`tests/e2e/`) verifying HTTP contracts, 0-KV-write assertions on unchanged ticks and cached GETs.
