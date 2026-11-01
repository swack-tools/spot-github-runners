# How-to guides

Each guide walks through one task in the dashboard, with a short animation of the whole task and a screenshot for each step. The pictures use example names, such as `example-org` and `ci-owner`, instead of a real deployment's.

## Get started

- [Sign in to the dashboard](sign-in.md): sign in with your GitHub account and a one-time device code.
- [Tour the dashboard](tour-the-dashboard.md): see what each page is for.
- [Add an administrator](add-an-administrator.md): let another GitHub user sign in and manage the fleet.

## Run jobs on cheap Spot runners

Follow these in order the first time:

1. [Find the cheapest hosts for your runners](find-cheap-hosts.md): describe one runner and let the matrix list the hosts that fit, cheapest first.
1. [Launch a runner pool](launch-a-pool.md): start a pool on the host you picked. It spins down to zero hosts when no jobs need it.
1. [Point a workflow at a pool](use-a-pool-in-a-workflow.md): copy the pool's `runs-on` line into a workflow job.
1. [Check on a pool](check-a-pool.md): see the pool start hosts for queued jobs, delete them when they go idle, and report launch errors.

## Change a pool

- [Make a runner setup template](make-a-runner-template.md): write a script that installs extra tools on every runner host.
- [Roll a template onto a running pool](change-a-pool-template.md): switch a pool to a new template without stopping running jobs.
- [Resize or remove a pool](resize-or-remove-a-pool.md): change how many hosts a pool may run, or delete it.

## Build Rust remotely

- [Launch a Rust builder](launch-a-builder.md): start a builder, build from your Mac, and delete it when you're done.
- [Track Rust builds](track-rust-builds.md): see build times, cache hits, and peak memory.

## Watch the fleet

- [Explore the fleet map](explore-the-fleet-map.md): see where hosts run and what they cost.
- [Review instances and cost](review-instances-and-cost.md): filter and sort every host, including deleted ones.
- [Read observability and logs](read-observability-and-logs.md): read host charts and find the job behind a log line.

## Control the fleet

- [Choose regions](choose-regions.md): pick the parts of the world the fleet may use.
- [Set autoscaling and budgets](set-autoscaling.md): cap spending and set what makes the fleet grow.
- [Edit a runner patch](edit-a-runner-patch.md): change what runner hosts install, through a pull request.
- [Save a VM template](save-a-vm-template.md): keep a host's shape for later.

For how pools work underneath, including their limits, see [Launch runners from the dashboard](../runner-pools.md).
