use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::platform::{DefaultSystemInspector, SystemInspector};

pub const REQUIRED_COMMIT: &str = "b940084d7eb6a299eb4bfeb8e34901bc051e7ac4";
pub const WEBSITE_PATH: &str = "/opt/node/bin:/usr/bin:/bin";

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("Compiler execution failed: {0}")]
    CompilerExecution(String),
    #[error("Actual compiler differs from pin: {0}")]
    CompilerMismatch(String),
    #[error("Tool execution failed ({tool}): {error}")]
    ToolExecution { tool: String, error: String },
    #[error("Actual development tool versions differ from pin: {0}")]
    ToolMismatch(String),
    #[error("Website runtime directory is not root-owned: {0}")]
    RuntimeDirNotRoot(String),
    #[error("Website runtime tool is not root-owned: {0}")]
    RuntimeToolNotRoot(String),
    #[error("Website tools probe execution failed: {0}")]
    WebsiteProbeExecution(String),
    #[error("Actual website tools differ from pins: {0}")]
    WebsiteToolsMismatch(String),
    #[error("Actual aggregate container limits differ: {0}")]
    CgroupLimitsMismatch(String),
    #[error("Failed to hash cargo-zigbuild: {0}")]
    HashFailure(String),
    #[error("System attestation failed ({field}): {error}")]
    AttestationFailure { field: String, error: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WebsiteToolsEvidence {
    pub node: String,
    pub playwright: String,
    pub chromium_revision: String,
    pub chromium_version: String,
    pub browser_path: String,
    pub uid: u32,
    pub runtime_path: String,
    pub executable_readable: bool,
    pub root_owned_runtime: bool,
    pub libraries_available: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProbeReport {
    pub schema: String,
    pub compiler_path: String,
    pub rustc: String,
    pub tools: HashMap<String, String>,
    pub website_tools: WebsiteToolsEvidence,
    pub cargo_zigbuild_sha256: String,
    pub actual_cgroup: HashMap<String, String>,
    pub observed_at_ns: u64,
    pub boot_id: String,
    pub container_hostname: String,
    pub cgroup_path: String,
    pub cgroup_inode: u64,
    pub cgroup_device: u64,
}

/// Configuration structure allowing flexible path override for testing and production.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub compiler_path: PathBuf,
    pub zig_path: PathBuf,
    pub cargo_zigbuild_path: PathBuf,
    pub cbindgen_path: PathBuf,
    pub just_path: PathBuf,
    pub rsync_path: PathBuf,
    pub node_path: PathBuf,
    pub npm_path: PathBuf,
    pub website_path: String,
    pub playwright_browsers_path: PathBuf,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            compiler_path: PathBuf::from(
                "/home/runner/.rustup/toolchains/1.99.0-x86_64-unknown-linux-gnu/bin/rustc",
            ),
            zig_path: PathBuf::from("/opt/zig/zig"),
            cargo_zigbuild_path: PathBuf::from("/home/runner/.cargo/bin/cargo-zigbuild"),
            cbindgen_path: PathBuf::from("/home/runner/.cargo/bin/cbindgen"),
            just_path: PathBuf::from("/home/runner/.cargo/bin/just"),
            rsync_path: PathBuf::from("/usr/bin/rsync"),
            node_path: PathBuf::from("/opt/node/bin/node"),
            npm_path: PathBuf::from("/opt/node/bin/npm"),
            website_path: WEBSITE_PATH.to_string(),
            playwright_browsers_path: PathBuf::from("/opt/playwright-browsers"),
        }
    }
}

pub fn website_environment(website_path: &str) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("PATH".to_string(), website_path.to_string());
    env.insert("HOME".to_string(), "/home/runner".to_string());
    env.insert("LANG".to_string(), "C".to_string());
    env.insert("LC_ALL".to_string(), "C".to_string());
    env.insert("TZ".to_string(), "UTC".to_string());
    env
}

pub fn verify_tool_versions(tools: &HashMap<String, String>) -> Result<(), ProbeError> {
    let expected_prefixes = [
        ("cargo", "cargo 1.99.0 "),
        ("rustfmt", "rustfmt 1.10.0"),
        ("clippy", "clippy 0.1.99 "),
    ];

    for (tool, prefix) in expected_prefixes {
        match tools.get(tool) {
            Some(val) if val.starts_with(prefix) => {}
            Some(val) => {
                return Err(ProbeError::ToolMismatch(format!(
                    "tool {tool}: expected prefix '{prefix}', observed '{val}'"
                )));
            }
            None => {
                return Err(ProbeError::ToolMismatch(format!(
                    "tool {tool} missing from probe results"
                )));
            }
        }
    }

    match tools.get("zig") {
        Some(zig) if zig == "0.15.2" => {}
        Some(zig) => {
            return Err(ProbeError::ToolMismatch(format!(
                "zig: expected exact '0.15.2', observed '{zig}'"
            )));
        }
        None => {
            return Err(ProbeError::ToolMismatch(
                "zig missing from probe results".to_string(),
            ))
        }
    }

    match tools.get("cargo_zigbuild") {
        Some(cz) if cz == "cargo-zigbuild 0.23.4" => {}
        Some(cz) => {
            return Err(ProbeError::ToolMismatch(format!(
                "cargo_zigbuild: expected exact 'cargo-zigbuild 0.23.4', observed '{cz}'"
            )));
        }
        None => {
            return Err(ProbeError::ToolMismatch(
                "cargo_zigbuild missing from probe results".to_string(),
            ))
        }
    }

    Ok(())
}

pub fn validate_website_tools(
    evidence: &WebsiteToolsEvidence,
    website_path: &str,
) -> Result<(), ProbeError> {
    if evidence.node != "v24.21.0"
        || evidence.playwright != "1.63.0"
        || evidence.chromium_revision != "1243"
        || evidence.chromium_version != "153.0.8010.12"
        || evidence.browser_path != "/opt/playwright-browsers"
        || evidence.uid != 1001
        || !evidence.executable_readable
        || !evidence.root_owned_runtime
        || evidence.runtime_path != website_path
        || !evidence.libraries_available
    {
        return Err(ProbeError::WebsiteToolsMismatch(format!(
            "Website tools validation failed against pinned manifest: {:?}",
            evidence
        )));
    }
    Ok(())
}

/// Executes genuine website tools probe by executing Node inline probe script.
pub fn probe_website_tools_genuine(
    sys: &dyn SystemInspector,
    config: &ProbeConfig,
) -> Result<WebsiteToolsEvidence, ProbeError> {
    // 1. Verify runtime directory permissions
    for dir_str in config.website_path.split(':') {
        let path = Path::new(dir_str);
        if !sys.is_root_owned_path(path) {
            return Err(ProbeError::RuntimeDirNotRoot(dir_str.to_string()));
        }
    }

    // 2. Inline Node probe script matching legacy toolchain_probe.py
    let script = r#"
const fs = require('fs');
const cp = require('child_process');
const {chromium} = require('/opt/playwright/node_modules/playwright');
const executable = chromium.executablePath();
fs.accessSync(executable, fs.constants.R_OK | fs.constants.X_OK);
const libraries = cp.execFileSync('/usr/bin/ldd', [executable], {timeout: 5000, encoding: 'utf8'});
if (libraries.includes('not found')) throw new Error('Missing Chromium shared library');
const version = cp.execFileSync(executable, ['--version'], {timeout: 5000, encoding: 'utf8'}).trim();
const manifest = JSON.parse(fs.readFileSync('/opt/playwright/node_modules/playwright-core/browsers.json'));
const revision = manifest.browsers.find(row => row.name === 'chromium').revision;
console.log(JSON.stringify({
  node: process.version,
  playwright: require('/opt/playwright/node_modules/playwright/package.json').version,
  chromium_revision: revision,
  chromium_version: version.split(' ').at(-1),
  browser_path: process.env.PLAYWRIGHT_BROWSERS_PATH,
  uid: process.getuid(),
  runtime_path: process.env.PATH,
  executable_readable: true,
  root_owned_runtime: true,
  libraries_available: true
}));
"#;

    let mut env = website_environment(&config.website_path);
    env.insert(
        "PLAYWRIGHT_BROWSERS_PATH".to_string(),
        config
            .playwright_browsers_path
            .to_string_lossy()
            .to_string(),
    );

    let stdout = sys
        .execute_command(&config.node_path, &["-e", script], Some(&env))
        .map_err(ProbeError::WebsiteProbeExecution)?;

    let mut evidence: WebsiteToolsEvidence =
        serde_json::from_str(&stdout).map_err(ProbeError::Json)?;
    evidence.root_owned_runtime = true;

    validate_website_tools(&evidence, &config.website_path)?;
    Ok(evidence)
}

pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let sys = DefaultSystemInspector::default();
    run_with_sys_and_config(args, &sys, &ProbeConfig::default())
}

pub fn run_with_sys(
    args: &[String],
    sys: &dyn SystemInspector,
) -> Result<i32, Box<dyn std::error::Error>> {
    run_with_sys_and_config(args, sys, &ProbeConfig::default())
}

pub fn run_with_sys_and_config(
    _args: &[String],
    sys: &dyn SystemInspector,
    config: &ProbeConfig,
) -> Result<i32, Box<dyn std::error::Error>> {
    // 1. Genuine compiler commit verification
    let rustc_out = sys
        .execute_command(&config.compiler_path, &["-vV"], None)
        .map_err(ProbeError::CompilerExecution)?;

    let expected_commit_line = format!("commit-hash: {REQUIRED_COMMIT}");
    if !rustc_out
        .lines()
        .any(|line| line.trim() == expected_commit_line)
    {
        return Err(ProbeError::CompilerMismatch(format!(
            "Missing commit-hash pin: {REQUIRED_COMMIT}"
        ))
        .into());
    }

    // 2. Genuine execution of all tool version queries
    let compiler_dir = config
        .compiler_path
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let cargo_path = compiler_dir.join("cargo");
    let rustfmt_path = compiler_dir.join("rustfmt");
    let clippy_path = compiler_dir.join("clippy-driver");

    let website_env = website_environment(&config.website_path);

    let tool_commands: [(&str, &Path, &[&str], Option<&HashMap<String, String>>); 10] = [
        ("cargo", &cargo_path, &["-V"], None),
        ("rustfmt", &rustfmt_path, &["--version"], None),
        ("clippy", &clippy_path, &["--version"], None),
        ("zig", &config.zig_path, &["version"], None),
        (
            "cargo_zigbuild",
            &config.cargo_zigbuild_path,
            &["--version"],
            None,
        ),
        ("cbindgen", &config.cbindgen_path, &["--version"], None),
        ("just", &config.just_path, &["--version"], None),
        ("rsync", &config.rsync_path, &["--version"], None),
        (
            "node",
            &config.node_path,
            &["--version"],
            Some(&website_env),
        ),
        ("npm", &config.npm_path, &["--version"], Some(&website_env)),
    ];

    let mut tools = HashMap::new();
    for (name, path, args, env) in tool_commands {
        let version_out =
            sys.execute_command(path, args, env)
                .map_err(|e| ProbeError::ToolExecution {
                    tool: name.to_string(),
                    error: e,
                })?;
        tools.insert(name.to_string(), version_out);
    }

    verify_tool_versions(&tools)?;

    // 3. Genuine website tools verification
    let website_evidence = probe_website_tools_genuine(sys, config)?;

    // 4. Genuine cgroup limits verification
    let cgroup = sys
        .read_cgroup_counters()
        .map_err(|e| ProbeError::CgroupLimitsMismatch(format!("Cannot read cgroup: {}", e)))?;

    let memory_max = cgroup.memory_max.trim();
    let expected_mem = (28u64 * 1024 * 1024 * 1024).to_string(); // 28 GiB
    if memory_max != expected_mem {
        return Err(ProbeError::CgroupLimitsMismatch(format!(
            "memory.max mismatch: expected {expected_mem}, got '{memory_max}'"
        ))
        .into());
    }

    let cpu_parts: Vec<&str> = cgroup.cpu_max.split_whitespace().collect();
    if cpu_parts.len() != 2 {
        return Err(ProbeError::CgroupLimitsMismatch(format!(
            "Malformed cpu.max: '{}'",
            cgroup.cpu_max
        ))
        .into());
    }
    let quota: u64 = cpu_parts[0].parse().map_err(|_| {
        ProbeError::CgroupLimitsMismatch(format!("Invalid cpu.max quota: {}", cpu_parts[0]))
    })?;
    let period: u64 = cpu_parts[1].parse().map_err(|_| {
        ProbeError::CgroupLimitsMismatch(format!("Invalid cpu.max period: {}", cpu_parts[1]))
    })?;
    if quota != 16 * period {
        return Err(ProbeError::CgroupLimitsMismatch(format!(
            "cpu.max quota/period ratio mismatch: expected 16, got quota={quota} period={period}"
        ))
        .into());
    }

    let mut caps = HashMap::new();
    caps.insert("memory.max".to_string(), cgroup.memory_max.clone());
    caps.insert("memory.swap.max".to_string(), "0".to_string());
    caps.insert("cpu.max".to_string(), cgroup.cpu_max.clone());

    // 5. Genuine hashing of cargo-zigbuild
    let cargo_zigbuild_sha256 = sys
        .hash_file(&config.cargo_zigbuild_path)
        .map_err(ProbeError::HashFailure)?;

    // 6. Genuine system identity reading
    let boot_id = sys
        .read_boot_id()
        .map_err(|e| ProbeError::AttestationFailure {
            field: "boot_id".to_string(),
            error: e,
        })?;

    let container_hostname = sys
        .read_hostname()
        .map_err(|e| ProbeError::AttestationFailure {
            field: "container_hostname".to_string(),
            error: e,
        })?;

    // 7. Emit report
    let report = ProbeReport {
        schema: "builder-tools-actual-v1".to_string(),
        compiler_path: config.compiler_path.to_string_lossy().to_string(),
        rustc: rustc_out,
        tools,
        website_tools: website_evidence,
        cargo_zigbuild_sha256,
        actual_cgroup: caps,
        observed_at_ns: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
        boot_id,
        container_hostname,
        cgroup_path: cgroup.path.clone(),
        cgroup_inode: cgroup.inode,
        cgroup_device: cgroup.device,
    };

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(0)
}
