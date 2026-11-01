# Launch runners from the dashboard

A runner pool is a set of Spot hosts that you start from the dashboard. You pick a machine from the **Qualifying Instances Matrix**, choose the organization or repository the runners register with, and say how many runners each host runs. The Worker's reconciler then starts the hosts, keeps the pool at its host count, and starts a replacement when Spot takes a host back.

Pools sit next to the fleet floor described in [Fleet and autoscaler](fleet-and-autoscaler.md). The floor keeps one fixed-shape x86 host for each built-in target. A pool can use any machine type the dashboard allows, including ARM64 Axion hosts, and any organization or repository the Worker's GitHub token can register runners with.

## Before you begin

- Sign in to the dashboard as the owner or as an administrator.
- The Worker's GitHub token needs permission to create runner registration tokens for the target: write access to an organization's self-hosted runners, or write access to a repository's administration settings. Without it the pool shows `runner registration token unavailable`.
- The Worker's GCP service account must be allowed to attach the project's default compute service account to new VMs (`iam.serviceAccounts.actAs`). Without it the pool shows `VM create failed`.

## Launch a pool

1. Open **Machine Qualification** and select **CI/CD Runner Candidates**.
1. Optional: pick an architecture in the filter, then sort the table. For example, click **CPU Benchmark** to sort by benchmark score.
1. Click **Launch** on the row you want. **Launch** is unavailable for machine types that can't host runners yet; hover over it to see why.
1. In the **Launch runners** dialog, set these fields:

    | Field | What it does |
    | --- | --- |
    | **Organization or repository** | Where the runners register. Pick a listed target, or pick **Other organization or repository** and type `org` or `owner/repo`. Runners for the organization that runs the dashboard join its runner group; runners for any other target join the default group. |
    | **Runners on each host** | How many runners each host runs, 1 to 8. |
    | **vCPU per runner** and **RAM per runner (GiB)** | Each runner's container limits. The runners together must fit the host, which keeps 2 GiB for itself. |
    | **Hosts** | How many hosts the pool keeps running, 1 to 10. |
    | **Extra labels** | Labels for `runs-on`, separated by commas. Every runner also gets `self-hosted`, `Linux`, `X64` or `ARM64`, `spot`, the machine type, and the zone. |
    | **Template** | Optional extra setup. See [Templates](#templates). |

1. Check the summary at the bottom of the dialog, then click **Launch**.

The pool appears in the **Runner pools** panel under the table. Within 5 minutes the reconciler starts the first host, and its runners usually come online 5 to 10 minutes after that. The reconciler starts at most one host for each pool on each run, so a pool with several hosts fills up over several runs.

Jobs reach the runners through their labels, for example:

```yaml
jobs:
  build:
    runs-on: [self-hosted, ARM64, spot]
```

## Templates

The template list shows the CI/CD runner templates from the dashboard's templates page. The default, **Built-in runner setup only**, adds nothing.

When you pick a template, its shell commands run as `root` while the host builds the runner image, after the Rust toolchain is installed. Anything the commands install is available to every job on the pool's runners. For example, a template that installs extra packages:

```bash
#!/bin/bash
set -euo pipefail
apt-get update
apt-get install -y --no-install-recommends protobuf-compiler
rm -rf /var/lib/apt/lists/*
```

The dialog shows the first lines of the template you pick. Keep these limits in mind:

- The template must be a shell script. `#cloud-config` YAML templates are listed but can't be picked, because these hosts don't run cloud-init.
- The host's own setup, such as Docker, the data disk, runner registration, and memory admission, always comes from the built-in bootstrap. A template can't replace it.
- The pool keeps a copy of the template from when you launched it. Editing the template later doesn't change the pool; remove the pool and launch it again to use the new version.
- A command that fails stops the image build, and the host never registers its runners. Test a new template on a pool with one host first.

## Check a pool

Each row in **Runner pools** shows:

- **Status**: running hosts out of the pool's host count, and online runners out of the number the pool should have. A red line shows why the last launch failed, for example `VM create failed` when GCP refused the VM, or `runner registration token unavailable` when GitHub refused the token.
- **Cost**: the estimated Spot price of all the pool's hosts per hour, from the same price table as the matrix.

Click **Refresh** to reload the panel. It also refreshes every minute while the page is open.

All pools together must stay under the hourly limit in the dashboard's **Max Budget** settings. The dashboard refuses a new pool that would go over it.

## Remove a pool

1. In **Runner pools**, click **Remove** on the pool's row and confirm.
1. On its next run, the reconciler deletes the pool's hosts and removes their idle runner registrations. Jobs that are running on those hosts stop.
1. When no hosts or runners are left, the pool disappears from the panel.
   A pool that never started a host disappears on the next run, even if GitHub refuses to list its runners.

To change a pool's machine, size, labels, or template, remove the pool. Then launch a new one.

## What pools don't do yet

- Pool hosts use the built-in runner setup and your template only. Runner patches from the configuration repository and [Repository runner specs](repo-runner-specs.md) apply to floor hosts.
- A pool keeps a fixed number of hosts. It doesn't grow with queued jobs.
- Pool hosts don't count toward the fleet floor or the backlog scale-up limit. When the floor is on, it keeps its own host for each built-in target next to any pools for that target.
- A host whose setup fails stays up and keeps counting toward the pool until you remove the pool. The **Status** column shows its runners as offline.
