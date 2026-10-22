use std::fs;
use std::os::unix::fs::symlink;
use std::sync::Mutex;
use tempfile::TempDir;

use host_agent::host_launcher::{
    checked_path, cleanup_source, container_command, is_command_allowed, resolve_uploader_uid,
    validate_project, HostLauncherError,
};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn test_validate_project() {
    assert!(validate_project("valid-project_123").is_ok());
    assert!(validate_project("A").is_ok());
    assert!(validate_project("rust_build-99").is_ok());

    assert!(matches!(
        validate_project("../bad").unwrap_err(),
        HostLauncherError::InvalidProject(_)
    ));
    assert!(matches!(
        validate_project("/etc/passwd").unwrap_err(),
        HostLauncherError::InvalidProject(_)
    ));
    assert!(matches!(
        validate_project("-bad-start").unwrap_err(),
        HostLauncherError::InvalidProject(_)
    ));
    assert!(matches!(
        validate_project("").unwrap_err(),
        HostLauncherError::InvalidProject(_)
    ));
    assert!(matches!(
        validate_project(&"x".repeat(65)).unwrap_err(),
        HostLauncherError::InvalidProject(_)
    ));
}

#[test]
fn test_container_command_generation() {
    let cmd = vec!["cargo".to_string(), "build".to_string()];
    let args = container_command("my-proj", &cmd, 16, 28, None, None).unwrap();

    assert_eq!(args[0], "docker");
    assert_eq!(args[1], "run");
    assert!(args.contains(&"--cpus=16".to_string()));
    assert!(args.contains(&"--memory=28g".to_string()));
    assert!(args.contains(&"--user=1001:1001".to_string()));
    assert!(args.contains(&"-v".to_string()));
    assert!(args.contains(&"/mnt/runner-data/remote-build/sources/my-proj:/src".to_string()));
    assert!(args.contains(&"/mnt/runner-data/remote-build/targets/my-proj:/target".to_string()));
    assert!(args.contains(&"/mnt/runner-data/remote-build/cargo:/cargo".to_string()));

    // Disallowed command
    let bad_cmd = vec!["bash".to_string()];
    assert!(matches!(
        container_command("my-proj", &bad_cmd, 16, 28, None, None).unwrap_err(),
        HostLauncherError::UnsupportedCommand(_)
    ));

    // Unreviewed image
    assert!(matches!(
        container_command("my-proj", &cmd, 16, 28, Some("evil-image:latest"), None).unwrap_err(),
        HostLauncherError::UnreviewedImage(_)
    ));
}

#[test]
fn test_checked_path_refuses_symlinks() {
    let temp = TempDir::new().unwrap();
    let temp_path = temp.path().canonicalize().unwrap();
    let real_dir = temp_path.join("real");
    fs::create_dir(&real_dir).unwrap();

    assert!(checked_path(&real_dir).is_ok());

    let link_dir = temp_path.join("symlink_dir");
    symlink(&real_dir, &link_dir).unwrap();

    assert!(matches!(
        checked_path(&link_dir).unwrap_err(),
        HostLauncherError::SymlinkedPath(_)
    ));
}

#[test]
fn test_cleanup_source_descriptor_relative() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(&source).unwrap();

    let external = temp.path().join("external");
    fs::create_dir_all(&external).unwrap();
    fs::write(external.join("important.txt"), "keep this").unwrap();

    // Create nested content inside source
    fs::write(source.join("file1.rs"), "code").unwrap();
    let sub = source.join("subdir");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("file2.rs"), "more code").unwrap();

    // Create nested symlink pointing to external
    symlink(&external, source.join("symlink_to_external")).unwrap();

    // Run cleanup
    cleanup_source(&source).unwrap();

    // Verify source is empty
    assert!(fs::read_dir(&source).unwrap().next().is_none());

    // Verify external directory is intact
    assert!(external.exists());
    assert!(external.join("important.txt").exists());
}

#[test]
fn test_cleanup_source_depth_bound() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(&source).unwrap();

    let mut curr = source.clone();
    for i in 0..70 {
        curr = curr.join(format!("level_{i}"));
        fs::create_dir(&curr).unwrap();
    }

    let err = cleanup_source(&source).unwrap_err();
    assert!(matches!(err, HostLauncherError::DepthBoundExceeded));
}

#[test]
fn test_prepare_refuses_symlinks_in_sources() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let sources = root.join("sources");
    fs::create_dir_all(&sources).unwrap();

    let external = temp.path().join("external_dir");
    fs::create_dir_all(&external).unwrap();

    // Place a symlink at sources/evil-proj pointing to external_dir
    let evil_link = sources.join("evil-proj");
    symlink(&external, &evil_link).unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    let res = host_agent::host_launcher::run(&["prepare".to_string(), "evil-proj".to_string()]);
    assert!(
        res.is_err(),
        "host-launcher prepare must refuse when project source is a symlink"
    );
}

#[test]
fn test_run_build_draining_sentinel_returns_75() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let lock_root = temp.path().canonicalize().unwrap();
    fs::write(lock_root.join("oxidex-build-draining"), "draining").unwrap();

    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    let code = host_agent::host_launcher::run(&["my-proj".to_string()]).unwrap();
    assert_eq!(
        code, 75,
        "Build invocation during drain must return exit code 75"
    );
}

#[test]
fn test_run_build_contended_lock_returns_75() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let lock_root = temp.path().canonicalize().unwrap();
    let lock_path = lock_root.join("oxidex-build.lock");
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .unwrap();

    // Take exclusive lock
    unsafe {
        libc::flock(
            std::os::unix::io::AsRawFd::as_raw_fd(&lock_file),
            libc::LOCK_EX | libc::LOCK_NB,
        );
    }

    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    let code = host_agent::host_launcher::run(&["my-proj".to_string()]).unwrap();
    assert_eq!(
        code, 75,
        "Build invocation with contended global lock must return exit code 75"
    );
}

#[test]
fn test_verify_mounts_not_active() {
    use host_agent::host_launcher::verify_mounts_not_active;
    use std::path::Path;
    let source = Path::new("/mnt/runner-data/remote-build/sources/my-proj");
    let target = Path::new("/mnt/runner-data/remote-build/targets/my-proj");

    // 1. Unmounted container json
    let json_clean = r#"[
        {
            "Id": "1111111111111111111111111111111111111111111111111111111111111111",
            "Mounts": [
                {"Source": "/mnt/runner-data/remote-build/sources/other-proj"}
            ]
        }
    ]"#;
    assert!(verify_mounts_not_active(source, target, json_clean).is_ok());

    // 2. Container actively mounting source
    let json_source_mounted = r#"[
        {
            "Id": "1111111111111111111111111111111111111111111111111111111111111111",
            "Mounts": [
                {"Source": "/mnt/runner-data/remote-build/sources/my-proj"}
            ]
        }
    ]"#;
    assert!(verify_mounts_not_active(source, target, json_source_mounted).is_err());

    // 3. Container actively mounting target
    let json_target_mounted = r#"[
        {
            "Id": "1111111111111111111111111111111111111111111111111111111111111111",
            "Mounts": [
                {"Source": "/mnt/runner-data/remote-build/targets/my-proj"}
            ]
        }
    ]"#;
    assert!(verify_mounts_not_active(source, target, json_target_mounted).is_err());
}

// ============================================================================
// Host-Launcher Cleanup Symlink Immunity Regression Suite
// ============================================================================

#[test]
fn test_cleanup_refuses_symlink_in_sources() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

    // 1. Setup external victim directory with confidential data and canary files
    let victim = temp.path().join("victim_sources");
    fs::create_dir_all(&victim).unwrap();
    let victim_proj = victim.join("alpha");
    fs::create_dir_all(&victim_proj).unwrap();

    let secret_file = victim_proj.join("secret_data.txt");
    fs::write(&secret_file, "CONFIDENTIAL_PAYLOAD_NEVER_DELETE").unwrap();

    let canary_file = victim.join("canary_sources.txt");
    fs::write(&canary_file, "CANARY_INTEGRITY_VERIFIED").unwrap();

    // 2. Setup root/sources as a symlink pointing to victim directory
    let sources_symlink = root.join("sources");
    symlink(&victim, &sources_symlink).unwrap();

    // Ensure targets directory exists as authentic real directory
    let targets = root.join("targets");
    fs::create_dir_all(&targets).unwrap();

    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    // 3. Execute cleanup on project 'alpha'
    let res = host_agent::host_launcher::run(&["cleanup".to_string(), "alpha".to_string()]);

    // 4. Assert failure with HostLauncherError::SymlinkedPath
    assert!(
        res.is_err(),
        "host-launcher cleanup MUST fail when root/sources is a symlink, but returned: {:?}",
        res
    );
    let err = res.unwrap_err();
    let launcher_err = err
        .downcast_ref::<HostLauncherError>()
        .expect("Returned error must downcast to HostLauncherError");
    assert!(
        matches!(launcher_err, HostLauncherError::SymlinkedPath(_)),
        "Expected SymlinkedPath error variant, got: {:?}",
        launcher_err
    );

    // 5. Verify all victim files and canaries outside the authentic tree are 100% intact
    assert!(
        secret_file.exists(),
        "CRITICAL INVARIANT VIOLATED: secret_data.txt was deleted from victim directory!"
    );
    assert_eq!(
        fs::read_to_string(&secret_file).unwrap(),
        "CONFIDENTIAL_PAYLOAD_NEVER_DELETE",
        "secret file content was modified"
    );
    assert!(
        canary_file.exists(),
        "canary_sources.txt was deleted from victim directory!"
    );
    assert_eq!(
        fs::read_to_string(&canary_file).unwrap(),
        "CANARY_INTEGRITY_VERIFIED",
        "canary file content was modified"
    );
}

#[test]
fn test_cleanup_refuses_symlink_in_targets() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

    // 1. Setup external victim directory containing build artifacts and cache
    let victim_targets = temp.path().join("victim_targets");
    fs::create_dir_all(&victim_targets).unwrap();
    let victim_proj = victim_targets.join("beta");
    fs::create_dir_all(&victim_proj).unwrap();

    let target_artifact = victim_proj.join("liboxidex.rlib");
    fs::write(&target_artifact, b"\x7fELF_FAKE_RLIB_CACHE_DATA").unwrap();

    let canary_file = victim_targets.join("canary_targets.txt");
    fs::write(&canary_file, "TARGETS_INTEGRITY_CANARY").unwrap();

    // 2. Setup authentic sources directory and project source
    let sources = root.join("sources");
    fs::create_dir_all(&sources).unwrap();
    let proj_source = sources.join("beta");
    fs::create_dir_all(&proj_source).unwrap();
    let source_main = proj_source.join("main.rs");
    fs::write(&source_main, "fn main() { println!(\"beta\"); }").unwrap();

    // 3. Symlink root/targets -> victim_targets
    let targets_symlink = root.join("targets");
    symlink(&victim_targets, &targets_symlink).unwrap();

    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    // 4. Execute cleanup on project 'beta'
    let res = host_agent::host_launcher::run(&["cleanup".to_string(), "beta".to_string()]);

    // 5. Assert failure with HostLauncherError::SymlinkedPath
    assert!(
        res.is_err(),
        "host-launcher cleanup MUST fail when root/targets is a symlink, but returned: {:?}",
        res
    );
    let err = res.unwrap_err();
    let launcher_err = err
        .downcast_ref::<HostLauncherError>()
        .expect("Returned error must downcast to HostLauncherError");
    assert!(
        matches!(launcher_err, HostLauncherError::SymlinkedPath(_)),
        "Expected SymlinkedPath error variant, got: {:?}",
        launcher_err
    );

    // 6. Verify victim target files and canaries are completely untouched
    assert!(
        target_artifact.exists(),
        "liboxidex.rlib was deleted from victim targets directory!"
    );
    assert_eq!(
        fs::read(&target_artifact).unwrap(),
        b"\x7fELF_FAKE_RLIB_CACHE_DATA",
        "target artifact content was modified"
    );
    assert!(
        canary_file.exists(),
        "canary_targets.txt was deleted from victim targets directory!"
    );
    assert_eq!(
        fs::read_to_string(&canary_file).unwrap(),
        "TARGETS_INTEGRITY_CANARY"
    );

    // 7. Verify that authentic source was preserved because cleanup aborted before source_not_mounted / cleanup_source
    assert!(
        source_main.exists(),
        "Authentic source main.rs was wiped even though targets check failed!"
    );
}

#[test]
fn test_cleanup_refuses_symlinked_root_or_ancestor() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();

    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    // ========================================================================
    // Case A: root itself is a direct symlink
    // ========================================================================
    {
        let real_build_root = temp.path().join("real_build_root");
        fs::create_dir_all(&real_build_root).unwrap();
        let sources = real_build_root.join("sources").join("gamma");
        fs::create_dir_all(&sources).unwrap();
        let source_file = sources.join("gamma.rs");
        fs::write(&source_file, "fn gamma() {}").unwrap();

        let targets = real_build_root.join("targets").join("gamma");
        fs::create_dir_all(&targets).unwrap();

        let canary_external = temp.path().join("root_canary_a.txt");
        fs::write(&canary_external, "ROOT_A_CANARY_VALUE").unwrap();

        let symlinked_root = temp.path().join("symlinked_root");
        symlink(&real_build_root, &symlinked_root).unwrap();

        std::env::set_var("OXIDEX_BUILD_ROOT", symlinked_root.to_str().unwrap());

        let res = host_agent::host_launcher::run(&["cleanup".to_string(), "gamma".to_string()]);
        assert!(
            res.is_err(),
            "host-launcher cleanup MUST fail when root itself is a symlink, returned: {:?}",
            res
        );
        let err = res.unwrap_err();
        let launcher_err = err
            .downcast_ref::<HostLauncherError>()
            .expect("Must downcast to HostLauncherError");
        assert!(
            matches!(launcher_err, HostLauncherError::SymlinkedPath(_)),
            "Expected SymlinkedPath, got: {:?}",
            launcher_err
        );

        // Verification of preservation
        assert!(canary_external.exists());
        assert_eq!(
            fs::read_to_string(&canary_external).unwrap(),
            "ROOT_A_CANARY_VALUE"
        );
        assert!(
            source_file.exists(),
            "Files in real_build_root must be preserved because symlinked root is rejected"
        );
    }

    // ========================================================================
    // Case B: root path contains an ancestor directory that is a symlink
    // ========================================================================
    {
        let real_intermediate = temp.path().join("real_intermediate");
        fs::create_dir_all(&real_intermediate).unwrap();

        let nested_root = real_intermediate.join("nested_root");
        fs::create_dir_all(&nested_root).unwrap();
        let sources = nested_root.join("sources").join("delta");
        fs::create_dir_all(&sources).unwrap();
        let source_file = sources.join("delta.rs");
        fs::write(&source_file, "fn delta() {}").unwrap();

        let targets = nested_root.join("targets").join("delta");
        fs::create_dir_all(&targets).unwrap();

        let canary_external = temp.path().join("root_canary_b.txt");
        fs::write(&canary_external, "ROOT_B_CANARY_VALUE").unwrap();

        let symlink_intermediate = temp.path().join("symlink_intermediate");
        symlink(&real_intermediate, &symlink_intermediate).unwrap();

        // root_with_ancestor_symlink = .../symlink_intermediate/nested_root
        let root_with_ancestor_symlink = symlink_intermediate.join("nested_root");
        std::env::set_var(
            "OXIDEX_BUILD_ROOT",
            root_with_ancestor_symlink.to_str().unwrap(),
        );

        let res = host_agent::host_launcher::run(&["cleanup".to_string(), "delta".to_string()]);
        assert!(
            res.is_err(),
            "host-launcher cleanup MUST fail when root path has an ancestor symlink, returned: {:?}",
            res
        );
        let err = res.unwrap_err();
        let launcher_err = err
            .downcast_ref::<HostLauncherError>()
            .expect("Must downcast to HostLauncherError");
        assert!(
            matches!(launcher_err, HostLauncherError::SymlinkedPath(_)),
            "Expected SymlinkedPath, got: {:?}",
            launcher_err
        );

        // Verification of preservation
        assert!(canary_external.exists());
        assert_eq!(
            fs::read_to_string(&canary_external).unwrap(),
            "ROOT_B_CANARY_VALUE"
        );
        assert!(source_file.exists());
    }
}

#[test]
fn test_cleanup_preserves_external_files_untouched() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();

    // 1. Establish an external environment outside build_root representing sensitive host paths
    let external_root = temp.path().join("external_host_system");
    fs::create_dir_all(&external_root).unwrap();

    // Mock /etc files
    let mock_etc = external_root.join("etc");
    fs::create_dir_all(&mock_etc).unwrap();
    let passwd_file = mock_etc.join("passwd");
    fs::write(
        &passwd_file,
        "root:x:0:0:root:/root:/bin/bash\nuser:x:1001:1001::/home/user:/bin/bash",
    )
    .unwrap();
    let shadow_file = mock_etc.join("shadow");
    fs::write(&shadow_file, "root:$6$salt$hash:19000:0:99999:7:::").unwrap();

    // Mock user home files
    let mock_home = external_root.join("home_user");
    fs::create_dir_all(&mock_home).unwrap();
    let ssh_key = mock_home.join("id_ed25519");
    fs::write(
        &ssh_key,
        "-----BEGIN OPENSSH PRIVATE KEY-----\nMOCK_KEY\n-----END OPENSSH PRIVATE KEY-----",
    )
    .unwrap();

    // Mock adjacent project within authentic sources
    let sources = root.join("sources");
    fs::create_dir_all(&sources).unwrap();
    let adjacent_proj = sources.join("adjacent_workload");
    fs::create_dir_all(&adjacent_proj).unwrap();
    let adjacent_file = adjacent_proj.join("lib.rs");
    fs::write(
        &adjacent_file,
        "pub fn adjacent_safe_code() -> bool { true }",
    )
    .unwrap();

    let targets = root.join("targets");
    fs::create_dir_all(&targets).unwrap();

    // Record baseline digests/contents of all external files
    let external_files = vec![
        (&passwd_file, fs::read(&passwd_file).unwrap()),
        (&shadow_file, fs::read(&shadow_file).unwrap()),
        (&ssh_key, fs::read(&ssh_key).unwrap()),
        (&adjacent_file, fs::read(&adjacent_file).unwrap()),
    ];

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    // Attack Scenario A: Project source is a direct symlink to mock_etc
    let evil_link_etc = sources.join("evil_etc");
    symlink(&mock_etc, &evil_link_etc).unwrap();

    let res_a = host_agent::host_launcher::run(&["cleanup".to_string(), "evil_etc".to_string()]);
    assert!(
        res_a.is_err(),
        "Cleanup of symlinked project source to /etc must fail"
    );

    // Verify all baseline external files remain untouched
    for (path, original_bytes) in &external_files {
        assert!(
            path.exists(),
            "External file {:?} must exist after attack A",
            path
        );
        assert_eq!(
            &fs::read(path).unwrap(),
            original_bytes,
            "Content of {:?} modified after attack A",
            path
        );
    }

    fs::remove_file(&evil_link_etc).unwrap();

    // Attack Scenario B: Project source is a direct symlink to mock_home
    let evil_link_home = sources.join("evil_home");
    symlink(&mock_home, &evil_link_home).unwrap();

    let res_b = host_agent::host_launcher::run(&["cleanup".to_string(), "evil_home".to_string()]);
    assert!(
        res_b.is_err(),
        "Cleanup of symlinked project source to home must fail"
    );

    // Verify all baseline external files remain untouched
    for (path, original_bytes) in &external_files {
        assert!(
            path.exists(),
            "External file {:?} must exist after attack B",
            path
        );
        assert_eq!(
            &fs::read(path).unwrap(),
            original_bytes,
            "Content of {:?} modified after attack B",
            path
        );
    }

    fs::remove_file(&evil_link_home).unwrap();

    // Attack Scenario C: Targets is replaced with a symlink to mock_home
    fs::remove_dir(&targets).unwrap();
    symlink(&mock_home, &targets).unwrap();

    let res_c =
        host_agent::host_launcher::run(&["cleanup".to_string(), "adjacent_workload".to_string()]);
    assert!(
        res_c.is_err(),
        "Cleanup when targets is a symlink to home must fail"
    );

    // Verify all baseline external files remain untouched
    for (path, original_bytes) in &external_files {
        assert!(
            path.exists(),
            "External file {:?} must exist after attack C",
            path
        );
        assert_eq!(
            &fs::read(path).unwrap(),
            original_bytes,
            "Content of {:?} modified after attack C",
            path
        );
    }
}

#[test]
fn test_toolchain_probe_allowed_in_container_command() {
    assert!(is_command_allowed("oxidex-toolchain-probe"));
    assert!(is_command_allowed("/usr/local/bin/oxidex-toolchain-probe"));
    assert!(is_command_allowed("toolchain-probe"));
    assert!(is_command_allowed("/opt/bin/toolchain-probe"));
    assert!(!is_command_allowed("bash"));
    assert!(!is_command_allowed("/bin/sh"));

    let cmd = vec!["/usr/local/bin/oxidex-toolchain-probe".to_string()];
    let res = container_command("my-proj", &cmd, 16, 28, None, None);
    assert!(res.is_ok());
    let args = res.unwrap();
    assert!(args.contains(&"/usr/local/bin/oxidex-toolchain-probe".to_string()));
}

#[test]
fn test_resolve_uploader_uid_matrix() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let config_file = temp.path().join("uploader.json");
    fs::write(
        &config_file,
        r#"{"principal":"swackhamer","username":"oxidex-uploader","uid":1002}"#,
    )
    .unwrap();

    std::env::set_var("OXIDEX_UPLOADER_CONFIG", config_file.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    // 1. Matched SUDO_UID
    std::env::set_var("SUDO_UID", "1002");
    assert_eq!(resolve_uploader_uid(None).unwrap(), 1002);

    // 2. Mismatched SUDO_UID
    std::env::set_var("SUDO_UID", "1005");
    let err = resolve_uploader_uid(None).unwrap_err();
    assert!(matches!(err, HostLauncherError::InvalidUploader(_)));

    // 3. Matched explicit uid
    std::env::remove_var("SUDO_UID");
    assert_eq!(resolve_uploader_uid(Some(1002)).unwrap(), 1002);

    // 4. Mismatched explicit uid
    let err = resolve_uploader_uid(Some(1003)).unwrap_err();
    assert!(matches!(err, HostLauncherError::InvalidUploader(_)));

    // 5. Config only without SUDO_UID or explicit UID
    assert_eq!(resolve_uploader_uid(None).unwrap(), 1002);

    // 6. Root UID 0 is strictly rejected
    std::env::remove_var("OXIDEX_UPLOADER_CONFIG");
    let root_cfg = temp.path().join("root_uploader.json");
    fs::write(
        &root_cfg,
        r#"{"principal":"root","username":"root","uid":0}"#,
    )
    .unwrap();
    std::env::set_var("OXIDEX_UPLOADER_CONFIG", root_cfg.to_str().unwrap());
    let err = resolve_uploader_uid(None).unwrap_err();
    assert!(matches!(err, HostLauncherError::InvalidUploader(_)));

    // Cleanup env
    std::env::remove_var("OXIDEX_UPLOADER_CONFIG");
    std::env::remove_var("SUDO_UID");
}

#[test]
fn test_host_launcher_run_positional_syntax() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    // Standard prepare: prepare <project>
    let res1 = host_agent::host_launcher::run(&["prepare".to_string(), "alpha".to_string()]);
    assert_eq!(res1.unwrap(), 0);
    assert!(root.join("sources").join("alpha").exists());

    // Inverted prepare: <project> prepare
    let res2 = host_agent::host_launcher::run(&["beta".to_string(), "prepare".to_string()]);
    assert_eq!(res2.unwrap(), 0);
    assert!(root.join("sources").join("beta").exists());

    // Standard cleanup: cleanup <project>
    let res3 = host_agent::host_launcher::run(&["cleanup".to_string(), "alpha".to_string()]);
    assert_eq!(res3.unwrap(), 0);

    // Inverted cleanup: <project> cleanup
    let res4 = host_agent::host_launcher::run(&["beta".to_string(), "cleanup".to_string()]);
    assert_eq!(res4.unwrap(), 0);
}
