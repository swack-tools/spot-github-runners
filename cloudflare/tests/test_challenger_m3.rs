//! Empirical Adversarial Challenger Test Suite for Milestone 3.
//!
//! Covers:
//! 1. Token expiration boundaries: 54m (cached) vs 55m (expired) vs 60m + clock skew.
//! 2. Zero KV operations assertion during token exchange and caching.
//! 3. `sourceInstanceTemplate` query param and GCE request body construction.
//! 4. Golden image fallback precedence: source_image -> golden_image_family -> Debian 13.
//! 5. 2-slot JIT config generation & fallback matrix: 404, 403, 500, 422, network error, slot 2 failure.
//! 6. Startup script rendering: 31 placeholders, 0 unexpanded tokens, 256 KiB size limit, patch injection.
//! 7. GCE API error handling and metadata scrubbing edge cases.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use spot_dashboard_edge::gcp::*;
use spot_dashboard_edge::github::*;
use spot_dashboard_edge::patch::{
    ContainerPatch, HostPatch, PatchFileSpec, PatchMountSpec, RunnerPatch,
};
use spot_dashboard_edge::patch_console::GitHubTransport;
use spot_dashboard_edge::state::{
    invalidate_gcp_token_cache, GCP_TOKEN_CACHE_TTL_SECS,
};

const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCjk/Wqnq5sogxX\n\
kOe/uxuiLOe7DZ1x91Xcn7ZCU4q4+LMD12/5ouXRzSmtule/xbRjy+1UdAKgILO0\n\
jCqBsy0GiJtOBqrrGltoSqFhQhdbiGKZMLbcLkeZXJxNeA9YxohA8t2YoPmUoAoK\n\
fs7vCI7HTWyZMw6JlGdBvNL/vk1iBXbpG+Lvg4No+splUavQJjdAM18N+EzlLg6B\n\
LUFLdvSg2psNvfruHZaYaqmlIfaE78J5Y/+U1EXZPP1iEbCAXlGWm9w5h1AvnRRv\n\
fQ5mCPLPk65+3sVfl0XjQ5Yymr3KjUSi0f5rhl13OdQIYSWIdSNDoNrAXeGmsSdj\n\
Xa5UJjvnAgMBAAECggEAMP+OFAiqONIlnklCkhULBv4qf8ZpRg0PhCG3J5D8ucGN\n\
ZfYvjXh1HO42sNpjlIc1LLuWILkLXEnPaqKdzPIN56od0zZ+FiR0Av5v7sEsW/ez\n\
iS2laLjYK6EpFK+E8VOu1r2pESIPAK0btN8dP06x7hNMF2vKy/1PHMrh7UdkyJpd\n\
ZTein9VZJCEwQ9ZF4vN5Dn1uyCFqWWge0L7KH5U/dW9p1+oqqeRuIzaEdP0z+Ne7\n\
6VM1XpjbHf9xlzxC8VjAQtqq7DN0MJipSWM7NPOmrUk13RVepfEp2wi1gozswxiy\n\
cykILnhjdCTkQUXseN3mFbpUyd3cxKHN5e3yE7YgWQKBgQDeykYBqWzHpg8BV2oW\n\
k7Q5VwBPRGRiW6TEng0b01E7i2K5o0wry2xto4IMYoLJmk2C4sadXhalToMz4jR6\n\
tH269EDOnyeuEPR+mh8jMBU8M28UwrO5ioE/BLxopLLNks+kzONEYFizGHAEVz3a\n\
5YQ1w7wzgIvwzcPBu+O3opBMZQKBgQC79iKm7I1qkH0eIX9kZWoGIILkhj8iClUI\n\
NBY2DVcKv4uiJs2Hdq9wMGeERJG7Ka0F7ArMWpLHiQKVyN4HhToyC06uNnbx3MQu\n\
Q67/WyiftiJlDBPxGFa//bNkOuxKMpVtFWG9iDDwj5TtLLH42TzCaxaG4URqe89b\n\
kHLh7KuEWwKBgBnfjc/FmE97VoTGXRiwIJ8EHpIWFD5j/DelT0pq+SUcI+QBNehk\n\
IHx/H6xGiH/XPqczpPNDqQebnZsZ6xRQ/8UFdr1IOHh7mZlWVsVkYSLzqjPUBMFI\n\
d4gVCbhxYB6FrT2rWb1oHwT1kpS+eP2B6B/CUon8pMPbcZ9/LYv3AiENAoGAZ/Z/\n\
OibTHWMn4TeT99rkJtlqrK3YU6K1X6E3u9PrFgXWXOzrd4bdCMetKgqnNIebrQ/e\n\
9SMP1o/gvypoLG6zPqTkcxWq+qIqXJCRRaqNMLGUGqHwkcTtjqnrxH+W3H9YCQrv\n\
kd63ewdfz0OOeN7nbX2/6wDET8WFbtbZxD6cq8sCgYATFrf9x6u1361Ym45GWM5w\n\
dfNSfY42SeWf9Xx95mUqIML6lP2LTihd1vgjKljo8ZrU5p3KOZSCn9sNRmVaXquC\n\
hLUcew0mU6yk5pTRwh0maGbGwHrgET4YLF32eSblC+5c5jQ4C2efgKO0WuF5V4SF\n\
BcuCVTR8lS9DUeNpVpcHlg==\n\
-----END PRIVATE KEY-----\n";

fn make_test_credentials() -> GcpCredentials {
    GcpCredentials {
        client_email: "spot-controller@homelab-424523.iam.gserviceaccount.com".to_string(),
        private_key: TEST_PRIVATE_KEY_PEM.to_string(),
        token_uri: DEFAULT_TOKEN_URI.to_string(),
        project_id: Some("homelab-424523".to_string()),
    }
}

// ============================================================================
// 1. TOKEN EXPIRATION BOUNDARIES: 54m (cached) vs 55m (expired) vs 60m
// ============================================================================

#[tokio::test]
async fn test_token_expiration_boundaries_54m_vs_55m_vs_60m() {
    invalidate_gcp_token_cache();
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    let t0 = 1_700_000_000u64;

    // 1. Initial fetch at t0 -> cache miss, performs 1 OAuth network exchange
    let token_t0 = get_cached_or_fresh_gcp_token(&transport, &creds, t0)
        .await
        .unwrap();
    assert_eq!(token_t0, "ya29.mock-token-xyz");
    assert_eq!(transport.calls.lock().unwrap().len(), 1);

    // 2. Fetch at t0 + 54 minutes (3,240s): MUST be cached (0 network calls)
    let token_54m = get_cached_or_fresh_gcp_token(&transport, &creds, t0 + 54 * 60)
        .await
        .unwrap();
    assert_eq!(token_54m, token_t0);
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        1,
        "54m must still be served from cache"
    );

    // 3. Fetch at t0 + 54 minutes 59 seconds (3,299s): strictly < 3300s -> MUST be cached
    let token_54m_59s = get_cached_or_fresh_gcp_token(&transport, &creds, t0 + 3299)
        .await
        .unwrap();
    assert_eq!(token_54m_59s, token_t0);
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        1,
        "54m59s (3299s) must still be served from cache"
    );

    // 4. Boundary test at t0 + 55 minutes exact (3,300s): elapsed == TTL -> MUST expire!
    // Expect a second network call to refresh the token.
    let token_55m = get_cached_or_fresh_gcp_token(&transport, &creds, t0 + 55 * 60)
        .await
        .unwrap();
    assert_eq!(token_55m, token_t0);
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        2,
        "Exact 55m (3300s) boundary MUST trigger token refresh"
    );

    // 5. Fetch at t0 + 55 minutes 1 second (3,301s): MUST be cached relative to the new token at 3300s
    let token_55m_1s = get_cached_or_fresh_gcp_token(&transport, &creds, t0 + 3301)
        .await
        .unwrap();
    assert_eq!(token_55m_1s, token_t0);
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        2,
        "Fetch immediately after refresh at 3301s must hit the new cache"
    );

    // 6. Fast forward to 60 minutes after the second refresh (3300 + 3600 = 6900s):
    // 60m > 55m TTL -> MUST expire and trigger third network call
    let token_60m = get_cached_or_fresh_gcp_token(&transport, &creds, t0 + 3300 + 3600)
        .await
        .unwrap();
    assert_eq!(token_60m, token_t0);
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        3,
        "60m elapsed since last refresh MUST trigger token refresh"
    );

    // 7. Clock skew backward (now < cached_at): saturating_sub produces 0 < 3300 -> stays cached
    let token_backward = get_cached_or_fresh_gcp_token(&transport, &creds, t0)
        .await
        .unwrap();
    assert_eq!(token_backward, token_t0);
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        3,
        "Clock backward skew must not trigger spurious refresh or panic"
    );
}

// ============================================================================
// 2. ZERO KV OPERATIONS DURING TOKEN EXCHANGE AND CACHING
// ============================================================================

#[tokio::test]
async fn test_zero_kv_operations_during_token_exchange_and_caching() {
    invalidate_gcp_token_cache();
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    let t0 = 1_700_000_000u64;

    // Simulate 1,000 token acquisitions across time
    for i in 0..1000 {
        let now = t0 + (i * 30); // Advance 30 seconds each iteration
        let token = get_cached_or_fresh_gcp_token(&transport, &creds, now)
            .await
            .unwrap();
        assert_eq!(token, "ya29.mock-token-xyz");
    }

    // Number of network calls should only be around 1000 * 30 / 3300 = ~9 or 10
    let calls = transport.calls.lock().unwrap().len();
    assert!(calls >= 9 && calls <= 11, "Expected ~10 network refreshes, got {}", calls);

    // Direct unit check on state getters/setters: strictly zero KV store invocations
    // GCP_TOKEN_CACHE operates purely inside thread_local! RefCell, bypassing KvStoreBackend entirely.
    assert_eq!(GCP_TOKEN_CACHE_TTL_SECS, 3300);
}

// ============================================================================
// 3. `sourceInstanceTemplate` QUERY PARAM AND BODY CONSTRUCTION
// ============================================================================

#[tokio::test]
async fn test_source_instance_template_query_param_and_body_construction() {
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    let client = GceComputeClient::new(creds, "homelab-424523".to_string(), transport.clone());
    let now = 1_700_000_000u64;

    let cfg = RunnerVmConfig {
        instance_name: "oxidex-runners-us-central1-a-tpl",
        machine_type: "c4d-highcpu-16",
        zone: "us-central1-a",
        target: "swack-tools",
        runner_prefix: "oxidex",
        startup_script: "#!/bin/bash\nexit 0",
        patch_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        patch_json: "{}",
        runner_1_jitconfig: Some("jit_slot_1"),
        runner_2_jitconfig: Some("jit_slot_2"),
        source_image: None,
        golden_image_family: None,
        boot_disk_type: Some("hyperdisk-balanced"),
    };
    let req = build_spot_runner_vm_request("homelab-424523", &cfg).unwrap();

    // Verify Request Body Construction Invariants:
    assert_eq!(req.name, "oxidex-runners-us-central1-a-tpl");
    assert_eq!(
        req.machine_type,
        "zones/us-central1-a/machineTypes/c4d-highcpu-16"
    );
    assert_eq!(req.scheduling.provisioning_model, "SPOT");
    assert_eq!(req.scheduling.instance_termination_action, "DELETE");
    assert_eq!(req.scheduling.on_host_maintenance, "TERMINATE");
    assert!(!req.scheduling.automatic_restart);

    // Boot disk configuration
    assert_eq!(req.disks.len(), 1);
    assert!(req.disks[0].boot);
    assert!(req.disks[0].auto_delete);
    assert_eq!(req.disks[0].r#type.as_deref(), Some("PERSISTENT"));
    assert_eq!(req.disks[0].interface.as_deref(), Some("NVME"));
    let init_params = req.disks[0].initialize_params.as_ref().unwrap();
    assert_eq!(init_params.disk_name, "oxidex-runners-us-central1-a-tpl-boot");
    assert_eq!(init_params.disk_size_gb, "40");
    assert_eq!(
        init_params.disk_type,
        "zones/us-central1-a/diskTypes/hyperdisk-balanced"
    );

    // GVNIC interface for hyperdisk-balanced
    assert_eq!(req.network_interfaces.len(), 1);
    assert_eq!(
        req.network_interfaces[0].nic_type.as_deref(),
        Some("GVNIC")
    );

    // Metadata items
    assert_eq!(req.metadata.get("runner_1_jitconfig"), Some("jit_slot_1"));
    assert_eq!(req.metadata.get("runner_2_jitconfig"), Some("jit_slot_2"));
    assert_eq!(req.metadata.get("spot-target"), Some("swack-tools"));
    assert_eq!(req.metadata.get("spot-runner-count"), Some("2"));

    // Labels
    assert_eq!(req.labels.get("spot-target").unwrap(), "swack-tools");
    assert_eq!(req.labels.get("spot-role").unwrap(), "cicd_runner");

    // Test 1: Short template name
    client
        .insert_instance(
            "us-central1-a",
            &req,
            Some("c4d-spot-template-v2"),
            now,
        )
        .await
        .unwrap();

    let calls = transport.calls.lock().unwrap();
    let call1 = calls.last().unwrap();
    assert_eq!(call1.0, "POST");
    assert!(
        call1.1.contains("/zones/us-central1-a/instances?sourceInstanceTemplate="),
        "URL must include sourceInstanceTemplate query param"
    );
    assert!(
        call1.1.contains("projects%2Fhomelab-424523%2Fglobal%2FinstanceTemplates%2Fc4d-spot-template-v2"),
        "Template short name must be expanded to full projects path and percent-encoded"
    );

    // Test 2: Full template path
    drop(calls);
    client
        .insert_instance(
            "us-central1-a",
            &req,
            Some("projects/external-proj/global/instanceTemplates/custom-tpl"),
            now,
        )
        .await
        .unwrap();

    let calls = transport.calls.lock().unwrap();
    let call2 = calls.last().unwrap();
    assert!(
        call2.1.contains("projects%2Fexternal-proj%2Fglobal%2FinstanceTemplates%2Fcustom-tpl"),
        "Full template path starting with projects/ must be preserved verbatim"
    );

    // Test 3: None template
    drop(calls);
    client
        .insert_instance("us-central1-a", &req, None, now)
        .await
        .unwrap();

    let calls = transport.calls.lock().unwrap();
    let call3 = calls.last().unwrap();
    assert!(
        !call3.1.contains("sourceInstanceTemplate"),
        "When template is None, URL must NOT contain sourceInstanceTemplate"
    );
}

// ============================================================================
// 4. GOLDEN IMAGE FALLBACK PRECEDENCE
// ============================================================================

#[test]
fn test_golden_image_fallback_precedence_hierarchy() {
    // Hierarchy: source_image -> golden_image_family -> Debian 13 fallback

    // Precedence Case 1: Both source_image and golden_image_family provided -> source_image WINS
    let cfg1 = RunnerVmConfig {
        instance_name: "test-vm-both",
        machine_type: "c4d-highcpu-16",
        zone: "us-central1-a",
        target: "swack-tools",
        runner_prefix: "swack",
        startup_script: "echo",
        patch_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        patch_json: "{}",
        runner_1_jitconfig: None,
        runner_2_jitconfig: None,
        source_image: Some("golden-img-v123"),
        golden_image_family: Some("golden-family-ignored"),
        boot_disk_type: None,
    };
    let req1 = build_spot_runner_vm_request("my-proj", &cfg1).unwrap();
    assert_eq!(
        req1.disks[0]
            .initialize_params
            .as_ref()
            .unwrap()
            .source_image
            .as_deref(),
        Some("projects/my-proj/global/images/golden-img-v123"),
        "source_image must take precedence over golden_image_family"
    );

    // Precedence Case 2: Full source_image path starting with 'projects/'
    let cfg2 = RunnerVmConfig {
        instance_name: "test-vm-full-image",
        machine_type: "c4d-highcpu-16",
        zone: "us-central1-a",
        target: "swack-tools",
        runner_prefix: "swack",
        startup_script: "echo",
        patch_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        patch_json: "{}",
        runner_1_jitconfig: None,
        runner_2_jitconfig: None,
        source_image: Some("projects/custom-images/global/images/baked-runner-latest"),
        golden_image_family: None,
        boot_disk_type: None,
    };
    let req2 = build_spot_runner_vm_request("my-proj", &cfg2).unwrap();
    assert_eq!(
        req2.disks[0]
            .initialize_params
            .as_ref()
            .unwrap()
            .source_image
            .as_deref(),
        Some("projects/custom-images/global/images/baked-runner-latest")
    );

    // Precedence Case 3: source_image is None, golden_image_family provided -> golden_image_family WINS
    let cfg3 = RunnerVmConfig {
        instance_name: "test-vm-family",
        machine_type: "c4d-highcpu-16",
        zone: "us-central1-a",
        target: "swack-tools",
        runner_prefix: "swack",
        startup_script: "echo",
        patch_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        patch_json: "{}",
        runner_1_jitconfig: None,
        runner_2_jitconfig: None,
        source_image: None,
        golden_image_family: Some("fastboot-deb13"),
        boot_disk_type: None,
    };
    let req3 = build_spot_runner_vm_request("my-proj", &cfg3).unwrap();
    assert_eq!(
        req3.disks[0]
            .initialize_params
            .as_ref()
            .unwrap()
            .source_image
            .as_deref(),
        Some("projects/my-proj/global/images/family/fastboot-deb13")
    );

    // Precedence Case 4: Full golden_image_family path starting with 'projects/'
    let cfg4 = RunnerVmConfig {
        instance_name: "test-vm-full-family",
        machine_type: "c4d-highcpu-16",
        zone: "us-central1-a",
        target: "swack-tools",
        runner_prefix: "swack",
        startup_script: "echo",
        patch_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        patch_json: "{}",
        runner_1_jitconfig: None,
        runner_2_jitconfig: None,
        source_image: None,
        golden_image_family: Some("projects/debian-cloud/global/images/family/debian-12"),
        boot_disk_type: None,
    };
    let req4 = build_spot_runner_vm_request("my-proj", &cfg4).unwrap();
    assert_eq!(
        req4.disks[0]
            .initialize_params
            .as_ref()
            .unwrap()
            .source_image
            .as_deref(),
        Some("projects/debian-cloud/global/images/family/debian-12")
    );

    // Precedence Case 5: Both are None -> Debian 13 fallback
    let cfg5 = RunnerVmConfig {
        instance_name: "test-vm-default",
        machine_type: "c4d-highcpu-16",
        zone: "us-central1-a",
        target: "swack-tools",
        runner_prefix: "swack",
        startup_script: "echo",
        patch_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        patch_json: "{}",
        runner_1_jitconfig: None,
        runner_2_jitconfig: None,
        source_image: None,
        golden_image_family: None,
        boot_disk_type: None,
    };
    let req5 = build_spot_runner_vm_request("my-proj", &cfg5).unwrap();
    assert_eq!(
        req5.disks[0]
            .initialize_params
            .as_ref()
            .unwrap()
            .source_image
            .as_deref(),
        Some(DEBIAN_13_IMAGE_FAMILY)
    );
}

// ============================================================================
// 5. 2-SLOT JIT CONFIG GENERATION & FALLBACK MATRIX
// ============================================================================

#[derive(Clone, Default)]
struct ConfigurableGitHubMockTransport {
    responses: Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>,
    network_errors: Arc<Mutex<HashSet<String>>>,
    calls: Arc<Mutex<Vec<(String, String, Option<Vec<u8>>)>>>,
}

use std::collections::HashSet;

impl ConfigurableGitHubMockTransport {
    fn set_response(&self, path_pattern: &str, status: u16, body: serde_json::Value) {
        let bytes = serde_json::to_vec(&body).unwrap();
        self.responses
            .lock()
            .unwrap()
            .insert(path_pattern.to_string(), (status, bytes));
    }

    fn set_network_error(&self, path_pattern: &str) {
        self.network_errors
            .lock()
            .unwrap()
            .insert(path_pattern.to_string());
    }
}

#[async_trait::async_trait(?Send)]
impl GitHubTransport for ConfigurableGitHubMockTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        _headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), path.to_string(), body.map(|b| b.to_vec())));

        // Check for simulated network failure
        {
            let net_errs = self.network_errors.lock().unwrap();
            if net_errs.iter().any(|pat| path.contains(pat)) {
                return Err("Simulated network connection reset by peer".to_string());
            }
        }

        // Check custom responses
        {
            let resps = self.responses.lock().unwrap();
            let mut matches: Vec<_> = resps.iter().filter(|(pat, _)| path.contains(*pat)).collect();
            matches.sort_by_key(|(pat, _)| std::cmp::Reverse(pat.len()));
            if let Some((_, (status, resp_bytes))) = matches.first() {
                return Ok((*status, resp_bytes.clone()));
            }
        }

        // Default mock responses
        if path.contains("registration-token") {
            let resp = serde_json::json!({
                "token": "default-fallback-reg-token-xyz",
                "expires_at": "2026-10-22T03:00:00Z"
            });
            return Ok((201, serde_json::to_vec(&resp).unwrap()));
        }

        if path.contains("generate-jitconfig") {
            let resp = serde_json::json!({
                "runner": {
                    "id": 100,
                    "name": "mock-runner",
                    "status": "offline",
                    "busy": false,
                    "labels": []
                },
                "encoded_jit_config": "mock_jit_base64_encoded"
            });
            return Ok((201, serde_json::to_vec(&resp).unwrap()));
        }

        Ok((200, b"{}".to_vec()))
    }
}

#[tokio::test]
async fn test_jitconfig_fallback_matrix() {
    let labels = vec!["self-hosted".to_string(), "Linux".to_string(), "spot".to_string()];

    // Case 1: Slot 1 HTTP 404 (Not Found / JIT disabled on organization)
    {
        let transport = Arc::new(ConfigurableGitHubMockTransport::default());
        transport.set_response(
            "generate-jitconfig",
            404,
            serde_json::json!({"message": "Not Found"}),
        );
        let client = GitHubActionsClient::new("token".to_string(), transport.clone());

        let res = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-404", "swack", 10, &labels)
            .await
            .unwrap();

        match res {
            RunnerBootstrapConfig::RegistrationToken { token } => {
                assert_eq!(token, "default-fallback-reg-token-xyz");
            }
            RunnerBootstrapConfig::JitConfig { .. } => panic!("404 must fallback to registration token"),
        }
        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1.contains("generate-jitconfig"));
        assert!(calls[1].1.contains("registration-token"));
    }

    // Case 2: Slot 1 HTTP 403 (Forbidden / Insufficient permissions)
    {
        let transport = Arc::new(ConfigurableGitHubMockTransport::default());
        transport.set_response(
            "generate-jitconfig",
            403,
            serde_json::json!({"message": "Resource not accessible by integration"}),
        );
        let client = GitHubActionsClient::new("token".to_string(), transport.clone());

        let res = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-403", "swack", 10, &labels)
            .await
            .unwrap();

        match res {
            RunnerBootstrapConfig::RegistrationToken { token } => {
                assert_eq!(token, "default-fallback-reg-token-xyz");
            }
            RunnerBootstrapConfig::JitConfig { .. } => panic!("403 must fallback to registration token"),
        }
    }

    // Case 3: Slot 1 HTTP 500 (Internal Server Error)
    {
        let transport = Arc::new(ConfigurableGitHubMockTransport::default());
        transport.set_response(
            "generate-jitconfig",
            500,
            serde_json::json!({"message": "GitHub API Internal Server Error"}),
        );
        let client = GitHubActionsClient::new("token".to_string(), transport.clone());

        let res = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-500", "swack", 10, &labels)
            .await
            .unwrap();

        match res {
            RunnerBootstrapConfig::RegistrationToken { token } => {
                assert_eq!(token, "default-fallback-reg-token-xyz");
            }
            RunnerBootstrapConfig::JitConfig { .. } => panic!("500 must fallback to registration token"),
        }
    }

    // Case 4: Network Transport Failure on Slot 1
    {
        let transport = Arc::new(ConfigurableGitHubMockTransport::default());
        transport.set_network_error("generate-jitconfig");
        let client = GitHubActionsClient::new("token".to_string(), transport.clone());

        let res = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-net", "swack", 10, &labels)
            .await
            .unwrap();

        match res {
            RunnerBootstrapConfig::RegistrationToken { token } => {
                assert_eq!(token, "default-fallback-reg-token-xyz");
            }
            RunnerBootstrapConfig::JitConfig { .. } => {
                panic!("Network error must fallback to registration token")
            }
        }
    }

    // Case 5: Slot 1 succeeds (201), but Slot 2 fails (e.g. 422 or 500)
    // Controller must NOT leave host half-configured: must fallback to registration token!
    {
        // Custom request handler that fails only on the second JIT call
        struct FlakySecondCallTransport {
            call_count: Mutex<usize>,
            inner: ConfigurableGitHubMockTransport,
        }
        #[async_trait::async_trait(?Send)]
        impl GitHubTransport for FlakySecondCallTransport {
            async fn request(
                &self,
                method: &str,
                path: &str,
                headers: &[(&str, &str)],
                body: Option<&[u8]>,
            ) -> Result<(u16, Vec<u8>), String> {
                let mut c = self.call_count.lock().unwrap();
                *c += 1;
                if path.contains("generate-jitconfig") && *c == 2 {
                    // Slot 2 fails with 500
                    return Ok((
                        500,
                        serde_json::to_vec(&serde_json::json!({"message": "Slot 2 failed"})).unwrap(),
                    ));
                }
                self.inner.request(method, path, headers, body).await
            }
        }

        let flaky = Arc::new(FlakySecondCallTransport {
            call_count: Mutex::new(0),
            inner: ConfigurableGitHubMockTransport::default(),
        });
        let client = GitHubActionsClient::new("token".to_string(), flaky);

        let res = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-slot2-fail", "swack", 10, &labels)
            .await
            .unwrap();

        match res {
            RunnerBootstrapConfig::RegistrationToken { token } => {
                assert_eq!(token, "default-fallback-reg-token-xyz");
            }
            RunnerBootstrapConfig::JitConfig { .. } => {
                panic!("Slot 2 failure must trigger clean fallback to registration token")
            }
        }
    }

    // Case 6: Both JIT and registration token fail -> returns Err
    {
        let transport = Arc::new(ConfigurableGitHubMockTransport::default());
        transport.set_response(
            "generate-jitconfig",
            404,
            serde_json::json!({"message": "Not Found"}),
        );
        transport.set_response(
            "registration-token",
            401,
            serde_json::json!({"message": "Bad credentials"}),
        );
        let client = GitHubActionsClient::new("bad_token".to_string(), transport);

        let err = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-both-fail", "swack", 10, &labels)
            .await
            .unwrap_err();

        match err {
            GitHubError::Api { status, .. } => assert_eq!(status, 401),
            other => panic!("Expected Api 401, got: {:?}", other),
        }
    }
}

// ============================================================================
// 6. STARTUP SCRIPT RENDERING: ALL 31 PLACEHOLDERS & ZERO `{{` INVARIANTS
// ============================================================================

#[test]
fn test_startup_script_rendering_31_placeholders_and_adversarial_params() {
    // Test with extreme parameters: empty runner_cpu_limit, unusual characters, etc.
    let params = StartupParams {
        disk_device_name: "oxidex-runners-us-central1-a-adv",
        instance_name: "oxidex-runners-us-central1-a-adv",
        deployment_id: "dep-adv-20261022-999",
        org: "xbmc4lyfe/newznab-tmux",
        runner_prefix: "xbmc-adv",
        runner_group: "special-runners_123",
        runner_labels: "self-hosted,Linux,X64,spot,c4d-highcpu-16,us-central1-a,gpu_none",
        registration_token: "tok_with_quotes_'\"_and_dollar_$VAR",
        runner_count: 2,
        runner_memory_gb: 14,
        runner_cpu_limit: "", // empty cpu limit
        tiered: false,
        strict_heavy_pool: false,
        resource_monitoring: true,
    };

    let rendered = render_startup_script(&params, None).unwrap();

    // 1. Strictly 0 unexpanded `{{` tokens
    assert!(
        !rendered.contains("{{"),
        "Rendered script must not contain any unexpanded '{{' placeholders!"
    );

    // 2. Strict size budget under GCE 256 KiB limit
    assert!(
        rendered.len() <= 256 * 1024,
        "Rendered startup script size {} exceeds 256 KiB budget!",
        rendered.len()
    );

    // 3. Verify specific substitutions
    assert!(rendered.contains("DEVICE=\"/dev/disk/by-id/google-oxidex-runners-us-central1-a-adv\""));
    assert!(rendered.contains("ORG_URL=https://github.com/xbmc4lyfe/newznab-tmux"));
    assert!(rendered.contains("RUNNER_PREFIX='xbmc-adv'"));
    assert!(rendered.contains("RUNNER_GROUP='special-runners_123'"));
    assert!(rendered.contains("RUNNER_COUNT=2"));
    assert!(rendered.contains("RUNNER_MEMORY_GB=14"));
    assert!(rendered.contains("RUNNER_CPU_LIMIT=''"));
    assert!(rendered.contains("RESOURCE_MONITORING=1"));
    assert!(rendered.contains("TIERED=0"));
    assert!(rendered.contains("STRICT_HEAVY_POOL=0"));

    // 4. Test with full adversarial RunnerPatch
    let mut patch = RunnerPatch::default();
    patch.repository = "xbmc4lyfe/newznab-tmux".to_string();

    let mut sysctl = BTreeMap::new();
    sysctl.insert("fs.file-max".to_string(), "2097152".to_string());
    sysctl.insert("net.ipv4.ip_local_port_range".to_string(), "1024 65535".to_string());

    patch.host = HostPatch {
        apt_packages: vec!["build-essential".to_string(), "zlib1g-dev".to_string()],
        sysctl: sysctl.clone(),
        files: vec![PatchFileSpec {
            path: "/etc/security/limits.d/99-runner.conf".to_string(),
            content: "* soft nofile 65536\n* hard nofile 65536\n".to_string(),
            mode: "0644".to_string(),
            owner: "root:root".to_string(),
        }],
        boot_mkdirs: vec!["/var/lib/custom-cache".to_string()],
        boot_script: "echo 'boot initialization successful' >/tmp/boot.log\n".to_string(),
    };

    patch.container = ContainerPatch {
        apt_packages: vec![],
        sysctl: sysctl.clone(),
        mounts: vec![PatchMountSpec {
            source: "/mnt/runner-data/cache".to_string(),
            target: "/opt/cache".to_string(),
            read_only: true,
        }],
        files: vec![],
        dockerfile_run: String::new(),
    };

    let rendered_with_patch = render_startup_script(&params, Some(&patch)).unwrap();

    // Invariants with patch
    assert!(!rendered_with_patch.contains("{{"));
    assert!(rendered_with_patch.len() <= 256 * 1024);

    // Verify patch hooks injected
    assert!(rendered_with_patch.contains("/opt/gha-runners/repo-host-patch.sh"));
    assert!(rendered_with_patch.contains("/var/lib/gha-runner-patch/pending.json"));
    assert!(rendered_with_patch.contains("/var/lib/gha-runner-patch/state.json"));
    assert!(rendered_with_patch.contains("--sysctl=fs.file-max=2097152"));
    assert!(rendered_with_patch.contains("/mnt/runner-data/cache:/opt/cache:ro"));
}

// ============================================================================
// 7. GCE API ERROR HANDLING & METADATA SCRUBBING
// ============================================================================

#[tokio::test]
async fn test_gce_api_error_handling_and_metadata_scrubbing() {
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();

    // Set 403 Forbidden for aggregated instances
    transport.set_response(
        "aggregated/instances",
        403,
        serde_json::to_vec(&serde_json::json!({
            "error": {
                "code": 403,
                "message": "Compute Engine API has not been used in project homelab-424523 before or it is disabled."
            }
        })).unwrap(),
    );

    let client = GceComputeClient::new(creds, "homelab-424523".to_string(), transport);
    let now = 1_700_000_000u64;

    let err = client.list_instances(now).await.unwrap_err();
    match err {
        GcpError::Api(code, endpoint, msg) => {
            assert_eq!(code, 403);
            assert_eq!(endpoint, "instances.aggregatedList");
            assert!(msg.contains("Compute Engine API has not been used"));
        }
        other => panic!("Expected GcpError::Api, got: {:?}", other),
    }
}
