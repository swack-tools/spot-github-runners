//! Dashboard-launched Rust builders: input checks, the rendered startup script, and route guards.

use base64::Engine;
use sha2::{Digest, Sha256};
use spot_dashboard_edge::builders::*;
use spot_dashboard_edge::gcp::{GceBuilderInstance, GceDisk};

// A syntactically valid Ed25519 public key (random bytes, no private half exists in this repository).
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICxI6JCuFUgm18l4ZMqHtM08PDPXQHY/emPWwl+GqOXL me@laptop";
const KEY_NO_COMMENT: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICxI6JCuFUgm18l4ZMqHtM08PDPXQHY/emPWwl+GqOXL";

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn plan(zone: &str) -> BuilderLaunchPlan {
    plan_builder_launch(&BuilderLaunchInput {
        project: "test-project",
        zone,
        login: "swackhamer",
        github_uid: 619624,
        ssh_public_key: KEY,
        queue_token: Some("queue-token-value"),
        suffix: "a1b2c3",
        now: 1_800_000_000,
    })
    .expect("plan")
}

fn startup_script(p: &BuilderLaunchPlan) -> String {
    p.request
        .metadata
        .items
        .iter()
        .find(|i| i.key == "startup-script")
        .map(|i| i.value.clone())
        .expect("startup-script metadata")
}

#[test]
fn accepts_one_ed25519_public_key_and_drops_the_comment() {
    assert_eq!(normalize_ed25519_public_key(KEY).unwrap(), KEY_NO_COMMENT);
    assert_eq!(
        normalize_ed25519_public_key(&format!("  {KEY}\n")).unwrap(),
        KEY_NO_COMMENT
    );
}

#[test]
fn refuses_private_keys_other_types_and_junk() {
    let private = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";
    assert!(normalize_ed25519_public_key(private)
        .unwrap_err()
        .contains("private key"));
    assert!(normalize_ed25519_public_key("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ me").is_err());
    assert!(normalize_ed25519_public_key("").is_err());
    assert!(normalize_ed25519_public_key(&format!("{KEY}\n{KEY}")).is_err());
    assert!(normalize_ed25519_public_key("ssh-ed25519 not*base64").is_err());
    assert!(normalize_ed25519_public_key("ssh-ed25519 'AAAA';reboot").is_err());
    // Valid base64 but the wrong wire format (an RSA blob labelled as Ed25519).
    let rsa_blob =
        base64::engine::general_purpose::STANDARD.encode(b"\0\0\0\x07ssh-rsa\0\0\0\x03\x01\0\x01");
    assert!(normalize_ed25519_public_key(&format!("ssh-ed25519 {rsa_blob}")).is_err());
}

#[test]
fn usernames_are_safe_linux_accounts() {
    assert_eq!(uploader_username("swackhamer"), "swackhamer");
    assert_eq!(
        uploader_username("TheDancingDeveloper"),
        "thedancingdeveloper"
    );
    assert_eq!(uploader_username("123abc"), "u123abc");
    assert_eq!(uploader_username("root"), "root-dev");
    assert_eq!(uploader_username("docker"), "docker-dev");
    assert_eq!(uploader_username("a.b"), "a-b");
    let long = uploader_username(&"x".repeat(60));
    assert!(long.len() <= 32);
    for login in ["swackhamer", "Root", "9", "--", "a.b.c", &"Z".repeat(39)] {
        let u = uploader_username(login);
        assert!(u.len() <= 32 && !u.is_empty(), "{login} -> {u}");
        assert!(u.chars().next().unwrap().is_ascii_lowercase() || u.starts_with('_'));
        assert!(u
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'));
        assert_ne!(u, "root");
    }
}

#[test]
fn zones_and_names_fit_compute_engine_limits() {
    assert!(validate_zone("us-central1-a").is_ok());
    assert!(validate_zone("northamerica-northeast1-b").is_ok());
    for bad in [
        "",
        "us-central1",
        "us-central1-ab",
        "US-central1-a",
        "us-central1-a;",
        "a-b-c",
        "../x-y1-a",
    ] {
        assert!(validate_zone(bad).is_err(), "{bad}");
    }
    let name = builder_instance_name("northamerica-northeast1-b", "a1b2c3").unwrap();
    assert!(format!("{name}-cache").len() <= 63);
    assert!(builder_instance_name("us-central1-a", "ABC").is_err());
    assert!(builder_instance_name("us-central1-a", "").is_err());
}

#[test]
fn launch_plan_is_a_fixed_shape_spot_builder_with_a_kept_cache_disk() {
    let p = plan("us-central1-a");
    assert_eq!(p.instance_name, "builder-us-central1-a-a1b2c3");
    assert_eq!(p.cache_disk_name, "builder-us-central1-a-a1b2c3-cache");
    assert_eq!(p.ssh_user, "swackhamer");
    assert!(p.request.machine_type.ends_with("/n4-highcpu-16"));
    assert_eq!(p.request.scheduling.provisioning_model, "SPOT");
    let cache = p
        .request
        .disks
        .iter()
        .find(|d| !d.boot)
        .expect("cache disk");
    assert!(!cache.auto_delete);
    assert_eq!(
        cache.device_name.as_deref(),
        Some("builder-us-central1-a-a1b2c3-cache")
    );
    assert_eq!(
        p.request.labels.get("spot-role").map(String::as_str),
        Some("rust_builder")
    );
    assert_eq!(
        p.request.labels.get(SSH_USER_LABEL).map(String::as_str),
        Some("swackhamer")
    );
    assert_eq!(
        p.request.labels.get(LAUNCHED_BY_LABEL).map(String::as_str),
        Some("dashboard")
    );
}

#[test]
fn startup_script_is_complete_and_within_the_metadata_limit() {
    let p = plan("us-central1-a");
    let script = startup_script(&p);
    assert!(
        script.len() < METADATA_VALUE_LIMIT_BYTES,
        "{} bytes",
        script.len()
    );
    for placeholder in [
        "{{ASSET_WRITES}}",
        "{{UPLOADER_CONFIG}}",
        "{{UPLOADER_SHA}}",
        "{{INSTANCE_NAME}}",
        "{{PROJECT}}",
        "{{ZONE}}",
        "{{DISK_DEVICE_NAME}}",
        "{{DEPLOYMENT_ID}}",
        "{{QUEUE_BEARER_TOKEN}}",
    ] {
        assert!(
            !script.contains(placeholder),
            "{placeholder} left in script"
        );
    }
    // The pinned host-agent release and both checksums.
    let pin = spot_dashboard_edge::host_agent_release::current().unwrap();
    assert!(script.contains(&pin.download_base_url()));
    assert!(script.contains(&pin.sha256_x86_64));
    assert!(script.contains(&pin.sha256_aarch64));
    assert!(script.contains("sha256sum --check --status"));
    // The builder image files the bootstrap's docker build needs.
    let dockerfile = include_str!("../../builder/standalone/Dockerfile");
    let b64 = base64::engine::general_purpose::STANDARD.encode(dockerfile.as_bytes());
    assert!(script.contains(&format!(
        "printf '%s' '{b64}' | base64 -d >/opt/oxidex-builder/image/Dockerfile"
    )));
    assert!(script.contains("/opt/oxidex-builder/image/website-package-lock.json"));
    // Asset writes happen before the bootstrap copies host-agent into the image context.
    let asset_at = script
        .find("base64 -d >/opt/oxidex-builder/image/Dockerfile")
        .unwrap();
    let copy_at = script
        .find("cp /usr/local/bin/host-agent /opt/oxidex-builder/image/host-agent")
        .unwrap();
    assert!(asset_at < copy_at);
    assert!(
        script.contains("[[ \"$(metadata instance/name)\" == 'builder-us-central1-a-a1b2c3' ]]")
    );
    assert!(script.contains("[[ \"$(metadata project/project-id)\" == 'test-project' ]]"));
    assert!(script.contains("SPOT_BEARER_TOKEN=$queue_token"));
    // The uploader can traverse sources/ before the bootstrap's first launcher call creates it under umask 077.
    let sources_at = script
        .find("install -d -o0 -g1001 -m0710 \"$MOUNT_POINT/remote-build/sources\"")
        .expect("sources root created with group traverse");
    let prepare_at = script
        .find("oxidex-remote-build bootstrap-tools prepare")
        .unwrap();
    assert!(sources_at < prepare_at);
    assert!(p
        .request
        .metadata
        .items
        .iter()
        .any(|i| i.key == "block-project-ssh-keys" && i.value == "TRUE"));
    assert!(script.contains("queue_token='queue-token-value'"));
}

#[test]
fn uploader_mapping_passes_the_startup_script_checks() {
    let p = plan("us-central1-a");
    let script = startup_script(&p);
    let line = script
        .lines()
        .find(|l| l.starts_with("uploader_raw='{"))
        .expect("rendered uploader mapping");
    let json = line
        .strip_prefix("uploader_raw='")
        .and_then(|s| s.strip_suffix('\''))
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(v["schema"], "root-approved-ssh-mapping-v1");
    assert_eq!(v["username"], "swackhamer");
    assert_eq!(v["uid"], UPLOADER_UID);
    assert!(UPLOADER_UID >= 1000 && UPLOADER_UID != 1001);
    assert_eq!(v["public_key"], KEY_NO_COMMENT);
    assert_eq!(
        v["public_key_sha256"],
        sha256_hex(KEY_NO_COMMENT.as_bytes())
    );
    let mapping_sha = sha256_hex(json.as_bytes());
    assert!(script.contains(&format!("uploader_sha='{mapping_sha}'")));
}

#[test]
fn rendered_startup_script_parses_as_bash() {
    let p = plan("us-central1-a");
    let script = startup_script(&p);
    let dir = std::env::temp_dir().join(format!("builder-startup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("startup.sh");
    std::fs::write(&path, script).unwrap();
    let out = std::process::Command::new("bash")
        .arg("-n")
        .arg(&path)
        .output()
        .expect("bash");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "bash -n failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn builder_instances_expose_their_external_address() {
    let raw = serde_json::json!({
        "name": "builder-us-central1-a-a1b2c3",
        "zone": "https://www.googleapis.com/compute/v1/projects/p/zones/us-central1-a",
        "machineType": "https://www.googleapis.com/compute/v1/projects/p/zones/us-central1-a/machineTypes/n4-highcpu-16",
        "status": "RUNNING",
        "labels": { "spot-role": "rust_builder", "spot-builder-ssh-user": "swackhamer", "spot-launched-by": "dashboard" },
        "networkInterfaces": [{ "network": "x", "accessConfigs": [{ "name": "External NAT", "type": "ONE_TO_ONE_NAT", "natIP": "203.0.113.7" }] }]
    });
    let inst: GceBuilderInstance = serde_json::from_value(raw).unwrap();
    assert_eq!(inst.external_ip(), Some("203.0.113.7"));
    let summary = builder_summary(&inst);
    assert_eq!(summary["zone"], "us-central1-a");
    assert_eq!(summary["machine_type"], "n4-highcpu-16");
    assert_eq!(summary["ssh_user"], "swackhamer");
    assert_eq!(summary["launched_by_dashboard"], true);
    assert!(is_live_builder(&inst));

    let no_ip: GceBuilderInstance = serde_json::from_value(serde_json::json!({
        "name": "builder-x", "zone": "zones/us-central1-a", "status": "PROVISIONING"
    }))
    .unwrap();
    assert_eq!(no_ip.external_ip(), None);
}

#[test]
fn only_unattached_builder_cache_disks_are_leftovers() {
    let disk = |name: &str, users: Vec<String>| GceDisk {
        id: None,
        name: name.to_string(),
        zone: "zones/us-central1-a".to_string(),
        size_gb: Some("375".to_string()),
        r#type: None,
        self_link: String::new(),
        users,
        creation_timestamp: None,
    };
    assert!(is_leftover_builder_cache_disk(&disk(
        "builder-us-central1-a-a1b2c3-cache",
        vec![]
    )));
    assert!(!is_leftover_builder_cache_disk(&disk(
        "builder-us-central1-a-a1b2c3-cache",
        vec!["instances/builder-us-central1-a-a1b2c3".into()]
    )));
    assert!(!is_leftover_builder_cache_disk(&disk(
        "oxidex-runners-x-data",
        vec![]
    )));
    assert!(!is_leftover_builder_cache_disk(&disk(
        "builder-us-central1-a-a1b2c3-boot",
        vec![]
    )));
}

#[test]
fn builder_changes_are_owner_only_and_reads_need_an_administrator() {
    let lib = include_str!("../src/lib.rs");
    let arm = |head: &str| -> String {
        let start = lib.find(head).unwrap_or_else(|| panic!("{head} not found"));
        let rest = &lib[start + head.len()..];
        let end = rest.find("\n        (Method::").unwrap_or(rest.len());
        rest[..end].to_string()
    };
    for head in [
        "(Method::Post, \"/api/builders/launch\") =>",
        "(Method::Get, \"/api/builders/launch-status\") =>",
        "(Method::Post, \"/api/builders/delete\") =>",
        "(Method::Post, \"/api/builders/delete-cache-disk\") =>",
    ] {
        let body = arm(head);
        let owner_check = body.find("session.uid != state::OWNER_UID").expect(head);
        let gcp = body.find("builder_gcp_client(").expect(head);
        assert!(
            owner_check < gcp,
            "{head} must check the owner before touching GCP"
        );
    }
    for head in [
        "(Method::Get, \"/api/builders\") =>",
        "(Method::Get, \"/ui/builders.js\") =>",
    ] {
        let body = arm(head);
        let first = body.trim_start().trim_start_matches('{').trim_start();
        assert!(
            first.starts_with("if let Some(denied) = require_administrator("),
            "{head}"
        );
    }
}

#[test]
fn builder_script_writes_server_data_only_as_text() {
    let js = spot_dashboard_edge::ui_builders::BUILDERS_JS;
    assert!(!js.contains("innerHTML"));
    assert!(!js.contains("insertAdjacentHTML"));
    assert!(!js.contains("eval("));
    assert!(js.contains("window.openBuilderLaunch = openBuilderLaunch"));
}

#[test]
fn install_command_takes_only_plain_project_ids() {
    let cmd = install_command("my-project-123").expect("valid project");
    assert_eq!(
        cmd,
        format!("curl -fsSL {CARGO_REMOTE_URL} | bash -s -- install --project my-project-123")
    );
    for bad in [
        "",
        "short",
        "Upper-case",
        "1starts-with-digit",
        "ends-with-",
        "has space",
        "semi;colon",
        "x$(id)xxxx",
        "a-very-long-project-id-over-30-chars",
    ] {
        assert!(install_command(bad).is_none(), "{bad:?} must be refused");
    }
}

const CARGO_REMOTE: &str = include_str!("../../docs/cargo-remote.sh");

#[test]
fn cargo_remote_is_published_where_the_install_line_points() {
    assert!(CARGO_REMOTE_URL.ends_with("/cargo-remote.sh"));
    assert!(CARGO_REMOTE.contains(&format!("URL=\"{CARGO_REMOTE_URL}\"")));
    // The docs site is public: no deployment identifiers in the script.
    for secret_ish in ["homelab", "330135650610", "spot.swacktech.com", "@homelab"] {
        assert!(
            !CARGO_REMOTE.contains(secret_ish),
            "{secret_ish} leaked into the public script"
        );
    }
    // It must not edit the user's SSH configuration (comments may mention it).
    let code: String = CARGO_REMOTE
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!code.contains(".ssh/config"));
    assert!(!code.contains("ssh-keygen -R"));
}

/// Runs docs/cargo-remote.sh against stub gcloud, ssh, and rsync in a temporary HOME.
struct Sandbox {
    dir: std::path::PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cargo-remote-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in [
            "bin",
            "home/.cargo/bin",
            "home/.ssh",
            "remote",
            "proj-a",
            "proj-b",
        ] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let d = dir.display();
        let stub = |name: &str, body: String| {
            let p = dir.join("bin").join(name);
            std::fs::write(&p, body).unwrap();
            std::process::Command::new("chmod")
                .arg("+x")
                .arg(&p)
                .status()
                .unwrap();
        };
        stub(
            "gcloud",
            format!(
                "#!/usr/bin/env bash\necho \"$*\" >> {d}/gcloud.log\nprintf 'builder-z-bbb\\tdev\\t10.0.0.2\\nbuilder-z-aaa\\tdev\\t10.0.0.1\\nrunner-x\\tdev\\t10.0.0.9\\nbuilder-z-ccc\\tdev\\t\\n'\n"
            ),
        );
        stub(
            "ssh",
            format!(
                "#!/usr/bin/env bash\nargs=(\"$@\"); n=${{#args[@]}}; t=\"${{args[$((n-2))]}}\"; c=\"${{args[$((n-1))]}}\"\necho \"$t :: $c\" >> {d}/ssh.log\necho \"$*\" >> {d}/ssh-args.log\n[[ -f {d}/down-$t ]] && {{ echo 'Connection refused' >&2; exit 255; }}\n[[ -f {d}/busy-$t && \"$c\" == *' cargo '* ]] && exit 75\n[[ -f {d}/fail-$t && \"$c\" == *' cargo '* ]] && exit 101\ncase \"$c\" in *' status') echo 'OK 0' ;; *' prepare '*) mkdir -p {d}/remote/sources/\"${{c##* }}\" ;; printf*) bash -c \"${{c//\\/mnt\\/runner-data\\/remote-build/{d}/remote}}\" ;; esac\n"
            ),
        );
        stub(
            "rsync",
            format!(
                "#!/usr/bin/env bash\necho \"rsync $*\" >> {d}/rsync.log\nsrc=\"${{@: -2:1}}\"; dst=\"${{@: -1}}\"\nif [[ \"$src\" == *:* ]]; then mkdir -p \"$(dirname \"$dst\")\"; echo ELF > \"$dst\"; fi\n"
            ),
        );
        std::fs::write(dir.join("home/.ssh/config"), "Host keep-me\n").unwrap();
        std::fs::write(
            dir.join("proj-a/Cargo.toml"),
            "[package]\nname = \"alpha\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("proj-b/Cargo.toml"),
            "[package]\nname = \"beta\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("cargo-remote.sh"), CARGO_REMOTE).unwrap();
        Sandbox { dir }
    }

    fn run(&self, cwd: &str, args: &[&str]) -> std::process::Output {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        std::process::Command::new("bash")
            .arg(self.dir.join("cargo-remote.sh"))
            .args(args)
            .current_dir(self.dir.join(cwd))
            .env("PATH", path)
            .env("HOME", self.dir.join("home"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("SPOT_BUILDERS")
            .env_remove("SPOT_GCP_PROJECT")
            .env_remove("SPOT_BUILDER_PROJECT")
            .env_remove("SPOT_BUILDER_KEY")
            .output()
            .expect("bash")
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.join(rel)).unwrap_or_default()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn ok(out: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "cargo-remote failed: {err}");
    err
}

#[test]
fn cargo_remote_installs_into_cargo_bin_and_records_the_project() {
    let sb = Sandbox::new("install");
    ok(&sb.run(".", &["install", "--project", "my-project-123"]));
    assert_eq!(
        sb.read("home/.config/spot-builder/project").trim(),
        "my-project-123"
    );
    assert_eq!(sb.read("home/.cargo/bin/cargo-remote"), CARGO_REMOTE);
    let bad = sb.run(".", &["install", "--project", "x;id"]);
    assert!(!bad.status.success());
    // A crate named like a launcher subcommand would be misrouted on the builder.
    std::fs::write(
        sb.dir.join("proj-a/Cargo.toml"),
        "[package]\nname = \"status\"\n",
    )
    .unwrap();
    let out = sb.run("proj-a", &["remote", "build"]);
    assert_eq!(out.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&out.stderr).contains("reserves the name status"));
}

#[test]
fn cargo_remote_spreads_projects_keeps_them_sticky_and_fails_over() {
    let sb = Sandbox::new("rr");
    ok(&sb.run(".", &["install", "--project", "my-project-123"]));

    // First project gets the first builder by name, the next project the next one.
    let err = ok(&sb.run("proj-a", &["remote", "build", "--release"]));
    assert!(err.contains("built on builder-z-aaa"), "{err}");
    assert_eq!(sb.read("proj-a/target/release/alpha"), "ELF\n");
    assert_eq!(
        sb.read("remote/sources/alpha/rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.99.0\"\n"
    );
    let err = ok(&sb.run("proj-b", &["remote", "build"]));
    assert!(err.contains("built on builder-z-bbb"), "{err}");
    assert_eq!(sb.read("proj-b/target/debug/beta"), "ELF\n");

    // A project stays on its builder.
    let err = ok(&sb.run("proj-a", &["remote", "build", "--release"]));
    assert!(err.contains("built on builder-z-aaa"), "{err}");

    // When its builder is unreachable it moves to the next one and stays there.
    std::fs::write(sb.dir.join("down-dev@10.0.0.1"), "").unwrap();
    let err = ok(&sb.run("proj-a", &["remote", "build", "--release"]));
    assert!(err.contains("builder-z-aaa is not ready"), "{err}");
    assert!(err.contains("built on builder-z-bbb"), "{err}");
    assert_eq!(
        sb.read("home/.config/spot-builder/assign/alpha").trim(),
        "builder-z-bbb"
    );

    // A busy builder (launcher exit 75) passes the build to the next one; a failed build is reported, not retried.
    std::fs::remove_file(sb.dir.join("down-dev@10.0.0.1")).unwrap();
    std::fs::write(sb.dir.join("busy-dev@10.0.0.2"), "").unwrap();
    let err = ok(&sb.run("proj-a", &["remote", "build", "--release"]));
    assert!(err.contains("builder-z-bbb is busy"), "{err}");
    assert!(err.contains("built on builder-z-aaa"), "{err}");
    std::fs::write(sb.dir.join("fail-dev@10.0.0.1"), "").unwrap();
    let out = sb.run("proj-a", &["remote", "build", "--release"]);
    assert_eq!(out.status.code(), Some(101));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("builder-z-bbb"), "{err}");

    // Builders without an address and non-builder VMs are ignored; gcloud output is cached.
    assert!(!sb.read("ssh.log").contains("10.0.0.9"));
    assert_eq!(sb.read("gcloud.log").lines().count(), 1);
    assert!(sb.read("gcloud.log").contains("--project my-project-123"));
    assert!(sb
        .read("gcloud.log")
        .contains("labels.spot-role=rust_builder"));

    // Builds go through the launcher; the user's SSH config is untouched.
    assert!(sb.read("ssh.log").contains(
        "dev@10.0.0.2 :: sudo -n /usr/local/bin/oxidex-remote-build alpha cargo build --release"
    ));
    assert_eq!(sb.read("home/.ssh/config"), "Host keep-me\n");
    assert!(sb.read("rsync.log").contains("UserKnownHostsFile="));
    assert!(sb.read("rsync.log").contains("HostKeyAlias=builder-z-aaa"));
    assert!(sb
        .read("ssh-args.log")
        .contains("HostKeyAlias=builder-z-bbb"));
}

#[test]
fn cargo_remote_refuses_another_rust_version_and_reports_no_ready_builder() {
    let sb = Sandbox::new("refuse");
    std::fs::write(
        sb.dir.join("proj-a/rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.98.0\"\n",
    )
    .unwrap();
    let out = sb.run("proj-a", &["remote", "build"]);
    assert_eq!(out.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&out.stderr).contains("1.99.0"));

    std::fs::write(
        sb.dir.join("proj-a/rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.99.0\"\n",
    )
    .unwrap();
    std::fs::write(sb.dir.join("down-dev@10.0.0.1"), "").unwrap();
    std::fs::write(sb.dir.join("down-dev@10.0.0.2"), "").unwrap();
    let out = sb.run("proj-a", &["remote", "build"]);
    assert_eq!(out.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no builder is free"));
}

#[test]
fn launch_status_reports_a_quota_failure() {
    let op: spot_dashboard_edge::gcp::GceOperation = serde_json::from_value(serde_json::json!({
        "name": "operation-1",
        "status": "DONE",
        "error": {"errors": [{"code": "QUOTA_EXCEEDED", "message": "Quota 'HDB_TOTAL_GB' exceeded."}]},
    }))
    .unwrap();
    let status = launch_status(&op);
    assert_eq!(status["done"], true);
    assert_eq!(status["error"], "Quota 'HDB_TOTAL_GB' exceeded.");
}

#[test]
fn launch_status_waits_for_a_running_operation() {
    let op: spot_dashboard_edge::gcp::GceOperation =
        serde_json::from_value(serde_json::json!({"name": "operation-1", "status": "RUNNING"}))
            .unwrap();
    let status = launch_status(&op);
    assert_eq!(status["done"], false);
    assert!(status["error"].is_null());
}

#[test]
fn builder_startup_claims_gid_1001_before_installing_docker() {
    let script = spot_dashboard_edge::gcp::RAW_BUILDER_STARTUP_SCRIPT;
    let claim = script
        .find("\nclaim_build_gid\n")
        .expect("claim_build_gid is called");
    let docker = script
        .find("apt-get install -y docker-ce")
        .expect("docker is installed");
    assert!(claim < docker);
    assert!(script.contains("docker | lxd)"));
    // getent exits 2 for an unused GID, which pipefail and set -e must not turn into a failed setup.
    assert!(script.contains("holder=$(getent group 1001 | cut -d: -f1) || true"));
}
