use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use host_agent::clean_target::{clean_target_with_env, CleanOutcome, CleanTargetError};
use host_agent::docker_admission::{
    validate_build_policy, validate_project_name, DockerAdmissionError, BUILDER_IMAGE,
};
use host_agent::host_launcher::{
    run as host_launcher_run, verify_mounts_not_active, HostLauncherError,
};

// ============================================================================
// Fixture Helpers
// ============================================================================

struct CleanTargetFixture {
    _temp: TempDir,
    work: PathBuf,
    workspace: PathBuf,
    env: HashMap<String, String>,
}

impl CleanTargetFixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("create tempdir");
        let work = temp.path().join("work");
        let workspace = work.join("oxidex").join("oxidex");
        fs::create_dir_all(&workspace).expect("create workspace");

        let mut env = HashMap::new();
        env.insert("OXIDEX_CLEAN_TARGET".into(), "1".into());
        env.insert("GITHUB_REPOSITORY".into(), "swack-tools/oxidex".into());
        env.insert(
            "GITHUB_WORKSPACE".into(),
            workspace.to_string_lossy().into(),
        );

        Self {
            _temp: temp,
            work,
            workspace,
            env,
        }
    }
}

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

// ============================================================================
// Task 1: Symlink Escape Attacks against clean-target
// ============================================================================

#[test]
fn test_adversarial_clean_target_work_root_symlink() {
    let temp = TempDir::new().expect("create tempdir");
    let external_victim = temp.path().join("victim_dir");
    fs::create_dir_all(&external_victim).unwrap();
    let secret = external_victim.join("sensitive.txt");
    fs::write(&secret, "sensitive data").unwrap();

    // work_root is itself a symlink pointing to external_victim
    let work_symlink = temp.path().join("work_symlink");
    symlink(&external_victim, &work_symlink).unwrap();

    let workspace = work_symlink.join("oxidex").join("oxidex");
    let mut env = HashMap::new();
    env.insert("OXIDEX_CLEAN_TARGET".into(), "1".into());
    env.insert("GITHUB_REPOSITORY".into(), "swack-tools/oxidex".into());
    env.insert(
        "GITHUB_WORKSPACE".into(),
        workspace.to_string_lossy().into(),
    );

    let res = clean_target_with_env(&work_symlink, &env);
    assert!(res.is_err(), "Must refuse symlink at work_root");
    assert!(secret.exists(), "Victim data must be preserved");
}

#[test]
fn test_adversarial_clean_target_repo_dir_symlink() {
    let f = CleanTargetFixture::new();
    let external_victim = f.work.parent().unwrap().join("victim_repo");
    fs::create_dir_all(&external_victim).unwrap();
    let secret = external_victim.join("sensitive_repo.txt");
    fs::write(&secret, "sensitive repo data").unwrap();

    // Replace work/oxidex with a symlink to victim
    let repo_dir = f.work.join("oxidex");
    fs::remove_dir_all(&repo_dir).unwrap();
    symlink(&external_victim, &repo_dir).unwrap();

    let res = clean_target_with_env(&f.work, &f.env);
    assert!(res.is_err(), "Must refuse symlink at work/oxidex");
    assert!(secret.exists(), "Victim data must be preserved");
}

#[test]
fn test_adversarial_clean_target_workspace_symlink_escape() {
    let f = CleanTargetFixture::new();
    let external_victim = f.work.parent().unwrap().join("victim_workspace");
    fs::create_dir_all(external_victim.join("target")).unwrap();
    let secret = external_victim
        .join("target")
        .join("important_build_artifact");
    fs::write(&secret, "sensitive artifact").unwrap();

    // Replace work/oxidex/oxidex with a symlink to victim_workspace
    fs::remove_dir(&f.workspace).unwrap();
    symlink(&external_victim, &f.workspace).unwrap();

    let res = clean_target_with_env(&f.work, &f.env);
    assert!(res.is_err(), "Must refuse symlink at workspace level");
    assert!(secret.exists(), "Victim artifact must remain untouched");
}

#[test]
fn test_adversarial_clean_target_target_symlink_to_sensitive_dir() {
    let f = CleanTargetFixture::new();
    let external_sensitive = f.work.parent().unwrap().join("sensitive_directory");
    fs::create_dir_all(&external_sensitive).unwrap();
    let password_file = external_sensitive.join("passwords.txt");
    fs::write(&password_file, "admin:supersecret").unwrap();

    // target is a symlink pointing to sensitive_directory
    let target_symlink = f.workspace.join("target");
    symlink(&external_sensitive, &target_symlink).unwrap();

    let res = clean_target_with_env(&f.work, &f.env);
    match res {
        Err(CleanTargetError::NotADirectory) => (),
        other => panic!("Expected NotADirectory error, got: {:?}", other),
    }

    assert!(
        target_symlink.exists(),
        "Symlink itself was not removed on error"
    );
    assert!(
        external_sensitive.exists(),
        "External directory must remain intact"
    );
    assert!(
        password_file.exists(),
        "External file must remain untouched"
    );
}

#[test]
fn test_adversarial_clean_target_target_symlink_to_sensitive_file() {
    let f = CleanTargetFixture::new();
    let external_file = f.work.parent().unwrap().join("server.key");
    fs::write(&external_file, "-----BEGIN PRIVATE KEY-----").unwrap();

    // target is a symlink pointing to a file
    let target_symlink = f.workspace.join("target");
    symlink(&external_file, &target_symlink).unwrap();

    let res = clean_target_with_env(&f.work, &f.env);
    assert!(matches!(res, Err(CleanTargetError::NotADirectory)));
    assert!(
        external_file.exists(),
        "Target file must not be modified or deleted"
    );
}

#[test]
fn test_adversarial_clean_target_nested_symlinks_to_sensitive_targets() {
    let f = CleanTargetFixture::new();
    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();

    let external_victim = f.work.parent().unwrap().join("external_victim");
    fs::create_dir_all(&external_victim).unwrap();
    let victim_file = external_victim.join("db.sqlite");
    fs::write(&victim_file, "critical database").unwrap();

    // Symlink to external file
    symlink(&victim_file, target.join("link_to_db")).unwrap();
    // Symlink to external dir
    symlink(&external_victim, target.join("link_to_dir")).unwrap();
    // Symlink to root filesystem
    symlink(Path::new("/"), target.join("link_to_root")).unwrap();
    // Symlink to parent (..)
    symlink(Path::new(".."), target.join("link_to_parent")).unwrap();
    // Nested subdirectory with symlinks
    let sub = target.join("subdir");
    fs::create_dir_all(&sub).unwrap();
    symlink(&victim_file, sub.join("deep_link")).unwrap();

    let outcome = clean_target_with_env(&f.work, &f.env).expect("cleanup should succeed safely");
    assert!(matches!(outcome, CleanOutcome::TargetRemoved { .. }));

    // Target must be gone
    assert!(!target.exists(), "target directory must be cleaned");
    // External victim must remain intact
    assert!(
        external_victim.exists(),
        "Victim directory must not be deleted"
    );
    assert!(victim_file.exists(), "Victim file must not be deleted");
    assert_eq!(
        fs::read_to_string(&victim_file).unwrap(),
        "critical database",
        "Victim file content must not be modified"
    );
}

#[test]
fn test_adversarial_clean_target_circular_symlinks() {
    let f = CleanTargetFixture::new();
    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();

    // 1. Direct self loop: target/self_loop -> target/self_loop
    symlink("self_loop", target.join("self_loop")).unwrap();

    // 2. Loop to current dir: target/dot_loop -> .
    symlink(".", target.join("dot_loop")).unwrap();

    // 3. Mutual cycle: a -> b, b -> a
    symlink("cycle_b", target.join("cycle_a")).unwrap();
    symlink("cycle_a", target.join("cycle_b")).unwrap();

    // 4. Nested loop pointing back to target
    let sub = target.join("sub");
    fs::create_dir_all(&sub).unwrap();
    symlink("..", sub.join("loop_to_target")).unwrap();
    symlink("nested_self", sub.join("nested_self")).unwrap();

    // Add regular build output to verify it gets deleted too
    fs::write(target.join("output.o"), "binary code").unwrap();
    fs::write(sub.join("suboutput.o"), "binary code").unwrap();

    let outcome = clean_target_with_env(&f.work, &f.env)
        .expect("cleanup must handle circular symlinks safely");
    assert!(matches!(outcome, CleanOutcome::TargetRemoved { .. }));
    assert!(!target.exists(), "target directory must be deleted");
}

#[test]
fn test_adversarial_clean_target_depth_bound() {
    let f = CleanTargetFixture::new();
    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();

    // Nest 66 levels
    let mut current = target.clone();
    for i in 0..66 {
        current = current.join(format!("level_{i}"));
        fs::create_dir(&current).unwrap();
    }
    fs::write(current.join("leaf.txt"), "deep file").unwrap();

    let res = clean_target_with_env(&f.work, &f.env);
    assert!(matches!(res, Err(CleanTargetError::DepthBoundExceeded)));
}

// ============================================================================
// Task 2: Bypass Attacks against docker-admission
// ============================================================================

#[test]
fn test_adversarial_docker_admission_docker_sock_mount_bypasses() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-project";

    // Attempt 1: Direct /var/run/docker.sock mount
    let mut args = valid_builder_args(project, root);
    args.insert(1, "-v".to_string());
    args.insert(2, "/var/run/docker.sock:/var/run/docker.sock".to_string());
    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::ForbiddenMount(_)));

    // Attempt 2: Mount with disguised .sock name
    let mut args = valid_builder_args(project, root);
    args.insert(1, "-v".to_string());
    args.insert(2, "/tmp/evil_symlink.sock:/var/run/docker.sock".to_string());
    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::ForbiddenMount(_)));

    // Attempt 3: Mount /var/run parent directory
    let mut args = valid_builder_args(project, root);
    args.insert(1, "-v".to_string());
    args.insert(2, "/var/run:/var/run".to_string());
    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::ForbiddenMount(_)));

    // Attempt 4: Swapping destination to docker socket
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a.ends_with(":/target")) {
        args[pos] = format!("{root}/targets/{project}:/var/run/docker.sock");
    }
    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::ForbiddenMount(_)));

    // Attempt 5: Alias flag --volume
    let mut args = valid_builder_args(project, root);
    args.insert(
        1,
        "--volume=/var/run/docker.sock:/var/run/docker.sock".to_string(),
    );
    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::DisallowedOption(_)));

    // Attempt 6: Alias flag --mount
    let mut args = valid_builder_args(project, root);
    args.insert(
        1,
        "--mount=type=bind,source=/var/run/docker.sock,target=/var/run/docker.sock".to_string(),
    );
    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::DisallowedOption(_)));
}

#[test]
fn test_adversarial_docker_admission_flags_reordering() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-reorder";

    // 1. Valid flags in scrambled order
    let scrambled_args = vec![
        "run".to_string(),
        "-v".to_string(),
        format!("{root}/cargo:/cargo"),
        "--security-opt=no-new-privileges".to_string(),
        "--memory-swap=28g".to_string(),
        "--entrypoint=/usr/bin/time".to_string(),
        "--rm".to_string(),
        "--cpus=16".to_string(),
        "-v".to_string(),
        format!("{root}/sources/{project}:/src"),
        "--cap-drop=ALL".to_string(),
        "--label=oxidex.remote-build=true".to_string(),
        "--user=1001:1001".to_string(),
        "-v".to_string(),
        format!("{root}/targets/{project}:/target"),
        "--pids-limit=4096".to_string(),
        "--memory=28g".to_string(),
        BUILDER_IMAGE.to_string(),
        "cargo".to_string(),
        "build".to_string(),
    ];
    let res = validate_build_policy(&scrambled_args, 16, 28, Some(root)).unwrap();
    assert_eq!(
        res, project,
        "Arbitrary flag order must be accepted if all required flags are present"
    );

    // 2. Duplicate flags with conflicting values: --user=1001:1001 then --user=0:0
    let mut dup_args = valid_builder_args(project, root);
    dup_args.insert(1, "--user=0:0".to_string());
    let err = validate_build_policy(&dup_args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::RepeatedOption(_)));

    // 3. Space-separated options: --user 1001:1001 instead of --user=1001:1001
    let mut space_args = valid_builder_args(project, root);
    if let Some(pos) = space_args.iter().position(|a| a.starts_with("--user=")) {
        space_args.remove(pos);
        space_args.insert(pos, "--user".to_string());
        space_args.insert(pos + 1, "1001:1001".to_string());
    }
    let err = validate_build_policy(&space_args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::DisallowedOption(_)));

    // 4. Flags placed after image name
    let mut post_image_args = valid_builder_args(project, root);
    post_image_args.push("--privileged".to_string());
    // The policy succeeds because post-image args are payload commands, NOT docker flags
    let res = validate_build_policy(&post_image_args, 16, 28, Some(root)).unwrap();
    assert_eq!(res, project);
}

#[test]
fn test_adversarial_docker_admission_unapproved_capabilities() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-caps";

    // 1. --cap-add=ALL
    let mut args = valid_builder_args(project, root);
    args.insert(1, "--cap-add=ALL".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::DisallowedOption(_)
    ));

    // 2. --cap-add=SYS_ADMIN
    let mut args = valid_builder_args(project, root);
    args.insert(1, "--cap-add=SYS_ADMIN".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::DisallowedOption(_)
    ));

    // 3. --cap-drop=NET_ADMIN (dropping only one cap instead of ALL)
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a == "--cap-drop=ALL") {
        args[pos] = "--cap-drop=NET_ADMIN".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::PolicyMismatch(_)
    ));

    // 4. --privileged
    let mut args = valid_builder_args(project, root);
    args.insert(1, "--privileged".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::DisallowedOption(_)
    ));

    // 5. --security-opt=seccomp=unconfined
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args
        .iter()
        .position(|a| a == "--security-opt=no-new-privileges")
    {
        args[pos] = "--security-opt=seccomp=unconfined".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::PolicyMismatch(_)
    ));

    // 6. Multiple --cap-drop
    let mut args = valid_builder_args(project, root);
    args.insert(1, "--cap-drop=ALL".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::RepeatedOption(_)
    ));
}

#[test]
fn test_adversarial_docker_admission_root_user() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-user";

    // 1. --user=0:0
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a == "--user=1001:1001") {
        args[pos] = "--user=0:0".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::PolicyMismatch(_)
    ));

    // 2. --user=root
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a == "--user=1001:1001") {
        args[pos] = "--user=root".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::PolicyMismatch(_)
    ));

    // 3. --user=0
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a == "--user=1001:1001") {
        args[pos] = "--user=0".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::PolicyMismatch(_)
    ));

    // 4. --user=1001 (missing GID)
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a == "--user=1001:1001") {
        args[pos] = "--user=1001".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::PolicyMismatch(_)
    ));

    // 5. --user=1000:1000
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a == "--user=1001:1001") {
        args[pos] = "--user=1000:1000".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::PolicyMismatch(_)
    ));

    // 6. --group-add=0
    let mut args = valid_builder_args(project, root);
    args.insert(1, "--group-add=0".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::DisallowedOption(_)
    ));

    // 7. --group-add=docker
    let mut args = valid_builder_args(project, root);
    args.insert(1, "--group-add=docker".to_string());
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::DisallowedOption(_)
    ));
}

#[test]
fn test_adversarial_docker_admission_project_injection_and_resource_tampering() {
    let root = "/mnt/runner-data/remote-build";

    // 1. Project name path traversal
    assert!(validate_project_name("../../etc/passwd").is_err());
    assert!(validate_project_name("project/sub").is_err());
    assert!(validate_project_name("project\\sub").is_err());
    assert!(validate_project_name("project;rm -rf /").is_err());
    assert!(validate_project_name("project`whoami`").is_err());
    assert!(validate_project_name("project$(id)").is_err());

    // 2. Resource tampering: cpus=32
    let mut args = valid_builder_args("proj1", root);
    if let Some(pos) = args.iter().position(|a| a == "--cpus=16") {
        args[pos] = "--cpus=32".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::ResourceCapMismatch(_)
    ));

    // 3. Memory tampering: memory=32g
    let mut args = valid_builder_args("proj1", root);
    if let Some(pos) = args.iter().position(|a| a == "--memory=28g") {
        args[pos] = "--memory=32g".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::ResourceCapMismatch(_)
    ));

    // 4. Memory swap tampering: memory-swap=32g
    let mut args = valid_builder_args("proj1", root);
    if let Some(pos) = args.iter().position(|a| a == "--memory-swap=28g") {
        args[pos] = "--memory-swap=32g".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::ResourceCapMismatch(_)
    ));

    // 5. Pids limit tampering: pids-limit=8192
    let mut args = valid_builder_args("proj1", root);
    if let Some(pos) = args.iter().position(|a| a == "--pids-limit=4096") {
        args[pos] = "--pids-limit=8192".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::PolicyMismatch(_)
    ));

    // 6. Image tampering
    let mut args = valid_builder_args("proj1", root);
    if let Some(pos) = args.iter().position(|a| a == BUILDER_IMAGE) {
        args[pos] = "ubuntu:latest".to_string();
    }
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::ImageMismatch
    ));

    // 7. Missing required label
    let mut args = valid_builder_args("proj1", root);
    args.retain(|a| a != "--label=oxidex.remote-build=true");
    assert!(matches!(
        validate_build_policy(&args, 16, 28, Some(root)).unwrap_err(),
        DockerAdmissionError::PolicyMismatch(_)
    ));
}

#[test]
fn test_adversarial_docker_admission_volume_count_and_path_tampering() {
    let root = "/mnt/runner-data/remote-build";
    let project = "test-tamper";

    // 1. Missing /src mount (only 2 mounts)
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a.ends_with(":/src")) {
        args.remove(pos); // remove volume spec
        args.remove(pos - 1); // remove -v
    }
    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::ForbiddenMount(_)));

    // 2. Extra unapproved mount (4 mounts)
    let mut args = valid_builder_args(project, root);
    args.insert(1, "-v".to_string());
    args.insert(2, format!("{root}/extra:/extra"));
    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::ForbiddenMount(_)));

    // 3. Mount target pointing to different project
    let mut args = valid_builder_args(project, root);
    if let Some(pos) = args.iter().position(|a| a.ends_with(":/src")) {
        args[pos] = format!("{root}/sources/other_project:/src");
    }
    let err = validate_build_policy(&args, 16, 28, Some(root)).unwrap_err();
    assert!(matches!(err, DockerAdmissionError::ForbiddenMount(_)));
}

#[test]
fn test_adversarial_docker_admission_run_requires_root() {
    // If running as non-root user (euid != 0), run() must return exit code 1
    if unsafe { libc::geteuid() } != 0 {
        let args = vec![
            "run".to_string(),
            "--label=oxidex.remote-build=true".to_string(),
        ];
        let code = host_agent::docker_admission::run(&args).unwrap();
        assert_eq!(
            code, 1,
            "Non-root execution of docker-admission must return exit code 1"
        );
    }
}

#[test]
fn test_adversarial_clean_target_special_entries_socket_and_fifo() {
    use std::os::unix::net::UnixListener;

    let f = CleanTargetFixture::new();
    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();

    // Create a Unix domain socket inside target/
    let sock_path = target.join("test.sock");
    let _listener = UnixListener::bind(&sock_path).expect("create unix socket");

    // Create a symlink to the socket
    symlink("test.sock", target.join("link_to_sock")).unwrap();

    let outcome = clean_target_with_env(&f.work, &f.env).expect("cleanup of socket and link");
    assert!(matches!(outcome, CleanOutcome::TargetRemoved { .. }));
    assert!(
        !target.exists(),
        "target with socket must be safely removed"
    );
}

// ============================================================================
// Task 3: Stress-Test Host Launcher Prepare and Cleanup Symlink Attacks
// ============================================================================

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn test_adversarial_host_launcher_cleanup_ancestor_symlink_attack() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

    let victim = temp.path().join("victim_dir");
    fs::create_dir_all(&victim).unwrap();
    let victim_proj = victim.join("alpha");
    fs::create_dir_all(&victim_proj).unwrap();
    let secret = victim_proj.join("secret.txt");
    fs::write(&secret, "critical secret data").unwrap();

    // Symlink build_root/sources -> victim_dir
    let sources_link = root.join("sources");
    symlink(&victim, &sources_link).unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    let res = host_launcher_run(&["cleanup".to_string(), "alpha".to_string()]);
    println!(
        "host-launcher cleanup with ancestor symlink returned: {:?}",
        res
    );
    println!("Does secret still exist? {}", secret.exists());
    println!("Does victim_proj still exist? {}", victim_proj.exists());

    // Security Invariant: Cleanup MUST NOT follow symlinks in ancestor directories
    // and MUST NOT delete files in external victim directories.
    assert!(
        res.is_err(),
        "SECURITY VULNERABILITY: host-launcher cleanup must fail when sources directory is a symlink, but it succeeded: {:?}",
        res
    );
    assert!(
        secret.exists(),
        "SECURITY VULNERABILITY: host-launcher cleanup traversed ancestor symlink and deleted secret file in external victim directory!"
    );
}

#[test]
fn test_adversarial_host_launcher_prepare_symlink_in_source_path() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

    let external_victim = temp.path().join("external_victim");
    fs::create_dir_all(&external_victim).unwrap();
    let secret = external_victim.join("secret_data.txt");
    fs::write(&secret, "confidential host data").unwrap();

    // 1. Direct symlink in sources/evil-proj -> external_victim
    let sources = root.join("sources");
    fs::create_dir_all(&sources).unwrap();
    let evil_link = sources.join("evil-proj");
    symlink(&external_victim, &evil_link).unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    let res = host_launcher_run(&["prepare".to_string(), "evil-proj".to_string()]);
    assert!(
        res.is_err(),
        "prepare must fail when project source is a symlink"
    );
    assert_eq!(
        fs::read_to_string(&secret).unwrap(),
        "confidential host data",
        "Victim file must be untouched"
    );

    // 2. Ancestor symlink: sources itself is a symlink to external_victim
    fs::remove_file(&evil_link).unwrap();
    fs::remove_dir_all(&sources).unwrap();
    symlink(&external_victim, &sources).unwrap();

    let res_ancestor = host_launcher_run(&["prepare".to_string(), "any-proj".to_string()]);
    assert!(
        res_ancestor.is_err(),
        "prepare must fail when sources directory itself is a symlink"
    );

    // 3. Symlink in targets directory
    fs::remove_file(&sources).unwrap();
    fs::create_dir_all(&sources).unwrap();
    let targets = root.join("targets");
    symlink(&external_victim, &targets).unwrap();

    let res_target = host_launcher_run(&["prepare".to_string(), "any-proj".to_string()]);
    assert!(
        res_target.is_err(),
        "prepare must fail when targets directory is a symlink"
    );

    // 4. Symlink in cargo cache directory
    fs::remove_file(&targets).unwrap();
    fs::create_dir_all(&targets).unwrap();
    let cargo = root.join("cargo");
    symlink(&external_victim, &cargo).unwrap();

    let res_cargo = host_launcher_run(&["prepare".to_string(), "any-proj".to_string()]);
    assert!(
        res_cargo.is_err(),
        "prepare must fail when cargo directory is a symlink"
    );
}

#[test]
fn test_adversarial_host_launcher_cleanup_symlink_to_etc() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();
    let sources = root.join("sources");
    fs::create_dir_all(&sources).unwrap();

    // Create a mock /etc directory containing critical system files
    let mock_etc = temp.path().join("mock_etc");
    fs::create_dir_all(&mock_etc).unwrap();
    let passwd = mock_etc.join("passwd");
    fs::write(
        &passwd,
        "root:x:0:0:root:/root:/bin/bash\nuser:x:1000:1000::/home/user:/bin/bash",
    )
    .unwrap();
    let shadow = mock_etc.join("shadow");
    fs::write(&shadow, "root:$6$encryptedpassword:::0:99999:7:::").unwrap();
    let sudoers = mock_etc.join("sudoers");
    fs::write(&sudoers, "root ALL=(ALL) ALL").unwrap();
    let hosts = mock_etc.join("hosts");
    fs::write(&hosts, "127.0.0.1 localhost").unwrap();

    // Attack: Point sources/etc-attack as a symlink to mock_etc
    let attack_link = sources.join("etc-attack");
    symlink(&mock_etc, &attack_link).unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    // Execute cleanup
    let res = host_launcher_run(&["cleanup".to_string(), "etc-attack".to_string()]);

    // MUST fail with error (O_NOFOLLOW / ELOOP) and MUST NOT delete mock /etc files!
    assert!(
        res.is_err(),
        "cleanup must fail with error when project source is a symlink to /etc"
    );

    // Verify all mock /etc files are completely intact
    assert!(passwd.exists(), "passwd file must not be deleted");
    assert!(shadow.exists(), "shadow file must not be deleted");
    assert!(sudoers.exists(), "sudoers file must not be deleted");
    assert!(hosts.exists(), "hosts file must not be deleted");
    assert_eq!(
        fs::read_to_string(&passwd).unwrap(),
        "root:x:0:0:root:/root:/bin/bash\nuser:x:1000:1000::/home/user:/bin/bash"
    );
    assert_eq!(
        fs::read_to_string(&shadow).unwrap(),
        "root:$6$encryptedpassword:::0:99999:7:::"
    );

    // Verify against real /etc as well (ensuring real /etc is never deleted or modified)
    let real_etc_link = sources.join("real-etc");
    symlink("/etc", &real_etc_link).unwrap();
    let res_real = host_launcher_run(&["cleanup".to_string(), "real-etc".to_string()]);
    assert!(
        res_real.is_err(),
        "cleanup must fail on symlink to real /etc"
    );
    assert!(Path::new("/etc").exists(), "/etc must exist");
}

#[test]
fn test_adversarial_host_launcher_cleanup_nested_symlinks_to_etc_and_root() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();
    let sources = root.join("sources");
    let proj = sources.join("nested-attack");
    fs::create_dir_all(&proj).unwrap();

    // Mock sensitive target
    let mock_sensitive = temp.path().join("sensitive_vault");
    fs::create_dir_all(&mock_sensitive).unwrap();
    let vault_file = mock_sensitive.join("master_key.bin");
    fs::write(&vault_file, "0123456789abcdef").unwrap();

    // Regular project build files
    fs::write(proj.join("main.rs"), "fn main() {}").unwrap();
    fs::write(proj.join("Cargo.lock"), "# lockfile").unwrap();

    // Adversarial nested symlinks inside source
    symlink(&mock_sensitive, proj.join("link_to_vault")).unwrap();
    symlink(&vault_file, proj.join("link_to_key")).unwrap();
    symlink(Path::new("/etc"), proj.join("link_to_etc")).unwrap();
    symlink(Path::new("/"), proj.join("link_to_root")).unwrap();
    symlink(Path::new(".."), proj.join("link_to_parent")).unwrap();

    let sub = proj.join("deep_sub");
    fs::create_dir(&sub).unwrap();
    symlink(&vault_file, sub.join("deep_key_link")).unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    let res = host_launcher_run(&["cleanup".to_string(), "nested-attack".to_string()]).unwrap();
    assert_eq!(res, 0, "cleanup must succeed safely on nested symlinks");

    // Project source should now be empty
    assert!(
        fs::read_dir(&proj).unwrap().next().is_none(),
        "proj directory must be completely emptied"
    );

    // Sensitive external targets must remain completely intact
    assert!(
        mock_sensitive.exists(),
        "Sensitive vault directory must exist"
    );
    assert!(vault_file.exists(), "Master key file must exist");
    assert_eq!(
        fs::read_to_string(&vault_file).unwrap(),
        "0123456789abcdef",
        "Master key content must be unmodified"
    );
    assert!(Path::new("/etc").exists(), "/etc must exist");
}

#[test]
fn test_adversarial_host_launcher_cleanup_circular_symlinks() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();
    let sources = root.join("sources");
    let proj = sources.join("circular-proj");
    fs::create_dir_all(&proj).unwrap();

    // 1. Direct self loop: self -> self
    symlink("self", proj.join("self")).unwrap();

    // 2. Loop to current dir: dot -> .
    symlink(".", proj.join("dot")).unwrap();

    // 3. Mutual cycle: a -> b, b -> a
    symlink("cycle_b", proj.join("cycle_a")).unwrap();
    symlink("cycle_a", proj.join("cycle_b")).unwrap();

    // 4. Nested cycle to parent
    let sub = proj.join("sub");
    fs::create_dir_all(&sub).unwrap();
    symlink("..", sub.join("parent_loop")).unwrap();

    fs::write(proj.join("build.rs"), "fn main() {}").unwrap();
    fs::write(sub.join("sub.rs"), "fn sub() {}").unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    let res = host_launcher_run(&["cleanup".to_string(), "circular-proj".to_string()]).unwrap();
    assert_eq!(res, 0, "cleanup must safely handle circular symlinks");
    assert!(
        fs::read_dir(&proj).unwrap().next().is_none(),
        "circular proj directory must be empty"
    );
}

#[test]
fn test_adversarial_host_launcher_cleanup_active_cache_lock() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();

    let project = "busy-proj";
    let sources = root.join("sources");
    let proj_source = sources.join(project);
    fs::create_dir_all(&proj_source).unwrap();
    let source_file = proj_source.join("code.rs");
    fs::write(&source_file, "pub fn code() {}").unwrap();

    // Hold exclusive cache lock
    let lock_path = lock_root.join(format!("oxidex-cache-active-{project}.lock"));
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .unwrap();

    unsafe {
        libc::flock(
            std::os::unix::io::AsRawFd::as_raw_fd(&lock_file),
            libc::LOCK_EX | libc::LOCK_NB,
        );
    }

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    let code = host_launcher_run(&["cleanup".to_string(), project.to_string()]).unwrap();
    assert_eq!(
        code, 75,
        "Cleanup must return exit code 75 when cache lock is active"
    );

    // Source files must NOT be cleaned while cache lock is active
    assert!(
        source_file.exists(),
        "Source files must remain intact when cleanup is locked out"
    );
}

#[test]
fn test_adversarial_host_launcher_running_container_mount_detection() {
    let source = Path::new("/mnt/runner-data/remote-build/sources/my-active-proj");
    let target = Path::new("/mnt/runner-data/remote-build/targets/my-active-proj");

    // 1. Container actively mounting source
    let json_active_source = serde_json::json!([
        {
            "Id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "Mounts": [
                {
                    "Type": "bind",
                    "Source": "/mnt/runner-data/remote-build/sources/my-active-proj",
                    "Destination": "/src"
                }
            ]
        }
    ])
    .to_string();
    let err_src = verify_mounts_not_active(source, target, &json_active_source).unwrap_err();
    assert!(matches!(err_src, HostLauncherError::SourceMounted(_)));

    // 2. Container actively mounting target
    let json_active_target = serde_json::json!([
        {
            "Id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "Mounts": [
                {
                    "Type": "bind",
                    "Source": "/mnt/runner-data/remote-build/targets/my-active-proj",
                    "Destination": "/target"
                }
            ]
        }
    ])
    .to_string();
    let err_tgt = verify_mounts_not_active(source, target, &json_active_target).unwrap_err();
    assert!(matches!(err_tgt, HostLauncherError::SourceMounted(_)));

    // 3. Multi-container scenario: container 1 is clean, container 2 mounts target
    let json_multi = serde_json::json!([
        {
            "Id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "Mounts": [
                {
                    "Type": "bind",
                    "Source": "/mnt/runner-data/remote-build/sources/other-proj",
                    "Destination": "/src"
                }
            ]
        },
        {
            "Id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "Mounts": [
                {
                    "Type": "bind",
                    "Source": "/mnt/runner-data/remote-build/targets/my-active-proj",
                    "Destination": "/target"
                }
            ]
        }
    ])
    .to_string();
    assert!(matches!(
        verify_mounts_not_active(source, target, &json_multi).unwrap_err(),
        HostLauncherError::SourceMounted(_)
    ));

    // 4. Completely unrelated mounts -> Ok
    let json_clean = serde_json::json!([
        {
            "Id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "Mounts": [
                {
                    "Type": "bind",
                    "Source": "/mnt/runner-data/remote-build/sources/different-proj",
                    "Destination": "/src"
                }
            ]
        }
    ])
    .to_string();
    assert!(verify_mounts_not_active(source, target, &json_clean).is_ok());

    // 5. Container with empty mounts array or null mounts -> Ok
    let json_empty_mounts = serde_json::json!([
        {
            "Id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "Mounts": []
        },
        {
            "Id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        }
    ])
    .to_string();
    assert!(verify_mounts_not_active(source, target, &json_empty_mounts).is_ok());

    // 6. Malformed JSON inputs -> MalformedInventory
    assert!(matches!(
        verify_mounts_not_active(source, target, "not-valid-json").unwrap_err(),
        HostLauncherError::MalformedInventory(_)
    ));
    assert!(matches!(
        verify_mounts_not_active(source, target, "{}").unwrap_err(),
        HostLauncherError::MalformedInventory(_)
    ));
}

#[test]
fn test_adversarial_host_launcher_cleanup_nested_symlink_chain() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    // Case A: Multi-hop ancestor symlink chain:
    // root/sources -> hop1 -> hop2 -> victim_dir
    let victim_dir = temp.path().join("victim_a");
    fs::create_dir_all(&victim_dir).unwrap();
    let victim_proj = victim_dir.join("alpha");
    fs::create_dir_all(&victim_proj).unwrap();
    let secret_file = victim_proj.join("secret_nested.txt");
    fs::write(&secret_file, "SENSITIVE_DATA_NESTED_CHAIN_A").unwrap();

    let hop2 = temp.path().join("hop2_a");
    symlink(&victim_dir, &hop2).unwrap();
    let hop1 = temp.path().join("hop1_a");
    symlink(&hop2, &hop1).unwrap();
    let sources_link = root.join("sources");
    symlink(&hop1, &sources_link).unwrap();

    let res_a = host_launcher_run(&["cleanup".to_string(), "alpha".to_string()]);
    assert!(
        res_a.is_err(),
        "Cleanup MUST fail when sources is a multi-hop symlink chain"
    );
    let err_a = res_a.unwrap_err();
    let launcher_err_a = err_a
        .downcast_ref::<HostLauncherError>()
        .expect("Must downcast to HostLauncherError");
    assert!(
        matches!(launcher_err_a, HostLauncherError::SymlinkedPath(_)),
        "Expected SymlinkedPath, got: {:?}",
        launcher_err_a
    );
    assert!(
        secret_file.exists(),
        "Victim secret file was deleted in multi-hop attack A!"
    );
    assert_eq!(
        fs::read_to_string(&secret_file).unwrap(),
        "SENSITIVE_DATA_NESTED_CHAIN_A"
    );

    // Case B: Multi-hop symlink inside authentic sources:
    // root/sources is a real directory, but root/sources/beta -> hop1_b -> hop2_b -> victim_dir_b
    fs::remove_file(&sources_link).unwrap();
    fs::create_dir_all(&sources_link).unwrap();

    let victim_dir_b = temp.path().join("victim_b");
    fs::create_dir_all(&victim_dir_b).unwrap();
    let victim_proj_b = victim_dir_b.join("beta");
    fs::create_dir_all(&victim_proj_b).unwrap();
    let secret_file_b = victim_proj_b.join("secret_nested_b.txt");
    fs::write(&secret_file_b, "SENSITIVE_DATA_NESTED_CHAIN_B").unwrap();

    let hop2_b = temp.path().join("hop2_b");
    symlink(&victim_proj_b, &hop2_b).unwrap();
    let hop1_b = temp.path().join("hop1_b");
    symlink(&hop2_b, &hop1_b).unwrap();
    let beta_link = sources_link.join("beta");
    symlink(&hop1_b, &beta_link).unwrap();

    let res_b = host_launcher_run(&["cleanup".to_string(), "beta".to_string()]);
    assert!(
        res_b.is_err(),
        "Cleanup MUST fail when project directory is a multi-hop symlink chain"
    );
    assert!(
        secret_file_b.exists(),
        "Victim secret file was deleted in multi-hop attack B!"
    );
    assert_eq!(
        fs::read_to_string(&secret_file_b).unwrap(),
        "SENSITIVE_DATA_NESTED_CHAIN_B"
    );

    // Case C: Multi-hop symlink in targets:
    // root/targets -> hop1_c -> hop2_c -> victim_targets
    let victim_targets = temp.path().join("victim_targets");
    fs::create_dir_all(&victim_targets).unwrap();
    let target_canary = victim_targets.join("canary_targets.txt");
    fs::write(&target_canary, "CANARY_TARGETS_CHAIN_C").unwrap();

    let hop2_c = temp.path().join("hop2_c");
    symlink(&victim_targets, &hop2_c).unwrap();
    let hop1_c = temp.path().join("hop1_c");
    symlink(&hop2_c, &hop1_c).unwrap();
    let targets_link = root.join("targets");
    symlink(&hop1_c, &targets_link).unwrap();

    let res_c = host_launcher_run(&["cleanup".to_string(), "gamma".to_string()]);
    assert!(
        res_c.is_err(),
        "Cleanup MUST fail when targets is a multi-hop symlink chain"
    );
    assert!(
        target_canary.exists(),
        "Victim target canary was deleted in multi-hop attack C!"
    );
    assert_eq!(
        fs::read_to_string(&target_canary).unwrap(),
        "CANARY_TARGETS_CHAIN_C"
    );
}

#[test]
fn test_adversarial_host_launcher_cleanup_relative_symlinks() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    // Case A: Relative symlink pointing to parent's sibling:
    // root/sources is a symlink "../victim_relative_a"
    let victim_rel_a = temp.path().join("victim_relative_a");
    fs::create_dir_all(&victim_rel_a).unwrap();
    let victim_proj_a = victim_rel_a.join("delta");
    fs::create_dir_all(&victim_proj_a).unwrap();
    let secret_rel_a = victim_proj_a.join("secret_relative.txt");
    fs::write(&secret_rel_a, "CONFIDENTIAL_RELATIVE_DATA_A").unwrap();

    let sources_link = root.join("sources");
    symlink("../victim_relative_a", &sources_link).unwrap();

    let res_a = host_launcher_run(&["cleanup".to_string(), "delta".to_string()]);
    assert!(
        res_a.is_err(),
        "Cleanup MUST fail when sources is a relative symlink"
    );
    let err_a = res_a.unwrap_err();
    let launcher_err_a = err_a
        .downcast_ref::<HostLauncherError>()
        .expect("Must downcast to HostLauncherError");
    assert!(
        matches!(launcher_err_a, HostLauncherError::SymlinkedPath(_)),
        "Expected SymlinkedPath, got: {:?}",
        launcher_err_a
    );
    assert!(
        secret_rel_a.exists(),
        "Victim file was deleted via relative symlink A!"
    );
    assert_eq!(
        fs::read_to_string(&secret_rel_a).unwrap(),
        "CONFIDENTIAL_RELATIVE_DATA_A"
    );

    // Case B: Relative symlink traversing multiple levels up (escaping build tree):
    // root/sources is a symlink "../.."
    fs::remove_file(&sources_link).unwrap();
    symlink("../..", &sources_link).unwrap();

    let res_b = host_launcher_run(&["cleanup".to_string(), "delta".to_string()]);
    assert!(
        res_b.is_err(),
        "Cleanup MUST fail when sources is an escaping relative symlink"
    );

    // Case C: Project leaf inside authentic sources is a relative symlink:
    // root/sources is a real directory; root/sources/epsilon -> "../../victim_rel_c"
    fs::remove_file(&sources_link).unwrap();
    fs::create_dir_all(&sources_link).unwrap();

    let victim_rel_c = temp.path().join("victim_relative_c");
    fs::create_dir_all(&victim_rel_c).unwrap();
    let secret_rel_c = victim_rel_c.join("secret_c.txt");
    fs::write(&secret_rel_c, "CONFIDENTIAL_RELATIVE_DATA_C").unwrap();

    let proj_epsilon = sources_link.join("epsilon");
    symlink("../../victim_relative_c", &proj_epsilon).unwrap();

    let res_c = host_launcher_run(&["cleanup".to_string(), "epsilon".to_string()]);
    assert!(
        res_c.is_err(),
        "Cleanup MUST fail when project is a relative symlink"
    );
    assert!(
        secret_rel_c.exists(),
        "Victim file was deleted via project relative symlink C!"
    );
    assert_eq!(
        fs::read_to_string(&secret_rel_c).unwrap(),
        "CONFIDENTIAL_RELATIVE_DATA_C"
    );

    // Case D: Targets directory is a relative symlink:
    // root/targets -> "../victim_relative_targets"
    let targets_link = root.join("targets");
    symlink("../victim_relative_targets", &targets_link).unwrap();

    let res_d = host_launcher_run(&["cleanup".to_string(), "zeta".to_string()]);
    assert!(
        res_d.is_err(),
        "Cleanup MUST fail when targets is a relative symlink"
    );
}

#[test]
fn test_adversarial_host_launcher_cleanup_hard_links_and_mount_isolation() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

    let lock_root = temp.path().join("run");
    fs::create_dir_all(&lock_root).unwrap();
    let lock_root = lock_root.canonicalize().unwrap();

    std::env::set_var("OXIDEX_BUILD_ROOT", root.to_str().unwrap());
    std::env::set_var("OXIDEX_LOCK_ROOT", lock_root.to_str().unwrap());
    std::env::set_var("OXIDEX_ALLOW_UNPRIVILEGED", "1");

    // Scenario A: Directory hard link attack (if supported by OS)
    // POSIX and modern OS kernels (macOS APFS/HFS+, Linux ext4/xfs) forbid directory hard links
    let external_dir = temp.path().join("external_victim_dir");
    fs::create_dir_all(&external_dir).unwrap();
    let dir_link_target = root.join("sources");

    let hard_link_dir_res = fs::hard_link(&external_dir, &dir_link_target);
    match hard_link_dir_res {
        Ok(_) => {
            // If the filesystem permitted a directory hardlink:
            let _res = host_launcher_run(&["cleanup".to_string(), "any_proj".to_string()]);
            // Either cleanup fails or external_dir remains intact
            assert!(external_dir.exists());
        }
        Err(e) => {
            // Expected OS security invariant: directory hard links return EPERM / NotPermitted
            assert!(
                e.raw_os_error() == Some(libc::EPERM)
                    || e.kind() == std::io::ErrorKind::PermissionDenied,
                "Directory hard links must be forbidden by OS: {:?}",
                e
            );
        }
    }

    // Scenario B: File hard link attack inside legitimate project source:
    // Attacker places a hard link inside sources/theta/linked_secret.txt pointing to external_secret.txt
    let sources = root.join("sources");
    fs::create_dir_all(&sources).unwrap();
    let theta_proj = sources.join("theta");
    fs::create_dir_all(&theta_proj).unwrap();

    let external_secret = temp.path().join("external_sensitive_file.txt");
    fs::write(&external_secret, "CRITICAL_EXTERNAL_DATA_DO_NOT_DESTROY").unwrap();

    let linked_secret = theta_proj.join("linked_secret.txt");
    fs::hard_link(&external_secret, &linked_secret).unwrap();

    // Verify initial hard link state (nlink == 2)
    use std::os::unix::fs::MetadataExt;
    let meta_before = fs::metadata(&external_secret).unwrap();
    assert_eq!(meta_before.nlink(), 2, "File should have link count of 2");

    let targets = root.join("targets");
    fs::create_dir_all(&targets).unwrap();

    // Cleanup theta
    let res = host_launcher_run(&["cleanup".to_string(), "theta".to_string()]);
    assert_eq!(
        res.unwrap(),
        0,
        "Cleanup of authentic project directory should succeed"
    );

    // CRITICAL SECURITY INVARIANT:
    // The external secret file MUST STILL EXIST, and its content must be 100% UNTOUCHED!
    // Unlinking the hardlink inside theta only decremented nlink from 2 to 1 without destroying the inode data.
    assert!(
        external_secret.exists(),
        "CRITICAL INVARIANT VIOLATED: external_secret was deleted after hard link cleanup!"
    );
    assert_eq!(
        fs::read_to_string(&external_secret).unwrap(),
        "CRITICAL_EXTERNAL_DATA_DO_NOT_DESTROY",
        "External secret file content was modified!"
    );
    let meta_after = fs::metadata(&external_secret).unwrap();
    assert_eq!(
        meta_after.nlink(),
        1,
        "File link count should have decremented to 1"
    );

    // Scenario C: Foreign filesystem / Mount isolation mimic
    // Test empty_directory_fd directly when st_dev != expected_dev (e.g. cross-device bind mount)
    let foreign_test_dir = temp.path().join("foreign_test_dir");
    fs::create_dir_all(&foreign_test_dir).unwrap();
    let canary_inside_foreign = foreign_test_dir.join("must_not_delete.txt");
    fs::write(&canary_inside_foreign, "NEVER_DELETE_ACROSS_MOUNT_BOUNDARY").unwrap();

    use host_agent::clean_target::{empty_directory_fd, open_dir_nofollow};
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;
    let c_dir = CString::new(foreign_test_dir.to_str().unwrap()).unwrap();
    let fd = open_dir_nofollow(None, &c_dir).unwrap();

    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let stat_res = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
    assert_eq!(stat_res, 0);

    // Spoof expected_dev as a different device ID (e.g. dev + 1)
    let foreign_dev = st.st_dev + 1;
    let mut count = 0;
    let foreign_res = empty_directory_fd(fd.as_raw_fd(), foreign_dev, 0, &mut count);

    // Must return ForeignFilesystem error and NEVER delete any files
    assert!(
        matches!(
            foreign_res.unwrap_err(),
            CleanTargetError::ForeignFilesystem
        ),
        "Must abort with ForeignFilesystem on mount boundary crossing"
    );
    assert!(
        canary_inside_foreign.exists(),
        "Files inside foreign mount must be preserved intact!"
    );
    assert_eq!(
        fs::read_to_string(&canary_inside_foreign).unwrap(),
        "NEVER_DELETE_ACROSS_MOUNT_BOUNDARY"
    );
}
