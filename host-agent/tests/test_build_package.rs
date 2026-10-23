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
    let claim_response = serde_json::json!({
        "status": "claimed",
        "claimed": true,
        "build": {
            "build_id": 4553,
            "repo": format!("file://{}/example-rust-project", upstream_repo.parent().unwrap().display()),
            "branch": "main",
            "commit_sha": commit_sha,
            "command": "cargo test --workspace --locked",
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
}
