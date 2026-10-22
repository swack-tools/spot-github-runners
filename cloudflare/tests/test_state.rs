use spot_dashboard_edge::state::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

pub struct MockKvStore {
    pub data: Mutex<HashMap<String, String>>,
    pub get_count: AtomicUsize,
    pub put_count: AtomicUsize,
}

impl MockKvStore {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            get_count: AtomicUsize::new(0),
            put_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for MockKvStore {
    async fn get(&self, key: &str) -> Result<Option<String>, StateError> {
        self.get_count.fetch_add(1, Ordering::SeqCst);
        let guard = self.data.lock().unwrap();
        Ok(guard.get(key).cloned())
    }

    async fn put(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.put_count.fetch_add(1, Ordering::SeqCst);
        let mut guard = self.data.lock().unwrap();
        guard.insert(key.to_string(), value.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn test_cached_overview_zero_kv_writes_and_cache_hit() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // First GET: Cache miss, loads from KV (or creates default)
    let (status, _) = handle_api_overview(&kv, OverviewQuery::default(), now)
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0); // 0 KV writes!

    // Subsequent 10 GETs within 60s: All hit in-isolate memory cache
    for offset in 1..=10 {
        let (status, _) = handle_api_overview(&kv, OverviewQuery::default(), now + offset)
            .await
            .unwrap();
        assert_eq!(status, 200);
    }

    // Assert strictly ZERO additional KV reads and ZERO KV writes
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_cache_ttl_expiration_triggers_single_kv_read() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Initial fetch
    handle_api_overview(&kv, OverviewQuery::default(), now)
        .await
        .unwrap();
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);

    // Advance simulated clock past 60s TTL
    handle_api_overview(&kv, OverviewQuery::default(), now + 65)
        .await
        .unwrap();
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 2);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_scaling_update_performs_single_write_and_enforces_floor() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Populate state
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    save_state(&kv, &mut bundle, now).await.unwrap();
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // 1. Try to set queued_jobs_threshold < 20 -> must fail validation
    let invalid_req = UpdateScalingRequest {
        role: "cicd_runner".into(),
        policy: serde_json::json!({
            "min_runners": 4,
            "max_runners": 24,
            "runners_per_instance": 4,
            "runner_vcpu": 4,
            "runner_ram_gib": 16,
            "scale_up_step_runners": 2,
            "enable_queued_jobs_trigger": true,
            "queued_jobs_threshold": 19, // Below 20 floor!
            "enable_provisioned_jobs_trigger": true,
            "provisioned_jobs_threshold": 6,
            "enable_hourly_runs_scale_up": true,
            "hourly_runs_scale_up_threshold": 20,
            "hourly_runs_scale_up_target_runners": 8,
            "enable_cpu_utilization_trigger": true,
            "cpu_threshold_percent": 50.0,
            "cpu_scope": "majority",
            "enable_journal_oom_trigger": true,
            "enable_github_ci_oom_trigger": true,
            "oom_lookback_minutes": 60,
            "enable_hourly_runs_scale_down": true,
            "scale_down_max_jobs_past_hour": 5,
            "scale_down_target_runners": 4,
            "enable_idle_cpu_scale_down": true,
            "scale_down_cpu_percent": 25.0,
            "max_scale_up_hourly_usd": 5.0,
            "max_scale_up_daily_usd": 10.0,
            "max_scale_up_monthly_usd": 50.0,
            "include_disk_in_scale_budget": true,
            "shared_global_budget": false
        }),
    };
    let err = handle_post_scaling(&kv, invalid_req, now + 1)
        .await
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("queued_jobs_threshold must be >= 20"));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1); // 0 extra writes on validation failure

    // 2. Set queued_jobs_threshold >= 20 -> succeeds with exactly 1 write
    let valid_req = UpdateScalingRequest {
        role: "cicd_runner".into(),
        policy: serde_json::json!({
            "min_runners": 4,
            "max_runners": 24,
            "runners_per_instance": 4,
            "runner_vcpu": 4,
            "runner_ram_gib": 16,
            "scale_up_step_runners": 2,
            "enable_queued_jobs_trigger": true,
            "queued_jobs_threshold": 25,
            "enable_provisioned_jobs_trigger": true,
            "provisioned_jobs_threshold": 6,
            "enable_hourly_runs_scale_up": true,
            "hourly_runs_scale_up_threshold": 20,
            "hourly_runs_scale_up_target_runners": 8,
            "enable_cpu_utilization_trigger": true,
            "cpu_threshold_percent": 50.0,
            "cpu_scope": "majority",
            "enable_journal_oom_trigger": true,
            "enable_github_ci_oom_trigger": true,
            "oom_lookback_minutes": 60,
            "enable_hourly_runs_scale_down": true,
            "scale_down_max_jobs_past_hour": 5,
            "scale_down_target_runners": 4,
            "enable_idle_cpu_scale_down": true,
            "scale_down_cpu_percent": 25.0,
            "max_scale_up_hourly_usd": 5.0,
            "max_scale_up_daily_usd": 10.0,
            "max_scale_up_monthly_usd": 50.0,
            "include_disk_in_scale_budget": true,
            "shared_global_budget": false
        }),
    };

    let (status, _) = handle_post_scaling(&kv, valid_req, now + 2).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 2); // exactly 1 write for the mutation

    // Subsequent GET returns updated policy from memory cache without KV reads
    let gets_before = kv.get_count.load(Ordering::SeqCst);
    let (_, scaling) = handle_get_scaling(&kv, now + 3).await.unwrap();
    assert_eq!(scaling.cicd_runner.queued_jobs_threshold, 25);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), gets_before);
}

#[tokio::test]
async fn test_instance_actions_safety_gate_returns_plan_with_zero_kv_writes() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Add instance record
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.instances.push(InstanceRecord {
        id: "inst-1".into(),
        name: "oxidex-runners-za-1".into(),
        zone: "africa-south1-a".into(),
        region: "africa-south1".into(),
        machine_type: "c4d-highcpu-16".into(),
        status: "RUNNING".into(),
        role: "cicd_runner".into(),
        vcpu: 16,
        ram_gib: 32,
        runners_count: 2,
        runners_count_known: true,
        runner_limits_known: true,
    });
    save_state(&kv, &mut bundle, now).await.unwrap();
    let initial_puts = kv.put_count.load(Ordering::SeqCst);

    // POST /api/instances/delete
    let del_req = DeleteInstanceRequest {
        instance_id: Some("inst-1".into()),
        instance_name: None,
    };
    let (status, del_resp) = handle_post_instances_delete(&kv, del_req, now + 5)
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(del_resp["applied"], false);
    assert_eq!(del_resp["status"], "not_applied");
    assert_eq!(del_resp["message"], LIFECYCLE_ACTIONS_DISABLED_MESSAGE);

    // POST /api/instances/scale-region
    let scale_req = ScaleRegionRequest {
        region: "africa-south1".into(),
        target_instances: 2,
        role: "cicd_runner".into(),
    };
    let (status, scale_resp) = handle_post_instances_scale_region(&kv, scale_req, now + 6)
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(scale_resp["applied"], false);
    assert_eq!(scale_resp["status"], "not_applied");
    assert_eq!(scale_resp["message"], LIFECYCLE_ACTIONS_DISABLED_MESSAGE);

    // Assert safety gates performed 0 mutations and 0 KV writes
    assert_eq!(kv.put_count.load(Ordering::SeqCst), initial_puts);
}

#[tokio::test]
async fn test_gcp_oauth_token_caching_zero_kv_operations() {
    invalidate_gcp_token_cache();
    let now = 1792000000;

    assert_eq!(get_cached_gcp_token(now), None);
    set_cached_gcp_token(now, "mock_token_abc123".into());

    // Token valid within 55 minutes
    assert_eq!(
        get_cached_gcp_token(now + 1800),
        Some("mock_token_abc123".into())
    );

    // Token expires after 55 minutes (3300s)
    assert_eq!(get_cached_gcp_token(now + 3301), None);
}

#[tokio::test]
async fn test_overview_snapshot_serialization_parity() {
    let bundle = default_dashboard_bundle(1792000000).expect("Default bundle created");
    assert_eq!(bundle.schema, SNAPSHOT_SCHEMA);
    assert_eq!(bundle.writer_slot, WRITER_SLOT_EDGE);

    let serialized = serde_json::to_string(&bundle).expect("Serialize bundle");
    let deserialized: DashboardBundle =
        serde_json::from_str(&serialized).expect("Deserialize bundle");
    assert_eq!(bundle, deserialized);
}
