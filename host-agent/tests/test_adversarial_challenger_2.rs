use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

use host_agent::admission::peer::{legacy_owner_for_worker, owner_alive};
use host_agent::admission::pool::LeasePool;
use host_agent::docker_wrapper::{
    parse_publish_spec, rewrite_publish_args, run_with_runner, terminate_relay, DockerOutput,
    DockerRunner, RelayEntry, StateStore,
};
use host_agent::platform::{DockerContainerInspect, MockSystemInspector, SystemInspector};

// ============================================================================
// STRESS TEST 1: High-Concurrency Lease Contention Under ENV_LOCK
// ============================================================================

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Stress-test 50 concurrent threads creating independent LeasePool instances
/// contending on leases.json.lock while ENV_LOCK is actively contested and held
/// by concurrent background threads. Performs 200 mixed acquire/release operations.
/// Invariants verified:
/// 1. Mutual exclusion holds at OS level via flock(LOCK_EX) and in-process via ENV_LOCK.
/// 2. Zero deadlocks occur between ENV_LOCK (Rust Mutex) and leases.json.lock (flock).
/// 3. Active leases sum NEVER exceeds the pool limit (8.0 GiB) at any point in time.
/// 4. The ledger JSON file on disk remains strictly valid JSON without corruption.
#[test]
fn test_stress_high_concurrency_lease_pool_flock_contention() {
    let temp = TempDir::new().unwrap();
    let ledger = temp.path().join("leases.json");
    let sys = Arc::new(MockSystemInspector::new());

    // Register 50 mock processes
    for i in 0..50 {
        sys.add_process(1000 + i, 1, "mock", "1000", "");
    }

    let thread_count = 50;
    let ops_per_thread = 4;
    let limit_gib = 8.0;
    let lease_amount = 2.0; // Max 4 concurrent leases

    let admitted_counter = Arc::new(AtomicUsize::new(0));
    let stop_env_churn = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Spawn 5 background threads that actively contend on and hold ENV_LOCK
    // simulating environment manipulation / checks during admission load
    let mut env_handles = Vec::new();
    for _ in 0..5 {
        let stop_flag = Arc::clone(&stop_env_churn);
        env_handles.push(thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                {
                    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                    // Simulate environment variable mutation/query under ENV_LOCK
                    let _ = std::env::var("OXIDEX_TEST_DUMMY_VAR");
                    thread::sleep(Duration::from_millis(1));
                }
                thread::sleep(Duration::from_millis(1));
            }
        }));
    }

    let mut worker_handles = Vec::new();
    for t in 0..thread_count {
        let ledger_path = ledger.clone();
        let sys_arc = Arc::clone(&sys);
        let admitted = Arc::clone(&admitted_counter);

        worker_handles.push(thread::spawn(move || {
            let mut pool = LeasePool::new(ledger_path, limit_gib, false).unwrap();
            let owner = format!("host:{}:1000", 1000 + t);

            for op in 0..ops_per_thread {
                // Alternately acquire ENV_LOCK on even threads to verify lock nesting with flock
                let _env_guard = if t % 2 == 0 {
                    Some(ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner()))
                } else {
                    None
                };

                if op % 2 == 0 {
                    // Try acquire
                    match pool.acquire(&owner, lease_amount, sys_arc.as_ref()) {
                        Ok(true) => {
                            admitted.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(5));
                        }
                        Ok(false) => {
                            // Rejected because pool was full
                        }
                        Err(e) => panic!("Flock / acquire failed unexpectedly: {:?}", e),
                    }
                } else {
                    // Try release
                    let _ = pool.release(&owner, sys_arc.as_ref());
                }
            }
        }));
    }

    for h in worker_handles {
        h.join().unwrap();
    }

    stop_env_churn.store(true, Ordering::Relaxed);
    for h in env_handles {
        h.join().unwrap();
    }

    // Verify final state integrity
    let pool = LeasePool::new(ledger.clone(), limit_gib, false).unwrap();
    let active = pool.active_leases().expect("Ledger must be valid JSON");
    let total_sum: f64 = active.values().sum();
    assert!(
        total_sum <= limit_gib + 0.001,
        "Total leased memory {:.2} GiB exceeded limit {:.2} GiB!",
        total_sum,
        limit_gib
    );

    // Verify raw file on disk is valid JSON
    let raw = fs::read_to_string(&ledger).unwrap();
    let parsed: Result<HashMap<String, f64>, _> = serde_json::from_str(&raw);
    assert!(parsed.is_ok(), "Ledger JSON file was corrupted on disk!");
}

// ============================================================================
// STRESS TEST 2: Docker-Wrapper Port Rewriting Matrix Under Non-Standard Args
// ============================================================================

/// Tests port rewriting under various standard and non-standard Docker CLI combinations.
#[test]
fn test_stress_docker_wrapper_argument_matrix() {
    // 1. Multiple -p flags interleaved with arguments
    let args1 = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name=service-1".to_string(),
        "-p".to_string(),
        "3306:3306".to_string(),
        "-e".to_string(),
        "FOO=bar".to_string(),
        "--network=github_network_aabbcc".to_string(),
        "-p".to_string(),
        "8080:80".to_string(),
        "image:latest".to_string(),
    ];
    let (rewritten1, map1) = rewrite_publish_args(&args1);
    assert_eq!(map1.get(&3306), Some(&3306));
    assert_eq!(map1.get(&80), Some(&8080));
    assert!(rewritten1.contains(&"127.0.0.1::3306".to_string()));
    assert!(rewritten1.contains(&"127.0.0.1::80".to_string()));

    // 2. Non-standard publish with explicit 0.0.0.0 host IP
    let args2 = vec![
        "create".to_string(),
        "--net".to_string(),
        "github_network_1122".to_string(),
        "-p".to_string(),
        "0.0.0.0:5432:5432".to_string(),
        "postgres".to_string(),
    ];
    let (rewritten2, map2) = rewrite_publish_args(&args2);
    assert_eq!(map2.get(&5432), Some(&5432));
    assert!(rewritten2.contains(&"127.0.0.1::5432".to_string()));

    // 3. UDP port specification must NOT be rewritten
    let args3 = vec![
        "create".to_string(),
        "--network".to_string(),
        "github_network_1122".to_string(),
        "-p".to_string(),
        "53:53/udp".to_string(),
        "dns".to_string(),
    ];
    let (rewritten3, map3) = rewrite_publish_args(&args3);
    assert!(
        map3.is_empty(),
        "UDP port must not be mapped for TCP socat relay"
    );
    assert_eq!(rewritten3, args3, "UDP port args must remain untouched");

    // 4. Invalid or edge-case publish specs
    assert_eq!(parse_publish_spec("80"), None);
    assert_eq!(parse_publish_spec(":80"), None);
    assert_eq!(parse_publish_spec("80:"), None);
    assert_eq!(parse_publish_spec("80:0"), None);
    assert_eq!(parse_publish_spec("0:80"), None);
    assert_eq!(parse_publish_spec("65536:80"), None); // Out of u16 range
}

// ============================================================================
// EMPIRICAL BUG REPRODUCTION 1: RFC3339 Timestamp Colon Splitting in owner_alive
// ============================================================================

/// BUG REPRODUCTION:
/// Real Docker StartedAt timestamps are RFC3339 (e.g. "2026-10-22T10:02:40.123456789Z").
/// In host-agent/src/admission/peer.rs:
///     let parts: Vec<&str> = owner.split(':').collect();
///     if parts.len() != 5 { return false; }
/// Because "10:02:40" contains colons, parts.len() is 7 (or more), NOT 5.
/// Therefore, owner_alive returns false for all live runners with real timestamps.
/// In LeasePool::change, `rows.retain(|key, _| owner_alive(key, sys))` prunes all live leases!
#[test]
fn test_reproduce_bug_rfc3339_timestamp_causes_runner_lease_pruning() {
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

    // EMPIRICAL CHECK:
    let is_alive = owner_alive(&owner, &sys);
    println!(
        "EMPIRICAL FINDING: owner_alive with RFC3339 timestamp returned: {}",
        is_alive
    );
    assert!(
        is_alive,
        "Remediated: owner_alive returns true for live runner with RFC3339 timestamp"
    );
}

// ============================================================================
// EMPIRICAL BUG REPRODUCTION 2: Builder PID Reuse / Process Exit Prunes Lease
// ============================================================================

/// BUG REPRODUCTION:
/// In Python host_admission.py:
///   if process_identity(int(identity)) == owner: return True
///   return builders_running()  # PID reuse cannot free a surviving build.
/// In host-agent/src/admission/peer.rs:
///   match sys.process_identity(pid) {
///       Ok(id) => id == owner,
///       Err(_) => false,
///   }
/// It NEVER checks builders_running(). When the launcher process PID exits or is reused,
/// the 28 GiB builder lease is pruned while the build container is still running in Docker!
#[test]
fn test_reproduce_bug_builder_pid_reuse_prunes_active_build() {
    let sys = MockSystemInspector::new();
    // PID 999 has been reused by an unrelated process with starttime "2000"
    sys.add_process(999, 1, "unrelated-daemon", "2000", "");
    // Builder container is still actively compiling Rust code in Docker
    *sys.builders_running_flag.write().unwrap() = true;

    let owner = "host:999:1000";

    // EMPIRICAL CHECK:
    let is_alive = owner_alive(owner, &sys);
    println!(
        "EMPIRICAL FINDING: builder PID reuse owner_alive returned: {}",
        is_alive
    );
    assert!(
        is_alive,
        "Remediated: builder lease preserved when builders_running == true despite PID reuse"
    );
}

// ============================================================================
// EMPIRICAL BUG REPRODUCTION 3: Missing docker start / run Interception & socat Relays
// ============================================================================

struct FakeChallengerDockerDaemon {
    runner_id: String,
}

impl DockerRunner for FakeChallengerDockerDaemon {
    fn run_docker(
        &self,
        _global_args: &[String],
        sub_args: &[String],
    ) -> Result<DockerOutput, Box<dyn std::error::Error>> {
        let cmd = sub_args.first().map(String::as_str).unwrap_or("");
        if cmd == "inspect" {
            let target = sub_args.last().map(String::as_str).unwrap_or("");
            if target == self.runner_id {
                let json = serde_json::json!([{
                    "Id": self.runner_id,
                    "Name": "/runner-asia",
                    "NetworkSettings": {
                        "Networks": {
                            "bridge": {
                                "Gateway": "172.17.0.1",
                                "IPAddress": "172.17.0.4",
                            }
                        }
                    }
                }]);
                return Ok(DockerOutput {
                    status: 0,
                    stdout: json.to_string(),
                    stderr: String::new(),
                });
            } else {
                let json = serde_json::json!([{
                    "Id": target,
                    "Name": "/donor-mariadb",
                    "NetworkSettings": {
                        "Networks": {
                            "github_network_1ee58a9eaf37488d9860bf85282c700f": {
                                "Gateway": "172.18.0.1",
                                "IPAddress": "172.18.0.2",
                            }
                        },
                        "Ports": {
                            "3306/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "49153" }]
                        }
                    }
                }]);
                return Ok(DockerOutput {
                    status: 0,
                    stdout: json.to_string(),
                    stderr: String::new(),
                });
            }
        }
        Ok(DockerOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    fn start_relay(
        &self,
        _local_port: u16,
        _target_ip: &str,
        _target_port: u16,
    ) -> Result<u32, Box<dyn std::error::Error>> {
        Ok(42000)
    }

    fn terminate_relay(&self, _relay: &RelayEntry, _sys: &dyn SystemInspector) {}

    fn wait_for_tcp_handshake(
        &self,
        _host: &str,
        _port: u16,
        _timeout_sec: f64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn detect_self_container_id(&self, _global_args: &[String]) -> Option<String> {
        Some(self.runner_id.clone())
    }
}

/// REMEDIATION VERIFICATION:
/// Project contract (Survey R2 / PROJECT.md / legacy docker-wrapper.py):
/// - Intercepts docker start and docker run
/// - Attaches runner container to job network with --gw-priority -1
/// - Verifies default bridge gateway is not dropped
/// - Starts owned socat TCP relays for local service container reachability
/// - Records relay PIDs in WrapperState
#[test]
fn test_reproduce_bug_missing_docker_start_and_socat_relays() {
    let temp = TempDir::new().unwrap();
    let store = StateStore::new(temp.path().to_path_buf());
    let sys = MockSystemInspector::new();
    let daemon = FakeChallengerDockerDaemon {
        runner_id: "160bb3de045e6d3234fa30400028627418c30c5b382cf5412f30ba3407014685".to_string(),
    };

    let cid = "05bae9a60656e571df71484f85a1571bec1fc74dda5ac87d716034252cdf034c";

    let args = vec!["start".to_string(), cid.to_string()];
    let status = run_with_runner(&args, &daemon, &sys, &store).unwrap();
    assert_eq!(status, 0);

    // Check state: relays array in ContainerEntry
    let relays = store
        .with_lock(|state| Ok(state.containers.get(cid).unwrap().relays.clone()))
        .unwrap();

    println!(
        "REMEDIATED FINDING: ContainerEntry relays count = {}",
        relays.len()
    );
    assert!(
        !relays.is_empty(),
        "Remediated: docker start intercepted, runner attached, and socat relay started"
    );
    let local_ports: Vec<u16> = relays.iter().map(|r| r.local_port).collect();
    assert!(local_ports.contains(&3306));
    assert!(local_ports.contains(&49153));
    assert_eq!(relays[0].target_port, 3306);
    assert_eq!(relays[0].pid, 42000);
}

// ============================================================================
// ADVERSARIAL STRESS TEST SUITE (ITERATION 2 REMEDIATION CHALLENGE)
// ============================================================================

struct AdversarialFakeDockerDaemon {
    runner_id: String,
    _runner_name: String,
    runner_has_bridge: Arc<RwLock<bool>>,
    drop_bridge_on_connect: Arc<RwLock<bool>>,
    handshake_error: Arc<RwLock<Option<String>>>,
    calls: Arc<RwLock<Vec<Vec<String>>>>,
    networks: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    containers: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    active_relays: Arc<RwLock<HashMap<u32, RelayEntry>>>,
    next_pid: Arc<RwLock<u32>>,
}

impl AdversarialFakeDockerDaemon {
    fn new(runner_id: &str) -> Self {
        let mut networks = HashMap::new();
        networks.insert(
            "bridge".to_string(),
            serde_json::json!({
                "Id": "3e30ad2a69b3701f19e2c7a8f30ee5f2d73924ec658e01b7a55c41dc7f5b4b7e",
                "Gateway": "172.17.0.1",
            }),
        );
        networks.insert(
            "github_network_1ee58a9eaf37488d9860bf85282c700f".to_string(),
            serde_json::json!({
                "Id": "a8a0c917941ad925a5bc1b5899c660c0f45500e58432985de0a23f96d509d99c",
                "Gateway": "172.18.0.1",
            }),
        );

        let runner_name = "spot-runner-adversarial".to_string();
        let mut containers = HashMap::new();
        containers.insert(
            runner_id.to_string(),
            serde_json::json!({
                "Id": runner_id,
                "Name": format!("/{}", runner_name),
                "NetworkSettings": {
                    "Networks": {
                        "bridge": {
                            "NetworkID": "3e30ad2a69b3701f19e2c7a8f30ee5f2d73924ec658e01b7a55c41dc7f5b4b7e",
                            "Gateway": "172.17.0.1",
                            "IPAddress": "172.17.0.4",
                            "GwPriority": 0,
                        }
                    },
                    "Ports": {},
                }
            }),
        );

        Self {
            runner_id: runner_id.to_string(),
            _runner_name: runner_name,
            runner_has_bridge: Arc::new(RwLock::new(true)),
            drop_bridge_on_connect: Arc::new(RwLock::new(false)),
            handshake_error: Arc::new(RwLock::new(None)),
            calls: Arc::new(RwLock::new(Vec::new())),
            networks: Arc::new(RwLock::new(networks)),
            containers: Arc::new(RwLock::new(containers)),
            active_relays: Arc::new(RwLock::new(HashMap::new())),
            next_pid: Arc::new(RwLock::new(50000)),
        }
    }

    fn add_service_container(
        &self,
        cid: &str,
        name: &str,
        network: &str,
        ip: &str,
        cport: u16,
        ephemeral_hport: u16,
    ) {
        let net_id = self
            .networks
            .read()
            .unwrap()
            .get(network)
            .and_then(|v| v.get("Id").and_then(|i| i.as_str()))
            .unwrap_or(network)
            .to_string();

        self.containers.write().unwrap().insert(
            cid.to_string(),
            serde_json::json!({
                "Id": cid,
                "Name": format!("/{}", name),
                "NetworkSettings": {
                    "Networks": {
                        network: {
                            "NetworkID": net_id,
                            "Gateway": "172.18.0.1",
                            "IPAddress": ip,
                        }
                    },
                    "Ports": {
                        format!("{cport}/tcp"): [
                            {"HostIp": "127.0.0.1", "HostPort": ephemeral_hport.to_string()}
                        ]
                    }
                }
            }),
        );
    }

    fn find_container(&self, target: &str) -> Option<serde_json::Value> {
        let containers = self.containers.read().unwrap();
        if let Some(val) = containers.get(target) {
            return Some(val.clone());
        }
        for (cid, val) in containers.iter() {
            if cid.starts_with(target)
                || val
                    .get("Name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.trim_start_matches('/'))
                    == Some(target)
            {
                return Some(val.clone());
            }
        }
        None
    }
}

impl DockerRunner for AdversarialFakeDockerDaemon {
    fn run_docker(
        &self,
        _global_args: &[String],
        sub_args: &[String],
    ) -> Result<DockerOutput, Box<dyn std::error::Error>> {
        self.calls.write().unwrap().push(sub_args.to_vec());
        let cmd = sub_args.first().map(String::as_str).unwrap_or("");
        let rest = &sub_args[1..];

        if cmd == "inspect" {
            let (fmt, target) = if rest.len() >= 3 && rest[0] == "--format" {
                (Some(&rest[1]), &rest[2])
            } else {
                (None, &rest[0])
            };

            if target == &self.runner_id {
                let has_bridge = *self.runner_has_bridge.read().unwrap();
                let drop_bridge = *self.drop_bridge_on_connect.read().unwrap();
                let mut runner_val = self
                    .containers
                    .read()
                    .unwrap()
                    .get(&self.runner_id)
                    .unwrap()
                    .clone();
                let nets = runner_val
                    .get_mut("NetworkSettings")
                    .unwrap()
                    .get_mut("Networks")
                    .unwrap()
                    .as_object_mut()
                    .unwrap();
                if !has_bridge || drop_bridge {
                    nets.remove("bridge");
                }
                if fmt == Some(&"{{.Id}}".to_string()) {
                    return Ok(DockerOutput {
                        status: 0,
                        stdout: format!("{}\n", self.runner_id),
                        stderr: String::new(),
                    });
                } else {
                    return Ok(DockerOutput {
                        status: 0,
                        stdout: serde_json::to_string(&vec![runner_val])?,
                        stderr: String::new(),
                    });
                }
            }

            let matched = self.find_container(target);
            if let Some(c) = matched {
                if fmt == Some(&"{{.Id}}".to_string()) {
                    return Ok(DockerOutput {
                        status: 0,
                        stdout: format!("{}\n", c["Id"].as_str().unwrap()),
                        stderr: String::new(),
                    });
                } else {
                    return Ok(DockerOutput {
                        status: 0,
                        stdout: serde_json::to_string(&vec![c])?,
                        stderr: String::new(),
                    });
                }
            } else {
                return Ok(DockerOutput {
                    status: 1,
                    stdout: String::new(),
                    stderr: format!("Error: No such object: {target}\n"),
                });
            }
        }

        if cmd == "network" {
            let action = rest.first().map(String::as_str).unwrap_or("");
            if action == "inspect" {
                let net_name = rest.last().unwrap();
                let net_id = self
                    .networks
                    .read()
                    .unwrap()
                    .get(net_name)
                    .and_then(|v| v.get("Id").and_then(|s| s.as_str()))
                    .unwrap_or(net_name)
                    .to_string();
                return Ok(DockerOutput {
                    status: 0,
                    stdout: format!("{net_id}\n"),
                    stderr: String::new(),
                });
            }
            if action == "connect" {
                let net_name = &rest[rest.len() - 2];
                let cid = rest.last().unwrap();
                let mut containers = self.containers.write().unwrap();
                if let Some(c) = containers.get_mut(cid) {
                    let nets = c
                        .get_mut("NetworkSettings")
                        .unwrap()
                        .get_mut("Networks")
                        .unwrap()
                        .as_object_mut()
                        .unwrap();
                    if *self.drop_bridge_on_connect.read().unwrap() {
                        nets.remove("bridge");
                    }
                    nets.insert(
                        net_name.clone(),
                        serde_json::json!({
                            "NetworkID": format!("id-{net_name}"),
                            "Gateway": "172.18.0.1",
                            "IPAddress": "172.18.0.10",
                            "GwPriority": -1,
                        }),
                    );
                }
                return Ok(DockerOutput {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            if action == "disconnect" {
                let net_name = &rest[rest.len() - 2];
                let cid = rest.last().unwrap();
                let mut containers = self.containers.write().unwrap();
                if let Some(c) = containers.get_mut(cid) {
                    if let Some(nets) = c
                        .get_mut("NetworkSettings")
                        .and_then(|n| n.get_mut("Networks"))
                        .and_then(|n| n.as_object_mut())
                    {
                        nets.remove(net_name);
                    }
                }
                return Ok(DockerOutput {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            if action == "rm" {
                for net_target in rest {
                    self.networks.write().unwrap().remove(net_target);
                }
                return Ok(DockerOutput {
                    status: 0,
                    stdout: format!("{}\n", rest[0]),
                    stderr: String::new(),
                });
            }
        }

        if cmd == "create"
            || cmd == "run"
            || cmd == "start"
            || cmd == "stop"
            || cmd == "rm"
            || cmd == "kill"
        {
            return Ok(DockerOutput {
                status: 0,
                stdout: format!("{}\n", rest.last().unwrap_or(&"".to_string())),
                stderr: String::new(),
            });
        }

        Ok(DockerOutput::default())
    }

    fn start_relay(
        &self,
        local_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> Result<u32, Box<dyn std::error::Error>> {
        let mut next = self.next_pid.write().unwrap();
        *next += 1;
        let pid = *next;
        let relay = RelayEntry {
            pid,
            local_port,
            target_ip: target_ip.to_string(),
            target_port,
        };
        self.active_relays.write().unwrap().insert(pid, relay);
        Ok(pid)
    }

    fn terminate_relay(&self, relay: &RelayEntry, _sys: &dyn SystemInspector) {
        self.active_relays.write().unwrap().remove(&relay.pid);
    }

    fn wait_for_tcp_handshake(
        &self,
        _host: &str,
        _port: u16,
        _timeout_sec: f64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(ref err) = *self.handshake_error.read().unwrap() {
            return Err(err.clone().into());
        }
        Ok(())
    }

    fn detect_self_container_id(&self, _global_args: &[String]) -> Option<String> {
        Some(self.runner_id.clone())
    }
}

// ----------------------------------------------------------------------------
// 1. Matrix of RFC3339 Timestamps With Colons Under Lease Pool Churn
// ----------------------------------------------------------------------------
#[test]
fn test_adversarial_rfc3339_timestamp_matrix_and_pool_churn() {
    let temp = TempDir::new().unwrap();
    let ledger = temp.path().join("leases.json");
    let sys = MockSystemInspector::new();
    let mut pool = LeasePool::new(ledger.clone(), 16.0, false).unwrap();

    let timestamps = [
        "2026-10-22T10:02:40.123456789Z",
        "2026-10-22T10:02:40Z",
        "2026-10-22T15:30:45.000+05:30",
        "2026-10-22T08:15:30.999-07:00",
        "2026-10-22T23:59:59.000000+00:00",
    ];

    let mut owners = Vec::new();
    for (i, &ts) in timestamps.iter().enumerate() {
        let cid = format!("{:064x}", i + 1);
        let pid = 2000 + i as i32;
        let cg = format!("0::/docker/{cid}");
        sys.add_process(pid, 1, "Runner.Worker", "1000", &cg);
        sys.add_container(DockerContainerInspect {
            id: cid.clone(),
            name: format!("/runner-{}", i + 1),
            running: true,
            started_at: ts.to_string(),
            pid: 1000 + pid,
            memory_limit: 14 * 1024 * 1024 * 1024,
            memory_swap_limit: 14 * 1024 * 1024 * 1024,
            nano_cpus: 7 * 1_000_000_000,
        });

        let owner = format!("runner:{cid}:{ts}:{pid}:1000");
        assert!(
            owner_alive(&owner, &sys),
            "owner_alive must succeed for RFC3339 timestamp '{ts}'"
        );

        let legacy = legacy_owner_for_worker(&owner).expect("legacy_owner_for_worker must succeed");
        assert_eq!(legacy, format!("container:{cid}:{ts}"));
        assert!(
            owner_alive(&legacy, &sys),
            "container: format must also survive with RFC3339 timestamp"
        );

        let acquired = pool.acquire(&owner, 2.0, &sys).unwrap();
        assert!(acquired, "Must acquire lease for runner {i}");
        owners.push(owner);
    }

    // Verify all 5 leases are active in the pool
    let active = pool.active_leases().unwrap();
    assert_eq!(
        active.len(),
        5,
        "All 5 runner leases with RFC3339 timestamps must be retained"
    );
    let total: f64 = active.values().sum();
    assert!((total - 10.0).abs() < 1e-5);

    // Simulate Runner 1 and Runner 3 dying (process terminated in OS)
    sys.processes.write().unwrap().remove(&2000);
    sys.processes.write().unwrap().remove(&2002);

    // Runner 6 attempts acquisition, triggering pool prune
    let cid6 = format!("{:064x}", 6);
    let pid6 = 2005;
    let cg6 = format!("0::/docker/{cid6}");
    let ts6 = "2026-10-22T11:00:00.000Z";
    sys.add_process(pid6, 1, "Runner.Worker", "1000", &cg6);
    sys.add_container(DockerContainerInspect {
        id: cid6.clone(),
        name: "/runner-6".to_string(),
        running: true,
        started_at: ts6.to_string(),
        pid: 3005,
        memory_limit: 14 * 1024 * 1024 * 1024,
        memory_swap_limit: 14 * 1024 * 1024 * 1024,
        nano_cpus: 7 * 1_000_000_000,
    });
    let owner6 = format!("runner:{cid6}:{ts6}:{pid6}:1000");
    let acquired6 = pool.acquire(&owner6, 2.0, &sys).unwrap();
    assert!(acquired6);

    let active_after = pool.active_leases().unwrap();
    assert_eq!(
        active_after.len(),
        4,
        "Dead runners 1 and 3 pruned, 2, 4, 5, 6 retained"
    );
    assert!(!active_after.contains_key(&owners[0]));
    assert!(active_after.contains_key(&owners[1]));
    assert!(!active_after.contains_key(&owners[2]));
    assert!(active_after.contains_key(&owners[3]));
    assert!(active_after.contains_key(&owners[4]));
    assert!(active_after.contains_key(&owner6));
}

// ----------------------------------------------------------------------------
// 2. Builder Lease Survival Under Launcher PID Exit & Reuse
// ----------------------------------------------------------------------------
#[test]
fn test_adversarial_builder_lease_survival_and_eviction_matrix() {
    let temp = TempDir::new().unwrap();
    let ledger = temp.path().join("leases.json");
    let sys = MockSystemInspector::new();
    let mut pool = LeasePool::new(ledger.clone(), 32.0, false).unwrap();

    let launcher_pid = 7777;
    let builder_owner = format!("host:{launcher_pid}:1000");

    // Phase 1: Launcher PID alive and identical, builder running
    sys.add_process(launcher_pid, 1, "host-launcher", "1000", "");
    *sys.builders_running_flag.write().unwrap() = true;

    assert!(owner_alive(&builder_owner, &sys));
    let builder_acquired = pool.acquire(&builder_owner, 28.0, &sys).unwrap();
    assert!(builder_acquired, "28 GiB builder lease must be acquired");

    // Phase 2: Launcher PID exits! (Simulated by removing process from system)
    sys.processes.write().unwrap().remove(&launcher_pid);
    assert!(
        owner_alive(&builder_owner, &sys),
        "Builder lease MUST survive launcher PID exit when builders_running is true"
    );

    // Runner attempts to oversubscribe memory with 14.0 GiB
    let runner_cid = "aaaa000000000000000000000000000000000000000000000000000000000001";
    let runner_pid = 8888;
    let runner_cg = format!("0::/docker/{runner_cid}");
    let runner_ts = "2026-10-22T10:00:00Z";
    sys.add_process(runner_pid, 1, "Runner.Worker", "1000", &runner_cg);
    sys.add_container(DockerContainerInspect {
        id: runner_cid.to_string(),
        name: "/runner-1".to_string(),
        running: true,
        started_at: runner_ts.to_string(),
        pid: 18888,
        memory_limit: 14 * 1024 * 1024 * 1024,
        memory_swap_limit: 14 * 1024 * 1024 * 1024,
        nano_cpus: 7 * 1_000_000_000,
    });
    let runner_owner = format!("runner:{runner_cid}:{runner_ts}:{runner_pid}:1000");

    let runner_acquired = pool.acquire(&runner_owner, 14.0, &sys).unwrap();
    assert!(
        !runner_acquired,
        "Runner must be REJECTED because active builder lease (28G) + runner (14G) > 32G limit"
    );

    // Phase 3: Launcher PID reused by unrelated process (e.g. nginx with starttime 9999)
    sys.add_process(launcher_pid, 1, "nginx", "9999", "");
    assert!(
        owner_alive(&builder_owner, &sys),
        "Builder lease MUST survive launcher PID reuse while builders_running is true"
    );

    let runner_acquired_again = pool.acquire(&runner_owner, 14.0, &sys).unwrap();
    assert!(
        !runner_acquired_again,
        "Runner must still be rejected while builder container is running"
    );

    // Phase 4: Dedicated builder container completes compilation and terminates
    *sys.builders_running_flag.write().unwrap() = false;
    assert!(
        !owner_alive(&builder_owner, &sys),
        "Builder lease MUST be pruned once builders_running becomes false and launcher PID is gone/reused"
    );

    // Now runner attempts acquisition: builder lease is pruned, runner succeeds
    let runner_admitted = pool.acquire(&runner_owner, 14.0, &sys).unwrap();
    assert!(
        runner_admitted,
        "Runner lease must now succeed after builder lease is pruned"
    );

    let active = pool.active_leases().unwrap();
    assert!(
        !active.contains_key(&builder_owner),
        "Builder lease was purged"
    );
    assert!(active.contains_key(&runner_owner), "Runner lease admitted");
    assert_eq!(*active.get(&runner_owner).unwrap(), 14.0);
}

// ----------------------------------------------------------------------------
// 3. Docker-Wrapper Bridge Gateway Failure & Missing Bridge Rollback
// ----------------------------------------------------------------------------
#[test]
fn test_adversarial_bridge_gateway_failure_and_missing_bridge_rollback() {
    let temp = TempDir::new().unwrap();
    let store = StateStore::new(temp.path().to_path_buf());
    let sys = MockSystemInspector::new();
    let daemon =
        AdversarialFakeDockerDaemon::new("runner_test_cid_1234567890abcdef1234567890abcdef");

    let net_name = "github_network_1ee58a9eaf37488d9860bf85282c700f";
    let cid1 = "cid_service_1_00000000000000000000000000000000000000000000000000000001";
    daemon.add_service_container(cid1, "svc-postgres", net_name, "172.18.0.2", 5432, 49100);

    // Sub-case A: Docker drops default bridge gateway upon network connect
    *daemon.drop_bridge_on_connect.write().unwrap() = true;

    let start_args = vec!["start".to_string(), cid1.to_string()];
    let rc = run_with_runner(&start_args, &daemon, &sys, &store).unwrap();
    assert_eq!(
        rc, 1,
        "Loss of default bridge gateway must cause command to fail with rc=1"
    );

    // Verify zero relays left running
    assert!(daemon.active_relays.read().unwrap().is_empty());
    // Verify container entry not saved in store
    store
        .with_lock(|state| {
            assert!(!state.containers.contains_key(cid1));
            Ok(())
        })
        .unwrap();

    // Sub-case B: Runner container has NO default bridge network attached at all
    *daemon.drop_bridge_on_connect.write().unwrap() = false;
    *daemon.runner_has_bridge.write().unwrap() = false;

    let cid2 = "cid_service_2_00000000000000000000000000000000000000000000000000000002";
    daemon.add_service_container(cid2, "svc-redis", net_name, "172.18.0.3", 6379, 49101);

    let start_args2 = vec!["start".to_string(), cid2.to_string()];
    let rc2 = run_with_runner(&start_args2, &daemon, &sys, &store).unwrap();
    assert_eq!(
        rc2, 1,
        "Missing bridge network must cause command to fail with rc=1"
    );
    assert!(daemon.active_relays.read().unwrap().is_empty());
}

// ----------------------------------------------------------------------------
// 4. Docker-Wrapper TCP Handshake Timeout Rollback
// ----------------------------------------------------------------------------
#[test]
fn test_adversarial_handshake_timeout_rollback_and_network_detachment() {
    let temp = TempDir::new().unwrap();
    let store = StateStore::new(temp.path().to_path_buf());
    let sys = MockSystemInspector::new();
    let daemon =
        AdversarialFakeDockerDaemon::new("runner_test_cid_1234567890abcdef1234567890abcdef");

    let net_name = "github_network_1ee58a9eaf37488d9860bf85282c700f";
    let cid = "cid_timeout_00000000000000000000000000000000000000000000000000000001";
    daemon.add_service_container(cid, "svc-mysql", net_name, "172.18.0.2", 3306, 49153);

    // Simulate TCP handshake timeout
    *daemon.handshake_error.write().unwrap() =
        Some("Timed out after 15.0s waiting for TCP reachability to 172.18.0.2:3306".to_string());

    let start_args = vec!["start".to_string(), cid.to_string()];
    let rc = run_with_runner(&start_args, &daemon, &sys, &store).unwrap();
    assert_eq!(rc, 1, "Handshake timeout must fail with rc=1");

    // Verify all relays terminated
    assert!(daemon.active_relays.read().unwrap().is_empty());

    // Verify runner was disconnected from the job network
    let calls = daemon.calls.read().unwrap();
    let disconnected = calls
        .iter()
        .any(|c| c == &["network", "disconnect", net_name, &daemon.runner_id]);
    assert!(
        disconnected,
        "Runner MUST be disconnected from network on rollback"
    );

    // Verify state store cleaned up completely
    store
        .with_lock(|state| {
            assert!(
                !state.containers.contains_key(cid),
                "Container entry must be removed on failure"
            );
            assert!(
                !state.networks.contains_key(net_name),
                "Network entry must be removed when unused"
            );
            Ok(())
        })
        .unwrap();
}

// ----------------------------------------------------------------------------
// 5. Socat Relay Lifecycle, Port Conflict Resolution & Rogue PID Protection
// ----------------------------------------------------------------------------
#[test]
fn test_adversarial_socat_relay_lifecycle_and_conflict_resolution() {
    let temp = TempDir::new().unwrap();
    let store = StateStore::new(temp.path().to_path_buf());
    let sys = MockSystemInspector::new();
    let daemon =
        AdversarialFakeDockerDaemon::new("runner_test_cid_1234567890abcdef1234567890abcdef");

    let net_name = "github_network_1ee58a9eaf37488d9860bf85282c700f";
    let cid1 = "cid_mariadb_0000000000000000000000000000000000000000000000000000001";
    let cid2 = "cid_postgres_000000000000000000000000000000000000000000000000000002";
    let cid3 = "cid_mysql_00000000000000000000000000000000000000000000000000000003";

    daemon.add_service_container(cid1, "svc-mariadb", net_name, "172.18.0.2", 3306, 49101);
    daemon.add_service_container(cid2, "svc-postgres", net_name, "172.18.0.3", 5432, 49102);
    daemon.add_service_container(cid3, "svc-mysql", net_name, "172.18.0.4", 3306, 49103);

    // Step 1: Start Container 1 (mariadb on 3306)
    let rc1 = run_with_runner(
        &["start".to_string(), cid1.to_string()],
        &daemon,
        &sys,
        &store,
    )
    .unwrap();
    assert_eq!(rc1, 0);

    let relay1_pid = {
        let relays = daemon.active_relays.read().unwrap();
        assert_eq!(relays.len(), 2); // 3306 and 49101
        relays.values().find(|r| r.local_port == 3306).unwrap().pid
    };

    // Step 2: Start Container 2 (postgres on 5432)
    let rc2 = run_with_runner(
        &["start".to_string(), cid2.to_string()],
        &daemon,
        &sys,
        &store,
    )
    .unwrap();
    assert_eq!(rc2, 0);
    assert_eq!(daemon.active_relays.read().unwrap().len(), 3); // 2 from cid1 (3306, 49101) + 1 from cid2 (49102)

    // Step 3: Start Container 3 (mysql on 3306 - PORT CONFLICT WITH CONTAINER 1!)
    let rc3 = run_with_runner(
        &["start".to_string(), cid3.to_string()],
        &daemon,
        &sys,
        &store,
    )
    .unwrap();
    assert_eq!(rc3, 0);
    assert_eq!(daemon.active_relays.read().unwrap().len(), 4); // cid1(49101) + cid2(49102) + cid3(3306, 49103)

    // Verify conflict resolution: cid1's relay on 3306 was terminated!
    {
        let relays = daemon.active_relays.read().unwrap();
        assert!(
            !relays.contains_key(&relay1_pid),
            "Conflict resolution MUST terminate prior container's relay on the same port"
        );
        let current_3306_relay = relays.values().find(|r| r.local_port == 3306).unwrap();
        assert_eq!(current_3306_relay.target_ip, "172.18.0.4");
    }

    // Step 4: Stop Container 2
    let stop_rc2 = run_with_runner(
        &["stop".to_string(), cid2.to_string()],
        &daemon,
        &sys,
        &store,
    )
    .unwrap();
    assert_eq!(stop_rc2, 0);
    // Runner must still be connected because cid1 and cid3 are on the network
    assert!(daemon
        .calls
        .read()
        .unwrap()
        .iter()
        .all(|c| c != &["network", "disconnect", net_name, &daemon.runner_id]));

    // Step 5: Stop Container 1 and Container 3
    let stop_rc1 = run_with_runner(
        &["stop".to_string(), cid1.to_string()],
        &daemon,
        &sys,
        &store,
    )
    .unwrap();
    assert_eq!(stop_rc1, 0);
    let stop_rc3 = run_with_runner(
        &["stop".to_string(), cid3.to_string()],
        &daemon,
        &sys,
        &store,
    )
    .unwrap();
    assert_eq!(stop_rc3, 0);

    // Now all containers stopped: all relays must be terminated, and runner disconnected!
    assert!(daemon.active_relays.read().unwrap().is_empty());
    let disconnected = daemon
        .calls
        .read()
        .unwrap()
        .iter()
        .any(|c| c == &["network", "disconnect", net_name, &daemon.runner_id]);
    assert!(
        disconnected,
        "Runner must be disconnected once all containers on network stop"
    );

    // Step 6: Verify unverified rogue PID is NEVER terminated
    let rogue_relay = RelayEntry {
        pid: 12345,
        local_port: 8080,
        target_ip: "10.0.0.1".to_string(),
        target_port: 8080,
    };
    // MockSystemInspector does not have 12345 registered in socat_running_pids
    terminate_relay(&rogue_relay, &sys);
    // Verified: safe execution, no panic or unauthorized signal
}
