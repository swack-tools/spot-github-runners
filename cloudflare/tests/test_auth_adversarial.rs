use base64::Engine;
use spot_dashboard_edge::auth::*;

const TEST_SECRET: &[u8] = b"test-vector-secret-not-for-production-0123456789abcdef";

fn create_valid_session_data() -> SessionData {
    SessionData {
        v: 1,
        sub: "swackhamer".to_string(),
        uid: 619624,
        iat: 1792000000,
        exp: 1792043200,
        tgt: vec!["swack-tools/oxidex".to_string()],
    }
}

fn mint_valid_token(secret: &[u8], session: &SessionData) -> String {
    let key = derive_purpose_key(secret, "session");
    sign_token(&key, session).expect("Must mint valid token")
}

// ============================================================================
// 1. SESSION AUTHENTICATION TAMPERING, BIT FLIPS & TRUNCATION ORACLES
// ============================================================================

#[test]
fn test_session_bit_flip_oracle_rejects_every_single_bit_flip() {
    let session = create_valid_session_data();
    let valid_token = mint_valid_token(TEST_SECRET, &session);
    let now = session.iat + 100;

    // Verify baseline passes
    assert!(verify_session(&valid_token, TEST_SECRET, None, now).is_some());

    let parts: Vec<&str> = valid_token.split('.').collect();
    assert_eq!(parts.len(), 2);
    let (body_b64, sig_hex) = (parts[0], parts[1]);

    // Oracle 1: Flip every single bit in the base64url body
    let body_bytes = body_b64.as_bytes();
    for byte_idx in 0..body_bytes.len() {
        for bit_idx in 0..8 {
            let mut corrupted_bytes = body_bytes.to_vec();
            corrupted_bytes[byte_idx] ^= 1 << bit_idx;
            if let Ok(corrupted_body) = std::str::from_utf8(&corrupted_bytes) {
                // Skip if bit flip produced a dot or non-ascii
                if corrupted_body.contains('.') {
                    continue;
                }
                let tampered_token = format!("{corrupted_body}.{sig_hex}");
                let verified = verify_session(&tampered_token, TEST_SECRET, None, now);
                assert!(
                    verified.is_none(),
                    "Bit flip at body byte {byte_idx} bit {bit_idx} was NOT rejected: {tampered_token}"
                );
            }
        }
    }

    // Oracle 2: Flip every single bit in the hex signature
    let sig_bytes = sig_hex.as_bytes();
    for byte_idx in 0..sig_bytes.len() {
        for bit_idx in 0..8 {
            let mut corrupted_sig_bytes = sig_bytes.to_vec();
            corrupted_sig_bytes[byte_idx] ^= 1 << bit_idx;
            if let Ok(corrupted_sig) = std::str::from_utf8(&corrupted_sig_bytes) {
                if corrupted_sig.contains('.') {
                    continue;
                }
                let tampered_token = format!("{body_b64}.{corrupted_sig}");
                let verified = verify_session(&tampered_token, TEST_SECRET, None, now);
                assert!(
                    verified.is_none(),
                    "Bit flip at sig byte {byte_idx} bit {bit_idx} was NOT rejected: {tampered_token}"
                );
            }
        }
    }
}

#[test]
fn test_session_token_truncation_and_extension_attacks() {
    let session = create_valid_session_data();
    let valid_token = mint_valid_token(TEST_SECRET, &session);
    let now = session.iat + 100;

    // 1. Truncate token character by character down to 0
    for len in 0..valid_token.len() {
        let truncated = &valid_token[..len];
        assert!(
            verify_session(truncated, TEST_SECRET, None, now).is_none(),
            "Truncated token of length {len} must be rejected: '{truncated}'"
        );
    }

    // 2. Truncate signature only
    let parts: Vec<&str> = valid_token.split('.').collect();
    let (body, sig) = (parts[0], parts[1]);
    for sig_len in 0..sig.len() {
        let truncated_sig = &sig[..sig_len];
        let token = format!("{body}.{truncated_sig}");
        assert!(verify_session(&token, TEST_SECRET, None, now).is_none());
    }

    // 3. Extend signature by 1, 2, or 10 characters
    for extra in &["0", "a", "ff", "0123456789"] {
        let extended_sig = format!("{sig}{extra}");
        let token = format!("{body}.{extended_sig}");
        assert!(verify_session(&token, TEST_SECRET, None, now).is_none());
    }

    // 4. Dot structure attacks
    assert!(
        verify_session(&format!("{body}{sig}"), TEST_SECRET, None, now).is_none(),
        "Missing dot"
    );
    assert!(
        verify_session(&format!("{body}.{sig}."), TEST_SECRET, None, now).is_none(),
        "Trailing dot"
    );
    assert!(
        verify_session(&format!(".{body}.{sig}"), TEST_SECRET, None, now).is_none(),
        "Leading dot"
    );
    assert!(
        verify_session(&format!("{body}..{sig}"), TEST_SECRET, None, now).is_none(),
        "Double dot"
    );
    assert!(
        verify_session(&format!("{body}.{sig}.extra"), TEST_SECRET, None, now).is_none(),
        "Three parts"
    );
    assert!(verify_session(".", TEST_SECRET, None, now).is_none());
    assert!(verify_session("..", TEST_SECRET, None, now).is_none());
    assert!(verify_session("", TEST_SECRET, None, now).is_none());

    // 5. Exceed MAX_TOKEN_CHARS (4096)
    let giant_token = format!("{body}.{}{sig}", "0".repeat(4096));
    assert!(verify_session(&giant_token, TEST_SECRET, None, now).is_none());
}

#[test]
fn test_session_signature_case_sensitivity_oracle() {
    let session = create_valid_session_data();
    let valid_token = mint_valid_token(TEST_SECRET, &session);
    let now = session.iat + 100;
    let parts: Vec<&str> = valid_token.split('.').collect();
    let (body, sig) = (parts[0], parts[1]);

    // Uppercase signature must be rejected (only lowercase hex allowed)
    let uppercase_sig = sig.to_uppercase();
    assert_ne!(sig, uppercase_sig);
    let uppercase_token = format!("{body}.{uppercase_sig}");
    assert!(
        verify_session(&uppercase_token, TEST_SECRET, None, now).is_none(),
        "Uppercase hex signature must be rejected"
    );

    // Mixed-case signature must be rejected
    let mut mixed_chars: Vec<char> = sig.chars().collect();
    for i in (0..mixed_chars.len()).step_by(2) {
        mixed_chars[i] = mixed_chars[i].to_ascii_uppercase();
    }
    let mixed_sig: String = mixed_chars.into_iter().collect();
    let mixed_token = format!("{body}.{mixed_sig}");
    assert!(
        verify_session(&mixed_token, TEST_SECRET, None, now).is_none(),
        "Mixed-case hex signature must be rejected"
    );
}

#[test]
fn test_session_payload_tampering_attacks() {
    let session = create_valid_session_data();
    let valid_token = mint_valid_token(TEST_SECRET, &session);
    let now = session.iat + 100;
    let parts: Vec<&str> = valid_token.split('.').collect();
    let original_sig = parts[1];

    let session_key = derive_purpose_key(TEST_SECRET, "session");

    // Attack 1: Replace sub with attacker, keep original sig
    let mut tampered = session.clone();
    tampered.sub = "attacker".to_string();
    let tampered_payload_str = to_compact_sorted_json(&tampered).unwrap();
    let tampered_body =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tampered_payload_str.as_bytes());
    let token = format!("{tampered_body}.{original_sig}");
    assert!(verify_session(&token, TEST_SECRET, None, now).is_none());

    // Attack 2: Replace uid with different UID, keep original sig
    let mut tampered = session.clone();
    tampered.uid = 999999;
    let tampered_payload_str = to_compact_sorted_json(&tampered).unwrap();
    let tampered_body =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tampered_payload_str.as_bytes());
    let token = format!("{tampered_body}.{original_sig}");
    assert!(verify_session(&token, TEST_SECRET, None, now).is_none());

    // Attack 3: Elevate version v: 2, properly signed with key
    let mut tampered = session.clone();
    tampered.v = 2;
    let token = sign_token(&session_key, &tampered).unwrap();
    assert!(
        verify_session(&token, TEST_SECRET, None, now).is_none(),
        "v != 1 must be rejected"
    );

    // Attack 4: Missing fields in JSON payload (properly signed)
    let incomplete_payloads = vec![
        serde_json::json!({ "v": 1, "sub": "swackhamer", "uid": 619624, "iat": session.iat, "exp": session.exp }), // missing tgt
        serde_json::json!({ "v": 1, "sub": "swackhamer", "iat": session.iat, "exp": session.exp, "tgt": ["swack-tools/oxidex"] }), // missing uid
        serde_json::json!({ "v": 1, "uid": 619624, "iat": session.iat, "exp": session.exp, "tgt": ["swack-tools/oxidex"] }), // missing sub
        serde_json::json!({ "sub": "swackhamer", "uid": 619624, "iat": session.iat, "exp": session.exp, "tgt": ["swack-tools/oxidex"] }), // missing v
        serde_json::json!({ "v": 1, "sub": "swackhamer", "uid": 619624, "exp": session.exp, "tgt": ["swack-tools/oxidex"] }), // missing iat
        serde_json::json!({ "v": 1, "sub": "swackhamer", "uid": 619624, "iat": session.iat, "tgt": ["swack-tools/oxidex"] }), // missing exp
    ];
    for bad_json in incomplete_payloads {
        let token = sign_token(&session_key, &bad_json).unwrap();
        assert!(
            verify_session(&token, TEST_SECRET, None, now).is_none(),
            "Payload missing required field must be rejected: {bad_json}"
        );
    }
}

// ============================================================================
// 2. TIMING & CLOCK SKEW EDGE CASES
// ============================================================================

#[test]
fn test_timing_and_clock_skew_boundaries() {
    let session = create_valid_session_data();
    let valid_token = mint_valid_token(TEST_SECRET, &session);

    let iat = session.iat; // 1792000000
    let exp = session.exp; // 1792043200

    // 1. Exact lower skew bound: iat - 60
    assert!(
        verify_session(&valid_token, TEST_SECRET, None, iat - 60).is_some(),
        "iat - 60 must be accepted"
    );

    // 2. 1 second before lower skew bound: iat - 61
    assert!(
        verify_session(&valid_token, TEST_SECRET, None, iat - 61).is_none(),
        "iat - 61 must be rejected"
    );

    // 3. At iat exactly
    assert!(verify_session(&valid_token, TEST_SECRET, None, iat).is_some());

    // 4. Last second before expiration: exp - 1
    assert!(
        verify_session(&valid_token, TEST_SECRET, None, exp - 1).is_some(),
        "exp - 1 must be accepted"
    );

    // 5. At exact expiration: exp
    assert!(
        verify_session(&valid_token, TEST_SECRET, None, exp).is_none(),
        "now == exp must be rejected (strictly now < exp)"
    );

    // 6. After expiration: exp + 1
    assert!(
        verify_session(&valid_token, TEST_SECRET, None, exp + 1).is_none(),
        "exp + 1 must be rejected"
    );

    // 7. Max TTL boundary: exp - iat == MAX_SESSION_TTL_SECONDS (604,800s)
    let session_key = derive_purpose_key(TEST_SECRET, "session");
    let mut max_ttl_session = session.clone();
    max_ttl_session.exp = max_ttl_session.iat + MAX_SESSION_TTL_SECONDS;
    let max_ttl_token = sign_token(&session_key, &max_ttl_session).unwrap();
    assert!(
        verify_session(&max_ttl_token, TEST_SECRET, None, iat + 10).is_some(),
        "exp - iat == MAX_SESSION_TTL_SECONDS must be accepted"
    );

    // 8. Exceed Max TTL by 1 second: exp - iat == MAX_SESSION_TTL_SECONDS + 1
    let mut over_ttl_session = session.clone();
    over_ttl_session.exp = over_ttl_session.iat + MAX_SESSION_TTL_SECONDS + 1;
    let over_ttl_token = sign_token(&session_key, &over_ttl_session).unwrap();
    assert!(
        verify_session(&over_ttl_token, TEST_SECRET, None, iat + 10).is_none(),
        "exp - iat > MAX_SESSION_TTL_SECONDS must be rejected"
    );

    // 9. exp <= iat
    let mut inverted_session = session.clone();
    inverted_session.exp = inverted_session.iat; // exp == iat
    let inverted_token = sign_token(&session_key, &inverted_session).unwrap();
    assert!(verify_session(&inverted_token, TEST_SECRET, None, iat).is_none());

    inverted_session.exp = inverted_session.iat - 1; // exp < iat
    let inverted_token2 = sign_token(&session_key, &inverted_session).unwrap();
    assert!(verify_session(&inverted_token2, TEST_SECRET, None, iat).is_none());

    // 10. Low epoch values: iat = 10, no underflow on saturating_sub
    let mut low_epoch_session = session.clone();
    low_epoch_session.iat = 10;
    low_epoch_session.exp = 1000;
    let low_epoch_token = sign_token(&session_key, &low_epoch_session).unwrap();
    assert!(verify_session(&low_epoch_token, TEST_SECRET, None, 0).is_some());
    assert!(verify_session(&low_epoch_token, TEST_SECRET, None, 50).is_some());
}

// ============================================================================
// 3. COOKIE PARSER FUZZING
// ============================================================================

#[test]
fn test_cookie_parser_fuzzing_and_edge_cases() {
    // 1. Standard single cookie
    let c = parse_cookies("__Host-spot_session=tok123");
    assert_eq!(c.get("__Host-spot_session"), Some(&"tok123".to_string()));

    // 2. Leading, trailing, and inter-token spaces
    let c = parse_cookies("   __Host-spot_session=tok123   ;   other=val456   ");
    assert_eq!(c.get("__Host-spot_session"), Some(&"tok123".to_string()));
    assert_eq!(c.get("other"), Some(&"val456".to_string()));

    // 3. Whitespace around '='
    let c = parse_cookies("__Host-spot_session   =   tok123");
    assert_eq!(c.get("__Host-spot_session"), Some(&"tok123".to_string()));

    // 4. Missing '=' tokens
    let c = parse_cookies("no_equal_sign; __Host-spot_session=valid; another_no_equal");
    assert_eq!(c.get("__Host-spot_session"), Some(&"valid".to_string()));
    assert_eq!(c.len(), 1);

    // 5. Empty name or empty value
    let c = parse_cookies("=empty_name; __Host-spot_session=; valid=val");
    assert_eq!(c.get("__Host-spot_session"), Some(&"".to_string()));
    assert_eq!(c.get("valid"), Some(&"val".to_string()));
    assert!(!c.contains_key(""));

    // 6. Quoted tokens (RFC 6265 cookie-octet quoting)
    let c = parse_cookies("__Host-spot_session=\"quoted_token_val\"");
    assert_eq!(
        c.get("__Host-spot_session"),
        Some(&"quoted_token_val".to_string())
    );

    // 7. Empty quoted token
    let c = parse_cookies("__Host-spot_session=\"\"");
    assert_eq!(c.get("__Host-spot_session"), Some(&"".to_string()));

    // 8. Single quote or unmatched quotes
    let c = parse_cookies("c1=\"; c2=\"token; c3=token\"");
    assert_eq!(c.get("c1"), Some(&"\"".to_string()));
    assert_eq!(c.get("c2"), Some(&"\"token".to_string()));
    assert_eq!(c.get("c3"), Some(&"token\"".to_string()));

    // 9. Value containing '=' characters (base64 or nested queries)
    let c = parse_cookies("__Host-spot_session=a=b=c; target=org/repo");
    assert_eq!(c.get("__Host-spot_session"), Some(&"a=b=c".to_string()));
    assert_eq!(c.get("target"), Some(&"org/repo".to_string()));

    // 10. Multiple occurrences of same cookie: First Occurrence Wins
    let c = parse_cookies("__Host-spot_session=FIRST; __Host-spot_session=SECOND");
    assert_eq!(c.get("__Host-spot_session"), Some(&"FIRST".to_string()));

    // 11. Multiple semicolons and empty fragments
    let c = parse_cookies(";;;__Host-spot_session=tok;;;other=val;;;");
    assert_eq!(c.get("__Host-spot_session"), Some(&"tok".to_string()));
    assert_eq!(c.get("other"), Some(&"val".to_string()));
    assert_eq!(c.len(), 2);

    // 12. Non-ascii and special characters in cookie header
    let c = parse_cookies("__Host-spot_session=tok123; unicode=🦀; special=!@#$%^&*()");
    assert_eq!(c.get("__Host-spot_session"), Some(&"tok123".to_string()));
    assert_eq!(c.get("unicode"), Some(&"🦀".to_string()));
    assert_eq!(c.get("special"), Some(&"!@#$%^&*()".to_string()));

    // 13. Completely empty header
    let c = parse_cookies("");
    assert!(c.is_empty());

    // 14. Header with only spaces and semicolons
    let c = parse_cookies("   ;   ;  ;   ");
    assert!(c.is_empty());
}

// ============================================================================
// 4. BULK REVOCATION (SESSION_NOT_BEFORE)
// ============================================================================

#[test]
fn test_bulk_revocation_numeric_and_non_numeric_strings() {
    let session = create_valid_session_data();
    let valid_token = mint_valid_token(TEST_SECRET, &session);
    let now = session.iat + 100;
    let iat = session.iat; // 1792000000

    // 1. None: Unset bulk revocation, passes
    assert!(verify_session(&valid_token, TEST_SECRET, None, now).is_some());

    // 2. Empty string or whitespace: Unset, passes
    assert!(verify_session(&valid_token, TEST_SECRET, Some(""), now).is_some());
    assert!(verify_session(&valid_token, TEST_SECRET, Some("   "), now).is_some());

    // 3. Numeric: Exactly equal to iat -> Passes (iat >= nbf)
    assert!(verify_session(&valid_token, TEST_SECRET, Some(&iat.to_string()), now).is_some());

    // 4. Numeric: iat + 1 -> Revoked (iat < nbf)
    assert!(verify_session(&valid_token, TEST_SECRET, Some(&(iat + 1).to_string()), now).is_none());

    // 5. Numeric: iat - 1 -> Passes
    assert!(verify_session(&valid_token, TEST_SECRET, Some(&(iat - 1).to_string()), now).is_some());

    // 6. Numeric with leading/trailing spaces -> Trimmed, correctly evaluated
    assert!(verify_session(&valid_token, TEST_SECRET, Some(&format!("  {iat}  ")), now).is_some());
    assert!(verify_session(
        &valid_token,
        TEST_SECRET,
        Some(&format!("  {}  ", iat + 1)),
        now
    )
    .is_none());

    // 7. Non-numeric strings: MUST FAIL CLOSED (return None for all sessions)
    let non_numeric_strings = vec![
        "soon",
        "now",
        "yesterday",
        "revoked",
        "-1",
        "-1792000000",
        "+1792000000",
        "123.45",
        "1e9",
        "0x1000",
        "null",
        "undefined",
        "NaN",
        "1792000000 123",
        "1792000000a",
        "a1792000000",
        "1792000000\0",
        "9999999999999999", // 16 digits: exceeds max 15 digits
    ];
    for bad_nbf in non_numeric_strings {
        assert!(
            parse_session_not_before(Some(bad_nbf)).is_none(),
            "parse_session_not_before must return None (fail closed) for '{bad_nbf}'"
        );
        assert!(
            verify_session(&valid_token, TEST_SECRET, Some(bad_nbf), now).is_none(),
            "verify_session must return None (fail closed) for non-numeric SESSION_NOT_BEFORE '{bad_nbf}'"
        );
    }

    // 8. Valid 15-digit boundary (maximum allowed length)
    let max_15_digits = "999999999999999";
    assert_eq!(
        parse_session_not_before(Some(max_15_digits)),
        Some(999999999999999)
    );
}

// ============================================================================
// 5. IDENTITY VALIDATION (GITHUB USERNAME & UID FUZZING)
// ============================================================================

#[test]
fn test_github_username_identity_validation_oracle() {
    let u39 = "a".repeat(39);
    // Valid GitHub usernames (match [A-Za-z0-9]([A-Za-z0-9-]{0,38}))
    let valid_usernames = vec![
        "swackhamer",
        "a",
        "0",
        "A",
        "swack-tools",
        "user-123-abc",
        "0-admin",
        u39.as_str(), // exactly 39 chars
    ];
    for username in valid_usernames {
        assert!(
            is_valid_github_login(username),
            "Valid username was rejected: '{username}'"
        );
    }

    let u40 = "a".repeat(40);
    let u100 = "a".repeat(100);
    // Invalid GitHub usernames (must be rejected)
    let invalid_usernames = vec![
        "",             // Empty
        "-swackhamer",  // Leading hyphen
        "-",            // Single hyphen
        "swack_hamer",  // Underscore
        "swack.hamer",  // Period/dot
        "swack@hamer",  // Special char @
        "swack/hamer",  // Special char /
        "swack\\hamer", // Backslash
        "swack hamer",  // Space inside
        " swackhamer",  // Leading space
        "swackhamer ",  // Trailing space
        "swack\nhamer", // Newline
        "swack\thamer", // Tab
        "swack\0hamer", // Null byte
        "swäckhamer",   // Non-ASCII Unicode umlaut
        "swackhamer🦀", // Non-ASCII Emoji
        "日本語",       // Multi-byte CJK
        u40.as_str(),   // 40 chars: exceeds 39 limit
        u100.as_str(),  // 100 chars
    ];
    for username in invalid_usernames {
        assert!(
            !is_valid_github_login(username),
            "Invalid username was accepted: '{username}'"
        );

        // Verify session verification rejects it as well
        let session_key = derive_purpose_key(TEST_SECRET, "session");
        let mut session = create_valid_session_data();
        session.sub = username.to_string();
        if let Ok(token) = sign_token(&session_key, &session) {
            assert!(
                verify_session(&token, TEST_SECRET, None, session.iat + 10).is_none(),
                "Session with invalid username '{username}' must be rejected by verify_session"
            );
        }
    }
}

#[test]
fn test_github_uid_validation_oracle() {
    let session_key = derive_purpose_key(TEST_SECRET, "session");
    let session = create_valid_session_data();
    let now = session.iat + 10;

    // 1. uid == 0: Must be rejected
    let mut zero_uid_session = session.clone();
    zero_uid_session.uid = 0;
    let token = sign_token(&session_key, &zero_uid_session).unwrap();
    assert!(
        verify_session(&token, TEST_SECRET, None, now).is_none(),
        "uid == 0 must be rejected"
    );

    // 2. uid == (1 << 53) - 1 (9007199254740991, max safe JS integer): Accepted
    let max_safe_uid = (1u64 << 53) - 1;
    let mut max_uid_session = session.clone();
    max_uid_session.uid = max_safe_uid;
    let token = sign_token(&session_key, &max_uid_session).unwrap();
    assert!(verify_session(&token, TEST_SECRET, None, now).is_some());

    // 3. uid == (1 << 53) (9007199254740992, exceeds max safe JS integer): Must be rejected
    let mut over_safe_session = session.clone();
    over_safe_session.uid = 1u64 << 53;
    let token = sign_token(&session_key, &over_safe_session).unwrap();
    assert!(
        verify_session(&token, TEST_SECRET, None, now).is_none(),
        "uid > (1 << 53) - 1 must be rejected"
    );

    // 4. uid == u64::MAX: Must be rejected
    let mut max_u64_session = session.clone();
    max_u64_session.uid = u64::MAX;
    let token = sign_token(&session_key, &max_u64_session).unwrap();
    assert!(
        verify_session(&token, TEST_SECRET, None, now).is_none(),
        "uid == u64::MAX must be rejected"
    );

    // 5. Negative and non-integer UIDs in signed JSON payloads
    let invalid_uid_payloads = vec![
        serde_json::json!({ "v": 1, "sub": "swackhamer", "uid": -1, "iat": session.iat, "exp": session.exp, "tgt": ["swack-tools/oxidex"] }),
        serde_json::json!({ "v": 1, "sub": "swackhamer", "uid": -619624, "iat": session.iat, "exp": session.exp, "tgt": ["swack-tools/oxidex"] }),
        serde_json::json!({ "v": 1, "sub": "swackhamer", "uid": 123.45, "iat": session.iat, "exp": session.exp, "tgt": ["swack-tools/oxidex"] }),
        serde_json::json!({ "v": 1, "sub": "swackhamer", "uid": "619624", "iat": session.iat, "exp": session.exp, "tgt": ["swack-tools/oxidex"] }),
        serde_json::json!({ "v": 1, "sub": "swackhamer", "uid": true, "iat": session.iat, "exp": session.exp, "tgt": ["swack-tools/oxidex"] }),
        serde_json::json!({ "v": 1, "sub": "swackhamer", "uid": serde_json::Value::Null, "iat": session.iat, "exp": session.exp, "tgt": ["swack-tools/oxidex"] }),
    ];
    for bad_payload in invalid_uid_payloads {
        let token = sign_token(&session_key, &bad_payload).unwrap();
        assert!(
            verify_session(&token, TEST_SECRET, None, now).is_none(),
            "Payload with non-u64 UID must be rejected: {bad_payload}"
        );
    }
}

// ============================================================================
// 6. CRYPTOGRAPHIC DOMAIN SEPARATION & PURPOSE KEY ISOLATION
// ============================================================================

#[test]
fn test_purpose_key_cryptographic_domain_isolation() {
    let session = create_valid_session_data();

    // Derive keys for different purposes
    let session_key = derive_purpose_key(TEST_SECRET, "session");
    let control_key = derive_purpose_key(TEST_SECRET, "control");
    let pkce_key = derive_purpose_key(TEST_SECRET, "pkce");
    let state_key = derive_purpose_key(TEST_SECRET, "oauth-state");
    let raw_purpose_key = derive_purpose_key(TEST_SECRET, "spot-dashboard/session/v1");

    // All purpose keys must be mutually distinct
    let keys = vec![
        session_key,
        control_key,
        pkce_key,
        state_key,
        raw_purpose_key,
    ];
    for i in 0..keys.len() {
        for j in (i + 1)..keys.len() {
            assert_ne!(keys[i], keys[j], "Purpose keys must be strictly orthogonal");
        }
    }

    // A token signed with "control" key must be rejected by verify_session
    let control_token = sign_token(&control_key, &session).unwrap();
    assert!(verify_session(&control_token, TEST_SECRET, None, session.iat + 10).is_none());

    // A token signed with "oauth-state" key must be rejected by verify_session
    let state_token = sign_token(&state_key, &session).unwrap();
    assert!(verify_session(&state_token, TEST_SECRET, None, session.iat + 10).is_none());

    // A token signed with a different secret must be rejected
    let diff_secret = b"completely-different-session-secret-abcdef0123456789";
    let diff_token = mint_valid_token(diff_secret, &session);
    assert!(verify_session(&diff_token, TEST_SECRET, None, session.iat + 10).is_none());
}

// ============================================================================
// 7. ADMIN AUTHORIZATION & CONTROLLED TARGET CHECKS
// ============================================================================

#[test]
fn test_check_session_owner_added_admins_and_target_verification() {
    let session = create_valid_session_data(); // sub: swackhamer, uid: 619624, tgt: ["swack-tools/oxidex"]
    let controlled_targets = vec!["swack-tools/oxidex".to_string()];

    // 1. Owner with exact target -> Ok
    let status = check_session(&session, &controlled_targets, &[], false);
    assert_eq!(status, SessionCheckStatus::Ok);

    // 2. Owner case-insensitivity on login
    assert!(is_owner("SWACKHAMER", 619624));
    assert!(is_owner("SwackHamer", 619624));

    // 3. Unauthorized login with owner UID -> Forbidden
    let mut spoof_login = session.clone();
    spoof_login.sub = "attacker".to_string();
    let status = check_session(&spoof_login, &controlled_targets, &[], false);
    assert!(matches!(status, SessionCheckStatus::Forbidden { .. }));

    // 4. Owner login with wrong UID -> Forbidden
    let mut spoof_uid = session.clone();
    spoof_uid.uid = 999999;
    let status = check_session(&spoof_uid, &controlled_targets, &[], false);
    assert!(matches!(status, SessionCheckStatus::Forbidden { .. }));

    // 5. Added admin present in added_admins -> Ok
    let mut alice_session = session.clone();
    alice_session.sub = "alice".to_string();
    alice_session.uid = 10001;
    let status = check_session(&alice_session, &controlled_targets, &[10001], false);
    assert_eq!(status, SessionCheckStatus::Ok);

    // 6. Added admin NOT present in added_admins (e.g. revoked) -> Forbidden
    let status = check_session(&alice_session, &controlled_targets, &[20002], false);
    assert!(matches!(status, SessionCheckStatus::Forbidden { .. }));

    // 7. Missing controlled targets -> Forbidden with missing_targets listed
    let multi_targets = vec![
        "swack-tools/oxidex".to_string(),
        "swack-tools/extra-repo".to_string(),
    ];
    let status = check_session(&session, &multi_targets, &[], false);
    match status {
        SessionCheckStatus::Forbidden {
            missing_targets, ..
        } => {
            assert_eq!(missing_targets, vec!["swack-tools/extra-repo".to_string()]);
        }
        _ => panic!("Expected Forbidden with missing targets"),
    }

    // 8. Controlled targets case-insensitivity
    let upper_targets = vec!["SWACK-TOOLS/OXIDEX".to_string()];
    let status = check_session(&session, &upper_targets, &[], false);
    assert_eq!(status, SessionCheckStatus::Ok);

    // 9. Global admin exemption: misses targets but passes because global_admins is true
    let status = check_session(&session, &multi_targets, &[], true);
    assert_eq!(status, SessionCheckStatus::Ok);
}

// ============================================================================
// 8. SAFE RETURN PATH VALIDATION
// ============================================================================

#[test]
fn test_safe_return_path_open_redirect_defenses() {
    // Valid same-origin absolute paths
    assert_eq!(safe_return_path("/"), "/");
    assert_eq!(safe_return_path("/dashboard"), "/dashboard");
    assert_eq!(
        safe_return_path("/api/overview?slot=a"),
        "/api/overview?slot=a"
    );
    assert_eq!(
        safe_return_path("/deep/nested/path/123"),
        "/deep/nested/path/123"
    );

    // Open redirect attacks (must all fall back to "/")
    let attack_paths = vec![
        "",                                         // Empty
        "https://evil.com",                         // Absolute scheme
        "http://evil.com",                          // HTTP scheme
        "//evil.com",                               // Protocol-relative scheme
        "///evil.com",                              // Triple slash
        "/\\evil.com",                              // Backslash evasion
        "\\evil.com",                               // Windows backslash
        "javascript:alert(1)",                      // Javascript pseudo-protocol
        "data:text/html,<script>alert(1)</script>", // Data URI
        "/path://evil.com",                         // Embedded ://
        "/path with space",                         // Unsafe space (byte 0x20 < 0x21)
        "/path\r\nSet-Cookie: evil=1",              // CRLF injection attempt
        "/path\0null",                              // Null byte
        "/path\u{007f}del",                         // DEL character (0x7F)
    ];
    for attack in attack_paths {
        assert_eq!(
            safe_return_path(attack),
            "/",
            "Open redirect attack '{attack}' did not fall back to '/'"
        );
    }

    // Excess length (> 512 chars)
    let long_path = format!("/{}", "a".repeat(513));
    assert_eq!(safe_return_path(&long_path), "/");
}

// ============================================================================
// 9. OAUTH PKCE CODE CHALLENGE & VERIFIER MATHEMATICAL INVARIANTS
// ============================================================================

#[test]
fn test_pkce_challenge_and_verifier_rfc7636_parity() {
    let now = 1792000000;
    let (state1, _) = create_oauth_state(TEST_SECRET, "/return1", now).unwrap();
    let (state2, _) = create_oauth_state(TEST_SECRET, "/return2", now).unwrap();

    let verifier1 = pkce_verifier(TEST_SECRET, &state1);
    let verifier2 = pkce_verifier(TEST_SECRET, &state2);

    // RFC 7636: 43 base64url characters without padding
    assert_eq!(verifier1.len(), 43);
    assert_eq!(verifier2.len(), 43);
    assert!(!verifier1.contains('='));
    assert!(!verifier1.contains('+'));
    assert!(!verifier1.contains('/'));

    // Distinct states must produce distinct verifiers
    assert_ne!(verifier1, verifier2);

    // Deterministic reproduction
    let verifier1_repeat = pkce_verifier(TEST_SECRET, &state1);
    assert_eq!(verifier1, verifier1_repeat);

    // S256 Challenge
    let challenge1 = pkce_challenge(&verifier1);
    let challenge2 = pkce_challenge(&verifier2);

    assert_eq!(challenge1.len(), 43);
    assert_eq!(challenge2.len(), 43);
    assert!(!challenge1.contains('='));
    assert_ne!(challenge1, challenge2);
}

// ============================================================================
// 10. OAUTH STATE CREATION, EXPIRATION, CLOCK SKEW & MISMATCH DEFENSES
// ============================================================================

#[test]
fn test_oauth_state_adversarial_tampering_and_mismatch() {
    let now = 1792000000;
    let (state, cookie_str) = create_oauth_state(TEST_SECRET, "/admin/overview", now).unwrap();

    // Parse the cookie to get raw cookie value
    let cookies = parse_cookies(&cookie_str);
    let cookie_val = cookies
        .get(OAUTH_STATE_COOKIE_NAME)
        .expect("Must have state cookie");
    assert_eq!(cookie_val, &state);

    // 1. Baseline: valid state matches cookie
    let verified =
        verify_oauth_state(TEST_SECRET, &state, cookie_val, now).expect("Valid state must verify");
    assert_eq!(verified.r, "/admin/overview");
    assert_eq!(verified.v, 1);

    // 2. State mismatch: query param != cookie
    let (other_state, _) = create_oauth_state(TEST_SECRET, "/other", now).unwrap();
    assert!(verify_oauth_state(TEST_SECRET, &state, &other_state, now).is_err());
    assert!(verify_oauth_state(TEST_SECRET, &other_state, &state, now).is_err());

    // 3. Empty state or empty cookie
    assert!(verify_oauth_state(TEST_SECRET, "", cookie_val, now).is_err());
    assert!(verify_oauth_state(TEST_SECRET, &state, "", now).is_err());

    // 4. Bit flip in state token signature
    let mut tampered_sig = state.clone();
    let last_char = tampered_sig.pop().unwrap();
    let flipped_char = if last_char == 'a' { 'b' } else { 'a' };
    tampered_sig.push(flipped_char);
    assert!(verify_oauth_state(TEST_SECRET, &tampered_sig, &tampered_sig, now).is_err());

    // 5. Expiration boundary: valid at exp - 1, expired at exp
    assert!(verify_oauth_state(TEST_SECRET, &state, cookie_val, verified.exp - 1).is_ok());
    assert!(verify_oauth_state(TEST_SECRET, &state, cookie_val, verified.exp).is_err());
    assert!(verify_oauth_state(TEST_SECRET, &state, cookie_val, verified.exp + 100).is_err());

    // 6. Clock skew check: valid at iat - CLOCK_SKEW_SECONDS, invalid at iat - CLOCK_SKEW_SECONDS - 1
    assert!(verify_oauth_state(
        TEST_SECRET,
        &state,
        cookie_val,
        verified.iat - CLOCK_SKEW_SECONDS
    )
    .is_ok());
    assert!(verify_oauth_state(
        TEST_SECRET,
        &state,
        cookie_val,
        verified.iat - CLOCK_SKEW_SECONDS - 1
    )
    .is_err());
}

// ============================================================================
// 11. OAUTH STATE NONCE SINGLE-USE REPLAY ATTACK MITIGATION
// ============================================================================

#[test]
fn test_oauth_state_nonce_single_use_replay_protection() {
    let nonce = generate_nonce().unwrap();
    assert_eq!(nonce.len(), 32);

    // First consumption succeeds
    assert!(mark_state_nonce_used(&nonce));

    // Replay with exact same nonce must fail
    assert!(!mark_state_nonce_used(&nonce));
    assert!(!mark_state_nonce_used(&nonce));

    // Distinct nonce succeeds
    let nonce2 = generate_nonce().unwrap();
    assert_ne!(nonce, nonce2);
    assert!(mark_state_nonce_used(&nonce2));
}

// ============================================================================
// 12. RFC 8628 DEVICE FLOW COMPLETE LIFECYCLE & TIMING ATTACKS
// ============================================================================

#[test]
fn test_device_flow_complete_lifecycle_and_interval_slow_down() {
    let now = 1792000000;
    let (record, handle) = start_device_flow(TEST_SECRET, now).unwrap();

    // Invariants
    assert_eq!(record.user_code.len(), 9);
    assert!(record.user_code.contains('-'));
    assert_eq!(record.device_code.len(), 64);
    assert_eq!(record.interval, 5);
    assert_eq!(record.expires_at, now + 900);
    assert_eq!(record.verification_uri, "https://github.com/login/device");

    // Poll 1 at now + 5 (at interval boundary): pending
    let poll1 = poll_device_flow(&record.device_code, now + 5);
    assert_eq!(poll1, DevicePollResult::Pending);

    // Rapid Poll 2 at now + 6 (< next_poll_at which is now + 10): triggers SlowDown with interval = 10
    let poll2 = poll_device_flow(&record.device_code, now + 6);
    assert_eq!(poll2, DevicePollResult::SlowDown { interval: 10 });

    // Poll 3 with invalid handle / code
    let poll_invalid = poll_device_flow("nonexistent_device_code_12345", now);
    assert_eq!(poll_invalid, DevicePollResult::Invalid);

    // Authorize device
    assert!(authorize_device(
        &record.device_code,
        ALLOWED_ADMIN_LOGIN,
        ALLOWED_ADMIN_ID
    ));

    // Poll 4 after authorization with device_code -> Authorized
    let poll_auth_code = poll_device_flow(&record.device_code, now + 25);
    assert_eq!(
        poll_auth_code,
        DevicePollResult::Authorized {
            user_login: ALLOWED_ADMIN_LOGIN.to_string(),
            user_id: ALLOWED_ADMIN_ID,
        }
    );

    // Poll 5 after authorization with device_handle -> Authorized
    let poll_auth_handle = poll_device_flow(&handle, now + 25);
    assert_eq!(
        poll_auth_handle,
        DevicePollResult::Authorized {
            user_login: ALLOWED_ADMIN_LOGIN.to_string(),
            user_id: ALLOWED_ADMIN_ID,
        }
    );

    // Poll 6 after expiration (now + 901) -> Expired
    let (exp_record, _) = start_device_flow(TEST_SECRET, now).unwrap();
    let poll_exp = poll_device_flow(&exp_record.device_code, now + 901);
    assert_eq!(poll_exp, DevicePollResult::Expired);
}
