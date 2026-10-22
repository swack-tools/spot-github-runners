use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::platform::{AggregateCounters, DefaultSystemInspector, SystemInspector};

pub const ALLOWED_COMMANDS: &[&str] = &[
    "cargo",
    "rustc",
    "rustfmt",
    "clippy-driver",
    "just",
    "uv",
    "python3",
    "cbindgen",
];

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("Bounded JSON worktree specification required (< 32 KiB)")]
    PayloadTooLarge,
    #[error("Exactly two aggregate worktrees required")]
    WrongJobCount,
    #[error("Worktrees and targets must be distinct")]
    DuplicateWorktree,
    #[error("Invalid worktree identity: {0}")]
    InvalidIdentity(String),
    #[error("Missing worktree source: {0}")]
    MissingSource(String),
    #[error("Symlinked worktree/cache boundary: {0}")]
    SymlinkedPath(String),
    #[error("Unsupported/oversized command argv")]
    InvalidArgv,
    #[error("Worktree target override forbidden")]
    TargetOverrideForbidden,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorktreeJob {
    pub worktree_id: String,
    pub argv: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChildRecord {
    pub worktree_id: String,
    pub executable: String,
    pub argv_sha256: String,
    pub pid: u32,
    pub target: String,
    pub started_monotonic_ns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reaped_monotonic_ns: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AggregateWorktreeResult {
    pub schema: String,
    pub qualification: bool,
    pub children: Vec<ChildRecord>,
    pub exit_codes: Vec<i32>,
    pub spawn_error: Option<String>,
    pub overlap_observed: bool,
    pub overlap_probe_start_ns: u64,
    pub overlap_probe_end_ns: u64,
    pub aggregate_before: Option<AggregateCounters>,
    pub aggregate_after: Option<AggregateCounters>,
    pub complete: bool,
    pub gaps: Vec<String>,
}

pub fn validate_worktree_id(name: &str) -> Result<(), WorktreeError> {
    let re = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$").unwrap();
    if re.is_match(name) {
        Ok(())
    } else {
        Err(WorktreeError::InvalidIdentity(name.to_string()))
    }
}

pub fn checked_worktree_boundary(path: &Path) -> Result<(), WorktreeError> {
    if let Ok(canon) = path.canonicalize() {
        if canon != path {
            return Err(WorktreeError::SymlinkedPath(
                path.to_string_lossy().to_string(),
            ));
        }
    }
    let mut curr = Some(path);
    while let Some(p) = curr {
        if let Ok(meta) = fs::symlink_metadata(p) {
            if meta.file_type().is_symlink() {
                return Err(WorktreeError::SymlinkedPath(
                    p.to_string_lossy().to_string(),
                ));
            }
        }
        curr = p.parent();
    }
    Ok(())
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

pub fn run_worktree_jobs(
    jobs: &[WorktreeJob],
    sys: &dyn SystemInspector,
    src_root: Option<&Path>,
    target_root: Option<&Path>,
) -> Result<AggregateWorktreeResult, WorktreeError> {
    if jobs.len() != 2 {
        return Err(WorktreeError::WrongJobCount);
    }

    let mut names = HashSet::new();
    for job in jobs {
        validate_worktree_id(&job.worktree_id)?;
        if !names.insert(&job.worktree_id) {
            return Err(WorktreeError::DuplicateWorktree);
        }

        if job.argv.is_empty() || job.argv.len() > 256 {
            return Err(WorktreeError::InvalidArgv);
        }

        let cmd = &job.argv[0];
        if !ALLOWED_COMMANDS.contains(&cmd.as_str()) {
            return Err(WorktreeError::InvalidArgv);
        }

        for arg in &job.argv {
            if arg.len() > 16384 || arg.contains('\0') {
                return Err(WorktreeError::InvalidArgv);
            }
            if arg == "--target-dir" || arg.starts_with("--target-dir=") {
                return Err(WorktreeError::TargetOverrideForbidden);
            }
        }

        let base_src = src_root.unwrap_or_else(|| Path::new("/src/worktrees"));
        let base_tgt = target_root.unwrap_or_else(|| Path::new("/target"));
        let source_dir = base_src.join(&job.worktree_id);
        let target_dir = base_tgt.join(&job.worktree_id);

        checked_worktree_boundary(&source_dir)?;
        checked_worktree_boundary(&target_dir)?;

        if !source_dir.is_dir() {
            return Err(WorktreeError::MissingSource(
                source_dir.to_string_lossy().to_string(),
            ));
        }
    }

    let mut gaps = Vec::new();
    let before = match sys.read_cgroup_counters() {
        Ok(c) => Some(c),
        Err(e) => {
            gaps.push(format!("before:{e}"));
            None
        }
    };

    let mut children: Vec<Child> = Vec::new();
    let mut records: Vec<ChildRecord> = Vec::new();
    let mut spawn_error: Option<String> = None;

    let base_src = src_root.unwrap_or_else(|| Path::new("/src/worktrees"));
    let base_tgt = target_root.unwrap_or_else(|| Path::new("/target"));

    for job in jobs {
        let name = &job.worktree_id;
        let target = base_tgt.join(name);
        let _ = fs::create_dir_all(&target);

        let mut hasher = Sha256::new();
        let argv_json = serde_json::to_string(&job.argv).unwrap_or_default();
        hasher.update(argv_json.as_bytes());
        let argv_sha256 = hex::encode(hasher.finalize());

        let started_ns = now_ns();
        let child_res = Command::new(&job.argv[0])
            .args(&job.argv[1..])
            .current_dir(base_src.join(name))
            .env("CARGO_HOME", "/cargo")
            .env("CARGO_TARGET_DIR", &target)
            .env(
                "ZIG_LOCAL_CACHE_DIR",
                target.join(".zig-cache").to_string_lossy().as_ref(),
            )
            .spawn();

        match child_res {
            Ok(child) => {
                records.push(ChildRecord {
                    worktree_id: name.clone(),
                    executable: job.argv[0].clone(),
                    argv_sha256,
                    pid: child.id(),
                    target: target.to_string_lossy().to_string(),
                    started_monotonic_ns: started_ns,
                    reaped_monotonic_ns: None,
                });
                children.push(child);
            }
            Err(e) => {
                spawn_error = Some(e.to_string());
                break;
            }
        }
    }

    let probe_start_ns = now_ns();
    let overlap = if children.len() == 2 {
        let p0_first = children[0].try_wait().ok().flatten().is_none();
        let p1 = children[1].try_wait().ok().flatten().is_none();
        let p0_second = children[0].try_wait().ok().flatten().is_none();
        p0_first && p1 && p0_second
    } else {
        false
    };
    let probe_end_ns = now_ns();

    let mut exit_codes = Vec::new();
    for (i, child) in children.iter_mut().enumerate() {
        let status = child.wait().ok();
        records[i].reaped_monotonic_ns = Some(now_ns());
        exit_codes.push(status.and_then(|s| s.code()).unwrap_or(-1));
    }

    let after = match sys.read_cgroup_counters() {
        Ok(c) => Some(c),
        Err(e) => {
            gaps.push(format!("after:{e}"));
            None
        }
    };

    let complete = gaps.is_empty();

    Ok(AggregateWorktreeResult {
        schema: "aggregate-worktree-commands-v1".to_string(),
        qualification: false,
        children: records,
        exit_codes,
        spawn_error,
        overlap_observed: overlap,
        overlap_probe_start_ns: probe_start_ns,
        overlap_probe_end_ns: probe_end_ns,
        aggregate_before: before,
        aggregate_after: after,
        complete,
        gaps,
    })
}

pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let sys = DefaultSystemInspector::default();
    if args.is_empty() || args[0].len() > 32768 {
        eprintln!(
            "host-agent worktree-commands: Bounded JSON worktree specification required (< 32 KiB)"
        );
        return Ok(78);
    }

    let jobs: Vec<WorktreeJob> = serde_json::from_str(&args[0])?;
    let result = run_worktree_jobs(&jobs, &sys, None, None)?;

    println!("{}", serde_json::to_string_pretty(&result)?);

    if result.spawn_error.is_some() || result.exit_codes.len() != 2 {
        return Ok(78);
    }

    for code in result.exit_codes {
        if code != 0 {
            return Ok(code);
        }
    }

    Ok(0)
}
