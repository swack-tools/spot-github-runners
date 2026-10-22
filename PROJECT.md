# Project: 100% Pure-Rust + Cloudflare Workers migration (zero Python)

## Architecture

Migration of `swack-tools/spot-github-runners` from legacy Python control-plane, runner hooks, and builder utilities to a 100% pure-Rust + Cloudflare Workers + Cloudflare KV architecture:
- **Zero Python files**: All 235 `.py` files in `src/`, `tools/`, `tests/`, `runner/`, `builder/` removed (`git ls-files '*.py' == 0`).
- **Host agent crate (`host-agent/`)**: Dedicated native pure-Rust crate replacing host and container Python scripts (`clean-target`, `host-admission`, `docker-wrapper`, `docker-admission`, `host-launcher`, `builder-entrypoint`, `worktree-commands`, `toolchain-probe`), preserving all admission and security invariants with macOS mock fallback for local developer testing.
- **Container and startup assets**: `cloud-init/startup.sh`, `cloud-init/builder-startup.sh`, `runner/Dockerfile`, `runner/job-*.sh`, `builder/**/Dockerfile` converted to invoke `host-agent` binaries and POSIX `jq` with zero `python3` dependencies.
- **Edge control plane (`cloudflare/`)**: Pure-Rust Cloudflare Worker on `https://spot.swacktech.com` backed by `SPOT_KV` (`dashboard:state`), with `60s` in-isolate memory caching, `55m` GCP token caching, and diff-gated scheduled reconciler (<50 KV writes/day).
- **Test architecture**: Comprehensive pure-Rust tests across `cloudflare/tests/` and `host-agent/tests/`, preserving compile-time fixtures in `tests/fixtures/`.
- **Documentation and invariants**: Update `README.md`, `AGENTS.md`, `CLAUDE.md`, `PROJECT.md`, `justfile`, `.gitignore`, `docs/*.md` with Vale passing at 0 errors, 0 warnings.
- **Deployment and pull request review gates**: Release Wasm bundle < 3 MiB, redeploy to production edge, SSH-signed commits by `swackhamer`, pull request review, and squash-merge.

## Feature inventory

| # | Feature | Description | Milestone | Source |
|---|---------|-------------|-----------|--------|
| 1 | Pure-Rust `clean-target` | Descriptor-relative `openat`/`unlinkat` cleanup, refusing symlinks (`O_NOFOLLOW`) | M1 | Survey R2 |
| 2 | Pure-Rust `host-admission` | Memory lease reservation, release, headroom, HTTP/socket server on `/var/lib/gha-admission/leases.json` | M1 | Survey R2 |
| 3 | Pure-Rust `docker-wrapper` | Runner network container interception, port remapping to 127.0.0.1, `--gw-priority -1`, socat relay | M1 | Survey R2 |
| 4 | Pure-Rust `docker-admission` | Strict Docker flags (`--user=1001:1001`, `--cpus=16`, `--memory=28g --memory-swap=28g`, rejecting `/var/run/docker.sock`) | M1 | Survey R2 |
| 5 | Pure-Rust `host-launcher` | Dedicated builder launcher, project regular expression check, directory mode `02770`, descriptor-relative cleanup | M1 | Survey R2 |
| 6 | Pure-Rust `builder-entrypoint` | Standalone builder entrypoint, compiler commit hash validation, env configuration | M1 | Survey R2 |
| 7 | Pure-Rust `worktree-commands` | Concurrent worktree runner, cgroup v2 metrics collection, aggregate JSON output | M1 | Survey R2 |
| 8 | Pure-Rust `toolchain-probe` | Toolchain probe, root ownership checks, JSON output (`builder-tools-actual-v1`) | M1 | Survey R2 |
| 9 | macOS mock fallbacks | Conditional compilation (`#[cfg(target_os = "linux")]` / `#[cfg(not(target_os = "linux"))]`) for SO_PEERCRED and `/proc` | M1 | Survey R2 |
| 10 | `cloud-init/startup.sh` port | Install pure-Rust `host-agent`, replace Python heredocs with `jq`, systemd service | M2 | Survey R2 |
| 11 | `cloud-init/builder-startup.sh` port | Replace 9 Python heredocs with pure-Rust `host-agent` subcommands and `jq` | M2 | Survey R2 |
| 12 | `runner/Dockerfile` port | Remove `python3` and `pip`, copy `host-agent`, retain `socat` | M2 | Survey R2 |
| 13 | `runner/job-started.sh` port | Invoke `host-agent acquire` instead of Python | M2 | Survey R2 |
| 14 | `runner/job-completed.sh` port | Invoke `host-agent release` and `host-agent clean-target` instead of Python | M2 | Survey R2 |
| 15 | `builder/**/Dockerfile` port | Remove `python3-venv` and `uv`, copy `host-agent` entrypoint binaries | M2 | Survey R2 |
| 16 | Python file deletion (`git rm`) | Delete all 235 `.py` files across `src/`, `tools/`, `runner/`, `builder/` (`git ls-files '*.py' == 0`) | M3 | Survey R1 |
| 17 | Test Python deletion | Delete `tests/*.py` and `tests/e2e/*.py` while strictly preserving `tests/fixtures/*.json` | M3 | Survey R1 |
| 18 | Obsolete Dockerfiles deletion | Remove `autoscaler/Dockerfile` and `dashboard/Dockerfile` | M3 | Survey R1 |
| 19 | Cache directory removal | Remove all untracked `__pycache__` directories | M3 | Survey R1 |
| 20 | `justfile` Rust recipes | Update `justfile` to build and test pure-Rust crates (`host-agent`, `cloudflare`) | M4 | Survey R4 |
| 21 | `.gitignore` cleanliness | Remove `!*.py`, ensure proper ignore of Rust build targets | M4 | Survey R4 |
| 22 | `.env.example` update | Remove decommissioned control VM, Cloud Run, and GCS variables | M4 | Survey R4 |
| 23 | Root documentation update | Update `README.md`, `AGENTS.md`, `CLAUDE.md`, `PROJECT.md` for 100% Rust architecture | M4 | Survey R4 |
| 24 | `docs/*.md` documentation update | Update all 19 Markdown documents in `docs/` | M4 | Survey R4 |
| 25 | Vale prose verification | Ensure `vale README.md AGENTS.md CLAUDE.md docs/*.md` passes with 0 errors, 0 warnings | M4 | Survey R4 |
| 26 | Requirement-driven E2E test suite | Design and implement pure-Rust Tiers 1-4 test suite across `cloudflare/` and `host-agent/` | E2E | Survey R3 |
| 27 | Cloudflare Worker test pass | Verify all 166 tests pass in `cloudflare/tests/` | M5 | Survey R3 |
| 28 | Host agent test pass | Verify all unit and adversarial tests pass in `host-agent/tests/` | M5 | Survey R3 |
| 29 | E2E test suite 100% pass | Pass 100% of tests published in `TEST_READY.md` | M5 | Survey R3 |
| 30 | Adversarial coverage hardening | Tier 5 adversarial stress testing on host agent and Worker edge | M5 | Survey R3 |
| 31 | Wasm bundle size verification | Verify release Wasm bundle is < 3 MiB (target <= 1.8 MiB) | M5 | Survey R5 |
| 32 | Production edge redeployment | Deploy Wasm Worker to `https://spot.swacktech.com` using `/Users/allen/oxidex-ops/wrangler.deploy.toml` | M5 | Survey R5 |
| 33 | Production edge health verification | Probe `https://spot.swacktech.com/api/health` and verify HTTP 200 and caching | M5 | Survey R5 |
| 34 | Git review and squash-merge gate | Commit and SSH-sign as `swackhamer`, open PR, complete review loop, squash-merge | M5 | Survey R5 |

## Milestones

| # | Name | Scope | Dependencies | Status |
|---|------|-------|-------------|--------|
| M1 | Pure-Rust host agent crate | Create `host-agent/` crate with multi-call dispatcher and subcommands (`clean-target`, `host-admission`, `docker-wrapper`, `docker-admission`, `host-launcher`, `builder-entrypoint`, `worktree-commands`, `toolchain-probe`), preserving all security and admission invariants with macOS mock support. Verify unit tests. | None | DONE |
| M2 | Shell scripts and container ports | Update `cloud-init/startup.sh`, `cloud-init/builder-startup.sh`, `runner/Dockerfile`, `runner/job-started.sh`, `runner/job-completed.sh`, `builder/Dockerfile`, `builder/standalone/Dockerfile` to invoke `host-agent` and `jq` with zero `python3` references. | M1 | DONE |
| M3 | Python deletion and legacy removal | `git rm` all 235 `.py` files across `src/`, `tools/`, `runner/`, `builder/`, `tests/*.py`, `tests/e2e/*.py`. Remove `autoscaler/Dockerfile`, `dashboard/Dockerfile`. Remove untracked `__pycache__`. Preserve `tests/fixtures/*.json`. Verify `git ls-files '*.py' == 0`. | M1, M2 | DONE |
| M4 | Documentation, tooling and Vale lint | Update `README.md`, `AGENTS.md`, `CLAUDE.md`, `PROJECT.md`, `justfile`, `.gitignore`, `.env.example`, `docs/*.md`. Verify `vale` passes with 0 errors and 0 warnings. | M1, M2, M3 | IN PROGRESS |
| M5 | Final milestone: E2E pass, edge deploy and PR | Verify 100% pass of E2E test suite (Tiers 1-4) and Tier 5 adversarial hardening, verify Wasm build < 3 MiB, redeploy to `https://spot.swacktech.com`, verify edge health, author and SSH-sign commit as `swackhamer`, open PR, obtain review, and squash-merge to `main`. | M1, M2, M3, M4, E2E | PLANNED |
| E2E | E2E testing track | Design pure-Rust opaque-box requirement-driven test suite (Tiers 1-4) covering all features. Generate `TEST_INFRA.md` and publish `TEST_READY.md`. | None | PLANNED |

## Interface contracts

### 1. `host-agent` multi-call binary command-line contracts

The binary dispatches based on `argv[0]` or the first positional subcommand:
- `host-agent clean-target <workspace_path>`:
  - Opens `<workspace_path>` with `O_RDONLY | O_DIRECTORY | O_NOFOLLOW`.
  - Cleans `target/` using descriptor-relative `unlinkat` with `AT_SYMLINK_NOFOLLOW`. Refuses to traverse symlinks.
- `host-agent host-admission`:
  - `serve`: binds Unix domain socket `/run/gha-admission/socket` (configurable via `--socket-path`). Manages `/var/lib/gha-admission/leases.json` guarded by `/var/lib/gha-admission/leases.json.lock`.
  - `acquire`: connects to socket, requests memory lease for calling runner, waits for lease confirmation.
  - `release`: connects to socket, releases memory lease for calling runner.
  - `headroom`: calculates available host memory headroom based on total RAM and active leases.
- `host-agent docker-wrapper`:
  - Intercepts `docker create` / `docker run` commands.
  - For `github_network_*` containers, rewrites `-p HPORT:CPORT` to `127.0.0.1::CPORT`.
  - Connects runner network with `--gw-priority -1`.
  - Manages `socat` TCP port forwarders.
- `host-agent docker-admission`:
  - Validates container launch arguments: enforces `--user=1001:1001`, `--cpus=16`, `--memory=28g --memory-swap=28g`, `--pids-limit=4096`, `--cap-drop=ALL`, `--security-opt=no-new-privileges`.
  - Rejects mounts containing `/var/run/docker.sock`. Restricts volume mounts strictly to approved work directories (`/src`, `/target`, `/cargo`).
- `host-agent host-launcher`:
  - Validates project name matching regular expression `^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$`.
  - Prepares work directory with permissions `02770`.
  - Performs descriptor-relative cleanup before starting container.
- `host-agent builder-entrypoint`:
  - Checks `/src/rust-toolchain.toml` for `1.99.0` and validates compiler commit hash `b940084d7eb6a299eb4bfeb8e34901bc051e7ac4`.
  - Configures build environment and executes specified build command.
- `host-agent worktree-commands`:
  - Executes concurrent build tasks, samples cgroup v2 metrics before and after execution, and writes aggregate JSON (`aggregate-worktree-commands-v1`).
- `host-agent toolchain-probe`:
  - Validates toolchain versions and root ownership, checks cgroup limits (`memory.max == 28GiB`, `cpu.max == 16 * period`), and outputs JSON (`builder-tools-actual-v1`).

### 2. Cloudflare Worker edge state and quota contracts

- Consolidated document key: `dashboard:state` in `SPOT_KV`.
- In-isolate cache: `60s` TTL for state bundle, `55m` TTL for GCP OAuth 2.0 token.
- Diff gating: Skip `SPOT_KV.put` on scheduled reconciliation ticks when canonical fingerprint matches cached fingerprint.
- Wire authentication: HMAC-SHA256 (`spot-dashboard/session/v1`) in `__Host-spot_session` cookie.

## Code layout

- `host-agent/`:
  - `Cargo.toml`: Package definition for `host-agent` native binary and library.
  - `src/`:
    - `main.rs`: Multi-call command-line dispatcher.
    - `lib.rs`: Core types, error models, and platform abstractions.
    - `clean_target.rs`: Descriptor-relative target cleanup logic.
    - `admission.rs`: Lease ledger, locking, and socket server/client.
    - `docker_wrapper.rs`: Docker argument rewriting, network routing, and socat management.
    - `docker_admission.rs`: Security validation, flag enforcement, and socket mount rejection.
    - `host_launcher.rs`: Project validation, directory preparation, and descriptor cleanup.
    - `builder_entrypoint.rs`: Toolchain validation and build execution.
    - `worktree.rs`: Cgroup v2 metrics collection and worktree execution.
    - `probe.rs`: Toolchain probe and system verification.
  - `tests/`:
    - `test_clean_target.rs`: Unit and adversarial tests for symlink refusal and descriptor-relative deletion.
    - `test_admission.rs`: Lease reservation, concurrency, and headroom tests.
    - `test_docker_admission.rs`: Flag enforcement, socket rejection, and volume validation tests.
    - `test_docker_wrapper.rs`: Port remapping, gw-priority, and argument rewriting tests.
    - `test_host_launcher.rs`: Project regular expression, directory mode, and cleanup tests.
- `cloudflare/`:
  - `Cargo.toml`, `wrangler.toml`, `src/`, `tests/`.
- `cloud-init/`:
  - `startup.sh`, `builder-startup.sh`.
- `runner/`:
  - `Dockerfile`, `job-started.sh`, `job-completed.sh`.
- `builder/`:
  - `Dockerfile`, `dedicated/`, `standalone/`.
- `tests/fixtures/`:
  - `patch_vector.json`, `session_vector.json`, dedicated builder fixtures (MUST BE PRESERVED).
- `docs/`:
  - All documentation files updated to pure-Rust architecture.
