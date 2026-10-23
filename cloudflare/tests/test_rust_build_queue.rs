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
