//! Empirical Adversarial Challenger Test Suite for Milestone 2
//! Tests hardware sizing floor boundaries, permitted machine types, forbidden mounts,
//! and patch console error paths (unauthorized targets, path traversal, base SHA conflict, PUT failure rollback).

use async_trait::async_trait;
use serde_json::json;
use spot_dashboard_edge::patch::{
    validate_patch_manifest, FORBIDDEN_MOUNTS, PERMITTED_MACHINE_TYPES,
};
use spot_dashboard_edge::patch_console::{
    handle_manifest, handle_save, make_valid_manifest_json,
    manifest_path_for_target, GitHubTransport, MockGitHubTransport,
};

// ============================================================================
// 1. HARDWARE SIZING BOUNDARIES (EXHAUSTIVE ORACLES)
// ============================================================================

#[test]
fn test_sizing_boundary_vcpu_15_vs_16() {
    let mut m = make_valid_manifest_json("swack-tools");

    // 15 vCPU: must fail floor check
    m["hardware"]["min_vcpu"] = json!(15);
    let errs15 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs15.iter().any(|e| e.contains("min_vcpu_below_floor: 15 < 16")),
        "15 vCPU must fail floor: {:?}",
        errs15
    );

    // 16 vCPU: must pass floor check
    m["hardware"]["min_vcpu"] = json!(16);
    assert!(validate_patch_manifest(&m).is_ok(), "16 vCPU must pass");

    // 0 and 1 vCPU: must fail
    m["hardware"]["min_vcpu"] = json!(0);
    assert!(validate_patch_manifest(&m).is_err(), "0 vCPU must fail");
    m["hardware"]["min_vcpu"] = json!(1);
    assert!(validate_patch_manifest(&m).is_err(), "1 vCPU must fail");
}

#[test]
fn test_sizing_boundary_ram_31_vs_32() {
    let mut m = make_valid_manifest_json("swack-tools");

    // 31 GiB RAM: must fail floor check
    m["hardware"]["min_ram_gib"] = json!(31);
    let errs31 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs31.iter().any(|e| e.contains("min_ram_gib_below_floor: 31 < 32")),
        "31 GiB RAM must fail floor: {:?}",
        errs31
    );

    // 32 GiB RAM: must pass floor check
    m["hardware"]["min_ram_gib"] = json!(32);
    assert!(validate_patch_manifest(&m).is_ok(), "32 GiB RAM must pass");

    // 0 and 16 GiB RAM: must fail
    m["hardware"]["min_ram_gib"] = json!(0);
    assert!(validate_patch_manifest(&m).is_err(), "0 GiB RAM must fail");
    m["hardware"]["min_ram_gib"] = json!(16);
    assert!(validate_patch_manifest(&m).is_err(), "16 GiB RAM must fail");
}

#[test]
fn test_sizing_boundary_runners_count_1_vs_2_vs_3() {
    let mut m = make_valid_manifest_json("swack-tools");

    // 1 runner: must fail
    m["hardware"]["runners_count"] = json!(1);
    let errs1 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs1.iter().any(|e| e.contains("runners_count_must_be_2: got 1")),
        "1 runner must fail: {:?}",
        errs1
    );

    // 2 runners: must pass
    m["hardware"]["runners_count"] = json!(2);
    assert!(validate_patch_manifest(&m).is_ok(), "2 runners must pass");

    // 3 runners: must fail
    m["hardware"]["runners_count"] = json!(3);
    let errs3 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs3.iter().any(|e| e.contains("runners_count_must_be_2: got 3")),
        "3 runners must fail: {:?}",
        errs3
    );

    // 0 runners: must fail
    m["hardware"]["runners_count"] = json!(0);
    assert!(validate_patch_manifest(&m).is_err(), "0 runners must fail");
}

#[test]
fn test_sizing_boundary_runner_vcpu_6_vs_7_vs_8() {
    let mut m = make_valid_manifest_json("swack-tools");

    // 6 vCPU per runner: must fail
    m["hardware"]["runner_vcpu"] = json!(6);
    let errs6 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs6.iter().any(|e| e.contains("runner_vcpu_must_be_7: got 6")),
        "6 vCPU per runner must fail: {:?}",
        errs6
    );

    // 7 vCPU per runner: must pass
    m["hardware"]["runner_vcpu"] = json!(7);
    assert!(validate_patch_manifest(&m).is_ok(), "7 vCPU per runner must pass");

    // 8 vCPU per runner: must fail
    m["hardware"]["runner_vcpu"] = json!(8);
    let errs8 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs8.iter().any(|e| e.contains("runner_vcpu_must_be_7: got 8")),
        "8 vCPU per runner must fail: {:?}",
        errs8
    );
}

#[test]
fn test_sizing_boundary_runner_ram_13_vs_14_vs_15() {
    let mut m = make_valid_manifest_json("swack-tools");

    // 13 GiB RAM per runner: must fail
    m["hardware"]["runner_ram_gib"] = json!(13);
    let errs13 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs13.iter().any(|e| e.contains("runner_ram_gib_must_be_14: got 13")),
        "13 GiB RAM per runner must fail: {:?}",
        errs13
    );

    // 14 GiB RAM per runner: must pass
    m["hardware"]["runner_ram_gib"] = json!(14);
    assert!(validate_patch_manifest(&m).is_ok(), "14 GiB RAM per runner must pass");

    // 15 GiB RAM per runner: must fail
    m["hardware"]["runner_ram_gib"] = json!(15);
    let errs15 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs15.iter().any(|e| e.contains("runner_ram_gib_must_be_14: got 15")),
        "15 GiB RAM per runner must fail: {:?}",
        errs15
    );
}

#[test]
fn test_sizing_boundary_host_overhead_3_vs_4_vs_5() {
    let mut m = make_valid_manifest_json("swack-tools");

    // 3 GiB host overhead: must fail
    m["hardware"]["host_overhead_gib"] = json!(3);
    let errs3 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs3.iter().any(|e| e.contains("host_overhead_gib_must_be_4: got 3")),
        "3 GiB overhead must fail: {:?}",
        errs3
    );

    // 4 GiB host overhead: must pass
    m["hardware"]["host_overhead_gib"] = json!(4);
    assert!(validate_patch_manifest(&m).is_ok(), "4 GiB overhead must pass");

    // 5 GiB host overhead: must fail
    m["hardware"]["host_overhead_gib"] = json!(5);
    let errs5 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs5.iter().any(|e| e.contains("host_overhead_gib_must_be_4: got 5")),
        "5 GiB overhead must fail: {:?}",
        errs5
    );
}

#[test]
fn test_sizing_boundary_queued_jobs_threshold_19_vs_20() {
    let mut m = make_valid_manifest_json("swack-tools");

    // 19 queued jobs threshold: must fail
    m["hardware"]["queued_jobs_threshold"] = json!(19);
    let errs19 = validate_patch_manifest(&m).unwrap_err();
    assert!(
        errs19.iter().any(|e| e.contains("queued_jobs_threshold_below_floor: 19 < 20")),
        "19 queued jobs threshold must fail: {:?}",
        errs19
    );

    // 20 queued jobs threshold: must pass
    m["hardware"]["queued_jobs_threshold"] = json!(20);
    assert!(validate_patch_manifest(&m).is_ok(), "20 queued jobs threshold must pass");

    // 21 queued jobs threshold: must pass
    m["hardware"]["queued_jobs_threshold"] = json!(21);
    assert!(validate_patch_manifest(&m).is_ok(), "21 queued jobs threshold must pass");
}

// ============================================================================
// 2. MACHINE TYPES VALIDATION
// ============================================================================

#[test]
fn test_machine_types_invalid_shapes_and_casing() {
    let mut m = make_valid_manifest_json("swack-tools");

    let invalid_shapes = &[
        "e2-standard-4",
        "c4d-highcpu-8",
        "c4d-highcpu-32",
        "c4d-standard-16",
        "type-c4d-highcpu-16", // nested type- prefix
        "C4D-HIGHCPU-16",      // uppercase casing
        "c4d-HighCpu-16",      // mixed casing
        "N4D-HIGHCPU-16",      // uppercase
        "n4d-Standard-16",     // invalid type + mixed case
        "c2d-standard-16",     // standard instead of highcpu
        "t2d-standard-16",     // non-approved family
    ];

    for bad in invalid_shapes {
        m["hardware"]["extra_labels"] = json!([format!("type-{}", bad)]);
        let errs = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("unpermitted_machine_type")),
            "Invalid machine type '{}' must be rejected: {:?}",
            bad,
            errs
        );
    }
}

#[test]
fn test_machine_types_all_permitted_shapes() {
    let mut m = make_valid_manifest_json("swack-tools");

    for permitted in PERMITTED_MACHINE_TYPES {
        m["hardware"]["extra_labels"] = json!([format!("type-{}", permitted)]);
        assert!(
            validate_patch_manifest(&m).is_ok(),
            "Permitted machine type '{}' must pass validation",
            permitted
        );
    }
}

// ============================================================================
// 3. FORBIDDEN MOUNTS ENFORCEMENT
// ============================================================================

#[test]
fn test_forbidden_mounts_exact_and_subpaths() {
    let mut m = make_valid_manifest_json("swack-tools");

    let forbidden_targets = &[
        "/var/run/docker.sock",
        "/run/docker.sock",
        "/proc",
        "/sys",
        "/etc",
        "/root",
        "/dev",
    ];

    for forbidden in forbidden_targets {
        // Exact forbidden mount
        m["container"]["mounts"] = json!([
            {"source": *forbidden, "target": "/data"}
        ]);
        let errs_exact = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs_exact.iter().any(|e| e.contains("forbidden_mount_source")),
            "Exact forbidden mount '{}' must be rejected: {:?}",
            forbidden,
            errs_exact
        );

        // Subpath forbidden mount
        m["container"]["mounts"] = json!([
            {"source": format!("{}/subpath/item", forbidden), "target": "/data"}
        ]);
        let errs_sub = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs_sub.iter().any(|e| e.contains("forbidden_mount_source")),
            "Subpath mount '{}/subpath/item' must be rejected: {:?}",
            forbidden,
            errs_sub
        );

        // Trailing slash forbidden mount
        m["container"]["mounts"] = json!([
            {"source": format!("{}/", forbidden), "target": "/data"}
        ]);
        let errs_slash = validate_patch_manifest(&m).unwrap_err();
        assert!(
            errs_slash.iter().any(|e| e.contains("forbidden_mount_source")),
            "Trailing slash mount '{}/' must be rejected: {:?}",
            forbidden,
            errs_slash
        );
    }

    // Verify all listed constants in FORBIDDEN_MOUNTS are rejected
    for f in FORBIDDEN_MOUNTS {
        m["container"]["mounts"] = json!([
            {"source": *f, "target": "/data"}
        ]);
        assert!(
            validate_patch_manifest(&m).is_err(),
            "Constant FORBIDDEN_MOUNTS '{}' must be rejected",
            f
        );
    }
}

// ============================================================================
// 4. PATCH CONSOLE ERROR PATHS
// ============================================================================

#[tokio::test]
async fn test_patch_console_error_paths_unauthorized_and_traversal() {
    let mock = MockGitHubTransport::new();

    // 1. Path traversal attacks on manifest endpoint
    let traversals = &[
        "../defaults.yaml",
        "../../etc/passwd",
        "repos/../../../etc/shadow",
        "..",
        "./swack-tools",
    ];
    for t in traversals {
        let (status, body) = handle_manifest(t, &mock, "mock_token").await;
        assert_eq!(status, 400, "Traversal path '{}' must return 400", t);
        assert_eq!(body["error"], "unsupported_or_missing_repository");
        assert!(manifest_path_for_target(t).is_err(), "manifest_path_for_target must reject '{}'", t);
    }

    // 2. Unauthorized targets on manifest endpoint
    let unauthorized = &[
        "unauthorized/repo",
        "evil-corp/backdoor",
        "swack-tools/random",
        "xbmc4lyfe/unregistered",
        "",
    ];
    for u in unauthorized {
        let (status, body) = handle_manifest(u, &mock, "mock_token").await;
        assert_eq!(status, 400, "Unauthorized target '{}' must return 400", u);
        assert_eq!(body["error"], "unsupported_or_missing_repository");

        // Save on unauthorized target
        let save_req = json!({
            "manifest": {
                "schema": "spot-runner-patch-v1",
                "repository": u
            }
        });
        let (save_status, save_body) = handle_save(&save_req, &mock, "mock_token").await;
        assert_eq!(save_status, 400, "Save for unauthorized target '{}' must return 400", u);
        assert_eq!(save_body["error"], "unsupported_repository");
    }
}

// Transport that simulates GitHub API 409 Conflict on file commit PUT
struct ConflictPutTransport {
    mock: MockGitHubTransport,
}

#[async_trait(?Send)]
impl GitHubTransport for ConflictPutTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        if method == "PUT" && path.contains("/contents/") {
            // Return 409 Conflict (e.g. concurrent commit or base SHA mismatch)
            return Ok((409, b"{\"message\": \"Conflict: file was modified concurrently\"}".to_vec()));
        }
        self.mock.request(method, path, headers, body).await
    }
}

#[tokio::test]
async fn test_patch_console_put_failure_rollback_and_status() {
    let conflict_transport = ConflictPutTransport {
        mock: MockGitHubTransport::new(),
    };

    let m = make_valid_manifest_json("swack-tools");
    let save_body = json!({
        "manifest": m,
        "base_sha": "a".repeat(40),
    });

    let (status, body) = handle_save(&save_body, &conflict_transport, "ghp_token").await;

    // 1. Verify rollback branch deletion occurred via DELETE
    let calls = conflict_transport.mock.calls.lock().unwrap();
    let delete_call = calls.iter().find(|(method, path, _)| {
        method == "DELETE" && path.contains("/git/refs/heads/spot-dashboard/swack-tools-")
    });
    assert!(
        delete_call.is_some(),
        "Rollback DELETE must be called on branch when PUT fails"
    );

    // 2. Note HTTP status returned when PUT fails:
    // Current implementation maps PUT failure to HTTP 502 with error: github_commit_failed
    assert_eq!(status, 502, "PUT failure maps to 502 Bad Gateway");
    assert_eq!(body["error"], "github_commit_failed");
}
