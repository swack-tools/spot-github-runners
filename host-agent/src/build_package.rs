//! Three-Artifact Same-Region GCS Packager for Dedicated Rust Builders.
//!
//! Before a Spot `builder-*` VM finishes a queued job or shuts down, it must
//! always package and upload three separate artifacts for the build ID:
//! 1. `build.log`: stdout/stderr compilation and test output log.
//! 2. `binaries.tar.zst`: compressed archive of compiled executable binaries
//!    from `target/{release,debug}/`.
//! 3. `target.tar.zst`: compressed archive of the `target/` directory and `build.log`.

use crate::clean_target::open_dir_nofollow;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{CStr, CString};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_GCS_BUCKET: &str = "example-rust-project-build-artifacts";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildPackageConfig {
    pub build_id: u64,
    pub workspace: PathBuf,
    pub target_dir: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub out_dir: PathBuf,
    pub gcs_bucket: String,
    pub profile: Option<String>,
    pub binary_name: Option<String>,
    pub upload: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildPackageManifest {
    pub build_id: u64,
    pub gcs_bucket: String,
    pub gcs_prefix: String,
    pub log_path: String,
    pub binary_archive_path: String,
    pub target_archive_path: String,
    pub log_url: String,
    pub binary_url: String,
    pub target_zip_url: String,
    pub log_size_bytes: u64,
    pub binary_size_bytes: u64,
    pub target_zip_size_bytes: u64,
    pub log_sha256: String,
    pub binary_sha256: String,
    pub target_zip_sha256: String,
    pub binaries_included: Vec<String>,
    pub uploaded: bool,
}

fn print_help() {
    eprintln!(
        "host-agent build-package — Package 3 build artifacts (log, binaries.tar.zst, target.tar.zst)\n\n\
         USAGE:\n    \
             host-agent build-package --build-id <ID> --workspace <PATH> --out-dir <PATH> [OPTIONS]\n\n\
         OPTIONS:\n    \
             --build-id <ID>       Numeric build ID from the queue (e.g. 4553)\n    \
             --workspace <PATH>    Workspace root containing target/\n    \
             --target-dir <PATH>   Explicit Cargo target directory (e.g. /mnt/runner-data/remote-build/targets/<project>)\n    \
             --log-file <PATH>     Path to build/test stdout+stderr log file\n    \
             --out-dir <PATH>      Output directory for staged builds/<id>/ artifacts\n    \
             --bucket <NAME>       Same-region GCS bucket name (default: {DEFAULT_GCS_BUCKET})\n    \
             --profile <NAME>      Build profile dir to scan first (release or debug)\n    \
             --binary <NAME>       Specific compiled binary name to package\n    \
             --upload              Upload staged artifacts to gs://<bucket>/builds/<id>/ via gcloud storage\n    \
             -h, --help            Print help information\n"
    );
}

pub fn parse_args(args: &[String]) -> Result<Option<BuildPackageConfig>, String> {
    let mut build_id: Option<u64> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut target_dir: Option<PathBuf> = None;
    let mut log_file: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut gcs_bucket = DEFAULT_GCS_BUCKET.to_string();
    let mut profile: Option<String> = None;
    let mut binary_name: Option<String> = None;
    let mut upload = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" | "help" => {
                print_help();
                return Ok(None);
            }
            "--build-id" => {
                i += 1;
                let raw = args.get(i).ok_or("--build-id requires a value")?;
                let parsed = raw
                    .trim_start_matches('#')
                    .parse::<u64>()
                    .map_err(|_| format!("Invalid numeric --build-id '{raw}'"))?;
                if parsed == 0 {
                    return Err("--build-id must be > 0".to_string());
                }
                build_id = Some(parsed);
            }
            "--workspace" => {
                i += 1;
                let raw = args.get(i).ok_or("--workspace requires a path")?;
                workspace = Some(PathBuf::from(raw));
            }
            "--target-dir" => {
                i += 1;
                let raw = args.get(i).ok_or("--target-dir requires a path")?;
                target_dir = Some(PathBuf::from(raw));
            }
            "--log-file" => {
                i += 1;
                let raw = args.get(i).ok_or("--log-file requires a path")?;
                log_file = Some(PathBuf::from(raw));
            }
            "--out-dir" => {
                i += 1;
                let raw = args.get(i).ok_or("--out-dir requires a path")?;
                out_dir = Some(PathBuf::from(raw));
            }
            "--bucket" => {
                i += 1;
                let raw = args.get(i).ok_or("--bucket requires a bucket name")?.trim();
                if raw.is_empty()
                    || raw.contains("..")
                    || raw.contains('/')
                    || !raw
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
                {
                    return Err(format!("Invalid --bucket name '{raw}'"));
                }
                gcs_bucket = raw.to_string();
            }
            "--profile" => {
                i += 1;
                let raw = args.get(i).ok_or("--profile requires a value")?.trim();
                if raw.contains("..") || raw.contains('/') || raw.is_empty() {
                    return Err(format!("Invalid --profile '{raw}'"));
                }
                profile = Some(raw.to_string());
            }
            "--binary" => {
                i += 1;
                let raw = args.get(i).ok_or("--binary requires a value")?.trim();
                if raw.contains("..") || raw.contains('/') || raw.is_empty() {
                    return Err(format!("Invalid --binary '{raw}'"));
                }
                binary_name = Some(raw.to_string());
            }
            "--upload" => {
                upload = true;
            }
            other => {
                return Err(format!("Unknown argument '{other}'"));
            }
        }
        i += 1;
    }

    let build_id = build_id.ok_or("Missing required --build-id <ID>")?;
    let workspace = workspace.ok_or("Missing required --workspace <PATH>")?;
    let out_dir = out_dir.ok_or("Missing required --out-dir <PATH>")?;

    Ok(Some(BuildPackageConfig {
        build_id,
        workspace,
        target_dir,
        log_file,
        out_dir,
        gcs_bucket,
        profile,
        binary_name,
        upload,
    }))
}

fn compute_file_size_and_sha256(path: &Path) -> Result<(u64, String), String> {
    let mut file = fs::File::open(path)
        .map_err(|e| format!("Failed to open '{}' for hashing: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("Failed reading '{}': {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hex::encode(hasher.finalize())))
}

fn is_candidate_executable_mode_and_name(
    file_name: &str,
    mode: u32,
    expected_name: Option<&str>,
) -> bool {
    if (mode & 0o111) == 0 {
        return false;
    }
    if let Some(expected) = expected_name {
        return file_name == expected;
    }
    if file_name.starts_with('.') {
        return false;
    }
    for disallowed_ext in [".d", ".rlib", ".rmeta", ".so", ".dylib", ".a", ".o", ".ll"] {
        if file_name.ends_with(disallowed_ext) {
            return false;
        }
    }
    true
}

pub fn validate_profile_dir_name(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 64
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "Invalid build profile '{raw}'; must be a single path component (alphanumeric, '-', '_')"
        ));
    }
    Ok(trimmed.to_string())
}

pub fn validate_binary_file_name(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 128
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!(
            "Invalid binary name '{raw}'; must be a single filename without path separators"
        ));
    }
    Ok(trimmed.to_string())
}

/// Discover and copy candidate executables from `<target_fd>/<profile>` into `bin_stage_dir`
/// using descriptor-relative `openat(..., O_NOFOLLOW)` and `fstatat(..., AT_SYMLINK_NOFOLLOW)`
/// so symlinks or path traversal can never escape `target_dir`.
fn discover_profile_executables_nofollow(
    target_fd: RawFd,
    expected_dev: libc::dev_t,
    profile_name: &str,
    expected_binary: Option<&str>,
    best_candidates: &mut std::collections::BTreeMap<String, (OwnedFd, i64, i64)>,
) -> Result<(), String> {
    let valid_prof = validate_profile_dir_name(profile_name)?;
    let c_prof = CString::new(valid_prof.as_bytes()).map_err(|e| e.to_string())?;
    let prof_fd = match open_dir_nofollow(Some(target_fd), &c_prof) {
        Ok(fd) => fd,
        Err(_) => return Ok(()),
    };

    let mut prof_st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(prof_fd.as_raw_fd(), &mut prof_st) } != 0 {
        return Ok(());
    }
    if prof_st.st_dev != expected_dev {
        return Err(format!(
            "Foreign filesystem detected inside target profile '{valid_prof}'"
        ));
    }

    let dup_fd = unsafe { libc::dup(prof_fd.as_raw_fd()) };
    if dup_fd < 0 {
        return Ok(());
    }
    let dir_ptr = unsafe { libc::fdopendir(dup_fd) };
    if dir_ptr.is_null() {
        unsafe {
            libc::close(dup_fd);
        }
        return Ok(());
    }

    loop {
        let entry = unsafe { libc::readdir(dir_ptr) };
        if entry.is_null() {
            break;
        }
        let entry_cstr = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = entry_cstr.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let Ok(file_name) = entry_cstr.to_str() else {
            continue;
        };

        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let stat_res = unsafe {
            libc::fstatat(
                prof_fd.as_raw_fd(),
                entry_cstr.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_res != 0 {
            continue;
        }
        if st.st_dev != expected_dev {
            continue;
        }
        if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
            continue;
        }
        let mode = st.st_mode as u32;
        if !is_candidate_executable_mode_and_name(file_name, mode, expected_binary) {
            continue;
        }

        let file_raw_fd = unsafe {
            libc::openat(
                prof_fd.as_raw_fd(),
                entry_cstr.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if file_raw_fd < 0 {
            continue;
        }
        let file_owned_fd = unsafe { OwnedFd::from_raw_fd(file_raw_fd) };
        let mut opened_st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(file_owned_fd.as_raw_fd(), &mut opened_st) } != 0
            || opened_st.st_dev != expected_dev
            || (opened_st.st_mode & libc::S_IFMT) != libc::S_IFREG
            || (opened_st.st_mode as u32 & 0o111) == 0
        {
            continue;
        }

        let mtime_sec = opened_st.st_mtime;
        let mtime_nsec = opened_st.st_mtime_nsec;
        let replace = match best_candidates.get(file_name) {
            None => true,
            Some((_, prev_sec, prev_nsec)) => (mtime_sec, mtime_nsec) > (*prev_sec, *prev_nsec),
        };
        if replace {
            best_candidates.insert(
                file_name.to_string(),
                (file_owned_fd, mtime_sec, mtime_nsec),
            );
        }
    }

    unsafe {
        libc::closedir(dir_ptr);
    }
    Ok(())
}

fn create_compressed_archive(
    source_dir: &Path,
    entries: &[&str],
    dest_archive: &Path,
    extra_dir_and_entry: Option<(&Path, &str)>,
) -> Result<(), String> {
    // Try `tar --zstd -cf <dest> -C <source_dir> <entries...>` first, then fallback to `tar -czf`
    let mut zstd_cmd = Command::new("tar");
    zstd_cmd
        .arg("--zstd")
        .arg("-cf")
        .arg(dest_archive)
        .arg("-C")
        .arg(source_dir);
    for entry in entries {
        zstd_cmd.arg(entry);
    }
    if let Some((extra_dir, extra_entry)) = extra_dir_and_entry {
        zstd_cmd.arg("-C").arg(extra_dir).arg(extra_entry);
    }
    if let Ok(status) = zstd_cmd.status() {
        if status.success() && dest_archive.exists() {
            return Ok(());
        }
    }

    let mut gz_cmd = Command::new("tar");
    gz_cmd
        .arg("-czf")
        .arg(dest_archive)
        .arg("-C")
        .arg(source_dir);
    for entry in entries {
        gz_cmd.arg(entry);
    }
    if let Some((extra_dir, extra_entry)) = extra_dir_and_entry {
        gz_cmd.arg("-C").arg(extra_dir).arg(extra_entry);
    }
    let status = gz_cmd
        .status()
        .map_err(|e| format!("Failed to invoke tar for '{}': {e}", dest_archive.display()))?;
    if !status.success() {
        return Err(format!(
            "tar exited with non-zero status ({status}) creating '{}'",
            dest_archive.display()
        ));
    }
    Ok(())
}

fn fetch_gce_metadata_access_token() -> Result<String, String> {
    if let Ok(env_tok) = std::env::var("GCS_OAUTH_TOKEN") {
        let trimmed = env_tok.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "10",
            "-H",
            "Metadata-Flavor: Google",
            "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token",
        ])
        .output()
        .map_err(|e| format!("Failed querying GCE metadata token endpoint: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "GCE metadata token endpoint returned status {}",
            output.status
        ));
    }
    let val: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Invalid JSON from GCE metadata token endpoint: {e}"))?;
    val.get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "GCE metadata token response missing access_token".to_string())
}

fn upload_artifacts_to_gcs(
    bucket: &str,
    build_id: u64,
    dest_log: &Path,
    dest_binaries_archive: &Path,
    dest_target_archive: &Path,
) -> Result<(), String> {
    let gcs_prefix = format!("gs://{bucket}/builds/{build_id}");

    // 1. Try `gcloud storage cp` if `gcloud` is installed on the host
    if let Ok(status) = Command::new("gcloud")
        .args([
            "storage",
            "cp",
            "--quiet",
            &dest_log.to_string_lossy(),
            &dest_binaries_archive.to_string_lossy(),
            &dest_target_archive.to_string_lossy(),
            &format!("{gcs_prefix}/"),
        ])
        .status()
    {
        if status.success() {
            return Ok(());
        }
    }

    // 2. Upload directly to GCS JSON API using curl + GCE instance service account token
    let token = fetch_gce_metadata_access_token()?;
    let objects = [
        (dest_log, "build.log", "text/plain; charset=utf-8"),
        (
            dest_binaries_archive,
            "binaries.tar.zst",
            "application/octet-stream",
        ),
        (
            dest_target_archive,
            "target.tar.zst",
            "application/octet-stream",
        ),
    ];

    for (local_path, file_name, content_type) in objects {
        let object_name = format!("builds/{build_id}/{file_name}");
        let encoded_name = object_name.replace('/', "%2F");
        let upload_url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{bucket}/o?uploadType=media&name={encoded_name}"
        );
        let output = Command::new("curl")
            .args([
                "-fsSL",
                "--max-time",
                "300",
                "-X",
                "POST",
                "-H",
                &format!("Authorization: Bearer {token}"),
                "-H",
                &format!("Content-Type: {content_type}"),
                "--data-binary",
                &format!("@{}", local_path.to_string_lossy()),
                &upload_url,
            ])
            .output()
            .map_err(|e| format!("Failed invoking curl for GCS upload of '{file_name}': {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "GCS HTTP upload failed for {gcs_prefix}/{file_name} ({}): {}",
                output.status,
                stderr.trim()
            ));
        }
    }

    Ok(())
}

/// Resolve the effective Cargo output directory under `target/` for a build job.
///
/// Cargo writes `"dev"` and `"test"` profiles into `target/debug`, `"release"` and `"bench"`
/// into `target/release`, and custom `--profile <name>` into `target/<name>`.
/// When `explicit_profile` is unset or set to the control-plane default `"test"`, the command
/// tokens determine whether `--release` or `--profile <name>` was requested.
pub fn resolve_cargo_target_profile(
    explicit_profile: Option<&str>,
    command: Option<&str>,
) -> String {
    if let Some(cmd) = command {
        let tokens: Vec<&str> = cmd.split_whitespace().collect();
        let mut idx = 0;
        while idx < tokens.len() {
            let tok = tokens[idx];
            if tok == "--release" || tok == "-r" {
                return "release".to_string();
            }
            if let Some(prof) = tok.strip_prefix("--profile=") {
                return match prof.trim() {
                    "dev" | "test" | "" => "debug".to_string(),
                    "bench" | "release" => "release".to_string(),
                    custom => custom.to_string(),
                };
            }
            if tok == "--profile" {
                if let Some(prof) = tokens.get(idx + 1) {
                    return match prof.trim() {
                        "dev" | "test" | "" => "debug".to_string(),
                        "bench" | "release" => "release".to_string(),
                        custom => custom.to_string(),
                    };
                }
            }
            idx += 1;
        }
        // If a command was supplied without `--release` or `--profile`, and `explicit_profile`
        // is None or the default `"test"`/`"dev"`, Cargo outputs into `target/debug`.
        match explicit_profile.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("test") | Some("dev") | Some("debug") => return "debug".to_string(),
            Some("bench") | Some("release") => return "release".to_string(),
            Some(custom) => return custom.to_string(),
        }
    }

    match explicit_profile.map(str::trim).filter(|s| !s.is_empty()) {
        Some("dev") | Some("test") | Some("debug") => "debug".to_string(),
        Some("bench") | Some("release") => "release".to_string(),
        Some(custom) => custom.to_string(),
        None => "debug".to_string(),
    }
}

pub fn package_build_artifacts(cfg: &BuildPackageConfig) -> Result<BuildPackageManifest, String> {
    if !cfg.workspace.exists() {
        return Err(format!(
            "Workspace path '{}' does not exist",
            cfg.workspace.display()
        ));
    }

    let build_stage_dir = cfg.out_dir.join("builds").join(cfg.build_id.to_string());
    fs::create_dir_all(&build_stage_dir).map_err(|e| {
        format!(
            "Failed to create staging directory '{}': {e}",
            build_stage_dir.display()
        )
    })?;

    // 1. Always stage Artifact #1: `build.log`
    let dest_log = build_stage_dir.join("build.log");
    if let Some(ref src_log) = cfg.log_file {
        if src_log.exists() {
            fs::copy(src_log, &dest_log).map_err(|e| {
                format!(
                    "Failed to copy log '{}' to '{}': {e}",
                    src_log.display(),
                    dest_log.display()
                )
            })?;
        } else {
            fs::write(
                &dest_log,
                format!(
                    "Build #{} log file '{}' was not found at packaging time.\n",
                    cfg.build_id,
                    src_log.display()
                ),
            )
            .map_err(|e| format!("Failed to write fallback log '{}': {e}", dest_log.display()))?;
        }
    } else {
        fs::write(
            &dest_log,
            format!(
                "Build #{} completed (no --log-file specified).\n",
                cfg.build_id
            ),
        )
        .map_err(|e| format!("Failed to write default log '{}': {e}", dest_log.display()))?;
    }

    // 2. Always stage Artifact #2: `binaries.tar.zst`
    let bin_stage_dir = build_stage_dir.join(".bin-stage");
    if bin_stage_dir.exists() {
        let _ = fs::remove_dir_all(&bin_stage_dir);
    }
    fs::create_dir_all(&bin_stage_dir)
        .map_err(|e| format!("Failed to create binary stage dir: {e}"))?;

    let ws_target = cfg.workspace.join("target");
    let sibling_target = cfg
        .workspace
        .parent()
        .and_then(|p| p.parent())
        .and_then(|root| {
            cfg.workspace
                .file_name()
                .map(|name| root.join("targets").join(name))
        });
    let (target_dir, is_workspace_target) = if let Some(ref explicit_target) = cfg.target_dir {
        let same_as_ws = explicit_target == &ws_target;
        (explicit_target.clone(), same_as_ws)
    } else if ws_target.is_dir() {
        (ws_target, true)
    } else if let Some(ref sib) = sibling_target {
        if sib.is_dir() {
            (sib.clone(), false)
        } else {
            (ws_target, true)
        }
    } else {
        (ws_target, true)
    };
    let validated_binary_name = match cfg
        .binary_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(b) => Some(validate_binary_file_name(b)?),
        None => None,
    };

    let profile_dirs: Vec<String> = match cfg
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(explicit) => {
            let resolved = resolve_cargo_target_profile(Some(explicit), None);
            vec![validate_profile_dir_name(&resolved)?]
        }
        None => vec!["debug".to_string(), "release".to_string()],
    };

    // Collect candidate executables across the eligible profile directory(ies) using
    // descriptor-relative nofollow traversal so symlinks inside `target_dir` cannot escape.
    let mut best_candidates: std::collections::BTreeMap<String, (OwnedFd, i64, i64)> =
        std::collections::BTreeMap::new();
    if let Ok(c_target) = CString::new(target_dir.as_os_str().as_bytes()) {
        if let Ok(target_fd) = open_dir_nofollow(None, &c_target) {
            let mut target_st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(target_fd.as_raw_fd(), &mut target_st) } == 0 {
                let expected_dev = target_st.st_dev;
                for prof in &profile_dirs {
                    discover_profile_executables_nofollow(
                        target_fd.as_raw_fd(),
                        expected_dev,
                        prof,
                        validated_binary_name.as_deref(),
                        &mut best_candidates,
                    )?;
                }
            }
        }
    }

    let mut binaries_included = Vec::new();
    for (name, (src_fd, _, _)) in best_candidates {
        let dest_bin = bin_stage_dir.join(&name);
        let mut src_file = fs::File::from(src_fd);
        let mut dest_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o755)
            .open(&dest_bin)
            .map_err(|e| {
                format!(
                    "Failed creating staged binary '{}': {e}",
                    dest_bin.display()
                )
            })?;
        std::io::copy(&mut src_file, &mut dest_file)
            .map_err(|e| format!("Failed copying binary '{name}': {e}"))?;
        binaries_included.push(name);
    }
    binaries_included.sort();

    if binaries_included.is_empty() {
        // Ensure the archive is always valid and non-empty even for test-only runs
        fs::write(
            bin_stage_dir.join("BUILD_INFO.txt"),
            format!(
                "build_id={}\nnote=No standalone executable binaries matched in target/{{release,debug}}\n",
                cfg.build_id
            ),
        )
        .map_err(|e| format!("Failed writing BUILD_INFO.txt: {e}"))?;
    }

    let dest_binaries_archive = build_stage_dir.join("binaries.tar.zst");
    create_compressed_archive(&bin_stage_dir, &["."], &dest_binaries_archive, None)?;
    let _ = fs::remove_dir_all(&bin_stage_dir);

    // 3. Always stage Artifact #3: `target.tar.zst` (full target/ directory + build.log)
    // Never write into the container-writable target_dir to avoid following malicious symlinks.
    let dest_target_archive = build_stage_dir.join("target.tar.zst");
    if target_dir.is_dir() {
        if is_workspace_target {
            create_compressed_archive(
                &cfg.workspace,
                &["target"],
                &dest_target_archive,
                Some((&build_stage_dir, "build.log")),
            )?;
        } else {
            create_compressed_archive(
                &target_dir,
                &["."],
                &dest_target_archive,
                Some((&build_stage_dir, "build.log")),
            )?;
        }
    } else {
        create_compressed_archive(&build_stage_dir, &["build.log"], &dest_target_archive, None)?;
    }

    let (log_size_bytes, log_sha256) = compute_file_size_and_sha256(&dest_log)?;
    let (binary_size_bytes, binary_sha256) = compute_file_size_and_sha256(&dest_binaries_archive)?;
    let (target_zip_size_bytes, target_zip_sha256) =
        compute_file_size_and_sha256(&dest_target_archive)?;

    let gcs_prefix = format!("gs://{}/builds/{}", cfg.gcs_bucket, cfg.build_id);
    let log_url = format!("{gcs_prefix}/build.log");
    let binary_url = format!("{gcs_prefix}/binaries.tar.zst");
    let target_zip_url = format!("{gcs_prefix}/target.tar.zst");

    let mut uploaded = false;
    if cfg.upload {
        upload_artifacts_to_gcs(
            &cfg.gcs_bucket,
            cfg.build_id,
            &dest_log,
            &dest_binaries_archive,
            &dest_target_archive,
        )?;
        uploaded = true;
    }

    let manifest = BuildPackageManifest {
        build_id: cfg.build_id,
        gcs_bucket: cfg.gcs_bucket.clone(),
        gcs_prefix,
        log_path: dest_log.to_string_lossy().into_owned(),
        binary_archive_path: dest_binaries_archive.to_string_lossy().into_owned(),
        target_archive_path: dest_target_archive.to_string_lossy().into_owned(),
        log_url,
        binary_url,
        target_zip_url,
        log_size_bytes,
        binary_size_bytes,
        target_zip_size_bytes,
        log_sha256,
        binary_sha256,
        target_zip_sha256,
        binaries_included,
        uploaded,
    };

    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| format!("Failed serializing manifest JSON: {e}"))?;
    fs::write(build_stage_dir.join("manifest.json"), &manifest_json)
        .map_err(|e| format!("Failed writing manifest.json: {e}"))?;

    Ok(manifest)
}

pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let Some(cfg) = parse_args(args)? else {
        return Ok(0);
    };
    let manifest = package_build_artifacts(&cfg)?;
    let out = serde_json::to_string_pretty(&manifest)
        .map_err(|e| format!("JSON serialization error: {e}"))?;
    println!("{out}");
    Ok(0)
}

// ============================================================================
// Builder Queue Consumer Worker (`host-agent queue-worker`)
// ============================================================================

pub const DEFAULT_DASHBOARD_URL: &str = "https://spot.swacktech.com";
pub const DEFAULT_SOURCES_ROOT: &str = "/mnt/runner-data/remote-build/sources";
pub const DEFAULT_ARTIFACTS_OUT_DIR: &str = "/mnt/runner-data/remote-build/artifacts";
pub const DEFAULT_BUILDER_LAUNCHER: &str = "/usr/local/bin/example-rust-project-remote-build";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueWorkerConfig {
    pub dashboard_url: String,
    pub instance_name: String,
    pub zone: Option<String>,
    pub sources_root: PathBuf,
    pub out_dir: PathBuf,
    pub launcher: PathBuf,
    pub poll_interval_secs: u64,
    pub upload: bool,
    pub once: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimedBuildJob {
    pub build_id: u64,
    pub repo: String,
    pub branch: String,
    pub commit_sha: String,
    pub command: String,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub binary_name: Option<String>,
    #[serde(default = "default_gcs_bucket_string")]
    pub gcs_bucket: String,
}

fn default_gcs_bucket_string() -> String {
    DEFAULT_GCS_BUCKET.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueWorkerCycleResult {
    pub claimed: bool,
    pub build_id: Option<u64>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub manifest: Option<BuildPackageManifest>,
}

fn print_worker_help() {
    eprintln!(
        "host-agent queue-worker — Poll, claim, execute, package & complete queued Rust builds\n\n\
         USAGE:\n    \
             host-agent queue-worker [OPTIONS]\n\n\
         OPTIONS:\n    \
             --dashboard-url <URL>       Control-plane base URL (default: {DEFAULT_DASHBOARD_URL})\n    \
             --instance-name <NAME>      Builder VM instance name\n    \
             --zone <ZONE>               Builder GCE zone (e.g. us-central1-a)\n    \
             --sources-root <PATH>       Root directory for project workspaces (default: {DEFAULT_SOURCES_ROOT})\n    \
             --out-dir <PATH>            Output staging directory for packaged artifacts (default: {DEFAULT_ARTIFACTS_OUT_DIR})\n    \
             --launcher <PATH>           Builder launcher binary (default: {DEFAULT_BUILDER_LAUNCHER})\n    \
             --poll-interval-secs <SEC>  Idle poll interval in seconds (default: 10)\n    \
             --upload                    Upload packaged artifacts to GCS (default)\n    \
             --no-upload                 Stage packaged artifacts locally without invoking gcloud storage cp\n    \
             --once                      Run a single claim/build/complete cycle and exit\n    \
             -h, --help                  Print help information\n"
    );
}

pub fn parse_worker_args(args: &[String]) -> Result<Option<QueueWorkerConfig>, String> {
    let mut dashboard_url = std::env::var("SPOT_DASHBOARD_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_DASHBOARD_URL.to_string());
    let mut instance_name = std::env::var("BUILDER_INSTANCE_NAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "builder-local".to_string());
    let mut zone = std::env::var("BUILDER_ZONE")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let mut sources_root = PathBuf::from(DEFAULT_SOURCES_ROOT);
    let mut out_dir = PathBuf::from(DEFAULT_ARTIFACTS_OUT_DIR);
    let mut launcher = PathBuf::from(DEFAULT_BUILDER_LAUNCHER);
    let mut poll_interval_secs: u64 = 10;
    let mut upload = true;
    let mut once = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" | "help" => {
                print_worker_help();
                return Ok(None);
            }
            "--dashboard-url" => {
                i += 1;
                let raw = args.get(i).ok_or("--dashboard-url requires a URL")?.trim();
                if raw.is_empty() {
                    return Err("--dashboard-url cannot be empty".to_string());
                }
                dashboard_url = raw.trim_end_matches('/').to_string();
            }
            "--instance-name" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or("--instance-name requires a value")?
                    .trim();
                if raw.is_empty() {
                    return Err("--instance-name cannot be empty".to_string());
                }
                instance_name = raw.to_string();
            }
            "--zone" => {
                i += 1;
                let raw = args.get(i).ok_or("--zone requires a value")?.trim();
                if !raw.is_empty() {
                    zone = Some(raw.to_string());
                }
            }
            "--sources-root" => {
                i += 1;
                let raw = args.get(i).ok_or("--sources-root requires a path")?;
                sources_root = PathBuf::from(raw);
            }
            "--out-dir" => {
                i += 1;
                let raw = args.get(i).ok_or("--out-dir requires a path")?;
                out_dir = PathBuf::from(raw);
            }
            "--launcher" => {
                i += 1;
                let raw = args.get(i).ok_or("--launcher requires a path")?;
                launcher = PathBuf::from(raw);
            }
            "--poll-interval-secs" => {
                i += 1;
                let raw = args.get(i).ok_or("--poll-interval-secs requires a value")?;
                let secs = raw
                    .parse::<u64>()
                    .map_err(|_| format!("Invalid --poll-interval-secs '{raw}'"))?;
                if secs == 0 {
                    return Err("--poll-interval-secs must be >= 1".to_string());
                }
                poll_interval_secs = secs;
            }
            "--upload" => {
                upload = true;
            }
            "--no-upload" => {
                upload = false;
            }
            "--once" => {
                once = true;
            }
            other => {
                return Err(format!("Unknown argument '{other}'"));
            }
        }
        i += 1;
    }

    Ok(Some(QueueWorkerConfig {
        dashboard_url: dashboard_url.trim_end_matches('/').to_string(),
        instance_name,
        zone,
        sources_root,
        out_dir,
        launcher,
        poll_interval_secs,
        upload,
        once,
    }))
}

pub fn sanitize_project_slug(repo: &str) -> Result<String, String> {
    let trimmed = repo.trim().trim_matches('/');
    if trimmed.contains("..") {
        return Err(format!(
            "Repository '{repo}' contains forbidden path traversal"
        ));
    }
    let base = trimmed.rsplit('/').next().unwrap_or(trimmed).trim();
    let base = base.strip_suffix(".git").unwrap_or(base);
    if base.is_empty()
        || base.len() > 64
        || base == "."
        || base == ".."
        || !base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!(
            "Repository '{repo}' does not produce a valid builder project slug"
        ));
    }
    Ok(base.to_string())
}

fn resolve_git_remote_url(repo: &str) -> String {
    let trimmed = repo.trim();
    if trimmed.starts_with("https://")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("ssh://")
        || trimmed.starts_with("git@")
        || trimmed.starts_with("file://")
        || Path::new(trimmed).exists()
    {
        return trimmed.to_string();
    }
    let base = std::env::var("SPOT_GIT_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://github.com".to_string());
    let base_trimmed = base.trim_end_matches('/');
    let repo_trimmed = trimmed.trim_matches('/');
    if let Some(local_root) = base_trimmed.strip_prefix("file://") {
        let candidate = Path::new(local_root).join(repo_trimmed);
        if candidate.exists() {
            return format!("file://{}", candidate.display());
        }
    }
    if repo_trimmed.ends_with(".git") {
        format!("{base_trimmed}/{repo_trimmed}")
    } else {
        format!("{base_trimmed}/{repo_trimmed}.git")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApprovedUploaderIdentity {
    username: String,
    uid: u32,
}

fn resolve_approved_uploader_identity() -> Option<ApprovedUploaderIdentity> {
    let config_path = std::env::var("OXIDEX_UPLOADER_CONFIG")
        .or_else(|_| std::env::var("EXAMPLE_RUST_PROJECT_UPLOADER_CONFIG"))
        .unwrap_or_else(|_| "/etc/oxidex-builder-uploader.json".to_string());
    let content = fs::read_to_string(&config_path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let username = val.get("username")?.as_str()?.trim().to_string();
    let uid = val.get("uid")?.as_u64()? as u32;
    if username.is_empty() || username == "root" || uid < 1000 || uid == 1001 {
        return None;
    }
    Some(ApprovedUploaderIdentity { username, uid })
}

fn build_git_command(uploader: Option<&ApprovedUploaderIdentity>) -> Command {
    if unsafe { libc::geteuid() } == 0 {
        if let Some(id) = uploader {
            let mut cmd = Command::new("sudo");
            cmd.arg("-u").arg(&id.username).arg("--").arg("git");
            return cmd;
        }
    }
    Command::new("git")
}

pub fn materialize_repository_checkout(
    workspace: &Path,
    job: &ClaimedBuildJob,
) -> Result<(), String> {
    let remote_url = resolve_git_remote_url(&job.repo);
    let uploader = resolve_approved_uploader_identity();

    if !workspace.join(".git").exists() {
        let init_status = build_git_command(uploader.as_ref())
            .arg("init")
            .arg("--quiet")
            .arg(workspace)
            .status()
            .map_err(|e| format!("Failed invoking git init in '{}': {e}", workspace.display()))?;
        if !init_status.success() {
            return Err(format!(
                "git init failed with status {init_status} in '{}'",
                workspace.display()
            ));
        }
    }

    // Configure or update origin remote URL
    let has_origin = build_git_command(uploader.as_ref())
        .arg("-C")
        .arg(workspace)
        .args(["remote", "get-url", "origin"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let remote_cmd = if has_origin {
        ["remote", "set-url", "origin", &remote_url]
    } else {
        ["remote", "add", "origin", &remote_url]
    };
    let remote_status = build_git_command(uploader.as_ref())
        .arg("-C")
        .arg(workspace)
        .args(remote_cmd)
        .status()
        .map_err(|e| format!("Failed configuring git origin '{remote_url}': {e}"))?;
    if !remote_status.success() {
        return Err(format!(
            "git remote configuration failed with status {remote_status} for '{remote_url}'"
        ));
    }

    // Fetch requested commit_sha directly first, falling back to fetching branch/all refs
    let fetch_sha_ok = build_git_command(uploader.as_ref())
        .arg("-C")
        .arg(workspace)
        .args([
            "fetch",
            "--quiet",
            "--depth",
            "1",
            "origin",
            &job.commit_sha,
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !fetch_sha_ok {
        let fetch_branch_ok = build_git_command(uploader.as_ref())
            .arg("-C")
            .arg(workspace)
            .args(["fetch", "--quiet", "origin", &job.branch])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !fetch_branch_ok {
            let fetch_all = build_git_command(uploader.as_ref())
                .arg("-C")
                .arg(workspace)
                .args(["fetch", "--all", "--quiet"])
                .output()
                .map_err(|e| format!("Failed invoking git fetch from '{remote_url}': {e}"))?;
            if !fetch_all.status.success() {
                let stderr = String::from_utf8_lossy(&fetch_all.stderr);
                return Err(format!(
                    "git fetch failed from '{remote_url}' ({}): {}",
                    fetch_all.status,
                    stderr.trim()
                ));
            }
        }
    }

    let checkout_out = build_git_command(uploader.as_ref())
        .arg("-C")
        .arg(workspace)
        .args(["checkout", "--force", &job.commit_sha])
        .output()
        .map_err(|e| format!("Failed invoking git checkout '{}': {e}", job.commit_sha))?;
    if !checkout_out.status.success() {
        let stderr = String::from_utf8_lossy(&checkout_out.stderr);
        return Err(format!(
            "git checkout '{}' failed ({}): {}",
            job.commit_sha,
            checkout_out.status,
            stderr.trim()
        ));
    }

    // Strictly verify that resolved HEAD matches the requested commit_sha
    let head_out = build_git_command(uploader.as_ref())
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("Failed invoking git rev-parse HEAD: {e}"))?;
    if !head_out.status.success() {
        return Err("git rev-parse HEAD failed after checkout".to_string());
    }
    let resolved_head = String::from_utf8_lossy(&head_out.stdout)
        .trim()
        .to_lowercase();
    let requested_sha = job.commit_sha.trim().to_lowercase();
    if requested_sha.len() < 7 || !resolved_head.starts_with(&requested_sha) {
        return Err(format!(
            "Checked out HEAD '{resolved_head}' does not match requested commit_sha '{requested_sha}'"
        ));
    }

    Ok(())
}

static RENEWED_BEARER_TOKEN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
pub const WORKER_LEASE_HEARTBEAT_INTERVAL_SECS: u64 = 1800; // 30 minutes (well inside 2h lease, conserving KV writes)

fn active_bearer_token() -> Option<String> {
    if let Ok(guard) = RENEWED_BEARER_TOKEN.lock() {
        if let Some(ref tok) = *guard {
            if !tok.trim().is_empty() {
                return Some(tok.clone());
            }
        }
    }
    std::env::var("SPOT_BEARER_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn persist_renewed_queue_token(new_token: &str) {
    let trimmed = new_token.trim();
    if trimmed.is_empty() || trimmed.contains('\n') || trimmed.contains('\r') {
        return;
    }
    if let Ok(mut guard) = RENEWED_BEARER_TOKEN.lock() {
        *guard = Some(trimmed.to_string());
    }
    let env_file_path = std::env::var("SPOT_QUEUE_WORKER_ENV_FILE")
        .unwrap_or_else(|_| "/etc/default/example-rust-project-queue-worker".to_string());
    let path = Path::new(&env_file_path);
    if let Ok(existing) = fs::read_to_string(path) {
        let mut updated_lines = Vec::new();
        let mut replaced = false;
        for line in existing.lines() {
            if line.starts_with("SPOT_BEARER_TOKEN=") {
                updated_lines.push(format!("SPOT_BEARER_TOKEN={trimmed}"));
                replaced = true;
            } else {
                updated_lines.push(line.to_string());
            }
        }
        if !replaced {
            updated_lines.push(format!("SPOT_BEARER_TOKEN={trimmed}"));
        }
        let new_content = format!("{}\n", updated_lines.join("\n"));
        let _ = fs::write(path, new_content);
    }
}

fn capture_renewed_token_from_response(resp: &serde_json::Value) {
    if let Some(new_tok) = resp.get("renewed_queue_token").and_then(|v| v.as_str()) {
        persist_renewed_queue_token(new_tok);
    }
}

struct ActiveBuildHeartbeatGuard {
    stop_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ActiveBuildHeartbeatGuard {
    fn start(
        dashboard_url: String,
        build_id: u64,
        instance_name: String,
        interval_secs: u64,
    ) -> Self {
        let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_clone = std::sync::Arc::clone(&stop_flag);
        let handle = std::thread::spawn(move || {
            let step_ms = 200u64;
            let total_wait_ms = interval_secs.max(1).saturating_mul(1000);
            let mut waited_ms = 0u64;
            while !flag_clone.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(step_ms));
                if flag_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                waited_ms = waited_ms.saturating_add(step_ms);
                if waited_ms >= total_wait_ms {
                    waited_ms = 0;
                    let endpoint = format!("/api/rust-builds/{build_id}/heartbeat");
                    let payload = serde_json::json!({
                        "build_id": build_id,
                        "instance_name": instance_name,
                    });
                    let _ = post_control_plane_json(&dashboard_url, &endpoint, &payload);
                }
            }
        });
        Self {
            stop_flag,
            handle: Some(handle),
        }
    }
}

impl Drop for ActiveBuildHeartbeatGuard {
    fn drop(&mut self) {
        self.stop_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn post_control_plane_json(
    base_url: &str,
    endpoint_path: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let body_str =
        serde_json::to_string(payload).map_err(|e| format!("Failed serializing JSON body: {e}"))?;

    if let Some(local_root) = base_url.strip_prefix("file://") {
        let root_path = Path::new(local_root);
        fs::create_dir_all(root_path)
            .map_err(|e| format!("Failed creating mock control dir: {e}"))?;
        if endpoint_path == "/api/rust-builds/claim" {
            fs::write(root_path.join("claim_request.json"), &body_str)
                .map_err(|e| format!("Failed writing claim_request.json: {e}"))?;
            let resp_file = root_path.join("claim_response.json");
            if !resp_file.exists() {
                return Ok(serde_json::json!({
                    "status": "empty",
                    "claimed": false,
                    "build": serde_json::Value::Null
                }));
            }
            let raw = fs::read_to_string(&resp_file)
                .map_err(|e| format!("Failed reading '{}': {e}", resp_file.display()))?;
            let parsed: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| format!("Invalid JSON in '{}': {e}", resp_file.display()))?;
            capture_renewed_token_from_response(&parsed);
            return Ok(parsed);
        } else if endpoint_path.starts_with("/api/rust-builds/")
            && endpoint_path.ends_with("/heartbeat")
        {
            let id_part = endpoint_path
                .trim_start_matches("/api/rust-builds/")
                .trim_end_matches("/heartbeat")
                .trim_matches('/');
            let hb_path = root_path.join(format!("heartbeat_{id_part}.json"));
            fs::write(&hb_path, &body_str)
                .map_err(|e| format!("Failed writing '{}': {e}", hb_path.display()))?;
            return Ok(serde_json::json!({
                "status": "renewed",
                "build_id": id_part.parse::<u64>().unwrap_or(0)
            }));
        } else if endpoint_path.starts_with("/api/rust-builds/")
            && endpoint_path.ends_with("/complete")
        {
            let id_part = endpoint_path
                .trim_start_matches("/api/rust-builds/")
                .trim_end_matches("/complete")
                .trim_matches('/');
            let complete_path = root_path.join(format!("complete_{id_part}.json"));
            fs::write(&complete_path, &body_str)
                .map_err(|e| format!("Failed writing '{}': {e}", complete_path.display()))?;
            return Ok(serde_json::json!({
                "status": "ok",
                "build_id": id_part.parse::<u64>().unwrap_or(0)
            }));
        }
    }

    let url = format!("{}{}", base_url.trim_end_matches('/'), endpoint_path);
    let mut cmd = Command::new("curl");
    cmd.args([
        "-fsSL",
        "--max-time",
        "30",
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
    ]);
    if let Ok(cookie) = std::env::var("SPOT_SESSION_COOKIE") {
        let trimmed = cookie.trim();
        if !trimmed.is_empty() {
            let header = if trimmed.contains('=') {
                format!("Cookie: {trimmed}")
            } else {
                format!("Cookie: spot_session={trimmed}")
            };
            cmd.arg("-H").arg(header);
        }
    }
    if let Some(trimmed) = active_bearer_token() {
        cmd.arg("-H")
            .arg(format!("Authorization: Bearer {trimmed}"));
    }
    cmd.arg("-d").arg(&body_str).arg(&url);

    let output = cmd
        .output()
        .map_err(|e| format!("Failed invoking curl for '{url}': {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "POST {url} failed with status {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| format!("Invalid JSON response from {url}: {e}"))?;
    capture_renewed_token_from_response(&parsed);
    Ok(parsed)
}

pub fn process_next_queued_build(
    cfg: &QueueWorkerConfig,
) -> Result<QueueWorkerCycleResult, String> {
    let claim_payload = serde_json::json!({
        "instance_name": cfg.instance_name,
        "zone": cfg.zone,
    });
    let claim_resp =
        post_control_plane_json(&cfg.dashboard_url, "/api/rust-builds/claim", &claim_payload)?;

    let claimed = claim_resp
        .get("claimed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !claimed {
        return Ok(QueueWorkerCycleResult {
            claimed: false,
            build_id: None,
            status: "empty".to_string(),
            exit_code: None,
            manifest: None,
        });
    }

    let build_val = claim_resp
        .get("build")
        .ok_or("Claim response marked claimed=true but missing 'build' object")?;
    let job: ClaimedBuildJob = serde_json::from_value(build_val.clone())
        .map_err(|e| format!("Failed parsing claimed build job: {e}"))?;

    // Maintain a background lease heartbeat for the duration of checkout, compilation, and GCS upload
    let _heartbeat_guard = ActiveBuildHeartbeatGuard::start(
        cfg.dashboard_url.clone(),
        job.build_id,
        cfg.instance_name.clone(),
        WORKER_LEASE_HEARTBEAT_INTERVAL_SECS,
    );

    let project_slug = sanitize_project_slug(&job.repo)?;
    let workspace = cfg.sources_root.join(&project_slug);
    let launcher_target_dir = cfg
        .sources_root
        .parent()
        .unwrap_or(&cfg.sources_root)
        .join("targets")
        .join(&project_slug);

    // Reserve the project's active build lock and verify that neither source nor target
    // is mounted in a running container before preparing or modifying the shared source checkout.
    let lock_root = PathBuf::from(
        std::env::var("OXIDEX_LOCK_ROOT")
            .or_else(|_| std::env::var("EXAMPLE_RUST_PROJECT_LOCK_ROOT"))
            .unwrap_or_else(|_| "/run".to_string()),
    );
    let project_locks = match crate::docker_admission::acquire_build_locks(
        &project_slug,
        &lock_root,
    ) {
        Ok(locks) => locks,
        Err(lock_code) => {
            let lock_err = format!(
                "Project '{}' build lock or draining sentinel is active in '{}' (exit {lock_code}); refusing to overwrite active source workspace",
                project_slug,
                lock_root.display()
            );
            let fail_payload = serde_json::json!({
                "build_id": job.build_id,
                "status": "failed",
                "exit_code": lock_code,
                "compile_duration_seconds": 0.0,
                "delivery_duration_seconds": 0.0,
                "instance_name": cfg.instance_name,
                "zone": cfg.zone,
            });
            let complete_endpoint = format!("/api/rust-builds/{}/complete", job.build_id);
            let _ = post_control_plane_json(&cfg.dashboard_url, &complete_endpoint, &fail_payload);
            return Err(lock_err);
        }
    };
    if let Err(mount_err) =
        crate::host_launcher::source_not_mounted(&workspace, &launcher_target_dir)
    {
        let busy_err = format!(
            "Workspace or target for project '{}' is actively mounted in a container: {mount_err}",
            project_slug
        );
        let fail_payload = serde_json::json!({
            "build_id": job.build_id,
            "status": "failed",
            "exit_code": 75,
            "compile_duration_seconds": 0.0,
            "delivery_duration_seconds": 0.0,
            "instance_name": cfg.instance_name,
            "zone": cfg.zone,
        });
        let complete_endpoint = format!("/api/rust-builds/{}/complete", job.build_id);
        let _ = post_control_plane_json(&cfg.dashboard_url, &complete_endpoint, &fail_payload);
        return Err(busy_err);
    }

    // 1. Verify and invoke dedicated builder launcher to prepare workspace (fail closed if missing)
    if !cfg.launcher.is_file() {
        let missing_err = format!(
            "Dedicated builder launcher '{}' is missing or unavailable; refusing to execute build command on host",
            cfg.launcher.display()
        );
        let fail_payload = serde_json::json!({
            "build_id": job.build_id,
            "status": "failed",
            "exit_code": 127,
            "compile_duration_seconds": 0.0,
            "delivery_duration_seconds": 0.0,
            "instance_name": cfg.instance_name,
            "zone": cfg.zone,
        });
        let complete_endpoint = format!("/api/rust-builds/{}/complete", job.build_id);
        let _ = post_control_plane_json(&cfg.dashboard_url, &complete_endpoint, &fail_payload);
        return Err(missing_err);
    }
    let prep_status = Command::new(&cfg.launcher)
        .arg(&project_slug)
        .arg("prepare")
        .status()
        .map_err(|e| {
            format!(
                "Failed invoking launcher '{}' prepare: {e}",
                cfg.launcher.display()
            )
        })?;
    if !prep_status.success() {
        let prep_err = format!(
            "Dedicated builder launcher '{}' prepare failed with status {prep_status}",
            cfg.launcher.display()
        );
        let fail_payload = serde_json::json!({
            "build_id": job.build_id,
            "status": "failed",
            "exit_code": prep_status.code().unwrap_or(1),
            "compile_duration_seconds": 0.0,
            "delivery_duration_seconds": 0.0,
            "instance_name": cfg.instance_name,
            "zone": cfg.zone,
        });
        let complete_endpoint = format!("/api/rust-builds/{}/complete", job.build_id);
        let _ = post_control_plane_json(&cfg.dashboard_url, &complete_endpoint, &fail_payload);
        return Err(prep_err);
    }
    fs::create_dir_all(&workspace).map_err(|e| {
        format!(
            "Failed creating workspace directory '{}': {e}",
            workspace.display()
        )
    })?;

    let build_stage_dir = cfg.out_dir.join("builds").join(job.build_id.to_string());
    fs::create_dir_all(&build_stage_dir).map_err(|e| {
        format!(
            "Failed creating build stage dir '{}': {e}",
            build_stage_dir.display()
        )
    })?;
    let raw_log_path = build_stage_dir.join("raw-build.log");

    // 2. Materialize repository checkout at the requested commit_sha before executing build
    if let Err(git_err) = materialize_repository_checkout(&workspace, &job) {
        let _ = fs::write(
            &raw_log_path,
            format!(
                "=== Build #{} ({} @ {} / {}) ===\nFATAL: {git_err}\n",
                job.build_id, job.repo, job.branch, job.commit_sha
            ),
        );
        let fail_payload = serde_json::json!({
            "build_id": job.build_id,
            "status": "failed",
            "exit_code": 128,
            "compile_duration_seconds": 0.0,
            "delivery_duration_seconds": 0.0,
            "instance_name": cfg.instance_name,
            "zone": cfg.zone,
        });
        let complete_endpoint = format!("/api/rust-builds/{}/complete", job.build_id);
        let _ = post_control_plane_json(&cfg.dashboard_url, &complete_endpoint, &fail_payload);
        return Err(git_err);
    }

    // 3. Execute the queued build/test command strictly via the dedicated builder launcher
    // Release the pre-checkout active lock immediately before handing off to `cfg.launcher`
    // so `docker-admission` inside the launcher can acquire its exclusive active lock.
    drop(project_locks);
    let start_instant = std::time::Instant::now();
    let cmd_tokens: Vec<&str> = job.command.split_whitespace().collect();
    if cmd_tokens.is_empty() {
        return Err("Claimed build command is empty".to_string());
    }
    let cmd_output = Command::new(&cfg.launcher)
        .arg(&project_slug)
        .args(&cmd_tokens)
        .current_dir(&workspace)
        .output()
        .map_err(|e| {
            format!(
                "Failed executing launcher '{}': {e}",
                cfg.launcher.display()
            )
        })?;
    let compile_duration_seconds = start_instant.elapsed().as_secs_f64();
    let exit_code = cmd_output.status.code().unwrap_or(1);

    let mut combined_log =
        Vec::with_capacity(cmd_output.stdout.len() + cmd_output.stderr.len() + 128);
    combined_log.extend_from_slice(
        format!(
            "=== Build #{} ({} @ {} / {}) ===\n$ {}\n",
            job.build_id, job.repo, job.branch, job.commit_sha, job.command
        )
        .as_bytes(),
    );
    combined_log.extend_from_slice(&cmd_output.stdout);
    if !cmd_output.stderr.is_empty() {
        if !combined_log.ends_with(b"\n") {
            combined_log.push(b'\n');
        }
        combined_log.extend_from_slice(&cmd_output.stderr);
    }
    fs::write(&raw_log_path, &combined_log)
        .map_err(|e| format!("Failed writing raw build log: {e}"))?;

    // 4. Always package the 3 artifacts (build.log, binaries.tar.zst, target.tar.zst)
    let delivery_start = std::time::Instant::now();
    let resolved_target_dir = if launcher_target_dir.is_dir() {
        Some(launcher_target_dir)
    } else {
        None
    };
    let effective_profile =
        resolve_cargo_target_profile(job.profile.as_deref(), Some(&job.command));
    let pkg_cfg = BuildPackageConfig {
        build_id: job.build_id,
        workspace: workspace.clone(),
        target_dir: resolved_target_dir,
        log_file: Some(raw_log_path.clone()),
        out_dir: cfg.out_dir.clone(),
        gcs_bucket: job.gcs_bucket.clone(),
        profile: Some(effective_profile),
        binary_name: job.binary_name.clone(),
        upload: cfg.upload,
    };
    let manifest = match package_build_artifacts(&pkg_cfg) {
        Ok(m) => m,
        Err(upload_err) => {
            // Preserve upload/delivery failure: report failed status without advertising nonexistent remote artifacts
            let fail_payload = serde_json::json!({
                "build_id": job.build_id,
                "status": "failed",
                "exit_code": if exit_code != 0 { exit_code } else { 74 },
                "compile_duration_seconds": compile_duration_seconds,
                "delivery_duration_seconds": delivery_start.elapsed().as_secs_f64(),
                "instance_name": cfg.instance_name,
                "zone": cfg.zone,
            });
            let complete_endpoint = format!("/api/rust-builds/{}/complete", job.build_id);
            let _ = post_control_plane_json(&cfg.dashboard_url, &complete_endpoint, &fail_payload);
            return Err(upload_err);
        }
    };
    let delivery_duration_seconds = delivery_start.elapsed().as_secs_f64();

    // 5. Report completion back to `/api/rust-builds/{id}/complete`
    let completion_status = if exit_code == 0 {
        "completed"
    } else {
        "failed"
    };
    let complete_payload = serde_json::json!({
        "build_id": job.build_id,
        "status": completion_status,
        "exit_code": exit_code,
        "compile_duration_seconds": compile_duration_seconds,
        "delivery_duration_seconds": delivery_duration_seconds,
        "instance_name": cfg.instance_name,
        "zone": cfg.zone,
        "log_url": manifest.log_url,
        "binary_url": manifest.binary_url,
        "target_zip_url": manifest.target_zip_url,
        "log_size_bytes": manifest.log_size_bytes,
        "binary_size_bytes": manifest.binary_size_bytes,
        "target_zip_size_bytes": manifest.target_zip_size_bytes,
        "log_sha256": manifest.log_sha256,
        "binary_sha256": manifest.binary_sha256,
        "target_zip_sha256": manifest.target_zip_sha256,
    });
    let complete_endpoint = format!("/api/rust-builds/{}/complete", job.build_id);
    let _ = post_control_plane_json(&cfg.dashboard_url, &complete_endpoint, &complete_payload)?;

    Ok(QueueWorkerCycleResult {
        claimed: true,
        build_id: Some(job.build_id),
        status: completion_status.to_string(),
        exit_code: Some(exit_code),
        manifest: Some(manifest),
    })
}

pub fn run_worker(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let Some(cfg) = parse_worker_args(args)? else {
        return Ok(0);
    };

    loop {
        let cycle = process_next_queued_build(&cfg)?;
        if cfg.once {
            let out = serde_json::to_string_pretty(&cycle)
                .map_err(|e| format!("JSON serialization error: {e}"))?;
            println!("{out}");
            return Ok(0);
        }
        if !cycle.claimed {
            std::thread::sleep(std::time::Duration::from_secs(cfg.poll_interval_secs));
        }
    }
}
