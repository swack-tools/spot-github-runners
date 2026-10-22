# Spot runner fleet and dedicated builders

Provision and operate persistent GitHub Actions runners and dedicated Rust build
VMs on GCP (Google's cloud platform) Spot instances. The repository uses a pure-Rust
serverless and host architecture:

- **Edge control plane (`cloudflare/`)**: A pure-Rust Cloudflare Worker on
  `https://spot.swacktech.com` backed by Cloudflare KV (`SPOT_KV`). It provides the
  Web UI dashboard, OAuth 2.0 and session management, Runner Patch Console, and a
  5-minute scheduled reconciler that manages Spot runner capacity and cleans up
  preempted VMs.
- **Host agent (`host-agent/`)**: A native multi-call binary for runner hosts and
  dedicated builder VMs. It handles descriptor-safe workspace cleanup, memory lease
  admission, Docker container isolation, and dedicated Rust 1.99.0 build execution.

The edge reconciler manages runner capacity for configured GitHub organizations and
repositories. Dedicated builders are separate VMs with no GitHub runner registration.
SSH terminates on the host; app builds run as an unprivileged user (UID 1001)
in a resource-limited Docker container with persistent Cargo and target caches.

This guide describes the source on `main`. Merging source does not update a
running controller, container, or VM. Inspect the deployed configuration and
actual tool versions before routing work to a host.

## Documentation

The full documentation is a MkDocs site. The **Documentation** workflow
(`.github/workflows/docs.yml`) builds the pages in `docs/` and deploys them to GitHub
Pages. You can also read the Markdown sources directly.

| Topic | Page |
| --- | --- |
| Overview and architecture | [Home](docs/index.md) |
| Prerequisites, `.env`, first read-only commands | [Getting started](docs/getting-started.md) |
| Spot price tools | [Spot pricing and offers](docs/spot-pricing.md) |
| Create or adopt runner hosts, sizing, strict pools | [Provision runner hosts](docs/provisioning.md) |
| Scaling modes, runner floor, price rotation, drain, admission | [Fleet and autoscaler](docs/fleet-and-autoscaler.md) |
| Patch manifests and hot patching | [Runner patches](docs/runner-patches.md) |
| Sign-in, administrators, patch console | [Dashboard](docs/dashboard.md) |
| Cloudflare Worker, KV state, reconciler, auth | [Edge dashboard and reconciler](docs/edge-dashboard.md) |
| Dedicated Rust builders | [Set up a builder](docs/dedicated-builders.md), [Remote builds and clients](docs/remote-builds.md), and [Runtime updates](docs/builder-runtime-updates.md) |
| Local ARM64 Docker runners | [Local Docker runners](docs/local-arm64-runners.md) |
| Every environment variable | [Configuration](docs/configuration.md) |
| Every command and flag | [Commands](docs/commands.md) |
| Safety rules and common failures | [Operations and troubleshooting](docs/operations.md) |
| Trust boundaries and secrets | [Security model](docs/security.md) |
| Tests, Vale, and the review workflow | [Development](docs/development.md) |

## Repository components and entry points

| Component | Entry point | Purpose | Mutates resources? |
| --- | --- | --- | --- |
| Edge Web UI and REST API | `cloudflare/src/lib.rs` (`#[event(fetch)]`) | Serve the edge dashboard, authenticate sessions via HMAC cookies, and expose read-only overview and scaling endpoints | No |
| Scheduled edge reconciler | `cloudflare/src/lib.rs` (`#[event(scheduled)]`) | Run the 5-minute cron reconciler to maintain floor targets, scale up on queued backlog, clean up terminated VMs, and deregister stale runners | Yes (provisions or terminates Spot VMs when required) |
| Runner Patch Console | `POST /api/patch-console/save` | Validate manifest changes and open pull requests against `swack-tools/spot-runners-config` | Creates Git branch and pull request |
| Host target cleanup | `host-agent clean-target <path>` | Perform descriptor-relative Cargo `target/` cleanup using `unlinkat` while refusing symlink traversal | Modifies local workspace files only |
| Host memory admission | `host-agent admission <subcommand>` | Run the host admission daemon (`serve`), acquire (`acquire`), release (`release`), or inspect headroom (`headroom`) for runner memory leases | Updates `/var/lib/gha-admission/leases.json` |
| Runner Docker wrapper | `host-agent docker-wrapper` (or `docker` symlink) | Intercept runner network container launches, rewrite published ports to loopback, and manage `socat` relays | Local container networking only |
| Builder Docker admission | `host-agent docker-admission` (or `docker` symlink) | Validate container arguments, enforce UID 1001:1001, 16 CPUs, and 28 GiB RAM, and reject `/var/run/docker.sock` mounts | Prevents unauthorized container options |
| Dedicated builder launcher | `host-agent host-launcher` | Validate project name, configure permissions (`02770`), clean sources, and launch build container | Prepares builder host workspace |
| Builder container entrypoint | `host-agent builder-entrypoint` | Verify Rust 1.99.0 toolchain commit hash, set up environment, and run compilation | Runs build commands inside container |
| Worktree command runner | `host-agent worktree-commands` | Execute concurrent worktree builds and sample cgroup v2 metrics | Runs worktree build commands |
| Toolchain verification probe | `host-agent toolchain-probe` | Probe toolchain binaries, root ownership, and cgroup resource limits | Read-only local system probe |

Keep the distinction between registered listeners and active jobs. Twenty
registered runners do not mean that twenty jobs can each use 32 GiB at once.
Admission reserves memory before starting work; the physical host also needs
space for its operating system, services, and idle listeners.

## Quick start

Install Rust 1.80 or later, Cargo, `worker-build`, Git, `gcloud`, and `gh`.
Clone the repository and run the test suites:

```bash
git clone https://github.com/swack-tools/spot-github-runners.git
cd spot-github-runners

# Run host-agent unit and adversarial tests
cargo test --manifest-path host-agent/Cargo.toml

# Run Cloudflare Worker unit and adversarial tests
cargo test --manifest-path cloudflare/Cargo.toml

# Build the release Wasm Worker bundle
cd cloudflare && worker-build --release
```

For edge deployment, configure Cloudflare secrets (`SPOT_KV`, `GCP_SERVICE_ACCOUNT_KEY`,
`SESSION_SECRET`, `GITHUB_CLIENT_ID`, `GITHUB_CLIENT_SECRET`). See
[Getting started](docs/getting-started.md) for the full setup.

## Safety rules

- A request to edit code or documentation does not authorize changing live
  resources. Use read-only commands, test suites, or dry-run previews
  (`/api/instances/delete`, `/api/instances/scale-region`) unless you explicitly
  intend a live operational action.
- Never delete or hand-edit the admission lease ledger (`/var/lib/gha-admission/leases.json`)
  or its lock file (`leases.json.lock`) to bypass an admission hold.
- Maintain container security boundaries: `host-agent docker-admission` strictly
  enforces UID 1001:1001, 16 vCPUs, 28 GiB memory limits, and rejects Docker
  socket mounts (`/var/run/docker.sock`). Never weaken or bypass these checks.
- Reconciler rate limiting and KV quota: `cloudflare/src/reconciler.rs` uses
  diff-gating to avoid writing to `SPOT_KV` when the fleet state is unchanged. Do
  not remove or turn off fingerprint checks.
- Keep untrusted external pull requests off persistent self-hosted runners.

See [Operations and troubleshooting](docs/operations.md) and the
[Security model](docs/security.md).

## Repository layout

| Path | Responsibility |
| --- | --- |
| `host-agent/` | Native pure-Rust multi-call binary and library for runner admission, target cleanup, and builder runtime |
| `cloudflare/` | Pure-Rust Cloudflare Worker (`spot-dashboard-edge`, compiled to Wasm) providing the Web UI, OAuth 2.0, Patch Console, GCP client, and 5-minute scheduled reconciler backed by `SPOT_KV` |
| `cloud-init/` | Canonical startup scripts for runner hosts (`startup.sh`) and dedicated builders (`builder-startup.sh`) invoking `host-agent` and `jq` |
| `runner/` | Docker runner image (`Dockerfile`), entrypoint, and job admission hooks (`job-started.sh`, `job-completed.sh`) |
| `builder/standalone/`, `builder/dedicated/` | Standalone Rust 1.99.0 builder image, Playwright website audit tools, toolchain probe, worktree command runner, and dedicated host launcher |
| `tests/fixtures/` | Test vectors for canonical patch digests (`patch_vector.json`) and session tokens (`session_vector.json`) |
| `m4air/` | Persistent local ARM64 Docker runners |
| `docs/` | MkDocs Material documentation site sources |

## Development

Develop on an isolated branch based on current `main`. Run the full test suites
before you commit:

```bash
# Run host-agent test suite
cargo test --manifest-path host-agent/Cargo.toml

# Run Cloudflare Worker test suite
cargo test --manifest-path cloudflare/Cargo.toml

# Verify Wasm compilation and release bundle size (< 3 MiB)
cd cloudflare && worker-build --release
```

Check documentation with Vale and build the site in strict mode:

```bash
vale sync
vale README.md AGENTS.md CLAUDE.md docs/*.md

python3 -m venv "$HOME/oxidex-ops/docs-venv"
"$HOME/oxidex-ops/docs-venv/bin/pip" install -r docs/requirements.txt
"$HOME/oxidex-ops/docs-venv/bin/mkdocs" build --strict --site-dir "$HOME/oxidex-ops/docs-site"
```

See [Development](docs/development.md) for the build and verification rules and the
pull request review workflow.
