# AI agent guidelines and model routing

This file defines subagent model routing for Claude, Gemini, and Codex harnesses, followed by repository-wide engineering, verification, operational safety, and Git review rules for `swack-tools/spot-github-runners`.

## Claude subagent model routing

These model rules apply when launching Claude subagents. Keep the human-selected main-session model unchanged. Delegate bounded, independent tasks when parallel research or isolated implementation is useful; handle small, coupled changes directly.

- Use Opus 5.5 (`opus-5.5`) at High (`high`) effort for more complex coding tasks, ambiguous architecture, security and container admission boundaries, and VM or storage lifecycle safety.
- Use Sonnet 5.5 (`sonnet-5.5`) at High (`high`) effort for cross-module debugging, state-machine or concurrency reasoning, autoscaler and tier policy analysis, and multi-file refactoring.
- Use Sonnet 5.5 (`sonnet-5.5`) at Medium (`medium`) effort for bounded implementation, unit test additions, documentation updates, and focused code reviews with explicit contracts.
- Use Haiku 5.5 (`haiku-5.5`) at High (`high`) effort for basic code identification and grepping scenarios, fast file or symbol discovery, log summarization, and read-only configuration checks.

| Delegated task | Model | Model ID | Effort |
| --- | --- | --- | --- |
| Basic code identification and grepping scenarios, file/symbol lookup, log summarization, and read-only checks | Haiku 5.5 | `haiku-5.5` | High (`high`) |
| Bounded implementation, unit test additions, or focused review with clear contracts | Sonnet 5.5 | `sonnet-5.5` | Medium (`medium`) |
| Cross-module debugging, lock/lease concurrency reasoning, and tier or pricing policy tracing | Sonnet 5.5 | `sonnet-5.5` | High (`high`) |
| More complex coding tasks, ambiguous architecture, admission/security boundaries, and VM/disk recovery safety | Opus 5.5 | `opus-5.5` | High (`high`) |

### Claude delegation rules

- Pass both `model` and `effort` explicitly on subagent launches (`--model` and `--effort` for command-line children) with a self-contained brief covering the goal, file paths, constraints, owned files, and verification commands.
- Select the route by failure impact and state complexity rather than diff size. Escalate with a concrete explanation when a task exceeds its initial tier.
- Verify that `CLAUDE_CODE_EFFORT_LEVEL` or global subagent model overrides do not silently downgrade the requested route; report any mismatch instead of claiming the requested effort was applied.
- If a subagent launch or model request fails due to a transient model, capacity, rate-limit, or transport error, always retry until it succeeds.

## Gemini subagent model routing

These model rules apply when launching Gemini subagents in Google Antigravity, the Gemini command-line tool, or the Google GenAI and Antigravity SDKs. Keep the human-selected main-session model (such as Gemini 4 Argon at High effort) unchanged.

- Use Gemini 4 Argon (`gemini-4-argon`, Antigravity `pro` or `inherit` tier) at High (`high`) effort for more complex coding tasks, ambiguous architecture, security and container admission boundaries, fleet/VM/disk recovery invariants, and final pre-PR verification.
- Use Gemini 3.8 Flash (`gemini-3.8-flash`, Antigravity `flash` tier) at High (`high`) effort for cross-module debugging, multi-file feature or test implementation, pricing and storage catalog logic, and state-transition tracing.
- Use Gemini 3.8 Flash (`gemini-3.8-flash`, Antigravity `flash` tier) at Medium (`medium`) effort for bounded implementation, straightforward unit tests, documentation updates, and focused code reviews with clear contracts.
- Use Gemini 3.5 Flash-Lite (`gemini-3.5-flash-lite`, Antigravity `flash_lite` tier) at High (`high`) effort for basic code identification and grepping scenarios, fast file or symbol inventory, log filtering, and read-only repository lookups.

| Delegated task | Model | Model ID / Antigravity tier | Effort (`thinking_level`) |
| --- | --- | --- | --- |
| Basic code identification and grepping scenarios, file/symbol inventory, log filtering, and read-only lookups | Gemini 3.5 Flash-Lite | `gemini-3.5-flash-lite` (`flash_lite`) | High (`high`) |
| Bounded implementation, unit test additions, documentation updates, and focused review with clear contracts | Gemini 3.8 Flash | `gemini-3.8-flash` (`flash`) | Medium (`medium`) |
| Cross-module debugging, multi-file changes, pricing/storage catalog logic, and state-machine tracing | Gemini 3.8 Flash | `gemini-3.8-flash` (`flash`) | High (`high`) |
| More complex coding tasks, ambiguous architecture, admission/security boundaries, and VM/disk recovery safety | Gemini 4 Argon | `gemini-4-argon` (`pro` / `inherit`) | High (`high`) |

### Gemini delegation rules

- When invoking Antigravity subagents (`invoke_subagent`), select the matching `Model` tier (`flash_lite`, `flash`, `pro`, or `inherit` when the parent session already runs `gemini-4-argon` at `high`) and set `Workspace` to `inherit` for read-only tasks or `share`/`branch` when isolating concurrent writers.
- When configuring Gemini API or SDK subagents directly, set both `model` and `thinking_level` (`low`, `medium`, or `high`) explicitly. Never use deprecated `gemini-2.5-*`, `gemini-2.0-*`, or `gemini-1.5-*` models.
- Give each subagent a self-contained prompt with explicit target paths, invariants to preserve, and verification steps. Keep concurrent writers on disjoint file sets or isolated workspaces.
- If a subagent launch (`invoke_subagent`, `send_message`) or model request fails due to a transient model, capacity, rate-limit, or transport error, always retry until it succeeds.

## Codex subagent and review routing

When delegating to OpenAI Codex subagents or running local Codex reviews, apply the equivalent task routing and keep the human-selected main-session model unchanged:

| Delegated task | Model | Effort |
| --- | --- | --- |
| Basic code identification, file/symbol inventory, grepping scenarios, and mechanical edits | `gpt-6-luna` | `high` |
| Bounded implementation or focused correctness review with clear contracts | `gpt-6.1-sol` | `medium` |
| Cross-module debugging, concurrency/lease reasoning, or PR verification review | `gpt-6.1-sol` | `high` |
| More complex coding tasks, ambiguous architecture, security boundaries, and storage/release safety | `gpt-6-astra` | `high` |

## Repository overview and architecture

This repository provisions and operates persistent GitHub Actions runners and dedicated Rust 1.99 build VMs on GCP (Google's cloud platform) Spot instances, primarily serving `swack-tools/oxidex`, plus persistent local ARM64 Docker runners (`m4air/`).

The architecture is 100% pure Rust across two primary execution tiers:
- **Serverless edge control plane (`cloudflare/`)**: A Cloudflare Worker compiled from Rust to Wasm, backed by Cloudflare KV (`SPOT_KV`). It serves the Web UI dashboard, enforces HMAC-SHA256 session auth, runs the Runner Patch Console, and executes a 5-minute scheduled cron reconciler that queries GCP Compute Engine and GitHub Actions APIs to manage Spot VM capacity.
- **Native host agent (`host-agent/`)**: A pure-Rust multi-call binary deployed to runner hosts and dedicated builders, managing memory lease admission, descriptor-safe cleanup, Docker command wrapping and admission, and dedicated builder toolchain execution.

Zero Python files exist in the repository; all legacy control VMs (`spot-control-*`), `Cloud Run` jobs, and GCS state buckets are decommissioned.

### Primary entry points

| Entry point | Purpose | Creates or changes resources? |
| --- | --- | --- |
| `cloudflare/src/lib.rs` (`#[event(fetch)]`) | Edge Web UI, session auth, login gate, and REST APIs (`/api/overview`, `/api/health`, `/api/scaling`, `/api/admins`, `/api/patch-console/*`) | Read-only; patch save opens a pull request |
| `cloudflare/src/lib.rs` (`#[event(scheduled)]`) | 5-minute cron reconciler maintaining floor targets, scaling on queued backlog (>=20 jobs), cleaning terminated VMs, and syncing KV | Yes (manages GCP Spot VMs and GitHub runner registrations) |
| `host-agent clean-target <path>` | Descriptor-relative safe cleanup of Cargo `target/` directory using `unlinkat`, refusing symlinks | Modifies local workspace files only |
| `host-agent host-admission <subcommand>` | Memory lease daemon (`serve`), reservation (`acquire`), release (`release`), and headroom calculation (`headroom`) | Updates `/var/lib/gha-admission/leases.json` |
| `host-agent docker-wrapper` | Intercept runner container commands, rewrite port publishing to loopback, set `--gw-priority -1`, and manage `socat` relays | Local container networking only |
| `host-agent docker-admission` | Enforce builder container security flags (UID 1001, 16 CPUs, 28 GiB RAM, cap-drop ALL) and reject `/var/run/docker.sock` mounts | Prevents unauthorized container options |
| `host-agent host-launcher` | Dedicated builder launcher, project regular expression check, directory mode `02770`, and descriptor cleanup | Prepares builder host workspace |
| `host-agent builder-entrypoint` | Unprivileged builder container entrypoint verifying Rust 1.99.0 toolchain commit hash and environment | Runs build commands inside container |
| `host-agent worktree-commands` | Concurrent worktree build execution with cgroup v2 metrics collection | Runs worktree build commands |
| `host-agent toolchain-probe` | Probe toolchain binaries, root ownership, and cgroup resource limits | Read-only local system probe |

### Key directories and modules

| Path | Responsibility |
| --- | --- |
| `host-agent/` | Native pure-Rust multi-call binary and library modules (`clean_target`, `admission`, `docker_wrapper`, `docker_admission`, `host_launcher`, `builder_entrypoint`, `worktree_commands`, `toolchain_probe`, `platform`). |
| `cloudflare/` | Pure-Rust Cloudflare Worker crate (`spot-dashboard-edge`, compiled to Wasm) covering UI (`ui.rs`), session/OAuth 2.0 (`auth.rs`), KV state (`state.rs`), patch validation (`patch.rs`), patch PR console (`patch_console.rs`), GCP Compute client (`gcp.rs`), GitHub Actions client (`github.rs`), and scheduled reconciler (`reconciler.rs`). |
| `cloud-init/` | Startup templates for runner hosts (`startup.sh`) and dedicated builders (`builder-startup.sh`) invoking `host-agent` and `jq`. |
| `runner/` | Docker runner image (`Dockerfile`), entrypoint (`entrypoint.sh`), and job admission hooks (`job-started.sh`, `job-completed.sh`). |
| `builder/standalone/` and `builder/dedicated/` | Standalone Rust 1.99.0 builder image, Playwright website audit tools, toolchain probe, worktree command runner, root-owned host launcher, and Docker admission wrapper. |
| `tests/fixtures/` | Canonical test vectors (`patch_vector.json`, `session_vector.json`). |
| `docs/` and `mkdocs.yml` | MkDocs Material documentation site, built and deployed to GitHub Pages by `.github/workflows/docs.yml`. The site is public: describing the architecture is fine, but keep deployment identifiers out of it: project, account, namespace, runner group, internal hostnames other than the public dashboard URL, and any token or secret value. `docs/requirements.txt` holds build-time dependencies only. |

## Codebase invariants and synchronization rules

### Embedded asset parity and test fixtures

Several modules embed or compile canonical assets and fixtures that are tested for exact parity:

- `cloudflare/src/ui.rs` embeds the single-page dashboard HTML, CSS, and client-side JavaScript. Keep embedded UI assets under 200 KiB.
- `tests/fixtures/patch_vector.json` and `tests/fixtures/session_vector.json` define canonical serialization and HMAC-SHA256 signature test vectors. `cloudflare/tests/` verifies these vectors against the Rust implementation.
- `host-agent` multi-call binary dispatches on `argv[0]`. Keep symlink aliases (`clean-target`, `host-admission`, `docker`, `host-launcher`, `builder-entrypoint`, `worktree-commands`, `toolchain-probe`) aligned with subcommands.

### Test suites in pure Rust

- `host-agent/tests/` contains comprehensive unit, concurrency, and adversarial challenger tests verifying descriptor-relative cleanup, lease locking, and Docker admission boundaries.
- `cloudflare/tests/` contains comprehensive unit, session, patch validation, GCP client, and scheduled reconciler quota tests.

### Allowlisted `.gitignore` and `.vale.ini`

- The root `.gitignore` uses a deny-by-default (`*`) rule at the top and explicitly un-ignores tracked paths (`!README.md`, `!AGENTS.md`, `!CLAUDE.md`, `!*.rs`, `!*.toml`, `!*.sh`, `!*.yaml`, and so on). When adding a new root documentation file or non-standard asset path, add a matching `!` allowlist entry in `.gitignore` so Git tracks it cleanly without `git add -f`.
- Register new Markdown documentation files in `.vale.ini` under `BasedOnStyles = Google` and verify them with Vale before committing.

## Operational safety and infrastructure guardrails

- **No unauthorized cloud or live-fleet mutations**: a prompt to edit code or documentation does not authorize creating, modifying, draining, restarting, or deleting GCP VMs, persistent disks, GitHub runner registrations, Cloudflare Workers, KV namespaces, or secrets. Always use read-only commands, test suites, or dry-run previews (`/api/instances/delete`, `/api/instances/scale-region`) unless the user explicitly authorizes a live operational action.
- **Keep runner and dedicated-builder lifecycles separate**:
  - Runner hosts (`oxidex-runners-*`) register GitHub Actions listeners (controller-managed hosts use runner group ID 10 for `swack-tools/oxidex`) and use auto-deleting data disks. Registered listener counts do not equal concurrent memory capacity; `host-agent host-admission` reserves memory before a job starts.
  - Dedicated builders (`builder-*`) fix the shape to `n4-highcpu-16` (16 vCPU, 32 GiB RAM), register zero GitHub runners, run builds inside one unprivileged UID/GID 1001 container capped at 16 CPUs and 28 GiB with no extra swap, and attach a persistent cache disk with `auto-delete=no`. Deleting a builder VM does not delete its cache disk.
- **Preserve admission, security, and recovery boundaries**:
  - Never weaken `host-agent host-launcher`, `host-agent docker-admission`, or `host-agent host-admission` checks. Do not pass the Docker socket into builder containers, broaden directory permissions to bypass an uploader/control account mismatch, or replace direct authenticated SSH for `oxidex-uploader` with `gcloud compute ssh/scp`.
  - Never delete or hand-edit admission lease ledgers (`/var/lib/gha-admission/leases.json`) or their locks to bypass an admission hold.
  - Distinguish host OOM from container OOM and require kernel or cgroup evidence joined to the active job interval before classifying exit code 137.
- **Worker routes deny access by default**: every route in `cloudflare/src/lib.rs` that returns or changes fleet, administrator, patch, template, build, or configuration state, or that uses a Worker secret such as the GitHub token, must check the caller before it loads state.
  - Call `require_administrator` at the top of the route. It returns 401 without a session and 403 unless `auth::may_sign_in` passes for the owner or an added administrator. A valid session alone is not enough, because a removed administrator keeps a signed cookie until it expires.
  - Owner-only routes also compare the caller with `state::OWNER_UID`. Builder routes (claim, heartbeat, complete, and `/api/observability/ingest`) accept a builder queue token, `SPOT_BUILDER_QUEUE_TOKEN`, or an administrator session, and builder tokens never unlock administrator reads.
  - Only the login gate, `/favicon.ico`, `/api/health`, `/api/auth/status`, and the `/auth/*` sign-in and sign-out routes may answer anonymous callers, and they must not return deployment data. Adding a public route needs the user's explicit approval and an entry in `PUBLIC_ROUTES` in `cloudflare/tests/test_route_auth_coverage.rs`.
  - Removing fields from a response is not access control. Refuse the request instead.
  - Never make an authorization check depend on configuration such as `is_production_oauth_configured`. A missing secret must fail closed; mock sign-in runs only with `SPOT_ALLOW_MOCK_AUTH=1` under `wrangler dev`.
  - Insert server data into inline event handlers only through `jsArg(...)`, and into `href` only through `safeUrl(...)`. HTML escaping alone does not protect a JavaScript string inside an attribute.
  - Keep `test_route_auth_coverage` and `test_security_regressions` passing. When either fails, fix the route; never add a marker or a public entry to silence it.
  - When a change touches a route, also check the routes it reads from or depends on, not only the lines in the diff.
- **Label evidence states accurately**: local unit tests, Cloudflare Worker tests, GitHub CI checks, merged commits on `main`, rendered startup payloads, and live VM/container observations prove different things. State the exact command and environment for every verification claim, and never claim a live deployment or workload qualification from a source-only test run.

## Development and verification workflow

### Environment and dependencies

- Control-plane and host-agent components target stable Rust (1.80+) and Cargo, plus `worker-build` for Cloudflare Workers. Do not add runtime dependencies without explicit user approval. The pinned MkDocs packages in `docs/requirements.txt` are build-time only for the documentation site.
- Keep secrets, `.env`, SSH private keys, approved uploader JSON files, rendered launch directories, and operational state outside the Git checkout (for example under `$HOME/oxidex-ops`). If a change adds or renames an environment variable, document it in `.env.example`.

### Running tests and checks

1. **Pure-Rust unit and adversarial test suites**. Run targeted tests while iterating, and run full test suites before committing:
   ```bash
   # Run host-agent test suite
   cargo test --manifest-path host-agent/Cargo.toml

   # Run Cloudflare Worker test suite
   cargo test --manifest-path cloudflare/Cargo.toml
   ```
2. **Cloudflare Worker Wasm build**. Verify release compilation and bundle size (< 3 MiB):
   ```bash
   cd cloudflare && worker-build --release
   ```
3. **Documentation and prose lint**. Documentation follows the Google developer documentation style guide (sentence-case headings, active voice, direct instructions, explicit prerequisites). Synchronize and run Vale before committing documentation changes:
   ```bash
   vale sync
   vale README.md AGENTS.md CLAUDE.md docs/*.md
   ```
   Also verify command-line flags in documentation examples against `--help` output from the corresponding subcommand.


## Git workflow and review gates

A human task prompt authorizes agents to work on an isolated feature branch, commit task changes, push the branch, open a pull request against `main`, resolve review comments, and squash-merge the pull request into `main` once all review and CI gates pass. It does not authorize force-pushing, changing branch protections, or mutating live cloud resources.

### Branch, identity, and account setup

- Develop on a dedicated branch or isolated Git worktree based on the latest `origin/main`. Never commit or push directly to `main`.
- Ensure commits are authored and SSH-signed as `swackhamer` (`user.name=swackhamer`, `user.email=swackhamer@users.noreply.github.com`, `commit.gpgsign=true`). Verify local commit signature headers (`git cat-file commit HEAD` or `git -c gpg.ssh.allowedSignersFile=... log -n 1 --show-signature`) and verify GitHub's verification status on the actual pushed pull request commits (`gh api repos/swack-tools/spot-github-runners/commits/<sha> --jq '.commit.verification'`). A synthetic unsigned commit created inside an ephemeral review sandbox (for example `Codex <codex@openai.com>`) does not establish the published pull request commit's author or signature status.
- When multiple GitHub command-line tool accounts are logged in on the workstation, export the `swackhamer` token for the shell session or prefix every `gh` command (including `gh pr create`, `gh pr view`, `gh pr comment`, `gh pr checks`, and `gh pr merge`) so no operation falls back to another stored account:
  ```bash
  export GH_TOKEN="$(gh auth token --user swackhamer)"
  gh pr status --repo swack-tools/spot-github-runners
  ```
- Stage task files by explicit path (`git add <path>`) so scratch files, local `.vale/styles/`, and unrelated edits are never committed.

### Local review and pull request review loop

1. **Local verification and review before opening a pull request**:
   - Run the full unit test suite and `vale` checks on all modified documentation files.
   - Obtain an independent local review of the complete diff against `origin/main` from Antigravity (via an independent Antigravity subagent or session) or Codex (for example via `codex exec --model gpt-6.1-sol -c model_reasoning_effort=high -c service_tier=default --sandbox read-only --ephemeral review --base origin/main`) and resolve any findings before opening the pull request.
2. **Pull request review loop**:
   - Push the feature branch and open a pull request targeting `main` in `swack-tools/spot-github-runners`.
   - Obtain an independent Antigravity review on the pull request or trigger the GitHub Codex review (`@codex review`).
   - Inspect every review comment against the codebase. During `@codex review`, only fix `P0` and `P1` issues in the pull request branch.
   - For any `P2` or lower findings, open a follow-up GitHub issue in `swack-tools/spot-github-runners` (`GH_TOKEN="$(gh auth token --user swackhamer)" gh issue create --repo swack-tools/spot-github-runners ...`), reply to the review comment stating that the issue should be tracked in a follow-up ticket (linking the created issue), and resolve the review thread via GraphQL.
   - For `P0` and `P1` issues, fix the code, rerun unit tests and Vale checks, push the signed commit, reply to the review comment with the fix and verification evidence, and resolve the review thread.
   - After resolving review comments, obtain a fresh Antigravity review or comment `@codex review` on the pull request if `P0` or `P1` issues were fixed.
   - Once a review pass has zero `P0` or `P1` issues, all review threads are resolved, and required GitHub checks (such as CodeQL) pass, the pull request is good to go (`gtg`) to squash-merge into `main` immediately without waiting for additional re-review cycles on `P2` items.
3. **Squash and merge**:
   - Confirm that `origin/main` has not advanced in a conflicting way and that the reviewed head commit matches the pull request head, then squash-merge the pull request into `main` while pinning the reviewed head commit so the review and CI gate cannot be raced:
     ```bash
     GH_TOKEN="$(gh auth token --user swackhamer)" gh pr merge --repo swack-tools/spot-github-runners \
       --squash --delete-branch --match-head-commit "$REVIEWED_HEAD_SHA"
     ```

## Automated PR review scope (@codex)

- Flag only concrete P0/P1 defects: auth bypasses, secret leaks, panics/crashes, KV quota regressions, or broken API/test contracts.
- Do not flag P2 nitpicks on fallback display defaults, plan-only UI preview endpoints, or doc examples unless they cause a runtime failure or fail `just check`.
- When reviewing a follow-up fix commit, verify only that prior review threads were resolved and no P1 regressions were introduced.
- For any P2 or lower findings that are posted, open a follow-up GitHub issue (ticket) in `swack-tools/spot-github-runners`, reply to the review thread with the ticket link, and resolve the thread. Once a pull request has 0 P0 or P1 issues, all threads are resolved, and required CI checks pass, it is good to go (`gtg`) to squash-merge into `main`.

### Local pre-PR checklist before `gh pr create`

Always run the independent local review gate before opening a pull request (`gh pr create`) so the first pushed commit is already clean:
- Verify null and `NaN` guards on query parameters (such as `window_hours.is_finite()`) and optional JSON or JavaScript fields (such as `inst.pricing` and `origin_ip`).
- Verify that every Worker route, not only new or modified ones, calls `require_administrator` (or an equivalent `may_sign_in`, owner, or builder-token check) or is on the public list, and that `cargo test --manifest-path cloudflare/Cargo.toml --test test_route_auth_coverage` passes.
- Verify that documentation examples respect GCE's 256 KiB `startup-script` metadata limit and do not rely on single-pass `sed` `r` insertions for placeholder substitution.

