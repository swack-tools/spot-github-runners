use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread::sleep;
use std::time::Duration;

use crate::admission::types::{
    AdmissionAction, AdmissionError, AdmissionKind, AdmissionRequest, AdmissionResponse,
};

pub const DEFAULT_SOCKET_PATH: &str = "/run/gha-admission/socket";

pub fn request(
    action: AdmissionAction,
    kind: AdmissionKind,
    socket_path: Option<&str>,
) -> Result<bool, AdmissionError> {
    let path = socket_path.unwrap_or(DEFAULT_SOCKET_PATH);
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let req = AdmissionRequest { action, kind };
    let json_bytes = serde_json::to_vec(&req)?;
    stream.write_all(&json_bytes)?;

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Err(AdmissionError::Transport(
            "Empty response from server".to_string(),
        ));
    }

    let resp: AdmissionResponse = serde_json::from_slice(&buf[..n])?;
    if let Some(ref err) = resp.error {
        if resp.code.as_deref() == Some("orphaned-runner-hook")
            || err.starts_with("ORPHAN_RUNNER_HOOK:")
        {
            return Err(AdmissionError::OrphanRunnerHook(err.clone()));
        }
        return Err(AdmissionError::Server(err.clone()));
    }

    Ok(resp.accepted)
}

pub fn acquire_build(socket_path: Option<&str>) -> Result<(), i32> {
    match request(
        AdmissionAction::Acquire,
        AdmissionKind::Builder,
        socket_path,
    ) {
        Ok(true) => Ok(()),
        Ok(false) => Err(75),
        Err(_) => Err(75),
    }
}

pub fn release_build(socket_path: Option<&str>) {
    let _ = request(
        AdmissionAction::Release,
        AdmissionKind::Builder,
        socket_path,
    );
}

pub fn acquire_runner(socket_path: Option<&str>) -> Result<(), AdmissionError> {
    loop {
        match request(AdmissionAction::Acquire, AdmissionKind::Runner, socket_path) {
            Ok(true) => return Ok(()),
            Ok(false) => {
                sleep(Duration::from_secs(5));
            }
            Err(AdmissionError::OrphanRunnerHook(msg)) => {
                return Err(AdmissionError::OrphanRunnerHook(msg));
            }
            Err(_) => {
                // Root service restarts or transient socket disconnects: sleep & retry
                sleep(Duration::from_secs(5));
            }
        }
    }
}

pub fn release_runner(socket_path: Option<&str>) -> Result<(), i32> {
    let mut retries = 5;
    while retries > 0 {
        match request(AdmissionAction::Release, AdmissionKind::Runner, socket_path) {
            Ok(_) => return Ok(()),
            Err(AdmissionError::OrphanRunnerHook(_)) => return Err(75),
            Err(_) => {
                retries -= 1;
                sleep(Duration::from_millis(500));
            }
        }
    }
    Ok(())
}
