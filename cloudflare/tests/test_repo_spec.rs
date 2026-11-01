//! Repository-owned runner specs (`.github/spot-runners.yml`, `spot-runner-spec-v1`).

use async_trait::async_trait;
use base64::Engine as _;
use spot_dashboard_edge::github::{
    render_dockerfile, render_startup_script, repo_specs_report, GitHubActionsClient,
    RepoSpecFetch, StartupParams, RUNNER_DOCKERFILE,
};
use spot_dashboard_edge::patch_console::GitHubTransport;
use spot_dashboard_edge::repo_spec::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A two-pool spec no floor host can serve yet: x64 jobs need Docker, the other pool is arm64.
const NEWZNAB_SPEC: &str = r#"# Runners this repository asks swack-tools/spot-github-runners for.
schema: spot-runner-spec-v1

pools:
  # runs-on: ubuntu-latest or ubuntu-24.04
  - name: linux-x64
    arch: x64
    labels: [ubuntu-latest, ubuntu-24.04]
    cpus: 2
    memory_gib: 8
    min: 0
    max: 10
    needs:
      docker: true # MariaDB service containers
    toolchains:
      rust:
        toolchains: ["1.98.0"]
        components: [clippy, rustfmt]
    apt_packages: [par2, unrar, python3]

  # runs-on: ubuntu-24.04-arm
  - name: linux-arm64
    arch: arm64
    labels: [ubuntu-24.04-arm]
    cpus: 2
    memory_gib: 8
    min: 0
    max: 10
    toolchains:
      rust:
        toolchains: ["1.98.0"]
        components: [clippy, rustfmt]
    apt_packages: [par2, unrar, python3]
"#;

/// A spec a floor host can serve today: x64, no Docker, fits a 7 vCPU / 14 GiB slot.
const FLOOR_SPEC: &str = r#"schema: spot-runner-spec-v1
pools:
  - name: arm
    arch: arm64
    labels: [nntmux-arm]
  - name: build
    arch: x64
    labels:
      - nntmux-build
      - ubuntu-24.04
    cpus: 4
    memory_gib: 8
    isa: [avx2]
    toolchains:
      rust:
        toolchains: ["1.98.0", nightly-2026-10-01]
        components: [clippy, rustfmt]
        targets: [x86_64-unknown-linux-musl]
    apt_packages: [par2, unrar]
"#;

const DEFAULTS_YAML: &str = "schema: spot-runner-patch-v1\n\nhardware:\n  arch: x86_64\n  min_vcpu: 16\n  min_ram_gib: 32\n  isa: []\n  extra_labels: []\n\nhost:\n  apt_packages: []\n  sysctl: {}\n  files: []\n  boot_mkdirs: []\n  boot_script: \"\"\n\ncontainer:\n  apt_packages: []\n  sysctl: {}\n  mounts: []\n  files: []\n  dockerfile_run: \"\"\n";

const CENTRAL_YAML: &str = "schema: spot-runner-patch-v1\nrepository: xbmc4lyfe/newznab-tmux\n\nhardware:\n  arch: x86_64\n  extra_labels:\n    - x86\n\nhost:\n  sysctl:\n    vm.max_map_count: 262144\n\ncontainer:\n  apt_packages:\n    - php-cli\n  dockerfile_run: |\n    php -v\n";

/// Serves file contents by `owner/repo` and path; everything else is 404.
#[derive(Default, Clone)]
struct RepoFiles {
    files: Arc<Mutex<HashMap<String, String>>>,
    status: Arc<Mutex<HashMap<String, u16>>>,
    body_403: Arc<Mutex<String>>,
}

impl RepoFiles {
    fn with(files: &[(&str, &str, &str)]) -> Self {
        let t = Self::default();
        for (repo, path, text) in files {
            t.files.lock().unwrap().insert(
                format!("repos/{}/contents/{}", repo, path),
                text.to_string(),
            );
        }
        t
    }
}

#[async_trait(?Send)]
impl GitHubTransport for RepoFiles {
    async fn request(
        &self,
        method: &str,
        path: &str,
        _headers: &[(&str, &str)],
        _body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        assert_eq!(method, "GET");
        if let Some(code) = self.status.lock().unwrap().get(path) {
            let custom = self.body_403.lock().unwrap().clone();
            if *code == 403 && !custom.is_empty() {
                return Ok((403, custom.into_bytes()));
            }
            return Ok((*code, b"{\"message\":\"boom\"}".to_vec()));
        }
        match self.files.lock().unwrap().get(path) {
            Some(text) => {
                let body = serde_json::json!({
                    "encoding": "base64",
                    "content": base64::engine::general_purpose::STANDARD.encode(text.as_bytes()),
                });
                Ok((200, serde_json::to_vec(&body).unwrap()))
            }
            None => Ok((404, b"{\"message\":\"Not Found\"}".to_vec())),
        }
    }
}

const CONFIG: &str = "swack-tools/spot-runners-config";
const NNTMUX: &str = "xbmc4lyfe/newznab-tmux";

fn errors_of(text: &str) -> String {
    parse_spec(text).unwrap_err().join("\n")
}

// ============================================================================
// Parsing and validation
// ============================================================================

#[test]
fn newznab_spec_parses_into_two_pools() {
    let spec = parse_spec(NEWZNAB_SPEC).unwrap();
    assert_eq!(spec.pools.len(), 2);
    let x64 = &spec.pools[0];
    assert_eq!(x64.name, "linux-x64");
    assert_eq!(x64.arch, "x64");
    assert_eq!(x64.labels, vec!["ubuntu-latest", "ubuntu-24.04"]);
    assert_eq!((x64.cpus, x64.memory_gib), (Some(2), Some(8)));
    assert_eq!((x64.min, x64.max), (0, Some(10)));
    assert!(x64.needs_docker);
    let rust = x64.rust.as_ref().unwrap();
    assert_eq!(rust.toolchains, vec!["1.98.0"]);
    assert_eq!(rust.components, vec!["clippy", "rustfmt"]);
    assert_eq!(x64.apt_packages, vec!["par2", "unrar", "python3"]);
    let arm = &spec.pools[1];
    assert_eq!(arm.arch, "arm64");
    assert!(!arm.needs_docker);
}

#[test]
fn arch_aliases_normalise() {
    for (given, want) in [
        ("x86_64", "x64"),
        ("amd64", "x64"),
        ("X64", "x64"),
        ("aarch64", "arm64"),
    ] {
        let text = format!(
            "schema: spot-runner-spec-v1\npools:\n  - name: p\n    arch: {}\n    labels: [a]\n",
            given
        );
        assert_eq!(parse_spec(&text).unwrap().pools[0].arch, want, "{}", given);
    }
    let spec =
        parse_spec("schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n").unwrap();
    assert_eq!(spec.pools[0].arch, "x64", "x64 is the default");
}

#[test]
fn schema_and_pools_are_required() {
    let e = errors_of("pools:\n  - name: p\n    labels: [a]\n");
    assert!(e.contains("schema: missing"), "{}", e);
    let e = errors_of("schema: spot-runner-patch-v1\npools:\n  - name: p\n    labels: [a]\n");
    assert!(e.contains("expected spot-runner-spec-v1"), "{}", e);
    let e = errors_of("schema: spot-runner-spec-v1\npools: []\n");
    assert!(e.contains("at least one pool"), "{}", e);
    let e = errors_of("schema: spot-runner-spec-v1\npools:\n  - name: p\n");
    assert!(e.contains("labels: list at least one label"), "{}", e);
}

#[test]
fn unknown_keys_are_refused_at_every_level() {
    for (text, key) in [
        ("schema: spot-runner-spec-v1\nhost: {}\npools:\n  - name: p\n    labels: [a]\n", "host"),
        (
            "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    boot_script: rm\n",
            "boot_script",
        ),
        (
            "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    machine_type: c4d-highcpu-16\n",
            "machine_type",
        ),
        (
            "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    needs:\n      gpu: true\n",
            "gpu",
        ),
        (
            "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    toolchains:\n      go: [\"1.23\"]\n",
            "go",
        ),
        (
            "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    toolchains:\n      rust:\n        toolchains: [\"1.98.0\"]\n        profile: default\n",
            "profile",
        ),
    ] {
        let e = errors_of(text);
        assert!(e.contains(&format!("unknown key {:?}", key)), "{}: {}", key, e);
    }
}

#[test]
fn misindented_lines_are_refused_not_dropped() {
    // A key indented one space would otherwise end the document early and be skipped.
    let text = "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n apt_packages: [par2]\n";
    let e = errors_of(text);
    assert!(e.contains("not part of the document"), "{}", e);
}

#[test]
fn labels_the_fleet_adds_or_picks_are_refused() {
    for label in [
        "self-hosted",
        "Linux",
        "X64",
        "ARM64",
        "spot",
        "c4d-highcpu-16",
        "n4a-standard-8",
        "us-central1-a",
        "europe-west4-b",
    ] {
        let text = format!(
            "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [ok, {}]\n",
            label
        );
        let e = errors_of(&text);
        assert!(e.contains(&format!("{:?}", label)), "{}: {}", label, e);
    }
    // Ordinary names that only look a little like zones are fine.
    parse_spec("schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [ubuntu-24.04-arm, ubuntu-latest, nntmux-simd]\n").unwrap();
}

#[test]
fn values_outside_the_grammar_are_refused() {
    let cases = [
        ("    labels: [\"a b\"]\n", "labels"),
        ("    labels: [a]\n    apt_packages: [\"par2;reboot\"]\n", "apt_packages"),
        ("    labels: [a]\n    apt_packages: [-y]\n", "apt_packages"),
        ("    labels: [a]\n    apt_packages: [\"php=8.5\"]\n", "apt_packages"),
        ("    labels: [a]\n    apt_packages: [Par2]\n", "apt_packages"),
        ("    labels: [a]\n    cpus: 0\n", "cpus"),
        ("    labels: [a]\n    memory_gib: 4096\n", "memory_gib"),
        ("    labels: [a]\n    min: 5\n    max: 2\n", "min (5) is larger than max (2)"),
        ("    labels: [a]\n    max: 51\n", "max"),
        ("    labels: [a]\n    arch: riscv64\n", "must be x64 or arm64"),
        ("    labels: [a]\n    needs:\n      docker: yes-please\n", "true or false"),
        (
            "    labels: [a]\n    toolchains:\n      rust:\n        toolchains: [1.98]\n",
            "reads as a number",
        ),
        (
            "    labels: [a]\n    toolchains:\n      rust:\n        toolchains: [\"1.98.0; curl x\"]\n",
            "must be a release",
        ),
        (
            "    labels: [a]\n    toolchains:\n      rust:\n        toolchains: [\"2.0.0\"]\n",
            "must be a release",
        ),
        (
            "    labels: [a]\n    toolchains:\n      rust:\n        toolchains: [\"1.98.0\"]\n        components: [cargo-evil]\n",
            "components",
        ),
        (
            "    labels: [a]\n    toolchains:\n      rust:\n        toolchains: [\"1.98.0\"]\n        targets: [\"$(id)\"]\n",
            "target triple",
        ),
        ("    labels: [a]\n    toolchains:\n      rust:\n        components: [clippy]\n", "list at least one toolchain"),
    ];
    for (body, want) in cases {
        let text = format!("schema: spot-runner-spec-v1\npools:\n  - name: p\n{}", body);
        let e = errors_of(&text);
        assert!(e.contains(want), "{:?} -> {}", body, e);
    }
    let e = errors_of("schema: spot-runner-spec-v1\npools:\n  - name: Bad_Name\n    labels: [a]\n");
    assert!(e.contains("lowercase letters"), "{}", e);
    let e = errors_of(
        "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n  - name: p\n    labels: [b]\n",
    );
    assert!(e.contains("used twice"), "{}", e);
}

#[test]
fn caps_bound_every_list_and_the_file() {
    let many: Vec<String> = (0..=MAX_APT_PACKAGES)
        .map(|i| format!("pkg{}", i))
        .collect();
    let text = format!(
        "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    apt_packages: [{}]\n",
        many.join(", ")
    );
    assert!(errors_of(&text).contains("at most 64 entries"));

    let mut text = "schema: spot-runner-spec-v1\npools:\n".to_string();
    for i in 0..=MAX_POOLS {
        text.push_str(&format!("  - name: p{}\n    labels: [a]\n", i));
    }
    assert!(errors_of(&text).contains("at most 8 pools"));

    let big = format!(
        "schema: spot-runner-spec-v1\n#{}\n",
        "x".repeat(MAX_SPEC_BYTES)
    );
    assert!(errors_of(&big).contains("the limit is"));
}

// ============================================================================
// Floor-host fit
// ============================================================================

#[test]
fn floor_host_gaps_name_what_is_missing() {
    let spec = parse_spec(NEWZNAB_SPEC).unwrap();
    let x64 = floor_host_gaps(&spec.pools[0]);
    assert_eq!(x64.len(), 1, "{:?}", x64);
    assert!(x64[0].contains("Docker daemon"));
    let arm = floor_host_gaps(&spec.pools[1]);
    assert_eq!(arm.len(), 1, "{:?}", arm);
    assert!(arm[0].contains("arm64"));

    let err = select_floor_pool(&spec).unwrap_err();
    assert!(
        err.contains("pool linux-x64 needs a Docker daemon"),
        "{}",
        err
    );
    assert!(err.contains("pool linux-arm64 needs arm64"), "{}", err);

    let big = parse_spec(
        "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    cpus: 8\n    memory_gib: 15\n",
    )
    .unwrap();
    let gaps = floor_host_gaps(&big.pools[0]);
    assert_eq!(gaps.len(), 2, "{:?}", gaps);
}

#[test]
fn the_first_pool_a_floor_host_can_serve_is_chosen() {
    let spec = parse_spec(FLOOR_SPEC).unwrap();
    assert_eq!(select_floor_pool(&spec).unwrap().name, "build");
}

// ============================================================================
// Translation into the runner patch
// ============================================================================

#[test]
fn pool_to_patch_carries_labels_packages_and_toolchains_but_no_host_settings() {
    let spec = parse_spec(FLOOR_SPEC).unwrap();
    let pool = select_floor_pool(&spec).unwrap();
    let patch = pool_to_patch(pool, NNTMUX);
    assert_eq!(patch.repository, NNTMUX);
    assert_eq!(patch.hardware.arch, "x86_64");
    assert_eq!(patch.hardware.isa, vec!["avx2"]);
    assert_eq!(
        patch.hardware.extra_labels,
        vec!["nntmux-build", "ubuntu-24.04"]
    );
    assert_eq!(patch.container.apt_packages, vec!["par2", "unrar"]);
    assert!(patch.container.mounts.is_empty() && patch.container.files.is_empty());
    assert_eq!(
        patch.host,
        Default::default(),
        "a repository never sets host settings"
    );

    let run = &patch.container.dockerfile_run;
    assert!(run.contains(
        "rustup toolchain install '1.98.0' --profile minimal --no-self-update --component 'clippy' --component 'rustfmt' --target 'x86_64-unknown-linux-musl'"
    ), "{}", run);
    assert!(
        run.contains("rustup toolchain install 'nightly-2026-10-01'"),
        "{}",
        run
    );
    assert!(
        run.contains("rustup run '1.98.0' rustc --version"),
        "{}",
        run
    );
    assert!(
        run.trim_end()
            .ends_with("chmod -R a-w \"$RUSTUP_HOME\" \"$CARGO_HOME\""),
        "{}",
        run
    );
    spot_dashboard_edge::patch::validate_patch_constraints(&patch).unwrap();
}

#[test]
fn toolchain_layer_runs_as_root_before_the_runner_user() {
    let spec = parse_spec(FLOOR_SPEC).unwrap();
    let patch = pool_to_patch(select_floor_pool(&spec).unwrap(), NNTMUX);
    let rendered = render_dockerfile(RUNNER_DOCKERFILE, Some(&patch)).unwrap();
    let lock = rendered.find("chmod -R a-w /opt/runner-rust").unwrap();
    let layer = rendered.find("# Repo patch ").unwrap();
    let user = rendered
        .find("USER runner\nENV CARGO_HOME=/work/.cargo\n")
        .unwrap();
    assert!(lock < layer && layer < user);
    assert!(
        rendered[layer..user].contains("install -y --no-install-recommends 'par2' 'unrar'")
            || rendered[layer..user].contains("install -y --no-install-recommends par2 unrar")
    );
}

#[test]
fn the_largest_spec_stays_far_under_the_startup_script_limit() {
    let pkgs: Vec<String> = (0..MAX_APT_PACKAGES)
        .map(|i| format!("package-name-that-is-long-{:02}{}", i, "x".repeat(30)))
        .collect();
    let targets: Vec<String> = (0..MAX_RUST_TARGETS)
        .map(|i| format!("x86_64-unknown-linux-gnu{}", i))
        .collect();
    let text = format!(
        "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    toolchains:\n      rust:\n        toolchains: [nightly-2026-07-01, nightly-2026-08-01, nightly-2026-09-01, nightly-2026-10-01]\n        components: [{}]\n        targets: [{}]\n    apt_packages: [{}]\n",
        RUST_COMPONENTS.join(", "),
        targets.join(", "),
        pkgs.join(", ")
    );
    let spec = parse_spec(&text).unwrap();
    let patch = pool_to_patch(&spec.pools[0], NNTMUX);
    let params = StartupParams {
        disk_device_name: "oxidex-runners-x-data",
        instance_name: "oxidex-runners-x",
        deployment_id: "dep-1",
        org: NNTMUX,
        runner_prefix: "oxidex",
        runner_group: "default",
        runner_labels: "self-hosted,Linux,X64,spot,a",
        registration_token: "tok",
        runner_count: 2,
        runner_memory_gb: 14,
        runner_cpu_limit: "7",
        tiered: true,
        strict_heavy_pool: false,
        resource_monitoring: true,
    };
    let base = render_startup_script(&params, None).unwrap();
    let script = render_startup_script(&params, Some(&patch)).unwrap();
    let added = script.len() - base.len();
    // Packages and toolchains travel base64-encoded inside the Dockerfile, so the cost is
    // about 4/3 of the text. The cap keeps it a small slice of GCE's 256 KiB limit.
    assert!(added < 32 * 1024, "the spec added {} bytes", added);
    assert!(
        script.len() < 192 * 1024,
        "startup script is {} bytes (base {})",
        script.len(),
        base.len()
    );
}

// ============================================================================
// Reading specs from GitHub
// ============================================================================

#[tokio::test]
async fn fetch_repo_spec_reads_the_default_branch_file() {
    let t = RepoFiles::with(&[(NNTMUX, SPEC_PATH, NEWZNAB_SPEC)]);
    let client = GitHubActionsClient::new("tok".into(), Arc::new(t));
    match client.fetch_repo_spec(NNTMUX).await.unwrap() {
        RepoSpecFetch::Found(spec) => assert_eq!(spec.pools.len(), 2),
        other => panic!("{:?}", other),
    }
    assert_eq!(
        client.fetch_repo_spec("xbmc4lyfe/rustnzb").await.unwrap(),
        RepoSpecFetch::Missing
    );
    assert_eq!(
        client.fetch_repo_spec("swack-tools").await.unwrap(),
        RepoSpecFetch::Missing,
        "an organization has no spec"
    );
    assert!(client.fetch_repo_spec("../../etc/passwd").await.is_err());
}

#[tokio::test]
async fn fetch_target_patch_layers_the_repo_spec_over_the_central_patch() {
    let t = RepoFiles::with(&[
        (CONFIG, "defaults.yaml", DEFAULTS_YAML),
        (CONFIG, "repos/xbmc4lyfe/newznab-tmux.yaml", CENTRAL_YAML),
        (NNTMUX, SPEC_PATH, FLOOR_SPEC),
    ]);
    let client = GitHubActionsClient::new("tok".into(), Arc::new(t));
    let patch = client.fetch_target_patch(NNTMUX).await.unwrap();
    assert_eq!(patch.repository, NNTMUX);
    // Central labels and packages stay; the spec's are added.
    assert_eq!(
        patch.hardware.extra_labels,
        vec!["x86", "nntmux-build", "ubuntu-24.04"]
    );
    assert_eq!(
        patch.container.apt_packages,
        vec!["php-cli", "par2", "unrar"]
    );
    assert_eq!(patch.hardware.isa, vec!["avx2"]);
    // Central host settings stay, and the central container script runs before the toolchains.
    assert_eq!(
        patch
            .host
            .sysctl
            .get("vm.max_map_count")
            .map(String::as_str),
        Some("262144")
    );
    let run = &patch.container.dockerfile_run;
    let php = run.find("php -v").unwrap();
    let rustup = run.find("rustup toolchain install '1.98.0'").unwrap();
    assert!(php < rustup, "{}", run);
}

#[tokio::test]
async fn fetch_target_patch_without_a_spec_is_unchanged() {
    let t = RepoFiles::with(&[
        (CONFIG, "defaults.yaml", DEFAULTS_YAML),
        (CONFIG, "repos/xbmc4lyfe/newznab-tmux.yaml", CENTRAL_YAML),
    ]);
    let client = GitHubActionsClient::new("tok".into(), Arc::new(t));
    let patch = client.fetch_target_patch(NNTMUX).await.unwrap();
    assert_eq!(patch.hardware.extra_labels, vec!["x86"]);
    assert_eq!(patch.container.apt_packages, vec!["php-cli"]);
    assert!(!patch.container.dockerfile_run.contains("rustup"));
}

#[tokio::test]
async fn fetch_target_patch_holds_on_an_invalid_or_unservable_spec() {
    let bad = "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [self-hosted]\n";
    let t = RepoFiles::with(&[
        (CONFIG, "defaults.yaml", DEFAULTS_YAML),
        (NNTMUX, SPEC_PATH, bad),
    ]);
    let client = GitHubActionsClient::new("tok".into(), Arc::new(t));
    let err = client
        .fetch_target_patch(NNTMUX)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(".github/spot-runners.yml in xbmc4lyfe/newznab-tmux is invalid"),
        "{}",
        err
    );
    assert!(err.contains("self-hosted"), "{}", err);

    let t = RepoFiles::with(&[
        (CONFIG, "defaults.yaml", DEFAULTS_YAML),
        (NNTMUX, SPEC_PATH, NEWZNAB_SPEC),
    ]);
    let client = GitHubActionsClient::new("tok".into(), Arc::new(t));
    let err = client
        .fetch_target_patch(NNTMUX)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no pool can run on a floor host"), "{}", err);
    assert!(err.contains("Docker daemon"), "{}", err);
}

#[tokio::test]
async fn fetch_target_patch_holds_when_the_spec_is_unreadable() {
    let t = RepoFiles::with(&[(CONFIG, "defaults.yaml", DEFAULTS_YAML)]);
    t.status
        .lock()
        .unwrap()
        .insert(format!("repos/{}/contents/{}", NNTMUX, SPEC_PATH), 502);
    let client = GitHubActionsClient::new("tok".into(), Arc::new(t));
    let err = client
        .fetch_target_patch(NNTMUX)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("502"), "{}", err);
}

#[tokio::test]
async fn repo_specs_report_covers_every_repository_target() {
    let t = RepoFiles::with(&[
        (NNTMUX, SPEC_PATH, NEWZNAB_SPEC),
        ("xbmc4lyfe/nntp-proxy", SPEC_PATH, "schema: nope\n"),
        ("xbmc4lyfe/demo", SPEC_PATH, FLOOR_SPEC),
    ]);
    t.status.lock().unwrap().insert(
        format!("repos/xbmc4lyfe/broken/contents/{}", SPEC_PATH),
        500,
    );
    let client = GitHubActionsClient::new("tok".into(), Arc::new(t));
    let targets: Vec<String> = [
        "swack-tools",
        NNTMUX,
        "xbmc4lyfe/rustnzb",
        "xbmc4lyfe/nntp-proxy",
        "xbmc4lyfe/demo",
        "xbmc4lyfe/broken",
        "XBMC4LYFE/newznab-tmux",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let report = repo_specs_report(&client, &targets).await;
    let repos = report["repositories"].as_array().unwrap();
    let by = |t: &str| repos.iter().find(|r| r["target"] == t).unwrap().clone();
    assert_eq!(repos.len(), 5, "orgs and case-duplicates are skipped");
    assert_eq!(report["path"], SPEC_PATH);

    let nn = by(NNTMUX);
    assert_eq!(nn["status"], "valid");
    assert_eq!(nn["floor_pool"], serde_json::Value::Null);
    assert_eq!(nn["pools"][0]["floor_host_can_serve"], false);
    assert_eq!(nn["pools"][1]["arch"], "arm64");

    assert_eq!(by("xbmc4lyfe/rustnzb")["status"], "missing");
    assert_eq!(by("xbmc4lyfe/nntp-proxy")["status"], "invalid");
    assert_eq!(by("xbmc4lyfe/demo")["floor_pool"], "build");
    assert_eq!(by("xbmc4lyfe/broken")["status"], "unreadable");
}

#[test]
fn validate_text_report_answers_without_github() {
    let ok = validate_text_report(FLOOR_SPEC);
    assert_eq!(ok["status"], "valid");
    assert_eq!(ok["floor_pool"], "build");
    let bad = validate_text_report("schema: spot-runner-spec-v1\n");
    assert_eq!(bad["status"], "invalid");
    assert!(!bad["errors"].as_array().unwrap().is_empty());
}

#[test]
fn repo_target_names_are_checked_before_they_reach_a_path() {
    assert!(is_repo_target("xbmc4lyfe/newznab-tmux"));
    assert!(is_repo_target("swack-tools/spot-github-runners"));
    assert!(is_repo_target("Appz4Fun/nzbget"));
    for bad in [
        "swack-tools",
        "a/b/c",
        "../x",
        "a/..",
        "a/b?ref=x",
        "a/b#c",
        "a b/c",
        "/x",
    ] {
        assert!(!is_repo_target(bad), "{}", bad);
    }
}

// ============================================================================
// Hostile text can't hang or crash the parser
// ============================================================================

/// Parses on a small-stack thread and fails if it takes more than five seconds.
fn parse_bounded(text: String) -> Result<RepoRunnerSpec, Vec<String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || {
            let _ = tx.send(parse_spec(&text));
        })
        .unwrap();
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .expect("parser hung or crashed")
}

#[test]
fn dash_lines_without_a_space_are_refused_not_looped_on() {
    for tail in [
        "    apt_packages:\n      -par2\n",
        "    -x\n",
        "    --foo\n",
        "    labels: [b]\n    -\n",
    ] {
        let text = format!(
            "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n{}",
            tail
        );
        assert!(parse_bounded(text).is_err(), "{:?}", tail);
    }
}

#[test]
fn deep_nesting_is_bounded() {
    let flow = format!(
        "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: {}a{}\n",
        "[".repeat(8000),
        "]".repeat(8000)
    );
    assert!(parse_bounded(flow).is_err());
    let map = format!(
        "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    needs: {}x: 1{}\n",
        "{a: ".repeat(5000),
        "}".repeat(5000)
    );
    assert!(parse_bounded(map).is_err());
    let mut block =
        "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    needs:\n"
            .to_string();
    for depth in 0..200 {
        block.push_str(&format!("{}k:\n", " ".repeat(6 + depth)));
    }
    let e = parse_bounded(block).unwrap_err().join("\n");
    assert!(e.contains("nests deeper"), "{}", e);
}

#[test]
fn miri_needs_nightly_and_nightly_dates_must_exist() {
    let base = "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    toolchains:\n      rust:\n";
    let e = errors_of(&format!(
        "{}        toolchains: [\"1.98.0\"]\n        components: [miri]\n",
        base
    ));
    assert!(e.contains("miri needs nightly"), "{}", e);
    parse_spec(&format!(
        "{}        toolchains: [nightly-2026-10-01]\n        components: [miri]\n",
        base
    ))
    .unwrap();
    for bad in [
        "nightly-2026-13-01",
        "nightly-2026-00-10",
        "nightly-2026-02-32",
    ] {
        let e = errors_of(&format!("{}        toolchains: [{}]\n", base, bad));
        assert!(e.contains("must be a release"), "{}: {}", bad, e);
    }
}

#[tokio::test]
async fn the_config_repository_arch_wins_over_the_spec() {
    let arm_central = "schema: spot-runner-patch-v1\nhardware:\n  arch: arm64\n";
    let t = RepoFiles::with(&[
        (CONFIG, "defaults.yaml", DEFAULTS_YAML),
        (CONFIG, "repos/xbmc4lyfe/newznab-tmux.yaml", arm_central),
        (NNTMUX, SPEC_PATH, FLOOR_SPEC),
    ]);
    let client = GitHubActionsClient::new("tok".into(), Arc::new(t));
    let err = client
        .fetch_target_patch(NNTMUX)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("the config repository sets arm64"), "{}", err);
}

#[tokio::test]
async fn a_token_without_contents_access_does_not_hold_the_floor() {
    let t = RepoFiles::with(&[(CONFIG, "defaults.yaml", DEFAULTS_YAML)]);
    t.status
        .lock()
        .unwrap()
        .insert(format!("repos/{}/contents/{}", NNTMUX, SPEC_PATH), 403);
    let client = GitHubActionsClient::new("tok".into(), Arc::new(t.clone()));
    let patch = client.fetch_target_patch(NNTMUX).await.unwrap();
    assert!(patch.container.apt_packages.is_empty());
    let report = repo_specs_report(&client, &[NNTMUX.to_string()]).await;
    assert_eq!(report["repositories"][0]["status"], "unreadable");
}

#[test]
fn long_flow_items_parse_in_linear_time() {
    let spaces = " ".repeat(32_000);
    let text = format!(
        "schema: spot-runner-spec-v1\npools:\n  - name: p\n    labels: [a]\n    isa: {}a{}b{}\n",
        "[".repeat(16),
        spaces,
        "]".repeat(16)
    );
    let started = std::time::Instant::now();
    assert!(parse_bounded(text).is_err());
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_rate_limited_spec_read_still_holds_the_floor() {
    let t = RepoFiles::with(&[(CONFIG, "defaults.yaml", DEFAULTS_YAML)]);
    t.status
        .lock()
        .unwrap()
        .insert(format!("repos/{}/contents/{}", NNTMUX, SPEC_PATH), 403);
    *t.body_403.lock().unwrap() =
        "{\"message\":\"API rate limit exceeded for installation\"}".into();
    let client = GitHubActionsClient::new("tok".into(), Arc::new(t));
    let err = client
        .fetch_target_patch(NNTMUX)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("403"), "{}", err);
}
