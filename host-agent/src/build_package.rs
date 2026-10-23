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
