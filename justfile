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
