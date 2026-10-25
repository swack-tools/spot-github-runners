# Fleet and edge reconciler

The fleet controller runs as an autonomous serverless reconciler inside the Cloudflare Worker edge control plane
(`cloudflare/src/reconciler.rs`). On a 5-minute scheduled cron cadence (`#[event(scheduled)]`), the reconciler queries
the Compute Engine and GitHub Actions APIs, evaluates fleet capacity, manages Spot VM lifecycles, and persists
the consolidated fleet snapshot to Cloudflare Workers KV (`SPOT_KV`).

Runner hosts run the native pure-Rust `host-agent` binary to manage memory leases, container isolation,
and workspace cleanup.

## What the edge reconciler does

<div class="carousel" markdown>
<div class="carousel-track" markdown>
<figure class="shot" markdown>
![Autoscaling and memory admission flowchart](images/flow-reconciler-scaling.svg)
<figcaption markdown>Flowchart showing how edge backlog scaling (`queued_jobs >= 20`) coordinates with on-host memory lease admission (`host-agent admission`) and post-job target cleanup.</figcaption>
</figure>
<figure class="shot" markdown>
![Scale-up summary](images/ui-kpis-scale-up.png)
<figcaption markdown>Dashboard summary cards during a backlog scale-up event where `24` queued jobs trigger a scale-up from `6` to `8` runners.</figcaption>
</figure>
<figure class="shot" markdown>
![Scale-up decision](images/scaling-decision-scale-up.png)
<figcaption markdown>Live Controller Evaluation box showing `SCALE_UP (6 -> 8 CI/CD Runners / 4 VMs)` driven by `[queued_jobs] 24 queued jobs >= threshold 20`.</figcaption>
</figure>
<figure class="shot" markdown>
![Steady state summary](images/ui-kpis.png)
<figcaption markdown>Dashboard summary cards in steady state (`HOLD` at the `6`-runner floor across `3` VMs when queue depth is below threshold).</figcaption>
</figure>
</div>
</div>
<!-- /shot -->

The edge reconciler executes an autonomous control loop on every 5-minute cron trigger:

1. **Inventory discovery**: queries Compute Engine (`instances.aggregatedList`, `disks.aggregatedList`) and GitHub Actions
   runner registrations for the configured organization.
2. **Backlog detection**: queries GitHub Actions workflow runs and jobs to detect queued job backlog.
3. **Floor target maintenance**: ensures at least one 16-vCPU Spot runner host per configured controlled target (`controlled_targets`).
4. **Demand-driven scale-up**: launches additional 16-vCPU Spot runner hosts when queued backlog reaches or exceeds 20 jobs (`queued_jobs >= 20`).
5. **JIT configuration minting**: requests 2-slot Just-in-Time (JIT) configurations from GitHub Actions API (`generate-jitconfig`)
   with registration token fallback.
6. **Preempted VM cleanup**: detects instances in `TERMINATED` status and cleans up orphaned VMs and auto-deleting disks.
7. **Stale runner cleanup**: deletes offline, idle GitHub runner registrations belonging to terminated or missing VMs.
8. **Diff-gated KV synchronization**: computes a SHA-256 state fingerprint and skips `SPOT_KV.put` on unchanged ticks
   unless a coarse heartbeat interval (>=30 minutes) has elapsed, conserving KV write quota (<50 writes/day).

## Registered listeners and concurrent capacity

A runner host registers two GitHub runner listeners, but the listener count is not the limit of concurrent work.
On each host, the `gha-admission` service (`host-agent host-admission`) reserves memory before a job starts. The memory
reservation, not the listener count, enforces concurrency limits.

### Sizing and resource allocation

Runner hosts provision 16-vCPU shapes with at least 32 GiB RAM:

```text
runner_count = 2
runner_cpu = 7 vCPUs each
runner_memory = 14 GiB each
host_overhead = 4 GiB (reserved for OS and Docker services)
```

This configuration ensures each runner container has dedicated vCPU and memory allocations while leaving adequate headroom
for the host kernel, systemd services, and the Docker engine.

## Fast-boot GCP observability and dynamic runner density

<figure class="shot" markdown>
![GCP observability and dynamic runner density flowchart](images/flow-observability-density.svg)
<figcaption markdown>End-to-end architecture showing zero-boot-tax `host-agent telemetry` startup (`127.0.0.1:9464`), asynchronous `google-cloud-ops-agent` installation, and dynamic runner slot scaling.</figcaption>
</figure>
<!-- /shot -->

Spot VMs maximize utilization and minimize boot latency by combining native `host-agent` cgroup v2 telemetry with the [GCP Ops Agent](https://cloud.google.com/stackdriver/docs/solutions/agents/ops-agent):

1. **Zero boot-latency critical path**: during `cloud-init/startup.sh` and `cloud-init/builder-startup.sh`, the pure-Rust `gha-telemetry.service` (`host-agent telemetry serve --listen 127.0.0.1:9464`) starts in under `5 ms` alongside `gha-admission.service`. Runner containers launch and register with GitHub Actions immediately without waiting for `apt-get`.
2. **Asynchronous Ops Agent launch**: after runner containers are online, `systemd-run --unit=gha-ops-agent-bootstrap --no-block` installs and starts `google-cloud-ops-agent` in the background using `/etc/google-cloud-ops-agent/config.yaml`. The Ops Agent scrapes `http://127.0.0.1:9464/metrics` every 30 seconds and tails structured runner logs (`_diag/Worker_*.log`) and `/var/log/gha-host-telemetry.jsonl`.
3. **Dynamic Runner Density (`enable_dynamic_runner_density: true` by default)**: many CI workflows consume only `1.4–2.0 GiB` of true working-set memory (`memory.current - inactive_file`) and `0.8–1.2` vCPU cores on average. When `enable_dynamic_runner_density` is enabled (the default, with opt-out available in the dashboard or `/api/scaling`), the edge control plane and `host-agent telemetry scale-runners` evaluate observed peak working-set RAM and CPU utilization across active workflows and scale up the number of runner containers on each VM:
   - **Memory-bounded slots**: $\lfloor \text{allocatable\_ram} \times (\text{target\_memory\_percent} / 100) / \text{peak\_working\_set\_gib} \rfloor$
   - **CPU-bounded slots**: $\lfloor \text{vcpu} \times (\text{target\_cpu\_percent} / 100) / \text{avg\_cpu\_cores\_per\_runner} \rfloor$
   - **Safety guards**: holds or scales down toward the baseline floor whenever kernel OOM events (`oom_kill > 0`) or swap thrashing (`swap_used > 512 MiB`) are observed, and caps total slots at `max_runners_per_instance`.
4. **Non-disruptive slot expansion**: adding runner slots (`gha-runner-<host>-3`, `-4`, and so on) launches only the new container indices (`seq $((CURRENT + 1)) $RECOMMENDED`) and updates GCE `spot-runner-count` metadata. Existing runner containers and in-flight CI jobs continue executing without interruption.

```mermaid
sequenceDiagram
  participant Boot as cloud-init/startup.sh
  participant HA as host-agent telemetry (:9464)
  participant GHA as GitHub Actions Containers
  participant Ops as google-cloud-ops-agent
  participant Edge as Cloudflare Worker Reconciler

  Boot->>HA: Start gha-telemetry.service (<5 ms)
  Boot->>GHA: Register baseline runner slots (-1..N)
  Boot--)Ops: systemd-run --unit=gha-ops-agent-bootstrap --no-block
  Ops->>HA: Scrape /metrics & tail /var/log/gha-host-telemetry.jsonl
  HA->>Edge: Report working-set RAM, CPU & workflow logs
  Edge->>HA: Evaluate Dynamic Runner Density (enabled by default)
  HA->>GHA: Spawn additional runner slots (N+1..M) with zero job interruption
```

See the [GCP Ops Agent documentation](https://cloud.google.com/stackdriver/docs/solutions/agents/ops-agent) and [Ops Agent configuration guide](https://cloud.google.com/stackdriver/docs/solutions/agents/ops-agent/configuration) for details on host metrics and Prometheus receivers.

## Runner floor maintenance

The reconciler maintains a runner floor for each target in `controlled_targets`. Each entry is an organization or `OWNER/REPO` string (for example `example-org` or `example-org/example-repo`). The reconciler derives the runner group and machine shapes automatically during deployment.

If no active runner host exists for a target, the reconciler immediately provisions a replacement.

## Scale-up and backlog management

<div class="carousel" markdown>
<div class="carousel-track" markdown>
<figure class="shot" markdown>
![Runner scaling policy](images/scaling-runner-policy.png)
<figcaption markdown>CI/CD runner autoscaling policy panel highlighting the `20`-job queued backlog threshold used by the scheduled reconciler alongside scenario evaluation inputs.</figcaption>
</figure>
<figure class="shot" markdown>
![Builder scaling policy](images/scaling-builder-policy.png)
<figcaption markdown>Dedicated Rust builder autoscaling subpanel showing instance floor and ceiling, queued build threshold, CPU threshold, and idle scale-down rules.</figcaption>
</figure>
<figure class="shot" markdown>
![Scaling tab](images/scaling-tab.png)
<figcaption markdown>Full Scaling and OOM Deep-Links tab combining policy configuration on the left with GitHub Actions and kernel journal OOM evidence on the right.</figcaption>
</figure>
</div>
</div>
<!-- /shot -->

When GitHub Actions reports a queued job backlog of 20 or more jobs (`queued_jobs >= 20`), the reconciler triggers
scale-up:

- It evaluates candidate Spot offers across configured regions (`us-central1`, `us-east1`, `us-west1`).
- It provisions an additional 16-vCPU Spot runner host up to the configured fleet capacity cap.
- It attaches a 40 GiB boot disk and a 100 GiB data disk formatted with ext4.
- When the queued backlog clears and runners become idle, the reconciler drains and terminates surplus hosts.

## Admission and memory leases

<figure class="shot" markdown>
![Admission subcommand](images/term-host-agent-admission.png)
<figcaption markdown>Terminal output of `host-agent admission` showing the `serve`, `acquire`, `release`, and `headroom` subcommands that guard `/var/lib/gha-admission/leases.json`.</figcaption>
</figure>
<!-- /shot -->

Runner hosts execute a root-owned host admission daemon managed by systemd (`systemctl status gha-admission`).
The daemon listens on Unix domain socket `/run/gha-admission/socket` and manages the lease ledger:

- **Ledger file**: `/var/lib/gha-admission/leases.json`, guarded by `/var/lib/gha-admission/leases.json.lock`.
- **Peer verification**: the daemon verifies caller process credentials (`SO_PEERCRED`) on Linux to ensure a client
  cannot supply its own budget or release another container's lease.
- **Job acquisition**: `runner/job-started.sh` calls `host-agent host-admission acquire`, blocking if total memory leases
  exceed available headroom.
- **Job release**: `runner/job-completed.sh` calls `host-agent host-admission release` upon job termination.
- **Headroom calculation**: `host-agent host-admission headroom` reports available memory for new jobs.

Never edit or delete `/var/lib/gha-admission/leases.json` or its lock file to bypass an admission hold.

## Descriptor-relative target cleanup

<figure class="shot" markdown>
![Target cleanup](images/term-clean-target.png)
<figcaption markdown>Terminal output demonstrating `host-agent clean-target` performing descriptor-relative `openat`/`unlinkat` cleanup while refusing symbolic links.</figcaption>
</figure>
<!-- /shot -->

After a job completes, `runner/job-completed.sh` executes `host-agent clean-target <workspace_path>`. The tool:

- Opens the workspace path with `O_RDONLY | O_DIRECTORY | O_NOFOLLOW`.
- Cleans the Cargo `target/` directory using descriptor-relative `unlinkat` with `AT_SYMLINK_NOFOLLOW`.
- Refuses to follow symlinks to avoid directory traversal attacks.

## Edge state and quota conservation

The edge reconciler persists state to Cloudflare Workers KV under key `dashboard:state`:

- **Single consolidated document**: replaces fragmented keys with a single JSON document containing overview telemetry,
  active VMs, runner allocations, and pricing catalog.
- **In-isolate memory caching**: a 60-second TTL in-isolate memory cache ensures read requests perform 0 KV writes
  and at most 1 KV read per 60 seconds per isolate.
- **Diff-gated reconciliation**: unchanged ticks perform 0 KV writes, keeping write operations well below Cloudflare's
  free tier limit of 1,000 writes per day.

## Related pages

- [Runner patches](runner-patches.md): configuration manifests and pull request workflow.
- [Dashboard](dashboard.md): edge Web UI overview and administration.
- [Edge hosting](edge-dashboard.md): edge control plane and KV storage on Cloudflare Workers.
- [Commands](commands.md): command-line options and subcommands.
- [Security model](security.md): host admission, Docker wrapper, and container isolation.
- [Operations and troubleshooting](operations.md): monitoring and operational runbooks.
