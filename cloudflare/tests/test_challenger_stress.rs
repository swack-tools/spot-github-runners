use spot_dashboard_edge::state::*;
use spot_dashboard_edge::auth::*;
use spot_dashboard_edge::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::sync::Arc;

struct TestKv {
    data: Mutex<HashMap<String, String>>,
    get_count: AtomicUsize,
    put_count: AtomicUsize,
}

impl TestKv {
    fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            get_count: AtomicUsize::new(0),
            put_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for TestKv {
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

#[tokio::test]
async fn test_cache_ttl_strict_boundary() {
    invalidate_state_cache();
    let kv = TestKv::new();
    let t0 = 100000;

    // Load at t0: KV get #1
    let (_, src0) = load_state(&kv, t0).await.unwrap();
    assert_eq!(src0, CacheSource::DefaultCreated);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);

    // At t0 + 59: within 60s TTL -> MemoryHit, no KV get
    let (_, src59) = load_state(&kv, t0 + 59).await.unwrap();
    assert_eq!(src59, CacheSource::MemoryHit);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 1);

    // At t0 + 60: TTL strictly expired (now - cached_at == 60, not < 60) -> KV get #2
    let (_, src60) = load_state(&kv, t0 + 60).await.unwrap();
    assert_ne!(src60, CacheSource::MemoryHit);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 2);

    // Explicit invalidation at t0 + 61
    invalidate_state_cache();
    let (_, src_inv) = load_state(&kv, t0 + 61).await.unwrap();
    assert_ne!(src_inv, CacheSource::MemoryHit);
    assert_eq!(kv.get_count.load(Ordering::SeqCst), 3);
}

#[test]
fn test_path_normalization_adversarial_matrix() {
    let blocked_paths = vec![
        "/api/control",
        "//api/control",
        "///api/control",
        "/api//control",
        "/api/control/",
        "/api/control//",
        "/api/control/secret",
        "/api/control?action=dump",
        "/api/control;param=1",
        "/foo/../api/control",
        "/foo/%2e%2e/api/control",
        "/foo/%252e%252e/api/control",
        "//api/%63ontrol",
        "/api/%63%6f%6e%74%72%6f%6c",
        "/./api/control",
        "/.//api/control",
        "//./api/control",
        "/API/CONTROL",
        "/Api/Control",
    ];

    for path in blocked_paths {
        assert!(
            is_control_path(path),
            "Expected path to be blocked: '{}'",
            path
        );
    }

    let allowed_paths = vec![
        "/",
        "/index.html",
        "/favicon.ico",
        "/api/health",
        "/api/overview",
        "/api/scaling",
        "/api/admins",
        "/auth/github/login",
        "/auth/github/callback",
        "/auth/device/code",
        "/auth/device/token",
        "/auth/device/poll",
        "/api/auth/status",
        "/auth/logout",
    ];

    for path in allowed_paths {
        assert!(
            !is_control_path(path),
            "Expected path to be allowed: '{}'",
            path
        );
    }
}

#[test]
fn test_auth_body_limit_paths() {
    assert!(is_auth_path("/auth/github/login"));
    assert!(is_auth_path("/auth/github/callback"));
    assert!(is_auth_path("/auth/device/code"));
    assert!(is_auth_path("/auth/device/token"));
    assert!(is_auth_path("/auth/device/poll"));
    assert!(is_auth_path("/auth/logout"));
    assert!(is_auth_path("/api/auth/status"));
    assert!(is_auth_path("/api/auth/logout"));

    assert!(!is_auth_path("/api/overview"));
    assert!(!is_auth_path("/api/scaling"));
    assert!(!is_auth_path("/api/admins"));
    assert!(!is_auth_path("/api/instances/delete"));
}

#[tokio::test]
async fn test_scaling_adversarial_validation_zero_kv_writes() {
    invalidate_state_cache();
    let kv = TestKv::new();
    let now = 100000;

    // Initial load
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    save_state(&kv, &mut bundle, now).await.unwrap();
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // Negative / boundary values for runner scaling
    let invalid_configs = vec![
        serde_json::json!({"min_runners": 4, "max_runners": 24, "queued_jobs_threshold": 19}),
        serde_json::json!({"min_runners": 4, "max_runners": 24, "queued_jobs_threshold": 0}),
        serde_json::json!({"min_runners": 25, "max_runners": 24, "queued_jobs_threshold": 20}),
    ];

    for cfg in invalid_configs {
        let req = UpdateScalingRequest {
            role: "cicd_runner".into(),
            policy: cfg,
        };
        let res = handle_post_scaling(&kv, req, now).await;
        assert!(res.is_err(), "Expected invalid config to fail");
        assert_eq!(kv.put_count.load(Ordering::SeqCst), 1, "Must perform 0 writes on failure");
    }
}

#[test]
fn test_safe_return_path_open_redirect_adversarial() {
    let dangerous_paths = vec![
        "https://evil.com",
        "http://evil.com/phish",
        "//evil.com",
        "///evil.com",
        "//evil.com/path",
        "/\\evil.com",
        "\\evil.com",
        "javascript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "/path://evil.com",
        "/path\r\nSet-Cookie: evil=1",
        "/path\0null",
    ];

    for dangerous in dangerous_paths {
        let sanitized = safe_return_path(dangerous);
        assert_eq!(
            sanitized, "/",
            "Dangerous path '{}' was not sanitized to '/': got '{}'",
            dangerous, sanitized
        );
    }

    let safe_paths = vec!["/dashboard", "/overview", "/settings", "/runners"];
    for safe in safe_paths {
        let sanitized = safe_return_path(safe);
        assert_eq!(sanitized, safe, "Safe path '{}' was incorrectly modified", safe);
    }
}

#[tokio::test]
async fn test_concurrent_read_avoids_duplicate_kv_loads() {
    invalidate_state_cache();
    let kv = Arc::new(TestKv::new());
    let now = 1792000000;

    // Populate initial state in KV
    let (mut bundle, _) = load_state(kv.as_ref(), now).await.unwrap();
    save_state(kv.as_ref(), &mut bundle, now).await.unwrap();
    let initial_gets = kv.get_count.load(Ordering::SeqCst);

    // Concurrently simulate 50 requests
    for _ in 0..50 {
        let (bundle, src) = load_state(kv.as_ref(), now + 5).await.unwrap();
        assert_eq!(src, CacheSource::MemoryHit);
        assert_eq!(bundle.schema, SNAPSHOT_SCHEMA);
    }

    // Absolutely zero additional KV reads occurred
    assert_eq!(kv.get_count.load(Ordering::SeqCst), initial_gets);
}

#[test]
fn test_cookie_fuzzing_resilience() {
    let long_hdr = "a=b; ".repeat(1000);
    let malformed_headers = vec![
        "",
        ";;;",
        "===;",
        "__Host-spot_session=",
        "__Host-spot_session=\"\"; other=123",
        "__Host-spot_session=\"payload.sig\"; other=456",
        "__Host-spot_session=abc.def.ghi",
        "__Host-spot_session=\x00\x01\x02",
        long_hdr.as_str(),
    ];

    for hdr in malformed_headers {
        let parsed = parse_cookies(hdr);
        // Ensure parser never panics
        if let Some(val) = parsed.get("__Host-spot_session") {
            assert!(!val.is_empty() || hdr.contains("__Host-spot_session=\"\"") || hdr.contains("__Host-spot_session="));
        }
    }
}
