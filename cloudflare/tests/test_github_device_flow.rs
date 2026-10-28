//! Production device flow: the user code must come from GitHub, and the
//! browser polls with a signed, expiring handle around GitHub's device code.

use serde_json::json;
use spot_dashboard_edge::auth::*;

const TEST_SECRET: &[u8] = b"test-vector-secret-not-for-production-0123456789abcdef";

fn github_code() -> GithubDeviceCode {
    parse_github_device_code_response(&json!({
        "device_code": "3584d83530557fdd1f46af8289938c8ef79f9dc5",
        "user_code": "WDJB-MJHT",
        "verification_uri": "https://github.com/login/device",
        "expires_in": 900,
        "interval": 5
    }))
    .unwrap()
}

#[test]
fn device_code_response_keeps_githubs_user_code() {
    let code = github_code();
    assert_eq!(code.user_code, "WDJB-MJHT");
    assert_eq!(code.device_code, "3584d83530557fdd1f46af8289938c8ef79f9dc5");
    assert_eq!(code.verification_uri, "https://github.com/login/device");
    assert_eq!(code.expires_in, 900);
    assert_eq!(code.interval, 5);
}

#[test]
fn device_code_response_surfaces_disabled_device_flow() {
    let err = parse_github_device_code_response(&json!({
        "error": "device_flow_disabled",
        "error_description": "Device Flow must be explicitly enabled for this App"
    }))
    .unwrap_err();
    assert!(err.starts_with("device_flow_disabled"));
}

#[test]
fn device_code_response_rejects_missing_fields_and_clamps_timing() {
    assert!(parse_github_device_code_response(&json!({ "user_code": "AAAA-BBBB" })).is_err());
    assert!(parse_github_device_code_response(&serde_json::Value::Null).is_err());

    let code = parse_github_device_code_response(&json!({
        "device_code": "d", "user_code": "u", "verification_uri": "https://github.com/login/device",
        "expires_in": 999_999, "interval": 0
    }))
    .unwrap();
    assert_eq!(code.expires_in, 1800);
    assert_eq!(code.interval, DEVICE_MIN_INTERVAL_SECONDS);
}

#[test]
fn device_handle_round_trips_and_expires() {
    let now = 1_800_000_000;
    let code = github_code();
    let handle = sign_device_handle(TEST_SECRET, &code, now).unwrap();
    assert!(!handle.contains(&code.user_code));

    let rec = verify_device_handle(TEST_SECRET, &handle, now + 10).unwrap();
    assert_eq!(rec.device_code, code.device_code);
    assert_eq!(rec.interval, 5);

    assert_eq!(
        verify_device_handle(TEST_SECRET, &handle, now + 900),
        Err(DevicePollResult::Expired)
    );
}

#[test]
fn device_handle_rejects_forgery_and_wrong_secret() {
    let now = 1_800_000_000;
    let handle = sign_device_handle(TEST_SECRET, &github_code(), now).unwrap();
    assert_eq!(
        verify_device_handle(b"another-secret-entirely-000000000000000", &handle, now),
        Err(DevicePollResult::Invalid)
    );
    // A raw GitHub device code is not a handle.
    assert_eq!(
        verify_device_handle(TEST_SECRET, "3584d83530557fdd1f46af8289938c8ef79f9dc5", now),
        Err(DevicePollResult::Invalid)
    );
    assert_eq!(
        verify_device_handle(TEST_SECRET, "", now),
        Err(DevicePollResult::Invalid)
    );
    // A handle signed for another purpose (OAuth state) is refused.
    let (state, _) = create_oauth_state(TEST_SECRET, "/", now).unwrap();
    assert_eq!(
        verify_device_handle(TEST_SECRET, &state, now),
        Err(DevicePollResult::Invalid)
    );
}

#[test]
fn token_responses_map_to_poll_outcomes() {
    assert_eq!(
        classify_github_device_token_response(
            &json!({ "access_token": "gho_x", "token_type": "bearer" }),
            5
        ),
        GithubDeviceTokenOutcome::AccessToken("gho_x".to_string())
    );
    assert_eq!(
        classify_github_device_token_response(&json!({ "error": "authorization_pending" }), 5),
        GithubDeviceTokenOutcome::Pending
    );
    assert_eq!(
        classify_github_device_token_response(&json!({ "error": "slow_down", "interval": 10 }), 5),
        GithubDeviceTokenOutcome::SlowDown { interval: 10 }
    );
    assert_eq!(
        classify_github_device_token_response(&json!({ "error": "slow_down" }), 5),
        GithubDeviceTokenOutcome::SlowDown { interval: 10 }
    );
    assert_eq!(
        classify_github_device_token_response(&json!({ "error": "expired_token" }), 5),
        GithubDeviceTokenOutcome::Expired
    );
    assert_eq!(
        classify_github_device_token_response(&json!({ "error": "access_denied" }), 5),
        GithubDeviceTokenOutcome::Denied
    );
    assert!(matches!(
        classify_github_device_token_response(
            &json!({ "error": "incorrect_client_credentials", "error_description": "bad id" }),
            5
        ),
        GithubDeviceTokenOutcome::Failed(d) if d == "bad id"
    ));
    assert!(matches!(
        classify_github_device_token_response(&json!({ "access_token": "" }), 5),
        GithubDeviceTokenOutcome::Failed(_)
    ));
}

#[test]
fn authorize_url_offers_account_picker() {
    let url =
        build_github_authorize_url("cid", "https://spot.example/auth/github/callback", "s", "c");
    assert!(url.contains("&prompt=select_account"));
}

#[test]
fn not_admin_message_names_the_account() {
    let msg = not_admin_message("xbmc4lyfe");
    assert!(msg.contains("'xbmc4lyfe'"));
    assert!(msg.contains("not an authorized administrator"));
}
