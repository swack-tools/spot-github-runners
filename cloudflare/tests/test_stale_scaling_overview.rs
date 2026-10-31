//! Regression tests for the stale scaling view left over from the retired
//! Python control plane: with the fleet at zero, the overview kept showing
//! "HOLD (3 runners)", 14 runs/1h and $0.15/hr, and the Rust build queue
//! reported one active builder that did not exist.

use spot_dashboard_edge::state::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

static TEST_MUTEX: Mutex<()> = Mutex::new(());

pub struct MockKvStore {
    pub data: Mutex<HashMap<String, String>>,
    pub put_count: AtomicUsize,
}

impl MockKvStore {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            put_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for MockKvStore {
    async fn get(&self, key: &str) -> Result<Option<String>, StateError> {
        Ok(self.data.lock().unwrap().get(key).cloned())
    }

    async fn put(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.put_count.fetch_add(1, Ordering::SeqCst);
        self.data
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
}

/// The precomputed overview as the Python control plane last wrote it, before
/// the fleet went to zero (values copied from the live dashboard).
fn legacy_overview() -> serde_json::Value {
    let legacy_eval = serde_json::json!({
        "role": "cicd_runner",
        "direction": "hold",
        "current_instances": 3,
        "current_runners": 3,
        "desired_instances": 3,
        "desired_runners": 3,
        "projected_hourly_usd": 0.1542,
        "projected_daily_usd": 3.7,
        "projected_monthly_usd": 112.57,
        "budget_capped": true,
        "budget_cap_note": "Scale-up capped at 3 instance(s)",
        "hold_reason": "The live inventory is not current",
        "scale_up_reasons": [],
        "scale_down_reasons": []
    });
    serde_json::json!({
        "updated_at": "2026-10-22T03:20:33Z",
        "scaling_evaluation": legacy_eval,
        "builder_scaling_evaluation": {
            "role": "rust_builder",
            "direction": "hold",
            "current_instances": 0,
            "desired_instances": 0,
            "projected_hourly_usd": 0,
            "scale_up_reasons": [],
            "scale_down_reasons": []
        },
        "scaling_evaluations": { "cicd_runner": legacy_eval },
        "scaling_telemetry": {
            "current_instances": 3,
            "current_runners": 3,
            "jobs_ran_past_hour": 14,
            "provisioned_jobs": 7,
            "queued_jobs": 3,
            "running_instance_hourly_rates": [0.0522, 0.0522, 0.0498],
            "instance_cpu_percentages": [],
            "journal_oom_events": [],
            "github_oom_runs": []
        },
        "instances_view": {
            "instances": [],
            "active_hourly_burn_usd": 0.2041,
            "active_daily_burn_usd": 4.9,
            "active_monthly_burn_usd": 148.99,
            "running_runner_slots": 3,
            "running_runner_vms": 3,
            "running_builder_vms": 0
        },
        "regions": []
    })
}

async fn seed_zero_fleet(kv: &MockKvStore, now: u64) {
    invalidate_state_cache();
    let (mut bundle, _) = load_state(kv, now).await.unwrap();
    let state = &mut bundle.state;
    state.overview = Some(legacy_overview());
    state.instances.clear();
    state.instances_view.clear();
    state.scaling.cicd_runner.min_runners = 0;
    state.scaling.cicd_runner.max_runners = 0;
    state.scaling.rust_builder.min_instances = 0;
    state.scaling.rust_builder.max_instances = 0;
    // The struct copy also carries the legacy counters.
    state.scaling_telemetry.current_runners = 0;
    state.scaling_telemetry.queued_jobs = 0;
    state.scaling_telemetry.jobs_ran_past_hour = 14;
    state.scaling_telemetry.provisioned_jobs = 7;
    state.rust_builds = serde_json::json!({ "queue": [], "builds": [] });
    save_state(kv, &mut bundle, now).await.unwrap();
    invalidate_state_cache();
}

#[tokio::test]
async fn overview_scaling_view_follows_live_zero_fleet() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let kv = MockKvStore::new();
    let now = 1_793_378_454;
    seed_zero_fleet(&kv, now).await;
    let puts_before = kv.put_count.load(Ordering::SeqCst);

    let (status, ov) = handle_api_overview(&kv, OverviewQuery::default(), now)
        .await
        .unwrap();
    assert_eq!(status, 200);

    let se = &ov["scaling_evaluation"];
    assert_eq!(se["source"], "worker");
    assert_eq!(se["direction"], "hold");
    assert_eq!(se["current_instances"], 0);
    assert_eq!(se["current_runners"], 0);
    assert_eq!(se["desired_runners"], 0);
    assert_eq!(se["budget_capped"], false);
    assert_eq!(se["projected_hourly_usd"], 0.0);
    assert!(se["hold_reason"]
        .as_str()
        .unwrap()
        .contains("Scaling is off"));
    assert_eq!(ov["scaling_evaluations"]["cicd_runner"], *se);

    let bev = &ov["scaling_evaluations"]["rust_builder"];
    assert_eq!(bev["direction"], "hold");
    assert_eq!(bev["current_instances"], 0);
    assert_eq!(bev["desired_instances"], 0);
    assert_eq!(ov["builder_scaling_evaluation"], *bev);

    let st = &ov["scaling_telemetry"];
    assert_eq!(st["current_instances"], 0);
    assert_eq!(st["current_runners"], 0);
    assert_eq!(st["queued_jobs"], 0);
    assert!(st["jobs_ran_past_hour"].is_null());
    assert!(st["provisioned_jobs"].is_null());
    assert_eq!(st["running_instance_hourly_rates"], serde_json::json!([]));
    assert_eq!(st["current_builder_instances"], 0);

    let iv = &ov["instances_view"];
    assert_eq!(iv["active_hourly_burn_usd"], 0.0);
    assert_eq!(iv["running_runner_vms"], 0);
    assert_eq!(iv["running_runner_slots"], 0);

    // Reading the overview never writes KV.
    assert_eq!(kv.put_count.load(Ordering::SeqCst), puts_before);
}

#[tokio::test]
async fn overview_reports_floor_scale_up_from_zero() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let kv = MockKvStore::new();
    let now = 1_793_378_454;
    seed_zero_fleet(&kv, now).await;
    {
        let (mut bundle, _) = load_state(&kv, now).await.unwrap();
        bundle.state.scaling.cicd_runner.min_runners = 2;
        bundle.state.scaling.cicd_runner.max_runners = 8;
        bundle.state.controlled_targets = vec!["swack-tools".to_string()];
        save_state(&kv, &mut bundle, now).await.unwrap();
        invalidate_state_cache();
    }

    let (_, ov) = handle_api_overview(&kv, OverviewQuery::default(), now)
        .await
        .unwrap();
    let se = &ov["scaling_evaluation"];
    assert_eq!(se["direction"], "scale_up");
    assert_eq!(se["current_instances"], 0);
    assert_eq!(se["desired_instances"], 1);
    assert_eq!(se["desired_runners"], 2);
    assert_eq!(se["scale_up_reasons"][0]["trigger"], "floor");
    // No live VM to price from, so the projection is unreported, not $0.
    assert!(se["projected_hourly_usd"].is_null());
}

#[tokio::test]
async fn rust_build_queue_does_not_invent_a_builder() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let kv = MockKvStore::new();
    let now = 1_793_378_454;
    seed_zero_fleet(&kv, now).await;

    let (status, body) = handle_get_rust_builds_queue(&kv, true, now).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body["active_builders"], 0);
    assert_eq!(body["target_builders"], 0);
    assert_eq!(body["scaling_decision"], "HOLD");
    assert!(body["scaling_summary"]
        .as_str()
        .unwrap()
        .starts_with("Hold at 0 builders"));
}

#[tokio::test]
async fn rust_build_queue_counts_live_builders() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let kv = MockKvStore::new();
    let now = 1_793_378_454;
    seed_zero_fleet(&kv, now).await;
    {
        let (mut bundle, _) = load_state(&kv, now).await.unwrap();
        let builder = |name: &str, status: &str| InstanceRecord {
            id: name.to_string(),
            name: name.to_string(),
            zone: "us-central1-a".to_string(),
            region: "us-central1".to_string(),
            machine_type: "n4-highcpu-16".to_string(),
            status: status.to_string(),
            role: "rust_builder".to_string(),
            vcpu: 16,
            ram_gib: 32,
            runners_count: 0,
            runners_count_known: true,
            runner_limits_known: true,
            created_at: None,
            created_epoch: None,
        };
        bundle.state.instances_view = vec![
            builder("builder-a", "RUNNING"),
            builder("builder-b", "RUNNING"),
            builder("builder-c", "TERMINATED"),
        ];
        save_state(&kv, &mut bundle, now).await.unwrap();
        invalidate_state_cache();
    }

    let (_, body) = handle_get_rust_builds_queue(&kv, true, now).await.unwrap();
    assert_eq!(body["active_builders"], 2);
    assert_eq!(body["target_builders"], 2);
}
