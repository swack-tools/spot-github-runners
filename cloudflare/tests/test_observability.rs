use spot_dashboard_edge::observability::{
    clear_ingested_snapshots, compute_dynamic_runner_recommendation, handle_get_observability,
    handle_post_dynamic_density, parse_observability_query, record_ingested_snapshot,
    IngestedDensityPayload, IngestedHostSnapshot, ObservabilityLogEntry, ObservabilityQuery,
    UpdateDynamicDensityRequest, WorkflowCorrelationEntry,
};
use spot_dashboard_edge::state::{KvStoreBackend, StateError};
use std::collections::HashMap;
use std::sync::Mutex;

struct MemKv(Mutex<HashMap<String, String>>);

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for MemKv {
    async fn get(&self, key: &str) -> Result<Option<String>, StateError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    async fn put(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
}

#[test]
fn test_dynamic_runner_density_recommendation_unlocks_slots_when_underutilized() {
    clear_ingested_snapshots();

    // 16 vCPU, 64 GiB RAM host running 4 runners using only 1.15 GiB peak working set and 1.2 CPU cores/runner
    let (rec_enabled, extra_enabled, reason_enabled) =
        compute_dynamic_runner_recommendation(true, 16, 64, 4, 4, 1.15, 1.2, 0, 80.0, 80.0, 8);
    assert!(
        rec_enabled > 4 && rec_enabled <= 8,
        "Expected dynamic runner density to unlock additional slots on underutilized 16-vCPU/64-GiB host, got {rec_enabled}"
    );
    assert_eq!(extra_enabled, rec_enabled - 4);
    assert!(reason_enabled.to_lowercase().contains("peak working set"));

    // Opt-out (enabled = false) holds at baseline
    let (rec_disabled, extra_disabled, reason_disabled) =
        compute_dynamic_runner_recommendation(false, 16, 64, 4, 4, 1.15, 1.2, 0, 80.0, 80.0, 8);
    assert_eq!(rec_disabled, 4);
    assert_eq!(extra_disabled, 0);
    assert!(reason_disabled.contains("opted out"));
}

#[test]
fn test_parse_observability_query_filters_and_nan_guard() {
    let q = parse_observability_query(&[
        ("role".to_string(), "cicd_runner".to_string()),
        ("orgs".to_string(), "swack-tools,xbmc4lyfe".to_string()),
        (
            "repos".to_string(),
            "xbmc4lyfe/newznab-tmux,swack-tools/oxidex".to_string(),
        ),
        ("window_hours".to_string(), "12".to_string()),
        ("q".to_string(), "cargo test".to_string()),
        ("severity".to_string(), "warn".to_string()),
    ])
    .expect("valid query");

    assert_eq!(q.role, "cicd_runner");
    assert_eq!(q.organizations, vec!["swack-tools", "xbmc4lyfe"]);
    assert_eq!(
        q.repositories,
        vec!["xbmc4lyfe/newznab-tmux", "swack-tools/oxidex"]
    );
    assert_eq!(q.window_hours, 12.0);
    assert_eq!(q.q, "cargo test");
    assert_eq!(q.severity, "WARN");

    assert!(parse_observability_query(&[("window_hours".to_string(), "NaN".to_string())]).is_err());
    assert!(parse_observability_query(&[("window_hours".to_string(), "-5".to_string())]).is_err());
}

#[tokio::test]
async fn test_observability_filters_correlated_logs_and_dynamic_density_persistence() {
    clear_ingested_snapshots();
    let kv = MemKv(Mutex::new(HashMap::new()));
    let now = 1_792_000_000;

    // 1. Default policy has dynamic runner density enabled by default
    let default_query = ObservabilityQuery {
        role: "all".to_string(),
        window_hours: 6.0,
        ..Default::default()
    };
    let (status, resp) = handle_get_observability(&kv, &default_query, now)
        .await
        .expect("default observability");
    assert_eq!(status, 200);
    assert!(
        resp.dynamic_density.enabled,
        "Dynamic runner density must be enabled by default"
    );
    assert!(
        resp.dynamic_density.extra_runners_unlocked > 0,
        "Default fleet should unlock extra dynamic runner slots"
    );
    assert!(!resp.timeseries.is_empty());
    assert!(!resp.logs.is_empty());
    assert!(resp
        .logs
        .iter()
        .any(|l| l.github_workflow_url.starts_with("https://github.com/")));

    // 2. Filter to CI/CD runners and specific repo xbmc4lyfe/newznab-tmux
    let repo_query = ObservabilityQuery {
        role: "cicd_runner".to_string(),
        repositories: vec!["xbmc4lyfe/newznab-tmux".to_string()],
        window_hours: 6.0,
        ..Default::default()
    };
    let (_, repo_resp) = handle_get_observability(&kv, &repo_query, now)
        .await
        .expect("repo filtered observability");
    assert!(!repo_resp.instances.is_empty());
    for inst in &repo_resp.instances {
        assert_eq!(inst.role, "cicd_runner");
        assert_eq!(inst.repository, "xbmc4lyfe/newznab-tmux");
    }
    for log in &repo_resp.logs {
        assert_eq!(log.repository, "xbmc4lyfe/newznab-tmux");
    }

    // 3. Ingest live host telemetry with a correlated GitHub workflow job log and search for it
    record_ingested_snapshot(
        IngestedHostSnapshot {
            instance_name: "oxidex-runners-xbmc4lyfe-newznab-tmux-arm-01".to_string(),
            zone: "us-central1-a".to_string(),
            role: "cicd_runner".to_string(),
            arch: "arm64".to_string(),
            collected_epoch: now,
            vcpu: 4,
            loadavg_1m: 1.42,
            cpu_utilization_pct: 35.5,
            mem_total_gib: 16.0,
            mem_used_gib: 1.18,
            mem_available_gib: 14.82,
            mem_utilization_pct: 7.38,
            disk_total_gib: 75.0,
            disk_used_gib: 12.4,
            disk_utilization_pct: 16.5,
            workflow_correlations: vec![WorkflowCorrelationEntry {
                runner_name: "xbmc4lyfe-newznab-tmux-arm-01-1".to_string(),
                container_id: "042d4c25e0f4".to_string(),
                organization: "xbmc4lyfe".to_string(),
                repository: "xbmc4lyfe/newznab-tmux".to_string(),
                workflow_name: "CI".to_string(),
                job_name: "Unit Tests (PHP 8.4)".to_string(),
                run_id: "24854572144".to_string(),
                run_number: "942".to_string(),
                job_id: "120964372558".to_string(),
                github_workflow_url:
                    "https://github.com/xbmc4lyfe/newznab-tmux/actions/runs/24854572144/job/120964372558"
                        .to_string(),
                commit_sha: "abc1234".to_string(),
                started_at: "2026-10-25T00:55:00Z".to_string(),
                completed_at: None,
                status: "in_progress".to_string(),
            }],
            recent_logs: vec![ObservabilityLogEntry {
                timestamp: "2026-10-25T00:56:12Z".to_string(),
                epoch: now,
                severity: "INFO".to_string(),
                source: "gha_worker".to_string(),
                instance_name: "oxidex-runners-xbmc4lyfe-newznab-tmux-arm-01".to_string(),
                zone: "us-central1-a".to_string(),
                role: "cicd_runner".to_string(),
                arch: "arm64".to_string(),
                runner_name: "xbmc4lyfe-newznab-tmux-arm-01-1".to_string(),
                container_id: "042d4c25e0f4".to_string(),
                organization: "xbmc4lyfe".to_string(),
                repository: "xbmc4lyfe/newznab-tmux".to_string(),
                workflow_name: "CI".to_string(),
                job_name: "Unit Tests (PHP 8.4)".to_string(),
                run_id: "24854572144".to_string(),
                run_number: "942".to_string(),
                job_id: "120964372558".to_string(),
                github_workflow_url:
                    "https://github.com/xbmc4lyfe/newznab-tmux/actions/runs/24854572144/job/120964372558"
                        .to_string(),
                message: "UNIQUE_CANARY_MARKER_942: Running PHPUnit suite inside container 042d4c25e0f4"
                    .to_string(),
            }],
            density: IngestedDensityPayload {
                enabled: true,
                current_runners: 1,
                baseline_runners: 1,
                recommended_runners: 2,
                can_scale_up_slots: 1,
                observed_peak_working_set_gib: 1.18,
                observed_peak_cgroup_gib: 2.05,
                observed_avg_cpu_cores_per_runner: 1.42,
                reason: "Observed peak working-set 1.18 GiB/runner fits 2 slots".to_string(),
            },
            ..Default::default()
        },
        now,
    );

    let search_query = ObservabilityQuery {
        role: "cicd_runner".to_string(),
        q: "UNIQUE_CANARY_MARKER_942".to_string(),
        window_hours: 6.0,
        ..Default::default()
    };
    let (_, search_resp) = handle_get_observability(&kv, &search_query, now)
        .await
        .expect("search observability");
    assert_eq!(search_resp.logs.len(), 1);
    assert_eq!(
        search_resp.logs[0].github_workflow_url,
        "https://github.com/xbmc4lyfe/newznab-tmux/actions/runs/24854572144/job/120964372558"
    );

    // 4. Opt out of dynamic runner density via POST /api/observability/density
    let (post_status, post_body) = handle_post_dynamic_density(
        &kv,
        UpdateDynamicDensityRequest {
            enabled: Some(false),
            target_cpu_percent: Some(75.0),
            target_memory_percent: Some(75.0),
            max_runners_per_instance: Some(6),
        },
        now + 5,
    )
    .await
    .expect("update density policy");
    assert_eq!(post_status, 200);
    assert_eq!(post_body["enable_dynamic_runner_density"], false);

    let (_, after_opt_out) = handle_get_observability(&kv, &default_query, now + 6)
        .await
        .expect("observability after opt-out");
    assert!(!after_opt_out.dynamic_density.enabled);
    assert_eq!(after_opt_out.dynamic_density.extra_runners_unlocked, 0);

    clear_ingested_snapshots();
}
