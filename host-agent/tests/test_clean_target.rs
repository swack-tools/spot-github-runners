use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use host_agent::clean_target::{clean_target_with_env, run, CleanOutcome, CleanTargetError};

struct TestFixture {
    _temp: TempDir,
    work: PathBuf,
    workspace: PathBuf,
    env: HashMap<String, String>,
}

impl TestFixture {
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

#[test]
fn test_only_current_runner_target_removed() {
    let f = TestFixture::new();
    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("build-output"), "regenerable").unwrap();

    let source = f.workspace.join("source.rs");
    fs::write(&source, "preserve").unwrap();

    let other_runner = f
        .work
        .join("other-runner")
        .join("oxidex")
        .join("oxidex")
        .join("target");
    fs::create_dir_all(&other_runner).unwrap();
    fs::write(other_runner.join("build-output"), "preserve").unwrap();

    let shared = f.work.join("_tool");
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join("cache"), "preserve").unwrap();

    let outcome = clean_target_with_env(&f.work, &f.env).expect("cleanup succeeds");
    assert!(matches!(outcome, CleanOutcome::TargetRemoved { .. }));

    assert!(!target.exists(), "target directory must be deleted");
    assert!(source.exists(), "source.rs in workspace must be preserved");
    assert!(
        other_runner.join("build-output").exists(),
        "other runner target must be preserved"
    );
    assert!(
        shared.join("cache").exists(),
        "shared tool cache must be preserved"
    );
}

#[test]
fn test_example_rust_project_clean_target_switch() {
    let f = TestFixture::new();
    let target = f.workspace.join("target");

    // The documented switch turns cleanup off even when the runner image sets OXIDEX_CLEAN_TARGET=1.
    fs::create_dir_all(&target).unwrap();
    let mut env = f.env.clone();
    env.insert("EXAMPLE_RUST_PROJECT_CLEAN_TARGET".into(), "0".into());
    let outcome = clean_target_with_env(&f.work, &env).unwrap();
    assert_eq!(outcome, CleanOutcome::Disabled);
    assert!(target.exists());

    // It also turns cleanup on by itself.
    let mut env = f.env.clone();
    env.remove("OXIDEX_CLEAN_TARGET");
    env.insert("EXAMPLE_RUST_PROJECT_CLEAN_TARGET".into(), "1".into());
    let outcome = clean_target_with_env(&f.work, &env).unwrap();
    assert!(matches!(outcome, CleanOutcome::TargetRemoved { .. }));
    assert!(!target.exists());

    // With neither switch set, cleanup stays off.
    fs::create_dir_all(&target).unwrap();
    let mut env = f.env.clone();
    env.remove("OXIDEX_CLEAN_TARGET");
    let outcome = clean_target_with_env(&f.work, &env).unwrap();
    assert_eq!(outcome, CleanOutcome::Disabled);
    assert!(target.exists());
}

#[test]
fn test_opt_in_repo_and_workspace_required() {
    let f = TestFixture::new();
    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();

    // 1. Disabled
    let mut env = f.env.clone();
    env.insert("OXIDEX_CLEAN_TARGET".into(), "0".into());
    let outcome = clean_target_with_env(&f.work, &env).unwrap();
    assert_eq!(outcome, CleanOutcome::Disabled);
    assert!(target.exists());

    // 2. Mismatched repo
    let mut env = f.env.clone();
    env.insert("GITHUB_REPOSITORY".into(), "another/repo".into());
    let outcome = clean_target_with_env(&f.work, &env).unwrap();
    assert_eq!(outcome, CleanOutcome::SkippedOtherRepository);
    assert!(target.exists());

    // 3. Mismatched workspace
    let mut env = f.env.clone();
    env.insert("GITHUB_WORKSPACE".into(), f.work.to_string_lossy().into());
    let outcome = clean_target_with_env(&f.work, &env).unwrap();
    assert_eq!(outcome, CleanOutcome::SkippedUnexpectedWorkspace);
    assert!(target.exists());
}

#[test]
fn test_missing_target_is_clean_skip() {
    let f = TestFixture::new();
    let outcome = clean_target_with_env(&f.work, &f.env).unwrap();
    assert!(matches!(outcome, CleanOutcome::TargetAbsent { .. }));
}

#[test]
fn test_target_symlink_refused_without_following() {
    let f = TestFixture::new();
    let external = f.work.parent().unwrap().join("external");
    fs::create_dir_all(&external).unwrap();
    let keep_file = external.join("keep");
    fs::write(&keep_file, "preserve").unwrap();

    let target_symlink = f.workspace.join("target");
    symlink(&external, &target_symlink).unwrap();

    let err = clean_target_with_env(&f.work, &f.env).expect_err("must refuse symlink target");
    assert!(matches!(err, CleanTargetError::NotADirectory));

    assert!(external.exists());
    assert!(keep_file.exists());
}

#[test]
fn test_workspace_symlink_refused() {
    let f = TestFixture::new();
    let other = f.work.parent().unwrap().join("other");
    fs::create_dir_all(other.join("target")).unwrap();

    fs::remove_dir(&f.workspace).unwrap();
    symlink(&other, &f.workspace).unwrap();

    let err = clean_target_with_env(&f.work, &f.env).expect_err("must refuse symlink workspace");
    assert!(matches!(err, CleanTargetError::Io(_)));

    assert!(
        other.join("target").exists(),
        "external target must remain untouched"
    );
}

#[test]
fn test_nested_symlink_unlinked_without_following() {
    let f = TestFixture::new();
    let external = f.work.parent().unwrap().join("external");
    fs::create_dir_all(&external).unwrap();
    let keep_file = external.join("keep");
    fs::write(&keep_file, "preserve").unwrap();

    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();
    symlink(&external, target.join("nested_link")).unwrap();

    let outcome = clean_target_with_env(&f.work, &f.env).expect("cleanup succeeds");
    assert!(matches!(outcome, CleanOutcome::TargetRemoved { .. }));

    assert!(!target.exists());
    assert!(external.exists());
    assert!(keep_file.exists());
}

#[test]
fn test_target_regular_file_refused() {
    let f = TestFixture::new();
    let target = f.workspace.join("target");
    fs::write(&target, "I am a file, not a directory").unwrap();

    let err = clean_target_with_env(&f.work, &f.env).expect_err("must refuse regular file");
    assert!(matches!(err, CleanTargetError::NotADirectory));
    assert!(target.exists());
}

#[test]
fn test_symlink_to_root_fs_inside_target() {
    let f = TestFixture::new();
    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();
    symlink(Path::new("/"), target.join("root_link")).unwrap();

    let outcome = clean_target_with_env(&f.work, &f.env).expect("cleanup succeeds");
    assert!(matches!(outcome, CleanOutcome::TargetRemoved { .. }));

    assert!(!target.exists());
    assert!(
        Path::new("/etc").exists(),
        "root filesystem must be untouched"
    );
}

#[test]
fn test_depth_bound_exceeded() {
    let f = TestFixture::new();
    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();

    let mut curr = target.clone();
    for i in 0..70 {
        curr = curr.join(format!("depth_{i}"));
        fs::create_dir(&curr).unwrap();
    }

    let err = clean_target_with_env(&f.work, &f.env).expect_err("must fail depth bound");
    assert!(matches!(err, CleanTargetError::DepthBoundExceeded));
}

#[test]
fn test_cli_exit_code_discipline() {
    let f = TestFixture::new();
    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();

    // 1. Success exit code 0
    std::env::set_var("OXIDEX_CLEAN_TARGET", "1");
    std::env::set_var("GITHUB_REPOSITORY", "swack-tools/oxidex");
    std::env::set_var(
        "GITHUB_WORKSPACE",
        f.workspace.to_string_lossy().to_string(),
    );
    let code = run(&[f.work.to_string_lossy().to_string()]).unwrap();
    assert_eq!(code, 0);
    assert!(!target.exists());

    // 2. Target absent exit code 0
    let code = run(&[f.work.to_string_lossy().to_string()]).unwrap();
    assert_eq!(code, 0);

    // 3. Symlink target exit code 1
    let external = f.work.parent().unwrap().join("external_dir");
    fs::create_dir_all(&external).unwrap();
    symlink(&external, f.workspace.join("target")).unwrap();
    let code = run(&[f.work.to_string_lossy().to_string()]).unwrap();
    assert_eq!(code, 1);
}
