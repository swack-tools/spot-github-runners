use spot_dashboard_edge::reconciler::{
    reset_reconciler_state, run_reconciliation, SimulatedFleetProvider,
};
use spot_dashboard_edge::state::{
    handle_get_rust_build_status, handle_get_rust_builds_queue, handle_post_rust_builds_claim,
    handle_post_rust_builds_complete, handle_post_rust_builds_enqueue, invalidate_state_cache,
    load_state, ClaimRustBuildRequest, CompleteRustBuildRequest, EnqueueRustBuildRequest,
    KvStoreBackend, StateError,
};
use std::collections::HashMap;
use std::sync::Mutex;

struct MockKvStore {
    data: Mutex<HashMap<String, String>>,
}

impl MockKvStore {
    fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for MockKvStore {
    async fn get(&self, key: &str) -> Result<Option<String>, StateError> {
        Ok(self.data.lock().unwrap().get(key).cloned())
    }

    async fn put(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.data
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn test_enqueue_poll_claim_complete_and_three_artifact_selection() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1_792_000_000;

    // 1. Enqueue Build #1 (wants target_zip)
    let req1 = EnqueueRustBuildRequest {
        repo: "example-org/example-rust-project".to_string(),
        branch: "feat/remote-queue".to_string(),
        commit_sha: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string(),
        command: "cargo test --workspace --locked".to_string(),
        requested_output: Some("target_zip".to_string()),
        template_id: Some("rust-builder-default".to_string()),
        profile: Some("test".to_string()),
        binary_name: None,
        gcs_bucket: Some("example-rust-project-build-artifacts".to_string()),
        origin_ip: Some("198.51.100.42".to_string()),
    };
    let (status1, body1) = handle_post_rust_builds_enqueue(&kv, req1, now)
        .await
        .expect("enqueue 1");
    assert_eq!(status1, 200);
    assert_eq!(body1["build_id"], 4553);
    assert_eq!(body1["build_status"], "queued");
    assert_eq!(body1["queue_position"], 1);
    assert_eq!(body1["scaling_up"], false); // threshold is 2 by default
    assert_eq!(body1["eta_seconds"], 600); // default 600s avg * 1 wave

    // 2. Enqueue Build #2 (wants binary) -> hits queued_builds_threshold (2) -> triggers scale-up!
    let req2 = EnqueueRustBuildRequest {
        repo: "example-org/example-rust-project".to_string(),
        branch: "feat/binary-check".to_string(),
        commit_sha: "b2c3d4e5f60718293a4b5c6d7e8f90123456789a".to_string(),
        command: "cargo build --release --locked".to_string(),
        requested_output: Some("binary".to_string()),
        template_id: None,
        profile: Some("release".to_string()),
        binary_name: Some("example-rust-project".to_string()),
        gcs_bucket: None,
        origin_ip: Some("198.51.100.43".to_string()),
    };
    let (status2, body2) = handle_post_rust_builds_enqueue(&kv, req2, now + 5)
        .await
        .expect("enqueue 2");
    assert_eq!(status2, 200);
    assert_eq!(body2["build_id"], 4554);
    assert_eq!(body2["queue_position"], 2);
    assert_eq!(body2["scaling_up"], true);
    assert_eq!(body2["scaling_decision"], "SCALE_UP");
    assert!(body2["message"]
        .as_str()
        .unwrap()
        .contains("Build #4554 is #2 in line"));
    assert!(body2["message"]
        .as_str()
        .unwrap()
        .contains("scaling up right now"));

    // 3. Poll queue unauthenticated -> origin_ip must be redacted
    let (_, q_unauth) = handle_get_rust_builds_queue(&kv, false, now + 6)
        .await
        .expect("get queue unauth");
    assert_eq!(q_unauth["queued_builds"], 2);
    assert!(q_unauth["queue"][0].get("origin_ip").is_none());

    let (_, s_unauth) = handle_get_rust_build_status(&kv, 4553, false, now + 6)
        .await
        .expect("get status unauth");
    assert!(s_unauth["build"].get("origin_ip").is_none());

    // 4. Builder claims first queued build (#4553) in FIFO order
    let claim_req = ClaimRustBuildRequest {
        instance_name: "builder-us-central1-a-01".to_string(),
        zone: Some("us-central1-a".to_string()),
    };
    let (c_status, c_body) = handle_post_rust_builds_claim(&kv, claim_req, now + 10)
        .await
        .expect("claim 1");
    assert_eq!(c_status, 200);
    assert_eq!(c_body["claimed"], true);
    assert_eq!(c_body["build"]["build_id"], 4553);
    assert_eq!(c_body["build"]["status"], "running");
    assert_eq!(
        c_body["upload_targets"]["log_url"],
        "gs://example-rust-project-build-artifacts/builds/4553/build.log"
    );
    assert_eq!(
        c_body["upload_targets"]["binary_url"],
        "gs://example-rust-project-build-artifacts/builds/4553/binaries.tar.zst"
    );
    assert_eq!(
        c_body["upload_targets"]["target_zip_url"],
        "gs://example-rust-project-build-artifacts/builds/4553/target.tar.zst"
    );

    // Poll #4554 -> now #1 in line!
    let (_, poll_4554) = handle_get_rust_build_status(&kv, 4554, true, now + 12)
        .await
        .expect("poll 4554");
    assert_eq!(poll_4554["queue_position"], 1);
    assert_eq!(poll_4554["running_builds"], 1);

    // 5. Complete #4553 with 1200s duration -> updates rolling average to 1202s and selects target_zip_url
    let comp_req = CompleteRustBuildRequest {
        build_id: Some(4553),
        status: "completed".to_string(),
        exit_code: Some(0),
        compile_duration_seconds: Some(1200.0),
        delivery_duration_seconds: Some(2.0),
        instance_name: Some("builder-us-central1-a-01".to_string()),
        zone: Some("us-central1-a".to_string()),
        machine_type: Some("n4-highcpu-16".to_string()),
        vcpu: Some(16),
        cache_hits: Some(380),
        total_crates: Some(400),
        peak_ram_gib: Some(21.5),
        cpu_time_seconds: Some(9500.0),
        log_url: None,
        binary_url: None,
        target_zip_url: None,
        log_size_bytes: Some(48_000),
        binary_size_bytes: Some(14_500_000),
        target_zip_size_bytes: Some(98_000_000),
        log_sha256: Some("11".repeat(32)),
        binary_sha256: Some("22".repeat(32)),
        target_zip_sha256: Some("33".repeat(32)),
    };
    let (done_status, done_body) =
        handle_post_rust_builds_complete(&kv, 4553, comp_req, now + 1210)
            .await
            .expect("complete 4553");
    assert_eq!(done_status, 200);
    assert_eq!(done_body["build_status"], "completed");
    assert_eq!(done_body["queue_position"], 0);
    // Because #4553 requested "target_zip", selected_artifact_url is target.tar.zst, while all 3 URLs exist in artifacts
    assert_eq!(
        done_body["selected_artifact_url"],
        "gs://example-rust-project-build-artifacts/builds/4553/target.tar.zst"
    );
    assert_eq!(
        done_body["artifacts"]["log_url"],
        "gs://example-rust-project-build-artifacts/builds/4553/build.log"
    );
    assert_eq!(
        done_body["artifacts"]["binary_url"],
        "gs://example-rust-project-build-artifacts/builds/4553/binaries.tar.zst"
    );
    assert_eq!(
        done_body["artifacts"]["target_zip_url"],
        "gs://example-rust-project-build-artifacts/builds/4553/target.tar.zst"
    );

    // Poll #4554 again -> ETA now reflects the new 1202s rolling average!
    let (_, poll_4554_after) = handle_get_rust_build_status(&kv, 4554, true, now + 1215)
        .await
        .expect("poll 4554 after");
    assert_eq!(poll_4554_after["average_build_duration_seconds"], 1202.0);
    assert_eq!(poll_4554_after["eta_seconds"], 1202);
}

#[tokio::test]
async fn test_reconciler_scales_up_rust_builder_when_queue_reaches_threshold() {
    reset_reconciler_state();
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1_792_010_000;

    // Enqueue 2 builds so queued_rust_builds (2) >= queued_builds_threshold (2)
    for i in 0..2 {
        let req = EnqueueRustBuildRequest {
            repo: "example-org/example-rust-project".to_string(),
            branch: "main".to_string(),
            commit_sha: format!("abcdef0123456789abcdef0123456789abcdef0{i}"),
            command: "cargo test --workspace".to_string(),
            requested_output: Some("log".to_string()),
            template_id: None,
            profile: None,
            binary_name: None,
            gcs_bucket: None,
            origin_ip: None,
        };
        handle_post_rust_builds_enqueue(&kv, req, now + i)
            .await
            .expect("enqueue");
    }

    let sim = SimulatedFleetProvider;
    let report = run_reconciliation(&kv, &sim, &sim, now + 10)
        .await
        .expect("reconcile");
    assert!(
        report
            .launched_hosts
            .iter()
            .any(|h| h.starts_with("builder-scale-")),
        "Expected reconciler to launch a builder-scale-* VM when queue backlog >= threshold, got: {:?}",
        report.launched_hosts
    );

    let (bundle, _) = load_state(&kv, now + 10).await.unwrap();
    assert_eq!(
        bundle.state.scaling_evaluations["rust_builder"]["decision"],
        "SCALE_UP"
    );

    // Also verify that when 0 live builders exist and even a single build (1 < threshold 2)
    // is queued, the reconciler provisions a bootstrap builder to satisfy min_instances (1)
    reset_reconciler_state();
    invalidate_state_cache();
    let kv_single = MockKvStore::new();
    let req_single = EnqueueRustBuildRequest {
        repo: "example-org/example-rust-project".to_string(),
        branch: "main".to_string(),
        commit_sha: "abcdef0123456789abcdef0123456789abcdef99".to_string(),
        command: "cargo test --workspace".to_string(),
        requested_output: Some("log".to_string()),
        template_id: None,
        profile: None,
        binary_name: None,
        gcs_bucket: None,
        origin_ip: None,
    };
    handle_post_rust_builds_enqueue(&kv_single, req_single, now + 20)
        .await
        .expect("enqueue single");
    let report_single = run_reconciliation(&kv_single, &sim, &sim, now + 30)
        .await
        .expect("reconcile single");
    assert!(
        report_single
            .launched_hosts
            .iter()
            .any(|h| h.starts_with("builder-scale-")),
        "Expected reconciler to provision a builder when 1 build is queued and 0 builders are running, got: {:?}",
        report_single.launched_hosts
    );
}

#[tokio::test]
async fn test_validation_rejects_invalid_inputs_and_non_finite_floats() {
    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1_792_020_000;

    // Invalid commit_sha
    let bad_sha = EnqueueRustBuildRequest {
        repo: "example-org/example-rust-project".to_string(),
        branch: "main".to_string(),
        commit_sha: "not-a-hex-sha!".to_string(),
        command: "cargo test".to_string(),
        requested_output: Some("log".to_string()),
        template_id: None,
        profile: None,
        binary_name: None,
        gcs_bucket: None,
        origin_ip: None,
    };
    assert!(handle_post_rust_builds_enqueue(&kv, bad_sha, now)
        .await
        .is_err());

    // Invalid requested_output
    let bad_out = EnqueueRustBuildRequest {
        repo: "example-org/example-rust-project".to_string(),
        branch: "main".to_string(),
        commit_sha: "abcdef0123456789".to_string(),
        command: "cargo test".to_string(),
        requested_output: Some("invalid_kind".to_string()),
        template_id: None,
        profile: None,
        binary_name: None,
        gcs_bucket: None,
        origin_ip: None,
    };
    assert!(handle_post_rust_builds_enqueue(&kv, bad_out, now)
        .await
        .is_err());

    // Enqueue valid build, then test NaN rejection on complete
    let ok_req = EnqueueRustBuildRequest {
        repo: "example-org/example-rust-project".to_string(),
        branch: "main".to_string(),
        commit_sha: "abcdef0123456789".to_string(),
        command: "cargo test".to_string(),
        requested_output: Some("log".to_string()),
        template_id: None,
        profile: None,
        binary_name: None,
        gcs_bucket: None,
        origin_ip: None,
    };
    let (_, enq) = handle_post_rust_builds_enqueue(&kv, ok_req, now)
        .await
        .unwrap();
    let bid = enq["build_id"].as_u64().unwrap();

    let nan_complete = CompleteRustBuildRequest {
        build_id: Some(bid),
        status: "completed".to_string(),
        exit_code: Some(0),
        compile_duration_seconds: Some(f64::NAN),
        delivery_duration_seconds: None,
        instance_name: None,
        zone: None,
        machine_type: None,
        vcpu: None,
        cache_hits: None,
        total_crates: None,
        peak_ram_gib: None,
        cpu_time_seconds: None,
        log_url: None,
        binary_url: None,
        target_zip_url: None,
        log_size_bytes: None,
        binary_size_bytes: None,
        target_zip_size_bytes: None,
        log_sha256: None,
        binary_sha256: None,
        target_zip_sha256: None,
    };
    assert!(
        handle_post_rust_builds_complete(&kv, bid, nan_complete, now + 1)
            .await
            .is_err()
    );
}

#[test]
fn test_builder_vm_request_attaches_persistent_cache_disk_and_excludes_runner_target() {
    use spot_dashboard_edge::gcp::{
        build_spot_builder_vm_request, BuilderVmConfig, RAW_BUILDER_STARTUP_SCRIPT,
    };

    // Stub script without host-agent / queue-worker / cache disk must be rejected
    let bad_cfg = BuilderVmConfig {
        instance_name: "builder-scale-1792000000-abcd1234",
        machine_type: "n4-highcpu-16",
        zone: "us-central1-a",
        deployment_id: "dep-builder-1792000000",
        cache_disk_name: "builder-scale-1792000000-abcd1234-cache",
        cache_disk_device_name: "builder-scale-1792000000-abcd1234-cache",
        startup_script_template: "#!/bin/bash\nset -euo pipefail\n",
        queue_bearer_token: Some("tok-123"),
        boot_disk_type: Some("hyperdisk-balanced"),
        cache_disk_type: Some("hyperdisk-balanced"),
    };
    assert!(
        build_spot_builder_vm_request("example-ci-fleet", &bad_cfg).is_err(),
        "Nonfunctional builder startup stub must be rejected"
    );

    // N4 with pd-balanced must be rejected
    let bad_pd_cfg = BuilderVmConfig {
        instance_name: "builder-scale-1792000000-abcd1234",
        machine_type: "n4-highcpu-16",
        zone: "us-central1-a",
        deployment_id: "dep-builder-1792000000",
        cache_disk_name: "builder-scale-1792000000-abcd1234-cache",
        cache_disk_device_name: "builder-scale-1792000000-abcd1234-cache",
        startup_script_template: RAW_BUILDER_STARTUP_SCRIPT,
        queue_bearer_token: Some("tok-123"),
        boot_disk_type: Some("pd-balanced"),
        cache_disk_type: Some("pd-balanced"),
    };
    assert!(
        build_spot_builder_vm_request("example-ci-fleet", &bad_pd_cfg).is_err(),
        "N4 builder VM with pd-balanced must be rejected"
    );

    let minted_tok = spot_dashboard_edge::auth::mint_builder_queue_token(
        b"test-session-secret-0123456789abcdef",
        "builder-scale-1792000000-abcd1234",
        1_792_000_000,
    )
    .expect("mint builder queue token");
    let verified = spot_dashboard_edge::auth::verify_builder_queue_token(
        &minted_tok,
        b"test-session-secret-0123456789abcdef",
        1_792_000_010,
    )
    .expect("verify builder queue token");
    assert_eq!(verified.inst, "builder-scale-1792000000-abcd1234");

    let cfg = BuilderVmConfig {
        instance_name: "builder-scale-1792000000-abcd1234",
        machine_type: "n4-highcpu-16",
        zone: "us-central1-a",
        deployment_id: "dep-builder-1792000000",
        cache_disk_name: "builder-scale-1792000000-abcd1234-cache",
        cache_disk_device_name: "builder-scale-1792000000-abcd1234-cache",
        startup_script_template: RAW_BUILDER_STARTUP_SCRIPT,
        queue_bearer_token: Some(&minted_tok),
        boot_disk_type: None,
        cache_disk_type: None,
    };

    let req = build_spot_builder_vm_request("example-ci-fleet", &cfg)
        .expect("build spot builder vm request");
    assert_eq!(
        req.disks.len(),
        2,
        "Builder VM must attach boot + cache disks"
    );
    assert_eq!(
        req.service_accounts.len(),
        1,
        "Builder VM must attach default GCE service account for same-region GCS uploads"
    );
    assert_eq!(req.service_accounts[0].email, "default");
    assert_eq!(
        req.service_accounts[0].scopes,
        vec!["https://www.googleapis.com/auth/devstorage.read_write".to_string()]
    );
    assert!(req.disks[0].boot);
    assert!(req.disks[0].auto_delete);
    assert_eq!(
        req.disks[0].initialize_params.as_ref().unwrap().disk_type,
        "zones/us-central1-a/diskTypes/hyperdisk-balanced"
    );
    assert!(!req.disks[1].boot);
    assert!(
        !req.disks[1].auto_delete,
        "Dedicated builder cache disk must persist (auto_delete = false)"
    );
    assert_eq!(
        req.disks[1].initialize_params.as_ref().unwrap().disk_type,
        "zones/us-central1-a/diskTypes/hyperdisk-balanced"
    );
    assert_eq!(
        req.disks[1].device_name.as_deref(),
        Some("builder-scale-1792000000-abcd1234-cache")
    );
    assert_eq!(
        req.disks[1]
            .initialize_params
            .as_ref()
            .unwrap()
            .disk_size_gb,
        "375"
    );

    let role_meta = req
        .metadata
        .items
        .iter()
        .find(|m| m.key == "spot-role")
        .map(|m| m.value.as_str());
    assert_eq!(role_meta, Some("rust_builder"));
    let tok_meta = req
        .metadata
        .items
        .iter()
        .find(|m| m.key == "spot-queue-token")
        .map(|m| m.value.as_str());
    assert_eq!(tok_meta, Some(minted_tok.as_str()));
    assert!(
        req.metadata.items.iter().all(|m| m.key != "spot-target"),
        "Builder VM must not set spot-target metadata used by runner floor accounting"
    );

    let startup = req
        .metadata
        .items
        .iter()
        .find(|m| m.key == "startup-script")
        .map(|m| m.value.as_str())
        .unwrap();
    assert!(startup.contains("builder-scale-1792000000-abcd1234"));
    assert!(startup.contains("example-ci-fleet"));
    assert!(startup.contains("us-central1-a"));
    assert!(startup.contains("/dev/disk/by-id/google-builder-scale-1792000000-abcd1234-cache"));
    assert!(startup.contains("dep-builder-1792000000"));
    assert!(startup.contains("example-rust-project-queue-worker.service"));
    assert!(startup.contains(&format!("queue_token='{minted_tok}'")));
    assert!(startup.contains("SPOT_BEARER_TOKEN=$queue_token"));
}

#[tokio::test]
async fn test_queue_overflow_rejects_when_all_items_unfinished_and_evicts_completed() {
    use spot_dashboard_edge::state::MAX_RUST_BUILD_QUEUE_ITEMS;

    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1_792_030_000;

    for i in 0..MAX_RUST_BUILD_QUEUE_ITEMS {
        let req = EnqueueRustBuildRequest {
            repo: "example-org/example-rust-project".to_string(),
            branch: "main".to_string(),
            commit_sha: format!("abcdef0123456789abcdef0123456789{:08x}", i),
            command: "cargo test --workspace".to_string(),
            requested_output: Some("log".to_string()),
            template_id: None,
            profile: None,
            binary_name: None,
            gcs_bucket: None,
            origin_ip: None,
        };
        handle_post_rust_builds_enqueue(&kv, req, now + (i as u64))
            .await
            .expect("enqueue within capacity");
    }

    // 201st enqueue when all 200 items are unfinished ("queued") must be rejected with Conflict
    let overflow_req = EnqueueRustBuildRequest {
        repo: "example-org/example-rust-project".to_string(),
        branch: "main".to_string(),
        commit_sha: "abcdef0123456789abcdef0123456789ffffffff".to_string(),
        command: "cargo test --workspace".to_string(),
        requested_output: Some("log".to_string()),
        template_id: None,
        profile: None,
        binary_name: None,
        gcs_bucket: None,
        origin_ip: None,
    };
    let err = handle_post_rust_builds_enqueue(&kv, overflow_req.clone(), now + 500)
        .await
        .expect_err("overflow must reject when all builds are unfinished");
    assert!(
        matches!(err, StateError::Conflict(_)),
        "Expected StateError::Conflict on full unfinished queue, got: {err:?}"
    );

    // Oldest unfinished build (#4553) must still be present
    let (_, status_first) = handle_get_rust_build_status(&kv, 4553, true, now + 501)
        .await
        .expect("build #4553 must not be evicted while unfinished");
    assert_eq!(status_first["build_status"], "queued");

    // Attempting machine completion on unclaimed queued build #4553 must be rejected with Conflict
    let unclaimed_comp = CompleteRustBuildRequest {
        build_id: Some(4553),
        status: "completed".to_string(),
        exit_code: Some(0),
        compile_duration_seconds: Some(120.0),
        delivery_duration_seconds: Some(1.0),
        instance_name: Some("builder-us-central1-a-01".to_string()),
        zone: Some("us-central1-a".to_string()),
        machine_type: None,
        vcpu: None,
        cache_hits: None,
        total_crates: None,
        peak_ram_gib: None,
        cpu_time_seconds: None,
        log_url: Some(
            "gs://example-rust-project-build-artifacts-us-central1/builds/4553/build.log"
                .to_string(),
        ),
        binary_url: Some(
            "gs://example-rust-project-build-artifacts-us-central1/builds/4553/binaries.tar.zst"
                .to_string(),
        ),
        target_zip_url: Some(
            "gs://example-rust-project-build-artifacts-us-central1/builds/4553/target.tar.zst"
                .to_string(),
        ),
        log_size_bytes: Some(1024),
        binary_size_bytes: Some(2048),
        target_zip_size_bytes: Some(4096),
        log_sha256: Some(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        ),
        binary_sha256: Some(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
        ),
        target_zip_sha256: Some(
            "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210".to_string(),
        ),
    };
    let unclaimed_err =
        handle_post_rust_builds_complete(&kv, 4553, unclaimed_comp.clone(), now + 502)
            .await
            .expect_err("unclaimed build must reject machine completion");
    assert!(
        matches!(unclaimed_err, StateError::Conflict(_)),
        "Expected StateError::Conflict for unclaimed machine completion, got: {unclaimed_err:?}"
    );

    // Once build #4553 is claimed by builder-us-central1-a-01 and completed, a new enqueue can evict the finished entry
    handle_post_rust_builds_claim(
        &kv,
        ClaimRustBuildRequest {
            instance_name: "builder-us-central1-a-01".to_string(),
            zone: Some("us-central1-a".to_string()),
        },
        now + 502,
    )
    .await
    .expect("claim #4553 before completing");
    let comp = unclaimed_comp;
    handle_post_rust_builds_complete(&kv, 4553, comp, now + 502)
        .await
        .expect("complete 4553");

    let (st, body) = handle_post_rust_builds_enqueue(&kv, overflow_req, now + 503)
        .await
        .expect("enqueue after completing oldest item");
    assert_eq!(st, 200);
    assert_eq!(body["build_id"], 4553 + (MAX_RUST_BUILD_QUEUE_ITEMS as u64));
}

#[tokio::test]
async fn test_queue_mutations_bypass_stale_isolate_cache() {
    use spot_dashboard_edge::state::{default_dashboard_bundle, set_cached_state};

    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1_792_040_000;

    // Enqueue build #4553 into KV
    let req = EnqueueRustBuildRequest {
        repo: "example-org/example-rust-project".to_string(),
        branch: "main".to_string(),
        commit_sha: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
        command: "cargo test".to_string(),
        requested_output: Some("log".to_string()),
        template_id: None,
        profile: None,
        binary_name: None,
        gcs_bucket: None,
        origin_ip: None,
    };
    handle_post_rust_builds_enqueue(&kv, req, now)
        .await
        .expect("enqueue 1");

    // Poison the in-isolate STATE_CACHE with an empty default bundle at `now + 1`
    let stale_bundle = default_dashboard_bundle(now + 1).expect("default bundle");
    set_cached_state(now + 1, stale_bundle);

    // Claiming at `now + 2` (within the 60s cache TTL) must bypass STATE_CACHE and claim #4553 from KV
    let claim_req = ClaimRustBuildRequest {
        instance_name: "builder-us-central1-a-01".to_string(),
        zone: Some("us-central1-a".to_string()),
    };
    let (_, claim_body) = handle_post_rust_builds_claim(&kv, claim_req, now + 2)
        .await
        .expect("claim must read authoritative KV state");
    assert_eq!(claim_body["claimed"], true);
    assert_eq!(claim_body["build"]["build_id"], 4553);
}

#[tokio::test]
async fn test_recover_orphaned_running_builds_on_preempted_builder_or_expired_lease() {
    use spot_dashboard_edge::state::RUST_BUILD_CLAIM_LEASE_SECS;

    invalidate_state_cache();
    let kv = MockKvStore::new();
    let now = 1_792_050_000;

    let req = EnqueueRustBuildRequest {
        repo: "example-org/example-rust-project".to_string(),
        branch: "main".to_string(),
        commit_sha: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
        command: "cargo test".to_string(),
        requested_output: Some("log".to_string()),
        template_id: None,
        profile: None,
        binary_name: None,
        gcs_bucket: None,
        origin_ip: None,
    };
    handle_post_rust_builds_enqueue(&kv, req, now)
        .await
        .expect("enqueue");

    // First builder claims #4553 at `now + 1`
    let claim1 = ClaimRustBuildRequest {
        instance_name: "builder-preempted-01".to_string(),
        zone: Some("us-central1-a".to_string()),
    };
    let (_, b1) = handle_post_rust_builds_claim(&kv, claim1, now + 1)
        .await
        .expect("claim 1");
    assert_eq!(b1["claimed"], true);
    assert_eq!(b1["build"]["build_id"], 4553);

    // Before 30m (1800s), an early heartbeat at `now + 300` is coalesced (0 KV writes, persisted == false)
    let hb_early_req = spot_dashboard_edge::state::HeartbeatRustBuildRequest {
        build_id: Some(4553),
        instance_name: "builder-preempted-01".to_string(),
    };
    let (_, hb_early_body) = spot_dashboard_edge::state::handle_post_rust_builds_heartbeat(
        &kv,
        4553,
        hb_early_req,
        now + 300,
    )
    .await
    .expect("early heartbeat coalesced");
    assert_eq!(hb_early_body["status"], "renewed");
    assert_eq!(hb_early_body["persisted"], false);

    // Also verify enqueue rejects invalid profile or binary_name containing path traversal / separators
    let mut bad_prof_req = EnqueueRustBuildRequest {
        repo: "example-org/example-rust-project".to_string(),
        branch: "main".to_string(),
        commit_sha: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
        command: "cargo build".to_string(),
        requested_output: Some("binary".to_string()),
        template_id: None,
        profile: Some("/etc".to_string()),
        binary_name: Some("shadow".to_string()),
        gcs_bucket: None,
        origin_ip: None,
    };
    assert!(
        handle_post_rust_builds_enqueue(&kv, bad_prof_req.clone(), now + 310)
            .await
            .is_err(),
        "Enqueue must reject profile with path separators"
    );
    bad_prof_req.profile = Some("release".to_string());
    bad_prof_req.binary_name = Some("../shadow".to_string());
    assert!(
        handle_post_rust_builds_enqueue(&kv, bad_prof_req, now + 311)
            .await
            .is_err(),
        "Enqueue must reject binary_name with path traversal"
    );

    // Before lease expiry, if builder-preempted-01 sends a heartbeat at `now + 3600` (>= 1800s),
    // the lease is renewed and persisted so `now + 1 + RUST_BUILD_CLAIM_LEASE_SECS` does NOT requeue #4553!
    let hb_req = spot_dashboard_edge::state::HeartbeatRustBuildRequest {
        build_id: Some(4553),
        instance_name: "builder-preempted-01".to_string(),
    };
    let (hb_st, hb_body) = spot_dashboard_edge::state::handle_post_rust_builds_heartbeat(
        &kv,
        4553,
        hb_req,
        now + 3600,
    )
    .await
    .expect("heartbeat renews lease");
    assert_eq!(hb_st, 200);
    assert_eq!(hb_body["status"], "renewed");
    assert_eq!(hb_body["persisted"], true);

    // At `now + 1 + RUST_BUILD_CLAIM_LEASE_SECS` (before the renewed lease expires),
    // another builder attempting to claim sees an empty queue because #4553's lease is still active
    let claim_early = ClaimRustBuildRequest {
        instance_name: "builder-replacement-02".to_string(),
        zone: Some("us-central1-a".to_string()),
    };
    let (_, b_early) =
        handle_post_rust_builds_claim(&kv, claim_early, now + 1 + RUST_BUILD_CLAIM_LEASE_SECS)
            .await
            .expect("claim before renewed lease expiry");
    assert_eq!(b_early["claimed"], false);

    // After the renewed lease (`now + 3600 + RUST_BUILD_CLAIM_LEASE_SECS`) expires without further heartbeats,
    // a replacement builder can reclaim #4553
    let claim2 = ClaimRustBuildRequest {
        instance_name: "builder-replacement-02".to_string(),
        zone: Some("us-central1-a".to_string()),
    };
    let (_, b2) =
        handle_post_rust_builds_claim(&kv, claim2, now + 3600 + RUST_BUILD_CLAIM_LEASE_SECS)
            .await
            .expect("claim 2 after renewed lease expiry");
    assert_eq!(b2["claimed"], true);
    assert_eq!(b2["build"]["build_id"], 4553);
    assert_eq!(b2["build"]["assigned_instance"], "builder-replacement-02");

    // Also verify builder queue token renewal threshold detection
    let tok = spot_dashboard_edge::auth::mint_builder_queue_token(
        b"test-secret",
        "builder-replacement-02",
        now,
    )
    .expect("mint token");
    let verified_fresh =
        spot_dashboard_edge::auth::verify_builder_queue_token(&tok, b"test-secret", now + 86_400)
            .expect("verify fresh");
    assert!(
        !spot_dashboard_edge::auth::should_renew_builder_queue_token(&verified_fresh, now + 86_400)
    );
    let verified_near_expiry = spot_dashboard_edge::auth::verify_builder_queue_token(
        &tok,
        b"test-secret",
        now + 5 * 86_400,
    )
    .expect("verify near expiry");
    assert!(spot_dashboard_edge::auth::should_renew_builder_queue_token(
        &verified_near_expiry,
        now + 5 * 86_400
    ));
}
