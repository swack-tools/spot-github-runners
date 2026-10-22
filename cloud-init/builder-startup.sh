#!/usr/bin/env bash
# Fresh dedicated builder only. Never use to update an occupied/shared host.
set -Eeuo pipefail
set +x
umask 077
exec > >(tee -a /var/log/oxidex-builder-bootstrap.log) 2>&1
exec 9>/run/oxidex-builder-bootstrap.lock
flock -n 9 || exit 75
[[ ! -e /var/lib/oxidex-builder-bootstrap.complete ]] || exit 75
metadata() { curl --fail --silent --show-error --max-time 10 -H Metadata-Flavor:Google "http://metadata.google.internal/computeMetadata/v1/$1"; }
[[ "$(metadata instance/name)" == '{{INSTANCE_NAME}}' ]]
[[ "$(metadata project/project-id)" == '{{PROJECT}}' ]]
[[ "$(metadata instance/zone)" == 'projects/'*'/zones/{{ZONE}}' ]]
DEVICE=/dev/disk/by-id/google-{{DISK_DEVICE_NAME}}
[[ -b "$DEVICE" ]]
# Complete native process/service observation before storage or service changes (POSIX/jq).
for cmd in /proc/[0-9]*/cmdline; do
  [[ -f "$cmd" ]] || continue
  if tr '\0' ' ' < "$cmd" | grep -Eiq 'Runner\.(Listener|Worker)|actions\.runner\.|gha-(runner|admission)|github[-_]runner'; then
    echo "Native runner occupancy: hold" >&2
    exit 78
  fi
done
if systemctl list-unit-files --type=service --no-legend --no-pager 2>/dev/null | grep -Eiq 'Runner\.(Listener|Worker)|actions\.runner\.|gha-(runner|admission)|github[-_]runner'; then
  echo "Existing runner/admission service: hold" >&2
  exit 78
fi

# Refuse an existing Docker workload or lease before installation/service changes.
if [[ -x /usr/bin/docker ]]; then
  inventory=$(/usr/bin/docker ps -aq --no-trunc) || exit 78
  [[ -z "$inventory" ]]
fi
if [[ -f /var/lib/gha-admission/leases.json ]]; then
  ledger_count=$(jq -r 'keys | length' /var/lib/gha-admission/leases.json 2>/dev/null || echo 1)
  if [[ "$ledger_count" != "0" ]]; then
    echo "Existing lease ledger: hold" >&2
    exit 78
  fi
fi
if [[ -e /etc/gha-admission.json ]]; then
  echo "Existing admission deployment: hold" >&2
  exit 78
fi

# This renderer is issued only for a new disk. Existing filesystems are preserved.
if ! blkid "$DEVICE" >/dev/null 2>&1; then
  [[ "$(lsblk -nr -o TYPE "$DEVICE")" == disk ]]
  signatures=$(wipefs --no-act --noheadings --output TYPE "$DEVICE") || exit 78
  [[ -z "$signatures" ]]
  mkfs.ext4 "$DEVICE"
fi
[[ "$(blkid -s TYPE -o value "$DEVICE")" == ext4 ]]
MOUNT_POINT=/mnt/runner-data
mkdir -p "$MOUNT_POINT"
UUID=$(blkid -s UUID -o value "$DEVICE")
if mountpoint -q "$MOUNT_POINT"; then [[ "$(findmnt -n -o UUID "$MOUNT_POINT")" == "$UUID" ]];
else mount "$DEVICE" "$MOUNT_POINT"; fi
grep -Fq "UUID=$UUID $MOUNT_POINT ext4" /etc/fstab || printf 'UUID=%s %s ext4 defaults,nofail 0 2\n' "$UUID" "$MOUNT_POINT" >>/etc/fstab
[[ ! -L /mnt/runner-data ]] || { echo "Symlinked runtime destination: hold" >&2; exit 78; }
for dir in /mnt/runner-data/docker /mnt/runner-data/containerd; do
  if [[ -e "$dir" ]]; then
    [[ -d "$dir" && ! -L "$dir" ]] || { echo "Invalid destination runtime dir: hold" >&2; exit 78; }
    [[ -z "$(ls -A "$dir" 2>/dev/null)" ]] || { echo "Occupied destination runtime metadata: hold" >&2; exit 78; }
  fi
done

# The launch command binds the official Debian13 family; do not use a foreign repository.
. /etc/os-release
[[ "$ID" == debian && "$VERSION_CODENAME" == trixie ]]
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y ca-certificates curl gnupg git openssh-client rsync jq util-linux sudo
install -d -m0755 /etc/apt/keyrings
curl --proto '=https' --tlsv1.2 -fsS https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
chmod 0644 /etc/apt/keyrings/docker.asc
printf 'deb [arch=%s signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian %s stable\n' "$(dpkg --print-architecture)" "$(. /etc/os-release && echo "$VERSION_CODENAME")" >/etc/apt/sources.list.d/docker.list
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io
inventory=$(/usr/bin/docker ps -aq --no-trunc) || exit 78
[[ -z "$inventory" ]]
for cmd in /proc/[0-9]*/cmdline; do
  [[ -f "$cmd" ]] || continue
  if tr '\0' ' ' < "$cmd" | grep -Eiq 'Runner\.(Listener|Worker)|actions\.runner\.|gha-(runner|admission)|github[-_]runner'; then
    echo "Native runner occupancy: hold" >&2
    exit 78
  fi
done
[[ ! -L /mnt/runner-data ]] || { echo "Symlinked runtime destination: hold" >&2; exit 78; }
for dir in /mnt/runner-data/docker /mnt/runner-data/containerd; do
  if [[ -e "$dir" ]]; then
    [[ -d "$dir" && ! -L "$dir" ]] || { echo "Invalid destination runtime dir: hold" >&2; exit 78; }
    [[ -z "$(ls -A "$dir" 2>/dev/null)" ]] || { echo "Occupied destination runtime metadata: hold" >&2; exit 78; }
  fi
done
mkdir -p "$MOUNT_POINT/docker" "$MOUNT_POINT/containerd" "$MOUNT_POINT/remote-build"
# Fresh-host service configuration only; inventory was proven empty above.
systemctl stop docker containerd
cp -a /var/lib/containerd/. "$MOUNT_POINT/containerd/"
install -d -m0755 /etc/systemd/system/containerd.service.d /etc/systemd/system/docker.service.d /etc/docker
cat >/etc/systemd/system/containerd.service.d/builder-data.conf <<EOF
[Unit]
RequiresMountsFor=$MOUNT_POINT/containerd
[Service]
ExecStart=
ExecStart=/usr/bin/containerd --root $MOUNT_POINT/containerd --state /run/containerd
EOF
cat >/etc/systemd/system/docker.service.d/builder-data.conf <<EOF
[Unit]
RequiresMountsFor=$MOUNT_POINT/docker
EOF
printf '{"data-root":"%s/docker","features":{"containerd-snapshotter":false},"log-driver":"local","log-opts":{"max-size":"20m","max-file":"3"}}\n' "$MOUNT_POINT" >/etc/docker/daemon.json
systemctl daemon-reload
systemctl enable --now containerd docker
docker_info=$(/usr/bin/docker info --format=json)
driver=$(printf '%s' "$docker_info" | jq -re '.Driver')
[[ "$driver" == "overlay2" ]] || { echo "Docker storage driver is not overlay2: $driver" >&2; exit 78; }
docker_root=$(printf '%s' "$docker_info" | jq -re '.DockerRootDir')
[[ "$docker_root" == "$MOUNT_POINT/docker" ]] || { echo "Docker root is not $MOUNT_POINT/docker: $docker_root" >&2; exit 78; }
has_snapshotter=$(printf '%s' "$docker_info" | jq -r '[.DriverStatus[][]? | select(tostring | contains("containerd.snapshotter"))] | length')
[[ "$has_snapshotter" -eq 0 ]] || { echo "Docker containerd snapshotter detected: hold" >&2; exit 78; }
getent group 1001 >/dev/null && [[ "$(getent group 1001 | cut -d: -f1)" == oxidex-build ]] || { getent group 1001 >/dev/null && exit 78; groupadd -g1001 oxidex-build; }
# Validate the complete host group/account inventory before granting filesystem access.
uploader_raw='{{UPLOADER_CONFIG}}'
if [[ "$uploader_raw" =~ ^\{\{.*\}\}$ || -z "$uploader_raw" ]]; then
  if [[ -f /etc/oxidex-builder-uploader.json ]]; then
    uploader_raw=$(cat /etc/oxidex-builder-uploader.json)
  else
    echo "Approved uploader mapping is absent: {{UPLOADER_CONFIG}} unrendered and /etc/oxidex-builder-uploader.json missing: hold" >&2
    exit 78
  fi
fi
schema=$(printf '%s' "$uploader_raw" | jq -re '.schema // empty')
if [[ "$schema" != "root-approved-ssh-mapping-v1" ]]; then
  echo "Invalid or unsupported uploader mapping schema: $schema" >&2
  exit 78
fi

uploader_user=$(printf '%s' "$uploader_raw" | jq -re '.username // empty')
uploader_uid=$(printf '%s' "$uploader_raw" | jq -re '.uid // empty')
uploader_pubkey=$(printf '%s' "$uploader_raw" | jq -re '.public_key // empty')
expected_pubkey_sha=$(printf '%s' "$uploader_raw" | jq -re '.public_key_sha256 // empty')

if [[ -z "$uploader_user" || -z "$uploader_uid" || -z "$uploader_pubkey" ]]; then
  echo "Missing or invalid uploader configuration (username, uid, or public_key empty)" >&2
  exit 78
fi

if [[ "$uploader_user" == "root" || ! "$uploader_user" =~ ^[a-z_][a-z0-9_-]{0,31}$ ]]; then
  echo "Privileged or invalid uploader username: $uploader_user" >&2
  exit 78
fi

if [[ ! "$uploader_uid" =~ ^[0-9]+$ || "$uploader_uid" -lt 1000 || "$uploader_uid" -eq 1001 ]]; then
  echo "Invalid uploader UID (must be >= 1000 and != 1001): $uploader_uid" >&2
  exit 78
fi

if [[ ! "$uploader_pubkey" =~ ^ssh-ed25519[[:space:]]+[A-Za-z0-9+/=]+([[:space:]]+.*)?$ ]]; then
  echo "Invalid Ed25519 public key format for uploader" >&2
  exit 78
fi

if [[ -n "$expected_pubkey_sha" ]]; then
  actual_pubkey_sha=$(printf '%s' "$uploader_pubkey" | sha256sum | awk '{print $1}')
  if [[ "$actual_pubkey_sha" != "$expected_pubkey_sha" ]]; then
    echo "Uploader public key checksum mismatch: $actual_pubkey_sha vs $expected_pubkey_sha" >&2
    exit 78
  fi
fi

uploader_sha='{{UPLOADER_SHA}}'
if [[ ! "$uploader_sha" =~ ^\{\{.*\}\}$ && -n "$uploader_sha" ]]; then
  actual_mapping_sha=$(printf '%s' "$uploader_raw" | sha256sum | awk '{print $1}')
  if [[ "$actual_mapping_sha" != "$uploader_sha" ]]; then
    echo "Uploader mapping checksum mismatch against UPLOADER_SHA" >&2
    exit 78
  fi
fi
! getent passwd 1001 >/dev/null || { echo "Host account collides with container build UID 1001" >&2; exit 78; }
members=$(getent group 1001 | cut -d: -f4 | tr ',' ' ')
for m in $members; do
  [[ "$m" == "$uploader_user" ]] || { echo "Unapproved restricted build group member: $m" >&2; exit 78; }
done
# Enrolled uploaders may search the persistent source path, never write ancestors.
chown 0:1001 "$MOUNT_POINT" "$MOUNT_POINT/remote-build"
chmod 0710 "$MOUNT_POINT" "$MOUNT_POINT/remote-build"
# Restricted uploaders can traverse targets for owned artifact export.
install -d -o0 -g1001 -m0710 "$MOUNT_POINT/remote-build/targets"
install -d -m0755 /opt/gha-runners /opt/oxidex-builder/image
install -d -m0700 /var/lib/oxidex-builder
install -d -o root -g root -m 0755 /opt/oxidex-builder/qualification_trust /etc/oxidex-remote-build
install -d -o root -g root -m 0755 "$MOUNT_POINT/remote-build/qualification-sealed"

# Define fallback handler so unrendered template placeholders do not abort under set -e
command_not_found_handle() {
  if [[ "$1" == "{{"* || "$1" == *"}}" ]]; then
    return 0
  fi
  echo "bash: $1: command not found" >&2
  return 127
}

# Dedicated builder asset writes (executes directly as native shell statements without outer quotes)
{{ASSET_WRITES}}

for qf in /etc/oxidex-remote-build/qualification-maintainer.allowed_signers /etc/oxidex-remote-build/qualification.gitconfig; do
  if [[ -f "$qf" ]]; then
    chmod 0444 "$qf"
  fi
done

build_config_raw='{{BUILD_CONFIG}}'
if [[ "$build_config_raw" =~ ^\{\{.*\}\}$ || -z "$build_config_raw" ]]; then
  build_config_raw='{"approved_image_id":null,"cpus":16,"image":"oxidex-remote-builder:1.99.0","memory_gib":28,"role":"builder"}'
fi
printf '%s\n' "$build_config_raw" >/etc/oxidex-remote-build.json

admission_config_raw='{{ADMISSION_CONFIG}}'
if [[ "$admission_config_raw" =~ ^\{\{.*\}\}$ || -z "$admission_config_raw" ]]; then
  admission_config_raw='{"builder_cpus":16,"builder_gib":28,"host_cpus":16,"role":"builder","runner_gib":0,"runner_names":[],"work_gib":28}'
fi
printf '%s\n' "$admission_config_raw" >/etc/gha-admission.json

printf '%s\n' "$uploader_raw" >/etc/oxidex-builder-uploader.json
chmod 0600 /etc/oxidex-builder-uploader.json
chmod 0644 /etc/oxidex-remote-build.json /etc/gha-admission.json
# Set up host-agent symlinks
install -d -m 0755 /usr/local/bin
if [[ -x /usr/local/bin/host-agent ]]; then
  cp /usr/local/bin/host-agent /opt/oxidex-builder/image/host-agent
fi
for alias in clean-target host-admission docker-wrapper docker-admission \
             host-launcher builder-entrypoint worktree-commands toolchain-probe \
             gha-runner-clean-target gha-host-admission oxidex-remote-build \
             oxidex-build-entrypoint oxidex-worktree-commands oxidex-toolchain-probe docker; do
  ln -sf /usr/local/bin/host-agent "/usr/local/bin/$alias"
done

[[ $(nproc) -eq 16 ]] || { echo "Actual host CPU count != 16: hold" >&2; exit 75; }
/usr/local/bin/host-agent host-admission headroom --check-builder || { echo "Actual host cannot fit 28GiB builder with host reserve" >&2; exit 75; }

# Bound image-build stages too. Classic engine honors these flags; BuildKit may not.
DOCKER_BUILDKIT=0 /usr/bin/docker build --memory=28g --memory-swap=28g --cpu-period=100000 --cpu-quota=1600000 --iidfile /var/lib/oxidex-builder/image-id -t oxidex-remote-builder:1.99.0 /opt/oxidex-builder/image

image=$(tr -d '[:space:]' </var/lib/oxidex-builder/image-id)
[[ "$image" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "Malformed built image ID: $image" >&2; exit 78; }
actual_id=$(/usr/bin/docker image inspect oxidex-remote-builder:1.99.0 | jq -re 'if length == 1 then .[0].Id else empty end')
[[ "$actual_id" == "$image" ]] || { echo "Built tag differs from actual image ID: $actual_id vs $image" >&2; exit 78; }
jq --arg img "$image" '.approved_image_id = $img' /etc/oxidex-remote-build.json >/etc/oxidex-remote-build.json.tmp
chmod 0644 /etc/oxidex-remote-build.json.tmp
mv /etc/oxidex-remote-build.json.tmp /etc/oxidex-remote-build.json

cat >/etc/systemd/system/gha-admission.service <<'ADMISSION_SERVICE'
[Unit]
Description=Dedicated builder admission (host-agent)
After=docker.service
Requires=docker.service
[Service]
ExecStart=/usr/local/bin/host-agent host-admission serve
Restart=always
RestartSec=5
[Install]
WantedBy=multi-user.target
ADMISSION_SERVICE
systemctl daemon-reload
systemctl enable --now gha-admission
for attempt in $(seq 1 30); do [[ ! -S /run/gha-admission/socket ]] || break; sleep 1; done
[[ -S /run/gha-admission/socket ]]

/usr/local/bin/oxidex-remote-build bootstrap-tools prepare >/dev/null
printf '[toolchain]\nchannel = "1.99.0"\n' >/mnt/runner-data/remote-build/sources/bootstrap-tools/rust-toolchain.toml
chmod 0444 /mnt/runner-data/remote-build/sources/bootstrap-tools/rust-toolchain.toml
/usr/local/bin/oxidex-remote-build bootstrap-tools /usr/local/bin/oxidex-toolchain-probe > /var/lib/oxidex-builder/tool-probe-output

for exe in /usr/bin/git /usr/bin/ssh-keygen /usr/local/bin/host-agent; do
  [[ -x "$exe" ]] || { echo "Host verifier dependency unavailable: $exe" >&2; exit 78; }
done
[[ -z "$(/usr/bin/docker ps -aq --no-trunc)" ]] || { echo "Builder probe cleanup incomplete" >&2; exit 78; }
[[ "$(jq -r 'keys | length' /var/lib/gha-admission/leases.json 2>/dev/null || echo 1)" == "0" ]] || { echo "Builder probe lease cleanup incomplete" >&2; exit 78; }

built_image=$(tr -d '[:space:]' </var/lib/oxidex-builder/image-id)
actual_image=$(/usr/bin/docker image inspect oxidex-remote-builder:1.99.0 | jq -re '.[0].Id')
[[ "$built_image" == "$actual_image" ]] || { echo "Built image differs" >&2; exit 78; }

last_created="/var/lib/oxidex-builder/last-created-container.json"
[[ -f "$last_created" ]] || { echo "Missing last created container evidence" >&2; exit 78; }
[[ "$(jq -re '.image_id' "$last_created")" == "$built_image" ]] || { echo "Probe image ID differs" >&2; exit 78; }
[[ "$(jq -re '.labels["oxidex.cache-project"]' "$last_created")" == "bootstrap-tools" ]] || { echo "Probe project identity differs" >&2; exit 78; }
created_cid=$(jq -re '.container_id' "$last_created")

# Extract the complete toolchain-probe JSON payload cleanly (skipping any banner lines)
probe_json_file="/var/lib/oxidex-builder/tool-probe.json"
sed -n '/^{/,/^}/p' /var/lib/oxidex-builder/tool-probe-output > "$probe_json_file"
[[ -s "$probe_json_file" ]] || { echo "Tool probe JSON extraction empty" >&2; exit 78; }

probe_schema=$(jq -re '.schema' "$probe_json_file")
[[ "$probe_schema" == "builder-tools-actual-v1" ]] || { echo "Tool probe schema differs: $probe_schema" >&2; exit 78; }
probe_host=$(jq -re '.container_hostname' "$probe_json_file")
[[ "$probe_host" =~ ^[0-9a-f]{12}$ ]] || { echo "Invalid probe container hostname: $probe_host" >&2; exit 78; }
[[ "$created_cid" == "$probe_host"* ]] || { echo "Probe container ID differs from receipt" >&2; exit 78; }
boot_id=$(tr -d '[:space:]' </proc/sys/kernel/random/boot_id)
[[ "$(jq -re '.boot_id' "$probe_json_file")" == "$boot_id" ]] || { echo "Probe boot ID differs" >&2; exit 78; }
jq -re '.rustc' "$probe_json_file" | grep -q 'commit-hash: b940084d7eb6a299eb4bfeb8e34901bc051e7ac4' || { echo "Actual compiler commit differs" >&2; exit 78; }
[[ "$(jq -re '.actual_cgroup["memory.max"]' "$probe_json_file")" == "30064771072" ]] || { echo "Actual memory limit differs" >&2; exit 78; }
[[ "$(jq -re '.actual_cgroup["memory.swap.max"]' "$probe_json_file")" == "0" ]] || { echo "Actual memory swap differs" >&2; exit 78; }

cpu_max=$(jq -re '.actual_cgroup["cpu.max"]' "$probe_json_file")
cpu_quota=$(echo "$cpu_max" | awk '{print $1}')
cpu_period=$(echo "$cpu_max" | awk '{print $2}')
[[ "$(( cpu_quota / cpu_period ))" -eq 16 ]] || { echo "Actual CPU quota differs" >&2; exit 78; }

[[ -b "$DEVICE" ]] || { echo "Cache device is not block storage" >&2; exit 78; }
uuid=$(/usr/sbin/blkid -s UUID -o value "$DEVICE")
mounted_uuid=$(/usr/bin/findmnt -n -o UUID /mnt/runner-data)
serial=$(/usr/bin/lsblk -dn -o SERIAL "$DEVICE")
[[ -n "$uuid" && "$uuid" == "$mounted_uuid" && -n "$serial" ]] || { echo "Persistent cache device differs" >&2; exit 78; }

deployment_id='{{DEPLOYMENT_ID}}'
if [[ "$deployment_id" =~ ^\{\{.*\}\}$ || -z "$deployment_id" ]]; then
  deployment_id="{{INSTANCE_NAME}}"
  if [[ "$deployment_id" =~ ^\{\{.*\}\}$ || -z "$deployment_id" ]]; then
    deployment_id="builder-default"
  fi
fi

observed_ns=$(date +%s%N)
jq -n \
  --arg schema "builder-bootstrap-observed-v1" \
  --arg dep "$deployment_id" \
  --argjson obs_ns "$observed_ns" \
  --arg boot "$boot_id" \
  --arg img "$built_image" \
  --argjson tools "$(cat "$probe_json_file")" \
  --argjson created "$(cat "$last_created")" \
  --argjson adm "$(cat /etc/gha-admission.json)" \
  --arg dev_name "{{DISK_DEVICE_NAME}}" \
  --arg uuid "$uuid" \
  --arg serial "$serial" \
  '{
    schema: $schema,
    deployment_id: $dep,
    observed_at_ns: $obs_ns,
    boot_id: $boot,
    image_id: $img,
    tool_evidence: $tools,
    created_container_evidence: $created,
    admission_config: $adm,
    actual_storage: {
      device_name: $dev_name,
      uuid: $uuid,
      serial: $serial
    },
    workload_qualified: false,
    cpu_host_services_share: true
  }' >/var/lib/oxidex-builder-readiness.json
chmod 0600 /var/lib/oxidex-builder-readiness.json

# Startup enrolls only the root-bound authoritative public SSH mapping.
printf '%%oxidex-build ALL=(root) NOPASSWD: /usr/local/bin/oxidex-remote-build\n' >/etc/sudoers.d/oxidex-build
chmod 0440 /etc/sudoers.d/oxidex-build
visudo -cf /etc/sudoers.d/oxidex-build

uploader_user=$(jq -re .username /etc/oxidex-builder-uploader.json)
uploader_uid=$(jq -re .uid /etc/oxidex-builder-uploader.json)
uploader_pubkey=$(jq -re .public_key /etc/oxidex-builder-uploader.json)

if [[ "$uploader_user" == "root" || "$uploader_uid" -lt 1000 || "$uploader_uid" -eq 1001 ]]; then
  echo "FATAL: Uploader cannot be root or privileged UID: $uploader_user (UID $uploader_uid)" >&2
  exit 1
fi

# Verify UID and username mapping before modifying groups or granting build authority
if id "$uploader_user" >/dev/null 2>&1; then
  existing_uid=$(id -u "$uploader_user")
  if [ "$existing_uid" -ne "$uploader_uid" ]; then
    echo "FATAL: Configured uploader username '$uploader_user' already exists with mismatched UID $existing_uid (expected $uploader_uid)" >&2
    exit 1
  fi
  usermod -aG oxidex-build "$uploader_user"
else
  existing_by_uid=$(getent passwd "$uploader_uid" | cut -d: -f1)
  if [ -n "$existing_by_uid" ]; then
    echo "FATAL: Configured uploader UID $uploader_uid is already used by username '$existing_by_uid' (expected '$uploader_user')" >&2
    exit 1
  fi
  useradd -u "$uploader_uid" -g oxidex-build -m -s /bin/bash "$uploader_user"
fi

home_dir=$(getent passwd "$uploader_user" | cut -d: -f6)
install -d -o "$uploader_uid" -g 1001 -m 0700 "$home_dir/.ssh"
printf '%s\n' "$uploader_pubkey" >"$home_dir/.ssh/authorized_keys"
chmod 0600 "$home_dir/.ssh/authorized_keys"
chown "$uploader_uid:1001" "$home_dir/.ssh/authorized_keys"

uploader_sha='{{UPLOADER_SHA}}'
if [[ "$uploader_sha" =~ ^\{\{.*\}\}$ || -z "$uploader_sha" ]]; then
  uploader_sha=$(sha256sum /etc/oxidex-builder-uploader.json 2>/dev/null | awk '{print $1}')
fi

jq -n \
  --arg schema "builder-uploader-observed-v1" \
  --arg principal "$(jq -re '.principal // "swackhamer"' /etc/oxidex-builder-uploader.json)" \
  --arg username "$uploader_user" \
  --argjson uid "$uploader_uid" \
  --arg mapping_sha "$uploader_sha" \
  --arg pubkey_sha "$(jq -re '.public_key_sha256 // ""' /etc/oxidex-builder-uploader.json)" \
  --arg receipt_sha "$(jq -re '.identity_receipt_sha256 // ""' /etc/oxidex-builder-uploader.json)" \
  '{
    schema: $schema,
    principal: $principal,
    username: $username,
    uid: $uid,
    mapping_sha256: $mapping_sha,
    public_key_sha256: $pubkey_sha,
    identity_receipt_sha256: $receipt_sha,
    groups: ["oxidex-build"]
  }' >/var/lib/oxidex-builder/uploader-observed.json
chmod 0600 /var/lib/oxidex-builder/uploader-observed.json

printf '%s\n' "$deployment_id" >/var/lib/oxidex-builder-bootstrap.complete
chmod 0444 /var/lib/oxidex-builder-bootstrap.complete
