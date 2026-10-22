use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use host_agent::builder_entrypoint::{
    setup_builder_env, verify_compiler_commit_hash, verify_toolchain_pin_content, EntrypointError,
};
use host_agent::platform::{AggregateCounters, MockSystemInspector, SystemInspector};
use host_agent::toolchain_probe::{
    run_with_sys_and_config, validate_website_tools, verify_tool_versions, ProbeConfig, ProbeError,
    WebsiteToolsEvidence, REQUIRED_COMMIT, WEBSITE_PATH,
};
use host_agent::worktree_commands::{
    run_worktree_jobs, validate_worktree_id, WorktreeError, WorktreeJob,
};

/// Helper to configure a valid mock environment with authentic tool outputs.
fn setup_valid_mock_environment(
    sys: &MockSystemInspector,
    config: &ProbeConfig,
    zigbuild_binary_path: &Path,
) {
    // 1. Valid compiler -vV output
    let compiler_dir = config.compiler_path.parent().unwrap();
    sys.set_command_result(
        &format!("{} -vV", config.compiler_path.to_string_lossy()),
        Ok(format!(
            "rustc 1.99.0 (58cf8f9c3 2026-10-13)\nbinary: rustc\ncommit-hash: {REQUIRED_COMMIT}\nrelease: 1.99.0"
        )),
    );

    // 2. Valid tool version commands
    sys.set_command_result(
        &format!("{} -V", compiler_dir.join("cargo").to_string_lossy()),
        Ok("cargo 1.99.0 (5f94df478 2026-08-27)".to_string()),
    );
    sys.set_command_result(
        &format!(
            "{} --version",
            compiler_dir.join("rustfmt").to_string_lossy()
        ),
        Ok("rustfmt 1.10.0".to_string()),
    );
    sys.set_command_result(
        &format!(
            "{} --version",
            compiler_dir.join("clippy-driver").to_string_lossy()
        ),
        Ok("clippy 0.1.99 (58cf8f9c3 2026-10-13)".to_string()),
    );
    sys.set_command_result(
        &format!("{} version", config.zig_path.to_string_lossy()),
        Ok("0.15.2".to_string()),
    );
    sys.set_command_result(
        &format!("{} --version", config.cargo_zigbuild_path.to_string_lossy()),
        Ok("cargo-zigbuild 0.23.4".to_string()),
    );
    sys.set_command_result(
        &format!("{} --version", config.cbindgen_path.to_string_lossy()),
        Ok("cbindgen 0.28.0".to_string()),
    );
    sys.set_command_result(
        &format!("{} --version", config.just_path.to_string_lossy()),
        Ok("just 1.40.0".to_string()),
    );
    sys.set_command_result(
        &format!("{} --version", config.rsync_path.to_string_lossy()),
        Ok("rsync version 3.3.0".to_string()),
    );
    sys.set_command_result(
        &format!("{} --version", config.node_path.to_string_lossy()),
        Ok("v24.21.0".to_string()),
    );
    sys.set_command_result(
        &format!("{} --version", config.npm_path.to_string_lossy()),
        Ok("11.3.0".to_string()),
    );

    // 3. Valid Node website probe output
    let valid_evidence_json = serde_json::json!({
        "node": "v24.21.0",
        "playwright": "1.63.0",
        "chromium_revision": "1243",
        "chromium_version": "153.0.8010.12",
        "browser_path": "/opt/playwright-browsers",
        "uid": 1001,
        "runtime_path": config.website_path,
        "executable_readable": true,
        "root_owned_runtime": true,
        "libraries_available": true
    });
    sys.set_command_result("node", Ok(valid_evidence_json.to_string()));

    // 4. Valid cgroups (28 GiB, 0 swap, 16 vCPU)
    *sys.cgroup_counters_result.write().unwrap() = Some(AggregateCounters {
        path: "/sys/fs/cgroup/builder".to_string(),
        inode: 12345,
        device: 67890,
        monotonic_ns: 1000000,
        memory_current: "1073741824".to_string(),
        memory_peak: "2147483648".to_string(),
        memory_max: (28u64 * 1024 * 1024 * 1024).to_string(),
        memory_events: "oom 0".to_string(),
        cpu_stat: "usage_usec 5000".to_string(),
        cpu_max: "1600000 100000".to_string(), // 16 * 100000
    });

    // 5. Authentic binary content for hashing
    fs::write(
        zigbuild_binary_path,
        b"genuine-cargo-zigbuild-binary-payload",
    )
    .unwrap();

    // 6. Valid system identity
    sys.set_boot_id("4a7d32c1-8b9e-4f0a-9d2c-1e3f5a7b9c0d");
    sys.set_hostname("oxidex-dedicated-builder-01");
}

#[test]
fn test_toolchain_pin_verification() {
    let toml = r#"
[toolchain]
channel = "1.99.0"
components = ["rustfmt", "clippy"]
"#;
    assert_eq!(verify_toolchain_pin_content(toml).unwrap(), "1.99.0");

    let wrong_pin = r#"
[toolchain]
channel = "1.98.0"
"#;
    assert!(matches!(
        verify_toolchain_pin_content(wrong_pin).unwrap_err(),
        EntrypointError::WrongPin(_)
    ));

    let missing = r#"
[toolchain]
profile = "minimal"
"#;
    assert!(matches!(
        verify_toolchain_pin_content(missing).unwrap_err(),
        EntrypointError::MissingPin
    ));
}

#[test]
fn test_compiler_commit_hash() {
    let valid_rustc = format!(
        "rustc 1.99.0 (58cf8f9c3 2026-10-13)\nbinary: rustc\ncommit-hash: {REQUIRED_COMMIT}\ncommit-date: 2026-10-13\nhost: x86_64-unknown-linux-gnu\nrelease: 1.99.0"
    );
    assert!(verify_compiler_commit_hash(&valid_rustc).is_ok());

    let wrong_commit = "rustc 1.99.0\ncommit-hash: 0000000000000000000000000000000000000000\n";
    assert!(matches!(
        verify_compiler_commit_hash(wrong_commit).unwrap_err(),
        EntrypointError::CommitMismatch
    ));
}

#[test]
fn test_builder_env_setup() {
    let compiler = Path::new("/opt/rust/bin/rustc");
    let cargo = Path::new("/opt/rust/bin/cargo");
    let mut base = HashMap::new();
    base.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
    base.insert("SDKROOT".to_string(), "/bad/sdk".to_string());

    let env = setup_builder_env(compiler, cargo, &base);

    assert_eq!(env.get("RUSTC"), Some(&"/opt/rust/bin/rustc".to_string()));
    assert_eq!(
        env.get("RUSTDOC"),
        Some(&"/opt/rust/bin/rustdoc".to_string())
    );
    assert_eq!(env.get("RUSTUP_TOOLCHAIN"), Some(&"1.99.0".to_string()));
    assert_eq!(
        env.get("CARGO_ZIGBUILD_ZIG_PATH"),
        Some(&"/opt/zig/zig".to_string())
    );
    assert_eq!(
        env.get("CARGO_ZIGBUILD_CACHE_DIR"),
        Some(&"/cargo/zigbuild-cache".to_string())
    );
    assert_eq!(
        env.get("ZIG_GLOBAL_CACHE_DIR"),
        Some(&"/cargo/zig-global-cache".to_string())
    );
    assert_eq!(
        env.get("ZIG_LOCAL_CACHE_DIR"),
        Some(&"/target/.zig-cache".to_string())
    );
    assert_eq!(
        env.get("OXIDEX_DARWIN_FRAMEWORK_DIR"),
        Some(&"/opt/opensourceDarwinStubs".to_string())
    );
    assert!(env
        .get("PATH")
        .unwrap()
        .starts_with("/opt/rust/bin:/usr/bin:/bin"));
    assert!(!env.contains_key("SDKROOT"));
}

#[test]
fn test_toolchain_probe_tool_versions() {
    let mut tools = HashMap::new();
    tools.insert("cargo".to_string(), "cargo 1.99.0 (5f94df478)".to_string());
    tools.insert("rustfmt".to_string(), "rustfmt 1.10.0".to_string());
    tools.insert(
        "clippy".to_string(),
        "clippy 0.1.99 (58cf8f9c3)".to_string(),
    );
    tools.insert("zig".to_string(), "0.15.2".to_string());
    tools.insert(
        "cargo_zigbuild".to_string(),
        "cargo-zigbuild 0.23.4".to_string(),
    );

    assert!(verify_tool_versions(&tools).is_ok());

    let mut bad_tools = tools.clone();
    bad_tools.insert("zig".to_string(), "0.14.0".to_string());
    assert!(matches!(
        verify_tool_versions(&bad_tools).unwrap_err(),
        ProbeError::ToolMismatch(_)
    ));
}

#[test]
fn test_toolchain_probe_genuine_full_run() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let zigbuild_bin = root.join("cargo-zigbuild");
    let config = ProbeConfig {
        compiler_path: root.join("bin/rustc"),
        zig_path: root.join("zig"),
        cargo_zigbuild_path: zigbuild_bin.clone(),
        cbindgen_path: root.join("cbindgen"),
        just_path: root.join("just"),
        rsync_path: root.join("rsync"),
        node_path: root.join("node"),
        npm_path: root.join("npm"),
        website_path: WEBSITE_PATH.to_string(),
        playwright_browsers_path: PathBuf::from("/opt/playwright-browsers"),
    };
    fs::create_dir_all(config.compiler_path.parent().unwrap()).unwrap();

    let sys = MockSystemInspector::new();
    setup_valid_mock_environment(&sys, &config, &zigbuild_bin);

    let res = run_with_sys_and_config(&[], &sys, &config);
    assert!(res.is_ok(), "Expected probe run to succeed: {:?}", res);
    assert_eq!(res.unwrap(), 0);
}

#[test]
fn test_toolchain_probe_rejects_compiler_commit_mismatch() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let zigbuild_bin = root.join("cargo-zigbuild");

    let config = ProbeConfig {
        compiler_path: root.join("bin/rustc"),
        cargo_zigbuild_path: zigbuild_bin.clone(),
        ..ProbeConfig::default()
    };
    fs::create_dir_all(config.compiler_path.parent().unwrap()).unwrap();

    let sys = MockSystemInspector::new();
    setup_valid_mock_environment(&sys, &config, &zigbuild_bin);

    // Tamper with compiler commit hash
    sys.set_command_result(
        &format!("{} -vV", config.compiler_path.to_string_lossy()),
        Ok(
            "rustc 1.99.0\ncommit-hash: 0000000000000000000000000000000000000000\nrelease: 1.99.0"
                .to_string(),
        ),
    );

    let err = run_with_sys_and_config(&[], &sys, &config).unwrap_err();
    let probe_err = err.downcast_ref::<ProbeError>().unwrap();
    assert!(
        matches!(probe_err, ProbeError::CompilerMismatch(_)),
        "Expected CompilerMismatch, got {:?}",
        probe_err
    );
}

#[test]
fn test_toolchain_probe_rejects_tool_version_mismatches() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let zigbuild_bin = root.join("cargo-zigbuild");

    let config = ProbeConfig {
        compiler_path: root.join("bin/rustc"),
        cargo_zigbuild_path: zigbuild_bin.clone(),
        ..ProbeConfig::default()
    };
    fs::create_dir_all(config.compiler_path.parent().unwrap()).unwrap();

    // Matrix of invalid tool versions that must be rejected
    let mismatch_cases = [
        ("zig", "0.14.0"),
        ("cargo_zigbuild", "cargo-zigbuild 0.22.0"),
        ("cargo", "cargo 1.98.0 (outdated)"),
        ("rustfmt", "rustfmt 1.8.0"),
        ("clippy", "clippy 0.1.98"),
    ];

    for (tool_key, bad_version) in mismatch_cases {
        let sys = MockSystemInspector::new();
        setup_valid_mock_environment(&sys, &config, &zigbuild_bin);

        // Inject the bad version
        let compiler_dir = config.compiler_path.parent().unwrap();
        let cmd_key = match tool_key {
            "zig" => format!("{} version", config.zig_path.to_string_lossy()),
            "cargo_zigbuild" => {
                format!("{} --version", config.cargo_zigbuild_path.to_string_lossy())
            }
            "cargo" => format!("{} -V", compiler_dir.join("cargo").to_string_lossy()),
            "rustfmt" => format!(
                "{} --version",
                compiler_dir.join("rustfmt").to_string_lossy()
            ),
            "clippy" => format!(
                "{} --version",
                compiler_dir.join("clippy-driver").to_string_lossy()
            ),
            _ => unreachable!(),
        };
        sys.set_command_result(&cmd_key, Ok(bad_version.to_string()));

        let err = run_with_sys_and_config(&[], &sys, &config).unwrap_err();
        let probe_err = err.downcast_ref::<ProbeError>().unwrap();
        assert!(
            matches!(probe_err, ProbeError::ToolMismatch(_)),
            "Case {tool_key}={bad_version} expected ToolMismatch, got {:?}",
            probe_err
        );
    }
}

#[test]
fn test_toolchain_probe_detects_tool_execution_failure() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let zigbuild_bin = root.join("cargo-zigbuild");

    let config = ProbeConfig {
        compiler_path: root.join("bin/rustc"),
        cargo_zigbuild_path: zigbuild_bin.clone(),
        ..ProbeConfig::default()
    };
    fs::create_dir_all(config.compiler_path.parent().unwrap()).unwrap();

    let sys = MockSystemInspector::new();
    setup_valid_mock_environment(&sys, &config, &zigbuild_bin);

    // Fail zig execution
    sys.set_command_result(
        &format!("{} version", config.zig_path.to_string_lossy()),
        Err("Executable /opt/zig/zig not found (os error 2)".to_string()),
    );

    let err = run_with_sys_and_config(&[], &sys, &config).unwrap_err();
    let probe_err = err.downcast_ref::<ProbeError>().unwrap();
    assert!(
        matches!(probe_err, ProbeError::ToolExecution { tool, .. } if tool == "zig"),
        "Expected ToolExecution for zig, got {:?}",
        probe_err
    );
}

#[test]
fn test_toolchain_probe_verifies_genuine_file_hash() {
    let temp = TempDir::new().unwrap();
    let file_path = temp.path().join("sample_binary");
    fs::write(&file_path, b"test-binary-sha256-verification-bytes").unwrap();

    let sys = MockSystemInspector::new();
    let hash = sys.hash_file(&file_path).unwrap();

    // Compute expected SHA-256 via pure rust sha2
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"test-binary-sha256-verification-bytes");
    let expected = hex::encode(hasher.finalize());

    assert_eq!(hash, expected);
    // Ensure it is NOT the empty-string sha256 (e3b0c442...)
    assert_ne!(
        hash,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn test_toolchain_probe_rejects_invalid_cgroup_limits() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let zigbuild_bin = root.join("cargo-zigbuild");

    let config = ProbeConfig {
        compiler_path: root.join("bin/rustc"),
        cargo_zigbuild_path: zigbuild_bin.clone(),
        ..ProbeConfig::default()
    };
    fs::create_dir_all(config.compiler_path.parent().unwrap()).unwrap();

    // 1. Memory limit wrong (16 GiB instead of 28 GiB)
    let sys = MockSystemInspector::new();
    setup_valid_mock_environment(&sys, &config, &zigbuild_bin);
    *sys.cgroup_counters_result.write().unwrap() = Some(AggregateCounters {
        memory_max: (16u64 * 1024 * 1024 * 1024).to_string(),
        cpu_max: "1600000 100000".to_string(),
        ..Default::default()
    });
    let err = run_with_sys_and_config(&[], &sys, &config).unwrap_err();
    assert!(matches!(
        err.downcast_ref::<ProbeError>().unwrap(),
        ProbeError::CgroupLimitsMismatch(_)
    ));

    // 2. CPU quota ratio wrong (8 vCPU instead of 16 vCPU)
    let sys = MockSystemInspector::new();
    setup_valid_mock_environment(&sys, &config, &zigbuild_bin);
    *sys.cgroup_counters_result.write().unwrap() = Some(AggregateCounters {
        memory_max: (28u64 * 1024 * 1024 * 1024).to_string(),
        cpu_max: "800000 100000".to_string(), // 8 cores instead of 16
        ..Default::default()
    });
    let err = run_with_sys_and_config(&[], &sys, &config).unwrap_err();
    assert!(matches!(
        err.downcast_ref::<ProbeError>().unwrap(),
        ProbeError::CgroupLimitsMismatch(_)
    ));
}

#[test]
fn test_toolchain_probe_requires_genuine_boot_id_and_hostname() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let zigbuild_bin = root.join("cargo-zigbuild");

    let config = ProbeConfig {
        compiler_path: root.join("bin/rustc"),
        cargo_zigbuild_path: zigbuild_bin.clone(),
        ..ProbeConfig::default()
    };
    fs::create_dir_all(config.compiler_path.parent().unwrap()).unwrap();

    // Omit boot_id from mock
    let sys = MockSystemInspector::new();
    setup_valid_mock_environment(&sys, &config, &zigbuild_bin);
    *sys.boot_id.write().unwrap() = None;

    let err = run_with_sys_and_config(&[], &sys, &config).unwrap_err();
    let probe_err = err.downcast_ref::<ProbeError>().unwrap();
    assert!(
        matches!(probe_err, ProbeError::AttestationFailure { field, .. } if field == "boot_id"),
        "Expected AttestationFailure for boot_id, got {:?}",
        probe_err
    );
}

#[test]
fn test_website_tools_adversarial_validation() {
    let valid = WebsiteToolsEvidence {
        node: "v24.21.0".to_string(),
        playwright: "1.63.0".to_string(),
        chromium_revision: "1243".to_string(),
        chromium_version: "153.0.8010.12".to_string(),
        browser_path: "/opt/playwright-browsers".to_string(),
        uid: 1001,
        runtime_path: WEBSITE_PATH.to_string(),
        executable_readable: true,
        root_owned_runtime: true,
        libraries_available: true,
    };
    assert!(validate_website_tools(&valid, WEBSITE_PATH).is_ok());

    // 1. Wrong UID (e.g. root 0 instead of builder 1001)
    let mut bad_uid = valid.clone();
    bad_uid.uid = 0;
    assert!(validate_website_tools(&bad_uid, WEBSITE_PATH).is_err());

    // 2. Missing shared libraries (ldd reported 'not found')
    let mut bad_libs = valid.clone();
    bad_libs.libraries_available = false;
    assert!(validate_website_tools(&bad_libs, WEBSITE_PATH).is_err());

    // 3. Executable not readable
    let mut bad_exec = valid.clone();
    bad_exec.executable_readable = false;
    assert!(validate_website_tools(&bad_exec, WEBSITE_PATH).is_err());

    // 4. Runtime path modified
    let mut bad_path = valid.clone();
    bad_path.runtime_path = "/usr/local/bin:/usr/bin".to_string();
    assert!(validate_website_tools(&bad_path, WEBSITE_PATH).is_err());
}

#[test]
fn test_worktree_validation_and_execution() {
    assert!(validate_worktree_id("worktree_1").is_ok());
    assert!(validate_worktree_id("bench-2").is_ok());
    assert!(matches!(
        validate_worktree_id("../bad").unwrap_err(),
        WorktreeError::InvalidIdentity(_)
    ));

    let temp = TempDir::new().unwrap();
    let temp_canon = temp.path().canonicalize().unwrap();
    let src_root = temp_canon.join("src");
    let target_root = temp_canon.join("target");

    let wt1 = src_root.join("wt1");
    let wt2 = src_root.join("wt2");
    fs::create_dir_all(&wt1).unwrap();
    fs::create_dir_all(&wt2).unwrap();

    let jobs = vec![
        WorktreeJob {
            worktree_id: "wt1".to_string(),
            argv: vec!["cargo".to_string(), "--version".to_string()],
        },
        WorktreeJob {
            worktree_id: "wt2".to_string(),
            argv: vec!["cargo".to_string(), "--version".to_string()],
        },
    ];

    let sys = MockSystemInspector::new();
    let res = run_worktree_jobs(&jobs, &sys, Some(&src_root), Some(&target_root)).unwrap();

    assert_eq!(res.schema, "aggregate-worktree-commands-v1");
    assert_eq!(res.children.len(), 2);
    assert_eq!(res.exit_codes.len(), 2);
    assert_eq!(res.exit_codes[0], 0);
    assert_eq!(res.exit_codes[1], 0);
    assert!(res.complete);
}

#[test]
fn test_worktree_target_override_forbidden() {
    let temp = TempDir::new().unwrap();
    let temp_canon = temp.path().canonicalize().unwrap();
    let src_root = temp_canon.join("src");
    let target_root = temp_canon.join("target");
    let wt1 = src_root.join("wt1");
    let wt2 = src_root.join("wt2");
    fs::create_dir_all(&wt1).unwrap();
    fs::create_dir_all(&wt2).unwrap();

    let jobs = vec![
        WorktreeJob {
            worktree_id: "wt1".to_string(),
            argv: vec![
                "cargo".to_string(),
                "build".to_string(),
                "--target-dir=/tmp/bad".to_string(),
            ],
        },
        WorktreeJob {
            worktree_id: "wt2".to_string(),
            argv: vec!["cargo".to_string(), "build".to_string()],
        },
    ];

    let sys = MockSystemInspector::new();
    let err = run_worktree_jobs(&jobs, &sys, Some(&src_root), Some(&target_root)).unwrap_err();
    assert!(matches!(err, WorktreeError::TargetOverrideForbidden));
}
