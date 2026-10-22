use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use spot_dashboard_edge::state::*;

/// High-fidelity mock KV store tracking get and put invocations.
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

    pub fn with_bundle(bundle: DashboardBundle) -> Self {
        let store = Self::new();
        let raw = serde_json::to_string(&bundle).expect("Serialize initial bundle");
        store.data.lock().unwrap().insert(KV_STATE_KEY.to_string(), raw);
        store
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

// ============================================================================
// 1. IN-ISOLATE MEMORY CACHE STRESS TESTS
// ============================================================================

#[tokio::test]
async fn test_in_isolate_cache_stress_1000_consecutive_requests() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let start_time = 1792000000;

    // First request: Cache miss, loads from KV
    let (status, _) = handle_api_overview(&kv, OverviewQuery::default(), start_time).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // Stress: 1,000 consecutive requests distributed across the 60-second TTL window (offsets 0 to 59)
    for i in 0..1000 {
        let offset = (i % 60) as u64;
        let (st, _) = handle_api_overview(&kv, OverviewQuery::default(), start_time + offset).await.unwrap();
        assert_eq!(st, 200);
    }

    // KV reads must REMAIN strictly 1. Zero extra reads, zero writes.
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // Boundary Test: At exactly start_time + 60, the 60s TTL expires
    let (st_exp, _) = handle_api_overview(&kv, OverviewQuery::default(), start_time + 60).await.unwrap();
    assert_eq!(st_exp, 200);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 2, "60s TTL expiration must trigger exactly 1 new KV read");
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // Next 500 requests in window [60..119] must all hit the refreshed memory cache
    for i in 0..500 {
        let offset = 60 + (i % 60) as u64;
        let (st, _) = handle_api_overview(&kv, OverviewQuery::default(), start_time + offset).await.unwrap();
        assert_eq!(st, 200);
    }
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 2);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_in_isolate_cache_multi_endpoint_and_clock_skew() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Load initial state into cache
    let (health_status, health_body) = handle_api_health(&kv, now).await;
    assert_eq!(health_status, 200);
    assert_eq!(health_body["cache_source"], "default");
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);

    // Consecutive calls to different GET endpoints within TTL must all hit memory cache
    let (_, _) = handle_api_overview(&kv, OverviewQuery::default(), now + 10).await.unwrap();
    let (_, _) = handle_get_scaling(&kv, now + 20).await.unwrap();
    let (h_st, h_body) = handle_api_health(&kv, now + 30).await;
    assert_eq!(h_st, 200);
    assert_eq!(h_body["cache_source"], "memory");

    // All endpoints shared the same in-isolate memory cache; KV get_count is still 1
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // Clock Skew Test: Simulated clock jumps backward (e.g. now - 100)
    // saturating_sub ensures no panic/underflow, treated as delta 0 < 60s -> cache hit
    let (st_skew, _) = handle_api_overview(&kv, OverviewQuery::default(), now.saturating_sub(100)).await.unwrap();
    assert_eq!(st_skew, 200);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1, "Clock skew must not trigger extra KV reads or panic");
}

#[tokio::test]
async fn test_in_isolate_cache_immediate_write_reflection_zero_subsequent_reads() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // 1. Initial hydrate
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    save_state(&kv, &mut bundle, now).await.unwrap();
    let gets_after_init = kv.get_count.load(Ordering::SeqCst);
    let puts_after_init = kv.put_count.load(Ordering::SeqCst);

    // 2. Perform valid scaling mutation
    let valid_req = UpdateScalingRequest {
        role: "cicd_runner".into(),
        policy: serde_json::json!({
            "min_runners": 8,
            "max_runners": 32,
            "runners_per_instance": 4,
            "runner_vcpu": 4,
            "runner_ram_gib": 16,
            "scale_up_step_runners": 4,
            "enable_queued_jobs_trigger": true,
            "queued_jobs_threshold": 30,
            "enable_provisioned_jobs_trigger": true,
            "provisioned_jobs_threshold": 8,
            "enable_hourly_runs_scale_up": true,
            "hourly_runs_scale_up_threshold": 25,
            "hourly_runs_scale_up_target_runners": 12,
            "enable_cpu_utilization_trigger": true,
            "cpu_threshold_percent": 60.0,
            "cpu_scope": "majority",
            "enable_journal_oom_trigger": true,
            "enable_github_ci_oom_trigger": true,
            "oom_lookback_minutes": 60,
            "enable_hourly_runs_scale_down": true,
            "scale_down_max_jobs_past_hour": 5,
            "scale_down_target_runners": 4,
            "enable_idle_cpu_scale_down": true,
            "scale_down_cpu_percent": 25.0,
            "max_scale_up_hourly_usd": 6.0,
            "max_scale_up_daily_usd": 15.0,
            "max_scale_up_monthly_usd": 70.0,
            "include_disk_in_scale_budget": true,
            "shared_global_budget": false
        }),
    };

    let (mut_status, _) = handle_post_scaling(&kv, valid_req, now + 1).await.unwrap();
    assert_eq!(mut_status, 200);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_after_init + 1, "Exactly 1 write on mutation");

    // 3. Immediately query scaling and overview 100 times
    for offset in 2..=100 {
        let (status, scaling) = handle_get_scaling(&kv, now + (offset % 50)).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(scaling.cicd_runner.queued_jobs_threshold, 30, "Cache reflects updated state immediately");
        assert_eq!(scaling.cicd_runner.max_runners, 32);
    }

    // Zero extra KV reads performed after mutation!
    assert_eq!(kv.get_count.load(Ordering::SeqCst), gets_after_init, "Writes must update in-isolate cache; 0 extra KV reads");
}

// ============================================================================
// 2. KV WRITE GATING STRESS TESTS
// ============================================================================

#[tokio::test]
async fn test_kv_write_gating_stress_steady_state_10000_reads_zero_writes() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Simulate 10,000 read requests over a multi-hour span
    for i in 0..10_000 {
        let simulated_now = now + (i * 2) as u64; // Advances 2s per request
        match i % 3 {
            0 => {
                let (st, _) = handle_api_overview(&kv, OverviewQuery::default(), simulated_now).await.unwrap();
                assert_eq!(st, 200);
            }
            1 => {
                let (st, _) = handle_api_health(&kv, simulated_now).await;
                assert_eq!(st, 200);
            }
            2 => {
                let (st, _) = handle_get_scaling(&kv, simulated_now).await.unwrap();
                assert_eq!(st, 200);
            }
            _ => unreachable!(),
        }
    }

    // CRITICAL KV WRITE BUDGET ASSERTION:
    // Steady-state reads must perform EXACTLY 0 KV WRITES!
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0, "Steady-state reads must perform EXACTLY 0 KV writes");
}

#[tokio::test]
async fn test_kv_write_gating_stress_invalid_scaling_payloads_zero_writes() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Pre-populate state
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    save_state(&kv, &mut bundle, now).await.unwrap();
    let baseline_puts = kv.put_count.load(Ordering::SeqCst);

    // Adversarial Matrix: queued_jobs_threshold values below 20 floor
    let invalid_thresholds = vec![0, 1, 2, 5, 10, 15, 18, 19];
    for threshold in invalid_thresholds {
        let req = UpdateScalingRequest {
            role: "cicd_runner".into(),
            policy: serde_json::json!({
                "min_runners": 4,
                "max_runners": 24,
                "runners_per_instance": 4,
                "runner_vcpu": 4,
                "runner_ram_gib": 16,
                "scale_up_step_runners": 2,
                "enable_queued_jobs_trigger": true,
                "queued_jobs_threshold": threshold, // INVALID (< 20)
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

        let err = handle_post_scaling(&kv, req, now + 1).await.unwrap_err();
        assert!(err.to_string().contains("queued_jobs_threshold must be >= 20"));
        assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts, "No writes on threshold < 20");
    }

    // Adversarial: min_runners > max_runners
    let invalid_bounds_req = UpdateScalingRequest {
        role: "cicd_runner".into(),
        policy: serde_json::json!({
            "min_runners": 50,
            "max_runners": 10, // INVALID
            "runners_per_instance": 4,
            "runner_vcpu": 4,
            "runner_ram_gib": 16,
            "scale_up_step_runners": 2,
            "enable_queued_jobs_trigger": true,
            "queued_jobs_threshold": 20,
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
    let err = handle_post_scaling(&kv, invalid_bounds_req, now + 2).await.unwrap_err();
    assert!(err.to_string().contains("max_runners must be >= min_runners"));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts);

    // Adversarial: Builder min_instances > max_instances
    let invalid_builder_req = UpdateScalingRequest {
        role: "rust_builder".into(),
        policy: serde_json::json!({
            "min_instances": 10,
            "max_instances": 2, // INVALID
            "builder_vcpu": 16,
            "builder_ram_gib": 28,
            "container_vcpu": 16,
            "container_ram_gib": 28,
            "enable_queue_trigger": true,
            "queued_builds_threshold": 2,
            "enable_cpu_utilization_trigger": true,
            "cpu_threshold_percent": 65.0,
            "cpu_scope": "majority",
            "enable_journal_oom_trigger": true,
            "oom_lookback_minutes": 60,
            "enable_low_builds_scale_down": true,
            "scale_down_max_builds_past_hour": 2,
            "scale_down_target_instances": 1,
            "max_scale_up_hourly_usd": 3.5,
            "max_scale_up_daily_usd": 12.0,
            "max_scale_up_monthly_usd": 80.0,
            "include_disk_in_scale_budget": true,
            "shared_global_budget": false
        }),
    };
    let err = handle_post_scaling(&kv, invalid_builder_req, now + 3).await.unwrap_err();
    assert!(err.to_string().contains("max_instances must be >= min_instances"));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts);

    // Adversarial: Invalid workload roles
    for bad_role in &["gpu_worker", "storage_node", "", "CICD_RUNNER", "cicd-runner"] {
        let req = UpdateScalingRequest {
            role: bad_role.to_string(),
            policy: serde_json::json!({}),
        };
        let err = handle_post_scaling(&kv, req, now + 4).await.unwrap_err();
        assert!(err.to_string().contains("Unsupported workload role"));
        assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts);
    }

    // Adversarial: Malformed / incomplete policy schemas
    for bad_payload in &[serde_json::json!({}), serde_json::json!(null), serde_json::json!("string"), serde_json::json!({"min_runners": "not_a_number"})] {
        let req = UpdateScalingRequest {
            role: "cicd_runner".into(),
            policy: bad_payload.clone(),
        };
        assert!(handle_post_scaling(&kv, req, now + 5).await.is_err());
        assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts);
    }

    // Cumulative check: Zero writes occurred during all adversarial tests
    assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts);
}

#[tokio::test]
async fn test_kv_write_gating_stress_invalid_admin_payloads_zero_writes() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Pre-populate state
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    save_state(&kv, &mut bundle, now).await.unwrap();
    let baseline_puts = kv.put_count.load(Ordering::SeqCst);

    // 1. Non-owner actor attempts to add admin
    let non_owner_add = AddAdminRequest {
        login: "badactor".into(),
        uid: 999999,
        confirm: true,
    };
    let err = handle_post_admins_add(&kv, non_owner_add, "badactor", 999999, now + 1).await.unwrap_err();
    assert!(matches!(err, StateError::Forbidden(_)));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts);

    // 2. Add admin with confirm: false
    let unconfirmed_add = AddAdminRequest {
        login: "newadmin".into(),
        uid: 888888,
        confirm: false,
    };
    let err = handle_post_admins_add(&kv, unconfirmed_add, OWNER_LOGIN, OWNER_UID, now + 2).await.unwrap_err();
    assert!(matches!(err, StateError::Validation(_)));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts);

    // 3. Add admin with uid == 0
    let zero_uid_add = AddAdminRequest {
        login: "zero".into(),
        uid: 0,
        confirm: true,
    };
    let err = handle_post_admins_add(&kv, zero_uid_add, OWNER_LOGIN, OWNER_UID, now + 3).await.unwrap_err();
    assert!(matches!(err, StateError::Validation(_)));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts);

    // 4. Add owner as admin (uid == OWNER_UID)
    let owner_add = AddAdminRequest {
        login: OWNER_LOGIN.into(),
        uid: OWNER_UID,
        confirm: true,
    };
    let err = handle_post_admins_add(&kv, owner_add, OWNER_LOGIN, OWNER_UID, now + 4).await.unwrap_err();
    assert!(matches!(err, StateError::Conflict(_)));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts);

    // 5. Valid add -> Exactly 1 write
    let valid_add = AddAdminRequest {
        login: "alice".into(),
        uid: 10001,
        confirm: true,
    };
    let (st, _) = handle_post_admins_add(&kv, valid_add, OWNER_LOGIN, OWNER_UID, now + 5).await.unwrap();
    assert_eq!(st, 200);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), baseline_puts + 1);
    let puts_after_alice = kv.put_count.load(Ordering::SeqCst);

    // 6. Duplicate add (alice again) -> Conflict, 0 writes
    let dup_add = AddAdminRequest {
        login: "alice".into(),
        uid: 10001,
        confirm: true,
    };
    let err = handle_post_admins_add(&kv, dup_add, OWNER_LOGIN, OWNER_UID, now + 6).await.unwrap_err();
    assert!(matches!(err, StateError::Conflict(_)));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_after_alice);

    // 7. Remove admin with confirm: false -> 0 writes
    let unconf_rem = RemoveAdminRequest {
        uid: 10001,
        confirm: false,
    };
    let err = handle_post_admins_remove(&kv, unconf_rem, OWNER_LOGIN, OWNER_UID, now + 7).await.unwrap_err();
    assert!(matches!(err, StateError::Validation(_)));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_after_alice);

    // 8. Remove owner -> 0 writes
    let owner_rem = RemoveAdminRequest {
        uid: OWNER_UID,
        confirm: true,
    };
    let err = handle_post_admins_remove(&kv, owner_rem, OWNER_LOGIN, OWNER_UID, now + 8).await.unwrap_err();
    assert!(matches!(err, StateError::Validation(_)));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_after_alice);

    // 9. Remove non-existent admin -> NotFound, 0 writes
    let nonexist_rem = RemoveAdminRequest {
        uid: 99999999,
        confirm: true,
    };
    let err = handle_post_admins_remove(&kv, nonexist_rem, OWNER_LOGIN, OWNER_UID, now + 9).await.unwrap_err();
    assert!(matches!(err, StateError::NotFound(_)));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_after_alice);

    // 10. Valid remove of alice -> Exactly 1 write
    let valid_rem = RemoveAdminRequest {
        uid: 10001,
        confirm: true,
    };
    let (st_rem, _) = handle_post_admins_remove(&kv, valid_rem, OWNER_LOGIN, OWNER_UID, now + 10).await.unwrap();
    assert_eq!(st_rem, 200);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_after_alice + 1);
}

// ============================================================================
// 3. INSTANCE ACTION SAFETY GATES STRESS TESTS
// ============================================================================

#[tokio::test]
async fn test_instance_action_safety_gates_delete_never_mutates_state_and_zero_writes() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Seed state with 1 CI/CD runner host and 1 dedicated Rust builder
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.instances = vec![
        InstanceRecord {
            id: "inst-runner-100".into(),
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
        },
        InstanceRecord {
            id: "inst-builder-200".into(),
            name: "builder-dedicated-1".into(),
            zone: "us-central1-a".into(),
            region: "us-central1".into(),
            machine_type: "n4-highcpu-16".into(),
            status: "RUNNING".into(),
            role: "rust_builder".into(),
            vcpu: 16,
            ram_gib: 32,
            runners_count: 0,
            runners_count_known: true,
            runner_limits_known: true,
        },
    ];
    save_state(&kv, &mut bundle, now).await.unwrap();
    let puts_baseline = kv.put_count.load(Ordering::SeqCst);

    // 1. Delete Runner by ID
    let del_by_id = DeleteInstanceRequest {
        instance_id: Some("inst-runner-100".into()),
        instance_name: None,
    };
    let (st1, resp1) = handle_post_instances_delete(&kv, del_by_id, now + 1).await.unwrap();
    assert_eq!(st1, 200);
    assert_eq!(resp1["applied"], false);
    assert_eq!(resp1["status"], "not_applied");
    assert_eq!(resp1["message"], LIFECYCLE_ACTIONS_DISABLED_MESSAGE);
    assert!(resp1["next_step"].as_str().unwrap().contains("spot_runner_autoscaler.py"));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_baseline, "0 KV writes on delete preview");

    // 2. Delete Builder by Name
    let del_by_name = DeleteInstanceRequest {
        instance_id: None,
        instance_name: Some("builder-dedicated-1".into()),
    };
    let (st2, resp2) = handle_post_instances_delete(&kv, del_by_name, now + 2).await.unwrap();
    assert_eq!(st2, 200);
    assert_eq!(resp2["applied"], false);
    assert_eq!(resp2["status"], "not_applied");
    assert_eq!(resp2["message"], LIFECYCLE_ACTIONS_DISABLED_MESSAGE);
    assert!(resp2["next_step"].as_str().unwrap().contains("Dedicated builders keep their cache disk"));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_baseline, "0 KV writes on delete preview");

    // 3. Verify state integrity: instances are STILL present!
    let (fresh_bundle, _) = load_state(&kv, now + 3).await.unwrap();
    assert_eq!(fresh_bundle.state.instances.len(), 2, "Instances must NEVER be deleted by safety gate");
    assert_eq!(fresh_bundle.state.instances[0].id, "inst-runner-100");
    assert_eq!(fresh_bundle.state.instances[1].id, "inst-builder-200");

    // 4. Delete non-existent instance
    let del_nonexist = DeleteInstanceRequest {
        instance_id: Some("inst-ghost-999".into()),
        instance_name: None,
    };
    let err = handle_post_instances_delete(&kv, del_nonexist, now + 4).await.unwrap_err();
    assert!(matches!(err, StateError::NotFound(_)));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_baseline);
}

#[tokio::test]
async fn test_instance_action_safety_gates_scale_region_never_mutates_state_and_zero_writes() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Seed state with instances in europe-west1
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.instances = vec![
        InstanceRecord {
            id: "inst-eu-1".into(),
            name: "oxidex-runners-eu-1".into(),
            zone: "europe-west1-b".into(),
            region: "europe-west1".into(),
            machine_type: "c4d-highcpu-16".into(),
            status: "RUNNING".into(),
            role: "cicd_runner".into(),
            vcpu: 16,
            ram_gib: 32,
            runners_count: 2,
            runners_count_known: true,
            runner_limits_known: true,
        },
        InstanceRecord {
            id: "inst-eu-2".into(),
            name: "oxidex-runners-eu-2".into(),
            zone: "europe-west1-c".into(),
            region: "europe-west1".into(),
            machine_type: "c4d-highcpu-16".into(),
            status: "RUNNING".into(),
            role: "cicd_runner".into(),
            vcpu: 16,
            ram_gib: 32,
            runners_count: 2,
            runners_count_known: true,
            runner_limits_known: true,
        },
    ];
    save_state(&kv, &mut bundle, now).await.unwrap();
    let puts_baseline = kv.put_count.load(Ordering::SeqCst);

    // Test scale targets from 0 to 20
    for target in &[0, 1, 2, 5, 10, 20] {
        let scale_req = ScaleRegionRequest {
            region: "europe-west1".into(),
            target_instances: *target,
            role: "cicd_runner".into(),
        };

        let (st, resp) = handle_post_instances_scale_region(&kv, scale_req, now + 1).await.unwrap();
        assert_eq!(st, 200);
        assert_eq!(resp["status"], "not_applied");
        assert_eq!(resp["applied"], false);
        assert_eq!(resp["message"], LIFECYCLE_ACTIONS_DISABLED_MESSAGE);
        assert_eq!(resp["current_count"], 2);
        assert_eq!(resp["planned_count"], *target);
        assert_eq!(resp["would_launch"].as_array().unwrap().len(), 0);
        assert_eq!(resp["would_retire"].as_array().unwrap().len(), 0);

        // Strict assertion: 0 KV writes
        assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_baseline, "0 KV writes on scale-region preview");
    }

    // Verify state integrity: running instances in europe-west1 are still exactly 2
    let (fresh_bundle, _) = load_state(&kv, now + 2).await.unwrap();
    assert_eq!(fresh_bundle.state.instances.len(), 2);

    // Adversarial: target_instances > 20 must fail validation
    let excess_req = ScaleRegionRequest {
        region: "europe-west1".into(),
        target_instances: 21,
        role: "cicd_runner".into(),
    };
    let err = handle_post_instances_scale_region(&kv, excess_req, now + 3).await.unwrap_err();
    assert!(err.to_string().contains("exceeds maximum allowed instances (20)"));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_baseline);

    // Adversarial: unsupported role must fail validation
    let bad_role_req = ScaleRegionRequest {
        region: "europe-west1".into(),
        target_instances: 5,
        role: "unsupported_worker".into(),
    };
    let err = handle_post_instances_scale_region(&kv, bad_role_req, now + 4).await.unwrap_err();
    assert!(err.to_string().contains("Unsupported workload role"));
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_baseline);
}

// ============================================================================
// 4. RESILIENCE, SCHEMA VALIDATION & TOKEN ISOLATION
// ============================================================================

#[tokio::test]
async fn test_edge_state_corruption_and_malformed_schema_resilience() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // 1. Corrupted JSON in KV
    kv.data.lock().unwrap().insert(KV_STATE_KEY.to_string(), "not valid json {{{".to_string());
    let err = load_state(&kv, now).await.unwrap_err();
    assert!(matches!(err, StateError::Serialization(_)));

    // Health check returns 503 unavailable
    let (status, body) = handle_api_health(&kv, now).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"], UNAVAILABLE_ERROR);
    assert_eq!(body["message"], UNAVAILABLE_MESSAGE);

    // 2. Schema mismatch in KV
    invalidate_state_cache();
    let invalid_schema_bundle = serde_json::json!({
        "schema": "spot-dashboard-snapshot-v0-obsolete",
        "state_version": 1,
        "writer_slot": "edge",
        "written_at": now,
        "state_fingerprint": "abc",
        "state": default_dashboard_bundle(now).unwrap().state
    });
    kv.data.lock().unwrap().insert(KV_STATE_KEY.to_string(), invalid_schema_bundle.to_string());
    let err = load_state(&kv, now).await.unwrap_err();
    assert!(matches!(err, StateError::Validation(_)));
    assert!(err.to_string().contains("Invalid schema"));
}

#[tokio::test]
async fn test_gcp_oauth_token_cache_stress_and_clock_skew() {
    invalidate_gcp_token_cache();
    let now = 1792000000;

    // Zero KV operations: ensure mock store is not even touched
    let kv = MockKvStore::new();

    set_cached_gcp_token(now, "ya29.mock_oauth_access_token_empirically_tested".into());

    // Rapid reads: 10,000 queries within 55-minute TTL
    for offset in (0..3000).step_by(10) {
        let token = get_cached_gcp_token(now + offset);
        assert_eq!(token.as_deref(), Some("ya29.mock_oauth_access_token_empirically_tested"));
    }

    // Boundary at 3300s (55 min)
    assert!(get_cached_gcp_token(now + 3299).is_some());
    assert!(get_cached_gcp_token(now + 3300).is_none(), "Must expire at exactly 3300s");
    assert!(get_cached_gcp_token(now + 3301).is_none());

    // Clock skew resilience: saturating_sub ensures no underflow
    set_cached_gcp_token(now, "ya29.clock_skew_token".into());
    assert_eq!(get_cached_gcp_token(now.saturating_sub(500)), Some("ya29.clock_skew_token".into()));

    // KV remains completely untouched: 0 reads and 0 writes
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 0);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);
}
