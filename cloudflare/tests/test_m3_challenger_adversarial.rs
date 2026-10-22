//! Empirical Adversarial Challenge Suite for Milestone 3
//! GCP Spot Fleet Controller, Scheduled Reconciler & KV Quota Conservation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use spot_dashboard_edge::gcp::{GceInsertInstanceRequest, GceMetadata, GceOperation};
use spot_dashboard_edge::github::RunnerBootstrapConfig;
use spot_dashboard_edge::patch::CONTROLLED_TARGETS;
use spot_dashboard_edge::reconciler::*;
use spot_dashboard_edge::state::{invalidate_state_cache, KvStoreBackend, StateError};

// ============================================================================
// Adversarial Test Doubles
// ============================================================================

pub struct AdversarialMockKvStore {
    pub data: Mutex<HashMap<String, String>>,
    pub get_count: AtomicUsize,
    pub put_count: AtomicUsize,
}

impl AdversarialMockKvStore {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            get_count: AtomicUsize::new(0),
            put_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for AdversarialMockKvStore {
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
pub struct AdversarialGcpReconciler {
    pub instances: Arc<Mutex<Vec<SimpleGceInstance>>>,
    pub deleted_instances: Arc<Mutex<Vec<String>>>,
    pub scrubbed_instances: Arc<Mutex<Vec<String>>>,
    pub inserted_instances: Arc<Mutex<Vec<GceInsertInstanceRequest>>>,
}

#[async_trait::async_trait(?Send)]
impl GcpReconcilerClient for AdversarialGcpReconciler {
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
            id: Some("op-insert".into()),
            name: Some("op-insert".into()),
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
            id: Some("op-delete".into()),
            name: Some("op-delete".into()),
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
            id: Some("op-scrub".into()),
            name: Some("op-scrub".into()),
            status: "DONE".into(),
            target_link: None,
            error: None,
        })
    }
}

#[derive(Default, Clone)]
pub struct AdversarialGitHubReconciler {
    pub runners: Arc<Mutex<Vec<SimpleGitHubRunner>>>,
    pub deleted_runners: Arc<Mutex<Vec<u64>>>,
    pub queued_jobs: Arc<Mutex<u32>>,
}

#[async_trait::async_trait(?Send)]
impl GitHubReconcilerClient for AdversarialGitHubReconciler {
    async fn list_runners(
        &self,
        _target_api_path: &str,
    ) -> Result<Vec<SimpleGitHubRunner>, ReconcilerError> {
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
            runner_1_jitconfig: "jit-slot-1-token".to_string(),
            runner_2_jitconfig: "jit-slot-2-token".to_string(),
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

static ADVERSARIAL_TEST_MUTEX: Mutex<()> = Mutex::new(());

fn populate_all_floor_hosts(gcp: &AdversarialGcpReconciler) {
    for target in CONTROLLED_TARGETS {
        let slug = target.replace('/', "-").to_lowercase();
        gcp.instances.lock().unwrap().push(SimpleGceInstance {
            id: Some(format!("vm-{}", slug)),
            name: format!("oxidex-runners-{}-healthy", slug),
            zone: "us-central1-a".to_string(),
            machine_type: "c4d-highcpu-16".to_string(),
            status: "RUNNING".to_string(),
            metadata: GceMetadata::default(),
            disks: vec![],
        });
    }
}

// ============================================================================
// 1. Strictly 0 KV Writes Stress Test Across 500 Unchanged Ticks
// ============================================================================

#[tokio::test]
async fn test_adversarial_diff_gating_500_ticks_zero_kv_writes() {
    let _guard = ADVERSARIAL_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = AdversarialMockKvStore::new();
    let gcp = AdversarialGcpReconciler::default();
    let gh = AdversarialGitHubReconciler::default();

    populate_all_floor_hosts(&gcp);

    let mut now = 1700000000;

    // Initial tick: writes state snapshot (1 KV put)
    let rep1 = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert!(rep1.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // Stress test: 500 consecutive ticks advancing by 2 seconds each (total 1000s < 1800s heartbeat)
    for tick_idx in 2..=501 {
        now += 2;
        let rep = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
        assert_eq!(rep.tick, tick_idx);
        assert!(
            !rep.snapshot_written,
            "Tick {} wrote snapshot despite 0 fleet changes!",
            tick_idx
        );
        assert_eq!(rep.launched_hosts.len(), 0);
        assert_eq!(rep.cleaned_vms.len(), 0);
        assert_eq!(rep.cleaned_runners.len(), 0);
    }

    // Assert strictly zero additional KV writes across all 500 unchanged ticks
    assert_eq!(
        kv.put_count.load(Ordering::SeqCst),
        1,
        "Expected strictly 1 KV write for all 501 ticks"
    );
}

// ============================================================================
// 2. Coarse Heartbeat Boundary and Clock Skew Challenge
// ============================================================================

#[tokio::test]
async fn test_adversarial_coarse_heartbeat_boundaries_and_clock_skew() {
    let _guard = ADVERSARIAL_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = AdversarialMockKvStore::new();
    let gcp = AdversarialGcpReconciler::default();
    let gh = AdversarialGitHubReconciler::default();

    populate_all_floor_hosts(&gcp);

    let base_now = 1700000000;
    // Initial tick at T0
    let rep0 = run_reconciliation(&kv, &gcp, &gh, base_now).await.unwrap();
    assert!(rep0.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // Clock skew test: timestamp moving backward (e.g. system clock adjust)
    // T = base_now - 500s -> must NOT crash or trigger false write
    let rep_skew = run_reconciliation(&kv, &gcp, &gh, base_now.saturating_sub(500))
        .await
        .unwrap();
    assert!(!rep_skew.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // Exact boundary test at 1799s (1 second before 30m heartbeat)
    let rep_1799 = run_reconciliation(&kv, &gcp, &gh, base_now + 1799)
        .await
        .unwrap();
    assert!(
        !rep_1799.snapshot_written,
        "At 1799s heartbeat should not have fired yet"
    );
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // Exact boundary test at 1800s (exactly 30m heartbeat) -> MUST write snapshot
    let rep_1800 = run_reconciliation(&kv, &gcp, &gh, base_now + 1800)
        .await
        .unwrap();
    assert!(
        rep_1800.snapshot_written,
        "At 1800s heartbeat MUST trigger snapshot write"
    );
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 2);

    // Immediately subsequent tick at 1801s (only 1s elapsed since new written_at) -> 0 writes
    let rep_1801 = run_reconciliation(&kv, &gcp, &gh, base_now + 1801)
        .await
        .unwrap();
    assert!(
        !rep_1801.snapshot_written,
        "Heartbeat just fired, 1801s should not write"
    );
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 2);

    // Second heartbeat at 1800 + 1800 = 3600s
    let rep_3600 = run_reconciliation(&kv, &gcp, &gh, base_now + 3600)
        .await
        .unwrap();
    assert!(
        rep_3600.snapshot_written,
        "Second heartbeat at 3600s MUST trigger snapshot write"
    );
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 3);
}

// ============================================================================
// 3. Scaling Thresholds Challenge: 0, 19, 20, 21, 100, and Capacity Saturation
// ============================================================================

#[tokio::test]
async fn test_adversarial_scaling_threshold_boundaries_and_capacity_cap() {
    let _guard = ADVERSARIAL_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = AdversarialMockKvStore::new();
    let gcp = AdversarialGcpReconciler::default();
    let gh = AdversarialGitHubReconciler::default();

    populate_all_floor_hosts(&gcp);
    let now = 1700000000;

    // Sub-threshold check: 0, 1, 10, 18, 19 queued jobs -> NO scale up!
    for queued in [0, 1, 10, 18, 19] {
        *gh.queued_jobs.lock().unwrap() = queued;
        let rep = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
        assert_eq!(
            rep.launched_hosts.len(),
            0,
            "Queued jobs {} should NOT scale up",
            queued
        );
    }

    // Exact threshold check: 20 queued jobs -> MUST scale up exactly 1 host
    *gh.queued_jobs.lock().unwrap() = 20;
    let rep20 = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert_eq!(rep20.launched_hosts.len(), 1);
    assert!(rep20.launched_hosts[0].contains("scale"));

    // Over-threshold check: 21 queued jobs -> MUST scale up
    *gh.queued_jobs.lock().unwrap() = 21;
    let rep21 = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert_eq!(rep21.launched_hosts.len(), 1);

    // High backlog check: 100 queued jobs -> MUST scale up
    *gh.queued_jobs.lock().unwrap() = 100;
    let rep100 = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert_eq!(rep100.launched_hosts.len(), 1);

    // Default state has max_runners = 24 -> max_instances = 24 / 2 = 12 instances.
    // Add 12 running instances to reach max capacity
    gcp.instances.lock().unwrap().clear();
    for i in 1..=12 {
        gcp.instances.lock().unwrap().push(SimpleGceInstance {
            id: Some(format!("vm-cap-{}", i)),
            name: format!("oxidex-runners-swack-tools-cap-{}", i),
            zone: "us-central1-a".to_string(),
            machine_type: "c4d-highcpu-16".to_string(),
            status: "RUNNING".to_string(),
            metadata: GceMetadata::default(),
            disks: vec![],
        });
    }

    // Even with 100 queued jobs, current_live (12) >= max_instances (12) -> MUST NOT exceed capacity!
    *gh.queued_jobs.lock().unwrap() = 100;
    let rep_saturated = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    let scale_hosts: Vec<_> = rep_saturated
        .launched_hosts
        .iter()
        .filter(|h| h.contains("scale"))
        .collect();
    assert_eq!(
        scale_hosts.len(),
        0,
        "Must not scale up beyond max instance capacity"
    );
}

// ============================================================================
// 4. Floor Target Keeper: 0 Hosts, Partial Floor, and In-Flight Deduplication
// ============================================================================

#[tokio::test]
async fn test_adversarial_floor_keeper_exact_specs_and_inflight_dedup() {
    let _guard = ADVERSARIAL_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = AdversarialMockKvStore::new();
    let gcp = AdversarialGcpReconciler::default();
    let gh = AdversarialGitHubReconciler::default();
    let now = 1700000000;

    // 0 hosts in GCE: Floor keeper must launch exactly 1 host per controlled target (4 targets)
    let rep = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();
    assert_eq!(rep.launched_hosts.len(), CONTROLLED_TARGETS.len());
    assert_eq!(
        gcp.inserted_instances.lock().unwrap().len(),
        CONTROLLED_TARGETS.len()
    );

    // Validate hardware floor and configuration invariants for every launched host
    for req in gcp.inserted_instances.lock().unwrap().iter() {
        assert_eq!(
            req.machine_type,
            format!(
                "zones/{}/machineTypes/{}",
                DEFAULT_ZONE, DEFAULT_MACHINE_TYPE
            )
        );
        assert_eq!(req.scheduling.provisioning_model, "SPOT");
        assert_eq!(req.scheduling.instance_termination_action, "DELETE");
        assert_eq!(req.metadata.get("spot-runner-count"), Some("2"));
        assert_eq!(
            req.metadata.get("runner_1_jitconfig"),
            Some("jit-slot-1-token")
        );
        assert_eq!(
            req.metadata.get("runner_2_jitconfig"),
            Some("jit-slot-2-token")
        );
        let startup = req.metadata.get("startup-script").unwrap();
        assert!(
            startup.starts_with("#!/usr/bin/env bash") || startup.starts_with("#!/bin/bash"),
            "Startup script has invalid shebang: {}",
            &startup[..50]
        );
        assert!(
            !startup.contains("{{"),
            "Startup script contains unexpanded template tokens!"
        );
    }

    // Next tick: In-flight launches exist in ACTIVE_LAUNCHES -> Floor keeper MUST NOT duplicate!
    let rep_inflight = run_reconciliation(&kv, &gcp, &gh, now + 10).await.unwrap();
    assert_eq!(
        rep_inflight.launched_hosts.len(),
        0,
        "In-flight launches must prevent duplicate provisioning"
    );
}

// ============================================================================
// 5. Preempted VM Deletion: TERMINATED vs All Non-Terminated States
// ============================================================================

#[tokio::test]
async fn test_adversarial_preempted_vm_status_matrix() {
    let _guard = ADVERSARIAL_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = AdversarialMockKvStore::new();
    let gcp = AdversarialGcpReconciler::default();
    let gh = AdversarialGitHubReconciler::default();

    // Adversarial status matrix:
    // Terminated variants (must be deleted): "TERMINATED", "terminated", "Terminated"
    // Non-terminated states (must be preserved): "RUNNING", "PROVISIONING", "STAGING", "STOPPING", "SUSPENDED", "REPAIRING"
    let statuses = vec![
        ("vm-term-upper", "TERMINATED", true),
        ("vm-term-lower", "terminated", true),
        ("vm-term-mixed", "Terminated", true),
        ("vm-running", "RUNNING", false),
        ("vm-provisioning", "PROVISIONING", false),
        ("vm-staging", "STAGING", false),
        ("vm-stopping", "STOPPING", false),
        ("vm-suspended", "SUSPENDED", false),
        ("vm-repairing", "REPAIRING", false),
    ];

    for (name, status, _) in &statuses {
        gcp.instances.lock().unwrap().push(SimpleGceInstance {
            id: Some(format!("id-{}", name)),
            name: name.to_string(),
            zone: "us-central1-a".to_string(),
            machine_type: "c4d-highcpu-16".to_string(),
            status: status.to_string(),
            metadata: GceMetadata::default(),
            disks: vec![],
        });
    }

    let now = 1700000000;
    let _rep = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();

    let deleted = gcp.deleted_instances.lock().unwrap().clone();
    assert_eq!(deleted.len(), 3);
    assert!(deleted.contains(&"vm-term-upper".to_string()));
    assert!(deleted.contains(&"vm-term-lower".to_string()));
    assert!(deleted.contains(&"vm-term-mixed".to_string()));

    // Verify non-terminated instances were NOT touched
    for (name, _, should_delete) in &statuses {
        if !should_delete {
            assert!(
                !deleted.contains(&name.to_string()),
                "Instance {} should NOT be deleted",
                name
            );
        }
    }
}

// ============================================================================
// 6. Stale Runner Registration Cleanup Matrix
// ============================================================================

#[tokio::test]
async fn test_adversarial_stale_runner_cleanup_matrix() {
    let _guard = ADVERSARIAL_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = AdversarialMockKvStore::new();
    let gcp = AdversarialGcpReconciler::default();
    let gh = AdversarialGitHubReconciler::default();

    // 1 Live VM in GCE
    gcp.instances.lock().unwrap().push(SimpleGceInstance {
        id: Some("vm-live".into()),
        name: "oxidex-runners-swack-live".into(),
        zone: "us-central1-a".into(),
        machine_type: "c4d-highcpu-16".into(),
        status: "RUNNING".into(),
        metadata: GceMetadata::default(),
        disks: vec![],
    });

    // Runner scenarios:
    // 1. Online & Idle matching live VM -> PROTECTED
    // 2. Online & Busy matching live VM -> PROTECTED
    // 3. Online & Idle NO live VM -> PROTECTED (heartbeat active, VM query might be partial)
    // 4. Offline & Busy matching live VM -> PROTECTED (job running/draining)
    // 5. Offline & Busy NO live VM -> PROTECTED (draining before shutdown)
    // 6. Offline & Idle matching live VM -> PROTECTED (VM may be restarting)
    // 7. Offline & Idle NO live VM (status: "offline") -> DELETED!
    // 8. Offline & Idle NO live VM (status: "OFFLINE") -> DELETED!
    // 9. Offline & Idle NO live VM (status: "Offline") -> DELETED!
    gh.runners.lock().unwrap().extend(vec![
        SimpleGitHubRunner {
            id: 101,
            name: "oxidex-runners-swack-live-r1".into(),
            status: "online".into(),
            busy: false,
        },
        SimpleGitHubRunner {
            id: 102,
            name: "oxidex-runners-swack-live-r2".into(),
            status: "online".into(),
            busy: true,
        },
        SimpleGitHubRunner {
            id: 103,
            name: "oxidex-runners-swack-ghost-r1".into(),
            status: "online".into(),
            busy: false,
        },
        SimpleGitHubRunner {
            id: 104,
            name: "oxidex-runners-swack-live-r3".into(),
            status: "offline".into(),
            busy: true,
        },
        SimpleGitHubRunner {
            id: 105,
            name: "oxidex-runners-swack-ghost-r2".into(),
            status: "offline".into(),
            busy: true,
        },
        SimpleGitHubRunner {
            id: 106,
            name: "oxidex-runners-swack-live-r4".into(),
            status: "offline".into(),
            busy: false,
        },
        SimpleGitHubRunner {
            id: 201,
            name: "oxidex-runners-dead-host-1".into(),
            status: "offline".into(),
            busy: false,
        },
        SimpleGitHubRunner {
            id: 202,
            name: "oxidex-runners-dead-host-2".into(),
            status: "OFFLINE".into(),
            busy: false,
        },
        SimpleGitHubRunner {
            id: 203,
            name: "oxidex-runners-dead-host-3".into(),
            status: "Offline".into(),
            busy: false,
        },
    ]);

    let now = 1700000000;
    let _rep = run_reconciliation(&kv, &gcp, &gh, now).await.unwrap();

    let deleted_ids = gh.deleted_runners.lock().unwrap().clone();
    assert_eq!(deleted_ids.len(), 3);
    assert!(deleted_ids.contains(&201));
    assert!(deleted_ids.contains(&202));
    assert!(deleted_ids.contains(&203));

    // Protected runners must not be in deleted list
    for protected_id in [101, 102, 103, 104, 105, 106] {
        assert!(
            !deleted_ids.contains(&protected_id),
            "Runner {} was incorrectly deleted",
            protected_id
        );
    }
}

// ============================================================================
// 7. Concurrency Lease 240s Guard Stress Test
// ============================================================================

#[tokio::test]
async fn test_adversarial_concurrency_lease_skips_and_recovery() {
    let _guard = ADVERSARIAL_TEST_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = AdversarialMockKvStore::new();
    let gcp = AdversarialGcpReconciler::default();
    let gh = AdversarialGitHubReconciler::default();
    let now = 1700000000;

    // Simulate an ongoing tick with active lease until now + 240s
    set_reconciler_lease(Some(now + 240));

    // Overlapping tick 1: now + 1s -> must skip
    let rep1 = run_reconciliation(&kv, &gcp, &gh, now + 1).await.unwrap();
    assert_eq!(rep1.status, "skipped_lease_active");
    assert!(!rep1.snapshot_written);

    // Overlapping tick 2: now + 239s -> must skip
    let rep2 = run_reconciliation(&kv, &gcp, &gh, now + 239).await.unwrap();
    assert_eq!(rep2.status, "skipped_lease_active");

    // Expired lease tick: now + 241s -> executes normally
    let rep3 = run_reconciliation(&kv, &gcp, &gh, now + 241).await.unwrap();
    assert_eq!(rep3.status, "reconciled");
}
