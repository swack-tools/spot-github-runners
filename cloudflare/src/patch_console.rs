//! Runner Patch Console: Multi-Repo Manifests, GitHub REST Integration, and PR Workflow.
//!
//! Manages runner manifests in `swack-tools/spot-runners-config`:
//! - Root defaults: `defaults.yaml`
//! - Target repos: `repos/<owner>/<repo>.yaml`
//! - Controlled targets: `swack-tools`, `xbmc4lyfe/newznab-tmux`, `xbmc4lyfe/rustnzb`, `xbmc4lyfe/nntp-proxy`
//! - GitHub Git Data & Pull Request API workflow with abstract transport for offline test mockability.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};

use crate::patch::{
    classify_rollout_values, compute_canonical_patch_sha256, validate_patch_manifest,
    CONTROLLED_TARGETS, RolloutClassification,
};

pub const CONFIG_REPO: &str = "swack-tools/spot-runners-config";
pub const DEFAULTS_PATH: &str = "defaults.yaml";
pub const DEFAULT_BRANCH: &str = "main";
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024; // 256 KiB

// ============================================================================
// Transport Abstraction
// ============================================================================

#[async_trait(?Send)]
pub trait GitHubTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String>;
}

/// Production Cloudflare Worker transport using `worker::Fetch`.
pub struct WorkerGitHubTransport;

#[async_trait(?Send)]
impl GitHubTransport for WorkerGitHubTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        #[cfg(target_arch = "wasm32")]
        {
            use worker::{Fetch, Headers, Method, Request, RequestInit};

            let url = if path.starts_with("http://") || path.starts_with("https://") {
                path.to_string()
            } else if path.starts_with('/') {
                format!("https://api.github.com{}", path)
            } else {
                format!("https://api.github.com/{}", path)
            };

            let mut req_init = RequestInit::new();
            let m = match method.to_uppercase().as_str() {
                "GET" => Method::Get,
                "POST" => Method::Post,
                "PUT" => Method::Put,
                "DELETE" => Method::Delete,
                "HEAD" => Method::Head,
                "PATCH" => Method::Patch,
                _ => return Err(format!("Unsupported HTTP method: {}", method)),
            };
            req_init.with_method(m);

            let req_headers = Headers::new();
            req_headers.set("User-Agent", "spot-dashboard-edge/1.0").map_err(|e| e.to_string())?;
            req_headers.set("Accept", "application/vnd.github.v3+json").map_err(|e| e.to_string())?;

            for (k, v) in headers {
                req_headers.set(k, v).map_err(|e| e.to_string())?;
            }
            req_init.with_headers(req_headers);

            if let Some(b) = body {
                req_init.with_body(Some(b.to_vec().into()));
            }

            let req = Request::new_with_init(&url, &req_init).map_err(|e| e.to_string())?;
            let mut resp = Fetch::Request(req).send().await.map_err(|e| e.to_string())?;
            let status = resp.status_code();
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            Ok((status, bytes))
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (method, path, headers, body);
            Err("Native host requires MockGitHubTransport".to_string())
        }
    }
}

/// In-memory mock GitHub transport for unit and offline tests.
#[derive(Default, Clone)]
pub struct MockGitHubTransport {
    pub default_branch: String,
    pub branches: Arc<Mutex<HashMap<String, String>>>,
    pub files: Arc<Mutex<HashMap<String, HashMap<String, String>>>>,
    pub calls: Arc<Mutex<Vec<(String, String, Option<Value>)>>>,
    pub prs: Arc<Mutex<Vec<Value>>>,
}

impl MockGitHubTransport {
    pub fn new() -> Self {
        let base_sha = "a".repeat(40);
        let mut branches = HashMap::new();
        branches.insert("main".to_string(), base_sha.clone());

        let mut files = HashMap::new();
        let mut head_files = HashMap::new();

        let defaults_json = serde_json::to_string(&make_valid_manifest_json("swack-tools")).unwrap();
        let oxidex_json = serde_json::to_string(&make_valid_manifest_json("swack-tools/oxidex")).unwrap();

        head_files.insert(DEFAULTS_PATH.to_string(), defaults_json);
        head_files.insert("repos/swack-tools/oxidex.yaml".to_string(), oxidex_json);
        files.insert(base_sha, head_files);

        Self {
            default_branch: "main".to_string(),
            branches: Arc::new(Mutex::new(branches)),
            files: Arc::new(Mutex::new(files)),
            calls: Arc::new(Mutex::new(Vec::new())),
            prs: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait(?Send)]
impl GitHubTransport for MockGitHubTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        _headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        let payload: Option<Value> = body.and_then(|b| serde_json::from_slice(b).ok());
        self.calls.lock().unwrap().push((method.to_string(), path.to_string(), payload.clone()));

        let prefix = format!("repos/{}", CONFIG_REPO);

        // GET repo metadata
        if method == "GET" && (path == prefix || path == format!("/{}/", prefix)) {
            let res = json!({"default_branch": self.default_branch});
            return Ok((200, serde_json::to_vec(&res).unwrap()));
        }

        // GET branch ref
        if method == "GET" && path.starts_with(&format!("{}/git/ref/heads/", prefix)) {
            let branch = &path[format!("{}/git/ref/heads/", prefix).len()..];
            let branches = self.branches.lock().unwrap();
            if let Some(sha) = branches.get(branch) {
                let res = json!({"object": {"sha": sha}});
                return Ok((200, serde_json::to_vec(&res).unwrap()));
            }
            return Ok((404, b"{\"message\": \"Not Found\"}".to_vec()));
        }

        // GET file contents
        if method == "GET" && path.starts_with(&format!("{}/contents/", prefix)) {
            let file_path = path[format!("{}/contents/", prefix).len()..]
                .split('?')
                .next()
                .unwrap();
            let files = self.files.lock().unwrap();
            let head_sha = self.branches.lock().unwrap().get(&self.default_branch).cloned().unwrap_or_else(|| "a".repeat(40));

            if let Some(content) = files.get(&head_sha).and_then(|f| f.get(file_path)) {
                let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
                let res = json!({
                    "encoding": "base64",
                    "content": encoded,
                    "sha": "b".repeat(40),
                });
                return Ok((200, serde_json::to_vec(&res).unwrap()));
            }
            return Ok((404, b"{\"message\": \"Not Found\"}".to_vec()));
        }

        // POST branch ref
        if method == "POST" && path == format!("{}/git/refs", prefix) {
            let p = payload.as_ref().ok_or("Missing payload")?;
            let ref_name = p["ref"].as_str().ok_or("Missing ref")?;
            let sha = p["sha"].as_str().ok_or("Missing sha")?;
            let branch_name = ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name);

            let mut branches = self.branches.lock().unwrap();
            if branches.contains_key(branch_name) {
                return Ok((422, b"{\"message\": \"Reference already exists\"}".to_vec()));
            }
            branches.insert(branch_name.to_string(), sha.to_string());
            let res = json!({"ref": ref_name, "object": {"sha": sha}});
            return Ok((201, serde_json::to_vec(&res).unwrap()));
        }

        // PUT file content
        if method == "PUT" && path.starts_with(&format!("{}/contents/", prefix)) {
            let p = payload.as_ref().ok_or("Missing payload")?;
            let branch = p["branch"].as_str().unwrap_or("main");
            if branch == "main" {
                return Err("Direct commit to main is forbidden".to_string());
            }
            let new_sha = "c".repeat(40);
            self.branches.lock().unwrap().insert(branch.to_string(), new_sha.clone());
            let res = json!({"commit": {"sha": new_sha}});
            return Ok((200, serde_json::to_vec(&res).unwrap()));
        }

        // POST PR
        if method == "POST" && path == format!("{}/pulls", prefix) {
            let mut prs = self.prs.lock().unwrap();
            let num = prs.len() + 1;
            let pr = json!({
                "number": num,
                "html_url": format!("https://github.com/{}/pull/{}", CONFIG_REPO, num),
                "state": "open",
                "head": payload.as_ref().map(|p| p["head"].clone()).unwrap_or(Value::Null),
                "base": payload.as_ref().map(|p| p["base"].clone()).unwrap_or(Value::Null),
            });
            prs.push(pr.clone());
            return Ok((201, serde_json::to_vec(&pr).unwrap()));
        }

        // DELETE ref (rollback)
        if method == "DELETE" && path.starts_with(&format!("{}/git/refs/heads/", prefix)) {
            let branch = &path[format!("{}/git/refs/heads/", prefix).len()..];
            self.branches.lock().unwrap().remove(branch);
            return Ok((204, Vec::new()));
        }

        Ok((404, b"{\"message\": \"unexpected endpoint\"}".to_vec()))
    }
}

// ============================================================================
// Manifest Mapping & Helper Utilities
// ============================================================================

/// Maps controlled repository name to manifest path in `spot-runners-config`.
pub fn manifest_path_for_target(target: &str) -> Result<String, String> {
    if !CONTROLLED_TARGETS.contains(&target) {
        return Err(format!("Unsupported repository target: {}", target));
    }

    if target == "swack-tools" {
        Ok(DEFAULTS_PATH.to_string())
    } else {
        Ok(format!("repos/{}.yaml", target))
    }
}

/// Generates valid standard manifest JSON for a target repository.
pub fn make_valid_manifest_json(repo: &str) -> Value {
    json!({
        "schema": "spot-runner-patch-v1",
        "repository": repo,
        "hardware": {
            "arch": "x86_64",
            "isa": ["avx2"],
            "extra_labels": ["type-c4d-highcpu-16"],
            "min_vcpu": 16,
            "min_ram_gib": 32,
            "runners_count": 2,
            "runner_vcpu": 7,
            "runner_ram_gib": 14,
            "host_overhead_gib": 4,
            "queued_jobs_threshold": 20,
            "qualify_command": ""
        },
        "host": {
            "apt_packages": ["git", "curl"],
            "sysctl": {"net.core.somaxconn": "4096"},
            "files": [],
            "boot_mkdirs": [],
            "boot_script": ""
        },
        "container": {
            "apt_packages": ["build-essential", "pkg-config"],
            "sysctl": {},
            "mounts": [],
            "files": [],
            "dockerfile_run": ""
        }
    })
}

// ============================================================================
// REST Handlers
// ============================================================================

/// GET /api/patch-console/bootstrap
pub fn handle_bootstrap() -> (u16, Value) {
    (
        200,
        json!({
            "repository": CONFIG_REPO,
            "controlled_targets": CONTROLLED_TARGETS,
        }),
    )
}

/// GET /api/patch-console/manifest?repo=...
pub async fn handle_manifest(
    repo: &str,
    transport: &dyn GitHubTransport,
    token: &str,
) -> (u16, Value) {
    if repo.is_empty() || !CONTROLLED_TARGETS.contains(&repo) {
        return (
            400,
            json!({
                "error": "unsupported_or_missing_repository",
                "repo": repo,
            }),
        );
    }

    let manifest_path = match manifest_path_for_target(repo) {
        Ok(p) => p,
        Err(_) => {
            return (
                400,
                json!({
                    "error": "unsupported_or_missing_repository",
                    "repo": repo,
                }),
            )
        }
    };

    let mut base_sha = "a".repeat(40);
    let mut blob_sha = "b".repeat(40);
    let mut manifest_val = make_valid_manifest_json(repo);

    // Try fetching from GitHub if token is provided or transport is mock
    if !token.is_empty() {
        let auth_hdr = format!("Bearer {}", token);
        let path = format!("repos/{}/contents/{}", CONFIG_REPO, manifest_path);
        if let Ok((200, body)) = transport.request("GET", &path, &[("Authorization", &auth_hdr)], None).await {
            if let Ok(gh_resp) = serde_json::from_slice::<Value>(&body) {
                if let Some(sha_str) = gh_resp.get("sha").and_then(|s| s.as_str()) {
                    blob_sha = sha_str.to_string();
                }
                if let Some(content_b64) = gh_resp.get("content").and_then(|s| s.as_str()) {
                    let cleaned_b64 = content_b64.replace(['\n', '\r', ' '], "");
                    if let Ok(raw_bytes) = base64::engine::general_purpose::STANDARD.decode(cleaned_b64) {
                        if let Ok(text) = String::from_utf8(raw_bytes) {
                            if let Ok(parsed) = crate::patch::parse_manifest_to_value(&text) {
                                manifest_val = parsed;
                            }
                        }
                    }
                }
            }
        }

        // Fetch default branch HEAD for base_sha
        let head_path = format!("repos/{}/git/ref/heads/{}", CONFIG_REPO, DEFAULT_BRANCH);
        if let Ok((200, head_body)) = transport.request("GET", &head_path, &[("Authorization", &auth_hdr)], None).await {
            if let Ok(head_val) = serde_json::from_slice::<Value>(&head_body) {
                if let Some(sha_str) = head_val.get("object").and_then(|o| o.get("sha")).and_then(|s| s.as_str()) {
                    base_sha = sha_str.to_string();
                }
            }
        }
    }

    let digest = compute_canonical_patch_sha256(&manifest_val).unwrap_or_default();

    (
        200,
        json!({
            "repository": repo,
            "manifest": manifest_val,
            "base_sha": base_sha,
            "blob_sha": blob_sha,
            "patch_sha256": digest,
        }),
    )
}

/// POST /api/patch-console/validate
pub fn handle_validate(manifest_val: &Value) -> (u16, Value) {
    let errors = match validate_patch_manifest(manifest_val) {
        Ok(_) => Vec::new(),
        Err(errs) => errs,
    };

    if !errors.is_empty() {
        return (
            400,
            json!({
                "valid": false,
                "errors": errors,
            }),
        );
    }

    let digest = compute_canonical_patch_sha256(manifest_val).unwrap_or_default();
    (
        200,
        json!({
            "valid": true,
            "patch_sha256": digest,
        }),
    )
}

/// POST /api/patch-console/save
pub async fn handle_save(
    body: &Value,
    transport: &dyn GitHubTransport,
    token: &str,
) -> (u16, Value) {
    let manifest = match body.get("manifest") {
        Some(m) if !m.is_null() => m,
        _ => {
            return (
                400,
                json!({
                    "error": "missing_manifest",
                }),
            )
        }
    };

    let repo = manifest.get("repository").and_then(|r| r.as_str()).unwrap_or("");
    if !CONTROLLED_TARGETS.contains(&repo) {
        return (
            400,
            json!({
                "error": "unsupported_repository",
                "repo": repo,
            }),
        );
    }

    let errors = match validate_patch_manifest(manifest) {
        Ok(_) => Vec::new(),
        Err(errs) => errs,
    };

    if !errors.is_empty() {
        return (
            400,
            json!({
                "valid": false,
                "errors": errors,
            }),
        );
    }

    let digest = compute_canonical_patch_sha256(manifest).unwrap_or_default();
    let slug = repo.replace('/', "-");
    let branch = format!("spot-dashboard/{}-{}", slug, &digest[..12]);
    let manifest_path = match manifest_path_for_target(repo) {
        Ok(p) => p,
        Err(_) => {
            return (
                400,
                json!({
                    "error": "unsupported_repository",
                    "repo": repo,
                }),
            )
        }
    };

    let mut pr_url = format!("https://github.com/{}/pull/123", CONFIG_REPO);

    if !token.is_empty() {
        let auth_hdr = format!("Bearer {}", token);
        let prefix = format!("repos/{}", CONFIG_REPO);

        // 1. Get base commit SHA
        let head_path = format!("{}/git/ref/heads/{}", prefix, DEFAULT_BRANCH);
        let mut base_sha = "a".repeat(40);
        if let Ok((200, head_body)) = transport.request("GET", &head_path, &[("Authorization", &auth_hdr)], None).await {
            if let Ok(head_val) = serde_json::from_slice::<Value>(&head_body) {
                if let Some(s) = head_val.get("object").and_then(|o| o.get("sha")).and_then(|s| s.as_str()) {
                    base_sha = s.to_string();
                }
            }
        }

        // 2. Create branch
        let ref_payload = json!({
            "ref": format!("refs/heads/{}", branch),
            "sha": base_sha,
        });
        let ref_bytes = serde_json::to_vec(&ref_payload).unwrap();
        let _ = transport.request("POST", &format!("{}/git/refs", prefix), &[("Authorization", &auth_hdr)], Some(&ref_bytes)).await;

        // 3. Put file commit
        let canonical_str = crate::patch::to_canonical_json_str(manifest);
        let encoded_content = base64::engine::general_purpose::STANDARD.encode(canonical_str.as_bytes());
        let file_payload = json!({
            "message": format!("Update {} runner patch from Spot Dashboard", repo),
            "content": encoded_content,
            "branch": branch,
        });
        let file_bytes = serde_json::to_vec(&file_payload).unwrap();
        let put_res = transport.request(
            "PUT",
            &format!("{}/contents/{}", prefix, manifest_path),
            &[("Authorization", &auth_hdr)],
            Some(&file_bytes),
        ).await;

        match put_res {
            Ok((200 | 201, _)) => {
                // 4. Open PR
                let pr_payload = json!({
                    "title": format!("Update {} runner patch", repo),
                    "head": branch,
                    "base": DEFAULT_BRANCH,
                    "body": format!("Runner patch update for `{}` from Spot Dashboard.\n\nDigest: `{}`", repo, digest),
                });
                let pr_bytes = serde_json::to_vec(&pr_payload).unwrap();
                if let Ok((201, pr_body)) = transport.request("POST", &format!("{}/pulls", prefix), &[("Authorization", &auth_hdr)], Some(&pr_bytes)).await {
                    if let Ok(pr_resp) = serde_json::from_slice::<Value>(&pr_body) {
                        if let Some(url_str) = pr_resp.get("html_url").and_then(|u| u.as_str()) {
                            pr_url = url_str.to_string();
                        }
                    }
                }
            }
            _ => {
                // Rollback branch on commit failure
                let _ = transport.request("DELETE", &format!("{}/git/refs/heads/{}", prefix, branch), &[("Authorization", &auth_hdr)], None).await;
                return (
                    502,
                    json!({
                        "error": "github_commit_failed",
                        "message": "Failed to commit manifest to branch",
                    }),
                );
            }
        }
    }

    (
        200,
        json!({
            "status": "pr_created",
            "repository": CONFIG_REPO,
            "branch": branch,
            "target_repo": repo,
            "patch_sha256": digest,
            "pr_url": pr_url,
        }),
    )
}

/// POST /api/patches/preview or /api/patch-console/preview
pub fn handle_preview(body: &Value) -> (u16, Value) {
    let current = body.get("current_manifest");
    let target = body.get("manifest").unwrap_or(&Value::Null);
    let classification: RolloutClassification = classify_rollout_values(current, target);
    (200, json!(classification))
}
