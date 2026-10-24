use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::platform::{DefaultSystemInspector, SystemInspector};

pub const GLOBAL_VALUE_FLAGS: &[&str] = &[
    "-H",
    "--host",
    "--config",
    "-c",
    "--context",
    "-l",
    "--log-level",
];

pub fn real_docker_binary() -> String {
    std::env::var("GHA_REAL_DOCKER").unwrap_or_else(|_| "/usr/bin/docker".to_string())
}

pub fn default_state_dir() -> PathBuf {
    std::env::var("GHA_DOCKER_WRAPPER_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/gha-docker-wrapper"))
}

pub fn handshake_timeout_sec() -> f64 {
    std::env::var("GHA_DOCKER_WRAPPER_HANDSHAKE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(15.0)
}

pub fn is_github_network(net_name: &str) -> bool {
    let prefix = "github_network_";
    if let Some(suffix) = net_name.strip_prefix(prefix) {
        !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_hexdigit())
    } else {
        false
    }
}

// ----------------------------------------------------------------------------
// STATE MODELS
// ----------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default, Clone, PartialEq)]
pub struct WrapperState {
    #[serde(default)]
    pub runner_id: Option<String>,
    #[serde(default)]
    pub networks: HashMap<String, NetworkEntry>,
    #[serde(default)]
    pub containers: HashMap<String, ContainerEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct NetworkEntry {
    pub network_id: String,
    pub runner_id: String,
    pub owned_by_wrapper: bool,
    #[serde(default)]
    pub containers: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct ContainerEntry {
    pub name: String,
    #[serde(default)]
    pub networks: Vec<String>,
    pub target_ip: String,
    #[serde(default)]
    pub requested_ports: HashMap<String, u16>,
    #[serde(default)]
    pub relays: Vec<RelayEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct RelayEntry {
    pub pid: u32,
    pub local_port: u16,
    pub target_ip: String,
    pub target_port: u16,
}

// ----------------------------------------------------------------------------
// ARGUMENT PARSING
// ----------------------------------------------------------------------------

pub fn split_global_and_sub_args(args: &[String]) -> (Vec<String>, Option<String>, Vec<String>) {
    let mut i = 0;
    while i < args.len() {
        let token = &args[i];
        if GLOBAL_VALUE_FLAGS.contains(&token.as_str()) {
            i += 2;
            continue;
        }
        if GLOBAL_VALUE_FLAGS
            .iter()
            .any(|f| token.starts_with(&format!("{f}=")))
        {
            i += 1;
            continue;
        }
        if token.starts_with('-') {
            i += 1;
            continue;
        }
        return (
            args[..i].to_vec(),
            Some(token.clone()),
            args[i + 1..].to_vec(),
        );
    }
    (args.to_vec(), None, vec![])
}

#[derive(Debug, PartialEq, Eq)]
pub struct PublishSpec {
    pub host_ip: Option<String>,
    pub host_port: u16,
    pub container_port: u16,
}

pub fn parse_publish_spec(spec: &str) -> Option<PublishSpec> {
    let (body, proto) = if let Some((b, p)) = spec.rsplit_once('/') {
        (b, p.to_lowercase())
    } else {
        (spec, "tcp".to_string())
    };
    if proto != "tcp" {
        return None;
    }
    let parts: Vec<&str> = body.split(':').collect();
    let (host_ip, hport_str, cport_str) = match parts.len() {
        2 => (None, parts[0], parts[1]),
        3 => (Some(parts[0].to_string()), parts[1], parts[2]),
        _ => return None,
    };
    let host_port: u16 = hport_str.parse().ok()?;
    let container_port: u16 = cport_str.parse().ok()?;
    if host_port == 0 || container_port == 0 {
        return None;
    }
    Some(PublishSpec {
        host_ip,
        host_port,
        container_port,
    })
}

pub fn extract_option_value(args: &[String], flags: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let tok = &args[i];
        if flags.contains(&tok.as_str()) && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        for flag in flags {
            if let Some(rest) = tok.strip_prefix(&format!("{flag}=")) {
                return Some(rest.to_string());
            }
        }
        i += 1;
    }
    None
}

pub fn extract_positional_targets(sub_args: &[String], flags_with_value: &[&str]) -> Vec<String> {
    let mut targets = Vec::new();
    let mut i = 0;
    while i < sub_args.len() {
        let tok = &sub_args[i];
        if tok == "--" {
            targets.extend(sub_args[i + 1..].iter().cloned());
            break;
        }
        if flags_with_value.contains(&tok.as_str()) {
            i += 2;
            continue;
        }
        if flags_with_value
            .iter()
            .any(|f| tok.starts_with(&format!("{f}=")))
        {
            i += 1;
            continue;
        }
        if tok.starts_with('-') {
            i += 1;
            continue;
        }
        targets.push(tok.clone());
        i += 1;
    }
    targets
}

pub fn rewrite_publish_args(args: &[String]) -> (Vec<String>, HashMap<u16, u16>) {
    let (global, subcmd, sub_args) = split_global_and_sub_args(args);
    let subcmd = match subcmd {
        Some(s) if s == "create" || s == "run" => s,
        _ => return (args.to_vec(), HashMap::new()),
    };

    let network = extract_option_value(&sub_args, &["--network", "--net"]);
    let is_github_net = network.as_deref().map_or(false, is_github_network);

    if !is_github_net {
        return (args.to_vec(), HashMap::new());
    }

    let mut rewritten_sub = sub_args.clone();
    let mut requested_map = HashMap::new();
    let mut i = 0;
    while i < rewritten_sub.len() {
        let tok = &rewritten_sub[i];
        if (tok == "-p" || tok == "--publish") && i + 1 < rewritten_sub.len() {
            if let Some(spec) = parse_publish_spec(&rewritten_sub[i + 1]) {
                rewritten_sub[i + 1] = format!("127.0.0.1::{}", spec.container_port);
                requested_map.insert(spec.container_port, spec.host_port);
            }
            i += 2;
            continue;
        }
        if tok.starts_with("-p=") || tok.starts_with("--publish=") {
            let (prefix, val) = tok.split_once('=').unwrap();
            if let Some(spec) = parse_publish_spec(val) {
                rewritten_sub[i] = format!("{prefix}=127.0.0.1::{}", spec.container_port);
                requested_map.insert(spec.container_port, spec.host_port);
            }
        }
        i += 1;
    }

    let mut full = global;
    full.push(subcmd);
    full.extend(rewritten_sub);
    (full, requested_map)
}

// ----------------------------------------------------------------------------
// STATE STORE WITH FLOCK
// ----------------------------------------------------------------------------

pub struct StateStore {
    pub dir: PathBuf,
    pub lock_path: PathBuf,
    pub state_path: PathBuf,
}

impl StateStore {
    pub fn new(dir: PathBuf) -> Self {
        let lock_path = dir.join("state.lock");
        let state_path = dir.join("state.json");
        Self {
            dir,
            lock_path,
            state_path,
        }
    }

    pub fn with_lock<F, R>(&self, f: F) -> Result<R, Box<dyn std::error::Error>>
    where
        F: FnOnce(&mut WrapperState) -> Result<R, Box<dyn std::error::Error>>,
    {
        fs::create_dir_all(&self.dir)?;
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&self.lock_path)?;

        let fd = lock_file.as_raw_fd();
        unsafe {
            libc::flock(fd, libc::LOCK_EX);
        }

        struct LockGuard(libc::c_int);
        impl Drop for LockGuard {
            fn drop(&mut self) {
                unsafe {
                    libc::flock(self.0, libc::LOCK_UN);
                }
            }
        }
        let _guard = LockGuard(fd);

        let mut state = if self.state_path.exists() {
            let content = fs::read_to_string(&self.state_path).unwrap_or_default();
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            WrapperState::default()
        };

        let result = f(&mut state)?;

        let tmp_path = self.dir.join(format!("state.{}.tmp", std::process::id()));
        {
            let mut tmp_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o600)
                .open(&tmp_path)?;

            let bytes = serde_json::to_vec_pretty(&state)?;
            tmp_file.write_all(&bytes)?;
            tmp_file.flush()?;
            unsafe {
                libc::fsync(tmp_file.as_raw_fd());
            }
        }
        fs::rename(&tmp_path, &self.state_path)?;

        Ok(result)
    }
}

// ----------------------------------------------------------------------------
// RELAY & NETWORKING HELPERS
// ----------------------------------------------------------------------------

pub fn terminate_relay(relay: &RelayEntry, sys: &dyn SystemInspector) {
    if relay.pid <= 1 {
        return;
    }
    if !sys.verify_socat_cmdline(
        relay.pid,
        relay.local_port,
        &relay.target_ip,
        relay.target_port,
    ) {
        return;
    }

    unsafe {
        libc::kill(relay.pid as i32, libc::SIGTERM);
    }
    for _ in 0..50 {
        if unsafe { libc::kill(relay.pid as i32, 0) } != 0 {
            return;
        }
        sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(relay.pid as i32, libc::SIGKILL);
    }
}

pub fn start_owned_relay(
    local_port: u16,
    target_ip: &str,
    target_port: u16,
) -> Result<u32, Box<dyn std::error::Error>> {
    let listen_arg = format!("TCP-LISTEN:{local_port},bind=127.0.0.1,reuseaddr,fork");
    let connect_arg = format!("TCP:{target_ip}:{target_port}");

    let mut child = Command::new("socat")
        .args([&listen_arg, &connect_arg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;

    sleep(Duration::from_millis(50));

    match child.try_wait()? {
        Some(status) => {
            let mut err = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                use std::io::Read;
                let _ = stderr.read_to_string(&mut err);
            }
            Err(format!(
                "socat relay for 127.0.0.1:{local_port} -> {target_ip}:{target_port} exited immediately (rc={}): {}",
                status.code().unwrap_or(-1),
                err.trim()
            ).into())
        }
        None => {
            let pid = child.id();
            drop(child.stderr.take());
            Ok(pid)
        }
    }
}

pub fn wait_for_tcp_handshake(
    host: &str,
    port: u16,
    timeout_sec: f64,
) -> Result<(), Box<dyn std::error::Error>> {
    if timeout_sec <= 0.0 {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_sec);
    let mut last_err = None;
    let addr_str = format!("{host}:{port}");

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let attempt_timeout = remaining
            .min(Duration::from_secs(1))
            .max(Duration::from_millis(50));

        if let Ok(mut addrs) = addr_str.to_socket_addrs() {
            if let Some(addr) = addrs.next() {
                match TcpStream::connect_timeout(&addr, attempt_timeout) {
                    Ok(stream) => {
                        drop(stream);
                        return Ok(());
                    }
                    Err(e) => {
                        last_err = Some(e);
                    }
                }
            }
        }
        sleep(Duration::from_millis(50));
    }

    Err(format!(
        "Timed out after {:.1}s waiting for TCP reachability to {}:{} (last_error={:?})",
        timeout_sec, host, port, last_err
    )
    .into())
}

fn get_system_hostname() -> Option<String> {
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.trim().is_empty() {
            return Some(h.trim().to_string());
        }
    }
    let mut buf = [0u8; 256];
    let res = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if res == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(s) = std::str::from_utf8(&buf[..len]) {
            if !s.trim().is_empty() {
                return Some(s.trim().to_string());
            }
        }
    }
    None
}

// ----------------------------------------------------------------------------
// DOCKER RUNNER TRAIT & REAL IMPLEMENTATION
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DockerOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait DockerRunner: Send + Sync {
    fn run_docker(
        &self,
        global_args: &[String],
        sub_args: &[String],
    ) -> Result<DockerOutput, Box<dyn std::error::Error>>;
    fn start_relay(
        &self,
        local_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> Result<u32, Box<dyn std::error::Error>>;
    fn terminate_relay(&self, relay: &RelayEntry, sys: &dyn SystemInspector);
    fn wait_for_tcp_handshake(
        &self,
        host: &str,
        port: u16,
        timeout_sec: f64,
    ) -> Result<(), Box<dyn std::error::Error>>;
    fn detect_self_container_id(&self, global_args: &[String]) -> Option<String>;
}

#[derive(Default, Clone)]
pub struct RealDockerRunner;

impl DockerRunner for RealDockerRunner {
    fn run_docker(
        &self,
        global_args: &[String],
        sub_args: &[String],
    ) -> Result<DockerOutput, Box<dyn std::error::Error>> {
        let output = Command::new(real_docker_binary())
            .args(global_args)
            .args(sub_args)
            .output()?;
        Ok(DockerOutput {
            status: output.status.code().unwrap_or(0),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    fn start_relay(
        &self,
        local_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> Result<u32, Box<dyn std::error::Error>> {
        start_owned_relay(local_port, target_ip, target_port)
    }

    fn terminate_relay(&self, relay: &RelayEntry, sys: &dyn SystemInspector) {
        terminate_relay(relay, sys);
    }

    fn wait_for_tcp_handshake(
        &self,
        host: &str,
        port: u16,
        timeout_sec: f64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        wait_for_tcp_handshake(host, port, timeout_sec)
    }

    fn detect_self_container_id(&self, global_args: &[String]) -> Option<String> {
        if let Ok(override_id) = std::env::var("GHA_RUNNER_CONTAINER_ID") {
            if !override_id.trim().is_empty() {
                return Some(override_id.trim().to_string());
            }
        }
        let mut candidates = Vec::new();
        if let Some(h) = get_system_hostname() {
            candidates.push(h);
        }
        if let Ok(rname) = std::env::var("RUNNER_NAME") {
            let trimmed = rname.trim().to_string();
            if !trimmed.is_empty() && !candidates.contains(&trimmed) {
                candidates.push(trimmed);
            }
        }
        for cand in candidates {
            if let Ok(res) = self.run_docker(
                global_args,
                &["inspect".into(), "--format".into(), "{{.Id}}".into(), cand],
            ) {
                if res.status == 0 && !res.stdout.trim().is_empty() {
                    if let Some(first_line) = res.stdout.lines().next() {
                        let id = first_line.trim().to_string();
                        if !id.is_empty() {
                            return Some(id);
                        }
                    }
                }
            }
        }
        None
    }
}

// ----------------------------------------------------------------------------
// SERVICE NETWORKING LIFECYCLE
// ----------------------------------------------------------------------------

pub fn ensure_runner_attached_to_network(
    global_args: &[String],
    state: &mut WrapperState,
    network_name: &str,
    runner: &dyn DockerRunner,
) -> Result<String, Box<dyn std::error::Error>> {
    let runner_id = match &state.runner_id {
        Some(id) if !id.is_empty() => id.clone(),
        _ => {
            let id = runner
                .detect_self_container_id(global_args)
                .ok_or("Could not determine self runner container ID for network attachment")?;
            state.runner_id = Some(id.clone());
            id
        }
    };

    let inspect_res = runner.run_docker(global_args, &["inspect".into(), runner_id.clone()])?;
    if inspect_res.status != 0 {
        return Err(format!(
            "docker inspect runner failed: {}",
            inspect_res.stderr.trim()
        )
        .into());
    }

    let runner_info_vec: Vec<serde_json::Value> = serde_json::from_str(&inspect_res.stdout)?;
    let runner_info = runner_info_vec
        .first()
        .ok_or("Empty inspect output for runner")?;
    let attached_nets = runner_info
        .get("NetworkSettings")
        .and_then(|n| n.get("Networks"))
        .and_then(|n| n.as_object())
        .ok_or("Missing NetworkSettings.Networks on runner")?;

    if !attached_nets.contains_key("bridge") {
        let short_id = if runner_id.len() >= 12 {
            &runner_id[..12]
        } else {
            &runner_id
        };
        return Err(format!(
            "Runner container {short_id} is not attached to default 'bridge' network"
        )
        .into());
    }

    let is_attached = attached_nets.contains_key(network_name);
    let net_entry_exists = state.networks.contains_key(network_name);

    if !is_attached {
        let conn_res = runner.run_docker(
            global_args,
            &[
                "network".into(),
                "connect".into(),
                "--gw-priority".into(),
                "-1".into(),
                network_name.into(),
                runner_id.clone(),
            ],
        )?;

        if conn_res.status != 0 {
            // Recheck inspect
            let recheck_res =
                runner.run_docker(global_args, &["inspect".into(), runner_id.clone()])?;
            let recheck_vec: Vec<serde_json::Value> = serde_json::from_str(&recheck_res.stdout)?;
            let recheck_nets = recheck_vec
                .first()
                .and_then(|v| v.get("NetworkSettings"))
                .and_then(|n| n.get("Networks"))
                .and_then(|n| n.as_object());
            let still_missing = recheck_nets.map_or(true, |m| !m.contains_key(network_name));
            if still_missing {
                let detail = conn_res.stderr.trim();
                let short_id = if runner_id.len() >= 12 {
                    &runner_id[..12]
                } else {
                    &runner_id
                };
                return Err(format!(
                    "Failed to attach runner {short_id} to job network {network_name}: {detail}"
                )
                .into());
            }
        }

        // Post connect check: verify bridge gateway was not lost
        let post_res = runner.run_docker(global_args, &["inspect".into(), runner_id.clone()])?;
        let post_vec: Vec<serde_json::Value> = serde_json::from_str(&post_res.stdout)?;
        let post_nets = post_vec
            .first()
            .and_then(|v| v.get("NetworkSettings"))
            .and_then(|n| n.get("Networks"))
            .and_then(|n| n.as_object())
            .ok_or("Missing NetworkSettings.Networks on post-inspect")?;

        let bridge_gateway = post_nets
            .get("bridge")
            .and_then(|b| b.get("Gateway"))
            .and_then(|g| g.as_str())
            .unwrap_or("");

        if bridge_gateway.is_empty() {
            let short_id = if runner_id.len() >= 12 {
                &runner_id[..12]
            } else {
                &runner_id
            };
            return Err(format!(
                "Runner {short_id} default bridge gateway was lost after attaching {network_name}"
            )
            .into());
        }

        if !net_entry_exists {
            let net_id_res = runner.run_docker(
                global_args,
                &[
                    "network".into(),
                    "inspect".into(),
                    "--format".into(),
                    "{{.Id}}".into(),
                    network_name.into(),
                ],
            )?;
            let net_id = if net_id_res.status == 0 && !net_id_res.stdout.trim().is_empty() {
                net_id_res.stdout.trim().to_string()
            } else {
                network_name.to_string()
            };
            state.networks.insert(
                network_name.to_string(),
                NetworkEntry {
                    network_id: net_id,
                    runner_id: runner_id.clone(),
                    owned_by_wrapper: true,
                    containers: vec![],
                },
            );
        }
    } else if !net_entry_exists {
        let net_id_res = runner.run_docker(
            global_args,
            &[
                "network".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}".into(),
                network_name.into(),
            ],
        )?;
        let net_id = if net_id_res.status == 0 && !net_id_res.stdout.trim().is_empty() {
            net_id_res.stdout.trim().to_string()
        } else {
            network_name.to_string()
        };
        state.networks.insert(
            network_name.to_string(),
            NetworkEntry {
                network_id: net_id,
                runner_id: runner_id.clone(),
                owned_by_wrapper: false,
                containers: vec![],
            },
        );
    }

    Ok(runner_id)
}

pub fn detach_runner_from_network_if_unused(
    global_args: &[String],
    state: &mut WrapperState,
    network_name: &str,
    force: bool,
    runner: &dyn DockerRunner,
) {
    let should_detach = {
        let net_entry = match state.networks.get_mut(network_name) {
            Some(e) => e,
            None => return,
        };
        net_entry
            .containers
            .retain(|cid| state.containers.contains_key(cid));
        if !net_entry.containers.is_empty() && !force {
            return;
        }
        net_entry.owned_by_wrapper
    };

    if should_detach {
        let runner_id = state
            .networks
            .get(network_name)
            .map(|e| e.runner_id.clone())
            .or_else(|| state.runner_id.clone())
            .or_else(|| runner.detect_self_container_id(global_args));

        if let Some(rid) = runner_id {
            let disc_res = runner.run_docker(
                global_args,
                &[
                    "network".into(),
                    "disconnect".into(),
                    network_name.into(),
                    rid.clone(),
                ],
            );
            if let Ok(res) = disc_res {
                if res.status != 0 {
                    let err_lower = format!("{} {}", res.stdout, res.stderr).to_lowercase();
                    if !err_lower.contains("is not connected")
                        && !err_lower.contains("not found")
                        && !err_lower.contains("no such network")
                    {
                        let _ = runner.run_docker(
                            global_args,
                            &[
                                "network".into(),
                                "disconnect".into(),
                                "-f".into(),
                                network_name.into(),
                                rid,
                            ],
                        );
                    }
                }
            }
        }
    }

    state.networks.remove(network_name);
}

pub fn cleanup_container_state(
    global_args: &[String],
    state: &mut WrapperState,
    cid_or_name: &str,
    runner: &dyn DockerRunner,
    sys: &dyn SystemInspector,
) {
    let mut matched_cids = Vec::new();
    for (cid, cinfo) in &state.containers {
        if cid == cid_or_name
            || cid.starts_with(cid_or_name)
            || cinfo.name == cid_or_name
            || cinfo.name == cid_or_name.trim_start_matches('/')
        {
            matched_cids.push(cid.clone());
        }
    }

    if matched_cids.is_empty() {
        if let Ok(res) = runner.run_docker(
            global_args,
            &[
                "inspect".into(),
                "--format".into(),
                "{{.Id}}".into(),
                cid_or_name.into(),
            ],
        ) {
            if res.status == 0 && !res.stdout.trim().is_empty() {
                if let Some(full_id) = res.stdout.lines().next().map(|s| s.trim()) {
                    if state.containers.contains_key(full_id) {
                        matched_cids.push(full_id.to_string());
                    }
                }
            }
        }
    }

    for cid in matched_cids {
        if let Some(cinfo) = state.containers.remove(&cid) {
            for relay in &cinfo.relays {
                runner.terminate_relay(relay, sys);
            }
            for net_name in &cinfo.networks {
                if let Some(net_entry) = state.networks.get_mut(net_name) {
                    net_entry.containers.retain(|c| c != &cid);
                }
                detach_runner_from_network_if_unused(global_args, state, net_name, false, runner);
            }
        }
    }
}

pub fn setup_service_container(
    global_args: &[String],
    state: &mut WrapperState,
    cid_or_name: &str,
    requested_ports_override: Option<&HashMap<u16, u16>>,
    runner: &dyn DockerRunner,
    sys: &dyn SystemInspector,
) -> Result<(), Box<dyn std::error::Error>> {
    let inspect_res =
        runner.run_docker(global_args, &["inspect".into(), cid_or_name.to_string()])?;
    if inspect_res.status != 0 {
        return Err(format!(
            "docker inspect failed for {}: {}",
            cid_or_name,
            inspect_res.stderr.trim()
        )
        .into());
    }

    let inspect_vec: Vec<serde_json::Value> = serde_json::from_str(&inspect_res.stdout)?;
    let info = inspect_vec.first().ok_or("Empty inspect output")?;
    let cid = info
        .get("Id")
        .and_then(|v| v.as_str())
        .ok_or("Missing Id in container inspect")?
        .to_string();
    let cname = info
        .get("Name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();
    let net_settings = info.get("NetworkSettings").and_then(|v| v.as_object());
    let networks = net_settings
        .and_then(|ns| ns.get("Networks"))
        .and_then(|v| v.as_object());

    let empty_map = serde_json::Map::new();
    let networks_map = networks.unwrap_or(&empty_map);

    let job_networks: Vec<String> = networks_map
        .keys()
        .filter(|k| is_github_network(k))
        .cloned()
        .collect();

    let ports_map = net_settings
        .and_then(|ns| ns.get("Ports"))
        .and_then(|v| v.as_object());
    let has_published_tcp_ports = ports_map.map_or(false, |ports| {
        ports.iter().any(|(k, v)| {
            k.ends_with("/tcp")
                && v.as_array().map_or(false, |arr| {
                    arr.iter().any(|b| {
                        b.get("HostPort")
                            .and_then(|hp| hp.as_str())
                            .and_then(|s| s.parse::<u16>().ok())
                            .unwrap_or(0)
                            > 0
                    })
                })
        })
    });
    let bridge_ip: Option<String> = networks_map
        .get("bridge")
        .and_then(|b| b.get("IPAddress"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            net_settings
                .and_then(|ns| ns.get("IPAddress"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
        });

    if job_networks.is_empty() && (!has_published_tcp_ports || bridge_ip.is_none()) {
        return Ok(());
    }

    let existing_entry = state.containers.get(&cid).cloned().unwrap_or_default();
    let mut requested_ports = existing_entry.requested_ports.clone();
    if let Some(overrides) = requested_ports_override {
        for (cport, hport) in overrides {
            requested_ports.insert(cport.to_string(), *hport);
        }
    }

    // Terminate any existing relays for this container before recreation
    for old_relay in &existing_entry.relays {
        runner.terminate_relay(old_relay, sys);
    }

    let mut attached_job_networks = Vec::new();
    let mut target_ip = String::new();
    let mut new_relays = Vec::new();

    let mut try_block = || -> Result<(), Box<dyn std::error::Error>> {
        for net_name in &job_networks {
            ensure_runner_attached_to_network(global_args, state, net_name, runner)?;
            attached_job_networks.push(net_name.clone());

            if let Some(net_entry) = state.networks.get_mut(net_name) {
                if !net_entry.containers.contains(&cid) {
                    net_entry.containers.push(cid.clone());
                }
            }

            if target_ip.is_empty() {
                if let Some(net_val) = networks_map.get(net_name) {
                    if let Some(ip) = net_val.get("IPAddress").and_then(|v| v.as_str()) {
                        if !ip.is_empty() {
                            target_ip = ip.to_string();
                        }
                    }
                }
            }
        }

        if target_ip.is_empty() {
            if let Some(ref bip) = bridge_ip {
                target_ip = bip.clone();
            }
        }

        if target_ip.is_empty() {
            let short_id = if cid.len() >= 12 { &cid[..12] } else { &cid };
            return Err(format!(
                "Service container {short_id} on {:?} has no IPv4 address",
                job_networks
            )
            .into());
        }
        if let Some(ports) = ports_map {
            let mut port_keys: Vec<&String> = ports.keys().collect();
            port_keys.sort();

            for cport_proto in port_keys {
                if !cport_proto.ends_with("/tcp") {
                    continue;
                }
                let bindings = match ports.get(cport_proto).and_then(|v| v.as_array()) {
                    Some(b) if !b.is_empty() => b,
                    _ => continue,
                };

                let cport: u16 = match cport_proto.split('/').next().and_then(|s| s.parse().ok()) {
                    Some(p) => p,
                    None => continue,
                };

                let mut local_ports: BTreeSet<u16> = BTreeSet::new();
                for binding in bindings {
                    if let Some(hport_str) = binding.get("HostPort").and_then(|v| v.as_str()) {
                        if let Ok(hp) = hport_str.parse::<u16>() {
                            if hp > 0 {
                                local_ports.insert(hp);
                            }
                        }
                    }
                }

                if let Some(&req_hport) = requested_ports.get(&cport.to_string()) {
                    local_ports.insert(req_hport);
                } else if cport == 3306 {
                    local_ports.insert(3306);
                }

                if local_ports.is_empty() {
                    continue;
                }

                // Upstream reachability check
                runner.wait_for_tcp_handshake(&target_ip, cport, handshake_timeout_sec())?;

                for &lport in &local_ports {
                    // Conflict resolution: terminate other container's relay using this local port
                    for (other_cid, other_info) in state.containers.iter_mut() {
                        if other_cid == &cid {
                            continue;
                        }
                        let mut kept = Vec::new();
                        for r in other_info.relays.drain(..) {
                            if r.local_port == lport {
                                runner.terminate_relay(&r, sys);
                            } else {
                                kept.push(r);
                            }
                        }
                        other_info.relays = kept;
                    }

                    let pid = runner.start_relay(lport, &target_ip, cport)?;
                    new_relays.push(RelayEntry {
                        pid,
                        local_port: lport,
                        target_ip: target_ip.clone(),
                        target_port: cport,
                    });

                    // Local listener reachability check
                    runner.wait_for_tcp_handshake("127.0.0.1", lport, 5.0)?;
                }
            }
        }

        Ok(())
    };

    match try_block() {
        Ok(()) => {
            state.containers.insert(
                cid,
                ContainerEntry {
                    name: cname,
                    networks: attached_job_networks,
                    target_ip,
                    requested_ports,
                    relays: new_relays,
                },
            );
            Ok(())
        }
        Err(err) => {
            // Fail closed: rollback relays, container entry, and network attachments
            for relay in &new_relays {
                runner.terminate_relay(relay, sys);
            }
            state.containers.remove(&cid);
            for net_name in &attached_job_networks {
                if let Some(net_entry) = state.networks.get_mut(net_name) {
                    net_entry.containers.retain(|c| c != &cid);
                }
                detach_runner_from_network_if_unused(global_args, state, net_name, true, runner);
            }
            Err(err)
        }
    }
}

pub fn record_created_container(
    _global_args: &[String],
    state: &mut WrapperState,
    cid: &str,
    sub_args: &[String],
    requested_map: &HashMap<u16, u16>,
) {
    if requested_map.is_empty() {
        return;
    }
    let cname = extract_option_value(sub_args, &["--name"]).unwrap_or_default();
    let net_name = extract_option_value(sub_args, &["--network", "--net"]).unwrap_or_default();
    let mut ports = HashMap::new();
    for (&c, &h) in requested_map {
        ports.insert(c.to_string(), h);
    }
    state.containers.insert(
        cid.to_string(),
        ContainerEntry {
            name: cname.trim_start_matches('/').to_string(),
            networks: if is_github_network(&net_name) {
                vec![net_name]
            } else {
                vec![]
            },
            target_ip: String::new(),
            requested_ports: ports,
            relays: vec![],
        },
    );
}

pub fn handle_port_command(
    global_args: &[String],
    sub_args: &[String],
    store: &StateStore,
    runner: &dyn DockerRunner,
) -> Result<i32, Box<dyn std::error::Error>> {
    let mut cmd_args = vec!["port".to_string()];
    cmd_args.extend(sub_args.iter().cloned());
    let res = runner.run_docker(global_args, &cmd_args)?;
    eprint!("{}", res.stderr);

    if res.status != 0 || sub_args.is_empty() {
        print!("{}", res.stdout);
        return Ok(res.status);
    }

    let target_ref = &sub_args[0];
    let requested_map: HashMap<String, u16> = store
        .with_lock(|state| {
            for (cid, cinfo) in &state.containers {
                if cid == target_ref
                    || cid.starts_with(target_ref)
                    || cinfo.name == *target_ref
                    || cinfo.name == target_ref.trim_start_matches('/')
                {
                    return Ok(cinfo.requested_ports.clone());
                }
            }
            Ok(HashMap::new())
        })
        .unwrap_or_default();

    let mut out_lines = Vec::new();
    for line in res.stdout.lines() {
        if let Some((cport_proto, _)) = line.split_once(" -> ") {
            if cport_proto.ends_with("/tcp") {
                let cport_str = cport_proto.split('/').next().unwrap_or("");
                if let Some(&req_hport) = requested_map.get(cport_str) {
                    out_lines.push(format!("{cport_proto} -> 0.0.0.0:{req_hport}"));
                    continue;
                }
                if cport_str == "3306" {
                    out_lines.push("3306/tcp -> 0.0.0.0:3306".to_string());
                    continue;
                }
            }
        }
        out_lines.push(line.to_string());
    }

    if !out_lines.is_empty() {
        println!("{}", out_lines.join("\n"));
    }
    Ok(res.status)
}

// ----------------------------------------------------------------------------
// ENTRYPOINTS
// ----------------------------------------------------------------------------

pub fn run_with_runner(
    args: &[String],
    runner: &dyn DockerRunner,
    sys: &dyn SystemInspector,
    store: &StateStore,
) -> Result<i32, Box<dyn std::error::Error>> {
    let (rewritten_args, requested_map) = rewrite_publish_args(args);
    let (global_args, subcmd, sub_args) = split_global_and_sub_args(&rewritten_args);

    let subcmd_str = match subcmd {
        Some(ref s) => s.as_str(),
        None => {
            let res = runner.run_docker(&global_args, &sub_args)?;
            print!("{}", res.stdout);
            eprint!("{}", res.stderr);
            return Ok(res.status);
        }
    };

    if subcmd_str == "port" {
        return handle_port_command(&global_args, &sub_args, store, runner);
    }

    if subcmd_str == "create" {
        let mut full_sub = vec![subcmd_str.to_string()];
        full_sub.extend(sub_args.clone());
        let res = runner.run_docker(&global_args, &full_sub)?;
        if res.status == 0 && !requested_map.is_empty() && !res.stdout.trim().is_empty() {
            let cid = res
                .stdout
                .trim()
                .lines()
                .last()
                .unwrap_or("")
                .trim()
                .to_string();
            if !cid.is_empty() {
                let _ = store.with_lock(|state| {
                    record_created_container(&global_args, state, &cid, &sub_args, &requested_map);
                    Ok(())
                });
            }
        }
        print!("{}", res.stdout);
        eprint!("{}", res.stderr);
        return Ok(res.status);
    }

    let is_detached_run = subcmd_str == "run"
        && sub_args
            .iter()
            .any(|a| a == "-d" || a == "--detach" || a.starts_with("--detach="));

    if subcmd_str == "start" || is_detached_run {
        let mut full_sub = vec![subcmd_str.to_string()];
        full_sub.extend(sub_args.clone());
        let res = runner.run_docker(&global_args, &full_sub)?;
        if res.status != 0 {
            print!("{}", res.stdout);
            eprint!("{}", res.stderr);
            return Ok(res.status);
        }

        let mut targets = Vec::new();
        if subcmd_str == "run" {
            if let Some(last_line) = res.stdout.trim().lines().last() {
                let trimmed = last_line.trim();
                if !trimmed.is_empty() {
                    targets.push(trimmed.to_string());
                }
            }
        } else {
            targets = extract_positional_targets(
                &sub_args,
                &["--detach-keys", "-a", "--attach", "-i", "--interactive"],
            );
        }

        let setup_res = store.with_lock(|state| {
            for target in &targets {
                setup_service_container(
                    &global_args,
                    state,
                    target,
                    if subcmd_str == "run" {
                        Some(&requested_map)
                    } else {
                        None
                    },
                    runner,
                    sys,
                )?;
            }
            Ok(())
        });

        if let Err(err) = setup_res {
            eprintln!("[docker-wrapper] error setting up service container reachability: {err}");
            return Ok(1);
        }

        print!("{}", res.stdout);
        eprint!("{}", res.stderr);
        return Ok(res.status);
    }

    if subcmd_str == "stop" || subcmd_str == "rm" || subcmd_str == "kill" {
        let targets = extract_positional_targets(&sub_args, &["-t", "--time", "-s", "--signal"]);
        let _ = store.with_lock(|state| {
            for target in &targets {
                cleanup_container_state(&global_args, state, target, runner, sys);
            }
            Ok(())
        });

        let mut full_sub = vec![subcmd_str.to_string()];
        full_sub.extend(sub_args);
        let res = runner.run_docker(&global_args, &full_sub)?;
        print!("{}", res.stdout);
        eprint!("{}", res.stderr);
        return Ok(res.status);
    }

    if subcmd_str == "network" && sub_args.first().map(String::as_str) == Some("rm") {
        let net_targets = extract_positional_targets(&sub_args[1..], &[]);
        let _ = store.with_lock(|state| {
            for net_target in &net_targets {
                let mut matched_nets = Vec::new();
                for (net_name, net_entry) in &state.networks {
                    if net_name == net_target
                        || net_entry.network_id.starts_with(net_target.as_str())
                    {
                        matched_nets.push((net_name.clone(), net_entry.containers.clone()));
                    }
                }
                for (net_name, cids) in matched_nets {
                    for cid in cids {
                        cleanup_container_state(&global_args, state, &cid, runner, sys);
                    }
                    detach_runner_from_network_if_unused(
                        &global_args,
                        state,
                        &net_name,
                        true,
                        runner,
                    );
                }
            }
            Ok(())
        });

        let mut full_sub = vec![subcmd_str.to_string()];
        full_sub.extend(sub_args);
        let res = runner.run_docker(&global_args, &full_sub)?;
        print!("{}", res.stdout);
        eprint!("{}", res.stderr);
        return Ok(res.status);
    }

    // Default pass through
    let mut full_sub = vec![subcmd_str.to_string()];
    full_sub.extend(sub_args);
    let res = runner.run_docker(&global_args, &full_sub)?;
    print!("{}", res.stdout);
    eprint!("{}", res.stderr);
    Ok(res.status)
}

pub fn run_with_sys(
    args: &[String],
    sys: &dyn SystemInspector,
) -> Result<i32, Box<dyn std::error::Error>> {
    let runner = RealDockerRunner::default();
    let store = StateStore::new(default_state_dir());
    run_with_runner(args, &runner, sys, &store)
}

pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let sys = DefaultSystemInspector::default();
    run_with_sys(args, &sys)
}
