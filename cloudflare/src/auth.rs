//! HMAC Session Authentication and OAuth module for Cloudflare Worker.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use hmac::{Hmac, Mac};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

type HmacSha256 = Hmac<Sha256>;

pub const ALLOWED_ADMIN_LOGIN: &str = "swackhamer";
pub const ALLOWED_ADMIN_ID: u64 = 619624;
pub const MAX_ADDED_ADMINS: usize = 50;

pub const SESSION_COOKIE_NAME: &str = "__Host-spot_session";
pub const OAUTH_STATE_COOKIE_NAME: &str = "__Host-spot_oauth_state";
pub const SESSION_PURPOSE_LABEL: &str = "spot-dashboard/session/v1";

pub const MAX_TOKEN_CHARS: usize = 4096;
pub const MAX_SESSION_COOKIE_BYTES: usize = 3900;
pub const MAX_STATE_TOKEN_CHARS: usize = 3072;
pub const MAX_SESSION_TTL_SECONDS: u64 = 604_800; // 7 days
pub const DEFAULT_DASHBOARD_SESSION_TTL_SECONDS: u64 = 43_200; // 12 hours
pub const DEFAULT_STATE_TTL_SECONDS: u64 = 600; // 10 minutes
pub const CLOCK_SKEW_SECONDS: u64 = 60;
pub const DEFAULT_SCOPES: &str = "repo read:org";

pub const DEVICE_MIN_INTERVAL_SECONDS: u64 = 5;
pub const DEVICE_SLOW_DOWN_INCREMENT_SECONDS: u64 = 5;
pub const DEVICE_MAX_INTERVAL_SECONDS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionData {
    pub exp: u64,
    pub iat: u64,
    pub sub: String,
    pub tgt: Vec<String>,
    pub uid: u64,
    pub v: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthStatePayload {
    pub exp: u64,
    pub iat: u64,
    pub n: String,
    pub r: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tc: Option<Value>,
    pub v: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHandshakeRecord {
    pub device_code: String,
    pub expires_at: u64,
    pub interval: u64,
    pub next_poll_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCheckStatus {
    Ok,
    None,
    Forbidden { message: String, missing_targets: Vec<String> },
}

/// Derives a 32-byte purpose key: HMAC-SHA256(secret, "spot-dashboard/<purpose>/v1")
pub fn derive_purpose_key(secret: &[u8], purpose: &str) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC can take key of any size");
    let label = format!("spot-dashboard/{purpose}/v1");
    mac.update(label.as_bytes());
    mac.finalize().into_bytes().into()
}

/// Constant-time comparison between two slices
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Constant-time hex string comparison
pub fn constant_time_hex_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    constant_time_eq(a.as_bytes(), b.as_bytes())
}

/// Converts any serde_json Value to compact JSON with alphabetically sorted keys
pub fn to_compact_sorted_json<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let val = serde_json::to_value(value)?;
    fn sort_val(v: Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut btree = BTreeMap::new();
                for (k, v) in map {
                    btree.insert(k, sort_val(v));
                }
                let mut sorted_map = serde_json::Map::new();
                for (k, v) in btree {
                    sorted_map.insert(k, v);
                }
                Value::Object(sorted_map)
            }
            Value::Array(arr) => Value::Array(arr.into_iter().map(sort_val).collect()),
            other => other,
        }
    }
    serde_json::to_string(&sort_val(val))
}

/// Signs payload: base64url_nopad(compact_sorted_json) + "." + hex(HMAC-SHA256(key, body_ascii))
pub fn sign_token<T: Serialize>(key: &[u8; 32], payload: &T) -> Result<String, String> {
    let json_str = to_compact_sorted_json(payload).map_err(|e| e.to_string())?;
    let body_b64 = URL_SAFE_NO_PAD.encode(json_str.as_bytes());

    let mut mac = HmacSha256::new_from_slice(key).map_err(|e| e.to_string())?;
    mac.update(body_b64.as_bytes());
    let sig_bytes = mac.finalize().into_bytes();
    let sig_hex = hex::encode(sig_bytes);

    let token = format!("{body_b64}.{sig_hex}");
    if token.len() > MAX_TOKEN_CHARS {
        return Err("Token exceeds maximum length".to_string());
    }
    Ok(token)
}

/// Verifies token signature and parses JSON payload
pub fn verify_token_signature(key: &[u8; 32], token: &str) -> Option<Value> {
    if token.is_empty() || token.len() > MAX_TOKEN_CHARS {
        return None;
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 2 {
        return None;
    }
    let (body_b64, sig_hex) = (parts[0], parts[1]);
    if sig_hex.len() != 64 || !sig_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(body_b64.as_bytes());
    let expected_sig = hex::encode(mac.finalize().into_bytes());

    if !constant_time_hex_eq(sig_hex, &expected_sig) {
        return None;
    }

    let decoded_bytes = URL_SAFE_NO_PAD.decode(body_b64).ok()?;
    let json_val: Value = serde_json::from_slice(&decoded_bytes).ok()?;
    if !json_val.is_object() {
        return None;
    }
    Some(json_val)
}

pub fn parse_cookies(cookie_header: &str) -> HashMap<String, String> {
    let mut cookies = HashMap::new();
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some((name, val)) = part.split_once('=') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let mut val = val.trim();
            if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
                val = &val[1..val.len() - 1];
            }
            cookies.entry(name.to_string()).or_insert_with(|| val.to_string());
        }
    }
    cookies
}

pub fn format_session_cookie(token: &str, max_age: u64) -> String {
    format!("{SESSION_COOKIE_NAME}={token}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax")
}

pub fn format_cleared_session_cookie() -> String {
    format!("{SESSION_COOKIE_NAME}=; Path=/; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT; HttpOnly; Secure; SameSite=Lax")
}

pub fn format_state_cookie(state: &str, max_age: u64) -> String {
    format!("{OAUTH_STATE_COOKIE_NAME}={state}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax")
}

pub fn format_cleared_state_cookie() -> String {
    format!("{OAUTH_STATE_COOKIE_NAME}=; Path=/; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT; HttpOnly; Secure; SameSite=Lax")
}

pub fn safe_return_path(value: &str) -> String {
    if value.is_empty() || value.len() > 512 {
        return "/".to_string();
    }
    if !value.starts_with('/') || value.starts_with("//") || value.contains('\\') {
        return "/".to_string();
    }
    if !value.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return "/".to_string();
    }
    if value.contains("://") {
        return "/".to_string();
    }
    value.to_string()
}

pub fn is_valid_github_login(login: &str) -> bool {
    if login.is_empty() || login.len() > 39 {
        return false;
    }
    let bytes = login.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'-')
}

pub fn parse_session_not_before(raw: Option<&str>) -> Option<u64> {
    match raw {
        None => Some(0),
        Some(s) if s.trim().is_empty() => Some(0),
        Some(s) => {
            let s = s.trim();
            if s.len() > 15 || !s.chars().all(|c| c.is_ascii_digit()) {
                None // Fail closed on malformed non-numeric timestamp
            } else {
                s.parse::<u64>().ok()
            }
        }
    }
}

pub fn verify_session(
    token: &str,
    session_secret: &[u8],
    session_not_before_str: Option<&str>,
    now: u64,
) -> Option<SessionData> {
    let nbf = parse_session_not_before(session_not_before_str)?;
    let session_key = derive_purpose_key(session_secret, "session");
    let payload_val = verify_token_signature(&session_key, token)?;

    let session: SessionData = serde_json::from_value(payload_val).ok()?;
    if session.v != 1 {
        return None;
    }
    if session.exp <= session.iat || session.exp - session.iat > MAX_SESSION_TTL_SECONDS {
        return None;
    }
    // Clock skew check: iat - 60 <= now < exp
    let min_now = session.iat.saturating_sub(CLOCK_SKEW_SECONDS);
    if !(min_now <= now && now < session.exp) {
        return None;
    }
    // Bulk revocation check
    if session.iat < nbf {
        return None;
    }
    // Identity syntax checks
    if !is_valid_github_login(&session.sub) {
        return None;
    }
    if session.uid == 0 || session.uid > ((1u64 << 53) - 1) {
        return None;
    }
    Some(session)
}

pub fn is_owner(login: &str, uid: u64) -> bool {
    login.eq_ignore_ascii_case(ALLOWED_ADMIN_LOGIN) && uid == ALLOWED_ADMIN_ID
}

pub fn may_sign_in(login: &str, uid: u64, added_admins: &[u64]) -> bool {
    if is_owner(login, uid) {
        return true;
    }
    uid > 0 && added_admins.contains(&uid)
}

pub fn check_session(
    session: &SessionData,
    controlled_targets: &[String],
    added_admins: &[u64],
    global_admins: bool,
) -> SessionCheckStatus {
    if !may_sign_in(&session.sub, session.uid, added_admins) {
        return SessionCheckStatus::Forbidden {
            message: format!("Forbidden: GitHub user '{}' is not an authorized administrator", session.sub),
            missing_targets: Vec::new(),
        };
    }
    if global_admins {
        return SessionCheckStatus::Ok;
    }
    let verified: std::collections::HashSet<String> = session.tgt.iter().map(|t| t.to_lowercase()).collect();
    let missing: Vec<String> = controlled_targets
        .iter()
        .filter(|t| !verified.contains(&t.to_lowercase()))
        .cloned()
        .collect();

    if !missing.is_empty() {
        return SessionCheckStatus::Forbidden {
            message: format!(
                "Forbidden: session was not verified for all controlled targets; sign in again. Missing: {}",
                missing.join(", ")
            ),
            missing_targets: missing,
        };
    }
    SessionCheckStatus::Ok
}

pub fn pkce_verifier(secret: &[u8], state: &str) -> String {
    let pkce_key = derive_purpose_key(secret, "pkce");
    let mut mac = HmacSha256::new_from_slice(&pkce_key).expect("Valid key");
    mac.update(state.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

pub fn pkce_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

pub fn generate_nonce() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| e.to_string())?;
    Ok(hex::encode(bytes))
}

pub fn create_oauth_state(
    secret: &[u8],
    return_to: &str,
    now: u64,
) -> Result<(String, String), String> {
    let nonce = generate_nonce()?;
    let payload = OAuthStatePayload {
        v: 1,
        n: nonce,
        iat: now,
        exp: now + DEFAULT_STATE_TTL_SECONDS,
        r: safe_return_path(return_to),
        tc: None,
    };
    let state_key = derive_purpose_key(secret, "oauth-state");
    let signed_state = sign_token(&state_key, &payload)?;
    let cookie = format_state_cookie(&signed_state, DEFAULT_STATE_TTL_SECONDS);
    Ok((signed_state, cookie))
}

pub fn build_github_authorize_url(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> String {
    let encoded_redirect = percent_encoding::utf8_percent_encode(
        redirect_uri,
        percent_encoding::NON_ALPHANUMERIC,
    );
    let encoded_state = percent_encoding::utf8_percent_encode(
        state,
        percent_encoding::NON_ALPHANUMERIC,
    );
    let encoded_challenge = percent_encoding::utf8_percent_encode(
        challenge,
        percent_encoding::NON_ALPHANUMERIC,
    );
    format!(
        "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope=read%3Aorg%2Crepo&state={}&code_challenge={}&code_challenge_method=S256&allow_signup=false",
        client_id, encoded_redirect, encoded_state, encoded_challenge
    )
}

pub fn verify_oauth_state(
    secret: &[u8],
    param_state: &str,
    cookie_state: &str,
    now: u64,
) -> Result<OAuthStatePayload, &'static str> {
    if param_state.is_empty() || cookie_state.is_empty() {
        return Err("Missing state parameter or cookie");
    }
    if param_state.len() > MAX_TOKEN_CHARS || cookie_state.len() > MAX_TOKEN_CHARS {
        return Err("State exceeds maximum length");
    }
    if !constant_time_eq(param_state.as_bytes(), cookie_state.as_bytes()) {
        return Err("State mismatch between query parameter and cookie");
    }
    let state_key = derive_purpose_key(secret, "oauth-state");
    let val = verify_token_signature(&state_key, param_state)
        .ok_or("Invalid state token signature")?;
    let payload: OAuthStatePayload = serde_json::from_value(val)
        .map_err(|_| "Malformed state token payload")?;
    if payload.v != 1 {
        return Err("Invalid state token version");
    }
    if payload.exp <= now || payload.iat > now + CLOCK_SKEW_SECONDS || payload.exp.saturating_sub(payload.iat) > DEFAULT_STATE_TTL_SECONDS {
        return Err("State token expired or invalid timestamp");
    }
    Ok(payload)
}

thread_local! {
    static USED_NONCES: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    static DEVICE_STORE: RefCell<HashMap<String, DeviceRecord>> = RefCell::new(HashMap::new());
    static HANDLE_TO_CODE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

pub fn mark_state_nonce_used(nonce: &str) -> bool {
    USED_NONCES.with(|cell| {
        let mut set = cell.borrow_mut();
        if set.contains(nonce) {
            false
        } else {
            set.insert(nonce.to_string());
            true
        }
    })
}

pub fn mint_session_cookie(
    secret: &[u8],
    login: &str,
    uid: u64,
    targets: Vec<String>,
    now: u64,
) -> Result<String, String> {
    let session = SessionData {
        v: 1,
        sub: login.to_string(),
        uid,
        iat: now,
        exp: now + DEFAULT_DASHBOARD_SESSION_TTL_SECONDS,
        tgt: targets,
    };
    let session_key = derive_purpose_key(secret, "session");
    let token = sign_token(&session_key, &session)?;
    Ok(format_session_cookie(&token, DEFAULT_DASHBOARD_SESSION_TTL_SECONDS))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_at: u64,
    pub interval: u64,
    pub next_poll_at: u64,
    pub authorized: bool,
    pub user_login: String,
    pub user_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePollResult {
    Authorized { user_login: String, user_id: u64 },
    Pending,
    SlowDown { interval: u64 },
    Expired,
    Invalid,
}

pub fn start_device_flow(
    secret: &[u8],
    now: u64,
) -> Result<(DeviceRecord, String), String> {
    let mut dev_bytes = [0u8; 32];
    getrandom::getrandom(&mut dev_bytes).map_err(|e| e.to_string())?;
    let device_code = hex::encode(dev_bytes);

    let mut user_bytes = [0u8; 8];
    getrandom::getrandom(&mut user_bytes).map_err(|e| e.to_string())?;
    const ALPHANUM: &[u8] = b"BCDFGHJKLMNPQRSTVWXYZ23456789";
    let p1: String = (0..4).map(|i| ALPHANUM[(user_bytes[i] as usize) % ALPHANUM.len()] as char).collect();
    let p2: String = (4..8).map(|i| ALPHANUM[(user_bytes[i] as usize) % ALPHANUM.len()] as char).collect();
    let user_code = format!("{}-{}", p1, p2);

    let handshake = DeviceHandshakeRecord {
        device_code: device_code.clone(),
        expires_at: now + 900,
        interval: 5,
        next_poll_at: now + 5,
    };
    let device_key = derive_purpose_key(secret, "device-flow");
    let device_handle = sign_token(&device_key, &handshake)?;

    let record = DeviceRecord {
        device_code: device_code.clone(),
        user_code,
        verification_uri: "https://github.com/login/device".to_string(),
        expires_at: now + 900,
        interval: 5,
        next_poll_at: now + 5,
        authorized: false,
        user_login: ALLOWED_ADMIN_LOGIN.to_string(),
        user_id: ALLOWED_ADMIN_ID,
    };

    DEVICE_STORE.with(|cell| {
        let mut store = cell.borrow_mut();
        store.insert(device_code.clone(), record.clone());
    });
    HANDLE_TO_CODE.with(|cell| {
        let mut map = cell.borrow_mut();
        map.insert(device_handle.clone(), device_code.clone());
    });

    Ok((record, device_handle))
}

pub fn poll_device_flow(code_or_handle: &str, now: u64) -> DevicePollResult {
    if code_or_handle.is_empty() || code_or_handle.len() > 512 {
        return DevicePollResult::Invalid;
    }
    let code = HANDLE_TO_CODE.with(|cell| {
        cell.borrow().get(code_or_handle).cloned().unwrap_or_else(|| code_or_handle.to_string())
    });
    DEVICE_STORE.with(|cell| {
        let mut store = cell.borrow_mut();
        let record = match store.get_mut(&code) {
            Some(r) => r,
            None => return DevicePollResult::Invalid,
        };
        if now >= record.expires_at {
            return DevicePollResult::Expired;
        }
        if record.authorized {
            return DevicePollResult::Authorized {
                user_login: record.user_login.clone(),
                user_id: record.user_id,
            };
        }
        if now < record.next_poll_at {
            record.interval = (record.interval + DEVICE_SLOW_DOWN_INCREMENT_SECONDS).min(DEVICE_MAX_INTERVAL_SECONDS);
            record.next_poll_at = now + record.interval;
            return DevicePollResult::SlowDown { interval: record.interval };
        }
        record.next_poll_at = now + record.interval;
        DevicePollResult::Pending
    })
}

pub fn authorize_device(code_or_handle: &str, user_login: &str, user_id: u64) -> bool {
    let code = HANDLE_TO_CODE.with(|cell| {
        cell.borrow().get(code_or_handle).cloned().unwrap_or_else(|| code_or_handle.to_string())
    });
    DEVICE_STORE.with(|cell| {
        let mut store = cell.borrow_mut();
        if let Some(record) = store.get_mut(&code) {
            record.authorized = true;
            record.user_login = user_login.to_string();
            record.user_id = user_id;
            true
        } else {
            false
        }
    })
}
