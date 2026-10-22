//! Test Suite for GitHub Actions REST API Client & Startup Script Rendering.

use spot_dashboard_edge::github::*;
use spot_dashboard_edge::patch::{
    ContainerPatch, HostPatch, PatchFileSpec, PatchMountSpec, RunnerPatch,
};
use spot_dashboard_edge::patch_console::GitHubTransport;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Custom mock transport to simulate specific status codes for JIT and registration endpoints.
#[derive(Clone, Default)]
struct CustomMockGitHubTransport {
    responses: Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>,
    calls: Arc<Mutex<Vec<(String, String, Option<Vec<u8>>)>>>,
}

impl CustomMockGitHubTransport {
    fn new() -> Self {
        Self::default()
    }

    fn set_response(&self, path_substring: &str, status: u16, body: serde_json::Value) {
        let bytes = serde_json::to_vec(&body).unwrap();
        self.responses
            .lock()
            .unwrap()
            .insert(path_substring.to_string(), (status, bytes));
    }
}

#[async_trait::async_trait(?Send)]
impl GitHubTransport for CustomMockGitHubTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        _headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        self.calls.lock().unwrap().push((
            method.to_string(),
            path.to_string(),
            body.map(|b| b.to_vec()),
        ));

        let resps = self.responses.lock().unwrap();
        let mut matches: Vec<_> = resps
            .iter()
            .filter(|(pat, _)| path.contains(*pat))
            .collect();
        matches.sort_by_key(|(pat, _)| std::cmp::Reverse(pat.len()));

        if let Some((_, (status, resp_bytes))) = matches.first() {
            return Ok((*status, resp_bytes.clone()));
        }

        // Default mock responses
        if path.contains("registration-token") {
            let resp = serde_json::json!({
                "token": "mock-reg-token-12345",
                "expires_at": "2026-10-22T01:00:00Z"
            });
            return Ok((201, serde_json::to_vec(&resp).unwrap()));
        }

        if path.contains("generate-jitconfig") {
            let resp = serde_json::json!({
                "runner": {
                    "id": 99,
                    "name": "mock-runner",
                    "status": "offline",
                    "busy": false,
                    "labels": []
                },
                "encoded_jit_config": "mock_jit_base64_encoded_config"
            });
            return Ok((201, serde_json::to_vec(&resp).unwrap()));
        }

        Ok((200, b"{}".to_vec()))
    }
}

#[tokio::test]
async fn test_2_slot_jitconfig_generation() {
    let transport = Arc::new(CustomMockGitHubTransport::new());
    transport.set_response(
        "generate-jitconfig",
        201,
        serde_json::json!({
            "runner": {
                "id": 101,
                "name": "runner-slot",
                "status": "offline",
                "busy": false,
                "labels": []
            },
            "encoded_jit_config": "base64_jit_slot_payload"
        }),
    );

    let client = GitHubActionsClient::new("fake_pat_token".to_string(), transport.clone());
    let labels = vec![
        "self-hosted".to_string(),
        "Linux".to_string(),
        "X64".to_string(),
        "spot".to_string(),
    ];

    let result = client
        .mint_2slot_runner_configs(
            "orgs/swack-tools",
            "oxidex-runners-us-central1-a-test",
            "swack",
            10,
            &labels,
        )
        .await
        .unwrap();

    match result {
        RunnerBootstrapConfig::JitConfig {
            runner_1_jitconfig,
            runner_2_jitconfig,
        } => {
            assert_eq!(runner_1_jitconfig, "base64_jit_slot_payload");
            assert_eq!(runner_2_jitconfig, "base64_jit_slot_payload");
        }
        RunnerBootstrapConfig::RegistrationToken { .. } => {
            panic!("Expected RunnerBootstrapConfig::JitConfig, got RegistrationToken");
        }
    }

    // Assert two calls were made to generate-jitconfig with distinct runner names
    let calls = transport.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    let body1: serde_json::Value = serde_json::from_slice(calls[0].2.as_ref().unwrap()).unwrap();
    let body2: serde_json::Value = serde_json::from_slice(calls[1].2.as_ref().unwrap()).unwrap();
    assert_eq!(body1["name"], "swack-oxidex-runners-us-central1-a-test-1");
    assert_eq!(body2["name"], "swack-oxidex-runners-us-central1-a-test-2");
}

#[tokio::test]
async fn test_jitconfig_failure_falls_back_to_registration_token() {
    let transport = Arc::new(CustomMockGitHubTransport::new());
    // Simulate 404 Not Found (e.g. endpoint not supported on repo or organization)
    transport.set_response(
        "generate-jitconfig",
        404,
        serde_json::json!({
            "message": "Not Found",
            "documentation_url": "https://docs.github.com/rest"
        }),
    );
    transport.set_response(
        "registration-token",
        201,
        serde_json::json!({
            "token": "fallback-registration-token-abc",
            "expires_at": "2026-10-22T02:00:00Z"
        }),
    );

    let client = GitHubActionsClient::new("fake_pat_token".to_string(), transport.clone());
    let labels = vec!["self-hosted".to_string()];

    let result = client
        .mint_2slot_runner_configs(
            "repos/xbmc4lyfe/newznab-tmux",
            "oxidex-runners-us-central1-a-fallback",
            "xbmc",
            1,
            &labels,
        )
        .await
        .unwrap();

    match result {
        RunnerBootstrapConfig::RegistrationToken { token } => {
            assert_eq!(token, "fallback-registration-token-abc");
        }
        RunnerBootstrapConfig::JitConfig { .. } => {
            panic!("Expected fallback to RegistrationToken, got JitConfig");
        }
    }

    // Verify calls: 1 attempted JIT call, followed by 1 registration-token fallback call
    let calls = transport.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].1.contains("generate-jitconfig"));
    assert!(calls[1].1.contains("registration-token"));
}

#[tokio::test]
async fn test_list_runners_pagination_and_deduplication() {
    let transport = Arc::new(CustomMockGitHubTransport::new());

    let page1 = serde_json::json!({
        "total_count": 3,
        "runners": [
            {
                "id": 1,
                "name": "runner-1",
                "os": "Linux",
                "status": "online",
                "busy": true,
                "labels": [{"name": "self-hosted"}]
            },
            {
                "id": 2,
                "name": "runner-2",
                "os": "Linux",
                "status": "offline",
                "busy": false,
                "labels": [{"name": "self-hosted"}]
            }
        ]
    });

    let page2 = serde_json::json!({
        "total_count": 3,
        "runners": [
            {
                "id": 3,
                "name": "runner-3",
                "os": "Linux",
                "status": "online",
                "busy": false,
                "labels": [{"name": "self-hosted"}]
            }
        ]
    });

    transport.set_response("runners?per_page=100&page=1", 200, page1);
    transport.set_response("runners?per_page=100&page=2", 200, page2);

    let client = GitHubActionsClient::new("fake_token".to_string(), transport);
    let runners = client.list_runners("orgs/swack-tools").await.unwrap();

    assert_eq!(runners.len(), 3);
    assert_eq!(runners[0].id, 1);
    assert_eq!(runners[0].status, "online");
    assert!(runners[0].busy);

    assert_eq!(runners[1].id, 2);
    assert_eq!(runners[1].status, "offline");
    assert!(!runners[1].busy);

    assert_eq!(runners[2].id, 3);
    assert_eq!(runners[2].status, "online");
    assert!(!runners[2].busy);
}

#[tokio::test]
async fn test_delete_runner_safe_handling() {
    let transport = Arc::new(CustomMockGitHubTransport::new());
    transport.set_response("actions/runners/101", 204, serde_json::json!({}));
    transport.set_response(
        "actions/runners/102",
        404,
        serde_json::json!({"message": "Not Found"}),
    );
    transport.set_response(
        "actions/runners/103",
        500,
        serde_json::json!({"message": "Server Error"}),
    );

    let client = GitHubActionsClient::new("fake_token".to_string(), transport);

    // 204 No Content -> Ok
    assert!(client.delete_runner("orgs/swack-tools", 101).await.is_ok());

    // 404 Not Found -> Ok (already deleted / idempotent)
    assert!(client.delete_runner("orgs/swack-tools", 102).await.is_ok());

    // 500 Server Error -> Err
    let err = client
        .delete_runner("orgs/swack-tools", 103)
        .await
        .unwrap_err();
    match err {
        GitHubError::Api { status, .. } => assert_eq!(status, 500),
        _ => panic!("Expected Api error"),
    }
}

#[tokio::test]
async fn test_count_queued_jobs_backlog() {
    let transport = Arc::new(CustomMockGitHubTransport::new());

    // Runs response: 2 queued runs
    let runs_resp = serde_json::json!({
        "total_count": 2,
        "workflow_runs": [
            { "id": 1001, "status": "queued" },
            { "id": 1002, "status": "queued" }
        ]
    });
    transport.set_response("actions/runs?status=queued", 200, runs_resp);

    // Jobs for run 1001: 2 queued, 1 in_progress
    let jobs_1001 = serde_json::json!({
        "total_count": 3,
        "jobs": [
            { "id": 1, "name": "build", "status": "queued" },
            { "id": 2, "name": "test", "status": "waiting" },
            { "id": 3, "name": "lint", "status": "in_progress" } // Ignored
        ]
    });
    transport.set_response("runs/1001/jobs", 200, jobs_1001);

    // Jobs for run 1002: 1 pending, 1 completed
    let jobs_1002 = serde_json::json!({
        "total_count": 2,
        "jobs": [
            { "id": 4, "name": "deploy", "status": "pending" },
            { "id": 5, "name": "notify", "status": "completed" } // Ignored
        ]
    });
    transport.set_response("runs/1002/jobs", 200, jobs_1002);

    let client = GitHubActionsClient::new("fake_token".to_string(), transport);
    let queued = client
        .count_queued_jobs_for_repo("swack-tools", "oxidex")
        .await
        .unwrap();

    // 2 (run 1001: queued + waiting) + 1 (run 1002: pending) = 3
    assert_eq!(queued, 3);
}

#[test]
fn test_render_startup_script_31_placeholders() {
    let params = StartupParams {
        disk_device_name: "oxidex-runners-us-central1-a-test",
        instance_name: "oxidex-runners-us-central1-a-test",
        deployment_id: "dep-20261022-001",
        org: "swack-tools",
        runner_prefix: "oxidex",
        runner_group: "default",
        runner_labels: "self-hosted,Linux,X64,spot,c4d-highcpu-16,us-central1",
        registration_token: "test_token_12345",
        runner_count: 2,
        runner_memory_gb: 14,
        runner_cpu_limit: "7",
        tiered: true,
        strict_heavy_pool: false,
        resource_monitoring: false,
    };

    let script = render_startup_script(&params, None).unwrap();

    // Invariant: 0 unexpanded placeholder tokens
    assert!(
        !script.contains("{{"),
        "Found unexpanded placeholder in script"
    );

    // Invariant: strict size budget under 256 KiB
    assert!(
        script.len() < 256 * 1024,
        "Script length {} exceeds 256 KiB",
        script.len()
    );

    // Invariant: concrete substitutions
    assert!(script.contains("DEVICE=\"/dev/disk/by-id/google-oxidex-runners-us-central1-a-test\""));
    assert!(script.contains("TIERED=1"));
    assert!(script.contains("STRICT_HEAVY_POOL=0"));
    assert!(script.contains("REGISTRATION_TOKEN=test_token_12345"));
    assert!(script.contains("RUNNER_COUNT=2"));
    assert!(script.contains("RUNNER_MEMORY_GB=14"));
    assert!(script.contains("RUNNER_CPU_LIMIT='7'"));
    assert!(script.contains("oxidex-self-hosted-runner:2.337.0"));
    assert!(script.contains("ORG_URL=https://github.com/swack-tools"));
    assert!(script.contains("dep-20261022-001"));

    // Verify embedded Dockerfile and entrypoint parity
    assert!(script.contains("printf '%s' '"));
    assert!(script.contains("/opt/gha-runners/build/Dockerfile"));
}

#[test]
fn test_render_startup_script_patch_injection() {
    let params = StartupParams {
        disk_device_name: "oxidex-runners-us-central1-a-patch",
        instance_name: "oxidex-runners-us-central1-a-patch",
        deployment_id: "dep-20261022-002",
        org: "swack-tools",
        runner_prefix: "oxidex",
        runner_group: "default",
        runner_labels: "self-hosted",
        registration_token: "token_abc",
        runner_count: 2,
        runner_memory_gb: 14,
        runner_cpu_limit: "",
        tiered: true,
        strict_heavy_pool: false,
        resource_monitoring: false,
    };

    let mut patch = RunnerPatch::default();
    patch.repository = "swack-tools/oxidex".to_string();

    let mut sysctl = std::collections::BTreeMap::new();
    sysctl.insert("net.core.somaxconn".to_string(), "4096".to_string());
    patch.host = HostPatch {
        apt_packages: vec!["libssl-dev".to_string(), "pkg-config".to_string()],
        sysctl: sysctl.clone(),
        files: vec![PatchFileSpec {
            path: "/etc/test-patch.conf".to_string(),
            content: "test=true\n".to_string(),
            mode: "0644".to_string(),
            owner: "root:root".to_string(),
        }],
        boot_mkdirs: vec!["/var/cache/oxidex".to_string()],
        boot_script: "echo patch_boot_done\n".to_string(),
    };

    patch.container = ContainerPatch {
        apt_packages: vec![],
        sysctl: sysctl.clone(),
        mounts: vec![PatchMountSpec {
            source: "/mnt/runner-data/custom-cache".to_string(),
            target: "/opt/custom-cache".to_string(),
            read_only: false,
        }],
        files: vec![],
        dockerfile_run: String::new(),
    };

    let script = render_startup_script(&params, Some(&patch)).unwrap();

    // Verify 0 unexpanded tokens and size under 256 KiB
    assert!(!script.contains("{{"));
    assert!(script.len() < 256 * 1024);

    // Verify host patch script injected before entrypoint anchor
    assert!(script.contains("/opt/gha-runners/repo-host-patch.sh"));
    assert!(script.contains("chmod 0755 /opt/gha-runners/repo-host-patch.sh"));

    // Verify patch pending and completion receipt commands injected
    assert!(script.contains("/var/lib/gha-runner-patch/pending.json"));
    assert!(script.contains("/var/lib/gha-runner-patch/state.json"));
    assert!(script.contains("state.json.tmp"));

    // Verify extra docker args injected
    assert!(script.contains("--sysctl=net.core.somaxconn=4096"));
    assert!(script.contains("/mnt/runner-data/custom-cache:/opt/custom-cache"));
}
