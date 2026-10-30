use host_agent::build_package::{package_build_artifacts, parse_args, BuildPackageConfig};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[test]
fn test_parse_args_valid_and_invalid_boundaries() {
    let args = vec![
        "--build-id".to_string(),
        "#4553".to_string(),
        "--workspace".to_string(),
        "/tmp/ws".to_string(),
        "--log-file".to_string(),
        "/tmp/build.log".to_string(),
        "--out-dir".to_string(),
        "/tmp/out".to_string(),
        "--bucket".to_string(),
        "custom-rust-artifacts".to_string(),
        "--profile".to_string(),
        "release".to_string(),
        "--binary".to_string(),
        "my-server".to_string(),
    ];
    let parsed = parse_args(&args)
        .expect("parse valid args")
        .expect("some config");
    assert_eq!(parsed.build_id, 4553);
    assert_eq!(parsed.gcs_bucket, "custom-rust-artifacts");
    assert_eq!(parsed.profile.as_deref(), Some("release"));
    assert_eq!(parsed.binary_name.as_deref(), Some("my-server"));
    assert!(!parsed.upload);

    // Reject path traversal in bucket, profile, or binary
    let bad_bucket = vec![
        "--build-id".to_string(),
        "4553".to_string(),
        "--workspace".to_string(),
        "/tmp/ws".to_string(),
        "--out-dir".to_string(),
        "/tmp/out".to_string(),
        "--bucket".to_string(),
        "../evil".to_string(),
    ];
    assert!(parse_args(&bad_bucket).is_err());

    let zero_id = vec![
        "--build-id".to_string(),
        "0".to_string(),
        "--workspace".to_string(),
        "/tmp/ws".to_string(),
        "--out-dir".to_string(),
        "/tmp/out".to_string(),
    ];
    assert!(parse_args(&zero_id).is_err());
}

#[test]
fn test_package_three_artifacts_produces_log_binaries_and_target_archives() {
    let temp = TempDir::new().expect("tempdir");
    let ws = temp.path().join("workspace");
    let rel_dir = ws.join("target").join("release");
    fs::create_dir_all(&rel_dir).expect("create target/release");

    // Create a compiled executable binary and a non-executable .rlib/.d file
    let bin_path = rel_dir.join("example-rust-project");
    fs::write(&bin_path, b"#!/bin/sh\necho ok\n").expect("write binary");
    let mut perms = fs::metadata(&bin_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&bin_path, perms).expect("chmod +x");

    fs::write(rel_dir.join("libfoo.rlib"), b"rlib-bytes").expect("write rlib");
    fs::write(rel_dir.join("example-rust-project.d"), b"dep-info").expect("write .d");

    let log_file = temp.path().join("test-suite.log");
    fs::write(
        &log_file,
        b"running 42 tests\ntest result: ok. 42 passed; 0 failed\n",
    )
    .expect("write log");

    let out_dir = temp.path().join("staged");
    let cfg = BuildPackageConfig {
        build_id: 4553,
        workspace: ws,
        target_dir: None,
        log_file: Some(log_file),
        out_dir: out_dir.clone(),
        gcs_bucket: "example-rust-project-build-artifacts".to_string(),
        profile: Some("release".to_string()),
        binary_name: None,
        upload: false,
    };

    let manifest = package_build_artifacts(&cfg).expect("package 3 artifacts");
    assert_eq!(manifest.build_id, 4553);
    assert_eq!(
        manifest.log_url,
        "gs://example-rust-project-build-artifacts/builds/4553/build.log"
    );
    assert_eq!(
        manifest.binary_url,
        "gs://example-rust-project-build-artifacts/builds/4553/binaries.tar.zst"
    );
    assert_eq!(
        manifest.target_zip_url,
        "gs://example-rust-project-build-artifacts/builds/4553/target.tar.zst"
    );
    assert_eq!(manifest.binaries_included, vec!["example-rust-project"]);
    assert!(manifest.log_size_bytes > 0);
    assert!(manifest.binary_size_bytes > 0);
    assert!(manifest.target_zip_size_bytes > 0);
    assert_eq!(manifest.log_sha256.len(), 64);
    assert_eq!(manifest.binary_sha256.len(), 64);
    assert_eq!(manifest.target_zip_sha256.len(), 64);

    let staged_build_dir = out_dir.join("builds").join("4553");
    assert!(staged_build_dir.join("build.log").exists());
    assert!(staged_build_dir.join("binaries.tar.zst").exists());
    assert!(staged_build_dir.join("target.tar.zst").exists());
    assert!(staged_build_dir.join("manifest.json").exists());
}

#[test]
fn test_package_refuses_to_overwrite_symlink_in_target_dir_and_checkout_rejects_unpushed_sha() {
    use host_agent::build_package::{materialize_repository_checkout, ClaimedBuildJob};
    use std::process::Command;

    let temp = TempDir::new().expect("tempdir");
    let ws = temp.path().join("workspace");
    let target_dir = temp.path().join("targets").join("example-rust-project");
    fs::create_dir_all(&ws).expect("create ws");
    fs::create_dir_all(target_dir.join("release")).expect("create target/release");

    // 1. Place a sensitive sentinel file outside target_dir and symlink `target_dir/build.log -> sentinel`
    let sentinel = temp.path().join("host-sentinel.conf");
    fs::write(&sentinel, b"ORIGINAL_ROOT_CONFIG=1\n").expect("write sentinel");
    std::os::unix::fs::symlink(&sentinel, target_dir.join("build.log")).expect("create symlink");

    let log_file = temp.path().join("raw.log");
    fs::write(&log_file, b"MALICIOUS_BUILD_LOG_OUTPUT\n").expect("write raw log");

    let out_dir = temp.path().join("staged");
    let cfg = BuildPackageConfig {
        build_id: 9001,
        workspace: ws.clone(),
        target_dir: Some(target_dir),
        log_file: Some(log_file),
        out_dir,
        gcs_bucket: "example-rust-project-build-artifacts".to_string(),
        profile: Some("release".to_string()),
        binary_name: None,
        upload: false,
    };
    let _ =
        package_build_artifacts(&cfg).expect("package artifacts without touching symlink referent");
    let sentinel_after = fs::read_to_string(&sentinel).expect("read sentinel");
    assert_eq!(
        sentinel_after, "ORIGINAL_ROOT_CONFIG=1\n",
        "Packaging must never follow or overwrite a symlink at target/build.log"
    );

    // 2. Verify materialize_repository_checkout fails when requested commit_sha does not exist in remote
    let upstream = temp.path().join("upstream-git");
    fs::create_dir_all(&upstream).expect("create upstream");
    assert!(Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(&upstream)
        .status()
        .unwrap()
        .success());
    fs::write(upstream.join("README.md"), b"hello\n").expect("write README");
    assert!(Command::new("git")
        .arg("-C")
        .arg(&upstream)
        .args(["add", "README.md"])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&upstream)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "init",
        ])
        .status()
        .unwrap()
        .success());

    let unpushed_job = ClaimedBuildJob {
        build_id: 9002,
        repo: format!("file://{}", upstream.display()),
        branch: "main".to_string(),
        commit_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
        command: "cargo test".to_string(),
        profile: None,
        binary_name: None,
        gcs_bucket: "example-rust-project-build-artifacts".to_string(),
    };
    let checkout_ws = temp.path().join("checkout-ws");
    fs::create_dir_all(&checkout_ws).expect("create checkout-ws");
    let err = materialize_repository_checkout(&checkout_ws, &unpushed_job)
        .expect_err("unpushed commit_sha must fail instead of falling back to branch tip");
    assert!(
        err.contains("deadbeef"),
        "Expected checkout error mentioning unpushed SHA, got: {err}"
    );
}

#[test]
fn test_queue_worker_claims_executes_packages_and_completes_queued_build() {
    use host_agent::build_package::{
        parse_worker_args, process_next_queued_build, sanitize_project_slug,
    };
    use std::process::Command;

    assert_eq!(
        sanitize_project_slug("example-org/example-rust-project").unwrap(),
        "example-rust-project"
    );
    assert!(sanitize_project_slug("../evil").is_err());

    let temp = TempDir::new().expect("tempdir");
    let remote_build_root = temp.path().join("remote-build");
    let control_dir = temp.path().join("control");
    let sources_root = remote_build_root.join("sources");
    let targets_root = remote_build_root.join("targets");
    let out_dir = remote_build_root.join("artifacts");
    fs::create_dir_all(&control_dir).expect("create control dir");

    // 1. Create a real local Git repository with a committed Cargo.toml so materialize_repository_checkout clones & checks out the exact SHA
    let upstream_repo = temp.path().join("upstream-repo");
    fs::create_dir_all(&upstream_repo).expect("create upstream repo");
    assert!(Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(&upstream_repo)
        .status()
        .unwrap()
        .success());
    fs::write(
        upstream_repo.join("Cargo.toml"),
        "[package]\nname = \"example-rust-project\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    assert!(Command::new("git")
        .arg("-C")
        .arg(&upstream_repo)
        .args(["add", "Cargo.toml"])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&upstream_repo)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "initial",
        ])
        .status()
        .unwrap()
        .success());
    let sha_out = Command::new("git")
        .arg("-C")
        .arg(&upstream_repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse HEAD");
    let commit_sha = String::from_utf8(sha_out.stdout)
        .unwrap()
        .trim()
        .to_string();

    // 2. Create an executable mock `host-launcher` that enforces ALLOWED_COMMANDS (`cargo`, not `bash`)
    // and writes compiled outputs into `<remote_build_root>/targets/<project>/release/`
    let mock_launcher = temp.path().join("mock-host-launcher");
    let launcher_script = format!(
        "#!/bin/sh\nset -eu\nPROJECT=\"$1\"\nSHIFT_CMD=\"$2\"\nif [ \"$SHIFT_CMD\" = \"prepare\" ]; then\n  mkdir -p \"{sources}/$PROJECT\" \"{targets}/$PROJECT\"\n  exit 0\nfi\nif [ \"$SHIFT_CMD\" != \"cargo\" ] && [ \"$SHIFT_CMD\" != \"just\" ]; then\n  echo \"UnsupportedCommand: $SHIFT_CMD\" >&2\n  exit 1\nfi\ntest -f \"{sources}/$PROJECT/Cargo.toml\"\nmkdir -p \"{targets}/$PROJECT/release\"\nprintf '#!/bin/sh\\necho ok\\n' > \"{targets}/$PROJECT/release/example-rust-project\"\nchmod 755 \"{targets}/$PROJECT/release/example-rust-project\"\necho 'test result: ok. 12 passed; 0 failed'\n",
        sources = sources_root.display(),
        targets = targets_root.display(),
    );
    fs::write(&mock_launcher, launcher_script).expect("write mock launcher");
    let mut perms = fs::metadata(&mock_launcher).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mock_launcher, perms).expect("chmod +x mock launcher");

    // 3. Seed a claimed build response in the mock control-plane directory
    let env_file = temp.path().join("queue-worker.env");
    fs::write(
        &env_file,
        "BUILDER_INSTANCE_NAME=builder-scale-1792000000-abcd1234\nSPOT_BEARER_TOKEN=initial-token-v1\n",
    )
    .expect("write env file");
    std::env::set_var("SPOT_QUEUE_WORKER_ENV_FILE", &env_file);
    let lock_dir = temp.path().join("locks");
    fs::create_dir_all(&lock_dir).expect("create lock dir");
    std::env::set_var("OXIDEX_LOCK_ROOT", &lock_dir);

    let claim_response = serde_json::json!({
        "status": "claimed",
        "claimed": true,
        "renewed_queue_token": "renewed-token-v2",
        "build": {
            "build_id": 4553,
            "repo": format!("file://{}/example-rust-project", upstream_repo.parent().unwrap().display()),
            "branch": "main",
            "commit_sha": commit_sha,
            "command": "cargo test --workspace --locked --release",
            "profile": "release",
            "binary_name": "example-rust-project",
            "gcs_bucket": "example-rust-project-build-artifacts"
        }
    });
    // Symlink `example-rust-project` to `upstream-repo` so the repo slug resolves to `example-rust-project`
    std::os::unix::fs::symlink(&upstream_repo, temp.path().join("example-rust-project"))
        .expect("symlink upstream repo");
    fs::write(
        control_dir.join("claim_response.json"),
        serde_json::to_string_pretty(&claim_response).unwrap(),
    )
    .expect("write claim_response.json");

    let worker_args = vec![
        "--dashboard-url".to_string(),
        format!("file://{}", control_dir.display()),
        "--instance-name".to_string(),
        "builder-scale-1792000000-abcd1234".to_string(),
        "--zone".to_string(),
        "us-central1-a".to_string(),
        "--sources-root".to_string(),
        sources_root.to_string_lossy().into_owned(),
        "--out-dir".to_string(),
        out_dir.to_string_lossy().into_owned(),
        "--launcher".to_string(),
        mock_launcher.to_string_lossy().into_owned(),
        "--no-upload".to_string(),
        "--once".to_string(),
    ];

    let cfg = parse_worker_args(&worker_args)
        .expect("parse worker args")
        .expect("worker config");
    assert!(cfg.once);
    assert!(!cfg.upload);

    let result = process_next_queued_build(&cfg).expect("process next queued build");
    assert!(result.claimed);
    assert_eq!(result.build_id, Some(4553));
    assert_eq!(result.status, "completed");
    assert_eq!(result.exit_code, Some(0));

    let manifest = result.manifest.expect("manifest");
    assert_eq!(manifest.binaries_included, vec!["example-rust-project"]);

    let complete_file = control_dir.join("complete_4553.json");
    assert!(
        complete_file.exists(),
        "Expected queue-worker to POST completion payload to complete_4553.json"
    );
    let updated_env = fs::read_to_string(&env_file).expect("read updated env file");
    assert!(
        updated_env.contains("SPOT_BEARER_TOKEN=renewed-token-v2"),
        "Expected queue-worker to persist renewed_queue_token into SPOT_QUEUE_WORKER_ENV_FILE, got: {updated_env}"
    );
    let complete_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&complete_file).unwrap()).unwrap();
    assert_eq!(complete_json["build_id"], 4553);
    assert_eq!(complete_json["status"], "completed");
    assert_eq!(complete_json["exit_code"], 0);
    assert_eq!(
        complete_json["instance_name"],
        "builder-scale-1792000000-abcd1234"
    );
    assert_eq!(
        complete_json["log_url"],
        "gs://example-rust-project-build-artifacts/builds/4553/build.log"
    );
    assert_eq!(
        complete_json["binary_url"],
        "gs://example-rust-project-build-artifacts/builds/4553/binaries.tar.zst"
    );
    assert_eq!(
        complete_json["target_zip_url"],
        "gs://example-rust-project-build-artifacts/builds/4553/target.tar.zst"
    );

    // Also verify that if the dedicated builder launcher does not exist on disk,
    // process_next_queued_build fails closed (exit_code 127) and never falls back to host `sh -c`
    let claim_missing_launcher = serde_json::json!({
        "status": "claimed",
        "claimed": true,
        "build": {
            "build_id": 4554,
            "repo": format!("file://{}/example-rust-project", upstream_repo.parent().unwrap().display()),
            "branch": "main",
            "commit_sha": commit_sha,
            "command": "cargo test --workspace",
            "profile": "debug",
            "binary_name": null,
            "gcs_bucket": "example-rust-project-build-artifacts"
        }
    });
    fs::write(
        control_dir.join("claim_response.json"),
        serde_json::to_string_pretty(&claim_missing_launcher).unwrap(),
    )
    .expect("write claim_response.json for 4554");

    let mut cfg_missing_launcher = cfg.clone();
    cfg_missing_launcher.launcher = temp.path().join("nonexistent-host-launcher");
    let missing_err = process_next_queued_build(&cfg_missing_launcher)
        .expect_err("missing dedicated launcher must fail closed");
    assert!(
        missing_err.contains("missing or unavailable"),
        "Expected fail-closed launcher error, got: {missing_err}"
    );
    let complete_4554: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(control_dir.join("complete_4554.json")).unwrap())
            .unwrap();
    assert_eq!(complete_4554["status"], "failed");
    assert_eq!(complete_4554["exit_code"], 127);

    // Also verify that if another build holds the active project lock (`oxidex-cache-active-<project>.lock`),
    // `process_next_queued_build` fails closed with exit_code 75 BEFORE modifying the shared workspace
    let held_lock =
        host_agent::docker_admission::acquire_build_locks("example-rust-project", &lock_dir)
            .expect("acquire active project lock");
    let claim_locked = serde_json::json!({
        "status": "claimed",
        "claimed": true,
        "build": {
            "build_id": 4555,
            "repo": format!("file://{}/example-rust-project", upstream_repo.parent().unwrap().display()),
            "branch": "main",
            "commit_sha": commit_sha,
            "command": "cargo test --workspace",
            "profile": "debug",
            "binary_name": null,
            "gcs_bucket": "example-rust-project-build-artifacts"
        }
    });
    fs::write(
        control_dir.join("claim_response.json"),
        serde_json::to_string_pretty(&claim_locked).unwrap(),
    )
    .expect("write claim_response.json for 4555");
    let locked_err = process_next_queued_build(&cfg)
        .expect_err("contended project lock must prevent workspace checkout");
    assert!(
        locked_err.contains("build lock or draining sentinel is active"),
        "Expected active lock error, got: {locked_err}"
    );
    let complete_4555: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(control_dir.join("complete_4555.json")).unwrap())
            .unwrap();
    assert_eq!(complete_4555["status"], "failed");
    assert_eq!(complete_4555["exit_code"], 75);
    drop(held_lock);
}

#[test]
fn test_debug_build_after_cached_release_build_packages_debug_binary_not_stale_release() {
    use host_agent::build_package::resolve_cargo_target_profile;

    assert_eq!(
        resolve_cargo_target_profile(Some("test"), Some("cargo build")),
        "debug"
    );
    assert_eq!(
        resolve_cargo_target_profile(None, Some("cargo build --release")),
        "release"
    );
    assert_eq!(
        resolve_cargo_target_profile(Some("test"), Some("cargo test --workspace")),
        "debug"
    );

    let temp = TempDir::new().expect("tempdir");
    let ws = temp.path().join("workspace");
    let rel_dir = ws.join("target").join("release");
    let dbg_dir = ws.join("target").join("debug");
    fs::create_dir_all(&rel_dir).expect("create target/release");
    fs::create_dir_all(&dbg_dir).expect("create target/debug");

    // 1. Seed a stale binary in `target/release/app` from an earlier release build (commit A)
    let rel_bin = rel_dir.join("app");
    fs::write(&rel_bin, b"#!/bin/sh\necho STALE_COMMIT_A_RELEASE\n").expect("write release bin");
    let mut perms = fs::metadata(&rel_bin).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&rel_bin, perms).expect("chmod +x release");

    // 2. Write a freshly compiled debug binary in `target/debug/app` from commit B (`cargo build`)
    let dbg_bin = dbg_dir.join("app");
    fs::write(
        &dbg_bin,
        b"#!/bin/sh\necho FRESH_COMMIT_B_DEBUG_BINARY_PAYLOAD\n",
    )
    .expect("write debug bin");
    let mut perms_dbg = fs::metadata(&dbg_bin).unwrap().permissions();
    perms_dbg.set_mode(0o755);
    fs::set_permissions(&dbg_bin, perms_dbg).expect("chmod +x debug");

    // Package with profile resolved from `"test"` + `"cargo build"` -> `"debug"`
    let out_dir = temp.path().join("staged");
    let effective_profile = resolve_cargo_target_profile(Some("test"), Some("cargo build"));
    let cfg = BuildPackageConfig {
        build_id: 4554,
        workspace: ws.clone(),
        target_dir: None,
        log_file: None,
        out_dir: out_dir.clone(),
        gcs_bucket: "example-rust-project-build-artifacts".to_string(),
        profile: Some(effective_profile),
        binary_name: Some("app".to_string()),
        upload: false,
    };
    let manifest_dbg = package_build_artifacts(&cfg).expect("package debug build");
    assert_eq!(manifest_dbg.binaries_included, vec!["app"]);

    // Verify that packaging with `profile: Some("release")` produces a DIFFERENT binary archive SHA-256
    let cfg_rel = BuildPackageConfig {
        build_id: 4555,
        workspace: ws.clone(),
        target_dir: None,
        log_file: None,
        out_dir,
        gcs_bucket: "example-rust-project-build-artifacts".to_string(),
        profile: Some("release".to_string()),
        binary_name: Some("app".to_string()),
        upload: false,
    };
    let manifest_rel = package_build_artifacts(&cfg_rel).expect("package release build");
    assert_ne!(
        manifest_dbg.binary_sha256, manifest_rel.binary_sha256,
        "Debug build must package target/debug/app rather than the stale target/release/app"
    );

    // 3. Verify that an absolute/traversal profile (`profile: Some("/etc")` + `binary_name: Some("shadow")`)
    // or a symlinked profile directory (`target/evil -> /etc`) is rejected / ignored by descriptor-safe packaging
    let cfg_abs_profile = BuildPackageConfig {
        build_id: 4556,
        workspace: ws.clone(),
        target_dir: None,
        log_file: None,
        out_dir: temp.path().join("staged-abs"),
        gcs_bucket: "example-rust-project-build-artifacts".to_string(),
        profile: Some("/etc".to_string()),
        binary_name: Some("shadow".to_string()),
        upload: false,
    };
    assert!(
        package_build_artifacts(&cfg_abs_profile).is_err(),
        "Absolute profile path must be rejected by package_build_artifacts"
    );

    let host_secret_dir = temp.path().join("host-secrets");
    fs::create_dir_all(&host_secret_dir).expect("create host-secrets");
    let secret_file = host_secret_dir.join("secret-bin");
    fs::write(&secret_file, b"#!/bin/sh\necho TOP_SECRET\n").expect("write secret");
    let mut secret_perms = fs::metadata(&secret_file).unwrap().permissions();
    secret_perms.set_mode(0o755);
    fs::set_permissions(&secret_file, secret_perms).expect("chmod +x secret");

    std::os::unix::fs::symlink(&host_secret_dir, ws.join("target").join("customprof"))
        .expect("symlink profile dir to outside directory");
    let cfg_symlink_prof = BuildPackageConfig {
        build_id: 4557,
        workspace: ws,
        target_dir: None,
        log_file: None,
        out_dir: temp.path().join("staged-sym"),
        gcs_bucket: "example-rust-project-build-artifacts".to_string(),
        profile: Some("customprof".to_string()),
        binary_name: Some("secret-bin".to_string()),
        upload: false,
    };
    let sym_manifest =
        package_build_artifacts(&cfg_symlink_prof).expect("package with symlinked profile");
    assert!(
        sym_manifest.binaries_included.is_empty(),
        "Symlinked profile directory must not be traversed by open_dir_nofollow, got: {:?}",
        sym_manifest.binaries_included
    );
}

#[test]
fn test_materialize_checkout_refuses_option_like_git_arguments() {
    use host_agent::build_package::{materialize_repository_checkout, ClaimedBuildJob};

    let temp = TempDir::new().expect("tempdir");
    let marker = temp.path().join("pwned");
    let job = |repo: &str, branch: &str, sha: &str| ClaimedBuildJob {
        build_id: 9100,
        repo: repo.to_string(),
        branch: branch.to_string(),
        commit_sha: sha.to_string(),
        command: "cargo test".to_string(),
        profile: None,
        binary_name: None,
        gcs_bucket: "example-rust-project-build-artifacts".to_string(),
    };
    let upload_pack = format!("--upload-pack=touch {}", marker.display());
    for bad in [
        job("example-org/example-rust-project", &upload_pack, "deadbeef"),
        job("--upload-pack=sh", "main", "deadbeef"),
        job("example-org/example-rust-project", "main", "--orphan"),
        job("example-org/example-rust-project", "main", "not-hex!"),
    ] {
        let ws = temp
            .path()
            .join(format!("ws-{}", bad.repo.len() + bad.branch.len()));
        fs::create_dir_all(&ws).expect("create ws");
        let err = materialize_repository_checkout(&ws, &bad)
            .expect_err("option-like git arguments must be refused");
        assert!(err.contains("Refusing claimed build job"), "got: {err}");
        assert!(
            !ws.join(".git").exists(),
            "git must not run for a refused job"
        );
    }
    assert!(!marker.exists());
}

#[test]
fn test_materialize_checkout_succeeds_for_a_pushed_commit() {
    use host_agent::build_package::{materialize_repository_checkout, ClaimedBuildJob};
    use std::process::Command;

    let temp = TempDir::new().expect("tempdir");
    let upstream = temp.path().join("upstream");
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&upstream)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    fs::create_dir_all(&upstream).expect("create upstream");
    git(&["init", "--quiet", "--initial-branch=main"]);
    fs::write(upstream.join("README.md"), b"hello\n").expect("write README");
    git(&["add", "README.md"]);
    git(&["commit", "--quiet", "-m", "init"]);
    let sha = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&upstream)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let job = ClaimedBuildJob {
        build_id: 9101,
        repo: format!("file://{}", upstream.display()),
        branch: "main".to_string(),
        commit_sha: sha,
        command: "cargo test".to_string(),
        profile: None,
        binary_name: None,
        gcs_bucket: "example-rust-project-build-artifacts".to_string(),
    };
    let ws = temp.path().join("ws");
    fs::create_dir_all(&ws).expect("create ws");
    materialize_repository_checkout(&ws, &job).expect("a pushed commit checks out");
    assert_eq!(fs::read(ws.join("README.md")).unwrap(), b"hello\n");
}
