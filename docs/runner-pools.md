# Launch runners from the dashboard

A runner pool is a set of Spot hosts that you start from the dashboard. You pick a machine from the **Qualifying Instances Matrix**, choose the organization or repository the runners register with, and say how many runners each host runs. By default a pool spins down when it's idle: it runs no hosts until a job asks for its runners, starts hosts for the queued jobs, and deletes hosts that sit idle. The Worker's reconciler does this every 5 minutes, and starts a replacement when Spot takes a host back.

Pools sit next to the fleet floor described in [Fleet and autoscaler](fleet-and-autoscaler.md). The floor keeps one fixed-shape x86 host for each built-in target. A pool can use any machine type that the matrix marks as launchable, including small 2 vCPU hosts and ARM64 Axion hosts, and any organization or repository the Worker's GitHub token can register runners with.

## Before you begin

- Sign in to the dashboard as the owner or as an administrator.
- The Worker's GitHub token needs permission to create runner registration tokens for the target: write access to an organization's self-hosted runners, or write access to a repository's administration settings. Without it the pool shows `runner registration token unavailable`.
- For pools that spin down, the token also needs read access to Actions on the target's repositories, so the reconciler can see queued jobs. Without it the pool's **Status** says it couldn't read GitHub's job queue, and the pool keeps the hosts it has.
- The Worker's GCP service account must be allowed to attach the project's default compute service account to new VMs (`iam.serviceAccounts.actAs`). Without it the pool shows `VM create failed`.

## Find a machine

1. Open **Machine Qualification**.
1. In the CI/CD runner sizing form, describe one runner. For example, for small 2 vCPU runners:

    | Field | Value |
    | --- | --- |
    | **Per-Runner vCPU Requirement** | `2` |
    | **Per-Runner RAM (GiB)** | `6` |
    | **Min Host vCPU** | `2` |
    | **Min Host RAM GiB** | `8` |
    | **Min CPU Benchmark Score** | `0`, or a score your jobs need |

1. The **Qualifying Instances Matrix** updates as you type. It lists the machines that fit, cheapest first, with each host's Spot price per hour, how many runners it holds, and how many hosts the **Minimum # of GitHub Runners** needs.
1. Optional: click **Save CI/CD Runner Profile** so the matrix opens with this sizing next time.

Each host keeps 2 GiB of RAM for itself, so an 8 GiB host holds one runner with 6 GiB.

The prices are GCP's Spot list prices for `us-central1`, per vCPU and per GiB of RAM, plus the data disk when the budget includes disk costs. Spot prices in other regions differ by a few percent, and GCP can change Spot prices at any time, so treat the numbers as estimates.

## Launch a pool

1. In the matrix, select **CI/CD Runner Candidates**, then click **Launch** on the row you want. **Launch** is unavailable for machine types that can't host runners yet; hover over it to see why.
1. In the **Launch runners** dialog, set these fields:

    | Field | What it does |
    | --- | --- |
    | **Organization or repository** | Where the runners register. Pick a listed target, or pick **Other organization or repository** and type `org` or `owner/repo`. Runners for the organization that runs the dashboard join its runner group; runners for any other target join the default group. |
    | **Runners on each host** | How many runners each host runs, 1 to 8. |
    | **vCPU per runner** and **RAM per runner (GiB)** | Each runner's container limits. The runners together must fit the host, which keeps 2 GiB for itself. |
    | **Most hosts** | The most hosts the pool runs at once, 1 to 10. |
    | **Spin down when idle** | On by default. Clear it for a pool that always runs **Most hosts**. |
    | **Fewest hosts** | Hosts the pool keeps even when no jobs run. `0` costs nothing while the pool is idle. |
    | **Idle minutes before spin-down** | How long a host sits without a job before the reconciler deletes it, 5 to 240 minutes. |
    | **Extra labels** | Labels for `runs-on`, separated by commas. Every runner also gets `self-hosted`, `Linux`, `X64` or `ARM64`, `spot`, the machine type, and the zone. |
    | **Template** | Optional extra setup. See [Templates](#templates). |

1. Check the summary at the bottom of the dialog. It shows the hourly cost when the pool is idle and when every host is busy. Click **Launch**.
1. The dialog shows the `runs-on` line for the pool's runners. Click **Copy runs-on**, then click **Done**.

The pool appears in the **Runner pools** panel under the matrix.

## Use the pool in a workflow

Put the pool's labels in the job's `runs-on`. A job goes to a runner that has every label in its `runs-on` list, so add an extra label to the pool, such as `small`, to keep other self-hosted jobs off it:

```yaml
jobs:
  test:
    runs-on: [self-hosted, ARM64, spot, small]
    steps:
      - uses: actions/checkout@v4
      - run: cargo test
```

The **Runner pools** panel shows each pool's `runs-on` line with a **Copy** button.

For step-by-step walkthroughs with pictures, see [Find the cheapest hosts](guides/find-cheap-hosts.md), [Launch a runner pool](guides/launch-a-pool.md), and [Point a workflow at a pool](guides/use-a-pool-in-a-workflow.md).

<figure class="shot walkthrough" markdown>
![Launching a runner pool from the matrix](images/launch-a-pool/launch-a-pool.gif)
<figcaption markdown>Launching a spin-down pool on `n4a-standard-2` for `example-org` with the extra label `small`.</figcaption>
</figure>

## How a pool spins up and down

On every run, every 5 minutes, the reconciler does the following for a pool that spins down when idle:

1. It reads the target's queued jobs and counts the ones whose `runs-on` labels the pool's runners all have.
1. It works out how many hosts the queued and running jobs need. The answer is never less than **Fewest hosts** or more than **Most hosts**.
1. It starts one host if the pool needs more. The host's runners usually take their first job about 10 minutes after the job was queued.
1. It deletes the newest host that has had no job for **Idle minutes before spin-down**, after removing its runner registrations. It never deletes a host whose runner is running a job, or a host that started less than 25 minutes ago and has no runner online yet.

A job therefore waits up to about 15 minutes when the pool has no hosts. Set **Fewest hosts** to `1` for a pool whose jobs can't wait, and pay for one host all the time.

Keep these limits in mind:

- For an organization, the reconciler reads the five repositories pushed most recently, and only those pushed in the last 7 days. It reads up to 10 workflow runs on each run. A job queued in a quieter repository, or behind many other runs, can wait until the scan reaches it. A pool for a single repository reads only that repository.
- When GitHub refuses to show the queue or list the pool's runners, the reconciler keeps the hosts the pool has and starts or deletes none for the queue. The **Status** column says so.
- Jobs that ask for labels the pool doesn't have never start a host. Check that every label in the job's `runs-on` is in the pool's labels.

## Templates

A template adds setup to the runner image of every host in a pool. When you pick a template, its shell commands run as `root` while the host builds the runner image, after the Rust toolchain is installed. Anything the commands install is available to every job on the pool's runners.

### Create a template

<!-- vale Google.WordList = NO -->
<!-- The dashboard's page is named Cloud-Init & Templates. -->

1. Open **Cloud-Init & Templates**.
1. Click **＋ New runner setup script**. The editor opens a starter script that installs two packages:

    ```bash
    #!/bin/bash
    # Runs as root while each runner host builds its runner image,
    # so every job on the pool can use what it installs.
    set -euo pipefail
    apt-get update
    apt-get install -y --no-install-recommends protobuf-compiler jq
    rm -rf /var/lib/apt/lists/*
    ```

1. Name the template, change the commands, and click **Save As New Template & Assign**.
1. The page offers **Go to runner pools**. Pick the template in the **Launch runners** dialog, or roll it onto a running pool as described in the next section.

<!-- vale Google.WordList = YES -->

### Change a running pool's template

1. In **Runner pools**, click **Change** in the pool's **Template** column.
1. Pick a template, or **Built-in runner setup only**, and click **Use this template**.

The pool keeps a copy of the template you pick. On each run, the reconciler replaces one idle host that started before the change with a new host that uses the new template. Busy hosts keep running until they're idle. A pool with no hosts uses the new template for its next host.

Editing a template on the templates page doesn't change pools that use it. Click **Change** and pick the template again to roll out the edited version.

For walkthroughs with pictures, see [Make a runner setup template](guides/make-a-runner-template.md) and [Roll a template onto a running pool](guides/change-a-pool-template.md).

### Template limits

- The template must be a shell script. `#cloud-config` YAML templates are listed but can't be picked, because these hosts don't run cloud-init.
- The host's own setup, such as Docker, the data disk, runner registration, and memory admission, always comes from the built-in bootstrap. A template can't replace it.
- A command that fails stops the image build, and the host never registers its runners. Test a new template on a pool with one host first.

## Check a pool

Each row in **Runner pools** shows:

- **Runners**: the fewest and most hosts, the runners on each host, and how long a host may sit idle.
- **Status**: running hosts and online runners. For a pool that spins down, it also shows queued jobs for the pool and the host count the reconciler is moving to, the last spin-down, and `Spun down · no hosts running` when the pool is idle at zero hosts.
  A red line shows why the last launch failed, for example `VM create failed` when GCP refused the VM, or `runner registration token unavailable` when GitHub refused the token.
  GCP can also refuse a VM a few seconds after it accepts the request. The reconciler reads that outcome on its next run and shows it the same way, for example `VM create failed in us-west1-a: ZONE_RESOURCE_POOL_EXHAUSTED` when the zone has no Spot capacity for the machine. The reconciler keeps trying every 5 minutes. If the error stays, remove the pool and launch it in another zone.
  The red line clears when a new host starts or the pool reaches its host count.
- **Cost**: the estimated Spot price of the hosts running now, and of the pool with every host running.

The **Status** column can lag by up to one reconciler run. Click **Refresh** to reload the panel. It also refreshes every minute while the page is open.

<figure class="shot walkthrough" markdown>
![Checking a pool while jobs run and after it spun down](images/check-a-pool/check-a-pool.gif)
<figcaption markdown>A pool with two hosts running jobs, and the same pool after its hosts sat idle and were deleted. See [Check on a pool](guides/check-a-pool.md).</figcaption>
</figure>

All pools together, at their most hosts, must stay under the hourly limit in the dashboard's **Max Budget** settings. The dashboard refuses a new pool that would go over it.

## Change the number of hosts

1. In **Runner pools**, click **Resize** on the pool's row.
1. Enter the new most hosts, from 1 to 10, and confirm. For a pool that spins down, also enter the fewest hosts. Entering the same number for both turns spin-down off.
1. On its next runs, the reconciler matches the pool to the new numbers:
   - To add hosts, it starts one host per run, every 5 minutes.
   - To remove hosts, it deletes the newest idle hosts first and removes their runner registrations. A host whose runner is running a job stays until the job ends. If GitHub refuses to list the pool's runners, the reconciler waits instead of guessing which hosts are idle.

Growing a pool must keep all pools under the **Max Budget** hourly limit. Shrinking always works.

## Remove a pool

1. In **Runner pools**, click **Remove** on the pool's row and confirm.
1. On its next run, the reconciler deletes the pool's hosts and removes their idle runner registrations. Jobs that are running on those hosts stop.
1. When no hosts or runners are left, the pool disappears from the panel.
   A pool that never started a host disappears on the next run, even if GitHub refuses to list its runners.

To change a pool's machine, runner size, or labels, remove the pool. Then launch a new one.

## What pools don't do yet

- Pool hosts use the built-in runner setup and your template only. Runner patches from the configuration repository and [Repository runner specs](repo-runner-specs.md) apply to floor hosts.
- Pool hosts don't count toward the fleet floor or the backlog scale-up limit. When the floor is on, it keeps its own host for each built-in target next to any pools for that target.
- A host whose setup fails stays up and keeps counting toward the pool until you remove the pool. The **Status** column shows its runners as offline.
