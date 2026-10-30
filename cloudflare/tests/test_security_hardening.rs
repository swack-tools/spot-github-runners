use spot_dashboard_edge::auth::{
    build_github_authorize_url, builder_token_issued_after_cutoff, is_cross_origin_request,
    mint_builder_queue_token, verify_builder_queue_token,
};

const ORIGIN: &str = "https://spot.swacktech.com";

#[test]
fn sign_in_only_asks_github_for_the_users_identity() {
    let url = build_github_authorize_url("client", "https://spot.swacktech.com/cb", "st", "ch");
    assert!(url.contains("&scope=read%3Auser&"), "{url}");
    assert!(
        !url.contains("repo"),
        "the dashboard never needs repository access: {url}"
    );
}

#[test]
fn builder_queue_tokens_honor_the_session_cutoff() {
    let now = 1_800_000_000;
    let tok = mint_builder_queue_token(b"secret", "builder-1", now).unwrap();
    let payload = verify_builder_queue_token(&tok, b"secret", now + 10).unwrap();

    assert!(builder_token_issued_after_cutoff(&payload, None));
    assert!(builder_token_issued_after_cutoff(&payload, Some("")));
    assert!(builder_token_issued_after_cutoff(
        &payload,
        Some(&now.to_string())
    ));
    assert!(!builder_token_issued_after_cutoff(
        &payload,
        Some(&(now + 1).to_string())
    ));
    // A malformed cut-off revokes rather than ignores.
    assert!(!builder_token_issued_after_cutoff(&payload, Some("soon")));
}

#[test]
fn cross_origin_cookie_writes_are_detected() {
    // Same-origin dashboard calls and non-browser clients pass.
    assert!(!is_cross_origin_request(
        Some(ORIGIN),
        Some("same-origin"),
        ORIGIN
    ));
    assert!(!is_cross_origin_request(None, None, ORIGIN));
    assert!(!is_cross_origin_request(None, Some("none"), ORIGIN));

    // Sibling subdomains and other sites are refused.
    assert!(is_cross_origin_request(
        Some("https://docs.swacktech.com"),
        Some("same-site"),
        ORIGIN
    ));
    assert!(is_cross_origin_request(
        Some("https://evil.example"),
        Some("cross-site"),
        ORIGIN
    ));
    assert!(is_cross_origin_request(
        Some("https://evil.example"),
        None,
        ORIGIN
    ));
    assert!(is_cross_origin_request(None, Some("cross-site"), ORIGIN));
    assert!(is_cross_origin_request(Some("null"), None, ORIGIN));
}
