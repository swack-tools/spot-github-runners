use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;

use regex::Regex;
use thiserror::Error;

use crate::admission::client::{acquire_build, release_build};

pub const BUILDER_IMAGE: &str = "oxidex-remote-builder:1.99.0";
pub const DEFAULT_BUILD_ROOT: &str = "/mnt/runner-data/remote-build";

#[derive(Debug, Error)]
pub enum DockerAdmissionError {
    #[error("Remote builder admission requires root")]
    NotRoot,
    #[error("Unknown/conflicting Docker option: {0}")]
    DisallowedOption(String),
    #[error("Repeated Docker policy option: {0}")]
    RepeatedOption(String),
    #[error("Actual Docker cap differs from reserved builder resources: {0}")]
    ResourceCapMismatch(String),
    #[error("Builder privilege/runtime policy differs from launcher: {0}")]
    PolicyMismatch(String),
    #[error("Forbidden volume mount: {0}")]
    ForbiddenMount(String),
    #[error("Builder image differs from reviewed toolchain")]
    ImageMismatch,
    #[error("Contended builder lock or draining (exit 75)")]
    ContendedOrDraining,
    #[error("Invalid project identifier: {0}")]
    InvalidProject(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn validate_project_name(project: &str) -> Result<(), DockerAdmissionError> {
    let re = Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$").unwrap();
    if re.is_match(project) {
        Ok(())
    } else {
        Err(DockerAdmissionError::InvalidProject(project.to_string()))
    }
}

pub fn expected_volumes(root: &str, project: &str) -> Vec<String> {
    vec![
        format!("{root}/sources/{project}:/src"),
        format!("{root}/targets/{project}:/target"),
        format!("{root}/cargo:/cargo"),
    ]
}

pub fn validate_build_policy(
    arguments: &[String],
    expected_cpus: u32,
    expected_mem_gib: u32,
    root: Option<&str>,
) -> Result<String, DockerAdmissionError> {
    if arguments.is_empty() || arguments[0] != "run" {
        return Err(DockerAdmissionError::DisallowedOption(
            "Command must be docker run".to_string(),
        ));
    }

    let root_str = root.unwrap_or(DEFAULT_BUILD_ROOT);

    let allowed: HashSet<&str> = [
        "--user",
        "--cpus",
        "--label",
        "--memory",
        "--memory-swap",
        "--pids-limit",
        "--cap-drop",
        "--security-opt",
        "--entrypoint",
    ]
    .into_iter()
    .collect();

    let mut observed_values: HashMap<String, Vec<String>> = HashMap::new();
    let mut mounts = Vec::new();
    let mut index = 1;

    while index < arguments.len() {
        let word = &arguments[index];
        if word == "--rm" {
            index += 1;
            continue;
        }
        if word == "-v" {
            if index + 1 >= arguments.len() {
                return Err(DockerAdmissionError::ForbiddenMount(
                    "Missing volume spec".to_string(),
                ));
            }
            mounts.push(arguments[index + 1].clone());
            index += 2;
            continue;
        }
        if word.starts_with('-') {
            if word.starts_with("--name")
                || word.starts_with("--label=oxidex.cache-project=")
                || word.starts_with("--label=oxidex.run-id=")
            {
                return Err(DockerAdmissionError::DisallowedOption(
                    "Caller-assigned identity forbidden".to_string(),
                ));
            }
            if let Some((key, val)) = word.split_once('=') {
                if !allowed.contains(key) || val.is_empty() {
                    return Err(DockerAdmissionError::DisallowedOption(word.clone()));
                }
                observed_values
                    .entry(key.to_string())
                    .or_default()
                    .push(val.to_string());
                index += 1;
                continue;
            } else {
                return Err(DockerAdmissionError::DisallowedOption(word.clone()));
            }
        }
        break;
    }

    if index >= arguments.len() || arguments[index] != BUILDER_IMAGE {
        return Err(DockerAdmissionError::ImageMismatch);
    }

    // Verify no repeated flags (except --label)
    for key in allowed.iter() {
        if *key != "--label" && observed_values.get(*key).map_or(0, |v| v.len()) > 1 {
            return Err(DockerAdmissionError::RepeatedOption((*key).to_string()));
        }
    }

    // Verify exact values
    let get_single = |k: &str| {
        observed_values
            .get(k)
            .and_then(|v| v.first().cloned())
            .unwrap_or_default()
    };

    if get_single("--user") != "1001:1001" {
        return Err(DockerAdmissionError::PolicyMismatch(
            "--user must be 1001:1001".to_string(),
        ));
    }
    if get_single("--cpus") != expected_cpus.to_string()
        && get_single("--cpus") != format!("{}.0", expected_cpus)
    {
        return Err(DockerAdmissionError::ResourceCapMismatch(format!(
            "--cpus != {}",
            expected_cpus
        )));
    }
    if get_single("--memory") != format!("{}g", expected_mem_gib) {
        return Err(DockerAdmissionError::ResourceCapMismatch(format!(
            "--memory != {}g",
            expected_mem_gib
        )));
    }
    if get_single("--memory-swap") != format!("{}g", expected_mem_gib) {
        return Err(DockerAdmissionError::ResourceCapMismatch(format!(
            "--memory-swap != {}g",
            expected_mem_gib
        )));
    }
    if get_single("--pids-limit") != "4096" {
        return Err(DockerAdmissionError::PolicyMismatch(
            "--pids-limit must be 4096".to_string(),
        ));
    }
    if get_single("--cap-drop") != "ALL" {
        return Err(DockerAdmissionError::PolicyMismatch(
            "--cap-drop must be ALL".to_string(),
        ));
    }
    if get_single("--security-opt") != "no-new-privileges" {
        return Err(DockerAdmissionError::PolicyMismatch(
            "--security-opt must be no-new-privileges".to_string(),
        ));
    }
    if get_single("--entrypoint") != "/usr/bin/time" {
        return Err(DockerAdmissionError::PolicyMismatch(
            "--entrypoint must be /usr/bin/time".to_string(),
        ));
    }
    let labels = observed_values.get("--label").cloned().unwrap_or_default();
    if !labels.contains(&"oxidex.remote-build=true".to_string()) {
        return Err(DockerAdmissionError::PolicyMismatch(
            "Missing --label=oxidex.remote-build=true".to_string(),
        ));
    }

    // Verify volume mounts strictly
    for mount in &mounts {
        if mount.contains("/var/run/docker.sock") || mount.ends_with(".sock") {
            return Err(DockerAdmissionError::ForbiddenMount(
                "docker.sock mount forbidden".to_string(),
            ));
        }
    }

    let target_mount = mounts
        .iter()
        .find(|m| m.ends_with(":/target"))
        .ok_or_else(|| {
            DockerAdmissionError::ForbiddenMount("Missing :/target mount".to_string())
        })?;

    let expected_prefix = format!("{root_str}/targets/");
    if !target_mount.starts_with(&expected_prefix) {
        return Err(DockerAdmissionError::ForbiddenMount(format!(
            "Unapproved builder cache path: {target_mount}"
        )));
    }

    let project = target_mount
        .trim_start_matches(&expected_prefix)
        .trim_end_matches(":/target")
        .to_string();

    validate_project_name(&project)?;

    let mut sorted_mounts = mounts.clone();
    sorted_mounts.sort();
    let mut expected_vols = expected_volumes(root_str, &project);
    expected_vols.sort();

    if sorted_mounts != expected_vols {
        return Err(DockerAdmissionError::ForbiddenMount(
            "Builder mounts differ from fixed cache/source boundaries".to_string(),
        ));
    }

    Ok(project)
}

#[derive(Debug)]
pub struct BuildLocks {
    _global: std::fs::File,
    _active: std::fs::File,
}

pub fn acquire_build_locks(project: &str, lock_root: &Path) -> Result<BuildLocks, i32> {
    if lock_root.join("oxidex-build-draining").exists() {
        return Err(75);
    }

    fs::create_dir_all(lock_root).map_err(|_| 75)?;

    let global_path = lock_root.join("oxidex-build.lock");
    let active_path = lock_root.join(format!("oxidex-cache-active-{project}.lock"));

    let global_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&global_path)
        .map_err(|_| 75)?;

    let active_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&active_path)
        .map_err(|_| 75)?;

    // Shared lock on global_lock, exclusive lock on active cache
    let g_res = unsafe { libc::flock(global_file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if g_res != 0 {
        return Err(75);
    }

    let a_res = unsafe { libc::flock(active_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if a_res != 0 {
        return Err(75);
    }

    Ok(BuildLocks {
        _global: global_file,
        _active: active_file,
    })
}

pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    // If not a dedicated build command, pass through to vendor docker
    let is_dedicated_build = args.iter().any(|a| a == "--label=oxidex.remote-build=true");
    if !is_dedicated_build {
        let status = Command::new("/usr/bin/docker").args(args).status()?;
        return Ok(status.code().unwrap_or(0));
    }

    // Must be root
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("Remote builder admission requires root");
        return Ok(1);
    }

    let project = match validate_build_policy(args, 16, 28, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Docker admission error: {e}");
            return Ok(1);
        }
    };

    let lock_root = Path::new("/run");
    let _locks = match acquire_build_locks(&project, lock_root) {
        Ok(l) => l,
        Err(code) => return Ok(code),
    };

    if let Err(code) = acquire_build(None) {
        return Ok(code);
    }

    struct LeaseGuard;
    impl Drop for LeaseGuard {
        fn drop(&mut self) {
            release_build(None);
        }
    }
    let _lease_guard = LeaseGuard;

    let status = Command::new("/usr/bin/docker").args(args).status()?;
    Ok(status.code().unwrap_or(0))
}
