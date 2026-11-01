//! Dedicated Rust builders started from the dashboard.
//!
//! The owner picks a zone and pastes an Ed25519 public key. The Worker renders
//! `cloud-init/builder-startup.sh` with everything a stock Debian 13 VM needs to finish its bootstrap on its own:
//! the pinned `host-agent` release, the builder image files, and the approved uploader mapping that lets the
//! owner's key reach the root-owned build launcher over SSH. Nothing else on the host changes: the launcher,
//! Docker admission, and the UID 1001 container limits are the same as for a hand-rendered builder.

use crate::gcp::{BuilderVmConfig, GceInsertInstanceRequest, GceMetadataItem};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sha2::{Digest, Sha256};

/// The only reviewed builder shape: 16 vCPU and 32 GiB, so the 16-CPU, 28 GiB container fits with host reserve.
pub const BUILDER_MACHINE_TYPE: &str = "n4-highcpu-16";
/// UID of the SSH account that uploads source (at least 1000 and never the container's 1001).
pub const UPLOADER_UID: u32 = 2000;
/// Live builders the dashboard will start before it refuses another launch.
pub const MAX_LIVE_BUILDERS: usize = 4;
/// Compute Engine limit for one metadata value, which holds the whole startup script.
pub const METADATA_VALUE_LIMIT_BYTES: usize = 256 * 1024;
/// Instance label that names the SSH account, so the dashboard can show how to connect.
pub const SSH_USER_LABEL: &str = "spot-builder-ssh-user";
/// Instance label set on builders the dashboard launched.
pub const LAUNCHED_BY_LABEL: &str = "spot-launched-by";
/// Public guide for pointing Cargo on a workstation at a builder.
pub const CLIENT_SETUP_DOCS_URL: &str =
    "https://swack-tools.github.io/spot-github-runners/client-workstation-setup/";
/// Where the docs site publishes `docs/cargo-remote.sh`, the workstation helper the dashboard's install line fetches.
pub const CARGO_REMOTE_URL: &str =
    "https://swack-tools.github.io/spot-github-runners/cargo-remote.sh";

/// The one line an administrator pastes on a workstation to install `cargo-remote` for this deployment's
/// project. Returns `None` for anything that isn't a plain GCP project ID, so nothing else reaches a shell.
pub fn install_command(project: &str) -> Option<String> {
    let ok = (6..=30).contains(&project.len())
        && project.starts_with(|c: char| c.is_ascii_lowercase())
        && !project.ends_with('-')
        && project
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then(|| format!("curl -fsSL {CARGO_REMOTE_URL} | bash -s -- install --project {project}"))
}

const BUILDER_DOCKERFILE: &str = include_str!("../../builder/standalone/Dockerfile");
const WEBSITE_PACKAGE_JSON: &str = include_str!("../../builder/standalone/website-package.json");
const WEBSITE_PACKAGE_LOCK: &str =
    include_str!("../../builder/standalone/website-package-lock.json");

/// Accounts that already exist on Debian or that the builder bootstrap creates.
const RESERVED_USERNAMES: &[&str] = &[
    "root",
    "daemon",
    "bin",
    "sys",
    "sync",
    "games",
    "man",
    "lp",
    "mail",
    "news",
    "uucp",
    "proxy",
    "www-data",
    "backup",
    "list",
    "irc",
    "_apt",
    "nobody",
    "systemd-network",
    "systemd-timesync",
    "messagebus",
    "sshd",
    "polkitd",
    "docker",
    "oxidex-build",
    "admin",
    "debian",
];

/// Checks a Compute Engine zone name such as `us-central1-a`.
pub fn validate_zone(zone: &str) -> Result<(), String> {
    let parts: Vec<&str> = zone.split('-').collect();
    let ok = zone.len() <= 32
        && parts.len() == 3
        && !parts[0].is_empty()
        && parts[0].bytes().all(|b| b.is_ascii_lowercase())
        && parts[1].len() >= 2
        && parts[1].bytes().any(|b| b.is_ascii_digit())
        && parts[1]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && parts[1].as_bytes()[0].is_ascii_lowercase()
        && parts[1].as_bytes()[parts[1].len() - 1].is_ascii_digit()
        && parts[2].len() == 1
        && parts[2].as_bytes()[0].is_ascii_lowercase();
    if ok {
        Ok(())
    } else {
        Err(format!(
            "'{zone}' is not a Compute Engine zone such as us-central1-a"
        ))
    }
}

/// Accepts one OpenSSH Ed25519 public key line and returns it as `ssh-ed25519 <base64>`.
///
/// The comment is dropped, so nothing the person typed besides the key itself reaches the startup script.
pub fn normalize_ed25519_public_key(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.contains("PRIVATE KEY") {
        return Err(
            "That is a private key. Paste the public key instead: the contents of the .pub file."
                .to_string(),
        );
    }
    if trimmed.is_empty() {
        return Err(
            "Paste an SSH public key, for example the contents of ~/.ssh/id_ed25519.pub"
                .to_string(),
        );
    }
    if trimmed.lines().count() != 1 {
        return Err("Paste exactly one public key line".to_string());
    }
    let mut parts = trimmed.split_whitespace();
    let key_type = parts.next().unwrap_or_default();
    let blob = parts.next().unwrap_or_default();
    if key_type != "ssh-ed25519" {
        return Err(format!(
            "Builders accept only Ed25519 keys (ssh-ed25519), not '{key_type}'. Create one with: ssh-keygen -t ed25519"
        ));
    }
    if blob.len() > 128
        || !blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    {
        return Err("The key data is not valid base64".to_string());
    }
    let decoded = STANDARD
        .decode(blob)
        .map_err(|_| "The key data is not valid base64".to_string())?;
    // Wire format: string "ssh-ed25519", then a 32-byte public key, each with a 4-byte big-endian length.
    let mut expected_prefix = Vec::with_capacity(19);
    expected_prefix.extend_from_slice(&11u32.to_be_bytes());
    expected_prefix.extend_from_slice(b"ssh-ed25519");
    expected_prefix.extend_from_slice(&32u32.to_be_bytes());
    if decoded.len() != 51 || !decoded.starts_with(&expected_prefix) {
        return Err("The key data is not an Ed25519 public key".to_string());
    }
    Ok(format!("ssh-ed25519 {blob}"))
}

/// Linux account name for a GitHub login: lower case, `[a-z0-9_-]`, at most 32 characters, never a system account.
pub fn uploader_username(login: &str) -> String {
    let mut name: String = login
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if !name
        .chars()
        .next()
        .map(|c| c.is_ascii_lowercase() || c == '_')
        .unwrap_or(false)
    {
        name.insert(0, 'u');
    }
    name.truncate(28);
    while name.ends_with('-') {
        name.pop();
    }
    if name.is_empty() || RESERVED_USERNAMES.contains(&name.as_str()) {
        name = format!("{}-dev", if name.is_empty() { "builder" } else { &name });
    }
    name
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// The approved uploader mapping (`root-approved-ssh-mapping-v1`) and its SHA-256, as the startup script checks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploaderMapping {
    pub json: String,
    pub sha256: String,
    pub username: String,
}

pub fn uploader_mapping(
    login: &str,
    github_uid: u64,
    public_key: &str,
    instance_name: &str,
    now: u64,
) -> Result<UploaderMapping, String> {
    let key = normalize_ed25519_public_key(public_key)?;
    let username = uploader_username(login);
    let principal: String = login
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(39)
        .collect();
    let receipt = format!("spot-dashboard-launch:{principal}:{github_uid}:{instance_name}:{now}");
    let value = serde_json::json!({
        "schema": "root-approved-ssh-mapping-v1",
        "principal": principal,
        "username": username,
        "uid": UPLOADER_UID,
        "identity_receipt_sha256": sha256_hex(receipt.as_bytes()),
        "public_key": key,
        "public_key_sha256": sha256_hex(key.as_bytes()),
    });
    let json = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    // The script holds the mapping in single quotes.
    if json.contains('\'') || json.contains('\n') {
        return Err(
            "Uploader mapping contains a character the startup script cannot hold".to_string(),
        );
    }
    Ok(UploaderMapping {
        sha256: sha256_hex(json.as_bytes()),
        json,
        username,
    })
}

fn write_file_b64(path: &str, contents: &str) -> String {
    format!(
        "printf '%s' '{}' | base64 -d >{}\n",
        STANDARD.encode(contents.as_bytes()),
        path
    )
}

/// Shell statements for `{{ASSET_WRITES}}`: install the pinned `host-agent`, then write the builder image files.
pub fn asset_writes(pin: &crate::host_agent_release::PinnedHostAgent) -> String {
    let mut out = String::new();
    out.push_str("install -d -m 0755 /usr/local/bin /opt/oxidex-builder/image\n");
    out.push_str("case \"$(uname -m)\" in\n");
    out.push_str(&format!(
        "  x86_64) host_agent_asset=host-agent-x86_64-linux-musl; host_agent_sha256='{}' ;;\n",
        pin.sha256_x86_64
    ));
    out.push_str(&format!(
        "  aarch64) host_agent_asset=host-agent-aarch64-linux-musl; host_agent_sha256='{}' ;;\n",
        pin.sha256_aarch64
    ));
    out.push_str("  *) echo \"No host-agent build for $(uname -m)\" >&2; exit 1 ;;\nesac\n");
    out.push_str("host_agent_download=$(mktemp)\n");
    out.push_str(&format!(
        "curl --proto '=https' --tlsv1.2 -fsSL --retry 20 --retry-delay 15 --retry-all-errors --retry-max-time 600 --max-time 120 \\\n  -o \"$host_agent_download\" '{}'\"/$host_agent_asset\"\n",
        pin.download_base_url()
    ));
    out.push_str("printf '%s  %s\\n' \"$host_agent_sha256\" \"$host_agent_download\" | sha256sum --check --status ||\n");
    out.push_str("  { echo \"host-agent download does not match its pinned SHA-256\" >&2; rm -f \"$host_agent_download\"; exit 1; }\n");
    out.push_str(
        "install -m 0755 -o root -g root \"$host_agent_download\" /usr/local/bin/host-agent\n",
    );
    out.push_str("rm -f \"$host_agent_download\"\n");
    out.push_str(&write_file_b64(
        "/opt/oxidex-builder/image/Dockerfile",
        BUILDER_DOCKERFILE,
    ));
    out.push_str(&write_file_b64(
        "/opt/oxidex-builder/image/website-package.json",
        WEBSITE_PACKAGE_JSON,
    ));
    out.push_str(&write_file_b64(
        "/opt/oxidex-builder/image/website-package-lock.json",
        WEBSITE_PACKAGE_LOCK,
    ));
    out
}

/// Fills the placeholders that only a dashboard launch knows. `build_spot_builder_vm_request` fills the rest.
pub fn render_builder_template(
    mapping: &UploaderMapping,
    pin: &crate::host_agent_release::PinnedHostAgent,
) -> String {
    crate::gcp::RAW_BUILDER_STARTUP_SCRIPT
        .replace("{{ASSET_WRITES}}", &asset_writes(pin))
        .replace("{{UPLOADER_CONFIG}}", &mapping.json)
        .replace("{{UPLOADER_SHA}}", &mapping.sha256)
}

/// `builder-<zone>-<suffix>`, which leaves room for the `-cache` and `-boot` disk names under 63 characters.
pub fn builder_instance_name(zone: &str, suffix: &str) -> Result<String, String> {
    validate_zone(zone)?;
    if suffix.is_empty()
        || suffix.len() > 8
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    {
        return Err("Invalid instance name suffix".to_string());
    }
    Ok(format!("builder-{zone}-{suffix}"))
}

/// Everything needed to insert one builder VM.
#[derive(Debug, Clone)]
pub struct BuilderLaunchPlan {
    pub instance_name: String,
    pub zone: String,
    pub cache_disk_name: String,
    pub ssh_user: String,
    pub request: GceInsertInstanceRequest,
}

pub struct BuilderLaunchInput<'a> {
    pub project: &'a str,
    pub zone: &'a str,
    pub login: &'a str,
    pub github_uid: u64,
    pub ssh_public_key: &'a str,
    pub queue_token: Option<&'a str>,
    pub suffix: &'a str,
    pub now: u64,
}

pub fn plan_builder_launch(input: &BuilderLaunchInput) -> Result<BuilderLaunchPlan, String> {
    let instance_name = builder_instance_name(input.zone, input.suffix)?;
    let cache_disk_name = format!("{instance_name}-cache");
    let mapping = uploader_mapping(
        input.login,
        input.github_uid,
        input.ssh_public_key,
        &instance_name,
        input.now,
    )?;
    let pin = crate::host_agent_release::current()?;
    let template = render_builder_template(&mapping, &pin);
    let cfg = BuilderVmConfig {
        instance_name: &instance_name,
        machine_type: BUILDER_MACHINE_TYPE,
        zone: input.zone,
        deployment_id: &instance_name,
        cache_disk_name: &cache_disk_name,
        cache_disk_device_name: &cache_disk_name,
        startup_script_template: &template,
        queue_bearer_token: input.queue_token,
        boot_disk_type: Some("hyperdisk-balanced"),
        cache_disk_type: Some("hyperdisk-balanced"),
    };
    let mut request = crate::gcp::build_spot_builder_vm_request(input.project, &cfg)
        .map_err(|e| e.to_string())?;

    let script = request
        .metadata
        .items
        .iter()
        .find(|i| i.key == "startup-script")
        .map(|i| i.value.as_str())
        .unwrap_or_default();
    if script.contains("{{ASSET_WRITES}}")
        || script.contains("{{UPLOADER_CONFIG}}")
        || script.contains("{{INSTANCE_NAME}}")
        || script.contains("{{DISK_DEVICE_NAME}}")
    {
        return Err("Builder startup script still has unrendered placeholders".to_string());
    }
    if script.len() > METADATA_VALUE_LIMIT_BYTES {
        return Err(format!(
            "Builder startup script is {} bytes, over the {} byte metadata limit",
            script.len(),
            METADATA_VALUE_LIMIT_BYTES
        ));
    }

    request.metadata.items.push(GceMetadataItem {
        key: "spot-builder-ssh-user".into(),
        value: mapping.username.clone(),
    });
    // Only the approved uploader key may sign in. Project-wide keys would also make the guest agent create
    // local accounts, which can collide with the build UIDs the bootstrap reserves and hold it.
    request.metadata.items.push(GceMetadataItem {
        key: "block-project-ssh-keys".into(),
        value: "TRUE".into(),
    });
    request
        .labels
        .insert(SSH_USER_LABEL.into(), mapping.username.clone());
    request
        .labels
        .insert(LAUNCHED_BY_LABEL.into(), "dashboard".into());

    Ok(BuilderLaunchPlan {
        instance_name,
        zone: input.zone.to_string(),
        cache_disk_name,
        ssh_user: mapping.username,
        request,
    })
}

/// A builder cache disk left behind after its VM was deleted or preempted (the disk is `auto-delete=no`).
pub fn is_leftover_builder_cache_disk(disk: &crate::gcp::GceDisk) -> bool {
    disk.is_orphan() && disk.name.starts_with("builder-") && disk.name.ends_with("-cache")
}

/// What the dashboard shows for one builder VM.
pub fn builder_summary(inst: &crate::gcp::GceBuilderInstance) -> serde_json::Value {
    serde_json::json!({
        "name": inst.name,
        "zone": inst.zone_short(),
        "machine_type": inst.machine_type_short(),
        "status": inst.status,
        "external_ip": inst.external_ip(),
        "ssh_user": inst.labels.get(SSH_USER_LABEL),
        "launched_by_dashboard": inst.labels.get(LAUNCHED_BY_LABEL).map(|v| v == "dashboard").unwrap_or(false),
        "created_at": inst.creation_timestamp,
    })
}

/// Live builders count against [`MAX_LIVE_BUILDERS`]; stopped and terminated ones do not.
pub fn is_live_builder(inst: &crate::gcp::GceBuilderInstance) -> bool {
    !inst.status.eq_ignore_ascii_case("TERMINATED") && !inst.status.eq_ignore_ascii_case("STOPPING")
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICxI6JCuFUgm18l4ZMqHtM08PDPXQHY/emPWwl+GqOXL me@laptop";

    #[test]
    fn zone_validation() {
        assert!(validate_zone("us-central1-a").is_ok());
        assert!(validate_zone("northamerica-northeast1-b").is_ok());
        assert!(validate_zone("asia-east2-a").is_ok());
        for bad in [
            "",
            "us-central1",
            "us-central1-ab",
            "US-central1-a",
            "us-central1-a;",
            "us--a",
            "../x-y1-a",
        ] {
            assert!(validate_zone(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn key_normalization_drops_comment() {
        let k = normalize_ed25519_public_key(KEY).unwrap();
        assert_eq!(k, KEY.rsplit_once(' ').unwrap().0);
    }
}
