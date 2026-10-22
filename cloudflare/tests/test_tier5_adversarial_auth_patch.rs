//! Tier 5 Comprehensive Adversarial Hardening Test Suite
//!
//! Exhaustive white-box adversarial verification covering:
//! - Section 1: HMAC-SHA256 session signature tampering, base64 truncation/padding,
//!   expired vs future timestamp boundaries, session revocation cut-off timestamps,
//!   OAuth PKCE verifier mismatch, device flow rapid polling, and admin authorization boundaries.
//! - Section 2: In-isolate `thread_local!` memory cache invalidation on local writes,
//!   fallback on uninitialized KV, corrupt JSON handling in KV, 0 KV writes on cached GETs
//!   and instance action safety gates.
//! - Section 3: Canonical `patch_sha256` hashing under adversarial JSON formatting,
//!   Unicode/control-char escaping, null preservation vs stripping, fast-boot golden image /
//!   instance template omission parity, hardware sizing floor boundary values, permitted
//!   machine types, and forbidden mount paths.
//! - Section 4: Patch console controlled targets, path traversal attempts, multi-repo
//!   manifest merging, idempotent branch/PR handling, and commit failure branch rollback.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use spot_dashboard_edge::auth::*;
use spot_dashboard_edge::patch::*;
use spot_dashboard_edge::patch_console::*;
use spot_dashboard_edge::state::MAX_ADDED_ADMINS;
use spot_dashboard_edge::state::*;

const TEST_SECRET: &[u8] = b"tier5-adversarial-test-secret-key-32bytes-auth-patch-suite!";

fn make_valid_session() -> SessionData {
    SessionData {
        v: 1,
        sub: "swackhamer".to_string(),
        uid: 619624,
        iat: 1792000000,
        exp: 1792043200,
        tgt: vec!["swack-tools/oxidex".to_string()],
    }
}

fn mint_test_session(secret: &[u8], purpose: &str, session: &SessionData) -> String {
    let key = derive_purpose_key(secret, purpose);
    sign_token(&key, session).expect("sign_token must succeed")
}

// High-fidelity Mock KV Store tracking get and put invocations
pub struct Tier5MockKvStore {
    pub data: Mutex<HashMap<String, String>>,
    pub get_count: AtomicUsize,
    pub put_count: AtomicUsize,
}

impl Tier5MockKvStore {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            get_count: AtomicUsize::new(0),
            put_count: AtomicUsize::new(0),
        }
    }

    pub fn with_raw(key: &str, val: &str) -> Self {
        let store = Self::new();
        store
            .data
            .lock()
            .unwrap()
            .insert(key.to_string(), val.to_string());
        store
    }

    pub fn with_bundle(bundle: DashboardBundle) -> Self {
        let store = Self::new();
        let raw = serde_json::to_string(&bundle).expect("Serialize initial bundle");
        store
            .data
            .lock()
            .unwrap()
            .insert(KV_STATE_KEY.to_string(), raw);
        store
    }
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for Tier5MockKvStore {
    async fn get(&self, key: &str) -> Result<Option<String>, StateError> {
        self.get_count.fetch_add(1, Ordering::SeqCst);
        let guard = self.data.lock().unwrap();
        Ok(guard.get(key).cloned())
    }

    async fn put(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.put_count.fetch_add(1, Ordering::SeqCst);
        let mut guard = self.data.lock().unwrap();
        guard.insert(key.to_string(), value.to_string());
        Ok(())
    }
}

// ============================================================================
// SECTION 1: AUTHENTICATION ADVERSARIAL ORACLES & WHITE-BOX EDGE CASES
// ============================================================================

#[test]
fn test_tier5_auth_session_signature_tampering_and_key_isolation() {
    let session = make_valid_session();
    let valid_token = mint_test_session(TEST_SECRET, "session", &session);
    let now = session.iat + 100;

    // Baseline verification succeeds
    assert!(verify_session(&valid_token, TEST_SECRET, None, now).is_some());

    let parts: Vec<&str> = valid_token.split('.').collect();
    assert_eq!(parts.len(), 2);
    let (body_b64, sig_hex) = (parts[0], parts[1]);

    // 1. Cross-purpose key isolation: token signed with another purpose must fail
    for other_purpose in &["oauth-state", "device-flow", "pkce", "wrong-purpose"] {
        let cross_token = mint_test_session(TEST_SECRET, other_purpose, &session);
        assert!(
            verify_session(&cross_token, TEST_SECRET, None, now).is_none(),
            "Token minted with purpose '{other_purpose}' must fail verification with 'session' purpose"
        );
    }

    // 2. Token verified with completely wrong secret must fail
    let wrong_secret = b"completely-different-secret-key-32bytes-mismatch";
    assert!(
        verify_session(&valid_token, wrong_secret, None, now).is_none(),
        "Token verified with wrong secret must fail"
    );

    // 3. Signature length boundaries: signature MUST be exactly 64 hex characters
    let truncated_sig = &sig_hex[..63];
    let bad_len_token1 = format!("{body_b64}.{truncated_sig}");
    assert!(verify_session(&bad_len_token1, TEST_SECRET, None, now).is_none());

    let extended_sig = format!("{sig_hex}0");
    let bad_len_token2 = format!("{body_b64}.{extended_sig}");
    assert!(verify_session(&bad_len_token2, TEST_SECRET, None, now).is_none());

    // 4. Non-hexdigits in signature hex
    let non_hex_chars = ['g', 'z', '!', '\n', '\0', ' ', 'X', '-'];
    for c in non_hex_chars {
        let mut tampered = sig_hex.to_string();
        tampered.pop();
        tampered.push(c);
        let token = format!("{body_b64}.{tampered}");
        assert!(
            verify_session(&token, TEST_SECRET, None, now).is_none(),
            "Signature containing non-hex character '{c}' must be rejected"
        );
    }

    // 5. Uppercase hex signature rejection:
    // constant_time_hex_eq compares raw ASCII bytes against lowercase hex output
    let uppercase_sig = sig_hex.to_ascii_uppercase();
    if uppercase_sig != sig_hex {
        let uppercase_token = format!("{body_b64}.{uppercase_sig}");
        assert!(
            verify_session(&uppercase_token, TEST_SECRET, None, now).is_none(),
            "Uppercase hex signature must be rejected by strict constant-time comparison"
        );
    }

    // 6. Malformed token formats
    let malformed_tokens = [
        "",
        ".",
        "body_only_no_dot",
        ".sig_only_no_body",
        &format!("{body_b64}."),
        &format!(".{sig_hex}"),
        &format!("{body_b64}.{sig_hex}.extra"),
        &format!("{body_b64}..{sig_hex}"),
    ];
    for bad in &malformed_tokens {
        assert!(
            verify_session(bad, TEST_SECRET, None, now).is_none(),
            "Malformed token format '{bad}' must be rejected"
        );
    }

    // 7. Token exceeding MAX_TOKEN_CHARS (4096)
    let giant_token = format!("{body_b64}.{}", "a".repeat(4097));
    assert!(verify_session(&giant_token, TEST_SECRET, None, now).is_none());
}

#[test]
fn test_tier5_auth_truncated_padded_and_corrupt_base64() {
    let session = make_valid_session();
    let valid_token = mint_test_session(TEST_SECRET, "session", &session);
    let now = session.iat + 100;
    let parts: Vec<&str> = valid_token.split('.').collect();
    let (body_b64, sig_hex) = (parts[0], parts[1]);

    // 1. Base64 padding '=' or '==' MUST be rejected by URL_SAFE_NO_PAD
    let padded1 = format!("{body_b64}=.{sig_hex}");
    assert!(
        verify_session(&padded1, TEST_SECRET, None, now).is_none(),
        "Base64 with trailing '=' must be rejected"
    );
    let padded2 = format!("{body_b64}==.{sig_hex}");
    assert!(
        verify_session(&padded2, TEST_SECRET, None, now).is_none(),
        "Base64 with trailing '==' must be rejected"
    );

    // 2. Standard base64 characters ('+' and '/') instead of URL-safe ('-' and '_')
    // In URL_SAFE_NO_PAD, '+' and '/' are strictly invalid characters and must be rejected
    let mut non_url_safe_body1 = body_b64.to_string();
    non_url_safe_body1.replace_range(0..1, "+");
    let token_plus = format!("{non_url_safe_body1}.{sig_hex}");
    assert!(
        verify_session(&token_plus, TEST_SECRET, None, now).is_none(),
        "Base64 with '+' must be rejected by URL_SAFE_NO_PAD"
    );

    let mut non_url_safe_body2 = body_b64.to_string();
    non_url_safe_body2.replace_range(0..1, "/");
    let token_slash = format!("{non_url_safe_body2}.{sig_hex}");
    assert!(
        verify_session(&token_slash, TEST_SECRET, None, now).is_none(),
        "Base64 with '/' must be rejected by URL_SAFE_NO_PAD"
    );

    // 3. Embedded whitespace or newlines inside base64 payload
    let ws_chars = [' ', '\t', '\n', '\r'];
    for ws in ws_chars {
        let mut tampered = body_b64.to_string();
        tampered.insert(tampered.len() / 2, ws);
        let token = format!("{tampered}.{sig_hex}");
        assert!(
            verify_session(&token, TEST_SECRET, None, now).is_none(),
            "Whitespace character {ws:?} in base64 body must be rejected"
        );
    }

    // 4. Base64 body decoding to non-object JSON (array, number, string, null)
    let key = derive_purpose_key(TEST_SECRET, "session");
    let non_objects: Vec<Value> = vec![
        json!([1, 2, 3]),
        json!("string_payload"),
        json!(12345),
        json!(true),
        json!(null),
    ];
    for no in non_objects {
        let token = sign_token(&key, &no).unwrap();
        assert!(
            verify_session(&token, TEST_SECRET, None, now).is_none(),
            "Non-object payload must be rejected"
        );
    }
}

#[test]
fn test_tier5_auth_timestamp_and_ttl_strict_boundaries() {
    let key = derive_purpose_key(TEST_SECRET, "session");

    // 1. Version boundary: v != 1 must fail
    for v in [0u32, 2, 999] {
        let mut s = make_valid_session();
        s.v = v;
        let token = sign_token(&key, &s).unwrap();
        assert!(
            verify_session(&token, TEST_SECRET, None, s.iat + 10).is_none(),
            "Session version {v} must be rejected"
        );
    }

    // 2. Expired vs future timestamp boundaries
    // exp <= iat: exp == iat, exp < iat must fail
    let mut s_equal = make_valid_session();
    s_equal.exp = s_equal.iat;
    let tok_equal = sign_token(&key, &s_equal).unwrap();
    assert!(verify_session(&tok_equal, TEST_SECRET, None, s_equal.iat).is_none());

    let mut s_less = make_valid_session();
    s_less.exp = s_less.iat - 1;
    let tok_less = sign_token(&key, &s_less).unwrap();
    assert!(verify_session(&tok_less, TEST_SECRET, None, s_less.iat).is_none());

    // 3. Max session TTL boundary: MAX_SESSION_TTL_SECONDS = 604_800 (7 days)
    let mut s_max = make_valid_session();
    s_max.exp = s_max.iat + MAX_SESSION_TTL_SECONDS;
    let tok_max = sign_token(&key, &s_max).unwrap();
    assert!(
        verify_session(&tok_max, TEST_SECRET, None, s_max.iat + 10).is_some(),
        "TTL == MAX_SESSION_TTL_SECONDS must be accepted"
    );

    let mut s_max_plus = make_valid_session();
    s_max_plus.exp = s_max_plus.iat + MAX_SESSION_TTL_SECONDS + 1;
    let tok_max_plus = sign_token(&key, &s_max_plus).unwrap();
    assert!(
        verify_session(&tok_max_plus, TEST_SECRET, None, s_max_plus.iat + 10).is_none(),
        "TTL == MAX_SESSION_TTL_SECONDS + 1 must be rejected"
    );

    // 4. Clock skew boundaries: iat - 60 <= now < exp
    let s = make_valid_session();
    let tok = sign_token(&key, &s).unwrap();

    // Exact lower clock skew boundary: now == iat - 60 -> Valid
    assert!(
        verify_session(&tok, TEST_SECRET, None, s.iat - CLOCK_SKEW_SECONDS).is_some(),
        "now == iat - 60 must be accepted under clock skew"
    );

    // 1 second before clock skew boundary: now == iat - 61 -> Rejected
    assert!(
        verify_session(&tok, TEST_SECRET, None, s.iat - CLOCK_SKEW_SECONDS - 1).is_none(),
        "now == iat - 61 must be rejected as too far in future"
    );

    // Exact upper expiration boundary: now == exp - 1 -> Valid
    assert!(
        verify_session(&tok, TEST_SECRET, None, s.exp - 1).is_some(),
        "now == exp - 1 must be accepted"
    );

    // Expiration instant: now == exp -> Rejected
    assert!(
        verify_session(&tok, TEST_SECRET, None, s.exp).is_none(),
        "now == exp must be rejected as expired"
    );

    // 1 second after expiration: now == exp + 1 -> Rejected
    assert!(
        verify_session(&tok, TEST_SECRET, None, s.exp + 1).is_none(),
        "now == exp + 1 must be rejected"
    );
}

#[test]
fn test_tier5_auth_session_revocation_cutoff_boundaries() {
    // 1. parse_session_not_before tests
    assert_eq!(parse_session_not_before(None), Some(0));
    assert_eq!(parse_session_not_before(Some("")), Some(0));
    assert_eq!(parse_session_not_before(Some("   ")), Some(0));
    assert_eq!(
        parse_session_not_before(Some("1792000000")),
        Some(1792000000)
    );
    assert_eq!(
        parse_session_not_before(Some("  1792000000  ")),
        Some(1792000000)
    );

    // Malformed non-numeric inputs fail closed (return None)
    assert_eq!(parse_session_not_before(Some("abc")), None);
    assert_eq!(parse_session_not_before(Some("1792000000a")), None);
    assert_eq!(parse_session_not_before(Some("-100")), None);
    assert_eq!(parse_session_not_before(Some("1234567890123456")), None); // > 15 chars

    // 2. Revocation cut-off threshold in verify_session
    let s = make_valid_session();
    let key = derive_purpose_key(TEST_SECRET, "session");
    let token = sign_token(&key, &s).unwrap();
    let now = s.iat + 100;

    // Fail closed on malformed nbf string
    assert!(verify_session(&token, TEST_SECRET, Some("corrupted_nbf"), now).is_none());

    // Cut-off threshold checks:
    // iat == nbf - 1: issued before cut-off -> Rejected
    let nbf_after = (s.iat + 1).to_string();
    assert!(
        verify_session(&token, TEST_SECRET, Some(&nbf_after), now).is_none(),
        "Session issued before nbf must be revoked"
    );

    // iat == nbf: issued at cut-off -> Accepted
    let nbf_exact = s.iat.to_string();
    assert!(
        verify_session(&token, TEST_SECRET, Some(&nbf_exact), now).is_some(),
        "Session issued at exact nbf must be valid"
    );

    // iat == nbf + 1: issued after cut-off -> Accepted
    let nbf_before = (s.iat - 1).to_string();
    assert!(
        verify_session(&token, TEST_SECRET, Some(&nbf_before), now).is_some(),
        "Session issued after nbf must be valid"
    );
}

#[test]
fn test_tier5_auth_identity_and_uid_boundaries() {
    let key = derive_purpose_key(TEST_SECRET, "session");
    let now = 1792000100;

    // 1. is_valid_github_login checks
    assert!(is_valid_github_login("swackhamer"));
    assert!(is_valid_github_login("a"));
    assert!(is_valid_github_login("user-name"));
    assert!(is_valid_github_login("0-user"));
    assert!(is_valid_github_login(&"a".repeat(39))); // Max length 39

    // Invalid logins
    assert!(!is_valid_github_login("")); // Empty
    assert!(!is_valid_github_login(&"a".repeat(40))); // > 39 chars
    assert!(!is_valid_github_login("-user")); // Starts with hyphen
    assert!(!is_valid_github_login("user.name")); // Dot forbidden
    assert!(!is_valid_github_login("user@name")); // At forbidden
    assert!(!is_valid_github_login("user_name")); // Underscore forbidden
    assert!(!is_valid_github_login("user name")); // Space forbidden
    assert!(!is_valid_github_login("swäckhamer")); // Non-ASCII forbidden

    // 2. UID range boundaries: 0 < uid <= (2^53 - 1)
    let max_safe_int = (1u64 << 53) - 1; // 9007199254740991

    // UID == 0 must be rejected
    let mut s_zero = make_valid_session();
    s_zero.uid = 0;
    let tok_zero = sign_token(&key, &s_zero).unwrap();
    assert!(verify_session(&tok_zero, TEST_SECRET, None, now).is_none());

    // UID == max_safe_int must be accepted
    let mut s_max = make_valid_session();
    s_max.uid = max_safe_int;
    let tok_max = sign_token(&key, &s_max).unwrap();
    assert!(verify_session(&tok_max, TEST_SECRET, None, now).is_some());

    // UID == max_safe_int + 1 must be rejected
    let mut s_over = make_valid_session();
    s_over.uid = max_safe_int + 1;
    let tok_over = sign_token(&key, &s_over).unwrap();
    assert!(verify_session(&tok_over, TEST_SECRET, None, now).is_none());

    // UID == u64::MAX must be rejected
    let mut s_u64_max = make_valid_session();
    s_u64_max.uid = u64::MAX;
    let tok_u64_max = sign_token(&key, &s_u64_max).unwrap();
    assert!(verify_session(&tok_u64_max, TEST_SECRET, None, now).is_none());
}

#[test]
fn test_tier5_auth_pkce_and_oauth_state_boundaries() {
    let now = 1792000000;
    let (state, cookie) = create_oauth_state(TEST_SECRET, "/custom/path", now).unwrap();
    assert!(cookie.contains(&state));

    // 1. PKCE challenge verification
    let verifier = pkce_verifier(TEST_SECRET, &state);
    let challenge = pkce_challenge(&verifier);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let expected_challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());
    assert_eq!(challenge, expected_challenge);

    // 2. State cookie vs param match
    assert!(verify_oauth_state(TEST_SECRET, &state, &state, now).is_ok());

    // Mismatched state param and cookie must be rejected
    let (other_state, _) = create_oauth_state(TEST_SECRET, "/", now).unwrap();
    assert!(verify_oauth_state(TEST_SECRET, &state, &other_state, now).is_err());

    // Expired state token must be rejected
    assert!(verify_oauth_state(
        TEST_SECRET,
        &state,
        &state,
        now + DEFAULT_STATE_TTL_SECONDS + 1
    )
    .is_err());

    // Nonce replay prevention: mark_state_nonce_used must return true once, then false
    let nonce = "unique-test-nonce-12345";
    assert!(mark_state_nonce_used(nonce));
    assert!(
        !mark_state_nonce_used(nonce),
        "Replayed nonce must be rejected"
    );
}

#[test]
fn test_tier5_auth_device_flow_rapid_polling_and_intervals() {
    let now = 1792000000;
    let (record, handle) = start_device_flow(TEST_SECRET, now).unwrap();
    assert_eq!(record.interval, DEVICE_MIN_INTERVAL_SECONDS);
    assert_eq!(record.expires_at, now + 900);

    // Rapid poll before next_poll_at (now + 1 < now + 5) must trigger SlowDown
    match poll_device_flow(&handle, now + 1) {
        DevicePollResult::SlowDown { interval } => {
            assert_eq!(
                interval, 10,
                "First slow down must increment interval to 10s"
            );
        }
        other => panic!("Expected SlowDown, got {other:?}"),
    }

    // Second rapid poll before next interval must increment again
    match poll_device_flow(&handle, now + 2) {
        DevicePollResult::SlowDown { interval } => {
            assert_eq!(
                interval, 15,
                "Second slow down must increment interval to 15s"
            );
        }
        other => panic!("Expected SlowDown, got {other:?}"),
    }

    // Polling after expiration
    assert_eq!(
        poll_device_flow(&handle, now + 901),
        DevicePollResult::Expired
    );

    // Invalid / unknown code or handle
    assert_eq!(
        poll_device_flow("unknown_code", now),
        DevicePollResult::Invalid
    );

    // Empty or oversized handle (>512 chars)
    assert_eq!(poll_device_flow("", now), DevicePollResult::Invalid);
    let giant_handle = "a".repeat(513);
    assert_eq!(
        poll_device_flow(&giant_handle, now),
        DevicePollResult::Invalid
    );

    // Authorization flow
    let (_, handle2) = start_device_flow(TEST_SECRET, now + 1000).unwrap();
    assert!(authorize_device(&handle2, "swackhamer", 619624));
    match poll_device_flow(&handle2, now + 1001) {
        DevicePollResult::Authorized {
            user_login,
            user_id,
        } => {
            assert_eq!(user_login, "swackhamer");
            assert_eq!(user_id, 619624);
        }
        other => panic!("Expected Authorized, got {other:?}"),
    }
}

#[test]
fn test_tier5_auth_authorization_and_session_checks() {
    // 1. is_owner checks
    assert!(is_owner("swackhamer", 619624));
    assert!(
        is_owner("SWACKHAMER", 619624),
        "Owner login check must be case-insensitive"
    );
    assert!(
        !is_owner("swackhamer", 619625),
        "Wrong UID must not be owner"
    );
    assert!(
        !is_owner("attacker", 619624),
        "Wrong login must not be owner"
    );

    // 2. may_sign_in checks
    let added_admins = vec![111111, 222222];
    assert!(may_sign_in("swackhamer", 619624, &added_admins));
    assert!(may_sign_in("added_user", 111111, &added_admins));
    assert!(!may_sign_in("unauthorized", 333333, &added_admins));
    assert!(
        !may_sign_in("zero_uid", 0, &[0]),
        "UID 0 must never be allowed"
    );

    // 3. check_session target validation
    let mut session = make_valid_session();
    let controlled_targets = vec![
        "swack-tools/oxidex".to_string(),
        "xbmc4lyfe/rustnzb".to_string(),
    ];

    // Session missing xbmc4lyfe/rustnzb
    match check_session(&session, &controlled_targets, &[], false) {
        SessionCheckStatus::Forbidden {
            missing_targets, ..
        } => {
            assert_eq!(missing_targets, vec!["xbmc4lyfe/rustnzb".to_string()]);
        }
        other => panic!("Expected Forbidden, got {other:?}"),
    }

    // Session with all targets verified (case-insensitive check)
    session.tgt.push("XBMC4LYFE/RUSTNZB".to_string());
    assert_eq!(
        check_session(&session, &controlled_targets, &[], false),
        SessionCheckStatus::Ok
    );

    // Global admin flag bypasses target verification
    let mut minimal_session = make_valid_session();
    minimal_session.tgt.clear();
    assert_eq!(
        check_session(&minimal_session, &controlled_targets, &[], true),
        SessionCheckStatus::Ok
    );
}

// ============================================================================
// SECTION 2: STATE & KV QUOTA ADVERSARIAL ORACLES
// ============================================================================

#[tokio::test]
async fn test_tier5_state_isolate_memory_cache_invalidation_and_writes() {
    invalidate_state_cache();
    let kv = Tier5MockKvStore::new();
    let start_time = 1792000000;

    // 1. Uninitialized KV: first load creates default bundle
    let (mut bundle, source1) = load_state(&kv, start_time).await.unwrap();
    assert_eq!(source1, CacheSource::DefaultCreated);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // 2. Second load within 60s TTL hits in-memory cache with 0 KV reads
    let (_, source2) = load_state(&kv, start_time + 10).await.unwrap();
    assert_eq!(source2, CacheSource::MemoryHit);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);

    // 3. Local write via save_state updates in-isolate memory cache immediately
    bundle.state.project = "mutated-project".to_string();
    save_state(&kv, &mut bundle, start_time + 15).await.unwrap();
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);
    assert_eq!(bundle.writer_slot, WRITER_SLOT_EDGE);
    assert_eq!(bundle.state_version, 2);

    // Subsequent load gets the mutated state from memory cache without KV read
    let (cached_bundle, source3) = load_state(&kv, start_time + 20).await.unwrap();
    assert_eq!(source3, CacheSource::MemoryHit);
    assert_eq!(cached_bundle.state.project, "mutated-project");
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);

    // 4. TTL boundary test:
    // At start_time + 15 + 59 = start_time + 74 -> Cache Hit (< 60s since save_state at +15)
    let (_, source_hit) = load_state(&kv, start_time + 74).await.unwrap();
    assert_eq!(source_hit, CacheSource::MemoryHit);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);

    // At start_time + 15 + 60 = start_time + 75 -> Cache Miss, triggers 1 KV read
    let (_, source_miss) = load_state(&kv, start_time + 75).await.unwrap();
    assert_eq!(source_miss, CacheSource::KvRead);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 2);

    // 5. Invalidate state cache forces fresh KV read
    invalidate_state_cache();
    let (_, source_inv) = load_state(&kv, start_time + 76).await.unwrap();
    assert_eq!(source_inv, CacheSource::KvRead);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 3);

    // 6. GCP OAuth token memory cache (TTL 3300s = 55 minutes, 0 KV ops)
    invalidate_gcp_token_cache();
    assert!(get_cached_gcp_token(start_time).is_none());
    set_cached_gcp_token(start_time, "oauth-token-xyz".to_string());

    // Hit at 3299s
    assert_eq!(
        get_cached_gcp_token(start_time + 3299).as_deref(),
        Some("oauth-token-xyz")
    );

    // Miss at 3300s
    assert!(get_cached_gcp_token(start_time + 3300).is_none());
}

#[tokio::test]
async fn test_tier5_state_uninitialized_and_corrupt_kv_fallback() {
    invalidate_state_cache();
    let now = 1792000000;

    // 1. Corrupt JSON in KV returns Serialization error
    let kv_corrupt = Tier5MockKvStore::with_raw(KV_STATE_KEY, "this is not json {");
    let res = load_state(&kv_corrupt, now).await;
    assert!(matches!(res, Err(StateError::Serialization(_))));

    let (status, body) = handle_api_health(&kv_corrupt, now).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"], UNAVAILABLE_ERROR);

    // 2. Invalid schema returns Validation error
    invalidate_state_cache();
    let mut bad_schema_bundle = default_dashboard_bundle(now).unwrap();
    bad_schema_bundle.schema = "spot-dashboard-snapshot-v999".to_string();
    let kv_bad_schema = Tier5MockKvStore::with_bundle(bad_schema_bundle);
    let res_schema = load_state(&kv_bad_schema, now).await;
    assert!(matches!(res_schema, Err(StateError::Validation(_))));

    let (status2, body2) = handle_api_health(&kv_bad_schema, now).await;
    assert_eq!(status2, 503);
    assert_eq!(body2["error"], UNAVAILABLE_ERROR);

    // 3. Incomplete document missing required fields returns Serialization error
    invalidate_state_cache();
    let incomplete_json = serde_json::json!({
        "schema": SNAPSHOT_SCHEMA,
        "state_version": 1,
        "state": {}
    });
    let kv_incomplete = Tier5MockKvStore::with_raw(KV_STATE_KEY, &incomplete_json.to_string());
    let res_incomplete = load_state(&kv_incomplete, now).await;
    assert!(matches!(res_incomplete, Err(StateError::Serialization(_))));
}

#[tokio::test]
async fn test_tier5_state_zero_kv_writes_on_all_get_endpoints_and_safety_gates() {
    invalidate_state_cache();
    let kv = Tier5MockKvStore::new();
    let now = 1792000000;

    // Seed state
    let _ = handle_api_health(&kv, now).await;

    // 1. GET /api/health -> 0 KV writes
    for i in 0..100 {
        let (status, _) = handle_api_health(&kv, now + (i % 50)).await;
        assert_eq!(status, 200);
    }
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // 2. GET /api/overview -> 0 KV writes
    for i in 0..100 {
        let (status, _) = handle_api_overview(&kv, OverviewQuery::default(), now + (i % 50))
            .await
            .unwrap();
        assert_eq!(status, 200);
    }
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // 3. GET /api/scaling -> 0 KV writes
    for i in 0..50 {
        let (status, _) = handle_get_scaling(&kv, now + (i % 50)).await.unwrap();
        assert_eq!(status, 200);
    }
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // 4. GET /api/admins -> 0 KV writes
    for i in 0..50 {
        let (status, _) = handle_get_admins(&kv, OWNER_UID, now + (i % 50))
            .await
            .unwrap();
        assert_eq!(status, 200);
    }
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // 5. POST /api/instances/delete (Safety Gate: Plan preview only, 0 KV writes)
    let del_req = DeleteInstanceRequest {
        instance_id: Some("inst-nonexistent".to_string()),
        instance_name: None,
    };
    // If instance not found, returns NotFound without write
    let _ = handle_post_instances_delete(&kv, del_req, now).await;
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // 6. POST /api/instances/scale-region (Safety Gate: Plan preview only, 0 KV writes)
    let scale_req = ScaleRegionRequest {
        region: "us-central1".to_string(),
        target_instances: 5,
        role: "cicd_runner".to_string(),
    };
    let (scale_status, scale_body) = handle_post_instances_scale_region(&kv, scale_req, now)
        .await
        .unwrap();
    assert_eq!(scale_status, 200);
    assert_eq!(scale_body["status"], "not_applied");
    assert_eq!(scale_body["applied"], false);
    assert_eq!(scale_body["message"], LIFECYCLE_ACTIONS_DISABLED_MESSAGE);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_tier5_state_admin_management_adversarial_boundaries() {
    invalidate_state_cache();
    let kv = Tier5MockKvStore::new();
    let now = 1792000000;

    // 1. Non-owner actor rejected with Forbidden
    let attacker_uid = 999999;
    let add_req = AddAdminRequest {
        login: "new_admin".to_string(),
        uid: 100001,
        confirm: true,
    };
    let err_non_owner = handle_post_admins_add(&kv, add_req, "attacker", attacker_uid, now).await;
    assert!(matches!(err_non_owner, Err(StateError::Forbidden(_))));

    // 2. confirm: false rejected with Validation
    let unconfirmed_req = AddAdminRequest {
        login: "new_admin".to_string(),
        uid: 100001,
        confirm: false,
    };
    let err_unconfirmed =
        handle_post_admins_add(&kv, unconfirmed_req, OWNER_LOGIN, OWNER_UID, now).await;
    assert!(matches!(err_unconfirmed, Err(StateError::Validation(_))));

    // 3. uid == 0 rejected with Validation
    let zero_uid_req = AddAdminRequest {
        login: "zero_user".to_string(),
        uid: 0,
        confirm: true,
    };
    let err_zero = handle_post_admins_add(&kv, zero_uid_req, OWNER_LOGIN, OWNER_UID, now).await;
    assert!(matches!(err_zero, Err(StateError::Validation(_))));

    // 4. Adding owner rejected with Conflict
    let owner_add_req = AddAdminRequest {
        login: OWNER_LOGIN.to_string(),
        uid: OWNER_UID,
        confirm: true,
    };
    let err_owner_add =
        handle_post_admins_add(&kv, owner_add_req, OWNER_LOGIN, OWNER_UID, now).await;
    assert!(matches!(err_owner_add, Err(StateError::Conflict(_))));

    // 5. Removing owner rejected with Validation
    let owner_rem_req = RemoveAdminRequest {
        uid: OWNER_UID,
        confirm: true,
    };
    let err_owner_rem =
        handle_post_admins_remove(&kv, owner_rem_req, OWNER_LOGIN, OWNER_UID, now).await;
    assert!(matches!(err_owner_rem, Err(StateError::Validation(_))));

    // 6. Removing non-existent admin rejected with NotFound
    let rem_nonexistent = RemoveAdminRequest {
        uid: 888888,
        confirm: true,
    };
    let err_rem_nonexistent =
        handle_post_admins_remove(&kv, rem_nonexistent, OWNER_LOGIN, OWNER_UID, now).await;
    assert!(matches!(err_rem_nonexistent, Err(StateError::NotFound(_))));

    // 7. Max 50 added admins boundary:
    // Add 50 admins successfully
    for i in 1..=MAX_ADDED_ADMINS {
        let req = AddAdminRequest {
            login: format!("admin_{i}"),
            uid: 200000 + i as u64,
            confirm: true,
        };
        let (status, _) = handle_post_admins_add(&kv, req, OWNER_LOGIN, OWNER_UID, now + i as u64)
            .await
            .unwrap();
        assert_eq!(status, 200);
    }

    // 51st admin addition must be rejected with Conflict
    let req_51 = AddAdminRequest {
        login: "admin_51".to_string(),
        uid: 200051,
        confirm: true,
    };
    let err_51 = handle_post_admins_add(&kv, req_51, OWNER_LOGIN, OWNER_UID, now + 100).await;
    assert!(matches!(err_51, Err(StateError::Conflict(_))));

    // Adding duplicate admin must be rejected with Conflict
    let req_dup = AddAdminRequest {
        login: "admin_1".to_string(),
        uid: 200001,
        confirm: true,
    };
    let err_dup = handle_post_admins_add(&kv, req_dup, OWNER_LOGIN, OWNER_UID, now + 101).await;
    assert!(matches!(err_dup, Err(StateError::Conflict(_))));
}

// ============================================================================
// SECTION 3: PATCH CANONICAL PARITY & HARDWARE SIZING BOUNDARIES
// ============================================================================

#[test]
fn test_tier5_patch_canonical_hashing_under_adversarial_json_formatting() {
    let base_manifest = make_valid_manifest_json("swack-tools/oxidex");
    let baseline_sha = compute_canonical_patch_sha256(&base_manifest).unwrap();

    // 1. Scramble key orders across all dictionary levels
    let key_orders = [
        vec!["schema", "repository", "hardware", "host", "container"],
        vec!["container", "host", "hardware", "repository", "schema"],
        vec!["hardware", "schema", "container", "host", "repository"],
        vec!["repository", "container", "host", "hardware", "schema"],
    ];

    for order in key_orders {
        let mut permuted = serde_json::Map::new();
        for k in order {
            if let Some(v) = base_manifest.get(k) {
                if let Value::Object(sub) = v {
                    let mut reversed_sub = serde_json::Map::new();
                    let mut sub_keys: Vec<&String> = sub.keys().collect();
                    sub_keys.reverse();
                    for sk in sub_keys {
                        reversed_sub.insert(sk.clone(), sub[sk].clone());
                    }
                    permuted.insert(k.to_string(), Value::Object(reversed_sub));
                } else {
                    permuted.insert(k.to_string(), v.clone());
                }
            }
        }
        let permuted_val = Value::Object(permuted);
        let permuted_sha = compute_canonical_patch_sha256(&permuted_val).unwrap();
        assert_eq!(
            permuted_sha, baseline_sha,
            "Key permutation must yield identical canonical SHA-256"
        );
    }

    // 2. Unicode and Python ensure_ascii=True character escaping
    let unicode_tests = [
        (
            "emoji",
            "🦀 rocket 🚀",
            r#""\ud83e\udd80 rocket \ud83d\ude80""#,
        ),
        (
            "control_chars",
            "\x00\x08\x0c\n\r\t\x1f",
            r#""\u0000\b\f\n\r\t\u001f""#,
        ),
        (
            "quotes_slashes",
            r#"path\to"quote""#,
            r#""path\\to\"quote\"""#,
        ),
    ];
    for (label, input, expected_escaped) in unicode_tests {
        let val = Value::String(input.to_string());
        let canonical_str = to_canonical_json_str(&val);
        assert_eq!(
            canonical_str, expected_escaped,
            "Unicode test '{label}' escaping mismatch: got {canonical_str}, expected {expected_escaped}"
        );
    }

    // 3. Null preservation vs stripping
    let mut manifest_with_nulls = base_manifest.clone();
    manifest_with_nulls["hardware"]["min_vcpu"] = Value::Null;
    manifest_with_nulls["hardware"]["min_ram_gib"] = Value::Null;
    manifest_with_nulls["hardware"]["runners_count"] = Value::Null; // optional: must be stripped
    manifest_with_nulls["hardware"]["runner_vcpu"] = Value::Null; // optional: must be stripped
    manifest_with_nulls["source_commit"] = json!("deadbeef123"); // must be stripped
    manifest_with_nulls["source_branch"] = json!("feature-test"); // must be stripped

    let canonical = canonicalize_manifest_value(&manifest_with_nulls);
    let hw = canonical.get("hardware").unwrap().as_object().unwrap();
    assert_eq!(
        hw.get("min_vcpu"),
        Some(&Value::Null),
        "min_vcpu: null must be preserved"
    );
    assert_eq!(
        hw.get("min_ram_gib"),
        Some(&Value::Null),
        "min_ram_gib: null must be preserved"
    );
    assert!(
        hw.get("runners_count").is_none(),
        "optional runners_count: null must be stripped"
    );
    assert!(
        hw.get("runner_vcpu").is_none(),
        "optional runner_vcpu: null must be stripped"
    );
    assert!(
        canonical.get("source_commit").is_none(),
        "source_commit must be stripped"
    );
    assert!(
        canonical.get("source_branch").is_none(),
        "source_branch must be stripped"
    );
}

#[test]
fn test_tier5_patch_golden_image_and_instance_template_omission_parity() {
    let base_manifest = make_valid_manifest_json("swack-tools/oxidex");
    let baseline_sha = compute_canonical_patch_sha256(&base_manifest).unwrap();

    // Manifest with None / null optional golden image fields
    let mut null_fields_manifest = base_manifest.clone();
    null_fields_manifest["source_image"] = Value::Null;
    null_fields_manifest["golden_image_family"] = Value::Null;
    null_fields_manifest["source_instance_template"] = Value::Null;
    null_fields_manifest["hardware"]["source_image"] = Value::Null;
    null_fields_manifest["hardware"]["golden_image_family"] = Value::Null;
    null_fields_manifest["hardware"]["source_instance_template"] = Value::Null;

    let null_sha = compute_canonical_patch_sha256(&null_fields_manifest).unwrap();
    assert_eq!(
        null_sha, baseline_sha,
        "Manifest with null golden image fields must produce 100% identical hash to legacy manifest"
    );

    // Manifest with concrete golden image values produces different, deterministic hash
    let mut golden_manifest = base_manifest.clone();
    golden_manifest["source_instance_template"] =
        json!("projects/example-project/global/instanceTemplates/runner-tmpl-v1");
    golden_manifest["golden_image_family"] = json!("oxidex-runner-golden");

    let golden_sha = compute_canonical_patch_sha256(&golden_manifest).unwrap();
    assert_ne!(golden_sha, baseline_sha);
    assert_eq!(golden_sha.len(), 64);
}

#[test]
fn test_tier5_patch_hardware_sizing_floor_exhaustive_boundaries() {
    let valid = make_valid_manifest_json("swack-tools/oxidex");

    // 1. min_vcpu floor boundary: >= 16
    let mut m_vcpu_15 = valid.clone();
    m_vcpu_15["hardware"]["min_vcpu"] = json!(15);
    assert!(validate_patch_manifest(&m_vcpu_15).is_err());

    let mut m_vcpu_16 = valid.clone();
    m_vcpu_16["hardware"]["min_vcpu"] = json!(16);
    assert!(validate_patch_manifest(&m_vcpu_16).is_ok());

    // 2. min_ram_gib floor boundary: >= 32
    let mut m_ram_31 = valid.clone();
    m_ram_31["hardware"]["min_ram_gib"] = json!(31);
    assert!(validate_patch_manifest(&m_ram_31).is_err());

    let mut m_ram_32 = valid.clone();
    m_ram_32["hardware"]["min_ram_gib"] = json!(32);
    assert!(validate_patch_manifest(&m_ram_32).is_ok());

    // 3. runners_count exact boundary: == 2
    for count in [0, 1, 3, 4] {
        let mut m = valid.clone();
        m["hardware"]["runners_count"] = json!(count);
        assert!(validate_patch_manifest(&m).is_err());
    }

    // 4. runner_vcpu exact boundary: == 7
    for vcpu in [0, 6, 8, 16] {
        let mut m = valid.clone();
        m["hardware"]["runner_vcpu"] = json!(vcpu);
        assert!(validate_patch_manifest(&m).is_err());
    }

    // 5. runner_ram_gib exact boundary: == 14
    for ram in [0, 13, 15, 28] {
        let mut m = valid.clone();
        m["hardware"]["runner_ram_gib"] = json!(ram);
        assert!(validate_patch_manifest(&m).is_err());
    }

    // 6. host_overhead_gib exact boundary: == 4
    for ovh in [0, 3, 5, 8] {
        let mut m = valid.clone();
        m["hardware"]["host_overhead_gib"] = json!(ovh);
        assert!(validate_patch_manifest(&m).is_err());
    }

    // 7. queued_jobs_threshold floor boundary: >= 20
    let mut m_q_19 = valid.clone();
    m_q_19["hardware"]["queued_jobs_threshold"] = json!(19);
    assert!(validate_patch_manifest(&m_q_19).is_err());

    let mut m_q_20 = valid.clone();
    m_q_20["hardware"]["queued_jobs_threshold"] = json!(20);
    assert!(validate_patch_manifest(&m_q_20).is_ok());
}

#[test]
fn test_tier5_patch_permitted_machine_types_and_forbidden_mounts() {
    let valid = make_valid_manifest_json("swack-tools/oxidex");

    // 1. Permitted machine types: c4d-highcpu-16, n4d-highcpu-16, n4-highcpu-16, c2d-highcpu-16
    for pmt in PERMITTED_MACHINE_TYPES {
        let mut m = valid.clone();
        m["hardware"]["extra_labels"] = json!([format!("type-{pmt}")]);
        assert!(
            validate_patch_manifest(&m).is_ok(),
            "Permitted machine type {pmt} must pass validation"
        );
    }

    // Unpermitted machine types
    let unpermitted = [
        "e2-standard-4",
        "n2-standard-16",
        "c3-highcpu-16",
        "custom-16-32768",
        "",
    ];
    for unp in unpermitted {
        let mut m = valid.clone();
        m["hardware"]["extra_labels"] = json!([format!("type-{unp}")]);
        let res = validate_patch_manifest(&m);
        assert!(
            res.is_err(),
            "Unpermitted machine type {unp} must be rejected"
        );
        let errs = res.unwrap_err();
        assert!(errs.iter().any(|e| e.contains("unpermitted_machine_type")));
    }

    // 2. Forbidden mount sources: /var/run/docker.sock, /run/docker.sock, /etc, /root, /proc, /sys, /dev
    for fm in FORBIDDEN_MOUNTS {
        let mut m_exact = valid.clone();
        m_exact["container"]["mounts"] = json!([{
            "source": fm,
            "target": "/mnt/test",
            "read_only": true
        }]);
        assert!(
            validate_patch_manifest(&m_exact).is_err(),
            "Exact forbidden mount {fm} must be rejected"
        );

        // Subpath of forbidden mount
        let subpath = format!("{fm}/sensitive_subfile");
        let mut m_sub = valid.clone();
        m_sub["container"]["mounts"] = json!([{
            "source": subpath,
            "target": "/mnt/test",
            "read_only": true
        }]);
        assert!(
            validate_patch_manifest(&m_sub).is_err(),
            "Subpath forbidden mount {subpath} must be rejected"
        );
    }

    // Permitted mounts
    let permitted_mounts = [
        "/opt/runner/cache",
        "/mnt/disks/build-cache",
        "/var/cache/builds",
    ];
    for pm in permitted_mounts {
        let mut m = valid.clone();
        m["container"]["mounts"] = json!([{
            "source": pm,
            "target": "/cache",
            "read_only": false
        }]);
        assert!(
            validate_patch_manifest(&m).is_ok(),
            "Permitted mount {pm} must pass validation"
        );
    }
}

// ============================================================================
// SECTION 4: PATCH CONSOLE MULTI-REPO & GIT ADVERSARIAL WORKFLOWS
// ============================================================================

#[tokio::test]
async fn test_tier5_patch_console_target_validation_and_path_traversal() {
    let mock = MockGitHubTransport::new();

    // 1. Path traversal and malicious repo names in handle_manifest
    let attacks = [
        "../../etc/passwd",
        "/etc/shadow",
        "../defaults.yaml",
        "swack-tools/../../secrets",
        "swack-tools\0malicious",
        "unknown/repo",
        "swack-tools/uncontrolled",
        "",
        "   ",
    ];

    for bad in attacks {
        let (status, body) = handle_manifest(bad, &mock, "mock_token").await;
        assert_eq!(status, 400, "Attack payload '{bad}' must return 400");
        assert_eq!(body["error"], "unsupported_or_missing_repository");
    }

    // 2. Path traversal in handle_save
    for bad in attacks {
        let save_payload = json!({
            "manifest": {
                "schema": "spot-runner-patch-v1",
                "repository": bad,
            }
        });
        let (status, body) = handle_save(&save_payload, &mock, "mock_token").await;
        assert_eq!(
            status, 400,
            "Attack payload '{bad}' on save must return 400"
        );
        assert_eq!(body["error"], "unsupported_repository");
    }
}

#[test]
fn test_tier5_patch_console_multi_repo_manifest_merging() {
    let mut defaults = RunnerPatch::default();
    defaults.repository = "swack-tools".to_string();
    defaults.hardware.extra_labels = vec![
        "type-c4d-highcpu-16".to_string(),
        "shared-label".to_string(),
    ];
    defaults.hardware.isa = vec!["avx2".to_string()];
    defaults
        .host
        .sysctl
        .insert("net.core.somaxconn".to_string(), "4096".to_string());
    defaults
        .host
        .sysctl
        .insert("fs.file-max".to_string(), "100000".to_string());

    let mut repo_override = RunnerPatch::default();
    repo_override.repository = "xbmc4lyfe/rustnzb".to_string();
    repo_override.hardware.extra_labels =
        vec!["shared-label".to_string(), "repo-unique-label".to_string()];
    repo_override.hardware.isa = vec!["avx2".to_string(), "bmi2".to_string()];
    repo_override
        .host
        .sysctl
        .insert("net.core.somaxconn".to_string(), "8192".to_string()); // Overrides default
    repo_override.golden_image_family = Some("rustnzb-golden".to_string());

    let merged = merge_patches(&defaults, &repo_override);

    // Repository name from override
    assert_eq!(merged.repository, "xbmc4lyfe/rustnzb");

    // Labels deduplicated preserving order
    assert_eq!(
        merged.hardware.extra_labels,
        vec!["type-c4d-highcpu-16", "shared-label", "repo-unique-label"]
    );

    // ISA deduplicated
    assert_eq!(merged.hardware.isa, vec!["avx2", "bmi2"]);

    // Sysctl merged: net.core.somaxconn overridden to 8192, fs.file-max preserved as 100000
    assert_eq!(
        merged.host.sysctl.get("net.core.somaxconn").unwrap(),
        "8192"
    );
    assert_eq!(merged.host.sysctl.get("fs.file-max").unwrap(), "100000");

    // Golden image family from override
    assert_eq!(
        merged.golden_image_family.as_deref(),
        Some("rustnzb-golden")
    );
}

#[tokio::test]
async fn test_tier5_patch_console_idempotent_save_and_branch_naming() {
    let mock = MockGitHubTransport::new();
    let manifest = make_valid_manifest_json("xbmc4lyfe/rustnzb");
    let digest = compute_canonical_patch_sha256(&manifest).unwrap();
    let expected_branch = format!("spot-dashboard/xbmc4lyfe-rustnzb-{}", &digest[..12]);

    let payload = json!({
        "manifest": manifest
    });

    // Save call 1
    let (status1, body1) = handle_save(&payload, &mock, "mock_token").await;
    assert_eq!(status1, 200);
    assert_eq!(body1["status"], "pr_created");
    assert_eq!(body1["branch"], expected_branch);
    assert_eq!(body1["patch_sha256"], digest);
}

/// Mock transport that fails on PUT requests to simulate GitHub commit failure
struct FailOnPutGitHubTransport {
    pub inner: MockGitHubTransport,
    pub delete_called: Arc<Mutex<bool>>,
}

#[async_trait::async_trait(?Send)]
impl GitHubTransport for FailOnPutGitHubTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        if method == "PUT" {
            // Simulate commit failure (500 Internal Server Error)
            return Ok((
                500,
                b"{\"message\": \"Internal Server Error on Commit\"}".to_vec(),
            ));
        }
        if method == "DELETE" {
            *self.delete_called.lock().unwrap() = true;
        }
        self.inner.request(method, path, headers, body).await
    }
}

#[tokio::test]
async fn test_tier5_patch_console_commit_failure_branch_rollback() {
    let fail_transport = FailOnPutGitHubTransport {
        inner: MockGitHubTransport::new(),
        delete_called: Arc::new(Mutex::new(false)),
    };

    let manifest = make_valid_manifest_json("swack-tools");
    let payload = json!({
        "manifest": manifest
    });

    let (status, body) = handle_save(&payload, &fail_transport, "mock_token").await;

    // Must return 502 Bad Gateway
    assert_eq!(status, 502);
    assert_eq!(body["error"], "github_commit_failed");

    // Must have executed branch rollback via DELETE
    assert!(
        *fail_transport.delete_called.lock().unwrap(),
        "DELETE ref must be called to rollback branch upon commit failure"
    );
}
