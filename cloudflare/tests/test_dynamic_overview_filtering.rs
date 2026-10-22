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
        "project": "homelab-424523",
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
