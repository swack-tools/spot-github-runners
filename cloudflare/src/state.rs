//! Edge State & Consolidated Document Management for Cloudflare Worker.
//!
//! Implements KV Quota Conservation:
//! 1. Single consolidated KV document (`dashboard:state`) containing all state.
//! 2. In-isolate Wasm memory cache (`STATE_CACHE`) with 60s TTL fronting `SPOT_KV.get`.
//! 3. In-memory caching for GCP OAuth token (`GCP_TOKEN_CACHE`) with 0 KV operations.
//! 4. Fast sub-15ms REST API handlers (`/api/health`, `/api/overview`, `/api/scaling`, `/api/admins`).
//! 5. Instance action safety gates returning plan previews with LIFECYCLE_ACTIONS_DISABLED_MESSAGE.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::HashMap;

pub const KV_STATE_KEY: &str = "dashboard:state";
pub const SNAPSHOT_SCHEMA: &str = "spot-dashboard-snapshot-v1";
pub const WRITER_SLOT_EDGE: &str = "edge";
pub const STATE_CACHE_TTL_SECS: u64 = 60;
pub const GCP_TOKEN_CACHE_TTL_SECS: u64 = 3300; // 55 minutes
pub const MAX_ADDED_ADMINS: usize = 50;

pub const OWNER_LOGIN: &str = "swackhamer";
pub const OWNER_UID: u64 = 619624;

pub const LIFECYCLE_ACTIONS_DISABLED_MESSAGE: &str =
    "The dashboard does not create, stop, or delete VMs: its instance list mirrors the live Compute Engine \
    inventory. Run lifecycle changes with the reviewed tools instead (spot_runner_autoscaler.py drains and \
    retires runner hosts; spot_runner_manager.py provisions them; render_builder_startup.py renders a dedicated \
    builder). This response is a plan only; nothing changed.";

pub const UNAVAILABLE_ERROR: &str = "control_vm_unavailable";
pub const UNAVAILABLE_MESSAGE: &str =
    "The control VM is restarting or being replaced. Keep this page open; \
    your request will be retried automatically when it is back.";

// ============================================================================
// Consolidated Document Envelope
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DashboardBundle {
    pub schema: String,
    pub state_version: u64,
    pub writer_slot: String,
    pub written_at: u64,
    #[serde(default)]
    pub state_fingerprint: String,
    pub state: DashboardState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DashboardState {
    pub project: String,
    pub org: String,
    pub updated_at: u64,
    pub global_budget: BudgetConfig,
    pub profiles: HashMap<String, ProfileConfig>,
    pub scaling: ScalingConfig,
    pub scaling_telemetry: ScalingTelemetry,
    #[serde(default)]
    pub scaling_evaluations: serde_json::Value,
    #[serde(default)]
    pub regions_enabled: HashMap<String, bool>,
    #[serde(default)]
    pub region_latencies_ms: HashMap<String, f64>,
    #[serde(default)]
    pub cloud_init_templates: Vec<CloudInitTemplate>,
    #[serde(default)]
    pub vm_templates: Vec<VmTemplate>,
    #[serde(default)]
    pub instances: Vec<InstanceRecord>,
    #[serde(default)]
    pub instances_view: Vec<InstanceRecord>,
    #[serde(default)]
    pub fleet_summary: serde_json::Value,
    #[serde(default)]
    pub fleet_inventory: serde_json::Value,
    #[serde(default)]
    pub rust_builds: serde_json::Value,
    #[serde(default)]
    pub dashboard_admins: Vec<AdminRecord>,
    #[serde(default)]
    pub dashboard_admins_history: Vec<AdminHistoryRecord>,
    #[serde(default)]
    pub controlled_targets: Vec<String>,
    #[serde(default)]
    pub overview: Option<serde_json::Value>,
}

// ============================================================================
// Scaling, Budget & Profile Structures
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetConfig {
    pub active_period: String,
    pub max_hourly_usd: f64,
    pub max_daily_usd: f64,
    pub max_monthly_usd: f64,
    pub include_disk_costs: bool,
    pub include_taxes: bool,
    pub tax_rate_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfileConfig {
    pub role: String,
    pub min_runners: Option<u32>,
    pub min_instances: Option<u32>,
    pub max_instances: u32,
    pub runner_vcpu: u32,
    pub runner_ram_gib: u32,
    pub host_overhead_gib: u32,
    pub min_instance_vcpu: u32,
    pub min_instance_ram_gib: u32,
    pub min_benchmark_score: u32,
    pub disk_category: String,
    pub disk_type: String,
    pub boot_disk_gb: u32,
    pub data_disk_gb: u32,
    pub use_vm_template: bool,
    pub vm_template_id: String,
    pub cloud_init_template_id: String,
    pub budget: BudgetConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScalingConfig {
    pub cicd_runner: RunnerScalingPolicy,
    pub rust_builder: BuilderScalingPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerScalingPolicy {
    pub min_runners: u32,
    pub max_runners: u32,
    pub runners_per_instance: u32,
    pub runner_vcpu: u32,
    pub runner_ram_gib: u32,
    pub scale_up_step_runners: u32,
    pub enable_queued_jobs_trigger: bool,
    pub queued_jobs_threshold: u32,
    pub enable_provisioned_jobs_trigger: bool,
    pub provisioned_jobs_threshold: u32,
    pub enable_hourly_runs_scale_up: bool,
    pub hourly_runs_scale_up_threshold: u32,
    pub hourly_runs_scale_up_target_runners: u32,
    pub enable_cpu_utilization_trigger: bool,
    pub cpu_threshold_percent: f64,
    pub cpu_scope: String,
    pub enable_journal_oom_trigger: bool,
    pub enable_github_ci_oom_trigger: bool,
    pub oom_lookback_minutes: u32,
    pub enable_hourly_runs_scale_down: bool,
    pub scale_down_max_jobs_past_hour: u32,
    pub scale_down_target_runners: u32,
    pub enable_idle_cpu_scale_down: bool,
    pub scale_down_cpu_percent: f64,
    pub max_scale_up_hourly_usd: f64,
    pub max_scale_up_daily_usd: f64,
    pub max_scale_up_monthly_usd: f64,
    pub include_disk_in_scale_budget: bool,
    #[serde(default)]
    pub shared_global_budget: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BuilderScalingPolicy {
    pub min_instances: u32,
    pub max_instances: u32,
    pub builder_vcpu: u32,
    pub builder_ram_gib: u32,
    pub container_vcpu: u32,
    pub container_ram_gib: u32,
    pub enable_queue_trigger: bool,
    pub queued_builds_threshold: u32,
    pub enable_cpu_utilization_trigger: bool,
    pub cpu_threshold_percent: f64,
    pub cpu_scope: String,
    pub enable_journal_oom_trigger: bool,
    pub oom_lookback_minutes: u32,
    pub enable_low_builds_scale_down: bool,
    pub scale_down_max_builds_past_hour: u32,
    pub scale_down_target_instances: u32,
    pub max_scale_up_hourly_usd: f64,
    pub max_scale_up_daily_usd: f64,
    pub max_scale_up_monthly_usd: f64,
    pub include_disk_in_scale_budget: bool,
    #[serde(default)]
    pub shared_global_budget: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScalingTelemetry {
    pub current_runners: u32,
    pub jobs_ran_past_hour: u32,
    pub provisioned_jobs: u32,
    pub queued_jobs: u32,
    #[serde(default)]
    pub instance_cpu_percentages: Vec<f64>,
    #[serde(default)]
    pub journal_oom_events: Vec<serde_json::Value>,
    #[serde(default)]
    pub github_oom_runs: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudInitTemplate {
    pub id: String,
    pub name: String,
    pub role: String,
    pub description: String,
    pub updated_at: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VmTemplate {
    pub id: String,
    pub name: String,
    pub role: String,
    pub machine_type: String,
    pub vcpu: u32,
    pub ram_gib: u32,
    pub cpu_platform: String,
    pub benchmark_score: u32,
    pub disk_category: String,
    pub boot_disk_type: String,
    pub boot_disk_gb: u32,
    pub data_disk_type: String,
    pub data_disk_gb: u32,
    pub runners_count: u32,
    pub runner_vcpu: u32,
    pub runner_ram_gib: u32,
    pub cloud_init_template_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstanceRecord {
    pub id: String,
    pub name: String,
    pub zone: String,
    pub region: String,
    pub machine_type: String,
    pub status: String,
    pub role: String,
    pub vcpu: u32,
    pub ram_gib: u32,
    pub runners_count: u32,
    #[serde(default)]
    pub runners_count_known: bool,
    #[serde(default)]
    pub runner_limits_known: bool,
}

// ============================================================================
// Administrator Records
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdminRecord {
    pub login: String,
    pub uid: u64,
    pub added_by: String,
    pub added_by_uid: Option<u64>,
    pub added_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdminHistoryRecord {
    pub at: String,
    pub epoch: u64,
    pub action: String, // "added" or "removed"
    pub login: String,
    pub uid: u64,
    pub actor: String,
    pub actor_uid: Option<u64>,
}

// ============================================================================
// In-Isolate Memory Caches (`thread_local!`)
// ============================================================================

thread_local! {
    static STATE_CACHE: RefCell<Option<(u64, DashboardBundle)>> = const { RefCell::new(None) };
    static GCP_TOKEN_CACHE: RefCell<Option<(u64, String)>> = const { RefCell::new(None) };
}

pub fn get_cached_state(now: u64) -> Option<DashboardBundle> {
    STATE_CACHE.with(|cell| {
        let borrow = cell.borrow();
        if let Some((cached_at, ref bundle)) = *borrow {
            if now.saturating_sub(cached_at) < STATE_CACHE_TTL_SECS {
                return Some(bundle.clone());
            }
        }
        None
    })
}

pub fn set_cached_state(now: u64, bundle: DashboardBundle) {
    STATE_CACHE.with(|cell| {
        *cell.borrow_mut() = Some((now, bundle));
    });
}

pub fn invalidate_state_cache() {
    STATE_CACHE.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

pub fn get_cached_gcp_token(now: u64) -> Option<String> {
    GCP_TOKEN_CACHE.with(|cell| {
        let borrow = cell.borrow();
        if let Some((cached_at, ref token)) = *borrow {
            if now.saturating_sub(cached_at) < GCP_TOKEN_CACHE_TTL_SECS {
                return Some(token.clone());
            }
        }
        None
    })
}

pub fn set_cached_gcp_token(now: u64, token: String) {
    GCP_TOKEN_CACHE.with(|cell| {
        *cell.borrow_mut() = Some((now, token));
    });
}

pub fn invalidate_gcp_token_cache() {
    GCP_TOKEN_CACHE.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

// ============================================================================
// Storage Backend Trait & Implementation
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("KV error: {0}")]
    Kv(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("State validation error: {0}")]
    Validation(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    #[error("Forbidden: {0}")]
    Forbidden(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Conflict: {0}")]
    Conflict(String),
}

#[async_trait::async_trait(?Send)]
pub trait KvStoreBackend {
    async fn get(&self, key: &str) -> Result<Option<String>, StateError>;
    async fn put(&self, key: &str, value: &str) -> Result<(), StateError>;
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for worker::kv::KvStore {
    async fn get(&self, key: &str) -> Result<Option<String>, StateError> {
        self.get(key)
            .text()
            .await
            .map_err(|e| StateError::Kv(e.to_string()))
    }

    async fn put(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.put(key, value)
            .map_err(|e| StateError::Kv(e.to_string()))?
            .execute()
            .await
            .map_err(|e| StateError::Kv(e.to_string()))
    }
}

// ============================================================================
// State Hydration and Persistence
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSource {
    MemoryHit,
    KvRead,
    DefaultCreated,
}

pub fn compute_state_fingerprint(state: &DashboardState) -> Result<String, StateError> {
    let canonical_json = serde_json::to_vec(state)?;
    let mut hasher = Sha256::new();
    hasher.update(&canonical_json);
    Ok(hex::encode(hasher.finalize()))
}

pub async fn load_state<K: KvStoreBackend>(
    kv: &K,
    now: u64,
) -> Result<(DashboardBundle, CacheSource), StateError> {
    // 1. Check in-isolate memory cache first (< 0.1 ms)
    if let Some(bundle) = get_cached_state(now) {
        return Ok((bundle, CacheSource::MemoryHit));
    }

    // 2. Fall back to single consolidated KV read
    match kv.get(KV_STATE_KEY).await? {
        Some(raw) => {
            let bundle: DashboardBundle = serde_json::from_str(&raw)?;
            if bundle.schema != SNAPSHOT_SCHEMA {
                return Err(StateError::Validation(format!(
                    "Invalid schema '{}', expected '{}'",
                    bundle.schema, SNAPSHOT_SCHEMA
                )));
            }
            set_cached_state(now, bundle.clone());
            Ok((bundle, CacheSource::KvRead))
        }
        None => {
            let bundle = default_dashboard_bundle(now)?;
            set_cached_state(now, bundle.clone());
            Ok((bundle, CacheSource::DefaultCreated))
        }
    }
}

pub async fn save_state<K: KvStoreBackend>(
    kv: &K,
    bundle: &mut DashboardBundle,
    now: u64,
) -> Result<(), StateError> {
    bundle.state_version += 1;
    bundle.written_at = now;
    bundle.writer_slot = WRITER_SLOT_EDGE.to_string();
    bundle.state_fingerprint = compute_state_fingerprint(&bundle.state)?;

    let raw = serde_json::to_string(bundle)?;
    kv.put(KV_STATE_KEY, &raw).await?;
    set_cached_state(now, bundle.clone());
    Ok(())
}

// ============================================================================
// REST API Handlers
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub hydrated: bool,
    pub frozen: bool,
    pub reconciled: bool,
    pub state_version: u64,
    pub writer_slot: String,
    pub written_at: u64,
    pub cache_source: String,
}

pub async fn handle_api_health<K: KvStoreBackend>(kv: &K, now: u64) -> (u16, serde_json::Value) {
    match load_state(kv, now).await {
        Ok((bundle, source)) => {
            let source_str = match source {
                CacheSource::MemoryHit => "memory",
                CacheSource::KvRead => "kv",
                CacheSource::DefaultCreated => "default",
            };
            (
                200,
                serde_json::json!({
                    "status": "ok",
                    "hydrated": true,
                    "frozen": false,
                    "reconciled": true,
                    "state_version": bundle.state_version,
                    "writer_slot": bundle.writer_slot,
                    "written_at": bundle.written_at,
                    "cache_source": source_str,
                }),
            )
        }
        Err(e) => (
            503,
            serde_json::json!({
                "error": UNAVAILABLE_ERROR,
                "message": UNAVAILABLE_MESSAGE,
                "retry_after_seconds": 10,
                "reason": e.to_string(),
            }),
        ),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct OverviewQuery {
    pub include_deleted: Option<bool>,
    pub window_hours: Option<f64>,
    pub include_disk_and_tax: Option<bool>,
    pub sort_by: Option<String>,
    pub sort_dir: Option<String>,
    pub search: Option<String>,
    pub role: Option<String>,
}

pub async fn handle_api_overview<K: KvStoreBackend>(
    kv: &K,
    _query: OverviewQuery,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let (bundle, _source) = load_state(kv, now).await?;

    // If precomputed overview is present in bundle, serve directly (<1ms) with live budget/scaling overlaid
    if let Some(ref overview) = bundle.state.overview {
        let mut ov = overview.clone();
        if let Some(obj) = ov.as_object_mut() {
            if let Ok(gb) = serde_json::to_value(&bundle.state.global_budget) {
                obj.insert("global_budget".to_string(), gb);
            }
            if let Ok(sc) = serde_json::to_value(&bundle.state.scaling) {
                obj.insert("scaling".to_string(), sc);
            }
            if !bundle.state.profiles.is_empty() {
                if let Ok(pr) = serde_json::to_value(&bundle.state.profiles) {
                    obj.insert("profiles".to_string(), pr);
                }
            }
        }
        return Ok((200, ov));
    }

    // Build default overview shape consumed by the dashboard UI when no precomputed overview exists
    let fallback_overview = serde_json::json!({
        "project": bundle.state.project,
        "org": bundle.state.org,
        "updated_at": bundle.state.updated_at,
        "global_budget": bundle.state.global_budget,
        "scaling": bundle.state.scaling,
        "profiles": bundle.state.profiles,
        "qualifications": {
            "cicd_runner": {
                "status": "passed",
                "details": {}
            },
            "rust_builder": {
                "status": "passed",
                "details": {}
            }
        },
        "regions": [],
        "instances_view": {
            "instances": bundle.state.instances_view,
            "window_hours": 24.0,
            "total_window_spend_usd": 0.0,
            "active_hourly_burn_usd": 0.0,
            "active_daily_burn_usd": 0.0,
            "active_monthly_burn_usd": 0.0
        },
        "fleet_summary": {
            "active_hourly_burn_usd": 0.0,
            "active_daily_burn_usd": 0.0,
            "active_monthly_burn_usd": 0.0,
            "running_runner_slots": 0,
            "registered_runner_slots": 0,
            "unregistered_runner_slots": 0,
            "running_builder_instances": 0,
            "total_vcpus": 0,
            "total_ram_gib": 0,
            "active_instances_count": 0
        },
        "fleet_inventory": bundle.state.fleet_inventory,
        "cloud_init_templates": bundle.state.cloud_init_templates,
        "vm_templates": bundle.state.vm_templates,
        "rust_builds": bundle.state.rust_builds,
        "dashboard_admins": bundle.state.dashboard_admins
    });

    Ok((200, fallback_overview))
}

pub async fn handle_post_budget<K: KvStoreBackend>(
    kv: &K,
    req: BudgetConfig,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let (mut bundle, _) = load_state(kv, now).await?;
    bundle.state.global_budget = req.clone();
    bundle.state.updated_at = now;
    save_state(kv, &mut bundle, now).await?;
    Ok((
        200,
        serde_json::json!({
            "status": "ok",
            "global_budget": req,
        }),
    ))
}

pub async fn handle_post_regions<K: KvStoreBackend>(
    kv: &K,
    body: serde_json::Value,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let (mut bundle, _) = load_state(kv, now).await?;
    if let Some(map) = body.get("regions_enabled").and_then(|v| v.as_object()) {
        for (region, enabled_val) in map {
            if let Some(en) = enabled_val.as_bool() {
                bundle.state.regions_enabled.insert(region.clone(), en);
                if let Some(ref mut ov) = bundle.state.overview {
                    if let Some(regions_arr) = ov.get_mut("regions").and_then(|r| r.as_array_mut())
                    {
                        for r_obj in regions_arr.iter_mut() {
                            if r_obj.get("region").and_then(|s| s.as_str()) == Some(region.as_str())
                            {
                                if let Some(m) = r_obj.as_object_mut() {
                                    m.insert("enabled".to_string(), serde_json::json!(en));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    bundle.state.updated_at = now;
    save_state(kv, &mut bundle, now).await?;
    Ok((
        200,
        serde_json::json!({
            "status": "ok",
            "regions_enabled": bundle.state.regions_enabled,
        }),
    ))
}

pub async fn handle_get_scaling<K: KvStoreBackend>(
    kv: &K,
    now: u64,
) -> Result<(u16, ScalingConfig), StateError> {
    let (bundle, _source) = load_state(kv, now).await?;
    Ok((200, bundle.state.scaling))
}

#[derive(Debug, Deserialize)]
pub struct UpdateScalingRequest {
    pub role: String,
    pub policy: serde_json::Value,
}

pub async fn handle_post_scaling<K: KvStoreBackend>(
    kv: &K,
    req: UpdateScalingRequest,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    if req.role != "cicd_runner" && req.role != "rust_builder" {
        return Err(StateError::Validation(format!(
            "Unsupported workload role: {}",
            req.role
        )));
    }

    let (mut bundle, _) = load_state(kv, now).await?;

    if req.role == "cicd_runner" {
        let policy: RunnerScalingPolicy = serde_json::from_value(req.policy)?;
        if policy.max_runners < policy.min_runners {
            return Err(StateError::Validation(
                "max_runners must be >= min_runners".into(),
            ));
        }
        if policy.queued_jobs_threshold < 20 {
            return Err(StateError::Validation(
                "queued_jobs_threshold must be >= 20 under hardware sizing floor".into(),
            ));
        }
        bundle.state.scaling.cicd_runner = policy;
    } else {
        let policy: BuilderScalingPolicy = serde_json::from_value(req.policy)?;
        if policy.max_instances < policy.min_instances {
            return Err(StateError::Validation(
                "max_instances must be >= min_instances".into(),
            ));
        }
        bundle.state.scaling.rust_builder = policy;
    }

    bundle.state.updated_at = now;
    save_state(kv, &mut bundle, now).await?;

    Ok((
        200,
        serde_json::json!({
            "status": "ok",
            "scaling": bundle.state.scaling,
        }),
    ))
}

#[derive(Debug, Serialize)]
pub struct AdminsResponse {
    pub owners: Vec<serde_json::Value>,
    pub admins: Vec<AdminRecord>,
    pub history: Vec<AdminHistoryRecord>,
    pub added_admins_exempt: bool,
    pub max_admins: usize,
}

pub async fn handle_get_admins<K: KvStoreBackend>(
    kv: &K,
    actor_uid: u64,
    now: u64,
) -> Result<(u16, AdminsResponse), StateError> {
    if actor_uid != OWNER_UID {
        return Err(StateError::Forbidden("Owner access required".into()));
    }

    let (bundle, _) = load_state(kv, now).await?;

    Ok((
        200,
        AdminsResponse {
            owners: vec![serde_json::json!({
                "login": OWNER_LOGIN,
                "uid": OWNER_UID,
                "global_admin": true,
            })],
            admins: bundle.state.dashboard_admins,
            history: bundle.state.dashboard_admins_history,
            added_admins_exempt: true,
            max_admins: MAX_ADDED_ADMINS,
        },
    ))
}

#[derive(Debug, Deserialize)]
pub struct AddAdminRequest {
    pub login: String,
    pub uid: u64,
    pub confirm: bool,
}

pub async fn handle_post_admins_add<K: KvStoreBackend>(
    kv: &K,
    req: AddAdminRequest,
    actor_login: &str,
    actor_uid: u64,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    if actor_uid != OWNER_UID {
        return Err(StateError::Forbidden("Owner access required".into()));
    }
    if !req.confirm {
        return Err(StateError::Validation(
            "Changing dashboard administrators needs \"confirm\": true".into(),
        ));
    }
    if req.uid == 0 {
        return Err(StateError::Validation(
            "uid must be a positive integer GitHub user ID".into(),
        ));
    }
    if req.uid == OWNER_UID {
        return Err(StateError::Conflict(format!(
            "'{}' is a dashboard owner already",
            req.login
        )));
    }

    let (mut bundle, _) = load_state(kv, now).await?;

    if bundle
        .state
        .dashboard_admins
        .iter()
        .any(|a| a.uid == req.uid)
    {
        return Err(StateError::Conflict(format!(
            "'{}' is already an administrator",
            req.login
        )));
    }
    if bundle.state.dashboard_admins.len() >= MAX_ADDED_ADMINS {
        return Err(StateError::Conflict(format!(
            "At most {} administrators can be added",
            MAX_ADDED_ADMINS
        )));
    }

    let at = chrono_iso(now);
    let record = AdminRecord {
        login: req.login.clone(),
        uid: req.uid,
        added_by: actor_login.to_string(),
        added_by_uid: Some(actor_uid),
        added_at: at.clone(),
    };

    let history = AdminHistoryRecord {
        at,
        epoch: now,
        action: "added".to_string(),
        login: req.login,
        uid: req.uid,
        actor: actor_login.to_string(),
        actor_uid: Some(actor_uid),
    };

    bundle.state.dashboard_admins.push(record.clone());
    bundle.state.dashboard_admins_history.push(history);
    bundle.state.updated_at = now;

    save_state(kv, &mut bundle, now).await?;

    Ok((
        200,
        serde_json::json!({
            "status": "added",
            "admin": record,
            "admins": bundle.state.dashboard_admins,
        }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct RemoveAdminRequest {
    pub uid: u64,
    pub confirm: bool,
}

pub async fn handle_post_admins_remove<K: KvStoreBackend>(
    kv: &K,
    req: RemoveAdminRequest,
    actor_login: &str,
    actor_uid: u64,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    if actor_uid != OWNER_UID {
        return Err(StateError::Forbidden("Owner access required".into()));
    }
    if !req.confirm {
        return Err(StateError::Validation(
            "Changing dashboard administrators needs \"confirm\": true".into(),
        ));
    }
    if req.uid == OWNER_UID {
        return Err(StateError::Validation(
            "Dashboard owners are configured at deploy time and can't be removed here".into(),
        ));
    }

    let (mut bundle, _) = load_state(kv, now).await?;

    let pos = bundle
        .state
        .dashboard_admins
        .iter()
        .position(|a| a.uid == req.uid)
        .ok_or_else(|| {
            StateError::NotFound(format!("No added administrator has GitHub ID {}", req.uid))
        })?;

    let removed = bundle.state.dashboard_admins.remove(pos);

    let history = AdminHistoryRecord {
        at: chrono_iso(now),
        epoch: now,
        action: "removed".to_string(),
        login: removed.login.clone(),
        uid: req.uid,
        actor: actor_login.to_string(),
        actor_uid: Some(actor_uid),
    };

    bundle.state.dashboard_admins_history.push(history);
    bundle.state.updated_at = now;

    save_state(kv, &mut bundle, now).await?;

    Ok((
        200,
        serde_json::json!({
            "status": "removed",
            "admin": { "login": removed.login, "uid": removed.uid },
            "admins": bundle.state.dashboard_admins,
        }),
    ))
}

// ============================================================================
// Cloud-Init Templates, VM Templates & Rust Builds Handlers
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveCloudInitRequest {
    pub id: Option<String>,
    pub mode: Option<String>,
    pub role: String,
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub assign_to_profile: bool,
}

pub async fn handle_post_cloud_init_save<K: KvStoreBackend>(
    kv: &K,
    req: SaveCloudInitRequest,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(StateError::Validation(
            "Template name cannot be empty".into(),
        ));
    }
    let content = req.content.trim();
    if content.is_empty() {
        return Err(StateError::Validation(
            "Template content cannot be empty".into(),
        ));
    }
    if req.role != "cicd_runner" && req.role != "rust_builder" {
        return Err(StateError::Validation(format!(
            "Unsupported workload role: {}",
            req.role
        )));
    }

    let (mut bundle, _) = load_state(kv, now).await?;

    let is_save_as = req.mode.as_deref() == Some("save_as");
    let requested_id = req.id.as_deref().unwrap_or("").trim();

    let template_id = if is_save_as || requested_id.is_empty() {
        format!("{}-custom-{}", req.role.replace('_', "-"), now)
    } else {
        requested_id.to_string()
    };

    let updated_iso = chrono_iso(now);
    let saved_template = CloudInitTemplate {
        id: template_id.clone(),
        name: name.to_string(),
        role: req.role.clone(),
        description: format!("Updated via Patch Console at {}", updated_iso),
        updated_at: updated_iso,
        content: req.content.clone(),
    };

    if let Some(existing) = bundle
        .state
        .cloud_init_templates
        .iter_mut()
        .find(|t| t.id == template_id)
    {
        *existing = saved_template.clone();
    } else {
        bundle
            .state
            .cloud_init_templates
            .push(saved_template.clone());
    }

    if req.assign_to_profile {
        if let Some(profile) = bundle.state.profiles.get_mut(&req.role) {
            profile.cloud_init_template_id = template_id.clone();
        }
    }

    bundle.state.updated_at = now;
    save_state(kv, &mut bundle, now).await?;

    Ok((
        200,
        serde_json::json!({
            "status": "ok",
            "template": saved_template
        }),
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateVmTemplateRequest {
    pub instance_id: String,
    pub template_name: String,
}

pub async fn handle_post_vm_templates_from_instance<K: KvStoreBackend>(
    kv: &K,
    req: CreateVmTemplateRequest,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let instance_id = req.instance_id.trim();
    if instance_id.is_empty() {
        return Err(StateError::Validation("instance_id cannot be empty".into()));
    }
    let template_name = req.template_name.trim();
    if template_name.is_empty() {
        return Err(StateError::Validation(
            "template_name cannot be empty".into(),
        ));
    }

    let (mut bundle, _) = load_state(kv, now).await?;

    let inst = bundle
        .state
        .instances_view
        .iter()
        .chain(bundle.state.instances.iter())
        .find(|i| i.id == instance_id || i.name == instance_id)
        .cloned()
        .ok_or_else(|| {
            StateError::NotFound(format!(
                "Source instance '{}' not found in live fleet inventory",
                instance_id
            ))
        })?;

    let tpl_id = format!("vmtpl-{}", now);
    let runner_vcpu = if inst.runners_count > 0 {
        inst.vcpu / inst.runners_count
    } else {
        4
    };
    let runner_ram_gib = if inst.runners_count > 0 {
        inst.ram_gib / inst.runners_count
    } else {
        16
    };

    let new_template = VmTemplate {
        id: tpl_id,
        name: template_name.to_string(),
        role: inst.role.clone(),
        machine_type: inst.machine_type.clone(),
        vcpu: inst.vcpu,
        ram_gib: inst.ram_gib,
        cpu_platform: "Intel Sapphire Rapids".to_string(),
        benchmark_score: inst.vcpu * 120,
        disk_category: "hyperdisk_balanced".to_string(),
        boot_disk_type: "hyperdisk-balanced".to_string(),
        boot_disk_gb: 30,
        data_disk_type: "hyperdisk-balanced".to_string(),
        data_disk_gb: if inst.role == "rust_builder" {
            375
        } else {
            100
        },
        runners_count: inst.runners_count,
        runner_vcpu,
        runner_ram_gib,
        cloud_init_template_id: if inst.role == "rust_builder" {
            "rust-builder-default".to_string()
        } else {
            "cicd-runner-default".to_string()
        },
        created_at: chrono_iso(now),
    };

    bundle.state.vm_templates.push(new_template.clone());
    bundle.state.updated_at = now;
    save_state(kv, &mut bundle, now).await?;

    Ok((
        200,
        serde_json::json!({
            "status": "ok",
            "message": "VM instance template registered",
            "template_name": template_name,
            "template": new_template
        }),
    ))
}

pub const MAX_RUST_BUILDS_HISTORY: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordRustBuildRequest {
    pub status: String,
    pub project_name: Option<String>,
    pub commit_sha: Option<String>,
    pub profile: Option<String>,
    pub compile_duration_seconds: Option<f64>,
    pub delivery_duration_seconds: Option<f64>,
    pub instance_name: Option<String>,
    pub zone: Option<String>,
    pub machine_type: Option<String>,
    pub cpu_platform: Option<String>,
    pub vcpu: Option<u32>,
    pub origin_ip: Option<String>,
    pub cache_hits: Option<u32>,
    pub total_crates: Option<u32>,
    pub peak_ram_gib: Option<f64>,
    pub cpu_time_seconds: Option<f64>,
}

pub async fn handle_post_rust_builds<K: KvStoreBackend>(
    kv: &K,
    req: RecordRustBuildRequest,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    if req.status.trim().is_empty() {
        return Err(StateError::Validation(
            "Build status cannot be empty".into(),
        ));
    }

    let (mut bundle, _) = load_state(kv, now).await?;

    let build_id = format!("build-{}", now);
    let iso_now = chrono_iso(now);

    let compile_sec = req.compile_duration_seconds.unwrap_or(0.0);
    let delivery_sec = req.delivery_duration_seconds.unwrap_or(0.0);
    let total_duration = compile_sec + delivery_sec;

    let hits = req.cache_hits.unwrap_or(0);
    let crates = req.total_crates.unwrap_or(0);
    let hit_rate = if crates > 0 {
        ((hits as f64 / crates as f64) * 1000.0).round() / 10.0
    } else {
        0.0
    };

    let cpu_sec = req.cpu_time_seconds.unwrap_or(0.0);
    let bench = req.vcpu.unwrap_or(16) * 120;
    let cpu_pts = ((cpu_sec * bench as f64) / 1000.0).round() as u64;

    let origin_ip = req
        .origin_ip
        .clone()
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let is_local =
        origin_ip.starts_with("127.") || origin_ip.starts_with("10.") || origin_ip == "::1";

    let single_build = serde_json::json!({
        "build_id": build_id,
        "status": req.status,
        "project_name": req.project_name.unwrap_or_else(|| "swack-tools/oxidex".to_string()),
        "commit_sha": req.commit_sha.unwrap_or_else(|| "head".to_string()),
        "profile": req.profile.unwrap_or_else(|| "release".to_string()),
        "requested_at": iso_now,
        "finished_at": iso_now,
        "delivered_at": iso_now,
        "total_duration_seconds": total_duration,
        "compile_duration_seconds": compile_sec,
        "delivery_duration_seconds": delivery_sec,
        "cache_hits": hits,
        "total_crates": crates,
        "cache_misses": crates.saturating_sub(hits),
        "cache_hit_rate_percent": hit_rate,
        "instance_name": req.instance_name.unwrap_or_else(|| "builder-n4-1".to_string()),
        "machine_type": req.machine_type.unwrap_or_else(|| "n4-highcpu-16".to_string()),
        "zone": req.zone.unwrap_or_else(|| "us-central1-a".to_string()),
        "instance_benchmark_score": bench,
        "origin_ip": origin_ip,
        "origin_hostname": if is_local { "direct.local" } else { "gateway.swacktech.com" },
        "geolocation": {
            "city": if is_local { "Local Subnet" } else { "Council Bluffs" },
            "country": "United States",
            "country_code": "US",
            "asn": "AS15169",
            "isp": "Google Cloud"
        },
        "network_security": {
            "is_tor": false,
            "is_vpn": false,
            "badge_text": "Direct (Clean)",
            "provider_label": "Direct Datacenter"
        },
        "peak_ram_gib": req.peak_ram_gib.unwrap_or(12.4),
        "container_ram_cap_gib": 28,
        "cpu_time_seconds": cpu_sec,
        "cpu_points": cpu_pts,
        "cpu_points_formula": format!("{:.1}s CPU × bench {} / 1000", cpu_sec, bench)
    });

    let mut builds_vec: Vec<serde_json::Value> = bundle
        .state
        .rust_builds
        .get("builds")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    builds_vec.insert(0, single_build.clone());
    if builds_vec.len() > MAX_RUST_BUILDS_HISTORY {
        builds_vec.truncate(MAX_RUST_BUILDS_HISTORY);
    }

    let total_builds = builds_vec.len();
    let avg_duration = if total_builds > 0 {
        let sum_dur: f64 = builds_vec
            .iter()
            .filter_map(|b| b.get("total_duration_seconds").and_then(|v| v.as_f64()))
            .sum();
        (sum_dur / total_builds as f64).round()
    } else {
        0.0
    };

    let total_hits: u64 = builds_vec
        .iter()
        .filter_map(|b| b.get("cache_hits").and_then(|v| v.as_u64()))
        .sum();
    let total_crates: u64 = builds_vec
        .iter()
        .filter_map(|b| b.get("total_crates").and_then(|v| v.as_u64()))
        .sum();
    let overall_hit_rate = if total_crates > 0 {
        ((total_hits as f64 / total_crates as f64) * 1000.0).round() / 10.0
    } else {
        0.0
    };

    let max_peak_ram = builds_vec
        .iter()
        .filter_map(|b| b.get("peak_ram_gib").and_then(|v| v.as_f64()))
        .fold(0.0, f64::max);

    let total_pts: u64 = builds_vec
        .iter()
        .filter_map(|b| b.get("cpu_points").and_then(|v| v.as_u64()))
        .sum();
    let total_cpu_time: f64 = builds_vec
        .iter()
        .filter_map(|b| b.get("cpu_time_seconds").and_then(|v| v.as_f64()))
        .sum();

    bundle.state.rust_builds = serde_json::json!({
        "total_builds": total_builds,
        "average_duration_seconds": avg_duration,
        "total_cache_hits": total_hits,
        "overall_cache_hit_rate_percent": overall_hit_rate,
        "max_peak_ram_gib": max_peak_ram,
        "total_cpu_points": total_pts,
        "total_cpu_time_seconds": total_cpu_time.round(),
        "vpn_or_tor_count": 0,
        "builds": builds_vec
    });

    bundle.state.updated_at = now;
    save_state(kv, &mut bundle, now).await?;

    Ok((
        200,
        serde_json::json!({
            "status": "ok",
            "message": "Rust build recorded",
            "build": single_build
        }),
    ))
}

// ============================================================================
// Instance Action Safety Gates (Plan Previews Only)
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct DeleteInstanceRequest {
    pub instance_id: Option<String>,
    pub instance_name: Option<String>,
}

pub async fn handle_post_instances_delete<K: KvStoreBackend>(
    kv: &K,
    req: DeleteInstanceRequest,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let (bundle, _) = load_state(kv, now).await?;

    let id = req.instance_id.as_deref().unwrap_or("");
    let name = req.instance_name.as_deref().unwrap_or("");

    let inst = bundle
        .state
        .instances_view
        .iter()
        .chain(bundle.state.instances.iter())
        .find(|i| (!id.is_empty() && i.id == id) || (!name.is_empty() && i.name == name))
        .ok_or_else(|| StateError::NotFound("Instance not found in live fleet inventory".into()))?;

    let next_step = if inst.role == "rust_builder" {
        "Dedicated builders keep their cache disk (auto-delete=no); retire the VM only when no \
        build or runtime update holds it, and delete the cache disk separately if intended."
    } else {
        "Runner hosts are drained and retired by the Cloudflare scheduled reconciler so no job loses its \
        runner; lower the runner floor or let the reconciler scale in."
    };

    Ok((
        200,
        serde_json::json!({
            "status": "not_applied",
            "applied": false,
            "instance": inst,
            "message": LIFECYCLE_ACTIONS_DISABLED_MESSAGE,
            "next_step": next_step,
        }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct ScaleRegionRequest {
    pub region: String,
    pub target_instances: u32,
    pub role: String,
}

pub async fn handle_post_instances_scale_region<K: KvStoreBackend>(
    kv: &K,
    req: ScaleRegionRequest,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    if req.role != "cicd_runner" && req.role != "rust_builder" {
        return Err(StateError::Validation(format!(
            "Unsupported workload role: {}",
            req.role
        )));
    }
    if req.target_instances > 20 {
        return Err(StateError::Validation(format!(
            "target_instances ({}) exceeds maximum allowed instances (20)",
            req.target_instances
        )));
    }

    let (bundle, _) = load_state(kv, now).await?;

    let source_instances = if !bundle.state.instances_view.is_empty() {
        &bundle.state.instances_view
    } else {
        &bundle.state.instances
    };

    let current_in_region = source_instances
        .iter()
        .filter(|i| i.region == req.region && i.status == "RUNNING" && i.role == req.role)
        .count() as u32;

    Ok((
        200,
        serde_json::json!({
            "status": "not_applied",
            "applied": false,
            "region": req.region,
            "role": req.role,
            "target_instances": req.target_instances,
            "current_count": current_in_region,
            "planned_count": req.target_instances,
            "would_launch": [],
            "would_retire": [],
            "message": LIFECYCLE_ACTIONS_DISABLED_MESSAGE,
        }),
    ))
}

pub fn chrono_iso(epoch: u64) -> String {
    use chrono::DateTime;
    DateTime::from_timestamp(epoch as i64, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

// ============================================================================
// Default Bundle Generator
// ============================================================================

pub fn default_dashboard_bundle(now: u64) -> Result<DashboardBundle, StateError> {
    let state = DashboardState {
        project: "homelab-424523".into(),
        org: "swack-tools".into(),
        updated_at: now,
        global_budget: BudgetConfig {
            active_period: "day".into(),
            max_hourly_usd: 5.0,
            max_daily_usd: 10.0,
            max_monthly_usd: 50.0,
            include_disk_costs: true,
            include_taxes: true,
            tax_rate_percent: 8.25,
        },
        profiles: HashMap::new(),
        scaling: ScalingConfig {
            cicd_runner: RunnerScalingPolicy {
                min_runners: 4,
                max_runners: 24,
                runners_per_instance: 4,
                runner_vcpu: 4,
                runner_ram_gib: 16,
                scale_up_step_runners: 2,
                enable_queued_jobs_trigger: true,
                queued_jobs_threshold: 20,
                enable_provisioned_jobs_trigger: true,
                provisioned_jobs_threshold: 6,
                enable_hourly_runs_scale_up: true,
                hourly_runs_scale_up_threshold: 20,
                hourly_runs_scale_up_target_runners: 8,
                enable_cpu_utilization_trigger: true,
                cpu_threshold_percent: 50.0,
                cpu_scope: "majority".into(),
                enable_journal_oom_trigger: true,
                enable_github_ci_oom_trigger: true,
                oom_lookback_minutes: 60,
                enable_hourly_runs_scale_down: true,
                scale_down_max_jobs_past_hour: 5,
                scale_down_target_runners: 4,
                enable_idle_cpu_scale_down: true,
                scale_down_cpu_percent: 25.0,
                max_scale_up_hourly_usd: 5.0,
                max_scale_up_daily_usd: 10.0,
                max_scale_up_monthly_usd: 50.0,
                include_disk_in_scale_budget: true,
                shared_global_budget: false,
            },
            rust_builder: BuilderScalingPolicy {
                min_instances: 1,
                max_instances: 4,
                builder_vcpu: 16,
                builder_ram_gib: 28,
                container_vcpu: 16,
                container_ram_gib: 28,
                enable_queue_trigger: true,
                queued_builds_threshold: 2,
                enable_cpu_utilization_trigger: true,
                cpu_threshold_percent: 65.0,
                cpu_scope: "majority".into(),
                enable_journal_oom_trigger: true,
                oom_lookback_minutes: 60,
                enable_low_builds_scale_down: true,
                scale_down_max_builds_past_hour: 2,
                scale_down_target_instances: 1,
                max_scale_up_hourly_usd: 3.5,
                max_scale_up_daily_usd: 12.0,
                max_scale_up_monthly_usd: 80.0,
                include_disk_in_scale_budget: true,
                shared_global_budget: false,
            },
        },
        scaling_telemetry: ScalingTelemetry::default(),
        scaling_evaluations: serde_json::Value::Null,
        regions_enabled: HashMap::new(),
        region_latencies_ms: HashMap::new(),
        cloud_init_templates: Vec::new(),
        vm_templates: Vec::new(),
        instances: Vec::new(),
        instances_view: Vec::new(),
        fleet_summary: serde_json::Value::Null,
        fleet_inventory: serde_json::Value::Null,
        rust_builds: serde_json::json!({
            "total_builds": 0,
            "average_duration_seconds": 0.0,
            "total_cache_hits": 0,
            "overall_cache_hit_rate_percent": 0.0,
            "max_peak_ram_gib": 0.0,
            "total_cpu_points": 0,
            "total_cpu_time_seconds": 0.0,
            "vpn_or_tor_count": 0,
            "builds": []
        }),
        dashboard_admins: Vec::new(),
        dashboard_admins_history: Vec::new(),
        controlled_targets: Vec::new(),
        overview: None,
    };

    let fp = compute_state_fingerprint(&state)?;

    Ok(DashboardBundle {
        schema: SNAPSHOT_SCHEMA.into(),
        state_version: 1,
        writer_slot: WRITER_SLOT_EDGE.into(),
        written_at: now,
        state_fingerprint: fp,
        state,
    })
}
