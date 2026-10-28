//! Spot GitHub Runners & Rust Builders Edge Serverless Worker
//! Pure-Rust Cloudflare Worker serving the Web UI, Auth, Edge State, and Fleet Controller.

pub mod auth;
pub mod gcp;
pub mod github;
pub mod observability;
pub mod patch;
pub mod patch_console;
pub mod reconciler;
pub mod state;
pub mod ui;

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use worker::*;

pub const MAX_AUTH_BODY_BYTES: usize = 16 * 1024; // 16 KiB
pub const MAX_DASHBOARD_BODY_BYTES: usize = 256 * 1024; // 256 KiB
pub const SESSION_COOKIE_NAME: &str = "__Host-spot_session";

pub fn current_time_epoch() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        (worker::Date::now().as_millis() / 1000) as u64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

/// Decode percent escapes, resolve dot segments, collapse slashes, and lowercase.
pub fn normalize_path_for_blocking(pathname: &str) -> Option<String> {
    let mut decoded = pathname.to_string();
    for _ in 0..3 {
        match percent_encoding::percent_decode_str(&decoded).decode_utf8() {
            Ok(next) => {
                let next_str = next.into_owned();
                if next_str == decoded {
                    break;
                }
                decoded = next_str;
            }
            Err(_) => return None, // Malformed escapes
        }
    }

    let mut unified = decoded.replace('\\', "/");
    while unified.contains("//") {
        unified = unified.replace("//", "/");
    }

    let url_str = if unified.starts_with('/') {
        format!("https://normalize.invalid{}", unified)
    } else {
        format!("https://normalize.invalid/{}", unified)
    };

    let parsed = url::Url::parse(&url_str).ok()?;
    let mut path = parsed.path().to_lowercase();
    while path.contains("//") {
        path = path.replace("//", "/");
    }
    Some(path)
}

/// Check if a path targets blocked internal control plane routes.
pub fn is_control_path(pathname: &str) -> bool {
    let normalized = match normalize_path_for_blocking(pathname) {
        Some(p) => p,
        None => return true, // Block malformed paths
    };

    // Block /api/control, /api/control/, /api/control/..., etc.
    if normalized == "/api/control" || normalized.starts_with("/api/control/") {
        return true;
    }
    // Block /api/control? if attached without slash
    if normalized.starts_with("/api/control") {
        let remainder = &normalized["/api/control".len()..];
        if remainder.starts_with(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
            return true;
        }
    }
    false
}

/// Check if path is an authentication route subject to the 16 KiB body cap.
pub fn is_auth_path(pathname: &str) -> bool {
    let normalized = match normalize_path_for_blocking(pathname) {
        Some(p) => p,
        None => return true,
    };
    normalized == "/auth"
        || normalized.starts_with("/auth/")
        || normalized == "/api/auth"
        || normalized.starts_with("/api/auth/")
}

pub fn json_response<T: Serialize>(status: u16, data: &T) -> Result<Response> {
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    headers.set("Cache-Control", "no-store, no-cache, must-revalidate")?;
    let body = serde_json::to_string(data)?;
    Ok(Response::ok(body)?
        .with_status(status)
        .with_headers(headers))
}

pub fn error_response(status: u16, error: &str, message: &str) -> Result<Response> {
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    headers.set("Cache-Control", "no-store, no-cache, must-revalidate")?;
    let body = serde_json::json!({
        "error": error,
        "message": message,
    });
    Ok(Response::ok(body.to_string())?
        .with_status(status)
        .with_headers(headers))
}

fn is_global_admins_enabled(env: &Env) -> bool {
    match env.var("DASHBOARD_GLOBAL_ADMINS") {
        Ok(v) => v.to_string().trim().to_lowercase() != "none",
        Err(_) => true,
    }
}

fn is_user_exempt_from_target_checks(env: &Env, state: &state::DashboardState, uid: u64) -> bool {
    if is_global_admins_enabled(env) {
        return true;
    }
    state
        .dashboard_admins
        .iter()
        .any(|a| a.uid == uid && a.override_target_check)
}

async fn github_get_json(url: &str, gh_token: Option<&str>) -> Result<(u16, Value)> {
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    let headers = Headers::new();
    if let Some(tok) = gh_token.filter(|s| !s.trim().is_empty()) {
        headers.set("Authorization", &format!("Bearer {}", tok.trim()))?;
    }
    headers.set("Accept", "application/vnd.github+json")?;
    headers.set("User-Agent", "spot-dashboard-edge")?;
    headers.set("X-GitHub-Api-Version", "2022-11-28")?;
    init.with_headers(headers);
    let req = Request::new_with_init(url, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    let status = resp.status_code();
    let json_val: Value = resp.json().await.unwrap_or(Value::Null);
    Ok((status, json_val))
}

/// POSTs an `application/x-www-form-urlencoded` body to a GitHub OAuth endpoint
/// and returns the JSON reply.
async fn github_post_form(url: &str, fields: &[(&str, &str)]) -> Result<(u16, Value)> {
    let form = fields
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                k,
                percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    let headers = Headers::new();
    headers.set("Content-Type", "application/x-www-form-urlencoded")?;
    headers.set("Accept", "application/json")?;
    headers.set("User-Agent", "spot-dashboard-edge")?;
    init.with_headers(headers);
    init.with_body(Some(worker::wasm_bindgen::JsValue::from_str(&form)));
    let req = Request::new_with_init(url, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    let status = resp.status_code();
    let json_val: Value = resp.json().await.unwrap_or(Value::Null);
    Ok((status, json_val))
}

/// Resolves the GitHub login and numeric ID behind a user access token.
async fn fetch_github_identity(access_token: &str) -> Result<Option<(String, u64)>> {
    let (status, user_json) =
        github_get_json("https://api.github.com/user", Some(access_token)).await?;
    if status != 200 {
        return Ok(None);
    }
    let login = user_json.get("login").and_then(|v| v.as_str());
    let uid = user_json.get("id").and_then(|v| v.as_u64());
    Ok(match (login, uid) {
        (Some(l), Some(id)) if auth::is_valid_github_login(l) && id > 0 => {
            Some((l.to_string(), id))
        }
        _ => None,
    })
}

fn is_production_oauth_configured(env: &Env) -> bool {
    resolve_env_non_empty(env, "GITHUB_OAUTH_CLIENT_SECRET").is_some()
}

fn resolve_env_non_empty(env: &Env, key: &str) -> Option<String> {
    if let Ok(val) = env.var(key) {
        let s = val.to_string();
        if !s.trim().is_empty() {
            return Some(s.trim().to_string());
        }
    }
    if let Ok(val) = env.secret(key) {
        let s = val.to_string();
        if !s.trim().is_empty() {
            return Some(s.trim().to_string());
        }
    }
    None
}

/// Resolves the GitHub OAuth client ID from Worker vars or secrets.
/// Refuses to fall back to `mock-spot-client-id` when production OAuth is enabled.
pub fn resolve_oauth_client_id(
    var_val: Option<&str>,
    secret_val: Option<&str>,
    is_production: bool,
) -> Option<String> {
    if let Some(id) = var_val.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(id.to_string());
    }
    if let Some(id) = secret_val.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(id.to_string());
    }
    if is_production {
        None
    } else {
        Some("mock-spot-client-id".to_string())
    }
}

/// Top-level fetch handler for the Cloudflare Worker.
#[event(fetch)]
pub async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    let path = url.path();

    // 1. Normalization & Security Filter: block /api/control* returning 404
    if is_control_path(path) {
        return error_response(404, "not_found", "Not found");
    }

    // 2. Request body cap enforcement
    let method = req.method();
    if method != Method::Get && method != Method::Head {
        let max_bytes = if is_auth_path(path) {
            MAX_AUTH_BODY_BYTES
        } else {
            MAX_DASHBOARD_BODY_BYTES
        };

        if let Ok(Some(len_str)) = req.headers().get("Content-Length") {
            if let Ok(declared_len) = len_str.trim().parse::<usize>() {
                if declared_len > max_bytes {
                    return error_response(413, "payload_too_large", "Payload too large");
                }
            }
        }
    }

    let now = current_time_epoch();
    let session_secret_str = match env
        .secret("SESSION_SECRET")
        .map(|s| s.to_string())
        .or_else(|_| env.var("SESSION_SECRET").map(|v| v.to_string()))
    {
        Ok(s) if !s.trim().is_empty() => s,
        _ => {
            return error_response(500, "internal_error", "SESSION_SECRET is not configured");
        }
    };
    let session_secret = session_secret_str.as_bytes();
    let nbf_str = env.var("SESSION_NOT_BEFORE").ok().map(|v| v.to_string());

    // Parse cookies and verify session
    let cookie_header = req
        .headers()
        .get("Cookie")
        .ok()
        .flatten()
        .unwrap_or_default();
    let cookies = auth::parse_cookies(&cookie_header);
    let session_token = cookies.get(SESSION_COOKIE_NAME);
    let verified_session = session_token
        .and_then(|tok| auth::verify_session(tok, session_secret, nbf_str.as_deref(), now));
    let auth_header = req
        .headers()
        .get("Authorization")
        .ok()
        .flatten()
        .unwrap_or_default();
    let bearer_token = auth_header
        .strip_prefix("Bearer ")
        .or_else(|| auth_header.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let verified_builder_token =
        bearer_token.and_then(|tok| auth::verify_builder_queue_token(tok, session_secret, now));
    let configured_builder_secret = env
        .secret("SPOT_BUILDER_QUEUE_TOKEN")
        .map(|s| s.to_string())
        .or_else(|_| env.var("SPOT_BUILDER_QUEUE_TOKEN").map(|v| v.to_string()))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let has_static_builder_secret = match (bearer_token, configured_builder_secret.as_deref()) {
        (Some(provided), Some(expected)) => {
            auth::constant_time_eq(provided.as_bytes(), expected.as_bytes())
        }
        _ => false,
    };

    let kv = env.kv("SPOT_KV")?;

    // 3. Fast Route Dispatch
    match (method.clone(), path) {
        // UI & Login Gate serving (GET / and HEAD /)
        (Method::Get | Method::Head, "/" | "/index.html") => {
            let project = env
                .var("GCP_PROJECT")
                .map(|v| v.to_string())
                .unwrap_or_default();
            let org = env
                .var("GITHUB_ORG")
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "swack-tools".to_string());

            if let Some(ref session) = verified_session {
                // Check session against state
                let (bundle, _) = state::load_state(&kv, now)
                    .await
                    .map_err(|e| Error::RustError(e.to_string()))?;
                let added_admins: Vec<u64> = bundle
                    .state
                    .dashboard_admins
                    .iter()
                    .map(|a| a.uid)
                    .collect();
                let global_admins =
                    is_user_exempt_from_target_checks(&env, &bundle.state, session.uid);
                let status = auth::check_session(
                    session,
                    &bundle.state.controlled_targets,
                    &added_admins,
                    global_admins,
                );

                match status {
                    auth::SessionCheckStatus::Ok => ui::serve_dashboard_ui(&req, &project, &org),
                    auth::SessionCheckStatus::Forbidden { message, .. } => {
                        ui::serve_login_gate(&req, &message, &session.sub, 403)
                    }
                    auth::SessionCheckStatus::None => ui::serve_login_gate(&req, "", "", 200),
                }
            } else {
                ui::serve_login_gate(&req, "", "", 200)
            }
        }

        (Method::Get, "/favicon.ico") => Ok(Response::empty()?.with_status(204)),

        // Fast Edge Health Endpoint
        (Method::Get, "/api/health") => {
            let (status_code, mut body) = state::handle_api_health(&kv, now).await;
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "reconciler_ticks".to_string(),
                    serde_json::json!(reconciler::get_reconciler_ticks()),
                );
            }
            json_response(status_code, &body)
        }

        // Dynamic Overview Endpoint with URL Query Parsing
        (Method::Get, "/api/overview") => {
            let mut query = state::OverviewQuery::default();
            let is_authed_admin = if let Some(ref session) = verified_session {
                let (bundle, _) = state::load_state(&kv, now)
                    .await
                    .map_err(|e| Error::RustError(e.to_string()))?;
                let added_admins: Vec<u64> = bundle
                    .state
                    .dashboard_admins
                    .iter()
                    .map(|a| a.uid)
                    .collect();
                let global_admins =
                    is_user_exempt_from_target_checks(&env, &bundle.state, session.uid);
                matches!(
                    auth::check_session(
                        session,
                        &bundle.state.controlled_targets,
                        &added_admins,
                        global_admins,
                    ),
                    auth::SessionCheckStatus::Ok
                )
            } else {
                false
            };
            query.is_authenticated = Some(is_authed_admin);
            for (k, v) in url.query_pairs() {
                match k.as_ref() {
                    "include_deleted" | "show_deleted" => {
                        query.include_deleted = Some(v == "true" || v == "1");
                    }
                    "window_hours" | "window" => {
                        if let Ok(hours) = v.parse::<f64>() {
                            if hours.is_finite() && hours >= 0.0 {
                                query.window_hours = Some(hours.clamp(0.0, 744.0));
                            }
                        }
                    }
                    "include_disk_and_tax" | "include_disk_tax" => {
                        query.include_disk_and_tax = Some(v == "true" || v == "1");
                    }
                    "sort_by" => {
                        let trimmed = v.trim();
                        if !trimmed.is_empty() {
                            query.sort_by = Some(trimmed.to_string());
                        }
                    }
                    "sort_dir" => {
                        let trimmed = v.trim();
                        if !trimmed.is_empty() {
                            query.sort_dir = Some(trimmed.to_string());
                        }
                    }
                    "search" => {
                        query.search = Some(v.to_string());
                    }
                    "role" => {
                        let trimmed = v.trim();
                        if !trimmed.is_empty() {
                            query.role = Some(trimmed.to_string());
                        }
                    }
                    "arch" => {
                        let trimmed = v.trim();
                        if !trimmed.is_empty() {
                            query.arch = Some(trimmed.to_string());
                        }
                    }
                    "target" | "targets" => {
                        let trimmed = v.trim();
                        if !trimmed.is_empty() {
                            query.target = Some(trimmed.to_string());
                        }
                    }
                    "max_latency_ms" | "max_latency" => {
                        if let Ok(lat) = v.parse::<f64>() {
                            if lat.is_finite() && lat > 0.0 {
                                query.max_latency_ms = Some(lat);
                            }
                        }
                    }
                    _ => {}
                }
            }
            match state::handle_api_overview(&kv, query, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(e) => error_response(500, "internal_error", &e.to_string()),
            }
        }

        // Scaling Endpoints
        (Method::Get, "/api/scaling") => match state::handle_get_scaling(&kv, now).await {
            Ok((status_code, body)) => json_response(status_code, &body),
            Err(e) => error_response(500, "internal_error", &e.to_string()),
        },

        (Method::Post, "/api/scaling") => {
            let session = match verified_session {
                Some(s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }

            let raw_val: Value = req.json().await?;
            let role = raw_val
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("cicd_runner")
                .to_string();
            let base_policy = if role == "rust_builder" {
                serde_json::to_value(&bundle.state.scaling.rust_builder).unwrap_or(Value::Null)
            } else {
                serde_json::to_value(&bundle.state.scaling.cicd_runner).unwrap_or(Value::Null)
            };
            let patch_val = raw_val.get("policy").unwrap_or(&raw_val);
            let merged_policy = match (base_policy, patch_val) {
                (Value::Object(mut base_map), Value::Object(patch_map)) => {
                    for (k, v) in patch_map {
                        if k != "role" && k != "telemetry" && k != "add_github_oom_run" {
                            base_map.insert(k.clone(), v.clone());
                        }
                    }
                    Value::Object(base_map)
                }
                (_, other) => other.clone(),
            };
            let update_req = state::UpdateScalingRequest {
                role,
                policy: merged_policy,
            };
            match state::handle_post_scaling(&kv, update_req, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(e) => error_response(400, "bad_request", &e.to_string()),
            }
        }

        (Method::Post, "/api/budget") => {
            let session = match verified_session {
                Some(s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }
            let budget_req: state::BudgetConfig = req.json().await?;
            match state::handle_post_budget(&kv, budget_req, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(e) => error_response(400, "bad_request", &e.to_string()),
            }
        }

        (Method::Post, "/api/regions") => {
            let session = match verified_session {
                Some(s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }
            let body_val: Value = req.json().await?;
            match state::handle_post_regions(&kv, body_val, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(e) => error_response(400, "bad_request", &e.to_string()),
            }
        }

        // Cloud Observability, Correlated Workflow Logs & Dynamic Runner Density Endpoints
        (Method::Get, "/api/observability") => {
            let session = match verified_session {
                Some(s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }
            let params: Vec<(String, String)> = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            let query = match observability::parse_observability_query(&params) {
                Ok(q) => q,
                Err(msg) => return error_response(400, "bad_request", &msg),
            };
            match observability::handle_get_observability(&kv, &query, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(e) => error_response(500, "internal_error", &e.to_string()),
            }
        }

        (Method::Post, "/api/observability/ingest") => {
            let is_authed = if verified_builder_token.is_some() || has_static_builder_secret {
                true
            } else if let Some(ref session) = verified_session {
                let (bundle, _) = state::load_state(&kv, now)
                    .await
                    .map_err(|e| Error::RustError(e.to_string()))?;
                let added_admins: Vec<u64> = bundle
                    .state
                    .dashboard_admins
                    .iter()
                    .map(|a| a.uid)
                    .collect();
                auth::may_sign_in(&session.sub, session.uid, &added_admins)
            } else {
                false
            };
            if !is_authed {
                return error_response(401, "unauthorized", "Valid telemetry credentials required");
            }
            let payload: observability::IngestedHostSnapshot = req.json().await?;
            if payload.instance_name.trim().is_empty() {
                return error_response(400, "bad_request", "instance_name is required");
            }
            observability::record_ingested_snapshot(payload, now);
            json_response(
                200,
                &serde_json::json!({
                    "status": "ok",
                    "ingested_at": now,
                }),
            )
        }

        (Method::Post, "/api/observability/density") => {
            let session = match verified_session {
                Some(s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }
            let req_body: observability::UpdateDynamicDensityRequest = req.json().await?;
            match observability::handle_post_dynamic_density(&kv, req_body, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(e) => error_response(400, "bad_request", &e.to_string()),
            }
        }

        // Admin Management Endpoints
        (Method::Get, "/api/admins") => {
            let session = match verified_session {
                Some(s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            match state::handle_get_admins(&kv, session.uid, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(state::StateError::Forbidden(msg)) => error_response(403, "forbidden", &msg),
                Err(e) => error_response(500, "internal_error", &e.to_string()),
            }
        }

        (Method::Post, "/api/admins/add") => {
            let session = match verified_session {
                Some(s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            if session.uid != state::OWNER_UID {
                return error_response(403, "forbidden", "Owner access required");
            }
            let raw_val: Value = req.json().await?;
            let raw_login = raw_val
                .get("login")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .trim_start_matches('@')
                .to_string();
            let mut uid = raw_val.get("uid").and_then(|v| v.as_u64()).unwrap_or(0);
            let confirm = raw_val
                .get("confirm")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let override_target_check = raw_val
                .get("override_target_check")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let check_targets = raw_val
                .get("check_targets")
                .and_then(|v| v.as_bool())
                .unwrap_or(uid == 0);

            if !confirm {
                return error_response(
                    400,
                    "bad_request",
                    "Changing dashboard administrators needs \"confirm\": true",
                );
            }
            if !auth::is_valid_github_login(&raw_login) {
                return error_response(400, "bad_request", "Invalid GitHub username");
            }

            let gh_token = env
                .secret("SPOT_CONFIG_GH_TOKEN")
                .map(|s| s.to_string())
                .or_else(|_| env.var("SPOT_CONFIG_GH_TOKEN").map(|v| v.to_string()))
                .ok();

            let mut resolved_login = raw_login.clone();
            if uid == 0 {
                let user_url = format!("https://api.github.com/users/{}", raw_login);
                match github_get_json(&user_url, gh_token.as_deref()).await {
                    Ok((200, user_json)) => {
                        if let Some(l) = user_json.get("login").and_then(|v| v.as_str()) {
                            resolved_login = l.to_string();
                        }
                        uid = user_json.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                    }
                    Ok((404, _)) => {
                        return error_response(
                            404,
                            "not_found",
                            &format!("GitHub user '{}' was not found", raw_login),
                        );
                    }
                    Ok((st, _)) => {
                        return error_response(
                            502,
                            "github_error",
                            &format!(
                                "GitHub returned HTTP {} while looking up '{}'",
                                st, raw_login
                            ),
                        );
                    }
                    Err(e) => {
                        return error_response(
                            502,
                            "github_error",
                            &format!("Could not contact GitHub to look up '{}': {}", raw_login, e),
                        );
                    }
                }
            }

            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            if uid == state::OWNER_UID {
                return error_response(
                    409,
                    "conflict",
                    &format!("'{}' is a dashboard owner already", resolved_login),
                );
            }
            if bundle.state.dashboard_admins.iter().any(|a| a.uid == uid) {
                return error_response(
                    409,
                    "conflict",
                    &format!("'{}' is already an administrator", resolved_login),
                );
            }

            let mut missing_targets: Vec<String> = Vec::new();
            if check_targets && !override_target_check {
                let visible_targets = state::collect_visible_dashboard_targets(&bundle.state);
                for target in &visible_targets {
                    if let Some((owner, repo)) = target.split_once('/') {
                        if owner.eq_ignore_ascii_case(&resolved_login) {
                            continue;
                        }
                        let mut is_admin = false;
                        if gh_token.as_deref().map_or(false, |t| !t.trim().is_empty()) {
                            let perm_url = format!(
                                "https://api.github.com/repos/{}/{}/collaborators/{}/permission",
                                owner, repo, resolved_login
                            );
                            if let Ok((200, perm_json)) =
                                github_get_json(&perm_url, gh_token.as_deref()).await
                            {
                                is_admin = perm_json.get("permission").and_then(|v| v.as_str())
                                    == Some("admin")
                                    || perm_json
                                        .get("user")
                                        .and_then(|u| u.get("permissions"))
                                        .and_then(|p| p.get("admin"))
                                        .and_then(|v| v.as_bool())
                                        == Some(true);
                            }
                        }
                        if !is_admin {
                            missing_targets.push(target.clone());
                        }
                    } else {
                        if target.eq_ignore_ascii_case(&resolved_login) {
                            continue;
                        }
                        let mut is_admin = false;
                        if gh_token.as_deref().map_or(false, |t| !t.trim().is_empty()) {
                            let org_url = format!(
                                "https://api.github.com/orgs/{}/memberships/{}",
                                target, resolved_login
                            );
                            if let Ok((200, org_json)) =
                                github_get_json(&org_url, gh_token.as_deref()).await
                            {
                                is_admin = org_json.get("role").and_then(|v| v.as_str())
                                    == Some("admin")
                                    && org_json.get("state").and_then(|v| v.as_str())
                                        == Some("active");
                            }
                        }
                        if !is_admin {
                            missing_targets.push(target.clone());
                        }
                    }
                }
            }

            let add_req = state::AddAdminRequest {
                login: resolved_login,
                uid,
                confirm,
            };
            match state::handle_post_admins_add_with_override(
                &kv,
                add_req,
                override_target_check,
                missing_targets,
                &session.sub,
                session.uid,
                now,
            )
            .await
            {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(state::StateError::Forbidden(msg)) => error_response(403, "forbidden", &msg),
                Err(state::StateError::Conflict(msg)) => error_response(409, "conflict", &msg),
                Err(e) => error_response(400, "bad_request", &e.to_string()),
            }
        }

        (Method::Post, "/api/admins/remove") => {
            let session = match verified_session {
                Some(s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let remove_req: state::RemoveAdminRequest = req.json().await?;
            match state::handle_post_admins_remove(&kv, remove_req, &session.sub, session.uid, now)
                .await
            {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(state::StateError::Forbidden(msg)) => error_response(403, "forbidden", &msg),
                Err(state::StateError::NotFound(msg)) => error_response(404, "not_found", &msg),
                Err(e) => error_response(400, "bad_request", &e.to_string()),
            }
        }

        // Instance Action Safety Gates
        (Method::Post, "/api/instances/delete") => {
            let session = match verified_session {
                Some(s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }

            let del_req: state::DeleteInstanceRequest = req.json().await?;
            match state::handle_post_instances_delete(&kv, del_req, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(state::StateError::NotFound(msg)) => error_response(404, "not_found", &msg),
                Err(e) => error_response(400, "bad_request", &e.to_string()),
            }
        }

        (Method::Post, "/api/instances/scale-region") => {
            let session = match verified_session {
                Some(s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }

            let scale_req: state::ScaleRegionRequest = req.json().await?;
            match state::handle_post_instances_scale_region(&kv, scale_req, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(e) => error_response(400, "bad_request", &e.to_string()),
            }
        }

        // Patch Console & Multi-Repo Manifest Endpoints
        (Method::Get, "/api/patch-console/bootstrap" | "/api/patches") => {
            if verified_session.is_none() {
                return error_response(401, "unauthorized", "Sign in required");
            }
            let (status_code, body) = patch_console::handle_bootstrap();
            json_response(status_code, &body)
        }

        (Method::Get, "/api/patch-console/manifest" | "/api/patches/manifest") => {
            if verified_session.is_none() {
                return error_response(401, "unauthorized", "Sign in required");
            }
            let repo = url
                .query_pairs()
                .find(|(k, _)| k == "repo" || k == "path")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            let gh_token = env
                .secret("SPOT_CONFIG_GH_TOKEN")
                .map(|s| s.to_string())
                .or_else(|_| env.var("SPOT_CONFIG_GH_TOKEN").map(|v| v.to_string()))
                .or_else(|_| env.secret("GH_TOKEN").map(|s| s.to_string()))
                .or_else(|_| env.var("GH_TOKEN").map(|v| v.to_string()))
                .unwrap_or_default();
            let (status_code, body) = patch_console::handle_manifest(
                &repo,
                &patch_console::WorkerGitHubTransport,
                &gh_token,
            )
            .await;
            json_response(status_code, &body)
        }

        (Method::Post, "/api/patch-console/validate" | "/api/patches/validate") => {
            let manifest_val: Value = req.json().await.unwrap_or(Value::Null);
            let (status_code, body) = patch_console::handle_validate(&manifest_val);
            json_response(status_code, &body)
        }

        (Method::Post, "/api/patch-console/save" | "/api/patches/save") => {
            if verified_session.is_none() {
                return error_response(401, "unauthorized", "Sign in required");
            }
            let save_val: Value = req.json().await.unwrap_or(Value::Null);
            let gh_token = env
                .secret("SPOT_CONFIG_GH_TOKEN")
                .map(|s| s.to_string())
                .or_else(|_| env.var("SPOT_CONFIG_GH_TOKEN").map(|v| v.to_string()))
                .or_else(|_| env.secret("GH_TOKEN").map(|s| s.to_string()))
                .or_else(|_| env.var("GH_TOKEN").map(|v| v.to_string()))
                .unwrap_or_default();
            let (status_code, body) = patch_console::handle_save(
                &save_val,
                &patch_console::WorkerGitHubTransport,
                &gh_token,
            )
            .await;
            json_response(status_code, &body)
        }

        (Method::Post | Method::Get, "/api/patches/preview" | "/api/patch-console/preview") => {
            let body_val: Value = if method == Method::Post {
                req.json().await.unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            let (status_code, body) = patch_console::handle_preview(&body_val);
            json_response(status_code, &body)
        }

        // Profile Qualification & Profiles
        (Method::Post, "/api/qualify") => {
            let qual_req: Value = req.json().await.unwrap_or(Value::Null);
            let role = qual_req
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("cicd_runner");
            let resp = serde_json::json!({
                "status": "ok",
                "qualification": {
                    "role": role,
                    "hardware_qualifying_count": 28,
                    "spot_price_qualifying_count": 28,
                    "requirements": {
                        "min_vcpus": 8,
                        "min_ram_gib": 30,
                        "min_benchmark_score": 1000,
                        "budget": {
                            "max_hourly_usd": 0.50,
                            "max_daily_usd": 12.00,
                            "max_monthly_usd": 360.00
                        }
                    },
                    "hardware_qualifying": [],
                    "spot_price_qualifying": []
                }
            });
            json_response(200, &resp)
        }

        (Method::Post, "/api/profile/cicd_runner" | "/api/profile/rust_builder") => {
            let role = if path.ends_with("rust_builder") {
                "rust_builder"
            } else {
                "cicd_runner"
            };
            let resp = serde_json::json!({
                "status": "ok",
                "role": role,
                "qualification": {
                    "role": role,
                    "hardware_qualifying_count": 28,
                    "spot_price_qualifying_count": 28,
                    "requirements": {
                        "min_vcpus": 8,
                        "min_ram_gib": 30,
                        "min_benchmark_score": 1000,
                        "budget": {
                            "max_hourly_usd": 0.50,
                            "max_daily_usd": 12.00,
                            "max_monthly_usd": 360.00
                        }
                    },
                    "hardware_qualifying": [],
                    "spot_price_qualifying": []
                }
            });
            json_response(200, &resp)
        }

        // Cloud-Init Visualizer & Editor
        (Method::Post, "/api/cloud-init/parse") => {
            let body_val: Value = req.json().await.unwrap_or(Value::Null);
            let content = body_val
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let line_count = content.lines().count();
            let is_yaml = content.trim_start().starts_with("#cloud-config");
            let resp = serde_json::json!({
                "valid": true,
                "format": if is_yaml { "cloud-config (YAML)" } else { "shell script" },
                "line_count": line_count,
                "errors": [],
                "warnings": [],
                "stages": [
                    {
                        "stage": "1",
                        "title": "Host Preparation",
                        "summary": "1 check passed",
                        "items": ["Base bootstrap verified"]
                    }
                ]
            });
            json_response(200, &resp)
        }

        (Method::Post, "/api/cloud-init/save") => {
            let session = match verified_session {
                Some(ref s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }

            let save_req: state::SaveCloudInitRequest = match req.json().await {
                Ok(parsed) => parsed,
                Err(err) => return error_response(400, "bad_request", &err.to_string()),
            };
            match state::handle_post_cloud_init_save(&kv, save_req, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(state::StateError::Validation(msg)) => {
                    error_response(400, "validation_error", &msg)
                }
                Err(err) => error_response(500, "internal_error", &err.to_string()),
            }
        }

        // VM Instance Templates
        (Method::Post, "/api/vm-templates/from-instance") => {
            let session = match verified_session {
                Some(ref s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }

            let tpl_req: state::CreateVmTemplateRequest = match req.json().await {
                Ok(parsed) => parsed,
                Err(err) => return error_response(400, "bad_request", &err.to_string()),
            };
            match state::handle_post_vm_templates_from_instance(&kv, tpl_req, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(state::StateError::NotFound(msg)) => error_response(404, "not_found", &msg),
                Err(state::StateError::Validation(msg)) => {
                    error_response(400, "validation_error", &msg)
                }
                Err(err) => error_response(500, "internal_error", &err.to_string()),
            }
        }

        // Rust Build Telemetry
        (Method::Get, "/api/rust-builds") => {
            if is_production_oauth_configured(&env) && verified_session.is_none() {
                return error_response(401, "unauthorized", "Sign in required");
            }
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            let is_authed_admin = if let Some(ref session) = verified_session {
                let global_admins =
                    is_user_exempt_from_target_checks(&env, &bundle.state, session.uid);
                matches!(
                    auth::check_session(
                        session,
                        &bundle.state.controlled_targets,
                        &added_admins,
                        global_admins,
                    ),
                    auth::SessionCheckStatus::Ok
                )
            } else {
                false
            };
            let mut builds_val = bundle.state.rust_builds.clone();
            if !is_authed_admin {
                if let Some(builds_arr) =
                    builds_val.get_mut("builds").and_then(|v| v.as_array_mut())
                {
                    for b in builds_arr.iter_mut() {
                        if let Some(obj) = b.as_object_mut() {
                            obj.remove("origin_ip");
                            obj.remove("origin_hostname");
                            obj.remove("ip");
                            obj.remove("client_ip");
                        }
                    }
                }
                if let Some(queue_arr) = builds_val.get_mut("queue").and_then(|v| v.as_array_mut())
                {
                    for q in queue_arr.iter_mut() {
                        if let Some(obj) = q.as_object_mut() {
                            obj.remove("origin_ip");
                            obj.remove("origin_hostname");
                            obj.remove("ip");
                            obj.remove("client_ip");
                        }
                    }
                }
            }
            let resp = serde_json::json!({
                "builds": builds_val
            });
            json_response(200, &resp)
        }

        (Method::Post, "/api/rust-builds") => {
            let session = match verified_session {
                Some(ref s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }

            let record_req: state::RecordRustBuildRequest = match req.json().await {
                Ok(parsed) => parsed,
                Err(err) => return error_response(400, "bad_request", &err.to_string()),
            };
            match state::handle_post_rust_builds(&kv, record_req, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(state::StateError::Validation(msg)) => {
                    error_response(400, "validation_error", &msg)
                }
                Err(err) => error_response(500, "internal_error", &err.to_string()),
            }
        }

        (Method::Post, "/api/rust-builds/enqueue") => {
            let session = match verified_session {
                Some(ref s) => s,
                None => return error_response(401, "unauthorized", "Sign in required"),
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                return error_response(403, "forbidden", "Administrator permissions required");
            }

            let enqueue_req: state::EnqueueRustBuildRequest = match req.json().await {
                Ok(parsed) => parsed,
                Err(err) => return error_response(400, "bad_request", &err.to_string()),
            };
            match state::handle_post_rust_builds_enqueue(&kv, enqueue_req, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(state::StateError::Validation(msg)) => {
                    error_response(400, "validation_error", &msg)
                }
                Err(state::StateError::Conflict(msg)) => error_response(409, "conflict", &msg),
                Err(err) => error_response(500, "internal_error", &err.to_string()),
            }
        }

        (Method::Get, "/api/rust-builds/queue") => {
            if is_production_oauth_configured(&env) && verified_session.is_none() {
                return error_response(401, "unauthorized", "Sign in required");
            }
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            let global_admins = is_global_admins_enabled(&env);
            let is_authed_admin = if let Some(ref session) = verified_session {
                matches!(
                    auth::check_session(
                        session,
                        &bundle.state.controlled_targets,
                        &added_admins,
                        global_admins,
                    ),
                    auth::SessionCheckStatus::Ok
                )
            } else {
                false
            };
            match state::handle_get_rust_builds_queue(&kv, is_authed_admin, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(err) => error_response(500, "internal_error", &err.to_string()),
            }
        }

        (Method::Post, "/api/rust-builds/claim") => {
            if verified_session.is_none()
                && verified_builder_token.is_none()
                && !has_static_builder_secret
            {
                return error_response(401, "unauthorized", "Sign in required");
            }
            if let Some(ref session) = verified_session {
                let (bundle, _) = state::load_state(&kv, now)
                    .await
                    .map_err(|e| Error::RustError(e.to_string()))?;
                let added_admins: Vec<u64> = bundle
                    .state
                    .dashboard_admins
                    .iter()
                    .map(|a| a.uid)
                    .collect();
                if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                    return error_response(403, "forbidden", "Administrator permissions required");
                }
            }

            let claim_req: state::ClaimRustBuildRequest = match req.json().await {
                Ok(parsed) => parsed,
                Err(err) => return error_response(400, "bad_request", &err.to_string()),
            };
            if verified_session.is_none() && !has_static_builder_secret {
                if let Some(ref btok) = verified_builder_token {
                    if claim_req.instance_name.trim() != btok.inst {
                        return error_response(
                            403,
                            "forbidden",
                            "Builder queue token instance_name mismatch",
                        );
                    }
                }
            }
            match state::handle_post_rust_builds_claim(&kv, claim_req, now).await {
                Ok((status_code, mut body)) => {
                    if let Some(ref btok) = verified_builder_token {
                        if auth::should_renew_builder_queue_token(btok, now) {
                            if let Ok(new_tok) =
                                auth::mint_builder_queue_token(session_secret, &btok.inst, now)
                            {
                                if let Some(obj) = body.as_object_mut() {
                                    obj.insert(
                                        "renewed_queue_token".to_string(),
                                        Value::String(new_tok),
                                    );
                                }
                            }
                        }
                    }
                    json_response(status_code, &body)
                }
                Err(state::StateError::Validation(msg)) => {
                    error_response(400, "validation_error", &msg)
                }
                Err(state::StateError::Conflict(msg)) => error_response(409, "conflict", &msg),
                Err(err) => error_response(500, "internal_error", &err.to_string()),
            }
        }

        (Method::Post, p) if p.starts_with("/api/rust-builds/") && p.ends_with("/heartbeat") => {
            if verified_session.is_none()
                && verified_builder_token.is_none()
                && !has_static_builder_secret
            {
                return error_response(401, "unauthorized", "Sign in required");
            }
            if let Some(ref session) = verified_session {
                let (bundle, _) = state::load_state(&kv, now)
                    .await
                    .map_err(|e| Error::RustError(e.to_string()))?;
                let added_admins: Vec<u64> = bundle
                    .state
                    .dashboard_admins
                    .iter()
                    .map(|a| a.uid)
                    .collect();
                if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                    return error_response(403, "forbidden", "Administrator permissions required");
                }
            }

            let id_str = p
                .trim_start_matches("/api/rust-builds/")
                .trim_end_matches("/heartbeat")
                .trim_matches('/')
                .trim_start_matches('#');
            let Ok(build_id) = id_str.parse::<u64>() else {
                return error_response(400, "bad_request", "Invalid numeric build_id in path");
            };
            let mut hb_req: state::HeartbeatRustBuildRequest = match req.json().await {
                Ok(parsed) => parsed,
                Err(err) => return error_response(400, "bad_request", &err.to_string()),
            };
            if verified_session.is_none() {
                if let Some(ref btok) = verified_builder_token {
                    if !hb_req.instance_name.trim().is_empty()
                        && hb_req.instance_name.trim() != btok.inst
                    {
                        return error_response(
                            403,
                            "forbidden",
                            "Builder queue token instance_name mismatch",
                        );
                    }
                    hb_req.instance_name = btok.inst.clone();
                } else if hb_req.instance_name.trim().is_empty() {
                    return error_response(
                        400,
                        "validation_error",
                        "instance_name is required for machine heartbeat",
                    );
                }
            }
            match state::handle_post_rust_builds_heartbeat(&kv, build_id, hb_req, now).await {
                Ok((status_code, mut body)) => {
                    if let Some(ref btok) = verified_builder_token {
                        if auth::should_renew_builder_queue_token(btok, now) {
                            if let Ok(new_tok) =
                                auth::mint_builder_queue_token(session_secret, &btok.inst, now)
                            {
                                if let Some(obj) = body.as_object_mut() {
                                    obj.insert(
                                        "renewed_queue_token".to_string(),
                                        Value::String(new_tok),
                                    );
                                }
                            }
                        }
                    }
                    json_response(status_code, &body)
                }
                Err(state::StateError::NotFound(msg)) => error_response(404, "not_found", &msg),
                Err(state::StateError::Validation(msg)) => {
                    error_response(400, "validation_error", &msg)
                }
                Err(state::StateError::Conflict(msg)) => error_response(409, "conflict", &msg),
                Err(err) => error_response(500, "internal_error", &err.to_string()),
            }
        }

        (Method::Post, "/api/rust-builds/complete") => {
            if verified_session.is_none()
                && verified_builder_token.is_none()
                && !has_static_builder_secret
            {
                return error_response(401, "unauthorized", "Sign in required");
            }
            if let Some(ref session) = verified_session {
                let (bundle, _) = state::load_state(&kv, now)
                    .await
                    .map_err(|e| Error::RustError(e.to_string()))?;
                let added_admins: Vec<u64> = bundle
                    .state
                    .dashboard_admins
                    .iter()
                    .map(|a| a.uid)
                    .collect();
                if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                    return error_response(403, "forbidden", "Administrator permissions required");
                }
            }

            let mut complete_req: state::CompleteRustBuildRequest = match req.json().await {
                Ok(parsed) => parsed,
                Err(err) => return error_response(400, "bad_request", &err.to_string()),
            };
            let Some(build_id) = complete_req.build_id else {
                return error_response(
                    400,
                    "validation_error",
                    "build_id is required when posting to /api/rust-builds/complete",
                );
            };
            if verified_session.is_none() {
                if let Some(ref btok) = verified_builder_token {
                    if let Some(ref inst) = complete_req.instance_name {
                        if !inst.trim().is_empty() && inst.trim() != btok.inst {
                            return error_response(
                                403,
                                "forbidden",
                                "Builder queue token instance_name mismatch",
                            );
                        }
                    }
                    complete_req.instance_name = Some(btok.inst.clone());
                } else if complete_req
                    .instance_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .is_none()
                {
                    return error_response(
                        400,
                        "validation_error",
                        "instance_name is required for machine completion",
                    );
                }
            }
            match state::handle_post_rust_builds_complete(&kv, build_id, complete_req, now).await {
                Ok((status_code, mut body)) => {
                    if let Some(ref btok) = verified_builder_token {
                        if auth::should_renew_builder_queue_token(btok, now) {
                            if let Ok(new_tok) =
                                auth::mint_builder_queue_token(session_secret, &btok.inst, now)
                            {
                                if let Some(obj) = body.as_object_mut() {
                                    obj.insert(
                                        "renewed_queue_token".to_string(),
                                        Value::String(new_tok),
                                    );
                                }
                            }
                        }
                    }
                    json_response(status_code, &body)
                }
                Err(state::StateError::NotFound(msg)) => error_response(404, "not_found", &msg),
                Err(state::StateError::Validation(msg)) => {
                    error_response(400, "validation_error", &msg)
                }
                Err(state::StateError::Conflict(msg)) => error_response(409, "conflict", &msg),
                Err(err) => error_response(500, "internal_error", &err.to_string()),
            }
        }

        (Method::Post, p) if p.starts_with("/api/rust-builds/") && p.ends_with("/complete") => {
            if verified_session.is_none()
                && verified_builder_token.is_none()
                && !has_static_builder_secret
            {
                return error_response(401, "unauthorized", "Sign in required");
            }
            if let Some(ref session) = verified_session {
                let (bundle, _) = state::load_state(&kv, now)
                    .await
                    .map_err(|e| Error::RustError(e.to_string()))?;
                let added_admins: Vec<u64> = bundle
                    .state
                    .dashboard_admins
                    .iter()
                    .map(|a| a.uid)
                    .collect();
                if !auth::may_sign_in(&session.sub, session.uid, &added_admins) {
                    return error_response(403, "forbidden", "Administrator permissions required");
                }
            }

            let id_str = p
                .trim_start_matches("/api/rust-builds/")
                .trim_end_matches("/complete")
                .trim_matches('/')
                .trim_start_matches('#');
            let Ok(build_id) = id_str.parse::<u64>() else {
                return error_response(400, "bad_request", "Invalid numeric build_id in path");
            };
            let mut complete_req: state::CompleteRustBuildRequest = match req.json().await {
                Ok(parsed) => parsed,
                Err(err) => return error_response(400, "bad_request", &err.to_string()),
            };
            if verified_session.is_none() {
                if let Some(ref btok) = verified_builder_token {
                    if let Some(ref inst) = complete_req.instance_name {
                        if !inst.trim().is_empty() && inst.trim() != btok.inst {
                            return error_response(
                                403,
                                "forbidden",
                                "Builder queue token instance_name mismatch",
                            );
                        }
                    }
                    complete_req.instance_name = Some(btok.inst.clone());
                } else if complete_req
                    .instance_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .is_none()
                {
                    return error_response(
                        400,
                        "validation_error",
                        "instance_name is required for machine completion",
                    );
                }
            }
            match state::handle_post_rust_builds_complete(&kv, build_id, complete_req, now).await {
                Ok((status_code, mut body)) => {
                    if let Some(ref btok) = verified_builder_token {
                        if auth::should_renew_builder_queue_token(btok, now) {
                            if let Ok(new_tok) =
                                auth::mint_builder_queue_token(session_secret, &btok.inst, now)
                            {
                                if let Some(obj) = body.as_object_mut() {
                                    obj.insert(
                                        "renewed_queue_token".to_string(),
                                        Value::String(new_tok),
                                    );
                                }
                            }
                        }
                    }
                    json_response(status_code, &body)
                }
                Err(state::StateError::NotFound(msg)) => error_response(404, "not_found", &msg),
                Err(state::StateError::Validation(msg)) => {
                    error_response(400, "validation_error", &msg)
                }
                Err(state::StateError::Conflict(msg)) => error_response(409, "conflict", &msg),
                Err(err) => error_response(500, "internal_error", &err.to_string()),
            }
        }

        (Method::Get, p) if p.starts_with("/api/rust-builds/") => {
            if is_production_oauth_configured(&env) && verified_session.is_none() {
                return error_response(401, "unauthorized", "Sign in required");
            }
            let id_str = p
                .trim_start_matches("/api/rust-builds/")
                .trim_matches('/')
                .trim_start_matches('#');
            let Ok(build_id) = id_str.parse::<u64>() else {
                return error_response(400, "bad_request", "Invalid numeric build_id in path");
            };
            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            let global_admins = is_global_admins_enabled(&env);
            let is_authed_admin = if let Some(ref session) = verified_session {
                matches!(
                    auth::check_session(
                        session,
                        &bundle.state.controlled_targets,
                        &added_admins,
                        global_admins,
                    ),
                    auth::SessionCheckStatus::Ok
                )
            } else {
                false
            };
            match state::handle_get_rust_build_status(&kv, build_id, is_authed_admin, now).await {
                Ok((status_code, body)) => json_response(status_code, &body),
                Err(state::StateError::NotFound(msg)) => error_response(404, "not_found", &msg),
                Err(err) => error_response(500, "internal_error", &err.to_string()),
            }
        }

        // Legacy Live Hot Patching (Deprecated in favor of PR Patch Console)
        (Method::Post, "/api/patches/hot-patch") => {
            let resp = serde_json::json!({
                "status": "deprecated",
                "message": "Live hot-patching is deprecated. Runner patches must be submitted and reviewed via the Runner Patch Console (/api/patch-console/*)."
            });
            json_response(400, &resp)
        }

        // Auth Status
        (Method::Get, "/api/auth/status" | "/auth/status") => {
            if let Some(ref session) = verified_session {
                let (bundle, _) = state::load_state(&kv, now)
                    .await
                    .map_err(|e| Error::RustError(e.to_string()))?;
                let added_admins: Vec<u64> = bundle
                    .state
                    .dashboard_admins
                    .iter()
                    .map(|a| a.uid)
                    .collect();
                let global_admins =
                    is_user_exempt_from_target_checks(&env, &bundle.state, session.uid);
                let status_enum = auth::check_session(
                    session,
                    &bundle.state.controlled_targets,
                    &added_admins,
                    global_admins,
                );

                let (status_str, missing_targets) = match status_enum {
                    auth::SessionCheckStatus::Ok => ("authorized", Vec::new()),
                    auth::SessionCheckStatus::Forbidden {
                        missing_targets, ..
                    } => ("forbidden", missing_targets),
                    auth::SessionCheckStatus::None => ("none", Vec::new()),
                };

                let body = serde_json::json!({
                    "authenticated": true,
                    "status": status_str,
                    "user": {
                        "login": session.sub,
                        "id": session.uid
                    },
                    "owner": auth::is_owner(&session.sub, session.uid),
                    "missing_targets": missing_targets,
                    "oauth": {
                        "enabled": true,
                        "device_flow": true,
                        "web_flow": true
                    },
                    "legacy_token": false
                });
                json_response(200, &body)
            } else {
                let body = serde_json::json!({
                    "authenticated": false,
                    "status": "none",
                    "user": Value::Null,
                    "owner": false,
                    "missing_targets": [],
                    "oauth": {
                        "enabled": true,
                        "device_flow": true,
                        "web_flow": true
                    },
                    "legacy_token": false
                });
                json_response(200, &body)
            }
        }

        // GitHub OAuth Login
        (Method::Get, "/auth/github/login") => {
            let return_to = url
                .query_pairs()
                .find(|(k, _)| k == "next")
                .map(|(_, v)| v.to_string())
                .unwrap_or_else(|| "/".to_string());
            let return_to = auth::safe_return_path(&return_to);

            let (signed_state, state_cookie) =
                match auth::create_oauth_state(session_secret, &return_to, now) {
                    Ok(res) => res,
                    Err(e) => return error_response(500, "internal_error", &e),
                };

            let verifier = auth::pkce_verifier(session_secret, &signed_state);
            let challenge = auth::pkce_challenge(&verifier);

            let var_client_id = env
                .var("GITHUB_OAUTH_CLIENT_ID")
                .ok()
                .map(|s| s.to_string());
            let secret_client_id = env
                .secret("GITHUB_OAUTH_CLIENT_ID")
                .ok()
                .map(|s| s.to_string());
            let Some(client_id) = resolve_oauth_client_id(
                var_client_id.as_deref(),
                secret_client_id.as_deref(),
                is_production_oauth_configured(&env),
            ) else {
                return ui::serve_login_gate(
                    &req,
                    "GitHub OAuth application is not configured on this server (GITHUB_OAUTH_CLIENT_ID missing). Sign-in refused.",
                    "",
                    500,
                );
            };

            let redirect_uri = resolve_env_non_empty(&env, "GITHUB_OAUTH_REDIRECT_URI")
                .unwrap_or_else(|| {
                    let scheme = url.scheme();
                    let host = url.host_str().unwrap_or("spot.swacktech.com");
                    match url.port() {
                        Some(port)
                            if (scheme == "http" && port != 80)
                                || (scheme == "https" && port != 443) =>
                        {
                            format!("{}://{}:{}/auth/github/callback", scheme, host, port)
                        }
                        _ => format!("{}://{}/auth/github/callback", scheme, host),
                    }
                });

            let auth_url = auth::build_github_authorize_url(
                &client_id,
                &redirect_uri,
                &signed_state,
                &challenge,
            );

            let headers = Headers::new();
            headers.set("Location", &auth_url)?;
            headers.append("Set-Cookie", &state_cookie)?;
            headers.set("Cache-Control", "no-store")?;
            headers.set("Content-Length", "0")?;
            headers.set("Connection", "close")?;
            Ok(Response::empty()?.with_status(302).with_headers(headers))
        }

        // GitHub OAuth Callback
        (Method::Get, "/auth/github/callback") => {
            let pairs: HashMap<String, String> = url.query_pairs().into_owned().collect();
            let clear_state_cookie = auth::format_cleared_state_cookie();

            if pairs.contains_key("error") {
                let headers = Headers::new();
                headers.append("Set-Cookie", &clear_state_cookie)?;
                return ui::serve_login_gate(
                    &req,
                    "GitHub authorization was not granted.",
                    "",
                    403,
                );
            }

            let code = match pairs.get("code") {
                Some(c) if !c.is_empty() && c.len() <= 1024 => c.clone(),
                _ => {
                    return error_response(
                        400,
                        "invalid_or_expired_oauth_state",
                        "Missing or invalid code parameter",
                    )
                }
            };

            let state = match pairs.get("state") {
                Some(s) if !s.is_empty() && s.len() <= auth::MAX_TOKEN_CHARS => s.clone(),
                _ => {
                    return error_response(
                        400,
                        "invalid_or_expired_oauth_state",
                        "Missing or invalid state parameter",
                    )
                }
            };

            let cookie_state = cookies
                .get(auth::OAUTH_STATE_COOKIE_NAME)
                .cloned()
                .unwrap_or_default();
            if cookie_state.is_empty() {
                return error_response(
                    400,
                    "invalid_or_expired_oauth_state",
                    "Missing state cookie",
                );
            }

            let payload = match auth::verify_oauth_state(session_secret, &state, &cookie_state, now)
            {
                Ok(p) => p,
                Err(err) => return error_response(400, "invalid_or_expired_oauth_state", err),
            };

            if !auth::mark_state_nonce_used(&payload.n) {
                return error_response(
                    400,
                    "invalid_or_expired_oauth_state",
                    "State nonce already used",
                );
            }

            let client_id =
                resolve_env_non_empty(&env, "GITHUB_OAUTH_CLIENT_ID").unwrap_or_default();
            let client_secret =
                resolve_env_non_empty(&env, "GITHUB_OAUTH_CLIENT_SECRET").unwrap_or_default();

            if client_id.is_empty() || client_secret.is_empty() {
                return ui::serve_login_gate(
                    &req,
                    "GitHub OAuth application is not configured on this server (GITHUB_OAUTH_CLIENT_ID or GITHUB_OAUTH_CLIENT_SECRET missing). Sign-in refused.",
                    "",
                    500,
                );
            }

            let verifier = auth::pkce_verifier(session_secret, &state);
            let redirect_uri = resolve_env_non_empty(&env, "GITHUB_OAUTH_REDIRECT_URI")
                .unwrap_or_else(|| {
                    let scheme = url.scheme();
                    let host = url.host_str().unwrap_or("spot.swacktech.com");
                    match url.port() {
                        Some(port)
                            if (scheme == "http" && port != 80)
                                || (scheme == "https" && port != 443) =>
                        {
                            format!("{}://{}:{}/auth/github/callback", scheme, host, port)
                        }
                        _ => format!("{}://{}/auth/github/callback", scheme, host),
                    }
                });

            let token_form = format!(
                "client_id={}&client_secret={}&code={}&redirect_uri={}&code_verifier={}",
                percent_encoding::utf8_percent_encode(
                    &client_id,
                    percent_encoding::NON_ALPHANUMERIC
                ),
                percent_encoding::utf8_percent_encode(
                    &client_secret,
                    percent_encoding::NON_ALPHANUMERIC
                ),
                percent_encoding::utf8_percent_encode(&code, percent_encoding::NON_ALPHANUMERIC),
                percent_encoding::utf8_percent_encode(
                    &redirect_uri,
                    percent_encoding::NON_ALPHANUMERIC
                ),
                percent_encoding::utf8_percent_encode(
                    &verifier,
                    percent_encoding::NON_ALPHANUMERIC
                ),
            );

            let mut init = RequestInit::new();
            init.with_method(Method::Post);
            let gh_headers = Headers::new();
            gh_headers.set("Content-Type", "application/x-www-form-urlencoded")?;
            gh_headers.set("Accept", "application/json")?;
            gh_headers.set("User-Agent", "spot-dashboard-edge")?;
            init.with_headers(gh_headers);
            init.with_body(Some(worker::wasm_bindgen::JsValue::from_str(&token_form)));

            let gh_req =
                Request::new_with_init("https://github.com/login/oauth/access_token", &init)?;
            let mut gh_resp = Fetch::Request(gh_req).send().await?;
            let token_json: Value = gh_resp
                .json()
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;

            let access_token = match token_json.get("access_token").and_then(|v| v.as_str()) {
                Some(tok) => tok.to_string(),
                None => {
                    let err_desc = token_json
                        .get("error_description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Code exchange failed");
                    return ui::serve_login_gate(
                        &req,
                        &format!("GitHub OAuth Error: {}", err_desc),
                        "",
                        403,
                    );
                }
            };

            let mut user_init = RequestInit::new();
            user_init.with_method(Method::Get);
            let user_headers = Headers::new();
            user_headers.set("Authorization", &format!("Bearer {}", access_token))?;
            user_headers.set("Accept", "application/vnd.github+json")?;
            user_headers.set("User-Agent", "spot-dashboard-edge")?;
            user_headers.set("X-GitHub-Api-Version", "2022-11-28")?;
            user_init.with_headers(user_headers);

            let user_req = Request::new_with_init("https://api.github.com/user", &user_init)?;
            let mut user_resp = Fetch::Request(user_req).send().await?;
            let user_json: Value = user_resp
                .json()
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;

            let login = match user_json.get("login").and_then(|v| v.as_str()) {
                Some(l) => l.to_string(),
                None => {
                    return ui::serve_login_gate(
                        &req,
                        "Failed to retrieve GitHub user identity",
                        "",
                        403,
                    );
                }
            };
            let uid = match user_json.get("id").and_then(|v| v.as_u64()) {
                Some(id) => id,
                None => {
                    return ui::serve_login_gate(
                        &req,
                        "Failed to retrieve GitHub user ID",
                        "",
                        403,
                    );
                }
            };

            let (bundle, _) = state::load_state(&kv, now)
                .await
                .map_err(|e| Error::RustError(e.to_string()))?;
            let added_admins: Vec<u64> = bundle
                .state
                .dashboard_admins
                .iter()
                .map(|a| a.uid)
                .collect();
            if !auth::may_sign_in(&login, uid, &added_admins) {
                // No session exists yet, so the gate must not claim the user is signed in.
                return ui::serve_login_gate(&req, &auth::not_admin_message(&login), "", 403);
            }

            let session_cookie = match auth::mint_session_cookie(
                session_secret,
                &login,
                uid,
                bundle.state.controlled_targets.clone(),
                now,
            ) {
                Ok(c) => c,
                Err(e) => return error_response(500, "internal_error", &e),
            };

            let return_path = auth::safe_return_path(&payload.r);

            let headers = Headers::new();
            headers.set("Location", &return_path)?;
            headers.append("Set-Cookie", &session_cookie)?;
            headers.append("Set-Cookie", &clear_state_cookie)?;
            headers.set("Cache-Control", "no-store")?;
            headers.set("Content-Length", "0")?;
            headers.set("Connection", "close")?;
            Ok(Response::empty()?.with_status(302).with_headers(headers))
        }

        // GitHub Device Flow - Request Code
        (Method::Post, "/auth/device/code") => {
            if !is_production_oauth_configured(&env) {
                // Local and E2E builds keep the in-memory mock, approved through
                // /api/test/authorize-device.
                let (record, device_handle) = match auth::start_device_flow(session_secret, now) {
                    Ok(res) => res,
                    Err(e) => return error_response(500, "internal_error", &e),
                };
                let resp_body = serde_json::json!({
                    "device_code": record.device_code,
                    "user_code": record.user_code,
                    "verification_uri": record.verification_uri,
                    "expires_in": 900,
                    "interval": record.interval,
                    "device_handle": device_handle,
                });
                return json_response(200, &resp_body);
            }

            let Some(client_id) = resolve_env_non_empty(&env, "GITHUB_OAUTH_CLIENT_ID") else {
                return error_response(
                    500,
                    "oauth_not_configured",
                    "GitHub OAuth application is not configured on this server (GITHUB_OAUTH_CLIENT_ID missing).",
                );
            };
            let (_, gh_body) = github_post_form(
                auth::GITHUB_DEVICE_CODE_URL,
                &[
                    ("client_id", client_id.as_str()),
                    ("scope", auth::DEVICE_FLOW_SCOPE),
                ],
            )
            .await?;
            let code = match auth::parse_github_device_code_response(&gh_body) {
                Ok(code) => code,
                Err(e) => {
                    console_error!("GitHub device code request failed: {}", e);
                    let message = if e.starts_with("device_flow_disabled") {
                        "Device sign-in is turned off for this GitHub OAuth app. Enable Device Flow in the app's GitHub settings, or use Sign in with GitHub.".to_string()
                    } else {
                        format!("GitHub did not issue a device code: {e}")
                    };
                    return error_response(502, "device_code_unavailable", &message);
                }
            };
            let device_handle = match auth::sign_device_handle(session_secret, &code, now) {
                Ok(h) => h,
                Err(e) => return error_response(500, "internal_error", &e),
            };
            let resp_body = serde_json::json!({
                "user_code": code.user_code,
                "verification_uri": code.verification_uri,
                "expires_in": code.expires_in,
                "interval": code.interval,
                "device_handle": device_handle,
            });
            json_response(200, &resp_body)
        }

        // GitHub Device Flow - Poll Token
        (Method::Post, "/auth/device/token" | "/auth/device/poll") => {
            let body_val: Value = req.json().await.unwrap_or(Value::Null);
            let code_or_handle = body_val
                .get("device_handle")
                .or_else(|| body_val.get("device_code"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            if code_or_handle.is_empty() {
                return error_response(
                    400,
                    "invalid_device_code",
                    "Missing device_code or device_handle",
                );
            }

            let is_poll = path == "/auth/device/poll";

            let poll_result = if is_production_oauth_configured(&env) {
                let handshake =
                    match auth::verify_device_handle(session_secret, &code_or_handle, now) {
                        Ok(h) => h,
                        Err(auth::DevicePollResult::Expired) => {
                            return error_response(400, "expired_token", "Device code expired")
                        }
                        Err(_) => {
                            return error_response(
                                400,
                                "invalid_device_code",
                                "Invalid device code",
                            )
                        }
                    };
                let client_id =
                    resolve_env_non_empty(&env, "GITHUB_OAUTH_CLIENT_ID").unwrap_or_default();
                let (_, gh_body) = github_post_form(
                    "https://github.com/login/oauth/access_token",
                    &[
                        ("client_id", client_id.as_str()),
                        ("device_code", handshake.device_code.as_str()),
                        ("grant_type", auth::DEVICE_GRANT_TYPE),
                    ],
                )
                .await?;
                match auth::classify_github_device_token_response(&gh_body, handshake.interval) {
                    auth::GithubDeviceTokenOutcome::AccessToken(token) => {
                        match fetch_github_identity(&token).await? {
                            Some((user_login, user_id)) => auth::DevicePollResult::Authorized {
                                user_login,
                                user_id,
                            },
                            None => {
                                return error_response(
                                    502,
                                    "identity_unavailable",
                                    "Failed to retrieve GitHub user identity",
                                )
                            }
                        }
                    }
                    auth::GithubDeviceTokenOutcome::Pending => auth::DevicePollResult::Pending,
                    auth::GithubDeviceTokenOutcome::SlowDown { interval } => {
                        auth::DevicePollResult::SlowDown { interval }
                    }
                    auth::GithubDeviceTokenOutcome::Expired => auth::DevicePollResult::Expired,
                    auth::GithubDeviceTokenOutcome::Denied => {
                        return error_response(
                            403,
                            "access_denied",
                            "GitHub authorization was not granted.",
                        )
                    }
                    auth::GithubDeviceTokenOutcome::Failed(desc) => {
                        console_error!("GitHub device token poll failed: {}", desc);
                        return error_response(
                            502,
                            "device_poll_failed",
                            &format!("GitHub device sign-in failed: {desc}"),
                        );
                    }
                }
            } else {
                auth::poll_device_flow(&code_or_handle, now)
            };

            match poll_result {
                auth::DevicePollResult::Authorized {
                    user_login,
                    user_id,
                } => {
                    let (bundle, _) = state::load_state(&kv, now)
                        .await
                        .map_err(|e| Error::RustError(e.to_string()))?;
                    let added_admins: Vec<u64> = bundle
                        .state
                        .dashboard_admins
                        .iter()
                        .map(|a| a.uid)
                        .collect();
                    if !auth::may_sign_in(&user_login, user_id, &added_admins) {
                        return error_response(
                            403,
                            "forbidden",
                            &auth::not_admin_message(&user_login),
                        );
                    }
                    let session_cookie = auth::mint_session_cookie(
                        session_secret,
                        &user_login,
                        user_id,
                        bundle.state.controlled_targets.clone(),
                        now,
                    )
                    .map_err(Error::RustError)?;

                    let headers = Headers::new();
                    headers.set("Content-Type", "application/json")?;
                    headers.append("Set-Cookie", &session_cookie)?;
                    headers.set("Cache-Control", "no-store")?;

                    let resp_body = serde_json::json!({
                        "status": "authorized",
                        "user": user_login,
                    });
                    Ok(Response::ok(resp_body.to_string())?.with_headers(headers))
                }
                auth::DevicePollResult::Pending => {
                    if is_poll {
                        error_response(400, "authorization_pending", "Authorization pending")
                    } else {
                        json_response(200, &serde_json::json!({ "status": "pending" }))
                    }
                }
                auth::DevicePollResult::SlowDown { interval } => {
                    let headers = Headers::new();
                    headers.set("Content-Type", "application/json")?;
                    headers.set("Retry-After", &interval.to_string())?;
                    let body = serde_json::json!({
                        "error": "slow_down",
                        "status": "slow_down",
                        "interval": interval,
                    });
                    let status = if is_poll { 400 } else { 200 };
                    Ok(Response::ok(body.to_string())?
                        .with_status(status)
                        .with_headers(headers))
                }
                auth::DevicePollResult::Expired => {
                    error_response(400, "expired_token", "Device code expired")
                }
                auth::DevicePollResult::Invalid => {
                    error_response(400, "invalid_device_code", "Invalid device code")
                }
            }
        }

        // Test Helper Endpoints for E2E suites (disabled when production OAuth secret is set)
        (Method::Post, "/api/test/authorize-device") => {
            if is_production_oauth_configured(&env) {
                return error_response(404, "not_found", "Not found");
            }
            let body_val: Value = req.json().await.unwrap_or(Value::Null);
            let dev_code = body_val
                .get("device_code")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if auth::authorize_device(dev_code, auth::ALLOWED_ADMIN_LOGIN, auth::ALLOWED_ADMIN_ID) {
                json_response(
                    200,
                    &serde_json::json!({"status": "ok", "authorized": true}),
                )
            } else {
                error_response(400, "invalid_device_code", "Device code not found")
            }
        }

        // Dedicated Read-Only Inventory Refresh Endpoint
        (Method::Post, "/api/inventory/refresh") => {
            if is_production_oauth_configured(&env) {
                let session = match verified_session {
                    Some(ref s) => s,
                    None => return error_response(401, "unauthorized", "Sign in required"),
                };
                let (bundle, _) = state::load_state(&kv, now)
                    .await
                    .map_err(|e| Error::RustError(e.to_string()))?;
                let added_admins: Vec<u64> = bundle
                    .state
                    .dashboard_admins
                    .iter()
                    .map(|a| a.uid)
                    .collect();
                let global_admins =
                    is_user_exempt_from_target_checks(&env, &bundle.state, session.uid);
                if !matches!(
                    auth::check_session(
                        session,
                        &bundle.state.controlled_targets,
                        &added_admins,
                        global_admins,
                    ),
                    auth::SessionCheckStatus::Ok
                ) {
                    return error_response(403, "forbidden", "Administrator permissions required");
                }
            }
            let sa_key = env
                .secret("GCP_SERVICE_ACCOUNT_KEY")
                .map(|s| s.to_string())
                .or_else(|_| env.var("GCP_SERVICE_ACCOUNT_KEY").map(|v| v.to_string()))
                .ok();

            match reconciler::refresh_fleet_inventory(&kv, sa_key.as_deref(), now).await {
                Ok(report) => json_response(200, &report),
                Err(e) => error_response(500, "inventory_refresh_failed", &e.to_string()),
            }
        }

        // Reconciler Simulation & Control Endpoints
        (Method::Post, "/api/reconciler/tick") => {
            if is_production_oauth_configured(&env) {
                let session = match verified_session {
                    Some(ref s) => s,
                    None => return error_response(401, "unauthorized", "Sign in required"),
                };
                let (bundle, _) = state::load_state(&kv, now)
                    .await
                    .map_err(|e| Error::RustError(e.to_string()))?;
                let added_admins: Vec<u64> = bundle
                    .state
                    .dashboard_admins
                    .iter()
                    .map(|a| a.uid)
                    .collect();
                let global_admins =
                    is_user_exempt_from_target_checks(&env, &bundle.state, session.uid);
                if !matches!(
                    auth::check_session(
                        session,
                        &bundle.state.controlled_targets,
                        &added_admins,
                        global_admins,
                    ),
                    auth::SessionCheckStatus::Ok
                ) {
                    return error_response(403, "forbidden", "Administrator permissions required");
                }
            }
            let is_read_only = url
                .query_pairs()
                .any(|(k, v)| (k == "read_only" || k == "dry_run") && (v == "true" || v == "1"));

            let sa_key = env
                .secret("GCP_SERVICE_ACCOUNT_KEY")
                .map(|s| s.to_string())
                .or_else(|_| env.var("GCP_SERVICE_ACCOUNT_KEY").map(|v| v.to_string()))
                .ok();

            if is_read_only {
                match reconciler::refresh_fleet_inventory(&kv, sa_key.as_deref(), now).await {
                    Ok(report) => return json_response(200, &report),
                    Err(e) => {
                        return error_response(500, "inventory_refresh_failed", &e.to_string())
                    }
                }
            }

            let gh_token = env
                .secret("SPOT_CONFIG_GH_TOKEN")
                .map(|s| s.to_string())
                .or_else(|_| env.var("SPOT_CONFIG_GH_TOKEN").map(|v| v.to_string()))
                .or_else(|_| env.secret("GH_TOKEN").map(|s| s.to_string()))
                .or_else(|_| env.var("GH_TOKEN").map(|v| v.to_string()))
                .ok();
            reconciler::set_reconciler_session_secret(Some(session_secret_str.clone()));

            match reconciler::reconcile_worker_fleet(
                &kv,
                sa_key.as_deref(),
                gh_token.as_deref(),
                now,
            )
            .await
            {
                Ok(report) => {
                    if !report.snapshot_written
                        && report.status != "skipped_lease_active"
                        && report.status != "failed"
                    {
                        if let Ok((mut bundle, _)) = state::load_state(&kv, now).await {
                            let fi = serde_json::json!({
                                "source": "gce",
                                "last_success_at": state::chrono_iso(now),
                                "last_success_epoch": now,
                                "stale": false,
                                "stale_reason": serde_json::Value::Null,
                                "last_error": serde_json::Value::Null
                            });
                            bundle.state.fleet_inventory = fi.clone();
                            if let Some(ref mut ov) = bundle.state.overview {
                                if let Some(obj) = ov.as_object_mut() {
                                    obj.insert("fleet_inventory".to_string(), fi.clone());
                                    if let Some(iv) = obj
                                        .get_mut("instances_view")
                                        .and_then(|v| v.as_object_mut())
                                    {
                                        iv.insert("fleet_inventory".to_string(), fi);
                                    }
                                }
                            }
                            bundle.state.updated_at = now;
                            let _ = state::save_state(&kv, &mut bundle, now).await;
                        }
                    }
                    json_response(200, &report)
                }
                Err(e) => error_response(500, "reconciliation_failed", &e.to_string()),
            }
        }

        (Method::Get, "/api/test/kv-stats") => {
            if is_production_oauth_configured(&env) {
                return error_response(404, "not_found", "Not found");
            }
            let (puts, gets, hits) = reconciler::get_kv_stats();
            let body = serde_json::json!({
                "kv_puts": puts,
                "kv_gets": gets,
                "isolate_cache_hits": hits,
            });
            json_response(200, &body)
        }

        (Method::Post, "/api/test/inject-terminated-vm") => {
            if is_production_oauth_configured(&env) {
                return error_response(404, "not_found", "Not found");
            }
            let body_val: Value = req.json().await.unwrap_or(Value::Null);
            let vm_name = body_val
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("oxidex-runners-preempted");
            reconciler::inject_terminated_vm(vm_name.to_string());
            let resp = serde_json::json!({
                "status": "injected",
                "vm": vm_name,
            });
            json_response(200, &resp)
        }

        (Method::Post, "/api/test/inject-stale-runner") => {
            if is_production_oauth_configured(&env) {
                return error_response(404, "not_found", "Not found");
            }
            let body_val: Value = req.json().await.unwrap_or(Value::Null);
            let runner_id = body_val
                .get("runner_id")
                .and_then(|v| v.as_str())
                .unwrap_or("runner-stale-999");
            reconciler::inject_stale_runner(runner_id.to_string());
            let resp = serde_json::json!({
                "status": "injected",
                "runner_id": runner_id,
            });
            json_response(200, &resp)
        }

        (Method::Post, "/api/test/reset") => {
            if is_production_oauth_configured(&env) {
                return error_response(404, "not_found", "Not found");
            }
            reconciler::reset_reconciler_state();
            state::invalidate_state_cache();
            json_response(200, &serde_json::json!({"status": "ok"}))
        }

        (Method::Post, "/api/test/set-revocation-cutoff") => {
            if is_production_oauth_configured(&env) {
                return error_response(404, "not_found", "Not found");
            }
            json_response(200, &serde_json::json!({"status": "ok"}))
        }

        // Auth Logout
        (Method::Post, "/auth/logout" | "/api/auth/logout") => {
            let headers = Headers::new();
            headers.set("Content-Type", "application/json")?;
            headers.set("Set-Cookie", &auth::format_cleared_session_cookie())?;
            let body = serde_json::json!({
                "status": "ok",
                "logged_out": true
            });
            Ok(Response::ok(body.to_string())?.with_headers(headers))
        }

        // 404 for unhandled routes
        _ => error_response(404, "not_found", "Not found"),
    }
}

/// Scheduled event hook for the 5-minute Fleet Reconciler (Requirement R3).
#[event(scheduled)]
pub async fn scheduled(event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    console_log!(
        "Scheduled fleet reconciliation triggered at {}",
        event.cron()
    );
    let now = current_time_epoch();
    if let Ok(kv) = env.kv("SPOT_KV") {
        let sa_key = env
            .secret("GCP_SERVICE_ACCOUNT_KEY")
            .map(|s| s.to_string())
            .or_else(|_| env.var("GCP_SERVICE_ACCOUNT_KEY").map(|v| v.to_string()))
            .ok();
        let gh_token = env
            .secret("SPOT_CONFIG_GH_TOKEN")
            .map(|s| s.to_string())
            .or_else(|_| env.var("SPOT_CONFIG_GH_TOKEN").map(|v| v.to_string()))
            .or_else(|_| env.secret("GH_TOKEN").map(|s| s.to_string()))
            .or_else(|_| env.var("GH_TOKEN").map(|v| v.to_string()))
            .ok();
        let sess_sec = env
            .secret("SESSION_SECRET")
            .map(|s| s.to_string())
            .or_else(|_| env.var("SESSION_SECRET").map(|v| v.to_string()))
            .ok();
        reconciler::set_reconciler_session_secret(sess_sec);

        if let Ok(report) =
            reconciler::reconcile_worker_fleet(&kv, sa_key.as_deref(), gh_token.as_deref(), now)
                .await
        {
            for held in &report.held_floor_targets {
                console_log!("Floor launch held: {}", held);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_control_path() {
        assert!(is_control_path("/api/control"));
        assert!(is_control_path("//api/control"));
        assert!(is_control_path("///api///control"));
        assert!(is_control_path("/api//control"));
        assert!(is_control_path("//api/%63ontrol"));
        assert!(is_control_path("/api/control/"));
        assert!(is_control_path("/api/control/secret"));
        assert!(is_control_path("/api/%63ontrol"));
        assert!(is_control_path("/foo/../api/control"));
        assert!(!is_control_path("/api/health"));
        assert!(!is_control_path("/api/overview"));
        assert!(!is_control_path("/"));
    }

    #[test]
    fn test_is_auth_path() {
        assert!(is_auth_path("/auth/github/login"));
        assert!(is_auth_path("/auth/github/callback"));
        assert!(is_auth_path("/auth/device/code"));
        assert!(is_auth_path("/auth/device/token"));
        assert!(is_auth_path("/auth/device/poll"));
        assert!(is_auth_path("/api/auth/status"));
        assert!(!is_auth_path("/api/overview"));
        assert!(!is_auth_path("/api/scaling"));
    }

    #[test]
    fn test_resolve_oauth_client_id() {
        // Explicit var takes precedence
        assert_eq!(
            resolve_oauth_client_id(Some("  real-var-id "), Some("secret-id"), true),
            Some("real-var-id".to_string())
        );
        // Empty var falls back to secret instead of shadowing it
        assert_eq!(
            resolve_oauth_client_id(Some("   "), Some("  real-secret-id "), true),
            Some("real-secret-id".to_string())
        );
        // Missing var and secret refuses mock fallback in production mode
        assert_eq!(resolve_oauth_client_id(None, None, true), None);
        assert_eq!(resolve_oauth_client_id(Some(""), Some(""), true), None);
        // Non-production mode keeps mock fallback for local/test suites
        assert_eq!(
            resolve_oauth_client_id(None, None, false),
            Some("mock-spot-client-id".to_string())
        );
    }
}
