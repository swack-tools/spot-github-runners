//! Guards against new or changed routes that forget the administrator check.
//!
//! Every route arm in `src/lib.rs` must either be on the short public list below
//! or contain a recognised authorization check before it touches state.

const LIB_RS: &str = include_str!("../src/lib.rs");

/// Routes that are reachable without a session by design. Keep this list short:
/// the login gate, sign-in and sign-out, and a minimal health check.
const PUBLIC_ROUTES: &[&str] = &[
    "\"/\"",
    "\"/index.html\"",
    "\"/favicon.ico\"",
    "\"/api/health\"",
    "\"/api/auth/status\"",
    "\"/auth/github/login\"",
    "\"/auth/github/callback\"",
    "\"/auth/device/code\"",
    "\"/auth/device/token\"",
    "\"/auth/logout\"",
];

/// Markers that show an arm enforces administrator or builder-token auth, or is
/// a test-only route that answers 404 once production OAuth is configured.
const AUTH_MARKERS: &[&str] = &[
    "require_administrator(",
    "session.uid != state::OWNER_UID",
    "may_sign_in(",
    "auth::check_session(",
    "verified_builder_token",
    "has_static_builder_secret",
    "error_response(404, \"not_found\", \"Not found\")",
];

/// Splits the router body into `(first line, arm body)` pairs.
fn route_arms() -> Vec<(String, String)> {
    let lines: Vec<&str> = LIB_RS.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("        (Method::"))
        .map(|(i, _)| i)
        .collect();
    assert!(starts.len() > 30, "router not found in src/lib.rs");
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(lines.len());
            (
                lines[start].trim().to_string(),
                lines[start..end].join("\n"),
            )
        })
        .collect()
}

#[test]
fn every_route_is_public_by_design_or_checks_authorization() {
    let mut unguarded = Vec::new();
    for (head, body) in route_arms() {
        let is_public = PUBLIC_ROUTES.iter().any(|p| head.contains(p));
        let is_guarded = AUTH_MARKERS.iter().any(|m| body.contains(m));
        if !is_public && !is_guarded {
            unguarded.push(head);
        }
    }
    assert!(
        unguarded.is_empty(),
        "Routes without an authorization check (add require_administrator or list them as public on purpose):\n{}",
        unguarded.join("\n")
    );
}

#[test]
fn state_reads_require_an_administrator() {
    for route in [
        "\"/api/overview\"",
        "\"/api/scaling\"",
        "\"/api/patches\"",
        "\"/api/patches/manifest\"",
        "\"/api/patches/save\"",
        "\"/api/rust-builds\"",
        "\"/api/rust-builds/queue\"",
        "(Method::Get, p) if p.starts_with(\"/api/rust-builds/\")",
        "\"/api/admins\")",
        "\"/api/inventory/refresh\"",
        "\"/api/reconciler/tick\"",
    ] {
        let (_, body) = route_arms()
            .into_iter()
            .find(|(head, _)| head.contains(route) && head.contains("Method::"))
            .unwrap_or_else(|| panic!("route {route} not found"));
        assert!(
            body.contains("require_administrator(") || body.contains("auth::admin_read_access("),
            "{route} must call require_administrator"
        );
    }
}
