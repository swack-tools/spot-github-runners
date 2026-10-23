use std::collections::HashSet;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;
use thiserror::Error;

use crate::clean_target::{empty_directory_fd, open_dir_nofollow};
use crate::docker_wrapper::real_docker_binary;

pub const DEFAULT_ROOT: &str = "/mnt/runner-data/remote-build";
pub const ALLOWED_COMMANDS: &[&str] = &[
    "cargo",
    "rustc",
    "rustfmt",
    "clippy-driver",
    "just",
    "uv",
    "python3",
    "cbindgen",
    "oxidex-toolchain-probe",
    "example-rust-project-toolchain-probe",
    "toolchain-probe",
];

#[derive(Debug, Error)]
pub enum HostLauncherError {
    #[error("Project must be a simple identifier, never a path: {0}")]
    InvalidProject(String),
    #[error("Unreviewed builder image: {0}")]
    UnreviewedImage(String),
    #[error("Unsupported build command: {0}")]
    UnsupportedCommand(String),
    #[error("Resource limits must be positive")]
    InvalidLimits,
    #[error("Symlinked builder paths are forbidden: {0}")]
    SymlinkedPath(String),
    #[error("Source cleanup depth bound")]
    DepthBoundExceeded,
    #[error("Source cleanup entry bound")]
    EntryBoundExceeded,
    #[error("Foreign filesystem inside source")]
    ForeignFilesystem,
    #[error("Contended cache lock")]
    ContendedLock,
    #[error("Source or target still mounted in running container: {0}")]
    SourceMounted(String),
    #[error("Malformed Docker inventory: {0}")]
    MalformedInventory(String),
    #[error("Invalid or unapproved uploader identity: {0}")]
    InvalidUploader(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn validate_project(project: &str) -> Result<&str, HostLauncherError> {
    let re = Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$").unwrap();
    if re.is_match(project) {
        Ok(project)
    } else {
        Err(HostLauncherError::InvalidProject(project.to_string()))
    }
}

pub fn checked_path(path: &Path) -> Result<(), HostLauncherError> {
    if let Ok(canon) = path.canonicalize() {
        if canon != path {
            return Err(HostLauncherError::SymlinkedPath(
                path.to_string_lossy().to_string(),
            ));
        }
    }
    let mut current = Some(path);
    while let Some(p) = current {
        if let Ok(meta) = fs::symlink_metadata(p) {
            if meta.file_type().is_symlink() {
                return Err(HostLauncherError::SymlinkedPath(
                    p.to_string_lossy().to_string(),
                ));
            }
        }
        current = p.parent();
    }
    Ok(())
}

pub fn cleanup_source(source: &Path) -> Result<(), HostLauncherError> {
    let c_source = CString::new(source.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let root_fd = open_dir_nofollow(None, &c_source)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let res = unsafe { libc::fstat(root_fd.as_raw_fd(), &mut st) };
    if res != 0 {
        return Err(HostLauncherError::Io(std::io::Error::last_os_error()));
    }

    let dev = st.st_dev;
    let mut count = 0;
    empty_directory_fd(root_fd.as_raw_fd(), dev, 0, &mut count).map_err(|e| match e {
        crate::clean_target::CleanTargetError::DepthBoundExceeded => {
            HostLauncherError::DepthBoundExceeded
        }
        crate::clean_target::CleanTargetError::EntryBoundExceeded => {
            HostLauncherError::EntryBoundExceeded
        }
        crate::clean_target::CleanTargetError::ForeignFilesystem => {
            HostLauncherError::ForeignFilesystem
        }
        crate::clean_target::CleanTargetError::Io(io_err) => HostLauncherError::Io(io_err),
        _ => HostLauncherError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            e.to_string(),
        )),
    })?;

    unsafe {
        libc::fsync(root_fd.as_raw_fd());
    }

    Ok(())
}

pub fn verify_mounts_not_active(
    source: &Path,
    target: &Path,
    containers_json: &str,
) -> Result<(), HostLauncherError> {
    let rows: serde_json::Value = serde_json::from_str(containers_json)
        .map_err(|e| HostLauncherError::MalformedInventory(e.to_string()))?;
    let arr = rows
        .as_array()
        .ok_or_else(|| HostLauncherError::MalformedInventory("Expected JSON array".to_string()))?;

    let src_str = source.to_string_lossy();
    let tgt_str = target.to_string_lossy();

    for row in arr {
        if let Some(mounts) = row.get("Mounts").and_then(|m| m.as_array()) {
            for mount in mounts {
                if let Some(m_src) = mount.get("Source").and_then(|s| s.as_str()) {
                    if m_src == src_str || m_src == tgt_str {
                        return Err(HostLauncherError::SourceMounted(format!(
                            "Source or target is mounted in container: {m_src}"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn source_not_mounted(source: &Path, target: &Path) -> Result<(), HostLauncherError> {
    let docker = real_docker_binary();
    let output = match Command::new(&docker)
        .args(["ps", "-aq", "--no-trunc"])
        .output()
    {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(HostLauncherError::Io(e)),
    };

    if !output.status.success() {
        return Err(HostLauncherError::MalformedInventory(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let ids: Vec<&str> = text.split_whitespace().collect();
    if ids.is_empty() {
        return Ok(());
    }

    let hex_re = Regex::new(r"^[0-9a-f]{64}$").unwrap();
    let mut id_set = HashSet::new();
    for id in &ids {
        if !hex_re.is_match(id) {
            return Err(HostLauncherError::MalformedInventory(format!(
                "Malformed container ID: {id}"
            )));
        }
        if !id_set.insert(id) {
            return Err(HostLauncherError::MalformedInventory(format!(
                "Duplicate container ID: {id}"
            )));
        }
    }

    let inspect_output = Command::new(&docker).arg("inspect").args(&ids).output()?;

    if !inspect_output.status.success() {
        return Err(HostLauncherError::MalformedInventory(
            String::from_utf8_lossy(&inspect_output.stderr).to_string(),
        ));
    }

    let inspect_text = String::from_utf8_lossy(&inspect_output.stdout);
    verify_mounts_not_active(source, target, &inspect_text)
}

pub const TRUSTED_BIN_DIRS: &[&str] = &[
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/opt/bin",
    "/opt/rust/bin",
    "/usr/local/cargo/bin",
];

pub fn is_command_allowed(cmd: &str) -> bool {
    let path = Path::new(cmd);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let parent_str = parent.to_string_lossy();
            if !TRUSTED_BIN_DIRS.iter().any(|&dir| parent_str == dir) {
                return false;
            }
        }
    }
    let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or(cmd);
    ALLOWED_COMMANDS.contains(&file_name)
}

pub fn resolve_uploader_uid(explicit_uid: Option<u32>) -> Result<u32, HostLauncherError> {
    let config_path = std::env::var("OXIDEX_UPLOADER_CONFIG")
        .unwrap_or_else(|_| "/etc/oxidex-builder-uploader.json".to_string());
    let approved_uid: Option<u32> = if Path::new(&config_path).exists() {
        let content = fs::read_to_string(&config_path)?;
        let val: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
            HostLauncherError::InvalidUploader(format!("malformed uploader config: {e}"))
        })?;
        val.get("uid").and_then(|u| u.as_u64()).map(|u| u as u32)
    } else {
        None
    };

    let sudo_uid: Option<u32> = std::env::var("SUDO_UID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok());

    if let (Some(exp), Some(sudo)) = (explicit_uid, sudo_uid) {
        if exp != sudo {
            return Err(HostLauncherError::InvalidUploader(format!(
                "explicit uid {exp} does not match caller SUDO_UID {sudo}"
            )));
        }
    }
    if let (Some(sudo), Some(app)) = (sudo_uid, approved_uid) {
        if sudo != app {
            return Err(HostLauncherError::InvalidUploader(format!(
                "caller SUDO_UID {sudo} does not match approved uploader uid {app}"
            )));
        }
    }
    if let (Some(exp), Some(app)) = (explicit_uid, approved_uid) {
        if exp != app {
            return Err(HostLauncherError::InvalidUploader(format!(
                "explicit uid {exp} does not match approved uploader uid {app}"
            )));
        }
    }

    let target_uid = match explicit_uid.or(sudo_uid).or(approved_uid) {
        Some(uid) => uid,
        None => {
            if std::env::var("OXIDEX_ALLOW_UNPRIVILEGED").is_ok() {
                let uid = unsafe { libc::getuid() };
                if uid != 0 {
                    uid
                } else {
                    1001
                }
            } else {
                return Err(HostLauncherError::InvalidUploader(
                    "cannot derive uploader UID: SUDO_UID unset and /etc/oxidex-builder-uploader.json missing".to_string(),
                ));
            }
        }
    };

    if target_uid == 0 {
        return Err(HostLauncherError::InvalidUploader(
            "uploader UID cannot be root (0)".to_string(),
        ));
    }
    if target_uid == 1001 && std::env::var("OXIDEX_ALLOW_UNPRIVILEGED").is_err() {
        return Err(HostLauncherError::InvalidUploader(
            "uploader UID collides with container build UID 1001".to_string(),
        ));
    }

    Ok(target_uid)
}

pub fn container_command(
    project: &str,
    command: &[String],
    cpus: u32,
    memory_gib: u32,
    image: Option<&str>,
    root: Option<&str>,
) -> Result<Vec<String>, HostLauncherError> {
    validate_project(project)?;
    let img = image.unwrap_or("oxidex-remote-builder:1.99.0");
    if img != "oxidex-remote-builder:1.97.1"
        && img != "oxidex-remote-builder:1.99.0"
        && img != "example-rust-project-remote-builder:1.99.0"
    {
        return Err(HostLauncherError::UnreviewedImage(img.to_string()));
    }
    if command.is_empty() || !is_command_allowed(&command[0]) {
        return Err(HostLauncherError::UnsupportedCommand(
            command.first().cloned().unwrap_or_default(),
        ));
    }
    if cpus < 1 || memory_gib < 1 {
        return Err(HostLauncherError::InvalidLimits);
    }
    let r = root.unwrap_or(DEFAULT_ROOT);

    let mut args = vec![
        "docker".to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "--user=1001:1001".to_string(),
        format!("--cpus={cpus}"),
        "--label=oxidex.remote-build=true".to_string(),
        format!("--memory={memory_gib}g"),
        format!("--memory-swap={memory_gib}g"),
        "--pids-limit=4096".to_string(),
        "--cap-drop=ALL".to_string(),
        "--security-opt=no-new-privileges".to_string(),
        "--entrypoint=/usr/bin/time".to_string(),
        "-v".to_string(),
        format!("{r}/sources/{project}:/src"),
        "-v".to_string(),
        format!("{r}/targets/{project}:/target"),
        "-v".to_string(),
        format!("{r}/cargo:/cargo"),
        img.to_string(),
        "-v".to_string(),
        "/usr/local/bin/oxidex-build-entrypoint".to_string(),
    ];
    args.extend_from_slice(command);
    Ok(args)
}

pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    if args.is_empty() {
        eprintln!("host-launcher: expected subcommand (prepare, cleanup, build)");
        return Ok(1);
    }

    if unsafe { libc::geteuid() } != 0
        && std::env::var("OXIDEX_ALLOW_UNPRIVILEGED").is_err()
        && std::env::var("EXAMPLE_RUST_PROJECT_ALLOW_UNPRIVILEGED").is_err()
    {
        eprintln!("host-launcher: invoke this root-owned launcher with sudo");
        return Ok(1);
    }

    let root_str = std::env::var("OXIDEX_BUILD_ROOT")
        .or_else(|_| std::env::var("EXAMPLE_RUST_PROJECT_BUILD_ROOT"))
        .unwrap_or_else(|_| DEFAULT_ROOT.to_string());
    let root = PathBuf::from(root_str);
    let lock_root = PathBuf::from(
        std::env::var("OXIDEX_LOCK_ROOT")
            .or_else(|_| std::env::var("EXAMPLE_RUST_PROJECT_LOCK_ROOT"))
            .unwrap_or_else(|_| "/run".to_string()),
    );
    let is_draining = |lr: &Path| {
        lr.join("oxidex-build-draining").exists()
            || lr.join("example-rust-project-build-draining").exists()
    };

    enum LauncherAction<'a> {
        Prepare {
            project: &'a str,
            explicit_uid: Option<u32>,
        },
        Cleanup {
            project: &'a str,
        },
        Build {
            project: &'a str,
            cmd: Vec<String>,
        },
    }

    let action = if args[0] == "prepare" {
        if args.len() < 2 {
            eprintln!("Usage: host-launcher prepare <project> [uploader_uid]");
            return Ok(1);
        }
        LauncherAction::Prepare {
            project: args[1].as_str(),
            explicit_uid: args.get(2).and_then(|s| s.parse().ok()),
        }
    } else if args.len() >= 2 && args[1] == "prepare" {
        LauncherAction::Prepare {
            project: args[0].as_str(),
            explicit_uid: args.get(2).and_then(|s| s.parse().ok()),
        }
    } else if args[0] == "cleanup" {
        if args.len() < 2 {
            eprintln!("Usage: host-launcher cleanup <project>");
            return Ok(1);
        }
        LauncherAction::Cleanup {
            project: args[1].as_str(),
        }
    } else if args.len() >= 2 && args[1] == "cleanup" {
        LauncherAction::Cleanup {
            project: args[0].as_str(),
        }
    } else {
        let cmd = if args.len() > 1 {
            args[1..].to_vec()
        } else {
            vec![
                "cargo".to_string(),
                "build".to_string(),
                "--release".to_string(),
                "--locked".to_string(),
                "--bin".to_string(),
                "oxidex".to_string(),
            ]
        };
        LauncherAction::Build {
            project: args[0].as_str(),
            cmd,
        }
    };

    match action {
        LauncherAction::Prepare {
            project,
            explicit_uid,
        } => {
            validate_project(project)?;
            if is_draining(&lock_root) {
                eprintln!("Builder host is draining");
                return Ok(75);
            }

            let uploader_uid = resolve_uploader_uid(explicit_uid)?;

            let source = root.join("sources").join(project);
            let target = root.join("targets").join(project);
            let cargo = root.join("cargo");

            // Verify no pre-existing symlinks in ancestors or targets
            checked_path(&source)?;
            checked_path(&target)?;
            checked_path(&cargo)?;

            fs::create_dir_all(&source)?;
            fs::create_dir_all(&target)?;
            fs::create_dir_all(&cargo)?;

            // Recheck after creation
            checked_path(&source)?;
            checked_path(&target)?;
            checked_path(&cargo)?;

            // Apply 02770 and uploader ownership on source using O_NOFOLLOW
            let c_source = CString::new(source.as_os_str().as_bytes())?;
            let fd = unsafe {
                libc::open(
                    c_source.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                return Err(Box::new(HostLauncherError::Io(
                    std::io::Error::last_os_error(),
                )));
            }

            let chmod_res = unsafe { libc::fchmod(fd, 0o2770) };
            if chmod_res != 0 {
                unsafe {
                    libc::close(fd);
                }
                return Err(Box::new(HostLauncherError::Io(
                    std::io::Error::last_os_error(),
                )));
            }

            if unsafe { libc::geteuid() } == 0 {
                let chown_res = unsafe { libc::fchown(fd, uploader_uid, 1001) };
                if chown_res != 0 {
                    unsafe {
                        libc::close(fd);
                    }
                    return Err(Box::new(HostLauncherError::Io(
                        std::io::Error::last_os_error(),
                    )));
                }
            }
            unsafe {
                libc::close(fd);
            }

            // Ensure targets and cargo are owned by 1001:1001
            for p in [&target, &cargo] {
                let c_p = CString::new(p.as_os_str().as_bytes())?;
                let p_fd = unsafe {
                    libc::open(
                        c_p.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    )
                };
                if p_fd >= 0 {
                    if unsafe { libc::geteuid() } == 0 {
                        unsafe {
                            libc::fchown(p_fd, 1001, 1001);
                        }
                    }
                    unsafe {
                        libc::close(p_fd);
                    }
                }
            }

            println!("{}", source.display());
            Ok(0)
        }
        LauncherAction::Cleanup { project } => {
            validate_project(project)?;

            fs::create_dir_all(&lock_root)?;
            let lock_path = lock_root.join(format!("oxidex-cache-active-{project}.lock"));
            let lock_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o600)
                .open(&lock_path)?;

            let lock_res =
                unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if lock_res != 0 {
                eprintln!("Cache lock active for project {project}");
                return Ok(75);
            }

            let source = root.join("sources").join(project);
            let target = root.join("targets").join(project);

            // Enforce symlink immunity across source and target ancestry
            checked_path(&source)?;
            checked_path(&target)?;

            // Verify neither source nor target is actively mounted in Docker
            source_not_mounted(&source, &target)?;

            if source.exists() {
                cleanup_source(&source)?;
            }

            drop(lock_file);
            Ok(0)
        }
        LauncherAction::Build { project, cmd } => {
            // Default execution: host-launcher <project> [command...]
            validate_project(project)?;

            // 1. Fast check for draining sentinel
            if is_draining(&lock_root) {
                eprintln!("Builder host is draining");
                return Ok(75);
            }

            // 2. Acquire shared build lock on oxidex-build.lock
            fs::create_dir_all(&lock_root)?;
            let global_path = lock_root.join("oxidex-build.lock");
            let global_lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o600)
                .open(&global_path)?;

            let lock_res =
                unsafe { libc::flock(global_lock.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
            if lock_res != 0 {
                eprintln!("Contended build lock on {}", global_path.display());
                return Ok(75);
            }

            // 3. Re-check draining sentinel under lock
            if is_draining(&lock_root) {
                eprintln!("Builder host is draining");
                return Ok(75);
            }

            let source = root.join("sources").join(project);
            let target = root.join("targets").join(project);
            let cargo = root.join("cargo");

            // Verify no symlinks exist in builder volume mounts
            checked_path(&source)?;
            checked_path(&target)?;
            checked_path(&cargo)?;

            let cmd = if !cmd.is_empty() {
                cmd
            } else {
                vec![
                    "cargo".to_string(),
                    "build".to_string(),
                    "--release".to_string(),
                    "--locked".to_string(),
                    "--bin".to_string(),
                    "oxidex".to_string(),
                ]
            };

            let invocation =
                container_command(project, &cmd, 16, 28, None, Some(&root.to_string_lossy()))?;
            let status = Command::new(&invocation[0])
                .args(&invocation[1..])
                .status()?;

            // Hold lock across entire child execution
            drop(global_lock);

            Ok(status.code().unwrap_or(0))
        }
    }
}
