use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

use crate::admission::types::{AdmissionConfig, AdmissionError, AdmissionKind};
use crate::platform::SystemInspector;

pub fn runner_cgroup(cgroup: &str) -> Option<String> {
    let re = Regex::new(r"(?:docker[-/])([a-f0-9]{64})(?:\.scope)?").ok()?;
    let mut ids = HashSet::new();
    for line in cgroup.lines() {
        for caps in re.captures_iter(line) {
            if let Some(m) = caps.get(1) {
                ids.insert(m.as_str().to_string());
            }
        }
    }
    if ids.len() == 1 {
        ids.into_iter().next()
    } else {
        None
    }
}

pub fn worker_owner(
    mut pid: i32,
    container_id: &str,
    container_started: &str,
    sys: &dyn SystemInspector,
) -> Result<String, AdmissionError> {
    let mut seen = HashSet::new();
    for _ in 0..32 {
        if pid <= 1 || seen.contains(&pid) {
            break;
        }
        seen.insert(pid);

        let rec = sys.process_record(pid).map_err(|_| {
            AdmissionError::OrphanRunnerHook("Runner hook lost its Worker ancestry".to_string())
        })?;

        let cg = runner_cgroup(&rec.cgroup);
        if cg.as_deref() != Some(container_id) {
            return Err(AdmissionError::OrphanRunnerHook(
                "Runner hook left its container".to_string(),
            ));
        }

        if rec.comm == "Runner.Worker" {
            return Ok(format!(
                "runner:{}:{}:{}:{}",
                container_id, container_started, pid, rec.starttime
            ));
        }

        pid = rec.ppid;
    }

    Err(AdmissionError::OrphanRunnerHook(
        "Runner hook has no live Worker ancestor".to_string(),
    ))
}

pub fn peer_owner(
    pid: i32,
    uid: u32,
    config: &AdmissionConfig,
    sys: &dyn SystemInspector,
) -> Result<(String, AdmissionKind), AdmissionError> {
    if uid == 0 {
        let identity = sys
            .process_identity(pid)
            .map_err(|e| AdmissionError::System(format!("Process identity error: {e}")))?;
        return Ok((identity, AdmissionKind::Builder));
    }

    if uid != 1001 {
        return Err(AdmissionError::UnauthorizedPeer(uid));
    }

    let rec = sys
        .process_record(pid)
        .map_err(|e| AdmissionError::System(format!("Process record error: {e}")))?;

    let container_id = runner_cgroup(&rec.cgroup).ok_or_else(|| {
        AdmissionError::InvalidBudget("Runner must belong to a Docker cgroup".to_string())
    })?;

    let inspect = sys
        .docker_inspect(&container_id)
        .map_err(|e| AdmissionError::System(format!("Docker inspect error: {e}")))?;

    if !inspect.running {
        return Err(AdmissionError::InvalidBudget(
            "Container is not running".to_string(),
        ));
    }

    let clean_name = inspect.name.trim_start_matches('/');
    if !config.runner_names.is_empty() && !config.runner_names.iter().any(|n| n == clean_name) {
        return Err(AdmissionError::InvalidBudget(
            "Container is outside this runner deployment".to_string(),
        ));
    }

    let owner = worker_owner(pid, &container_id, &inspect.started_at, sys)?;
    Ok((owner, AdmissionKind::Runner))
}

pub fn legacy_owner_for_worker(owner: &str) -> Result<String, AdmissionError> {
    let re = Regex::new(r"^runner:([a-f0-9]{64}):(.+):([0-9]+):([0-9]+)$")
        .map_err(|e| AdmissionError::System(e.to_string()))?;
    let caps = re.captures(owner).ok_or_else(|| {
        AdmissionError::InvalidBudget(format!("Invalid Worker lease identity: '{owner}'"))
    })?;
    Ok(format!("container:{}:{}", &caps[1], &caps[2]))
}

static RUNNER_RE: OnceLock<Regex> = OnceLock::new();

pub fn owner_alive(owner: &str, sys: &dyn SystemInspector) -> bool {
    if owner.starts_with("runner:") {
        let re = RUNNER_RE.get_or_init(|| {
            Regex::new(r"^runner:([a-f0-9]{64}):(.+):([0-9]+):([0-9]+)$")
                .expect("Valid runner owner regex")
        });

        let caps = match re.captures(owner) {
            Some(c) => c,
            None => return false,
        };

        let cid = &caps[1];
        let started = &caps[2];
        let worker_pid: i32 = match caps[3].parse() {
            Ok(p) => p,
            Err(_) => return false,
        };
        let worker_start = &caps[4];

        let rec = match sys.process_record(worker_pid) {
            Ok(r) => r,
            Err(_) => return false,
        };

        if rec.comm != "Runner.Worker" || rec.starttime != worker_start {
            return false;
        }

        if runner_cgroup(&rec.cgroup).as_deref() != Some(cid) {
            return false;
        }

        match sys.docker_inspect(cid) {
            Ok(inspect) => inspect.running && inspect.started_at == started,
            Err(_) => false,
        }
    } else if owner.starts_with("host:") {
        let parts: Vec<&str> = owner.splitn(3, ':').collect();
        if parts.len() != 3 {
            return false;
        }
        let pid: i32 = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => return false,
        };
        match sys.process_identity(pid) {
            Ok(id) if id == owner => true,
            _ => sys.builders_running(true),
        }
    } else if owner.starts_with("container:") {
        let parts: Vec<&str> = owner.splitn(3, ':').collect();
        if parts.len() != 3 {
            return false;
        }
        let cid = parts[1];
        let started = parts[2];
        match sys.docker_inspect(cid) {
            Ok(inspect) => inspect.running && inspect.started_at == started,
            Err(_) => false,
        }
    } else {
        false
    }
}
