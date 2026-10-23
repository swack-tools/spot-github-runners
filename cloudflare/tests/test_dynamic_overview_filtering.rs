use spot_dashboard_edge::gcp::{GceInsertInstanceRequest, GceMetadata, GceOperation};
use spot_dashboard_edge::github::RunnerBootstrapConfig;
use spot_dashboard_edge::reconciler::{
    refresh_fleet_inventory, run_inventory_refresh, run_reconciliation, set_reconciler_lease,
    GcpReconcilerClient, GitHubReconcilerClient, ReconcilerError, SimpleGceInstance,
    SimpleGitHubRunner,
};
use spot_dashboard_edge::state::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

static TEST_MUTEX: Mutex<()> = Mutex::new(());

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

fn create_sample_overview(now: u64) -> serde_json::Value {
    let now_f64 = now as f64;
    serde_json::json!({
        "project": "example-project",
        "org": "swack-tools",
        "instances_view": {
            "instances": [
                {
                    "id": "inst-run-1",
                    "instance_id": "inst-run-1",
                    "name": "oxidex-runners-swack-tools-1001",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "zone": "us-central1-a",
                    "region": "us-central1",
                    "display_location": "Council Bluffs, Iowa",
                    "machine_type": "c4d-standard-16",
                    "cpu_platform": "AMD Genoa",
                    "benchmark_score": 3400,
                    "runners_count": 2,
                    "started_epoch": now_f64 - 36000.0, // 10 hours ago
                    "deleted_epoch": serde_json::Value::Null,
                    "deletion_trigger": serde_json::Value::Null,
                    "pricing": {
                        "compute_hourly_usd": 0.12,
                        "compute_daily_usd": 2.88,
                        "compute_monthly_usd": 86.4,
                        "total_hourly_usd": 0.18,
                        "total_daily_usd": 4.32,
                        "total_monthly_usd": 129.6
                    }
                },
                {
                    "id": "inst-del-old",
                    "instance_id": "inst-del-old",
                    "name": "oxidex-runners-swack-tools-old",
                    "status": "TERMINATED",
                    "role": "cicd_runner",
                    "zone": "us-central1-b",
                    "region": "us-central1",
                    "display_location": "Council Bluffs, Iowa",
                    "machine_type": "c4d-standard-16",
                    "cpu_platform": "AMD Genoa",
                    "benchmark_score": 3400,
                    "runners_count": 2,
                    "started_epoch": now_f64 - 100000.0,
                    "deleted_epoch": now_f64 - 40000.0, // Deleted ~11.1h ago
                    "deletion_trigger": "scale_down_idle",
                    "pricing": {
                        "compute_hourly_usd": 0.12,
                        "compute_daily_usd": 2.88,
                        "compute_monthly_usd": 86.4,
                        "total_hourly_usd": 0.18,
                        "total_daily_usd": 4.32,
                        "total_monthly_usd": 129.6
                    }
                },
                {
                    "id": "inst-builder-1",
                    "instance_id": "inst-builder-1",
                    "name": "builder-dedicated-asia",
                    "status": "RUNNING",
                    "role": "rust_builder",
                    "zone": "asia-east2-a",
                    "region": "asia-east2",
                    "display_location": "Hong Kong",
                    "machine_type": "n4-highcpu-16",
                    "cpu_platform": "Intel Emerald Rapids",
                    "benchmark_score": 3850,
                    "runners_count": 0,
                    "started_epoch": now_f64 - 7200.0, // 2 hours ago
                    "deleted_epoch": serde_json::Value::Null,
                    "deletion_trigger": serde_json::Value::Null,
                    "pricing": {
                        "compute_hourly_usd": 0.20,
                        "compute_daily_usd": 4.80,
                        "compute_monthly_usd": 144.0,
                        "total_hourly_usd": 0.25,
                        "total_daily_usd": 6.00,
                        "total_monthly_usd": 180.0
                    }
                }
            ],
            "fleet_inventory": {
                "source": "gce",
                "last_success_at": chrono_iso(now),
                "last_success_epoch": now,
                "stale": false,
                "stale_reason": serde_json::Value::Null,
                "last_error": serde_json::Value::Null
            },
            "window_hours": 24.0,
            "running_count": 2,
            "deleted_count": 1,
            "total_window_spend_usd": 0.0,
            "sort_by": "total_cost_in_window_usd",
            "sort_dir": "desc"
        },
        "rust_builds": {
            "builds": [
                {
                    "build_id": "b-1",
                    "project_name": "oxidex",
                    "commit_sha": "abc12345",
                    "origin_ip": "1.2.3.4",
                    "origin_hostname": "corp.vpn",
                    "geolocation": {
                        "city": "Dallas",
                        "country": "United States",
                        "country_code": "US",
                        "asn": "AS123",
                        "isp": "TestISP"
                    },
                    "network_security": {
                        "is_tor": false,
                        "is_vpn": true,
                        "badge_text": "ProtonVPN (Clean)",
                        "provider_label": "ProtonVPN"
                    }
                }
            ],
            "total_builds": 1
        }
    })
}

#[tokio::test]
async fn test_overview_filter_include_deleted_and_zero_kv_writes() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Seed state with sample overview
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.overview = Some(create_sample_overview(now));
    save_state(&kv, &mut bundle, now).await.unwrap();
    let initial_puts = kv.put_count.load(Ordering::SeqCst);

    // 1. Query with include_deleted = false
    let query_no_deleted = OverviewQuery {
        include_deleted: Some(false),
        window_hours: Some(24.0),
        ..Default::default()
    };
    let (status, resp) = handle_api_overview(&kv, query_no_deleted, now)
        .await
        .unwrap();
    assert_eq!(status, 200);

    let iv = &resp["instances_view"];
    assert_eq!(iv["running_count"], 2);
    assert_eq!(iv["deleted_count"], 0);
    let instances = iv["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 2);
    for inst in instances {
        assert_eq!(inst["status"], "RUNNING");
    }

    // 2. Query with include_deleted = true
    let query_with_deleted = OverviewQuery {
        include_deleted: Some(true),
        window_hours: Some(24.0),
        ..Default::default()
    };
    let (status2, resp2) = handle_api_overview(&kv, query_with_deleted, now)
        .await
        .unwrap();
    assert_eq!(status2, 200);
    let iv2 = &resp2["instances_view"];
    assert_eq!(iv2["running_count"], 2);
    assert_eq!(iv2["deleted_count"], 1);
    assert_eq!(iv2["instances"].as_array().unwrap().len(), 3);

    // Verify STRICTLY zero additional KV writes occurred during queries
    assert_eq!(kv.put_count.load(Ordering::SeqCst), initial_puts);
}

#[tokio::test]
async fn test_overview_filter_window_hours_exclusion_and_overlap() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.overview = Some(create_sample_overview(now));
    save_state(&kv, &mut bundle, now).await.unwrap();

    // Query 1 hour window:
    // - inst-del-old was deleted 40000s (~11.1h) ago, so deleted_epoch < window_start (3600s ago) -> MUST BE EXCLUDED!
    // - inst-run-1 started 10h ago -> hours_in_window must be clamped to 1.0h
    // - inst-builder-1 started 2h ago -> hours_in_window must be clamped to 1.0h
    let query_1h = OverviewQuery {
        include_deleted: Some(true),
        window_hours: Some(1.0),
        ..Default::default()
    };
    let (status, resp) = handle_api_overview(&kv, query_1h, now).await.unwrap();
    assert_eq!(status, 200);

    let iv = &resp["instances_view"];
    assert_eq!(iv["window_hours"], 1.0);
    assert_eq!(iv["running_count"], 2);
    assert_eq!(iv["deleted_count"], 0); // Terminated instance excluded because it was deleted outside the 1h window
    let instances = iv["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 2);

    for inst in instances {
        assert_eq!(inst["hours_in_window"], 1.0);
    }
}

#[tokio::test]
async fn test_overview_pricing_toggle_disk_and_tax() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.overview = Some(create_sample_overview(now));
    save_state(&kv, &mut bundle, now).await.unwrap();

    // 1. With Disk & Tax included:
    // builder-dedicated-asia has total_hourly_usd: 0.25
    let q_tax = OverviewQuery {
        role: Some("rust_builder".into()),
        include_disk_and_tax: Some(true),
        window_hours: Some(2.0),
        ..Default::default()
    };
    let (_, resp_tax) = handle_api_overview(&kv, q_tax, now).await.unwrap();
    let inst_tax = &resp_tax["instances_view"]["instances"][0];
    assert_eq!(inst_tax["active_hourly_usd"], 0.25);
    assert_eq!(inst_tax["total_cost_in_window_usd"], 0.50); // 2.0h * 0.25 = 0.50

    // 2. Without Disk & Tax (Compute rate only):
    // builder-dedicated-asia has compute_hourly_usd: 0.20
    let q_notax = OverviewQuery {
        role: Some("rust_builder".into()),
        include_disk_and_tax: Some(false),
        window_hours: Some(2.0),
        ..Default::default()
    };
    let (_, resp_notax) = handle_api_overview(&kv, q_notax, now).await.unwrap();
    let inst_notax = &resp_notax["instances_view"]["instances"][0];
    assert_eq!(inst_notax["active_hourly_usd"], 0.20);
    assert_eq!(inst_notax["total_cost_in_window_usd"], 0.40); // 2.0h * 0.20 = 0.40
}

#[tokio::test]
async fn test_overview_filter_role_and_multi_field_search() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.overview = Some(create_sample_overview(now));
    save_state(&kv, &mut bundle, now).await.unwrap();

    // 1. Role filter: cicd_runner only
    let q_runner = OverviewQuery {
        role: Some("cicd_runner".into()),
        ..Default::default()
    };
    let (_, resp_r) = handle_api_overview(&kv, q_runner, now).await.unwrap();
    let r_inst = resp_r["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(r_inst.len(), 2);
    for inst in r_inst {
        assert_eq!(inst["role"], "cicd_runner");
    }

    // 2. Role filter: rust_builder only
    let q_builder = OverviewQuery {
        role: Some("rust_builder".into()),
        ..Default::default()
    };
    let (_, resp_b) = handle_api_overview(&kv, q_builder, now).await.unwrap();
    let b_inst = resp_b["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(b_inst.len(), 1);
    assert_eq!(b_inst[0]["role"], "rust_builder");

    // 3. Search filter by location / zone
    let q_search_asia = OverviewQuery {
        search: Some("asia-east2".into()),
        ..Default::default()
    };
    let (_, resp_s1) = handle_api_overview(&kv, q_search_asia, now).await.unwrap();
    assert_eq!(
        resp_s1["instances_view"]["instances"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        resp_s1["instances_view"]["instances"][0]["name"],
        "builder-dedicated-asia"
    );

    // 4. Search filter by non-matching string
    let q_search_none = OverviewQuery {
        search: Some("nonexistent-machine-spec".into()),
        ..Default::default()
    };
    let (_, resp_s2) = handle_api_overview(&kv, q_search_none, now).await.unwrap();
    assert_eq!(
        resp_s2["instances_view"]["instances"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn test_overview_sorting_ascending_and_descending() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.overview = Some(create_sample_overview(now));
    save_state(&kv, &mut bundle, now).await.unwrap();

    // Sort by name asc
    let q_sort_name_asc = OverviewQuery {
        sort_by: Some("name".into()),
        sort_dir: Some("asc".into()),
        ..Default::default()
    };
    let (_, resp_asc) = handle_api_overview(&kv, q_sort_name_asc, now)
        .await
        .unwrap();
    let inst_asc = resp_asc["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(inst_asc[0]["name"], "builder-dedicated-asia");
    assert_eq!(inst_asc[1]["name"], "oxidex-runners-swack-tools-1001");
    assert_eq!(inst_asc[2]["name"], "oxidex-runners-swack-tools-old");

    // Sort by name desc
    let q_sort_name_desc = OverviewQuery {
        sort_by: Some("name".into()),
        sort_dir: Some("desc".into()),
        ..Default::default()
    };
    let (_, resp_desc) = handle_api_overview(&kv, q_sort_name_desc, now)
        .await
        .unwrap();
    let inst_desc = resp_desc["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(inst_desc[0]["name"], "oxidex-runners-swack-tools-old");
    assert_eq!(inst_desc[1]["name"], "oxidex-runners-swack-tools-1001");
    assert_eq!(inst_desc[2]["name"], "builder-dedicated-asia");
}

#[tokio::test]
async fn test_telemetry_origin_ip_redaction_for_unauthenticated_reads() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    let ov = create_sample_overview(now);
    bundle.state.rust_builds = ov["rust_builds"].clone();
    bundle.state.overview = Some(ov);
    save_state(&kv, &mut bundle, now).await.unwrap();

    // Query overview unauthenticated: origin_ip and origin_hostname must be stripped
    let q_unauth = OverviewQuery {
        is_authenticated: Some(false),
        ..Default::default()
    };
    let (_, resp_unauth) = handle_api_overview(&kv, q_unauth, now).await.unwrap();
    let build_unauth = &resp_unauth["rust_builds"]["builds"][0];
    assert!(build_unauth.get("origin_ip").is_none());
    assert!(build_unauth.get("origin_hostname").is_none());

    // Query overview authenticated: origin_ip preserved
    let q_auth = OverviewQuery {
        is_authenticated: Some(true),
        ..Default::default()
    };
    let (_, resp_auth) = handle_api_overview(&kv, q_auth, now).await.unwrap();
    let build_auth = &resp_auth["rust_builds"]["builds"][0];
    assert_eq!(build_auth["origin_ip"], "1.2.3.4");
}

// Mock clients for testing reconciler defects
struct MockGcpFloorClient {
    instances: Vec<SimpleGceInstance>,
}

#[async_trait::async_trait(?Send)]
impl GcpReconcilerClient for MockGcpFloorClient {
    async fn list_instances(
        &self,
        _project: &str,
        _now: u64,
    ) -> Result<Vec<SimpleGceInstance>, ReconcilerError> {
        Ok(self.instances.clone())
    }

    async fn insert_instance(
        &self,
        _project: &str,
        _zone: &str,
        _req: &GceInsertInstanceRequest,
        _template: Option<&str>,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        Ok(GceOperation {
            id: Some("op-1".into()),
            name: Some("op-1".into()),
            status: "DONE".into(),
            target_link: None,
            error: None,
        })
    }

    async fn delete_instance(
        &self,
        _project: &str,
        _zone: &str,
        _instance: &str,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        Ok(GceOperation {
            id: Some("op-del-1".into()),
            name: Some("op-del-1".into()),
            status: "DONE".into(),
            target_link: None,
            error: None,
        })
    }

    async fn scrub_startup_script(
        &self,
        _project: &str,
        _zone: &str,
        _instance: &str,
        _current_metadata: &GceMetadata,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        Ok(GceOperation {
            id: Some("op-scrub-1".into()),
            name: Some("op-scrub-1".into()),
            status: "DONE".into(),
            target_link: None,
            error: None,
        })
    }
}

struct MockGitHubFloorClient;

#[async_trait::async_trait(?Send)]
impl GitHubReconcilerClient for MockGitHubFloorClient {
    async fn list_runners(
        &self,
        _target: &str,
    ) -> Result<Vec<SimpleGitHubRunner>, ReconcilerError> {
        Ok(vec![])
    }

    async fn delete_runner(&self, _target: &str, _id: u64) -> Result<(), ReconcilerError> {
        Ok(())
    }

    async fn mint_2slot_runner_configs(
        &self,
        _target: &str,
        _host_name: &str,
        _prefix: &str,
        _group_id: u32,
        _labels: &[String],
    ) -> Result<RunnerBootstrapConfig, ReconcilerError> {
        Ok(RunnerBootstrapConfig::RegistrationToken {
            token: "reg-tok".to_string(),
        })
    }

    async fn count_queued_jobs(&self, _owner: &str, _repo: &str) -> Result<u32, ReconcilerError> {
        Ok(0)
    }
}

#[tokio::test]
async fn test_floor_keeper_target_collision_org_vs_repo() {
    let _lock = TEST_MUTEX.lock().unwrap();
    set_reconciler_lease(None);
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    // Configure both org and repo targets
    bundle.state.controlled_targets =
        vec!["swack-tools".to_string(), "swack-tools/oxidex".to_string()];
    save_state(&kv, &mut bundle, now).await.unwrap();

    // GCP currently only has an active VM for the repo target swack-tools/oxidex
    let gcp = MockGcpFloorClient {
        instances: vec![SimpleGceInstance {
            id: Some("1".into()),
            name: "oxidex-runners-swack-tools-oxidex-17000-abcd".to_string(),
            zone: "us-central1-a".to_string(),
            machine_type: "c4d-standard-16".to_string(),
            status: "RUNNING".to_string(),
            metadata: spot_dashboard_edge::gcp::GceMetadata {
                fingerprint: None,
                items: vec![spot_dashboard_edge::gcp::GceMetadataItem {
                    key: "spot-target".to_string(),
                    value: "swack-tools/oxidex".to_string(),
                }],
            },
            disks: vec![],
        }],
    };
    let gh = MockGitHubFloorClient;

    let report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();

    // The floor keeper MUST launch a VM for "swack-tools" (org) because the repo VM does NOT satisfy the org floor!
    assert!(
        report.launched_hosts.iter().any(|h| h.contains("swack-tools") && !h.contains("swack-tools-oxidex")),
        "Org target swack-tools must have launched a floor host despite the presence of swack-tools/oxidex VM"
    );
    set_reconciler_lease(None);
}

#[tokio::test]
async fn test_read_only_inventory_refresh_never_mutates_fleet() {
    let _lock = TEST_MUTEX.lock().unwrap();
    set_reconciler_lease(None);
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.controlled_targets = vec!["swack-tools".to_string()];
    save_state(&kv, &mut bundle, now).await.unwrap();

    // Empty instances in GCP (if this were full reconciler, it would launch a host)
    let gcp = MockGcpFloorClient { instances: vec![] };

    let report = run_inventory_refresh(&kv, &gcp, now).await.unwrap();
    assert_eq!(report.status, "inventory_refresh_success");
    assert!(
        report.launched_hosts.is_empty(),
        "Read-only inventory refresh must NEVER launch hosts"
    );
    assert!(
        report.cleaned_vms.is_empty(),
        "Read-only inventory refresh must NEVER delete VMs"
    );
    assert!(
        report.cleaned_runners.is_empty(),
        "Read-only inventory refresh must NEVER delete runners"
    );

    // State inventory freshness was updated
    let (fresh_bundle, _) = load_state(&kv, now).await.unwrap();
    assert_eq!(fresh_bundle.state.fleet_inventory["source"], "gce");
    assert_eq!(fresh_bundle.state.fleet_inventory["stale"], false);
}

#[tokio::test]
async fn test_overview_filter_nan_and_inf_window_hours_handling() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.overview = Some(create_sample_overview(now));
    save_state(&kv, &mut bundle, now).await.unwrap();

    let query_nan = OverviewQuery {
        window_hours: Some(f64::NAN),
        ..Default::default()
    };
    let (status, resp) = handle_api_overview(&kv, query_nan, now).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(resp["instances_view"]["window_hours"], 24.0);

    let query_inf = OverviewQuery {
        window_hours: Some(f64::INFINITY),
        ..Default::default()
    };
    let (status2, resp2) = handle_api_overview(&kv, query_inf, now).await.unwrap();
    assert_eq!(status2, 200);
    assert_eq!(resp2["instances_view"]["window_hours"], 24.0);
}

#[tokio::test]
async fn test_read_only_inventory_refresh_marks_unqueried_runners_unknown() {
    let _lock = TEST_MUTEX.lock().unwrap();
    set_reconciler_lease(None);
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.instances = vec![];
    bundle.state.instances_view = vec![];
    save_state(&kv, &mut bundle, now).await.unwrap();

    // Host discovered in GCP: 1 builder and 1 runner host
    let gcp = MockGcpFloorClient {
        instances: vec![
            SimpleGceInstance {
                id: Some("1".into()),
                name: "builder-dedicated-asia".to_string(),
                zone: "asia-east2-a".to_string(),
                machine_type: "n4-highcpu-16".to_string(),
                status: "RUNNING".to_string(),
                metadata: spot_dashboard_edge::gcp::GceMetadata {
                    fingerprint: None,
                    items: vec![],
                },
                disks: vec![],
            },
            SimpleGceInstance {
                id: Some("2".into()),
                name: "oxidex-runners-swack-tools-1001".to_string(),
                zone: "us-central1-a".to_string(),
                machine_type: "c4d-standard-16".to_string(),
                status: "RUNNING".to_string(),
                metadata: spot_dashboard_edge::gcp::GceMetadata {
                    fingerprint: None,
                    items: vec![],
                },
                disks: vec![],
            },
        ],
    };

    let report = run_inventory_refresh(&kv, &gcp, now).await.unwrap();
    assert_eq!(report.status, "inventory_refresh_success");

    let (fresh_bundle, _) = load_state(&kv, now).await.unwrap();
    let builder_rec = fresh_bundle
        .state
        .instances_view
        .iter()
        .find(|i| i.name == "builder-dedicated-asia")
        .unwrap();
    assert_eq!(builder_rec.runners_count, 0);
    assert!(builder_rec.runners_count_known);

    let runner_rec = fresh_bundle
        .state
        .instances_view
        .iter()
        .find(|i| i.name == "oxidex-runners-swack-tools-1001")
        .unwrap();
    // GitHub was not queried during inventory refresh, so runner count is unknown
    assert_eq!(runner_rec.runners_count, 0);
    assert!(!runner_rec.runners_count_known);
    set_reconciler_lease(None);
}

#[tokio::test]
async fn test_inventory_refresh_diff_gating_zero_kv_writes_on_unchanged_inventory() {
    let _lock = TEST_MUTEX.lock().unwrap();
    set_reconciler_lease(None);
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.instances = vec![];
    bundle.state.instances_view = vec![];
    save_state(&kv, &mut bundle, now).await.unwrap();

    let gcp = MockGcpFloorClient {
        instances: vec![SimpleGceInstance {
            id: Some("1".into()),
            name: "builder-dedicated-asia".to_string(),
            zone: "asia-east2-a".to_string(),
            machine_type: "n4-highcpu-16".to_string(),
            status: "RUNNING".to_string(),
            metadata: spot_dashboard_edge::gcp::GceMetadata {
                fingerprint: None,
                items: vec![],
            },
            disks: vec![],
        }],
    };

    // First refresh writes state
    let report1 = run_inventory_refresh(&kv, &gcp, now).await.unwrap();
    assert!(report1.snapshot_written);
    let kv_puts_after_first = kv.put_count.load(Ordering::SeqCst);

    // Second refresh with identical inventory must diff-gate: snapshot_written = false, 0 KV puts
    let report2 = run_inventory_refresh(&kv, &gcp, now + 10).await.unwrap();
    assert!(!report2.snapshot_written);
    let kv_puts_after_second = kv.put_count.load(Ordering::SeqCst);
    assert_eq!(kv_puts_after_first, kv_puts_after_second);
}

#[tokio::test]
async fn test_refresh_fleet_inventory_fails_without_credentials() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.instances = vec![InstanceRecord {
        id: "inst-seed".into(),
        name: "oxidex-runners-c4d-seed".into(),
        zone: "us-central1-a".into(),
        region: "us-central1".into(),
        machine_type: "c4d-highcpu-16".into(),
        status: "RUNNING".into(),
        role: "cicd_runner".into(),
        vcpu: 16,
        ram_gib: 32,
        runners_count: 2,
        runners_count_known: true,
        runner_limits_known: true,
        created_at: None,
        created_epoch: None,
    }];
    save_state(&kv, &mut bundle, now).await.unwrap();

    // Absent credentials must return Err rather than silently wiping inventory with simulated provider
    let res_none = refresh_fleet_inventory(&kv, None, now).await;
    assert!(res_none.is_err());

    let res_empty = refresh_fleet_inventory(&kv, Some("   "), now).await;
    assert!(res_empty.is_err());

    let res_corrupt = refresh_fleet_inventory(&kv, Some("invalid-json"), now).await;
    assert!(res_corrupt.is_err());

    // Verify existing state was preserved untouched
    let (fresh_bundle, _) = load_state(&kv, now).await.unwrap();
    assert_eq!(fresh_bundle.state.instances.len(), 1);
    assert_eq!(
        fresh_bundle.state.instances[0].name,
        "oxidex-runners-c4d-seed"
    );
}

#[tokio::test]
async fn test_overview_filter_preserves_live_gce_provisioning_and_staging_states() {
    let now = 1792000000;
    let now_f64 = now as f64;

    let mut ov = serde_json::json!({
        "instances_view": {
            "instances": [
                {
                    "name": "oxidex-runners-c4d-staging",
                    "status": "STAGING",
                    "role": "cicd_runner",
                    "started_epoch": now_f64 - 100.0,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.18 }
                },
                {
                    "name": "oxidex-runners-c4d-provisioning",
                    "status": "PROVISIONING",
                    "role": "cicd_runner",
                    "started_epoch": now_f64 - 50.0,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.18 }
                },
                {
                    "name": "oxidex-runners-c4d-terminated",
                    "status": "TERMINATED",
                    "role": "cicd_runner",
                    "started_epoch": now_f64 - 5000.0,
                    "deleted_epoch": now_f64 - 1000.0,
                    "pricing": { "total_hourly_usd": 0.18 }
                }
            ]
        }
    });

    let query = OverviewQuery {
        include_deleted: Some(false),
        ..Default::default()
    };
    apply_dynamic_overview_filtering(&mut ov, &query, &[], now);

    let instances = ov["instances_view"]["instances"].as_array().unwrap();
    // Live non-running states (STAGING, PROVISIONING) must remain visible when include_deleted = false
    assert_eq!(instances.len(), 2);
    let names: Vec<&str> = instances
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"oxidex-runners-c4d-staging"));
    assert!(names.contains(&"oxidex-runners-c4d-provisioning"));
    assert!(!names.contains(&"oxidex-runners-c4d-terminated"));
}

#[tokio::test]
async fn test_overview_filter_retires_snapshot_rows_absent_from_live_inventory() {
    let now = 1792000000;
    let now_f64 = now as f64;

    let mut ov = serde_json::json!({
        "instances_view": {
            "instances": [
                {
                    "name": "oxidex-runners-still-alive",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "started_epoch": now_f64 - 3600.0,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.18 }
                },
                {
                    "name": "oxidex-runners-deleted-from-gce",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "started_epoch": now_f64 - 7200.0,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.18 }
                }
            ]
        }
    });

    // Live inventory only contains "oxidex-runners-still-alive"
    let live_inventory = vec![InstanceRecord {
        id: "1".into(),
        name: "oxidex-runners-still-alive".into(),
        zone: "us-central1-a".into(),
        region: "us-central1".into(),
        machine_type: "c4d-standard-16".into(),
        status: "RUNNING".into(),
        role: "cicd_runner".into(),
        vcpu: 16,
        ram_gib: 32,
        runners_count: 2,
        runners_count_known: true,
        runner_limits_known: true,
        created_at: None,
        created_epoch: None,
    }];

    // 1. With include_deleted = false, the absent VM is retired to TERMINATED and excluded from output
    let query_active_only = OverviewQuery {
        include_deleted: Some(false),
        ..Default::default()
    };
    let mut ov_clone = ov.clone();
    apply_dynamic_overview_filtering(&mut ov_clone, &query_active_only, &live_inventory, now);
    let active_instances = ov_clone["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(active_instances.len(), 1);
    assert_eq!(active_instances[0]["name"], "oxidex-runners-still-alive");

    // 2. With include_deleted = true, the absent VM is shown with TERMINATED status and deleted_epoch = now
    let query_all = OverviewQuery {
        include_deleted: Some(true),
        ..Default::default()
    };
    apply_dynamic_overview_filtering(&mut ov, &query_all, &live_inventory, now);
    let all_instances = ov["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(all_instances.len(), 2);
    let retired_vm = all_instances
        .iter()
        .find(|i| i["name"] == "oxidex-runners-deleted-from-gce")
        .unwrap();
    assert_eq!(retired_vm["status"], "TERMINATED");
    assert_eq!(retired_vm["deleted_epoch"], now_f64);
}

#[tokio::test]
async fn test_overview_filter_excludes_unrelated_project_vms() {
    let now = 1792000000;
    let now_f64 = now as f64;

    let mut ov = serde_json::json!({
        "instances_view": {
            "instances": [
                {
                    "name": "database-primary-prod",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "started_epoch": now_f64 - 3600.0,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.50 }
                },
                {
                    "name": "oxidex-runners-eu-1",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "started_epoch": now_f64 - 3600.0,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.18 }
                },
                {
                    "name": "builder-dedicated-asia",
                    "status": "RUNNING",
                    "role": "rust_builder",
                    "started_epoch": now_f64 - 3600.0,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.25 }
                }
            ]
        }
    });

    let live_inventory = vec![
        InstanceRecord {
            id: "db-1".into(),
            name: "unrelated-bastion-host".into(),
            zone: "us-central1-a".into(),
            region: "us-central1".into(),
            machine_type: "e2-micro".into(),
            status: "RUNNING".into(),
            role: "cicd_runner".into(),
            vcpu: 2,
            ram_gib: 2,
            runners_count: 0,
            runners_count_known: false,
            runner_limits_known: false,
            created_at: None,
            created_epoch: None,
        },
        InstanceRecord {
            id: "runner-1".into(),
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
            created_at: None,
            created_epoch: None,
        },
    ];

    let query = OverviewQuery {
        include_deleted: Some(true),
        ..Default::default()
    };
    apply_dynamic_overview_filtering(&mut ov, &query, &live_inventory, now);

    let instances = ov["instances_view"]["instances"].as_array().unwrap();
    let names: Vec<&str> = instances
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();

    // database-primary-prod and unrelated-bastion-host must NOT appear in instances
    assert!(!names.contains(&"database-primary-prod"));
    assert!(!names.contains(&"unrelated-bastion-host"));
    assert!(names.contains(&"oxidex-runners-eu-1"));
    assert!(names.contains(&"builder-dedicated-asia"));
}

#[tokio::test]
async fn test_overview_filter_preserves_observed_hardware_or_unknown() {
    let now = 1792000000;
    let mut ov = serde_json::json!({
        "instances_view": {
            "instances": []
        },
        "regions": [
            {
                "region": "asia-east2",
                "qualified_types": [
                    {
                        "machine_type": "n4-highcpu-16",
                        "cpu_platform": "Intel Emerald Rapids",
                        "benchmark_score": 4100,
                        "data_disk_gb": 100,
                        "data_disk_type": "hyperdisk-balanced",
                        "disk_category": "Hyperdisk"
                    }
                ]
            }
        ]
    });

    let live_inventory = vec![
        InstanceRecord {
            id: "1".into(),
            name: "builder-dedicated-asia".into(),
            zone: "asia-east2-a".into(),
            region: "asia-east2".into(),
            machine_type: "n4-highcpu-16".into(),
            status: "RUNNING".into(),
            role: "rust_builder".into(),
            vcpu: 16,
            ram_gib: 32,
            runners_count: 0,
            runners_count_known: true,
            runner_limits_known: true,
            created_at: None,
            created_epoch: None,
        },
        InstanceRecord {
            id: "2".into(),
            name: "oxidex-runners-custom-hw".into(),
            zone: "us-west1-a".into(),
            region: "us-west1".into(),
            machine_type: "custom-unconfigured-16".into(),
            status: "RUNNING".into(),
            role: "cicd_runner".into(),
            vcpu: 16,
            ram_gib: 32,
            runners_count: 2,
            runners_count_known: true,
            runner_limits_known: true,
            created_at: None,
            created_epoch: None,
        },
    ];

    let query = OverviewQuery {
        include_deleted: Some(true),
        ..Default::default()
    };
    apply_dynamic_overview_filtering(&mut ov, &query, &live_inventory, now);

    let instances = ov["instances_view"]["instances"].as_array().unwrap();

    let builder_inst = instances
        .iter()
        .find(|i| i["name"] == "builder-dedicated-asia")
        .unwrap();
    // Observed hardware from catalog
    assert_eq!(builder_inst["cpu_platform"], "Intel Emerald Rapids");
    assert_eq!(builder_inst["benchmark_score"], 4100);
    assert_eq!(builder_inst["data_disk_gb"], 100);

    let unconfigured_inst = instances
        .iter()
        .find(|i| i["name"] == "oxidex-runners-custom-hw")
        .unwrap();
    // Unobserved hardware must be explicitly Unknown/null, not fabricated Intel Cascade Lake
    assert_eq!(unconfigured_inst["cpu_platform"], "Unknown");
    assert!(unconfigured_inst["benchmark_score"].is_null());
    assert!(unconfigured_inst["data_disk_gb"].is_null());
}

#[tokio::test]
async fn test_inventory_refresh_skips_when_reconciler_lease_active() {
    let _lock = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    // Simulate an active reconciler lease
    set_reconciler_lease(Some(now + 120));

    let gcp = MockGcpFloorClient { instances: vec![] };
    let report = run_inventory_refresh(&kv, &gcp, now).await.unwrap();

    assert_eq!(report.status, "skipped_lease_active");
    assert!(!report.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // Release lease
    set_reconciler_lease(None);
}

#[tokio::test]
async fn test_overview_filter_clears_deletion_markers_when_instance_is_live_again() {
    let now = 1792000000;
    let mut ov = serde_json::json!({
        "instances_view": {
            "instances": [
                {
                    "name": "oxidex-runners-restarted-vm",
                    "status": "TERMINATED",
                    "zone": "us-central1-a",
                    "region": "us-central1",
                    "machine_type": "c4d-standard-16",
                    "started_epoch": (now - 7200) as f64,
                    "deleted_epoch": (now - 3600) as f64,
                    "deleted_at": "2026-10-22T18:00:00Z",
                    "deletion_trigger": "preempted",
                    "pricing": {
                        "hourly_usd": 0.25,
                        "total_hourly_usd": 0.28
                    }
                }
            ]
        }
    });

    let live_inventory = vec![InstanceRecord {
        id: "restarted-1".to_string(),
        name: "oxidex-runners-restarted-vm".to_string(),
        zone: "us-central1-a".to_string(),
        region: "us-central1".to_string(),
        machine_type: "c4d-standard-16".to_string(),
        status: "RUNNING".to_string(),
        role: "cicd_runner".to_string(),
        vcpu: 16,
        ram_gib: 32,
        runners_count: 2,
        runners_count_known: false,
        runner_limits_known: false,
        created_at: None,
        created_epoch: None,
    }];

    // Query with include_deleted = false
    let query = OverviewQuery {
        include_deleted: Some(false),
        ..Default::default()
    };
    apply_dynamic_overview_filtering(&mut ov, &query, &live_inventory, now);

    let instances = ov["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(
        instances.len(),
        1,
        "Restarted instance must not be filtered out as deleted"
    );

    let restarted = &instances[0];
    assert_eq!(restarted["name"], "oxidex-runners-restarted-vm");
    assert_eq!(restarted["status"], "RUNNING");
    assert!(
        restarted["deleted_epoch"].is_null(),
        "deleted_epoch must be cleared"
    );
    assert!(
        restarted["deleted_at"].is_null(),
        "deleted_at must be cleared"
    );
    assert!(
        restarted["deletion_trigger"].is_null(),
        "deletion_trigger must be cleared"
    );
}

#[tokio::test]
async fn test_reconciler_normalizes_zone_and_machine_type_and_resets_reused_vm_id() {
    let _lock = TEST_MUTEX.lock().unwrap();
    set_reconciler_lease(None);
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    // Seed existing instance with old GCE ID "old-id-123" and old created timestamp
    bundle.state.instances = vec![InstanceRecord {
        id: "old-id-123".to_string(),
        name: "oxidex-runners-reused-name".to_string(),
        zone: "us-central1-a".to_string(),
        region: "us-central1".to_string(),
        machine_type: "c4d-standard-16".to_string(),
        status: "RUNNING".to_string(),
        role: "cicd_runner".to_string(),
        vcpu: 16,
        ram_gib: 32,
        runners_count: 2,
        runners_count_known: true,
        runner_limits_known: true,
        created_at: Some("2026-01-01T00:00:00Z".to_string()),
        created_epoch: Some(1767225600),
    }];
    save_state(&kv, &mut bundle, now).await.unwrap();

    // GCE responds with resource-qualified zone and machine type, and a NEW GCE ID "new-id-456"
    let gcp = MockGcpFloorClient {
        instances: vec![SimpleGceInstance {
            id: Some("new-id-456".to_string()),
            name: "oxidex-runners-reused-name".to_string(),
            zone: "zones/us-central1-a".to_string(),
            machine_type: "machineTypes/c4d-standard-16".to_string(),
            status: "RUNNING".to_string(),
            metadata: spot_dashboard_edge::gcp::GceMetadata {
                fingerprint: None,
                items: vec![],
            },
            disks: vec![],
        }],
    };

    let report = run_inventory_refresh(&kv, &gcp, now).await.unwrap();
    assert_eq!(report.status, "inventory_refresh_success");

    let (fresh_bundle, _) = load_state(&kv, now).await.unwrap();
    assert_eq!(fresh_bundle.state.instances_view.len(), 1);
    let refreshed = &fresh_bundle.state.instances_view[0];

    // Resource names are normalized
    assert_eq!(refreshed.zone, "us-central1-a");
    assert_eq!(refreshed.region, "us-central1");
    assert_eq!(refreshed.machine_type, "c4d-standard-16");

    // Reused VM name with new GCE ID had identity and start reset
    assert_eq!(refreshed.id, "new-id-456");
    assert_ne!(
        refreshed.created_at,
        Some("2026-01-01T00:00:00Z".to_string())
    );
}

#[test]
fn test_dynamic_overview_filtering_resets_reused_vm_id_and_start_time() {
    let now = 1792000000;
    let old_epoch = (now - 86400) as f64;
    let new_epoch = (now - 1800) as f64;

    let mut ov = serde_json::json!({
        "instances_view": {
            "instances": [
                {
                    "id": "old-gce-id-111",
                    "instance_id": "old-gce-id-111",
                    "name": "oxidex-runners-reused-worker",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "zone": "us-central1-a",
                    "region": "us-central1",
                    "machine_type": "c4d-standard-16",
                    "started_epoch": old_epoch,
                    "started_at": chrono_iso(old_epoch as u64),
                    "pricing": {
                        "compute_hourly_usd": 0.12,
                        "total_hourly_usd": 0.18
                    }
                }
            ]
        }
    });

    let live_inventory = vec![InstanceRecord {
        id: "new-gce-id-222".to_string(),
        name: "oxidex-runners-reused-worker".to_string(),
        zone: "us-central1-a".to_string(),
        region: "us-central1".to_string(),
        machine_type: "c4d-standard-16".to_string(),
        status: "RUNNING".to_string(),
        role: "cicd_runner".to_string(),
        vcpu: 16,
        ram_gib: 32,
        runners_count: 2,
        runners_count_known: true,
        runner_limits_known: true,
        created_at: Some(chrono_iso(new_epoch as u64)),
        created_epoch: Some(new_epoch as u64),
    }];

    let query = OverviewQuery::default();
    apply_dynamic_overview_filtering(&mut ov, &query, &live_inventory, now);

    let instances = ov["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);
    let vm = &instances[0];

    assert_eq!(vm["id"], "new-gce-id-222");
    assert_eq!(vm["instance_id"], "new-gce-id-222");
    assert_eq!(vm["started_epoch"], new_epoch);
    assert_eq!(vm["started_at"], chrono_iso(new_epoch as u64));
    assert!(ov["pricing"].is_object());
    assert_eq!(ov["pricing"]["running_runner_slots"], 2);
    assert_eq!(ov["pricing"]["active_instances_count"], 1);
}

#[test]
fn test_parse_launch_epoch_from_name_decodes_base36_suffix() {
    assert_eq!(
        parse_launch_epoch_from_name("oxidex-runners-useast1-c-rkcv-tn5zecc6cas"),
        Some(1_792_429_428)
    );
    assert_eq!(
        parse_launch_epoch_from_name("oxidex-runners-uswest1-a-grks-tn7iyfbjr5l"),
        Some(1_792_501_431)
    );
    assert_eq!(
        parse_launch_epoch_from_name("oxidex-runners-uswest1-b-awwi-tna9fanbqld"),
        Some(1_792_629_046)
    );
    // Plain words must not be misidentified as base36 epochs
    assert_eq!(
        parse_launch_epoch_from_name("oxidex-runners-c4d-staging"),
        None
    );
}

#[tokio::test]
async fn test_reconciler_persists_deleted_epoch_in_overview_and_ages_out_of_window() {
    let _lock = TEST_MUTEX.lock().unwrap();
    set_reconciler_lease(None);
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let t0 = 1_792_629_046u64;

    let (mut bundle, _) = load_state(&kv, t0).await.unwrap();
    bundle.state.instances = vec![
        InstanceRecord {
            id: "1".into(),
            name: "oxidex-runners-useast1-c-rkcv-tn5zecc6cas".into(),
            zone: "us-east1-c".into(),
            region: "us-east1".into(),
            machine_type: "n4d-standard-4".into(),
            status: "RUNNING".into(),
            role: "cicd_runner".into(),
            vcpu: 4,
            ram_gib: 16,
            runners_count: 2,
            runners_count_known: true,
            runner_limits_known: true,
            created_at: Some(chrono_iso(t0 - 36000)),
            created_epoch: Some(t0 - 36000),
        },
        InstanceRecord {
            id: "2".into(),
            name: "oxidex-runners-uswest1-b-awwi-tna9fanbqld".into(),
            zone: "us-west1-b".into(),
            region: "us-west1".into(),
            machine_type: "n4d-standard-4".into(),
            status: "RUNNING".into(),
            role: "cicd_runner".into(),
            vcpu: 4,
            ram_gib: 16,
            runners_count: 2,
            runners_count_known: true,
            runner_limits_known: true,
            created_at: Some(chrono_iso(t0)),
            created_epoch: Some(t0),
        },
    ];
    bundle.state.instances_view = bundle.state.instances.clone();
    bundle.state.overview = Some(serde_json::json!({
        "instances_view": {
            "instances": [
                {
                    "id": "1",
                    "instance_id": "1",
                    "name": "oxidex-runners-useast1-c-rkcv-tn5zecc6cas",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "zone": "us-east1-c",
                    "region": "us-east1",
                    "display_location": "Moncks Corner, South Carolina, USA",
                    "machine_type": "n4d-standard-4",
                    "started_epoch": (t0 - 36000) as f64,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.0691 }
                },
                {
                    "id": "2",
                    "instance_id": "2",
                    "name": "oxidex-runners-uswest1-b-awwi-tna9fanbqld",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "zone": "us-west1-b",
                    "region": "us-west1",
                    "display_location": "The Dalles, Oregon, USA",
                    "machine_type": "n4d-standard-4",
                    "started_epoch": t0 as f64,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.0675 }
                }
            ]
        }
    }));
    save_state(&kv, &mut bundle, t0).await.unwrap();

    // At t_del = t0 + 3600 (1h after start), awwi is preempted and gone from GCE
    let t_del = t0 + 3600;
    let gcp = MockGcpFloorClient {
        instances: vec![SimpleGceInstance {
            id: Some("1".into()),
            name: "oxidex-runners-useast1-c-rkcv-tn5zecc6cas".into(),
            zone: "zones/us-east1-c".into(),
            machine_type: "machineTypes/n4d-standard-4".into(),
            status: "RUNNING".into(),
            metadata: spot_dashboard_edge::gcp::GceMetadata {
                fingerprint: None,
                items: vec![],
            },
            disks: vec![],
        }],
    };
    let report = run_inventory_refresh(&kv, &gcp, t_del).await.unwrap();
    assert!(report.snapshot_written);

    // Verify deleted_epoch was persisted in bundle.state.overview in KV
    let (saved_bundle, _) = load_state(&kv, t_del).await.unwrap();
    let saved_ov = saved_bundle.state.overview.as_ref().unwrap();
    let saved_awwi = saved_ov["instances_view"]["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "oxidex-runners-uswest1-b-awwi-tna9fanbqld")
        .unwrap();
    assert_eq!(saved_awwi["status"], "TERMINATED");
    assert_eq!(saved_awwi["deleted_epoch"], t_del as f64);

    // 2 hours after deletion (t_later = t_del + 7200), query a 1h window with include_deleted = true:
    // awwi must be excluded because deleted_epoch < window_start!
    let t_later = t_del + 7200;
    let q_1h = OverviewQuery {
        include_deleted: Some(true),
        window_hours: Some(1.0),
        ..Default::default()
    };
    let (_, resp_1h) = handle_api_overview(&kv, q_1h, t_later).await.unwrap();
    let iv_1h = &resp_1h["instances_view"];
    assert_eq!(iv_1h["running_count"], 1);
    assert_eq!(iv_1h["deleted_count"], 0);
    assert_eq!(iv_1h["instances"].as_array().unwrap().len(), 1);
    assert_eq!(
        iv_1h["instances"][0]["name"],
        "oxidex-runners-useast1-c-rkcv-tn5zecc6cas"
    );

    // Query a 24h window at t_later: awwi is included and its runtime is frozen at 1.0h (not 3.0h!)
    let q_24h = OverviewQuery {
        include_deleted: Some(true),
        window_hours: Some(24.0),
        ..Default::default()
    };
    let (_, resp_24h) = handle_api_overview(&kv, q_24h, t_later).await.unwrap();
    let awwi_24h = resp_24h["instances_view"]["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "oxidex-runners-uswest1-b-awwi-tna9fanbqld")
        .unwrap();
    assert_eq!(awwi_24h["runtime_hours"], 1.0);
}

#[tokio::test]
async fn test_self_heals_untimestamped_zombie_and_normalizes_urls_and_supports_zero_window() {
    let _lock = TEST_MUTEX.lock().unwrap();
    set_reconciler_lease(None);
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1_792_718_740u64;
    let awwi_start = 1_792_629_113u64; // > 24.8h ago

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    // Simulate KV state where instances_view has raw GCE URLs for 2 running VMs and awwi is already gone from instances,
    // but overview still has awwi as RUNNING with deleted_epoch = null
    bundle.state.instances = vec![
        InstanceRecord {
            id: "6471737508688692579".into(),
            name: "oxidex-runners-useast1-c-rkcv-tn5zecc6cas".into(),
            zone: "https://www.googleapis.com/compute/v1/projects/homelab-424523/zones/us-east1-c"
                .into(),
            region: "https://www.googleapis.com/compute/v1/projects/homelab-424523/zones/us".into(),
            machine_type: "https://www.googleapis.com/compute/v1/projects/homelab-424523/zones/us-east1-c/machineTypes/n4d-standard-4".into(),
            status: "RUNNING".into(),
            role: "cicd_runner".into(),
            vcpu: 4,
            ram_gib: 16,
            runners_count: 2,
            runners_count_known: true,
            runner_limits_known: true,
            created_at: None,
            created_epoch: None,
        },
        InstanceRecord {
            id: "6484907347288971327".into(),
            name: "oxidex-runners-uswest1-a-grks-tn7iyfbjr5l".into(),
            zone: "https://www.googleapis.com/compute/v1/projects/homelab-424523/zones/us-west1-a"
                .into(),
            region: "https://www.googleapis.com/compute/v1/projects/homelab-424523/zones/us".into(),
            machine_type: "https://www.googleapis.com/compute/v1/projects/homelab-424523/zones/us-west1-a/machineTypes/n4d-standard-4".into(),
            status: "RUNNING".into(),
            role: "cicd_runner".into(),
            vcpu: 4,
            ram_gib: 16,
            runners_count: 2,
            runners_count_known: true,
            runner_limits_known: true,
            created_at: None,
            created_epoch: None,
        },
    ];
    bundle.state.instances_view = bundle.state.instances.clone();
    bundle.state.overview = Some(serde_json::json!({
        "regions": [
            {
                "region": "us-east1",
                "display_location": "Moncks Corner, South Carolina, USA"
            },
            {
                "region": "us-west1",
                "display_location": "The Dalles, Oregon, USA"
            }
        ],
        "instances_view": {
            "instances": [
                {
                    "id": "6471737508688692579",
                    "name": "oxidex-runners-useast1-c-rkcv-tn5zecc6cas",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "zone": "https://www.googleapis.com/compute/v1/projects/homelab-424523/zones/us-east1-c",
                    "region": "us-east1",
                    "display_location": "Moncks Corner, South Carolina, USA",
                    "machine_type": "https://www.googleapis.com/compute/v1/projects/homelab-424523/zones/us-east1-c/machineTypes/n4d-standard-4",
                    "started_epoch": (now - 289000) as f64,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.0691 }
                },
                {
                    "id": "2715653080635285543",
                    "name": "oxidex-runners-uswest1-b-awwi-tna9fanbqld",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "zone": "us-west1-b",
                    "region": "us-west1",
                    "display_location": "us-west1",
                    "machine_type": "n4d-standard-4",
                    "started_epoch": awwi_start as f64,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.0675 }
                }
            ]
        }
    }));
    save_state(&kv, &mut bundle, now).await.unwrap();

    // 1. Immediate read with 1h window excludes the stale awwi zombie and normalizes URLs
    let q_1h = OverviewQuery {
        include_deleted: Some(true),
        window_hours: Some(1.0),
        ..Default::default()
    };
    let (_, resp_1h) = handle_api_overview(&kv, q_1h, now).await.unwrap();
    let insts_1h = resp_1h["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(insts_1h.len(), 2);
    for inst in insts_1h {
        assert_ne!(inst["name"], "oxidex-runners-uswest1-b-awwi-tna9fanbqld");
        assert_eq!(inst["machine_type"], "n4d-standard-4");
        assert!(!inst["zone"].as_str().unwrap().contains('/'));
        assert!(!inst["region"].as_str().unwrap().contains('/'));
    }
    let grks = insts_1h
        .iter()
        .find(|i| i["name"] == "oxidex-runners-uswest1-a-grks-tn7iyfbjr5l")
        .unwrap();
    assert_eq!(grks["zone"], "us-west1-a");
    assert_eq!(grks["region"], "us-west1");
    assert_eq!(grks["display_location"], "The Dalles, Oregon, USA");

    // 2. Window of 0.0 hours ("Now (0h)") strictly excludes all terminated instances
    let q_0h = OverviewQuery {
        include_deleted: Some(true),
        window_hours: Some(0.0),
        ..Default::default()
    };
    let (_, resp_0h) = handle_api_overview(&kv, q_0h, now).await.unwrap();
    assert_eq!(resp_0h["instances_view"]["window_hours"], 0.0);
    assert_eq!(resp_0h["instances_view"]["deleted_count"], 0);
    assert_eq!(resp_0h["instances_view"]["running_count"], 2);
}

#[tokio::test]
async fn test_zero_fleet_inventory_refresh_retires_last_running_instance_in_overview() {
    let _lock = TEST_MUTEX.lock().unwrap();
    set_reconciler_lease(None);
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1_792_000_000u64;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.instances = vec![InstanceRecord {
        id: "last-1".into(),
        name: "oxidex-runners-last-standing".into(),
        zone: "us-central1-a".into(),
        region: "us-central1".into(),
        machine_type: "c4d-standard-16".into(),
        status: "RUNNING".into(),
        role: "cicd_runner".into(),
        vcpu: 16,
        ram_gib: 32,
        runners_count: 2,
        runners_count_known: true,
        runner_limits_known: true,
        created_at: Some(chrono_iso(now - 3600)),
        created_epoch: Some(now - 3600),
    }];
    bundle.state.instances_view = bundle.state.instances.clone();
    bundle.state.overview = Some(serde_json::json!({
        "instances_view": {
            "instances": [
                {
                    "id": "last-1",
                    "name": "oxidex-runners-last-standing",
                    "status": "RUNNING",
                    "role": "cicd_runner",
                    "zone": "us-central1-a",
                    "region": "us-central1",
                    "machine_type": "c4d-standard-16",
                    "started_epoch": (now - 3600) as f64,
                    "deleted_epoch": serde_json::Value::Null,
                    "pricing": { "total_hourly_usd": 0.18 }
                }
            ]
        }
    }));
    save_state(&kv, &mut bundle, now).await.unwrap();

    // GCE returns 0 instances (last VM was deleted)
    let gcp = MockGcpFloorClient { instances: vec![] };
    let report = run_inventory_refresh(&kv, &gcp, now + 600).await.unwrap();
    assert!(report.snapshot_written);

    // Query overview with include_deleted = false: 0 running instances
    let q_active = OverviewQuery {
        include_deleted: Some(false),
        ..Default::default()
    };
    let (_, resp_active) = handle_api_overview(&kv, q_active, now + 600).await.unwrap();
    assert_eq!(resp_active["instances_view"]["running_count"], 0);
    assert_eq!(
        resp_active["instances_view"]["instances"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn test_arm64_first_class_catalog_enrichment_and_arch_filtering() {
    let _lock = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1_792_000_000u64;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    let mut ov = create_sample_overview(now);
    // Add an ARM64 running instance alongside the existing x86_64 instances
    ov["instances_view"]["instances"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "id": "inst-arm-1",
            "instance_id": "inst-arm-1",
            "name": "oxidex-runners-arm64-axion-1",
            "status": "RUNNING",
            "role": "cicd_runner",
            "zone": "us-central1-a",
            "region": "us-central1",
            "display_location": "Council Bluffs, Iowa",
            "machine_type": "c4a-highcpu-16",
            "cpu_platform": "Google Axion (Neoverse V2)",
            "benchmark_score": 2150,
            "runners_count": 2,
            "started_epoch": (now - 3600) as f64,
            "deleted_epoch": serde_json::Value::Null,
            "deletion_trigger": serde_json::Value::Null,
            "pricing": {
                "compute_hourly_usd": 0.1404,
                "compute_daily_usd": 3.37,
                "compute_monthly_usd": 101.09,
                "total_hourly_usd": 0.165,
                "total_daily_usd": 3.96,
                "total_monthly_usd": 118.80
            }
        }));
    ov["qualifications"] = serde_json::json!({
        "cicd_runner": {
            "hardware_qualifying": [],
            "spot_price_qualifying": []
        },
        "rust_builder": {
            "hardware_qualifying": [],
            "spot_price_qualifying": []
        }
    });
    ov["regions"] = serde_json::json!([
        {
            "region": "us-central1",
            "zones": ["us-central1-a", "us-central1-b"],
            "families": ["c4d", "n4"],
            "cpu_platforms": ["AMD Turin"],
            "disk_types": ["hyperdisk-balanced", "pd-balanced"],
            "running_instances": []
        }
    ]);
    ov["vm_templates"] = serde_json::json!([]);
    bundle.state.overview = Some(ov);
    save_state(&kv, &mut bundle, now).await.unwrap();

    // 1. Default query (`arch = all`) includes both ARM64 and x86_64 instances and enriches ARM64 catalog
    let (_, resp_all) = handle_api_overview(&kv, OverviewQuery::default(), now)
        .await
        .unwrap();
    let qual_runner = resp_all["qualifications"]["cicd_runner"]["spot_price_qualifying"]
        .as_array()
        .unwrap();
    assert!(qual_runner.iter().any(|o| o["machine_type"] == "c4a-highcpu-16" && o["arch"] == "arm64"));
    assert!(qual_runner.iter().any(|o| o["machine_type"] == "n4a-highcpu-8" && o["arch"] == "arm64"));
    assert!(qual_runner.iter().any(|o| o["machine_type"] == "t2a-standard-8" && o["arch"] == "arm64"));

    // Verify VM templates include ARM64 runner and builder presets
    let vm_tpls = resp_all["vm_templates"].as_array().unwrap();
    assert!(vm_tpls.iter().any(|t| t["id"] == "tpl-runner-c4a-highcpu16" && t["arch"] == "arm64"));
    assert!(vm_tpls.iter().any(|t| t["id"] == "tpl-builder-c4a-highcpu16" && t["arch"] == "arm64"));

    // Verify region metadata includes c4a/n4a/t2a families
    let reg_fams = resp_all["regions"][0]["families"].as_array().unwrap();
    assert!(reg_fams.iter().any(|f| f == "c4a"));
    assert!(reg_fams.iter().any(|f| f == "n4a"));
    assert!(reg_fams.iter().any(|f| f == "t2a"));

    // 2. Query with `?arch=arm64` filters instances and qualification offers to ARM64 only
    let q_arm = OverviewQuery {
        arch: Some("arm64".into()),
        include_deleted: Some(true),
        ..Default::default()
    };
    let (_, resp_arm) = handle_api_overview(&kv, q_arm, now).await.unwrap();
    let arm_insts = resp_arm["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(arm_insts.len(), 1);
    assert_eq!(arm_insts[0]["machine_type"], "c4a-highcpu-16");
    assert_eq!(arm_insts[0]["arch"], "arm64");

    let arm_offers = resp_arm["qualifications"]["cicd_runner"]["spot_price_qualifying"]
        .as_array()
        .unwrap();
    assert!(!arm_offers.is_empty());
    for offer in arm_offers {
        assert_eq!(offer["arch"], "arm64");
    }
}

#[test]
fn test_btad_arm64_manifest_and_gce_vm_request_builder() {
    use spot_dashboard_edge::gcp::{
        build_spot_runner_vm_request, RunnerVmConfig, DEBIAN_13_ARM64_IMAGE_FAMILY,
    };
    use spot_dashboard_edge::patch::{is_supported_patch_target, validate_patch_manifest};
    use spot_dashboard_edge::patch_console::make_valid_manifest_json;

    // 1. `swack-tools/bt-to-all-devices` (btad) is supported and defaults to ARM64 on-demand C4A
    assert!(is_supported_patch_target("swack-tools/bt-to-all-devices"));
    let btad_manifest = make_valid_manifest_json("swack-tools/bt-to-all-devices");
    let patch = validate_patch_manifest(&btad_manifest).expect("btad ARM64 manifest must be valid");
    assert_eq!(patch.hardware.arch, "arm64");
    assert_eq!(
        patch.hardware.provisioning_mode.as_deref(),
        Some("on_demand")
    );
    assert!(patch.hardware.isa.contains(&"neon".to_string()));
    assert!(patch
        .hardware
        .extra_labels
        .contains(&"type-c4a-highcpu-8".to_string()));

    // 2. GCE VM request builder selects debian-13-arm64 and hyperdisk-balanced + GVNIC for C4A/N4A
    let req = build_spot_runner_vm_request(
        "test-proj",
        &RunnerVmConfig {
            instance_name: "oxidex-runners-btad-arm64-1",
            machine_type: "c4a-highcpu-16",
            zone: "us-central1-a",
            target: "swack-tools/bt-to-all-devices",
            runner_prefix: "oxidex-runners",
            startup_script: "#!/bin/bash\necho arm64",
            patch_sha256: "abc123",
            patch_json: "{}",
            runner_1_jitconfig: None,
            runner_2_jitconfig: None,
            source_image: None,
            golden_image_family: None,
            boot_disk_type: None,
        },
    )
    .expect("c4a-highcpu-16 VM request should succeed");

    let boot_init = req.disks[0].initialize_params.as_ref().unwrap();
    assert_eq!(
        boot_init.source_image.as_deref(),
        Some(DEBIAN_13_ARM64_IMAGE_FAMILY)
    );
    assert!(boot_init.disk_type.ends_with("/diskTypes/hyperdisk-balanced"));
    assert_eq!(
        req.network_interfaces[0].nic_type.as_deref(),
        Some("GVNIC")
    );
}

#[tokio::test]
async fn test_dynamic_overview_filtering_by_org_and_repo_target() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 2_000_000_000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    let mut ov = create_sample_overview(now);
    if let Some(arr) = ov["instances_view"]["instances"].as_array_mut() {
        arr.push(serde_json::json!({
            "id": "inst-rustnzb-1",
            "name": "oxidex-runners-xbmc4lyfe-rustnzb-2000000000-abcd",
            "zone": "us-central1-a",
            "region": "us-central1",
            "display_location": "Iowa",
            "machine_type": "c4d-highcpu-16",
            "status": "RUNNING",
            "role": "cicd_runner",
            "vcpu": 16,
            "ram_gib": 32,
            "runners_count": 2,
            "started_epoch": (now - 3600) as f64,
            "active_hourly_usd": 0.18
        }));
    }
    bundle.state.overview = Some(ov);
    save_state(&kv, &mut bundle, now).await.unwrap();

    // 1. Default ("all") returns all instances and populates available_targets + inst.target
    let (_, resp_all) = handle_api_overview(&kv, OverviewQuery::default(), now)
        .await
        .unwrap();
    let avail = resp_all["available_targets"].as_array().unwrap();
    assert!(avail.iter().any(|t| t == "swack-tools"));
    assert!(avail.iter().any(|t| t == "xbmc4lyfe/rustnzb"));
    let all_insts = resp_all["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(all_insts.len(), 4);

    // 2. Single repo filter ("xbmc4lyfe/rustnzb")
    let q_rustnzb = OverviewQuery {
        target: Some("xbmc4lyfe/rustnzb".into()),
        ..Default::default()
    };
    let (_, resp_rustnzb) = handle_api_overview(&kv, q_rustnzb, now).await.unwrap();
    let rustnzb_insts = resp_rustnzb["instances_view"]["instances"]
        .as_array()
        .unwrap();
    assert_eq!(rustnzb_insts.len(), 1);
    assert_eq!(rustnzb_insts[0]["target"], "xbmc4lyfe/rustnzb");

    // 3. Multi-select filter ("swack-tools,xbmc4lyfe/rustnzb")
    let q_multi = OverviewQuery {
        target: Some("swack-tools,xbmc4lyfe/rustnzb".into()),
        ..Default::default()
    };
    let (_, resp_multi) = handle_api_overview(&kv, q_multi, now).await.unwrap();
    assert_eq!(
        resp_multi["instances_view"]["instances"]
            .as_array()
            .unwrap()
            .len(),
        4
    );

    // 4. Empty selection ("__none__" when user reverses from all-checked or unchecks All)
    let q_none = OverviewQuery {
        target: Some("__none__".into()),
        ..Default::default()
    };
    let (_, resp_none) = handle_api_overview(&kv, q_none, now).await.unwrap();
    assert_eq!(
        resp_none["instances_view"]["instances"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(resp_none["instances_view"]["running_count"], 0);
}

#[tokio::test]
async fn test_overview_filter_by_max_latency_ms() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    let mut ov = create_sample_overview(now);
    ov["qualifications"] = serde_json::json!({
        "cicd_runner": {
            "hardware_qualifying": [],
            "spot_price_qualifying": []
        },
        "rust_builder": {
            "hardware_qualifying": [],
            "spot_price_qualifying": []
        }
    });
    ov["regions"] = serde_json::json!([
        {
            "region": "us-central1",
            "zones": ["us-central1-a", "us-central1-b"],
            "latency_ms": 18,
            "enabled": true,
            "running_instances": []
        },
        {
            "region": "asia-east2",
            "zones": ["asia-east2-a"],
            "latency_ms": 148,
            "enabled": true,
            "running_instances": []
        }
    ]);
    bundle.state.overview = Some(ov);
    save_state(&kv, &mut bundle, now).await.unwrap();

    // Filter with max_latency_ms = 20.0 -> only us-central1 (18 ms) instances and offers qualify
    let q_20ms = OverviewQuery {
        max_latency_ms: Some(20.0),
        include_deleted: Some(false),
        ..Default::default()
    };
    let (_, resp_20ms) = handle_api_overview(&kv, q_20ms, now).await.unwrap();
    let insts = resp_20ms["instances_view"]["instances"].as_array().unwrap();
    assert_eq!(insts.len(), 1);
    assert_eq!(insts[0]["region"], "us-central1");
    assert_eq!(insts[0]["latency_ms"], 18.0);

    let qual_spot = resp_20ms["qualifications"]["cicd_runner"]["spot_price_qualifying"]
        .as_array()
        .unwrap();
    for offer in qual_spot {
        let lat = offer["latency_ms"].as_f64().unwrap();
        assert!(lat <= 20.0, "expected offer latency <= 20ms, got {lat}");
    }
}

#[tokio::test]
async fn test_sanitize_legacy_oxidex_templates_and_ssh_previews() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1792000000;

    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    let mut ov = create_sample_overview(now);
    ov["cloud_init_templates"] = serde_json::json!([
        {
            "id": "ci-runner-strict-pool",
            "name": "CI/CD Strict Pool",
            "role": "cicd_runner",
            "content": "#!/usr/bin/env bash\nLABELS=\"self-hosted,linux,x64,gcp-spot,oxidex-strict\"\ndocker run --label oxidex.strict-runner=true oxidex-self-hosted-runner:2.337.0\n",
            "parsed": {
                "stages": ["docker run --label oxidex.strict-runner=true oxidex-self-hosted-runner:2.337.0"]
            }
        }
    ]);
    ov["qualifications"] = serde_json::json!({
        "cicd_runner": {
            "hardware_qualifying": [],
            "spot_price_qualifying": [
                {
                    "region": "us-central1",
                    "machine_type": "c4-highcpu-16",
                    "ssh_command_preview": "gcloud compute ssh oxidex-runners-uscentral1-c4h16 --zone=us-central1-a",
                    "pricing": { "compute_hourly_usd": 0.14 }
                }
            ]
        },
        "rust_builder": {
            "hardware_qualifying": [],
            "spot_price_qualifying": [
                {
                    "region": "us-central1",
                    "machine_type": "n4-highcpu-16",
                    "ssh_command_preview": "gcloud compute ssh builder-oxidex-uscentral1-n4h16 --zone=us-central1-a",
                    "pricing": { "compute_hourly_usd": 0.16 }
                }
            ]
        }
    });
    ov["instances_view"]["instances"][0]["registration_target"] =
        serde_json::json!("xbmc4lyfe/newznab-tmux");
    bundle.state.overview = Some(ov);
    save_state(&kv, &mut bundle, now).await.unwrap();

    let (_, resp) = handle_api_overview(&kv, OverviewQuery::default(), now)
        .await
        .unwrap();

    let tpl_str = serde_json::to_string(&resp["cloud_init_templates"]).unwrap();
    assert!(
        !tpl_str.contains("oxidex"),
        "expected cloud_init_templates to have zero oxidex strings, got: {tpl_str}"
    );
    let qual_str = serde_json::to_string(&resp["qualifications"]).unwrap();
    assert!(
        !qual_str.contains("oxidex"),
        "expected qualifications to have zero oxidex strings, got: {qual_str}"
    );

    let insts = resp["instances_view"]["instances"].as_array().unwrap();
    let newznab_inst = insts
        .iter()
        .find(|i| i["name"] == "oxidex-runners-swack-tools-1001")
        .unwrap();
    assert_eq!(newznab_inst["target"], "xbmc4lyfe/newznab-tmux");
}




