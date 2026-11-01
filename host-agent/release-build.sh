#!/usr/bin/env bash
# Build the static host-agent binaries that runner VMs download at boot, and print their SHA-256 sums.
#
# The build is reproducible: the same commit, toolchain (rust-toolchain.toml) and Cargo.lock give the same
# bytes in any checkout directory, so CI can check that the checksums pinned in
# cloudflare/src/host_agent_release.rs match the source on this commit.
#
# Usage: host-agent/release-build.sh [OUTPUT_DIR]   (default: host-agent/target/release-assets)
# Writes host-agent-x86_64-linux-musl, host-agent-aarch64-linux-musl and SHA256SUMS to OUTPUT_DIR, then exits
# non-zero (printing the values to pin) if they differ from cloudflare/src/host_agent_release.rs.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
out_dir=${1:-"$repo_root/host-agent/target/release-assets"}
mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd -P)
cargo_home=${CARGO_HOME:-"$HOME/.cargo"}
# rust-toolchain.toml in the repository root picks the compiler.
cd "$repo_root"

# Pure-Rust dependencies need no C toolchain; rust-lld links against the musl objects shipped with each target.
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
# Strip local paths from panic messages so the bytes do not depend on where the checkout or Cargo home is.
export RUSTFLAGS="--remap-path-prefix=$repo_root=/build --remap-path-prefix=$cargo_home=/cargo"
export CARGO_INCREMENTAL=0
unset RUSTC_WRAPPER

rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl >&2

for pair in x86_64:x86_64-unknown-linux-musl aarch64:aarch64-unknown-linux-musl; do
  arch=${pair%%:*}
  target=${pair#*:}
  cargo build --manifest-path "$repo_root/host-agent/Cargo.toml" --release --locked --target "$target" >&2
  install -m 0755 "$repo_root/host-agent/target/$target/release/host-agent" "$out_dir/host-agent-$arch-linux-musl"
done

(cd "$out_dir" && sha256sum host-agent-x86_64-linux-musl host-agent-aarch64-linux-musl >SHA256SUMS && cat SHA256SUMS)

# The Worker pins these sums; a VM refuses any other bytes. Fail when the pin no longer matches the source.
pin_file="$repo_root/cloudflare/src/host_agent_release.rs"
pinned() { sed -n "s/^pub const $1: &str = \"\([0-9a-f]*\)\";$/\1/p" "$pin_file"; }
built() { awk -v f="$1" '$2 == f {print $1}' "$out_dir/SHA256SUMS"; }
x86_64_sum=$(built host-agent-x86_64-linux-musl)
aarch64_sum=$(built host-agent-aarch64-linux-musl)
if [[ "$(pinned SHA256_X86_64)" != "$x86_64_sum" || "$(pinned SHA256_AARCH64)" != "$aarch64_sum" ]]; then
  {
    echo "cloudflare/src/host_agent_release.rs does not match this host-agent source. Set:"
    echo "pub const SHA256_X86_64: &str = \"$x86_64_sum\";"
    echo "pub const SHA256_AARCH64: &str = \"$aarch64_sum\";"
  } >&2
  exit 1
fi
echo "Pinned checksums match; release tag host-agent-${x86_64_sum:0:12}" >&2
