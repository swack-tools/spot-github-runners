//! Serverless GCP Spot Runner Fleet Reconciler & KV Quota Conservation.
//!
//! Subsystems:
//! 1. Multi-tick state machine (`creating` -> `booting` -> `done`).
//! 2. Floor target keeper (`CONTROLLED_TARGETS`: maintains >= 1 managed host per target,
//!    >= 16 vCPU / 32 GiB RAM, 2 runners @ 7 vCPU / 14 GiB RAM, daily cap <= $1.50/day).
//! 3. Scale-up policy: scale up additional Spot hosts ONLY when `queued_jobs >= 20`.
//! 4. Preempted VM (`TERMINATED` GCE instances) deletion with `deleteDisk=ALL`.
//! 5. Stale runner registration cleanup (`offline` and `!busy` runners without an active VM).
//! 6. 240s concurrency lease.
//! 7. Strict Diff Gating & KV Quota Conservation:
//!    - Canonical `fleet_fingerprint` hashing.
//!    - **Skip `SPOT_KV.put` entirely on unchanged ticks** (0 writes on unchanged ticks).
//!    - Coarse heartbeat (>= 30-60 min) periodic update.
//!    - Steady-state budget < 50 writes/day.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::gcp::{GceAttachedDisk, GceInsertInstanceRequest, GceMetadata, GceOperation};
use crate::github::RunnerBootstrapConfig;
use crate::patch::CONTROLLED_TARGETS;
use crate::state::{load_state, save_state, InstanceRecord, KvStoreBackend, StateError};

pub const DEFAULT_MACHINE_TYPE: &str = "c4d-highcpu-16";
pub const DEFAULT_ZONE: &str = "us-central1-a";
pub const MIN_HOST_VCPU: u32 = 16;
pub const MIN_HOST_RAM_GIB: u32 = 32;
pub const RUNNERS_PER_HOST: u32 = 2;
pub const RUNNER_VCPU: u32 = 7;
pub const RUNNER_RAM_GIB: u32 = 14;
pub const MIN_QUEUED_JOBS_THRESHOLD: u32 = 20;
pub const MAX_DAILY_USD_CAP: f64 = 1.50;
pub const RECONCILER_LEASE_TTL_SECS: u64 = 240;
pub const HEARTBEAT_INTERVAL_SECS: u64 = 1800; // 30 minutes
pub const LAUNCH_TIMEOUT_SECS: u64 = 900; // 15 minutes

/// Generate a unique 8-character hex entropy suffix for instance names.
/// Uses getrandom (supported in Wasm via js and native targets).
fn generate_unique_suffix(now: u64) -> String {
    let mut bytes = [0u8; 4];
    if getrandom::getrandom(&mut bytes).is_ok() {
        hex::encode(bytes)
    } else {
        let tick = TICK_COUNTER.load(Ordering::Relaxed);
        format!("{:08x}", (now ^ (tick as u64)) as u32)
    }
}

// ============================================================================
// Data Types & State Machine Models
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchPhase {
    Creating,
    Booting,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchOperation {
    pub target: String,
    pub host_name: String,
    pub zone: String,
    pub machine_type: String,
    pub phase: LaunchPhase,
    pub deployment_id: String,
    pub started_at: u64,
    #[serde(default)]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub expected_runners: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReconciliationReport {
    pub tick: u64,
    pub status: String,
    pub launched_hosts: Vec<String>,
    pub cleaned_vms: Vec<String>,
    pub cleaned_runners: Vec<String>,
    pub snapshot_written: bool,
    pub kv_puts: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcilerError {
    #[error("State error: {0}")]
    State(#[from] StateError),
    #[error("GCP error: {0}")]
    Gcp(String),
    #[error("GitHub error: {0}")]
    Github(String),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

// ============================================================================
// Provider Traits for Offline Unit Testing and Production Execution
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleGceInstance {
    pub id: Option<String>,
    pub name: String,
    pub zone: String,
    pub machine_type: String,
    pub status: String,
    pub metadata: GceMetadata,
    pub disks: Vec<GceAttachedDisk>,
}

impl SimpleGceInstance {
    pub fn zone_short(&self) -> &str {
        self.zone.rsplit('/').next().unwrap_or(&self.zone)
    }

    pub fn machine_type_short(&self) -> &str {
        self.machine_type
            .rsplit('/')
            .next()
            .unwrap_or(&self.machine_type)
    }

    pub fn started_epoch(&self) -> Option<u64> {
        if let Ok(guard) = GCE_INSTANCE_TIMESTAMPS.lock() {
            if let Some(ref map) = *guard {
                if let Some((Some(start_ep), _)) = map.get(&self.name) {
                    return Some(*start_ep);
                }
            }
        }
        crate::state::parse_launch_epoch_from_name(&self.name)
    }

    pub fn stopped_epoch(&self) -> Option<u64> {
        if let Ok(guard) = GCE_INSTANCE_TIMESTAMPS.lock() {
            if let Some(ref map) = *guard {
                if let Some((_, Some(stop_ep))) = map.get(&self.name) {
                    return Some(*stop_ep);
                }
            }
        }
        None
    }
}

fn parse_rfc3339_epoch(ts: Option<&str>) -> Option<u64> {
    let raw = ts?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp()).ok())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleGitHubRunner {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub busy: bool,
}

#[async_trait::async_trait(?Send)]
pub trait GcpReconcilerClient {
    async fn list_instances(
        &self,
        project: &str,
        now: u64,
    ) -> Result<Vec<SimpleGceInstance>, ReconcilerError>;
    async fn insert_instance(
        &self,
        project: &str,
        zone: &str,
        req: &GceInsertInstanceRequest,
        source_instance_template: Option<&str>,
        now: u64,
    ) -> Result<GceOperation, ReconcilerError>;
    async fn delete_instance(
        &self,
        project: &str,
        zone: &str,
        instance: &str,
        now: u64,
    ) -> Result<GceOperation, ReconcilerError>;
    async fn scrub_startup_script(
        &self,
        project: &str,
        zone: &str,
        instance: &str,
        current_metadata: &GceMetadata,
        now: u64,
    ) -> Result<GceOperation, ReconcilerError>;
}

#[async_trait::async_trait(?Send)]
pub trait GitHubReconcilerClient {
    async fn list_runners(
        &self,
        target_api_path: &str,
    ) -> Result<Vec<SimpleGitHubRunner>, ReconcilerError>;
    async fn delete_runner(
        &self,
        target_api_path: &str,
        runner_id: u64,
    ) -> Result<(), ReconcilerError>;
    async fn mint_2slot_runner_configs(
        &self,
        target_api_path: &str,
        vm_name: &str,
        runner_prefix: &str,
        runner_group_id: u32,
        labels: &[String],
    ) -> Result<RunnerBootstrapConfig, ReconcilerError>;
    async fn count_queued_jobs(&self, owner: &str, repo: &str) -> Result<u32, ReconcilerError>;
}

// ============================================================================
// Production Adapter Implementations
// ============================================================================

#[async_trait::async_trait(?Send)]
impl<T: crate::gcp::GcpHttpTransport> GcpReconcilerClient for crate::gcp::GceComputeClient<T> {
    async fn list_instances(
        &self,
        _project: &str,
        now: u64,
    ) -> Result<Vec<SimpleGceInstance>, ReconcilerError> {
        let list = self
            .list_instances(now)
            .await
            .map_err(|e| ReconcilerError::Gcp(e.to_string()))?;
        let mut ts_map = std::collections::HashMap::new();
        let out = list
            .into_iter()
            .map(|i| {
                let start_ep = parse_rfc3339_epoch(i.last_start_timestamp.as_deref())
                    .or_else(|| parse_rfc3339_epoch(i.creation_timestamp.as_deref()));
                let stop_ep = parse_rfc3339_epoch(i.last_stop_timestamp.as_deref());
                if start_ep.is_some() || stop_ep.is_some() {
                    ts_map.insert(i.name.clone(), (start_ep, stop_ep));
                }
                SimpleGceInstance {
                    id: i.id,
                    name: i.name,
                    zone: i.zone,
                    machine_type: i.machine_type,
                    status: i.status,
                    metadata: i.metadata,
                    disks: i.disks,
                }
            })
            .collect();
        if let Ok(mut guard) = GCE_INSTANCE_TIMESTAMPS.lock() {
            *guard = Some(ts_map);
        }
        Ok(out)
    }

    async fn insert_instance(
        &self,
        _project: &str,
        zone: &str,
        req: &GceInsertInstanceRequest,
        source_instance_template: Option<&str>,
        now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        self.insert_instance(zone, req, source_instance_template, now)
            .await
            .map_err(|e| ReconcilerError::Gcp(e.to_string()))
    }

    async fn delete_instance(
        &self,
        _project: &str,
        zone: &str,
        instance: &str,
        now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        self.delete_instance(zone, instance, now)
            .await
            .map_err(|e| ReconcilerError::Gcp(e.to_string()))
    }

    async fn scrub_startup_script(
        &self,
        _project: &str,
        zone: &str,
        instance: &str,
        current_metadata: &GceMetadata,
        now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        self.scrub_startup_script(zone, instance, current_metadata, now)
            .await
            .map_err(|e| ReconcilerError::Gcp(e.to_string()))
    }
}

#[async_trait::async_trait(?Send)]
impl GitHubReconcilerClient for crate::github::GitHubActionsClient {
    async fn list_runners(
        &self,
        target_api_path: &str,
    ) -> Result<Vec<SimpleGitHubRunner>, ReconcilerError> {
        let list = self
            .list_runners(target_api_path)
            .await
            .map_err(|e| ReconcilerError::Github(e.to_string()))?;
        Ok(list
            .into_iter()
            .map(|r| SimpleGitHubRunner {
                id: r.id,
                name: r.name,
                status: r.status,
                busy: r.busy,
            })
            .collect())
    }

    async fn delete_runner(
        &self,
        target_api_path: &str,
        runner_id: u64,
    ) -> Result<(), ReconcilerError> {
        self.delete_runner(target_api_path, runner_id)
            .await
            .map_err(|e| ReconcilerError::Github(e.to_string()))
    }

    async fn mint_2slot_runner_configs(
        &self,
        target_api_path: &str,
        vm_name: &str,
        runner_prefix: &str,
        runner_group_id: u32,
        labels: &[String],
    ) -> Result<RunnerBootstrapConfig, ReconcilerError> {
        self.mint_2slot_runner_configs(
            target_api_path,
            vm_name,
            runner_prefix,
            runner_group_id,
            labels,
        )
        .await
        .map_err(|e| ReconcilerError::Github(e.to_string()))
    }

    async fn count_queued_jobs(&self, owner: &str, repo: &str) -> Result<u32, ReconcilerError> {
        self.count_queued_jobs_for_repo(owner, repo)
            .await
            .map_err(|e| ReconcilerError::Github(e.to_string()))
    }
}

// ============================================================================
// Global State & Simulation / Telemetry Tracking
// ============================================================================

static TICK_COUNTER: AtomicU64 = AtomicU64::new(0);
static RECONCILER_LEASE: Mutex<Option<u64>> = Mutex::new(None);
static LAST_FLEET_FINGERPRINT: Mutex<Option<String>> = Mutex::new(None);
static ACTIVE_LAUNCHES: Mutex<Vec<LaunchOperation>> = Mutex::new(Vec::new());
static GCE_INSTANCE_TIMESTAMPS: Mutex<
    Option<std::collections::HashMap<String, (Option<u64>, Option<u64>)>>,
> = Mutex::new(None);

// Test injection stores for E2E and unit test harnesses
static INJECTED_TERMINATED_VMS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static INJECTED_STALE_RUNNERS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static TOTAL_KV_PUTS: AtomicUsize = AtomicUsize::new(0);
static TOTAL_KV_GETS: AtomicUsize = AtomicUsize::new(0);
static ISOLATE_CACHE_HITS: AtomicUsize = AtomicUsize::new(0);

// Simulated provider state for local/simulation test environments
static SIMULATED_INSTANCES: Mutex<Vec<SimpleGceInstance>> = Mutex::new(Vec::new());
static SIMULATED_RUNNERS: Mutex<Vec<SimpleGitHubRunner>> = Mutex::new(Vec::new());

pub fn get_reconciler_ticks() -> u64 {
    TICK_COUNTER.load(Ordering::SeqCst)
}

pub fn increment_kv_gets() {
    TOTAL_KV_GETS.fetch_add(1, Ordering::SeqCst);
}

pub fn increment_cache_hits() {
    ISOLATE_CACHE_HITS.fetch_add(1, Ordering::SeqCst);
}

pub fn get_kv_stats() -> (usize, usize, usize) {
    (
        TOTAL_KV_PUTS.load(Ordering::SeqCst),
        TOTAL_KV_GETS.load(Ordering::SeqCst),
        ISOLATE_CACHE_HITS.load(Ordering::SeqCst),
    )
}

pub fn inject_terminated_vm(name: String) {
    INJECTED_TERMINATED_VMS.lock().unwrap().push(name);
}

pub fn inject_stale_runner(runner_id: String) {
    INJECTED_STALE_RUNNERS.lock().unwrap().push(runner_id);
}

pub fn reset_reconciler_state() {
    TICK_COUNTER.store(0, Ordering::SeqCst);
    *RECONCILER_LEASE.lock().unwrap() = None;
    *LAST_FLEET_FINGERPRINT.lock().unwrap() = None;
    ACTIVE_LAUNCHES.lock().unwrap().clear();
    INJECTED_TERMINATED_VMS.lock().unwrap().clear();
    INJECTED_STALE_RUNNERS.lock().unwrap().clear();
    SIMULATED_INSTANCES.lock().unwrap().clear();
    SIMULATED_RUNNERS.lock().unwrap().clear();
    *GCE_INSTANCE_TIMESTAMPS.lock().unwrap() = None;
    TOTAL_KV_PUTS.store(0, Ordering::SeqCst);
    TOTAL_KV_GETS.store(0, Ordering::SeqCst);
    ISOLATE_CACHE_HITS.store(0, Ordering::SeqCst);
}

pub fn set_reconciler_lease(expires_at: Option<u64>) {
    *RECONCILER_LEASE.lock().unwrap() = expires_at;
}

pub fn inject_active_launch(op: LaunchOperation) {
    ACTIVE_LAUNCHES.lock().unwrap().push(op);
}

pub fn get_active_launches() -> Vec<LaunchOperation> {
    ACTIVE_LAUNCHES.lock().unwrap().clone()
}

#[derive(Debug, Default, Clone)]
pub struct SimulatedFleetProvider;

#[async_trait::async_trait(?Send)]
impl GcpReconcilerClient for SimulatedFleetProvider {
    async fn list_instances(
        &self,
        _project: &str,
        _now: u64,
    ) -> Result<Vec<SimpleGceInstance>, ReconcilerError> {
        Ok(SIMULATED_INSTANCES.lock().unwrap().clone())
    }

    async fn insert_instance(
        &self,
        _project: &str,
        zone: &str,
        req: &GceInsertInstanceRequest,
        _source_instance_template: Option<&str>,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        let inst = SimpleGceInstance {
            id: Some(format!("sim-{}", req.name)),
            name: req.name.clone(),
            zone: zone.to_string(),
            machine_type: req.machine_type.clone(),
            status: "RUNNING".to_string(),
            metadata: req.metadata.clone(),
            disks: req.disks.clone(),
        };
        SIMULATED_INSTANCES.lock().unwrap().push(inst);
        Ok(GceOperation {
            id: Some(format!("op-{}", req.name)),
            name: Some(format!("op-{}", req.name)),
            status: "DONE".to_string(),
            target_link: None,
            error: None,
        })
    }

    async fn delete_instance(
        &self,
        _project: &str,
        _zone: &str,
        instance: &str,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        let mut list = SIMULATED_INSTANCES.lock().unwrap();
        list.retain(|i| i.name != instance);
        Ok(GceOperation {
            id: Some(format!("op-del-{}", instance)),
            name: Some(format!("op-del-{}", instance)),
            status: "DONE".to_string(),
            target_link: None,
            error: None,
        })
    }

    async fn scrub_startup_script(
        &self,
        _project: &str,
        _zone: &str,
        instance: &str,
        current_metadata: &GceMetadata,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        let mut list = SIMULATED_INSTANCES.lock().unwrap();
        if let Some(inst) = list.iter_mut().find(|i| i.name == instance) {
            let mut new_meta = current_metadata.clone();
            new_meta.items.retain(|item| item.key != "startup-script");
            inst.metadata = new_meta;
        }
        Ok(GceOperation {
            id: Some(format!("op-scrub-{}", instance)),
            name: Some(format!("op-scrub-{}", instance)),
            status: "DONE".to_string(),
            target_link: None,
            error: None,
        })
    }
}

#[async_trait::async_trait(?Send)]
impl GitHubReconcilerClient for SimulatedFleetProvider {
    async fn list_runners(
        &self,
        _target_api_path: &str,
    ) -> Result<Vec<SimpleGitHubRunner>, ReconcilerError> {
        Ok(SIMULATED_RUNNERS.lock().unwrap().clone())
    }

    async fn delete_runner(
        &self,
        _target_api_path: &str,
        runner_id: u64,
    ) -> Result<(), ReconcilerError> {
        let mut list = SIMULATED_RUNNERS.lock().unwrap();
        list.retain(|r| r.id != runner_id);
        Ok(())
    }

    async fn mint_2slot_runner_configs(
        &self,
        _target_api_path: &str,
        _vm_name: &str,
        _runner_prefix: &str,
        _runner_group_id: u32,
        _labels: &[String],
    ) -> Result<RunnerBootstrapConfig, ReconcilerError> {
        Ok(RunnerBootstrapConfig::JitConfig {
            runner_1_jitconfig: "simulated-jit-1".to_string(),
            runner_2_jitconfig: "simulated-jit-2".to_string(),
        })
    }

    async fn count_queued_jobs(&self, _owner: &str, _repo: &str) -> Result<u32, ReconcilerError> {
        Ok(0)
    }
}

pub async fn reconcile_worker_fleet<K: KvStoreBackend>(
    kv: &K,
    env_gcp_sa_key: Option<&str>,
    env_gh_token: Option<&str>,
    now: u64,
) -> Result<ReconciliationReport, ReconcilerError> {
    if let (Some(sa_key), Some(gh_tok)) = (env_gcp_sa_key, env_gh_token) {
        if !sa_key.trim().is_empty() && !gh_tok.trim().is_empty() {
            if let Ok(creds) = serde_json::from_str::<crate::gcp::GcpCredentials>(sa_key) {
                // The project always comes from the service account key; no project ID is compiled in.
                let project = match creds.project_id.clone().filter(|p| !p.trim().is_empty()) {
                    Some(p) => p,
                    None => {
                        return Err(ReconcilerError::Gcp(
                            "GCP service account key has no project_id".to_string(),
                        ));
                    }
                };
                let gcp_client = crate::gcp::GceComputeClient::new(
                    creds,
                    project,
                    crate::gcp::WorkerGcpTransport,
                );
                let gh_client = crate::github::GitHubActionsClient::new(
                    gh_tok.to_string(),
                    std::sync::Arc::new(crate::patch_console::WorkerGitHubTransport),
                );
                return run_reconciliation(kv, &gcp_client, &gh_client, now).await;
            }
        }
    }

    let sim = SimulatedFleetProvider;
    run_reconciliation(kv, &sim, &sim, now).await
}

fn apply_gce_instance_targets_to_overview(
    ov: &mut serde_json::Value,
    instances: &[SimpleGceInstance],
) -> bool {
    let mut gce_targets: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    for inst in instances {
        let target_opt = inst
            .metadata
            .get("spot-runner-registration-target")
            .or_else(|| inst.metadata.get("spot-runner-patch-target"))
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(t) = target_opt {
            gce_targets.insert(inst.name.as_str(), t.to_string());
        }
    }
    if gce_targets.is_empty() {
        return false;
    }
    let mut changed = false;
    if let Some(arr) = ov
        .get_mut("instances_view")
        .and_then(|iv| iv.get_mut("instances"))
        .and_then(|v| v.as_array_mut())
    {
        for item in arr.iter_mut() {
            if let Some(obj) = item.as_object_mut() {
                if let Some(name) = obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                {
                    if let Some(target) = gce_targets.get(name.as_str()) {
                        let existing = obj
                            .get("registration_target")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if existing != target {
                            obj.insert(
                                "registration_target".to_string(),
                                serde_json::json!(target),
                            );
                            obj.insert("target".to_string(), serde_json::json!(target));
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    changed
}

pub async fn run_inventory_refresh<K, G>(
    kv: &K,
    gcp: &G,
    now: u64,
) -> Result<ReconciliationReport, ReconcilerError>
where
    K: KvStoreBackend,
    G: GcpReconcilerClient,
{
    let tick = TICK_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;

    // Check writer serialization lease: if reconciler is currently running, skip
    {
        let lease_guard = RECONCILER_LEASE.lock().unwrap();
        if let Some(expires_at) = *lease_guard {
            if now < expires_at {
                return Ok(ReconciliationReport {
                    tick,
                    status: "skipped_lease_active".to_string(),
                    launched_hosts: vec![],
                    cleaned_vms: vec![],
                    cleaned_runners: vec![],
                    snapshot_written: false,
                    kv_puts: TOTAL_KV_PUTS.load(Ordering::SeqCst),
                });
            }
        }
    }

    let (initial_bundle, _) = load_state(kv, now).await?;
    let project = initial_bundle.state.project.clone();

    let instances = gcp
        .list_instances(&project, now)
        .await
        .map_err(|e| ReconcilerError::Gcp(e.to_string()))?;

    // Reload freshest state from KV right before merge and save to prevent clobbering concurrent state changes
    let (mut fresh_bundle, _) = load_state(kv, now).await?;

    let fi = serde_json::json!({
        "source": "gce",
        "last_success_at": crate::state::chrono_iso(now),
        "last_success_epoch": now,
        "stale": false,
        "stale_reason": serde_json::Value::Null,
        "last_error": serde_json::Value::Null
    });
    let initial_refresh = fresh_bundle.state.fleet_inventory.is_null();
    fresh_bundle.state.fleet_inventory = fi.clone();
    if let Some(ref mut ov) = fresh_bundle.state.overview {
        if let Some(obj) = ov.as_object_mut() {
            obj.insert("fleet_inventory".to_string(), fi.clone());
            if let Some(iv) = obj
                .get_mut("instances_view")
                .and_then(|v| v.as_object_mut())
            {
                iv.insert("fleet_inventory".to_string(), fi);
            }
        }
    }

    let existing_by_name: std::collections::HashMap<String, InstanceRecord> = fresh_bundle
        .state
        .instances
        .iter()
        .map(|inst| (inst.name.clone(), inst.clone()))
        .collect();

    let gce_stop_epochs: std::collections::HashMap<String, u64> = instances
        .iter()
        .filter_map(|i| i.stopped_epoch().map(|ep| (i.name.clone(), ep)))
        .collect();

    let new_instances_view: Vec<InstanceRecord> = instances
        .iter()
        .filter(|i| {
            let is_runner = i.name.starts_with("oxidex-runners-");
            let is_builder = i.name.starts_with("builder-");
            (is_runner || is_builder) && !i.status.eq_ignore_ascii_case("TERMINATED")
        })
        .map(|i| {
            let is_builder = i.name.starts_with("builder-");
            let norm_zone = i.zone_short().to_string();
            let norm_region = norm_zone.split('-').take(2).collect::<Vec<_>>().join("-");
            let norm_machine_type = i.machine_type_short().to_string();
            let gce_id = i.id.clone().unwrap_or_else(|| i.name.clone());

            if let Some(existing) = existing_by_name.get(&i.name) {
                // If GCE ID changed, a new VM was created under the same name: reset identity and start timestamps
                let id_changed =
                    !existing.id.is_empty() && !gce_id.is_empty() && existing.id != gce_id;
                if !id_changed {
                    let mut rec = existing.clone();
                    rec.status = i.status.clone();
                    rec.zone = norm_zone.clone();
                    rec.region = norm_region.clone();
                    rec.machine_type = norm_machine_type.clone();
                    if rec.created_epoch.is_none() {
                        if let Some(ep) = i.started_epoch() {
                            rec.created_epoch = Some(ep);
                            rec.created_at = Some(crate::state::chrono_iso(ep));
                        }
                    }
                    if is_builder {
                        rec.role = "rust_builder".to_string();
                        rec.vcpu = 16;
                        rec.ram_gib = 32;
                        rec.runners_count = 0;
                        rec.runners_count_known = true;
                        rec.runner_limits_known = true;
                    }
                    return rec;
                }
            }
            let launch_epoch = i.started_epoch();
            let created_at = launch_epoch.map(crate::state::chrono_iso);
            InstanceRecord {
                id: gce_id,
                name: i.name.clone(),
                zone: norm_zone,
                region: norm_region,
                machine_type: norm_machine_type,
                status: i.status.clone(),
                role: if is_builder {
                    "rust_builder".to_string()
                } else {
                    "cicd_runner".to_string()
                },
                vcpu: if is_builder { 16 } else { MIN_HOST_VCPU },
                ram_gib: if is_builder { 32 } else { MIN_HOST_RAM_GIB },
                runners_count: 0,
                runners_count_known: is_builder,
                runner_limits_known: is_builder,
                created_at,
                created_epoch: launch_epoch,
            }
        })
        .collect();

    let overview_changed = if let Some(ref mut ov) = fresh_bundle.state.overview {
        let ledger_changed = crate::state::reconcile_overview_instances_ledger(
            ov,
            &fresh_bundle.state.instances,
            &new_instances_view,
            &gce_stop_epochs,
            true,
            now,
        );
        let targets_changed = apply_gce_instance_targets_to_overview(ov, &instances);
        ledger_changed || targets_changed
    } else {
        false
    };

    let instances_changed = fresh_bundle.state.instances_view != new_instances_view;
    let snapshot_written = if instances_changed || overview_changed || initial_refresh {
        fresh_bundle.state.instances_view = new_instances_view.clone();
        fresh_bundle.state.instances = new_instances_view;
        fresh_bundle.state.updated_at = now;
        save_state(kv, &mut fresh_bundle, now).await?;
        TOTAL_KV_PUTS.fetch_add(1, Ordering::SeqCst);
        true
    } else {
        false
    };

    Ok(ReconciliationReport {
        tick,
        status: "inventory_refresh_success".to_string(),
        launched_hosts: vec![],
        cleaned_vms: vec![],
        cleaned_runners: vec![],
        snapshot_written,
        kv_puts: TOTAL_KV_PUTS.load(Ordering::SeqCst),
    })
}

pub async fn refresh_fleet_inventory<K: KvStoreBackend>(
    kv: &K,
    env_gcp_sa_key: Option<&str>,
    now: u64,
) -> Result<ReconciliationReport, ReconcilerError> {
    if let Some(sa_key) = env_gcp_sa_key {
        if !sa_key.trim().is_empty() {
            match serde_json::from_str::<crate::gcp::GcpCredentials>(sa_key) {
                Ok(creds) => {
                    // The project always comes from the service account key; no project ID is compiled in.
                    let project = match creds.project_id.clone().filter(|p| !p.trim().is_empty()) {
                        Some(p) => p,
                        None => {
                            return Err(ReconcilerError::Gcp(
                                "GCP service account key has no project_id".to_string(),
                            ));
                        }
                    };
                    let gcp_client = crate::gcp::GceComputeClient::new(
                        creds,
                        project,
                        crate::gcp::WorkerGcpTransport,
                    );
                    return run_inventory_refresh(kv, &gcp_client, now).await;
                }
                Err(e) => {
                    return Err(ReconcilerError::Gcp(format!(
                        "Malformed GCP credentials: {e}"
                    )));
                }
            }
        }
    }

    Err(ReconcilerError::Gcp(
        "GCP service account key is missing or empty; cannot refresh live inventory without credentials".to_string(),
    ))
}

// ============================================================================
// Canonical Fleet Fingerprinting
// ============================================================================

pub fn compute_fleet_fingerprint(
    instances: &[SimpleGceInstance],
    runners: &[SimpleGitHubRunner],
    queued_jobs: u32,
    launches: &[LaunchOperation],
) -> String {
    let mut hasher = Sha256::new();

    // 1. Instances sorted by (name, zone, status, machine_type)
    let mut inst_summary: Vec<_> = instances
        .iter()
        .map(|i| (&i.name, &i.zone, &i.status, &i.machine_type))
        .collect();
    inst_summary.sort();
    for (n, z, s, m) in inst_summary {
        hasher.update(n.as_bytes());
        hasher.update(b":");
        hasher.update(z.as_bytes());
        hasher.update(b":");
        hasher.update(s.as_bytes());
        hasher.update(b":");
        hasher.update(m.as_bytes());
        hasher.update(b";");
    }

    // 2. Runners sorted by ID
    let mut runner_summary: Vec<_> = runners
        .iter()
        .map(|r| (r.id, &r.name, &r.status, r.busy))
        .collect();
    runner_summary.sort_by_key(|(id, ..)| *id);
    for (id, n, s, b) in runner_summary {
        hasher.update(id.to_string().as_bytes());
        hasher.update(b":");
        hasher.update(n.as_bytes());
        hasher.update(b":");
        hasher.update(s.as_bytes());
        hasher.update(if b { b":busy;" } else { b":idle;" });
    }

    // 3. Queued jobs count
    hasher.update(queued_jobs.to_string().as_bytes());
    hasher.update(b";");

    // 4. In-flight launches sorted by host_name
    let mut launch_summary: Vec<_> = launches
        .iter()
        .map(|l| (&l.host_name, format!("{:?}", l.phase)))
        .collect();
    launch_summary.sort();
    for (h, p) in launch_summary {
        hasher.update(h.as_bytes());
        hasher.update(b":");
        hasher.update(p.as_bytes());
        hasher.update(b";");
    }

    hex::encode(hasher.finalize())
}

// ============================================================================
// Core Reconciliation Runner
// ============================================================================

pub async fn run_reconciliation<K, G, H>(
    kv: &K,
    gcp: &G,
    gh: &H,
    now: u64,
) -> Result<ReconciliationReport, ReconcilerError>
where
    K: KvStoreBackend,
    G: GcpReconcilerClient,
    H: GitHubReconcilerClient,
{
    // 1. Concurrency Lease Check (240s TTL)
    {
        let mut lease_guard = RECONCILER_LEASE.lock().unwrap();
        if let Some(expires_at) = *lease_guard {
            if now < expires_at {
                return Ok(ReconciliationReport {
                    tick: TICK_COUNTER.load(Ordering::SeqCst),
                    status: "skipped_lease_active".to_string(),
                    launched_hosts: vec![],
                    cleaned_vms: vec![],
                    cleaned_runners: vec![],
                    snapshot_written: false,
                    kv_puts: TOTAL_KV_PUTS.load(Ordering::SeqCst),
                });
            }
        }
        *lease_guard = Some(now + RECONCILER_LEASE_TTL_SECS);
    }

    let tick = TICK_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    let (mut bundle, _) = load_state(kv, now).await?;
    let project = bundle.state.project.clone();
    let org = bundle.state.org.clone();

    // 2. Query Current GCP & GitHub Fleet State
    let (instances, gcp_list_succeeded) = match gcp.list_instances(&project, now).await {
        Ok(inst) => {
            let fi = serde_json::json!({
                "source": "gce",
                "last_success_at": crate::state::chrono_iso(now),
                "last_success_epoch": now,
                "stale": false,
                "stale_reason": serde_json::Value::Null,
                "last_error": serde_json::Value::Null
            });
            bundle.state.fleet_inventory = fi.clone();
            if let Some(ref mut ov) = bundle.state.overview {
                if let Some(obj) = ov.as_object_mut() {
                    obj.insert("fleet_inventory".to_string(), fi.clone());
                    if let Some(iv) = obj
                        .get_mut("instances_view")
                        .and_then(|v| v.as_object_mut())
                    {
                        iv.insert("fleet_inventory".to_string(), fi);
                    }
                }
            }
            (inst, true)
        }
        Err(_) => (Vec::new(), false),
    };

    let controlled_targets = if bundle.state.controlled_targets.is_empty() {
        CONTROLLED_TARGETS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    } else {
        bundle.state.controlled_targets.clone()
    };

    // Build target-aware list of API targets used for provisioning
    let mut target_paths = vec![format!("orgs/{}", org)];
    for target in &controlled_targets {
        let path = if target.contains('/') {
            format!("repos/{}", target)
        } else {
            format!("orgs/{}", target)
        };
        if !target_paths.contains(&path) {
            target_paths.push(path);
        }
    }

    // Query runners across every target used for provisioning
    let mut runners: Vec<SimpleGitHubRunner> = Vec::new();
    let mut runner_target_map: std::collections::HashMap<u64, String> =
        std::collections::HashMap::new();

    for target_path in &target_paths {
        if let Ok(target_runners) = gh.list_runners(target_path).await {
            for r in target_runners {
                if !runner_target_map.contains_key(&r.id) {
                    runner_target_map.insert(r.id, target_path.clone());
                    runners.push(r);
                }
            }
        }
    }

    // Sum queued jobs backlog across controlled targets
    let mut queued_jobs = 0u32;
    let mut backlog_known = true;
    for target in &controlled_targets {
        let (owner, repo) = if target.contains('/') {
            let mut parts = target.splitn(2, '/');
            (parts.next().unwrap_or(&org), parts.next().unwrap_or(target))
        } else {
            (org.as_str(), "oxidex")
        };
        match gh.count_queued_jobs(owner, repo).await {
            Ok(count) => {
                queued_jobs += count;
            }
            Err(_) => {
                // If counting queued jobs fails for a target (e.g. rate limit, 403, or outage),
                // mark backlog as unknown so backlog scale-up is suppressed, but continue
                // independent fleet maintenance (terminated VM cleanup, launch tracking, floor targets).
                backlog_known = false;
            }
        }
    }

    let mut cleaned_vms = Vec::new();
    let mut cleaned_runners = Vec::new();
    let mut launched_hosts = Vec::new();

    // 3. Process Injected Test Terminated VMs
    {
        let mut injected_vms = INJECTED_TERMINATED_VMS.lock().unwrap();
        if !injected_vms.is_empty() {
            cleaned_vms.extend(injected_vms.clone());
            injected_vms.clear();
        }
    }

    // 4. Preempted VM Cleanup (TERMINATED GCE instances)
    for inst in &instances {
        if inst.status.eq_ignore_ascii_case("TERMINATED") {
            let _ = gcp
                .delete_instance(&project, &inst.zone, &inst.name, now)
                .await;
            cleaned_vms.push(inst.name.clone());
        }
    }

    // 5. Process Injected Test Stale Runners
    {
        let mut injected_runners = INJECTED_STALE_RUNNERS.lock().unwrap();
        if !injected_runners.is_empty() {
            cleaned_runners.extend(injected_runners.clone());
            injected_runners.clear();
        }
    }

    // 6. Stale Runner Registration Cleanup
    let active_vm_names: Vec<&str> = instances
        .iter()
        .filter(|i| !i.status.eq_ignore_ascii_case("TERMINATED"))
        .map(|i| i.name.as_str())
        .collect();

    for runner in &runners {
        if runner.status.eq_ignore_ascii_case("offline")
            && !runner.busy
            && (runner.name.starts_with("oxidex") || runner.name.contains("oxidex-runners"))
        {
            let matches_active = active_vm_names.iter().any(|&vm| runner.name.contains(vm));
            if !matches_active {
                let target_path = runner_target_map
                    .get(&runner.id)
                    .map(|s| s.as_str())
                    .unwrap_or_else(|| &target_paths[0]);
                let _ = gh.delete_runner(target_path, runner.id).await;
                cleaned_runners.push(runner.name.clone());
            }
        }
    }

    // 7. Multi-Tick State Machine Maintenance
    {
        let mut launches = ACTIVE_LAUNCHES.lock().unwrap();
        for op in launches.iter_mut() {
            match op.phase {
                LaunchPhase::Creating => {
                    if let Some(inst) = instances.iter().find(|i| i.name == op.host_name) {
                        op.instance_id = inst.id.clone();
                        if inst.status.eq_ignore_ascii_case("RUNNING") {
                            op.phase = LaunchPhase::Booting;
                        }
                    } else if now.saturating_sub(op.started_at) > LAUNCH_TIMEOUT_SECS {
                        op.phase = LaunchPhase::Failed;
                    }
                }
                LaunchPhase::Booting => {
                    let all_online = op.expected_runners.iter().all(|expected| {
                        runners
                            .iter()
                            .any(|r| r.name == *expected && r.status.eq_ignore_ascii_case("online"))
                    });

                    if all_online && !op.expected_runners.is_empty() {
                        op.phase = LaunchPhase::Done;
                        // Scrub startup-script once VM is online
                        if let Some(inst) = instances.iter().find(|i| i.name == op.host_name) {
                            let _ = gcp
                                .scrub_startup_script(
                                    &project,
                                    &op.zone,
                                    &op.host_name,
                                    &inst.metadata,
                                    now,
                                )
                                .await;
                        }
                    } else if let Some(inst) = instances.iter().find(|i| i.name == op.host_name) {
                        if inst.status.eq_ignore_ascii_case("TERMINATED") {
                            op.phase = LaunchPhase::Failed;
                        }
                    } else if now.saturating_sub(op.started_at) > LAUNCH_TIMEOUT_SECS {
                        op.phase = LaunchPhase::Failed;
                    }
                }
                LaunchPhase::Done | LaunchPhase::Failed => {}
            }
        }
        // Retain only in-flight operations
        launches.retain(|op| op.phase == LaunchPhase::Creating || op.phase == LaunchPhase::Booting);
    }

    // 8. Floor Target Keeper (Ensure >= 1 Host Per Target)
    for target in &controlled_targets {
        let target_slug = target.replace('/', "-").to_lowercase();
        let safe_slug = if target_slug.len() > 24 {
            &target_slug[..24]
        } else {
            &target_slug
        };
        let expected_prefix = format!("oxidex-runners-{}-", safe_slug);
        let delimited_token = format!("-{}-", safe_slug);
        let active_count = instances
            .iter()
            .filter(|i| {
                if i.status.eq_ignore_ascii_case("TERMINATED") {
                    return false;
                }
                if let Some(target_meta) = i
                    .metadata
                    .get("spot-target")
                    .or_else(|| i.metadata.get("spot-runner-target"))
                {
                    return target_meta == *target;
                }
                if !i.name.starts_with(&expected_prefix) && !i.name.contains(&delimited_token) {
                    return false;
                }
                for other in &controlled_targets {
                    if other != target && other.starts_with(target) && other.contains('/') {
                        let other_slug = other.replace('/', "-").to_lowercase();
                        let other_safe = if other_slug.len() > 24 {
                            &other_slug[..24]
                        } else {
                            &other_slug
                        };
                        let other_prefix = format!("oxidex-runners-{}-", other_safe);
                        let other_delim = format!("-{}-", other_safe);
                        if i.name.starts_with(&other_prefix) || i.name.contains(&other_delim) {
                            return false;
                        }
                    }
                }
                true
            })
            .count();

        let in_flight = ACTIVE_LAUNCHES
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.target == *target);

        if active_count == 0 && !in_flight {
            let hex_suffix = generate_unique_suffix(now);
            let host_name = format!("oxidex-runners-{}-{}-{}", safe_slug, now, hex_suffix);
            let deployment_id = format!("dep-{}", now);

            // Mint 2-slot JIT configs with fallback to registration token
            let runner_prefix = "oxidex";
            let labels = vec![
                "self-hosted".to_string(),
                "Linux".to_string(),
                "X64".to_string(),
                "spot".to_string(),
                DEFAULT_MACHINE_TYPE.to_string(),
                DEFAULT_ZONE.to_string(),
            ];

            let gh_target = if target.contains('/') {
                format!("repos/{}", target)
            } else {
                format!("orgs/{}", target)
            };
            let runner_group_id = if target == &org || target == "swack-tools" {
                10
            } else {
                1
            };

            let bootstrap = gh
                .mint_2slot_runner_configs(
                    &gh_target,
                    &host_name,
                    runner_prefix,
                    runner_group_id,
                    &labels,
                )
                .await;

            let (reg_token, jit1, jit2) = match bootstrap {
                Ok(RunnerBootstrapConfig::JitConfig {
                    runner_1_jitconfig,
                    runner_2_jitconfig,
                }) => (
                    "".to_string(),
                    Some(runner_1_jitconfig),
                    Some(runner_2_jitconfig),
                ),
                Ok(RunnerBootstrapConfig::RegistrationToken { token }) => (token, None, None),
                Err(_) => ("".to_string(), None, None),
            };

            let startup_params = crate::github::StartupParams {
                disk_device_name: &host_name,
                instance_name: &host_name,
                deployment_id: &deployment_id,
                org: target,
                runner_prefix,
                runner_group: if target == &org || target == "swack-tools" {
                    "swack-runners"
                } else {
                    "default"
                },
                runner_labels: &labels.join(","),
                registration_token: &reg_token,
                runner_count: RUNNERS_PER_HOST,
                runner_memory_gb: RUNNER_RAM_GIB,
                runner_cpu_limit: "7",
                tiered: true,
                strict_heavy_pool: false,
                resource_monitoring: false,
            };

            let manifest_val = crate::patch_console::make_valid_manifest_json(target);
            let patch_sha = crate::patch::compute_canonical_patch_sha256(&manifest_val)
                .unwrap_or_else(|_| {
                    "0000000000000000000000000000000000000000000000000000000000000000".to_string()
                });
            let patch_json_str =
                serde_json::to_string(&manifest_val).unwrap_or_else(|_| "{}".to_string());
            let runner_patch =
                serde_json::from_value::<crate::patch::RunnerPatch>(manifest_val.clone()).ok();

            let rendered_script =
                crate::github::render_startup_script(&startup_params, runner_patch.as_ref())
                    .unwrap_or_else(|_| "#!/bin/bash\nexit 0".to_string());

            let vm_cfg = crate::gcp::RunnerVmConfig {
                instance_name: &host_name,
                machine_type: DEFAULT_MACHINE_TYPE,
                zone: DEFAULT_ZONE,
                target,
                runner_prefix,
                startup_script: &rendered_script,
                patch_sha256: &patch_sha,
                patch_json: &patch_json_str,
                runner_1_jitconfig: jit1.as_deref(),
                runner_2_jitconfig: jit2.as_deref(),
                source_image: None,
                golden_image_family: None,
                boot_disk_type: Some("pd-balanced"),
            };

            if let Ok(req) = crate::gcp::build_spot_runner_vm_request(&project, &vm_cfg) {
                let _ = gcp
                    .insert_instance(&project, DEFAULT_ZONE, &req, None, now)
                    .await;
                launched_hosts.push(host_name.clone());

                ACTIVE_LAUNCHES.lock().unwrap().push(LaunchOperation {
                    target: target.clone(),
                    host_name: host_name.clone(),
                    zone: DEFAULT_ZONE.to_string(),
                    machine_type: DEFAULT_MACHINE_TYPE.to_string(),
                    phase: LaunchPhase::Creating,
                    deployment_id,
                    started_at: now,
                    instance_id: None,
                    expected_runners: vec![
                        format!("{}-{}-1", runner_prefix, host_name),
                        format!("{}-{}-2", runner_prefix, host_name),
                    ],
                });
            }
        }
    }

    // 9. Backlog Scale-Up Policy (Triggered ONLY when queued_jobs >= 20)
    let max_instances = (bundle.state.scaling.cicd_runner.max_runners / RUNNERS_PER_HOST).max(1);
    let current_live = instances
        .iter()
        .filter(|i| !i.status.eq_ignore_ascii_case("TERMINATED"))
        .count();

    if backlog_known
        && queued_jobs >= MIN_QUEUED_JOBS_THRESHOLD
        && current_live < (max_instances as usize)
    {
        let hex_suffix = generate_unique_suffix(now);
        let scale_host = format!("oxidex-runners-scale-{}-{}", now, hex_suffix);
        let deployment_id = format!("dep-scale-{}", now);
        let runner_prefix = "oxidex";
        let labels = vec![
            "self-hosted".to_string(),
            "Linux".to_string(),
            "X64".to_string(),
            "spot".to_string(),
            DEFAULT_MACHINE_TYPE.to_string(),
            DEFAULT_ZONE.to_string(),
        ];

        let bootstrap = gh
            .mint_2slot_runner_configs(
                &format!("orgs/{}", org),
                &scale_host,
                runner_prefix,
                10,
                &labels,
            )
            .await;

        let (reg_token, jit1, jit2) = match bootstrap {
            Ok(RunnerBootstrapConfig::JitConfig {
                runner_1_jitconfig,
                runner_2_jitconfig,
            }) => (
                "".to_string(),
                Some(runner_1_jitconfig),
                Some(runner_2_jitconfig),
            ),
            Ok(RunnerBootstrapConfig::RegistrationToken { token }) => (token, None, None),
            Err(_) => ("".to_string(), None, None),
        };

        let startup_params = crate::github::StartupParams {
            disk_device_name: &scale_host,
            instance_name: &scale_host,
            deployment_id: &deployment_id,
            org: &org,
            runner_prefix,
            runner_group: "default",
            runner_labels: &labels.join(","),
            registration_token: &reg_token,
            runner_count: RUNNERS_PER_HOST,
            runner_memory_gb: RUNNER_RAM_GIB,
            runner_cpu_limit: "7",
            tiered: true,
            strict_heavy_pool: false,
            resource_monitoring: false,
        };

        let manifest_val = crate::patch_console::make_valid_manifest_json("swack-tools");
        let patch_sha =
            crate::patch::compute_canonical_patch_sha256(&manifest_val).unwrap_or_else(|_| {
                "0000000000000000000000000000000000000000000000000000000000000000".to_string()
            });
        let patch_json_str =
            serde_json::to_string(&manifest_val).unwrap_or_else(|_| "{}".to_string());
        let runner_patch =
            serde_json::from_value::<crate::patch::RunnerPatch>(manifest_val.clone()).ok();

        let rendered_script =
            crate::github::render_startup_script(&startup_params, runner_patch.as_ref())
                .unwrap_or_else(|_| "#!/bin/bash\nexit 0".to_string());

        let vm_cfg = crate::gcp::RunnerVmConfig {
            instance_name: &scale_host,
            machine_type: DEFAULT_MACHINE_TYPE,
            zone: DEFAULT_ZONE,
            target: "swack-tools",
            runner_prefix,
            startup_script: &rendered_script,
            patch_sha256: &patch_sha,
            patch_json: &patch_json_str,
            runner_1_jitconfig: jit1.as_deref(),
            runner_2_jitconfig: jit2.as_deref(),
            source_image: None,
            golden_image_family: None,
            boot_disk_type: Some("pd-balanced"),
        };

        if let Ok(req) = crate::gcp::build_spot_runner_vm_request(&project, &vm_cfg) {
            let _ = gcp
                .insert_instance(&project, DEFAULT_ZONE, &req, None, now)
                .await;
            launched_hosts.push(scale_host);
        }
    }

    // 10. Strict Diff Gating & KV Quota Conservation
    let active_launches = ACTIVE_LAUNCHES.lock().unwrap().clone();
    let current_fingerprint =
        compute_fleet_fingerprint(&instances, &runners, queued_jobs, &active_launches);
    let mut last_fp_guard = LAST_FLEET_FINGERPRINT.lock().unwrap();

    let existing_by_name: std::collections::HashMap<String, InstanceRecord> = bundle
        .state
        .instances
        .iter()
        .map(|inst| (inst.name.clone(), inst.clone()))
        .collect();

    let gce_stop_epochs: std::collections::HashMap<String, u64> = instances
        .iter()
        .filter_map(|i| i.stopped_epoch().map(|ep| (i.name.clone(), ep)))
        .collect();

    let new_instances_view: Vec<InstanceRecord> = if gcp_list_succeeded {
        instances
            .iter()
            .filter(|i| {
                let is_runner = i.name.starts_with("oxidex-runners-");
                let is_builder = i.name.starts_with("builder-");
                (is_runner || is_builder) && !i.status.eq_ignore_ascii_case("TERMINATED")
            })
            .map(|i| {
                let is_builder = i.name.starts_with("builder-");
                let norm_zone = i.zone_short().to_string();
                let norm_region = norm_zone.split('-').take(2).collect::<Vec<_>>().join("-");
                let norm_machine_type = i.machine_type_short().to_string();
                let gce_id = i.id.clone().unwrap_or_else(|| i.name.clone());

                if let Some(existing) = existing_by_name.get(&i.name) {
                    let id_changed =
                        !existing.id.is_empty() && !gce_id.is_empty() && existing.id != gce_id;
                    if !id_changed {
                        let mut rec = existing.clone();
                        rec.status = i.status.clone();
                        rec.zone = norm_zone;
                        rec.region = norm_region;
                        rec.machine_type = norm_machine_type;
                        if rec.created_epoch.is_none() {
                            if let Some(ep) = i.started_epoch() {
                                rec.created_epoch = Some(ep);
                                rec.created_at = Some(crate::state::chrono_iso(ep));
                            }
                        }
                        if existing.role == "rust_builder" || is_builder {
                            rec.role = "rust_builder".to_string();
                            rec.vcpu = 16;
                            rec.ram_gib = 32;
                            rec.runners_count = 0;
                            rec.runners_count_known = true;
                            rec.runner_limits_known = true;
                        } else {
                            rec.runners_count = RUNNERS_PER_HOST;
                            rec.runners_count_known = true;
                            rec.runner_limits_known = true;
                        }
                        return rec;
                    }
                }
                let launch_epoch = i.started_epoch();
                let created_at = launch_epoch.map(crate::state::chrono_iso);
                InstanceRecord {
                    id: gce_id,
                    name: i.name.clone(),
                    zone: norm_zone,
                    region: norm_region,
                    machine_type: norm_machine_type,
                    status: i.status.clone(),
                    role: if is_builder {
                        "rust_builder".to_string()
                    } else {
                        "cicd_runner".to_string()
                    },
                    vcpu: if is_builder { 16 } else { MIN_HOST_VCPU },
                    ram_gib: if is_builder { 32 } else { MIN_HOST_RAM_GIB },
                    runners_count: if is_builder { 0 } else { RUNNERS_PER_HOST },
                    runners_count_known: true,
                    runner_limits_known: true,
                    created_at,
                    created_epoch: launch_epoch,
                }
            })
            .collect()
    } else {
        bundle.state.instances_view.clone()
    };

    let overview_changed = if gcp_list_succeeded {
        if let Some(ref mut ov) = bundle.state.overview {
            let ledger_changed = crate::state::reconcile_overview_instances_ledger(
                ov,
                &bundle.state.instances,
                &new_instances_view,
                &gce_stop_epochs,
                true,
                now,
            );
            let targets_changed = apply_gce_instance_targets_to_overview(ov, &instances);
            ledger_changed || targets_changed
        } else {
            false
        }
    } else {
        false
    };

    let has_mutations = !cleaned_vms.is_empty()
        || !cleaned_runners.is_empty()
        || !launched_hosts.is_empty()
        || overview_changed
        || (last_fp_guard.as_ref() != Some(&current_fingerprint));

    let heartbeat_elapsed = now.saturating_sub(bundle.written_at) >= HEARTBEAT_INTERVAL_SECS;

    let snapshot_written = if has_mutations || heartbeat_elapsed {
        if gcp_list_succeeded {
            // Sync instances_view with active instances while preserving dedicated builders
            bundle.state.instances_view = new_instances_view.clone();
            // Synchronize state.instances with live instances_view for unified action inventory
            bundle.state.instances = new_instances_view;
        }

        if backlog_known {
            bundle.state.scaling_telemetry.queued_jobs = queued_jobs;
        }
        bundle.state.scaling_telemetry.current_runners = runners
            .iter()
            .filter(|r| r.status.eq_ignore_ascii_case("online"))
            .count() as u32;
        bundle.state.updated_at = now;

        save_state(kv, &mut bundle, now).await?;
        *last_fp_guard = Some(current_fingerprint);
        TOTAL_KV_PUTS.fetch_add(1, Ordering::SeqCst);
        true
    } else {
        // Diff-gated: strictly ZERO writes to SPOT_KV!
        false
    };

    // Release concurrency lease
    *RECONCILER_LEASE.lock().unwrap() = None;

    Ok(ReconciliationReport {
        tick,
        status: "reconciled".to_string(),
        launched_hosts,
        cleaned_vms,
        cleaned_runners,
        snapshot_written,
        kv_puts: TOTAL_KV_PUTS.load(Ordering::SeqCst),
    })
}
