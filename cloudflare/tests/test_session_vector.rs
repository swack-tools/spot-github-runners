use serde_json::Value;
use spot_dashboard_edge::auth::{
    derive_purpose_key, sign_token, verify_session, verify_token_signature,
};

#[test]
fn test_exact_session_fixture_vector() {
    let fixture_str = include_str!("../../tests/fixtures/session_vector.json");
    let fixture: Value = serde_json::from_str(fixture_str).expect("Valid fixture JSON");

    let secret = fixture["secret"].as_str().unwrap().as_bytes();
    let expected_token = fixture["valid_session"]["token"].as_str().unwrap();
    let now = fixture["valid_session"]["now"].as_u64().unwrap();
    let expired_now = fixture["expired_now"].as_u64().unwrap();
    let tampered_token = fixture["tampered_session_token"].as_str().unwrap();

    // 1. Verify valid token at valid now
    let session = verify_session(expected_token, secret, None, now).expect("Session must be valid");
    assert_eq!(session.sub, "swackhamer");
    assert_eq!(session.uid, 619624);
    assert_eq!(session.iat, 1792000000);
    assert_eq!(session.exp, 1792043200);
    assert_eq!(session.tgt, vec!["swack-tools/oxidex"]);

    // 2. Verify tampered token is rejected
    assert!(verify_session(tampered_token, secret, None, now).is_none());

    // 3. Verify expired token is rejected
    assert!(verify_session(expected_token, secret, None, expired_now).is_none());

    // 4. Verify clock skew boundary
    assert!(verify_session(expected_token, secret, None, 1792000000 - 60).is_some());
    assert!(verify_session(expected_token, secret, None, 1792000000 - 61).is_none());

    // 5. Verify bulk revocation
    assert!(verify_session(expected_token, secret, Some("1792000001"), now).is_none());
    assert!(verify_session(expected_token, secret, Some("1792000000"), now).is_some());
    assert!(verify_session(expected_token, secret, Some("malformed"), now).is_none());

    // 6. Verify bit-for-bit minting parity
    let session_key = derive_purpose_key(secret, "session");
    let minted = sign_token(&session_key, &session).expect("Must sign");
    assert_eq!(minted, expected_token);

    // 7. Verify control vector
    let control_secret = secret;
    let control_key = derive_purpose_key(control_secret, "control");
    let control_token = fixture["control"]["token"].as_str().unwrap();
    let verified_control =
        verify_token_signature(&control_key, control_token).expect("Control token valid");
    assert_eq!(verified_control["op"], "freeze");
    assert_eq!(verified_control["slot"], "a");
}
