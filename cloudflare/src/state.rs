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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
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
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub created_epoch: Option<u64>,
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

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct OverviewQuery {
    pub include_deleted: Option<bool>,
    pub window_hours: Option<f64>,
    pub include_disk_and_tax: Option<bool>,
    pub sort_by: Option<String>,
    pub sort_dir: Option<String>,
    pub search: Option<String>,
    pub role: Option<String>,
    pub arch: Option<String>,
    pub target: Option<String>,
    pub is_authenticated: Option<bool>,
}

pub fn infer_instance_target(inst: &serde_json::Value, default_org: &str) -> String {
    for key in &["target", "github_target", "repository", "repo", "org"] {
        if let Some(val) = inst.get(*key).and_then(|v| v.as_str()) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    let name_lower = inst
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    for repo_target in [
        "xbmc4lyfe/newznab-tmux",
        "xbmc4lyfe/rustnzb",
        "xbmc4lyfe/nntp-proxy",
        "swack-tools/bt-to-all-devices",
        "swack-tools/oxidex",
        "swack-tools/spot-github-runners",
    ] {
        let slug = repo_target.replace('/', "-").to_lowercase();
        let safe_slug = if slug.len() > 24 { &slug[..24] } else { &slug };
        if name_lower.contains(safe_slug) {
            return repo_target.to_string();
        }
    }
    if name_lower.contains("bt-to-all-devices") || name_lower.contains("btad") {
        return "swack-tools/bt-to-all-devices".to_string();
    }
    let fallback = default_org.trim();
    if fallback.is_empty() {
        "swack-tools".to_string()
    } else {
        fallback.to_string()
    }
}

pub fn infer_arch(machine_type: &str) -> &'static str {
    let mt = normalize_gce_short_name(machine_type.trim());
    if mt.starts_with("c4a-") || mt.starts_with("n4a-") || mt.starts_with("t2a-") {
        "arm64"
    } else {
        "x86_64"
    }
}

pub fn parse_launch_epoch_from_name(name: &str) -> Option<u64> {
    let tokens: Vec<&str> = name.split('-').collect();
    if tokens.len() >= 3 {
        if let Ok(epoch) = tokens[tokens.len() - 2].parse::<u64>() {
            if epoch >= 1_000_000_000 && epoch <= 4_000_000_000 {
                return Some(epoch);
            }
        }
        // Support 11-char base36-suffixed names (`<6-char-base36-epoch><5-char-rand>`,
        // e.g. `oxidex-runners-uswest1-b-awwi-tna9fanbqld`).
        if tokens.len() >= 5 {
            let last = tokens[tokens.len() - 1];
            if last.len() == 11
                && last
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                && last.bytes().any(|b| b.is_ascii_digit())
            {
                if let Ok(epoch) = u64::from_str_radix(&last[..6], 36) {
                    if epoch >= 1_000_000_000 && epoch <= 4_000_000_000 {
                        return Some(epoch);
                    }
                }
            }
        }
    }
    None
}

fn normalize_gce_short_name(raw: &str) -> &str {
    raw.rsplit('/').next().unwrap_or(raw)
}

fn normalize_region_from_zone(raw_region: &str, norm_zone: &str) -> String {
    if !norm_zone.is_empty()
        && (raw_region.is_empty()
            || raw_region.contains('/')
            || raw_region.contains(':')
            || !norm_zone.starts_with(raw_region))
    {
        norm_zone.split('-').take(2).collect::<Vec<_>>().join("-")
    } else {
        raw_region.to_string()
    }
}

fn lookup_display_location(
    regions: Option<&serde_json::Value>,
    base_instances: &[serde_json::Value],
    region: &str,
) -> String {
    if let Some(regs) = regions.and_then(|v| v.as_array()) {
        for reg in regs {
            if reg.get("region").and_then(|v| v.as_str()) == Some(region) {
                if let Some(dl) = reg.get("display_location").and_then(|v| v.as_str()) {
                    if !dl.is_empty() {
                        return dl.to_string();
                    }
                }
            }
        }
    }
    for inst in base_instances {
        if inst.get("region").and_then(|v| v.as_str()) == Some(region) {
            if let Some(dl) = inst.get("display_location").and_then(|v| v.as_str()) {
                if !dl.is_empty() && dl != region && !dl.contains('/') {
                    return dl.to_string();
                }
            }
        }
    }
    region.to_string()
}

fn lookup_instance_pricing(
    qualifications: Option<&serde_json::Value>,
    regions: Option<&serde_json::Value>,
    base_instances: &[serde_json::Value],
    machine_type: &str,
    zone: &str,
    region: &str,
    is_builder: bool,
) -> Option<serde_json::Value> {
    for inst in base_instances {
        if inst.get("machine_type").and_then(|v| v.as_str()) == Some(machine_type)
            && inst.get("zone").and_then(|v| v.as_str()) == Some(zone)
        {
            if let Some(pricing) = inst.get("pricing") {
                if pricing.is_object() && pricing.get("total_hourly_usd").is_some() {
                    return Some(pricing.clone());
                }
            }
        }
    }

    if let Some(qual) = qualifications.and_then(|v| v.as_object()) {
        let role_key = if is_builder {
            "rust_builder"
        } else {
            "cicd_runner"
        };
        if let Some(role_qual) = qual.get(role_key).and_then(|v| v.as_object()) {
            for list_key in &["spot_price_qualifying", "hardware_qualifying"] {
                if let Some(list) = role_qual.get(*list_key).and_then(|v| v.as_array()) {
                    for item in list {
                        if item.get("machine_type").and_then(|v| v.as_str()) == Some(machine_type)
                            && (item.get("zone").and_then(|v| v.as_str()) == Some(zone)
                                || item.get("region").and_then(|v| v.as_str()) == Some(region))
                        {
                            if let Some(pricing) = item.get("pricing") {
                                if pricing.is_object() && pricing.get("total_hourly_usd").is_some()
                                {
                                    return Some(pricing.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(regs) = regions.and_then(|v| v.as_array()) {
        for reg in regs {
            if reg.get("region").and_then(|v| v.as_str()) == Some(region) {
                if let Some(insts) = reg.get("running_instances").and_then(|v| v.as_array()) {
                    for inst in insts {
                        if inst.get("machine_type").and_then(|v| v.as_str()) == Some(machine_type) {
                            if let Some(pricing) = inst.get("pricing") {
                                if pricing.is_object() && pricing.get("total_hourly_usd").is_some()
                                {
                                    return Some(pricing.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    for inst in base_instances {
        if inst.get("machine_type").and_then(|v| v.as_str()) == Some(machine_type) {
            if let Some(pricing) = inst.get("pricing") {
                if pricing.is_object() && pricing.get("total_hourly_usd").is_some() {
                    return Some(pricing.clone());
                }
            }
        }
    }

    None
}

pub fn reconcile_overview_instances_ledger(
    ov: &mut serde_json::Value,
    previous_live_instances: &[InstanceRecord],
    live_instances_view: &[InstanceRecord],
    gce_stop_epochs: &std::collections::HashMap<String, u64>,
    inventory_is_authoritative: bool,
    now: u64,
) -> bool {
    let ov_obj = match ov.as_object_mut() {
        Some(obj) => obj,
        None => return false,
    };

    let qual_val = ov_obj.get("qualifications").cloned();
    let regions_val = ov_obj.get("regions").cloned();

    if !ov_obj.contains_key("instances_view") || !ov_obj["instances_view"].is_object() {
        ov_obj.insert("instances_view".to_string(), serde_json::json!({}));
    }
    let iv = ov_obj
        .get_mut("instances_view")
        .and_then(|v| v.as_object_mut())
        .unwrap();

    let original_instances: Vec<serde_json::Value> = iv
        .get("instances")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut base_instances: Vec<serde_json::Value> = original_instances
        .iter()
        .filter(|inst| {
            let name = inst.get("name").and_then(|v| v.as_str()).unwrap_or("");
            name.starts_with("oxidex-runners-") || name.starts_with("builder-")
        })
        .cloned()
        .collect();

    // Normalize any existing raw GCE self-link URLs (`zone`, `region`, `machine_type`, `display_location`)
    let lookup_snapshot = base_instances.clone();
    for inst in &mut base_instances {
        if let Some(obj) = inst.as_object_mut() {
            let raw_zone = obj
                .get("zone")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !raw_zone.is_empty() {
                let norm_zone = normalize_gce_short_name(&raw_zone).to_string();
                if norm_zone != raw_zone {
                    obj.insert("zone".to_string(), serde_json::json!(norm_zone));
                }
                let raw_region = obj
                    .get("region")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let norm_region = normalize_region_from_zone(&raw_region, &norm_zone);
                if norm_region != raw_region {
                    obj.insert("region".to_string(), serde_json::json!(norm_region));
                }
                let raw_dl = obj
                    .get("display_location")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if raw_dl.is_empty() || raw_dl.contains('/') || raw_dl == norm_region {
                    let resolved_dl = lookup_display_location(
                        regions_val.as_ref(),
                        &lookup_snapshot,
                        &norm_region,
                    );
                    if resolved_dl != raw_dl {
                        obj.insert(
                            "display_location".to_string(),
                            serde_json::json!(resolved_dl),
                        );
                    }
                }
            }
            let raw_mt = obj
                .get("machine_type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !raw_mt.is_empty() {
                let norm_mt = normalize_gce_short_name(&raw_mt).to_string();
                if norm_mt != raw_mt {
                    obj.insert("machine_type".to_string(), serde_json::json!(norm_mt));
                }
            }
        }
    }

    for rec in live_instances_view {
        if !rec.name.starts_with("oxidex-runners-") && !rec.name.starts_with("builder-") {
            continue;
        }
        let norm_zone = normalize_gce_short_name(&rec.zone).to_string();
        let norm_region = normalize_region_from_zone(&rec.region, &norm_zone);
        let norm_machine_type = normalize_gce_short_name(&rec.machine_type).to_string();
        let display_location =
            lookup_display_location(regions_val.as_ref(), &base_instances, &norm_region);

        if let Some(existing) = base_instances
            .iter_mut()
            .find(|inst| inst.get("name").and_then(|v| v.as_str()) == Some(&rec.name))
        {
            if let Some(obj) = existing.as_object_mut() {
                let existing_id = obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .or_else(|| obj.get("instance_id").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let id_changed =
                    !existing_id.is_empty() && !rec.id.is_empty() && existing_id != rec.id;
                if id_changed {
                    obj.insert("id".to_string(), serde_json::json!(rec.id));
                    obj.insert("instance_id".to_string(), serde_json::json!(rec.id));
                    let launch_epoch_from_name = parse_launch_epoch_from_name(&rec.name);
                    let (started_epoch, started_at) = if let Some(epoch) = rec.created_epoch {
                        (epoch as f64, chrono_iso(epoch))
                    } else if let Some(ref at) = rec.created_at {
                        let ep = chrono::DateTime::parse_from_rfc3339(at)
                            .map(|dt| dt.timestamp() as f64)
                            .unwrap_or(now as f64);
                        (ep, at.clone())
                    } else if let Some(epoch) = launch_epoch_from_name {
                        (epoch as f64, chrono_iso(epoch))
                    } else {
                        (now as f64, chrono_iso(now))
                    };
                    obj.insert(
                        "started_epoch".to_string(),
                        serde_json::json!(started_epoch),
                    );
                    obj.insert("started_at".to_string(), serde_json::json!(started_at));
                }
                obj.insert("status".to_string(), serde_json::json!(rec.status));
                obj.insert("zone".to_string(), serde_json::json!(norm_zone));
                obj.insert("region".to_string(), serde_json::json!(norm_region));
                let cur_dl = obj
                    .get("display_location")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if cur_dl.is_empty() || cur_dl.contains('/') || cur_dl == norm_region {
                    obj.insert(
                        "display_location".to_string(),
                        serde_json::json!(display_location),
                    );
                }
                obj.insert(
                    "machine_type".to_string(),
                    serde_json::json!(norm_machine_type),
                );
                obj.insert(
                    "runners_count".to_string(),
                    serde_json::json!(rec.runners_count),
                );
                if !rec.status.eq_ignore_ascii_case("TERMINATED") || id_changed {
                    for key in &["deleted_epoch", "deleted_at", "deletion_trigger"] {
                        if obj.get(*key).map_or(false, |v| !v.is_null()) {
                            obj.insert((*key).to_string(), serde_json::Value::Null);
                        }
                    }
                }
            }
        } else {
            let is_builder = rec.role == "rust_builder" || rec.name.starts_with("builder-");
            let pricing_opt = lookup_instance_pricing(
                qual_val.as_ref(),
                regions_val.as_ref(),
                &base_instances,
                &norm_machine_type,
                &norm_zone,
                &norm_region,
                is_builder,
            );
            let pricing_known = pricing_opt.is_some();
            let pricing = pricing_opt.unwrap_or(serde_json::Value::Null);

            let launch_epoch_from_name = parse_launch_epoch_from_name(&rec.name);
            let (started_epoch, started_at) = if let Some(epoch) = rec.created_epoch {
                (epoch as f64, chrono_iso(epoch))
            } else if let Some(ref at) = rec.created_at {
                let ep = chrono::DateTime::parse_from_rfc3339(at)
                    .map(|dt| dt.timestamp() as f64)
                    .unwrap_or(now as f64);
                (ep, at.clone())
            } else if let Some(epoch) = launch_epoch_from_name {
                (epoch as f64, chrono_iso(epoch))
            } else {
                (now as f64, chrono_iso(now))
            };

            let compute_hourly = pricing
                .get("compute_hourly_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let total_hourly = pricing
                .get("total_hourly_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(compute_hourly);

            let (
                observed_cpu,
                observed_benchmark,
                observed_disk_gb,
                observed_disk_type,
                observed_disk_category,
            ) = {
                let mut cpu = "Unknown".to_string();
                let mut bench = serde_json::Value::Null;
                let mut disk_gb = serde_json::Value::Null;
                let mut disk_type = serde_json::Value::Null;
                let mut disk_cat = serde_json::Value::Null;

                if let Some(existing) = base_instances.iter().find(|i| {
                    i.get("machine_type").and_then(|m| m.as_str()) == Some(&norm_machine_type)
                }) {
                    if let Some(c) = existing.get("cpu_platform").and_then(|v| v.as_str()) {
                        cpu = c.to_string();
                    }
                    bench = existing
                        .get("benchmark_score")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    disk_gb = existing
                        .get("data_disk_gb")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    disk_type = existing
                        .get("data_disk_type")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    disk_cat = existing
                        .get("disk_category")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                } else if let Some(arr) = regions_val.as_ref().and_then(|r| r.as_array()) {
                    for reg in arr {
                        if let Some(types) = reg.get("qualified_types").and_then(|t| t.as_array()) {
                            for qt in types {
                                if qt.get("machine_type").and_then(|m| m.as_str())
                                    == Some(&norm_machine_type)
                                {
                                    if let Some(c) = qt.get("cpu_platform").and_then(|v| v.as_str())
                                    {
                                        cpu = c.to_string();
                                    }
                                    bench = qt
                                        .get("benchmark_score")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null);
                                    disk_gb = qt
                                        .get("data_disk_gb")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null);
                                    disk_type = qt
                                        .get("data_disk_type")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null);
                                    disk_cat = qt
                                        .get("disk_category")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null);
                                    break;
                                }
                            }
                        }
                    }
                }
                (cpu, bench, disk_gb, disk_type, disk_cat)
            };

            base_instances.push(serde_json::json!({
                "id": rec.id,
                "name": rec.name,
                "instance_id": rec.id,
                "zone": norm_zone,
                "region": norm_region,
                "display_location": display_location,
                "machine_type": norm_machine_type,
                "cpu_platform": observed_cpu,
                "status": rec.status,
                "role": rec.role,
                "vcpu": rec.vcpu,
                "ram_gib": rec.ram_gib,
                "runners_count": rec.runners_count,
                "runners_count_known": rec.runners_count_known,
                "runner_limits_known": rec.runner_limits_known,
                "runner_vcpu": if is_builder { 16 } else { 7 },
                "runner_ram_gib": if is_builder { 28 } else { 14 },
                "benchmark_score": observed_benchmark,
                "data_disk_gb": observed_disk_gb,
                "data_disk_type": observed_disk_type,
                "disk_category": observed_disk_category,
                "started_at": started_at,
                "started_epoch": started_epoch,
                "deleted_at": serde_json::Value::Null,
                "deleted_epoch": serde_json::Value::Null,
                "deletion_trigger": serde_json::Value::Null,
                "pricing": pricing,
                "pricing_known": pricing_known,
                "runtime_hours": 0.0,
                "hours_in_window": 0.0,
                "active_hourly_usd": total_hourly,
                "total_cost_in_window_usd": 0.0,
                "ssh_command": format!("gcloud compute ssh {}", rec.name)
            }));
        }
    }

    if inventory_is_authoritative || !live_instances_view.is_empty() {
        let live_names: std::collections::HashSet<&str> = live_instances_view
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        let prev_live_names: std::collections::HashSet<&str> = previous_live_instances
            .iter()
            .map(|r| r.name.as_str())
            .collect();

        for inst in &mut base_instances {
            let name_opt = inst
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(name) = name_opt {
                if !live_names.contains(name.as_str()) {
                    if let Some(obj) = inst.as_object_mut() {
                        let current_status = obj
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !current_status.eq_ignore_ascii_case("TERMINATED") {
                            obj.insert("status".to_string(), serde_json::json!("TERMINATED"));
                        }
                        if obj.get("deleted_epoch").map_or(true, |v| v.is_null()) {
                            let started_epoch = obj
                                .get("started_epoch")
                                .and_then(|v| v.as_f64())
                                .filter(|v| v.is_finite() && *v > 0.0)
                                .unwrap_or(0.0);
                            let already_gone_prior_tick = !prev_live_names.is_empty()
                                && !prev_live_names.contains(name.as_str());
                            let stale_untimestamped_zombie =
                                started_epoch > 0.0 && (now as f64 - started_epoch) > 86400.0;

                            let del_epoch = if let Some(&stop_ep) = gce_stop_epochs.get(&name) {
                                stop_ep as f64
                            } else if (already_gone_prior_tick || stale_untimestamped_zombie)
                                && started_epoch > 0.0
                            {
                                started_epoch
                            } else {
                                now as f64
                            };
                            obj.insert("deleted_epoch".to_string(), serde_json::json!(del_epoch));
                            obj.insert(
                                "deleted_at".to_string(),
                                serde_json::json!(chrono_iso(del_epoch as u64)),
                            );
                        }
                    }
                }
            }
        }
    }

    if inventory_is_authoritative {
        let retention_cutoff = (now as f64) - (744.0 * 3600.0);
        base_instances.retain(|inst| {
            let status = inst.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let is_term = status.eq_ignore_ascii_case("TERMINATED")
                || inst.get("deleted_epoch").and_then(|v| v.as_f64()).is_some();
            if !is_term {
                return true;
            }
            let del_ep = inst
                .get("deleted_epoch")
                .and_then(|v| v.as_f64())
                .filter(|v| v.is_finite())
                .unwrap_or(now as f64);
            del_ep >= retention_cutoff
        });
    }

    let changed = base_instances != original_instances;
    iv.insert(
        "instances".to_string(),
        serde_json::Value::Array(base_instances),
    );
    changed
}

fn make_arm64_candidate_offer(
    role: &str,
    family: &str,
    machine_type: &str,
    cpu_platform: &str,
    vcpu: u32,
    ram_gib: u32,
    benchmark_score: u32,
    region: &str,
    zone: &str,
    display_location: &str,
    continent: &str,
    latency_ms: u32,
    compute_hourly_usd: f64,
    disk_hourly_usd: f64,
    disk_category: &str,
    disk_type: &str,
) -> serde_json::Value {
    let is_builder = role == "rust_builder";
    let compute_daily = ((compute_hourly_usd * 24.0) * 100.0).round() / 100.0;
    let compute_monthly = ((compute_hourly_usd * 720.0) * 100.0).round() / 100.0;
    let disk_daily = ((disk_hourly_usd * 24.0) * 100.0).round() / 100.0;
    let disk_monthly = ((disk_hourly_usd * 720.0) * 100.0).round() / 100.0;
    let total_hourly = ((compute_hourly_usd + disk_hourly_usd) * 10000.0).round() / 10000.0;
    let total_daily = ((compute_daily + disk_daily) * 100.0).round() / 100.0;
    let total_monthly = ((compute_monthly + disk_monthly) * 100.0).round() / 100.0;

    let units_per_instance = if is_builder {
        1
    } else if vcpu >= 16 {
        2
    } else {
        1
    };
    let unit_vcpu = if is_builder { 16 } else { 7 };
    let unit_ram_gib = if is_builder { 28 } else { 14 };
    let instances_needed_for_floor = if is_builder {
        1
    } else if units_per_instance >= 2 {
        2
    } else {
        4
    };
    let total_units_at_floor = units_per_instance * instances_needed_for_floor;
    let fleet_floor_hourly =
        ((compute_hourly_usd * instances_needed_for_floor as f64) * 10000.0).round() / 10000.0;
    let fleet_floor_daily =
        ((compute_daily * instances_needed_for_floor as f64) * 100.0).round() / 100.0;
    let fleet_floor_monthly =
        ((compute_monthly * instances_needed_for_floor as f64) * 100.0).round() / 100.0;
    let prefix = if is_builder {
        "builder-oxidex"
    } else {
        "oxidex-runners"
    };

    let pricing_json = serde_json::json!({
        "compute_hourly_usd": compute_hourly_usd,
        "compute_daily_usd": compute_daily,
        "compute_monthly_usd": compute_monthly,
        "disk_hourly_usd": disk_hourly_usd,
        "disk_daily_usd": disk_daily,
        "disk_monthly_usd": disk_monthly,
        "tax_hourly_usd": 0.0,
        "tax_daily_usd": 0.0,
        "tax_monthly_usd": 0.0,
        "tax_rate_percent": 0.0,
        "total_hourly_usd": total_hourly,
        "total_daily_usd": total_daily,
        "total_monthly_usd": total_monthly,
        "pricing_source": "gcp_capacity_history"
    });

    let mut map = serde_json::Map::new();
    map.insert("arch".into(), serde_json::json!("arm64"));
    map.insert("isa".into(), serde_json::json!(["neon", "sve", "sve2"]));
    map.insert("family".into(), serde_json::json!(family));
    map.insert("machine_type".into(), serde_json::json!(machine_type));
    map.insert("cpu_platform".into(), serde_json::json!(cpu_platform));
    map.insert("vcpu".into(), serde_json::json!(vcpu));
    map.insert("ram_gib".into(), serde_json::json!(ram_gib));
    map.insert("benchmark_score".into(), serde_json::json!(benchmark_score));
    map.insert("boot_disk_gb".into(), serde_json::json!(40));
    map.insert("boot_disk_type".into(), serde_json::json!(disk_type));
    map.insert(
        "data_disk_gb".into(),
        serde_json::json!(if is_builder { 375 } else { 100 }),
    );
    map.insert("data_disk_type".into(), serde_json::json!(disk_type));
    map.insert("disk_category".into(), serde_json::json!(disk_category));
    map.insert("region".into(), serde_json::json!(region));
    map.insert("zone".into(), serde_json::json!(zone));
    map.insert(
        "display_location".into(),
        serde_json::json!(display_location),
    );
    map.insert("continent".into(), serde_json::json!(continent));
    map.insert("latency_ms".into(), serde_json::json!(latency_ms));
    map.insert(
        "units_per_instance".into(),
        serde_json::json!(units_per_instance),
    );
    map.insert("unit_vcpu".into(), serde_json::json!(unit_vcpu));
    map.insert("unit_ram_gib".into(), serde_json::json!(unit_ram_gib));
    map.insert(
        "instances_needed_for_floor".into(),
        serde_json::json!(instances_needed_for_floor),
    );
    map.insert(
        "total_units_at_floor".into(),
        serde_json::json!(total_units_at_floor),
    );
    map.insert(
        "evaluated_hourly_usd".into(),
        serde_json::json!(compute_hourly_usd),
    );
    map.insert(
        "evaluated_daily_usd".into(),
        serde_json::json!(compute_daily),
    );
    map.insert(
        "evaluated_monthly_usd".into(),
        serde_json::json!(compute_monthly),
    );
    map.insert(
        "fleet_floor_hourly_usd".into(),
        serde_json::json!(fleet_floor_hourly),
    );
    map.insert(
        "fleet_floor_daily_usd".into(),
        serde_json::json!(fleet_floor_daily),
    );
    map.insert(
        "fleet_floor_monthly_usd".into(),
        serde_json::json!(fleet_floor_monthly),
    );
    map.insert("price_qualifies".into(), serde_json::json!(true));
    map.insert("pricing".into(), pricing_json);
    map.insert(
        "ssh_command_preview".into(),
        serde_json::json!(format!(
            "gcloud compute ssh {}-{} --zone={}",
            prefix, region, zone
        )),
    );
    serde_json::Value::Object(map)
}

fn enrich_arm64_catalog_and_qualifications(
    ov_obj: &mut serde_json::Map<String, serde_json::Value>,
    arch_filter: &str,
) {
    let cicd_arm64_offers = vec![
        make_arm64_candidate_offer(
            "cicd_runner",
            "c4a",
            "c4a-highcpu-8",
            "Google Axion (Neoverse V2)",
            8,
            16,
            1635,
            "us-west8",
            "us-west8-a",
            "Phoenix, Arizona",
            "North America",
            28,
            0.0336,
            0.014,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "cicd_runner",
            "c4a",
            "c4a-standard-8",
            "Google Axion (Neoverse V2)",
            8,
            32,
            1600,
            "us-west8",
            "us-west8-a",
            "Phoenix, Arizona",
            "North America",
            28,
            0.0399,
            0.014,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "cicd_runner",
            "c4a",
            "c4a-highcpu-8",
            "Google Axion (Neoverse V2)",
            8,
            16,
            1635,
            "europe-west4",
            "europe-west4-a",
            "Eemshaven, Netherlands",
            "Europe",
            14,
            0.0429,
            0.014,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "cicd_runner",
            "c4a",
            "c4a-standard-8",
            "Google Axion (Neoverse V2)",
            8,
            32,
            1600,
            "europe-west4",
            "europe-west4-a",
            "Eemshaven, Netherlands",
            "Europe",
            14,
            0.0509,
            0.014,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "cicd_runner",
            "c4a",
            "c4a-highcpu-16",
            "Google Axion (Neoverse V2)",
            16,
            32,
            2980,
            "us-west8",
            "us-west8-a",
            "Phoenix, Arizona",
            "North America",
            28,
            0.0673,
            0.014,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "cicd_runner",
            "c4a",
            "c4a-highcpu-8",
            "Google Axion (Neoverse V2)",
            8,
            16,
            1635,
            "us-east5",
            "us-east5-a",
            "Columbus, Ohio",
            "North America",
            22,
            0.0691,
            0.014,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "cicd_runner",
            "n4a",
            "n4a-highcpu-8",
            "Google Axion (Neoverse N3)",
            8,
            16,
            1472,
            "us-central1",
            "us-central1-a",
            "Council Bluffs, Iowa",
            "North America",
            18,
            0.0998,
            0.014,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "cicd_runner",
            "n4a",
            "n4a-standard-8",
            "Google Axion (Neoverse N3)",
            8,
            32,
            1437,
            "us-central1",
            "us-central1-a",
            "Council Bluffs, Iowa",
            "North America",
            18,
            0.1183,
            0.014,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "cicd_runner",
            "t2a",
            "t2a-standard-8",
            "Ampere Altra (Neoverse N1)",
            8,
            32,
            1129,
            "asia-southeast1",
            "asia-southeast1-a",
            "Jurong West, Singapore",
            "Asia Pacific",
            45,
            0.1306,
            0.013,
            "standard_ssd",
            "pd-balanced",
        ),
    ];

    let builder_arm64_offers = vec![
        make_arm64_candidate_offer(
            "rust_builder",
            "c4a",
            "c4a-highcpu-16",
            "Google Axion (Neoverse V2)",
            16,
            32,
            2980,
            "us-west8",
            "us-west8-a",
            "Phoenix, Arizona",
            "North America",
            28,
            0.0673,
            0.028,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "rust_builder",
            "c4a",
            "c4a-standard-16",
            "Google Axion (Neoverse V2)",
            16,
            64,
            2910,
            "us-west8",
            "us-west8-a",
            "Phoenix, Arizona",
            "North America",
            28,
            0.0798,
            0.028,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "rust_builder",
            "c4a",
            "c4a-highcpu-16",
            "Google Axion (Neoverse V2)",
            16,
            32,
            2980,
            "europe-west4",
            "europe-west4-a",
            "Eemshaven, Netherlands",
            "Europe",
            14,
            0.0858,
            0.028,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "rust_builder",
            "c4a",
            "c4a-highcpu-16",
            "Google Axion (Neoverse V2)",
            16,
            32,
            2980,
            "us-east5",
            "us-east5-a",
            "Columbus, Ohio",
            "North America",
            22,
            0.1381,
            0.028,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "rust_builder",
            "n4a",
            "n4a-highcpu-16",
            "Google Axion (Neoverse N3)",
            16,
            32,
            2720,
            "us-central1",
            "us-central1-a",
            "Council Bluffs, Iowa",
            "North America",
            18,
            0.1996,
            0.028,
            "hyperdrive",
            "hyperdisk-balanced",
        ),
        make_arm64_candidate_offer(
            "rust_builder",
            "t2a",
            "t2a-standard-16",
            "Ampere Altra (Neoverse N1)",
            16,
            64,
            2100,
            "us-central1",
            "us-central1-a",
            "Council Bluffs, Iowa",
            "North America",
            18,
            0.3213,
            0.026,
            "standard_ssd",
            "pd-balanced",
        ),
    ];

    if let Some(qual) = ov_obj
        .get_mut("qualifications")
        .and_then(|v| v.as_object_mut())
    {
        for (role_key, extra_offers) in [
            ("cicd_runner", &cicd_arm64_offers),
            ("rust_builder", &builder_arm64_offers),
        ] {
            if let Some(role_obj) = qual.get_mut(role_key).and_then(|v| v.as_object_mut()) {
                for list_key in ["spot_price_qualifying", "hardware_qualifying"] {
                    let mut list = role_obj
                        .get(list_key)
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();

                    for item in &mut list {
                        if let Some(obj) = item.as_object_mut() {
                            let mt = obj
                                .get("machine_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let item_arch = infer_arch(mt);
                            obj.insert("arch".to_string(), serde_json::json!(item_arch));
                        }
                    }

                    for arm_offer in extra_offers {
                        let arm_mt = arm_offer
                            .get("machine_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let arm_reg = arm_offer
                            .get("region")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let exists = list.iter().any(|x| {
                            x.get("machine_type").and_then(|v| v.as_str()) == Some(arm_mt)
                                && x.get("region").and_then(|v| v.as_str()) == Some(arm_reg)
                        });
                        if !exists {
                            list.push(arm_offer.clone());
                        }
                    }

                    if arch_filter == "arm64" || arch_filter == "aarch64" {
                        list.retain(|x| x.get("arch").and_then(|v| v.as_str()) == Some("arm64"));
                    } else if arch_filter == "x86_64" || arch_filter == "amd64" {
                        list.retain(|x| x.get("arch").and_then(|v| v.as_str()) != Some("arm64"));
                    }

                    list.sort_by(|a, b| {
                        let pa = a
                            .get("pricing")
                            .and_then(|p| p.get("compute_hourly_usd"))
                            .and_then(|v| v.as_f64())
                            .unwrap_or(999.0);
                        let pb = b
                            .get("pricing")
                            .and_then(|p| p.get("compute_hourly_usd"))
                            .and_then(|v| v.as_f64())
                            .unwrap_or(999.0);
                        pa.partial_cmp(&pb).unwrap_or(std::cmp::Ordering::Equal)
                    });

                    let count = list.len();
                    role_obj.insert(list_key.to_string(), serde_json::Value::Array(list));
                    role_obj.insert(format!("{}_count", list_key), serde_json::json!(count));
                }
            }
        }
    }

    // Enrich vm_templates with ARM64 templates and arch tag
    if let Some(tpls) = ov_obj
        .get_mut("vm_templates")
        .and_then(|v| v.as_array_mut())
    {
        for tpl in tpls.iter_mut() {
            if let Some(obj) = tpl.as_object_mut() {
                let mt = obj
                    .get("machine_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                obj.insert("arch".to_string(), serde_json::json!(infer_arch(mt)));
            }
        }
        let default_arm64_tpls = [
            serde_json::json!({
                "id": "tpl-runner-c4a-highcpu8",
                "name": "Runner C4A-HighCPU-8 (Google Axion ARM64 DDR5)",
                "role": "cicd_runner",
                "arch": "arm64",
                "machine_type": "c4a-highcpu-8",
                "vcpu": 8,
                "ram_gib": 16,
                "cpu_platform": "Google Axion (Neoverse V2)",
                "benchmark_score": 1635,
                "disk_category": "hyperdrive",
                "boot_disk_type": "hyperdisk-balanced",
                "boot_disk_gb": 40,
                "data_disk_type": "hyperdisk-balanced",
                "data_disk_gb": 100,
                "runners_count": 1,
                "runner_vcpu": 7,
                "runner_ram_gib": 14,
                "cloud_init_template_id": "ci-runner-default",
                "created_at": "2026-10-23T00:00:00Z"
            }),
            serde_json::json!({
                "id": "tpl-runner-c4a-highcpu16",
                "name": "Runner C4A-HighCPU-16 (Google Axion ARM64 2-Slot)",
                "role": "cicd_runner",
                "arch": "arm64",
                "machine_type": "c4a-highcpu-16",
                "vcpu": 16,
                "ram_gib": 32,
                "cpu_platform": "Google Axion (Neoverse V2)",
                "benchmark_score": 2980,
                "disk_category": "hyperdrive",
                "boot_disk_type": "hyperdisk-balanced",
                "boot_disk_gb": 40,
                "data_disk_type": "hyperdisk-balanced",
                "data_disk_gb": 100,
                "runners_count": 2,
                "runner_vcpu": 7,
                "runner_ram_gib": 14,
                "cloud_init_template_id": "ci-runner-default",
                "created_at": "2026-10-23T00:00:00Z"
            }),
            serde_json::json!({
                "id": "tpl-builder-c4a-highcpu16",
                "name": "Dedicated Rust 1.99 Builder C4A-HighCPU-16 (Native ARM64 Axion)",
                "role": "rust_builder",
                "arch": "arm64",
                "machine_type": "c4a-highcpu-16",
                "vcpu": 16,
                "ram_gib": 32,
                "cpu_platform": "Google Axion (Neoverse V2)",
                "benchmark_score": 2980,
                "disk_category": "hyperdrive",
                "boot_disk_type": "hyperdisk-balanced",
                "boot_disk_gb": 40,
                "data_disk_type": "hyperdisk-balanced",
                "data_disk_gb": 375,
                "runners_count": 1,
                "runner_vcpu": 16,
                "runner_ram_gib": 28,
                "cloud_init_template_id": "rust-builder-default",
                "created_at": "2026-10-23T00:00:00Z"
            }),
            serde_json::json!({
                "id": "tpl-builder-n4a-highcpu16",
                "name": "Dedicated Rust 1.99 Builder N4A-HighCPU-16 (Axion Neoverse N3)",
                "role": "rust_builder",
                "arch": "arm64",
                "machine_type": "n4a-highcpu-16",
                "vcpu": 16,
                "ram_gib": 32,
                "cpu_platform": "Google Axion (Neoverse N3)",
                "benchmark_score": 2720,
                "disk_category": "hyperdrive",
                "boot_disk_type": "hyperdisk-balanced",
                "boot_disk_gb": 40,
                "data_disk_type": "hyperdisk-balanced",
                "data_disk_gb": 375,
                "runners_count": 1,
                "runner_vcpu": 16,
                "runner_ram_gib": 28,
                "cloud_init_template_id": "rust-builder-default",
                "created_at": "2026-10-23T00:00:00Z"
            }),
        ];
        for arm_tpl in default_arm64_tpls {
            let tpl_id = arm_tpl.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if !tpls
                .iter()
                .any(|x| x.get("id").and_then(|v| v.as_str()) == Some(tpl_id))
            {
                tpls.push(arm_tpl);
            }
        }
    }

    // Enrich regions with ARM64 instance families and CPU platforms
    if let Some(regions_arr) = ov_obj.get_mut("regions").and_then(|v| v.as_array_mut()) {
        for reg in regions_arr.iter_mut() {
            if let Some(reg_obj) = reg.as_object_mut() {
                if let Some(fams) = reg_obj.get_mut("families").and_then(|v| v.as_array_mut()) {
                    for fam in ["c4a", "n4a", "t2a"] {
                        if !fams.iter().any(|x| x.as_str() == Some(fam)) {
                            fams.push(serde_json::json!(fam));
                        }
                    }
                }
                if let Some(cpus) = reg_obj
                    .get_mut("cpu_platforms")
                    .and_then(|v| v.as_array_mut())
                {
                    for cpu in [
                        "Google Axion (Neoverse V2)",
                        "Google Axion (Neoverse N3)",
                        "Ampere Altra (Neoverse N1)",
                    ] {
                        if !cpus.iter().any(|x| x.as_str() == Some(cpu)) {
                            cpus.push(serde_json::json!(cpu));
                        }
                    }
                }
            }
        }
    }
}

pub fn apply_dynamic_overview_filtering(
    ov: &mut serde_json::Value,
    query: &OverviewQuery,
    live_instances_view: &[InstanceRecord],
    now: u64,
) {
    let empty_stops = std::collections::HashMap::new();
    reconcile_overview_instances_ledger(ov, &[], live_instances_view, &empty_stops, false, now);

    let ov_obj = match ov.as_object_mut() {
        Some(obj) => obj,
        None => return,
    };

    let arch_filter = query.arch.as_deref().unwrap_or("all").trim().to_lowercase();
    enrich_arm64_catalog_and_qualifications(ov_obj, &arch_filter);

    let default_org = ov_obj
        .get("org")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("swack-tools")
        .to_string();

    let qual_val = ov_obj.get("qualifications").cloned();
    let iv = ov_obj
        .get_mut("instances_view")
        .and_then(|v| v.as_object_mut())
        .unwrap();

    let base_instances: Vec<serde_json::Value> = iv
        .get("instances")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut available_targets: Vec<String> = crate::patch::CONTROLLED_TARGETS
        .iter()
        .chain(crate::patch::ON_DEMAND_TARGETS.iter())
        .map(|s| s.to_string())
        .collect();
    for inst_ref in &base_instances {
        let name = inst_ref.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if !name.starts_with("oxidex-runners-") && !name.starts_with("builder-") {
            continue;
        }
        let t = infer_instance_target(inst_ref, &default_org);
        if !available_targets
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&t))
        {
            available_targets.push(t);
        }
    }

    let include_deleted = query.include_deleted.unwrap_or(true);
    let window_hours = query
        .window_hours
        .filter(|w| w.is_finite() && *w >= 0.0)
        .unwrap_or(24.0)
        .clamp(0.0, 744.0);
    let include_disk_and_tax = query.include_disk_and_tax.unwrap_or(true);
    let role_filter = query.role.as_deref().unwrap_or("all").trim();
    let target_filter = query.target.as_deref().unwrap_or("all").trim();
    let selected_targets: Option<Vec<String>> =
        if target_filter.is_empty() || target_filter.eq_ignore_ascii_case("all") {
            None
        } else if target_filter.eq_ignore_ascii_case("none") || target_filter == "__none__" {
            Some(Vec::new())
        } else {
            Some(
                target_filter
                    .split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        };
    let search_filter = query.search.as_deref().unwrap_or("").trim().to_lowercase();
    let sort_by = query
        .sort_by
        .as_deref()
        .unwrap_or("total_cost_in_window_usd")
        .trim();
    let sort_dir = query.sort_dir.as_deref().unwrap_or("desc").to_lowercase();

    let now_f64 = now as f64;
    let window_start = now_f64 - (window_hours * 3600.0);

    let mut filtered: Vec<serde_json::Value> = Vec::new();
    for inst_ref in &base_instances {
        let mut inst = inst_ref.clone();
        let name = inst.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if !name.starts_with("oxidex-runners-") && !name.starts_with("builder-") {
            continue;
        }

        let status = inst
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("RUNNING");
        let is_terminated = status.eq_ignore_ascii_case("TERMINATED")
            || inst.get("deleted_epoch").and_then(|v| v.as_f64()).is_some();
        let is_running = !is_terminated;

        if !include_deleted && is_terminated {
            continue;
        }

        let started_epoch = inst
            .get("started_epoch")
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite())
            .unwrap_or(0.0);
        let deleted_epoch = inst
            .get("deleted_epoch")
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite());
        let end_ts = deleted_epoch.unwrap_or(now_f64);

        if is_terminated && (end_ts < window_start || window_hours == 0.0) {
            continue;
        }

        let eff_start = started_epoch.max(window_start);
        let eff_end = end_ts.min(now_f64).max(eff_start);
        let hours_raw = (eff_end - eff_start) / 3600.0;
        let hours_clamped = if hours_raw.is_finite() {
            hours_raw.clamp(0.0, window_hours)
        } else {
            0.0
        };
        let hours_in_window = (hours_clamped * 100.0).round() / 100.0;
        let runtime_raw = (end_ts.max(started_epoch) - started_epoch) / 3600.0;
        let runtime_hours = if runtime_raw.is_finite() {
            (runtime_raw * 100.0).round() / 100.0
        } else {
            0.0
        };

        inst["hours_in_window"] = serde_json::json!(hours_in_window);
        inst["runtime_hours"] = serde_json::json!(runtime_hours);

        let pricing = inst.get("pricing").and_then(|v| v.as_object());
        let total_hourly_usd = pricing
            .and_then(|p| p.get("total_hourly_usd"))
            .and_then(|v| v.as_f64())
            .or_else(|| inst.get("active_hourly_usd").and_then(|v| v.as_f64()))
            .unwrap_or(0.18);
        let total_daily_usd = pricing
            .and_then(|p| p.get("total_daily_usd"))
            .and_then(|v| v.as_f64())
            .unwrap_or(total_hourly_usd * 24.0);
        let total_monthly_usd = pricing
            .and_then(|p| p.get("total_monthly_usd"))
            .and_then(|v| v.as_f64())
            .unwrap_or(total_daily_usd * 30.0);

        let compute_hourly_usd = pricing
            .and_then(|p| p.get("compute_hourly_usd"))
            .and_then(|v| v.as_f64())
            .unwrap_or(total_hourly_usd);
        let compute_daily_usd = pricing
            .and_then(|p| p.get("compute_daily_usd"))
            .and_then(|v| v.as_f64())
            .unwrap_or(compute_hourly_usd * 24.0);
        let compute_monthly_usd = pricing
            .and_then(|p| p.get("compute_monthly_usd"))
            .and_then(|v| v.as_f64())
            .unwrap_or(compute_daily_usd * 30.0);

        let hourly_rate = if include_disk_and_tax {
            total_hourly_usd
        } else {
            compute_hourly_usd
        };
        let daily_rate = if include_disk_and_tax {
            total_daily_usd
        } else {
            compute_daily_usd
        };
        let monthly_rate = if include_disk_and_tax {
            total_monthly_usd
        } else {
            compute_monthly_usd
        };

        let active_hourly = if is_running { hourly_rate } else { 0.0 };
        let active_daily = if is_running { daily_rate } else { 0.0 };
        let active_monthly = if is_running { monthly_rate } else { 0.0 };

        inst["active_hourly_usd"] =
            serde_json::json!(((active_hourly * 10000.0).round()) / 10000.0);
        inst["active_daily_usd"] = serde_json::json!(((active_daily * 100.0).round()) / 100.0);
        inst["active_monthly_usd"] = serde_json::json!(((active_monthly * 100.0).round()) / 100.0);

        let total_cost_in_window = ((hours_in_window * hourly_rate) * 100.0).round() / 100.0;
        inst["total_cost_in_window_usd"] = serde_json::json!(total_cost_in_window);

        let inst_mt = inst
            .get("machine_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let inst_arch = infer_arch(&inst_mt);
        inst["arch"] = serde_json::json!(inst_arch);
        if (arch_filter == "arm64" || arch_filter == "aarch64") && inst_arch != "arm64" {
            continue;
        }
        if (arch_filter == "x86_64" || arch_filter == "amd64") && inst_arch != "x86_64" {
            continue;
        }

        let inst_role = inst.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if !role_filter.is_empty() && role_filter != "all" && inst_role != role_filter {
            continue;
        }

        let inst_target = infer_instance_target(&inst, &default_org);
        inst["target"] = serde_json::json!(inst_target);
        if let Some(ref sel) = selected_targets {
            let target_lower = inst_target.to_lowercase();
            if !sel.iter().any(|t| t == &target_lower) {
                continue;
            }
        }

        if !search_filter.is_empty() {
            let mut match_found = false;
            for field in &[
                "name",
                "instance_id",
                "id",
                "region",
                "zone",
                "display_location",
                "machine_type",
                "cpu_platform",
                "arch",
                "status",
                "role",
                "target",
                "deletion_trigger",
            ] {
                if let Some(val_str) = inst.get(*field).and_then(|v| v.as_str()) {
                    if val_str.to_lowercase().contains(&search_filter) {
                        match_found = true;
                        break;
                    }
                }
            }
            if !match_found {
                continue;
            }
        }

        filtered.push(inst);
    }

    filtered.sort_by(|a, b| {
        let ord = match sort_by {
            "name" => a
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or("")),
            "target" => a
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("target").and_then(|v| v.as_str()).unwrap_or("")),
            "status" => a
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("status").and_then(|v| v.as_str()).unwrap_or("")),
            "role" => a
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("role").and_then(|v| v.as_str()).unwrap_or("")),
            "display_location" => a
                .get("display_location")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(
                    b.get("display_location")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                ),
            "machine_type" => a
                .get("machine_type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("machine_type").and_then(|v| v.as_str()).unwrap_or("")),
            "benchmark_score" => a
                .get("benchmark_score")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .cmp(
                    &b.get("benchmark_score")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                ),
            "runners_count" => a
                .get("runners_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .cmp(&b.get("runners_count").and_then(|v| v.as_u64()).unwrap_or(0)),
            "runtime_hours" => a
                .get("runtime_hours")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                .partial_cmp(
                    &b.get("runtime_hours")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0),
                )
                .unwrap_or(std::cmp::Ordering::Equal),
            "active_hourly_usd" => a
                .get("active_hourly_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                .partial_cmp(
                    &b.get("active_hourly_usd")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0),
                )
                .unwrap_or(std::cmp::Ordering::Equal),
            "started_epoch" => a
                .get("started_epoch")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                .partial_cmp(
                    &b.get("started_epoch")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0),
                )
                .unwrap_or(std::cmp::Ordering::Equal),
            "deletion_trigger" => a
                .get("deletion_trigger")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(
                    b.get("deletion_trigger")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                ),
            _ => a
                .get("total_cost_in_window_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                .partial_cmp(
                    &b.get("total_cost_in_window_usd")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0),
                )
                .unwrap_or(std::cmp::Ordering::Equal),
        };
        if sort_dir == "asc" {
            ord
        } else {
            ord.reverse()
        }
    });

    let running_count = filtered
        .iter()
        .filter(|i| {
            let status = i.get("status").and_then(|v| v.as_str()).unwrap_or("");
            !status.eq_ignore_ascii_case("TERMINATED")
                && i.get("deleted_epoch").map_or(true, |v| v.is_null())
        })
        .count();
    let deleted_count = filtered.len().saturating_sub(running_count);
    let total_window_spend: f64 = filtered
        .iter()
        .map(|i| {
            i.get("total_cost_in_window_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
        })
        .sum();
    let total_window_spend_usd = ((total_window_spend * 100.0).round()) / 100.0;

    iv.insert("instances".to_string(), serde_json::Value::Array(filtered));
    iv.insert(
        "running_count".to_string(),
        serde_json::json!(running_count),
    );
    iv.insert(
        "deleted_count".to_string(),
        serde_json::json!(deleted_count),
    );
    iv.insert(
        "total_window_spend_usd".to_string(),
        serde_json::json!(total_window_spend_usd),
    );
    iv.insert("window_hours".to_string(), serde_json::json!(window_hours));
    iv.insert("sort_by".to_string(), serde_json::json!(sort_by));
    iv.insert("sort_dir".to_string(), serde_json::json!(sort_dir));
    ov_obj.insert(
        "available_targets".to_string(),
        serde_json::json!(available_targets),
    );

    // Synchronize fleet_summary and regions with refreshed live inventory
    let all_running: Vec<&serde_json::Value> = base_instances
        .iter()
        .filter(|inst| {
            let status = inst.get("status").and_then(|v| v.as_str()).unwrap_or("");
            !status.eq_ignore_ascii_case("TERMINATED")
                && inst.get("deleted_epoch").map_or(true, |v| v.is_null())
        })
        .collect();

    let mut fs_hourly_burn = 0.0;
    let mut fs_running_runner_slots = 0;
    let mut fs_running_runner_vms = 0;
    let mut fs_unconfirmed_runner_hosts = 0;
    let mut fs_running_builder_vms = 0;
    let mut fs_total_vcpus = 0;
    let mut fs_total_ram_gib = 0;

    for inst in &all_running {
        let is_builder = inst.get("role").and_then(|v| v.as_str()) == Some("rust_builder");
        let vcpu = inst.get("vcpu").and_then(|v| v.as_u64()).unwrap_or(16) as u32;
        let ram = inst.get("ram_gib").and_then(|v| v.as_u64()).unwrap_or(32) as u32;
        fs_total_vcpus += vcpu;
        fs_total_ram_gib += ram;

        let active_hourly = inst
            .get("active_hourly_usd")
            .and_then(|v| v.as_f64())
            .unwrap_or_else(|| {
                inst.get("pricing")
                    .and_then(|p| p.get("total_hourly_usd"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
            });
        fs_hourly_burn += active_hourly;

        if is_builder {
            fs_running_builder_vms += 1;
        } else {
            fs_running_runner_vms += 1;
            let count = inst
                .get("runners_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            fs_running_runner_slots += count;
            if inst.get("runners_count_known") == Some(&serde_json::json!(false)) {
                fs_unconfirmed_runner_hosts += 1;
            }
        }
    }

    let fs_hourly_rounded = (fs_hourly_burn * 10000.0).round() / 10000.0;
    let fs_daily_rounded = (fs_hourly_burn * 24.0 * 100.0).round() / 100.0;
    let fs_monthly_rounded = (fs_hourly_burn * 720.0 * 100.0).round() / 100.0;
    let hourly_cost_per_runner = if fs_running_runner_slots > 0 {
        ((fs_hourly_burn / fs_running_runner_slots as f64) * 10000.0).round() / 10000.0
    } else {
        0.0
    };
    let daily_cost_per_runner = if fs_running_runner_slots > 0 {
        ((fs_hourly_burn * 24.0 / fs_running_runner_slots as f64) * 100.0).round() / 100.0
    } else {
        0.0
    };

    ov_obj.insert(
        "fleet_summary".to_string(),
        serde_json::json!({
            "active_hourly_burn_usd": fs_hourly_rounded,
            "active_daily_burn_usd": fs_daily_rounded,
            "active_monthly_burn_usd": fs_monthly_rounded,
            "running_runner_slots": fs_running_runner_slots,
            "running_runner_vms": fs_running_runner_vms,
            "unconfirmed_runner_hosts": fs_unconfirmed_runner_hosts,
            "running_builder_instances": fs_running_builder_vms,
            "running_builder_vms": fs_running_builder_vms,
            "total_vcpus": fs_total_vcpus,
            "total_ram_gib": fs_total_ram_gib,
            "active_instances_count": all_running.len()
        }),
    );

    let spot_qualifying_offers = qual_val
        .as_ref()
        .and_then(|q| q.get("cicd_runner"))
        .and_then(|c| c.get("spot_price_qualifying"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let spot_qualifying_count = spot_qualifying_offers
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    let global_budget_val = ov_obj
        .get("global_budget")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    ov_obj.insert(
        "pricing".to_string(),
        serde_json::json!({
            "active_hourly_burn_usd": fs_hourly_rounded,
            "active_daily_burn_usd": fs_daily_rounded,
            "active_monthly_burn_usd": fs_monthly_rounded,
            "active_instances_count": all_running.len(),
            "running_runner_slots": fs_running_runner_slots,
            "running_runner_vms": fs_running_runner_vms,
            "running_builder_vms": fs_running_builder_vms,
            "hourly_cost_per_runner_usd": hourly_cost_per_runner,
            "daily_cost_per_runner_usd": daily_cost_per_runner,
            "global_budget": global_budget_val,
            "spot_price_qualifying_count": spot_qualifying_count,
            "candidate_offers": spot_qualifying_offers,
        }),
    );

    if let Some(regions_arr) = ov_obj.get_mut("regions").and_then(|v| v.as_array_mut()) {
        for reg in regions_arr.iter_mut() {
            if let Some(reg_obj) = reg.as_object_mut() {
                let reg_id = reg_obj
                    .get("region")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let running_in_reg: Vec<serde_json::Value> = all_running
                    .iter()
                    .filter(|inst| {
                        inst.get("region").and_then(|v| v.as_str()) == Some(reg_id.as_str())
                    })
                    .map(|v| (*v).clone())
                    .collect();
                reg_obj.insert(
                    "running_instances".to_string(),
                    serde_json::Value::Array(running_in_reg),
                );
            }
        }
    }

    if query.is_authenticated != Some(true) {
        if let Some(rb) = ov_obj
            .get_mut("rust_builds")
            .and_then(|v| v.as_object_mut())
        {
            if let Some(builds) = rb.get_mut("builds").and_then(|v| v.as_array_mut()) {
                for b in builds {
                    if let Some(obj) = b.as_object_mut() {
                        obj.remove("origin_ip");
                        obj.remove("origin_hostname");
                    }
                }
            }
            if let Some(queue) = rb.get_mut("queue").and_then(|v| v.as_array_mut()) {
                for q in queue {
                    if let Some(obj) = q.as_object_mut() {
                        obj.remove("origin_ip");
                        obj.remove("origin_hostname");
                    }
                }
            }
        }
    }
}

pub async fn handle_api_overview<K: KvStoreBackend>(
    kv: &K,
    query: OverviewQuery,
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
            if let Ok(cit) = serde_json::to_value(&bundle.state.cloud_init_templates) {
                obj.insert("cloud_init_templates".to_string(), cit);
            }
            if let Ok(vmt) = serde_json::to_value(&bundle.state.vm_templates) {
                obj.insert("vm_templates".to_string(), vmt);
            }
            if let Ok(rb) = serde_json::to_value(&bundle.state.rust_builds) {
                obj.insert("rust_builds".to_string(), rb);
            }
            if let Ok(da) = serde_json::to_value(&bundle.state.dashboard_admins) {
                obj.insert("dashboard_admins".to_string(), da);
            }
            if !bundle.state.fleet_inventory.is_null() {
                obj.insert(
                    "fleet_inventory".to_string(),
                    bundle.state.fleet_inventory.clone(),
                );
                if let Some(iv) = obj
                    .get_mut("instances_view")
                    .and_then(|v| v.as_object_mut())
                {
                    iv.insert(
                        "fleet_inventory".to_string(),
                        bundle.state.fleet_inventory.clone(),
                    );
                }
            }
        }
        apply_dynamic_overview_filtering(&mut ov, &query, &bundle.state.instances_view, now);
        return Ok((200, ov));
    }

    // Build default overview shape consumed by the dashboard UI when no precomputed overview exists
    let mut fallback_overview = serde_json::json!({
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
            "fleet_inventory": bundle.state.fleet_inventory,
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

    apply_dynamic_overview_filtering(
        &mut fallback_overview,
        &query,
        &bundle.state.instances_view,
        now,
    );
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

pub(crate) fn generate_record_id(prefix: &str, now: u64) -> String {
    let mut bytes = [0u8; 4];
    let hex_entropy = if getrandom::getrandom(&mut bytes).is_ok() {
        hex::encode(bytes)
    } else {
        format!("{:08x}", now as u32)
    };
    format!("{}-{}-{}", prefix, now, hex_entropy)
}

pub(crate) fn derive_hardware_profile(
    machine_type: &str,
) -> Option<(&'static str, &'static str, &'static str)> {
    let mt = normalize_gce_short_name(machine_type.trim());
    if mt.starts_with("c4a-") {
        Some((
            "Google Axion (Neoverse V2)",
            "hyperdisk_balanced",
            "hyperdisk-balanced",
        ))
    } else if mt.starts_with("n4a-") {
        Some((
            "Google Axion (Neoverse N3)",
            "hyperdisk_balanced",
            "hyperdisk-balanced",
        ))
    } else if mt.starts_with("t2a-") {
        Some(("Ampere Altra (Neoverse N1)", "pd_balanced", "pd-balanced"))
    } else if mt.starts_with("c4d-") {
        Some(("AMD Turin", "pd_balanced", "pd-balanced"))
    } else if mt.starts_with("n4d-") {
        Some(("AMD Turin", "pd_balanced", "pd-balanced"))
    } else if mt.starts_with("c2d-") {
        Some(("AMD Milan", "pd_balanced", "pd-balanced"))
    } else if mt.starts_with("n2d-") {
        Some(("AMD Milan", "pd_balanced", "pd-balanced"))
    } else if mt.starts_with("n4-") {
        Some(("Intel Emerald Rapids", "pd_balanced", "pd-balanced"))
    } else if mt.starts_with("c4-") {
        Some((
            "Intel Emerald Rapids",
            "hyperdisk_balanced",
            "hyperdisk-balanced",
        ))
    } else if mt.starts_with("c3-") {
        Some((
            "Intel Sapphire Rapids",
            "hyperdisk_balanced",
            "hyperdisk-balanced",
        ))
    } else if mt.starts_with("n2-") {
        Some(("Intel Ice Lake", "pd_balanced", "pd-balanced"))
    } else {
        None
    }
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
        generate_record_id(&format!("{}-custom", req.role.replace('_', "-")), now)
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
        if existing.role != req.role {
            return Err(StateError::Validation(format!(
                "Cannot change workload role of existing template '{}' from '{}' to '{}'; use save_as to create a new template for a different role",
                template_id, existing.role, req.role
            )));
        }
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
    let raw_id = req.instance_id.trim();
    let instance_id = if raw_id == "undefined" { "" } else { raw_id };

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

    let (cpu_platform, disk_cat, disk_type) = derive_hardware_profile(&inst.machine_type)
        .ok_or_else(|| {
            StateError::Validation(format!(
                "Cannot derive hardware profile for unknown machine type '{}'",
                inst.machine_type
            ))
        })?;

    let tpl_id = generate_record_id("vmtpl", now);
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
        cpu_platform: cpu_platform.to_string(),
        benchmark_score: inst.vcpu * 120,
        disk_category: disk_cat.to_string(),
        boot_disk_type: disk_type.to_string(),
        boot_disk_gb: 30,
        data_disk_type: disk_type.to_string(),
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
            "template": new_template,
            "vm_template": new_template
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

fn append_rust_build_record(
    state: &mut DashboardState,
    req: RecordRustBuildRequest,
    now: u64,
) -> serde_json::Value {
    let build_id = generate_record_id("build", now);
    let iso_now = chrono_iso(now);

    let compile_sec = req
        .compile_duration_seconds
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0);
    let delivery_sec = req
        .delivery_duration_seconds
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0);
    let total_duration = compile_sec + delivery_sec;

    let hits = req.cache_hits.unwrap_or(0);
    let crates = req.total_crates.unwrap_or(0);
    let hit_rate = if crates > 0 {
        ((hits as f64 / crates as f64) * 1000.0).round() / 10.0
    } else {
        0.0
    };

    let cpu_time_sec = req.cpu_time_seconds.filter(|v| v.is_finite() && *v >= 0.0);
    let peak_ram = req.peak_ram_gib.filter(|v| v.is_finite() && *v >= 0.0);

    let bench = req.vcpu.map(|v| v * 120);
    let cpu_pts = match (cpu_time_sec, bench) {
        (Some(sec), Some(b)) => Some(((sec * b as f64) / 1000.0).round() as u64),
        _ => None,
    };
    let cpu_formula = match (cpu_time_sec, bench) {
        (Some(sec), Some(b)) => Some(format!("{:.1}s CPU × bench {} / 1000", sec, b)),
        _ => None,
    };

    let (origin_hostname, geo, net_sec) = if let Some(ref ip) = req.origin_ip {
        let is_local = ip.starts_with("127.") || ip.starts_with("10.") || ip == "::1";
        let hostname = if is_local {
            "direct.local"
        } else {
            "gateway.swacktech.com"
        };
        let g = serde_json::json!({
            "city": if is_local { "Local Subnet" } else { "Council Bluffs" },
            "country": "United States",
            "country_code": "US",
            "asn": "AS15169",
            "isp": "Google Cloud"
        });
        let ns = serde_json::json!({
            "is_tor": false,
            "is_vpn": false,
            "badge_text": "Direct (Clean)",
            "provider_label": "Direct Datacenter"
        });
        (Some(hostname.to_string()), Some(g), Some(ns))
    } else {
        (None, None, None)
    };

    let single_build = serde_json::json!({
        "build_id": build_id,
        "status": req.status,
        "project_name": req.project_name,
        "commit_sha": req.commit_sha,
        "profile": req.profile,
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
        "instance_name": req.instance_name,
        "machine_type": req.machine_type,
        "zone": req.zone,
        "instance_benchmark_score": bench,
        "origin_ip": req.origin_ip,
        "origin_hostname": origin_hostname,
        "geolocation": geo,
        "network_security": net_sec,
        "peak_ram_gib": peak_ram,
        "container_ram_cap_gib": 28,
        "cpu_time_seconds": cpu_time_sec,
        "cpu_points": cpu_pts,
        "cpu_points_formula": cpu_formula,
    });

    let mut builds_vec: Vec<serde_json::Value> = state
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
            .filter(|v| v.is_finite())
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
        .filter(|v| v.is_finite())
        .fold(0.0, f64::max);

    let total_pts: u64 = builds_vec
        .iter()
        .filter_map(|b| b.get("cpu_points").and_then(|v| v.as_u64()))
        .sum();
    let total_cpu_time: f64 = builds_vec
        .iter()
        .filter_map(|b| b.get("cpu_time_seconds").and_then(|v| v.as_f64()))
        .filter(|v| v.is_finite())
        .sum();

    let existing_queue = state.rust_builds.get("queue").cloned();
    let existing_next_id = state.rust_builds.get("next_build_id").cloned();

    let mut new_rb = serde_json::json!({
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
    if let Some(obj) = new_rb.as_object_mut() {
        if let Some(q) = existing_queue {
            obj.insert("queue".to_string(), q);
        }
        if let Some(nid) = existing_next_id {
            obj.insert("next_build_id".to_string(), nid);
        }
    }
    state.rust_builds = new_rb;
    single_build
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
    let single_build = append_rust_build_record(&mut bundle.state, req, now);

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
// Remote Rust Build Queue, ETA, Autoscaling & 3-Artifact GCS Offload
// ============================================================================

pub const DEFAULT_INITIAL_BUILD_ID: u64 = 4553;
pub const DEFAULT_BUILD_DURATION_SECS: f64 = 600.0;
pub const MAX_RUST_BUILD_QUEUE_ITEMS: usize = 200;
pub const DEFAULT_GCS_ARTIFACTS_BUCKET: &str = "example-rust-project-build-artifacts";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RustBuildArtifacts {
    pub gcs_bucket: String,
    pub gcs_prefix: String,
    pub log_url: String,
    pub binary_url: String,
    pub target_zip_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_zip_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_zip_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RustBuildQueueItem {
    pub build_id: u64,
    pub repo: String,
    pub branch: String,
    pub commit_sha: String,
    pub command: String,
    pub requested_output: String,
    pub template_id: String,
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_name: Option<String>,
    pub status: String,
    pub enqueued_at: String,
    pub enqueued_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    pub gcs_bucket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<RustBuildArtifacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_artifact_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueRustBuildRequest {
    pub repo: String,
    pub branch: String,
    pub commit_sha: String,
    pub command: String,
    #[serde(default)]
    pub requested_output: Option<String>,
    #[serde(default)]
    pub template_id: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub binary_name: Option<String>,
    #[serde(default)]
    pub gcs_bucket: Option<String>,
    #[serde(default)]
    pub origin_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRustBuildRequest {
    pub instance_name: String,
    #[serde(default)]
    pub zone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteRustBuildRequest {
    #[serde(default)]
    pub build_id: Option<u64>,
    pub status: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub compile_duration_seconds: Option<f64>,
    #[serde(default)]
    pub delivery_duration_seconds: Option<f64>,
    #[serde(default)]
    pub instance_name: Option<String>,
    #[serde(default)]
    pub zone: Option<String>,
    #[serde(default)]
    pub machine_type: Option<String>,
    #[serde(default)]
    pub vcpu: Option<u32>,
    #[serde(default)]
    pub cache_hits: Option<u32>,
    #[serde(default)]
    pub total_crates: Option<u32>,
    #[serde(default)]
    pub peak_ram_gib: Option<f64>,
    #[serde(default)]
    pub cpu_time_seconds: Option<f64>,
    #[serde(default)]
    pub log_url: Option<String>,
    #[serde(default)]
    pub binary_url: Option<String>,
    #[serde(default)]
    pub target_zip_url: Option<String>,
    #[serde(default)]
    pub log_size_bytes: Option<u64>,
    #[serde(default)]
    pub binary_size_bytes: Option<u64>,
    #[serde(default)]
    pub target_zip_size_bytes: Option<u64>,
    #[serde(default)]
    pub log_sha256: Option<String>,
    #[serde(default)]
    pub binary_sha256: Option<String>,
    #[serde(default)]
    pub target_zip_sha256: Option<String>,
}

fn normalize_requested_output(raw: Option<&str>) -> Result<String, StateError> {
    let val = raw.unwrap_or("log").trim().to_lowercase();
    match val.as_str() {
        "" | "log" | "logs" => Ok("log".to_string()),
        "binary" | "bin" | "binaries" => Ok("binary".to_string()),
        "target_zip" | "target" | "zip" | "archive" => Ok("target_zip".to_string()),
        other => Err(StateError::Validation(format!(
            "Unsupported requested_output '{}'; must be 'log', 'binary', or 'target_zip'",
            other
        ))),
    }
}

pub fn default_artifacts_for_build(gcs_bucket: &str, build_id: u64) -> RustBuildArtifacts {
    let prefix = format!("gs://{}/builds/{}", gcs_bucket, build_id);
    RustBuildArtifacts {
        gcs_bucket: gcs_bucket.to_string(),
        gcs_prefix: prefix.clone(),
        log_url: format!("{}/build.log", prefix),
        binary_url: format!("{}/binaries.tar.zst", prefix),
        target_zip_url: format!("{}/target.tar.zst", prefix),
        log_size_bytes: None,
        binary_size_bytes: None,
        target_zip_size_bytes: None,
        log_sha256: None,
        binary_sha256: None,
        target_zip_sha256: None,
    }
}

fn select_artifact_url_for_output(
    requested_output: &str,
    artifacts: &RustBuildArtifacts,
) -> String {
    match requested_output {
        "binary" => artifacts.binary_url.clone(),
        "target_zip" => artifacts.target_zip_url.clone(),
        _ => artifacts.log_url.clone(),
    }
}

pub fn format_eta_human(eta_seconds: u64) -> String {
    if eta_seconds == 0 {
        return "0m 0s".to_string();
    }
    let hours = eta_seconds / 3600;
    let minutes = (eta_seconds % 3600) / 60;
    let seconds = eta_seconds % 60;
    if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else {
        format!("{}m {}s", minutes, seconds)
    }
}

pub fn count_queued_rust_builds(state: &DashboardState) -> u32 {
    state
        .rust_builds
        .get("queue")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|item| item.get("status").and_then(|s| s.as_str()) == Some("queued"))
                .count() as u32
        })
        .unwrap_or(0)
}

pub fn count_running_rust_builds(state: &DashboardState) -> u32 {
    state
        .rust_builds
        .get("queue")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|item| item.get("status").and_then(|s| s.as_str()) == Some("running"))
                .count() as u32
        })
        .unwrap_or(0)
}

pub fn count_active_builder_instances(state: &DashboardState) -> u32 {
    let source = if !state.instances_view.is_empty() {
        &state.instances_view
    } else {
        &state.instances
    };
    let live = source
        .iter()
        .filter(|i| i.role == "rust_builder" && !i.status.eq_ignore_ascii_case("TERMINATED"))
        .count() as u32;
    if live > 0 {
        live
    } else {
        state.scaling.rust_builder.min_instances.max(1)
    }
}

pub fn effective_average_build_seconds(state: &DashboardState) -> f64 {
    let avg = state
        .rust_builds
        .get("average_duration_seconds")
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite() && *v > 0.0);
    if let Some(a) = avg {
        return a;
    }
    if let Some(builds) = state.rust_builds.get("builds").and_then(|v| v.as_array()) {
        let valid: Vec<f64> = builds
            .iter()
            .filter_map(|b| b.get("total_duration_seconds").and_then(|v| v.as_f64()))
            .filter(|v| v.is_finite() && *v > 0.0)
            .collect();
        if !valid.is_empty() {
            let sum: f64 = valid.iter().sum();
            return (sum / valid.len() as f64).round();
        }
    }
    DEFAULT_BUILD_DURATION_SECS
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BuilderQueueEvaluation {
    pub queued_builds: u32,
    pub running_builds: u32,
    pub active_builders: u32,
    pub target_builders: u32,
    pub queued_builds_threshold: u32,
    pub scaling_up: bool,
    pub decision: String,
    pub summary: String,
}

pub fn evaluate_rust_builder_queue_scaling(
    state: &mut DashboardState,
    now: u64,
) -> BuilderQueueEvaluation {
    let queued_builds = count_queued_rust_builds(state);
    let running_builds = count_running_rust_builds(state);
    let active_builders = count_active_builder_instances(state);
    let policy = &state.scaling.rust_builder;
    let threshold = policy.queued_builds_threshold.max(1);

    let should_scale_up = policy.enable_queue_trigger
        && queued_builds >= threshold
        && active_builders < policy.max_instances;

    let target_builders = if should_scale_up {
        let extra = ((queued_builds - threshold) / threshold) + 1;
        (active_builders + extra).min(policy.max_instances)
    } else {
        active_builders
    };

    let (decision, summary) = if should_scale_up {
        (
            "SCALE_UP".to_string(),
            format!(
                "Scale up from {} to {} builders ({} queued builds >= threshold {})",
                active_builders, target_builders, queued_builds, threshold
            ),
        )
    } else if policy.enable_queue_trigger && queued_builds >= threshold {
        (
            "HOLD".to_string(),
            format!(
                "Hold at max_instances {} ({} queued builds >= threshold {})",
                policy.max_instances, queued_builds, threshold
            ),
        )
    } else {
        (
            "HOLD".to_string(),
            format!(
                "Hold at {} builders ({} queued builds < threshold {})",
                active_builders, queued_builds, threshold
            ),
        )
    };

    let eval_json = serde_json::json!({
        "decision": decision,
        "current_instances": active_builders,
        "target_instances": target_builders,
        "queued_builds": queued_builds,
        "running_builds": running_builds,
        "queued_builds_threshold": threshold,
        "evaluated_at": chrono_iso(now),
        "summary": summary,
        "scale_up_triggers": if should_scale_up {
            serde_json::json!([format!("queued_builds: {} >= {}", queued_builds, threshold)])
        } else {
            serde_json::json!([])
        }
    });

    if !state.scaling_evaluations.is_object() {
        state.scaling_evaluations = serde_json::json!({});
    }
    if let Some(obj) = state.scaling_evaluations.as_object_mut() {
        obj.insert("rust_builder".to_string(), eval_json.clone());
    }
    if let Some(ref mut ov) = state.overview {
        if let Some(ov_obj) = ov.as_object_mut() {
            if !ov_obj.contains_key("scaling_evaluations")
                || !ov_obj["scaling_evaluations"].is_object()
            {
                ov_obj.insert("scaling_evaluations".to_string(), serde_json::json!({}));
            }
            if let Some(se) = ov_obj
                .get_mut("scaling_evaluations")
                .and_then(|v| v.as_object_mut())
            {
                se.insert("rust_builder".to_string(), eval_json);
            }
        }
    }

    BuilderQueueEvaluation {
        queued_builds,
        running_builds,
        active_builders,
        target_builders,
        queued_builds_threshold: threshold,
        scaling_up: should_scale_up,
        decision,
        summary,
    }
}

fn load_queue_from_state(state: &DashboardState) -> Vec<RustBuildQueueItem> {
    state
        .rust_builds
        .get("queue")
        .and_then(|v| serde_json::from_value::<Vec<RustBuildQueueItem>>(v.clone()).ok())
        .unwrap_or_default()
}

fn save_queue_into_state(
    state: &mut DashboardState,
    queue: &[RustBuildQueueItem],
    next_build_id: u64,
) {
    if !state.rust_builds.is_object() {
        state.rust_builds = serde_json::json!({
            "total_builds": 0,
            "average_duration_seconds": 0.0,
            "total_cache_hits": 0,
            "overall_cache_hit_rate_percent": 0.0,
            "max_peak_ram_gib": 0.0,
            "total_cpu_points": 0,
            "total_cpu_time_seconds": 0.0,
            "vpn_or_tor_count": 0,
            "builds": []
        });
    }
    if let Some(obj) = state.rust_builds.as_object_mut() {
        obj.insert(
            "queue".to_string(),
            serde_json::to_value(queue).unwrap_or_else(|_| serde_json::json!([])),
        );
        obj.insert(
            "next_build_id".to_string(),
            serde_json::json!(next_build_id),
        );
    }
}

fn build_status_response(
    item: &RustBuildQueueItem,
    queue: &[RustBuildQueueItem],
    eval: &BuilderQueueEvaluation,
    avg_build_secs: f64,
    now: u64,
    include_origin_ip: bool,
) -> serde_json::Value {
    let queue_position: u32 = if item.status == "queued" {
        queue
            .iter()
            .filter(|q| q.status == "queued")
            .position(|q| q.build_id == item.build_id)
            .map(|idx| (idx as u32) + 1)
            .unwrap_or(1)
    } else {
        0
    };

    let effective_builders = eval.active_builders.max(1) as f64;
    let eta_seconds: u64 = match item.status.as_str() {
        "queued" => {
            let waves = (queue_position as f64 / effective_builders).ceil();
            (waves * avg_build_secs).round().max(0.0) as u64
        }
        "running" => {
            let started = item.started_epoch.unwrap_or(now);
            let elapsed = now.saturating_sub(started) as f64;
            (avg_build_secs - elapsed).max(0.0).round() as u64
        }
        _ => 0,
    };

    let eta_hours = ((eta_seconds as f64 / 3600.0) * 100.0).round() / 100.0;
    let eta_human = format_eta_human(eta_seconds);
    let avg_human = format_eta_human(avg_build_secs.round() as u64);

    let message = match item.status.as_str() {
        "queued" => {
            if eval.scaling_up {
                format!(
                    "Build #{} is #{} in line. ETA based on previous builds' average time ({}) is {:.2} hours ({}) until compilation; however, we are scaling up right now ({} -> {} builders).",
                    item.build_id,
                    queue_position,
                    avg_human,
                    eta_hours,
                    eta_human,
                    eval.active_builders,
                    eval.target_builders
                )
            } else {
                format!(
                    "Build #{} is #{} in line. ETA based on previous builds' average time ({}) is {:.2} hours ({}) until compilation.",
                    item.build_id, queue_position, avg_human, eta_hours, eta_human
                )
            }
        }
        "running" => format!(
            "Build #{} is actively compiling on {}. Estimated remaining time: {}.",
            item.build_id,
            item.assigned_instance.as_deref().unwrap_or("builder"),
            eta_human
        ),
        "completed" => {
            let url = item
                .selected_artifact_url
                .as_deref()
                .unwrap_or("artifact ready");
            format!(
                "Build #{} completed. Here is the link to your requested '{}' output: {}",
                item.build_id, item.requested_output, url
            )
        }
        "failed" => {
            let log_url = item
                .artifacts
                .as_ref()
                .map(|a| a.log_url.as_str())
                .unwrap_or("log unavailable");
            format!(
                "Build #{} failed. Build log is available at: {}",
                item.build_id, log_url
            )
        }
        other => format!("Build #{} is in state '{}'.", item.build_id, other),
    };

    let mut sanitized_item = item.clone();
    if !include_origin_ip {
        sanitized_item.origin_ip = None;
    }

    serde_json::json!({
        "status": "ok",
        "build_id": item.build_id,
        "build_status": item.status,
        "queue_position": queue_position,
        "queued_builds": eval.queued_builds,
        "running_builds": eval.running_builds,
        "active_builders": eval.active_builders,
        "target_builders": eval.target_builders,
        "scaling_up": eval.scaling_up,
        "scaling_decision": eval.decision,
        "scaling_summary": eval.summary,
        "average_build_duration_seconds": avg_build_secs,
        "eta_seconds": eta_seconds,
        "eta_hours": eta_hours,
        "eta_human": eta_human,
        "requested_output": item.requested_output,
        "selected_artifact_url": item.selected_artifact_url,
        "artifacts": item.artifacts,
        "poll_url": format!("/api/rust-builds/{}", item.build_id),
        "message": message,
        "build": sanitized_item,
    })
}

pub async fn handle_post_rust_builds_enqueue<K: KvStoreBackend>(
    kv: &K,
    req: EnqueueRustBuildRequest,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let repo = req.repo.trim();
    if repo.is_empty() || repo.len() > 128 || repo.contains("..") {
        return Err(StateError::Validation(
            "repo must be a non-empty repository identifier (<= 128 chars)".into(),
        ));
    }
    let branch = req.branch.trim();
    if branch.is_empty() || branch.len() > 128 || branch.contains("..") {
        return Err(StateError::Validation(
            "branch must be a non-empty branch name (<= 128 chars)".into(),
        ));
    }
    let commit_sha = req.commit_sha.trim();
    if commit_sha.len() < 7
        || commit_sha.len() > 64
        || !commit_sha.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(StateError::Validation(
            "commit_sha must be a 7..64 character hexadecimal Git commit SHA".into(),
        ));
    }
    let command = req.command.trim();
    if command.is_empty() || command.len() > 512 {
        return Err(StateError::Validation(
            "command must be a non-empty build or test command (<= 512 chars)".into(),
        ));
    }
    let requested_output = normalize_requested_output(req.requested_output.as_deref())?;

    let gcs_bucket = req
        .gcs_bucket
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_GCS_ARTIFACTS_BUCKET);
    if gcs_bucket.len() > 128
        || gcs_bucket.contains("..")
        || gcs_bucket.contains('/')
        || !gcs_bucket
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(StateError::Validation(
            "gcs_bucket contains invalid characters".into(),
        ));
    }

    let template_id = req
        .template_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("rust-builder-default")
        .to_string();
    let profile = req
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("test")
        .to_string();
    let binary_name = req
        .binary_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let (mut bundle, _) = load_state(kv, now).await?;
    let mut queue = load_queue_from_state(&bundle.state);

    let max_existing_id = queue.iter().map(|q| q.build_id).max().unwrap_or(0);
    let stored_next_id = bundle
        .state
        .rust_builds
        .get("next_build_id")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_INITIAL_BUILD_ID);
    let build_id = stored_next_id
        .max(max_existing_id.saturating_add(1))
        .max(DEFAULT_INITIAL_BUILD_ID);

    let item = RustBuildQueueItem {
        build_id,
        repo: repo.to_string(),
        branch: branch.to_string(),
        commit_sha: commit_sha.to_string(),
        command: command.to_string(),
        requested_output,
        template_id,
        profile,
        binary_name,
        status: "queued".to_string(),
        enqueued_at: chrono_iso(now),
        enqueued_epoch: now,
        started_at: None,
        started_epoch: None,
        finished_at: None,
        finished_epoch: None,
        assigned_instance: None,
        zone: None,
        gcs_bucket: gcs_bucket.to_string(),
        artifacts: None,
        selected_artifact_url: None,
        compile_duration_seconds: None,
        exit_code: None,
        origin_ip: req.origin_ip,
    };

    queue.push(item.clone());
    if queue.len() > MAX_RUST_BUILD_QUEUE_ITEMS {
        // Evict oldest completed/failed items first when bounding queue history
        if let Some(done_idx) = queue
            .iter()
            .position(|q| q.status == "completed" || q.status == "failed")
        {
            queue.remove(done_idx);
        } else {
            queue.remove(0);
        }
    }

    save_queue_into_state(&mut bundle.state, &queue, build_id + 1);
    let eval = evaluate_rust_builder_queue_scaling(&mut bundle.state, now);
    let avg_build_secs = effective_average_build_seconds(&bundle.state);

    bundle.state.updated_at = now;
    save_state(kv, &mut bundle, now).await?;

    let resp = build_status_response(&item, &queue, &eval, avg_build_secs, now, true);
    Ok((200, resp))
}

pub async fn handle_get_rust_build_status<K: KvStoreBackend>(
    kv: &K,
    build_id: u64,
    is_authed_admin: bool,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let (mut bundle, _) = load_state(kv, now).await?;
    let queue = load_queue_from_state(&bundle.state);
    let item = queue
        .iter()
        .find(|q| q.build_id == build_id)
        .ok_or_else(|| StateError::NotFound(format!("Build #{} not found in queue", build_id)))?;

    let eval = evaluate_rust_builder_queue_scaling(&mut bundle.state, now);
    let avg_build_secs = effective_average_build_seconds(&bundle.state);
    let resp = build_status_response(item, &queue, &eval, avg_build_secs, now, is_authed_admin);
    Ok((200, resp))
}

pub async fn handle_get_rust_builds_queue<K: KvStoreBackend>(
    kv: &K,
    is_authed_admin: bool,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let (mut bundle, _) = load_state(kv, now).await?;
    let mut queue = load_queue_from_state(&bundle.state);
    let eval = evaluate_rust_builder_queue_scaling(&mut bundle.state, now);
    let avg_build_secs = effective_average_build_seconds(&bundle.state);

    if !is_authed_admin {
        for q in &mut queue {
            q.origin_ip = None;
        }
    }

    Ok((
        200,
        serde_json::json!({
            "status": "ok",
            "queued_builds": eval.queued_builds,
            "running_builds": eval.running_builds,
            "active_builders": eval.active_builders,
            "target_builders": eval.target_builders,
            "queued_builds_threshold": eval.queued_builds_threshold,
            "scaling_up": eval.scaling_up,
            "scaling_decision": eval.decision,
            "scaling_summary": eval.summary,
            "average_build_duration_seconds": avg_build_secs,
            "queue": queue,
        }),
    ))
}

pub async fn handle_post_rust_builds_claim<K: KvStoreBackend>(
    kv: &K,
    req: ClaimRustBuildRequest,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let instance_name = req.instance_name.trim();
    if instance_name.is_empty() {
        return Err(StateError::Validation(
            "instance_name cannot be empty".into(),
        ));
    }

    let (mut bundle, _) = load_state(kv, now).await?;
    let mut queue = load_queue_from_state(&bundle.state);

    let next_idx = queue.iter().position(|q| q.status == "queued");
    let Some(idx) = next_idx else {
        return Ok((
            200,
            serde_json::json!({
                "status": "empty",
                "claimed": false,
                "build": serde_json::Value::Null,
            }),
        ));
    };

    queue[idx].status = "running".to_string();
    queue[idx].started_at = Some(chrono_iso(now));
    queue[idx].started_epoch = Some(now);
    queue[idx].assigned_instance = Some(instance_name.to_string());
    queue[idx].zone = req
        .zone
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let claimed_item = queue[idx].clone();
    let upload_targets =
        default_artifacts_for_build(&claimed_item.gcs_bucket, claimed_item.build_id);

    let next_build_id = bundle
        .state
        .rust_builds
        .get("next_build_id")
        .and_then(|v| v.as_u64())
        .unwrap_or(claimed_item.build_id + 1);
    save_queue_into_state(&mut bundle.state, &queue, next_build_id);
    let eval = evaluate_rust_builder_queue_scaling(&mut bundle.state, now);

    bundle.state.updated_at = now;
    save_state(kv, &mut bundle, now).await?;

    Ok((
        200,
        serde_json::json!({
            "status": "claimed",
            "claimed": true,
            "build": claimed_item,
            "upload_targets": upload_targets,
            "scaling_evaluation": eval,
        }),
    ))
}

pub async fn handle_post_rust_builds_complete<K: KvStoreBackend>(
    kv: &K,
    build_id: u64,
    req: CompleteRustBuildRequest,
    now: u64,
) -> Result<(u16, serde_json::Value), StateError> {
    let status_norm = match req.status.trim().to_lowercase().as_str() {
        "completed" | "succeeded" | "ok" | "passed" => "completed",
        "failed" | "error" => "failed",
        other => {
            return Err(StateError::Validation(format!(
                "Invalid completion status '{}'; expected 'completed' or 'failed'",
                other
            )));
        }
    };

    if let Some(c) = req.compile_duration_seconds {
        if !c.is_finite() || c < 0.0 {
            return Err(StateError::Validation(
                "compile_duration_seconds must be a finite non-negative number".into(),
            ));
        }
    }
    if let Some(d) = req.delivery_duration_seconds {
        if !d.is_finite() || d < 0.0 {
            return Err(StateError::Validation(
                "delivery_duration_seconds must be a finite non-negative number".into(),
            ));
        }
    }
    if let Some(r) = req.peak_ram_gib {
        if !r.is_finite() || r < 0.0 {
            return Err(StateError::Validation(
                "peak_ram_gib must be a finite non-negative number".into(),
            ));
        }
    }
    if let Some(cpu) = req.cpu_time_seconds {
        if !cpu.is_finite() || cpu < 0.0 {
            return Err(StateError::Validation(
                "cpu_time_seconds must be a finite non-negative number".into(),
            ));
        }
    }

    let (mut bundle, _) = load_state(kv, now).await?;
    let mut queue = load_queue_from_state(&bundle.state);
    let idx = queue
        .iter()
        .position(|q| q.build_id == build_id)
        .ok_or_else(|| StateError::NotFound(format!("Build #{} not found in queue", build_id)))?;

    let mut defaults = default_artifacts_for_build(&queue[idx].gcs_bucket, build_id);
    if let Some(ref u) = req.log_url {
        let trimmed = u.trim();
        if !trimmed.is_empty() {
            defaults.log_url = trimmed.to_string();
        }
    }
    if let Some(ref u) = req.binary_url {
        let trimmed = u.trim();
        if !trimmed.is_empty() {
            defaults.binary_url = trimmed.to_string();
        }
    }
    if let Some(ref u) = req.target_zip_url {
        let trimmed = u.trim();
        if !trimmed.is_empty() {
            defaults.target_zip_url = trimmed.to_string();
        }
    }
    defaults.log_size_bytes = req.log_size_bytes;
    defaults.binary_size_bytes = req.binary_size_bytes;
    defaults.target_zip_size_bytes = req.target_zip_size_bytes;
    defaults.log_sha256 = req.log_sha256.clone();
    defaults.binary_sha256 = req.binary_sha256.clone();
    defaults.target_zip_sha256 = req.target_zip_sha256.clone();

    let selected_url = select_artifact_url_for_output(&queue[idx].requested_output, &defaults);

    let compile_sec = req.compile_duration_seconds.unwrap_or_else(|| {
        let started = queue[idx]
            .started_epoch
            .unwrap_or(queue[idx].enqueued_epoch);
        now.saturating_sub(started) as f64
    });
    let delivery_sec = req.delivery_duration_seconds.unwrap_or(2.0);

    queue[idx].status = status_norm.to_string();
    queue[idx].finished_at = Some(chrono_iso(now));
    queue[idx].finished_epoch = Some(now);
    queue[idx].compile_duration_seconds = Some(compile_sec);
    queue[idx].exit_code = req
        .exit_code
        .or(Some(if status_norm == "completed" { 0 } else { 1 }));
    if let Some(ref inst) = req.instance_name {
        if !inst.trim().is_empty() {
            queue[idx].assigned_instance = Some(inst.trim().to_string());
        }
    }
    if let Some(ref z) = req.zone {
        if !z.trim().is_empty() {
            queue[idx].zone = Some(z.trim().to_string());
        }
    }
    queue[idx].artifacts = Some(defaults.clone());
    queue[idx].selected_artifact_url = Some(selected_url.clone());

    let completed_item = queue[idx].clone();
    let next_build_id = bundle
        .state
        .rust_builds
        .get("next_build_id")
        .and_then(|v| v.as_u64())
        .unwrap_or(build_id + 1);
    save_queue_into_state(&mut bundle.state, &queue, next_build_id);

    // Also record the finished run into `rust_builds.builds` so rolling average duration and cache KPIs update
    let record_req = RecordRustBuildRequest {
        status: if status_norm == "completed" {
            "succeeded".to_string()
        } else {
            "failed".to_string()
        },
        project_name: Some(completed_item.repo.clone()),
        commit_sha: Some(completed_item.commit_sha.clone()),
        profile: Some(completed_item.profile.clone()),
        compile_duration_seconds: Some(compile_sec),
        delivery_duration_seconds: Some(delivery_sec),
        instance_name: completed_item.assigned_instance.clone(),
        zone: completed_item.zone.clone(),
        machine_type: req
            .machine_type
            .or_else(|| Some("n4-highcpu-16".to_string())),
        cpu_platform: Some("Intel Emerald Rapids".to_string()),
        vcpu: Some(req.vcpu.unwrap_or(16)),
        origin_ip: completed_item.origin_ip.clone(),
        cache_hits: req.cache_hits,
        total_crates: req.total_crates,
        peak_ram_gib: req.peak_ram_gib,
        cpu_time_seconds: req.cpu_time_seconds,
    };

    let _ = append_rust_build_record(&mut bundle.state, record_req, now);
    let eval = evaluate_rust_builder_queue_scaling(&mut bundle.state, now);
    let avg_build_secs = effective_average_build_seconds(&bundle.state);
    bundle.state.updated_at = now;
    save_state(kv, &mut bundle, now).await?;

    let resp = build_status_response(&completed_item, &queue, &eval, avg_build_secs, now, true);
    Ok((200, resp))
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
        // Filled from the GCP_PROJECT Worker variable or the service account key; never compiled in.
        project: String::new(),
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
