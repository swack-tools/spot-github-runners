use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::thread;
use tempfile::TempDir;

use host_agent::admission::budget::{
    admission_budget, builder_memory_budget, memory_envelope, parse_docker_memory_usage, RunnerTier,
};
use host_agent::admission::headroom::run_headroom;
use host_agent::admission::peer::peer_owner;
use host_agent::admission::pool::LeasePool;
use host_agent::admission::types::{AdmissionConfig, AdmissionError, AdmissionKind};
use host_agent::platform::{DockerContainerInspect, MockSystemInspector};

#[test]
fn test_admission_budget_math() {
    let tier = RunnerTier {
        initial_slots: 2,
        maximum_slots: 4,
        ram_gib: 32.0,
    };

    // Valid budget
    let res = admission_budget(&tier, 2.0, 14.0, Some(2)).unwrap();
    assert_eq!(res.reserve_gib, 2.5); // max(2.0, 2.0) + 0.25 * 2
    assert_eq!(res.work_gib, 29.5);
    assert_eq!(res.concurrency, 2);

    // Invalid slots
    assert!(admission_budget(&tier, 2.0, 14.0, Some(5)).is_err());
    assert!(admission_budget(&tier, 2.0, 14.0, Some(0)).is_err());

    // Workload cannot fit
    assert!(admission_budget(&tier, 28.0, 14.0, Some(2)).is_err());
}

#[test]
fn test_builder_memory_budget_math() {
    assert_eq!(builder_memory_budget(32.0, 28.0, 0.0), 28.0);
    assert_eq!(builder_memory_budget(32.0, 20.0, 0.0), 0.0);
    assert_eq!(builder_memory_budget(32.0, 35.0, 0.0), 0.0);
    assert_eq!(builder_memory_budget(-1.0, 28.0, 0.0), 0.0);
}

#[test]
fn test_parse_docker_memory_usage() {
    let gib = parse_docker_memory_usage("1024MiB / 28GiB").unwrap();
    assert!((gib - 1.0).abs() < 0.001);

    let b = parse_docker_memory_usage("1073741824B").unwrap();
    assert!((b - 1.0).abs() < 0.001);

    let config = AdmissionConfig {
        role: Some("runner".into()),
        work_gib: 28.0,
        runner_gib: 14.0,
        builder_gib: 28.0,
        runner_names: vec!["runner-1".into(), "runner-2".into()],
        heavy_runner: None,
        heavy_runners: None,
    };
    let env = memory_envelope(&config, 32.0, 4.0, &["1024MiB / 28GiB".into()]).unwrap();
    assert!(env > 20.0);
}

#[test]
fn test_lease_pool_concurrency_and_flock() {
    let temp = TempDir::new().unwrap();
    let ledger = temp.path().join("leases.json");
    let pool_arc = Arc::new(std::sync::Mutex::new(
        LeasePool::new(ledger.clone(), 6.0, false).unwrap(),
    ));

    let sys = Arc::new(MockSystemInspector::new());
    // Register mock processes for owners
    for i in 0..16 {
        sys.add_process(100 + i, 1, "mock", "1000", "");
    }

    let mut handles = Vec::new();
    for i in 0..16 {
        let p = Arc::clone(&pool_arc);
        let s = Arc::clone(&sys);
        handles.push(thread::spawn(move || {
            let owner = format!("host:{}:1000", 100 + i);
            let mut pool = p.lock().unwrap();
            pool.acquire(&owner, 2.0, s.as_ref()).unwrap()
        }));
    }

    let mut successes = 0;
    for h in handles {
        if h.join().unwrap() {
            successes += 1;
        }
    }

    // Limit is 6.0, each request is 2.0 -> exactly 3 can succeed
    assert_eq!(successes, 3);

    // Active leases count must be 3
    let p = pool_arc.lock().unwrap();
    let active = p.active_leases().unwrap();
    assert_eq!(active.len(), 3);
}

#[test]
fn test_heavy_builder_isolation() {
    let temp = TempDir::new().unwrap();
    let ledger = temp.path().join("leases.json");
    let mut pool = LeasePool::new(ledger, 32.0, true).unwrap();
    let sys = MockSystemInspector::new();
    let cid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    sys.add_process(
        101,
        1,
        "Runner.Worker",
        "1000",
        &format!("0::/docker/{cid}"),
    );
    sys.add_process(201, 1, "builder", "1000", "");
    sys.add_container(DockerContainerInspect {
        id: cid.to_string(),
        name: "heavy-runner".to_string(),
        running: true,
        started_at: "2026-10-22".to_string(),
        pid: 100,
        memory_limit: 32 * 1024 * 1024 * 1024,
        memory_swap_limit: 32 * 1024 * 1024 * 1024,
        nano_cpus: 16 * 1_000_000_000,
    });

    // Heavy runner requests 32 GiB
    let runner_owner = "runner:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:2026-10-22:101:1000";
    let ok = pool.acquire(runner_owner, 32.0, &sys).unwrap();
    assert!(ok);

    // Builder requests 28 GiB -> must be rejected due to heavy runner isolation!
    let builder_owner = "host:201:1000";
    let ok_builder = pool.acquire(builder_owner, 28.0, &sys).unwrap();
    assert!(!ok_builder);

    // Release heavy runner
    pool.release(runner_owner, &sys).unwrap();

    // Now builder succeeds
    let ok_builder2 = pool.acquire(builder_owner, 28.0, &sys).unwrap();
    assert!(ok_builder2);

    // And heavy runner is now blocked by builder
    let ok_runner2 = pool.acquire(runner_owner, 32.0, &sys).unwrap();
    assert!(!ok_runner2);
}

#[test]
fn test_legacy_owner_transition() {
    let temp = TempDir::new().unwrap();
    let ledger = temp.path().join("leases.json");
    let mut pool = LeasePool::new(ledger.clone(), 28.0, false).unwrap();
    let sys = MockSystemInspector::new();

    let cid = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    sys.add_process(
        301,
        1,
        "Runner.Worker",
        "1000",
        &format!("0::/docker/{cid}"),
    );
    sys.add_container(DockerContainerInspect {
        id: cid.to_string(),
        name: "runner-1".to_string(),
        running: true,
        started_at: "2026-10-22T00:00:00Z".to_string(),
        pid: 300,
        memory_limit: 14 * 1024 * 1024 * 1024,
        memory_swap_limit: 14 * 1024 * 1024 * 1024,
        nano_cpus: 7 * 1_000_000_000,
    });

    // Write a legacy container lease directly
    let legacy_key = format!("container:{cid}:2026-10-22T00:00:00Z");
    let mut map = HashMap::new();
    map.insert(legacy_key.clone(), 14.0);
    fs::write(&ledger, serde_json::to_string(&map).unwrap()).unwrap();

    // Upgraded worker acquires lease
    let worker_key = format!("runner:{cid}:2026-10-22T00:00:00Z:301:1000");
    let ok = pool.acquire_runner(&worker_key, 14.0, &sys).unwrap();
    assert!(ok);

    let active = pool.active_leases().unwrap();
    assert!(
        !active.contains_key(&legacy_key),
        "legacy key must be removed"
    );
    assert_eq!(active.get(&worker_key), Some(&14.0));
}

#[test]
fn test_peer_owner_and_ancestry_walk() {
    let sys = MockSystemInspector::new();
    let cid = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let cg = format!("0::/docker/{cid}");

    // Hierarchy: 1 -> 500 (Runner.Listener) -> 501 (Runner.Worker) -> 502 (job-started.sh)
    sys.add_process(500, 1, "Runner.Listener", "1000", &cg);
    sys.add_process(501, 500, "Runner.Worker", "1010", &cg);
    sys.add_process(502, 501, "job-started.sh", "1020", &cg);

    sys.add_container(DockerContainerInspect {
        id: cid.to_string(),
        name: "/runner-test".to_string(),
        running: true,
        started_at: "2026-10-22".to_string(),
        pid: 499,
        memory_limit: 14 * 1024 * 1024 * 1024,
        memory_swap_limit: 14 * 1024 * 1024 * 1024,
        nano_cpus: 7 * 1_000_000_000,
    });

    let config = AdmissionConfig {
        role: Some("runner".into()),
        work_gib: 28.0,
        runner_gib: 14.0,
        builder_gib: 28.0,
        runner_names: vec!["runner-test".into()],
        heavy_runner: None,
        heavy_runners: None,
    };

    // 1. UID 0 -> Builder
    let (owner, kind) = peer_owner(502, 0, &config, &sys).unwrap();
    assert_eq!(kind, AdmissionKind::Builder);
    assert!(owner.starts_with("host:502:"));

    // 2. UID 1001 -> Valid Runner
    let (owner, kind) = peer_owner(502, 1001, &config, &sys).unwrap();
    assert_eq!(kind, AdmissionKind::Runner);
    assert_eq!(owner, format!("runner:{cid}:2026-10-22:501:1010"));

    // 3. UID 1000 -> Unauthorized
    let err = peer_owner(502, 1000, &config, &sys).unwrap_err();
    assert!(matches!(err, AdmissionError::UnauthorizedPeer(1000)));

    // 4. Orphan hook: process 600 has no Runner.Worker ancestor
    sys.add_process(600, 1, "orphan.sh", "1000", &cg);
    let err = peer_owner(600, 1001, &config, &sys).unwrap_err();
    assert!(matches!(err, AdmissionError::OrphanRunnerHook(_)));
}

#[test]
fn test_headroom_check_builder() {
    let sys = MockSystemInspector::new();
    let temp = TempDir::new().unwrap();
    let ledger = temp.path().join("leases.json");

    // Total 32, available 28 -> builder fits (28 GiB budget)
    sys.set_meminfo(32.0, 28.0);
    let code = run_headroom(
        &[
            "--check-builder".to_string(),
            "--ledger-path".to_string(),
            ledger.to_string_lossy().to_string(),
        ],
        &sys,
    )
    .unwrap();
    assert_eq!(code, 0);

    // Total 32, available 10 -> cannot fit builder (budget 0)
    sys.set_meminfo(32.0, 10.0);
    let code = run_headroom(
        &[
            "--check-builder".to_string(),
            "--ledger-path".to_string(),
            ledger.to_string_lossy().to_string(),
        ],
        &sys,
    )
    .unwrap();
    assert_eq!(code, 75);
}

#[test]
fn test_owner_alive_rfc3339_timestamp_handling() {
    let sys = MockSystemInspector::new();
    let cid = "1111111111111111111111111111111111111111111111111111111111111111";
    let cg = format!("0::/docker/{cid}");
    let rfc3339_timestamp = "2026-10-22T10:02:40.123456789Z";

    sys.add_process(501, 1, "Runner.Worker", "1000", &cg);
    sys.add_container(DockerContainerInspect {
        id: cid.to_string(),
        name: "/runner-prod".to_string(),
        running: true,
        started_at: rfc3339_timestamp.to_string(),
        pid: 500,
        memory_limit: 14 * 1024 * 1024 * 1024,
        memory_swap_limit: 14 * 1024 * 1024 * 1024,
        nano_cpus: 7 * 1_000_000_000,
    });

    let owner = format!("runner:{cid}:{rfc3339_timestamp}:501:1000");
    assert!(
        host_agent::admission::peer::owner_alive(&owner, &sys),
        "Live runner with RFC3339 timestamp containing colons must return true"
    );

    // Also test container: lease with RFC3339 timestamp
    let container_owner = format!("container:{cid}:{rfc3339_timestamp}");
    assert!(
        host_agent::admission::peer::owner_alive(&container_owner, &sys),
        "Live container lease with RFC3339 timestamp must return true"
    );
}

#[test]
fn test_owner_alive_builder_lease_preserved_when_builders_running() {
    let sys = MockSystemInspector::new();
    // Dead / recycled PID
    sys.add_process(999, 1, "other-daemon", "2000", "");
    *sys.builders_running_flag.write().unwrap() = true;

    let owner = "host:999:1000";
    assert!(
        host_agent::admission::peer::owner_alive(owner, &sys),
        "Builder lease must remain active while sys.builders_running is true"
    );

    // When containers finish, lease is released
    *sys.builders_running_flag.write().unwrap() = false;
    assert!(
        !host_agent::admission::peer::owner_alive(owner, &sys),
        "Builder lease must be pruned when no builder containers are running"
    );
}
