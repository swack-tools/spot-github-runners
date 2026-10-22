//! Tests for Patch Console Git Data & Pull Request Workflow, Manifest Loading,
//! Sizing Validation API, and Multi-Repo Orchestration on Mock GitHub Transport.

use async_trait::async_trait;
use serde_json::json;
use spot_dashboard_edge::patch::{compute_canonical_patch_sha256, CONTROLLED_TARGETS};
use spot_dashboard_edge::patch_console::{
    handle_bootstrap, handle_manifest, handle_preview, handle_save, handle_validate,
    make_valid_manifest_json, GitHubTransport, MockGitHubTransport, CONFIG_REPO,
};

#[test]
fn test_patch_console_bootstrap() {
    let (status, body) = handle_bootstrap();
    assert_eq!(status, 200);
    assert_eq!(body["repository"], CONFIG_REPO);

    let targets = body["controlled_targets"]
        .as_array()
        .expect("array of targets");
    assert_eq!(targets.len(), 4);
    for t in CONTROLLED_TARGETS {
        assert!(
            targets.iter().any(|v| v.as_str() == Some(*t)),
            "Target {} must be in bootstrap",
            t
        );
    }
}

#[tokio::test]
async fn test_patch_console_manifest_loading_controlled_targets() {
    let mock = MockGitHubTransport::new();

    for target in CONTROLLED_TARGETS {
        let (status, body) = handle_manifest(target, &mock, "mock_token").await;
        assert_eq!(
            status, 200,
            "Manifest load for {} should return 200",
            target
        );
        assert_eq!(body["repository"], *target);
        assert!(body["manifest"].is_object());

        let base_sha = body["base_sha"].as_str().unwrap();
        assert_eq!(base_sha.len(), 40);
        assert!(base_sha.chars().all(|c| c.is_ascii_hexdigit()));

        let blob_sha = body["blob_sha"].as_str().unwrap();
        assert_eq!(blob_sha.len(), 40);
        assert!(blob_sha.chars().all(|c| c.is_ascii_hexdigit()));

        let patch_sha = body["patch_sha256"].as_str().unwrap();
        assert_eq!(patch_sha.len(), 64);
        let expected_sha = compute_canonical_patch_sha256(&body["manifest"]).unwrap();
        assert_eq!(patch_sha, expected_sha);
    }
}

#[tokio::test]
async fn test_patch_console_manifest_loading_unauthorized_and_path_traversal() {
    let mock = MockGitHubTransport::new();
    let attacks = &[
        "../../etc/passwd",
        "",
        " swack-tools ",
        "SWACK-TOOLS",
        "swack.*",
        "swack-tools-fake",
        "swack-tools/extra",
        "xbmc4lyfe.evil.com/rustnzb",
        "random/unauthorized",
    ];

    for bad_repo in attacks {
        let (status, body) = handle_manifest(bad_repo, &mock, "mock_token").await;
        assert_eq!(
            status, 400,
            "Target '{}' must be rejected with 400",
            bad_repo
        );
        assert_eq!(body["error"], "unsupported_or_missing_repository");
    }
}

#[test]
fn test_patch_console_validate_api() {
    // 1. Valid manifest
    let valid = make_valid_manifest_json("swack-tools/oxidex");
    let (status, body) = handle_validate(&valid);
    assert_eq!(status, 200);
    assert_eq!(body["valid"], true);
    let sha = body["patch_sha256"].as_str().unwrap();
    assert_eq!(sha.len(), 64);

    // 2. Hardware sizing floor violations
    let mut bad_vcpu = valid.clone();
    bad_vcpu["hardware"]["min_vcpu"] = json!(15);
    let (status, body) = handle_validate(&bad_vcpu);
    assert_eq!(status, 400);
    assert_eq!(body["valid"], false);
    let errs = body["errors"].as_array().unwrap();
    assert!(errs
        .iter()
        .any(|e| e.as_str().unwrap().contains("min_vcpu_below_floor")));

    // 3. Permitted machine type violation
    let mut bad_type = valid.clone();
    bad_type["hardware"]["extra_labels"] = json!(["type-e2-standard-4"]);
    let (status, body) = handle_validate(&bad_type);
    assert_eq!(status, 400);
    let errs = body["errors"].as_array().unwrap();
    assert!(errs
        .iter()
        .any(|e| e.as_str().unwrap().contains("unpermitted_machine_type")));

    // 4. Forbidden mount violation
    let mut bad_mount = valid.clone();
    bad_mount["container"]["mounts"] =
        json!([{"source": "/var/run/docker.sock", "target": "/var/run/docker.sock"}]);
    let (status, body) = handle_validate(&bad_mount);
    assert_eq!(status, 400);
    let errs = body["errors"].as_array().unwrap();
    assert!(errs
        .iter()
        .any(|e| e.as_str().unwrap().contains("forbidden_mount_source")));
}

#[tokio::test]
async fn test_patch_console_save_pr_happy_path() {
    let mock = MockGitHubTransport::new();
    let m = make_valid_manifest_json("swack-tools");
    let save_body = json!({"manifest": m});

    let (status, body) = handle_save(&save_body, &mock, "ghp_mock_token_12345").await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "pr_created");
    assert_eq!(body["repository"], CONFIG_REPO);
    assert_eq!(body["target_repo"], "swack-tools");

    let branch = body["branch"].as_str().unwrap();
    assert!(branch.starts_with("spot-dashboard/swack-tools-"));
    assert_less_than(branch.len(), 100);

    let pr_url = body["pr_url"].as_str().unwrap();
    assert!(pr_url.contains(&format!("https://github.com/{}/pull/", CONFIG_REPO)));

    let calls = mock.calls.lock().unwrap();
    assert!(calls
        .iter()
        .any(|(m, p, _)| m == "GET" && p.contains("/git/ref/heads/main")));
    assert!(calls
        .iter()
        .any(|(m, p, _)| m == "POST" && p.contains("/git/refs")));
    assert!(calls
        .iter()
        .any(|(m, p, _)| m == "PUT" && p.contains("/contents/defaults.yaml")));
    assert!(calls
        .iter()
        .any(|(m, p, _)| m == "POST" && p.contains("/pulls")));
}

#[tokio::test]
async fn test_patch_console_save_pr_multi_repo_targets() {
    let mock = MockGitHubTransport::new();
    let m = make_valid_manifest_json("xbmc4lyfe/rustnzb");
    let save_body = json!({"manifest": m});

    let (status, body) = handle_save(&save_body, &mock, "ghp_mock_token_12345").await;
    assert_eq!(status, 200);
    assert_eq!(body["target_repo"], "xbmc4lyfe/rustnzb");

    let branch = body["branch"].as_str().unwrap();
    assert!(branch.starts_with("spot-dashboard/xbmc4lyfe-rustnzb-"));

    let calls = mock.calls.lock().unwrap();
    assert!(calls
        .iter()
        .any(|(m, p, _)| m == "PUT" && p.contains("/contents/repos/xbmc4lyfe/rustnzb.yaml")));
}

#[tokio::test]
async fn test_patch_console_save_validation_failures() {
    let mock = MockGitHubTransport::new();

    // 1. Missing manifest
    let (status, body) = handle_save(&json!({}), &mock, "token").await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "missing_manifest");

    // 2. Null manifest
    let (status, body) = handle_save(&json!({"manifest": null}), &mock, "token").await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "missing_manifest");

    // 3. Unsupported repository
    let mut bad_repo = make_valid_manifest_json("swack-tools");
    bad_repo["repository"] = json!("unauthorized/repo");
    let (status, body) = handle_save(&json!({"manifest": bad_repo}), &mock, "token").await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "unsupported_repository");

    // 4. Sizing floor violation
    let mut bad_sizing = make_valid_manifest_json("swack-tools");
    bad_sizing["hardware"]["min_vcpu"] = json!(8);
    let (status, body) = handle_save(&json!({"manifest": bad_sizing}), &mock, "token").await;
    assert_eq!(status, 400);
    assert_eq!(body["valid"], false);
}

/// Transport that fails during PUT file commit to trigger rollback.
struct FailingCommitTransport {
    mock: MockGitHubTransport,
}

#[async_trait(?Send)]
impl GitHubTransport for FailingCommitTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        if method == "PUT" && path.contains("/contents/") {
            return Ok((500, b"{\"message\": \"Internal Server Error\"}".to_vec()));
        }
        self.mock.request(method, path, headers, body).await
    }
}

#[tokio::test]
async fn test_patch_console_save_commit_failure_and_rollback() {
    let failing_transport = FailingCommitTransport {
        mock: MockGitHubTransport::new(),
    };
    let m = make_valid_manifest_json("swack-tools");
    let save_body = json!({"manifest": m});

    let (status, body) = handle_save(&save_body, &failing_transport, "ghp_mock_token").await;
    assert_eq!(status, 502);
    assert_eq!(body["error"], "github_commit_failed");

    // Verify rollback DELETE was attempted
    let calls = failing_transport.mock.calls.lock().unwrap();
    assert!(calls
        .iter()
        .any(|(m, p, _)| m == "DELETE" && p.contains("/git/refs/heads/")));
}

#[test]
fn test_patch_console_preview() {
    let cur = make_valid_manifest_json("swack-tools");
    let mut tgt = cur.clone();

    // 1. Identical -> noop
    let (status, body) = handle_preview(&json!({"current_manifest": cur, "manifest": tgt}));
    assert_eq!(status, 200);
    assert_eq!(body["action"], "noop");

    // 2. Add apt package -> hot_patch
    tgt["container"]["apt_packages"]
        .as_array_mut()
        .unwrap()
        .push(json!("wget"));
    let (status, body) = handle_preview(&json!({"current_manifest": cur, "manifest": tgt}));
    assert_eq!(status, 200);
    assert_eq!(body["action"], "hot_patch");

    // 3. Sysctl change -> replace_vm
    let mut tgt_sysctl = cur.clone();
    tgt_sysctl["host"]["sysctl"]["net.core.somaxconn"] = json!("8192");
    let (status, body) = handle_preview(&json!({"current_manifest": cur, "manifest": tgt_sysctl}));
    assert_eq!(status, 200);
    assert_eq!(body["action"], "replace_vm");

    // 4. Dockerfile change -> recreate_containers
    let mut tgt_docker = cur.clone();
    tgt_docker["container"]["dockerfile_run"] = json!("RUN echo test");
    let (status, body) = handle_preview(&json!({"current_manifest": cur, "manifest": tgt_docker}));
    assert_eq!(status, 200);
    assert_eq!(body["action"], "recreate_containers");
}

fn assert_less_than(a: usize, b: usize) {
    assert!(a < b, "Expected {} < {}", a, b);
}
