//! Test Suite for Scheduled Reconciler & Strict KV Quota Conservation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use spot_dashboard_edge::gcp::{GceInsertInstanceRequest, GceMetadata, GceOperation};
use spot_dashboard_edge::github::RunnerBootstrapConfig;
use spot_dashboard_edge::reconciler::*;
use spot_dashboard_edge::state::{invalidate_state_cache, KvStoreBackend, StateError};

// ============================================================================
// Mock Test Doubles
// ============================================================================

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

#[derive(Default, Clone)]
pub struct MockGcpReconciler {
    pub instances: Arc<Mutex<Vec<SimpleGceInstance>>>,
    pub deleted_instances: Arc<Mutex<Vec<String>>>,
    pub scrubbed_instances: Arc<Mutex<Vec<String>>>,
    pub inserted_instances: Arc<Mutex<Vec<GceInsertInstanceRequest>>>,
}

#[async_trait::async_trait(?Send)]
impl GcpReconcilerClient for MockGcpReconciler {
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
            id: Some("op-insert-1".into()),
            name: Some("op-insert-1".into()),
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
            id: Some("op-delete-1".into()),
            name: Some("op-delete-1".into()),
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
            id: Some("op-scrub-1".into()),
            name: Some("op-scrub-1".into()),
            status: "DONE".into(),
            target_link: None,
            error: None,
        })
    }
}

#[derive(Default, Clone)]
pub struct MockGitHubReconciler {
    pub runners: Arc<Mutex<Vec<SimpleGitHubRunner>>>,
    pub target_runners: Arc<Mutex<HashMap<String, Vec<SimpleGitHubRunner>>>>,
    pub deleted_runners: Arc<Mutex<Vec<u64>>>,
    pub queued_jobs: Arc<Mutex<u32>>,
}

#[async_trait::async_trait(?Send)]
impl GitHubReconcilerClient for MockGitHubReconciler {
    async fn list_runners(
        &self,
        target_api_path: &str,
    ) -> Result<Vec<SimpleGitHubRunner>, ReconcilerError> {
        if let Some(target_list) = self.target_runners.lock().unwrap().get(target_api_path) {
            return Ok(target_list.clone());
        }
        Ok(self.runners.lock().unwrap().clone())
    }

    async fn delete_runner(
        &self,
        _target_api_path: &str,
        runner_id: u64,
    ) -> Result<(), ReconcilerError> {
        self.deleted_runners.lock().unwrap().push(runner_id);
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
            runner_1_jitconfig: "jit1-token-xyz".to_string(),
            runner_2_jitconfig: "jit2-token-xyz".to_string(),
        })
    }

    async fn count_queued_jobs(&self, owner: &str, repo: &str) -> Result<u32, ReconcilerError> {
        if owner == "swack-tools" || repo == "oxidex" {
            Ok(*self.queued_jobs.lock().unwrap())
        } else {
            Ok(0)
        }
    }
}

static TEST_MUTEX: Mutex<()> = Mutex::new(());

// ============================================================================
// Test Cases
// ============================================================================

#[tokio::test]
async fn test_floor_creation_zero_hosts_launches_one_per_target() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();
    let now = 1700000000;

    // Zero hosts exist -> floor keeper must launch 1 host per controlled target
    let report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();

    assert_eq!(report.tick, 1);
    assert_eq!(report.status, "reconciled");
    // Default controlled targets: ["swack-tools", "xbmc4lyfe/newznab-tmux", "xbmc4lyfe/rustnzb", "xbmc4lyfe/nntp-proxy"] = 4 targets
    assert_eq!(report.launched_hosts.len(), 4);
    assert_eq!(gcp.inserted_instances.lock().unwrap().len(), 4);

    for inserted in gcp.inserted_instances.lock().unwrap().iter() {
        assert!(inserted.machine_type.contains("c4d-highcpu-16"));
        assert_eq!(inserted.scheduling.provisioning_model, "SPOT");
        assert_eq!(inserted.scheduling.instance_termination_action, "DELETE");
        // Verify metadata has 2-slot JIT configs and runner count = 2
        assert_eq!(inserted.metadata.get("spot-runner-count"), Some("2"));
        assert_eq!(
            inserted.metadata.get("runner_1_jitconfig"),
            Some("jit1-token-xyz")
        );
        assert_eq!(
            inserted.metadata.get("runner_2_jitconfig"),
            Some("jit2-token-xyz")
        );
    }
}

#[tokio::test]
async fn test_strict_zero_kv_writes_on_unchanged_ticks() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();

    // Populate healthy running hosts for each target
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

    let mut now = 1700000000;

    // Tick 1: Initial state sync writes the snapshot
    let report1 = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert!(report1.snapshot_written);
    let initial_puts = kv.put_count.load(Ordering::SeqCst);
    assert_eq!(initial_puts, 1);

    // Ticks 2 through 100: State is UNCHANGED and within 1800s heartbeat window
    // MANDATORY KV CONSERVATION REQUIREMENT: strictly 0 writes to SPOT_KV!
    for i in 2..=100 {
        now += 10; // advance by 10s each tick (< 1800s heartbeat)
        let report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
        assert_eq!(report.tick, i);
        assert!(
            !report.snapshot_written,
            "Tick {} should not write snapshot when unchanged",
            i
        );
        assert_eq!(report.launched_hosts.len(), 0);
        assert_eq!(report.cleaned_vms.len(), 0);
        assert_eq!(report.cleaned_runners.len(), 0);
    }

    // Assert that across all 99 subsequent ticks, put_count did NOT increment at all!
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_coarse_heartbeat_triggers_single_kv_write() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();

    // Populate healthy running hosts
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

    let now = 1700000000;
    // Tick 1: Initial state sync
    run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // Tick 2 at now + 1799s (< 1800s heartbeat): 0 writes
    let report2 = run_reconciliation(&kv, &gcp, &gh, now + 1799)
        .await
        .unwrap();
    assert!(!report2.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // Tick 3 at now + 1801s (>= 1800s heartbeat): exactly 1 write
    let report3 = run_reconciliation(&kv, &gcp, &gh, now + 1801)
        .await
        .unwrap();
    assert!(report3.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_scale_up_at_exact_20_queued_jobs() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();

    // Floor hosts are present
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

    let now = 1700000000;

    // Exact 20 queued jobs -> must scale up
    *gh.queued_jobs.lock().unwrap() = 20;

    let report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert_eq!(report.launched_hosts.len(), 1);
    assert!(report.launched_hosts[0].contains("scale"));
}

#[tokio::test]
async fn test_no_scale_up_at_19_queued_jobs() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();

    // Floor hosts are present
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

    let now = 1700000000;

    // Exact 19 queued jobs (< 20 threshold) -> NO scale up!
    *gh.queued_jobs.lock().unwrap() = 19;

    let report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert_eq!(report.launched_hosts.len(), 0);
}

#[tokio::test]
async fn test_scale_up_at_21_queued_jobs() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();

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

    let now = 1700000000;

    // 21 queued jobs (> 20 threshold) -> scales up
    *gh.queued_jobs.lock().unwrap() = 21;

    let report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert_eq!(report.launched_hosts.len(), 1);
}

#[tokio::test]
async fn test_cleanup_terminated_preempted_vms() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();

    // 1 Running VM, 1 Terminated (Preempted) VM, 1 Staging VM, 1 Stopping VM
    gcp.instances.lock().unwrap().extend(vec![
        SimpleGceInstance {
            id: Some("vm-1".into()),
            name: "oxidex-runners-running".into(),
            zone: "us-central1-a".into(),
            machine_type: "c4d-highcpu-16".into(),
            status: "RUNNING".into(),
            metadata: GceMetadata::default(),
            disks: vec![],
        },
        SimpleGceInstance {
            id: Some("vm-2".into()),
            name: "oxidex-runners-preempted".into(),
            zone: "us-central1-a".into(),
            machine_type: "c4d-highcpu-16".into(),
            status: "TERMINATED".into(),
            metadata: GceMetadata::default(),
            disks: vec![],
        },
        SimpleGceInstance {
            id: Some("vm-3".into()),
            name: "oxidex-runners-staging".into(),
            zone: "us-central1-a".into(),
            machine_type: "c4d-highcpu-16".into(),
            status: "STAGING".into(),
            metadata: GceMetadata::default(),
            disks: vec![],
        },
        SimpleGceInstance {
            id: Some("vm-4".into()),
            name: "oxidex-runners-stopping".into(),
            zone: "us-central1-a".into(),
            machine_type: "c4d-highcpu-16".into(),
            status: "STOPPING".into(),
            metadata: GceMetadata::default(),
            disks: vec![],
        },
    ]);

    let now = 1700000000;
    let report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();

    // Only TERMINATED instance is deleted; STAGING, STOPPING, RUNNING are protected
    assert_eq!(report.cleaned_vms, vec!["oxidex-runners-preempted"]);
    assert_eq!(
        gcp.deleted_instances.lock().unwrap().clone(),
        vec!["oxidex-runners-preempted"]
    );
}

#[tokio::test]
async fn test_stale_runner_registration_cleanup() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();

    // Active VM
    gcp.instances.lock().unwrap().push(SimpleGceInstance {
        id: Some("vm-live".into()),
        name: "oxidex-runners-active-vm".into(),
        zone: "us-central1-a".into(),
        machine_type: "c4d-highcpu-16".into(),
        status: "RUNNING".into(),
        metadata: GceMetadata::default(),
        disks: vec![],
    });

    // Runners:
    // 1. Online runner (live VM) -> protect
    // 2. Offline busy runner (ghost VM) -> protect (job in progress)
    // 3. Offline idle runner matching active VM -> protect (may be restarting)
    // 4. Offline idle runner with NO active VM -> STALE, must delete!
    gh.runners.lock().unwrap().extend(vec![
        SimpleGitHubRunner {
            id: 1,
            name: "oxidex-runners-active-vm-1".into(),
            status: "online".into(),
            busy: false,
        },
        SimpleGitHubRunner {
            id: 2,
            name: "oxidex-runners-ghost-vm-1".into(),
            status: "offline".into(),
            busy: true, // Protected because busy
        },
        SimpleGitHubRunner {
            id: 3,
            name: "oxidex-runners-active-vm-2".into(),
            status: "offline".into(),
            busy: false, // Protected because matches active VM
        },
        SimpleGitHubRunner {
            id: 4,
            name: "oxidex-runners-stale-vm-1".into(),
            status: "offline".into(),
            busy: false, // Stale! No matching VM in GCE
        },
    ]);

    let now = 1700000000;
    let report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();

    assert_eq!(report.cleaned_runners, vec!["oxidex-runners-stale-vm-1"]);
    assert_eq!(gh.deleted_runners.lock().unwrap().clone(), vec![4]);
}

#[tokio::test]
async fn test_concurrency_lease_240s_skips_overlapping_ticks() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();
    let now = 1700000000;

    // Simulate an ongoing tick holding the lease
    set_reconciler_lease(Some(now + 100));

    // While lease is active, new tick must skip
    let report1 = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert_eq!(report1.status, "skipped_lease_active");
    assert_eq!(report1.launched_hosts.len(), 0);
    assert_eq!(report1.cleaned_vms.len(), 0);
    assert_eq!(report1.cleaned_runners.len(), 0);
    assert!(!report1.snapshot_written);

    // After lease expires (at now + 101), next tick executes normally
    let report2 = run_reconciliation(&kv, &gcp, &gh, now + 101).await.unwrap();
    assert_eq!(report2.status, "reconciled");
    assert_eq!(RECONCILER_LEASE_TTL_SECS, 240);
}

#[tokio::test]
async fn test_backlog_scale_up_unique_instance_names_across_successive_ticks() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();

    // 4 floor hosts present
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

    // Tick 1
    let now1 = 1700000000;
    let rep1 = run_reconciliation(&kv, &gcp, &gh, now1).await.unwrap();
    assert_eq!(rep1.launched_hosts.len(), 1);
    let name1 = rep1.launched_hosts[0].clone();
    assert!(name1.starts_with("oxidex-runners-scale-"));
    assert!(name1.len() <= 63);

    // Advance time by 300s (still within the 10,000s window where &now[..6] was constant)
    let now2 = 1700000300;
    set_reconciler_lease(None);
    let rep2 = run_reconciliation(&kv, &gcp, &gh, now2).await.unwrap();
    assert_eq!(rep2.launched_hosts.len(), 1);
    let name2 = rep2.launched_hosts[0].clone();
    assert!(name2.starts_with("oxidex-runners-scale-"));
    assert!(name2.len() <= 63);

    // Crucial check: names must be completely distinct
    assert_ne!(
        name1, name2,
        "Scale-up instance names across successive ticks must never collide"
    );
}

#[tokio::test]
async fn test_reconciler_queries_runners_across_org_and_repo_targets() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();

    // Set up repo target runner
    let repo_target = "repos/xbmc4lyfe/newznab-tmux";
    gh.target_runners.lock().unwrap().insert(
        repo_target.to_string(),
        vec![
            SimpleGitHubRunner {
                id: 991,
                name: "oxidex-runners-xbmc4lyfe-newznab-tmux-r1".to_string(),
                status: "online".to_string(),
                busy: false,
            },
            SimpleGitHubRunner {
                id: 992,
                name: "oxidex-runners-xbmc4lyfe-newznab-tmux-r2".to_string(),
                status: "online".to_string(),
                busy: false,
            },
        ],
    );

    // Set up active launch waiting in Booting phase
    inject_active_launch(LaunchOperation {
        target: "xbmc4lyfe/newznab-tmux".to_string(),
        host_name: "oxidex-runners-xbmc4lyfe-newznab-tmux-vm".to_string(),
        zone: "us-central1-a".to_string(),
        machine_type: "c4d-highcpu-16".to_string(),
        phase: LaunchPhase::Booting,
        deployment_id: "dep-test".to_string(),
        started_at: 1700000000,
        instance_id: Some("vm-newznab".to_string()),
        expected_runners: vec![
            "oxidex-runners-xbmc4lyfe-newznab-tmux-r1".to_string(),
            "oxidex-runners-xbmc4lyfe-newznab-tmux-r2".to_string(),
        ],
    });

    gcp.instances.lock().unwrap().push(SimpleGceInstance {
        id: Some("vm-newznab".to_string()),
        name: "oxidex-runners-xbmc4lyfe-newznab-tmux-vm".to_string(),
        zone: "us-central1-a".to_string(),
        machine_type: "c4d-highcpu-16".to_string(),
        status: "RUNNING".to_string(),
        metadata: GceMetadata::default(),
        disks: vec![],
    });

    // Add healthy running hosts for the other 3 targets so Section 8 does not trigger floor launches
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
    let _report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();

    // Verify launch transitioned to Done and was pruned from ACTIVE_LAUNCHES
    let remaining_launches = get_active_launches();
    assert!(
        !remaining_launches
            .iter()
            .any(|l| l.target == "xbmc4lyfe/newznab-tmux"),
        "Expected xbmc4lyfe/newznab-tmux launch to complete and be removed from in-flight launches"
    );

    // Verify startup script was scrubbed
    assert_eq!(
        gcp.scrubbed_instances.lock().unwrap().as_slice(),
        &["oxidex-runners-xbmc4lyfe-newznab-tmux-vm"]
    );
}

async fn set_runner_floor_and_cap(kv: &MockKvStore, min_runners: u32, max_runners: u32, now: u64) {
    let (mut bundle, _) = spot_dashboard_edge::state::load_state(kv, now)
        .await
        .unwrap();
    bundle.state.scaling.cicd_runner.min_runners = min_runners;
    bundle.state.scaling.cicd_runner.max_runners = max_runners;
    spot_dashboard_edge::state::save_state(kv, &mut bundle, now)
        .await
        .unwrap();
    invalidate_state_cache();
}

#[tokio::test]
async fn test_zero_min_runners_disables_floor_keeper() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();
    let now = 1700000000;
    set_runner_floor_and_cap(&kv, 0, 24, now).await;

    let report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert_eq!(report.status, "reconciled");
    assert!(report.launched_hosts.is_empty());
    assert!(gcp.inserted_instances.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_zero_max_runners_disables_backlog_scale_up() {
    let _guard = TEST_MUTEX.lock().unwrap();
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = MockKvStore::new();
    let gcp = MockGcpReconciler::default();
    let gh = MockGitHubReconciler::default();
    let now = 1700000000;
    set_runner_floor_and_cap(&kv, 0, 0, now).await;
    *gh.queued_jobs.lock().unwrap() = 50;

    let report = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert!(report.launched_hosts.is_empty());
    assert!(gcp.inserted_instances.lock().unwrap().is_empty());
}
