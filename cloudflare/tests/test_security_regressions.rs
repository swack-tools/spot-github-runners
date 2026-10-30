//! Regression tests for issues found in the October 2026 Worker security review.

use serde_json::json;
use spot_dashboard_edge::patch_console::{handle_preview, handle_validate};

const UI_RS: &str = include_str!("../src/ui.rs");

#[test]
fn block_scalar_with_unicode_indent_does_not_panic() {
    for content in [
        "a: |\n  x\n\u{3000}y\n",
        "a:\n  - b: |\n      x\n\u{3000}y\n",
        "a: |-\n  x\n\u{2003}\u{2003}y\n",
    ] {
        let body = json!({ "content": content });
        let _ = handle_validate(&body);
        let _ = handle_preview(&body);
    }
}

#[test]
fn inline_handlers_never_build_js_strings_with_html_escaping() {
    // HTML entities are decoded before an inline handler runs, so `${esc(x)}`
    // inside on*="..." lets a quote in x break out. Use jsArg(x) instead.
    let mut offenders = Vec::new();
    for line in UI_RS.lines() {
        for (idx, _) in line.match_indices(" on") {
            let rest = &line[idx + 3..];
            let name_len = rest.chars().take_while(|c| c.is_ascii_lowercase()).count();
            if name_len == 0 || !rest[name_len..].starts_with("=\"") {
                continue;
            }
            let value = rest[name_len + 2..].split('"').next().unwrap_or("");
            if value.contains("${esc(") {
                offenders.push(line.trim());
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "unsafe inline handlers:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn stored_links_go_through_safe_url() {
    for field in ["github_workflow_url", "deep_link"] {
        let raw = format!("href=\"${{esc({}.{field})}}\"", "x");
        assert!(!UI_RS.contains(&raw.replace("x.", "w.")));
        assert!(!UI_RS.contains(&raw.replace("x.", "l.")));
        assert!(!UI_RS.contains(&raw.replace("x.", "r.")));
        assert!(!UI_RS.contains(&raw.replace("x.", "run.")));
    }
    assert!(UI_RS.contains("function safeUrl(val)"));
}

#[test]
fn missing_oauth_secret_fails_closed() {
    use spot_dashboard_edge::oauth_mode_allows_request;
    for path in [
        "/",
        "/api/overview",
        "/auth/device/code",
        "/api/test/authorize-device",
        "/api/reconciler/tick",
        "/api/inventory/refresh",
    ] {
        assert!(
            !oauth_mode_allows_request(false, false, path),
            "{path} must be refused"
        );
        assert!(oauth_mode_allows_request(true, false, path));
        assert!(oauth_mode_allows_request(false, true, path));
    }
    assert!(oauth_mode_allows_request(false, false, "/api/health"));
}
