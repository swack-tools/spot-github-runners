use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, RwLock};
use std::thread;
use tempfile::TempDir;

use host_agent::docker_wrapper::{
    extract_option_value, parse_publish_spec, rewrite_publish_args, run_with_runner,
    setup_service_container, split_global_and_sub_args, terminate_relay, wait_for_tcp_handshake,
    DockerOutput, DockerRunner, PublishSpec, RelayEntry, StateStore, WrapperState,
};
use host_agent::platform::{MockSystemInspector, SystemInspector};

#[test]
fn test_parse_publish_spec() {
    // 2-part TCP
    assert_eq!(
        parse_publish_spec("3306:3306"),
        Some(PublishSpec {
            host_ip: None,
            host_port: 3306,
            container_port: 3306,
        })
    );

    // 3-part TCP with IP
    assert_eq!(
        parse_publish_spec("127.0.0.1:6379:6379"),
        Some(PublishSpec {
            host_ip: Some("127.0.0.1".into()),
            host_port: 6379,
            container_port: 6379,
        })
    );

    // Explicit /tcp
    assert_eq!(
        parse_publish_spec("8080:80/tcp"),
        Some(PublishSpec {
            host_ip: None,
            host_port: 8080,
            container_port: 80,
        })
    );

    // UDP should return None (ignored)
    assert_eq!(parse_publish_spec("53:53/udp"), None);

    // Invalid port numbers
    assert_eq!(parse_publish_spec("0:80"), None);
    assert_eq!(parse_publish_spec("80:70000"), None);
    assert_eq!(parse_publish_spec("bad:port"), None);
}

#[test]
fn test_split_global_and_sub_args() {
    let args = vec![
        "-H".to_string(),
        "unix:///var/run/docker.sock".to_string(),
        "-l".to_string(),
        "debug".to_string(),
        "--config=/tmp/cfg".to_string(),
        "run".to_string(),
        "-d".to_string(),
        "--network".to_string(),
        "github_network_1234".to_string(),
        "redis".to_string(),
    ];

    let (global, subcmd, sub_args) = split_global_and_sub_args(&args);
    assert_eq!(global.len(), 5);
    assert_eq!(subcmd, Some("run".to_string()));
    assert_eq!(
        sub_args,
        vec!["-d", "--network", "github_network_1234", "redis"]
    );
}

#[test]
fn test_rewrite_publish_args_github_network() {
    // Matches github_network_*
    let args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--network".to_string(),
        "github_network_1234abcd".to_string(),
        "-p".to_string(),
        "3306:3306".to_string(),
        "--publish=127.0.0.1:6379:6379".to_string(),
        "my-service".to_string(),
    ];

    let (rewritten, map) = rewrite_publish_args(&args);
    assert_eq!(map.get(&3306), Some(&3306));
    assert_eq!(map.get(&6379), Some(&6379));

    assert!(rewritten.contains(&"127.0.0.1::3306".to_string()));
    assert!(rewritten
        .iter()
        .any(|a| a.starts_with("--publish=127.0.0.1::6379")));
    assert!(!rewritten.contains(&"3306:3306".to_string()));
}

#[test]
fn test_rewrite_publish_args_non_github_network_untouched() {
    // Standard bridge network -> must NOT rewrite
    let args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--network".to_string(),
        "bridge".to_string(),
        "-p".to_string(),
        "3306:3306".to_string(),
        "my-service".to_string(),
    ];

    let (rewritten, map) = rewrite_publish_args(&args);
    assert!(map.is_empty());
    assert_eq!(rewritten, args);
}

#[test]
fn test_state_store_flock_concurrency() {
    let temp = TempDir::new().unwrap();
    let store_arc = Arc::new(StateStore::new(temp.path().to_path_buf()));

    let mut handles = Vec::new();
    for i in 0..10 {
        let s = Arc::clone(&store_arc);
        handles.push(thread::spawn(move || {
            s.with_lock(|state| {
                state.containers.insert(
                    format!("cid_{i}"),
                    host_agent::docker_wrapper::ContainerEntry {
                        name: format!("name_{i}"),
                        networks: vec![],
                        target_ip: format!("172.18.0.{i}"),
                        requested_ports: Default::default(),
                        relays: vec![],
                    },
                );
                Ok(())
            })
            .unwrap();
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    store_arc
        .with_lock(|state| {
            assert_eq!(state.containers.len(), 10);
            Ok(())
        })
        .unwrap();
}

#[test]
fn test_terminate_relay_verification() {
    let sys = MockSystemInspector::new();
    // Register verified socat pid
    sys.socat_running_pids
        .write()
        .unwrap()
        .insert(9999, (3306, "172.18.0.2".to_string(), 3306));

    let relay = RelayEntry {
        pid: 9999,
        local_port: 3306,
        target_ip: "172.18.0.2".to_string(),
        target_port: 3306,
    };

    // If verified, attempts termination
    terminate_relay(&relay, &sys);

    // Unverified relay (PID not in socat table) is safely skipped without signal
    let unverified = RelayEntry {
        pid: 8888,
        local_port: 3306,
        target_ip: "172.18.0.2".to_string(),
        target_port: 3306,
    };
    terminate_relay(&unverified, &sys);
}

// ----------------------------------------------------------------------------
// FAKE DOCKER DAEMON FOR UNIT TESTS
// ----------------------------------------------------------------------------

pub struct FakeDockerDaemon {
    pub runner_id: String,
    pub runner_name: String,
    pub calls: Arc<RwLock<Vec<Vec<String>>>>,
    pub networks: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    pub containers: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    pub fail_connect: Arc<RwLock<bool>>,
    pub drop_bridge_on_connect: Arc<RwLock<bool>>,
    pub active_relays: Arc<RwLock<HashMap<u32, RelayEntry>>>,
    pub next_pid: Arc<RwLock<u32>>,
    pub handshake_fail: Arc<RwLock<Option<String>>>,
}

impl FakeDockerDaemon {
    pub fn new(runner_id: &str) -> Self {
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

        let runner_name = "newznab-newznab-spot-runners-asia-3".to_string();
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
            runner_name,
            calls: Arc::new(RwLock::new(Vec::new())),
            networks: Arc::new(RwLock::new(networks)),
            containers: Arc::new(RwLock::new(containers)),
            fail_connect: Arc::new(RwLock::new(false)),
            drop_bridge_on_connect: Arc::new(RwLock::new(false)),
            active_relays: Arc::new(RwLock::new(HashMap::new())),
            next_pid: Arc::new(RwLock::new(40000)),
            handshake_fail: Arc::new(RwLock::new(None)),
        }
    }

    pub fn add_service_container(
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

impl DockerRunner for FakeDockerDaemon {
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
                if *self.fail_connect.read().unwrap() {
                    return Ok(DockerOutput {
                        status: 1,
                        stdout: String::new(),
                        stderr: "permission denied connecting network\n".to_string(),
                    });
                }
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

        if cmd == "create" {
            let name =
                extract_option_value(rest, &["--name"]).unwrap_or_else(|| "service-1".to_string());
            let net = extract_option_value(rest, &["--network", "--net"])
                .unwrap_or_else(|| "bridge".to_string());
            let cid = "05bae9a60656e571df71484f85a1571bec1fc74dda5ac87d716034252cdf034c";
            self.add_service_container(cid, &name, &net, "172.18.0.2", 3306, 49153);
            return Ok(DockerOutput {
                status: 0,
                stdout: format!("{cid}\n"),
                stderr: String::new(),
            });
        }

        if cmd == "run" {
            let name = extract_option_value(rest, &["--name"])
                .unwrap_or_else(|| "service-run".to_string());
            let net = extract_option_value(rest, &["--network", "--net"])
                .unwrap_or_else(|| "bridge".to_string());
            let cid = "run999a60656e571df71484f85a1571bec1fc74dda5ac87d716034252cdf034c";
            self.add_service_container(cid, &name, &net, "172.18.0.5", 6379, 49200);
            return Ok(DockerOutput {
                status: 0,
                stdout: format!("{cid}\n"),
                stderr: String::new(),
            });
        }

        if cmd == "start" || cmd == "stop" || cmd == "rm" || cmd == "kill" {
            return Ok(DockerOutput {
                status: 0,
                stdout: format!("{}\n", rest.last().unwrap_or(&"".to_string())),
                stderr: String::new(),
            });
        }

        if cmd == "port" {
            let target = &rest[0];
            let matched = self.find_container(target);
            if let Some(c) = matched {
                let mut lines = Vec::new();
                if let Some(ports) = c
                    .get("NetworkSettings")
                    .and_then(|n| n.get("Ports"))
                    .and_then(|p| p.as_object())
                {
                    for (cport_proto, bindings) in ports {
                        if let Some(arr) = bindings.as_array() {
                            for b in arr {
                                let ip = b
                                    .get("HostIp")
                                    .and_then(|i| i.as_str())
                                    .unwrap_or("0.0.0.0");
                                let port =
                                    b.get("HostPort").and_then(|p| p.as_str()).unwrap_or("0");
                                lines.push(format!("{cport_proto} -> {ip}:{port}"));
                            }
                        }
                    }
                }
                return Ok(DockerOutput {
                    status: 0,
                    stdout: format!("{}\n", lines.join("\n")),
                    stderr: String::new(),
                });
            } else {
                return Ok(DockerOutput {
                    status: 1,
                    stdout: String::new(),
                    stderr: "No such container\n".to_string(),
                });
            }
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
        if let Some(ref err) = *self.handshake_fail.read().unwrap() {
            return Err(err.clone().into());
        }
        Ok(())
    }

    fn detect_self_container_id(&self, _global_args: &[String]) -> Option<String> {
        Some(self.runner_id.clone())
    }
}

// ----------------------------------------------------------------------------
// SERVICE NETWORKING TESTS
// ----------------------------------------------------------------------------

#[test]
fn test_create_start_port_stop_and_network_rm_lifecycle() {
    let temp = TempDir::new().unwrap();
    let store = StateStore::new(temp.path().to_path_buf());
    let daemon =
        FakeDockerDaemon::new("160bb3de045e6d3234fa30400028627418c30c5b382cf5412f30ba3407014685");
    let sys = MockSystemInspector::new();

    let net_name = "github_network_1ee58a9eaf37488d9860bf85282c700f";
    let cid = "05bae9a60656e571df71484f85a1571bec1fc74dda5ac87d716034252cdf034c";

    // 1. docker create
    let create_args = vec![
        "create".to_string(),
        "--name".to_string(),
        "donor-mariadb".to_string(),
        "--network".to_string(),
        net_name.to_string(),
        "-p".to_string(),
        "3306:3306".to_string(),
        "mariadb:11.4.8".to_string(),
    ];
    let rc = run_with_runner(&create_args, &daemon, &sys, &store).unwrap();
    assert_eq!(rc, 0);

    // Verify requested ports stored in state
    store
        .with_lock(|state| {
            let cinfo = state.containers.get(cid).unwrap();
            assert_eq!(cinfo.requested_ports.get("3306"), Some(&3306));
            assert_eq!(cinfo.name, "donor-mariadb");
            Ok(())
        })
        .unwrap();

    // 2. docker start
    let start_args = vec!["start".to_string(), cid.to_string()];
    let rc = run_with_runner(&start_args, &daemon, &sys, &store).unwrap();
    assert_eq!(rc, 0);

    // Verify network attach was called with --gw-priority -1
    {
        let calls = daemon.calls.read().unwrap();
        assert!(calls.iter().any(|c| c
            == &[
                "network",
                "connect",
                "--gw-priority",
                "-1",
                net_name,
                &daemon.runner_id
            ]));
    }

    // Verify relays created in state and daemon
    {
        let relays = daemon.active_relays.read().unwrap();
        let relay_ports: Vec<u16> = relays.values().map(|r| r.local_port).collect();
        assert!(relay_ports.contains(&3306));
        assert!(relay_ports.contains(&49153));
    }

    // 3. docker port
    let port_args = vec!["port".to_string(), cid.to_string()];
    let rc = run_with_runner(&port_args, &daemon, &sys, &store).unwrap();
    assert_eq!(rc, 0);

    // 4. docker stop
    let stop_args = vec!["stop".to_string(), cid.to_string()];
    let rc = run_with_runner(&stop_args, &daemon, &sys, &store).unwrap();
    assert_eq!(rc, 0);

    // Verify relays terminated and runner disconnected
    assert!(daemon.active_relays.read().unwrap().is_empty());
    let calls = daemon.calls.read().unwrap();
    assert!(calls
        .iter()
        .any(|c| c == &["network", "disconnect", net_name, &daemon.runner_id]));
}

#[test]
fn test_run_detached_lifecycle() {
    let temp = TempDir::new().unwrap();
    let store = StateStore::new(temp.path().to_path_buf());
    let daemon =
        FakeDockerDaemon::new("160bb3de045e6d3234fa30400028627418c30c5b382cf5412f30ba3407014685");
    let sys = MockSystemInspector::new();

    let net_name = "github_network_1ee58a9eaf37488d9860bf85282c700f";
    let run_args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        "service-redis".to_string(),
        "--network".to_string(),
        net_name.to_string(),
        "-p".to_string(),
        "6379:6379".to_string(),
        "redis:7".to_string(),
    ];

    let rc = run_with_runner(&run_args, &daemon, &sys, &store).unwrap();
    assert_eq!(rc, 0);

    // Verify state store has the container and active relays
    store
        .with_lock(|state| {
            assert_eq!(state.containers.len(), 1);
            let cinfo = state.containers.values().next().unwrap();
            assert_eq!(cinfo.name, "service-redis");
            assert!(!cinfo.relays.is_empty());
            Ok(())
        })
        .unwrap();
}

#[test]
fn test_handshake_failure_fails_closed_and_rolls_back_network_and_relays() {
    let temp = TempDir::new().unwrap();
    let store = StateStore::new(temp.path().to_path_buf());
    let daemon =
        FakeDockerDaemon::new("160bb3de045e6d3234fa30400028627418c30c5b382cf5412f30ba3407014685");
    let sys = MockSystemInspector::new();

    let net_name = "github_network_1ee58a9eaf37488d9860bf85282c700f";
    let cid = "05bae9a60656e571df71484f85a1571bec1fc74dda5ac87d716034252cdf034c";

    let create_args = vec![
        "create".to_string(),
        "--name".to_string(),
        "donor-mariadb".to_string(),
        "--network".to_string(),
        net_name.to_string(),
        "-p".to_string(),
        "3306:3306".to_string(),
        "mariadb:11.4.8".to_string(),
    ];
    assert_eq!(
        run_with_runner(&create_args, &daemon, &sys, &store).unwrap(),
        0
    );

    // Set handshake to fail
    *daemon.handshake_fail.write().unwrap() =
        Some("upstream 172.18.0.2:3306 unreachable".to_string());

    let start_args = vec!["start".to_string(), cid.to_string()];
    let rc = run_with_runner(&start_args, &daemon, &sys, &store).unwrap();
    assert_eq!(rc, 1, "Handshake failure must exit code 1");

    // All relays rolled back
    assert!(daemon.active_relays.read().unwrap().is_empty());
    // Runner disconnected from job network
    let calls = daemon.calls.read().unwrap();
    assert!(calls
        .iter()
        .any(|c| c == &["network", "disconnect", net_name, &daemon.runner_id]));
}

#[test]
fn test_bridge_gateway_loss_fails_closed() {
    let temp = TempDir::new().unwrap();
    let store = StateStore::new(temp.path().to_path_buf());
    let daemon =
        FakeDockerDaemon::new("160bb3de045e6d3234fa30400028627418c30c5b382cf5412f30ba3407014685");
    let sys = MockSystemInspector::new();

    let net_name = "github_network_1ee58a9eaf37488d9860bf85282c700f";
    let cid = "05bae9a60656e571df71484f85a1571bec1fc74dda5ac87d716034252cdf034c";

    let create_args = vec![
        "create".to_string(),
        "--name".to_string(),
        "donor-mariadb".to_string(),
        "--network".to_string(),
        net_name.to_string(),
        "-p".to_string(),
        "3306:3306".to_string(),
        "mariadb:11.4.8".to_string(),
    ];
    assert_eq!(
        run_with_runner(&create_args, &daemon, &sys, &store).unwrap(),
        0
    );

    // Simulate docker dropping default bridge gateway
    *daemon.drop_bridge_on_connect.write().unwrap() = true;

    let start_args = vec!["start".to_string(), cid.to_string()];
    let rc = run_with_runner(&start_args, &daemon, &sys, &store).unwrap();
    assert_eq!(rc, 1, "Bridge gateway loss must exit code 1");
    assert!(daemon.active_relays.read().unwrap().is_empty());
}

#[test]
fn test_real_tcp_handshake_listener_and_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        drop(stream);
    });

    let res = wait_for_tcp_handshake("127.0.0.1", port, 1.0);
    assert!(res.is_ok());
    handle.join().unwrap();

    // Check timeout on closed port
    let unused_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_port = unused_listener.local_addr().unwrap().port();
    drop(unused_listener);

    let res_fail = wait_for_tcp_handshake("127.0.0.1", closed_port, 0.15);
    assert!(res_fail.is_err());
}

#[test]
fn test_bridge_network_published_port_relay() {
    let temp = TempDir::new().unwrap();
    let _store = StateStore::new(temp.path().to_path_buf());
    let daemon =
        FakeDockerDaemon::new("160bb3de045e6d3234fa30400028627418c30c5b382cf5412f30ba3407014685");
    let sys = MockSystemInspector::new();

    let cid = "b99999990656e571df71484f85a1571bec1fc74dda5ac87d716034252cdf034c";
    daemon.add_service_container(cid, "nntp-proxy-smoke", "bridge", "172.17.0.9", 80, 49155);

    let mut state = WrapperState::default();
    setup_service_container(&[], &mut state, cid, None, &daemon, &sys).unwrap();

    let entry = state.containers.get(cid).expect("bridge container tracked");
    assert_eq!(entry.target_ip, "172.17.0.9");
    assert_eq!(entry.relays.len(), 1);
    assert_eq!(entry.relays[0].local_port, 49155);
    assert_eq!(entry.relays[0].target_port, 80);
}
