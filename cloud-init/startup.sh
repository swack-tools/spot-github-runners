#!/usr/bin/env bash
# Rendered by Cloudflare Worker and delivered as GCE startup-script metadata.
set -Eeuo pipefail
set +x
exec > >(tee -a /var/log/gha-runner-bootstrap.log) 2>&1

export DEBIAN_FRONTEND=noninteractive
DEVICE="{{DISK_DEVICE}}"
MOUNT_POINT=/mnt/runner-data
if [[ ! -b "$DEVICE" ]]; then echo "Missing persistent runner disk $DEVICE" >&2; exit 1; fi
if ! blkid "$DEVICE" >/dev/null 2>&1; then mkfs.ext4 -F "$DEVICE"; fi
mkdir -p "$MOUNT_POINT"
UUID=$(blkid -s UUID -o value "$DEVICE")
grep -q "$UUID" /etc/fstab || echo "UUID=$UUID $MOUNT_POINT ext4 defaults,nofail 0 2" >>/etc/fstab
mountpoint -q "$MOUNT_POINT" || mount "$MOUNT_POINT"
mkdir -p "$MOUNT_POINT/apt/lists/partial" "$MOUNT_POINT/apt/cache/archives/partial"
cat >/etc/apt/apt.conf.d/99runner-data <<EOF
Dir::State::lists "$MOUNT_POINT/apt/lists";
Dir::Cache "$MOUNT_POINT/apt/cache";
EOF

required_packages=(ca-certificates curl gnupg git jq xfsprogs util-linux rsync)
missing_packages=()
for package in "${required_packages[@]}"; do
  dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q 'install ok installed' || missing_packages+=("$package")
done
if ((${#missing_packages[@]})); then
  apt-get clean
  rm -rf /var/lib/apt/lists/*
  apt-get update
  apt-get install -y "${missing_packages[@]}"
fi
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian $(. /etc/os-release && echo "$VERSION_CODENAME") stable" >/etc/apt/sources.list.d/docker.list
apt-get clean
rm -rf /var/lib/apt/lists/*
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
mkdir -p "$MOUNT_POINT/docker" "$MOUNT_POINT/instances" "$MOUNT_POINT/work"
mkdir -p "$MOUNT_POINT/containerd"
cat >/etc/docker/daemon.json <<EOF
{"data-root":"$MOUNT_POINT/docker","log-driver":"local","log-opts":{"max-size":"20m","max-file":"3"}}
EOF
# Docker's data-root alone does not move containerd snapshots. Copy its current
# content once, then point containerd at the persistent data disk before Docker
# starts. This puts image overlays (what `df /` inside a job sees) on the 100 GB
# disk too, rather than filling the VM's small boot disk.
systemctl stop docker || true
systemctl stop containerd || true
containerd_exec=$(systemctl show -p ExecStart --value containerd)
if [[ "$containerd_exec" != *"--root $MOUNT_POINT/containerd"* ]]; then
  # Repeat an interrupted copy safely from the untouched boot-disk source.
  cp -a /var/lib/containerd/. "$MOUNT_POINT/containerd/"
fi
install -d -m 0755 /etc/systemd/system/containerd.service.d
cat >/etc/systemd/system/containerd.service.d/runner-data.conf <<EOF
[Service]
ExecStart=
ExecStart=/usr/bin/containerd --root $MOUNT_POINT/containerd --state /run/containerd
EOF
# Job and build containers must not reach the GCE metadata server (HTTP; DNS stays open). It serves this instance's
# attributes (bootstrap credentials) and, where the VM has a service account, its access token.
# Host services keep metadata access: DOCKER-USER only filters traffic forwarded from containers.
# The unit runs before Docker so restarted containers never start without the rule.
cat >/usr/local/sbin/gce-metadata-container-block <<'METADATA_BLOCK'
#!/bin/bash
set -euo pipefail
# Rebuild the rules on every run so the DNS exceptions always sit above the REJECT.
# The metadata address is also the VPC DNS resolver containers inherit from the host, so DNS
# (port 53) returns to Docker's own rules and everything else to that address is rejected.
block() {
  local cmd=$1 addr=$2 rule
  "$cmd" -N DOCKER-USER 2>/dev/null || true
  for rule in "-p udp --dport 53 -j RETURN" "-p tcp --dport 53 -j RETURN" "-j REJECT"; do
    # shellcheck disable=SC2086
    while "$cmd" -D DOCKER-USER -d "$addr" $rule 2>/dev/null; do :; done
  done
  "$cmd" -I DOCKER-USER 1 -d "$addr" -j REJECT
  "$cmd" -I DOCKER-USER 1 -d "$addr" -p tcp --dport 53 -j RETURN
  "$cmd" -I DOCKER-USER 1 -d "$addr" -p udp --dport 53 -j RETURN
}
block iptables 169.254.169.254/32
# The IPv6 metadata address only answers on IPv6-enabled networks, so this part is best effort.
block ip6tables fd20:ce::254/128 2>/dev/null || true
METADATA_BLOCK
chmod 0755 /usr/local/sbin/gce-metadata-container-block
cat >/etc/systemd/system/gce-metadata-container-block.service <<'METADATA_BLOCK_UNIT'
[Unit]
Description=Block container access to the GCE metadata server
Before=docker.service
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/gce-metadata-container-block
[Install]
WantedBy=docker.service
METADATA_BLOCK_UNIT
systemctl daemon-reload
systemctl enable gce-metadata-container-block.service
/usr/local/sbin/gce-metadata-container-block
systemctl daemon-reload
systemctl enable --now containerd
systemctl enable docker
systemctl restart docker
containerd_exec=$(systemctl show -p ExecStart --value containerd)
[[ "$containerd_exec" == *"--root $MOUNT_POINT/containerd"* ]] || { echo "containerd service root is unexpected: $containerd_exec" >&2; exit 1; }
docker_root=$(docker info --format=json | jq -r .DockerRootDir)
[[ "$docker_root" == "$MOUNT_POINT/docker" ]] || { echo "Docker root is $docker_root, expected $MOUNT_POINT/docker" >&2; exit 1; }
# Refuse to start runners or builds if containers could still reach the metadata server.
iptables -C FORWARD -j DOCKER-USER && iptables -C DOCKER-USER -d 169.254.169.254/32 -j REJECT &&
  iptables -C DOCKER-USER -d 169.254.169.254/32 -p udp --dport 53 -j RETURN ||
  { echo "Container metadata block is not in place" >&2; exit 1; }
# Remove the previous default roots after both services use the persistent disk.
rm -rf /var/lib/containerd/* /var/lib/docker/* /var/lib/apt/lists/* /var/cache/apt/archives/*
apt-get clean

install -d -m 0755 /opt/gha-runners/build
printf '%s' '{{DOCKERFILE_B64}}' | base64 -d >/opt/gha-runners/build/Dockerfile
printf '%s' '{{ENTRYPOINT_B64}}' | base64 -d >/opt/gha-runners/build/entrypoint.sh
printf '%s' '{{JOB_COMPLETED_B64}}' | base64 -d >/opt/gha-runners/build/job-completed.sh
printf '%s' '{{JOB_STARTED_B64}}' | base64 -d >/opt/gha-runners/build/job-started.sh

# Pure-Rust host-agent binary placement and symlink setup
install -d -m 0755 /usr/local/bin
if [[ -x /usr/local/bin/host-agent ]]; then
  cp /usr/local/bin/host-agent /opt/gha-runners/build/host-agent
fi
for alias in clean-target host-admission docker-wrapper docker-admission \
             host-launcher builder-entrypoint worktree-commands toolchain-probe \
             telemetry gha-host-telemetry host-telemetry \
             gha-runner-clean-target gha-host-admission oxidex-remote-build \
             oxidex-build-entrypoint oxidex-worktree-commands oxidex-toolchain-probe; do
  ln -sf /usr/local/bin/host-agent "/usr/local/bin/$alias"
done

TIERED={{TIERED}}
STRICT_HEAVY_POOL={{STRICT_HEAVY_POOL}}
if [[ "$TIERED" == 1 ]]; then
  install -d -m 0755 /run/gha-admission
  ln -sf /usr/local/bin/host-agent /usr/local/bin/docker

  hostname_short=$(hostname -s)
  runner_names=$(jq -n --arg prefix "{{RUNNER_PREFIX}}" --arg host "$hostname_short" --argjson count {{RUNNER_COUNT}} \
    '[range(1; ($count + 1) | floor) | "\($prefix)-\($host)-\(.)"]')
  if [[ "{{STRICT_HEAVY_POOL}}" == "1" || "{{STRICT_HEAVY_POOL}}" == "True" || "{{STRICT_HEAVY_POOL}}" == "true" ]]; then
    jq -n \
      --argjson work_gib {{ADMISSION_WORK_GIB}} \
      --argjson runner_gib {{RUNNER_MEMORY_GB}} \
      --argjson builder_gib {{BUILDER_MEMORY_GIB}} \
      --argjson names "$runner_names" \
      '{work_gib: $work_gib, runner_gib: $runner_gib, builder_gib: $builder_gib, runner_names: $names, role: "runner", heavy_runners: $names}' \
      >/etc/gha-admission.json
  else
    jq -n \
      --argjson work_gib {{ADMISSION_WORK_GIB}} \
      --argjson runner_gib {{RUNNER_MEMORY_GB}} \
      --argjson builder_gib {{BUILDER_MEMORY_GIB}} \
      --argjson names "$runner_names" \
      '{work_gib: $work_gib, runner_gib: $runner_gib, builder_gib: $builder_gib, runner_names: $names}' \
      >/etc/gha-admission.json
  fi
  chmod 0644 /etc/gha-admission.json
  cat >/etc/systemd/system/gha-admission.service <<'ADMISSION_SERVICE'
[Unit]
Description=Shared runner and builder memory admission (host-agent)
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
  [[ -S /run/gha-admission/socket ]] || exit 1
fi
chmod 0755 /opt/gha-runners/build/entrypoint.sh
docker build --pull -t oxidex-self-hosted-runner:{{RUNNER_VERSION}} /opt/gha-runners/build

REMOTE_BUILDER={{REMOTE_BUILDER}}
if [[ "$REMOTE_BUILDER" == 1 ]]; then
  install -d -m 0755 /opt/gha-runners/builder
  printf '%s' '{{BUILDER_DOCKERFILE_B64}}' | base64 -d >/opt/gha-runners/builder/Dockerfile
  cp /usr/local/bin/host-agent /opt/gha-runners/builder/host-agent
  printf '%s\n' '{"cpus":{{BUILDER_CPUS}},"memory_gib":{{BUILDER_MEMORY_GIB}}}' >/etc/oxidex-remote-build.json
  chmod 0644 /etc/oxidex-remote-build.json
  docker build -t oxidex-remote-builder:1.97.1 /opt/gha-runners/builder
fi

ORG_URL=https://github.com/{{ORG}}
RUNNER_LABELS='{{RUNNER_LABELS}}'
RUNNER_GROUP='{{RUNNER_GROUP}}'
RUNNER_PREFIX='{{RUNNER_PREFIX}}'
RUNNER_IMAGE=$(docker image inspect oxidex-self-hosted-runner:{{RUNNER_VERSION}} | jq -er '.[0].Id')
[[ "$RUNNER_IMAGE" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 1
RUST_VOLUME="gha-rust-${RUNNER_IMAGE#sha256:}"
REGISTRATION_TOKEN={{REGISTRATION_TOKEN}}
RUNNER_COUNT={{RUNNER_COUNT}}
RUNNER_MEMORY_GB={{RUNNER_MEMORY_GB}}
# Empty unless fixed sizing requested an explicit per-runner vCPU cap.
RUNNER_CPU_LIMIT='{{RUNNER_CPU_LIMIT}}'
# Docker Engine storage (images, layers, build cache and containers) must stay
# on the mounted persistent data disk. Stop if it did not mount as expected.
docker_root=$(docker info --format=json | jq -r .DockerRootDir)
[[ "$docker_root" == "$MOUNT_POINT/docker" ]] || { echo "Docker root is $docker_root, expected $MOUNT_POINT/docker" >&2; exit 1; }
# Root-owned job-start cordon directory, bind-mounted read-only into every runner container. The
# controller places /run/gha-runner-cordon/draining while it retires this host, and the job-started hook
# refuses each new job while it exists. Docker recreates the empty directory after a reboot.
install -d -o root -g root -m 0755 /run/gha-runner-cordon
for i in $(seq 1 "$RUNNER_COUNT"); do
  name="${RUNNER_PREFIX}-$(hostname -s)-${i}"
  state="$MOUNT_POINT/instances/$name"
  work="$MOUNT_POINT/work/$name"
  mkdir -p "$state" "$work"
  chown -R 1001:1001 "$state" "$work"
  runner_labels="$RUNNER_LABELS"
  env_file="/run/${name}.env"
  umask 077
  printf 'GITHUB_RUNNER_TOKEN=%s\n' "$REGISTRATION_TOKEN" >"$env_file"
  docker run --rm --env-file "$env_file" -e RUNNER_CONFIGURE_ONLY=1 \
    -e RUNNER_NAME="$name" -e RUNNER_LABELS="$runner_labels" \
    -e RUNNER_GROUP_NAME="$RUNNER_GROUP" -e GITHUB_RUNNER_URL="$ORG_URL" \
    -e RUNNER_STATE=/runner-state \
    -v "$state:/runner-state" -v "$work:/work" "$RUNNER_IMAGE"
  rm -f "$env_file"
  docker rm -f "$name" >/dev/null 2>&1 || true
  # Equal CPU weights split cores only under contention; an idle host lets one
  # job use every vCPU. Memory stays capped so one job cannot starve the others.
  docker_args=(-d --restart unless-stopped --cpu-shares=1024 --memory="${RUNNER_MEMORY_GB}g" \
    --name "$name" -e RUNNER_NAME="$name" -e RUNNER_LABELS="$runner_labels" \
    -e RUNNER_GROUP_NAME="$RUNNER_GROUP" -e GITHUB_RUNNER_URL="$ORG_URL" \
    -e RUNNER_STATE=/runner-state -v "$state:/runner-state" -v "$work:/work" \
    -v /run/gha-runner-cordon:/run/gha-runner-cordon:ro)
  if [[ "$TIERED" == 1 ]]; then
    docker_args+=(-e GHA_ADMISSION_REQUIRED=1 -v /run/gha-admission:/run/gha-admission:ro)
  fi
  [[ -z "$RUNNER_CPU_LIMIT" ]] || docker_args+=(--cpus="$RUNNER_CPU_LIMIT")
  docker_args+=(--mount "type=volume,src=$RUST_VOLUME,dst=/opt/runner-rust,readonly")
  [[ "$STRICT_HEAVY_POOL" != 1 ]] || docker_args+=(--memory-swap="32g")
  docker_args+=("$RUNNER_IMAGE")
  docker run "${docker_args[@]}"
done
RESOURCE_MONITORING={{RESOURCE_MONITORING}}
if [[ "$RESOURCE_MONITORING" == 1 ]]; then
  if [[ -x /usr/local/bin/host-agent ]]; then
    cat >/etc/systemd/system/gha-telemetry.service <<'TELEMETRY_SERVICE'
[Unit]
Description=Runner cgroup v2, workflow correlation, and Prometheus telemetry exporter (host-agent)
After=docker.service network-online.target
[Service]
ExecStart=/usr/local/bin/host-agent telemetry serve --listen 127.0.0.1:9464
Restart=always
RestartSec=5
[Install]
WantedBy=multi-user.target
TELEMETRY_SERVICE
    systemctl daemon-reload
    systemctl enable --now --no-block gha-telemetry.service || true
  fi
  install -d -m 0755 /etc/google-cloud-ops-agent
  cat >/etc/google-cloud-ops-agent/config.yaml <<'METRICS'
logging:
  receivers:
    systemd_journald:
      type: systemd_journald
    runner_bootstrap:
      type: files
      include_paths:
        - /var/log/gha-runner-bootstrap.log
        - /var/log/oxidex-builder-bootstrap.log
    gha_workflow_logs:
      type: files
      include_paths:
        - /var/log/gha-host-telemetry.jsonl
  processors:
    parse_workflow_json:
      type: parse_json
  service:
    pipelines:
      default_pipeline:
        receivers: [systemd_journald, runner_bootstrap]
      workflow_pipeline:
        receivers: [gha_workflow_logs]
        processors: [parse_workflow_json]
metrics:
  receivers:
    hostmetrics:
      type: hostmetrics
      collection_interval: 30s
    gha_telemetry:
      type: prometheus
      config:
        scrape_configs:
          - job_name: 'gha_host_agent'
            scrape_interval: 30s
            static_configs:
              - targets: ['127.0.0.1:9464']
  processors:
    metrics_filter:
      type: exclude_metrics
      metrics_pattern:
        - agent.googleapis.com/processes/*
  service:
    pipelines:
      default_pipeline:
        receivers: [hostmetrics, gha_telemetry]
        processors: [metrics_filter]
METRICS
  if dpkg-query -W -f='${Status}' google-cloud-ops-agent 2>/dev/null | grep -q 'install ok installed'; then
    systemctl reload-or-restart --no-block google-cloud-ops-agent || true
  else
    systemd-run --unit=gha-ops-agent-bootstrap --no-block /bin/bash -c '
      curl --proto "=https" --tlsv1.2 -fsS https://dl.google.com/cloudagents/add-google-cloud-ops-agent-repo.sh -o /run/install-ops-agent.sh &&
      bash /run/install-ops-agent.sh --also-install &&
      rm -f /run/install-ops-agent.sh &&
      systemctl restart google-cloud-ops-agent
    ' || true
  fi
fi
unset REGISTRATION_TOKEN
printf '%s\n' '{{DEPLOYMENT_ID}}' >/var/lib/gha-runner-bootstrap.complete
