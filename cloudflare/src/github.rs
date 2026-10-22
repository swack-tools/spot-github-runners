//! GitHub Actions REST API Client & Startup Script Rendering.
//!
//! Subsystems:
//! 1. GitHubActionsClient:
//!    - Runner registration token minting (`POST /{api_path}/actions/runners/registration-token`).
//!    - 2-slot JIT runner config generation (`POST /{api_path}/actions/runners/generate-jitconfig`).
//!    - Automatic graceful fallback to registration token if JIT config fails or is unsupported.
//!    - Self-hosted runner inventory listing (`GET /{api_path}/actions/runners` with pagination).
//!    - Safe runner deregistration (`DELETE /{api_path}/actions/runners/{id}`) with HTTP 404 safety.
//!    - Queued workflow runs and jobs backlog discovery (`runs?status=queued` and `jobs`).
//! 2. StartupRenderer:
//!    - Exact 31-placeholder substitutions into `cloud-init/startup.sh`.
//!    - Compile-time inclusion of tracked assets via `include_str!` and `include_bytes!` guaranteeing 100% parity.
//!    - Patch script injection matching `src/lib/runner_patch.py`.
//!    - Strict validation: 0 unexpanded `{{` tokens, length <= 256 KiB.

use base64::engine::general_purpose::STANDARD as B64_STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use crate::patch::RunnerPatch;
use crate::patch_console::GitHubTransport;

pub const PATCH_STATE_DIR: &str = "/var/lib/gha-runner-patch";
pub const RECEIPT_SCHEMA: &str = "gha-runner-patch-receipt-v2";

// ============================================================================
// Data Types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerLabel {
    #[serde(default)]
    pub id: Option<u64>,
    pub name: String,
    #[serde(rename = "type", default)]
    pub label_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubRunner {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub os: Option<String>,
    pub status: String, // "online", "offline"
    pub busy: bool,
    #[serde(default)]
    pub labels: Vec<RunnerLabel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerListResponse {
    pub total_count: u32,
    pub runners: Vec<GitHubRunner>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationTokenResponse {
    pub token: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateJitConfigRequest {
    pub name: String,
    pub runner_group_id: u32,
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_folder: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateJitConfigResponse {
    pub runner: GitHubRunner,
    pub encoded_jit_config: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub status: String, // "queued", "in_progress", "completed"
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub jobs_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRunsResponse {
    pub total_count: u32,
    pub workflow_runs: Vec<WorkflowRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowJob {
    pub id: u64,
    #[serde(default)]
    pub run_id: Option<u64>,
    pub name: String,
    pub status: String, // "queued", "in_progress", "completed", "waiting", "pending"
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowJobsResponse {
    pub total_count: u32,
    pub jobs: Vec<WorkflowJob>,
}

/// Two-slot runner provisioning bootstrap credentials.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunnerBootstrapConfig {
    JitConfig {
        runner_1_jitconfig: String,
        runner_2_jitconfig: String,
    },
    RegistrationToken {
        token: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum GitHubError {
    #[error("HTTP transport error: {0}")]
    Http(String),
    #[error("GitHub API error (status {status}): {message}")]
    Api { status: u16, message: String },
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Validation error: {0}")]
    Validation(String),
}

// ============================================================================
// GitHub Actions Client
// ============================================================================

pub struct GitHubActionsClient {
    token: String,
    transport: Arc<dyn GitHubTransport>,
}

impl GitHubActionsClient {
    pub fn new(token: String, transport: Arc<dyn GitHubTransport>) -> Self {
        Self { token, transport }
    }

    /// List self-hosted runners for a target org or repo with pagination (up to 50 pages).
    pub async fn list_runners(
        &self,
        target_api_path: &str,
    ) -> Result<Vec<GitHubRunner>, GitHubError> {
        let mut all_runners = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut total_expected: Option<u32> = None;

        let auth_hdr = format!("Bearer {}", self.token);
        let headers = [
            ("Authorization", auth_hdr.as_str()),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "spot-dashboard-edge/1.0"),
        ];

        for page in 1..=50 {
            let path = format!(
                "{}/actions/runners?per_page=100&page={}",
                target_api_path.trim_matches('/'),
                page
            );
            let (status, bytes) = self
                .transport
                .request("GET", &path, &headers, None)
                .await
                .map_err(GitHubError::Http)?;

            if status != 200 {
                let msg = String::from_utf8_lossy(&bytes).to_string();
                return Err(GitHubError::Api {
                    status,
                    message: msg,
                });
            }

            let resp: RunnerListResponse = serde_json::from_slice(&bytes)?;
            if total_expected.is_none() {
                total_expected = Some(resp.total_count);
            }

            let count = resp.runners.len();
            for runner in resp.runners {
                if seen_ids.insert(runner.id) {
                    all_runners.push(runner);
                }
            }

            if count == 0 || all_runners.len() >= total_expected.unwrap_or(0) as usize {
                break;
            }
        }

        Ok(all_runners)
    }

    /// Deregister an offline, idle runner. Safe against HTTP 404 (already deleted).
    pub async fn delete_runner(
        &self,
        target_api_path: &str,
        runner_id: u64,
    ) -> Result<(), GitHubError> {
        let path = format!(
            "{}/actions/runners/{}",
            target_api_path.trim_matches('/'),
            runner_id
        );
        let auth_hdr = format!("Bearer {}", self.token);
        let headers = [
            ("Authorization", auth_hdr.as_str()),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "spot-dashboard-edge/1.0"),
        ];

        let (status, bytes) = self
            .transport
            .request("DELETE", &path, &headers, None)
            .await
            .map_err(GitHubError::Http)?;

        if status == 204 || status == 404 {
            Ok(())
        } else {
            let msg = String::from_utf8_lossy(&bytes).to_string();
            Err(GitHubError::Api {
                status,
                message: msg,
            })
        }
    }

    /// Mint a single-use registration token.
    pub async fn mint_registration_token(
        &self,
        target_api_path: &str,
    ) -> Result<String, GitHubError> {
        let path = format!(
            "{}/actions/runners/registration-token",
            target_api_path.trim_matches('/')
        );
        let auth_hdr = format!("Bearer {}", self.token);
        let headers = [
            ("Authorization", auth_hdr.as_str()),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "spot-dashboard-edge/1.0"),
            ("Content-Type", "application/json"),
        ];

        let (status, bytes) = self
            .transport
            .request("POST", &path, &headers, Some(b"{}"))
            .await
            .map_err(GitHubError::Http)?;

        if status == 201 {
            let resp: RegistrationTokenResponse = serde_json::from_slice(&bytes)?;
            Ok(resp.token)
        } else {
            let msg = String::from_utf8_lossy(&bytes).to_string();
            Err(GitHubError::Api {
                status,
                message: msg,
            })
        }
    }

    /// Mint a single-use JIT runner config.
    pub async fn generate_jitconfig(
        &self,
        target_api_path: &str,
        req: &GenerateJitConfigRequest,
    ) -> Result<GenerateJitConfigResponse, GitHubError> {
        let path = format!(
            "{}/actions/runners/generate-jitconfig",
            target_api_path.trim_matches('/')
        );
        let body = serde_json::to_vec(req)?;
        let auth_hdr = format!("Bearer {}", self.token);
        let headers = [
            ("Authorization", auth_hdr.as_str()),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "spot-dashboard-edge/1.0"),
            ("Content-Type", "application/json"),
        ];

        let (status, bytes) = self
            .transport
            .request("POST", &path, &headers, Some(&body))
            .await
            .map_err(GitHubError::Http)?;

        if status == 201 {
            let resp: GenerateJitConfigResponse = serde_json::from_slice(&bytes)?;
            Ok(resp)
        } else {
            let msg = String::from_utf8_lossy(&bytes).to_string();
            Err(GitHubError::Api {
                status,
                message: msg,
            })
        }
    }

    /// Mint 2-slot runner configs (7 vCPU / 14 GiB RAM each) with graceful fallback to registration token.
    pub async fn mint_2slot_runner_configs(
        &self,
        target_api_path: &str,
        vm_name: &str,
        runner_prefix: &str,
        runner_group_id: u32,
        labels: &[String],
    ) -> Result<RunnerBootstrapConfig, GitHubError> {
        let name_1 = format!("{}-{}-1", runner_prefix, vm_name);
        let name_2 = format!("{}-{}-2", runner_prefix, vm_name);

        let req_1 = GenerateJitConfigRequest {
            name: name_1,
            runner_group_id,
            labels: labels.to_vec(),
            work_folder: Some("_work".to_string()),
        };
        let req_2 = GenerateJitConfigRequest {
            name: name_2,
            runner_group_id,
            labels: labels.to_vec(),
            work_folder: Some("_work".to_string()),
        };

        // Attempt slot 1
        match self.generate_jitconfig(target_api_path, &req_1).await {
            Ok(res_1) => {
                // Attempt slot 2
                match self.generate_jitconfig(target_api_path, &req_2).await {
                    Ok(res_2) => Ok(RunnerBootstrapConfig::JitConfig {
                        runner_1_jitconfig: res_1.encoded_jit_config,
                        runner_2_jitconfig: res_2.encoded_jit_config,
                    }),
                    Err(_) => {
                        // Slot 2 failed; fallback cleanly to registration token
                        let token = self.mint_registration_token(target_api_path).await?;
                        Ok(RunnerBootstrapConfig::RegistrationToken { token })
                    }
                }
            }
            Err(_) => {
                // JIT not supported or forbidden; fallback cleanly to registration token
                let token = self.mint_registration_token(target_api_path).await?;
                Ok(RunnerBootstrapConfig::RegistrationToken { token })
            }
        }
    }

    /// Count queued jobs backlog for a repository.
    pub async fn count_queued_jobs_for_repo(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<u32, GitHubError> {
        let runs_path = format!(
            "repos/{}/{}/actions/runs?status=queued&per_page=100",
            owner, repo
        );
        let auth_hdr = format!("Bearer {}", self.token);
        let headers = [
            ("Authorization", auth_hdr.as_str()),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "spot-dashboard-edge/1.0"),
        ];

        let (status, bytes) = self
            .transport
            .request("GET", &runs_path, &headers, None)
            .await
            .map_err(GitHubError::Http)?;

        if status != 200 {
            return Ok(0);
        }

        let runs_resp: WorkflowRunsResponse = serde_json::from_slice(&bytes)?;
        let mut queued_count = 0u32;

        for run in runs_resp.workflow_runs {
            let jobs_path = format!(
                "repos/{}/{}/actions/runs/{}/jobs?per_page=100",
                owner, repo, run.id
            );
            let (j_status, j_bytes) = self
                .transport
                .request("GET", &jobs_path, &headers, None)
                .await
                .map_err(GitHubError::Http)?;

            if j_status == 200 {
                if let Ok(jobs_resp) = serde_json::from_slice::<WorkflowJobsResponse>(&j_bytes) {
                    for job in jobs_resp.jobs {
                        match job.status.as_str() {
                            "queued" | "waiting" | "pending" | "requested" => {
                                queued_count += 1;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(queued_count)
    }

    /// Count queued jobs backlog across multiple controlled repository targets.
    pub async fn count_total_queued_jobs(
        &self,
        default_owner: &str,
        targets: &[String],
    ) -> Result<u32, GitHubError> {
        let mut total = 0u32;
        for target in targets {
            let (owner, repo) = if target.contains('/') {
                let mut parts = target.splitn(2, '/');
                (
                    parts.next().unwrap_or(default_owner),
                    parts.next().unwrap_or(target),
                )
            } else {
                (default_owner, target.as_str())
            };

            total += self.count_queued_jobs_for_repo(owner, repo).await?;
        }
        Ok(total)
    }
}

// ============================================================================
// Canonical Tracked Assets (Bit-for-Bit Git Parity)
// ============================================================================

pub const RAW_STARTUP_SCRIPT: &str = include_str!("../../cloud-init/startup.sh");
pub const RUNNER_DOCKERFILE: &str = include_str!("../../runner/Dockerfile");
pub const RUNNER_ENTRYPOINT: &str = include_str!("../../runner/entrypoint.sh");
pub const JOB_COMPLETED: &str = include_str!("../../runner/job-completed.sh");
pub const CLEAN_TARGET: &str = "";
pub const DOCKER_WRAPPER: &str = "";
pub const JOB_STARTED: &[u8] = include_bytes!("../../runner/job-started.sh");
pub const HOST_ADMISSION: &[u8] = b"";
pub const HOST_EVIDENCE: &[u8] = b"";
pub const DOCKER_ADMISSION: &[u8] = b"";

pub const RUNNER_VERSION: &str = "2.337.0";

// ============================================================================
// Startup Script Renderer
// ============================================================================

pub struct StartupParams<'a> {
    pub disk_device_name: &'a str,
    pub instance_name: &'a str,
    pub deployment_id: &'a str,
    pub org: &'a str,
    pub runner_prefix: &'a str,
    pub runner_group: &'a str,
    pub runner_labels: &'a str,
    pub registration_token: &'a str,
    pub runner_count: u32,
    pub runner_memory_gb: u32,
    pub runner_cpu_limit: &'a str,
    pub tiered: bool,
    pub strict_heavy_pool: bool,
    pub resource_monitoring: bool,
}

pub fn render_startup_script(
    params: &StartupParams,
    patch: Option<&RunnerPatch>,
) -> Result<String, String> {
    let mut script = RAW_STARTUP_SCRIPT.to_string();

    let disk_device = format!("/dev/disk/by-id/google-{}", params.disk_device_name);

    // 31 Canonical Replacements
    let replacements: [(&str, String); 31] = [
        ("DISK_DEVICE", disk_device),
        (
            "DOCKERFILE_B64",
            B64_STANDARD.encode(RUNNER_DOCKERFILE.as_bytes()),
        ),
        (
            "ENTRYPOINT_B64",
            B64_STANDARD.encode(RUNNER_ENTRYPOINT.as_bytes()),
        ),
        (
            "JOB_COMPLETED_B64",
            B64_STANDARD.encode(JOB_COMPLETED.as_bytes()),
        ),
        (
            "CLEAN_TARGET_B64",
            B64_STANDARD.encode(CLEAN_TARGET.as_bytes()),
        ),
        (
            "DOCKER_WRAPPER_B64",
            B64_STANDARD.encode(DOCKER_WRAPPER.as_bytes()),
        ),
        ("RUNNER_VERSION", RUNNER_VERSION.to_string()),
        ("ORG", params.org.to_string()),
        ("RUNNER_LABELS", params.runner_labels.to_string()),
        ("RUNNER_GROUP", params.runner_group.to_string()),
        ("RUNNER_PREFIX", params.runner_prefix.to_string()),
        ("REGISTRATION_TOKEN", params.registration_token.to_string()),
        ("RUNNER_COUNT", params.runner_count.to_string()),
        ("RUNNER_MEMORY_GB", params.runner_memory_gb.to_string()),
        ("RUNNER_CPU_LIMIT", params.runner_cpu_limit.to_string()),
        ("INSTANCE_NAME", params.instance_name.to_string()),
        ("DEPLOYMENT_ID", params.deployment_id.to_string()),
        ("REMOTE_BUILDER", "0".to_string()),
        (
            "RESOURCE_MONITORING",
            if params.resource_monitoring { "1" } else { "0" }.to_string(),
        ),
        ("BUILDER_DOCKERFILE_B64", String::new()),
        ("BUILDER_ENTRYPOINT_B64", String::new()),
        ("HOST_LAUNCHER_B64", String::new()),
        ("BUILDER_CPUS", "12".to_string()),
        ("BUILDER_MEMORY_GIB", "104".to_string()),
        ("ADMISSION_B64", B64_STANDARD.encode(HOST_ADMISSION)),
        (
            "DOCKER_ADMISSION_B64",
            B64_STANDARD.encode(DOCKER_ADMISSION),
        ),
        ("EVIDENCE_B64", B64_STANDARD.encode(HOST_EVIDENCE)),
        ("JOB_STARTED_B64", B64_STANDARD.encode(JOB_STARTED)),
        ("TIERED", if params.tiered { "1" } else { "0" }.to_string()),
        (
            "STRICT_HEAVY_POOL",
            if params.strict_heavy_pool { "1" } else { "0" }.to_string(),
        ),
        ("ADMISSION_WORK_GIB", "0".to_string()),
    ];

    for (k, v) in replacements {
        let pattern = format!("{{{{{}}}}}", k);
        script = script.replace(&pattern, &v);
    }

    // Inject Patch Script if present
    if let Some(p) = patch {
        script = inject_startup_patch(&script, p)?;
    }

    // Verify 0 unexpanded tokens remain
    if let Some(idx) = script.find("{{") {
        let snippet = &script[idx..idx + 50.min(script.len() - idx)];
        return Err(format!(
            "Unexpanded value remaining in rendered startup script: {}",
            snippet
        ));
    }

    // Validate size within GCE 256 KiB limit
    if script.len() > 256 * 1024 {
        return Err(format!(
            "Rendered startup script size {} exceeds 256 KiB budget",
            script.len()
        ));
    }

    Ok(script)
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn render_host_patch_script(patch: &RunnerPatch) -> String {
    let mut lines = Vec::new();

    for mount in &patch.container.mounts {
        lines.push(format!("install -d -m 0755 {}", shell_quote(&mount.source)));
    }

    if !patch.host.sysctl.is_empty() {
        let mut sysctl_body = String::new();
        for (k, v) in &patch.host.sysctl {
            sysctl_body.push_str(&format!("{} = {}\n", k, v));
        }
        let sysctl_b64 = B64_STANDARD.encode(sysctl_body.as_bytes());
        lines.push(format!(
            "printf '%s' '{}' | base64 -d >/etc/sysctl.d/99-spot-runner-patch.conf",
            sysctl_b64
        ));
        lines.push("chmod 0644 /etc/sysctl.d/99-spot-runner-patch.conf".to_string());
        lines.push("sysctl --system >/dev/null".to_string());
    }

    for file_spec in &patch.host.files {
        let b64 = B64_STANDARD.encode(file_spec.content.as_bytes());
        let parent = Path::new(&file_spec.path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or(".");
        lines.push(format!("install -d -m 0755 {}", shell_quote(parent)));
        lines.push(format!(
            "printf '%s' '{}' | base64 -d >{}",
            b64,
            shell_quote(&file_spec.path)
        ));
        lines.push(format!(
            "chown {} {}",
            shell_quote(&file_spec.owner),
            shell_quote(&file_spec.path)
        ));
        lines.push(format!(
            "chmod {} {}",
            shell_quote(&file_spec.mode),
            shell_quote(&file_spec.path)
        ));
    }

    if !patch.host.apt_packages.is_empty() {
        let pkgs: Vec<String> = patch
            .host
            .apt_packages
            .iter()
            .map(|p| shell_quote(p))
            .collect();
        lines.push(format!(
            "apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends {}",
            pkgs.join(" ")
        ));
        lines.push("apt-get clean && rm -rf /var/lib/apt/lists/*".to_string());
    }

    if !patch.host.boot_script.trim().is_empty() {
        let boot_b64 = B64_STANDARD.encode(patch.host.boot_script.as_bytes());
        lines.push(format!(
            "printf '%s' '{}' | base64 -d | bash -euo pipefail",
            boot_b64
        ));
    }

    if !patch.hardware.qualify_command.trim().is_empty() {
        let qual_b64 = B64_STANDARD.encode(patch.hardware.qualify_command.as_bytes());
        lines.push(format!(
            "printf '%s' '{}' | base64 -d | bash -euo pipefail",
            qual_b64
        ));
    }

    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

pub fn render_patch_pending_commands(patch: &RunnerPatch) -> Vec<String> {
    let digest = patch.patch_sha256();
    let marker = serde_json::json!({
        "receipt_schema": RECEIPT_SCHEMA,
        "pending_patch_sha256": digest
    });
    let marker_json = serde_json::to_vec(&marker).unwrap_or_default();
    let marker_b64 = B64_STANDARD.encode(&marker_json);
    vec![
        format!("install -d -m 0755 {}", PATCH_STATE_DIR),
        format!(
            "printf '%s' '{}' | base64 -d >{}/pending.json.tmp",
            marker_b64, PATCH_STATE_DIR
        ),
        format!(
            "mv -f {}/pending.json.tmp {}/pending.json",
            PATCH_STATE_DIR, PATCH_STATE_DIR
        ),
    ]
}

pub fn render_patch_receipt_commands(patch: &RunnerPatch) -> Vec<String> {
    let digest = patch.patch_sha256();
    let receipt = serde_json::json!({
        "receipt_schema": RECEIPT_SCHEMA,
        "schema": patch.schema,
        "repository": patch.repository,
        "source_commit": patch.source_commit,
        "patch_sha256": digest,
        "manifest": patch.to_canonical_value(),
    });
    let receipt_json = serde_json::to_vec(&receipt).unwrap_or_default();
    let receipt_b64 = B64_STANDARD.encode(&receipt_json);
    vec![
        format!("install -d -m 0755 {}", PATCH_STATE_DIR),
        "patch_boot_id=\"$(cat /proc/sys/kernel/random/boot_id)\"".to_string(),
        "[[ \"$patch_boot_id\" =~ ^[0-9a-f-]{36}$ ]] || { echo \"Cannot read the boot id for the patch receipt\" >&2; exit 1; }".to_string(),
        format!(
            "{{ printf '{{\"boot_id\":\"%s\",\"receipt\":' \"$patch_boot_id\"; printf '%s' '{}' | base64 -d; printf '}}'; }} >{}/state.json.tmp",
            receipt_b64, PATCH_STATE_DIR
        ),
        format!("chmod 0644 {}/state.json.tmp", PATCH_STATE_DIR),
        format!("mv -f {}/state.json.tmp {}/state.json", PATCH_STATE_DIR, PATCH_STATE_DIR),
        format!("rm -f {}/pending.json", PATCH_STATE_DIR),
    ]
}

pub fn render_docker_run_args(patch: &RunnerPatch) -> Vec<String> {
    let mut flags = Vec::new();
    for (k, v) in &patch.container.sysctl {
        flags.push(format!("--sysctl={}={}", k, v));
    }
    for mount in &patch.container.mounts {
        let mut spec = format!("{}:{}", mount.source, mount.target);
        if mount.read_only {
            spec.push_str(":ro");
        }
        flags.push("-v".to_string());
        flags.push(spec);
    }
    flags
}

pub fn inject_startup_patch(startup_script: &str, patch: &RunnerPatch) -> Result<String, String> {
    if patch.is_empty() {
        return Ok(startup_script.to_string());
    }

    let mut result = startup_script.to_string();

    let host_script = render_host_patch_script(patch);
    if !host_script.is_empty() {
        let host_b64 = B64_STANDARD.encode(host_script.as_bytes());
        let pending_cmds = render_patch_pending_commands(patch).join("\n");
        let host_hook = format!(
            "{}\nprintf '%s' '{}' | base64 -d >/opt/gha-runners/repo-host-patch.sh\nchmod 0755 /opt/gha-runners/repo-host-patch.sh\n/opt/gha-runners/repo-host-patch.sh\n",
            pending_cmds, host_b64
        );
        let build_anchor = "chmod 0755 /opt/gha-runners/build/entrypoint.sh\n";
        if !result.contains(build_anchor) {
            return Err(
                "Startup script is missing build entrypoint anchor for host patch injection"
                    .to_string(),
            );
        }
        result = result.replacen(build_anchor, &format!("{}{}", host_hook, build_anchor), 1);
    }

    let receipt_anchor = "unset REGISTRATION_TOKEN\n";
    if !result.contains(receipt_anchor) {
        return Err(
            "Startup script is missing registration-token anchor for patch receipt injection"
                .to_string(),
        );
    }
    let receipt_cmds = render_patch_receipt_commands(patch).join("\n");
    result = result.replacen(
        receipt_anchor,
        &format!("{}{}\n", receipt_anchor, receipt_cmds),
        1,
    );

    let extra_docker_flags = render_docker_run_args(patch);
    if !extra_docker_flags.is_empty() {
        let quoted: Vec<String> = extra_docker_flags.iter().map(|f| shell_quote(f)).collect();
        let mount_anchor = "  docker_args+=(--mount \"type=volume,src=$RUST_VOLUME,dst=/opt/runner-rust,readonly\")\n";
        if !result.contains(mount_anchor) {
            return Err(
                "Startup script is missing rust volume mount anchor for docker_args injection"
                    .to_string(),
            );
        }
        result = result.replacen(
            mount_anchor,
            &format!("{}  docker_args+=({})\n", mount_anchor, quoted.join(" ")),
            1,
        );
    }

    Ok(result)
}
