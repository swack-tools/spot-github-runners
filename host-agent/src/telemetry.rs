//! Host & Container Telemetry Collector, Workflow Log Correlator, Prometheus Exporter,
//! and Graceful Dynamic Runner Slot Scaler (`host-agent telemetry`).
//!
//! Responsibilities:
//! 1. Collect host-level utilization (`/proc/meminfo`, `/proc/loadavg`, `/mnt/runner-data` statvfs,
//!    and `/var/lib/gha-admission/leases.json`).
//! 2. Collect per-container cgroup v2 telemetry (`memory.current`, `memory.peak`, `memory.stat`
//!    `anon`/`inactive_file` working set, `memory.max`, `memory.events` `oom`/`oom_kill`, `cpu.stat`).
//! 3. Correlate runner containers and log lines with active and recent GitHub Actions workflows
//!    by parsing `/mnt/runner-data/instances/*/_diag/Worker_*.log` and `Runner_*.log` to extract
//!    `organization`, `repository`, `workflow_name`, `job_name`, `run_id`, `run_number`,
//!    `job_id` (`check_run_id`), and `github_workflow_url`.
//! 4. Compute dynamic runner slot density (`recommended_runners`) based on historical peak
//!    working-set memory across all workflows and CPU load, guarded against OOM and swap pressure.
//! 5. Serve Prometheus `/metrics` and JSON `/status` on loopback (`127.0.0.1:9464`) for
//!    `google-cloud-ops-agent` scraping and emit structured JSONL logs to
//!    `/var/log/gha-host-telemetry.jsonl`.
//! 6. Gracefully scale up additional runner container slots (`scale-runners`) in-place without
//!    interrupting or restarting active CI jobs.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_METRICS_ADDR: &str = "127.0.0.1:9464";
pub const DEFAULT_INSTANCES_DIR: &str = "/mnt/runner-data/instances";
pub const DEFAULT_TELEMETRY_LOG_PATH: &str = "/var/log/gha-host-telemetry.jsonl";
pub const DEFAULT_PEAK_LEDGER_PATH: &str = "/var/lib/gha-admission/peak-telemetry.json";
pub const DEFAULT_TARGET_CPU_PCT: f64 = 80.0;
pub const DEFAULT_TARGET_MEM_PCT: f64 = 80.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowJobCorrelation {
    pub runner_name: String,
    pub organization: String,
    pub repository: String,
    pub workflow_name: String,
    pub job_name: String,
    pub run_id: String,
    pub run_number: String,
    pub job_id: String,
    pub github_workflow_url: String,
    pub commit_sha: String,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CorrelatedLogEntry {
    pub timestamp: String,
    pub epoch: u64,
    pub severity: String,
    pub source: String,
    pub instance_name: String,
    pub zone: String,
    pub role: String,
    pub runner_name: String,
    pub container_id: String,
    pub organization: String,
    pub repository: String,
    pub workflow_name: String,
    pub job_name: String,
    pub run_id: String,
    pub job_id: String,
    pub github_workflow_url: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ContainerCgroupTelemetry {
    pub container_id: String,
    pub name: String,
    pub is_runner: bool,
    pub memory_current_bytes: u64,
    pub memory_working_set_bytes: u64,
    pub memory_anon_bytes: u64,
    pub memory_peak_bytes: u64,
    pub memory_max_bytes: u64,
    pub memory_working_set_gib: f64,
    pub memory_peak_gib: f64,
    pub oom_events: u64,
    pub oom_kill_events: u64,
    pub cpu_usage_usec: u64,
    pub cpu_throttled_usec: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_workflow: Option<WorkflowJobCorrelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DynamicDensityEvaluation {
    pub enabled: bool,
    pub current_runners: u32,
    pub baseline_runners: u32,
    pub recommended_runners: u32,
    pub can_scale_up_slots: u32,
    pub observed_peak_working_set_gib: f64,
    pub observed_peak_cgroup_gib: f64,
    pub observed_avg_cpu_cores_per_runner: f64,
    pub target_cpu_percent: f64,
    pub target_memory_percent: f64,
    pub host_usable_ram_gib: f64,
    pub oom_guard_tripped: bool,
    pub swap_guard_tripped: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostTelemetrySnapshot {
    pub schema: String,
    pub collected_at: String,
    pub collected_epoch: u64,
    pub instance_name: String,
    pub zone: String,
    pub role: String,
    pub arch: String,
    pub vcpu: u32,
    pub loadavg_1m: f64,
    pub loadavg_5m: f64,
    pub loadavg_15m: f64,
    pub cpu_utilization_pct: f64,
    pub mem_total_gib: f64,
    pub mem_used_gib: f64,
    pub mem_available_gib: f64,
    pub mem_utilization_pct: f64,
    pub swap_total_gib: f64,
    pub swap_used_gib: f64,
    pub disk_total_gib: f64,
    pub disk_used_gib: f64,
    pub disk_utilization_pct: f64,
    pub active_leases_count: usize,
    pub active_leases_gib: f64,
    pub containers: Vec<ContainerCgroupTelemetry>,
    pub workflow_correlations: Vec<WorkflowJobCorrelation>,
    pub recent_logs: Vec<CorrelatedLogEntry>,
    pub density: DynamicDensityEvaluation,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PeakTelemetryLedger {
    pub max_runner_working_set_gib: f64,
    pub max_runner_cgroup_peak_gib: f64,
    pub max_avg_cpu_cores_per_runner: f64,
    pub updated_epoch: u64,
}

fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn format_utc_iso(epoch: u64) -> String {
    // Pure-Rust civil calendar calculation from UNIX epoch
    let days = (epoch / 86400) as i64;
    let secs_of_day = epoch % 86400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, m, d, hour, minute, second
    )
}

fn round2(val: f64) -> f64 {
    (val * 100.0).round() / 100.0
}

/// Parse a GitHub Actions `_diag/Worker_*.log` file content and optional `Runner_*.log`
/// to extract the exact workflow correlation and build the GitHub Actions job deep-link.
pub fn parse_worker_log_correlation(
    runner_name: &str,
    worker_file_name: &str,
    content: &str,
) -> Option<WorkflowJobCorrelation> {
    let extract_kv = |key: &str| -> Option<String> {
        let pat = format!(
            r#""k"\s*:\s*"{}"\s*,\s*"v"\s*:\s*"([^"]*)""#,
            regex::escape(key)
        );
        let re = Regex::new(&pat).ok()?;
        re.captures(content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let repository = extract_kv("repository")?;
    let organization = extract_kv("repository_owner").unwrap_or_else(|| {
        repository
            .split('/')
            .next()
            .unwrap_or(&repository)
            .to_string()
    });
    let run_id = extract_kv("run_id").unwrap_or_default();
    if run_id.is_empty() {
        return None;
    }
    let run_number = extract_kv("run_number").unwrap_or_else(|| "1".to_string());
    let workflow_name = extract_kv("workflow").unwrap_or_else(|| "CI Workflow".to_string());
    let commit_sha = extract_kv("sha").unwrap_or_default();

    let job_name_re = Regex::new(r#""jobDisplayName"\s*:\s*"([^"]+)""#).ok()?;
    let job_name = job_name_re
        .captures(content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_else(|| "job".to_string());

    // Extract numeric check_run_id (e.g. "k": "check_run_id", "v": 120964372558.0 or "120964372558")
    let check_run_re =
        Regex::new(r#""k"\s*:\s*"check_run_id"\s*,\s*"v"\s*:\s*"?([0-9]+)(?:\.[0-9]+)?"?"#).ok()?;
    let job_id = check_run_re
        .captures(content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();

    let github_workflow_url = if !job_id.is_empty() {
        format!(
            "https://github.com/{}/actions/runs/{}/job/{}",
            repository, run_id, job_id
        )
    } else {
        format!("https://github.com/{}/actions/runs/{}", repository, run_id)
    };

    // Extract timestamps from log lines `[2026-10-24 22:44:29Z INFO ...]` or filename `Worker_20261024-224429-utc.log`
    let ts_re = Regex::new(r"\[(\d{4}-\d{2}-\d{2}) (\d{2}:\d{2}:\d{2}Z)").ok()?;
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    for cap in ts_re.captures_iter(content) {
        if let (Some(d), Some(t)) = (cap.get(1), cap.get(2)) {
            let iso = format!("{}T{}", d.as_str(), t.as_str());
            if first_ts.is_none() {
                first_ts = Some(iso.clone());
            }
            last_ts = Some(iso);
        }
    }

    let started_at = first_ts.unwrap_or_else(|| {
        let fname_re = Regex::new(r"Worker_(\d{4})(\d{2})(\d{2})-(\d{2})(\d{2})(\d{2})-utc").ok();
        if let Some(re) = fname_re {
            if let Some(c) = re.captures(worker_file_name) {
                return format!(
                    "{}-{}-{}T{}:{}:{}Z",
                    &c[1], &c[2], &c[3], &c[4], &c[5], &c[6]
                );
            }
        }
        format_utc_iso(current_epoch_secs())
    });

    let is_completed = content.contains("Worker process finished")
        || content.contains("Job completed")
        || content.contains("FinalizeJob")
        || content.contains("Cleaning up task-key");

    Some(WorkflowJobCorrelation {
        runner_name: runner_name.to_string(),
        organization,
        repository,
        workflow_name,
        job_name,
        run_id,
        run_number,
        job_id,
        github_workflow_url,
        commit_sha,
        started_at,
        completed_at: if is_completed { last_ts } else { None },
        status: if is_completed {
            "completed".to_string()
        } else {
            "in_progress".to_string()
        },
    })
}

/// Extract correlated high-signal log entries from a runner's `Worker_*.log` and `Runner_*.log`.
pub fn extract_correlated_logs_from_worker(
    instance_name: &str,
    zone: &str,
    role: &str,
    container_id: &str,
    corr: &WorkflowJobCorrelation,
    worker_content: &str,
    max_entries: usize,
) -> Vec<CorrelatedLogEntry> {
    let mut entries = Vec::new();
    let line_re = match Regex::new(
        r"^\[(\d{4}-\d{2}-\d{2}) (\d{2}:\d{2}:\d{2}Z)\s+([A-Z]+)\s+([^\]]+)\]\s*(.+)$",
    ) {
        Ok(r) => r,
        Err(_) => return entries,
    };

    let now = current_epoch_secs();
    for line in worker_content.lines().rev() {
        if entries.len() >= max_entries {
            break;
        }
        if let Some(cap) = line_re.captures(line) {
            let component = cap.get(4).map(|m| m.as_str().trim()).unwrap_or("Worker");
            let msg = cap.get(5).map(|m| m.as_str().trim()).unwrap_or("");
            if msg.is_empty()
                || msg.starts_with('{')
                || msg.starts_with('"')
                || msg.contains("\"mask\"")
            {
                continue;
            }
            let is_interesting = component == "ActionManager"
                || component == "JobDispatcher"
                || component == "Terminal"
                || component == "StepsRunner"
                || component == "Worker"
                || cap.get(3).map(|m| m.as_str()) != Some("INFO");
            if !is_interesting {
                continue;
            }
            let ts = format!("{}T{}", &cap[1], &cap[2]);
            let sev = match &cap[3] {
                "ERR" | "ERROR" | "FATAL" => "ERROR",
                "WARN" | "WARNING" => "WARN",
                _ => "INFO",
            };
            entries.push(CorrelatedLogEntry {
                timestamp: ts,
                epoch: now,
                severity: sev.to_string(),
                source: "gha_worker".to_string(),
                instance_name: instance_name.to_string(),
                zone: zone.to_string(),
                role: role.to_string(),
                runner_name: corr.runner_name.clone(),
                container_id: container_id.to_string(),
                organization: corr.organization.clone(),
                repository: corr.repository.clone(),
                workflow_name: corr.workflow_name.clone(),
                job_name: corr.job_name.clone(),
                run_id: corr.run_id.clone(),
                job_id: corr.job_id.clone(),
                github_workflow_url: corr.github_workflow_url.clone(),
                message: format!("[{}] {}", component, msg),
            });
        }
    }
    entries.reverse();
    entries
}

/// Scan `/mnt/runner-data/instances/*/_diag/Worker_*.log` to discover active & recent workflows
/// and extract correlated log entries.
pub fn scan_instances_dir_for_workflows(
    instances_dir: &Path,
    instance_name: &str,
    zone: &str,
    role: &str,
    container_ids_by_runner: &BTreeMap<String, String>,
) -> (Vec<WorkflowJobCorrelation>, Vec<CorrelatedLogEntry>) {
    let mut correlations = Vec::new();
    let mut logs = Vec::new();

    let read_dir = match fs::read_dir(instances_dir) {
        Ok(d) => d,
        Err(_) => return (correlations, logs),
    };

    let mut runner_dirs: Vec<PathBuf> = read_dir
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    runner_dirs.sort();

    for runner_dir in runner_dirs {
        let runner_name = match runner_dir.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let diag_dir = runner_dir.join("_diag");
        let diag_entries = match fs::read_dir(&diag_dir) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let mut worker_files: Vec<PathBuf> = diag_entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("Worker_") && n.ends_with(".log"))
                    .unwrap_or(false)
            })
            .collect();
        worker_files.sort();

        // Inspect up to the 2 most recent Worker_*.log files per runner slot
        for wf in worker_files.iter().rev().take(2) {
            let fname = wf
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Worker.log");
            if let Ok(content) = fs::read_to_string(wf) {
                if let Some(corr) = parse_worker_log_correlation(&runner_name, fname, &content) {
                    let cid = container_ids_by_runner
                        .get(&runner_name)
                        .cloned()
                        .unwrap_or_else(|| "host".to_string());
                    let mut wf_logs = extract_correlated_logs_from_worker(
                        instance_name,
                        zone,
                        role,
                        &cid,
                        &corr,
                        &content,
                        8,
                    );
                    logs.append(&mut wf_logs);
                    correlations.push(corr);
                    break;
                }
            }
        }
    }

    (correlations, logs)
}

/// Compute dynamic runner slot density from observed host and container metrics.
pub fn evaluate_dynamic_runner_density(
    enabled: bool,
    vcpu: u32,
    mem_total_gib: f64,
    host_overhead_gib: f64,
    swap_used_gib: f64,
    current_runners: u32,
    baseline_runners: u32,
    observed_peak_working_set_gib: f64,
    observed_peak_cgroup_gib: f64,
    observed_avg_cpu_cores_per_runner: f64,
    total_oom_kills: u64,
    target_cpu_percent: f64,
    target_memory_percent: f64,
) -> DynamicDensityEvaluation {
    let baseline = baseline_runners.max(1);
    let current = current_runners.max(baseline);
    let usable_ram_gib = (mem_total_gib - host_overhead_gib).max(2.0)
        * (target_memory_percent.clamp(40.0, 95.0) / 100.0);
    let oom_guard = total_oom_kills > 0;
    let swap_guard = swap_used_gib >= 0.5;

    if !enabled {
        return DynamicDensityEvaluation {
            enabled: false,
            current_runners: current_runners,
            baseline_runners: baseline,
            recommended_runners: baseline,
            can_scale_up_slots: 0,
            observed_peak_working_set_gib: round2(observed_peak_working_set_gib),
            observed_peak_cgroup_gib: round2(observed_peak_cgroup_gib),
            observed_avg_cpu_cores_per_runner: round2(observed_avg_cpu_cores_per_runner),
            target_cpu_percent,
            target_memory_percent,
            host_usable_ram_gib: round2(usable_ram_gib),
            oom_guard_tripped: oom_guard,
            swap_guard_tripped: swap_guard,
            reason: "Dynamic runner density scaling is opted out (disabled)".to_string(),
        };
    }

    if oom_guard || swap_guard {
        return DynamicDensityEvaluation {
            enabled: true,
            current_runners,
            baseline_runners: baseline,
            recommended_runners: baseline,
            can_scale_up_slots: 0,
            observed_peak_working_set_gib: round2(observed_peak_working_set_gib),
            observed_peak_cgroup_gib: round2(observed_peak_cgroup_gib),
            observed_avg_cpu_cores_per_runner: round2(observed_avg_cpu_cores_per_runner),
            target_cpu_percent,
            target_memory_percent,
            host_usable_ram_gib: round2(usable_ram_gib),
            oom_guard_tripped: oom_guard,
            swap_guard_tripped: swap_guard,
            reason: if oom_guard {
                "Held at baseline: cgroup/host OOM kill detected".to_string()
            } else {
                "Held at baseline: host swap pressure detected".to_string()
            },
        };
    }

    // Reserve a 25% safety margin above observed peak working-set RAM per runner (floor 1.5 GiB)
    let effective_mem_per_runner = (observed_peak_working_set_gib.max(1.2) * 1.25).max(1.5);
    let mem_bound_slots = (usable_ram_gib / effective_mem_per_runner).floor() as u32;

    // Target CPU utilization (e.g. 80% of vCPUs) divided by observed CPU cores per runner (floor 1.15 cores)
    let usable_vcpu = (vcpu.max(1) as f64) * (target_cpu_percent.clamp(40.0, 95.0) / 100.0);
    let effective_cpu_per_runner = observed_avg_cpu_cores_per_runner.max(1.15);
    let cpu_bound_slots = (usable_vcpu / effective_cpu_per_runner).round() as u32;

    // Max ceiling: at most 1 runner per 1.5 vCPUs (e.g. up to 2-3 on 4-vCPU ARM, up to 8 on 16-vCPU x86)
    let max_ceiling = ((vcpu as f64 / 1.5).floor() as u32).clamp(2, 10);

    let recommended = mem_bound_slots
        .min(cpu_bound_slots)
        .clamp(baseline, max_ceiling)
        .max(current);

    let can_scale_up = recommended.saturating_sub(current_runners);
    let reason = if can_scale_up > 0 {
        format!(
            "Underutilized host (peak {:.2} GiB/runner, {:.2} cores/runner): can fit {} runners (+{} extra slots)",
            observed_peak_working_set_gib,
            observed_avg_cpu_cores_per_runner,
            recommended,
            can_scale_up
        )
    } else {
        format!(
            "Optimal runner density reached ({} active slots at {:.2} GiB peak/runner)",
            current_runners, observed_peak_working_set_gib
        )
    };

    DynamicDensityEvaluation {
        enabled: true,
        current_runners,
        baseline_runners: baseline,
        recommended_runners: recommended,
        can_scale_up_slots: can_scale_up,
        observed_peak_working_set_gib: round2(observed_peak_working_set_gib),
        observed_peak_cgroup_gib: round2(observed_peak_cgroup_gib),
        observed_avg_cpu_cores_per_runner: round2(observed_avg_cpu_cores_per_runner),
        target_cpu_percent,
        target_memory_percent,
        host_usable_ram_gib: round2(usable_ram_gib),
        oom_guard_tripped: false,
        swap_guard_tripped: false,
        reason,
    }
}

fn read_loadavg() -> (f64, f64, f64) {
    if let Ok(raw) = fs::read_to_string("/proc/loadavg") {
        let parts: Vec<&str> = raw.split_whitespace().collect();
        if parts.len() >= 3 {
            let l1 = parts[0].parse::<f64>().unwrap_or(0.0);
            let l5 = parts[1].parse::<f64>().unwrap_or(0.0);
            let l15 = parts[2].parse::<f64>().unwrap_or(0.0);
            return (round2(l1), round2(l5), round2(l15));
        }
    }
    (0.5, 0.5, 0.5)
}

fn read_meminfo_full() -> (f64, f64, f64, f64) {
    // Returns (total_gib, available_gib, swap_total_gib, swap_free_gib)
    if let Ok(raw) = fs::read_to_string("/proc/meminfo") {
        let mut total_kib = 0u64;
        let mut avail_kib = 0u64;
        let mut swap_total_kib = 0u64;
        let mut swap_free_kib = 0u64;
        for line in raw.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                total_kib = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
                avail_kib = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("SwapTotal:") {
                swap_total_kib = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("SwapFree:") {
                swap_free_kib = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            }
        }
        let gib = 1024.0 * 1024.0;
        if total_kib > 0 {
            return (
                total_kib as f64 / gib,
                avail_kib as f64 / gib,
                swap_total_kib as f64 / gib,
                swap_free_kib as f64 / gib,
            );
        }
    }
    (32.0, 26.0, 0.0, 0.0)
}

fn read_disk_usage_gib(path: &Path) -> (f64, f64, f64) {
    let target = if path.exists() { path } else { Path::new("/") };
    if let Ok(stat) = nix::sys::statvfs::statvfs(target) {
        let bsize = stat.fragment_size() as u64;
        let total_bytes = (stat.blocks() as u64).saturating_mul(bsize);
        let avail_bytes = (stat.blocks_available() as u64).saturating_mul(bsize);
        let used_bytes = total_bytes.saturating_sub(avail_bytes);
        let gib = (1024.0_f64).powi(3);
        let total_gib = total_bytes as f64 / gib;
        let used_gib = used_bytes as f64 / gib;
        let pct = if total_gib > 0.0 {
            (used_gib / total_gib * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        return (round2(total_gib), round2(used_gib), round2(pct));
    }
    (100.0, 18.0, 18.0)
}

fn find_container_cgroup_dir(container_id: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(format!(
        "/sys/fs/cgroup/system.slice/docker-{}.scope",
        container_id
    ));
    if direct.exists() {
        return Some(direct);
    }
    if let Ok(entries) = fs::read_dir("/sys/fs/cgroup/system.slice") {
        for e in entries.flatten() {
            let name = e.file_name();
            let s = name.to_string_lossy();
            if s.contains(container_id) && e.path().is_dir() {
                return Some(e.path());
            }
        }
    }
    None
}

fn read_container_cgroup_metrics(
    container_id: &str,
    name: &str,
    active_workflow: Option<WorkflowJobCorrelation>,
) -> ContainerCgroupTelemetry {
    let clean_name = name.trim_start_matches('/').to_string();
    let is_runner = clean_name.starts_with("gha-")
        || clean_name.starts_with("oxidex-")
        || clean_name.contains("-runners-");
    let mut telem = ContainerCgroupTelemetry {
        container_id: container_id.chars().take(12).collect(),
        name: clean_name,
        is_runner,
        active_workflow,
        ..Default::default()
    };

    if let Some(cg) = find_container_cgroup_dir(container_id) {
        if let Ok(s) = fs::read_to_string(cg.join("memory.current")) {
            telem.memory_current_bytes = s.trim().parse().unwrap_or(0);
        }
        if let Ok(s) = fs::read_to_string(cg.join("memory.peak")) {
            telem.memory_peak_bytes = s.trim().parse().unwrap_or(telem.memory_current_bytes);
        }
        if let Ok(s) = fs::read_to_string(cg.join("memory.max")) {
            telem.memory_max_bytes = s.trim().parse().unwrap_or(0);
        }
        let mut inactive_file = 0u64;
        if let Ok(stat_s) = fs::read_to_string(cg.join("memory.stat")) {
            for line in stat_s.lines() {
                if let Some(v) = line.strip_prefix("anon ") {
                    telem.memory_anon_bytes = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = line.strip_prefix("inactive_file ") {
                    inactive_file = v.trim().parse().unwrap_or(0);
                }
            }
        }
        telem.memory_working_set_bytes = telem.memory_current_bytes.saturating_sub(inactive_file);
        if let Ok(ev_s) = fs::read_to_string(cg.join("memory.events")) {
            for line in ev_s.lines() {
                if let Some(v) = line.strip_prefix("oom ") {
                    telem.oom_events = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = line.strip_prefix("oom_kill ") {
                    telem.oom_kill_events = v.trim().parse().unwrap_or(0);
                }
            }
        }
        if let Ok(cpu_s) = fs::read_to_string(cg.join("cpu.stat")) {
            for line in cpu_s.lines() {
                if let Some(v) = line.strip_prefix("usage_usec ") {
                    telem.cpu_usage_usec = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = line.strip_prefix("throttled_usec ") {
                    telem.cpu_throttled_usec = v.trim().parse().unwrap_or(0);
                }
            }
        }
    }

    let gib = (1024.0_f64).powi(3);
    telem.memory_working_set_gib = round2(telem.memory_working_set_bytes as f64 / gib);
    telem.memory_peak_gib = round2(telem.memory_peak_bytes as f64 / gib);
    telem
}

fn query_gce_metadata(path: &str) -> Option<String> {
    let out = Command::new("/usr/bin/curl")
        .args([
            "--fail",
            "--silent",
            "--max-time",
            "2",
            "-H",
            "Metadata-Flavor: Google",
            &format!("http://metadata.google.internal/computeMetadata/v1/{path}"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

pub fn collect_host_telemetry(
    instance_override: Option<&str>,
    zone_override: Option<&str>,
    instances_dir: &Path,
    peak_ledger_path: &Path,
) -> HostTelemetrySnapshot {
    let now = current_epoch_secs();
    let instance_name = instance_override
        .map(str::to_string)
        .or_else(|| query_gce_metadata("instance/name"))
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "runner-host".to_string());

    let zone = zone_override
        .map(str::to_string)
        .or_else(|| {
            query_gce_metadata("instance/zone")
                .map(|z| z.rsplit('/').next().unwrap_or(&z).to_string())
        })
        .unwrap_or_else(|| "us-central1-a".to_string());

    let role = if instance_name.starts_with("builder-")
        || Path::new("/etc/oxidex-remote-build.json").exists()
    {
        "rust_builder".to_string()
    } else {
        "cicd_runner".to_string()
    };

    let arch = if cfg!(target_arch = "aarch64") {
        "arm64".to_string()
    } else {
        "x86_64".to_string()
    };

    let vcpu = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(16);
    let (loadavg_1m, loadavg_5m, loadavg_15m) = read_loadavg();
    let cpu_utilization_pct = round2(((loadavg_1m / vcpu.max(1) as f64) * 100.0).clamp(0.0, 100.0));

    let (mem_total_gib, mem_available_gib, swap_total_gib, swap_free_gib) = read_meminfo_full();
    let mem_used_gib = (mem_total_gib - mem_available_gib).max(0.0);
    let mem_utilization_pct = if mem_total_gib > 0.0 {
        round2((mem_used_gib / mem_total_gib * 100.0).clamp(0.0, 100.0))
    } else {
        0.0
    };
    let swap_used_gib = (swap_total_gib - swap_free_gib).max(0.0);

    let (disk_total_gib, disk_used_gib, disk_utilization_pct) =
        read_disk_usage_gib(Path::new("/mnt/runner-data"));

    // Read admission leases if present
    let (active_leases_count, active_leases_gib) =
        if let Ok(raw) = fs::read_to_string("/var/lib/gha-admission/leases.json") {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(obj) = val.as_object() {
                    let sum_gib: f64 = obj
                        .values()
                        .filter_map(|v| v.get("gib").and_then(|g| g.as_f64()))
                        .sum();
                    (obj.len(), round2(sum_gib))
                } else {
                    (0, 0.0)
                }
            } else {
                (0, 0.0)
            }
        } else {
            (0, 0.0)
        };

    // Enumerate running Docker containers (`id|name`)
    let mut docker_containers: Vec<(String, String)> = Vec::new();
    let mut container_ids_by_runner: BTreeMap<String, String> = BTreeMap::new();
    let docker_bin = if Path::new("/usr/bin/docker").exists() {
        "/usr/bin/docker"
    } else {
        "docker"
    };
    if let Ok(out) = Command::new(docker_bin)
        .args(["ps", "--no-trunc", "--format", "{{.ID}}|{{.Names}}"])
        .output()
    {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some((cid, cname)) = line.trim().split_once('|') {
                    let clean_name = cname.trim_start_matches('/').to_string();
                    container_ids_by_runner
                        .insert(clean_name.clone(), cid.chars().take(12).collect());
                    docker_containers.push((cid.to_string(), clean_name));
                }
            }
        }
    }

    let (workflow_correlations, recent_logs) = scan_instances_dir_for_workflows(
        instances_dir,
        &instance_name,
        &zone,
        &role,
        &container_ids_by_runner,
    );

    let wf_by_runner: BTreeMap<String, WorkflowJobCorrelation> = workflow_correlations
        .iter()
        .map(|w| (w.runner_name.clone(), w.clone()))
        .collect();

    let mut containers = Vec::new();
    for (cid, cname) in &docker_containers {
        let wf = wf_by_runner.get(cname).cloned();
        containers.push(read_container_cgroup_metrics(cid, cname, wf));
    }

    let runner_containers: Vec<&ContainerCgroupTelemetry> =
        containers.iter().filter(|c| c.is_runner).collect();
    let current_runners = runner_containers.len() as u32;
    let current_max_ws = runner_containers
        .iter()
        .map(|c| c.memory_working_set_gib)
        .fold(0.0_f64, f64::max);
    let current_max_cg_peak = runner_containers
        .iter()
        .map(|c| c.memory_peak_gib)
        .fold(0.0_f64, f64::max);
    let total_oom_kills: u64 = containers.iter().map(|c| c.oom_kill_events).sum();

    let current_avg_cpu = if current_runners > 0 {
        loadavg_1m / (current_runners as f64)
    } else {
        1.25
    };

    // Update persistent historical peak ledger on disk
    let mut ledger: PeakTelemetryLedger = fs::read_to_string(peak_ledger_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if current_max_ws > ledger.max_runner_working_set_gib {
        ledger.max_runner_working_set_gib = current_max_ws;
    }
    if current_max_cg_peak > ledger.max_runner_cgroup_peak_gib {
        ledger.max_runner_cgroup_peak_gib = current_max_cg_peak;
    }
    if current_avg_cpu > ledger.max_avg_cpu_cores_per_runner {
        ledger.max_avg_cpu_cores_per_runner = current_avg_cpu;
    }
    ledger.updated_epoch = now;
    if let Some(parent) = peak_ledger_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(serialized) = serde_json::to_string(&ledger) {
        let _ = fs::write(peak_ledger_path, serialized);
    }

    let observed_ws_peak = ledger
        .max_runner_working_set_gib
        .max(current_max_ws)
        .max(1.25);
    let observed_cg_peak = ledger
        .max_runner_cgroup_peak_gib
        .max(current_max_cg_peak)
        .max(observed_ws_peak);
    let baseline_runners = if vcpu <= 4 { 1 } else { 2 };
    let host_overhead_gib = if vcpu <= 4 { 2.0 } else { 4.0 };

    let density = evaluate_dynamic_runner_density(
        true,
        vcpu,
        mem_total_gib,
        host_overhead_gib,
        swap_used_gib,
        current_runners,
        baseline_runners,
        observed_ws_peak,
        observed_cg_peak,
        current_avg_cpu,
        total_oom_kills,
        DEFAULT_TARGET_CPU_PCT,
        DEFAULT_TARGET_MEM_PCT,
    );

    HostTelemetrySnapshot {
        schema: "gha-host-telemetry-v1".to_string(),
        collected_at: format_utc_iso(now),
        collected_epoch: now,
        instance_name,
        zone,
        role,
        arch,
        vcpu,
        loadavg_1m,
        loadavg_5m,
        loadavg_15m,
        cpu_utilization_pct,
        mem_total_gib: round2(mem_total_gib),
        mem_used_gib: round2(mem_used_gib),
        mem_available_gib: round2(mem_available_gib),
        mem_utilization_pct,
        swap_total_gib: round2(swap_total_gib),
        swap_used_gib: round2(swap_used_gib),
        disk_total_gib,
        disk_used_gib,
        disk_utilization_pct,
        active_leases_count,
        active_leases_gib,
        containers,
        workflow_correlations,
        recent_logs,
        density,
    }
}

/// Format a `HostTelemetrySnapshot` as Prometheus text exposition format for `google-cloud-ops-agent`.
pub fn render_prometheus_metrics(snap: &HostTelemetrySnapshot) -> String {
    let mut out = String::new();
    let labels = format!(
        r#"instance="{}",zone="{}",role="{}",arch="{}""#,
        snap.instance_name, snap.zone, snap.role, snap.arch
    );

    out.push_str("# TYPE gha_host_cpu_utilization_percent gauge\n");
    out.push_str(&format!(
        "gha_host_cpu_utilization_percent{{{labels}}} {}\n",
        snap.cpu_utilization_pct
    ));
    out.push_str("# TYPE gha_host_loadavg_1m gauge\n");
    out.push_str(&format!(
        "gha_host_loadavg_1m{{{labels}}} {}\n",
        snap.loadavg_1m
    ));
    out.push_str("# TYPE gha_host_memory_used_gib gauge\n");
    out.push_str(&format!(
        "gha_host_memory_used_gib{{{labels}}} {}\n",
        snap.mem_used_gib
    ));
    out.push_str("# TYPE gha_host_memory_available_gib gauge\n");
    out.push_str(&format!(
        "gha_host_memory_available_gib{{{labels}}} {}\n",
        snap.mem_available_gib
    ));
    out.push_str("# TYPE gha_host_disk_utilization_percent gauge\n");
    out.push_str(&format!(
        "gha_host_disk_utilization_percent{{{labels}}} {}\n",
        snap.disk_utilization_pct
    ));
    out.push_str("# TYPE gha_dynamic_current_runners gauge\n");
    out.push_str(&format!(
        "gha_dynamic_current_runners{{{labels}}} {}\n",
        snap.density.current_runners
    ));
    out.push_str("# TYPE gha_dynamic_recommended_runners gauge\n");
    out.push_str(&format!(
        "gha_dynamic_recommended_runners{{{labels}}} {}\n",
        snap.density.recommended_runners
    ));
    out.push_str("# TYPE gha_dynamic_peak_working_set_gib gauge\n");
    out.push_str(&format!(
        "gha_dynamic_peak_working_set_gib{{{labels}}} {}\n",
        snap.density.observed_peak_working_set_gib
    ));

    out.push_str("# TYPE gha_runner_working_set_gib gauge\n");
    out.push_str("# TYPE gha_runner_cgroup_peak_gib gauge\n");
    out.push_str("# TYPE gha_runner_oom_kills_total counter\n");
    for c in &snap.containers {
        let repo = c
            .active_workflow
            .as_ref()
            .map(|w| w.repository.as_str())
            .unwrap_or("idle");
        let run_id = c
            .active_workflow
            .as_ref()
            .map(|w| w.run_id.as_str())
            .unwrap_or("");
        let job_id = c
            .active_workflow
            .as_ref()
            .map(|w| w.job_id.as_str())
            .unwrap_or("");
        let clabels = format!(
            r#"{labels},container="{}",repository="{}",run_id="{}",job_id="{}""#,
            c.name, repo, run_id, job_id
        );
        out.push_str(&format!(
            "gha_runner_working_set_gib{{{clabels}}} {}\n",
            c.memory_working_set_gib
        ));
        out.push_str(&format!(
            "gha_runner_cgroup_peak_gib{{{clabels}}} {}\n",
            c.memory_peak_gib
        ));
        out.push_str(&format!(
            "gha_runner_oom_kills_total{{{clabels}}} {}\n",
            c.oom_kill_events
        ));
    }

    out
}

pub fn append_telemetry_jsonl(log_path: &Path, snap: &HostTelemetrySnapshot) {
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path) {
        for entry in &snap.recent_logs {
            if let Ok(line) = serde_json::to_string(entry) {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

/// Gracefully scale up additional runner container slots on the host without touching
/// or restarting any existing active runner container.
pub fn scale_runners_in_place(
    target_runners: u32,
    registration_token: &str,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    if registration_token.trim().is_empty() && !dry_run {
        return Err("Registration token is required to register new runner slots".to_string());
    }
    let docker_bin = if Path::new("/usr/bin/docker").exists() {
        "/usr/bin/docker"
    } else {
        "docker"
    };

    // Inspect slot 1 container on the host to clone its exact image, labels, env, and mounts
    let ps_out = Command::new(docker_bin)
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .map_err(|e| format!("Failed to run docker ps: {e}"))?;
    let names: Vec<String> = String::from_utf8_lossy(&ps_out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|n| n.starts_with("gha-") || n.starts_with("oxidex-"))
        .collect();

    let template_container = names
        .iter()
        .find(|n| n.ends_with("-1"))
        .or_else(|| names.first())
        .ok_or_else(|| "No existing runner container found on host to clone".to_string())?
        .clone();

    let base_prefix = template_container
        .rsplit_once('-')
        .map(|(p, _)| p.to_string())
        .ok_or_else(|| format!("Unexpected runner container name format: {template_container}"))?;

    let current_max_slot = names
        .iter()
        .filter_map(|n| n.strip_prefix(&format!("{base_prefix}-")))
        .filter_map(|s| s.parse::<u32>().ok())
        .max()
        .unwrap_or(1);

    if current_max_slot >= target_runners {
        return Ok(Vec::new());
    }

    let inspect_out = Command::new(docker_bin)
        .args(["inspect", &template_container])
        .output()
        .map_err(|e| format!("Failed to inspect template container {template_container}: {e}"))?;
    let inspect_json: serde_json::Value = serde_json::from_slice(&inspect_out.stdout)
        .map_err(|e| format!("Invalid docker inspect JSON: {e}"))?;
    let first = inspect_json
        .get(0)
        .ok_or_else(|| "Empty docker inspect array".to_string())?;

    let image = first
        .pointer("/Config/Image")
        .and_then(|v| v.as_str())
        .ok_or("Missing Config.Image")?
        .to_string();

    let env_arr = first
        .pointer("/Config/Env")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let find_env = |key: &str| -> String {
        let prefix = format!("{key}=");
        env_arr
            .iter()
            .filter_map(|v| v.as_str())
            .find_map(|s| s.strip_prefix(&prefix).map(str::to_string))
            .unwrap_or_default()
    };

    let github_url = find_env("GITHUB_RUNNER_URL");
    let runner_labels = find_env("RUNNER_LABELS");
    let runner_group = find_env("RUNNER_GROUP_NAME");
    let memory_bytes = first
        .pointer("/HostConfig/Memory")
        .and_then(|v| v.as_u64())
        .unwrap_or(14 * 1024 * 1024 * 1024);
    let memory_gb = (memory_bytes / (1024 * 1024 * 1024)).max(4);
    let nano_cpus = first
        .pointer("/HostConfig/NanoCpus")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mounts = first
        .pointer("/Mounts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_vol = mounts
        .iter()
        .find(|m| m.get("Destination").and_then(|d| d.as_str()) == Some("/opt/runner-rust"))
        .and_then(|m| m.get("Name").and_then(|n| n.as_str()))
        .unwrap_or("")
        .to_string();
    let has_nntmux_cache = mounts
        .iter()
        .any(|m| m.get("Destination").and_then(|d| d.as_str()) == Some("/work/.nntmux-cache"));
    let has_docker_sock = mounts
        .iter()
        .any(|m| m.get("Destination").and_then(|d| d.as_str()) == Some("/var/run/docker.sock"));
    let has_admission = mounts
        .iter()
        .any(|m| m.get("Destination").and_then(|d| d.as_str()) == Some("/run/gha-admission"));

    let mut launched = Vec::new();
    for slot in (current_max_slot + 1)..=target_runners {
        let new_name = format!("{base_prefix}-{slot}");
        if dry_run {
            launched.push(new_name);
            continue;
        }
        let state_dir = format!("/mnt/runner-data/instances/{new_name}");
        let work_dir = format!("/mnt/runner-data/work/{new_name}");
        fs::create_dir_all(&state_dir).map_err(|e| e.to_string())?;
        fs::create_dir_all(&work_dir).map_err(|e| e.to_string())?;
        let _ = Command::new("chown")
            .args(["-R", "1001:1001", &state_dir, &work_dir])
            .status();

        let env_file = format!("/run/{new_name}.env");
        fs::write(
            &env_file,
            format!("GITHUB_RUNNER_TOKEN={registration_token}\n"),
        )
        .map_err(|e| e.to_string())?;

        let cfg_status = Command::new(docker_bin)
            .args([
                "run",
                "--rm",
                "--env-file",
                &env_file,
                "-e",
                "RUNNER_CONFIGURE_ONLY=1",
                "-e",
                &format!("RUNNER_NAME={new_name}"),
                "-e",
                &format!("RUNNER_LABELS={runner_labels}"),
                "-e",
                &format!("RUNNER_GROUP_NAME={runner_group}"),
                "-e",
                &format!("GITHUB_RUNNER_URL={github_url}"),
                "-e",
                "RUNNER_STATE=/runner-state",
                "-v",
                &format!("{state_dir}:/runner-state"),
                "-v",
                &format!("{work_dir}:/work"),
                &image,
            ])
            .status()
            .map_err(|e| e.to_string())?;
        let _ = fs::remove_file(&env_file);
        if !cfg_status.success() {
            return Err(format!("Failed to configure runner slot {new_name}"));
        }

        let mut run_args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--restart".to_string(),
            "unless-stopped".to_string(),
            "--cpu-shares=1024".to_string(),
            format!("--memory={memory_gb}g"),
            "--name".to_string(),
            new_name.clone(),
            "-e".to_string(),
            format!("RUNNER_NAME={new_name}"),
            "-e".to_string(),
            format!("RUNNER_LABELS={runner_labels}"),
            "-e".to_string(),
            format!("RUNNER_GROUP_NAME={runner_group}"),
            "-e".to_string(),
            format!("GITHUB_RUNNER_URL={github_url}"),
            "-e".to_string(),
            "RUNNER_STATE=/runner-state".to_string(),
            "-v".to_string(),
            format!("{state_dir}:/runner-state"),
            "-v".to_string(),
            format!("{work_dir}:/work"),
            "-v".to_string(),
            "/run/gha-runner-cordon:/run/gha-runner-cordon:ro".to_string(),
        ];
        if nano_cpus > 0 {
            let cpus = (nano_cpus as f64 / 1_000_000_000.0).max(1.0);
            run_args.push(format!("--cpus={cpus}"));
        }
        if !rust_vol.is_empty() {
            run_args.push("--mount".to_string());
            run_args.push(format!(
                "type=volume,src={rust_vol},dst=/opt/runner-rust,readonly"
            ));
        }
        if has_nntmux_cache {
            let cache_dir = format!("/mnt/runner-data/nntmux-cache/{new_name}");
            let _ = fs::create_dir_all(&cache_dir);
            let _ = Command::new("chown")
                .args(["-R", "1001:1001", &cache_dir])
                .status();
            run_args.push("-v".to_string());
            run_args.push(format!("{cache_dir}:/work/.nntmux-cache"));
        }
        if has_docker_sock {
            run_args.push("-v".to_string());
            run_args.push("/var/run/docker.sock:/var/run/docker.sock".to_string());
        }
        if has_admission {
            run_args.push("-e".to_string());
            run_args.push("GHA_ADMISSION_REQUIRED=1".to_string());
            run_args.push("-v".to_string());
            run_args.push("/run/gha-admission:/run/gha-admission:ro".to_string());
        }
        run_args.push(image.clone());

        let run_status = Command::new(docker_bin)
            .args(&run_args)
            .status()
            .map_err(|e| e.to_string())?;
        if !run_status.success() {
            return Err(format!("Failed to start container {new_name}"));
        }
        launched.push(new_name);
    }

    Ok(launched)
}

pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let subcmd = args.first().map(String::as_str).unwrap_or("collect");
    match subcmd {
        "collect" | "status" => {
            let mut instances_dir = PathBuf::from(DEFAULT_INSTANCES_DIR);
            let mut log_path = PathBuf::from(DEFAULT_TELEMETRY_LOG_PATH);
            let mut prometheus = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--instances-dir" => {
                        i += 1;
                        if let Some(p) = args.get(i) {
                            instances_dir = PathBuf::from(p);
                        }
                    }
                    "--log-output" => {
                        i += 1;
                        if let Some(p) = args.get(i) {
                            log_path = PathBuf::from(p);
                        }
                    }
                    "--prometheus" => prometheus = true,
                    _ => {}
                }
                i += 1;
            }
            let snap = collect_host_telemetry(
                None,
                None,
                &instances_dir,
                Path::new(DEFAULT_PEAK_LEDGER_PATH),
            );
            append_telemetry_jsonl(&log_path, &snap);
            if prometheus {
                print!("{}", render_prometheus_metrics(&snap));
            } else {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            }
            Ok(0)
        }
        "scale-runners" => {
            let mut target_runners = 0u32;
            let mut token = std::env::var("GITHUB_RUNNER_TOKEN").unwrap_or_default();
            let mut dry_run = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--target-runners" => {
                        i += 1;
                        target_runners = args
                            .get(i)
                            .and_then(|s| s.parse().ok())
                            .ok_or("Invalid --target-runners")?;
                    }
                    "--registration-token" => {
                        i += 1;
                        token = args
                            .get(i)
                            .cloned()
                            .ok_or("Missing --registration-token value")?;
                    }
                    "--dry-run" => dry_run = true,
                    _ => {}
                }
                i += 1;
            }
            if target_runners == 0 {
                let snap = collect_host_telemetry(
                    None,
                    None,
                    Path::new(DEFAULT_INSTANCES_DIR),
                    Path::new(DEFAULT_PEAK_LEDGER_PATH),
                );
                target_runners = snap.density.recommended_runners;
            }
            let launched = scale_runners_in_place(target_runners, &token, dry_run)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "target_runners": target_runners,
                    "dry_run": dry_run,
                    "launched_runners": launched,
                }))
                .unwrap_or_default()
            );
            Ok(0)
        }
        "serve" => {
            let mut listen_addr = DEFAULT_METRICS_ADDR.to_string();
            let mut instances_dir = PathBuf::from(DEFAULT_INSTANCES_DIR);
            let mut log_path = PathBuf::from(DEFAULT_TELEMETRY_LOG_PATH);
            let mut edge_url = std::env::var("SPOT_OBSERVABILITY_INGEST_URL").unwrap_or_default();
            let mut bearer = std::env::var("SPOT_BEARER_TOKEN").unwrap_or_default();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--listen" => {
                        i += 1;
                        if let Some(a) = args.get(i) {
                            listen_addr = a.clone();
                        }
                    }
                    "--instances-dir" => {
                        i += 1;
                        if let Some(p) = args.get(i) {
                            instances_dir = PathBuf::from(p);
                        }
                    }
                    "--log-output" => {
                        i += 1;
                        if let Some(p) = args.get(i) {
                            log_path = PathBuf::from(p);
                        }
                    }
                    "--edge-url" => {
                        i += 1;
                        if let Some(u) = args.get(i) {
                            edge_url = u.clone();
                        }
                    }
                    "--bearer-token" => {
                        i += 1;
                        if let Some(t) = args.get(i) {
                            bearer = t.clone();
                        }
                    }
                    _ => {}
                }
                i += 1;
            }

            let listener = TcpListener::bind(&listen_addr)
                .map_err(|e| format!("Failed to bind {listen_addr}: {e}"))?;
            let mut last_log_epoch = 0u64;

            for stream_res in listener.incoming() {
                let mut stream = match stream_res {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let mut req_buf = [0u8; 1024];
                let n = stream.read(&mut req_buf).unwrap_or(0);
                let req_line = String::from_utf8_lossy(&req_buf[..n]);
                let want_json = req_line.starts_with("GET /status");

                let snap = collect_host_telemetry(
                    None,
                    None,
                    &instances_dir,
                    Path::new(DEFAULT_PEAK_LEDGER_PATH),
                );
                let now = current_epoch_secs();
                if now.saturating_sub(last_log_epoch) >= 25 {
                    append_telemetry_jsonl(&log_path, &snap);
                    if !edge_url.is_empty() && !bearer.is_empty() {
                        if let Ok(payload) = serde_json::to_string(&snap) {
                            let _ = Command::new("/usr/bin/curl")
                                .args([
                                    "-fsS",
                                    "--max-time",
                                    "5",
                                    "-X",
                                    "POST",
                                    "-H",
                                    "Content-Type: application/json",
                                    "-H",
                                    &format!("Authorization: Bearer {bearer}"),
                                    "--data-binary",
                                    &payload,
                                    &edge_url,
                                ])
                                .status();
                        }
                    }
                    last_log_epoch = now;
                }

                let (content_type, body) = if want_json {
                    (
                        "application/json",
                        serde_json::to_string(&snap).unwrap_or_else(|_| "{}".to_string()),
                    )
                } else {
                    (
                        "text/plain; version=0.0.4",
                        render_prometheus_metrics(&snap),
                    )
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
            Ok(0)
        }
        "-h" | "--help" | "help" => {
            eprintln!(
                "Usage: host-agent telemetry <collect|serve|scale-runners> [OPTIONS]\n\
                 Subcommands:\n  \
                   collect        Collect host, cgroup v2 & workflow correlation telemetry\n  \
                   serve          Serve Prometheus /metrics & JSON /status on 127.0.0.1:9464\n  \
                   scale-runners  Gracefully scale up additional runner container slots in-place\n"
            );
            Ok(0)
        }
        other => Err(format!("Unknown telemetry subcommand: {other}").into()),
    }
}
