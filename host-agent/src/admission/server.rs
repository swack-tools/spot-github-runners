use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::admission::peer::{owner_alive, peer_owner};
use crate::admission::pool::LeasePool;
use crate::admission::types::{
    AdmissionAction, AdmissionConfig, AdmissionError, AdmissionKind, AdmissionRequest,
    AdmissionResponse,
};
use crate::platform::{get_peer_credentials, SystemInspector};

pub fn run_server(
    socket_path: &Path,
    config_path: &Path,
    ledger_path: &Path,
    sys: &dyn SystemInspector,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = ledger_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if socket_path.exists() {
        let _ = fs::remove_file(socket_path);
    }

    let listener = UnixListener::bind(socket_path)?;

    // Set permissions 0660 and chown 0:1001 if root
    let c_path = CString::new(socket_path.as_os_str().as_bytes())?;
    unsafe {
        libc::chmod(c_path.as_ptr(), 0o660);
        if libc::geteuid() == 0 {
            libc::chown(c_path.as_ptr(), 0, 1001);
        }
    }

    for stream_res in listener.incoming() {
        let mut stream = match stream_res {
            Ok(s) => s,
            Err(_) => continue,
        };

        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

        let response = match handle_client(&stream, config_path, ledger_path, sys) {
            Ok(accepted) => AdmissionResponse::ok(accepted),
            Err(AdmissionError::OrphanRunnerHook(msg)) => AdmissionResponse::orphan(&msg),
            Err(e) => AdmissionResponse::err(&e.to_string()),
        };

        if let Ok(bytes) = serde_json::to_vec(&response) {
            let _ = stream.write_all(&bytes);
        }
    }

    Ok(())
}

/// Calculates the memory allocation for a runner, checking heavy runner policies
/// against the inspected container name as well as backward-compatible owner substring.
pub fn calculate_runner_memory(
    config: &AdmissionConfig,
    runner_name: Option<&str>,
    owner: &str,
) -> f64 {
    let mut runner_mem = config.runner_gib;
    let r_name = runner_name.unwrap_or("").trim_start_matches('/');
    if let Some(ref hr) = config.heavy_runner {
        let hr_target = hr.name.trim_start_matches('/');
        if (!r_name.is_empty() && r_name == hr_target) || (r_name.is_empty() && owner == hr_target)
        {
            runner_mem = hr.memory_gib as f64;
        }
    }
    if let Some(ref hrs) = config.heavy_runners {
        for hr_name in hrs {
            let hr_target = hr_name.trim_start_matches('/');
            if (!r_name.is_empty() && r_name == hr_target)
                || (r_name.is_empty() && owner == hr_target)
            {
                runner_mem = 32.0;
                break;
            }
        }
    }
    runner_mem
}

fn handle_client(
    mut stream: &std::os::unix::net::UnixStream,
    config_path: &Path,
    ledger_path: &Path,
    sys: &dyn SystemInspector,
) -> Result<bool, AdmissionError> {
    let (pid, uid, _) = get_peer_credentials(stream)?;

    let config: AdmissionConfig = if config_path.exists() {
        let content = fs::read_to_string(config_path)?;
        serde_json::from_str(&content)?
    } else {
        AdmissionConfig {
            role: None,
            work_gib: 28.0,
            runner_gib: 14.0,
            builder_gib: 28.0,
            runner_names: vec![],
            heavy_runner: None,
            heavy_runners: None,
        }
    };

    let (owner, peer_kind, runner_name) = peer_owner(pid, uid, &config, sys)?;

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Err(AdmissionError::Transport("Empty request".to_string()));
    }

    let req: AdmissionRequest = serde_json::from_slice(&buf[..n])?;
    if req.kind != peer_kind {
        return Err(AdmissionError::InvalidBudget(
            "Admission kind does not match peer identity".to_string(),
        ));
    }

    let has_heavy_runner_policy = config.heavy_runner.is_some() || config.heavy_runners.is_some();
    let mut pool = LeasePool::new(
        PathBuf::from(ledger_path),
        config.work_gib,
        has_heavy_runner_policy,
    )?;

    match req.action {
        AdmissionAction::Release => {
            if peer_kind == AdmissionKind::Runner {
                pool.release_runner(&owner, sys)?;
            } else {
                pool.release(&owner, sys)?;
            }
            Ok(true)
        }
        AdmissionAction::Acquire => {
            let amount = if peer_kind == AdmissionKind::Runner {
                calculate_runner_memory(&config, runner_name.as_deref(), &owner)
            } else {
                // Dedicated builder: check draining
                if Path::new("/run/oxidex-build-draining").exists() {
                    return Ok(false);
                }
                config.builder_gib
            };

            let accepted = if peer_kind == AdmissionKind::Runner {
                pool.acquire_runner(&owner, amount, sys)?
            } else {
                pool.acquire(&owner, amount, sys)?
            };

            if accepted && peer_kind == AdmissionKind::Runner && !owner_alive(&owner, sys) {
                // Process died between authentication and ledger commit
                let _ = pool.release_runner(&owner, sys);
                return Ok(false);
            }

            Ok(accepted)
        }
    }
}
