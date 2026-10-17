# Provision runner hosts

`src/spot_runner_manager.py` creates or adopts one Spot runner host. The autoscaler calls the same code when it
launches a host, so everything on this page also describes what the controller does. Read the manager's
output as a plan until you add `--apply`.

## Preview a deployment

The manager checks the requested shape, compatible storage, price, and quota. Without `--apply`, it prints a
plan and doesn't create the VM:

```bash
python3 src/spot_runner_manager.py \
  --project YOUR_PROJECT \
  --org swack-tools \
  --max-cost 0.125 \
  --max-total-daily-usd 3 \
  --instance-type n4-standard-16 \
  --region asia-east2 --zone asia-east2-b \
  --cpu 16 --ram 64 \
  --tier large --tier-slots 20 \
  --tier-job-memory-gib 32 \
  --disk-size-gb 100 \
  --runner-group-id 10 \
  --runner-label spot \
  --runner-prefix oxidex \
  --instance-name oxidex-runners-example
```

`--max-cost` (hourly compute) and `--max-total-daily-usd` (compute plus boot and data storage) are acceptance
limits, not quoted prices. A price must be strictly below the applicable cap: an equal price doesn't qualify.
The example uses runner group 10, which is the group that controller launches use.

`--patch-live` is the exception to the preview rule. It hot-patches a running host immediately and has no preview. Use the
dashboard preview first. See [Runner patches](runner-patches.md).

## Apply the reviewed plan

Run the same command with `--apply` after you verify the plan. Bootstrap installs Docker, configures the
attached storage, builds the runner image, and registers the expected runner names. The manager waits for the
deployment-specific completion marker (up to `--registration-timeout-seconds`, 1200 by default) and then checks
registration, storage placement, and container isolation.

!!! warning "Applying creates billable resources"
    `--apply` creates a VM and a disk. Don't run it unless you intend to change the live fleet.

## Storage

Runner VMs use a 40 GiB boot disk and, by default, a 100 GiB data disk (`--disk-size-gb`). Newer machine
families use Hyperdisk Balanced with NVMe. Older families use Persistent Disk types. Without `--cheapest-storage` or `--disk-type`, an older family gets
`pd-standard` for both disks, and the controller treats a `pd-standard` data disk as incompatible for tier
hosts. `--cheapest-storage` compares the supported prices in the region and picks `pd-balanced` or `pd-ssd` for
the data disk. `--disk-type` selects a data disk type directly. The autoscaler always passes `--cheapest-storage` when it launches a host.

Runner disks use auto-delete, so deleting or losing a Spot VM can also remove its caches. Dedicated builder
cache disks are the opposite: they survive VM deletion. See [Set up a builder](dedicated-builders.md).

## Adopt an existing host

`--existing-instance` is an adoption and bootstrap path, not a harmless probe. It can restart an existing host.
Drain active work and verify the adoption scope before you use it. Never use the fresh dedicated builder
bootstrap on an occupied runner or benchmark host.

Adopting an unlabeled host with `--target` first lists the GitHub runners that are named for that VM under
these scopes: the new target, the default organization, and any registration scope recorded on the VM. The
runner data disk keeps each runner's registration, so the command refuses the adoption when any of those
runners belongs to another scope, or when it can't list a scope. Drain the host and remove those registrations,
or reprovision it, before you adopt it for the new target.

## Runner sizing

For a manual deployment without `--tier`, automatic sizing uses the host overhead that the code reserves
(2 GiB by default for hosts with 8 GiB of RAM or less, 4 GiB up to 16 GiB, and 8 GiB for larger hosts):

```text
runner_count = host_vCPU // 4
runner_memory_GiB = (host_memory_GiB - host_overhead) // runner_count
```

For example, a 32-vCPU, 128 GiB VM gets eight runners with a 15 GiB memory cap each. Automatic sizing gives the
runners equal CPU weights instead of a hard per-runner CPU quota, so a job can use otherwise idle cores. Small
hosts (fewer than 4 vCPUs or 8 GiB of RAM or less) get one runner. `--fixed-runner-sizing` applies
`--runner-count`, `--runner-cpu`, and `--runner-memory-gb` instead, with checks against host capacity.

Tier deployments override manual sizing:

- `--tier` must match the requested `--cpu` and `--ram`.
- `--tier-slots` sets the number of registered listeners. It defaults to the tier's initial slots.
- `--tier-job-memory-gib` sets the job cap and the admission reservation. The command-line default is 15 GiB.
  That default isn't proof that a heavy workload fits: use actual job measurements and out-of-memory (OOM)
  evidence.
- The tier's daily cap limits `--max-total-daily-usd`.

See [Fleet and autoscaler](fleet-and-autoscaler.md) for the tier table and how admission reserves memory.

## Strict heavy runner pools

`--heavy-runner-pool 2` or `--heavy-runner-pool 3` provisions a fixed number of strict CI-only members. Each
member has an 8-vCPU limit and a 32 GiB memory limit, including swap, and registers with the `spot-heavy32`
label. The host must also provide 2 vCPUs and 16 GiB of RAM for host services. A 32-vCPU, 128 GiB host meets
that requirement for either count.

The manager refuses a strict pool when:

- You pass `--tier` or `--remote-builder`: the pool can't share a host with remote builders or override a tier.
- The target is a repository. A strict pool needs an organization runner group.
- The runner group isn't selected-repository scoped, doesn't permit public repositories, or selects any
  repository other than `ORG/oxidex`.

Choose a currently priced Spot zone, and preview the configuration before you add `--apply`:

```bash
python3 src/spot_runner_manager.py \
  --project YOUR_PROJECT --region '<region>' --zone '<zone>' \
  --instance-type n4-standard-32 --cpu 32 --ram 128 \
  --heavy-runner-pool 2 --runner-group-id '<group-id>' \
  --max-cost 0.18 --max-total-daily-usd 4.50
```

The price limits are ceilings, not quotes. A label alone doesn't enforce the limits, so verify the admission
policy and the actual container limits before you publish the `spot-heavy32` label for jobs. For an existing
instance, the manager checks the cached agent identities against the actual GitHub group and label targets, and
it starts a stopped retained host before it reads that state. Turn off incoming work and let active jobs
finish before you apply an update to an existing host: a successful source review isn't a live drain or a
rollout proof.

## Runner images

Runner images include Rust 1.99.0, rustfmt, clippy, and the configured cross-platform standard-library
targets. The tools live under `/opt/runner-rust` in a shared read-only volume that is selected by the immutable
image ID. Cargo downloads and tools that jobs install use the writable `/work/.cargo` cache. Workflow setup must
verify the preinstalled toolchain instead of trying to update the read-only tool home.

After a job, the completed-job hook removes only the validated OxiDex workspace target directory.
Any value of `OXIDEX_CLEAN_TARGET` other than `1` turns that hook off. Custom target directories and dedicated builder targets follow their
own lifecycle.
