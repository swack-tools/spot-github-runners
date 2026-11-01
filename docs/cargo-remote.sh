#!/usr/bin/env bash
# cargo-remote: build Rust projects on your Spot Rust builders from your workstation.
#
# Install once. The dashboard's "Use from my Mac" button shows this line with your project filled in:
#   curl -fsSL https://swack-tools.github.io/spot-github-runners/cargo-remote.sh | bash -s -- install --project PROJECT
# Then, in any Cargo project:
#   cargo remote build --release      # Linux x86_64 binary lands in target/release/
#   cargo remote test
#   cargo remote status               # list builders and whether they are ready
#
# It finds your running builders with gcloud (or SPOT_BUILDERS="user@address ..."). Each project gets the
# next builder in turn and stays there so its Cargo cache stays warm; if that builder is gone or not ready,
# or busy with another build, the build moves to the next one. It never edits ~/.ssh/config: its settings and the builders' host keys
# live in ~/.config/spot-builder/. Written for the bash 3.2 that ships with macOS.
set -euo pipefail

URL="https://swack-tools.github.io/spot-github-runners/cargo-remote.sh"
CONF="${XDG_CONFIG_HOME:-$HOME/.config}/spot-builder"
KEY="${SPOT_BUILDER_KEY:-$HOME/.ssh/id_ed25519}"
LAUNCHER=/usr/local/bin/oxidex-remote-build
ROOT=/mnt/runner-data/remote-build
RUST_VERSION=1.99.0

die() { echo "cargo-remote: $*" >&2; exit 64; }
say() { echo "[cargo-remote] $*" >&2; }

[[ "${1:-}" == "remote" ]] && shift

install_self() {
  local project="" dest tmp
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --project) project="${2:-}"; shift 2 || shift ;;
      --project=*) project="${1#--project=}"; shift ;;
      *) die "unknown install option: $1" ;;
    esac
  done
  [[ -z "$project" || "$project" =~ ^[a-z][a-z0-9-]{4,28}[a-z0-9]$ ]] || die "not a GCP project ID: $project"
  mkdir -p "$CONF" && chmod 700 "$CONF"
  [[ -n "$project" ]] && printf '%s\n' "$project" > "$CONF/project"
  dest="$HOME/.cargo/bin"
  [[ -d "$dest" ]] || dest="$HOME/.local/bin"
  mkdir -p "$dest"
  tmp="$(mktemp)"
  # Copy this file when run from a path; under `curl | bash` there is no file, so download it.
  if [[ "${BASH_SOURCE[0]:-}" == */* && -f "${BASH_SOURCE[0]}" ]]; then
    cp "${BASH_SOURCE[0]}" "$tmp"
  else
    curl -fsSL "$URL" -o "$tmp"
  fi
  bash -n "$tmp" || { rm -f "$tmp"; die "the downloaded script is not valid bash"; }
  install -m 0755 "$tmp" "$dest/cargo-remote"
  rm -f "$tmp"
  echo "Installed $dest/cargo-remote."
  case ":$PATH:" in *":$dest:"*) ;; *) echo "Add $dest to your PATH, then open a new terminal." ;; esac
  command -v gcloud >/dev/null 2>&1 || echo "Install gcloud and sign in, or set SPOT_BUILDERS=\"user@address ...\", so it can find your builders."
  [[ -f "$KEY" ]] || echo "No $KEY yet: run ssh-keygen -t ed25519 and launch builders with ~/.ssh/id_ed25519.pub."
  echo "Now, in a Cargo project: cargo remote build --release"
}

if [[ "${1:-}" == "install" ]]; then
  shift
  install_self "$@"
  exit 0
fi

mkdir -p "$CONF/assign" && chmod 700 "$CONF"
SSH_OPTS=(-i "$KEY" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10
  -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$CONF/known_hosts"
  -o ControlMaster=auto -o ControlPath="$CONF/cm-%C" -o ControlPersist=10m)
# Host keys are recorded under the builder's name, so a new builder reusing an old address isn't refused.
rsh() { local name="$1" target="$2"; shift 2; ssh "${SSH_OPTS[@]}" -o HostKeyAlias="$name" "$target" "$@"; }
rsync_e() { echo "ssh ${SSH_OPTS[*]} -o HostKeyAlias=$1"; }
ERR="$(mktemp "$CONF/error.XXXXXX")"
trap 'rm -f "$ERR"' EXIT

# Prints "user@address name" for each running builder, sorted by name. Cached for a minute.
list_builders() {
  local cache="$CONF/builders" project tmp
  if [[ -n "${SPOT_BUILDERS:-}" ]]; then
    for b in $SPOT_BUILDERS; do echo "$b $b"; done
    return
  fi
  if [[ -s "$cache" && -n "$(find "$cache" -mmin -1 2>/dev/null)" ]]; then
    cat "$cache"
    return
  fi
  command -v gcloud >/dev/null 2>&1 || die "install gcloud and sign in, or set SPOT_BUILDERS=\"user@address ...\""
  project="${SPOT_GCP_PROJECT:-$(cat "$CONF/project" 2>/dev/null || true)}"
  tmp="$(mktemp "$cache.XXXXXX")"
  gcloud compute instances list ${project:+--project "$project"} \
    --filter='labels.spot-role=rust_builder AND status=RUNNING' \
    --format='value(name,labels.spot-builder-ssh-user,networkInterfaces[0].accessConfigs[0].natIP)' \
    | awk '$1 ~ /^builder-[a-z0-9-]+$/ && $2 ~ /^[a-z0-9_][a-z0-9_-]*$/ && $3 ~ /^[0-9.]+$/ {print $2 "@" $3 " " $1}' \
    | sort -k2 > "$tmp"
  mv -f "$tmp" "$cache"
  cat "$cache"
}

TARGETS=(); NAMES=()
while read -r target name; do
  [[ -n "$target" ]] || continue
  TARGETS+=("$target"); NAMES+=("$name")
done < <(list_builders)
COUNT=${#TARGETS[@]}
[[ $COUNT -gt 0 ]] || die "no running builders. Launch one on the dashboard (Machine Qualification, Your Rust builders)."

if [[ "${1:-}" == "status" ]]; then
  i=0
  while [[ $i -lt $COUNT ]]; do
    state="$(rsh "${NAMES[$i]}" "${TARGETS[$i]}" "sudo -n $LAUNCHER status" 2>&1 | head -1 || true)"
    echo "${NAMES[$i]}  ${TARGETS[$i]}  ${state:-unreachable}"
    i=$((i + 1))
  done
  exit 0
fi

[[ -f Cargo.toml ]] || die "run this in the folder that has Cargo.toml"
pkg="$(awk -F'"' '/^\[package\]/{p=1; next} /^\[/{p=0} p && /^name *=/{print $2; exit}' Cargo.toml)"
PROJECT="${SPOT_BUILDER_PROJECT:-${pkg:-$(basename "$PWD")}}"
[[ "$PROJECT" =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$ ]] || die "set SPOT_BUILDER_PROJECT to a simple name"
case "$PROJECT" in
  status|prepare|cleanup|bootstrap-tools) die "the builder reserves the name $PROJECT; set SPOT_BUILDER_PROJECT to another name" ;;
esac
if [[ -f rust-toolchain.toml ]] && ! grep -Eq "^channel *= *\"${RUST_VERSION//./\\.}\"" rust-toolchain.toml; then
  die "builders use Rust $RUST_VERSION; set channel = \"$RUST_VERSION\" in rust-toolchain.toml"
fi

# Builder order: the one this project used last, then the rest in turn from a shared rotation counter.
start=-1
last="$(cat "$CONF/assign/$PROJECT" 2>/dev/null || true)"
i=0
while [[ $i -lt $COUNT ]]; do
  [[ "${NAMES[$i]}" == "$last" ]] && start=$i
  i=$((i + 1))
done
if [[ $start -lt 0 ]]; then
  next="$(cat "$CONF/next" 2>/dev/null || echo 0)"
  [[ "$next" =~ ^[0-9]+$ ]] || next=0
  start=$((next % COUNT))
  echo $(((start + 1) % COUNT)) > "$CONF/next"
fi

# Find the profile directory and binary name the build writes.
profile_dir=debug; bin="${pkg:-$PROJECT}"; triple=""; prev=""
set_profile() { case "$1" in dev|test) profile_dir=debug ;; release|bench) profile_dir=release ;; *) profile_dir="$1" ;; esac; }
for arg in "$@"; do
  case "$prev" in
    --profile) set_profile "$arg" ;;
    --bin) bin="$arg" ;;
    --target) triple="$arg" ;;
  esac
  case "$arg" in
    --release|-r) profile_dir=release ;;
    --profile=*) set_profile "${arg#--profile=}" ;;
    --bin=*) bin="${arg#--bin=}" ;;
    --target=*) triple="${arg#--target=}" ;;
  esac
  prev="$arg"
done
out="${triple:+$triple/}$profile_dir"

# Sync the project to one builder and run Cargo there. Returns 75 when that builder can't take the build now.
build_on() {
  local host="$1" name="$2" rc=0
  shift 2
  rsh "$name" "$host" "sudo -n $LAUNCHER prepare $PROJECT" >/dev/null 2>"$ERR" || {
    say "$name is not ready ($(head -1 "$ERR")); trying the next builder"
    return 75
  }
  say "syncing $PWD to $name ($PROJECT)"
  # -rltz, not -a: keep the builder's own permissions so the build container can write Cargo.lock.
  rsync -rltz --delete --exclude target/ --exclude .git/ -e "$(rsync_e "$name")" ./ "$host:$ROOT/sources/$PROJECT/" || return $?
  # Builders compile with the pinned Rust and need the project to say so in rust-toolchain.toml.
  if [[ ! -f rust-toolchain.toml ]]; then
    rsh "$name" "$host" "printf '[toolchain]\\nchannel = \"$RUST_VERSION\"\\n' > $ROOT/sources/$PROJECT/rust-toolchain.toml" || return $?
  fi
  say "cargo $* on $name"
  rsh "$name" "$host" "sudo -n $LAUNCHER $PROJECT cargo$(printf ' %q' "$@")" || rc=$?
  if [[ $rc -eq 75 ]]; then say "$name is busy with another build; trying the next builder"; fi
  return $rc
}

HOST=""; NAME=""; rc=75
i=0
while [[ $i -lt $COUNT ]]; do
  k=$(((start + i) % COUNT))
  rc=0
  build_on "${TARGETS[$k]}" "${NAMES[$k]}" "$@" || rc=$?
  if [[ $rc -ne 75 ]]; then
    HOST="${TARGETS[$k]}"; NAME="${NAMES[$k]}"
    printf '%s\n' "$NAME" > "$CONF/assign/$PROJECT"
    break
  fi
  i=$((i + 1))
done
[[ -n "$HOST" ]] || die "no builder is free. A new builder needs 15 to 25 minutes of setup; check with: cargo remote status"
[[ $rc -eq 0 ]] || exit "$rc"

if [[ "${1:-}" == "build" ]]; then
  mkdir -p "target/$out"
  rsync -tz -e "$(rsync_e "$NAME")" "$HOST:$ROOT/targets/$PROJECT/$out/$bin" "target/$out/$bin"
  say "copied target/$out/$bin (Linux ${triple:-x86_64}, built on $NAME)"
fi
