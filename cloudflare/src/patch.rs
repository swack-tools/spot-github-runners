//! Canonical Runner Patch Specification, Hashing, Hardware Floor Validation & Sizing.
//!
//! Provides 100% bit-for-bit parity with Python's `src/lib/runner_patch.py`:
//! 1. Canonical dictionary construction:
//!    - Schema: `spot-runner-patch-v1`.
//!    - Strips `source_commit` and `source_branch`.
//!    - Deletes optional hardware keys when None: `runners_count`, `runner_vcpu`,
//!      `runner_ram_gib`, `host_overhead_gib`, `queued_jobs_threshold`.
//!    - Preserves `min_vcpu: null` and `min_ram_gib: null`.
//!    - Additive optional fields (`source_image`, `golden_image_family`, `source_instance_template`)
//!      are omitted via `skip_serializing_if = "Option::is_none"`, ensuring 100% identical digest
//!      parity when omitted.
//! 2. Canonical JSON serialization:
//!    - Alphabetically sorted keys at all dictionary levels.
//!    - Compact separators `( ',', ':' )` with zero extra whitespace.
//!    - UTF-8 encoding with ASCII-safe escape sequences matching Python's default `ensure_ascii=True`.
//! 3. Hardware sizing floor validation:
//!    - Minimum 16 vCPU, minimum 32 GiB RAM.
//!    - Exactly 2 runners per host at 7 vCPU and 14 GiB RAM each, with 4 GiB host overhead.
//!    - `queued_jobs_threshold >= 20`.
//!    - Permitted machine types: `c4d-highcpu-16`, `n4d-highcpu-16`, `n4-highcpu-16`, `c2d-highcpu-16`.
//! 4. Multi-repo manifest merging (`defaults.yaml` + `repos/<owner>/<repo>.yaml`).
//! 5. Rollout classification (`noop`, `hot_patch`, `recreate_containers`, `replace_vm`).

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;

pub const SCHEMA_VERSION: &str = "spot-runner-patch-v1";

// Hardware floor sizing constraints
pub const MIN_HOST_VCPU: u32 = 16;
pub const MIN_HOST_RAM_GIB: u32 = 32;
pub const REQUIRED_RUNNERS_COUNT: u32 = 2;
pub const REQUIRED_RUNNER_VCPU: u32 = 7;
pub const REQUIRED_RUNNER_RAM_GIB: u32 = 14;
pub const REQUIRED_HOST_OVERHEAD_GIB: u32 = 4;
pub const MIN_QUEUED_JOBS_THRESHOLD: u32 = 20;

pub const PERMITTED_MACHINE_TYPES: &[&str] = &[
    "c4d-highcpu-16",
    "n4d-highcpu-16",
    "n4-highcpu-16",
    "c2d-highcpu-16",
];

pub const PERMITTED_ARM64_MACHINE_TYPES: &[&str] = &[
    "c4a-highcpu-16",
    "c4a-standard-16",
    "c4a-highcpu-8",
    "c4a-standard-8",
    "n4a-highcpu-16",
    "n4a-standard-16",
    "n4a-highcpu-8",
    "n4a-standard-8",
    "t2a-standard-16",
    "t2a-standard-8",
];

pub const CONTROLLED_TARGETS: &[&str] = &[
    "swack-tools",
    "xbmc4lyfe/newznab-tmux",
    "xbmc4lyfe/rustnzb",
    "xbmc4lyfe/nntp-proxy",
];

pub const ON_DEMAND_TARGETS: &[&str] = &["swack-tools/bt-to-all-devices"];

pub fn is_supported_patch_target(target: &str) -> bool {
    CONTROLLED_TARGETS.contains(&target) || ON_DEMAND_TARGETS.contains(&target)
}

pub const FORBIDDEN_MOUNTS: &[&str] = &[
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/run/gha-admission",
    "/var/lib/gha-admission",
    "/etc",
    "/root",
    "/proc",
    "/sys",
    "/dev",
];

fn default_schema() -> String {
    SCHEMA_VERSION.to_string()
}

fn default_arch() -> String {
    "x86_64".to_string()
}

fn default_file_mode() -> String {
    "0644".to_string()
}

fn default_file_owner() -> String {
    "root:root".to_string()
}

fn deserialize_sysctl_map<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::MapAccess;
    struct SysctlVisitor;
    impl<'de> serde::de::Visitor<'de> for SysctlVisitor {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a map of sysctl keys and string/numeric values")
        }
        fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut map = BTreeMap::new();
            while let Some((key, val)) = access.next_entry::<String, Value>()? {
                let s = match val {
                    Value::String(s) => s,
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => {
                        return Err(serde::de::Error::custom(
                            "sysctl value must be string or number",
                        ))
                    }
                };
                map.insert(key, s);
            }
            Ok(map)
        }
    }
    deserializer.deserialize_map(SysctlVisitor)
}

// ============================================================================
// Data Structures
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatchFileSpec {
    pub path: String,
    pub content: String,
    #[serde(default = "default_file_mode")]
    pub mode: String,
    #[serde(default = "default_file_owner")]
    pub owner: String,
}

pub type FileConfig = PatchFileSpec;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatchMountSpec {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

pub type MountConfig = PatchMountSpec;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwarePatch {
    #[serde(default = "default_arch")]
    pub arch: String,
    #[serde(default)]
    pub isa: Vec<String>,
    #[serde(default)]
    pub extra_labels: Vec<String>,
    #[serde(default)]
    pub min_vcpu: Option<u32>,
    #[serde(default)]
    pub min_ram_gib: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub runners_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub runner_vcpu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub runner_ram_gib: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub host_overhead_gib: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub queued_jobs_threshold: Option<u32>,
    #[serde(default)]
    pub qualify_command: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub golden_image_family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_instance_template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub provisioning_mode: Option<String>,
}

pub type HardwareConfig = HardwarePatch;

impl Default for HardwarePatch {
    fn default() -> Self {
        Self {
            arch: default_arch(),
            isa: Vec::new(),
            extra_labels: Vec::new(),
            min_vcpu: None,
            min_ram_gib: None,
            runners_count: None,
            runner_vcpu: None,
            runner_ram_gib: None,
            host_overhead_gib: None,
            queued_jobs_threshold: None,
            qualify_command: String::new(),
            source_image: None,
            golden_image_family: None,
            source_instance_template: None,
            provisioning_mode: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HostPatch {
    #[serde(default)]
    pub apt_packages: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_sysctl_map")]
    pub sysctl: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<PatchFileSpec>,
    #[serde(default)]
    pub boot_mkdirs: Vec<String>,
    #[serde(default)]
    pub boot_script: String,
}

pub type HostConfig = HostPatch;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ContainerPatch {
    #[serde(default)]
    pub apt_packages: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_sysctl_map")]
    pub sysctl: BTreeMap<String, String>,
    #[serde(default)]
    pub mounts: Vec<PatchMountSpec>,
    #[serde(default)]
    pub files: Vec<PatchFileSpec>,
    #[serde(default)]
    pub dockerfile_run: String,
}

pub type ContainerConfig = ContainerPatch;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerPatch {
    #[serde(default = "default_schema")]
    pub schema: String,
    #[serde(default)]
    pub repository: String,
    #[serde(skip_serializing, default)]
    pub source_commit: Option<String>,
    #[serde(skip_serializing, default)]
    pub source_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub golden_image_family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_instance_template: Option<String>,
    #[serde(default)]
    pub hardware: HardwarePatch,
    #[serde(default)]
    pub host: HostPatch,
    #[serde(default)]
    pub container: ContainerPatch,
}

impl Default for RunnerPatch {
    fn default() -> Self {
        Self {
            schema: default_schema(),
            repository: String::new(),
            source_commit: None,
            source_branch: None,
            source_image: None,
            golden_image_family: None,
            source_instance_template: None,
            hardware: HardwarePatch::default(),
            host: HostPatch::default(),
            container: ContainerPatch::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RolloutClassification {
    pub action: String, // "noop", "hot_patch", "recreate_containers", "replace_vm"
    pub description: String,
}

// ============================================================================
// Canonical JSON & Hashing Implementation
// ============================================================================

/// Write JSON value with exact sorted keys and Python `ensure_ascii=True` string escaping.
pub fn write_canonical_json_value(val: &Value, out: &mut String) {
    match val {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\x08' => out.push_str("\\b"),
                    '\x0c' => out.push_str("\\f"),
                    c if (c as u32) < 0x20 => {
                        let _ = write!(out, "\\u{:04x}", c as u32);
                    }
                    c if (c as u32) <= 0x7e => {
                        out.push(c);
                    }
                    c if (c as u32) <= 0xffff => {
                        let _ = write!(out, "\\u{:04x}", c as u32);
                    }
                    c => {
                        // UTF-16 surrogate pairs for code points > 0xFFFF
                        let code = c as u32 - 0x10000;
                        let high = 0xd800 + ((code >> 10) & 0x3ff);
                        let low = 0xdc00 + (code & 0x3ff);
                        let _ = write!(out, "\\u{:04x}\\u{:04x}", high, low);
                    }
                }
            }
            out.push('"');
        }
        Value::Array(arr) => {
            out.push('[');
            for (i, elem) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_json_value(elem, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let btree: BTreeMap<&str, &Value> = map.iter().map(|(k, v)| (k.as_str(), v)).collect();
            out.push('{');
            for (i, (k, v)) in btree.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_json_value(&Value::String((*k).to_string()), out);
                out.push(':');
                write_canonical_json_value(v, out);
            }
            out.push('}');
        }
    }
}

/// Convert any serde_json Value into canonical compact sorted JSON string.
pub fn to_canonical_json_str(val: &Value) -> String {
    let mut out = String::new();
    write_canonical_json_value(val, &mut out);
    out
}

/// Clean up an arbitrary JSON manifest into canonical form according to spot-runner-patch-v1:
/// - Exclude `source_commit`, `source_branch`.
/// - In `hardware`, remove optional keys if null/missing:
///   `runners_count`, `runner_vcpu`, `runner_ram_gib`, `host_overhead_gib`, `queued_jobs_threshold`,
///   `source_image`, `golden_image_family`, `source_instance_template`.
/// - In `hardware`, preserve `min_vcpu: null`, `min_ram_gib: null`.
/// - At root, remove optional additive keys if null/missing:
///   `source_image`, `golden_image_family`, `source_instance_template`.
pub fn canonicalize_manifest_value(val: &Value) -> Value {
    let mut root = match val {
        Value::Object(m) => m.clone(),
        _ => return val.clone(),
    };

    root.remove("source_commit");
    root.remove("source_branch");

    for opt_root in &[
        "source_image",
        "golden_image_family",
        "source_instance_template",
    ] {
        if let Some(v) = root.get(*opt_root) {
            if v.is_null() {
                root.remove(*opt_root);
            }
        }
    }

    if let Some(Value::Object(mut hw)) = root.remove("hardware") {
        for opt_hw in &[
            "runners_count",
            "runner_vcpu",
            "runner_ram_gib",
            "host_overhead_gib",
            "queued_jobs_threshold",
            "source_image",
            "golden_image_family",
            "source_instance_template",
            "provisioning_mode",
        ] {
            if let Some(v) = hw.get(*opt_hw) {
                if v.is_null() {
                    hw.remove(*opt_hw);
                }
            }
        }
        root.insert("hardware".to_string(), Value::Object(hw));
    }

    Value::Object(root)
}

/// Compute canonical `patch_sha256` digest matching Python's `tests/e2e/fixtures.py:compute_canonical_patch_sha256`.
pub fn compute_canonical_patch_sha256(val: &Value) -> Result<String, String> {
    let cleaned = canonicalize_manifest_value(val);
    let canonical_json = to_canonical_json_str(&cleaned);
    let mut hasher = Sha256::new();
    hasher.update(canonical_json.as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

impl RunnerPatch {
    /// Return the canonical dictionary representation as a `serde_json::Value`.
    pub fn to_canonical_value(&self) -> Value {
        let val = serde_json::to_value(self).unwrap_or(Value::Null);
        canonicalize_manifest_value(&val)
    }

    /// Return the canonical compact sorted JSON string.
    pub fn to_canonical_json(&self) -> String {
        to_canonical_json_str(&self.to_canonical_value())
    }

    /// Compute the exact SHA-256 digest of the canonical JSON string.
    pub fn patch_sha256(&self) -> String {
        let canonical_json = self.to_canonical_json();
        let mut hasher = Sha256::new();
        hasher.update(canonical_json.as_bytes());
        hex::encode(hasher.finalize())
    }

    pub fn is_empty(&self) -> bool {
        self.hardware == HardwarePatch::default()
            && self.host == HostPatch::default()
            && self.container == ContainerPatch::default()
    }

    pub fn has_container_image_changes(&self) -> bool {
        !self.container.apt_packages.is_empty()
            || !self.container.files.is_empty()
            || !self.container.dockerfile_run.trim().is_empty()
    }

    pub fn has_host_changes(&self) -> bool {
        !self.hardware.isa.is_empty()
            || !self.hardware.qualify_command.trim().is_empty()
            || !self.host.apt_packages.is_empty()
            || !self.host.sysctl.is_empty()
            || !self.host.files.is_empty()
            || !self.host.boot_mkdirs.is_empty()
            || !self.host.boot_script.trim().is_empty()
            || !self.container.mounts.is_empty()
    }
}

// ============================================================================
// Hardware Sizing Floor Validation
// ============================================================================

/// Validates hardware sizing floors, permitted machine types, and forbidden mounts.
pub fn validate_patch_manifest(manifest: &Value) -> Result<RunnerPatch, Vec<String>> {
    let mut errors = Vec::new();

    let root = match manifest {
        Value::Object(m) => m,
        _ => return Err(vec!["manifest_must_be_dict".to_string()]),
    };

    // Schema check
    match root.get("schema") {
        Some(Value::String(s)) if s == SCHEMA_VERSION => {}
        Some(Value::String(s)) => {
            errors.push(format!(
                "invalid_schema: expected {}, got {}",
                SCHEMA_VERSION, s
            ));
        }
        _ => {
            errors.push(format!("invalid_schema: expected {}", SCHEMA_VERSION));
        }
    }

    // Hardware checks
    match root.get("hardware") {
        Some(Value::Object(hw)) => {
            let arch_str = hw.get("arch").and_then(|v| v.as_str()).unwrap_or("x86_64");
            let is_arm64 =
                arch_str.eq_ignore_ascii_case("arm64") || arch_str.eq_ignore_ascii_case("aarch64");
            let min_vcpu_floor = if is_arm64 { 8 } else { MIN_HOST_VCPU as u64 };
            let min_ram_floor = if is_arm64 {
                16
            } else {
                MIN_HOST_RAM_GIB as u64
            };

            // Floor checks
            if let Some(v) = hw.get("min_vcpu") {
                if let Some(n) = v.as_u64() {
                    if n < min_vcpu_floor {
                        errors.push(format!("min_vcpu_below_floor: {} < {}", n, min_vcpu_floor));
                    }
                } else if !v.is_null() {
                    errors.push("hardware.min_vcpu must be a positive integer".to_string());
                }
            }

            if let Some(v) = hw.get("min_ram_gib") {
                if let Some(n) = v.as_u64() {
                    if n < min_ram_floor {
                        errors.push(format!(
                            "min_ram_gib_below_floor: {} < {}",
                            n, min_ram_floor
                        ));
                    }
                } else if !v.is_null() {
                    errors.push("hardware.min_ram_gib must be a positive integer".to_string());
                }
            }

            if let Some(v) = hw.get("runners_count") {
                if let Some(n) = v.as_u64() {
                    if is_arm64 {
                        if !(1..=2).contains(&n) {
                            errors.push(format!("runners_count_must_be_1_or_2: got {}", n));
                        }
                    } else if n != REQUIRED_RUNNERS_COUNT as u64 {
                        errors.push(format!("runners_count_must_be_2: got {}", n));
                    }
                } else if !v.is_null() {
                    errors.push("hardware.runners_count must be a positive integer".to_string());
                }
            }

            if let Some(v) = hw.get("runner_vcpu") {
                if let Some(n) = v.as_u64() {
                    if is_arm64 {
                        if !(6..=14).contains(&n) {
                            errors.push(format!("runner_vcpu_must_be_6_to_14: got {}", n));
                        }
                    } else if n != REQUIRED_RUNNER_VCPU as u64 {
                        errors.push(format!("runner_vcpu_must_be_7: got {}", n));
                    }
                } else if !v.is_null() {
                    errors.push("hardware.runner_vcpu must be a positive integer".to_string());
                }
            }

            if let Some(v) = hw.get("runner_ram_gib") {
                if let Some(n) = v.as_u64() {
                    if is_arm64 {
                        if !(12..=28).contains(&n) {
                            errors.push(format!("runner_ram_gib_must_be_12_to_28: got {}", n));
                        }
                    } else if n != REQUIRED_RUNNER_RAM_GIB as u64 {
                        errors.push(format!("runner_ram_gib_must_be_14: got {}", n));
                    }
                } else if !v.is_null() {
                    errors.push("hardware.runner_ram_gib must be an integer >= 2".to_string());
                }
            }

            if let Some(v) = hw.get("host_overhead_gib") {
                if let Some(n) = v.as_u64() {
                    if is_arm64 {
                        if !(2..=4).contains(&n) {
                            errors.push(format!("host_overhead_gib_must_be_2_to_4: got {}", n));
                        }
                    } else if n != REQUIRED_HOST_OVERHEAD_GIB as u64 {
                        errors.push(format!("host_overhead_gib_must_be_4: got {}", n));
                    }
                } else if !v.is_null() {
                    errors.push(
                        "hardware.host_overhead_gib must be a non-negative integer".to_string(),
                    );
                }
            }

            if let Some(v) = hw.get("provisioning_mode") {
                if let Some(s) = v.as_str() {
                    if !matches!(s, "always_on" | "on_demand" | "scale_from_zero") {
                        errors.push(format!("invalid_provisioning_mode: {}", s));
                    }
                } else if !v.is_null() {
                    errors.push("hardware.provisioning_mode must be a string".to_string());
                }
            }

            if let Some(v) = hw.get("queued_jobs_threshold") {
                if let Some(n) = v.as_u64() {
                    if n < MIN_QUEUED_JOBS_THRESHOLD as u64 {
                        errors.push(format!(
                            "queued_jobs_threshold_below_floor: {} < {}",
                            n, MIN_QUEUED_JOBS_THRESHOLD
                        ));
                    }
                } else if !v.is_null() {
                    errors.push(
                        "hardware.queued_jobs_threshold must be a positive integer".to_string(),
                    );
                }
            }

            // Permitted machine types in extra_labels: `type-<machine_type>`
            if let Some(Value::Array(labels)) = hw.get("extra_labels") {
                for lbl in labels {
                    if let Some(s) = lbl.as_str() {
                        if let Some(mtype) = s.strip_prefix("type-") {
                            let allowed = if is_arm64 {
                                PERMITTED_ARM64_MACHINE_TYPES.contains(&mtype)
                            } else {
                                PERMITTED_MACHINE_TYPES.contains(&mtype)
                            };
                            if !allowed {
                                errors.push(format!("unpermitted_machine_type: {}", mtype));
                            }
                        }
                    }
                }
            }
        }
        Some(Value::Null) => {
            errors.push("hardware_must_be_dict".to_string());
        }
        Some(_) => {
            errors.push("hardware_must_be_dict".to_string());
        }
        None => {
            // hardware missing is allowed only if entire manifest has no hardware section
        }
    }

    // Forbidden mounts in container.mounts
    if let Some(Value::Object(cnt)) = root.get("container") {
        if let Some(Value::Array(mounts)) = cnt.get("mounts") {
            for m in mounts {
                if let Some(m_obj) = m.as_object() {
                    if let Some(src) = m_obj.get("source").and_then(|s| s.as_str()) {
                        for forbidden in FORBIDDEN_MOUNTS {
                            if src == *forbidden || src.starts_with(&format!("{}/", forbidden)) {
                                errors.push(format!("forbidden_mount_source: {}", src));
                            }
                        }
                    }
                }
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // Deserialize into typed RunnerPatch
    serde_json::from_value::<RunnerPatch>(manifest.clone()).map_err(|e| vec![e.to_string()])
}

/// Alias for `validate_patch_manifest`.
pub fn validate_patch_dict(manifest: &Value) -> Result<RunnerPatch, Vec<String>> {
    validate_patch_manifest(manifest)
}

/// Validates constraints on an already-typed `RunnerPatch`.
pub fn validate_patch_constraints(patch: &RunnerPatch) -> Result<(), Vec<String>> {
    let val = serde_json::to_value(patch).map_err(|e| vec![e.to_string()])?;
    validate_patch_manifest(&val).map(|_| ())
}

// ============================================================================
// Multi-Repo Manifest Merge & Rollout Classification
// ============================================================================

/// Merges an organization `defaults` manifest with a repository `override_patch`.
pub fn merge_patches(defaults: &RunnerPatch, override_patch: &RunnerPatch) -> RunnerPatch {
    RunnerPatch {
        schema: if override_patch.schema.is_empty() {
            defaults.schema.clone()
        } else {
            override_patch.schema.clone()
        },
        repository: if override_patch.repository.is_empty() {
            defaults.repository.clone()
        } else {
            override_patch.repository.clone()
        },
        source_commit: override_patch
            .source_commit
            .clone()
            .or_else(|| defaults.source_commit.clone()),
        source_branch: override_patch
            .source_branch
            .clone()
            .or_else(|| defaults.source_branch.clone()),
        source_image: override_patch
            .source_image
            .clone()
            .or_else(|| defaults.source_image.clone()),
        golden_image_family: override_patch
            .golden_image_family
            .clone()
            .or_else(|| defaults.golden_image_family.clone()),
        source_instance_template: override_patch
            .source_instance_template
            .clone()
            .or_else(|| defaults.source_instance_template.clone()),
        hardware: HardwarePatch {
            arch: if override_patch.hardware.arch.is_empty() {
                defaults.hardware.arch.clone()
            } else {
                override_patch.hardware.arch.clone()
            },
            isa: {
                let mut list = defaults.hardware.isa.clone();
                for item in &override_patch.hardware.isa {
                    if !list.contains(item) {
                        list.push(item.clone());
                    }
                }
                list
            },
            extra_labels: {
                let mut list = defaults.hardware.extra_labels.clone();
                for item in &override_patch.hardware.extra_labels {
                    if !list.contains(item) {
                        list.push(item.clone());
                    }
                }
                list
            },
            min_vcpu: override_patch
                .hardware
                .min_vcpu
                .or(defaults.hardware.min_vcpu),
            min_ram_gib: override_patch
                .hardware
                .min_ram_gib
                .or(defaults.hardware.min_ram_gib),
            runners_count: override_patch
                .hardware
                .runners_count
                .or(defaults.hardware.runners_count),
            runner_vcpu: override_patch
                .hardware
                .runner_vcpu
                .or(defaults.hardware.runner_vcpu),
            runner_ram_gib: override_patch
                .hardware
                .runner_ram_gib
                .or(defaults.hardware.runner_ram_gib),
            host_overhead_gib: override_patch
                .hardware
                .host_overhead_gib
                .or(defaults.hardware.host_overhead_gib),
            queued_jobs_threshold: override_patch
                .hardware
                .queued_jobs_threshold
                .or(defaults.hardware.queued_jobs_threshold),
            qualify_command: if override_patch.hardware.qualify_command.is_empty() {
                defaults.hardware.qualify_command.clone()
            } else {
                override_patch.hardware.qualify_command.clone()
            },
            source_image: override_patch
                .hardware
                .source_image
                .clone()
                .or_else(|| defaults.hardware.source_image.clone()),
            golden_image_family: override_patch
                .hardware
                .golden_image_family
                .clone()
                .or_else(|| defaults.hardware.golden_image_family.clone()),
            source_instance_template: override_patch
                .hardware
                .source_instance_template
                .clone()
                .or_else(|| defaults.hardware.source_instance_template.clone()),
            provisioning_mode: override_patch
                .hardware
                .provisioning_mode
                .clone()
                .or_else(|| defaults.hardware.provisioning_mode.clone()),
        },
        host: HostPatch {
            apt_packages: {
                let mut list = defaults.host.apt_packages.clone();
                for item in &override_patch.host.apt_packages {
                    if !list.contains(item) {
                        list.push(item.clone());
                    }
                }
                list
            },
            sysctl: {
                let mut map = defaults.host.sysctl.clone();
                map.extend(override_patch.host.sysctl.clone());
                map
            },
            files: {
                let mut map = BTreeMap::new();
                for f in &defaults.host.files {
                    map.insert(f.path.clone(), f.clone());
                }
                for f in &override_patch.host.files {
                    map.insert(f.path.clone(), f.clone());
                }
                map.into_values().collect()
            },
            boot_mkdirs: {
                let mut list = defaults.host.boot_mkdirs.clone();
                for item in &override_patch.host.boot_mkdirs {
                    if !list.contains(item) {
                        list.push(item.clone());
                    }
                }
                list
            },
            boot_script: {
                let s1 = defaults.host.boot_script.trim();
                let s2 = override_patch.host.boot_script.trim();
                match (s1.is_empty(), s2.is_empty()) {
                    (false, false) => format!("{}\n{}", s1, s2),
                    (false, true) => s1.to_string(),
                    (true, false) => s2.to_string(),
                    (true, true) => String::new(),
                }
            },
        },
        container: ContainerPatch {
            apt_packages: {
                let mut list = defaults.container.apt_packages.clone();
                for item in &override_patch.container.apt_packages {
                    if !list.contains(item) {
                        list.push(item.clone());
                    }
                }
                list
            },
            sysctl: {
                let mut map = defaults.container.sysctl.clone();
                map.extend(override_patch.container.sysctl.clone());
                map
            },
            mounts: {
                let mut map = BTreeMap::new();
                for m in &defaults.container.mounts {
                    map.insert(m.target.clone(), m.clone());
                }
                for m in &override_patch.container.mounts {
                    map.insert(m.target.clone(), m.clone());
                }
                map.into_values().collect()
            },
            files: {
                let mut map = BTreeMap::new();
                for f in &defaults.container.files {
                    map.insert(f.path.clone(), f.clone());
                }
                for f in &override_patch.container.files {
                    map.insert(f.path.clone(), f.clone());
                }
                map.into_values().collect()
            },
            dockerfile_run: {
                let s1 = defaults.container.dockerfile_run.trim();
                let s2 = override_patch.container.dockerfile_run.trim();
                match (s1.is_empty(), s2.is_empty()) {
                    (false, false) => format!("{}\n{}", s1, s2),
                    (false, true) => s1.to_string(),
                    (true, false) => s2.to_string(),
                    (true, true) => String::new(),
                }
            },
        },
    }
}

/// Classify rollout from `current` to `target` (returns `"noop"`, `"hot_patch"`, `"recreate_containers"`, or `"replace_vm"`).
pub fn classify_patch_diff(current: &RunnerPatch, target: &RunnerPatch) -> &'static str {
    if current.patch_sha256() == target.patch_sha256() {
        return "noop";
    }

    if current.hardware != target.hardware
        || current.host.boot_script != target.host.boot_script
        || current.host.sysctl != target.host.sysctl
    {
        return "replace_vm";
    }

    if current.container.dockerfile_run != target.container.dockerfile_run
        || current.container.mounts != target.container.mounts
    {
        return "recreate_containers";
    }

    "hot_patch"
}

/// Classify rollout returning structured `RolloutClassification`.
pub fn classify_rollout(
    current: Option<&RunnerPatch>,
    target: &RunnerPatch,
) -> RolloutClassification {
    let current_sha = current.map(|c| c.patch_sha256()).unwrap_or_default();
    let target_sha = target.patch_sha256();

    if current_sha == target_sha {
        return RolloutClassification {
            action: "noop".to_string(),
            description: "Manifests are identical".to_string(),
        };
    }

    if let Some(cur) = current {
        if cur.hardware != target.hardware
            || cur.host.boot_script != target.host.boot_script
            || cur.host.sysctl != target.host.sysctl
        {
            return RolloutClassification {
                action: "replace_vm".to_string(),
                description: "Hardware sizing, ISA, sysctl, or boot script modified".to_string(),
            };
        }

        if cur.container.dockerfile_run != target.container.dockerfile_run
            || cur.container.mounts != target.container.mounts
        {
            return RolloutClassification {
                action: "recreate_containers".to_string(),
                description: "Container Dockerfile or mounts modified".to_string(),
            };
        }
    } else {
        // Fresh host without prior patch
        if target.hardware.arch != "x86_64" || !target.hardware.isa.is_empty() {
            return RolloutClassification {
                action: "replace_vm".to_string(),
                description: "Hardware architecture or ISA modified".to_string(),
            };
        }
        if !target.container.mounts.is_empty() {
            return RolloutClassification {
                action: "recreate_containers".to_string(),
                description: "Container mounts required".to_string(),
            };
        }
    }

    RolloutClassification {
        action: "hot_patch".to_string(),
        description: "In-place package installation or file updates".to_string(),
    }
}

/// Classify rollout from serde_json Values directly (used by HTTP routes).
pub fn classify_rollout_values(current: Option<&Value>, target: &Value) -> RolloutClassification {
    let cur_empty = current
        .map(|c| c.as_object().map(|m| m.is_empty()).unwrap_or(true))
        .unwrap_or(true);
    let tgt_empty = target.as_object().map(|m| m.is_empty()).unwrap_or(true);

    if cur_empty && tgt_empty {
        return RolloutClassification {
            action: "noop".to_string(),
            description: "Manifests are identical".to_string(),
        };
    }

    let current_sha = current
        .and_then(|c| compute_canonical_patch_sha256(c).ok())
        .unwrap_or_default();
    let target_sha = compute_canonical_patch_sha256(target).unwrap_or_default();

    if !current_sha.is_empty() && current_sha == target_sha {
        return RolloutClassification {
            action: "noop".to_string(),
            description: "Manifests are identical".to_string(),
        };
    }

    let empty_obj = Value::Object(serde_json::Map::new());
    let cur_obj = current.and_then(|c| c.as_object());
    let tgt_obj = target.as_object();

    let cur_hw = cur_obj
        .and_then(|o| o.get("hardware"))
        .unwrap_or(&empty_obj);
    let tgt_hw = tgt_obj
        .and_then(|o| o.get("hardware"))
        .unwrap_or(&empty_obj);
    let cur_host = cur_obj.and_then(|o| o.get("host")).unwrap_or(&empty_obj);
    let tgt_host = tgt_obj.and_then(|o| o.get("host")).unwrap_or(&empty_obj);
    let cur_cnt = cur_obj
        .and_then(|o| o.get("container"))
        .unwrap_or(&empty_obj);
    let tgt_cnt = tgt_obj
        .and_then(|o| o.get("container"))
        .unwrap_or(&empty_obj);

    if cur_hw != tgt_hw
        || cur_host.get("boot_script") != tgt_host.get("boot_script")
        || cur_host.get("sysctl") != tgt_host.get("sysctl")
    {
        return RolloutClassification {
            action: "replace_vm".to_string(),
            description: "Hardware sizing, ISA, sysctl, or boot script modified".to_string(),
        };
    }

    if cur_cnt.get("dockerfile_run") != tgt_cnt.get("dockerfile_run")
        || cur_cnt.get("mounts") != tgt_cnt.get("mounts")
    {
        return RolloutClassification {
            action: "recreate_containers".to_string(),
            description: "Container Dockerfile or mounts modified".to_string(),
        };
    }

    RolloutClassification {
        action: "hot_patch".to_string(),
        description: "In-place package installation or file updates".to_string(),
    }
}

// ============================================================================
// Simple YAML / Text Parser
// ============================================================================

/// Strips comments (`# ...`) outside of quoted strings.
fn strip_yaml_comment(line: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    let mut out = String::new();

    for c in line.chars() {
        if c == '\'' && !in_double {
            in_single = !in_single;
            out.push(c);
        } else if c == '"' && !in_single {
            in_double = !in_double;
            out.push(c);
        } else if c == '#' && !in_single && !in_double {
            break;
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_yaml_scalar(s: &str) -> Value {
    let s = s.trim();
    if s.is_empty() {
        return Value::Null;
    }
    if s.eq_ignore_ascii_case("null") || s == "~" {
        return Value::Null;
    }
    if s.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if s.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Value::Number(i.into());
    }
    if let Ok(f) = s.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Value::Number(n);
        }
    }
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        return Value::String(s[1..s.len() - 1].to_string());
    }
    Value::String(s.to_string())
}

/// Lightweight parser for YAML manifests or JSON strings.
pub fn parse_manifest_to_value(text: &str) -> Result<Value, String> {
    let stripped = text.trim();
    if stripped.is_empty() {
        return Err("Manifest is empty".to_string());
    }
    if stripped.starts_with('{') {
        return serde_json::from_str(stripped).map_err(|e| format!("Invalid JSON manifest: {}", e));
    }

    let raw_lines: Vec<&str> = text.lines().collect();
    let n = raw_lines.len();

    let next_nonempty = |start: usize| -> Option<(usize, usize, String)> {
        let mut i = start;
        while i < n {
            let cleaned = strip_yaml_comment(raw_lines[i]);
            let trimmed = cleaned.trim();
            if !trimmed.is_empty() && trimmed != "---" {
                let indent = cleaned.len() - cleaned.trim_start().len();
                return Some((i, indent, trimmed.to_string()));
            }
            i += 1;
        }
        None
    };

    fn parse_block(
        start: usize,
        base_indent: usize,
        raw_lines: &[&str],
        n: usize,
    ) -> Result<(Value, usize), String> {
        let next_nonempty = |start: usize| -> Option<(usize, usize, String)> {
            let mut i = start;
            while i < n {
                let cleaned = strip_yaml_comment(raw_lines[i]);
                let trimmed = cleaned.trim();
                if !trimmed.is_empty() && trimmed != "---" {
                    let indent = cleaned.len() - cleaned.trim_start().len();
                    return Some((i, indent, trimmed.to_string()));
                }
                i += 1;
            }
            None
        };

        let peek = match next_nonempty(start) {
            Some(p) => p,
            None => return Ok((Value::Object(serde_json::Map::new()), start)),
        };

        if peek.1 < base_indent {
            return Ok((Value::Object(serde_json::Map::new()), start));
        }

        // List block
        if peek.2.starts_with("- ") || peek.2 == "-" {
            let mut items = Vec::new();
            let mut i = start;
            while let Some((line_idx, indent, content)) = next_nonempty(i) {
                if indent != base_indent || !(content.starts_with("- ") || content == "-") {
                    break;
                }
                let after_dash = content[1..].trim();
                i = line_idx + 1;
                if after_dash.is_empty() {
                    if let Some(sub) = next_nonempty(i) {
                        if sub.1 > indent {
                            let (val, next_i) = parse_block(i, sub.1, raw_lines, n)?;
                            items.push(val);
                            i = next_i;
                        } else {
                            items.push(Value::Null);
                        }
                    } else {
                        items.push(Value::Null);
                    }
                } else if after_dash.contains(':')
                    && !after_dash.starts_with('"')
                    && !after_dash.starts_with('\'')
                {
                    let mut parts = after_dash.splitn(2, ':');
                    let k = parts.next().unwrap().trim().to_string();
                    let v_str = parts.next().unwrap().trim();
                    let mut item_dict = serde_json::Map::new();

                    if v_str == "|" || v_str == "|-" {
                        let mut block_lines = Vec::new();
                        let mut block_indent = None;
                        while i < n {
                            let raw = raw_lines[i];
                            if raw.trim().is_empty() {
                                block_lines.push(String::new());
                                i += 1;
                                continue;
                            }
                            let cur_indent = raw.len() - raw.trim_start().len();
                            if block_indent.is_none() {
                                if cur_indent <= indent {
                                    break;
                                }
                                block_indent = Some(cur_indent);
                            }
                            if cur_indent < block_indent.unwrap() {
                                break;
                            }
                            block_lines.push(raw[block_indent.unwrap()..].to_string());
                            i += 1;
                        }
                        while block_lines.last().map(|s| s.is_empty()).unwrap_or(false) {
                            block_lines.pop();
                        }
                        let joined = block_lines.join("\n");
                        let final_str = if v_str == "|-" {
                            joined
                        } else if !joined.is_empty() {
                            format!("{}\n", joined)
                        } else {
                            String::new()
                        };
                        item_dict.insert(k, Value::String(final_str));
                    } else if !v_str.is_empty() {
                        item_dict.insert(k, parse_yaml_scalar(v_str));
                    } else if let Some(sub) = next_nonempty(i) {
                        if sub.1 > indent {
                            let (val, next_i) = parse_block(i, sub.1, raw_lines, n)?;
                            item_dict.insert(k, val);
                            i = next_i;
                        } else {
                            item_dict.insert(k, Value::Null);
                        }
                    } else {
                        item_dict.insert(k, Value::Null);
                    }

                    while let Some(sub) = next_nonempty(i) {
                        if sub.1 <= indent {
                            break;
                        }
                        let (sub_map, next_i) = parse_block(i, sub.1, raw_lines, n)?;
                        if let Value::Object(obj) = sub_map {
                            for (sk, sv) in obj {
                                item_dict.insert(sk, sv);
                            }
                            i = next_i;
                        } else {
                            break;
                        }
                    }
                    items.push(Value::Object(item_dict));
                } else {
                    items.push(parse_yaml_scalar(after_dash));
                }
            }
            return Ok((Value::Array(items), i));
        }

        // Mapping block
        let mut mapping = serde_json::Map::new();
        let mut i = start;
        while let Some((line_idx, indent, content)) = next_nonempty(i) {
            if indent < base_indent || indent != base_indent || content.starts_with('-') {
                break;
            }
            if !content.contains(':') {
                return Err(format!("Invalid YAML line {}: {}", line_idx + 1, content));
            }
            let mut parts = content.splitn(2, ':');
            let key = parts.next().unwrap().trim().to_string();
            let val_part = parts.next().unwrap().trim();
            i = line_idx + 1;

            if val_part == "|" || val_part == "|-" {
                let mut block_lines = Vec::new();
                let mut block_indent = None;
                while i < n {
                    let raw = raw_lines[i];
                    if raw.trim().is_empty() {
                        block_lines.push(String::new());
                        i += 1;
                        continue;
                    }
                    let cur_indent = raw.len() - raw.trim_start().len();
                    if block_indent.is_none() {
                        if cur_indent <= indent {
                            break;
                        }
                        block_indent = Some(cur_indent);
                    }
                    if cur_indent < block_indent.unwrap() {
                        break;
                    }
                    block_lines.push(raw[block_indent.unwrap()..].to_string());
                    i += 1;
                }
                while block_lines.last().map(|s| s.is_empty()).unwrap_or(false) {
                    block_lines.pop();
                }
                let joined = block_lines.join("\n");
                let final_str = if val_part == "|-" {
                    joined
                } else if !joined.is_empty() {
                    format!("{}\n", joined)
                } else {
                    String::new()
                };
                mapping.insert(key, Value::String(final_str));
            } else if !val_part.is_empty() {
                mapping.insert(key, parse_yaml_scalar(val_part));
            } else if let Some(sub) = next_nonempty(i) {
                if sub.1 > indent {
                    let (val, next_i) = parse_block(i, sub.1, raw_lines, n)?;
                    mapping.insert(key, val);
                    i = next_i;
                } else {
                    mapping.insert(key, Value::Null);
                }
            } else {
                mapping.insert(key, Value::Null);
            }
        }
        Ok((Value::Object(mapping), i))
    }

    let first = next_nonempty(0).ok_or_else(|| "Manifest text is empty".to_string())?;
    let (res, _) = parse_block(0, first.1, &raw_lines, n)?;
    if !res.is_object() {
        return Err("Manifest root must be a mapping".to_string());
    }
    Ok(res)
}

/// Parse YAML or JSON manifest text into a typed `RunnerPatch`.
pub fn parse_manifest_text(text: &str) -> Result<RunnerPatch, String> {
    let val = parse_manifest_to_value(text)?;
    validate_patch_manifest(&val).map_err(|errs| errs.join("; "))
}

// ============================================================================
// Test Fixture Helpers
// ============================================================================

pub fn make_test_manifest() -> RunnerPatch {
    RunnerPatch {
        schema: SCHEMA_VERSION.to_string(),
        repository: "swack-tools/oxidex".to_string(),
        source_commit: None,
        source_branch: None,
        source_image: None,
        golden_image_family: None,
        source_instance_template: None,
        hardware: HardwarePatch {
            arch: "x86_64".to_string(),
            isa: vec!["avx2".to_string()],
            extra_labels: vec!["type-c4d-highcpu-16".to_string()],
            min_vcpu: Some(16),
            min_ram_gib: Some(32),
            runners_count: Some(2),
            runner_vcpu: Some(7),
            runner_ram_gib: Some(14),
            host_overhead_gib: Some(4),
            queued_jobs_threshold: Some(20),
            qualify_command: String::new(),
            source_image: None,
            golden_image_family: None,
            source_instance_template: None,
            provisioning_mode: None,
        },
        host: HostPatch {
            apt_packages: vec!["git".to_string(), "curl".to_string()],
            sysctl: {
                let mut map = BTreeMap::new();
                map.insert("net.core.somaxconn".to_string(), "4096".to_string());
                map
            },
            files: Vec::new(),
            boot_mkdirs: Vec::new(),
            boot_script: String::new(),
        },
        container: ContainerPatch {
            apt_packages: vec!["build-essential".to_string(), "pkg-config".to_string()],
            sysctl: BTreeMap::new(),
            mounts: Vec::new(),
            files: Vec::new(),
            dockerfile_run: String::new(),
        },
    }
}
