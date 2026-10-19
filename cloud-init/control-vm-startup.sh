#!/usr/bin/env bash
# Boot script for the dashboard control VM (spot-control-a / spot-control-b), a Spot VM or the always-on
# free-tier e2-micro.
#
# GCE runs this on every boot, including each start after a Spot preemption or an operator stop, so every
# step is idempotent. Per-VM settings come from instance metadata; secrets come from Secret Manager
# through the VM service account and are written only to root-owned 0600 files. This script never
# echoes a secret and never enables shell tracing.
set -euo pipefail
shopt -s inherit_errexit
umask 077

readonly METADATA=http://metadata.google.internal/computeMetadata/v1
readonly CONFIG_DIR=/etc/spot-dashboard
readonly ENV_FILE="$CONFIG_DIR/dashboard.env"
readonly TUNNEL_ENV_FILE="$CONFIG_DIR/tunnel.env"
readonly STATE_DIR=/var/lib/spot-dashboard
readonly CONTAINER_UID=10001
readonly DASHBOARD_CONTAINER=spot-dashboard
readonly TUNNEL_CONTAINER=spot-cloudflared
# The free-tier e2-micro has 1 GB of RAM. It never builds the dashboard image (it pulls the published one), and
# before any package install it gets compressed zram swap in RAM (used first) backed by a swap file on disk,
# plus memory limits on both containers. Other shapes have at least 2 GB and get none of this.
readonly MICRO_MACHINE_TYPE=e2-micro
readonly SWAP_FILE=/swapfile
readonly SWAP_SIZE_MIB=2048
readonly SWAP_FILE_PRIORITY=10
readonly ZRAM_DEV=/dev/zram0
readonly ZRAM_SYS=/sys/block/zram0
readonly ZRAM_PRIORITY=100
readonly MEMINFO=/proc/meminfo
# Container logs are capped on every shape so they can't fill the small boot disk.
readonly LOG_ARGS=(--log-driver json-file --log-opt max-size=10m --log-opt max-file=3)
# One run at a time: the boot run and a rerun by the image watcher never overlap.
readonly RUN_LOCK=/run/spot-control-startup.lock
# The image watcher reruns this script when the dashboard-image metadata changes, so an image update swaps only the
# containers: same VM, same boot disk, same state file (see install_image_watcher).
readonly WATCH_SCRIPT=/usr/local/sbin/spot-dashboard-image-watch
readonly WATCH_UNIT=spot-dashboard-image-watch.service

log() {
  printf 'spot-control-startup: %s\n' "$*" >&2
}

fail() {
  log "ERROR: $*"
  exit 1
}

# Read one instance metadata attribute; empty when the key is absent.
metadata_attr() {
  curl -fsS --retry 5 --retry-connrefused --max-time 10 \
    -H 'Metadata-Flavor: Google' "$METADATA/instance/attributes/$1" 2>/dev/null || true
}

metadata_required() {
  local value
  value="$(metadata_attr "$1")"
  [[ -n "$value" ]] || fail "required metadata key $1 is missing"
  printf '%s' "$value"
}

wait_for_metadata() {
  local attempt
  for attempt in $(seq 1 60); do
    if curl -fsS --max-time 5 -H 'Metadata-Flavor: Google' "$METADATA/project/project-id" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  fail "metadata server is unreachable"
}

access_token() {
  local body token
  body="$(curl -fsS --retry 5 --retry-connrefused --max-time 10 \
    -H 'Metadata-Flavor: Google' "$METADATA/instance/service-accounts/default/token")" \
    || fail "could not obtain a service account token"
  token="$(printf '%s' "$body" | sed -n 's/.*"access_token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
  [[ -n "$token" ]] || fail "service account token response had no access_token"
  printf '%s' "$token"
}

# Fetch the latest version of one Secret Manager secret. The bearer token is passed through a
# process-substitution header file so it never appears in a process argument list.
fetch_secret() {
  local name="$1" body data value
  body="$(curl -fsS --retry 5 --retry-connrefused --max-time 20 \
    -H @<(printf 'Authorization: Bearer %s\n' "$TOKEN") \
    "https://secretmanager.googleapis.com/v1/projects/$PROJECT/secrets/$name/versions/latest:access")" \
    || fail "could not read secret $name"
  data="$(printf '%s' "$body" | tr -d '\n' | sed -n 's/.*"data"[[:space:]]*:[[:space:]]*"\([A-Za-z0-9+/=_-]*\)".*/\1/p')"
  [[ -n "$data" ]] || fail "secret $name has no payload"
  value="$(printf '%s' "$data" | tr '_-' '/+' | base64 -d)" || fail "secret $name payload is not base64"
  # Trailing newlines are trimmed by command substitution; reject anything else multi-line.
  [[ "$value" != *$'\n'* && "$value" != *$'\r'* ]] || fail "secret $name contains a line break"
  [[ -n "$value" ]] || fail "secret $name is empty"
  printf '%s' "$value"
}

# Atomically replace a root-owned 0600 file with stdin.
write_private_file() {
  local target="$1" temp
  temp="$(mktemp "$target.XXXXXX")"
  cat >"$temp"
  chmod 0600 "$temp"
  chown root:root "$temp"
  mv -f "$temp" "$target"
}

# Accept the same pinned image references as ControlConfig.create_problems() in src/lib/control_vm.py:
# name:tag, name@sha256:<64 hex>, or name:tag@sha256:<64 hex>. A bare name or a :latest tag is refused.
image_ref_pinned() {
  local ref="$1"
  [[ "$ref" =~ ^[a-z0-9][a-z0-9._/-]*(:[A-Za-z0-9._-]+)?(@sha256:[0-9a-f]{64})?$ ]] || return 1
  [[ "$ref" == *:* ]] || return 1
  [[ "$ref" != *:latest ]] || return 1
}

# The dashboard image the e2-micro pulls: pinned by digest (name[:tag]@sha256:<64 hex>), or by the 40-hex commit tag
# that .github/workflows/dashboard-image.yml publishes (name:<sha>). Mirrors ControlConfig.create_problems() in
# src/lib/control_vm.py for the standard mode; other shapes keep the digest-only check.
dashboard_image_ok() {
  local ref="$1"
  [[ "$ref" =~ ^[a-z0-9][a-z0-9._/-]*(:[A-Za-z0-9._-]+)?@sha256:[0-9a-f]{64}$ ]] && return 0
  [[ "$ref" =~ ^[a-z0-9][a-z0-9._/-]*:[0-9a-f]{40}$ ]]
}

# The dashboard-image checks for this shape: on the e2-micro (1 GB of RAM, never builds) a digest or a published
# commit tag; elsewhere a digest.
check_dashboard_image() {
  local ref="$1"
  if [[ "$MACHINE_TYPE" == "$MICRO_MACHINE_TYPE" ]]; then
    dashboard_image_ok "$ref" \
      || fail "the e2-micro control VM needs dashboard-image pinned by digest or a 40-hex commit tag; it never builds the image"
  else
    [[ "$ref" =~ @sha256:[0-9a-f]{64}$ ]] || fail "dashboard-image must be pinned by digest"
  fi
}

# Origin (https://host[:port]) of the public dashboard URL. The URL must be https:// plus a DNS host name, an
# optional port 1-65535 without leading zeros and at most a trailing slash: no path, query, fragment or user info. Mirrors
# public_origin() in src/lib/control_vm.py, which refuses the same values before a VM is created.
public_origin_of() {
  local url="$1" rest port label='[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?'
  [[ "$url" == https://* ]] || return 1
  rest="${url#https://}"
  rest="${rest%/}"
  [[ "$rest" =~ ^($label(\.$label)*)(:([1-9][0-9]{0,4}))?$ ]] || return 1
  port="${BASH_REMATCH[6]}"
  if [[ -n "$port" ]]; then
    ((10#$port >= 1 && 10#$port <= 65535)) || return 1
  fi
  printf 'https://%s' "$rest"
}

# An optional DASHBOARD_SESSION_NOT_BEFORE value: empty, or Unix seconds that are not in the future (the
# dashboard refuses a future cut-off). Mirrors session_not_before_problems() in src/lib/control_vm.py.
session_not_before_ok() {
  local value="$1" now
  [[ -z "$value" ]] && return 0
  [[ "$value" =~ ^[0-9]{1,15}$ ]] || return 1
  now="$(date +%s)"
  ((10#$value <= now + 60))
}

# An optional dashboard-config-repo value: empty (the dashboard default), or owner/repo with a GitHub login
# and repository name. Mirrors config_repo_problems() in src/lib/control_vm.py.
config_repo_ok() {
  local value="$1" repo
  [[ -z "$value" ]] && return 0
  [[ "$value" =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,38}/([A-Za-z0-9._-]{1,100})$ ]] || return 1
  repo="${BASH_REMATCH[1]}"
  [[ "$repo" != . && "$repo" != .. ]]
}

# Print the ;-separated dashboard-controlled-targets metadata value as the comma list the dashboard reads
# from DASHBOARD_CONTROLLED_TARGETS. Every entry must be an organization or owner/repo; an empty entry or
# anything else fails. Mirrors controlled_target_names() in src/lib/control_vm.py.
controlled_targets_csv() {
  local raw="$1" entry repo joined=""
  local -a entries=()
  [[ -z "$raw" ]] && return 0
  [[ "$raw" != *';' ]] || return 1
  IFS=';' read -r -a entries <<<"$raw"
  for entry in "${entries[@]}"; do
    if [[ "$entry" =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,38}/([A-Za-z0-9._-]{1,100})$ ]]; then
      repo="${BASH_REMATCH[1]}"
      [[ "$repo" != . && "$repo" != .. ]] || return 1
    elif [[ ! "$entry" =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,38}$ ]]; then
      return 1
    fi
    joined+="${joined:+,}$entry"
  done
  printf '%s' "$joined"
}

# Print the dashboard-global-admins metadata value as DASHBOARD_GLOBAL_ADMINS: empty stays empty (dashboard
# default), "none" stays "none", otherwise ;-separated login:numeric_id entries become a comma list. The only
# identity accepted is the dashboard owner swackhamer:619624. Owners stay deploy-configured; other
# administrators are added on the dashboard's Admins page and live in dashboard state.
# Mirrors global_admins_metadata() in src/lib/control_vm.py.
global_admins_csv() {
  local raw="$1" entry joined=""
  local -a entries=()
  [[ -z "$raw" ]] && return 0
  if [[ "$raw" == none ]]; then printf 'none'; return 0; fi
  [[ "$raw" != *';' ]] || return 1
  IFS=';' read -r -a entries <<<"$raw"
  for entry in "${entries[@]}"; do
    # The dashboard owner is the only identity that may be listed (dashboard_auth.parse_global_admins).
    [[ "$(printf '%s' "$entry" | tr 'A-Z' 'a-z')" == swackhamer:619624 ]] || return 1
    joined+="${joined:+,}$entry"
  done
  printf '%s' "$joined"
}

# Create (once) and enable (every boot) a root-owned 0600 swap file of size_mib MiB. Idempotent: an active swap
# file is left alone, and a complete file of the right size is reused. The file is built under a temporary
# name and renamed only after mkswap, so a crash never leaves a half-made file at $SWAP_FILE.
ensure_swap() {
  local size_mib="$1" bytes temp active
  bytes=$((size_mib * 1024 * 1024))
  if [[ -f "$SWAP_FILE" && "$(stat -c %s "$SWAP_FILE")" == "$bytes" ]]; then
    chown root:root "$SWAP_FILE"
    chmod 0600 "$SWAP_FILE"
  else
    log "creating ${size_mib} MiB swap file $SWAP_FILE"
    temp="$SWAP_FILE.partial"
    swapoff "$SWAP_FILE" >/dev/null 2>&1 || true
    rm -f "$temp" "$SWAP_FILE"
    (umask 077 && fallocate -l "$bytes" "$temp") || fail "could not allocate $SWAP_FILE"
    chown root:root "$temp"
    chmod 0600 "$temp"
    mkswap "$temp" >/dev/null || fail "mkswap failed for $SWAP_FILE"
    mv -f "$temp" "$SWAP_FILE"
  fi
  active="$(swapon --show=NAME --noheadings)" || fail "could not list the active swap areas"
  if ! grep -Fxq -- "$SWAP_FILE" <<<"$active"; then
    swapon --priority "$SWAP_FILE_PRIORITY" "$SWAP_FILE" || fail "could not enable swap file $SWAP_FILE"
    log "enabled swap file $SWAP_FILE"
  fi
}

# Compressed swap in RAM (zram, zstd or else lz4, half of MemTotal) at a higher priority than the swap file, so
# the kernel swaps there first. Idempotent, and never fatal: without the zram module, or on any zram error, it
# logs why and the VM keeps only the swap file.
ensure_zram_swap() {
  local active algorithms algorithm="" candidate mem_kib size=""
  active="$(swapon --show=NAME --noheadings 2>/dev/null)" || active=""
  if grep -Fxq -- "$ZRAM_DEV" <<<"$active"; then
    return 0
  fi
  if ! modprobe zram num_devices=1 >/dev/null 2>&1 || [[ ! -d "$ZRAM_SYS" ]]; then
    log "zram is unavailable on this kernel; continuing with only the swap file"
    return 0
  fi
  # A device sized by an earlier, interrupted boot keeps its size and algorithm (both are fixed once set).
  if [[ "$(cat "$ZRAM_SYS/disksize" 2>/dev/null || echo 0)" == 0 ]]; then
    algorithms="$(cat "$ZRAM_SYS/comp_algorithm" 2>/dev/null || true)"
    for candidate in zstd lz4; do
      if [[ " ${algorithms//[\[\]]/ } " == *" $candidate "* ]]; then
        algorithm="$candidate"
        break
      fi
    done
    if [[ -z "$algorithm" ]]; then
      log "zram offers neither zstd nor lz4 (${algorithms:-none listed}); keeping the kernel default"
    elif ! { printf '%s\n' "$algorithm" >"$ZRAM_SYS/comp_algorithm"; } 2>/dev/null; then
      log "could not select $algorithm for zram; keeping the kernel default"
      algorithm=""
    fi
    mem_kib="$(awk '/^MemTotal:/ {print $2}' "$MEMINFO" 2>/dev/null || true)"
    if [[ ! "$mem_kib" =~ ^[0-9]+$ ]]; then
      log "could not read MemTotal; continuing with only the swap file"
      return 0
    fi
    size=$((mem_kib * 1024 / 2))
    if ! { printf '%s\n' "$size" >"$ZRAM_SYS/disksize"; } 2>/dev/null; then
      log "could not size $ZRAM_DEV; continuing with only the swap file"
      return 0
    fi
  fi
  if ! mkswap "$ZRAM_DEV" >/dev/null 2>&1 || ! swapon --priority "$ZRAM_PRIORITY" "$ZRAM_DEV" >/dev/null 2>&1; then
    log "could not enable zram swap on $ZRAM_DEV; continuing with only the swap file"
    return 0
  fi
  log "enabled zram swap on $ZRAM_DEV (${size:-existing size} bytes, ${algorithm:-default} compression)"
}

# Per-shape docker run arguments: capped logs everywhere, plus memory limits on the 1 GB e2-micro so the
# dashboard and cloudflared fit next to the guest agents and dockerd (the swap file absorbs short peaks).
set_container_limits() {
  DASHBOARD_RUN_ARGS=("${LOG_ARGS[@]}")
  TUNNEL_RUN_ARGS=("${LOG_ARGS[@]}")
  if [[ "$MACHINE_TYPE" == "$MICRO_MACHINE_TYPE" ]]; then
    DASHBOARD_RUN_ARGS+=(--memory=320m --memory-swap=640m)
    TUNNEL_RUN_ARGS+=(--memory=160m --memory-swap=320m)
  fi
}

install_packages() {
  local missing=()
  command -v docker >/dev/null 2>&1 || missing+=(docker.io)
  if [[ -z "$DASHBOARD_IMAGE" ]]; then
    command -v git >/dev/null 2>&1 || missing+=(git)
  fi
  if ((${#missing[@]})); then
    log "installing ${missing[*]}"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -q
    apt-get install -y -q --no-install-recommends "${missing[@]}"
  fi
  systemctl enable --now docker >/dev/null
}

image_present() {
  docker image inspect "$1" >/dev/null 2>&1
}

pull_published_image() {
  if image_present "$DASHBOARD_IMAGE"; then
    log "reusing local image $DASHBOARD_IMAGE"
    return 0
  fi
  log "pulling $DASHBOARD_IMAGE"
  printf '%s' "$GITHUB_TOKEN" | docker login ghcr.io --username x-access-token --password-stdin >/dev/null
  # Bounded, so every run ends and reports (the controller treats an old "running" report as a dead run).
  timeout 900 docker pull "$DASHBOARD_IMAGE" >/dev/null || fail "could not pull $DASHBOARD_IMAGE"
  docker logout ghcr.io >/dev/null 2>&1 || true
}

# A pull can take minutes on the e2-micro, and the controller may pin another image meanwhile (a rollback). Read
# dashboard-image again after the pull and pull that image instead, so this run never starts a superseded image.
follow_image_changes() {
  local latest attempt
  for attempt in 1 2 3 4 5; do
    latest="$(metadata_attr dashboard-image)"
    if [[ -z "$latest" || "$latest" == "$DASHBOARD_IMAGE" ]]; then
      return 0
    fi
    check_dashboard_image "$latest"
    log "dashboard-image changed to $latest during the pull; following it"
    DASHBOARD_IMAGE="$latest"
    read_update_id
    pull_published_image
  done
  fail "dashboard-image kept changing during the pull"
}

BUILD_DIR=""
PREVIOUS_IMAGE_ID=""
# Report this run as the guest attribute spot-dashboard/run (the instance metadata enable-guest-attributes=TRUE
# allows it): "running <unix time>", then "done <update id> <image>" or "failed <update id> <unix time>", where the
# update id is the dashboard-update-id metadata the run applied. The controller releases a container only on the end
# report of the update it requested, since a pending run may still replace it.
report_run() {
  curl -fsS -X PUT --max-time 5 -H 'Metadata-Flavor: Google' --data "$1" \
    "$METADATA/instance/guest-attributes/spot-dashboard/run" >/dev/null 2>&1
}

# The "running" report is required once guest attributes are enabled: a run that can't announce itself stops
# before it touches a container, so the controller never mistakes it for no run at all.
report_run_start() {
  local enabled
  enabled="$(metadata_attr enable-guest-attributes)"
  if ! report_run "running $(date +%s)" && [[ "$enabled" == TRUE || "$enabled" == true ]]; then
    fail "could not report this run (guest attribute spot-dashboard/run)"
  fi
}

# The dashboard-update-id metadata: empty, or the controller's <transition id>-<phase>.
read_update_id() {
  UPDATE_ID="$(metadata_attr dashboard-update-id)"
  [[ -z "$UPDATE_ID" || "$UPDATE_ID" =~ ^[a-z0-9-]{1,64}$ ]] || fail "dashboard-update-id is malformed"
}

cleanup() {
  local status=$?
  if ((status != 0)); then
    report_run "failed ${UPDATE_ID:--} $(date +%s)" || true
  fi
  if [[ -n "$BUILD_DIR" ]]; then
    rm -rf "$BUILD_DIR"
  fi
  docker logout ghcr.io >/dev/null 2>&1 || true
}
trap cleanup EXIT

build_from_source() {
  local image="spot-dashboard:$SOURCE_REF" workdir head auth
  if image_present "$image"; then
    log "reusing local image $image"
    DASHBOARD_IMAGE_REF="$image"
    return 0
  fi
  BUILD_DIR="$(mktemp -d /var/tmp/spot-dashboard-src.XXXXXX)"
  workdir="$BUILD_DIR"
  auth="$(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 -w0)"
  git init -q "$workdir"
  git -C "$workdir" remote add origin "https://github.com/$DASHBOARD_REPO.git"
  log "fetching $DASHBOARD_REPO at $SOURCE_REF"
  # The token travels as an http.extraHeader set through the environment, never in the URL or argv.
  GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.extraHeader \
    GIT_CONFIG_VALUE_0="Authorization: Basic $auth" GIT_TERMINAL_PROMPT=0 \
    git -C "$workdir" fetch -q --depth 1 origin "$SOURCE_REF" || fail "could not fetch $SOURCE_REF"
  # Check out world-readable files (the script runs under umask 077). The Dockerfile also normalizes
  # modes, so the image is correct either way. The build directory itself stays 0700 (mktemp -d).
  (umask 022 && git -C "$workdir" -c advice.detachedHead=false checkout -q FETCH_HEAD)
  head="$(git -C "$workdir" rev-parse HEAD)"
  [[ "$head" == "$SOURCE_REF" ]] || fail "checked out $head instead of $SOURCE_REF"
  log "building $image"
  docker build -q -f "$workdir/dashboard/Dockerfile" -t "$image" "$workdir" >/dev/null \
    || fail "dashboard image build failed"
  DASHBOARD_IMAGE_REF="$image"
}

write_dashboard_env() {
  # Each secret is fetched by an assignment so a failed fetch stops the script (set -e) instead of
  # writing an empty value.
  local oauth_id oauth_secret session_secret control_secret kv_token tunnel_token
  oauth_id="$(fetch_secret spot-dashboard-oauth-client-id)"
  oauth_secret="$(fetch_secret spot-dashboard-oauth-client-secret)"
  session_secret="$(fetch_secret spot-dashboard-session-secret)"
  control_secret="$(fetch_secret spot-dashboard-control-secret)"
  kv_token="$(fetch_secret spot-dashboard-cloudflare-kv-token)"
  tunnel_token="$(fetch_secret "spot-dashboard-tunnel-token-$SLOT")"
  {
    printf 'GITHUB_OAUTH_CLIENT_ID=%s\n' "$oauth_id"
    printf 'GITHUB_OAUTH_CLIENT_SECRET=%s\n' "$oauth_secret"
    printf 'GITHUB_OAUTH_REDIRECT_URI=%s\n' "${PUBLIC_URL%/}/auth/github/callback"
    # The Worker reaches this VM through its tunnel hostname, so the browser Origin (the public URL)
    # differs from the upstream Host; the dashboard accepts POSTs from this origin as same-origin.
    printf 'DASHBOARD_PUBLIC_ORIGIN=%s\n' "$PUBLIC_ORIGIN"
    printf 'DASHBOARD_SESSION_SECRET=%s\n' "$session_secret"
    if [[ -n "$SESSION_NOT_BEFORE" ]]; then
      # Bulk sign-out: sessions issued before this Unix time are rejected (metadata, re-read every boot).
      printf 'DASHBOARD_SESSION_NOT_BEFORE=%s\n' "$SESSION_NOT_BEFORE"
    fi
    # Target policy (metadata, re-read every boot): the config repository whose repos/<owner>/<repo>.yaml
    # files are controlled targets, and the static controlled targets. Unset keeps the dashboard defaults.
    if [[ -n "$CONFIG_REPO" ]]; then
      printf 'SPOT_CONFIG_REPO=%s\n' "$CONFIG_REPO"
    fi
    if [[ -n "$CONTROLLED_TARGETS" ]]; then
      printf 'DASHBOARD_CONTROLLED_TARGETS=%s\n' "$CONTROLLED_TARGETS"
    fi
    if [[ -n "$GLOBAL_ADMINS" ]]; then
      printf 'DASHBOARD_GLOBAL_ADMINS=%s\n' "$GLOBAL_ADMINS"
    fi
    printf 'DASHBOARD_CONTROL_SECRET=%s\n' "$control_secret"
    printf 'CLOUDFLARE_ACCOUNT_ID=%s\n' "$CLOUDFLARE_ACCOUNT_ID"
    printf 'CLOUDFLARE_KV_NAMESPACE_ID=%s\n' "$CLOUDFLARE_KV_NAMESPACE_ID"
    printf 'CLOUDFLARE_KV_API_TOKEN=%s\n' "$kv_token"
    printf 'SPOT_CONFIG_GH_TOKEN=%s\n' "$GITHUB_TOKEN"
    printf 'GH_TOKEN=%s\n' "$GITHUB_TOKEN"
    printf 'SPOT_GCP_TOKEN_SOURCE=metadata\n'
    # Runner Patches preview: list runner hosts and their recorded spot-runner-patch metadata from Compute Engine.
    printf 'DASHBOARD_FLEET_INVENTORY=gce\n'
  } | write_private_file "$ENV_FILE"
  printf 'TUNNEL_TOKEN=%s\n' "$tunnel_token" | write_private_file "$TUNNEL_ENV_FILE"
}

# Run container NAME from the docker run arguments that follow, unless it already runs exactly them: same image,
# arguments, and env file (metadata and secrets), recorded as a hash in a label. A run that changes nothing (a boot,
# or a rerun superseded by a rollback) keeps the running container, so it is never stopped undrained. Otherwise a
# graceful stop lets it finish requests before it is replaced.
ensure_container() {
  local name="$1" env_file="$2" hash current
  shift 2
  hash="$({ printf '%s\0' "$@"; cat "$env_file"; } | sha256sum | cut -d' ' -f1)"
  current="$(docker inspect --format '{{index .Config.Labels "spot-run-hash"}} {{.State.Running}}' "$name" 2>/dev/null \
    || true)"
  if [[ -n "$hash" && "$current" == "$hash true" ]]; then
    log "keeping $name: its image, arguments, and settings are unchanged"
    return 0
  fi
  docker stop --time 20 "$name" >/dev/null 2>&1 || true
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --name "$name" --label "spot-run-hash=$hash" "$@" >/dev/null
}

run_containers() {
  install -d -m 0750 -o "$CONTAINER_UID" -g "$CONTAINER_UID" "$STATE_DIR"
  chown "$CONTAINER_UID:$CONTAINER_UID" "$STATE_DIR"
  chmod 0750 "$STATE_DIR"

  # Changed metadata, secrets, or images always take effect (see ensure_container). The image is pulled before
  # this point, so the old container serves until here.
  PREVIOUS_IMAGE_ID="$(docker inspect --format '{{.Image}}' "$DASHBOARD_CONTAINER" 2>/dev/null || true)"
  # DASHBOARD_IMAGE is reported by /api/health, so the controller can confirm which image serves.
  local -a dashboard_args=(--restart unless-stopped --network host
    "${DASHBOARD_RUN_ARGS[@]}"
    --user "$CONTAINER_UID:$CONTAINER_UID" --cap-drop ALL --security-opt no-new-privileges
    --env-file "$ENV_FILE" -e "DASHBOARD_IMAGE=$DASHBOARD_IMAGE_REF" -v "$STATE_DIR:/state" "$DASHBOARD_IMAGE_REF"
    --host 127.0.0.1 --port 8787 --state-file /state/state.json --control-slot "$SLOT" --project "$PROJECT")
  ensure_container "$DASHBOARD_CONTAINER" "$ENV_FILE" "${dashboard_args[@]}"

  image_present "$CLOUDFLARED_IMAGE" || docker pull "$CLOUDFLARED_IMAGE" >/dev/null
  # cloudflared reads the slot's tunnel token from TUNNEL_TOKEN, keeping it out of argv.
  local -a tunnel_args=(--restart unless-stopped --network host
    "${TUNNEL_RUN_ARGS[@]}"
    --cap-drop ALL --security-opt no-new-privileges
    --env-file "$TUNNEL_ENV_FILE" "$CLOUDFLARED_IMAGE" tunnel --no-autoupdate run)
  ensure_container "$TUNNEL_CONTAINER" "$TUNNEL_ENV_FILE" "${tunnel_args[@]}"
}

# In-place image updates pull each new image onto the same small boot disk. Remove every image except the one the
# dashboard now runs and the one it ran before this run (a rollback then needs no pull). Images that a container
# uses, such as cloudflared's, can't be removed without --force, so docker keeps them.
remove_old_images() {
  local current id
  current="$(docker inspect --format '{{.Image}}' "$DASHBOARD_CONTAINER" 2>/dev/null || true)"
  [[ -n "$current" ]] || return 0
  while read -r id; do
    [[ -z "$id" || "$id" == "$current" || "$id" == "$PREVIOUS_IMAGE_ID" ]] && continue
    docker image rm "$id" >/dev/null 2>&1 || true
  done < <(docker image ls --no-trunc --quiet | sort -u)
}

# Atomically replace a root-owned file with stdin, with the given mode. A rename never changes the file a running
# process (the watcher itself, during a rerun) is reading.
write_root_file() {
  local target="$1" mode="$2" temp
  temp="$(mktemp "$target.XXXXXX")"
  cat >"$temp"
  chmod "$mode" "$temp"
  chown root:root "$temp"
  mv -f "$temp" "$target"
}

# Install and start (once per boot) the service that reruns this script when the dashboard-image metadata changes.
# It long-polls the metadata server, so it costs no CPU while it waits. A rerun pulls the new image while the old
# container still serves, then recreates the containers. The service starts after the boot run of this script
# (After=google-startup-scripts.service), and both take RUN_LOCK, so runs never overlap. A rerun never restarts the
# watcher; a changed watcher takes effect at the next boot.
install_image_watcher() {
  write_root_file "$WATCH_SCRIPT" 0755 <<'WATCH'
#!/usr/bin/env bash
# Installed by the control VM startup script (cloud-init/control-vm-startup.sh); reruns it when the dashboard-image
# instance metadata names an image the dashboard container does not run.
set -uo pipefail
readonly ATTRS=http://metadata.google.internal/computeMetadata/v1/instance/attributes
readonly RUN_REPORT=http://metadata.google.internal/computeMetadata/v1/instance/guest-attributes/spot-dashboard/run
readonly CONTAINER=spot-dashboard
log() { printf 'spot-dashboard-image-watch: %s\n' "$*" >&2; }
attr() { curl -fsS --max-time 10 -H 'Metadata-Flavor: Google' "$ATTRS/$1" 2>/dev/null || true; }
etag=NONE
while true; do
  # Wakes on any metadata change: a request can change only dashboard-update-id (a restore of the running image).
  headers="$(mktemp)"
  if ! curl -fsS --max-time 3700 -D "$headers" -H 'Metadata-Flavor: Google' \
      "$ATTRS/?recursive=true&wait_for_change=true&timeout_sec=3600&last_etag=$etag" >/dev/null; then
    rm -f "$headers"
    sleep 15
    continue
  fi
  etag="$(tr -d '\r' <"$headers" | sed -n 's/^[Ee][Tt][Aa][Gg]:[[:space:]]*//p' | tail -n 1)"
  rm -f "$headers"
  [[ "$etag" =~ ^[A-Za-z0-9]+$ ]] || etag=NONE
  value="$(attr dashboard-image)"
  [[ -n "$value" ]] || continue
  running="$(docker inspect --format '{{.Config.Image}}' "$CONTAINER" 2>/dev/null || true)"
  if [[ "$value" == "$running" ]]; then
    # Nothing to swap: confirm the request (its update id) so the controller knows no run is pending for it.
    id="$(attr dashboard-update-id)"
    [[ "$id" =~ ^[a-z0-9-]{1,64}$ ]] || id=-
    curl -fsS -X PUT --max-time 5 -H 'Metadata-Flavor: Google' --data "done $id $value" "$RUN_REPORT" \
      >/dev/null 2>&1 || true
    continue
  fi
  log "dashboard-image is $value but $CONTAINER runs ${running:-nothing}; rerunning the startup script"
  google_metadata_script_runner startup || log "the startup script failed; retrying at the next change or in an hour"
done
WATCH
  write_root_file "/etc/systemd/system/$WATCH_UNIT" 0644 <<UNIT
[Unit]
Description=Swap the Spot dashboard container when its dashboard-image metadata changes
After=google-startup-scripts.service docker.service network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$WATCH_SCRIPT
Restart=always
RestartSec=15

[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable "$WATCH_UNIT" >/dev/null 2>&1 || fail "could not enable $WATCH_UNIT"
  # --no-block: during the boot run the start waits for this script to finish (After=), so it must not block.
  systemctl start --no-block "$WATCH_UNIT" || fail "could not start $WATCH_UNIT"
}

report_health() {
  local attempt status
  for attempt in $(seq 1 60); do
    status="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:8787/api/health || true)"
    if [[ "$status" == 200 ]]; then
      log "dashboard is healthy on 127.0.0.1:8787"
      return 0
    fi
    sleep 5
  done
  log "dashboard has not reported healthy yet (last HTTP status ${status:-none}); the autoscaler keeps waiting"
}

main() {
  [[ "$(id -u)" -eq 0 ]] || fail "must run as root"
  exec 9>"$RUN_LOCK"
  flock 9
  wait_for_metadata
  report_run_start
  read_update_id
  PROJECT="$(curl -fsS -H 'Metadata-Flavor: Google' "$METADATA/project/project-id")"
  # projects/<number>/machineTypes/<type>; the shape decides the swap file and the container memory limits.
  MACHINE_TYPE="$(curl -fsS --retry 5 --retry-connrefused --max-time 10 -H 'Metadata-Flavor: Google' \
    "$METADATA/instance/machine-type")" || fail "could not read the machine type"
  MACHINE_TYPE="${MACHINE_TYPE##*/}"
  [[ "$MACHINE_TYPE" =~ ^[a-z0-9-]+$ ]] || fail "machine type is malformed"
  SLOT="$(metadata_required spot-control-slot)"
  DASHBOARD_IMAGE="$(metadata_attr dashboard-image)"
  DASHBOARD_REPO="$(metadata_attr dashboard-repo)"
  DASHBOARD_REPO="${DASHBOARD_REPO:-swack-tools/spot-github-runners}"
  SOURCE_REF="$(metadata_attr dashboard-source-ref)"
  PUBLIC_URL="$(metadata_required dashboard-public-url)"
  CLOUDFLARE_ACCOUNT_ID="$(metadata_required cloudflare-account-id)"
  CLOUDFLARE_KV_NAMESPACE_ID="$(metadata_required cloudflare-kv-namespace-id)"
  CLOUDFLARED_IMAGE="$(metadata_required cloudflared-image)"
  SESSION_NOT_BEFORE="$(metadata_attr dashboard-session-not-before)"
  CONFIG_REPO="$(metadata_attr dashboard-config-repo)"
  CONTROLLED_TARGETS_METADATA="$(metadata_attr dashboard-controlled-targets)"
  GLOBAL_ADMINS_METADATA="$(metadata_attr dashboard-global-admins)"

  [[ "$SLOT" == a || "$SLOT" == b ]] || fail "spot-control-slot must be a or b"
  [[ "$PUBLIC_URL" == https://* ]] || fail "dashboard-public-url must be https"
  [[ "$DASHBOARD_REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || fail "dashboard-repo must be owner/name"
  [[ "$CLOUDFLARE_ACCOUNT_ID" =~ ^[A-Za-z0-9]+$ ]] || fail "cloudflare-account-id is malformed"
  [[ "$CLOUDFLARE_KV_NAMESPACE_ID" =~ ^[A-Za-z0-9]+$ ]] || fail "cloudflare-kv-namespace-id is malformed"
  image_ref_pinned "$CLOUDFLARED_IMAGE" || fail "cloudflared-image must be pinned to a version tag or digest"
  session_not_before_ok "$SESSION_NOT_BEFORE" \
    || fail "dashboard-session-not-before must be Unix seconds that are not in the future"
  config_repo_ok "$CONFIG_REPO" || fail "dashboard-config-repo must be owner/repo"
  CONTROLLED_TARGETS="$(controlled_targets_csv "$CONTROLLED_TARGETS_METADATA")" \
    || fail "dashboard-controlled-targets must be ;-separated organizations or owner/repo targets"
  GLOBAL_ADMINS="$(global_admins_csv "$GLOBAL_ADMINS_METADATA")" \
    || fail "dashboard-global-admins must be none or ;-separated login:numeric_id entries"
  PUBLIC_ORIGIN="$(public_origin_of "$PUBLIC_URL")" || fail "dashboard-public-url must be https://host[:port] without a path, query, or user info"
  if [[ "$MACHINE_TYPE" == "$MICRO_MACHINE_TYPE" || -n "$DASHBOARD_IMAGE" ]]; then
    check_dashboard_image "$DASHBOARD_IMAGE"
  else
    [[ "$SOURCE_REF" =~ ^[0-9a-f]{40}$ ]] || fail "dashboard-source-ref must be a 40-hex commit"
  fi

  if [[ "$MACHINE_TYPE" == "$MICRO_MACHINE_TYPE" ]]; then
    # Before apt, docker, and the image pull, which would otherwise risk the OOM killer on 1 GB.
    ensure_zram_swap
    ensure_swap "$SWAP_SIZE_MIB"
  fi
  set_container_limits

  install -d -m 0700 -o root -g root "$CONFIG_DIR"
  TOKEN="$(access_token)"
  GITHUB_TOKEN="$(fetch_secret spot-dashboard-github-token)"
  write_dashboard_env

  install_packages
  # Before anything that can fail (pull, build, container replacement), so a later metadata change always has a
  # watcher to apply it, including a rollback after this run fails.
  install_image_watcher
  if [[ -n "$DASHBOARD_IMAGE" ]]; then
    pull_published_image
    follow_image_changes
    DASHBOARD_IMAGE_REF="$DASHBOARD_IMAGE"
  else
    build_from_source
  fi
  unset TOKEN
  run_containers
  unset GITHUB_TOKEN
  remove_old_images
  report_run "done ${UPDATE_ID:--} $DASHBOARD_IMAGE_REF" || true
  report_health
}

main "$@"
