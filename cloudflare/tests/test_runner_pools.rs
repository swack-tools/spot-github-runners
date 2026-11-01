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
    /// Operations `get_zone_operation` returns, by name; others read as unknown.
    ops: Arc<Mutex<HashMap<String, GceOperation>>>,
    /// An error the insert's own (already finished) operation carries.
    insert_op_error: Arc<Mutex<Option<serde_json::Value>>>,
}

fn capacity_error() -> serde_json::Value {
    serde_json::json!({"errors": [{
        "code": "ZONE_RESOURCE_POOL_EXHAUSTED",
        "message": "The zone does not have enough resources available to fulfill the request."
    }]})
}

fn named_op(name: &str, status: &str, error: Option<serde_json::Value>) -> GceOperation {
    GceOperation {
        id: Some(name.into()),
        name: Some(name.into()),
        status: status.into(),
        target_link: None,
        error,
    }
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
        let error = self.insert_op_error.lock().unwrap().clone();
        let status = if error.is_some() { "DONE" } else { "RUNNING" };
        Ok(named_op(&format!("op-{}", req.name), status, error))
    }
    async fn get_zone_operation(
        &self,
        _project: &str,
        _zone: &str,
        operation: &str,
        _now: u64,
    ) -> Result<Option<GceOperation>, ReconcilerError> {
        Ok(self.ops.lock().unwrap().get(operation).cloned())
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
    refuse_delete: Arc<Mutex<Vec<u64>>>,
    /// Labels of the jobs waiting in GitHub's queue, and whether reading the queue fails.
    queued: Arc<Mutex<Vec<Vec<String>>>>,
    fail_queue: Arc<Mutex<bool>>,
    queue_reads: Arc<AtomicUsize>,
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
        if self.refuse_delete.lock().unwrap().contains(&id) {
            // GitHub answers 422 when the runner is running a job.
            return Err(ReconcilerError::Github("runner is busy".into()));
        }
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
    async fn queued_job_labels(
        &self,
        _target: &str,
        _now: u64,
    ) -> Result<Vec<Vec<String>>, ReconcilerError> {
        self.queue_reads.fetch_add(1, Ordering::SeqCst);
        if *self.fail_queue.lock().unwrap() {
            return Err(ReconcilerError::Github("queue unreadable".into()));
        }
        Ok(self.queued.lock().unwrap().clone())
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
        min_hosts: None,
        idle_minutes: None,
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
    // One c4a-highcpu-16 host is about $0.31/hr at Spot list prices; two go over.
    state.global_budget.max_hourly_usd = 0.50;
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
        assert!(vcpu >= 2 && ram >= 8, "{}", mt);
        // The name-derived shape and price agree with the pool table.
        let (q_vcpu, q_ram) = spot_dashboard_edge::qualify::machine_shape(mt).unwrap();
        assert_eq!((vcpu, ram as f64), (q_vcpu, q_ram.floor()), "{}", mt);
        assert!(
            spot_dashboard_edge::state::machine_type_hourly_usd(mt).is_some(),
            "{}",
            mt
        );
    }
    // Small hosts are for pools only; the floor's patch allowlists stay as they were.
    for mt in POOL_SMALL_MACHINE_TYPES {
        assert!(launchable_machine_types().contains(mt));
        assert!(!spot_dashboard_edge::patch::PERMITTED_MACHINE_TYPES.contains(mt));
        assert!(!spot_dashboard_edge::patch::PERMITTED_ARM64_MACHINE_TYPES.contains(mt));
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

#[test]
fn pool_script_ids_do_not_clash_with_the_dashboard() {
    // getElementById returns the first match, so a clashing id would wire the pool dialog to a
    // dashboard element (as the rollout preview's rp-hosts and rp-refresh once did).
    use spot_dashboard_edge::ui_runner_pools::RUNNER_POOLS_JS;
    let html = spot_dashboard_edge::ui::DASHBOARD_HTML;
    let mut checked = 0;
    for marker in ["id: \"", "num(\""] {
        let mut rest = RUNNER_POOLS_JS;
        while let Some(i) = rest.find(marker) {
            rest = &rest[i + marker.len()..];
            let id = &rest[..rest.find('"').unwrap()];
            assert!(
                !html.contains(&format!("id=\"{}\"", id)),
                "id {} is used by both the dashboard and the pool script",
                id
            );
            checked += 1;
        }
    }
    assert!(checked > 20, "{checked}");
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

// ============================================================================
// Resize
// ============================================================================

#[tokio::test]
async fn resize_changes_the_host_count_within_limits_and_budget() {
    let _g = lock();
    invalidate_state_cache();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;

    let view = handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: id.clone(),
            hosts: 3,
            min_hosts: None,
        },
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(view["pools"][0]["hosts"], 3);
    assert_eq!(kv.pools()[0].hosts, 3);

    for bad in [0, MAX_HOSTS_PER_POOL + 1] {
        let err = handle_resize_runner_pool(
            &kv,
            ResizeRunnerPoolRequest {
                id: id.clone(),
                hosts: bad,
                min_hosts: None,
            },
            NOW,
        )
        .await;
        assert!(matches!(err, Err(StateError::Validation(_))), "{}", bad);
    }
    let missing = handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: "nope00".into(),
            hosts: 1,
            min_hosts: None,
        },
        NOW,
    )
    .await;
    assert!(matches!(missing, Err(StateError::NotFound(_))));

    // Growing past the budget cap is refused; shrinking always works.
    let mut b = kv.bundle();
    b.state.global_budget.max_hourly_usd = 0.10;
    kv.store(&b);
    invalidate_state_cache();
    let over = handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: id.clone(),
            hosts: 10,
            min_hosts: None,
        },
        NOW,
    )
    .await;
    assert!(
        matches!(&over, Err(StateError::Validation(m)) if m.contains("budget")),
        "{:?}",
        over
    );
    handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: id.clone(),
            hosts: 1,
            min_hosts: None,
        },
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(kv.pools()[0].hosts, 1);

    // A pool being removed can't be resized.
    handle_remove_runner_pool(&kv, RemoveRunnerPoolRequest { id: id.clone() }, NOW)
        .await
        .unwrap();
    let err = handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id,
            hosts: 2,
            min_hosts: None,
        },
        NOW,
    )
    .await;
    assert!(matches!(err, Err(StateError::Validation(_))));
}

fn pool_runner(id: u64, host: &str, n: u32, busy: bool) -> SimpleGitHubRunner {
    SimpleGitHubRunner {
        id,
        name: format!("oxidex-{}-{}", host, n),
        status: "online".into(),
        busy,
    }
}

#[tokio::test]
async fn scaling_down_deletes_the_newest_idle_hosts_and_keeps_busy_ones() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let gcp = Gcp::default();
    let gh = Gh::default();
    let old = format!("{}{}-aaaa", pool_host_name_prefix(&id), NOW);
    let mid = format!("{}{}-bbbb", pool_host_name_prefix(&id), NOW + 300);
    let new = format!("{}{}-cccc", pool_host_name_prefix(&id), NOW + 600);
    for n in [&old, &mid, &new] {
        gcp.instances.lock().unwrap().push(host(n, &id, "RUNNING"));
    }
    // The newest host is running a job; the other two are idle.
    gh.runners.lock().unwrap().insert(
        "orgs/swack-tools".into(),
        vec![
            pool_runner(1, &old, 1, false),
            pool_runner(2, &mid, 1, false),
            pool_runner(3, &new, 1, true),
            pool_runner(4, &new, 2, false),
        ],
    );
    handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: id.clone(),
            hosts: 2,
            min_hosts: None,
        },
        NOW,
    )
    .await
    .unwrap();

    // GitHub's runner list fails: every host would look idle, so nothing is deleted.
    *gh.fail_list.lock().unwrap() = true;
    run_reconciliation(&kv, &gcp, &gh, NOW + 700).await.unwrap();
    assert!(gcp.deleted.lock().unwrap().is_empty());
    assert!(gcp.inserted.lock().unwrap().is_empty());

    // With a good list, one host goes: the newest idle one, not the busy newest one.
    *gh.fail_list.lock().unwrap() = false;
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 1000)
        .await
        .unwrap();
    assert_eq!(gcp.deleted.lock().unwrap().clone(), vec![mid.clone()]);
    assert!(r.cleaned_vms.contains(&mid));
    assert_eq!(gh.deleted.lock().unwrap().clone(), vec![2]);
    assert!(gcp.inserted.lock().unwrap().is_empty());

    // Down to 1 with only the busy host left above it: the idle old host goes, the busy one stays.
    gcp.instances.lock().unwrap().retain(|i| i.name != mid);
    handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: id.clone(),
            hosts: 1,
            min_hosts: None,
        },
        NOW,
    )
    .await
    .unwrap();
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, NOW + 1400)
        .await
        .unwrap();
    assert_eq!(gcp.deleted.lock().unwrap().clone(), vec![mid, old]);

    // Scaling back up launches a host again.
    gcp.instances.lock().unwrap().retain(|i| i.name == new);
    handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id,
            hosts: 2,
            min_hosts: None,
        },
        NOW,
    )
    .await
    .unwrap();
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 1800)
        .await
        .unwrap();
    assert_eq!(r.launched_hosts.len(), 1, "{:?}", r);
}

#[tokio::test]
async fn scaling_down_keeps_a_host_whose_runner_took_a_job_since_the_list() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let gcp = Gcp::default();
    let gh = Gh::default();
    let old = format!("{}{}-aaaa", pool_host_name_prefix(&id), NOW);
    let new = format!("{}{}-cccc", pool_host_name_prefix(&id), NOW + 600);
    // A host deleted last tick and still stopping doesn't count toward the pool.
    let stopping = format!("{}{}-dddd", pool_host_name_prefix(&id), NOW + 900);
    gcp.instances
        .lock()
        .unwrap()
        .push(host(&old, &id, "RUNNING"));
    gcp.instances
        .lock()
        .unwrap()
        .push(host(&new, &id, "RUNNING"));
    gcp.instances
        .lock()
        .unwrap()
        .push(host(&stopping, &id, "STOPPING"));
    gh.runners.lock().unwrap().insert(
        "orgs/swack-tools".into(),
        vec![
            pool_runner(1, &old, 1, false),
            pool_runner(2, &new, 1, false),
        ],
    );
    // The newest host's runner picked up a job after the list: GitHub refuses to remove it.
    gh.refuse_delete.lock().unwrap().push(2);
    handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: id.clone(),
            hosts: 1,
            min_hosts: None,
        },
        NOW,
    )
    .await
    .unwrap();

    run_reconciliation(&kv, &gcp, &gh, NOW + 1000)
        .await
        .unwrap();
    // The busy newest host stays; the older idle one goes instead. The stopping host is left alone.
    assert_eq!(gcp.deleted.lock().unwrap().clone(), vec![old]);
    assert_eq!(gh.deleted.lock().unwrap().clone(), vec![1]);
}

#[tokio::test]
async fn resize_does_not_undo_a_remove_saved_meanwhile() {
    let _g = lock();
    invalidate_state_cache();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let other = seed_other_pool(&kv).await;
    handle_remove_runner_pool(&kv, RemoveRunnerPoolRequest { id: other.clone() }, NOW)
        .await
        .unwrap();
    handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: id.clone(),
            hosts: 2,
            min_hosts: None,
        },
        NOW,
    )
    .await
    .unwrap();
    let pools = kv.pools();
    assert!(pools.iter().any(|p| p.id == other && p.removing));
    assert!(pools.iter().any(|p| p.id == id && p.hosts == 2));
}

// ============================================================================
// Create operations that fail after the call returns
// ============================================================================

#[tokio::test]
async fn capacity_errors_from_the_create_operation_show_on_the_pool() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let gcp = Gcp::default();
    let gh = Gh::default();

    // The create call succeeds; its operation is still running.
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    assert_eq!(r.launched_hosts.len(), 1);
    let first = gcp.inserted.lock().unwrap()[0].1.name.clone();
    let status = kv.bundle().state.runner_pool_status[&id].clone();
    assert_eq!(status.last_launch_op, Some(format!("op-{}", first)));
    assert!(status.last_error.is_none());

    // GCE finished the operation with no capacity: the pool shows why, and tries again.
    gcp.ops.lock().unwrap().insert(
        format!("op-{}", first),
        named_op(&format!("op-{}", first), "DONE", Some(capacity_error())),
    );
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 400).await.unwrap();
    let status = kv.bundle().state.runner_pool_status[&id].clone();
    let err = status.last_error.clone().unwrap();
    assert!(
        err.starts_with("VM create failed in us-west1-a: ZONE_RESOURCE_POOL_EXHAUSTED: "),
        "{}",
        err
    );
    assert_eq!(status.last_error_at, Some(NOW + 400));
    assert!(r
        .held_floor_targets
        .iter()
        .any(|h| h.contains(&id) && h.contains("ZONE_RESOURCE")));
    assert_eq!(r.launched_hosts.len(), 1, "{:?}", r);
    let second = gcp.inserted.lock().unwrap()[1].1.name.clone();
    assert_eq!(status.last_launch_op, Some(format!("op-{}", second)));

    // The retry fails the same way: the error and its first time stay.
    gcp.ops.lock().unwrap().insert(
        format!("op-{}", second),
        named_op(&format!("op-{}", second), "DONE", Some(capacity_error())),
    );
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, NOW + 800).await.unwrap();
    let status = kv.bundle().state.runner_pool_status[&id].clone();
    assert_eq!(status.last_error.as_deref(), Some(err.as_str()));
    assert_eq!(status.last_error_at, Some(NOW + 400));
    let third = gcp.inserted.lock().unwrap()[2].1.name.clone();

    // The third create works: its host shows up, the pool is full, and the error clears.
    gcp.instances
        .lock()
        .unwrap()
        .push(host(&third, &id, "RUNNING"));
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 1200)
        .await
        .unwrap();
    assert!(r.launched_hosts.is_empty(), "{:?}", r);
    let status = kv.bundle().state.runner_pool_status[&id].clone();
    assert!(status.last_error.is_none(), "{:?}", status);
    assert!(status.last_error_at.is_none());
    assert!(status.last_launch_op.is_none());
}

#[tokio::test]
async fn a_create_whose_operation_already_failed_is_reported_at_once() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let gcp = Gcp::default();
    *gcp.insert_op_error.lock().unwrap() = Some(serde_json::json!({"errors": [
        {"code": "QUOTA_EXCEEDED", "message": "Quota 'C4A_CPUS' exceeded."},
        {"code": "SECOND", "message": "x"},
        {"code": "THIRD", "message": "y"}
    ]}));
    let gh = Gh::default();

    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    assert!(r.launched_hosts.is_empty(), "{:?}", r);
    let status = kv.bundle().state.runner_pool_status[&id].clone();
    assert_eq!(
        status.last_error.as_deref(),
        Some(
            "VM create failed in us-west1-a: QUOTA_EXCEEDED: Quota 'C4A_CPUS' exceeded.; SECOND: x"
        )
    );
    assert!(status.last_launch_host.is_none());
    assert!(status.last_launch_op.is_none());
}

#[tokio::test]
async fn a_running_create_is_not_doubled_until_it_is_long_overdue() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let gcp = Gcp::default();
    let gh = Gh::default();

    run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    let first = gcp.inserted.lock().unwrap()[0].1.name.clone();
    gcp.ops.lock().unwrap().insert(
        format!("op-{}", first),
        named_op(&format!("op-{}", first), "RUNNING", None),
    );

    // Still running five minutes later: no second host.
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 400).await.unwrap();
    assert!(r.launched_hosts.is_empty(), "{:?}", r);
    assert_eq!(gcp.inserted.lock().unwrap().len(), 1);
    let status = kv.bundle().state.runner_pool_status[&id].clone();
    assert_eq!(status.last_launch_op, Some(format!("op-{}", first)));

    // Half an hour later it no longer holds the slot.
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 60 + 1800)
        .await
        .unwrap();
    assert_eq!(r.launched_hosts.len(), 1, "{:?}", r);
    assert_eq!(gcp.inserted.lock().unwrap().len(), 2);
}

// ============================================================================
// Autoscaling pools: spin up on queued jobs, spin down when idle
// ============================================================================

fn small_auto_request() -> CreateRunnerPoolRequest {
    CreateRunnerPoolRequest {
        target: "swack-tools".into(),
        machine_type: "n4a-standard-2".into(),
        zone: "us-central1-a".into(),
        hosts: 3,
        runners_per_host: 1,
        runner_vcpu: 2,
        runner_ram_gib: 6,
        labels: vec!["small".into()],
        cloud_init_template_id: String::new(),
        min_hosts: Some(0),
        idle_minutes: Some(10),
    }
}

fn labels(l: &[&str]) -> Vec<String> {
    l.iter().map(|s| s.to_string()).collect()
}

fn pool_status(kv: &Kv, id: &str) -> RunnerPoolStatus {
    kv.bundle()
        .state
        .runner_pool_status
        .get(id)
        .cloned()
        .unwrap_or_default()
}

#[test]
fn autoscaling_settings_are_validated() {
    let state = base_state().state;
    let pool = validate_create_request(&small_auto_request(), &state, "o", NOW).unwrap();
    assert!(pool.autoscales());
    assert_eq!(
        (pool.min_hosts(), pool.hosts, pool.idle_minutes()),
        (0, 3, 10)
    );
    assert_eq!(
        runs_on_labels(&pool),
        labels(&["self-hosted", "ARM64", "spot", "small"])
    );

    let mut req = small_auto_request();
    req.min_hosts = Some(4);
    assert!(validate_create_request(&req, &state, "o", NOW)
        .unwrap_err()
        .contains("Minimum hosts"));
    req.min_hosts = Some(0);
    req.idle_minutes = Some(1);
    assert!(validate_create_request(&req, &state, "o", NOW)
        .unwrap_err()
        .contains("Idle minutes"));
    // A minimum equal to the size, or none, is a fixed-size pool.
    req.idle_minutes = None;
    req.min_hosts = Some(3);
    let fixed = validate_create_request(&req, &state, "o", NOW).unwrap();
    assert!(!fixed.autoscales());
    assert_eq!(fixed.min_hosts, None);
    // Without labels of its own, runs-on names the machine type.
    req.labels = vec![];
    let bare = validate_create_request(&req, &state, "o", NOW).unwrap();
    assert_eq!(
        runs_on_labels(&bare),
        labels(&["self-hosted", "ARM64", "spot", "n4a-standard-2"])
    );
}

#[test]
fn a_job_fits_a_pool_only_when_the_pool_has_all_its_labels() {
    let pool = labels(&[
        "self-hosted",
        "Linux",
        "ARM64",
        "spot",
        "n4a-standard-2",
        "us-central1-a",
        "small",
    ]);
    assert!(job_fits_labels(
        &labels(&["self-hosted", "arm64", "small"]),
        &pool
    ));
    assert!(!job_fits_labels(
        &labels(&["self-hosted", "small", "gpu"]),
        &pool
    ));
    assert!(!job_fits_labels(&labels(&["ubuntu-latest"]), &pool));
    assert!(!job_fits_labels(&[], &pool));
}

#[tokio::test]
async fn an_autoscaling_pool_waits_for_a_matching_queued_job() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, small_auto_request()).await;
    let gcp = Gcp::default();
    let gh = Gh::default();

    // Nothing queued: no host.
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 60).await.unwrap();
    assert!(r.launched_hosts.is_empty(), "{:?}", r);
    let st = pool_status(&kv, &id);
    assert_eq!((st.queued_jobs, st.desired_hosts), (Some(0), Some(0)));

    // A job for other runners doesn't count.
    *gh.queued.lock().unwrap() = vec![labels(&["ubuntu-latest"]), labels(&["self-hosted", "X64"])];
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 360).await.unwrap();
    assert!(r.launched_hosts.is_empty(), "{:?}", r);

    // Two jobs this pool can take: it wants two hosts and starts one per tick.
    gh.queued
        .lock()
        .unwrap()
        .push(labels(&["self-hosted", "ARM64", "small"]));
    gh.queued
        .lock()
        .unwrap()
        .push(labels(&["self-hosted", "small"]));
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, NOW + 660).await.unwrap();
    assert_eq!(r.launched_hosts.len(), 1, "{:?}", r);
    let st = pool_status(&kv, &id);
    assert_eq!((st.queued_jobs, st.desired_hosts), (Some(2), Some(2)));
    let inserted = gcp.inserted.lock().unwrap().clone();
    assert_eq!(
        inserted[0].1.machine_type,
        "zones/us-central1-a/machineTypes/n4a-standard-2"
    );
}

#[tokio::test]
async fn idle_hosts_spin_down_after_the_idle_time_and_busy_ones_stay() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, small_auto_request()).await;
    let gcp = Gcp::default();
    let gh = Gh::default();
    let a = format!("{}{}-aaaa", pool_host_name_prefix(&id), NOW);
    let b = format!("{}{}-bbbb", pool_host_name_prefix(&id), NOW + 30);
    for n in [&a, &b] {
        gcp.instances.lock().unwrap().push(host(n, &id, "RUNNING"));
    }
    // Host a is idle, host b runs a job.
    gh.runners.lock().unwrap().insert(
        "orgs/swack-tools".into(),
        vec![pool_runner(1, &a, 1, false), pool_runner(2, &b, 1, true)],
    );

    // First tick sees a idle and starts its clock; nothing is deleted yet.
    let t0 = NOW + 1800;
    run_reconciliation(&kv, &gcp, &gh, t0).await.unwrap();
    assert!(gcp.deleted.lock().unwrap().is_empty());
    let st = pool_status(&kv, &id);
    assert_eq!(st.idle_since.get(&a), Some(&t0));
    assert!(!st.idle_since.contains_key(&b));
    assert_eq!(st.desired_hosts, Some(1));

    // Five minutes later it's still under the idle time.
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, t0 + 300).await.unwrap();
    assert!(gcp.deleted.lock().unwrap().is_empty());

    // If GitHub's queue can't be read, the pool keeps its hosts.
    *gh.fail_queue.lock().unwrap() = true;
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, t0 + 700).await.unwrap();
    assert!(gcp.deleted.lock().unwrap().is_empty());
    assert_eq!(pool_status(&kv, &id).queued_jobs, None);

    // Past ten idle minutes with the queue readable: a goes (registration first), b stays.
    *gh.fail_queue.lock().unwrap() = false;
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, t0 + 900).await.unwrap();
    assert_eq!(gcp.deleted.lock().unwrap().clone(), vec![a.clone()]);
    assert_eq!(gh.deleted.lock().unwrap().clone(), vec![1]);
    assert!(r.launched_hosts.is_empty());
    let st = pool_status(&kv, &id);
    assert_eq!(st.last_scale_down_at, Some(t0 + 900));
    assert!(!st.idle_since.contains_key(&a));

    // b finishes its job; ten idle minutes later the pool is down to zero hosts.
    gcp.instances.lock().unwrap().retain(|i| i.name == b);
    gh.runners.lock().unwrap().insert(
        "orgs/swack-tools".into(),
        vec![pool_runner(2, &b, 1, false)],
    );
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, t0 + 1200).await.unwrap();
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, t0 + 1200 + 600)
        .await
        .unwrap();
    assert_eq!(gcp.deleted.lock().unwrap().clone(), vec![a, b]);
}

#[tokio::test]
async fn a_new_host_is_not_idle_while_it_boots() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, small_auto_request()).await;
    let gcp = Gcp::default();
    let gh = Gh::default();
    let started = NOW + 100;
    let h = format!("{}{}-aaaa", pool_host_name_prefix(&id), started);
    gcp.instances.lock().unwrap().push(host(&h, &id, "RUNNING"));
    // No runner registered yet: still booting for the grace period.
    run_reconciliation(&kv, &gcp, &gh, started + 600)
        .await
        .unwrap();
    assert!(pool_status(&kv, &id).idle_since.is_empty());
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, started + 1200)
        .await
        .unwrap();
    assert!(gcp.deleted.lock().unwrap().is_empty());
    // A host whose setup never registered a runner counts as idle after the grace period.
    reset_reconciler_state();
    let late = started + HOST_BOOT_GRACE_SECS + 60;
    run_reconciliation(&kv, &gcp, &gh, late).await.unwrap();
    assert_eq!(pool_status(&kv, &id).idle_since.get(&h), Some(&late));
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, late + 600)
        .await
        .unwrap();
    assert_eq!(gcp.deleted.lock().unwrap().clone(), vec![h]);
}

#[tokio::test]
async fn fixed_size_pools_do_not_read_the_queue_or_spin_down() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, axion_request("swack-tools")).await;
    let gcp = Gcp::default();
    let gh = Gh::default();
    let h = format!("{}{}-aaaa", pool_host_name_prefix(&id), NOW);
    gcp.instances.lock().unwrap().push(host(&h, &id, "RUNNING"));
    gh.runners.lock().unwrap().insert(
        "orgs/swack-tools".into(),
        vec![pool_runner(1, &h, 1, false), pool_runner(2, &h, 2, false)],
    );
    for t in [NOW + 3600, NOW + 7200] {
        reset_reconciler_state();
        run_reconciliation(&kv, &gcp, &gh, t).await.unwrap();
    }
    assert!(gcp.deleted.lock().unwrap().is_empty());
    assert_eq!(gh.queue_reads.load(Ordering::SeqCst), 0);
    let st = pool_status(&kv, &id);
    assert_eq!((st.queued_jobs, st.desired_hosts), (None, None));
}

#[tokio::test]
async fn switching_a_pools_template_replaces_idle_hosts_one_at_a_time() {
    let _g = lock();
    invalidate_state_cache();
    reset_reconciler_state();
    let kv = Kv::default();
    let id = seed_pool(
        &kv,
        |s| {
            s.cloud_init_templates = vec![
                template("old", "cicd_runner", "#!/bin/bash\necho old\n"),
                template("new", "cicd_runner", "#!/bin/bash\necho new\n"),
                template("yaml", "cicd_runner", "#cloud-config\npackages: [jq]\n"),
            ]
        },
        CreateRunnerPoolRequest {
            hosts: 2,
            cloud_init_template_id: "old".into(),
            ..axion_request("swack-tools")
        },
    )
    .await;

    // Unusable templates and unknown pools are refused.
    let bad = handle_set_pool_template(
        &kv,
        SetPoolTemplateRequest {
            id: id.clone(),
            cloud_init_template_id: "yaml".into(),
        },
        NOW,
    )
    .await;
    assert!(matches!(bad, Err(StateError::Validation(_))), "{:?}", bad);
    let missing = handle_set_pool_template(
        &kv,
        SetPoolTemplateRequest {
            id: "nope00".into(),
            cloud_init_template_id: "new".into(),
        },
        NOW,
    )
    .await;
    assert!(matches!(missing, Err(StateError::NotFound(_))));

    let gcp = Gcp::default();
    let gh = Gh::default();
    let a = format!("{}{}-aaaa", pool_host_name_prefix(&id), NOW + 10);
    let b = format!("{}{}-bbbb", pool_host_name_prefix(&id), NOW + 20);
    for n in [&a, &b] {
        gcp.instances.lock().unwrap().push(host(n, &id, "RUNNING"));
    }
    gh.runners.lock().unwrap().insert(
        "orgs/swack-tools".into(),
        vec![
            pool_runner(1, &a, 1, false),
            pool_runner(2, &a, 2, false),
            pool_runner(3, &b, 1, true),
            pool_runner(4, &b, 2, false),
        ],
    );
    let changed = NOW + 1000;
    let view = handle_set_pool_template(
        &kv,
        SetPoolTemplateRequest {
            id: id.clone(),
            cloud_init_template_id: "new".into(),
        },
        changed,
    )
    .await
    .unwrap();
    assert_eq!(view["pools"][0]["cloud_init_template_name"], "new template");
    let pool = kv.pools().into_iter().find(|p| p.id == id).unwrap();
    assert_eq!(pool.setup_updated_at, Some(changed));
    assert!(pool.setup_script.contains("echo new"));

    // The idle old host goes; the busy one stays until its job ends.
    run_reconciliation(&kv, &gcp, &gh, changed + 60)
        .await
        .unwrap();
    assert_eq!(gcp.deleted.lock().unwrap().clone(), vec![a.clone()]);
    assert!(gcp.inserted.lock().unwrap().is_empty());

    // Next tick: the replacement starts with the new template.
    gcp.instances.lock().unwrap().retain(|i| i.name != a);
    reset_reconciler_state();
    let r = run_reconciliation(&kv, &gcp, &gh, changed + 360)
        .await
        .unwrap();
    assert_eq!(r.launched_hosts.len(), 1, "{:?}", r);
    let inserted = gcp.inserted.lock().unwrap().clone();
    let script = startup_script(&inserted[0].1);
    let marker = "printf '%s' '";
    let start = script.find(marker).unwrap() + marker.len();
    let b64 = &script[start..script[start..].find('\'').unwrap() + start];
    let dockerfile = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap(),
    )
    .unwrap();
    let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t);
    assert!(
        dockerfile.contains(&enc("#!/bin/bash\necho new\n")),
        "{}",
        dockerfile
    );
    assert!(!dockerfile.contains(&enc("#!/bin/bash\necho old\n")));

    // b's job ends, but the replacement is still booting: b stays, so the pool is never more
    // than one host short.
    let c = r.launched_hosts[0].clone();
    gcp.instances.lock().unwrap().push(host(&c, &id, "RUNNING"));
    gh.runners.lock().unwrap().insert(
        "orgs/swack-tools".into(),
        vec![pool_runner(3, &b, 1, false), pool_runner(4, &b, 2, false)],
    );
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, changed + 660)
        .await
        .unwrap();
    assert_eq!(gcp.deleted.lock().unwrap().clone(), vec![a.clone()]);

    // Once the replacement's runners are online, b is replaced too.
    gh.runners.lock().unwrap().insert(
        "orgs/swack-tools".into(),
        vec![
            pool_runner(3, &b, 1, false),
            pool_runner(4, &b, 2, false),
            pool_runner(5, &c, 1, false),
            pool_runner(6, &c, 2, false),
        ],
    );
    reset_reconciler_state();
    run_reconciliation(&kv, &gcp, &gh, changed + 960)
        .await
        .unwrap();
    assert_eq!(
        gcp.deleted.lock().unwrap().clone(),
        vec![a.clone(), b.clone()]
    );
}

#[tokio::test]
async fn resize_can_change_the_fewest_hosts() {
    let _g = lock();
    invalidate_state_cache();
    let kv = Kv::default();
    let id = seed_pool(&kv, |_| {}, small_auto_request()).await;
    handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: id.clone(),
            hosts: 4,
            min_hosts: Some(1),
        },
        NOW,
    )
    .await
    .unwrap();
    let p = kv.pools().into_iter().find(|p| p.id == id).unwrap();
    assert_eq!((p.min_hosts(), p.hosts), (1, 4));
    let err = handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: id.clone(),
            hosts: 2,
            min_hosts: Some(3),
        },
        NOW,
    )
    .await;
    assert!(matches!(err, Err(StateError::Validation(_))));
    // Keeping the minimum out of the request keeps it.
    handle_resize_runner_pool(
        &kv,
        ResizeRunnerPoolRequest {
            id: id.clone(),
            hosts: 5,
            min_hosts: None,
        },
        NOW,
    )
    .await
    .unwrap();
    let p = kv.pools().into_iter().find(|p| p.id == id).unwrap();
    assert_eq!((p.min_hosts(), p.hosts), (1, 5));
}
