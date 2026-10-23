//! Three-Artifact Same-Region GCS Packager for Dedicated Rust Builders.
//!
//! Before a Spot `builder-*` VM finishes a queued job or shuts down, it must
//! always package and upload three separate artifacts for the build ID:
//! 1. `build.log`: stdout/stderr compilation and test output log.
//! 2. `binaries.tar.zst`: compressed archive of compiled executable binaries
//!    from `target/{release,debug}/`.
//! 3. `target.tar.zst`: compressed archive of the `target/` directory and `build.log`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_GCS_BUCKET: &str = "example-rust-project-build-artifacts";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildPackageConfig {
    pub build_id: u64,
    pub workspace: PathBuf,
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

fn is_candidate_executable(path: &Path, expected_name: Option<&str>) -> bool {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if !meta.file_type().is_file() {
        return false;
    }
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
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
    (meta.permissions().mode() & 0o111) != 0
}

fn create_compressed_archive(
    source_dir: &Path,
    entries: &[&str],
    dest_archive: &Path,
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

    let target_dir = cfg.workspace.join("target");
    let mut profile_dirs = Vec::new();
    if let Some(ref prof) = cfg.profile {
        profile_dirs.push(prof.as_str());
    }
    for default_prof in ["release", "debug"] {
        if !profile_dirs.contains(&default_prof) {
            profile_dirs.push(default_prof);
        }
    }

    let mut binaries_included = Vec::new();
    for prof in profile_dirs {
        let prof_path = target_dir.join(prof);
        if !prof_path.is_dir() {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&prof_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if is_candidate_executable(&path, cfg.binary_name.as_deref()) {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if !binaries_included.iter().any(|b| b == name) {
                            let dest_bin = bin_stage_dir.join(name);
                            fs::copy(&path, &dest_bin).map_err(|e| {
                                format!("Failed copying binary '{}': {e}", path.display())
                            })?;
                            binaries_included.push(name.to_string());
                        }
                    }
                }
            }
        }
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
    create_compressed_archive(&bin_stage_dir, &["."], &dest_binaries_archive)?;
    let _ = fs::remove_dir_all(&bin_stage_dir);

    // 3. Always stage Artifact #3: `target.tar.zst` (full target/ directory + build.log)
    let dest_target_archive = build_stage_dir.join("target.tar.zst");
    if target_dir.is_dir() {
        // Copy build.log into target/build.log so target.tar.zst is self-contained with logs + build outputs
        let _ = fs::copy(&dest_log, target_dir.join("build.log"));
        create_compressed_archive(&cfg.workspace, &["target"], &dest_target_archive)?;
    } else {
        create_compressed_archive(&build_stage_dir, &["build.log"], &dest_target_archive)?;
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
        let status = Command::new("gcloud")
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
            .map_err(|e| format!("Failed to invoke gcloud storage cp: {e}"))?;
        if !status.success() {
            return Err(format!(
                "gcloud storage cp failed with status {status} uploading to {gcs_prefix}/"
            ));
        }
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
            return serde_json::from_str(&raw)
                .map_err(|e| format!("Invalid JSON in '{}': {e}", resp_file.display()));
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
    if let Ok(token) = std::env::var("SPOT_BEARER_TOKEN") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            cmd.arg("-H")
                .arg(format!("Authorization: Bearer {trimmed}"));
        }
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
    serde_json::from_str(&stdout).map_err(|e| format!("Invalid JSON response from {url}: {e}"))
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

    let project_slug = sanitize_project_slug(&job.repo)?;
    let workspace = cfg.sources_root.join(&project_slug);

    // 1. Prepare builder workspace (via host-launcher if present, else create directory directly)
    if cfg.launcher.exists() {
        let _ = Command::new(&cfg.launcher)
            .arg(&project_slug)
            .arg("prepare")
            .status();
    }
    fs::create_dir_all(&workspace).map_err(|e| {
        format!(
            "Failed creating workspace directory '{}': {e}",
            workspace.display()
        )
    })?;

    // 2. If workspace contains a Git checkout, fetch/checkout the requested commit_sha best-effort
    if workspace.join(".git").exists() {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&workspace)
            .args(["fetch", "--all", "--quiet"])
            .status();
        let _ = Command::new("git")
            .arg("-C")
            .arg(&workspace)
            .args(["checkout", "--force", &job.commit_sha])
            .status();
    }

    // 3. Execute the queued build/test command via launcher (or sh -c fallback when testing without launcher)
    let build_stage_dir = cfg.out_dir.join("builds").join(job.build_id.to_string());
    fs::create_dir_all(&build_stage_dir).map_err(|e| {
        format!(
            "Failed creating build stage dir '{}': {e}",
            build_stage_dir.display()
        )
    })?;
    let raw_log_path = build_stage_dir.join("raw-build.log");

    let start_instant = std::time::Instant::now();
    let cmd_output = if cfg.launcher.exists() {
        Command::new(&cfg.launcher)
            .arg(&project_slug)
            .arg("bash")
            .arg("-lc")
            .arg(&job.command)
            .current_dir(&workspace)
            .output()
            .map_err(|e| {
                format!(
                    "Failed executing launcher '{}': {e}",
                    cfg.launcher.display()
                )
            })?
    } else {
        Command::new("sh")
            .arg("-c")
            .arg(&job.command)
            .current_dir(&workspace)
            .output()
            .map_err(|e| format!("Failed executing build command: {e}"))?
    };
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
    let pkg_cfg = BuildPackageConfig {
        build_id: job.build_id,
        workspace: workspace.clone(),
        log_file: Some(raw_log_path.clone()),
        out_dir: cfg.out_dir.clone(),
        gcs_bucket: job.gcs_bucket.clone(),
        profile: job.profile.clone(),
        binary_name: job.binary_name.clone(),
        upload: cfg.upload,
    };
    let manifest = match package_build_artifacts(&pkg_cfg) {
        Ok(m) => m,
        Err(_upload_err) if cfg.upload => {
            // Fall back to local staging if gcloud upload is unavailable so completion still reports
            let mut fallback_cfg = pkg_cfg;
            fallback_cfg.upload = false;
            package_build_artifacts(&fallback_cfg)?
        }
        Err(e) => return Err(e),
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
