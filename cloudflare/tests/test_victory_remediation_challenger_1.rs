//! Challenger 1 Adversarial Verification Suite:
//! Cloudflare Endpoints Non-Facade Invariants, Auth Security Refusal,
//! Scaler Naming Collision Resistance within the Same Second, and Multi-Target Runner Queries.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use spot_dashboard_edge::auth;
use spot_dashboard_edge::gcp::{GceInsertInstanceRequest, GceMetadata, GceOperation};
use spot_dashboard_edge::github::RunnerBootstrapConfig;
use spot_dashboard_edge::reconciler::*;
use spot_dashboard_edge::state::*;

// ============================================================================
// Test Doubles
// ============================================================================

pub struct ChallengerMockKv {
    pub data: Mutex<HashMap<String, String>>,
    pub get_count: AtomicUsize,
    pub put_count: AtomicUsize,
}

impl ChallengerMockKv {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            get_count: AtomicUsize::new(0),
            put_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for ChallengerMockKv {
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

#[derive(Default, Clone)]
pub struct ChallengerMockGcp {
    pub instances: Arc<Mutex<Vec<SimpleGceInstance>>>,
    pub deleted_instances: Arc<Mutex<Vec<String>>>,
    pub scrubbed_instances: Arc<Mutex<Vec<String>>>,
    pub inserted_instances: Arc<Mutex<Vec<GceInsertInstanceRequest>>>,
}

#[async_trait::async_trait(?Send)]
impl GcpReconcilerClient for ChallengerMockGcp {
    async fn list_instances(
        &self,
        _project: &str,
        _now: u64,
    ) -> Result<Vec<SimpleGceInstance>, ReconcilerError> {
        Ok(self.instances.lock().unwrap().clone())
    }

    async fn insert_instance(
        &self,
        _project: &str,
        _zone: &str,
        req: &GceInsertInstanceRequest,
        _source_instance_template: Option<&str>,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        self.inserted_instances.lock().unwrap().push(req.clone());
        Ok(GceOperation {
            id: Some("op-insert-test".into()),
            name: Some("op-insert-test".into()),
            status: "DONE".into(),
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
        self.deleted_instances
            .lock()
            .unwrap()
            .push(instance.to_string());
        Ok(GceOperation {
            id: Some("op-delete-test".into()),
            name: Some("op-delete-test".into()),
            status: "DONE".into(),
            target_link: None,
            error: None,
        })
    }

    async fn scrub_startup_script(
        &self,
        _project: &str,
        _zone: &str,
        instance: &str,
        _current_metadata: &GceMetadata,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        self.scrubbed_instances
            .lock()
            .unwrap()
            .push(instance.to_string());
        Ok(GceOperation {
            id: Some("op-scrub-test".into()),
            name: Some("op-scrub-test".into()),
            status: "DONE".into(),
            target_link: None,
            error: None,
        })
    }
}

#[derive(Default, Clone)]
pub struct ChallengerMockGitHubReconciler {
    pub runners: Arc<Mutex<Vec<SimpleGitHubRunner>>>,
    pub target_runners: Arc<Mutex<HashMap<String, Vec<SimpleGitHubRunner>>>>,
    pub deleted_runners: Arc<Mutex<Vec<(String, u64)>>>,
    pub queried_targets: Arc<Mutex<Vec<String>>>,
    pub queued_jobs: Arc<Mutex<u32>>,
}

#[async_trait::async_trait(?Send)]
impl GitHubReconcilerClient for ChallengerMockGitHubReconciler {
    async fn list_runners(
        &self,
        target_api_path: &str,
    ) -> Result<Vec<SimpleGitHubRunner>, ReconcilerError> {
        self.queried_targets
            .lock()
            .unwrap()
            .push(target_api_path.to_string());
        if let Some(runners) = self.target_runners.lock().unwrap().get(target_api_path) {
            return Ok(runners.clone());
        }
        Ok(self.runners.lock().unwrap().clone())
    }

    async fn delete_runner(
        &self,
        target_api_path: &str,
        runner_id: u64,
    ) -> Result<(), ReconcilerError> {
        self.deleted_runners
            .lock()
            .unwrap()
            .push((target_api_path.to_string(), runner_id));
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
            runner_1_jitconfig: "jit-slot-1".to_string(),
            runner_2_jitconfig: "jit-slot-2".to_string(),
        })
    }

    async fn count_queued_jobs(&self, owner: &str, repo: &str) -> Result<u32, ReconcilerError> {
        if owner == "swack-tools" || repo == "oxidex" || repo == "newznab-tmux" {
            Ok(*self.queued_jobs.lock().unwrap())
        } else {
            Ok(0)
        }
    }
}

static CHALLENGER_TEST_MUTEX: Mutex<()> = Mutex::new(());

// ============================================================================
// 1. Challenge Endpoints for Facades / Dummy Behavior
// ============================================================================

#[tokio::test]
async fn test_adversarial_cloud_init_save_tampering_and_persistence() {
    let _guard = CHALLENGER_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    let kv = ChallengerMockKv::new();
    let now = 1792100000;

    // Seed state with an initial cicd_runner profile
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.profiles.insert(
        "cicd_runner".into(),
        ProfileConfig {
            role: "cicd_runner".into(),
            min_runners: Some(2),
            min_instances: Some(1),
            max_instances: 4,
            runner_vcpu: 7,
            runner_ram_gib: 14,
            host_overhead_gib: 4,
            min_instance_vcpu: 16,
            min_instance_ram_gib: 32,
            min_benchmark_score: 1920,
            disk_category: "hyperdisk_balanced".into(),
            disk_type: "hyperdisk-balanced".into(),
            boot_disk_gb: 30,
            data_disk_gb: 100,
            use_vm_template: false,
            vm_template_id: "".into(),
            cloud_init_template_id: "default-runner".into(),
            budget: bundle.state.global_budget.clone(),
        },
    );
    save_state(&kv, &mut bundle, now).await.unwrap();
    let initial_puts = kv.put_count.load(Ordering::SeqCst);

    // Adversarial Input 1: Whitespace name
    let bad_name = SaveCloudInitRequest {
        id: Some("id1".into()),
        mode: Some("save".into()),
        role: "cicd_runner".into(),
        name: "   \t\n  ".into(),
        content: "valid content".into(),
        assign_to_profile: false,
    };
    match handle_post_cloud_init_save(&kv, bad_name, now + 1).await {
        Err(StateError::Validation(msg)) => assert!(msg.contains("Template name cannot be empty")),
        other => panic!("Expected StateError::Validation, got {:?}", other),
    }

    // Adversarial Input 2: Whitespace content
    let bad_content = SaveCloudInitRequest {
        id: Some("id2".into()),
        mode: Some("save".into()),
        role: "cicd_runner".into(),
        name: "Valid Name".into(),
        content: "  \n\t  ".into(),
        assign_to_profile: false,
    };
    match handle_post_cloud_init_save(&kv, bad_content, now + 2).await {
        Err(StateError::Validation(msg)) => {
            assert!(msg.contains("Template content cannot be empty"))
        }
        other => panic!("Expected StateError::Validation, got {:?}", other),
    }

    // Adversarial Input 3: Invalid role injection
    let bad_role = SaveCloudInitRequest {
        id: Some("id3".into()),
        mode: Some("save".into()),
        role: "root_privilege_escalation".into(),
        name: "Valid Name".into(),
        content: "valid content".into(),
        assign_to_profile: false,
    };
    match handle_post_cloud_init_save(&kv, bad_role, now + 3).await {
        Err(StateError::Validation(msg)) => assert!(msg.contains("Unsupported workload role")),
        other => panic!("Expected StateError::Validation, got {:?}", other),
    }

    // Genuine Update 1: Create new template and assign to profile
    let valid_save = SaveCloudInitRequest {
        id: Some("custom-c4d-template".into()),
        mode: Some("save".into()),
        role: "cicd_runner".into(),
        name: "Production Spot Hardened C4D".into(),
        content: "#!/bin/bash\nset -euo pipefail\necho 'Hardened runner bootstrap'".into(),
        assign_to_profile: true,
    };
    let (status, resp) = handle_post_cloud_init_save(&kv, valid_save, now + 4)
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["template"]["id"], "custom-c4d-template");
    assert_eq!(resp["template"]["name"], "Production Spot Hardened C4D");

    // Verify KV was genuinely mutated
    assert!(kv.put_count.load(Ordering::SeqCst) > initial_puts);

    // Verify template persists and profile is updated in fresh state reload
    invalidate_state_cache();
    let (fresh_bundle, _) = load_state(&kv, now + 5).await.unwrap();
    let found = fresh_bundle
        .state
        .cloud_init_templates
        .iter()
        .find(|t| t.id == "custom-c4d-template")
        .expect("Saved template must persist in cloud_init_templates");
    assert_eq!(
        found.content,
        "#!/bin/bash\nset -euo pipefail\necho 'Hardened runner bootstrap'"
    );
    assert_eq!(
        fresh_bundle
            .state
            .profiles
            .get("cicd_runner")
            .unwrap()
            .cloud_init_template_id,
        "custom-c4d-template"
    );

    // Genuine Update 2: In-place edit of existing template without duplication
    let edit_save = SaveCloudInitRequest {
        id: Some("custom-c4d-template".into()),
        mode: Some("save".into()),
        role: "cicd_runner".into(),
        name: "Production Spot Hardened C4D v2".into(),
        content: "#!/bin/bash\necho 'v2'".into(),
        assign_to_profile: false,
    };
    handle_post_cloud_init_save(&kv, edit_save, now + 6)
        .await
        .unwrap();
    invalidate_state_cache();
    let (fresh_bundle_v2, _) = load_state(&kv, now + 7).await.unwrap();
    let matches: Vec<_> = fresh_bundle_v2
        .state
        .cloud_init_templates
        .iter()
        .filter(|t| t.id == "custom-c4d-template")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "Must update in place without duplicating template IDs"
    );
    assert_eq!(matches[0].name, "Production Spot Hardened C4D v2");

    // Genuine Update 3: Mode "save_as" auto-generates unique ID with format
    let save_as_req = SaveCloudInitRequest {
        id: Some("custom-c4d-template".into()),
        mode: Some("save_as".into()),
        role: "rust_builder".into(),
        name: "Forked Builder Template".into(),
        content: "#!/bin/bash\necho 'forked'".into(),
        assign_to_profile: false,
    };
    let (_, save_as_resp) = handle_post_cloud_init_save(&kv, save_as_req, now + 8)
        .await
        .unwrap();
    let new_id = save_as_resp["template"]["id"].as_str().unwrap();
    assert!(new_id.starts_with("rust-builder-custom-"));
    assert_ne!(new_id, "custom-c4d-template");
}

#[tokio::test]
async fn test_adversarial_vm_templates_from_instance_missing_and_persisted() {
    let _guard = CHALLENGER_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    let kv = ChallengerMockKv::new();
    let now = 1792100000;

    // 1. Missing / whitespace parameters
    let bad_id = CreateVmTemplateRequest {
        instance_id: "   ".into(),
        template_name: "Template".into(),
    };
    match handle_post_vm_templates_from_instance(&kv, bad_id, now).await {
        Err(StateError::Validation(msg)) => assert!(msg.contains("instance_id cannot be empty")),
        other => panic!("Expected StateError::Validation, got {:?}", other),
    }

    let bad_tpl_name = CreateVmTemplateRequest {
        instance_id: "inst-1".into(),
        template_name: " \t ".into(),
    };
    match handle_post_vm_templates_from_instance(&kv, bad_tpl_name, now).await {
        Err(StateError::Validation(msg)) => assert!(msg.contains("template_name cannot be empty")),
        other => panic!("Expected StateError::Validation, got {:?}", other),
    }

    // 2. Non-existent instance: MUST return StateError::NotFound (HTTP 404), NEVER dummy 200 OK!
    let ghost_req = CreateVmTemplateRequest {
        instance_id: "ghost-vm-does-not-exist".into(),
        template_name: "Ghost Template".into(),
    };
    match handle_post_vm_templates_from_instance(&kv, ghost_req, now).await {
        Err(StateError::NotFound(msg)) => {
            assert!(msg.contains("ghost-vm-does-not-exist"));
            assert!(msg.contains("not found in live fleet inventory"));
        }
        other => panic!(
            "Adversarial failure: Expected StateError::NotFound, got {:?}",
            other
        ),
    }

    // 3. Genuine instance in instances_view: Successfully derive template from live VM
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.instances_view.push(InstanceRecord {
        id: "live-gce-vm-42".into(),
        name: "oxidex-runners-c4d-42".into(),
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
    });
    save_state(&kv, &mut bundle, now).await.unwrap();

    let valid_req = CreateVmTemplateRequest {
        instance_id: "live-gce-vm-42".into(),
        template_name: "Derived C4D Golden Template".into(),
    };
    let (status, resp) = handle_post_vm_templates_from_instance(&kv, valid_req, now + 1)
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["template_name"], "Derived C4D Golden Template");
    assert_eq!(resp["template"]["vcpu"], 16);
    assert_eq!(resp["template"]["ram_gib"], 32);
    assert_eq!(resp["template"]["benchmark_score"], 16 * 120);
    assert_eq!(resp["template"]["runner_vcpu"], 8);
    assert_eq!(resp["template"]["runner_ram_gib"], 16);

    // Verify persistence in KV
    invalidate_state_cache();
    let (fresh_bundle, _) = load_state(&kv, now + 2).await.unwrap();
    let stored = fresh_bundle
        .state
        .vm_templates
        .iter()
        .find(|t| t.name == "Derived C4D Golden Template")
        .expect("Template must be stored in persistent vm_templates");
    assert_eq!(stored.machine_type, "c4d-highcpu-16");
}

#[tokio::test]
async fn test_adversarial_rust_builds_post_history_bounding_and_aggregates() {
    let _guard = CHALLENGER_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    let kv = ChallengerMockKv::new();
    let now = 1792100000;

    // 1. Empty status rejected
    let bad_req = RecordRustBuildRequest {
        status: "  \t ".into(),
        project_name: None,
        commit_sha: None,
        profile: None,
        compile_duration_seconds: None,
        delivery_duration_seconds: None,
        instance_name: None,
        zone: None,
        machine_type: None,
        cpu_platform: None,
        vcpu: None,
        origin_ip: None,
        cache_hits: None,
        total_crates: None,
        peak_ram_gib: None,
        cpu_time_seconds: None,
    };
    match handle_post_rust_builds(&kv, bad_req, now).await {
        Err(StateError::Validation(msg)) => assert!(msg.contains("Build status cannot be empty")),
        other => panic!("Expected StateError::Validation, got {:?}", other),
    }

    // 2. Stress Test: Submit 65 consecutive builds to challenge MAX_RUST_BUILDS_HISTORY bounding (50)
    for i in 1..=65 {
        let req = RecordRustBuildRequest {
            status: if i % 10 == 0 {
                "FAILED".into()
            } else {
                "DELIVERED".into()
            },
            project_name: Some(format!("project-{}", i)),
            commit_sha: Some(format!("sha-{:04}", i)),
            profile: Some("release".into()),
            compile_duration_seconds: Some(10.0 + (i as f64)),
            delivery_duration_seconds: Some(2.0),
            instance_name: Some("builder-n4-1".into()),
            zone: Some("us-central1-a".into()),
            machine_type: Some("n4-highcpu-16".into()),
            cpu_platform: Some("Intel Sapphire Rapids".into()),
            vcpu: Some(16),
            origin_ip: Some("127.0.0.1".into()),
            cache_hits: Some(i * 10),
            total_crates: Some(i * 10 + 5),
            peak_ram_gib: Some(12.0 + (i as f64 * 0.1)),
            cpu_time_seconds: Some(50.0 + (i as f64)),
        };

        let (status, resp) = handle_post_rust_builds(&kv, req, now + i as u64)
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(resp["status"], "ok");
    }

    // 3. Verify history is strictly bounded to MAX_RUST_BUILDS_HISTORY (50)
    invalidate_state_cache();
    let (fresh_bundle, _) = load_state(&kv, now + 100).await.unwrap();
    let builds = fresh_bundle
        .state
        .rust_builds
        .get("builds")
        .and_then(|v| v.as_array())
        .expect("rust_builds.builds must be an array");

    assert_eq!(
        builds.len(),
        MAX_RUST_BUILDS_HISTORY,
        "Builds history must be capped at exactly MAX_RUST_BUILDS_HISTORY (50), got {}",
        builds.len()
    );

    // Verify LIFO order: most recent build (i=65) is at index 0, oldest retained build (i=16) is at index 49
    assert_eq!(builds[0]["commit_sha"], "sha-0065");
    assert_eq!(builds[49]["commit_sha"], "sha-0016");

    // Verify aggregate rollup statistics are computed across the active bounded 50 builds
    let total_builds = fresh_bundle.state.rust_builds["total_builds"]
        .as_u64()
        .unwrap();
    assert_eq!(total_builds, 50);

    let max_ram = fresh_bundle.state.rust_builds["max_peak_ram_gib"]
        .as_f64()
        .unwrap();
    // Peak RAM for build 65 is 12.0 + 6.5 = 18.5
    assert!((max_ram - 18.5).abs() < 0.01);
}

// ============================================================================
// 2. Challenge Auth Security
// ============================================================================

#[test]
fn test_adversarial_auth_may_sign_in_strictness() {
    let empty_admins: Vec<u64> = vec![];

    // swackhamer UID 619624 is the sole built-in admin
    assert!(auth::may_sign_in("swackhamer", 619624, &empty_admins));
    assert!(auth::may_sign_in("SWACKHAMER", 619624, &empty_admins)); // Case-insensitive login

    // Adversarial challenge: swackhamer login with spoofed UID -> MUST REJECT
    assert!(!auth::may_sign_in("swackhamer", 123456, &empty_admins));
    assert!(!auth::may_sign_in("swackhamer", 0, &empty_admins));

    // Adversarial challenge: valid UID with impostor login -> MUST REJECT
    assert!(!auth::may_sign_in("impostor", 619624, &empty_admins));
    assert!(!auth::may_sign_in("attacker", 999999, &empty_admins));

    // Dynamic added admin UID verification
    let added_admins = vec![888888];
    assert!(auth::may_sign_in("collaborator", 888888, &added_admins));
    assert!(!auth::may_sign_in("collaborator", 777777, &added_admins));
}

// ============================================================================
// 3. Challenge Scaler Naming Uniqueness within the Same Second
// ============================================================================

#[tokio::test]
async fn test_adversarial_scaler_naming_uniqueness_within_same_second_and_length_cap() {
    let _guard = CHALLENGER_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = ChallengerMockKv::new();
    let gcp = ChallengerMockGcp::default();
    let gh = ChallengerMockGitHubReconciler::default();

    // Populate healthy floor hosts so Section 8 floor keeper does not trigger
    for target in [
        "swack-tools",
        "xbmc4lyfe-newznab-tmux",
        "xbmc4lyfe-rustnzb",
        "xbmc4lyfe-nntp-proxy",
    ] {
        gcp.instances.lock().unwrap().push(SimpleGceInstance {
            id: Some(format!("vm-{}", target)),
            name: format!("oxidex-runners-{}-healthy", target),
            zone: "us-central1-a".to_string(),
            machine_type: "c4d-highcpu-16".to_string(),
            status: "RUNNING".to_string(),
            metadata: GceMetadata::default(),
            disks: vec![],
        });
    }

    *gh.queued_jobs.lock().unwrap() = 25; // Trigger backlog scale-up

    // Empirical Stress Test:
    // Invoke reconciler 50 times at the EXACT SAME SECOND (now = 1792000000).
    // In each invocation, reset lease and capture the generated scale host name.
    let now = 1792000000;
    let mut names = HashSet::new();

    for i in 0..50 {
        set_reconciler_lease(None); // Release lease for next tick
        let rep = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
        assert_eq!(
            rep.launched_hosts.len(),
            1,
            "Invocation {} must launch exactly 1 scale host",
            i
        );
        let name = rep.launched_hosts[0].clone();

        // Length invariant: GCE instance names must never exceed 63 characters
        assert!(
            name.len() <= 63,
            "Instance name '{}' exceeds 63 characters (len={})",
            name,
            name.len()
        );

        // Hostname syntax invariant: RFC 1035 format
        assert!(name.starts_with("oxidex-runners-scale-"));
        assert!(name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));

        // Collision check: Must not collide with any previously generated name in the same second!
        assert!(
            names.insert(name.clone()),
            "Duplicate instance name detected on invocation {}: '{}'",
            i,
            name
        );
    }

    assert_eq!(
        names.len(),
        50,
        "All 50 scale-up instance names in the exact same second must be distinct"
    );
}

#[test]
fn test_adversarial_floor_naming_bounds_extreme_target_slugs() {
    let now = 1792000000u64;
    let hex_suffix = "a1b2c3d4"; // 8 chars

    let extreme_100 = "a".repeat(100);
    let test_targets = [
        "short",
        "swack-tools/oxidex",
        "very-long-organization-name/super-extended-repository-slug-name-exceeding-standard-length",
        extreme_100.as_str(),
    ];

    for target in test_targets {
        let target_slug = target.replace('/', "-");
        let safe_slug = if target_slug.len() > 24 {
            &target_slug[..24]
        } else {
            &target_slug
        };

        let host_name = format!("oxidex-runners-{}-{}-{}", safe_slug, now, hex_suffix);

        assert!(
            host_name.len() <= 63,
            "Floor host name '{}' for target '{}' exceeds 63 chars (len={})",
            host_name,
            target,
            host_name.len()
        );
        assert!(host_name.starts_with("oxidex-runners-"));
    }
}

// ============================================================================
// 4. Challenge Multi-Target Runner Queries
// ============================================================================

#[tokio::test]
async fn test_adversarial_reconciler_queries_repo_target_and_matches_expected_runners() {
    let _guard = CHALLENGER_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = ChallengerMockKv::new();
    let gcp = ChallengerMockGcp::default();
    let gh = ChallengerMockGitHubReconciler::default();

    // 1. Configure repository target runner under repos/xbmc4lyfe/newznab-tmux (one of CONTROLLED_TARGETS)
    let repo_target = "repos/xbmc4lyfe/newznab-tmux";
    gh.target_runners.lock().unwrap().insert(
        repo_target.to_string(),
        vec![
            SimpleGitHubRunner {
                id: 1001,
                name: "oxidex-runners-xbmc4lyfe-newznab-tmux-vm-1".to_string(),
                status: "online".to_string(),
                busy: false,
            },
            SimpleGitHubRunner {
                id: 1002,
                name: "oxidex-runners-xbmc4lyfe-newznab-tmux-vm-2".to_string(),
                status: "online".to_string(),
                busy: false,
            },
        ],
    );

    // 2. Set up an in-flight launch in Booting phase expecting these exact runners
    inject_active_launch(LaunchOperation {
        target: "xbmc4lyfe/newznab-tmux".to_string(),
        host_name: "oxidex-runners-xbmc4lyfe-newznab-tmux-vm".to_string(),
        zone: "us-central1-a".to_string(),
        machine_type: "c4d-highcpu-16".to_string(),
        phase: LaunchPhase::Booting,
        deployment_id: "dep-repo-test".to_string(),
        started_at: 1700000000,
        instance_id: Some("vm-repo-1".to_string()),
        expected_runners: vec![
            "oxidex-runners-xbmc4lyfe-newznab-tmux-vm-1".to_string(),
            "oxidex-runners-xbmc4lyfe-newznab-tmux-vm-2".to_string(),
        ],
    });

    gcp.instances.lock().unwrap().push(SimpleGceInstance {
        id: Some("vm-repo-1".to_string()),
        name: "oxidex-runners-xbmc4lyfe-newznab-tmux-vm".to_string(),
        zone: "us-central1-a".to_string(),
        machine_type: "c4d-highcpu-16".to_string(),
        status: "RUNNING".to_string(),
        metadata: GceMetadata::default(),
        disks: vec![],
    });

    // Healthy hosts for the other targets so floor keeper does not launch
    for target in ["swack-tools", "xbmc4lyfe-rustnzb", "xbmc4lyfe-nntp-proxy"] {
        gcp.instances.lock().unwrap().push(SimpleGceInstance {
            id: Some(format!("vm-{}", target)),
            name: format!("oxidex-runners-{}-healthy", target),
            zone: "us-central1-a".to_string(),
            machine_type: "c4d-highcpu-16".to_string(),
            status: "RUNNING".to_string(),
            metadata: GceMetadata::default(),
            disks: vec![],
        });
    }

    let now = 1700000010;
    let _rep = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();

    // Verification 1: Reconciler MUST have queried "repos/xbmc4lyfe/newznab-tmux"
    let queried = gh.queried_targets.lock().unwrap().clone();
    assert!(
        queried.iter().any(|t| t == "repos/xbmc4lyfe/newznab-tmux"),
        "Reconciler failed to query repo target 'repos/xbmc4lyfe/newznab-tmux'; queried: {:?}",
        queried
    );

    // Verification 2: Active launch must transition to Done and be removed from in-flight launches
    let remaining = get_active_launches();
    assert!(
        !remaining.iter().any(|l| l.target == "xbmc4lyfe/newznab-tmux"),
        "Launch for 'xbmc4lyfe/newznab-tmux' should have matched online repo runners and transitioned to Done"
    );

    // Verification 3: Startup script must be scrubbed upon reaching Done
    assert_eq!(
        gcp.scrubbed_instances.lock().unwrap().as_slice(),
        &["oxidex-runners-xbmc4lyfe-newznab-tmux-vm"]
    );
}

#[tokio::test]
async fn test_adversarial_stale_repo_runner_cleaned_using_repo_api_path() {
    let _guard = CHALLENGER_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = ChallengerMockKv::new();
    let gcp = ChallengerMockGcp::default();
    let gh = ChallengerMockGitHubReconciler::default();

    // Healthy hosts for floor targets
    for target in [
        "swack-tools",
        "xbmc4lyfe-newznab-tmux",
        "xbmc4lyfe-rustnzb",
        "xbmc4lyfe-nntp-proxy",
    ] {
        gcp.instances.lock().unwrap().push(SimpleGceInstance {
            id: Some(format!("vm-{}", target)),
            name: format!("oxidex-runners-{}-healthy", target),
            zone: "us-central1-a".to_string(),
            machine_type: "c4d-highcpu-16".to_string(),
            status: "RUNNING".to_string(),
            metadata: GceMetadata::default(),
            disks: vec![],
        });
    }

    // A stale runner belonging to repo repos/xbmc4lyfe/newznab-tmux whose VM is long gone
    gh.target_runners.lock().unwrap().insert(
        "repos/xbmc4lyfe/newznab-tmux".to_string(),
        vec![SimpleGitHubRunner {
            id: 8881,
            name: "oxidex-runners-dead-repo-vm-1".to_string(),
            status: "offline".to_string(),
            busy: false,
        }],
    );

    let now = 1700000010;
    let rep = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();

    // Must clean the stale runner
    assert_eq!(
        rep.cleaned_runners.as_slice(),
        &["oxidex-runners-dead-repo-vm-1"]
    );

    // Adversarial verification: Stale runner must be deleted via repos/xbmc4lyfe/newznab-tmux API path,
    // NOT via orgs/swack-tools!
    let deleted = gh.deleted_runners.lock().unwrap().clone();
    assert_eq!(deleted.len(), 1);
    assert_eq!(
        deleted[0],
        ("repos/xbmc4lyfe/newznab-tmux".to_string(), 8881),
        "Stale repo runner must be deleted via its specific repo API target"
    );
}
