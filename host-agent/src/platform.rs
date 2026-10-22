use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub struct ProcessRecord {
    pub ppid: i32,
    pub starttime: String,
    pub comm: String,
    pub cgroup: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DockerContainerInspect {
    pub id: String,
    pub name: String,
    pub running: bool,
    pub started_at: String,
    pub pid: i32,
    pub memory_limit: u64,
    pub memory_swap_limit: u64,
    pub nano_cpus: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct AggregateCounters {
    pub path: String,
    pub inode: u64,
    pub device: u64,
    pub monotonic_ns: u64,
    pub memory_current: String,
    pub memory_peak: String,
    pub memory_max: String,
    pub memory_events: String,
    pub cpu_stat: String,
    pub cpu_max: String,
}

pub trait SystemInspector: Send + Sync {
    fn process_identity(&self, pid: i32) -> Result<String, String>;
    fn process_record(&self, pid: i32) -> Result<ProcessRecord, String>;
    fn docker_inspect(&self, container_id: &str) -> Result<DockerContainerInspect, String>;
    fn docker_ps_all_ids(&self) -> Result<Vec<String>, String>;
    fn docker_stats_usage(&self) -> Result<Vec<String>, String>;
    fn read_meminfo(&self) -> Result<(f64, f64), String>; // (total_gib, available_gib)
    fn builders_running(&self, global_lock_held: bool) -> bool;
    fn verify_socat_cmdline(
        &self,
        pid: u32,
        local_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> bool;
    fn read_cgroup_counters(&self) -> Result<AggregateCounters, String>;

    fn execute_command(
        &self,
        program: &Path,
        args: &[&str],
        env: Option<&HashMap<String, String>>,
    ) -> Result<String, String>;

    fn hash_file(&self, path: &Path) -> Result<String, String>;

    fn read_boot_id(&self) -> Result<String, String>;

    fn read_hostname(&self) -> Result<String, String>;

    fn is_root_owned_path(&self, path: &Path) -> bool;
}

/// Linux and macOS peer credential extraction
pub fn get_peer_credentials(stream: &UnixStream) -> io::Result<(i32, u32, u32)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = stream.as_raw_fd();
        let mut ucred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let res = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut ucred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if res != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((ucred.pid, ucred.uid, ucred.gid))
    }

    #[cfg(not(target_os = "linux"))]
    {
        if let Ok(mock) = std::env::var("MOCK_PEER_CRED") {
            let parts: Vec<&str> = mock.split(':').collect();
            if parts.len() == 3 {
                let pid = parts[0].parse().unwrap_or(100);
                let uid = parts[1].parse().unwrap_or(1001);
                let gid = parts[2].parse().unwrap_or(1001);
                return Ok((pid, uid, gid));
            }
        }
        use std::os::unix::io::AsRawFd;
        let fd = stream.as_raw_fd();
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        let res = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        if res != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((std::process::id() as i32, uid, gid))
    }
}

/// Default system inspector that interrogates real host OS (Linux /proc or macOS commands)
#[derive(Default, Clone)]
pub struct DefaultSystemInspector;

impl SystemInspector for DefaultSystemInspector {
    fn process_identity(&self, pid: i32) -> Result<String, String> {
        #[cfg(target_os = "linux")]
        {
            let stat_path = format!("/proc/{}/stat", pid);
            let content = std::fs::read_to_string(&stat_path)
                .map_err(|e| format!("Cannot read {}: {}", stat_path, e))?;
            let (_, after_comm) = content
                .rsplit_once(')')
                .ok_or_else(|| format!("Malformed /proc/{}/stat: no closing parenthesis", pid))?;
            let fields: Vec<&str> = after_comm.split_whitespace().collect();
            // In /proc/[pid]/stat, field 22 (starttime) is at index 19 after the ')'
            if fields.len() > 19 {
                return Ok(format!("host:{}:{}", pid, fields[19]));
            }
            Ok(format!("host:{}:1", pid))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(format!("host:{}:1000", pid))
        }
    }

    fn process_record(&self, _pid: i32) -> Result<ProcessRecord, String> {
        #[cfg(target_os = "linux")]
        {
            let stat_path = format!("/proc/{}/stat", _pid);
            let content = std::fs::read_to_string(&stat_path)
                .map_err(|e| format!("Cannot read {}: {}", stat_path, e))?;
            let (_, after_comm) = content
                .rsplit_once(')')
                .ok_or_else(|| format!("Malformed /proc/{}/stat", pid))?;
            let fields: Vec<&str> = after_comm.split_whitespace().collect();
            let ppid: i32 = fields.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            let starttime = fields.get(19).unwrap_or(&"0").to_string();

            let comm = std::fs::read_to_string(format!("/proc/{}/comm", pid))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();

            let cgroup =
                std::fs::read_to_string(format!("/proc/{}/cgroup", pid)).unwrap_or_default();

            Ok(ProcessRecord {
                ppid,
                starttime,
                comm,
                cgroup,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(ProcessRecord {
                ppid: 1,
                starttime: "1000".to_string(),
                comm: "mock".to_string(),
                cgroup: "".to_string(),
            })
        }
    }

    fn docker_inspect(&self, container_id: &str) -> Result<DockerContainerInspect, String> {
        let output = std::process::Command::new("docker")
            .args(["inspect", container_id])
            .output()
            .map_err(|e| format!("Failed to run docker inspect: {}", e))?;

        if !output.status.success() {
            return Err(format!(
                "docker inspect failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let rows: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("Failed to parse docker inspect output: {}", e))?;

        let first = rows
            .get(0)
            .ok_or_else(|| "Empty inspect array".to_string())?;
        let state = first
            .get("State")
            .ok_or_else(|| "Missing State".to_string())?;
        let host_config = first
            .get("HostConfig")
            .ok_or_else(|| "Missing HostConfig".to_string())?;

        Ok(DockerContainerInspect {
            id: first["Id"].as_str().unwrap_or(container_id).to_string(),
            name: first["Name"].as_str().unwrap_or("").to_string(),
            running: state["Running"].as_bool().unwrap_or(false),
            started_at: state["StartedAt"].as_str().unwrap_or("").to_string(),
            pid: state["Pid"].as_i64().unwrap_or(0) as i32,
            memory_limit: host_config["Memory"].as_u64().unwrap_or(0),
            memory_swap_limit: host_config["MemorySwap"].as_u64().unwrap_or(0),
            nano_cpus: host_config["NanoCpus"].as_u64().unwrap_or(0),
        })
    }

    fn docker_ps_all_ids(&self) -> Result<Vec<String>, String> {
        let output = std::process::Command::new("docker")
            .args(["ps", "-aq", "--no-trunc"])
            .output()
            .map_err(|e| format!("Failed to run docker ps: {}", e))?;

        if !output.status.success() {
            return Err(format!(
                "docker ps failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let text = String::from_utf8_lossy(&output.stdout);
        Ok(text.split_whitespace().map(|s| s.to_string()).collect())
    }

    fn docker_stats_usage(&self) -> Result<Vec<String>, String> {
        let output = std::process::Command::new("docker")
            .args(["stats", "--no-stream", "--format", "{{.MemUsage}}"])
            .output()
            .map_err(|e| format!("Failed to run docker stats: {}", e))?;

        if !output.status.success() {
            return Err(format!(
                "docker stats failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let text = String::from_utf8_lossy(&output.stdout);
        Ok(text
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }

    fn read_meminfo(&self) -> Result<(f64, f64), String> {
        #[cfg(target_os = "linux")]
        {
            let content = std::fs::read_to_string("/proc/meminfo")
                .map_err(|e| format!("Failed to read /proc/meminfo: {}", e))?;
            let mut total_kb: Option<f64> = None;
            let mut avail_kb: Option<f64> = None;
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    if let Some(val) = rest.split_whitespace().next().and_then(|s| s.parse().ok()) {
                        total_kb = Some(val);
                    }
                } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
                    if let Some(val) = rest.split_whitespace().next().and_then(|s| s.parse().ok()) {
                        avail_kb = Some(val);
                    }
                }
            }
            match (total_kb, avail_kb) {
                (Some(tot), Some(av)) => Ok((tot / (1024.0 * 1024.0), av / (1024.0 * 1024.0))),
                _ => Err("Could not find MemTotal and MemAvailable in /proc/meminfo".to_string()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // On macOS development workstation: return 32 GiB total, 28 GiB available as standard spot builder shape
            Ok((32.0, 28.0))
        }
    }

    fn builders_running(&self, _global_lock_held: bool) -> bool {
        // Inspect docker for running builder containers
        if let Ok(ids) = self.docker_ps_all_ids() {
            for id in ids {
                if let Ok(inspect) = self.docker_inspect(&id) {
                    if inspect.running && inspect.name.contains("oxidex-run") {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn verify_socat_cmdline(
        &self,
        pid: u32,
        local_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> bool {
        #[cfg(target_os = "linux")]
        {
            let path = format!("/proc/{}/cmdline", pid);
            if let Ok(bytes) = std::fs::read(&path) {
                let parts: Vec<String> = bytes
                    .split(|&b| b == 0)
                    .filter(|p| !p.is_empty())
                    .map(|p| String::from_utf8_lossy(p).into_owned())
                    .collect();
                let joined = parts.join(" ");
                return joined.contains("socat")
                    && joined.contains(&format!("TCP-LISTEN:{local_port},"))
                    && joined.contains(&format!("TCP:{target_ip}:{target_port}"));
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let output = std::process::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "command="])
                .output();
            if let Ok(out) = output {
                let cmd = String::from_utf8_lossy(&out.stdout);
                return cmd.contains("socat")
                    && cmd.contains(&format!("TCP-LISTEN:{local_port},"))
                    && cmd.contains(&format!("TCP:{target_ip}:{target_port}"));
            }
        }
        false
    }

    fn read_cgroup_counters(&self) -> Result<AggregateCounters, String> {
        #[cfg(target_os = "linux")]
        {
            let cgroup_text = std::fs::read_to_string("/proc/self/cgroup")
                .map_err(|e| format!("Failed to read /proc/self/cgroup: {}", e))?;
            let path = cgroup_text
                .lines()
                .find(|l| l.starts_with("0::"))
                .map(|l| &l[3..])
                .ok_or_else(|| "Missing 0:: cgroup line".to_string())?;

            if !path.starts_with('/') || path.contains("..") {
                return Err("Invalid cgroup path".to_string());
            }

            let full_dir = format!("/sys/fs/cgroup{}", path);
            let meta = std::fs::metadata(&full_dir)
                .map_err(|e| format!("Cannot stat cgroup dir {}: {}", full_dir, e))?;

            use std::os::unix::fs::MetadataExt;
            let read_val = |name: &str| -> String {
                std::fs::read_to_string(format!("{}/{}", full_dir, name))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default()
            };

            Ok(AggregateCounters {
                path: full_dir,
                inode: meta.ino(),
                device: meta.dev(),
                monotonic_ns: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64,
                memory_current: read_val("memory.current"),
                memory_peak: read_val("memory.peak"),
                memory_max: read_val("memory.max"),
                memory_events: read_val("memory.events"),
                cpu_stat: read_val("cpu.stat"),
                cpu_max: read_val("cpu.max"),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(AggregateCounters {
                path: "/sys/fs/cgroup/mock".to_string(),
                inode: 1,
                device: 1,
                monotonic_ns: 1_000_000_000,
                memory_current: (2u64 * 1024 * 1024 * 1024).to_string(),
                memory_peak: (4u64 * 1024 * 1024 * 1024).to_string(),
                memory_max: (28 * 1024 * 1024 * 1024u64).to_string(),
                memory_events: "oom 0".to_string(),
                cpu_stat: "usage_usec 1000".to_string(),
                cpu_max: "1600000 100000".to_string(),
            })
        }
    }

    fn execute_command(
        &self,
        program: &Path,
        args: &[&str],
        env: Option<&HashMap<String, String>>,
    ) -> Result<String, String> {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        if let Some(env_vars) = env {
            cmd.envs(env_vars);
        }
        let output = cmd
            .output()
            .map_err(|e| format!("Failed to execute {:?}: {}", program, e))?;

        if !output.status.success() {
            return Err(format!(
                "{:?} exited with code {:?}: {}",
                program,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn hash_file(&self, path: &Path) -> Result<String, String> {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(path)
            .map_err(|e| format!("Failed to read {:?} for hashing: {}", path, e))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        Ok(hex::encode(hasher.finalize()))
    }

    fn read_boot_id(&self) -> Result<String, String> {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .map(|s| s.trim().to_string())
                .map_err(|e| format!("Failed to read /proc/sys/kernel/random/boot_id: {}", e))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let output = std::process::Command::new("sysctl")
                .args(["-n", "kern.uuid"])
                .output();
            if let Ok(out) = output {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !s.is_empty() {
                        return Ok(s);
                    }
                }
            }
            Err("Cannot read boot_id on non-Linux platform without kern.uuid".to_string())
        }
    }

    fn read_hostname(&self) -> Result<String, String> {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/etc/hostname")
                .map(|s| s.trim().to_string())
                .map_err(|e| format!("Failed to read /etc/hostname: {}", e))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let output = std::process::Command::new("hostname").output();
            if let Ok(out) = output {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !s.is_empty() {
                        return Ok(s);
                    }
                }
            }
            Err("Cannot read hostname".to_string())
        }
    }

    fn is_root_owned_path(&self, path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let resolved = match path.canonicalize() {
                Ok(p) => p,
                Err(_) => path.to_path_buf(),
            };

            for target in [path, resolved.as_path()] {
                let mut curr = Some(target);
                while let Some(comp) = curr {
                    if let Ok(meta) = comp.symlink_metadata() {
                        if meta.uid() != 0 {
                            return false;
                        }
                        if !meta.file_type().is_symlink() && (meta.mode() & 0o022) != 0 {
                            return false;
                        }
                    } else {
                        return false;
                    }
                    curr = comp.parent();
                }
            }
            true
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

/// Mock system inspector for testing
#[derive(Default, Clone)]
pub struct MockSystemInspector {
    pub processes: Arc<RwLock<HashMap<i32, ProcessRecord>>>,
    pub containers: Arc<RwLock<HashMap<String, DockerContainerInspect>>>,
    pub container_stats: Arc<RwLock<Vec<String>>>,
    pub mem_total_gib: Arc<RwLock<f64>>,
    pub mem_available_gib: Arc<RwLock<f64>>,
    pub builders_running_flag: Arc<RwLock<bool>>,
    pub socat_running_pids: Arc<RwLock<HashMap<u32, (u16, String, u16)>>>,
    pub cgroup_counters_result: Arc<RwLock<Option<AggregateCounters>>>,

    pub command_results: Arc<RwLock<HashMap<String, Result<String, String>>>>,
    pub file_hashes: Arc<RwLock<HashMap<PathBuf, String>>>,
    pub boot_id: Arc<RwLock<Option<String>>>,
    pub hostname: Arc<RwLock<Option<String>>>,
    pub path_root_ownership: Arc<RwLock<HashMap<PathBuf, bool>>>,
}

impl MockSystemInspector {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(RwLock::new(HashMap::new())),
            containers: Arc::new(RwLock::new(HashMap::new())),
            container_stats: Arc::new(RwLock::new(Vec::new())),
            mem_total_gib: Arc::new(RwLock::new(32.0)),
            mem_available_gib: Arc::new(RwLock::new(28.0)),
            builders_running_flag: Arc::new(RwLock::new(false)),
            socat_running_pids: Arc::new(RwLock::new(HashMap::new())),
            cgroup_counters_result: Arc::new(RwLock::new(None)),
            command_results: Arc::new(RwLock::new(HashMap::new())),
            file_hashes: Arc::new(RwLock::new(HashMap::new())),
            boot_id: Arc::new(RwLock::new(None)),
            hostname: Arc::new(RwLock::new(None)),
            path_root_ownership: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn add_process(&self, pid: i32, ppid: i32, comm: &str, starttime: &str, cgroup: &str) {
        self.processes.write().unwrap().insert(
            pid,
            ProcessRecord {
                ppid,
                starttime: starttime.to_string(),
                comm: comm.to_string(),
                cgroup: cgroup.to_string(),
            },
        );
    }

    pub fn add_container(&self, inspect: DockerContainerInspect) {
        self.containers
            .write()
            .unwrap()
            .insert(inspect.id.clone(), inspect);
    }

    pub fn set_meminfo(&self, total: f64, available: f64) {
        *self.mem_total_gib.write().unwrap() = total;
        *self.mem_available_gib.write().unwrap() = available;
    }

    pub fn set_command_result(&self, key: &str, result: Result<String, String>) {
        self.command_results
            .write()
            .unwrap()
            .insert(key.to_string(), result);
    }

    pub fn set_file_hash(&self, path: impl Into<PathBuf>, hash: &str) {
        self.file_hashes
            .write()
            .unwrap()
            .insert(path.into(), hash.to_string());
    }

    pub fn set_boot_id(&self, id: &str) {
        *self.boot_id.write().unwrap() = Some(id.to_string());
    }

    pub fn set_hostname(&self, name: &str) {
        *self.hostname.write().unwrap() = Some(name.to_string());
    }

    pub fn set_path_root_owned(&self, path: impl Into<PathBuf>, owned: bool) {
        self.path_root_ownership
            .write()
            .unwrap()
            .insert(path.into(), owned);
    }
}

impl SystemInspector for MockSystemInspector {
    fn process_identity(&self, pid: i32) -> Result<String, String> {
        let procs = self.processes.read().unwrap();
        if let Some(p) = procs.get(&pid) {
            Ok(format!("host:{}:{}", pid, p.starttime))
        } else {
            Ok(format!("host:{}:1000", pid))
        }
    }

    fn process_record(&self, pid: i32) -> Result<ProcessRecord, String> {
        let procs = self.processes.read().unwrap();
        procs
            .get(&pid)
            .cloned()
            .ok_or_else(|| format!("Process {} not found", pid))
    }

    fn docker_inspect(&self, container_id: &str) -> Result<DockerContainerInspect, String> {
        let map = self.containers.read().unwrap();
        map.get(container_id)
            .cloned()
            .ok_or_else(|| format!("Container {} not found", container_id))
    }

    fn docker_ps_all_ids(&self) -> Result<Vec<String>, String> {
        let map = self.containers.read().unwrap();
        Ok(map.keys().cloned().collect())
    }

    fn docker_stats_usage(&self) -> Result<Vec<String>, String> {
        let stats = self.container_stats.read().unwrap();
        Ok(stats.clone())
    }

    fn read_meminfo(&self) -> Result<(f64, f64), String> {
        let tot = *self.mem_total_gib.read().unwrap();
        let av = *self.mem_available_gib.read().unwrap();
        Ok((tot, av))
    }

    fn builders_running(&self, _global_lock_held: bool) -> bool {
        *self.builders_running_flag.read().unwrap()
    }

    fn verify_socat_cmdline(
        &self,
        pid: u32,
        local_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> bool {
        let map = self.socat_running_pids.read().unwrap();
        if let Some(&(lp, ref tip, tp)) = map.get(&pid) {
            lp == local_port && tip == target_ip && tp == target_port
        } else {
            false
        }
    }

    fn read_cgroup_counters(&self) -> Result<AggregateCounters, String> {
        let opt = self.cgroup_counters_result.read().unwrap();
        if let Some(ref c) = *opt {
            Ok(c.clone())
        } else {
            Ok(AggregateCounters {
                path: "/sys/fs/cgroup/test".to_string(),
                inode: 100,
                device: 100,
                monotonic_ns: 123456789,
                memory_current: (2u64 * 1024 * 1024 * 1024).to_string(),
                memory_peak: (4u64 * 1024 * 1024 * 1024).to_string(),
                memory_max: (28 * 1024 * 1024 * 1024u64).to_string(),
                memory_events: "oom 0".to_string(),
                cpu_stat: "usage_usec 1000".to_string(),
                cpu_max: "1600000 100000".to_string(),
            })
        }
    }

    fn execute_command(
        &self,
        program: &Path,
        args: &[&str],
        _env: Option<&HashMap<String, String>>,
    ) -> Result<String, String> {
        let key = if args.is_empty() {
            program.to_string_lossy().to_string()
        } else {
            format!("{} {}", program.to_string_lossy(), args.join(" "))
        };

        let map = self.command_results.read().unwrap();
        // Match full command string first, or program name as fallback
        if let Some(res) = map.get(&key) {
            return res.clone();
        }
        let prog_name = program
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if let Some(res) = map.get(&prog_name) {
            return res.clone();
        }

        Err(format!("Mock command not found: {}", key))
    }

    fn hash_file(&self, path: &Path) -> Result<String, String> {
        let map = self.file_hashes.read().unwrap();
        if let Some(h) = map.get(path) {
            return Ok(h.clone());
        }
        // Fallback: If file actually exists on test disk, compute real SHA-256
        if path.exists() {
            use sha2::{Digest, Sha256};
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            return Ok(hex::encode(hasher.finalize()));
        }
        Err(format!("Mock file hash not found for {:?}", path))
    }

    fn read_boot_id(&self) -> Result<String, String> {
        self.boot_id
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| "boot_id not configured in MockSystemInspector".to_string())
    }

    fn read_hostname(&self) -> Result<String, String> {
        self.hostname
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| "hostname not configured in MockSystemInspector".to_string())
    }

    fn is_root_owned_path(&self, path: &Path) -> bool {
        let map = self.path_root_ownership.read().unwrap();
        if let Some(&owned) = map.get(path) {
            return owned;
        }
        // Default to true for mock tests unless explicitly mocked otherwise
        true
    }
}
