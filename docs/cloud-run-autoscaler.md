# Serverless autoscaler

<!-- Cloud Run, Cloud Storage, Cloud Scheduler, and Cloud Logging are product names, not the old "Cloud" shorthand for GCP. -->
<!-- vale Google.WordList = NO -->

The runner autoscaler, `src/spot_runner_autoscaler.py --scaling-mode tiered`, can run as a Cloud Run job
instead of a macOS LaunchAgent loop on a workstation. Cloud Scheduler starts one execution every 5 minutes.
Each execution runs one `--once` check and exits. The controller state lives in Cloud Storage, and the
runner floor lease keeps two checks, or a check and the floor keeper, from acting on one target at once.

This page describes the design, then the deploy and the cutover from the LaunchAgent.

## Design

| Part | Name (defaults) | Role |
| --- | --- | --- |
| Cloud Scheduler job | `spot-runner-autoscaler` | Every 5 minutes, calls the Cloud Run API method `jobs/spot-runner-autoscaler:run` with an OAuth 2.0 access token of `spot-autoscaler-invoker`. No retries. |
| Cloud Run job | `spot-runner-autoscaler` | One task, parallelism 1, no retries, 1 vCPU, 2 GiB, 1-hour task timeout. Runs as `spot-runner-autoscaler`. Its entrypoint is `src/spot_runner_autoscaler_job.py`. |
| Image | `<region>-docker.pkg.dev/<project>/spot-control/spot-runner-autoscaler` | `autoscaler/Dockerfile`: the slim Google Cloud command-line tool image, pinned by digest, plus the GitHub command-line tool (`gh`) from a release tarball pinned by SHA-256, and `src/`, `cloud-init/`, `runner/`, and `builder/`. |
| Controller state | `gs://<project>-spot-control-state/autoscaler/state-<target key>.json` | The JSON document that `--state-file` holds on a workstation, in one object. |
| Deployment receipts | `autoscaler/state-<target key>/receipts/` in the same bucket | Copies of the receipts that a launch writes next to the state file. |
| Lease | `runner-floor/` in the same bucket | The runner floor keeper's per-target lease, which the autoscaler takes too. |
| Secrets | `spot-floor-github-swack-tools`, `spot-runner-autoscaler-ssh-key` | The GitHub token and the autoscaler's SSH private key, mounted as files. |
| Runner host account | `spot-runner-host` | The account that new runner hosts run as. It can only write metrics and logs. |

### Job, not service

The watchdog is a service, because its ticks are short and request-based billing pays only while a request
runs. The autoscaler is a job for three reasons:

- A check that launches a host waits for its runners to register: up to 40 minutes with the remote builder,
  after an offer search of up to 10 minutes. A job task may run for hours. A Cloud Scheduler HTTP call gives up
  after 30 minutes.
- Jobs use instance-based billing, which costs less per vCPU-second than request-based billing and has its own
  free tier. The one-minute minimum doesn't matter, because a check takes more than a minute.
- When Cloud Run stops a timed-out execution, the whole container goes, so no orphaned `gcloud` process keeps
  acting after the lease expires.

### One check at a time

Each execution works in this order:

1. The entrypoint refuses to start, with exit status 2, unless `SPOT_AUTOSCALER_STATE_URI` and
   `SPOT_RUNNER_FLOOR_STATE_URI` are set and no GitHub token is a plain environment variable.
2. It takes the target's runner floor lease. When another execution or the floor keeper holds it, the check
   exits with status 0 and changes nothing. A heartbeat thread renews the lease while the check runs.
3. It reads the state object and remembers its generation. A missing object stops a live check: seed it first.
4. Every state write updates the local copy, then uploads the same JSON with
   `ifGenerationMatch=<the generation it read or last wrote>`. The autoscaler saves its intent before every
   provider change, so the object is current before each change.
5. When a write fails or conflicts, or its outcome stays unknown after a read-back, the state stops being
   durable. From then on, the lease guard raises before any `gcloud`, `gh`, `ssh`, or deployment manager child
   process starts, and the execution exits with status 1.

The tier policy counts a sample only when it comes 300 to 450 seconds after the previous one. The work before
the sample takes a different time in each execution, so an execution that reaches its sample less than 300
seconds after the previous one waits for that boundary, up to 2 minutes, and then reads the runners again.

Between two executions, the lease is free for about 3 minutes. The floor keeper may act then, but it acts only on
a target that has no running managed host, as it does whenever the autoscaler isn't running.

A launch writes a deployment receipt next to the state file. A thread copies new receipts to Cloud Storage every
5 seconds. When an execution dies during a launch, the next one restores the receipt before it reconciles the
pending launch.

### Reach runner hosts

The tier policy reads kernel and cgroup evidence over `gcloud compute ssh`, and drains and admission updates use
it too. The job keeps that path:

- A dedicated ed25519 key lives in Secret Manager. The entrypoint copies it to `~/.ssh/google_compute_engine`
  with mode 0600, so every execution uses the same key.
- The runtime account can't write project metadata, so `gcloud compute ssh` falls back to the runner host's own
  metadata. It adds the key once for each host, and never again for that host. Each connection still logs one
  refused project metadata write, `Updating project ssh metadata...failed`.
- Writing a host's metadata needs `iam.serviceAccounts.actAs` on the account that the host runs as. The job has
  it only for `spot-runner-host`, so it can reach hosts that run as that account or with no account. A host that
  runs as the default Compute Engine account blocks the job's SSH evidence, and the tier policy holds. Replace
  such hosts before the cutover.
- Port 22 must be open to the internet on runner hosts, as the default network's `default-allow-ssh` rule does,
  because Cloud Run has no fixed outbound address.

OS Login would avoid metadata keys, but it needs `enable-oslogin` on every runner host, and project metadata keys,
such as the workstation's, stop working on hosts that turn it on.

### Least privilege

The predefined `roles/compute.instanceAdmin.v1` includes `compute.projects.setCommonInstanceMetadata`. An IAM
condition narrows only instances and disks, and the project always passes it. With that permission, `gcloud
compute ssh` would put the autoscaler's key into project metadata, which every VM in the project trusts. The deploy
therefore creates the custom role `spotRunnerHostAdmin`: the predefined role's permissions without
`compute.projects.setCommonInstanceMetadata`, `compute.instances.osAdminLogin`, `compute.instances.osLogin`, and
every `*.setIamPolicy`. It reads the predefined role when you run `--apply`.

The autoscaler used to create hosts as the project's default Compute Engine account. That account usually holds
broad roles, and acting as it would give the job those roles. The job instead passes
`--host-service-account`, so new hosts run as `spot-runner-host` with only the `logging-write` and
`monitoring-write` scopes. Hosts that already run keep their account.

| Resource | Member | Role | Why |
| --- | --- | --- | --- |
| Project, with the condition that instances and disks are named `oxidex-runners-*` or `runner-data*` | `spot-runner-autoscaler` | `spotRunnerHostAdmin` | Create, start, stop, label, set metadata on, and delete runner hosts and their disks. List zones, machine types, and CPU platforms. |
| Project | `spot-runner-autoscaler` | `roles/monitoring.viewer` | Read the hosts' Ops Agent metrics for the tier policy. |
| Service account `spot-runner-host` | `spot-runner-autoscaler` | `roles/iam.serviceAccountUser` | Create hosts that run as that account, and no other. |
| State bucket | `spot-runner-autoscaler` | `roles/storage.objectAdmin` | The state, the receipts, and the floor leases. |
| The two secrets | `spot-runner-autoscaler` | `roles/secretmanager.secretAccessor` | The GitHub token and the SSH key. |
| Project | `spot-runner-host` | `roles/monitoring.metricWriter`, `roles/logging.logWriter` | The hosts' Ops Agent. |
| Cloud Run job | `spot-autoscaler-invoker` | `roles/run.invoker` | Cloud Scheduler starts executions of this job only. |

### Cost

A check that changes nothing takes about 100 seconds of work: inventory, job tracking for every
`--job-repository`, and the SSH evidence of each host. At 8,640 executions a month and about 120 seconds each,
including the container start, executions with 1 vCPU and 2 GiB of memory use about 1.04 million vCPU-seconds
and 2.07 million GiB-seconds a month:

| Item | Monthly estimate |
| --- | --- |
| CPU: 1.04 million vCPU-seconds, minus 240,000 free, at $0.000018 | About $14 |
| Memory: 2.07 million GiB-seconds, minus 450,000 free, at $0.000002 | About $3 |
| Cloud Scheduler, Secret Manager access, Cloud Storage writes | Under $1 |
| Total | About $18 |

The free tier belongs to the billing account. A check that launches a host runs longer. A Cloud Run service that
runs the loop all the time would cost about $47 a month for the same size.

## Deploy

Prerequisites:

- The [watchdog](edge-dashboard.md#watchdog) is deployed: its bucket, its Artifact Registry repository, its
  image publisher, and its runner floor lease prefix.
- Owner access to the project.
- The [image](#build-the-image) pushed by the image workflow.

### Build the image

The `autoscaler` job of the **Control watchdog image** workflow builds `autoscaler/Dockerfile` with the watchdog's
publisher account. It reuses the `WATCHDOG_*` repository variables and needs two more:

```bash
gh variable set AUTOSCALER_GH_VERSION --repo swack-tools/spot-github-runners --body 'X.Y.Z'
```

```bash
gh variable set AUTOSCALER_GH_SHA256 --repo swack-tools/spot-github-runners --body 'SHA256_OF_THE_TARBALL'
```

`AUTOSCALER_GH_SHA256` is the SHA-256 of `gh_X.Y.Z_linux_amd64.tar.gz` from the GitHub command-line tool's
release page. The run summary prints the image digest.

### Create the SSH key secret

Create the key in a scratch directory and store it from the file, so the key never appears in a command line:

```bash
ssh-keygen -t ed25519 -N '' -C spot-runner-autoscaler -f "$SCRATCH/key"
```

```bash
gcloud secrets create spot-runner-autoscaler-ssh-key --data-file="$SCRATCH/key" \
  --replication-policy=automatic --project=PROJECT
```

```bash
rm "$SCRATCH/key" "$SCRATCH/key.pub"
```

### Write the autoscaler flags

Copy the LaunchAgent's flags into a file outside the checkout, one flag and its value on each line, without
`--state-file`, `--log-file`, and `--github-user`:

```text
--scaling-mode tiered
--maximum-hosts 5
--max-daily-usd 10
--interval-seconds 300
--project PROJECT
--org swack-tools
--remote-builder
--job-repository swack-tools/oxidex
```

The deploy adds `--host-service-account`, and the entrypoint adds `--once` and the state paths.

### Run the deploy

1. Preview everything. This runs nothing:

   ```bash
   PYTHONPATH=src python3 src/spot_runner_autoscaler_deploy.py --project=PROJECT \
     --image=IMAGE_DIGEST --autoscaler-args-file=ARGS_FILE --skip-scheduler
   ```

1. Deploy the job without its scheduler:

   ```bash
   PYTHONPATH=src python3 src/spot_runner_autoscaler_deploy.py --project=PROJECT \
     --image=IMAGE_DIGEST --autoscaler-args-file=ARGS_FILE --skip-scheduler --apply
   ```

1. Follow the [cutover](#cut-over-from-the-launchagent), which deploys the scheduler last.

## Cut over from the LaunchAgent

The LaunchAgent and the job must never both run live checks. The LaunchAgent may not take the floor lease, so the
lease doesn't protect the cutover.

1. List the runner hosts and the account that each runs as. Every host must show no account or
   `spot-runner-host`; see [Reach runner hosts](#reach-runner-hosts):

   ```bash
   gcloud compute instances list --project=PROJECT --filter="name~^oxidex-runners-" \
     --format="table(name,zone.basename(),serviceAccounts[].email.list())"
   ```

1. Run one read-only check in the job. It takes no lease, writes no state, and changes no provider. Its output is
   the tier plan as JSON:

   ```bash
   gcloud run jobs execute spot-runner-autoscaler --region=us-central1 --project=PROJECT \
     --update-env-vars=SPOT_AUTOSCALER_DRY_RUN=1 --wait
   ```

   The check starts from an empty state, because the state object doesn't exist yet, and it proves the image, the
   GitHub token, the IAM bindings, and SSH to the hosts. Read its log in Cloud Logging.

1. Stop the LaunchAgent. `launchctl` sends `SIGTERM`, and the controller saves its state before each provider
   change, so a stop in the middle of a check is safe:

   ```bash
   launchctl bootout "gui/$(id -u)/com.swacktools.spot-runner-autoscaler"
   ```

1. Confirm that it stopped:

   ```bash
   launchctl print "gui/$(id -u)/com.swacktools.spot-runner-autoscaler"
   ```

1. Seed the state object from the final state file. `--if-generation-match=0` refuses to overwrite an existing
   object:

   ```bash
   gcloud storage cp "$HOME/Library/Application Support/Spot Runner Autoscaler/state.json" \
     gs://PROJECT-spot-control-state/autoscaler/state-TARGET_KEY.json --if-generation-match=0
   ```

   The preview prints the exact object name on its `State object:` line.

1. Run one live check and read its log:

   ```bash
   gcloud run jobs execute spot-runner-autoscaler --region=us-central1 --project=PROJECT --wait
   ```

1. Deploy the scheduler:

   ```bash
   PYTHONPATH=src python3 src/spot_runner_autoscaler_deploy.py --project=PROJECT \
     --image=IMAGE_DIGEST --autoscaler-args-file=ARGS_FILE --apply
   ```

1. Move the plist out of `~/Library/LaunchAgents` so that a login doesn't start it again.

### Roll back

1. Pause the scheduler, and wait for a running execution to finish:

   ```bash
   gcloud scheduler jobs pause spot-runner-autoscaler --location=us-central1 --project=PROJECT
   ```

1. Copy the state object back to the state file, or set `SPOT_AUTOSCALER_STATE_URI` and
   `SPOT_RUNNER_FLOOR_STATE_URI` in the workstation's `.env` so that the workstation uses the same object.
1. Install the LaunchAgent again from a current checkout.

## Operate

- **Pause:** `gcloud scheduler jobs pause spot-runner-autoscaler`; resume it with `resume`.
- **One read-only check:** run the job with `--update-env-vars=SPOT_AUTOSCALER_DRY_RUN=1`.
- **Update the image:** run the deploy again with the new digest. The next execution uses it.
- **Logs:** each execution writes to Cloud Logging. A check whose state stopped being durable exits with status 1;
  a refused configuration exits with status 2.
- **State conflicts:** a conflict means that another controller wrote the object. Find that controller before
  you start the scheduler again. Never delete or edit the object to clear a hold.

<!-- vale Google.WordList = YES -->
