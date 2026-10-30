//! Cloud Observability (Google Cloud Ops Agent + Native `host-agent` Telemetry),
//! Dynamic GitHub Runner Density Engine, and Workflow-Correlated Log Explorer.

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use crate::patch::{CONTROLLED_TARGETS, ON_DEMAND_TARGETS};
use crate::state::{
    chrono_iso, load_state, save_state, InstanceRecord, KvStoreBackend, StateError,
};

pub const DEFAULT_WINDOW_HOURS: f64 = 6.0;
pub const MAX_INGESTED_HOSTS: usize = 64;
pub const MAX_INGESTED_LOGS_PER_HOST: usize = 50;
/// `ops_agent_status` for an instance that has not pushed host-agent telemetry.
pub const NO_TELEMETRY_STATUS: &str = "no_telemetry";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowCorrelationEntry {
    pub runner_name: String,
    #[serde(default)]
    pub container_id: String,
    pub organization: String,
    pub repository: String,
    pub workflow_name: String,
    pub job_name: String,
    pub run_id: String,
    pub run_number: String,
    pub job_id: String,
    pub github_workflow_url: String,
    #[serde(default)]
    pub commit_sha: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservabilityLogEntry {
    pub timestamp: String,
    pub epoch: u64,
    pub severity: String,
    pub source: String,
    pub instance_name: String,
    pub zone: String,
    pub role: String,
    #[serde(default)]
    pub arch: String,
    pub runner_name: String,
    pub container_id: String,
    pub organization: String,
    pub repository: String,
    pub workflow_name: String,
    pub job_name: String,
    pub run_id: String,
    #[serde(default)]
    pub run_number: String,
    pub job_id: String,
    pub github_workflow_url: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstanceObservabilityRecord {
    pub instance_name: String,
    pub zone: String,
    pub region: String,
    pub machine_type: String,
    pub arch: String,
    pub role: String,
    pub organization: String,
    pub repository: String,
    pub vcpu: u32,
    pub ram_gib: u32,
    pub cpu_utilization_pct: f64,
    pub loadavg_1m: f64,
    pub mem_used_gib: f64,
    pub mem_available_gib: f64,
    pub mem_utilization_pct: f64,
    pub disk_used_gib: f64,
    pub disk_total_gib: f64,
    pub disk_utilization_pct: f64,
    pub observed_peak_working_set_gib: f64,
    pub observed_peak_cgroup_gib: f64,
    pub observed_avg_cpu_cores_per_runner: f64,
    pub baseline_runners: u32,
    pub current_runners: u32,
    pub recommended_runners: u32,
    pub extra_runners_unlocked: u32,
    pub oom_kills: u64,
    pub ops_agent_status: String,
    pub density_reason: String,
    pub active_workflows: Vec<WorkflowCorrelationEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservabilityTimeSeriesPoint {
    pub timestamp: String,
    pub epoch: u64,
    pub cpu_utilization_pct: f64,
    pub container_cpu_pct: f64,
    pub target_cpu_pct: f64,
    pub mem_working_set_gib: f64,
    pub mem_cgroup_peak_gib: f64,
    pub mem_available_gib: f64,
    pub disk_read_mib_s: f64,
    pub disk_write_mib_s: f64,
    pub disk_utilization_pct: f64,
    pub active_runners: u32,
    pub recommended_dense_runners: u32,
    pub active_workflows: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DynamicDensitySummary {
    pub enabled: bool,
    pub target_cpu_percent: f64,
    pub target_memory_percent: f64,
    pub max_runners_per_instance: u32,
    pub baseline_total_runners: u32,
    pub current_total_runners: u32,
    pub recommended_total_runners: u32,
    pub extra_runners_unlocked: u32,
    pub avg_peak_working_set_gib: f64,
    pub avg_cgroup_peak_gib: f64,
    pub avg_cpu_utilization_pct: f64,
    pub avg_mem_utilization_pct: f64,
    pub density_multiplier: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservabilityFilterCatalog {
    pub roles: Vec<String>,
    pub organizations: Vec<String>,
    pub repositories: Vec<String>,
    pub instances: Vec<String>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservabilityResponse {
    pub status: String,
    pub generated_at: String,
    pub window_hours: f64,
    pub catalog: ObservabilityFilterCatalog,
    pub dynamic_density: DynamicDensitySummary,
    pub instances: Vec<InstanceObservabilityRecord>,
    pub timeseries: Vec<ObservabilityTimeSeriesPoint>,
    pub logs: Vec<ObservabilityLogEntry>,
    pub total_matching_logs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IngestedContainerPayload {
    #[serde(default)]
    pub container_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_runner: bool,
    #[serde(default)]
    pub memory_working_set_gib: f64,
    #[serde(default)]
    pub memory_peak_gib: f64,
    #[serde(default)]
    pub oom_kill_events: u64,
    #[serde(default)]
    pub active_workflow: Option<WorkflowCorrelationEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IngestedDensityPayload {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub current_runners: u32,
    #[serde(default)]
    pub baseline_runners: u32,
    #[serde(default)]
    pub recommended_runners: u32,
    #[serde(default)]
    pub can_scale_up_slots: u32,
    #[serde(default)]
    pub observed_peak_working_set_gib: f64,
    #[serde(default)]
    pub observed_peak_cgroup_gib: f64,
    #[serde(default)]
    pub observed_avg_cpu_cores_per_runner: f64,
    #[serde(default)]
    pub reason: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IngestedHostSnapshot {
    #[serde(default)]
    pub instance_name: String,
    #[serde(default)]
    pub zone: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub collected_epoch: u64,
    #[serde(default)]
    pub vcpu: u32,
    #[serde(default)]
    pub loadavg_1m: f64,
    #[serde(default)]
    pub cpu_utilization_pct: f64,
    #[serde(default)]
    pub mem_total_gib: f64,
    #[serde(default)]
    pub mem_used_gib: f64,
    #[serde(default)]
    pub mem_available_gib: f64,
    #[serde(default)]
    pub mem_utilization_pct: f64,
    #[serde(default)]
    pub swap_used_gib: f64,
    #[serde(default)]
    pub disk_total_gib: f64,
    #[serde(default)]
    pub disk_used_gib: f64,
    #[serde(default)]
    pub disk_utilization_pct: f64,
    #[serde(default)]
    pub containers: Vec<IngestedContainerPayload>,
    #[serde(default)]
    pub workflow_correlations: Vec<WorkflowCorrelationEntry>,
    #[serde(default)]
    pub recent_logs: Vec<ObservabilityLogEntry>,
    #[serde(default)]
    pub density: IngestedDensityPayload,
}

#[derive(Debug, Clone, Default)]
pub struct ObservabilityQuery {
    pub role: String,
    pub organizations: Vec<String>,
    pub repositories: Vec<String>,
    pub instance: String,
    pub arch: String,
    pub window_hours: f64,
    pub q: String,
    pub severity: String,
    pub source: String,
}

thread_local! {
    static INGESTED_SNAPSHOTS: RefCell<BTreeMap<String, IngestedHostSnapshot>> = const { RefCell::new(BTreeMap::new()) };
}

pub fn record_ingested_snapshot(mut snap: IngestedHostSnapshot, now: u64) {
    if snap.instance_name.trim().is_empty() {
        return;
    }
    if snap.collected_epoch == 0 {
        snap.collected_epoch = now;
    }
    if snap.recent_logs.len() > MAX_INGESTED_LOGS_PER_HOST {
        let drop_n = snap.recent_logs.len() - MAX_INGESTED_LOGS_PER_HOST;
        snap.recent_logs.drain(0..drop_n);
    }
    INGESTED_SNAPSHOTS.with(|cell| {
        let mut map = cell.borrow_mut();
        if map.len() >= MAX_INGESTED_HOSTS && !map.contains_key(&snap.instance_name) {
            if let Some(first_key) = map.keys().next().cloned() {
                map.remove(&first_key);
            }
        }
        map.insert(snap.instance_name.clone(), snap);
    });
}

pub fn get_ingested_snapshots() -> BTreeMap<String, IngestedHostSnapshot> {
    INGESTED_SNAPSHOTS.with(|cell| cell.borrow().clone())
}

pub fn clear_ingested_snapshots() {
    INGESTED_SNAPSHOTS.with(|cell| cell.borrow_mut().clear());
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

pub fn parse_observability_query(
    params: &[(String, String)],
) -> Result<ObservabilityQuery, String> {
    let mut role = "all".to_string();
    let mut organizations = Vec::new();
    let mut repositories = Vec::new();
    let mut instance = String::new();
    let mut arch = "all".to_string();
    let mut window_hours = DEFAULT_WINDOW_HOURS;
    let mut q = String::new();
    let mut severity = "all".to_string();
    let mut source = "all".to_string();

    for (k, v) in params {
        let val = v.trim();
        match k.as_str() {
            "role" => {
                if !val.is_empty() {
                    role = val.to_lowercase();
                }
            }
            "org" | "orgs" | "organization" | "organizations" => {
                for item in val.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if !organizations.contains(&item.to_string()) {
                        organizations.push(item.to_string());
                    }
                }
            }
            "repo" | "repos" | "repository" | "repositories" | "target" | "targets" => {
                for item in val.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if item.contains('/') {
                        if !repositories.contains(&item.to_string()) {
                            repositories.push(item.to_string());
                        }
                    } else if !organizations.contains(&item.to_string()) {
                        organizations.push(item.to_string());
                    }
                }
            }
            "instance" => instance = val.to_string(),
            "arch" => {
                if !val.is_empty() {
                    arch = val.to_lowercase();
                }
            }
            "window" | "window_hours" => {
                let parsed = val
                    .parse::<f64>()
                    .map_err(|_| format!("Invalid window_hours parameter: {val}"))?;
                if !parsed.is_finite() || parsed <= 0.0 {
                    return Err("window_hours must be a positive finite number".to_string());
                }
                window_hours = parsed.clamp(0.25, 744.0);
            }
            "q" | "query" | "search" => q = val.to_string(),
            "severity" => {
                if !val.is_empty() {
                    severity = val.to_uppercase();
                }
            }
            "source" => {
                if !val.is_empty() {
                    source = val.to_lowercase();
                }
            }
            _ => {}
        }
    }

    Ok(ObservabilityQuery {
        role,
        organizations,
        repositories,
        instance,
        arch,
        window_hours,
        q,
        severity,
        source,
    })
}

fn infer_instance_target(name: &str, role: &str) -> (String, String) {
    if role == "rust_builder" || name.starts_with("builder-") {
        return ("swack-tools".to_string(), "swack-tools/oxidex".to_string());
    }
    let lower = name.to_lowercase();
    if lower.contains("newznab-tmux") {
        (
            "xbmc4lyfe".to_string(),
            "xbmc4lyfe/newznab-tmux".to_string(),
        )
    } else if lower.contains("rustnzb") {
        ("xbmc4lyfe".to_string(), "xbmc4lyfe/rustnzb".to_string())
    } else if lower.contains("nntp-proxy") {
        ("xbmc4lyfe".to_string(), "xbmc4lyfe/nntp-proxy".to_string())
    } else if lower.contains("bt-to-all-devices") {
        (
            "swack-tools".to_string(),
            "swack-tools/bt-to-all-devices".to_string(),
        )
    } else {
        ("swack-tools".to_string(), "swack-tools/oxidex".to_string())
    }
}

fn infer_arch(machine_type: &str, name: &str) -> String {
    let mt = machine_type.to_lowercase();
    if mt.starts_with("c4a-")
        || mt.starts_with("n4a-")
        || mt.starts_with("t2a-")
        || name.contains("-arm-")
    {
        "arm64".to_string()
    } else {
        "x86_64".to_string()
    }
}

pub fn compute_dynamic_runner_recommendation(
    enabled: bool,
    vcpu: u32,
    ram_gib: u32,
    baseline_runners: u32,
    current_runners: u32,
    peak_ws_gib: f64,
    avg_cpu_cores: f64,
    oom_kills: u64,
    target_cpu_pct: f64,
    target_mem_pct: f64,
    max_per_instance: u32,
) -> (u32, u32, String) {
    let base = baseline_runners.max(1);
    if !enabled {
        return (
            base.max(current_runners),
            0,
            "Dynamic runner density scaling opted out (disabled)".to_string(),
        );
    }
    if oom_kills > 0 {
        return (
            base,
            0,
            "Held at baseline: container/host OOM event recorded".to_string(),
        );
    }
    let overhead = if vcpu <= 4 { 2.0 } else { 4.0 };
    let usable_ram =
        ((ram_gib as f64) - overhead).max(2.0) * (target_mem_pct.clamp(40.0, 95.0) / 100.0);
    let eff_mem_per_runner = (peak_ws_gib.max(1.2) * 1.25).max(1.5);
    let mem_slots = (usable_ram / eff_mem_per_runner).floor() as u32;

    let usable_cpu = (vcpu.max(1) as f64) * (target_cpu_pct.clamp(40.0, 95.0) / 100.0);
    let eff_cpu_per_runner = avg_cpu_cores.max(0.95);
    let cpu_slots = (usable_cpu / eff_cpu_per_runner).floor() as u32;

    let shape_cap = if vcpu <= 4 {
        3
    } else if vcpu <= 8 {
        5
    } else {
        max_per_instance.clamp(4, 12)
    };

    let recommended = mem_slots
        .min(cpu_slots)
        .clamp(base, shape_cap)
        .max(current_runners);
    let extra = recommended.saturating_sub(current_runners);
    let reason = if extra > 0 {
        format!(
            "Peak working set {:.2} GiB/runner & {:.2} cores/runner allows {} runners (+{} extra slots)",
            peak_ws_gib, avg_cpu_cores, recommended, extra
        )
    } else {
        format!(
            "Operating at optimal density ({} runners @ {:.2} GiB peak/runner)",
            recommended, peak_ws_gib
        )
    };
    (recommended, extra, reason)
}

fn build_instance_observability(
    inst: &InstanceRecord,
    ingested: Option<&IngestedHostSnapshot>,
    dyn_enabled: bool,
    target_cpu_pct: f64,
    target_mem_pct: f64,
    max_per_instance: u32,
) -> (InstanceObservabilityRecord, Vec<ObservabilityLogEntry>) {
    let arch = infer_arch(&inst.machine_type, &inst.name);
    let (default_org, default_repo) = infer_instance_target(&inst.name, &inst.role);
    let is_builder = inst.role == "rust_builder" || inst.name.starts_with("builder-");

    // Derive actual shape vCPU/RAM if the instance record stored floor defaults
    let (vcpu, ram_gib) = if inst.machine_type.contains("-4") {
        (4, 16)
    } else if inst.machine_type.contains("-8") {
        (8, 32)
    } else if inst.machine_type.starts_with("n4d-standard-16")
        || inst.machine_type.starts_with("c4-standard-16")
        || inst.machine_type.starts_with("n4-standard-16")
    {
        (16, 64)
    } else {
        (inst.vcpu.max(4), inst.ram_gib.max(16))
    };

    if let Some(snap) = ingested {
        let org = snap
            .workflow_correlations
            .first()
            .map(|w| w.organization.clone())
            .unwrap_or_else(|| default_org.clone());
        let repo = snap
            .workflow_correlations
            .first()
            .map(|w| w.repository.clone())
            .unwrap_or_else(|| default_repo.clone());
        let oom_kills: u64 = snap.containers.iter().map(|c| c.oom_kill_events).sum();
        let baseline = if is_builder {
            0
        } else if vcpu <= 4 {
            1
        } else {
            2
        };
        let current = if is_builder {
            0
        } else {
            snap.density.current_runners.max(1)
        };
        let peak_ws = snap.density.observed_peak_working_set_gib.max(1.25);
        let peak_cg = snap.density.observed_peak_cgroup_gib.max(peak_ws);
        let avg_cpu = snap.density.observed_avg_cpu_cores_per_runner.max(1.15);

        let (recommended, extra, reason) = if is_builder {
            (
                0,
                0,
                "Dedicated Rust builder (1 isolated 28 GiB container)".to_string(),
            )
        } else {
            compute_dynamic_runner_recommendation(
                dyn_enabled,
                snap.vcpu.max(vcpu),
                (snap.mem_total_gib.round() as u32).max(ram_gib),
                baseline,
                current,
                peak_ws,
                avg_cpu,
                oom_kills,
                target_cpu_pct,
                target_mem_pct,
                max_per_instance,
            )
        };

        let mut logs = snap.recent_logs.clone();
        for l in &mut logs {
            if l.arch.is_empty() {
                l.arch = arch.clone();
            }
        }

        let rec = InstanceObservabilityRecord {
            instance_name: inst.name.clone(),
            zone: inst.zone.clone(),
            region: inst.region.clone(),
            machine_type: inst.machine_type.clone(),
            arch,
            role: inst.role.clone(),
            organization: org,
            repository: repo,
            vcpu: snap.vcpu.max(vcpu),
            ram_gib: (snap.mem_total_gib.round() as u32).max(ram_gib),
            cpu_utilization_pct: snap.cpu_utilization_pct,
            loadavg_1m: snap.loadavg_1m,
            mem_used_gib: snap.mem_used_gib,
            mem_available_gib: snap.mem_available_gib,
            mem_utilization_pct: snap.mem_utilization_pct,
            disk_used_gib: snap.disk_used_gib,
            disk_total_gib: snap.disk_total_gib,
            disk_utilization_pct: snap.disk_utilization_pct,
            observed_peak_working_set_gib: peak_ws,
            observed_peak_cgroup_gib: peak_cg,
            observed_avg_cpu_cores_per_runner: avg_cpu,
            baseline_runners: baseline,
            current_runners: current,
            recommended_runners: recommended,
            extra_runners_unlocked: extra,
            oom_kills,
            ops_agent_status: "active".to_string(),
            density_reason: reason,
            active_workflows: snap.workflow_correlations.clone(),
        };
        return (rec, logs);
    }

    // No host-agent push for this instance: report what the control plane
    // actually knows and leave telemetry, workflows and logs empty rather
    // than inventing them.
    let baseline = if is_builder {
        0
    } else if vcpu <= 4 {
        1
    } else {
        2
    };
    let current = if is_builder || !inst.runners_count_known {
        0
    } else {
        inst.runners_count
    };
    let reason = if is_builder {
        "Dedicated Rust builder (1 isolated 28 GiB container)".to_string()
    } else {
        "No host-agent telemetry received yet; density recommendation unavailable".to_string()
    };

    let rec = InstanceObservabilityRecord {
        instance_name: inst.name.clone(),
        zone: inst.zone.clone(),
        region: inst.region.clone(),
        machine_type: inst.machine_type.clone(),
        arch,
        role: inst.role.clone(),
        organization: default_org,
        repository: default_repo,
        vcpu,
        ram_gib,
        cpu_utilization_pct: 0.0,
        loadavg_1m: 0.0,
        mem_used_gib: 0.0,
        mem_available_gib: 0.0,
        mem_utilization_pct: 0.0,
        disk_used_gib: 0.0,
        disk_total_gib: 0.0,
        disk_utilization_pct: 0.0,
        observed_peak_working_set_gib: 0.0,
        observed_peak_cgroup_gib: 0.0,
        observed_avg_cpu_cores_per_runner: 0.0,
        baseline_runners: baseline,
        current_runners: current,
        recommended_runners: current,
        extra_runners_unlocked: 0,
        oom_kills: 0,
        ops_agent_status: NO_TELEMETRY_STATUS.to_string(),
        density_reason: reason,
        active_workflows: Vec::new(),
    };

    (rec, Vec::new())
}

pub async fn handle_get_observability<K: KvStoreBackend>(
    kv: &K,
    query: &ObservabilityQuery,
    now: u64,
) -> Result<(u16, ObservabilityResponse), StateError> {
    let (bundle, _) = load_state(kv, now).await?;
    let ingested_map = get_ingested_snapshots();

    let policy = &bundle.state.scaling.cicd_runner;
    let dyn_enabled = policy.enable_dynamic_runner_density;
    let target_cpu_pct = policy.dynamic_target_cpu_percent.clamp(40.0, 95.0);
    let target_mem_pct = policy.dynamic_target_memory_percent.clamp(40.0, 95.0);
    let max_per_inst = policy.max_dynamic_runners_per_instance.clamp(2, 12);

    // Combine state instances with any newly ingested hosts
    let mut source_instances: Vec<InstanceRecord> = if !bundle.state.instances_view.is_empty() {
        bundle.state.instances_view.clone()
    } else {
        bundle.state.instances.clone()
    };
    for (name, snap) in &ingested_map {
        if !source_instances.iter().any(|i| &i.name == name) {
            let norm_region = snap.zone.split('-').take(2).collect::<Vec<_>>().join("-");
            source_instances.push(InstanceRecord {
                id: name.clone(),
                name: name.clone(),
                zone: snap.zone.clone(),
                region: norm_region,
                machine_type: if snap.vcpu <= 4 {
                    "c4a-standard-4".to_string()
                } else {
                    "n4d-standard-16".to_string()
                },
                status: "RUNNING".to_string(),
                role: if snap.role.is_empty() {
                    "cicd_runner".to_string()
                } else {
                    snap.role.clone()
                },
                vcpu: snap.vcpu.max(4),
                ram_gib: (snap.mem_total_gib.round() as u32).max(16),
                runners_count: snap.density.current_runners.max(1),
                runners_count_known: true,
                runner_limits_known: true,
                created_at: Some(chrono_iso(now)),
                created_epoch: Some(now),
            });
        }
    }

    // Build catalog of available orgs, repos, and instances
    let mut org_set: BTreeSet<String> = BTreeSet::new();
    let mut repo_set: BTreeSet<String> = BTreeSet::new();
    org_set.insert("swack-tools".to_string());
    org_set.insert("xbmc4lyfe".to_string());
    repo_set.insert("swack-tools/oxidex".to_string());
    for t in CONTROLLED_TARGETS.iter().chain(ON_DEMAND_TARGETS.iter()) {
        if t.contains('/') {
            repo_set.insert((*t).to_string());
            if let Some(o) = t.split('/').next() {
                org_set.insert(o.to_string());
            }
        } else {
            org_set.insert((*t).to_string());
        }
    }

    let mut all_records = Vec::new();
    let mut all_logs = Vec::new();
    let mut instance_names = Vec::new();

    for inst in &source_instances {
        if inst.status.eq_ignore_ascii_case("TERMINATED") {
            continue;
        }
        instance_names.push(inst.name.clone());
        let ing = ingested_map.get(&inst.name);
        let (rec, mut logs) = build_instance_observability(
            inst,
            ing,
            dyn_enabled,
            target_cpu_pct,
            target_mem_pct,
            max_per_inst,
        );
        org_set.insert(rec.organization.clone());
        repo_set.insert(rec.repository.clone());
        all_records.push(rec);
        all_logs.append(&mut logs);
    }

    // Apply role, org, repo, arch, and instance filters
    let matches_org_repo = |org: &str, repo: &str| -> bool {
        if query.organizations.is_empty() && query.repositories.is_empty() {
            return true;
        }
        let org_match = query
            .organizations
            .iter()
            .any(|o| o.eq_ignore_ascii_case(org));
        let repo_match = query
            .repositories
            .iter()
            .any(|r| r.eq_ignore_ascii_case(repo));
        org_match || repo_match
    };

    let filtered_instances: Vec<InstanceObservabilityRecord> = all_records
        .into_iter()
        .filter(|r| {
            if query.role != "all" && !query.role.is_empty() && r.role != query.role {
                return false;
            }
            if query.arch != "all" && !query.arch.is_empty() && r.arch != query.arch {
                return false;
            }
            if !query.instance.is_empty()
                && query.instance != "all"
                && !r.instance_name.eq_ignore_ascii_case(&query.instance)
            {
                return false;
            }
            if r.role == "cicd_runner" && !matches_org_repo(&r.organization, &r.repository) {
                return false;
            }
            if r.role == "rust_builder"
                && (!query.organizations.is_empty() || !query.repositories.is_empty())
                && query.role == "cicd_runner"
            {
                return false;
            }
            true
        })
        .collect();

    // Filter logs by role, org/repo, instance, arch, severity, source, and search query string `q`
    let q_lower = query.q.to_lowercase();
    let mut filtered_logs: Vec<ObservabilityLogEntry> = all_logs
        .into_iter()
        .filter(|l| {
            if query.role != "all" && !query.role.is_empty() && l.role != query.role {
                return false;
            }
            if query.arch != "all" && !query.arch.is_empty() && l.arch != query.arch {
                return false;
            }
            if !query.instance.is_empty()
                && query.instance != "all"
                && !l.instance_name.eq_ignore_ascii_case(&query.instance)
            {
                return false;
            }
            if l.role == "cicd_runner" && !matches_org_repo(&l.organization, &l.repository) {
                return false;
            }
            if query.severity != "ALL"
                && query.severity != "all"
                && !query.severity.is_empty()
                && !l.severity.eq_ignore_ascii_case(&query.severity)
            {
                return false;
            }
            if query.source != "all"
                && !query.source.is_empty()
                && !l.source.eq_ignore_ascii_case(&query.source)
            {
                return false;
            }
            if !q_lower.is_empty() {
                let hay = format!(
                    "{} {} {} {} {} {} {} {} {}",
                    l.instance_name,
                    l.runner_name,
                    l.container_id,
                    l.organization,
                    l.repository,
                    l.workflow_name,
                    l.job_name,
                    l.run_id,
                    l.message
                )
                .to_lowercase();
                if !hay.contains(&q_lower) {
                    return false;
                }
            }
            true
        })
        .collect();

    filtered_logs.sort_by(|a, b| b.epoch.cmp(&a.epoch));
    let total_matching_logs = filtered_logs.len();

    // Compute aggregate Dynamic Density Summary across matching runner instances
    let runner_insts: Vec<&InstanceObservabilityRecord> = filtered_instances
        .iter()
        .filter(|i| i.role == "cicd_runner")
        .collect();
    let baseline_total: u32 = runner_insts.iter().map(|i| i.baseline_runners).sum();
    let current_total: u32 = runner_insts.iter().map(|i| i.current_runners).sum();
    let recommended_total: u32 = runner_insts.iter().map(|i| i.recommended_runners).sum();
    let extra_unlocked: u32 = runner_insts.iter().map(|i| i.extra_runners_unlocked).sum();

    // Averages cover only hosts that pushed telemetry, so a silent host does
    // not drag the fleet figures toward zero.
    let reporting: Vec<&InstanceObservabilityRecord> = filtered_instances
        .iter()
        .filter(|i| i.ops_agent_status != NO_TELEMETRY_STATUS)
        .collect();
    let count_f = (reporting.len().max(1)) as f64;
    let avg_of = |f: fn(&InstanceObservabilityRecord) -> f64| -> f64 {
        reporting.iter().map(|i| f(i)).sum::<f64>() / count_f
    };
    let avg_peak_ws = round2(avg_of(|i| i.observed_peak_working_set_gib));
    let avg_peak_cg = round2(avg_of(|i| i.observed_peak_cgroup_gib));
    let avg_cpu = round2(avg_of(|i| i.cpu_utilization_pct));
    let avg_mem = round2(avg_of(|i| i.mem_utilization_pct));
    let avg_avail_gib = round2(avg_of(|i| i.mem_available_gib));
    let avg_used_gib = round2(avg_of(|i| i.mem_used_gib));
    let avg_disk_pct = round2(avg_of(|i| i.disk_utilization_pct));

    let density_multiplier = if baseline_total > 0 {
        round2(recommended_total as f64 / baseline_total as f64)
    } else {
        1.0
    };

    // The Worker keeps only each host's latest snapshot, so the series is a
    // single current sample, and empty when no host has reported.
    let mut timeseries = Vec::new();
    if !reporting.is_empty() {
        timeseries.push(ObservabilityTimeSeriesPoint {
            timestamp: chrono_iso(now),
            epoch: now,
            cpu_utilization_pct: avg_cpu,
            container_cpu_pct: avg_cpu,
            target_cpu_pct,
            mem_working_set_gib: avg_used_gib,
            mem_cgroup_peak_gib: avg_peak_cg,
            mem_available_gib: avg_avail_gib,
            disk_read_mib_s: 0.0,
            disk_write_mib_s: 0.0,
            disk_utilization_pct: avg_disk_pct,
            active_runners: current_total,
            recommended_dense_runners: recommended_total,
            active_workflows: reporting
                .iter()
                .map(|i| i.active_workflows.len() as u32)
                .sum(),
        });
    }

    let resp = ObservabilityResponse {
        status: "ok".to_string(),
        generated_at: chrono_iso(now),
        window_hours: query.window_hours,
        catalog: ObservabilityFilterCatalog {
            roles: vec![
                "all".to_string(),
                "cicd_runner".to_string(),
                "rust_builder".to_string(),
            ],
            organizations: org_set.into_iter().collect(),
            repositories: repo_set.into_iter().collect(),
            instances: instance_names,
            sources: vec![
                "all".to_string(),
                "gha_worker".to_string(),
                "ops_agent".to_string(),
                "admission".to_string(),
                "kernel".to_string(),
            ],
        },
        dynamic_density: DynamicDensitySummary {
            enabled: dyn_enabled,
            target_cpu_percent: target_cpu_pct,
            target_memory_percent: target_mem_pct,
            max_runners_per_instance: max_per_inst,
            baseline_total_runners: baseline_total,
            current_total_runners: current_total,
            recommended_total_runners: recommended_total,
            extra_runners_unlocked: extra_unlocked,
            avg_peak_working_set_gib: avg_peak_ws,
            avg_cgroup_peak_gib: avg_peak_cg,
            avg_cpu_utilization_pct: avg_cpu,
            avg_mem_utilization_pct: avg_mem,
            density_multiplier,
        },
        instances: filtered_instances,
        timeseries,
        logs: filtered_logs,
        total_matching_logs,
    };

    Ok((200, resp))
}

#[derive(Debug, Deserialize)]
pub struct UpdateDynamicDensityRequest {
    #[serde(alias = "enable_dynamic_runner_density")]
    pub enabled: Option<bool>,
    #[serde(alias = "dynamic_target_cpu_percent")]
    pub target_cpu_percent: Option<f64>,
    #[serde(alias = "dynamic_target_memory_percent")]
    pub target_memory_percent: Option<f64>,
    #[serde(alias = "max_dynamic_runners_per_instance")]
    pub max_runners_per_instance: Option<u32>,
}

pub async fn handle_post_dynamic_density<K: KvStoreBackend>(
    kv: &K,
    req: UpdateDynamicDensityRequest,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    if let Some(cpu) = req.target_cpu_percent {
        if !cpu.is_finite() || !(30.0..=95.0).contains(&cpu) {
            return Err(StateError::Validation(
                "target_cpu_percent must be between 30 and 95".to_string(),
            ));
        }
    }
    if let Some(mem) = req.target_memory_percent {
        if !mem.is_finite() || !(30.0..=95.0).contains(&mem) {
            return Err(StateError::Validation(
                "target_memory_percent must be between 30 and 95".to_string(),
            ));
        }
    }
    if let Some(max_r) = req.max_runners_per_instance {
        if !(1..=16).contains(&max_r) {
            return Err(StateError::Validation(
                "max_runners_per_instance must be between 1 and 16".to_string(),
            ));
        }
    }

    let (mut bundle, _) = load_state(kv, now).await?;
    let mut changed = false;

    if let Some(en) = req.enabled {
        if bundle
            .state
            .scaling
            .cicd_runner
            .enable_dynamic_runner_density
            != en
        {
            bundle
                .state
                .scaling
                .cicd_runner
                .enable_dynamic_runner_density = en;
            changed = true;
        }
    }
    if let Some(cpu) = req.target_cpu_percent {
        let c = round2(cpu);
        if (bundle.state.scaling.cicd_runner.dynamic_target_cpu_percent - c).abs() > 1e-6 {
            bundle.state.scaling.cicd_runner.dynamic_target_cpu_percent = c;
            changed = true;
        }
    }
    if let Some(mem) = req.target_memory_percent {
        let m = round2(mem);
        if (bundle
            .state
            .scaling
            .cicd_runner
            .dynamic_target_memory_percent
            - m)
            .abs()
            > 1e-6
        {
            bundle
                .state
                .scaling
                .cicd_runner
                .dynamic_target_memory_percent = m;
            changed = true;
        }
    }
    if let Some(max_r) = req.max_runners_per_instance {
        if bundle
            .state
            .scaling
            .cicd_runner
            .max_dynamic_runners_per_instance
            != max_r
        {
            bundle
                .state
                .scaling
                .cicd_runner
                .max_dynamic_runners_per_instance = max_r;
            changed = true;
        }
    }

    if changed {
        bundle.state.updated_at = now;
        save_state(kv, &mut bundle, now).await?;
    }

    Ok((
        200,
        serde_json::json!({
            "status": "ok",
            "enable_dynamic_runner_density": bundle.state.scaling.cicd_runner.enable_dynamic_runner_density,
            "dynamic_target_cpu_percent": bundle.state.scaling.cicd_runner.dynamic_target_cpu_percent,
            "dynamic_target_memory_percent": bundle.state.scaling.cicd_runner.dynamic_target_memory_percent,
            "max_dynamic_runners_per_instance": bundle.state.scaling.cicd_runner.max_dynamic_runners_per_instance,
        }),
    ))
}
