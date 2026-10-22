use base64::Engine;
use spot_dashboard_edge::gcp::*;

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

#[test]
fn test_gcp_credentials_parsing() {
    let json_data = serde_json::json!({
        "client_email": "runner-sa@test-project.iam.gserviceaccount.com",
        "private_key": TEST_PRIVATE_KEY_PEM,
        "project_id": "test-project"
    });
    let creds: GcpCredentials = serde_json::from_value(json_data).unwrap();
    assert_eq!(creds.client_email, "runner-sa@test-project.iam.gserviceaccount.com");
    assert_eq!(creds.token_uri, DEFAULT_TOKEN_URI);
    assert_eq!(creds.project_id.as_deref(), Some("test-project"));
}

#[test]
fn test_deterministic_rs256_assertion_generation() {
    let creds = make_test_credentials();
    let now = 1700000000;

    let assertion1 = mint_rs256_assertion(&creds, now).unwrap();
    let assertion2 = mint_rs256_assertion(&creds, now).unwrap();

    // Deterministic: signing twice with identical inputs must produce identical output
    assert_eq!(assertion1, assertion2);

    let parts: Vec<&str> = assertion1.split('.').collect();
    assert_eq!(parts.len(), 3);

    // Verify header
    let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[0]).unwrap();
    let header: serde_json::Value = serde_json::from_slice(&header_bytes).unwrap();
    assert_eq!(header["alg"], "RS256");
    assert_eq!(header["typ"], "JWT");

    // Verify claims
    let claims_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
    let claims: serde_json::Value = serde_json::from_slice(&claims_bytes).unwrap();
    assert_eq!(claims["iss"], creds.client_email);
    assert_eq!(claims["aud"], creds.token_uri);
    assert_eq!(claims["iat"], now);
    assert_eq!(claims["exp"], now + JWT_EXPIRY_SECS);
    assert!(claims["scope"].as_str().unwrap().contains("https://www.googleapis.com/auth/compute"));
}

#[tokio::test]
async fn test_token_exchange_and_zero_kv_operations() {
    spot_dashboard_edge::state::invalidate_gcp_token_cache();
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    let now = 1700000000;

    // First call: cache miss, exchanges assertion for access token
    let token1 = get_cached_or_fresh_gcp_token(&transport, &creds, now).await.unwrap();
    assert_eq!(token1, "ya29.mock-token-xyz");
    assert_eq!(transport.calls.lock().unwrap().len(), 1);

    // Second call 10 seconds later: cache hit in Wasm isolate memory (0 network calls, 0 KV ops)
    let token2 = get_cached_or_fresh_gcp_token(&transport, &creds, now + 10).await.unwrap();
    assert_eq!(token2, token1);
    assert_eq!(transport.calls.lock().unwrap().len(), 1); // No new network call!
}

#[tokio::test]
async fn test_cache_expiration_at_55_minutes() {
    spot_dashboard_edge::state::invalidate_gcp_token_cache();
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    let now = 1700000000;

    let token1 = get_cached_or_fresh_gcp_token(&transport, &creds, now).await.unwrap();
    assert_eq!(token1, "ya29.mock-token-xyz");

    // Advance by 3299 seconds (< 3300s TTL) -> Still cached
    let token2 = get_cached_or_fresh_gcp_token(&transport, &creds, now + 3299).await.unwrap();
    assert_eq!(token2, token1);
    assert_eq!(transport.calls.lock().unwrap().len(), 1);

    // Advance by 3301 seconds (>= 3300s TTL) -> Cache expired, new network call
    let token3 = get_cached_or_fresh_gcp_token(&transport, &creds, now + 3301).await.unwrap();
    assert_eq!(token3, token1);
    assert_eq!(transport.calls.lock().unwrap().len(), 2);
}

#[test]
fn test_spot_runner_vm_request_builder_permitted_machine_types() {
    let permitted = ["c4d-highcpu-16", "n4d-highcpu-16", "n4-highcpu-16", "c2d-highcpu-16"];
    for mt in permitted {
        let cfg = RunnerVmConfig {
            instance_name: "test-vm-1",
            machine_type: mt,
            zone: "us-central1-a",
            target: "swack-tools",
            runner_prefix: "swack",
            startup_script: "#!/bin/bash\necho hi",
            patch_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            patch_json: "{}",
            runner_1_jitconfig: Some("jit1"),
            runner_2_jitconfig: Some("jit2"),
            source_image: None,
            golden_image_family: None,
            boot_disk_type: Some("pd-balanced"),
        };
        let req = build_spot_runner_vm_request("test-proj", &cfg).unwrap();
        assert_eq!(req.name, "test-vm-1");
        assert_eq!(req.machine_type, format!("zones/us-central1-a/machineTypes/{}", mt));
        assert_eq!(req.scheduling.provisioning_model, "SPOT");
        assert_eq!(req.scheduling.instance_termination_action, "DELETE");

        // Verify metadata items include JIT configs
        assert_eq!(req.metadata.get("runner_1_jitconfig"), Some("jit1"));
        assert_eq!(req.metadata.get("runner_2_jitconfig"), Some("jit2"));
        assert_eq!(req.metadata.get("spot-runner-count"), Some("2"));
    }

    // Shapes under 16 vCPU or not in permitted list must be rejected
    let invalid_shapes = ["n2-standard-4", "e2-medium", "c4d-highcpu-8", "custom-8-32768"];
    for invalid in invalid_shapes {
        let cfg = RunnerVmConfig {
            instance_name: "test-vm-invalid",
            machine_type: invalid,
            zone: "us-central1-a",
            target: "swack-tools",
            runner_prefix: "swack",
            startup_script: "#!/bin/bash\necho hi",
            patch_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            patch_json: "{}",
            runner_1_jitconfig: None,
            runner_2_jitconfig: None,
            source_image: None,
            golden_image_family: None,
            boot_disk_type: None,
        };
        let err = build_spot_runner_vm_request("test-proj", &cfg).unwrap_err();
        match err {
            GcpError::Validation(msg) => {
                assert!(msg.contains("is not in permitted list"));
            }
            other => panic!("Expected GcpError::Validation, got: {:?}", other),
        }
    }
}

#[test]
fn test_golden_boot_disk_resolution() {
    // 1. Explicit source image
    let cfg1 = RunnerVmConfig {
        instance_name: "test-vm",
        machine_type: "c4d-highcpu-16",
        zone: "us-central1-a",
        target: "swack-tools",
        runner_prefix: "swack",
        startup_script: "echo",
        patch_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        patch_json: "{}",
        runner_1_jitconfig: None,
        runner_2_jitconfig: None,
        source_image: Some("my-custom-image-v1"),
        golden_image_family: None,
        boot_disk_type: None,
    };
    let req1 = build_spot_runner_vm_request("my-proj", &cfg1).unwrap();
    assert_eq!(
        req1.disks[0].initialize_params.as_ref().unwrap().source_image.as_deref(),
        Some("projects/my-proj/global/images/my-custom-image-v1")
    );

    // 2. Golden image family
    let cfg2 = RunnerVmConfig {
        instance_name: "test-vm",
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
        golden_image_family: Some("debian-13-fastboot"),
        boot_disk_type: None,
    };
    let req2 = build_spot_runner_vm_request("my-proj", &cfg2).unwrap();
    assert_eq!(
        req2.disks[0].initialize_params.as_ref().unwrap().source_image.as_deref(),
        Some("projects/my-proj/global/images/family/debian-13-fastboot")
    );

    // 3. Clean Debian 13 fallback when both are None
    let cfg3 = RunnerVmConfig {
        instance_name: "test-vm",
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
    let req3 = build_spot_runner_vm_request("my-proj", &cfg3).unwrap();
    assert_eq!(
        req3.disks[0].initialize_params.as_ref().unwrap().source_image.as_deref(),
        Some(DEBIAN_13_IMAGE_FAMILY)
    );
}

#[tokio::test]
async fn test_instances_insert_with_source_instance_template() {
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    let client = GceComputeClient::new(creds, "homelab-424523".to_string(), transport.clone());
    let now = 1700000000;

    let cfg = RunnerVmConfig {
        instance_name: "oxidex-runners-us-central1-a-test",
        machine_type: "c4d-highcpu-16",
        zone: "us-central1-a",
        target: "swack-tools",
        runner_prefix: "oxidex",
        startup_script: "#!/bin/bash\nexit 0",
        patch_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        patch_json: "{}",
        runner_1_jitconfig: None,
        runner_2_jitconfig: None,
        source_image: None,
        golden_image_family: None,
        boot_disk_type: None,
    };
    let req = build_spot_runner_vm_request("homelab-424523", &cfg).unwrap();

    // 1. Insert with sourceInstanceTemplate
    client.insert_instance("us-central1-a", &req, Some("c4d-template-v1"), now).await.unwrap();

    let calls = transport.calls.lock().unwrap();
    let last_call = calls.last().unwrap();
    assert_eq!(last_call.0, "POST");
    assert!(last_call.1.contains("/zones/us-central1-a/instances?sourceInstanceTemplate="));
    assert!(last_call.1.contains("projects%2Fhomelab-424523%2Fglobal%2FinstanceTemplates%2Fc4d-template-v1"));
}

#[tokio::test]
async fn test_instances_delete_includes_delete_disk_all() {
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    let client = GceComputeClient::new(creds, "homelab-424523".to_string(), transport.clone());
    let now = 1700000000;

    client.delete_instance("us-central1-a", "oxidex-runners-us-central1-a-old", now).await.unwrap();

    let calls = transport.calls.lock().unwrap();
    let last_call = calls.last().unwrap();
    assert_eq!(last_call.0, "DELETE");
    assert!(last_call.1.ends_with("/zones/us-central1-a/instances/oxidex-runners-us-central1-a-old?deleteDisk=ALL"));
}

#[tokio::test]
async fn test_instances_scrub_startup_script() {
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    let client = GceComputeClient::new(creds, "homelab-424523".to_string(), transport.clone());
    let now = 1700000000;

    let metadata = GceMetadata {
        fingerprint: Some("fingerprint-xyz123".to_string()),
        items: vec![
            GceMetadataItem { key: "startup-script".to_string(), value: "sensitive-token-12345".to_string() },
            GceMetadataItem { key: "spot-target".to_string(), value: "swack-tools".to_string() },
            GceMetadataItem { key: "runner_1_jitconfig".to_string(), value: "jit-base64".to_string() },
        ],
    };

    client.scrub_startup_script("us-central1-a", "oxidex-runners-us-central1-a-1", &metadata, now).await.unwrap();

    let calls = transport.calls.lock().unwrap();
    let last_call = calls.last().unwrap();
    assert_eq!(last_call.0, "POST");
    assert!(last_call.1.ends_with("/zones/us-central1-a/instances/oxidex-runners-us-central1-a-1/setMetadata"));

    let body_bytes = last_call.2.as_ref().unwrap();
    let scrubbed: GceMetadata = serde_json::from_slice(body_bytes).unwrap();

    // Verify startup-script is scrubbed
    assert_eq!(scrubbed.get("startup-script"), None);
    // Verify other metadata and fingerprint are preserved
    assert_eq!(scrubbed.fingerprint.as_deref(), Some("fingerprint-xyz123"));
    assert_eq!(scrubbed.get("spot-target"), Some("swack-tools"));
    assert_eq!(scrubbed.get("runner_1_jitconfig"), Some("jit-base64"));
}

#[tokio::test]
async fn test_orphan_disks_detection() {
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();

    let mock_disks_page = serde_json::json!({
        "items": {
            "zones/us-central1-a": {
                "disks": [
                    {
                        "name": "oxidex-runners-us-central1-a-orphan1",
                        "zone": "zones/us-central1-a",
                        "users": [] // Empty users => orphan!
                    },
                    {
                        "name": "oxidex-runners-us-central1-a-attached",
                        "zone": "zones/us-central1-a",
                        "users": ["projects/p/zones/z/instances/vm1"] // Attached => not orphan
                    },
                    {
                        "name": "other-disk-unrelated",
                        "zone": "zones/us-central1-a",
                        "users": [] // Unrelated prefix => ignored
                    }
                ]
            }
        }
    });

    transport.set_json_response("aggregated/disks", 200, &mock_disks_page);

    let client = GceComputeClient::new(creds, "homelab-424523".to_string(), transport.clone());
    let now = 1700000000;

    let orphans = client.list_orphan_runner_disks(now).await.unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].name, "oxidex-runners-us-central1-a-orphan1");
}

#[tokio::test]
async fn test_instances_aggregated_list_pagination() {
    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();

    let page1 = serde_json::json!({
        "items": {
            "zones/us-central1-a": {
                "instances": [
                    {
                        "name": "oxidex-runners-us-central1-a-1",
                        "zone": "zones/us-central1-a",
                        "machineType": "c4d-highcpu-16",
                        "status": "RUNNING"
                    }
                ]
            }
        },
        "nextPageToken": "page-2-token"
    });

    let page2 = serde_json::json!({
        "items": {
            "zones/us-east1-b": {
                "instances": [
                    {
                        "name": "oxidex-runners-us-east1-b-1",
                        "zone": "zones/us-east1-b",
                        "machineType": "n4d-highcpu-16",
                        "status": "RUNNING"
                    }
                ]
            }
        }
    });

    transport.set_json_response("maxResults=500", 200, &page1);
    transport.set_json_response("pageToken=page-2-token", 200, &page2);

    let client = GceComputeClient::new(creds, "homelab-424523".to_string(), transport);
    let now = 1700000000;

    let instances = client.list_instances(now).await.unwrap();
    assert_eq!(instances.len(), 2);
    assert_eq!(instances[0].name, "oxidex-runners-us-central1-a-1");
    assert_eq!(instances[1].name, "oxidex-runners-us-east1-b-1");
}
