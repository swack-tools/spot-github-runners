//! GCP Service Account Authentication and GCE REST Compute Client for Cloudflare Worker.
//!
//! Subsystems:
//! 1. GCP Service Account Authentication:
//!    - Credentials JSON parsing (`client_email`, `private_key` PKCS#8 PEM, `token_uri`, `project_id`).
//!    - RS256 JWT assertion generation (`exp = now + 3600`, `iat = now`).
//!    - Deterministic RSASSA-PKCS1-v1_5 SHA-256 signing via `rsa = 0.9.10` (no runtime RNG needed).
//!    - Token exchange with `https://oauth2.googleapis.com/token`.
//!    - In-isolate Wasm memory caching (`thread_local!`) with 55m TTL (3300s): strictly 0 KV ops.
//! 2. GCE REST Compute Engine Client:
//!    - `instances.aggregatedList` with pagination and `returnPartialSuccess=true`.
//!    - `disks.aggregatedList` to discover attached vs orphan disks.
//!    - `instances.insert`:
//!      - Optional `?sourceInstanceTemplate=...` query parameter.
//!      - Overrides `machineType` (>= 16 vCPU / 32 GiB), `zone`, labels, and metadata (`startup-script`).
//!      - Fast-boot golden image (`source_image` / `golden_image_family`) with clean Debian 13 fallback.
//!    - `instances.delete` with `deleteDisk=ALL`.
//!    - `instances.setMetadata` scrubbing `startup-script` once VM is running.
//!    - `disks.delete` for orphan disk cleanup.
//! 3. Decoupled `GcpHttpTransport` trait supporting native unit testing and `worker::Fetch`.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::RsaPrivateKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
pub const COMPUTE_API_BASE: &str = "https://compute.googleapis.com/compute/v1";
pub const GCP_OAUTH_SCOPE: &str =
    "https://www.googleapis.com/auth/compute https://www.googleapis.com/auth/cloud-platform";
/// Bootstrap credentials removed from instance metadata once the host is online. Jobs on the host can
/// read instance attributes, so the startup script (registration token) and the unused JIT runner
/// configs must not outlive the boot that needed them.
pub const SCRUBBED_BOOTSTRAP_METADATA_KEYS: &[&str] =
    &["startup-script", "runner_1_jitconfig", "runner_2_jitconfig"];
pub const TOKEN_CACHE_TTL_SECS: u64 = 3300; // 55 minutes
pub const JWT_EXPIRY_SECS: u64 = 3600; // 1 hour
pub const DEFAULT_BOOT_DISK_SIZE_GB: u64 = 40;
pub const DEBIAN_13_IMAGE_FAMILY: &str = "projects/debian-cloud/global/images/family/debian-13";
pub const DEBIAN_13_ARM64_IMAGE_FAMILY: &str =
    "projects/debian-cloud/global/images/family/debian-13-arm64";

// ============================================================================
// Service Account Credentials & Auth Models
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcpCredentials {
    pub client_email: String,
    pub private_key: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
    #[serde(default)]
    pub project_id: Option<String>,
}

fn default_token_uri() -> String {
    DEFAULT_TOKEN_URI.to_string()
}

#[derive(Debug, Serialize)]
struct GcpJwtHeader<'a> {
    alg: &'a str,
    typ: &'a str,
}

#[derive(Debug, Serialize)]
struct GcpJwtClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    exp: u64,
    iat: u64,
}

#[derive(Debug, Deserialize)]
pub struct GcpTokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    pub token_type: String,
}

// ============================================================================
// GCE Compute API Models
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GceMetadataItem {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GceMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub items: Vec<GceMetadataItem>,
}

impl GceMetadata {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|i| i.key == key)
            .map(|i| i.value.as_str())
    }

    pub fn without_key(&self, key: &str) -> Self {
        self.without_keys(&[key])
    }

    pub fn without_keys(&self, keys: &[&str]) -> Self {
        Self {
            fingerprint: self.fingerprint.clone(),
            items: self
                .items
                .iter()
                .filter(|i| !keys.contains(&i.key.as_str()))
                .cloned()
                .collect(),
        }
    }
}

pub const QUERY_VALUE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'/')
    .add(b':')
    .add(b'?')
    .add(b'&')
    .add(b'=');

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GceAttachedDisk {
    #[serde(default)]
    pub boot: bool,
    #[serde(default = "default_auto_delete", alias = "auto_delete")]
    pub auto_delete: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "device_name"
    )]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "disk_size_gb"
    )]
    pub disk_size_gb: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "initialize_params"
    )]
    pub initialize_params: Option<GceDiskInitializeParams>,
}

fn default_auto_delete() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GceDiskInitializeParams {
    #[serde(alias = "disk_name")]
    pub disk_name: String,
    #[serde(alias = "disk_size_gb")]
    pub disk_size_gb: String,
    #[serde(alias = "disk_type")]
    pub disk_type: String,
    #[serde(skip_serializing_if = "Option::is_none", alias = "source_image")]
    pub source_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GceScheduling {
    #[serde(alias = "provisioning_model")]
    pub provisioning_model: String, // "SPOT"
    #[serde(alias = "instance_termination_action")]
    pub instance_termination_action: String, // "DELETE"
    #[serde(alias = "on_host_maintenance")]
    pub on_host_maintenance: String, // "TERMINATE"
    #[serde(default, alias = "automatic_restart")]
    pub automatic_restart: bool, // false
}

impl Default for GceScheduling {
    fn default() -> Self {
        Self {
            provisioning_model: "SPOT".to_string(),
            instance_termination_action: "DELETE".to_string(),
            on_host_maintenance: "TERMINATE".to_string(),
            automatic_restart: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GceNetworkInterface {
    pub network: String,
    #[serde(skip_serializing_if = "Option::is_none", alias = "nic_type")]
    pub nic_type: Option<String>,
    #[serde(default, alias = "access_configs")]
    pub access_configs: Vec<GceAccessConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GceAccessConfig {
    pub name: String,
    pub r#type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GceInstance {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub zone: String,
    #[serde(alias = "machine_type")]
    pub machine_type: String,
    pub status: String,
    #[serde(default, alias = "cpu_platform")]
    pub cpu_platform: Option<String>,
    #[serde(default, alias = "creation_timestamp")]
    pub creation_timestamp: Option<String>,
    #[serde(default, alias = "last_start_timestamp")]
    pub last_start_timestamp: Option<String>,
    #[serde(default, alias = "last_stop_timestamp")]
    pub last_stop_timestamp: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub metadata: GceMetadata,
    #[serde(default)]
    pub disks: Vec<GceAttachedDisk>,
    #[serde(default)]
    pub scheduling: Option<GceScheduling>,
}

impl GceInstance {
    pub fn is_running(&self) -> bool {
        self.status.eq_ignore_ascii_case("RUNNING")
    }

    pub fn is_terminated(&self) -> bool {
        self.status.eq_ignore_ascii_case("TERMINATED")
    }

    pub fn zone_short(&self) -> &str {
        self.zone.rsplit('/').next().unwrap_or(&self.zone)
    }

    pub fn machine_type_short(&self) -> &str {
        self.machine_type
            .rsplit('/')
            .next()
            .unwrap_or(&self.machine_type)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GceDisk {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub zone: String,
    #[serde(default, alias = "size_gb")]
    pub size_gb: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default, alias = "self_link")]
    pub self_link: String,
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default, alias = "creation_timestamp")]
    pub creation_timestamp: Option<String>,
}

impl GceDisk {
    pub fn is_orphan(&self) -> bool {
        self.users.is_empty()
    }

    pub fn zone_short(&self) -> &str {
        self.zone.rsplit('/').next().unwrap_or(&self.zone)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GceServiceAccount {
    pub email: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GceInsertInstanceRequest {
    pub name: String,
    #[serde(alias = "machine_type")]
    pub machine_type: String,
    pub scheduling: GceScheduling,
    pub disks: Vec<GceAttachedDisk>,
    #[serde(alias = "network_interfaces")]
    pub network_interfaces: Vec<GceNetworkInterface>,
    pub metadata: GceMetadata,
    pub labels: BTreeMap<String, String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        alias = "service_accounts"
    )]
    pub service_accounts: Vec<GceServiceAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GceOperation {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub status: String, // "PENDING", "RUNNING", "DONE"
    #[serde(default)]
    pub target_link: Option<String>,
    #[serde(default)]
    pub error: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GceAggregatedListEnvelope<T> {
    #[serde(default)]
    pub items: BTreeMap<String, T>,
    #[serde(default, alias = "next_page_token")]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub unreachables: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct GceInstancesScopedList {
    #[serde(default)]
    pub instances: Vec<GceInstance>,
}

#[derive(Debug, Deserialize, Default)]
pub struct GceDisksScopedList {
    #[serde(default)]
    pub disks: Vec<GceDisk>,
}

// ============================================================================
// Error Types
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum GcpError {
    #[error("Credentials error: {0}")]
    Credentials(String),
    #[error("RSA/Cryptographic error: {0}")]
    Rsa(String),
    #[error("Token exchange failed (HTTP {0}): {1}")]
    TokenExchange(u16, String),
    #[error("Compute API HTTP {0} for {1}: {2}")]
    Api(u16, String, String),
    #[error("HTTP transport error: {0}")]
    Transport(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Validation error: {0}")]
    Validation(String),
}

// ============================================================================
// HTTP Transport Trait & Cloudflare Worker Implementation
// ============================================================================

#[async_trait::async_trait(?Send)]
pub trait GcpHttpTransport {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<(u16, Vec<u8>), GcpError>;
}

pub struct WorkerGcpTransport;

#[async_trait::async_trait(?Send)]
impl GcpHttpTransport for WorkerGcpTransport {
    async fn request(
        &self,
        method_str: &str,
        url: &str,
        headers_slice: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<(u16, Vec<u8>), GcpError> {
        #[cfg(target_arch = "wasm32")]
        {
            use worker::*;
            let method = match method_str {
                "GET" => Method::Get,
                "POST" => Method::Post,
                "DELETE" => Method::Delete,
                "PUT" => Method::Put,
                _ => {
                    return Err(GcpError::Transport(format!(
                        "Unsupported method: {}",
                        method_str
                    )))
                }
            };

            let mut init = RequestInit::new();
            init.with_method(method);

            let headers = Headers::new();
            for (k, v) in headers_slice {
                headers
                    .set(k, v)
                    .map_err(|e| GcpError::Transport(e.to_string()))?;
            }
            init.with_headers(headers);

            if let Some(b) = body {
                init.with_body(Some(b.into()));
            }

            let req = Request::new_with_init(url, &init)
                .map_err(|e| GcpError::Transport(e.to_string()))?;
            let mut resp = Fetch::Request(req)
                .send()
                .await
                .map_err(|e| GcpError::Transport(e.to_string()))?;

            let status = resp.status_code();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| GcpError::Transport(e.to_string()))?;

            Ok((status, bytes))
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (method_str, url, headers_slice, body);
            Err(GcpError::Transport(
                "Native test environment requires MockGcpHttpTransport".to_string(),
            ))
        }
    }
}

/// In-memory mock GCP HTTP transport for offline unit tests.
#[derive(Default, Clone)]
pub struct MockGcpHttpTransport {
    pub responses: Arc<Mutex<BTreeMap<String, (u16, Vec<u8>)>>>,
    pub calls: Arc<Mutex<Vec<(String, String, Option<Vec<u8>>)>>>,
    pub default_token: Option<String>,
}

impl MockGcpHttpTransport {
    pub fn new() -> Self {
        Self {
            responses: Arc::new(Mutex::new(BTreeMap::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
            default_token: Some("ya29.mock-token-xyz".to_string()),
        }
    }

    pub fn set_response(&self, url_pattern: &str, status: u16, body: Vec<u8>) {
        self.responses
            .lock()
            .unwrap()
            .insert(url_pattern.to_string(), (status, body));
    }

    pub fn set_json_response<S: Serialize>(&self, url_pattern: &str, status: u16, data: &S) {
        let body = serde_json::to_vec(data).expect("serialize mock response");
        self.set_response(url_pattern, status, body);
    }
}

#[async_trait::async_trait(?Send)]
impl GcpHttpTransport for MockGcpHttpTransport {
    async fn request(
        &self,
        method: &str,
        url: &str,
        _headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<(u16, Vec<u8>), GcpError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), url.to_string(), body.clone()));

        // Check registered responses (longer pattern matches first)
        {
            let resps = self.responses.lock().unwrap();
            let mut matches: Vec<_> = resps.iter().filter(|(pat, _)| url.contains(*pat)).collect();
            matches.sort_by_key(|(pat, _)| std::cmp::Reverse(pat.len()));
            if let Some((_, (status, resp_body))) = matches.first() {
                return Ok((*status, (*resp_body).clone()));
            }
        }

        // Automatic mock handler for token endpoint
        if url.contains("/oauth2.googleapis.com/token") || url.contains("token") {
            let tok = self
                .default_token
                .as_deref()
                .unwrap_or("ya29.mock-token-xyz");
            let resp = serde_json::json!({
                "access_token": tok,
                "expires_in": 3600,
                "token_type": "Bearer"
            });
            return Ok((200, serde_json::to_vec(&resp)?));
        }

        // Default 200 with empty JSON or operation
        let default_op = serde_json::json!({
            "id": "12345",
            "name": "operation-12345",
            "status": "DONE"
        });
        Ok((200, serde_json::to_vec(&default_op)?))
    }
}

// ============================================================================
// RS256 JWT Assertion & Token Acquisition
// ============================================================================

pub fn mint_rs256_assertion(creds: &GcpCredentials, now: u64) -> Result<String, GcpError> {
    // 1. Prepare Header and Claims
    let header = GcpJwtHeader {
        alg: "RS256",
        typ: "JWT",
    };
    let claims = GcpJwtClaims {
        iss: &creds.client_email,
        scope: GCP_OAUTH_SCOPE,
        aud: &creds.token_uri,
        exp: now + JWT_EXPIRY_SECS,
        iat: now,
    };

    let header_json = serde_json::to_vec(&header)?;
    let claims_json = serde_json::to_vec(&claims)?;

    let header_b64 = URL_SAFE_NO_PAD.encode(&header_json);
    let claims_b64 = URL_SAFE_NO_PAD.encode(&claims_json);
    let signing_input = format!("{}.{}", header_b64, claims_b64);

    // 2. Parse PKCS#8 or PKCS#1 RSA Private Key
    let key = RsaPrivateKey::from_pkcs8_pem(&creds.private_key)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(&creds.private_key))
        .map_err(|e| GcpError::Rsa(format!("Failed to parse private key PEM: {}", e)))?;

    // 3. Sign using deterministic PKCS#1 v1.5 SHA-256 (no runtime RNG needed)
    let signing_key = SigningKey::<Sha256>::new(key);
    let signature = signing_key.sign(signing_input.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

    Ok(format!("{}.{}", signing_input, sig_b64))
}

pub async fn exchange_jwt_for_access_token<T: GcpHttpTransport>(
    transport: &T,
    creds: &GcpCredentials,
    assertion: &str,
) -> Result<String, GcpError> {
    let form_body = format!(
        "grant_type={}&assertion={}",
        percent_encoding::utf8_percent_encode(
            "urn:ietf:params:oauth:grant-type:jwt-bearer",
            percent_encoding::NON_ALPHANUMERIC
        ),
        percent_encoding::utf8_percent_encode(assertion, percent_encoding::NON_ALPHANUMERIC)
    );

    let headers = [
        ("Content-Type", "application/x-www-form-urlencoded"),
        ("Accept", "application/json"),
        ("User-Agent", "spot-dashboard-edge/0.1.0"),
    ];

    let (status, resp_bytes) = transport
        .request(
            "POST",
            &creds.token_uri,
            &headers,
            Some(form_body.into_bytes()),
        )
        .await?;

    if status != 200 {
        let err_msg = String::from_utf8_lossy(&resp_bytes).into_owned();
        return Err(GcpError::TokenExchange(status, err_msg));
    }

    let token_resp: GcpTokenResponse = serde_json::from_slice(&resp_bytes)?;
    Ok(token_resp.access_token)
}

pub async fn get_cached_or_fresh_gcp_token<T: GcpHttpTransport>(
    transport: &T,
    creds: &GcpCredentials,
    now: u64,
) -> Result<String, GcpError> {
    // 1. Check in-isolate Wasm memory cache (<0.01 ms, 0 KV reads, 0 KV writes)
    if let Some(cached_token) = crate::state::get_cached_gcp_token(now) {
        return Ok(cached_token);
    }

    // 2. Cache miss: Mint RS256 JWT and exchange with Google OAuth2
    let assertion = mint_rs256_assertion(creds, now)?;
    let access_token = exchange_jwt_for_access_token(transport, creds, &assertion).await?;

    // 3. Store in in-isolate Wasm memory cache (0 KV writes!)
    crate::state::set_cached_gcp_token(now, access_token.clone());

    Ok(access_token)
}

// ============================================================================
// GceComputeClient
// ============================================================================

pub struct GceComputeClient<T: GcpHttpTransport> {
    pub credentials: GcpCredentials,
    pub project: String,
    pub transport: T,
}

impl<T: GcpHttpTransport> GceComputeClient<T> {
    pub fn new(credentials: GcpCredentials, project: String, transport: T) -> Self {
        Self {
            credentials,
            project,
            transport,
        }
    }

    async fn auth_headers(&self, now: u64) -> Result<Vec<(&'static str, String)>, GcpError> {
        let token = get_cached_or_fresh_gcp_token(&self.transport, &self.credentials, now).await?;
        Ok(vec![
            ("Authorization", format!("Bearer {}", token)),
            ("Accept", "application/json".to_string()),
            ("User-Agent", "spot-dashboard-edge/0.1.0".to_string()),
        ])
    }

    /// List all instances across all zones via instances.aggregatedList with pagination.
    pub async fn list_instances(&self, now: u64) -> Result<Vec<GceInstance>, GcpError> {
        let mut instances = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut url = format!(
                "{}/projects/{}/aggregated/instances?returnPartialSuccess=true&maxResults=500",
                COMPUTE_API_BASE, self.project
            );
            if let Some(ref pt) = page_token {
                url.push_str("&pageToken=");
                url.push_str(pt);
            }

            let headers = self.auth_headers(now).await?;
            let header_refs: Vec<(&str, &str)> =
                headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

            let (status, bytes) = self
                .transport
                .request("GET", &url, &header_refs, None)
                .await?;
            if status != 200 {
                return Err(GcpError::Api(
                    status,
                    "instances.aggregatedList".into(),
                    String::from_utf8_lossy(&bytes).into(),
                ));
            }

            let page: GceAggregatedListEnvelope<GceInstancesScopedList> =
                serde_json::from_slice(&bytes)?;
            for scoped in page.items.into_values() {
                instances.extend(scoped.instances);
            }

            match page.next_page_token {
                Some(tok) if !tok.is_empty() => page_token = Some(tok),
                _ => break,
            }
        }

        Ok(instances)
    }

    /// List all disks across all zones via disks.aggregatedList with pagination.
    pub async fn list_disks(&self, now: u64) -> Result<Vec<GceDisk>, GcpError> {
        let mut disks = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut url = format!(
                "{}/projects/{}/aggregated/disks?returnPartialSuccess=true&maxResults=500",
                COMPUTE_API_BASE, self.project
            );
            if let Some(ref pt) = page_token {
                url.push_str("&pageToken=");
                url.push_str(pt);
            }

            let headers = self.auth_headers(now).await?;
            let header_refs: Vec<(&str, &str)> =
                headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

            let (status, bytes) = self
                .transport
                .request("GET", &url, &header_refs, None)
                .await?;
            if status != 200 {
                return Err(GcpError::Api(
                    status,
                    "disks.aggregatedList".into(),
                    String::from_utf8_lossy(&bytes).into(),
                ));
            }

            let page: GceAggregatedListEnvelope<GceDisksScopedList> =
                serde_json::from_slice(&bytes)?;
            for scoped in page.items.into_values() {
                disks.extend(scoped.disks);
            }

            match page.next_page_token {
                Some(tok) if !tok.is_empty() => page_token = Some(tok),
                _ => break,
            }
        }

        Ok(disks)
    }

    /// Identify orphan runner disks (unattached disks matching runner naming prefix).
    pub async fn list_orphan_runner_disks(&self, now: u64) -> Result<Vec<GceDisk>, GcpError> {
        let disks = self.list_disks(now).await?;
        let orphan_disks = disks
            .into_iter()
            .filter(|d| {
                d.is_orphan()
                    && (d.name.starts_with("oxidex-runners-")
                        || d.name.starts_with("spot-runners-")
                        || d.name.contains("-runners-"))
            })
            .collect();
        Ok(orphan_disks)
    }

    /// Insert a new Spot runner VM with optional sourceInstanceTemplate query parameter.
    pub async fn insert_instance(
        &self,
        zone: &str,
        req: &GceInsertInstanceRequest,
        source_instance_template: Option<&str>,
        now: u64,
    ) -> Result<GceOperation, GcpError> {
        let mut url = format!(
            "{}/projects/{}/zones/{}/instances",
            COMPUTE_API_BASE, self.project, zone
        );
        if let Some(template) = source_instance_template {
            let template_url = if template.starts_with("projects/") {
                template.to_string()
            } else {
                format!(
                    "projects/{}/global/instanceTemplates/{}",
                    self.project, template
                )
            };
            url.push_str("?sourceInstanceTemplate=");
            url.push_str(
                &percent_encoding::utf8_percent_encode(&template_url, QUERY_VALUE).to_string(),
            );
        }

        let body = serde_json::to_vec(req)?;
        let mut headers = self.auth_headers(now).await?;
        headers.push(("Content-Type", "application/json".to_string()));
        let header_refs: Vec<(&str, &str)> =
            headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let (status, bytes) = self
            .transport
            .request("POST", &url, &header_refs, Some(body))
            .await?;
        if status != 200 {
            return Err(GcpError::Api(
                status,
                "instances.insert".into(),
                String::from_utf8_lossy(&bytes).into(),
            ));
        }

        let op: GceOperation = serde_json::from_slice(&bytes)?;
        Ok(op)
    }

    /// Delete an instance with deleteDisk=ALL.
    pub async fn delete_instance(
        &self,
        zone: &str,
        instance: &str,
        now: u64,
    ) -> Result<GceOperation, GcpError> {
        let url = format!(
            "{}/projects/{}/zones/{}/instances/{}?deleteDisk=ALL",
            COMPUTE_API_BASE, self.project, zone, instance
        );

        let headers = self.auth_headers(now).await?;
        let header_refs: Vec<(&str, &str)> =
            headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let (status, bytes) = self
            .transport
            .request("DELETE", &url, &header_refs, None)
            .await?;
        if status != 200 {
            return Err(GcpError::Api(
                status,
                "instances.delete".into(),
                String::from_utf8_lossy(&bytes).into(),
            ));
        }

        let op: GceOperation = serde_json::from_slice(&bytes)?;
        Ok(op)
    }

    /// Scrub the startup script and JIT runner configs from instance metadata once online.
    pub async fn scrub_startup_script(
        &self,
        zone: &str,
        instance: &str,
        current_metadata: &GceMetadata,
        now: u64,
    ) -> Result<GceOperation, GcpError> {
        let url = format!(
            "{}/projects/{}/zones/{}/instances/{}/setMetadata",
            COMPUTE_API_BASE, self.project, zone, instance
        );

        let scrubbed_metadata = current_metadata.without_keys(SCRUBBED_BOOTSTRAP_METADATA_KEYS);
        let body = serde_json::to_vec(&scrubbed_metadata)?;

        let mut headers = self.auth_headers(now).await?;
        headers.push(("Content-Type", "application/json".to_string()));
        let header_refs: Vec<(&str, &str)> =
            headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let (status, bytes) = self
            .transport
            .request("POST", &url, &header_refs, Some(body))
            .await?;
        if status != 200 {
            return Err(GcpError::Api(
                status,
                "instances.setMetadata".into(),
                String::from_utf8_lossy(&bytes).into(),
            ));
        }

        let op: GceOperation = serde_json::from_slice(&bytes)?;
        Ok(op)
    }

    /// Delete an unattached orphan disk.
    pub async fn delete_disk(
        &self,
        zone: &str,
        disk: &str,
        now: u64,
    ) -> Result<GceOperation, GcpError> {
        let url = format!(
            "{}/projects/{}/zones/{}/disks/{}",
            COMPUTE_API_BASE, self.project, zone, disk
        );

        let headers = self.auth_headers(now).await?;
        let header_refs: Vec<(&str, &str)> =
            headers.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let (status, bytes) = self
            .transport
            .request("DELETE", &url, &header_refs, None)
            .await?;
        if status != 200 {
            return Err(GcpError::Api(
                status,
                "disks.delete".into(),
                String::from_utf8_lossy(&bytes).into(),
            ));
        }

        let op: GceOperation = serde_json::from_slice(&bytes)?;
        Ok(op)
    }
}

// ============================================================================
// Spot Runner VM Request Builder
// ============================================================================

pub struct RunnerVmConfig<'a> {
    pub instance_name: &'a str,
    pub machine_type: &'a str, // e.g. "c4d-highcpu-16"
    pub zone: &'a str,
    pub target: &'a str,
    pub runner_prefix: &'a str,
    pub startup_script: &'a str,
    pub patch_sha256: &'a str,
    pub patch_json: &'a str,
    pub runner_1_jitconfig: Option<&'a str>,
    pub runner_2_jitconfig: Option<&'a str>,
    pub source_image: Option<&'a str>,
    pub golden_image_family: Option<&'a str>,
    pub boot_disk_type: Option<&'a str>, // defaults to "pd-balanced"
}

pub fn build_spot_runner_vm_request(
    project: &str,
    cfg: &RunnerVmConfig,
) -> Result<GceInsertInstanceRequest, GcpError> {
    // 1. Enforce sizing floor: machine type must be allowlisted (x86_64 or arm64)
    if !crate::patch::PERMITTED_MACHINE_TYPES.contains(&cfg.machine_type)
        && !crate::patch::PERMITTED_ARM64_MACHINE_TYPES.contains(&cfg.machine_type)
    {
        return Err(GcpError::Validation(format!(
            "Machine type '{}' is not in permitted list: {:?}",
            cfg.machine_type,
            crate::patch::PERMITTED_MACHINE_TYPES
        )));
    }

    let is_arm64 = cfg.machine_type.starts_with("c4a-")
        || cfg.machine_type.starts_with("n4a-")
        || cfg.machine_type.starts_with("t2a-");
    let is_hyperdisk_native =
        cfg.machine_type.starts_with("c4a-") || cfg.machine_type.starts_with("n4a-");

    // 2. Resolve Boot Disk Image (Golden Image -> Image Family -> Debian 13 x86_64/arm64 fallback)
    let source_image = if let Some(img) = cfg.source_image {
        if img.starts_with("projects/") {
            img.to_string()
        } else {
            format!("projects/{}/global/images/{}", project, img)
        }
    } else if let Some(family) = cfg.golden_image_family {
        if family.starts_with("projects/") {
            family.to_string()
        } else {
            format!("projects/{}/global/images/family/{}", project, family)
        }
    } else if is_arm64 {
        DEBIAN_13_ARM64_IMAGE_FAMILY.to_string()
    } else {
        DEBIAN_13_IMAGE_FAMILY.to_string()
    };

    let default_disk_type = if is_hyperdisk_native {
        "hyperdisk-balanced"
    } else {
        "pd-balanced"
    };
    let boot_disk_type = cfg.boot_disk_type.unwrap_or(default_disk_type);
    let full_boot_disk_type = format!("zones/{}/diskTypes/{}", cfg.zone, boot_disk_type);
    let full_machine_type = format!("zones/{}/machineTypes/{}", cfg.zone, cfg.machine_type);

    let boot_disk = GceAttachedDisk {
        boot: true,
        auto_delete: true,
        device_name: Some(cfg.instance_name.to_string()),
        source: None,
        disk_size_gb: Some(DEFAULT_BOOT_DISK_SIZE_GB.to_string()),
        r#type: Some("PERSISTENT".to_string()),
        mode: Some("READ_WRITE".to_string()),
        interface: Some("NVME".to_string()),
        initialize_params: Some(GceDiskInitializeParams {
            disk_name: format!("{}-boot", cfg.instance_name),
            disk_size_gb: DEFAULT_BOOT_DISK_SIZE_GB.to_string(),
            disk_type: full_boot_disk_type,
            source_image: Some(source_image),
        }),
    };

    let mut metadata_items = vec![
        GceMetadataItem {
            key: "startup-script".into(),
            value: cfg.startup_script.into(),
        },
        GceMetadataItem {
            key: "spot-target".into(),
            value: cfg.target.into(),
        },
        GceMetadataItem {
            key: "spot-runner-prefix".into(),
            value: cfg.runner_prefix.into(),
        },
        GceMetadataItem {
            key: "spot-runner-count".into(),
            value: "2".into(),
        },
        GceMetadataItem {
            key: "spot-runner-patch-target".into(),
            value: cfg.target.into(),
        },
        GceMetadataItem {
            key: "spot-runner-patch-sha256".into(),
            value: cfg.patch_sha256.into(),
        },
        GceMetadataItem {
            key: "spot-runner-patch".into(),
            value: cfg.patch_json.into(),
        },
    ];

    if let Some(jit1) = cfg.runner_1_jitconfig {
        metadata_items.push(GceMetadataItem {
            key: "runner_1_jitconfig".into(),
            value: jit1.into(),
        });
    }
    if let Some(jit2) = cfg.runner_2_jitconfig {
        metadata_items.push(GceMetadataItem {
            key: "runner_2_jitconfig".into(),
            value: jit2.into(),
        });
    }

    let mut labels = BTreeMap::new();
    labels.insert(
        "spot-target".into(),
        cfg.target.replace('/', "-").to_lowercase(),
    );
    labels.insert("spot-role".into(), "cicd_runner".into());

    let nic_type = if boot_disk_type == "hyperdisk-balanced" {
        Some("GVNIC".to_string())
    } else {
        None
    };

    let network = GceNetworkInterface {
        network: "global/networks/default".to_string(),
        nic_type,
        access_configs: vec![GceAccessConfig {
            name: "External NAT".to_string(),
            r#type: "ONE_TO_ONE_NAT".to_string(),
        }],
    };

    Ok(GceInsertInstanceRequest {
        name: cfg.instance_name.to_string(),
        machine_type: full_machine_type,
        scheduling: GceScheduling::default(),
        disks: vec![boot_disk],
        network_interfaces: vec![network],
        metadata: GceMetadata {
            fingerprint: None,
            items: metadata_items,
        },
        labels,
        service_accounts: Vec::new(),
    })
}

pub const DEFAULT_BUILDER_CACHE_DISK_SIZE_GB: u64 = 375;
pub const DEFAULT_BUILDER_STORAGE_SCOPE: &str =
    "https://www.googleapis.com/auth/devstorage.read_write";
pub const RAW_BUILDER_STARTUP_SCRIPT: &str = include_str!("../../cloud-init/builder-startup.sh");

pub struct BuilderVmConfig<'a> {
    pub instance_name: &'a str,
    pub machine_type: &'a str, // e.g. "n4-highcpu-16"
    pub zone: &'a str,
    pub deployment_id: &'a str,
    pub cache_disk_name: &'a str,
    pub cache_disk_device_name: &'a str,
    pub startup_script_template: &'a str,
    pub queue_bearer_token: Option<&'a str>,
    pub boot_disk_type: Option<&'a str>,
    pub cache_disk_type: Option<&'a str>,
}

pub fn is_functional_builder_startup_template(script: &str) -> bool {
    let trimmed = script.trim();
    !trimmed.is_empty()
        && trimmed.contains("{{DISK_DEVICE_NAME}}")
        && trimmed.contains("host-agent")
        && trimmed.contains("queue-worker")
}

pub fn build_spot_builder_vm_request(
    project: &str,
    cfg: &BuilderVmConfig,
) -> Result<GceInsertInstanceRequest, GcpError> {
    if !is_functional_builder_startup_template(cfg.startup_script_template) {
        return Err(GcpError::Validation(
            "Builder startup_script_template is incomplete or missing required host-agent queue-worker and cache disk bootstrap".to_string(),
        ));
    }
    if !crate::patch::PERMITTED_MACHINE_TYPES.contains(&cfg.machine_type)
        && !crate::patch::PERMITTED_ARM64_MACHINE_TYPES.contains(&cfg.machine_type)
    {
        return Err(GcpError::Validation(format!(
            "Builder machine type '{}' is not in permitted list",
            cfg.machine_type
        )));
    }

    let is_arm64 = cfg.machine_type.starts_with("c4a-")
        || cfg.machine_type.starts_with("n4a-")
        || cfg.machine_type.starts_with("t2a-");
    let is_hyperdisk_native = cfg.machine_type.starts_with("n4-")
        || cfg.machine_type.starts_with("c4-")
        || cfg.machine_type.starts_with("c4a-")
        || cfg.machine_type.starts_with("n4a-");

    let source_image = if is_arm64 {
        DEBIAN_13_ARM64_IMAGE_FAMILY.to_string()
    } else {
        DEBIAN_13_IMAGE_FAMILY.to_string()
    };

    let default_disk_type = if is_hyperdisk_native {
        "hyperdisk-balanced"
    } else {
        "pd-balanced"
    };
    let boot_disk_type = cfg.boot_disk_type.unwrap_or(default_disk_type);
    let cache_disk_type = cfg.cache_disk_type.unwrap_or(default_disk_type);
    if is_hyperdisk_native
        && (boot_disk_type != "hyperdisk-balanced" || cache_disk_type != "hyperdisk-balanced")
    {
        return Err(GcpError::Validation(format!(
            "Machine type '{}' requires hyperdisk-balanced for boot and cache disks (got boot='{}', cache='{}')",
            cfg.machine_type, boot_disk_type, cache_disk_type
        )));
    }
    let full_boot_disk_type = format!("zones/{}/diskTypes/{}", cfg.zone, boot_disk_type);
    let full_cache_disk_type = format!("zones/{}/diskTypes/{}", cfg.zone, cache_disk_type);
    let full_machine_type = format!("zones/{}/machineTypes/{}", cfg.zone, cfg.machine_type);

    let boot_disk = GceAttachedDisk {
        boot: true,
        auto_delete: true,
        device_name: Some(cfg.instance_name.to_string()),
        source: None,
        disk_size_gb: Some(DEFAULT_BOOT_DISK_SIZE_GB.to_string()),
        r#type: Some("PERSISTENT".to_string()),
        mode: Some("READ_WRITE".to_string()),
        interface: Some("NVME".to_string()),
        initialize_params: Some(GceDiskInitializeParams {
            disk_name: format!("{}-boot", cfg.instance_name),
            disk_size_gb: DEFAULT_BOOT_DISK_SIZE_GB.to_string(),
            disk_type: full_boot_disk_type,
            source_image: Some(source_image),
        }),
    };

    // Dedicated builders require a separate persistent cache disk with auto_delete=false
    // exposed at /dev/disk/by-id/google-{{DISK_DEVICE_NAME}}
    let cache_disk = GceAttachedDisk {
        boot: false,
        auto_delete: false,
        device_name: Some(cfg.cache_disk_device_name.to_string()),
        source: None,
        disk_size_gb: Some(DEFAULT_BUILDER_CACHE_DISK_SIZE_GB.to_string()),
        r#type: Some("PERSISTENT".to_string()),
        mode: Some("READ_WRITE".to_string()),
        interface: Some("NVME".to_string()),
        initialize_params: Some(GceDiskInitializeParams {
            disk_name: cfg.cache_disk_name.to_string(),
            disk_size_gb: DEFAULT_BUILDER_CACHE_DISK_SIZE_GB.to_string(),
            disk_type: full_cache_disk_type,
            source_image: None,
        }),
    };

    let queue_bearer_token = cfg
        .queue_bearer_token
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let rendered_startup = cfg
        .startup_script_template
        .replace("{{INSTANCE_NAME}}", cfg.instance_name)
        .replace("{{PROJECT}}", project)
        .replace("{{ZONE}}", cfg.zone)
        .replace("{{DISK_DEVICE_NAME}}", cfg.cache_disk_device_name)
        .replace("{{DEPLOYMENT_ID}}", cfg.deployment_id)
        .replace(
            "{{QUEUE_BEARER_TOKEN}}",
            queue_bearer_token.unwrap_or_default(),
        );

    let mut metadata_items = vec![
        GceMetadataItem {
            key: "startup-script".into(),
            value: rendered_startup,
        },
        GceMetadataItem {
            key: "spot-role".into(),
            value: "rust_builder".into(),
        },
        GceMetadataItem {
            key: "spot-runner-count".into(),
            value: "0".into(),
        },
        GceMetadataItem {
            key: "spot-cache-disk-device".into(),
            value: cfg.cache_disk_device_name.into(),
        },
    ];
    if let Some(tok) = queue_bearer_token {
        metadata_items.push(GceMetadataItem {
            key: "spot-queue-token".into(),
            value: tok.into(),
        });
    }

    let mut labels = BTreeMap::new();
    labels.insert("spot-role".into(), "rust_builder".into());

    let nic_type = if boot_disk_type == "hyperdisk-balanced" {
        Some("GVNIC".to_string())
    } else {
        None
    };

    let network = GceNetworkInterface {
        network: "global/networks/default".to_string(),
        nic_type,
        access_configs: vec![GceAccessConfig {
            name: "External NAT".to_string(),
            r#type: "ONE_TO_ONE_NAT".to_string(),
        }],
    };

    Ok(GceInsertInstanceRequest {
        name: cfg.instance_name.to_string(),
        machine_type: full_machine_type,
        scheduling: GceScheduling::default(),
        disks: vec![boot_disk, cache_disk],
        network_interfaces: vec![network],
        metadata: GceMetadata {
            fingerprint: None,
            items: metadata_items,
        },
        labels,
        service_accounts: vec![GceServiceAccount {
            email: "default".to_string(),
            scopes: vec![
                DEFAULT_BUILDER_STORAGE_SCOPE.to_string(),
                "https://www.googleapis.com/auth/logging.write".to_string(),
                "https://www.googleapis.com/auth/monitoring.write".to_string(),
            ],
        }],
    })
}
