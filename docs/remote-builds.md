# Remote builds and clients

This page describes the host launcher on a dedicated builder, the clients that start a build, and where build
data lives. To create a builder, see [Set up a builder](dedicated-builders.md). To change a builder that already
runs jobs, see [Runtime updates](builder-runtime-updates.md). To configure a lightweight laptop or development
box to pick an idle builder and run builds through `just`, `cargo`, VS Code, or an AI coding agent, see
[Client workstation setup](client-workstation-setup.md).

## Host launcher

The host launcher is `/usr/local/bin/example-rust-project-remote-build` (backed by `host-agent host-launcher`). Its configuration is `/etc/example-rust-project-remote-build.json`.
It checks the project identifier, the allowed command, and the image identity. The Docker admission wrapper
(`host-agent docker-admission`) then checks the mounts and limits and takes the admission ledger lease before the container starts. Containers run as UID and GID 1001 without a Docker socket, added capabilities, or
privilege escalation. See the [security model](security.md).

Use the approved uploader account for source transfer. A control account with `sudo` access is not necessarily
allowed to traverse the source directories. Do not change directory permissions to work around an identity
mismatch.

On a host with an already prepared source project, an example build command is:

```bash
sudo -n /usr/local/bin/example-rust-project-remote-build example-rust-project \
  cargo build --release --locked --bin example-rust-project
```

- `prepare` creates the project directories, gives the uploader source access, and gives UID 1001 the
  project's target directory and the shared Cargo directory. It does not erase existing source or stop a
  build. It is refused while the builder drains.
- Every launcher call on a dedicated builder resets the shared parent directories to `root:1001` with mode
  `0710` and refuses symbolic links in those paths.
- `cleanup` removes everything in the project's source directory after it checks the cache locks and
  container mounts. It keeps the targets, and it runs during a drain.
- Transfer the source completely before you start the build. Do not prepare or synchronize a project that
  another build is using.
- Cancellation and reconciliation checks in the admission and Docker adapter paths depend on exact run and
  container identities. They do not authorize a broad container stop.

Two Rust compilations can run inside the one aggregate container, but they share its CPU and memory limits.
They are not two independent 28 GiB reservations. Measure the actual concurrent workload before you claim that
it fits.

## Remote build client workflow

<div class="carousel" markdown>
<div class="carousel-track" markdown>
<figure class="shot" markdown>
![Remote build execution flowchart](images/flow-remote-build-execution.svg)
<figcaption markdown>End-to-end remote build execution flowchart through `host-agent host-launcher`, `docker-admission`, `builder-entrypoint`, and `worktree-commands`.</figcaption>
</figure>
<figure class="shot" markdown>
![Remote build records](images/rust-builds.png)
<figcaption markdown>Rust Builders Telemetry table displaying compile and delivery durations, crate cache hit rates, builder VM metadata, peak RAM, and CPU points (with origin IPs redacted).</figcaption>
</figure>
<figure class="shot" markdown>
![Filter builds](images/rust-builds-vpn-filter.png)
<figcaption markdown>Rust Builders Telemetry table filtered by `vpn` to isolate builds originating from VPN or proxy networks.</figcaption>
</figure>
<figure class="shot" markdown>
![Record a build](images/rust-record-build.png)
<figcaption markdown>Manual and API build telemetry recording form inside the Rust Builders tab.</figcaption>
</figure>
</div>
</div>
<!-- /shot -->

`example-rust-project` repository-owned `just` recipes and worker configuration are maintained in the `example-rust-project` repository. Configure
that client for the approved dedicated host, uploader identity, source, image, and cache before you enable its
route. This repository does not update `example-rust-project` workflow or registry. See
[Simplify remote builds](simplification.md) for the target workflow.

### Execute builds remotely

The build client snapshots tracked source files, synchronizes them to the dedicated builder through authenticated
`rsync` over SSH as `example-rust-project-uploader`, and executes the build using `/usr/local/bin/example-rust-project-remote-build`:

```bash
# Transfer source to builder
rsync -avz --delete \
  --exclude='.git' --exclude='target' \
  ./ example-rust-project-uploader@BUILDER_HOST:/mnt/runner-data/remote-build/sources/example-rust-project/

# Execute remote compilation through host launcher
ssh example-rust-project-uploader@BUILDER_HOST \
  "sudo -n /usr/local/bin/example-rust-project-remote-build example-rust-project cargo build --release --locked"

# Download compiled artifact
scp example-rust-project-uploader@BUILDER_HOST:/mnt/runner-data/remote-build/targets/example-rust-project/release/example-rust-project \
  ./target/remote-linux/release/example-rust-project
```

- Use a dedicated evidence directory for each run to retain verification receipts and artifact hashes.
- Dedicated builders accept only direct authenticated SSH for `example-rust-project-uploader` with pinned host keys. Never use
  `gcloud compute ssh` to bypass uploader isolation.
- Linux artifacts from remote builders do not run natively on macOS. Cross targets and SDK-free framework interfaces
  do not replace native artifact validation. A full ExifTool comparison needs its own pinned oracle, fixtures, and
  qualification workflow: the standalone Dockerfile does not install the full oracle corpus.

## Qualification source admission

Projects with the dedicated qualification contract authenticate signed source, bundle, and run identities before
they import any executable. A bounded importer and supervisor keep protective locks until all descendants are
reaped. The adapter seals executable source separately from writable measurement Git metadata, and it validates
the required mounts and the allowed oracle fetch.

These source controls do not certify an installed deployment. Production qualification still needs evidence from
the actual startup, authority, mounts, process lifetime, disk, network, and workload. Do not mark a worker
qualified from a successful source-only test suite or a bootstrap tool probe.

## Storage and cache lifecycle

<figure class="shot" markdown>
![Cache and memory summary](images/rust-kpis.png)
<figcaption markdown>Rust builder KPI bar highlighting the `87.9%` crate cache hit rate on the persistent NVMe/Hyperdisk volume and the `26.1 GiB` peak RAM high-water mark.</figcaption>
</figure>
<!-- /shot -->

| Data | Host path |
| --- | --- |
| Docker images and cache | `/mnt/runner-data/docker` |
| containerd layers | `/mnt/runner-data/containerd` |
| Runner state | `/mnt/runner-data/instances/<runner>` |
| Runner workspaces | `/mnt/runner-data/work/<runner>` |
| Build source | `/mnt/runner-data/remote-build/sources/<project>` |
| Cargo downloads | `/mnt/runner-data/remote-build/cargo` |
| Per-project targets | `/mnt/runner-data/remote-build/targets/<project>` |
| Exported artifacts (client transactions only, created and removed for each run) | `/mnt/runner-data/remote-build/exports/<run>` |

Mounting only the Docker data root is not enough if containerd still stores layers on the boot disk. Bootstrap
configures both on attached storage. Caches persist across builds on the same surviving disk. That retention
is not a backup, and it does not prove cache reuse across VMs. Do not format, delete, or detach an existing cache
disk to make a failed bootstrap retry succeed.

Builder drains deny new builds while active builds, units, leases, and cache or upload and export locks finish.
Preserve foreign or uncertain work, and record why a drain is blocked. Runner drains and builder drains have
different completion tests. See [Fleet and autoscaler](fleet-and-autoscaler.md).

## Remote build queue and three-artifact offload

When a test suite or multi-crate workspace build takes up to an hour on a laptop—or when multiple developers and AI subagents submit builds concurrently—use the Edge Worker build queue instead of contending for a single SSH lock.

### 1. Enqueue and poll build requests

Submit a build request to `POST /api/rust-builds/enqueue` with the repository, branch, hexadecimal commit SHA, command, and `requested_output` (`log`, `binary`, or `target_zip`):

```bash
curl -sS -X POST https://spot.swacktech.com/api/rust-builds/enqueue \
  -H "Content-Type: application/json" \
  -H "Cookie: $SPOT_SESSION_COOKIE" \
  -d '{
    "repo": "example-org/example-rust-project",
    "branch": "feat/new-parser",
    "commit_sha": "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
    "command": "cargo test --workspace --locked",
    "requested_output": "target_zip",
    "template_id": "rust-builder-default"
  }' | jq
```

The Worker assigns a monotonic build identifier (starting at `#4553`), appends the request to the first-in, first-out (FIFO) queue, evaluates the `rust_builder` scaling policy, and returns:

- `build_id`: numeric build identifier (for example `4553`).
- `queue_position`: position in line (`1`, `2`, and so on while `queued`, or `0` once `running`).
- `eta_seconds`, `eta_hours`, and `eta_human`: estimated wait time calculated from the rolling `average_duration_seconds` of prior builds and the active builder count.
- `scaling_up` and `scaling_summary`: whether the queue depth reached `queued_builds_threshold` and triggered a `SCALE_UP` decision to provision additional `builder-*` Spot VMs up to `max_instances`.

Poll `GET /api/rust-builds/4553` to check the live queue position, remaining ETA, scaling state, and artifact URLs.

### 2. Always-upload three-artifact storage in same-region GCP object storage

Because a Spot `builder-*` VM can shut down or be preempted immediately after a build finishes—and because an operator might inspect the log first and later download the compiled binary or full `target/` directory—the builder packages and uploads all three artifacts before calling `POST /api/rust-builds/{id}/complete`:

| Artifact | Path in bucket (`gs://<bucket>/builds/<id>/`) | Contents |
| --- | --- | --- |
| Build log | `build.log` | Combined `stdout` and `stderr` output from the build or test suite |
| Compiled binaries | `binaries.tar.zst` | Compressed executable binaries from `target/release/` and `target/debug/` |
| Full target archive | `target.tar.zst` | Compressed `target/` directory including `build.log` and build outputs |

Run `host-agent build-package` on the builder VM to stage and upload all three files to a GCP storage bucket in the same region (`us-central1`), where ingress from Compute Engine VMs in the same region incurs zero network transfer charges:

```bash
host-agent build-package \
  --build-id 4553 \
  --workspace /mnt/runner-data/remote-build/sources/example-rust-project \
  --log-file /tmp/build-4553.log \
  --out-dir /mnt/runner-data/remote-build/exports \
  --bucket example-rust-project-build-artifacts \
  --profile release \
  --upload
```

When the build completes, `GET /api/rust-builds/4553` returns `selected_artifact_url` pointing to the artifact that matches `requested_output` (`log`, `binary`, or `target_zip`), while `artifacts` retains all three URLs (`log_url`, `binary_url`, and `target_zip_url`) together with their byte sizes and SHA-256 digests.
