# Configuration reference

This page lists every environment variable and configuration setting used across the pure-Rust architecture,
grouped by subsystem. The system comprises two runtime environments: the Cloudflare Worker edge control
plane (`cloudflare/`) and the pure-Rust host agent (`host-agent/`).

For template values that you copy into a file, see
[`.env.example`](https://github.com/swack-tools/spot-github-runners/blob/main/.env.example).

## Credentials and sign-in

<div class="carousel" markdown>
<div class="carousel-track" markdown>
<figure class="shot" markdown>
![Environment template](images/term-env-example.png)
<figcaption markdown>`.env.example` with placeholder values.</figcaption>
</figure>
<figure class="shot" markdown>
![Sign-in page](images/login-gate.png)
<figcaption markdown>The sign-in methods that the OAuth 2.0 settings enable.</figcaption>
</figure>
</div>
</div>
<!-- /shot -->

These variables configure GitHub API access, GCP service account credentials, and OAuth 2.0 authentication
for the edge dashboard.

| Variable | Default | Read by | Meaning |
| --- | --- | --- | --- |
| `GH_TOKEN` | None. | Cloudflare Worker reconciler and `just` tasks. | **Secret.** Renewable GitHub personal access token or GitHub App installation token with organization runner-management permissions. |
| `SPOT_CONFIG_GH_TOKEN` | Empty (falls back to `GH_TOKEN`). | Cloudflare Worker (`cloudflare/src/patch.rs`). | **Secret.** Optional token for the central configuration repository. Read access covers listing and diff previews; opening patch pull requests requires repository write access. |
| `CONFIG_REPO` | Compile-time constant (`cloudflare/src/patch_console.rs`). | Cloudflare Worker. | Pinned configuration repository identifier (for example `example-org/spot-runners-config`). This setting is a compile-time constant, not an environment variable. |
| `GCP_SERVICE_ACCOUNT_KEY` | Empty. | Cloudflare Worker (`cloudflare/src/gcp.rs`). | **Secret.** Service account JSON key with Compute Engine administration permissions (`compute.instances.*`) used to manage Spot VM lifecycles directly via REST. Set in production using `wrangler secret put`. |
| `GITHUB_OAUTH_CLIENT_ID` | Empty (sign-in off). | Cloudflare Worker (`cloudflare/src/auth.rs`). | Client ID of the GitHub OAuth 2.0 App. Required to enable web dashboard sign-in. |
| `GITHUB_OAUTH_CLIENT_SECRET` | Empty. | Cloudflare Worker (`cloudflare/src/auth.rs`). | **Secret.** Client secret for the GitHub OAuth 2.0 App. Set in production using `wrangler secret put`. |
| `GITHUB_OAUTH_SCOPES` | `repo read:org`. | Cloudflare Worker (`cloudflare/src/auth.rs`). | OAuth 2.0 permission scopes requested during user authentication. |
| `GITHUB_OAUTH_REDIRECT_URI` | `https://spot.swacktech.com/auth/github/callback`. | Cloudflare Worker (`cloudflare/src/auth.rs`). | Absolute OAuth 2.0 callback URL registered with the GitHub app. |

## Dashboard session and authorization

These variables configure session lifetimes, signing keys, and administrator authorization rules for the
edge dashboard.

| Variable | Default | Read by | Meaning |
| --- | --- | --- | --- |
| `SESSION_SECRET` | None. | Cloudflare Worker (`cloudflare/src/auth.rs`). | **Secret.** Cryptographic HMAC key of at least 32 bytes used to sign and verify `__Host-spot_session` cookies. Set in production using `wrangler secret put`. |
| `SESSION_NOT_BEFORE` | `0` (no cut-off). | Cloudflare Worker (`cloudflare/src/auth.rs`). | Unix timestamp epoch seconds. Sessions issued before this timestamp are rejected, enabling immediate global sign-out. |
| `DASHBOARD_GLOBAL_ADMINS` | Unset (enabled). | Cloudflare Worker (`cloudflare/src/lib.rs`). | Toggle controlling whether added administrators skip per-target checks. The literal `none` disables the exemption (requiring added administrators to administer every controlled target); unset or any other value enables it. The primary owner is fixed in code. |

## Cloudflare Worker and deployment settings

<div class="carousel" markdown>
<div class="carousel-track" markdown>
<figure class="shot" markdown>
![GCP project from gcloud](images/term-gcp-project.png)
<figcaption markdown>The deploy recipe reads `GCP_PROJECT` from the active gcloud configuration.</figcaption>
</figure>
<figure class="shot" markdown>
![Configured header](images/ui-header.png)
<figcaption markdown>The dashboard header shows the configured project and organization.</figcaption>
</figure>
</div>
</div>
<!-- /shot -->

These variables configure deployment identifiers, Workers KV storage bindings, and Cloudflare API credentials.

| Variable | Default | Read by | Meaning |
| --- | --- | --- | --- |
| `GCP_PROJECT` | Unset (the header shows "not configured"). | Cloudflare Worker (`cloudflare/src/lib.rs`). | GCP project ID shown in the dashboard header. `just deploy-worker` passes it with `wrangler deploy --var`, taken from `gcloud config get-value project`. The reconciler calls Compute Engine with the `project_id` from `GCP_SERVICE_ACCOUNT_KEY` and stops with an error if the key has none. |
| `GITHUB_ORG` | `swack-tools`. | Cloudflare Worker (`cloudflare/wrangler.toml` `[vars]`). | GitHub organization whose runner registrations are managed by the edge reconciler. |
| `CLOUDFLARE_ACCOUNT_ID` | None. | Wrangler command-line tool. | Cloudflare account identifier that owns the Worker and Workers KV namespace. |
| `CLOUDFLARE_KV_NAMESPACE_ID` | None. | Wrangler command-line tool. | Workers KV namespace binding identifier for `SPOT_KV`, storing dashboard state. |
| `CLOUDFLARE_API_TOKEN` | None. | Wrangler command-line tool. | **Secret.** Cloudflare API token with permissions to manage Workers and Workers KV. |

## Host agent settings

These variables configure the pure-Rust `host-agent` binary running on runner hosts and dedicated builders.

| Variable | Default | Read by | Meaning |
| --- | --- | --- | --- |
| `GHA_REAL_DOCKER` | `/usr/bin/docker`. | `host-agent docker-wrapper`. | Path to the underlying Docker binary invoked after network proxy filtering. |
| `GHA_DOCKER_WRAPPER_STATE_DIR` | `/run/gha-docker-wrapper`. | `host-agent docker-wrapper`. | Ephemeral runtime directory where container socket proxies and descriptors are managed. |
| `GHA_DOCKER_WRAPPER_HANDSHAKE_TIMEOUT` | `15.0`. | `host-agent docker-wrapper`. | Timeout in seconds when waiting for Docker container socket handshakes. |
| `GHA_RUNNER_CONTAINER_ID` | Empty (auto-detected). | `host-agent docker-wrapper`. | Explicit runner container identifier override when running inside nested container environments. |
| `OXIDEX_BUILD_ROOT` | `/mnt/runner-data/remote-build`. | `host-agent host-launcher`. | Base directory for dedicated builder source trees, target output, and Cargo caches. |
| `OXIDEX_LOCK_ROOT` | `/run`. | `host-agent host-launcher`. | Runtime directory for advisory build and upload lock files. |
| `OXIDEX_ALLOW_UNPRIVILEGED` | Empty (root required). | `host-agent host-launcher`. | Set to `1` only during unprivileged local testing to bypass root permission checks. |

## Non-environment configuration

Configuration files that do not use environment variables:

| Configuration | File location | Purpose |
| --- | --- | --- |
| Worker deployment manifest | `cloudflare/wrangler.toml` | Declares Worker name, entrypoint, Cron Triggers (`*/5 * * * *`), and KV bindings. |
| Runner host bootstrap | `cloud-init/startup.sh` | Installs Docker, host-agent, and registers GitHub Actions runner listener. |
| Dedicated builder bootstrap | `cloud-init/builder-startup.sh` | Formats cache disk, sets up build root, installs Rust 1.99.0 container and toolchain. |
| Builder remote configuration | `/etc/oxidex-remote-build.json` | Specifies allowed projects, approved commands, container image, and resource limits. |
| Host admission configuration | `/etc/gha-admission.json` | Configures host memory budget, reservation limits, and peer leasing parameters. |
