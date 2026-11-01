//! Repository-owned runner specs (`spot-runner-spec-v1`).
//!
//! A repository the fleet serves can commit `.github/spot-runners.yml` to say which runners its
//! workflows need: the architecture, the labels its jobs select, the size of one runner, how many
//! it wants, whether jobs need a Docker daemon, CPU features, Rust toolchains and Ubuntu packages.
//!
//! The spec is a request, not configuration the Worker trusts blindly:
//! - Only targets the operator already serves (`CONTROLLED_TARGETS`, `ON_DEMAND_TARGETS`) are read.
//! - Every value is checked against a strict grammar and a cap. Unknown keys are refused so a typo
//!   never drops a setting silently.
//! - Repository text never reaches a shell. Toolchain installs are commands the Worker writes from
//!   validated version, component and target names, and packages go through the same quoted apt
//!   install the config repo's container patch uses.
//! - Host-level settings (boot scripts, host files, sysctl, mounts) stay in the operator's config
//!   repository (`swack-tools/spot-runners-config`); a repository can't set them here.
//! - The Worker checks what each pool asks for against what a floor host can give and holds the
//!   launch with a reason when it can't, instead of starting a runner whose jobs would fail.

use crate::patch::{
    ContainerPatch, HardwarePatch, HostPatch, RunnerPatch, PERMITTED_ARM64_MACHINE_TYPES,
    PERMITTED_MACHINE_TYPES, REQUIRED_RUNNER_RAM_GIB, REQUIRED_RUNNER_VCPU, SCHEMA_VERSION,
};
use serde::Serialize;
use serde_json::{json, Map, Value};

pub const SPEC_SCHEMA: &str = "spot-runner-spec-v1";
/// Where a repository keeps its spec, on its default branch.
pub const SPEC_PATH: &str = ".github/spot-runners.yml";
/// Larger specs are refused before parsing.
pub const MAX_SPEC_BYTES: usize = 32 * 1024;

pub const MAX_POOLS: usize = 8;
pub const MAX_LABELS: usize = 16;
pub const MAX_ISA: usize = 8;
pub const MAX_APT_PACKAGES: usize = 64;
pub const MAX_RUST_TOOLCHAINS: usize = 4;
pub const MAX_RUST_COMPONENTS: usize = 8;
pub const MAX_RUST_TARGETS: usize = 16;
pub const MAX_RUNNER_CPUS: u32 = 64;
pub const MAX_RUNNER_MEMORY_GIB: u32 = 256;
pub const MAX_POOL_RUNNERS: u32 = 50;

/// rustup components a spec may ask for.
pub const RUST_COMPONENTS: &[&str] = &[
    "clippy",
    "rustfmt",
    "rust-src",
    "rust-analyzer",
    "llvm-tools",
    "llvm-tools-preview",
    "miri",
    "rust-docs",
];

/// Labels every floor host registers on its own. A repository can't add them, so it can't claim
/// a shape, architecture or zone it won't get.
pub const RESERVED_LABELS: &[&str] = &[
    "self-hosted",
    "linux",
    "x64",
    "arm64",
    "arm",
    "x86_64",
    "aarch64",
    "spot",
    "windows",
    "macos",
];

/// What a floor host gives one runner today: two runners of 7 vCPU and 14 GiB on an x64
/// `c4d-highcpu-16`, inside a container without a Docker daemon.
pub const FLOOR_ARCH: &str = "x64";
pub const FLOOR_RUNNER_CPUS: u32 = REQUIRED_RUNNER_VCPU;
pub const FLOOR_RUNNER_MEMORY_GIB: u32 = REQUIRED_RUNNER_RAM_GIB;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct RustToolchains {
    pub toolchains: Vec<String>,
    pub components: Vec<String>,
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PoolSpec {
    pub name: String,
    /// `x64` or `arm64`.
    pub arch: String,
    pub labels: Vec<String>,
    /// vCPUs one runner needs.
    pub cpus: Option<u32>,
    /// Memory one runner needs.
    pub memory_gib: Option<u32>,
    /// Idle runners to keep online, counted in runners.
    pub min: u32,
    /// Most runners at once, counted in runners.
    pub max: Option<u32>,
    pub needs_docker: bool,
    pub isa: Vec<String>,
    pub rust: Option<RustToolchains>,
    pub apt_packages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RepoRunnerSpec {
    pub pools: Vec<PoolSpec>,
}

// ============================================================================
// Grammar
// ============================================================================

fn is_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_pool_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Debian package names: lowercase letters, digits, `+`, `-`, `.`; at least two characters,
/// starting with a letter or digit. No version pins, no options, no globs.
fn is_apt_package(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'+' | b'-' | b'.')
        })
}

fn all_digits(s: &str, max_len: usize) -> bool {
    !s.is_empty() && s.len() <= max_len && s.bytes().all(|c| c.is_ascii_digit())
}

/// `1.98.0`, `1.98`, `stable`, `beta`, `nightly` or `nightly-2026-10-01`.
fn is_rust_toolchain(s: &str) -> bool {
    if matches!(s, "stable" | "beta" | "nightly") {
        return true;
    }
    if let Some(date) = s.strip_prefix("nightly-") {
        let parts: Vec<&str> = date.split('-').collect();
        let in_range = |p: &str, lo: u32, hi: u32| {
            p.len() == 2 && all_digits(p, 2) && p.parse::<u32>().is_ok_and(|v| v >= lo && v <= hi)
        };
        return parts.len() == 3
            && parts[0].len() == 4
            && all_digits(parts[0], 4)
            && in_range(parts[1], 1, 12)
            && in_range(parts[2], 1, 31);
    }
    let parts: Vec<&str> = s.split('.').collect();
    (parts.len() == 2 || parts.len() == 3)
        && parts[0] == "1"
        && parts[1..].iter().all(|p| all_digits(p, 3))
}

/// Target triples such as `x86_64-unknown-linux-musl` or `wasm32-unknown-unknown`.
fn is_rust_target(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    s.len() <= 64
        && (2..=5).contains(&parts.len())
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.')
                })
        })
}

/// GCE zone names like `us-central1-a`, which floor hosts add as a label.
fn looks_like_zone(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 3
        && parts[0].bytes().all(|c| c.is_ascii_lowercase())
        && parts[1].bytes().any(|c| c.is_ascii_digit())
        && parts[2].len() == 1
        && parts[2].bytes().all(|c| c.is_ascii_lowercase())
}

fn reserved_label_reason(label: &str) -> Option<&'static str> {
    let lower = label.to_ascii_lowercase();
    if RESERVED_LABELS.contains(&lower.as_str()) {
        return Some("is added by the fleet itself");
    }
    if PERMITTED_MACHINE_TYPES.contains(&lower.as_str())
        || PERMITTED_ARM64_MACHINE_TYPES.contains(&lower.as_str())
        || lower == crate::reconciler::DEFAULT_MACHINE_TYPE
    {
        return Some("names a machine type, which the fleet picks");
    }
    if looks_like_zone(&lower) {
        return Some("names a zone, which the fleet picks");
    }
    None
}

// ============================================================================
// Parsing
// ============================================================================

struct Ctx {
    errors: Vec<String>,
}

impl Ctx {
    fn err(&mut self, msg: String) {
        self.errors.push(msg);
    }

    fn check_keys(&mut self, at: &str, obj: &Map<String, Value>, allowed: &[&str]) {
        for k in obj.keys() {
            if !allowed.contains(&k.as_str()) {
                self.err(format!(
                    "{}: unknown key {:?} (allowed: {})",
                    at,
                    k,
                    allowed.join(", ")
                ));
            }
        }
    }

    fn string_list(&mut self, at: &str, v: Option<&Value>, max: usize) -> Vec<String> {
        let Some(v) = v else { return Vec::new() };
        if v.is_null() {
            return Vec::new();
        }
        let Some(items) = v.as_array() else {
            self.err(format!("{}: must be a list", at));
            return Vec::new();
        };
        if items.len() > max {
            self.err(format!(
                "{}: at most {} entries, got {}",
                at,
                max,
                items.len()
            ));
        }
        let mut out: Vec<String> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            match item {
                Value::String(s) => {
                    let s = s.trim().to_string();
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
                Value::Number(n) => self.err(format!(
                    "{}[{}]: {} reads as a number; quote it (\"{}\")",
                    at, i, n, n
                )),
                _ => self.err(format!("{}[{}]: must be a string", at, i)),
            }
        }
        out
    }

    fn uint(&mut self, at: &str, v: Option<&Value>, lo: u32, hi: u32) -> Option<u32> {
        let v = v?;
        if v.is_null() {
            return None;
        }
        match v.as_u64() {
            Some(n) if n >= lo as u64 && n <= hi as u64 => Some(n as u32),
            _ => {
                self.err(format!(
                    "{}: must be a whole number from {} to {}",
                    at, lo, hi
                ));
                None
            }
        }
    }
}

/// Parse and validate a spec's text. Errors name the offending key.
pub fn parse_spec(text: &str) -> Result<RepoRunnerSpec, Vec<String>> {
    if text.len() > MAX_SPEC_BYTES {
        return Err(vec![format!(
            "spec is {} bytes; the limit is {}",
            text.len(),
            MAX_SPEC_BYTES
        )]);
    }
    let val = crate::patch::parse_manifest_to_value_strict(text).map_err(|e| vec![e])?;
    validate_spec_value(&val)
}

/// Validate an already-parsed spec document.
pub fn validate_spec_value(val: &Value) -> Result<RepoRunnerSpec, Vec<String>> {
    let mut cx = Ctx { errors: Vec::new() };
    let Some(root) = val.as_object() else {
        return Err(vec!["spec must be a mapping".to_string()]);
    };
    cx.check_keys("spec", root, &["schema", "pools"]);
    match root.get("schema").and_then(|v| v.as_str()) {
        Some(SPEC_SCHEMA) => {}
        Some(other) => cx.err(format!("schema: expected {}, got {:?}", SPEC_SCHEMA, other)),
        None => cx.err(format!("schema: missing; set it to {}", SPEC_SCHEMA)),
    }

    let mut pools = Vec::new();
    match root.get("pools").and_then(|v| v.as_array()) {
        None => cx.err("pools: must be a list with at least one pool".to_string()),
        Some(list) if list.is_empty() => {
            cx.err("pools: must be a list with at least one pool".to_string())
        }
        Some(list) => {
            if list.len() > MAX_POOLS {
                cx.err(format!(
                    "pools: at most {} pools, got {}",
                    MAX_POOLS,
                    list.len()
                ));
            }
            for (i, p) in list.iter().enumerate().take(MAX_POOLS) {
                if let Some(pool) = parse_pool(&mut cx, i, p) {
                    pools.push(pool);
                }
            }
        }
    }

    let mut names: Vec<&str> = Vec::new();
    for p in &pools {
        if names.contains(&p.name.as_str()) {
            cx.err(format!("pools: name {:?} is used twice", p.name));
        }
        names.push(&p.name);
    }

    if cx.errors.is_empty() {
        Ok(RepoRunnerSpec { pools })
    } else {
        Err(cx.errors)
    }
}

fn parse_pool(cx: &mut Ctx, i: usize, v: &Value) -> Option<PoolSpec> {
    let at = format!("pools[{}]", i);
    let Some(obj) = v.as_object() else {
        cx.err(format!("{}: must be a mapping", at));
        return None;
    };
    cx.check_keys(
        &at,
        obj,
        &[
            "name",
            "arch",
            "labels",
            "cpus",
            "memory_gib",
            "min",
            "max",
            "needs",
            "isa",
            "toolchains",
            "apt_packages",
        ],
    );

    let name = match obj.get("name").and_then(|v| v.as_str()) {
        Some(n) if is_pool_name(n) => n.to_string(),
        Some(n) => {
            cx.err(format!(
                "{}.name: {:?} must be 1-32 lowercase letters, digits or dashes",
                at, n
            ));
            n.to_string()
        }
        None => {
            cx.err(format!("{}.name: missing", at));
            String::new()
        }
    };
    let at = if is_pool_name(&name) {
        format!("pools.{}", name)
    } else {
        at
    };

    let arch = match obj
        .get("arch")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
    {
        None => "x64".to_string(),
        Some(a) if matches!(a.as_str(), "x64" | "x86_64" | "amd64") => "x64".to_string(),
        Some(a) if matches!(a.as_str(), "arm64" | "aarch64") => "arm64".to_string(),
        Some(a) => {
            cx.err(format!("{}.arch: {:?} must be x64 or arm64", at, a));
            a
        }
    };

    let labels = cx.string_list(&format!("{}.labels", at), obj.get("labels"), MAX_LABELS);
    if labels.is_empty() {
        cx.err(format!(
            "{}.labels: list at least one label your workflows put in runs-on",
            at
        ));
    }
    for l in &labels {
        if !is_label(l) {
            cx.err(format!(
                "{}.labels: {:?} must be 1-64 letters, digits, dots, dashes or underscores",
                at, l
            ));
        } else if let Some(why) = reserved_label_reason(l) {
            cx.err(format!("{}.labels: {:?} {}", at, l, why));
        }
    }

    let cpus = cx.uint(&format!("{}.cpus", at), obj.get("cpus"), 1, MAX_RUNNER_CPUS);
    let memory_gib = cx.uint(
        &format!("{}.memory_gib", at),
        obj.get("memory_gib"),
        1,
        MAX_RUNNER_MEMORY_GIB,
    );
    let min = cx
        .uint(&format!("{}.min", at), obj.get("min"), 0, MAX_POOL_RUNNERS)
        .unwrap_or(0);
    let max = cx.uint(&format!("{}.max", at), obj.get("max"), 1, MAX_POOL_RUNNERS);
    if let Some(m) = max {
        if min > m {
            cx.err(format!("{}: min ({}) is larger than max ({})", at, min, m));
        }
    }

    let mut needs_docker = false;
    if let Some(n) = obj.get("needs").filter(|v| !v.is_null()) {
        match n.as_object() {
            Some(nobj) => {
                cx.check_keys(&format!("{}.needs", at), nobj, &["docker"]);
                match nobj.get("docker") {
                    None | Some(Value::Null) => {}
                    Some(Value::Bool(b)) => needs_docker = *b,
                    Some(_) => cx.err(format!("{}.needs.docker: must be true or false", at)),
                }
            }
            None => cx.err(format!("{}.needs: must be a mapping", at)),
        }
    }

    let isa = cx.string_list(&format!("{}.isa", at), obj.get("isa"), MAX_ISA);
    for f in &isa {
        if !is_label(f) {
            cx.err(format!(
                "{}.isa: {:?} is not a valid CPU feature name",
                at, f
            ));
        }
    }

    let mut rust = None;
    if let Some(t) = obj.get("toolchains").filter(|v| !v.is_null()) {
        match t.as_object() {
            Some(tobj) => {
                cx.check_keys(&format!("{}.toolchains", at), tobj, &["rust"]);
                if let Some(r) = tobj.get("rust").filter(|v| !v.is_null()) {
                    rust = parse_rust(cx, &format!("{}.toolchains.rust", at), r);
                }
            }
            None => cx.err(format!("{}.toolchains: must be a mapping", at)),
        }
    }

    let apt_packages = cx.string_list(
        &format!("{}.apt_packages", at),
        obj.get("apt_packages"),
        MAX_APT_PACKAGES,
    );
    for p in &apt_packages {
        if !is_apt_package(p) {
            cx.err(format!(
                "{}.apt_packages: {:?} is not a plain Ubuntu package name",
                at, p
            ));
        }
    }

    Some(PoolSpec {
        name,
        arch,
        labels,
        cpus,
        memory_gib,
        min,
        max,
        needs_docker,
        isa,
        rust,
        apt_packages,
    })
}

fn parse_rust(cx: &mut Ctx, at: &str, v: &Value) -> Option<RustToolchains> {
    let Some(obj) = v.as_object() else {
        cx.err(format!("{}: must be a mapping", at));
        return None;
    };
    cx.check_keys(at, obj, &["toolchains", "components", "targets"]);
    let toolchains = cx.string_list(
        &format!("{}.toolchains", at),
        obj.get("toolchains"),
        MAX_RUST_TOOLCHAINS,
    );
    if toolchains.is_empty() {
        cx.err(format!(
            "{}.toolchains: list at least one toolchain, such as \"1.98.0\"",
            at
        ));
    }
    for t in &toolchains {
        if !is_rust_toolchain(t) {
            cx.err(format!(
                "{}.toolchains: {:?} must be a release like \"1.98.0\", stable, beta, nightly or nightly-YYYY-MM-DD",
                at, t
            ));
        }
    }
    let components = cx.string_list(
        &format!("{}.components", at),
        obj.get("components"),
        MAX_RUST_COMPONENTS,
    );
    for c in &components {
        if !RUST_COMPONENTS.contains(&c.as_str()) {
            cx.err(format!(
                "{}.components: {:?} is not one of {}",
                at,
                c,
                RUST_COMPONENTS.join(", ")
            ));
        }
    }
    // miri ships only with nightly toolchains; on stable the image build would fail.
    if components.iter().any(|c| c == "miri")
        && toolchains.iter().any(|t| !t.starts_with("nightly"))
    {
        cx.err(format!(
            "{}.components: miri needs nightly toolchains only",
            at
        ));
    }
    let targets = cx.string_list(
        &format!("{}.targets", at),
        obj.get("targets"),
        MAX_RUST_TARGETS,
    );
    for t in &targets {
        if !is_rust_target(t) {
            cx.err(format!("{}.targets: {:?} is not a target triple", at, t));
        }
    }
    Some(RustToolchains {
        toolchains,
        components,
        targets,
    })
}

// ============================================================================
// What a floor host can serve
// ============================================================================

/// Reasons a floor host can't serve this pool. Empty means it can.
pub fn floor_host_gaps(pool: &PoolSpec) -> Vec<String> {
    let mut gaps = Vec::new();
    if pool.arch != FLOOR_ARCH {
        gaps.push(format!(
            "needs {} runners; the fleet launches only x64 runners so far",
            pool.arch
        ));
    }
    if pool.needs_docker {
        gaps.push(
            "needs a Docker daemon (job service containers); runner containers don't have one yet"
                .to_string(),
        );
    }
    if let Some(c) = pool.cpus {
        if c > FLOOR_RUNNER_CPUS {
            gaps.push(format!(
                "asks for {} vCPUs per runner; a floor runner has {}",
                c, FLOOR_RUNNER_CPUS
            ));
        }
    }
    if let Some(m) = pool.memory_gib {
        if m > FLOOR_RUNNER_MEMORY_GIB {
            gaps.push(format!(
                "asks for {} GiB per runner; a floor runner has {}",
                m, FLOOR_RUNNER_MEMORY_GIB
            ));
        }
    }
    gaps
}

/// The first pool a floor host can serve, or every pool's reasons when none fits.
pub fn select_floor_pool(spec: &RepoRunnerSpec) -> Result<&PoolSpec, String> {
    let mut why = Vec::new();
    for p in &spec.pools {
        let gaps = floor_host_gaps(p);
        if gaps.is_empty() {
            return Ok(p);
        }
        why.push(format!("pool {} {}", p.name, gaps.join("; ")));
    }
    Err(format!(
        "no pool can run on a floor host: {}",
        why.join(" | ")
    ))
}

// ============================================================================
// Translation into the runner patch the launch path already applies
// ============================================================================

fn quote(s: &str) -> String {
    // Values are validated to a shell-safe alphabet; quoting is a second guard.
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The container build commands for a pool's Rust toolchains. Written by the Worker from
/// validated names; runs as root before the runner user, into the image's read-only shared
/// rustup store, so `rustup which --toolchain <version>` finds them without a download.
pub fn render_toolchain_script(pool: &PoolSpec) -> String {
    let Some(rust) = &pool.rust else {
        return String::new();
    };
    let mut lines = vec![format!(
        "# Rust toolchains from {} (pool {}), written by the fleet",
        SPEC_PATH, pool.name
    )];
    for t in &rust.toolchains {
        let mut cmd = format!(
            "rustup toolchain install {} --profile minimal --no-self-update",
            quote(t)
        );
        for c in &rust.components {
            cmd.push_str(&format!(" --component {}", quote(c)));
        }
        for tg in &rust.targets {
            cmd.push_str(&format!(" --target {}", quote(tg)));
        }
        lines.push(cmd);
        lines.push(format!("rustup run {} rustc --version", quote(t)));
    }
    lines.push("chmod -R a-w \"$RUSTUP_HOME\" \"$CARGO_HOME\"".to_string());
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// A runner patch holding what the pool asks for: its architecture, labels, CPU features,
/// packages and toolchain installs. Host settings stay empty: those belong to the operator.
pub fn pool_to_patch(pool: &PoolSpec, target: &str) -> RunnerPatch {
    RunnerPatch {
        schema: SCHEMA_VERSION.to_string(),
        repository: target.to_string(),
        hardware: HardwarePatch {
            arch: if pool.arch == "arm64" {
                "arm64".to_string()
            } else {
                "x86_64".to_string()
            },
            isa: pool.isa.clone(),
            extra_labels: pool.labels.clone(),
            ..HardwarePatch::default()
        },
        host: HostPatch::default(),
        container: ContainerPatch {
            apt_packages: pool.apt_packages.clone(),
            dockerfile_run: render_toolchain_script(pool),
            ..ContainerPatch::default()
        },
        ..RunnerPatch::default()
    }
}

// ============================================================================
// Reports for the dashboard
// ============================================================================

fn pool_report(pool: &PoolSpec) -> Value {
    let gaps = floor_host_gaps(pool);
    json!({
        "name": pool.name,
        "arch": pool.arch,
        "labels": pool.labels,
        "cpus": pool.cpus,
        "memory_gib": pool.memory_gib,
        "min": pool.min,
        "max": pool.max,
        "needs_docker": pool.needs_docker,
        "isa": pool.isa,
        "rust": pool.rust,
        "apt_packages": pool.apt_packages,
        "floor_host_can_serve": gaps.is_empty(),
        "floor_host_gaps": gaps,
    })
}

/// A report on one spec: which pools a floor host can serve and which one it would launch.
pub fn spec_report(spec: &RepoRunnerSpec) -> Value {
    let floor_pool = select_floor_pool(spec).ok().map(|p| p.name.clone());
    json!({
        "status": "valid",
        "pools": spec.pools.iter().map(pool_report).collect::<Vec<_>>(),
        "floor_pool": floor_pool,
        "errors": Vec::<String>::new(),
    })
}

/// A report on spec text that hasn't been fetched from GitHub (the dashboard's validator).
pub fn validate_text_report(text: &str) -> Value {
    match parse_spec(text) {
        Ok(spec) => spec_report(&spec),
        Err(errors) => json!({
            "status": "invalid",
            "pools": [],
            "floor_pool": Value::Null,
            "errors": errors,
        }),
    }
}

/// `owner/repo` with GitHub's name alphabet, so it is safe in an API path.
pub fn is_repo_target(target: &str) -> bool {
    let Some((owner, repo)) = target.split_once('/') else {
        return false;
    };
    let owner_ok = !owner.is_empty()
        && owner.len() <= 39
        && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let repo_ok = !repo.is_empty()
        && repo.len() <= 100
        && repo != "."
        && repo != ".."
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    owner_ok && repo_ok
}
