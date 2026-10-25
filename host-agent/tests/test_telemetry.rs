use host_agent::telemetry::{
    collect_host_telemetry, evaluate_dynamic_runner_density, parse_worker_log_correlation,
    render_prometheus_metrics, scan_instances_dir_for_workflows,
};
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;

const SAMPLE_WORKER_LOG: &str = r#"[2026-10-24 22:44:29Z INFO Worker] Waiting to receive the job message from the channel.
[2026-10-24 22:44:29Z INFO Worker] Job message:
{
  "messageType": "RunnerJobRequest",
  "plan": {
    "planId": "f3d25e03-e984-84ac-8caa-18b32e36f83b"
  },
  "jobId": "87d33bf1-59d9-5535-bcaf-8b748d0760d6",
  "jobDisplayName": "PHP 8.5",
  "contextData": {
    "github": {
      "t": 2,
      "d": [
        { "k": "sha", "v": "5871398b6dcd9dc834e704b0c8386b8991412806" },
        { "k": "repository", "v": "xbmc4lyfe/newznab-tmux" },
        { "k": "repository_owner", "v": "xbmc4lyfe" },
        { "k": "run_id", "v": "40124985102" },
        { "k": "run_number", "v": "1600" },
        { "k": "workflow", "v": "Run tests" }
      ]
    },
    "job": {
      "t": 2,
      "d": [
        { "k": "check_run_id", "v": 120964372558.0 }
      ]
    }
  }
}
[2026-10-24 22:44:30Z INFO ActionManager] Load action that reference repository from '/work/_actions/actions/checkout/v5'
[2026-10-24 22:44:45Z INFO Terminal] WRITE LINE: Running PHPUnit suite on PHP 8.5
"#;

#[test]
fn test_parse_worker_log_correlation_extracts_exact_github_job_url() {
    let corr = parse_worker_log_correlation(
        "gha-oxidex-runners-xbmc4lyfe-newznab-tmux-x86-1-1",
        "Worker_20261024-224429-utc.log",
        SAMPLE_WORKER_LOG,
    )
    .expect("should parse worker log correlation");

    assert_eq!(
        corr.runner_name,
        "gha-oxidex-runners-xbmc4lyfe-newznab-tmux-x86-1-1"
    );
    assert_eq!(corr.organization, "xbmc4lyfe");
    assert_eq!(corr.repository, "xbmc4lyfe/newznab-tmux");
    assert_eq!(corr.workflow_name, "Run tests");
    assert_eq!(corr.job_name, "PHP 8.5");
    assert_eq!(corr.run_id, "40124985102");
    assert_eq!(corr.run_number, "1600");
    assert_eq!(corr.job_id, "120964372558");
    assert_eq!(
        corr.github_workflow_url,
        "https://github.com/xbmc4lyfe/newznab-tmux/actions/runs/40124985102/job/120964372558"
    );
    assert_eq!(corr.commit_sha, "5871398b6dcd9dc834e704b0c8386b8991412806");
    assert_eq!(corr.started_at, "2026-10-24T22:44:29Z");
    assert_eq!(corr.status, "in_progress");
}

#[test]
fn test_scan_instances_dir_correlates_logs_with_container_and_workflow() {
    let dir = tempdir().unwrap();
    let runner_diag = dir
        .path()
        .join("gha-oxidex-runners-xbmc4lyfe-newznab-tmux-x86-1-1")
        .join("_diag");
    fs::create_dir_all(&runner_diag).unwrap();
    fs::write(
        runner_diag.join("Worker_20261024-224429-utc.log"),
        SAMPLE_WORKER_LOG,
    )
    .unwrap();

    let mut container_ids = BTreeMap::new();
    container_ids.insert(
        "gha-oxidex-runners-xbmc4lyfe-newznab-tmux-x86-1-1".to_string(),
        "9a8b7c6d5e4f".to_string(),
    );

    let (correlations, logs) = scan_instances_dir_for_workflows(
        dir.path(),
        "oxidex-runners-xbmc4lyfe-newznab-tmux-x86-1",
        "us-central1-a",
        "cicd_runner",
        &container_ids,
    );

    assert_eq!(correlations.len(), 1);
    assert!(!logs.is_empty());
    let first_log = &logs[0];
    assert_eq!(first_log.container_id, "9a8b7c6d5e4f");
    assert_eq!(first_log.organization, "xbmc4lyfe");
    assert_eq!(first_log.repository, "xbmc4lyfe/newznab-tmux");
    assert_eq!(
        first_log.github_workflow_url,
        "https://github.com/xbmc4lyfe/newznab-tmux/actions/runs/40124985102/job/120964372558"
    );
}

#[test]
fn test_dynamic_runner_density_scales_underutilized_arm_and_x86_hosts() {
    // 1. 4-vCPU / 16-GiB ARM host with 1 runner using 2.0 GiB working set & 1.3 cores -> recommends 2 runners
    let arm_eval = evaluate_dynamic_runner_density(
        true, 4, 16.0, 2.0, 0.0, 1, 1, 2.0, 4.5, 1.3, 0, 80.0, 80.0,
    );
    assert!(arm_eval.enabled);
    assert_eq!(arm_eval.recommended_runners, 2);
    assert_eq!(arm_eval.can_scale_up_slots, 1);
    assert!(!arm_eval.oom_guard_tripped);

    // 2. 16-vCPU / 64-GiB x86 host with 4 runners using 1.5 GiB working set & 1.25 cores -> recommends 8 runners
    let x86_eval = evaluate_dynamic_runner_density(
        true, 16, 64.0, 4.0, 0.0, 4, 2, 1.5, 8.5, 1.25, 0, 80.0, 80.0,
    );
    assert!(x86_eval.enabled);
    assert!(x86_eval.recommended_runners >= 6);
    assert!(x86_eval.can_scale_up_slots >= 2);

    // 3. Opt-out holds at baseline
    let opted_out = evaluate_dynamic_runner_density(
        false, 16, 64.0, 4.0, 0.0, 2, 2, 1.5, 8.5, 1.25, 0, 80.0, 80.0,
    );
    assert!(!opted_out.enabled);
    assert_eq!(opted_out.recommended_runners, 2);
    assert_eq!(opted_out.can_scale_up_slots, 0);

    // 4. OOM guard trips when oom_kills > 0
    let oom_tripped = evaluate_dynamic_runner_density(
        true, 16, 64.0, 4.0, 0.0, 4, 2, 1.5, 8.5, 1.25, 1, 80.0, 80.0,
    );
    assert!(oom_tripped.oom_guard_tripped);
    assert_eq!(oom_tripped.can_scale_up_slots, 0);
}

#[test]
fn test_collect_and_render_prometheus_metrics() {
    let dir = tempdir().unwrap();
    let ledger_path = dir.path().join("peak.json");
    let snap = collect_host_telemetry(
        Some("oxidex-runners-test-1"),
        Some("us-central1-a"),
        dir.path(),
        &ledger_path,
    );
    let prom = render_prometheus_metrics(&snap);
    assert!(prom.contains("gha_host_cpu_utilization_percent"));
    assert!(prom.contains("gha_dynamic_recommended_runners"));
    assert!(prom.contains(r#"instance="oxidex-runners-test-1""#));
}
