//! Runner pools: Spot hosts an administrator launches from the dashboard.
//!
//! A pool names one machine shape in one zone, how many hosts to keep, how many runners each host
//! runs and how big each runner is, the organization or repository the runners register with,
//! extra labels, and optionally a cloud-init template whose shell commands run while the runner
//! image builds. The administrator creates a pool with `POST /api/runner-pools`; the reconciler
//! then launches one missing host per pool on each tick and replaces hosts Spot takes back.
//! A pool with `min_hosts` below `hosts` scales itself: it starts hosts while jobs that its
//! runners can take wait in GitHub's queue, and deletes hosts that stayed idle for
//! `idle_minutes`, down to `min_hosts` (often zero).
//! `POST /api/runner-pools/delete` marks a pool as removing; the reconciler deletes its hosts and
//! runner registrations and then drops the record.
//!
//! Pools use the built-in runner bootstrap (`cloud-init/startup.sh`). Config-repo patches and
//! repository specs apply to floor hosts only.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::state::{
    load_authoritative_state, load_state, CloudInitTemplate, DashboardState, KvStoreBackend,
    StateError,
};

/// Host names start with this, so the dashboard's instance views and stale-runner cleanup treat
/// pool hosts like other runner hosts.
pub const POOL_HOST_PREFIX: &str = "oxidex-runners-pool-";
/// Instance metadata key and label that tie a host to its pool.
pub const POOL_METADATA_KEY: &str = "spot-pool";
/// Runner names start with this (`<prefix>-<host>-<n>`), which stale-runner cleanup recognises.
pub const POOL_RUNNER_PREFIX: &str = "oxidex";

pub const MAX_POOLS: usize = 20;
pub const MAX_HOSTS_PER_POOL: u32 = 10;
pub const MAX_RUNNERS_PER_HOST: u32 = 8;
pub const MAX_EXTRA_LABELS: usize = 10;
pub const MAX_SETUP_SCRIPT_BYTES: usize = 64 * 1024;
/// Memory left for Debian, Docker, the admission service and the Ops Agent.
pub const HOST_RESERVED_RAM_GIB: u32 = 2;
pub const POOL_ID_LEN: usize = 6;
/// How long an idle host stays before an autoscaling pool deletes it, when the pool doesn't say.
pub const DEFAULT_IDLE_MINUTES: u32 = 10;
pub const MIN_IDLE_MINUTES: u32 = 5;
pub const MAX_IDLE_MINUTES: u32 = 240;
/// A new host isn't idle until its runners had time to come online: building the runner image
/// takes several minutes, longer on 2 vCPU hosts.
pub const HOST_BOOT_GRACE_SECS: u64 = 25 * 60;

/// One pool, as an administrator created it. The reconciler never edits these fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerPool {
    pub id: String,
    /// `org` or `owner/repo`.
    pub target: String,
    pub machine_type: String,
    pub zone: String,
    pub hosts: u32,
    pub runners_per_host: u32,
    pub runner_vcpu: u32,
    pub runner_ram_gib: u32,
    /// Labels added to the built-in ones (self-hosted, Linux, X64 or ARM64, spot, shape, zone).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Empty for the built-in runner setup only.
    #[serde(default)]
    pub cloud_init_template_id: String,
    #[serde(default)]
    pub cloud_init_template_name: String,
    /// The template's shell commands as they were when the pool was created, so editing the
    /// template later doesn't change hosts the pool launches.
    #[serde(default)]
    pub setup_script: String,
    /// Estimated Spot cost of one host (compute and data disk), when the price table knows it.
    #[serde(default)]
    pub host_hourly_usd: Option<f64>,
    pub created_at: u64,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub removing: bool,
    /// Fewest hosts an autoscaling pool keeps. `None` (pools made before autoscaling) or a value
    /// equal to `hosts` keeps the pool at a fixed size; `hosts` is then also the most it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_hosts: Option<u32>,
    /// Minutes a host of an autoscaling pool may sit idle before it's deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_minutes: Option<u32>,
    /// When the pool last switched templates. Idle hosts started before then are replaced with
    /// hosts that run the new template, one per reconciler tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_updated_at: Option<u64>,
}

impl RunnerPool {
    /// Fewest hosts the pool keeps.
    pub fn min_hosts(&self) -> u32 {
        self.min_hosts.unwrap_or(self.hosts).min(self.hosts)
    }
    /// Whether the pool starts and stops hosts by itself.
    pub fn autoscales(&self) -> bool {
        self.min_hosts() < self.hosts
    }
    pub fn idle_minutes(&self) -> u32 {
        self.idle_minutes
            .unwrap_or(DEFAULT_IDLE_MINUTES)
            .clamp(MIN_IDLE_MINUTES, MAX_IDLE_MINUTES)
    }
}

/// What the reconciler last saw for a pool. Written by the reconciler only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RunnerPoolStatus {
    pub hosts_running: u32,
    pub runners_online: u32,
    #[serde(default)]
    pub last_launch_host: Option<String>,
    #[serde(default)]
    pub last_launch_at: Option<u64>,
    /// Why the last launch attempt failed, including a create GCE refused after the call
    /// returned (no capacity, quota). Cleared once a launch succeeds or the pool is full.
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_at: Option<u64>,
    /// The GCE operation of the last create, until the reconciler has read its outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_launch_op: Option<String>,
    /// Autoscaling pools: jobs in GitHub's queue that the pool's runners could take, as of the
    /// last tick. `None` when the queue couldn't be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_jobs: Option<u32>,
    /// Autoscaling pools: hosts the pool wanted on the last tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_hosts: Option<u32>,
    /// Autoscaling pools: when each idle host was first seen idle (host name to epoch seconds).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub idle_since: BTreeMap<String, u64>,
    /// When the pool last deleted an idle host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scale_down_at: Option<u64>,
}

/// Body of `POST /api/runner-pools`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateRunnerPoolRequest {
    pub target: String,
    pub machine_type: String,
    pub zone: String,
    #[serde(default = "default_hosts")]
    pub hosts: u32,
    pub runners_per_host: u32,
    pub runner_vcpu: u32,
    pub runner_ram_gib: u32,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub cloud_init_template_id: String,
    /// Fewest hosts to keep; leave out (or equal to `hosts`) for a fixed-size pool.
    #[serde(default)]
    pub min_hosts: Option<u32>,
    #[serde(default)]
    pub idle_minutes: Option<u32>,
}

fn default_hosts() -> u32 {
    1
}

/// Body of `POST /api/runner-pools/delete`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoveRunnerPoolRequest {
    pub id: String,
}

/// Body of `POST /api/runner-pools/resize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResizeRunnerPoolRequest {
    pub id: String,
    pub hosts: u32,
    /// New fewest hosts for an autoscaling pool; leave out to keep the pool's current setting.
    #[serde(default)]
    pub min_hosts: Option<u32>,
}

/// Body of `POST /api/runner-pools/template`: the template new hosts of the pool use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetPoolTemplateRequest {
    pub id: String,
    /// Empty for the built-in runner setup only.
    #[serde(default)]
    pub cloud_init_template_id: String,
}

/// vCPU and RAM (GiB) of each machine type a pool may use: the runner allowlists in `patch.rs`.
pub fn machine_shape(machine_type: &str) -> Option<(u32, u32)> {
    Some(match machine_type {
        "c4d-highcpu-16" => (16, 30),
        "n4d-highcpu-16" => (16, 32),
        "n4-highcpu-16" => (16, 32),
        "c2d-highcpu-16" => (16, 32),
        "c4a-highcpu-16" => (16, 32),
        "c4a-standard-16" => (16, 64),
        "c4a-highcpu-8" => (8, 16),
        "c4a-standard-8" => (8, 32),
        "n4a-highcpu-16" => (16, 32),
        "n4a-standard-16" => (16, 64),
        "n4a-highcpu-8" => (8, 16),
        "n4a-standard-8" => (8, 32),
        "t2a-standard-16" => (16, 64),
        "t2a-standard-8" => (8, 32),
        // Small hosts, for pools of one or two small runners.
        "n4a-standard-2" => (2, 8),
        "n4a-standard-4" => (4, 16),
        "c4a-standard-2" => (2, 8),
        "c4a-standard-4" => (4, 16),
        "t2a-standard-2" => (2, 8),
        "t2a-standard-4" => (4, 16),
        "t2d-standard-2" => (2, 8),
        "t2d-standard-4" => (4, 16),
        "e2-standard-2" => (2, 8),
        "e2-standard-4" => (4, 16),
        _ => return None,
    })
}

/// 2 and 4 vCPU machine types only runner pools launch. They stay out of the runner patch
/// allowlists in `patch.rs`, which the fleet floor uses.
pub const POOL_SMALL_MACHINE_TYPES: &[&str] = &[
    "n4a-standard-2",
    "n4a-standard-4",
    "c4a-standard-2",
    "c4a-standard-4",
    "t2a-standard-2",
    "t2a-standard-4",
    "t2d-standard-2",
    "t2d-standard-4",
    "e2-standard-2",
    "e2-standard-4",
];

/// Machine types a pool can launch, in a stable order.
pub fn launchable_machine_types() -> Vec<&'static str> {
    crate::patch::PERMITTED_MACHINE_TYPES
        .iter()
        .chain(crate::patch::PERMITTED_ARM64_MACHINE_TYPES.iter())
        .chain(POOL_SMALL_MACHINE_TYPES.iter())
        .copied()
        .filter(|mt| machine_shape(mt).is_some())
        .collect()
}

pub fn is_arm64_machine_type(machine_type: &str) -> bool {
    machine_type.starts_with("c4a-")
        || machine_type.starts_with("n4a-")
        || machine_type.starts_with("t2a-")
}

/// `org` or `owner/repo`, in GitHub's character set. The value reaches the startup script, the
/// GitHub API path and instance metadata, so nothing else is allowed.
pub fn is_valid_target(target: &str) -> bool {
    let mut parts = target.split('/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next();
    if parts.next().is_some() {
        return false;
    }
    let owner_ok = !owner.is_empty()
        && owner.len() <= 39
        && !owner.starts_with('-')
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let repo_ok = match repo {
        None => true,
        Some(r) => {
            !r.is_empty()
                && r.len() <= 100
                && r != "."
                && r != ".."
                && r.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        }
    };
    owner_ok && repo_ok
}

/// Every GCE zone the project can use (`gcloud compute zones list`, 2026-11-01), separated by
/// spaces. A zone outside this list would only fail later, at VM create, so pools refuse it.
pub const KNOWN_GCE_ZONES: &str = "\
    africa-south1-a africa-south1-b africa-south1-c asia-east1-a asia-east1-b asia-east1-c \
    asia-east2-a asia-east2-b asia-east2-c asia-northeast1-a asia-northeast1-b \
    asia-northeast1-c asia-northeast2-a asia-northeast2-b asia-northeast2-c \
    asia-northeast3-a asia-northeast3-b asia-northeast3-c asia-south1-a asia-south1-b \
    asia-south1-c asia-south2-a asia-south2-b asia-south2-c asia-southeast1-a \
    asia-southeast1-b asia-southeast1-c asia-southeast2-a asia-southeast2-b \
    asia-southeast2-c asia-southeast3-a asia-southeast3-b asia-southeast3-c \
    australia-southeast1-a australia-southeast1-b australia-southeast1-c \
    australia-southeast2-a australia-southeast2-b australia-southeast2-c europe-central2-a \
    europe-central2-b europe-central2-c europe-north1-a europe-north1-b europe-north1-c \
    europe-north2-a europe-north2-b europe-north2-c europe-southwest1-a europe-southwest1-b \
    europe-southwest1-c europe-west1-b europe-west1-c europe-west1-d europe-west10-a \
    europe-west10-b europe-west10-c europe-west12-a europe-west12-b europe-west12-c \
    europe-west2-a europe-west2-b europe-west2-c europe-west3-a europe-west3-b \
    europe-west3-c europe-west4-a europe-west4-b europe-west4-c europe-west6-a \
    europe-west6-b europe-west6-c europe-west8-a europe-west8-b europe-west8-c \
    europe-west9-a europe-west9-b europe-west9-c me-central1-a me-central1-b me-central1-c \
    me-central2-a me-central2-b me-central2-c me-west1-a me-west1-b me-west1-c \
    northamerica-northeast1-a northamerica-northeast1-b northamerica-northeast1-c \
    northamerica-northeast2-a northamerica-northeast2-b northamerica-northeast2-c \
    northamerica-south1-a northamerica-south1-b northamerica-south1-c southamerica-east1-a \
    southamerica-east1-b southamerica-east1-c southamerica-west1-a southamerica-west1-b \
    southamerica-west1-c us-central1-a us-central1-b us-central1-c us-central1-f us-east1-b \
    us-east1-c us-east1-d us-east4-a us-east4-b us-east4-c us-east5-a us-east5-b us-east5-c \
    us-south1-a us-south1-b us-south1-c us-west1-a us-west1-b us-west1-c us-west2-a \
    us-west2-b us-west2-c us-west3-a us-west3-b us-west3-c us-west4-a us-west4-b us-west4-c ";

/// Whether `zone` is in [`KNOWN_GCE_ZONES`].
pub fn is_known_gce_zone(zone: &str) -> bool {
    KNOWN_GCE_ZONES.split_whitespace().any(|z| z == zone)
}

/// A GCE zone name such as `us-west1-a`.
pub fn is_valid_zone(zone: &str) -> bool {
    let parts: Vec<&str> = zone.split('-').collect();
    zone.len() <= 32
        && parts.len() == 3
        && !parts[0].is_empty()
        && parts[0].bytes().all(|b| b.is_ascii_lowercase())
        && parts[1].len() >= 2
        && parts[1]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && parts[1].bytes().any(|b| b.is_ascii_digit())
        && parts[2].len() == 1
        && parts[2].bytes().all(|b| b.is_ascii_lowercase())
}

/// A runner label: GitHub's API and the startup script (inside single quotes) both accept these.
pub fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 64
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The labels every pool host's runners carry.
pub fn builtin_labels(machine_type: &str, zone: &str) -> Vec<String> {
    vec![
        "self-hosted".to_string(),
        "Linux".to_string(),
        if is_arm64_machine_type(machine_type) {
            "ARM64"
        } else {
            "X64"
        }
        .to_string(),
        "spot".to_string(),
        machine_type.to_string(),
        zone.to_string(),
    ]
}

/// Built-in labels followed by the pool's own, without case-insensitive duplicates.
pub fn pool_labels(pool: &RunnerPool) -> Vec<String> {
    let mut labels = builtin_labels(&pool.machine_type, &pool.zone);
    for label in &pool.labels {
        if !labels.iter().any(|l| l.eq_ignore_ascii_case(label)) {
            labels.push(label.clone());
        }
    }
    labels
}

/// Whether a runner with `pool_labels` can take a job that asks for `job_labels`: GitHub hands a
/// job only to a runner that has every label in its `runs-on`, compared without case.
pub fn job_fits_labels(job_labels: &[String], pool_labels: &[String]) -> bool {
    !job_labels.is_empty()
        && job_labels
            .iter()
            .all(|j| pool_labels.iter().any(|p| p.eq_ignore_ascii_case(j)))
}

/// The labels to put in a workflow's `runs-on` to reach this pool: `self-hosted`, the
/// architecture, `spot` and the pool's own labels. Without labels of its own, the machine type
/// keeps jobs off other hosts.
pub fn runs_on_labels(pool: &RunnerPool) -> Vec<String> {
    let mut out = vec!["self-hosted".to_string()];
    out.push(
        if is_arm64_machine_type(&pool.machine_type) {
            "ARM64"
        } else {
            "X64"
        }
        .to_string(),
    );
    out.push("spot".to_string());
    if pool.labels.is_empty() {
        out.push(pool.machine_type.clone());
    } else {
        out.extend(pool.labels.iter().cloned());
    }
    out
}

/// Host names for a pool start with this; the instance name adds `<epoch>-<8 hex>`.
pub fn pool_host_name_prefix(pool_id: &str) -> String {
    format!("{}{}-", POOL_HOST_PREFIX, pool_id)
}

pub fn pool_host_name(pool_id: &str, now: u64, suffix: &str) -> String {
    format!("{}{}-{}", pool_host_name_prefix(pool_id), now, suffix)
}

/// GitHub API path for registering and listing the pool's runners.
pub fn target_api_path(target: &str) -> String {
    if target.contains('/') {
        format!("repos/{}", target)
    } else {
        format!("orgs/{}", target)
    }
}

/// Runner group name for `config.sh --runnergroup`, and the group id the GitHub client takes.
/// The swack-tools organization keeps its runners in `swack-runners`; other organizations use
/// their default group, and repository runners have no group.
pub fn runner_group_for(target: &str) -> (&'static str, u32) {
    if target.contains('/') {
        ("", 1)
    } else if target.eq_ignore_ascii_case("swack-tools") {
        ("swack-runners", 10)
    } else {
        ("", 1)
    }
}

/// Targets the dashboard offers in the dropdown: the fleet's own targets, then any target a pool
/// already uses. Other organizations or repositories can still be typed in.
pub fn suggested_targets(state: &DashboardState) -> Vec<String> {
    let mut out: Vec<String> = if state.controlled_targets.is_empty() {
        crate::patch::CONTROLLED_TARGETS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        state.controlled_targets.clone()
    };
    for t in crate::patch::ON_DEMAND_TARGETS {
        if !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    for p in &state.runner_pools {
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&p.target)) {
            out.push(p.target.clone());
        }
    }
    out
}

/// Why a template can't be used as a pool's setup commands, if it can't.
pub fn template_unusable_reason(t: &CloudInitTemplate) -> Option<String> {
    if t.role != "cicd_runner" {
        return Some(format!("template is for {}, not CI/CD runners", t.role));
    }
    let body = t.content.trim_start();
    if body.starts_with("#cloud-config") {
        return Some(
            "cloud-config YAML can't run on these hosts; use a shell script template".to_string(),
        );
    }
    if body.is_empty() {
        return Some("template is empty".to_string());
    }
    if t.content.len() > MAX_SETUP_SCRIPT_BYTES {
        return Some(format!(
            "template is {} bytes; pools take at most {}",
            t.content.len(),
            MAX_SETUP_SCRIPT_BYTES
        ));
    }
    None
}

fn new_pool_id(existing: &[RunnerPool], now: u64) -> String {
    for attempt in 0u64..16 {
        let mut bytes = [0u8; 3];
        let id = if getrandom::getrandom(&mut bytes).is_ok() {
            hex::encode(bytes)
        } else {
            format!(
                "{:06x}",
                (now.wrapping_add(attempt.wrapping_mul(7919))) & 0xff_ffff
            )
        };
        if !existing.iter().any(|p| p.id == id) {
            return id;
        }
    }
    format!("{:06x}", now & 0xff_ffff)
}

/// Checks a create request against the current state and returns the pool to store.
pub fn validate_create_request(
    req: &CreateRunnerPoolRequest,
    state: &DashboardState,
    created_by: &str,
    now: u64,
) -> Result<RunnerPool, String> {
    let target = req.target.trim();
    if !is_valid_target(target) {
        return Err(format!(
            "'{}' isn't an organization or owner/repository name",
            target
        ));
    }
    let machine_type = req.machine_type.trim();
    let Some((host_vcpu, host_ram)) = machine_shape(machine_type) else {
        return Err(format!(
            "{} can't be launched as a runner host yet; pick one of: {}",
            machine_type,
            launchable_machine_types().join(", ")
        ));
    };
    let zone = req.zone.trim();
    if !is_valid_zone(zone) {
        return Err(format!("'{}' isn't a zone name", zone));
    }
    if !is_known_gce_zone(zone) {
        return Err(format!("'{}' isn't a GCE zone this project can use", zone));
    }
    if req.hosts == 0 || req.hosts > MAX_HOSTS_PER_POOL {
        return Err(format!("Hosts must be 1 to {}", MAX_HOSTS_PER_POOL));
    }
    if req.runners_per_host == 0 || req.runners_per_host > MAX_RUNNERS_PER_HOST {
        return Err(format!(
            "Runners per host must be 1 to {}",
            MAX_RUNNERS_PER_HOST
        ));
    }
    let min_hosts = match req.min_hosts {
        Some(m) if m > req.hosts => {
            return Err(format!(
                "Minimum hosts ({}) can't be more than the most hosts ({})",
                m, req.hosts
            ))
        }
        Some(m) if m < req.hosts => Some(m),
        _ => None,
    };
    let idle_minutes = match req.idle_minutes {
        Some(m) if !(MIN_IDLE_MINUTES..=MAX_IDLE_MINUTES).contains(&m) => {
            return Err(format!(
                "Idle minutes must be {} to {}",
                MIN_IDLE_MINUTES, MAX_IDLE_MINUTES
            ))
        }
        Some(m) if min_hosts.is_some() => Some(m),
        _ => None,
    };
    if req.runner_vcpu == 0 || req.runner_ram_gib == 0 {
        return Err("Each runner needs at least 1 vCPU and 1 GiB".to_string());
    }
    let vcpu_needed = req.runners_per_host.saturating_mul(req.runner_vcpu);
    if vcpu_needed > host_vcpu {
        return Err(format!(
            "{} runners x {} vCPU = {} vCPU, but {} has {}",
            req.runners_per_host, req.runner_vcpu, vcpu_needed, machine_type, host_vcpu
        ));
    }
    let ram_needed = req.runners_per_host.saturating_mul(req.runner_ram_gib);
    let ram_free = host_ram.saturating_sub(HOST_RESERVED_RAM_GIB);
    if ram_needed > ram_free {
        return Err(format!(
            "{} runners x {} GiB = {} GiB, but {} has {} GiB and keeps {} GiB for the host",
            req.runners_per_host,
            req.runner_ram_gib,
            ram_needed,
            machine_type,
            host_ram,
            HOST_RESERVED_RAM_GIB
        ));
    }

    if req.labels.len() > MAX_EXTRA_LABELS {
        return Err(format!("At most {} extra labels", MAX_EXTRA_LABELS));
    }
    let builtin = builtin_labels(machine_type, zone);
    let mut labels: Vec<String> = Vec::new();
    for raw in &req.labels {
        let label = raw.trim();
        if label.is_empty() {
            continue;
        }
        if !is_valid_label(label) {
            return Err(format!(
                "Label '{}' may use only letters, digits, '.', '_' and '-' (64 at most)",
                label
            ));
        }
        // A label that names another architecture would send jobs to hosts that can't run them.
        let lower = label.to_ascii_lowercase();
        let other_arch = if is_arm64_machine_type(machine_type) {
            ["x64", "x86", "x86_64", "x86-64", "amd64", "i386", "i686"].contains(&lower.as_str())
        } else {
            ["arm64", "aarch64", "arm"].contains(&lower.as_str())
        };
        if other_arch {
            return Err(format!(
                "Label '{}' names another architecture than {}",
                label, machine_type
            ));
        }
        if builtin.iter().any(|b| b.eq_ignore_ascii_case(label))
            || labels.iter().any(|l| l.eq_ignore_ascii_case(label))
        {
            continue;
        }
        labels.push(label.to_string());
    }

    let template_id = req.cloud_init_template_id.trim();
    let (template_name, setup_script) = if template_id.is_empty() {
        (String::new(), String::new())
    } else {
        let Some(t) = state
            .cloud_init_templates
            .iter()
            .find(|t| t.id == template_id)
        else {
            return Err(format!("Template '{}' not found", template_id));
        };
        if let Some(reason) = template_unusable_reason(t) {
            return Err(format!("Template '{}' can't be used: {}", t.name, reason));
        }
        (t.name.clone(), t.content.clone())
    };

    let active: Vec<&RunnerPool> = state.runner_pools.iter().filter(|p| !p.removing).collect();
    if active.len() >= MAX_POOLS {
        return Err(format!("At most {} pools", MAX_POOLS));
    }

    let host_hourly_usd = crate::state::machine_type_hourly_usd(machine_type);
    if let Some(host_cost) = host_hourly_usd {
        let committed: f64 = active
            .iter()
            .filter_map(|p| p.host_hourly_usd.map(|c| c * p.hosts as f64))
            .sum();
        let total = committed + host_cost * req.hosts as f64;
        let cap = state.global_budget.max_hourly_usd;
        if cap.is_finite() && cap > 0.0 && total > cap + 1e-9 {
            return Err(format!(
                "All pools together would cost about ${:.4}/hr, above the ${:.2}/hr budget cap",
                total, cap
            ));
        }
    }

    Ok(RunnerPool {
        id: new_pool_id(&state.runner_pools, now),
        target: target.to_string(),
        machine_type: machine_type.to_string(),
        zone: zone.to_string(),
        hosts: req.hosts,
        runners_per_host: req.runners_per_host,
        runner_vcpu: req.runner_vcpu,
        runner_ram_gib: req.runner_ram_gib,
        labels,
        cloud_init_template_id: template_id.to_string(),
        cloud_init_template_name: template_name,
        setup_script,
        host_hourly_usd,
        created_at: now,
        created_by: created_by.to_string(),
        removing: false,
        min_hosts,
        idle_minutes,
        setup_updated_at: None,
    })
}

/// The runner patch a pool host's image gets: only the template's commands, run as root while the
/// runner image builds, so jobs see what they install.
pub fn pool_runner_patch(pool: &RunnerPool) -> Option<crate::patch::RunnerPatch> {
    if pool.setup_script.trim().is_empty() {
        return None;
    }
    let mut patch = crate::patch::RunnerPatch::default();
    if pool.target.contains('/') {
        patch.repository = pool.target.clone();
    }
    patch.container.dockerfile_run = pool.setup_script.clone();
    Some(patch)
}

/// The pool list the dashboard shows: definitions without the setup script body, with status.
pub fn pools_view(state: &DashboardState) -> serde_json::Value {
    let pools: Vec<serde_json::Value> = state
        .runner_pools
        .iter()
        .map(|p| {
            let status = state
                .runner_pool_status
                .get(&p.id)
                .cloned()
                .unwrap_or_default();
            serde_json::json!({
                "id": p.id,
                "target": p.target,
                "machine_type": p.machine_type,
                "zone": p.zone,
                "arch": if is_arm64_machine_type(&p.machine_type) { "arm64" } else { "x86_64" },
                "hosts": p.hosts,
                "runners_per_host": p.runners_per_host,
                "runner_vcpu": p.runner_vcpu,
                "runner_ram_gib": p.runner_ram_gib,
                "labels": pool_labels(p),
                "cloud_init_template_id": p.cloud_init_template_id,
                "cloud_init_template_name": p.cloud_init_template_name,
                "host_hourly_usd": p.host_hourly_usd,
                "created_at": p.created_at,
                "created_by": p.created_by,
                "removing": p.removing,
                "min_hosts": p.min_hosts(),
                "autoscales": p.autoscales(),
                "idle_minutes": p.idle_minutes(),
                "setup_updated_at": p.setup_updated_at,
                "runs_on": runs_on_labels(p),
                "status": status,
            })
        })
        .collect();
    let templates: Vec<serde_json::Value> = state
        .cloud_init_templates
        .iter()
        .filter(|t| t.role == "cicd_runner")
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "name": t.name,
                "description": t.description,
                "usable": template_unusable_reason(t).is_none(),
                "reason": template_unusable_reason(t),
                "preview": t.content.chars().take(1200).collect::<String>(),
            })
        })
        .collect();
    let machine_types: Vec<serde_json::Value> = launchable_machine_types()
        .into_iter()
        .map(|mt| {
            let (vcpu, ram) = machine_shape(mt).unwrap_or((0, 0));
            serde_json::json!({
                "machine_type": mt,
                "vcpu": vcpu,
                "ram_gib": ram,
                "usable_ram_gib": ram.saturating_sub(HOST_RESERVED_RAM_GIB),
                "arch": if is_arm64_machine_type(mt) { "arm64" } else { "x86_64" },
            })
        })
        .collect();
    serde_json::json!({
        "pools": pools,
        "templates": templates,
        "targets": suggested_targets(state),
        "machine_types": machine_types,
        "budget_max_hourly_usd": state.global_budget.max_hourly_usd,
        "limits": {
            "max_pools": MAX_POOLS,
            "max_hosts_per_pool": MAX_HOSTS_PER_POOL,
            "max_runners_per_host": MAX_RUNNERS_PER_HOST,
            "max_extra_labels": MAX_EXTRA_LABELS,
            "host_reserved_ram_gib": HOST_RESERVED_RAM_GIB,
            "default_idle_minutes": DEFAULT_IDLE_MINUTES,
            "min_idle_minutes": MIN_IDLE_MINUTES,
            "max_idle_minutes": MAX_IDLE_MINUTES,
        },
    })
}

/// KV key that holds the pool list. Pools live outside the dashboard state blob because many
/// routes save that blob from a copy that can be up to a minute old; a pool created or removed in
/// the meantime would be undone. Only the pool routes and the reconciler write this key.
pub const KV_RUNNER_POOLS_KEY: &str = "dashboard:runner-pools";

/// The stored pool list. Without the key, it adopts any pools a state blob written before pools
/// moved out of it still holds, and writes them to the key, so later state saves (which no longer
/// carry pools) can't lose them. Until the first pool exists, this costs one extra KV read.
pub async fn load_pools<K: KvStoreBackend>(kv: &K) -> Result<Vec<RunnerPool>, StateError> {
    if let Some(raw) = kv.get(KV_RUNNER_POOLS_KEY).await? {
        return Ok(serde_json::from_str(&raw)?);
    }
    let legacy = match kv.get(crate::state::KV_STATE_KEY).await? {
        Some(raw) => {
            serde_json::from_str::<crate::state::DashboardBundle>(&raw)?
                .state
                .runner_pools
        }
        None => Vec::new(),
    };
    if !legacy.is_empty() {
        save_pools(kv, &legacy).await?;
    }
    Ok(legacy)
}

/// Called before a state blob is saved: if the blob was read before pools moved out of it, its pools
/// go to the pool key first, unless the key already exists.
pub async fn adopt_legacy_pools<K: KvStoreBackend>(
    kv: &K,
    legacy: &[RunnerPool],
) -> Result<(), StateError> {
    if legacy.is_empty() || kv.get(KV_RUNNER_POOLS_KEY).await?.is_some() {
        return Ok(());
    }
    save_pools(kv, legacy).await
}

pub async fn save_pools<K: KvStoreBackend>(kv: &K, pools: &[RunnerPool]) -> Result<(), StateError> {
    let raw = serde_json::to_string(pools)?;
    kv.put(KV_RUNNER_POOLS_KEY, &raw).await
}

/// Drops the pools whose removal the reconciler finished. The list is read again right before the
/// write, so a pool created while the tick ran is kept. Returns the pools that remain.
pub async fn finish_pool_removals<K: KvStoreBackend>(
    kv: &K,
    finished: &[String],
) -> Result<Vec<RunnerPool>, StateError> {
    let mut pools = load_pools(kv).await?;
    let before = pools.len();
    pools.retain(|p| !(p.removing && finished.contains(&p.id)));
    if pools.len() != before {
        save_pools(kv, &pools).await?;
    }
    Ok(pools)
}

pub async fn handle_get_runner_pools<K: KvStoreBackend>(
    kv: &K,
    now: u64,
) -> Result<serde_json::Value, StateError> {
    // Templates and statuses: the isolate cache (at most 60 s old) is fresh enough.
    let (mut bundle, _) = load_state(kv, now).await?;
    bundle.state.runner_pools = load_pools(kv).await?;
    Ok(pools_view(&bundle.state))
}

pub async fn handle_create_runner_pool<K: KvStoreBackend>(
    kv: &K,
    req: CreateRunnerPoolRequest,
    created_by: &str,
    now: u64,
) -> Result<serde_json::Value, StateError> {
    let (mut bundle, _) = load_authoritative_state(kv, now).await?;
    bundle.state.runner_pools = load_pools(kv).await?;
    let pool = validate_create_request(&req, &bundle.state, created_by, now)
        .map_err(StateError::Validation)?;
    bundle.state.runner_pools.push(pool.clone());
    save_pools(kv, &bundle.state.runner_pools).await?;
    let mut view = pools_view(&bundle.state);
    if let Some(obj) = view.as_object_mut() {
        obj.insert("created".to_string(), serde_json::json!(pool.id));
    }
    Ok(view)
}

pub async fn handle_remove_runner_pool<K: KvStoreBackend>(
    kv: &K,
    req: RemoveRunnerPoolRequest,
    now: u64,
) -> Result<serde_json::Value, StateError> {
    let (mut bundle, _) = load_state(kv, now).await?;
    bundle.state.runner_pools = load_pools(kv).await?;
    let id = req.id.trim();
    let Some(pool) = bundle.state.runner_pools.iter_mut().find(|p| p.id == id) else {
        return Err(StateError::NotFound(format!("Pool '{}' not found", id)));
    };
    if !pool.removing {
        pool.removing = true;
        save_pools(kv, &bundle.state.runner_pools).await?;
    }
    Ok(pools_view(&bundle.state))
}

/// Why a pool can't be resized to `hosts`, if it can't: the size limits, a removal in progress,
/// and the hourly budget cap across all pools.
pub fn resize_refusal(state: &DashboardState, pool: &RunnerPool, hosts: u32) -> Option<String> {
    if hosts == 0 || hosts > MAX_HOSTS_PER_POOL {
        return Some(format!(
            "Hosts must be 1 to {}; use Remove to delete the pool",
            MAX_HOSTS_PER_POOL
        ));
    }
    if pool.removing {
        return Some(format!("Pool '{}' is being removed", pool.id));
    }
    let cap = state.global_budget.max_hourly_usd;
    if let Some(host_cost) = pool.host_hourly_usd {
        let others: f64 = state
            .runner_pools
            .iter()
            .filter(|p| !p.removing && p.id != pool.id)
            .filter_map(|p| p.host_hourly_usd.map(|c| c * p.hosts as f64))
            .sum();
        let total = others + host_cost * hosts as f64;
        if hosts > pool.hosts && cap.is_finite() && cap > 0.0 && !(total <= cap + 1e-9) {
            return Some(format!(
                "All pools together would cost about ${:.4}/hr, above the ${:.2}/hr budget cap",
                total, cap
            ));
        }
    }
    None
}

pub async fn handle_resize_runner_pool<K: KvStoreBackend>(
    kv: &K,
    req: ResizeRunnerPoolRequest,
    now: u64,
) -> Result<serde_json::Value, StateError> {
    // The budget cap comes from the stored state, not the isolate's cached copy.
    let (mut bundle, _) = load_authoritative_state(kv, now).await?;
    bundle.state.runner_pools = load_pools(kv).await?;
    let id = req.id.trim();
    let Some(idx) = bundle.state.runner_pools.iter().position(|p| p.id == id) else {
        return Err(StateError::NotFound(format!("Pool '{}' not found", id)));
    };
    let pool = bundle.state.runner_pools[idx].clone();
    if let Some(reason) = resize_refusal(&bundle.state, &pool, req.hosts) {
        return Err(StateError::Validation(reason));
    }
    if let Some(m) = req.min_hosts {
        if m > req.hosts {
            return Err(StateError::Validation(format!(
                "Minimum hosts ({}) can't be more than the most hosts ({})",
                m, req.hosts
            )));
        }
    }
    let new_min = req.min_hosts.map(|m| (m < req.hosts).then_some(m));
    if pool.hosts != req.hosts || new_min.is_some_and(|m| m != pool.min_hosts) {
        // Read the list again right before writing and change only this pool's host count, so a
        // pool created or removed meanwhile isn't undone.
        let mut pools = load_pools(kv).await?;
        let Some(fresh) = pools.iter_mut().find(|p| p.id == id) else {
            return Err(StateError::NotFound(format!("Pool '{}' not found", id)));
        };
        if fresh.removing {
            return Err(StateError::Validation(format!(
                "Pool '{}' is being removed",
                id
            )));
        }
        fresh.hosts = req.hosts;
        if let Some(m) = new_min {
            fresh.min_hosts = m;
        }
        save_pools(kv, &pools).await?;
        bundle.state.runner_pools = pools;
    }
    Ok(pools_view(&bundle.state))
}

/// Switches the template new hosts of a pool use. Idle hosts started earlier are replaced, one per
/// reconciler tick, so the change reaches the whole pool without stopping running jobs.
pub async fn handle_set_pool_template<K: KvStoreBackend>(
    kv: &K,
    req: SetPoolTemplateRequest,
    now: u64,
) -> Result<serde_json::Value, StateError> {
    let (mut bundle, _) = load_authoritative_state(kv, now).await?;
    let template_id = req.cloud_init_template_id.trim();
    let (name, script) = if template_id.is_empty() {
        (String::new(), String::new())
    } else {
        let Some(t) = bundle
            .state
            .cloud_init_templates
            .iter()
            .find(|t| t.id == template_id)
        else {
            return Err(StateError::NotFound(format!(
                "Template '{}' not found",
                template_id
            )));
        };
        if let Some(reason) = template_unusable_reason(t) {
            return Err(StateError::Validation(format!(
                "Template '{}' can't be used: {}",
                t.name, reason
            )));
        }
        (t.name.clone(), t.content.clone())
    };
    let id = req.id.trim();
    let mut pools = load_pools(kv).await?;
    let Some(pool) = pools.iter_mut().find(|p| p.id == id) else {
        return Err(StateError::NotFound(format!("Pool '{}' not found", id)));
    };
    if pool.removing {
        return Err(StateError::Validation(format!(
            "Pool '{}' is being removed",
            id
        )));
    }
    if pool.setup_script != script || pool.cloud_init_template_id != template_id {
        pool.cloud_init_template_id = template_id.to_string();
        pool.cloud_init_template_name = name;
        pool.setup_script = script;
        pool.setup_updated_at = Some(now);
        save_pools(kv, &pools).await?;
    }
    bundle.state.runner_pools = pools;
    Ok(pools_view(&bundle.state))
}

/// Sets the statuses the reconciler saves with the dashboard state: one per remaining pool, from
/// this tick when it has one, otherwise the stored one. Statuses of removed pools are dropped.
pub fn merge_pool_statuses(
    state: &mut DashboardState,
    pools: &[RunnerPool],
    statuses: &BTreeMap<String, RunnerPoolStatus>,
) {
    let mut kept: BTreeMap<String, RunnerPoolStatus> = BTreeMap::new();
    for p in pools {
        if let Some(s) = statuses
            .get(&p.id)
            .or_else(|| state.runner_pool_status.get(&p.id))
        {
            kept.insert(p.id.clone(), s.clone());
        }
    }
    state.runner_pool_status = kept;
}
