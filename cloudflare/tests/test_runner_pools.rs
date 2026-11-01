//! Runner pools: validation, the admin handlers, and the reconciler keeping pools at size.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use spot_dashboard_edge::gcp::{
    GceInsertInstanceRequest, GceMetadata, GceMetadataItem, GceOperation,
};
use spot_dashboard_edge::github::RunnerBootstrapConfig;
use spot_dashboard_edge::patch::RunnerPatch;
use spot_dashboard_edge::reconciler::*;
use spot_dashboard_edge::runner_pools::*;
use spot_dashboard_edge::state::{
    default_dashboard_bundle, invalidate_state_cache, save_state, CloudInitTemplate,
    DashboardBundle, DashboardState, KvStoreBackend, StateError, KV_STATE_KEY,
};

// ============================================================================
// Test doubles
// ============================================================================

#[derive(Default)]
struct Kv {
    data: Mutex<HashMap<String, String>>,
    puts: AtomicUsize,
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for Kv {
    async fn get(&self, key: &str) -> Result<Option<String>, StateError> {
        Ok(self.data.lock().unwrap().get(key).cloned())
    }
    async fn put(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.data
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
}

impl Kv {
    fn bundle(&self) -> DashboardBundle {
        serde_json::from_str(self.data.lock().unwrap().get(KV_STATE_KEY).unwrap()).unwrap()
    }
    fn pools(&self) -> Vec<RunnerPool> {
        self.data
            .lock()
            .unwrap()
            .get(KV_RUNNER_POOLS_KEY)
            .map(|raw| serde_json::from_str(raw).unwrap())
            .unwrap_or_default()
    }
    fn store_pools(&self, pools: &[RunnerPool]) {
        self.data.lock().unwrap().insert(
            KV_RUNNER_POOLS_KEY.to_string(),
            serde_json::to_string(pools).unwrap(),
        );
    }
    fn store(&self, bundle: &DashboardBundle) {
        self.data.lock().unwrap().insert(
            KV_STATE_KEY.to_string(),
            serde_json::to_string(bundle).unwrap(),
        );
    }
}

#[derive(Default, Clone)]
struct Gcp {
    instances: Arc<Mutex<Vec<SimpleGceInstance>>>,
    inserted: Arc<Mutex<Vec<(String, GceInsertInstanceRequest)>>>,
    deleted: Arc<Mutex<Vec<String>>>,
    fail_list: Arc<Mutex<bool>>,
    fail_inserts: Arc<Mutex<bool>>,
}

fn op() -> GceOperation {
    GceOperation {
        id: Some("op".into()),
        name: Some("op".into()),
        status: "DONE".into(),
        target_link: None,
        error: None,
    }
}

#[async_trait::async_trait(?Send)]
impl GcpReconcilerClient for Gcp {
    async fn list_instances(
        &self,
        _project: &str,
        _now: u64,
    ) -> Result<Vec<SimpleGceInstance>, ReconcilerError> {
        if *self.fail_list.lock().unwrap() {
            return Err(ReconcilerError::Gcp("list failed".into()));
        }
        Ok(self.instances.lock().unwrap().clone())
    }
    async fn insert_instance(
        &self,
        _project: &str,
        zone: &str,
        req: &GceInsertInstanceRequest,
        _t: Option<&str>,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        if *self.fail_inserts.lock().unwrap() {
            return Err(ReconcilerError::Gcp(
                "GCE API error (status 403): iam.serviceAccounts.actAs".into(),
            ));
        }
        self.inserted
            .lock()
            .unwrap()
            .push((zone.to_string(), req.clone()));
        Ok(op())
    }
    async fn delete_instance(
        &self,
        _project: &str,
        _zone: &str,
        instance: &str,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        self.deleted.lock().unwrap().push(instance.to_string());
        Ok(op())
    }
    async fn scrub_startup_script(
        &self,
        _project: &str,
        _zone: &str,
        _instance: &str,
        _m: &GceMetadata,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        Ok(op())
    }
}

#[derive(Default, Clone)]
struct Gh {
    runners: Arc<Mutex<HashMap<String, Vec<SimpleGitHubRunner>>>>,
    deleted: Arc<Mutex<Vec<u64>>>,
    minted: Arc<Mutex<Vec<(String, u32, Vec<String>)>>>,
    fail_list: Arc<Mutex<bool>>,
}

#[async_trait::async_trait(?Send)]
impl GitHubReconcilerClient for Gh {
    async fn list_runners(&self, path: &str) -> Result<Vec<SimpleGitHubRunner>, ReconcilerError> {
        if *self.fail_list.lock().unwrap() {
            return Err(ReconcilerError::Github("list failed".into()));
        }
        Ok(self
            .runners
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_default())
    }
    async fn delete_runner(&self, _path: &str, id: u64) -> Result<(), ReconcilerError> {
        self.deleted.lock().unwrap().push(id);
        Ok(())
    }
    async fn mint_2slot_runner_configs(
        &self,
        path: &str,
        _vm: &str,
        _prefix: &str,
        group_id: u32,
        labels: &[String],
    ) -> Result<RunnerBootstrapConfig, ReconcilerError> {
        self.minted
            .lock()
            .unwrap()
            .push((path.to_string(), group_id, labels.to_vec()));
        Ok(RunnerBootstrapConfig::RegistrationToken {
            token: "AREGTOKEN123".into(),
        })
    }
    async fn count_queued_jobs(&self, _o: &str, _r: &str) -> Result<u32, ReconcilerError> {
        Ok(0)
    }
    async fn fetch_target_patch(&self, _t: &str) -> Result<Option<RunnerPatch>, ReconcilerError> {
        Ok(None)
    }
}

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
const NOW: u64 = 1_793_500_000;

/// State with the floor and backlog scale-up off, so only pools launch anything.
fn base_state() -> DashboardBundle {
    let mut b = default_dashboard_bundle(NOW).unwrap();
    b.state.scaling.cicd_runner.min_runners = 0;
    b.state.scaling.cicd_runner.max_runners = 0;
    b.state.scaling.rust_builder.enable_queue_trigger = false;
    b.state.global_budget.max_hourly_usd = 5.0;
    b
}

fn axion_request(target: &str) -> CreateRunnerPoolRequest {
    CreateRunnerPoolRequest {
        target: target.into(),
        machine_type: "c4a-highcpu-16".into(),
        zone: "us-west1-a".into(),
        hosts: 1,
        runners_per_host: 2,
        runner_vcpu: 7,
        runner_ram_gib: 14,
        labels: vec!["rust".into(), "big".into()],
        cloud_init_template_id: String::new(),
    }
}

fn template(id: &str, role: &str, content: &str) -> CloudInitTemplate {
    CloudInitTemplate {
        id: id.into(),
        name: format!("{} template", id),
        role: role.into(),
        description: String::new(),
        updated_at: String::new(),
        content: content.into(),
    }
}

fn host(name: &str, pool: &str, status: &str) -> SimpleGceInstance {
    SimpleGceInstance {
        id: Some(format!("id-{}", name)),
        name: name.into(),
        zone: "zones/us-west1-a".into(),
        machine_type: "zones/us-west1-a/machineTypes/c4a-highcpu-16".into(),
        status: status.into(),
        metadata: GceMetadata {
            fingerprint: None,
            items: vec![
                GceMetadataItem {
                    key: POOL_METADATA_KEY.into(),
                    value: pool.into(),
                },
                GceMetadataItem {
                    key: "spot-target".into(),
                    value: "swack-tools".into(),
                },
            ],
        },
        disks: vec![],
    }
}

fn startup_script(req: &GceInsertInstanceRequest) -> String {
    req.metadata.get("startup-script").unwrap().to_string()
}

// ============================================================================
// Validation
// ============================================================================

#[test]
fn accepts_the_axion_row_with_two_runners() {
    let state = base_state().state;
    let pool =
        validate_create_request(&axion_request("swack-tools"), &state, "owner", NOW).unwrap();
    assert_eq!(pool.id.len(), POOL_ID_LEN);
    assert!(pool.id.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(pool.machine_type, "c4a-highcpu-16");
    assert_eq!(pool.labels, vec!["rust".to_string(), "big".to_string()]);
    assert!(pool.host_hourly_usd.unwrap() > 0.0);
    assert_eq!(
        pool_labels(&pool),
        vec![
            "self-hosted",
            "Linux",
            "ARM64",
            "spot",
            "c4a-highcpu-16",
            "us-west1-a",
            "rust",
            "big"
        ]
    );
}

#[test]
fn rejects_runners_that_do_not_fit_the_host() {
    let state = base_state().state;
    let mut req = axion_request("swack-tools");
    req.runner_ram_gib = 16; // 2 x 16 = 32 > 32 - 2
    let err = validate_create_request(&req, &state, "o", NOW).unwrap_err();
    assert!(err.contains("32 GiB"), "{}", err);
    req.runner_ram_gib = 15; // 2 x 15 = 30 fits exactly
    assert!(validate_create_request(&req, &state, "o", NOW).is_ok());

    let mut req = axion_request("swack-tools");
    req.runner_vcpu = 9; // 18 > 16
    assert!(validate_create_request(&req, &state, "o", NOW).is_err());

    let mut req = axion_request("swack-tools");
    req.runners_per_host = 1;
    req.runner_ram_gib = 30;
    req.runner_vcpu = 16;
    assert!(validate_create_request(&req, &state, "o", NOW).is_ok());
}

#[test]
fn rejects_unknown_shapes_bad_targets_zones_and_labels() {
    let state = base_state().state;
    let cases: Vec<Box<dyn Fn(&mut CreateRunnerPoolRequest)>> = vec![
        Box::new(|r| r.machine_type = "n4-standard-16".into()),
        Box::new(|r| r.machine_type = "e2-micro".into()),
        Box::new(|r| r.target = "swack-tools; rm -rf /".into()),
        Box::new(|r| r.target = "a/b/c".into()),
        Box::new(|r| r.target = "".into()),
        Box::new(|r| r.target = "-bad".into()),
        Box::new(|r| r.target = "owner/..".into()),
        Box::new(|r| r.zone = "us-west1".into()),
        Box::new(|r| r.zone = "us-west1-a'; echo".into()),
        Box::new(|r| r.labels = vec!["has space".into()]),
        Box::new(|r| r.labels = vec!["it's".into()]),
        Box::new(|r| r.labels = vec!["X64".into()]), // ARM64 host
        Box::new(|r| r.labels = (0..11).map(|i| format!("l{}", i)).collect()),
        Box::new(|r| r.hosts = 0),
        Box::new(|r| r.hosts = 11),
        Box::new(|r| r.runners_per_host = 0),
        Box::new(|r| r.runners_per_host = 9),
        Box::new(|r| r.runner_vcpu = 0),
        Box::new(|r| r.cloud_init_template_id = "missing".into()),
    ];
    for (n, mutate) in cases.iter().enumerate() {
        let mut req = axion_request("swack-tools");
        mutate(&mut req);
        assert!(
            validate_create_request(&req, &state, "o", NOW).is_err(),
            "case {} should be refused: {:?}",
            n,
            req
        );
    }
}

#[test]
fn accepts_repositories_and_other_orgs() {
    let state = base_state().state;
    for target in ["xbmc4lyfe/rustnzb", "some-org", "Owner/Repo.name_1"] {
        assert!(
            validate_create_request(&axion_request(target), &state, "o", NOW).is_ok(),
            "{}",
            target
        );
    }
    assert_eq!(runner_group_for("swack-tools"), ("swack-runners", 10));
    assert_eq!(runner_group_for("xbmc4lyfe/rustnzb"), ("", 1));
    assert_eq!(runner_group_for("other-org"), ("", 1));
}

#[test]
fn builtin_labels_are_not_duplicated_and_x86_gets_x64() {
    let state = base_state().state;
    let mut req = axion_request("swack-tools");
    req.machine_type = "c4d-highcpu-16".into();
    req.zone = "us-central1-a".into();
    req.labels = vec![
        "SPOT".into(),
        "self-hosted".into(),
        "rust".into(),
        "Rust".into(),
    ];
    let pool = validate_create_request(&req, &state, "o", NOW).unwrap();
    assert_eq!(pool.labels, vec!["rust".to_string()]);
    assert_eq!(pool_labels(&pool)[2], "X64");
    req.labels = vec!["arm64".into()];
    assert!(validate_create_request(&req, &state, "o", NOW).is_err());
}

#[test]
fn templates_must_be_runner_shell_scripts() {
    let mut state = base_state().state;
    state.cloud_init_templates = vec![
        template(
            "ok",
            "cicd_runner",
            "#!/bin/bash\napt-get install -y protobuf-compiler\n",
        ),
        template("yaml", "cicd_runner", "#cloud-config\npackages: [jq]\n"),
        template("builder", "rust_builder", "#!/bin/bash\necho hi\n"),
        template(
            "huge",
            "cicd_runner",
            &"x".repeat(MAX_SETUP_SCRIPT_BYTES + 1),
        ),
    ];
    let mut req = axion_request("swack-tools");
    req.cloud_init_template_id = "ok".into();
    let pool = validate_create_request(&req, &state, "o", NOW).unwrap();
    assert_eq!(pool.cloud_init_template_name, "ok template");
    assert!(pool.setup_script.contains("protobuf-compiler"));
    for bad in ["yaml", "builder", "huge"] {
        req.cloud_init_template_id = bad.into();
        let err = validate_create_request(&req, &state, "o", NOW).unwrap_err();
        assert!(err.contains("can't be used"), "{}: {}", bad, err);
    }
}

#[test]
fn pools_together_stay_under_the_hourly_budget() {
    let mut state = base_state().state;
    state.global_budget.max_hourly_usd = 0.10;
    let one = validate_create_request(&axion_request("swack-tools"), &state, "o", NOW).unwrap();
    state.runner_pools.push(one);
    let err = validate_create_request(&axion_request("swack-tools"), &state, "o", NOW).unwrap_err();
    assert!(err.contains("budget"), "{}", err);
    // A pool being removed no longer counts.
    state.runner_pools[0].removing = true;
    assert!(validate_create_request(&axion_request("swack-tools"), &state, "o", NOW).is_ok());
}

#[test]
fn every_launchable_machine_type_is_allowlisted_and_sized() {
    for mt in launchable_machine_types() {
        let (vcpu, ram) = machine_shape(mt).unwrap();
        assert!(vcpu >= 8 && ram >= 16, "{}", mt);
    }
    assert!(launchable_machine_types().contains(&"c4a-highcpu-16"));
    assert!(!launchable_machine_types().contains(&"n4-standard-16"));
}

// ============================================================================
// Handlers
// ============================================================================

#[tokio::test]
async fn create_list_and_remove_round_trip() {
    let _g = lock();
    invalidate_state_cache();
    let kv = Kv::default();
    kv.store(&base_state());

    let view = handle_create_runner_pool(&kv, axion_request("swack-tools"), "swackhamer", NOW)
        .await
        .unwrap();
    let id = view["created"].as_str().unwrap().to_string();
    assert_eq!(view["pools"][0]["id"], id.as_str());
    assert_eq!(view["pools"][0]["arch"], "arm64");
    assert_eq!(view["pools"][0]["created_by"], "swackhamer");
    // The list never carries the setup script itself.
    assert!(view["pools"][0].get("setup_script").is_none());

    let listed = handle_get_runner_pools(&kv, NOW).await.unwrap();
    assert_eq!(listed["pools"].as_array().unwrap().len(), 1);
    assert!(listed["machine_types"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["machine_type"] == "c4a-highcpu-16" && m["usable_ram_gib"] == 30));
    assert!(listed["targets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t == "swack-tools"));

    let removed = handle_remove_runner_pool(&kv, RemoveRunnerPoolRequest { id: id.clone() }, NOW)
        .await
        .unwrap();
    assert_eq!(removed["pools"][0]["removing"], true);
    assert!(kv.pools()[0].removing);

    let missing = handle_remove_runner_pool(
        &kv,
        RemoveRunnerPoolRequest {
            id: "nope00".into(),
        },
        NOW,
    )
    .await;
    assert!(matches!(missing, Err(StateError::NotFound(_))));
}

#[tokio::test]
async fn create_refuses_invalid_requests_without_writing() {
    let _g = lock();
    invalidate_state_cache();
    let kv = Kv::default();
    kv.store(&base_state());
    let mut req = axion_request("swack-tools");
    req.runner_ram_gib = 64;
    let res = handle_create_runner_pool(&kv, req, "o", NOW).await;
    assert!(matches!(res, Err(StateError::Validation(_))));
    assert_eq!(kv.puts.load(Ordering::SeqCst), 0);
}

// ============================================================================
// Reconciler
// ============================================================================

async fn seed_pool(
    kv: &Kv,
    state_edit: impl FnOnce(&mut DashboardState),
    req: CreateRunnerPoolRequest,
) -> String {
    let mut b = base_state();
    state_edit(&mut b.state);
    kv.store(&b);
    let view = handle_create_runner_pool(kv, req, "o", NOW).await.unwrap();
    view["created"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn reconciler_launches_a_missing_pool_host_on_its_shape_and_zone() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(
        &kv,
        |s| {
            s.cloud_init_templates = vec![template(
                "tpl",
                "cicd_runner",
                "#!/bin/bash\napt-get install -y protobuf-compiler\n",
            )]
        },
        CreateRunnerPoolRequest {
            cloud_init_template_id: "tpl".into(),
            ..axion_request("swack-tools")
        },
    )
    .await;
    let gcp = Gcp::default();
    let gh = Gh::default();

    let report = run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    assert_eq!(report.launched_hosts.len(), 1, "{:?}", report);
    let inserted = gcp.inserted.lock().unwrap().clone();
    let (zone, req) = &inserted[0];
    assert_eq!(zone, "us-west1-a");
    assert!(req.name.starts_with(&pool_host_name_prefix(&id)));
    assert!(req.name.len() + "-data".len() <= 63, "{}", req.name);
    assert_eq!(
        req.machine_type,
        "zones/us-west1-a/machineTypes/c4a-highcpu-16"
    );
    assert_eq!(req.metadata.get(POOL_METADATA_KEY), Some(id.as_str()));
    assert_eq!(req.metadata.get("spot-runner-count"), Some("2"));
    assert_eq!(req.labels.get(POOL_METADATA_KEY), Some(&id));
    assert_eq!(req.scheduling.provisioning_model, "SPOT");
    // Arm64 image and Hyperdisk for C4A.
    let boot = req.disks.iter().find(|d| d.boot).unwrap();
    let init = boot.initialize_params.as_ref().unwrap();
    assert!(init
        .source_image
        .as_deref()
        .unwrap()
        .contains("debian-13-arm64"));
    assert!(init.disk_type.ends_with("hyperdisk-balanced"));

    let script = startup_script(req);
    assert!(script.contains("RUNNER_COUNT=2"));
    assert!(script.contains("RUNNER_MEMORY_GB=14"));
    assert!(script.contains("RUNNER_CPU_LIMIT='7'"));
    assert!(script.contains("--argjson work_gib 28"));
    assert!(script.contains("RUNNER_GROUP='swack-runners'"));
    assert!(script.contains("ORG_URL=https://github.com/swack-tools"));
    assert!(script.contains("REGISTRATION_TOKEN=AREGTOKEN123"));
    assert!(script.contains(
        "RUNNER_LABELS='self-hosted,Linux,ARM64,spot,c4a-highcpu-16,us-west1-a,rust,big'"
    ));
    // The template runs inside the runner image build.
    let marker = "printf '%s' '";
    let start = script.find("printf '%s' '").unwrap() + marker.len();
    let dockerfile_b64 = &script[start..script[start..].find('\'').unwrap() + start];
    let dockerfile = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(dockerfile_b64)
            .unwrap(),
    )
    .unwrap();
    assert!(
        dockerfile.contains("base64 -d | bash -euo pipefail"),
        "{}",
        dockerfile
    );

    let (path, group, labels) = gh.minted.lock().unwrap()[0].clone();
    assert_eq!(path, "orgs/swack-tools");
    assert_eq!(group, 10);
    assert!(labels.contains(&"ARM64".to_string()));

    let status = &kv.bundle().state.runner_pool_status[&id];
    assert_eq!(status.last_launch_host.as_deref(), Some(req.name.as_str()));
    assert!(status.last_error.is_none());
    assert!(!kv.pools().is_empty());
}

#[tokio::test]
async fn reconciler_does_not_relaunch_a_full_pool_or_act_on_a_failed_list() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("xbmc4lyfe/rustnzb")).await;
    let gcp = Gcp::default();
    let gh = Gh::default();

    // A failed instance list must not read as "no hosts".
    *gcp.fail_list.lock().unwrap() = true;
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    assert!(r.launched_hosts.is_empty());
    assert!(gcp.inserted.lock().unwrap().is_empty());

    // A pool already at its host count launches nothing; preempted hosts don't count.
    *gcp.fail_list.lock().unwrap() = false;
    let name = format!("{}{}-1", pool_host_name_prefix(&id), NOW);
    gcp.instances
        .lock()
        .unwrap()
        .push(host(&name, &id, "RUNNING"));
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 400).await.unwrap();
    assert!(r.launched_hosts.is_empty(), "{:?}", r);

    gcp.instances.lock().unwrap()[0].status = "TERMINATED".into();
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 800).await.unwrap();
    assert_eq!(r.launched_hosts.len(), 1);
    let (_, req) = gcp.inserted.lock().unwrap()[0].clone();
    // Repository runners: repo path and no runner group.
    assert_eq!(gh.minted.lock().unwrap()[0].0, "repos/xbmc4lyfe/rustnzb");
    assert!(startup_script(&req).contains("RUNNER_GROUP=''"));
    assert!(startup_script(&req).contains("ORG_URL=https://github.com/xbmc4lyfe/rustnzb"));
}

#[tokio::test]
async fn reconciler_records_launch_failures_on_the_pool() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let gcp = Gcp::default();
    *gcp.fail_inserts.lock().unwrap() = true;
    let gh = Gh::default();

    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    assert!(r.launched_hosts.is_empty());
    assert!(r.held_floor_targets.iter().any(|h| h.contains(&id)));
    let status = kv.bundle().state.runner_pool_status[&id].clone();
    assert!(status.last_error.unwrap().contains("VM create failed"));
    assert_eq!(status.last_error_at, Some(NOW + 60));

    // The same failure again keeps its first timestamp and writes nothing new.
    let puts = kv.puts.load(Ordering::SeqCst);
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 70).await.unwrap();
    assert!(!r.snapshot_written, "{:?}", r);
    assert_eq!(kv.puts.load(Ordering::SeqCst), puts);
    assert_eq!(
        kv.bundle().state.runner_pool_status[&id].last_error_at,
        Some(NOW + 60)
    );
}

#[tokio::test]
async fn removing_a_pool_deletes_its_hosts_then_its_record() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let other = seed_other_pool(&kv).await;
    let gcp = Gcp::default();
    let gh = Gh::default();
    let name = format!("{}{}-abcd", pool_host_name_prefix(&id), NOW);
    let other_name = format!("{}{}-ffff", pool_host_name_prefix(&other), NOW);
    gcp.instances
        .lock()
        .unwrap()
        .push(host(&name, &id, "RUNNING"));
    gcp.instances
        .lock()
        .unwrap()
        .push(host(&other_name, &other, "RUNNING"));
    // An unrelated VM with no pool metadata is never touched.
    let mut stray = host("oxidex-runners-swack-tools-1-aaaa", "", "RUNNING");
    stray.metadata.items.retain(|i| i.key != POOL_METADATA_KEY);
    gcp.instances.lock().unwrap().push(stray);
    gh.runners.lock().unwrap().insert(
        "orgs/swack-tools".into(),
        vec![
            SimpleGitHubRunner {
                id: 1,
                name: format!("oxidex-{}-1", name),
                status: "online".into(),
                busy: false,
            },
            SimpleGitHubRunner {
                id: 2,
                name: format!("oxidex-{}-2", name),
                status: "online".into(),
                busy: true,
            },
        ],
    );

    handle_remove_runner_pool(&kv, RemoveRunnerPoolRequest { id: id.clone() }, NOW)
        .await
        .unwrap();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    assert_eq!(gcp.deleted.lock().unwrap().clone(), vec![name.clone()]);
    assert!(r.cleaned_vms.contains(&name));
    // The busy runner is left to finish; the idle one is removed.
    assert_eq!(gh.deleted.lock().unwrap().clone(), vec![1]);
    assert!(kv.pools().iter().any(|p| p.id == id && p.removing));

    // Next tick: the host is gone and the busy runner finished, so the record goes too.
    gcp.instances.lock().unwrap().retain(|i| i.name != name);
    gh.runners
        .lock()
        .unwrap()
        .insert("orgs/swack-tools".into(), vec![]);
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, NOW + 400).await.unwrap();
    let pools = kv.pools();
    assert!(!pools.iter().any(|p| p.id == id), "{:?}", pools);
    assert!(pools.iter().any(|p| p.id == other));
    assert!(!kv.bundle().state.runner_pool_status.contains_key(&id));
}

#[tokio::test]
async fn a_removal_waits_for_a_good_runner_list() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let gcp = Gcp::default();
    let gh = Gh::default();
    // The pool starts a host (gone again by the next list), so it may have registered runners.
    run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    assert!(kv.bundle().state.runner_pool_status[&id]
        .last_launch_host
        .is_some());
    handle_remove_runner_pool(&kv, RemoveRunnerPoolRequest { id: id.clone() }, NOW + 100)
        .await
        .unwrap();

    // No hosts, but GitHub's runner list failed: registrations may remain, so keep the record.
    *gh.fail_list.lock().unwrap() = true;
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, NOW + 400).await.unwrap();
    assert!(kv.pools().iter().any(|p| p.id == id));

    *gh.fail_list.lock().unwrap() = false;
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, NOW + 800).await.unwrap();
    assert!(!kv.pools().iter().any(|p| p.id == id));
}

#[tokio::test]
async fn a_pool_that_never_launched_is_removed_even_if_runners_cannot_be_listed() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    // For example a repository the Worker's token can't manage: every launch failed at the token.
    let id = seed_pool(&kv, |_| {}, axion_request("xbmc4lyfe/newznab-tmux")).await;
    handle_remove_runner_pool(&kv, RemoveRunnerPoolRequest { id: id.clone() }, NOW)
        .await
        .unwrap();
    let gcp = Gcp::default();
    let gh = Gh::default();
    *gh.fail_list.lock().unwrap() = true;
    run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    assert!(!kv.pools().iter().any(|p| p.id == id));
}

#[test]
fn rejects_zones_the_project_does_not_have_and_other_arch_labels() {
    let state = base_state().state;
    let mut req = axion_request("swack-tools");
    req.zone = "us-west8-a".into();
    let err = validate_create_request(&req, &state, "o", NOW).unwrap_err();
    assert!(err.contains("isn't a GCE zone"), "{}", err);
    assert_eq!(KNOWN_GCE_ZONES.split_whitespace().count(), 130);
    assert!(is_known_gce_zone("us-west1-a") && is_known_gce_zone("asia-southeast1-b"));
    assert!(!is_known_gce_zone("us-west1") && !is_known_gce_zone(""));
    for label in ["x86", "X86", "amd64", "i686"] {
        let mut req = axion_request("swack-tools");
        req.labels = vec![label.into()];
        assert!(
            validate_create_request(&req, &state, "o", NOW).is_err(),
            "{}",
            label
        );
    }
    let mut req = axion_request("swack-tools");
    req.machine_type = "c4d-highcpu-16".into();
    req.runner_ram_gib = 14;
    req.labels = vec!["arm".into()];
    assert!(validate_create_request(&req, &state, "o", NOW).is_err());
}

#[test]
fn every_zone_in_the_qualification_catalog_is_a_known_zone() {
    // Every quoted zone literal ("region-a") in state.rs must be a zone the
    // project has, or the dashboard offers rows that can never launch.
    let src = include_str!("../src/state.rs");
    let mut checked = 0;
    for piece in src.split('"').skip(1).step_by(2) {
        let parts: Vec<&str> = piece.split('-').collect();
        let looks_like_zone = parts.len() == 3
            && parts[0].chars().all(|c| c.is_ascii_lowercase())
            && parts[1].chars().any(|c| c.is_ascii_digit())
            && parts[1].chars().all(|c| c.is_ascii_alphanumeric())
            && parts[2].len() == 1
            && parts[2].chars().all(|c| c.is_ascii_lowercase());
        if looks_like_zone {
            assert!(
                spot_dashboard_edge::runner_pools::is_known_gce_zone(piece),
                "state.rs offers zone {piece}, which the project doesn't have"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "found no zone literals in state.rs");
}

#[tokio::test]
async fn pool_hosts_do_not_count_toward_the_floor() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(
        &kv,
        |s| s.scaling.cicd_runner.min_runners = 2,
        axion_request("swack-tools"),
    )
    .await;
    let gcp = Gcp::default();
    let gh = Gh::default();
    let name = format!("{}{}-abcd", pool_host_name_prefix(&id), NOW);
    gcp.instances
        .lock()
        .unwrap()
        .push(host(&name, &id, "RUNNING"));

    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    // The pool is full, so nothing launches for it; the floor still starts its own host for
    // swack-tools (and for the other built-in targets).
    assert!(
        !r.launched_hosts
            .iter()
            .any(|h| h.starts_with(POOL_HOST_PREFIX)),
        "{:?}",
        r.launched_hosts
    );
    assert!(
        r.launched_hosts
            .iter()
            .any(|h| h.starts_with("oxidex-runners-swack-tools-")),
        "{:?}",
        r.launched_hosts
    );
}

async fn seed_other_pool(kv: &Kv) -> String {
    let mut req = axion_request("swack-tools");
    req.labels = vec!["other".into()];
    let view = handle_create_runner_pool(kv, req, "o", NOW).await.unwrap();
    view["created"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_pool_created_during_a_tick_survives_the_reconciler_write() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let first = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let gcp = Gcp::default();
    let gh = Gh::default();
    run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();

    // Simulate an admin write that the reconciler's cached copy hasn't seen.
    let mut b = kv.bundle();
    let mut extra =
        validate_create_request(&axion_request("xbmc4lyfe/rustnzb"), &b.state, "o", NOW).unwrap();
    extra.id = "abc123".into();
    let mut pools = kv.pools();
    pools.push(extra);
    kv.store_pools(&pools);

    // Fail the listed host's launch path so the tick has something to write.
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, NOW + 400).await.unwrap();
    let ids: Vec<String> = kv.pools().iter().map(|p| p.id.clone()).collect();
    assert!(
        ids.contains(&first) && ids.contains(&"abc123".to_string()),
        "{:?}",
        ids
    );
}

#[test]
fn statuses_follow_the_remaining_pools() {
    let mut state = base_state().state;
    let mut a = validate_create_request(&axion_request("swack-tools"), &state, "o", NOW).unwrap();
    a.id = "aaaaaa".into();
    state.runner_pool_status.insert(
        "bbbbbb".to_string(),
        RunnerPoolStatus {
            hosts_running: 2,
            ..Default::default()
        },
    );
    let mut statuses = std::collections::BTreeMap::new();
    statuses.insert(
        "aaaaaa".to_string(),
        RunnerPoolStatus {
            hosts_running: 1,
            ..Default::default()
        },
    );
    merge_pool_statuses(&mut state, &[a], &statuses);
    // "bbbbbb" is no longer a pool, so its stored status goes.
    assert_eq!(state.runner_pool_status.len(), 1);
    assert_eq!(state.runner_pool_status["aaaaaa"].hosts_running, 1);
}

#[tokio::test]
async fn saving_an_older_copy_of_the_state_keeps_pools_and_removals() {
    let _g = lock();
    invalidate_state_cache();
    let kv = Kv::default();
    kv.store(&base_state());
    // Another route holds a copy of the state taken before the pool existed.
    let mut stale = kv.bundle();

    let view = handle_create_runner_pool(&kv, axion_request("swack-tools"), "o", NOW)
        .await
        .unwrap();
    let id = view["created"].as_str().unwrap().to_string();
    save_state(&kv, &mut stale, NOW + 1).await.unwrap();
    assert!(kv.pools().iter().any(|p| p.id == id && !p.removing));
    // The pool list never goes into the state blob.
    let raw = kv.data.lock().unwrap().get(KV_STATE_KEY).cloned().unwrap();
    assert!(!raw.contains(&id), "{}", raw);

    handle_remove_runner_pool(&kv, RemoveRunnerPoolRequest { id: id.clone() }, NOW + 2)
        .await
        .unwrap();
    save_state(&kv, &mut stale, NOW + 3).await.unwrap();
    assert!(kv.pools().iter().any(|p| p.id == id && p.removing));
    let listed = handle_get_runner_pools(&kv, NOW + 4).await.unwrap();
    assert_eq!(listed["pools"][0]["removing"], true);
}

#[tokio::test]
async fn finishing_a_removal_keeps_a_pool_created_meanwhile() {
    let _g = lock();
    invalidate_state_cache();
    let kv = Kv::default();
    kv.store(&base_state());
    let state = base_state().state;
    let mut gone =
        validate_create_request(&axion_request("swack-tools"), &state, "o", NOW).unwrap();
    gone.id = "aaaaaa".into();
    gone.removing = true;
    let mut fresh = gone.clone();
    fresh.id = "bbbbbb".into();
    fresh.removing = false;
    kv.store_pools(&[gone, fresh]);
    let left = finish_pool_removals(&kv, &["aaaaaa".to_string(), "bbbbbb".to_string()])
        .await
        .unwrap();
    // "bbbbbb" isn't being removed, so a stray finished id can't drop it.
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, "bbbbbb");
    assert_eq!(kv.pools().len(), 1);
}

#[test]
fn pool_script_stays_small_and_uses_safe_dom_helpers() {
    use spot_dashboard_edge::ui_runner_pools::{RUNNER_POOLS_JS, RUNNER_POOLS_JS_MAX_BYTES};
    assert!(RUNNER_POOLS_JS.len() < RUNNER_POOLS_JS_MAX_BYTES);
    // Server data is placed with textContent and attributes, never innerHTML.
    assert!(!RUNNER_POOLS_JS.contains("innerHTML"));
    assert!(!RUNNER_POOLS_JS.contains("insertAdjacentHTML"));
    assert!(!RUNNER_POOLS_JS.contains("eval("));
    let html = spot_dashboard_edge::ui::DASHBOARD_HTML;
    assert!(html.contains("<script src=/ui/runner-pools.js></script>"));
    assert!(html.contains("onclick=\"launchQualRow(${i})\""));
}

fn legacy_blob_with(pools: &[RunnerPool]) -> String {
    // A state blob written before pools moved to their own key.
    let mut v = serde_json::to_value(base_state()).unwrap();
    v["state"]["runner_pools"] = serde_json::to_value(pools).unwrap();
    v.to_string()
}

#[tokio::test]
async fn pools_in_a_legacy_state_blob_are_adopted_on_first_read() {
    let _g = lock();
    invalidate_state_cache();
    let kv = Kv::default();
    let state = base_state().state;
    let mut old = validate_create_request(&axion_request("swack-tools"), &state, "o", NOW).unwrap();
    old.id = "legacy".into();
    kv.data
        .lock()
        .unwrap()
        .insert(KV_STATE_KEY.to_string(), legacy_blob_with(&[old.clone()]));

    let listed = handle_get_runner_pools(&kv, NOW).await.unwrap();
    assert_eq!(listed["pools"][0]["id"], "legacy");
    assert_eq!(kv.pools(), vec![old]);

    // Once the key exists, an empty list stays empty; the blob isn't consulted again.
    kv.store_pools(&[]);
    assert!(load_pools(&kv).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_state_save_from_a_legacy_blob_keeps_its_pools() {
    let _g = lock();
    invalidate_state_cache();
    let kv = Kv::default();
    let state = base_state().state;
    let mut old = validate_create_request(&axion_request("swack-tools"), &state, "o", NOW).unwrap();
    old.id = "legacy".into();
    let raw = legacy_blob_with(&[old.clone()]);
    kv.data
        .lock()
        .unwrap()
        .insert(KV_STATE_KEY.to_string(), raw.clone());

    // Some route saves the blob before anything read the pool key.
    let mut bundle: DashboardBundle = serde_json::from_str(&raw).unwrap();
    save_state(&kv, &mut bundle, NOW + 1).await.unwrap();
    assert!(!kv.data.lock().unwrap()[KV_STATE_KEY].contains("legacy"));
    assert_eq!(kv.pools(), vec![old]);
}
