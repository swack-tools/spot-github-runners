# 100% Pure-Rust + Cloudflare Workers infrastructure repository.
# Provides development, test, build, lint, and verification recipes.

export PATH := env_var('HOME') + "/.cargo/bin:" + env_var('PATH')

# Default recipe listing available commands.
default:
    @just --list

# Run all unit, adversarial, and integration tests across host-agent and Cloudflare Worker.
test: test-host-agent test-cloudflare

# Run host-agent test suite.
test-host-agent:
    cargo test --manifest-path host-agent/Cargo.toml

# Run Cloudflare Worker edge test suite.
test-cloudflare:
    cargo test --manifest-path cloudflare/Cargo.toml

# Build release binaries for host-agent and Cloudflare Worker Wasm package.
build: build-host-agent build-worker

# Build native host-agent binary in release mode.
build-host-agent:
    cargo build --manifest-path host-agent/Cargo.toml --release

# Build Cloudflare Worker Wasm bundle and JavaScript shim.
build-worker:
    cd cloudflare && worker-build --release

# Fast check for compiler errors across both crates and verify documentation with Vale.
check: check-crates check-vale

# Run cargo check on host-agent and Cloudflare Worker crates.
check-crates:
    cargo check --manifest-path host-agent/Cargo.toml
    cargo check --manifest-path cloudflare/Cargo.toml

# Run Vale prose linter on documentation.
check-vale:
    vale README.md AGENTS.md CLAUDE.md docs/*.md docs/maintenance/*.md

# Synchronize Vale packages and verify documentation prose.
vale:
    vale sync
    vale README.md AGENTS.md CLAUDE.md docs/*.md docs/maintenance/*.md

# Format Rust source code across all crates.
fmt:
    cargo fmt --manifest-path host-agent/Cargo.toml
    cargo fmt --manifest-path cloudflare/Cargo.toml

# Check Rust source code formatting without modifying files.
fmt-check:
    cargo fmt --manifest-path host-agent/Cargo.toml -- --check
    cargo fmt --manifest-path cloudflare/Cargo.toml -- --check

# Run production health and telemetry probe against edge dashboard.
probe-edge:
    node tools/edge_dashboard_probe.js

# Clean build artifacts for host-agent and Cloudflare Worker.
clean:
    cargo clean --manifest-path host-agent/Cargo.toml
    cargo clean --manifest-path cloudflare/Cargo.toml
    rm -rf cloudflare/build

# Print the GCP project the Worker will be deployed with (the active gcloud default project).
gcp-project:
    @gcloud config get-value project 2>/dev/null

# Enqueue a remote Rust build or full test suite on the Spot builder queue, poll queue position and ETA, and retrieve the requested output ("log", "binary", or "target_zip").
test-remote output="log" command="cargo test --workspace --locked" template_id="rust-builder-default" binary_name="":
    #!/usr/bin/env bash
    set -euo pipefail
    api_base="${SPOT_DASHBOARD_URL:-https://spot.swacktech.com}"
    cookie_header=()
    if [ -n "${SPOT_SESSION_COOKIE:-}" ]; then
        cookie_header=(-H "Cookie: ${SPOT_SESSION_COOKIE}")
    fi
    repo="${SPOT_REMOTE_REPO:-$(git config --get remote.origin.url 2>/dev/null | sed -E 's#.*[:/]([^/]+/[^/.]+)(\.git)?$#\1#' || echo "example-org/example-rust-project")}"
    branch="${SPOT_REMOTE_BRANCH:-$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "main")}"
    commit_sha="${SPOT_REMOTE_COMMIT:-$(git rev-parse HEAD 2>/dev/null || echo "0000000000000000000000000000000000000000")}"
    payload="$(jq -nc \
        --arg repo "$repo" \
        --arg branch "$branch" \
        --arg sha "$commit_sha" \
        --arg cmd "{{command}}" \
        --arg out "{{output}}" \
        --arg tpl "{{template_id}}" \
        --arg bin "{{binary_name}}" \
        '{repo: $repo, branch: $branch, commit_sha: $sha, command: $cmd, requested_output: $out, template_id: $tpl, binary_name: (if $bin == "" then null else $bin end)}')"
    echo "Enqueuing remote Rust build for ${repo}@${branch} (${commit_sha:0:12})..."
    resp="$(curl -fsSL -X POST "${api_base}/api/rust-builds/enqueue" \
        -H "Content-Type: application/json" \
        "${cookie_header[@]}" \
        -d "$payload")"
    build_id="$(printf '%s' "$resp" | jq -r '.build_id')"
    msg="$(printf '%s' "$resp" | jq -r '.message')"
    echo "✓ Acked Build #${build_id}: ${msg}"
    if [ "${SPOT_QUEUE_NO_POLL:-0}" = "1" ]; then
        printf '%s\n' "$resp" | jq .
        exit 0
    fi
    poll_interval="${SPOT_POLL_INTERVAL_SECS:-10}"
    while true; do
        status_json="$(curl -fsSL "${api_base}/api/rust-builds/${build_id}" "${cookie_header[@]}")"
        bstatus="$(printf '%s' "$status_json" | jq -r '.build_status')"
        bmsg="$(printf '%s' "$status_json" | jq -r '.message')"
        echo "[Build #${build_id} · ${bstatus}] ${bmsg}"
        if [ "$bstatus" = "completed" ]; then
            selected_url="$(printf '%s' "$status_json" | jq -r '.selected_artifact_url')"
            echo "✓ Build #${build_id} completed. Requested ({{output}}) artifact URL: ${selected_url}"
            printf '%s\n' "$status_json" | jq '.artifacts'
            break
        elif [ "$bstatus" = "failed" ]; then
            log_url="$(printf '%s' "$status_json" | jq -r '.artifacts.log_url')"
            echo "✗ Build #${build_id} failed. Log artifact URL: ${log_url}" >&2
            exit 1
        fi
        sleep "$poll_interval"
    done

# This changes the live Worker: run it only when you intend a production deploy.
# Deploy the Cloudflare Worker with GCP_PROJECT taken from the active gcloud configuration.
deploy-worker:
    #!/usr/bin/env bash
    set -euo pipefail
    project="$(gcloud config get-value project 2>/dev/null)"
    if [ -z "$project" ] || [ "$project" = "(unset)" ]; then
        echo "No default gcloud project. Run: gcloud config set project PROJECT_ID" >&2
        exit 1
    fi
    echo "Deploying spot-dashboard-edge with GCP_PROJECT=$project"
    cd cloudflare && wrangler deploy --var "GCP_PROJECT:$project"
