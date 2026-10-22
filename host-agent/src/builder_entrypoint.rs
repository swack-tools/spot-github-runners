use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;
use thiserror::Error;

pub const REQUIRED_PIN: &str = "1.99.0";
pub const REQUIRED_COMMIT: &str = "b940084d7eb6a299eb4bfeb8e34901bc051e7ac4";

#[derive(Debug, Error)]
pub enum EntrypointError {
    #[error("Missing /src/rust-toolchain.toml: {0}")]
    MissingToolchainFile(String),
    #[error("A numeric Rust toolchain pin is required")]
    MissingPin,
    #[error("Dedicated builder requires Rust 1.99.0, observed {0}")]
    WrongPin(String),
    #[error("Dedicated compiler differs from full commit pin")]
    CommitMismatch,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn verify_toolchain_pin_content(toml_text: &str) -> Result<String, EntrypointError> {
    let re = Regex::new(r#"(?m)^channel\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)""#).unwrap();
    let caps = re.captures(toml_text).ok_or(EntrypointError::MissingPin)?;
    let pin = caps[1].to_string();
    if pin != REQUIRED_PIN {
        return Err(EntrypointError::WrongPin(pin));
    }
    Ok(pin)
}

pub fn verify_compiler_commit_hash(rustc_output: &str) -> Result<(), EntrypointError> {
    let needle = format!("commit-hash: {REQUIRED_COMMIT}");
    if rustc_output.lines().any(|line| line.trim() == needle) {
        Ok(())
    } else {
        Err(EntrypointError::CommitMismatch)
    }
}

pub fn setup_builder_env(
    compiler_path: &Path,
    _cargo_path: &Path,
    base_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut env = base_env.clone();
    env.insert(
        "RUSTC".to_string(),
        compiler_path.to_string_lossy().to_string(),
    );

    let parent_str = compiler_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let old_path = env.get("PATH").cloned().unwrap_or_default();
    env.insert("PATH".to_string(), format!("{parent_str}:{old_path}"));

    let rustdoc = compiler_path.with_file_name("rustdoc");
    env.insert("RUSTDOC".to_string(), rustdoc.to_string_lossy().to_string());

    env.insert("RUSTUP_TOOLCHAIN".to_string(), REQUIRED_PIN.to_string());
    env.insert(
        "CARGO_ZIGBUILD_ZIG_PATH".to_string(),
        "/opt/zig/zig".to_string(),
    );
    env.insert(
        "CARGO_ZIGBUILD_CACHE_DIR".to_string(),
        "/cargo/zigbuild-cache".to_string(),
    );
    env.insert(
        "ZIG_GLOBAL_CACHE_DIR".to_string(),
        "/cargo/zig-global-cache".to_string(),
    );
    env.insert(
        "ZIG_LOCAL_CACHE_DIR".to_string(),
        "/target/.zig-cache".to_string(),
    );
    env.insert(
        "OXIDEX_DARWIN_FRAMEWORK_DIR".to_string(),
        "/opt/opensourceDarwinStubs".to_string(),
    );
    env.remove("SDKROOT");
    env
}

pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let toolchain_path = Path::new("/src/rust-toolchain.toml");
    let content = fs::read_to_string(toolchain_path)
        .map_err(|e| EntrypointError::MissingToolchainFile(e.to_string()))?;

    let pin = verify_toolchain_pin_content(&content)?;

    // Locate rustc and cargo via rustup
    let compiler = Command::new("rustup")
        .args(["which", "--toolchain", &pin, "rustc"])
        .output()
        .map_err(|e| format!("Failed to find rustc: {e}"))?;
    let compiler_str = String::from_utf8_lossy(&compiler.stdout).trim().to_string();
    let compiler_path = PathBuf::from(&compiler_str);

    let version_out = Command::new(&compiler_path)
        .arg("-vV")
        .output()
        .map_err(|e| format!("Failed to execute rustc -vV: {e}"))?;
    let version_str = String::from_utf8_lossy(&version_out.stdout);
    verify_compiler_commit_hash(&version_str)?;

    let cargo_out = Command::new("rustup")
        .args(["which", "--toolchain", &pin, "cargo"])
        .output()
        .map_err(|e| format!("Failed to find cargo: {e}"))?;
    let cargo_str = String::from_utf8_lossy(&cargo_out.stdout)
        .trim()
        .to_string();
    let cargo_path = PathBuf::from(&cargo_str);

    let base_env: HashMap<String, String> = std::env::vars().collect();
    let env = setup_builder_env(&compiler_path, &cargo_path, &base_env);

    println!("{}", version_str);
    let cargo_ver = Command::new(&cargo_path).arg("-V").output()?;
    println!("{}", String::from_utf8_lossy(&cargo_ver.stdout));

    let mut command: Vec<String> = if !args.is_empty() {
        args.to_vec()
    } else {
        vec![
            "cargo".to_string(),
            "build".to_string(),
            "--release".to_string(),
            "--locked".to_string(),
            "--bin".to_string(),
            "oxidex".to_string(),
        ]
    };

    if command[0] == "cargo" {
        command[0] = cargo_str;
    }

    use std::os::unix::process::CommandExt;
    let err = Command::new(&command[0])
        .args(&command[1..])
        .envs(&env)
        .exec();

    Err(Box::new(err))
}
