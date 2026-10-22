use std::fs;
use std::os::unix::io::AsRawFd;
use tempfile::TempDir;

use host_agent::docker_admission::{
    acquire_build_locks, validate_build_policy, validate_project_name, DockerAdmissionError,
    BUILDER_IMAGE,
};

fn valid_builder_args(project: &str, root: &str) -> Vec<String> {
    vec![
        "run".to_string(),
        "--rm".to_string(),
        "--user=1001:1001".to_string(),
        "--cpus=16".to_string(),
        "--label=oxidex.remote-build=true".to_string(),
        "--memory=28g".to_string(),
        "--memory-swap=28g".to_string(),
        "--pids-limit=4096".to_string(),
        "--cap-drop=ALL".to_string(),
        "--security-opt=no-new-privileges".to_string(),
        "--entrypoint=/usr/bin/time".to_string(),
        "-v".to_string(),
        format!("{root}/cargo:/cargo"),
        "-v".to_string(),
        format!("{root}/sources/{project}:/src"),
        "-v".to_string(),
        format!("{root}/targets/{project}:/target"),
        BUILDER_IMAGE.to_string(),
        "cargo".to_string(),
        "build".to_string(),
    ]
}

#[test]
fn test_valid_build_policy_passes() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-project_1";
    let args = valid_builder_args(project, root);
    let p = validate_build_policy(&args, 16, 28, Some(root)).unwrap();
    assert_eq!(p, project);
}

#[test]
fn test_docker_sock_strictly_rejected() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-proj";
    let mut args = valid_builder_args(project, root);

    // Inject /var/run/docker.sock mount
    args.insert(1, "-v".to_string());
    args.insert(2, "/var/run/docker.sock:/var/run/docker.sock".to_string());

    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::ForbiddenMount(_)));

    // Inject disguised .sock mount
    let mut args2 = valid_builder_args(project, root);
    args2.insert(1, "-v".to_string());
    args2.insert(2, "/tmp/evil.sock:/container.sock".to_string());
    let err2 = validate_build_policy(&args2, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err2, DockerAdmissionError::ForbiddenMount(_)));
}

#[test]
fn test_disallowed_options_rejected() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-proj";

    // Privileged
    let mut args = valid_builder_args(project, root);
    args.insert(1, "--privileged".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::DisallowedOption(_)
    ));

    // Cap add
    let mut args = valid_builder_args(project, root);
    args.insert(1, "--cap-add=SYS_ADMIN".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::DisallowedOption(_)
    ));

    // Network host
    let mut args = valid_builder_args(project, root);
    args.insert(1, "--net=host".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::DisallowedOption(_)
    ));
}

#[test]
fn test_repeated_options_rejected() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-proj";
    let mut args = valid_builder_args(project, root);
    args.insert(1, "--cpus=16".to_string());

    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::RepeatedOption(_)));
}

#[test]
fn test_resource_tampering_rejected() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-proj";

    // 32 CPUs
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a == "--cpus=16") {
        args[pos] = "--cpus=32".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::ResourceCapMismatch(_)
    ));

    // 32g memory
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a == "--memory=28g") {
        args[pos] = "--memory=32g".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::ResourceCapMismatch(_)
    ));

    // User root
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a == "--user=1001:1001") {
        args[pos] = "--user=0:0".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::PolicyMismatch(_)
    ));
}

#[test]
fn test_caller_assigned_identity_rejected() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-proj";

    let mut args = valid_builder_args(project, root);
    args.insert(1, "--name=evil".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::DisallowedOption(_)
    ));

    let mut args = valid_builder_args(project, root);
    args.insert(1, "--label=oxidex.cache-project=foo".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::DisallowedOption(_)
    ));
}

#[test]
fn test_project_name_regex() {
    assert!(validate_project_name("valid-project_123").is_ok());
    assert!(validate_project_name("A").is_ok());

    // Bad project names
    assert!(validate_project_name("../escape").is_err());
    assert!(validate_project_name("-invalid-leading-dash").is_err());
    assert!(validate_project_name("has space").is_err());
    assert!(validate_project_name("has$dollar").is_err());
    assert!(validate_project_name("").is_err());
    assert!(validate_project_name(&"x".repeat(65)).is_err());
}

#[test]
fn test_lock_files_and_draining() {
    let temp = TempDir::new().unwrap();
    let lock_root = temp.path();

    // 1. Success acquiring locks
    let locks = acquire_build_locks("test-proj", lock_root).unwrap();
    drop(locks);

    // 2. Draining file present -> returns 75
    let drain_file = lock_root.join("oxidex-build-draining");
    fs::write(&drain_file, "draining").unwrap();
    let res = acquire_build_locks("test-proj", lock_root);
    assert_eq!(res.unwrap_err(), 75);
    fs::remove_file(&drain_file).unwrap();

    // 3. Contended exclusive cache lock -> returns 75
    let active_path = lock_root.join("oxidex-cache-active-test-proj.lock");
    let active_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&active_path)
        .unwrap();
    unsafe {
        libc::flock(active_file.as_raw_fd(), libc::LOCK_EX);
    }

    let res = acquire_build_locks("test-proj", lock_root);
    assert_eq!(res.unwrap_err(), 75);

    unsafe {
        libc::flock(active_file.as_raw_fd(), libc::LOCK_UN);
    }
}
