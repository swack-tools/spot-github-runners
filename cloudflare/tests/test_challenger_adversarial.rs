//! Empirical Adversarial Challenger Test Suite for Milestone 1
//! Tests path normalization, slash smuggling, OAuth state & CSRF, session validation,
//! purpose domain isolation, and boundary conditions.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::Mac;
use spot_dashboard_edge::auth::*;
use spot_dashboard_edge::*;

const TEST_SECRET: &[u8] = b"challenger-adversarial-test-secret-key-32bytes!";

fn mint_custom_session(secret: &[u8], purpose: &str, session: &SessionData) -> String {
    let key = derive_purpose_key(secret, purpose);
    sign_token(&key, session).expect("signing must succeed")
}

// ============================================================================
// TASK 1A: PATH NORMALIZATION & SLASH SMUGGLING ORACLES
// ============================================================================

#[test]
fn test_slash_smuggling_and_control_path_blocking_exhaustive() {
    let blocked_cases = vec![
        // Canonical and basic control paths
        "/api/control",
        "/api/control/",
        "/api/control/secret",
        "/api/control/sub/path",
        // Consecutive slash smuggling
        "//api/control",
        "///api///control",
        "/api//control",
        "/api//control/",
        "//api//control//",
        "//////api//////control//////",
        "//api///control/something",
        // Percent-encoded variations (single encoding)
        "//api/%63ontrol",
        "/api/%63%6f%6e%74%72%6f%6c",
        "/%61%70%69/%63%6f%6e%74%72%6f%6c",
        "//%61%70%69//%63%6f%6e%74%72%6f%6c",
        "/api/%43ontrol", // uppercase 'C'
        "/API/CONTROL",
        "//API/CONTROL",
        // Double percent-encoding (%2563 -> %63 -> c)
        "//api/%2563ontrol",
        "/api/%2563%256f%256e%2574%2572%256f%256c",
        // Dot segments and path traversal
        "/foo/../api/control",
        "/foo/bar/../../api/control",
        "/./api/./control",
        "/././api/././control",
        "/api/./control",
        "//api/./control",
        "/api/%2e/control",
        "/api/%2e%2e/api/control",
        // Suffix & delimiter smuggling
        "/api/control?anything",
        "/api/control?foo=bar&baz=1",
        "//api/control?foo=bar",
        "/api/control#fragment",
        "/api/control;param=1",
        "/api/control.json",
        "/api/control.php",
        // Encoded slash and control variations
        "/api/control%2f",
        "/api/control%2F",
        "/api%2fcontrol",
        "//api%2fcontrol",
        "/api/control%20",
        "/api/control%09",
        // Backslash mixing and smuggling
        "\\api\\control",
        "/api\\control",
        "\\api/control",
        "//\\api/\\control",
        "/\\api/\\control",
        "\\\\api\\\\control",
        "\\\\api/control",
        // Path without leading slash
        "api/control",
        "api//control",
    ];

    for path in blocked_cases {
        assert!(
            is_control_path(path),
            "Security vulnerability: control path candidate was NOT blocked: '{path}'"
        );
    }

    // Legitimate edge routes must NOT be blocked
    let allowed_cases = vec![
        "/",
        "/index.html",
        "/favicon.ico",
        "/api/health",
        "/api/overview",
        "/api/scaling",
        "/api/admins",
        "/api/admins/add",
        "/api/admins/remove",
        "/api/instances/delete",
        "/api/instances/scale-region",
        "/api/auth/status",
        "/auth/github/login",
        "/auth/github/callback",
        "/auth/device/code",
        "/auth/device/token",
        "/auth/device/poll",
        "/auth/logout",
        "/api/controller",  // legitimate hypothetical distinct route
        "/api/controlling", // legitimate hypothetical distinct route
    ];

    for path in allowed_cases {
        assert!(
            !is_control_path(path),
            "False positive: legitimate route was erroneously blocked: '{path}'"
        );
    }
}

// ============================================================================
// TASK 1B: CSRF & OAUTH STATE TOKENS ADVERSARIAL TESTING
// ============================================================================

#[test]
fn test_oauth_state_hmac_forgery_expired_and_nonce_replay() {
    let now = 1792000000u64;

    // 1. Legitimate baseline
    let (valid_state, cookie) = create_oauth_state(TEST_SECRET, "/overview", now)
        .expect("OAuth state creation should succeed");
    assert!(cookie.contains(&valid_state));

    let parsed_cookie = parse_cookies(&cookie);
    let cookie_val = parsed_cookie
        .get(OAUTH_STATE_COOKIE_NAME)
        .expect("Cookie must exist");

    let verified = verify_oauth_state(TEST_SECRET, &valid_state, cookie_val, now);
    assert!(
        verified.is_ok(),
        "Baseline verification failed: {:?}",
        verified
    );
    let payload = verified.unwrap();
    assert_eq!(payload.r, "/overview");
    assert_eq!(payload.v, 1);

    // 2. Nonce single-use replay protection
    assert!(
        mark_state_nonce_used(&payload.n),
        "First use of state nonce must succeed"
    );
    assert!(
        !mark_state_nonce_used(&payload.n),
        "Replay attack! Second use of the same state nonce MUST be rejected"
    );
    assert!(
        !mark_state_nonce_used(&payload.n),
        "Third use of the same state nonce MUST be rejected"
    );

    // 3. Forged HMAC signature attacks
    let parts: Vec<&str> = valid_state.split('.').collect();
    assert_eq!(parts.len(), 2);
    let (body_b64, valid_sig) = (parts[0], parts[1]);

    // Attack A: Signature signed with completely different secret
    let forged_key = derive_purpose_key(b"wrong-attacker-secret-key-32-bytes!!", "oauth-state");
    let fake_token = sign_token(&forged_key, &payload).unwrap();
    assert!(
        verify_oauth_state(TEST_SECRET, &fake_token, &fake_token, now).is_err(),
        "State signed with wrong secret must be rejected"
    );

    // Attack B: Signature signed with raw secret instead of derived purpose key
    let mut raw_mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(TEST_SECRET).unwrap();
    use hmac::Mac;
    raw_mac.update(body_b64.as_bytes());
    let raw_sig = hex::encode(raw_mac.finalize().into_bytes());
    let raw_token = format!("{body_b64}.{raw_sig}");
    assert!(
        verify_oauth_state(TEST_SECRET, &raw_token, &raw_token, now).is_err(),
        "State signed with raw secret must be rejected"
    );

    // Attack C: Signature signed with WRONG purpose key (e.g. session or pkce)
    let session_key = derive_purpose_key(TEST_SECRET, "session");
    let cross_purpose_token = sign_token(&session_key, &payload).unwrap();
    assert!(
        verify_oauth_state(TEST_SECRET, &cross_purpose_token, &cross_purpose_token, now).is_err(),
        "State signed with cross-purpose key must be rejected"
    );

    let pkce_key = derive_purpose_key(TEST_SECRET, "pkce");
    let cross_pkce_token = sign_token(&pkce_key, &payload).unwrap();
    assert!(
        verify_oauth_state(TEST_SECRET, &cross_pkce_token, &cross_pkce_token, now).is_err(),
        "State signed with PKCE key must be rejected"
    );

    // Attack D: Tampered payload body with original signature
    let mut tampered_payload = payload.clone();
    tampered_payload.r = "/evil-redirect".to_string();
    let tampered_json = to_compact_sorted_json(&tampered_payload).unwrap();
    let tampered_body_b64 = URL_SAFE_NO_PAD.encode(tampered_json.as_bytes());
    let tampered_token = format!("{tampered_body_b64}.{valid_sig}");
    assert!(
        verify_oauth_state(TEST_SECRET, &tampered_token, &tampered_token, now).is_err(),
        "Tampered payload body with original signature must be rejected"
    );

    // Attack E: Cookie vs Parameter mismatch (CSRF attack)
    let (state2, _) = create_oauth_state(TEST_SECRET, "/scaling", now).unwrap();
    assert!(
        verify_oauth_state(TEST_SECRET, &valid_state, &state2, now).is_err(),
        "Mismatch between query param state and cookie state MUST be rejected"
    );

    // 4. Expired and boundary timestamps
    let state_exp = payload.exp;
    // Exactly at expiration
    assert!(
        verify_oauth_state(TEST_SECRET, &valid_state, cookie_val, state_exp).is_err(),
        "State at exactly expiration timestamp must be rejected"
    );
    // After expiration
    assert!(
        verify_oauth_state(TEST_SECRET, &valid_state, cookie_val, state_exp + 1).is_err(),
        "State past expiration timestamp must be rejected"
    );
    // Way past expiration
    assert!(
        verify_oauth_state(TEST_SECRET, &valid_state, cookie_val, state_exp + 3600).is_err(),
        "Expired state must be rejected"
    );

    // State issued in future beyond clock skew
    let future_state_payload = OAuthStatePayload {
        v: 1,
        n: "nonce-future".to_string(),
        iat: now + 120, // 2 minutes in future (> 60s clock skew)
        exp: now + 720,
        r: "/".to_string(),
        tc: None,
    };
    let state_key = derive_purpose_key(TEST_SECRET, "oauth-state");
    let future_token = sign_token(&state_key, &future_state_payload).unwrap();
    assert!(
        verify_oauth_state(TEST_SECRET, &future_token, &future_token, now).is_err(),
        "State issued too far in the future must be rejected"
    );
}

// ============================================================================
// TASK 1C: SESSION TOKEN VALIDATION ADVERSARIAL TESTING
// ============================================================================

#[test]
fn test_session_token_malformed_base64_bad_signatures_and_missing_purpose() {
    let now = 1792000000u64;
    let base_session = SessionData {
        v: 1,
        sub: "swackhamer".to_string(),
        uid: 619624,
        iat: now,
        exp: now + 43200,
        tgt: vec!["swack-tools/oxidex".to_string()],
    };

    let session_key = derive_purpose_key(TEST_SECRET, "session");
    let valid_token = sign_token(&session_key, &base_session).expect("valid token");
    assert!(verify_session(&valid_token, TEST_SECRET, None, now).is_some());

    let parts: Vec<&str> = valid_token.split('.').collect();
    let (body_b64, valid_sig) = (parts[0], parts[1]);

    // 1. Malformed Base64 Bodies
    let malformed_bodies: Vec<String> = vec![
        "".to_string(),                             // empty body
        "!!!not_valid_base64!!!".to_string(),       // invalid chars
        "abc===".to_string(),                       // invalid padding length
        "abcde=".to_string(), // standard base64 with '=' padding (URL_SAFE_NO_PAD required)
        "abcde==".to_string(), // double padding
        "a+b/c==".to_string(), // standard base64 alphabet ('+' and '/')
        "hello%20world".to_string(), // url-encoded instead of b64
        "{\"v\":1}".to_string(), // raw json not b64
        body_b64[..body_b64.len() - 1].to_string(), // truncated base64
        format!("{body_b64}@@"), // corrupted b64
    ];

    for bad_body in &malformed_bodies {
        let token = format!("{bad_body}.{valid_sig}");
        assert!(
            verify_session(&token, TEST_SECRET, None, now).is_none(),
            "Malformed base64 body was NOT rejected: '{token}'"
        );
    }

    // 2. Decodable Base64 but Malformed JSON / Schema
    let non_json_bodies = vec![
        URL_SAFE_NO_PAD.encode(b"not json at all"),
        URL_SAFE_NO_PAD.encode(b"12345"),             // json number
        URL_SAFE_NO_PAD.encode(b"\"just a string\""), // json string
        URL_SAFE_NO_PAD.encode(b"[1, 2, 3]"),         // json array
        URL_SAFE_NO_PAD.encode(b"true"),              // json bool
        URL_SAFE_NO_PAD.encode(b"{\"sub\": \"admin\"}"), // missing required fields
        URL_SAFE_NO_PAD.encode(b"\x00\x01\x02\x03"),  // binary bytes
    ];

    for bad_body in non_json_bodies {
        // Sign it with valid key to isolate body schema failure
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&session_key).unwrap();
        use hmac::Mac;
        mac.update(bad_body.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        let token = format!("{bad_body}.{sig}");
        assert!(
            verify_session(&token, TEST_SECRET, None, now).is_none(),
            "Non-JSON or invalid schema body was NOT rejected: '{token}'"
        );
    }

    // 3. Incorrect / Tampered Signatures
    let bad_signatures: Vec<String> = vec![
        "".to_string(),    // empty sig
        "abc".to_string(), // too short
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde".to_string(), // 63 chars
        format!("{valid_sig}0"), // 65 chars
        "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".to_string(), // non-hex chars
        valid_sig.to_uppercase(), // uppercase hex (spec requires lowercase)
        "0000000000000000000000000000000000000000000000000000000000000000".to_string(), // null signature
    ];

    for bad_sig in &bad_signatures {
        let token = format!("{body_b64}.{bad_sig}");
        assert!(
            verify_session(&token, TEST_SECRET, None, now).is_none(),
            "Bad signature was NOT rejected: '{token}'"
        );
    }

    // 4. Missing / Cross-Purpose Domain Isolation
    // Tokens signed with wrong purpose label MUST NEVER validate as sessions
    let other_purposes = vec![
        "oauth-state",
        "pkce",
        "device-flow",
        "session-v2",
        "session-v1",
        "admin",
        "state",
        "dashboard",
        "",
    ];

    for wrong_purpose in other_purposes {
        let wrong_key = derive_purpose_key(TEST_SECRET, wrong_purpose);
        let cross_token = sign_token(&wrong_key, &base_session).unwrap();
        assert!(
            verify_session(&cross_token, TEST_SECRET, None, now).is_none(),
            "Cross-purpose token for purpose '{wrong_purpose}' MUST NOT validate as session!"
        );
    }

    // Token signed with raw purpose label "session" without "spot-dashboard/<purpose>/v1" wrapper
    let mut raw_purpose_mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(TEST_SECRET).unwrap();
    raw_purpose_mac.update(b"session");
    let raw_purpose_key: [u8; 32] = raw_purpose_mac.finalize().into_bytes().into();
    let raw_purpose_token = sign_token(&raw_purpose_key, &base_session).unwrap();
    assert!(
        verify_session(&raw_purpose_token, TEST_SECRET, None, now).is_none(),
        "Token signed with raw 'session' purpose without wrapper MUST NOT validate as session!"
    );

    // Token signed with raw secret without purpose derivation
    let mut raw_mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(TEST_SECRET).unwrap();
    raw_mac.update(body_b64.as_bytes());
    let raw_sig = hex::encode(raw_mac.finalize().into_bytes());
    let raw_token = format!("{body_b64}.{raw_sig}");
    assert!(
        verify_session(&raw_token, TEST_SECRET, None, now).is_none(),
        "Token signed with raw secret MUST NOT validate as session"
    );
}

// ============================================================================
// TASK 1D: BOUNDARY CHECKS (UID, CLOCK SKEW, USERNAME SYNTAX, NBF)
// ============================================================================

#[test]
fn test_session_adversarial_boundaries_and_identity() {
    let now = 1792000000u64;

    // A. UID 0 must be rejected (anonymous / unauthenticated)
    let uid_zero = SessionData {
        v: 1,
        sub: "swackhamer".to_string(),
        uid: 0,
        iat: now,
        exp: now + 43200,
        tgt: vec!["swack-tools/oxidex".to_string()],
    };
    let tok = mint_custom_session(TEST_SECRET, "session", &uid_zero);
    assert!(verify_session(&tok, TEST_SECRET, None, now).is_none());

    // B. Invalid GitHub usernames
    let invalid_logins = vec![
        "",                                                             // empty
        "-swackhamer",       // leading hyphen (illegal in GitHub)
        "swack_hamer",       // underscore (illegal in GitHub user logins)
        "swack..hamer",      // consecutive dots
        "swackhamer@github", // email style
        "swack/hamer",       // slash
        "a-very-long-github-username-that-exceeds-39-characters-total", // > 39 chars
    ];

    for bad_login in invalid_logins {
        let bad_user = SessionData {
            v: 1,
            sub: bad_login.to_string(),
            uid: 619624,
            iat: now,
            exp: now + 43200,
            tgt: vec!["swack-tools/oxidex".to_string()],
        };
        let tok = mint_custom_session(TEST_SECRET, "session", &bad_user);
        assert!(
            verify_session(&tok, TEST_SECRET, None, now).is_none(),
            "Invalid username '{bad_login}' was not rejected"
        );
    }

    // C. Non-numeric / malformed SESSION_NOT_BEFORE fail-closed oracle
    let valid_user = SessionData {
        v: 1,
        sub: "swackhamer".to_string(),
        uid: 619624,
        iat: now,
        exp: now + 43200,
        tgt: vec!["swack-tools/oxidex".to_string()],
    };
    let valid_tok = mint_custom_session(TEST_SECRET, "session", &valid_user);

    let malformed_nbfs = vec![
        "invalid-not-before",
        "123abc456",
        "-100",
        "1792000000.5",
        " 1792000000 ",        // leading/trailing spaces with non-digit
        "1792000000000000000", // > 15 chars
    ];

    for bad_nbf in malformed_nbfs {
        if bad_nbf.trim().chars().all(|c| c.is_ascii_digit()) && bad_nbf.trim().len() <= 15 {
            continue;
        }
        assert!(
            verify_session(&valid_tok, TEST_SECRET, Some(bad_nbf), now).is_none(),
            "Malformed SESSION_NOT_BEFORE '{bad_nbf}' must fail closed and reject all sessions"
        );
    }
}
