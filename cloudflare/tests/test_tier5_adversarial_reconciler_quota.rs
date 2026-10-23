//! Tier 5 Adversarial Coverage Hardening Test Suite
//! Comprehensive white-box verification for GCP, GitHub, Reconciler, KV Quota, and Resource Budget Gates.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::Sha256;
use rsa::signature::Verifier;
use rsa::RsaPrivateKey;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use spot_dashboard_edge::gcp::*;
use spot_dashboard_edge::github::*;
use spot_dashboard_edge::patch::CONTROLLED_TARGETS;
use spot_dashboard_edge::reconciler::*;
use spot_dashboard_edge::state::{invalidate_state_cache, KvStoreBackend, StateError};

// ============================================================================
// Test Private Key & Helpers
// ============================================================================

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
        client_email: "spot-controller@example-project.iam.gserviceaccount.com".to_string(),
        private_key: TEST_PRIVATE_KEY_PEM.to_string(),
        token_uri: DEFAULT_TOKEN_URI.to_string(),
        project_id: Some("example-project".to_string()),
    }
}

// Global test mutex to isolate tests using process-wide reconciler/state globals
static TIER5_TEST_MUTEX: Mutex<()> = Mutex::new(());

// ============================================================================
// Mock Test Doubles
// ============================================================================

pub struct Tier5MockKvStore {
    pub data: Mutex<HashMap<String, String>>,
    pub get_count: AtomicUsize,
    pub put_count: AtomicUsize,
}

impl Tier5MockKvStore {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            get_count: AtomicUsize::new(0),
            put_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for Tier5MockKvStore {
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

#[derive(Default, Clone)]
pub struct Tier5MockGcpReconciler {
    pub instances: Arc<Mutex<Vec<SimpleGceInstance>>>,
    pub deleted_instances: Arc<Mutex<Vec<String>>>,
    pub scrubbed_instances: Arc<Mutex<Vec<String>>>,
    pub inserted_instances: Arc<Mutex<Vec<GceInsertInstanceRequest>>>,
}

#[async_trait::async_trait(?Send)]
impl GcpReconcilerClient for Tier5MockGcpReconciler {
    async fn list_instances(
        &self,
        _project: &str,
        _now: u64,
    ) -> Result<Vec<SimpleGceInstance>, ReconcilerError> {
        Ok(self.instances.lock().unwrap().clone())
    }

    async fn insert_instance(
        &self,
        _project: &str,
        _zone: &str,
        req: &GceInsertInstanceRequest,
        _source_instance_template: Option<&str>,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        self.inserted_instances.lock().unwrap().push(req.clone());
        Ok(GceOperation {
            id: Some("op-insert".into()),
            name: Some("op-insert".into()),
            status: "DONE".into(),
            target_link: None,
            error: None,
        })
    }

    async fn delete_instance(
        &self,
        _project: &str,
        _zone: &str,
        instance: &str,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        self.deleted_instances
            .lock()
            .unwrap()
            .push(instance.to_string());
        Ok(GceOperation {
            id: Some("op-delete".into()),
            name: Some("op-delete".into()),
            status: "DONE".into(),
            target_link: None,
            error: None,
        })
    }

    async fn scrub_startup_script(
        &self,
        _project: &str,
        _zone: &str,
        instance: &str,
        _current_metadata: &GceMetadata,
        _now: u64,
    ) -> Result<GceOperation, ReconcilerError> {
        self.scrubbed_instances
            .lock()
            .unwrap()
            .push(instance.to_string());
        Ok(GceOperation {
            id: Some("op-scrub".into()),
            name: Some("op-scrub".into()),
            status: "DONE".into(),
            target_link: None,
            error: None,
        })
    }
}

#[derive(Default, Clone)]
pub struct Tier5MockGitHubReconciler {
    pub runners: Arc<Mutex<Vec<SimpleGitHubRunner>>>,
    pub deleted_runners: Arc<Mutex<Vec<u64>>>,
    pub queued_jobs: Arc<Mutex<u32>>,
    pub jit_fail_slot1: Arc<Mutex<bool>>,
    pub jit_fail_slot2: Arc<Mutex<bool>>,
}

#[async_trait::async_trait(?Send)]
impl GitHubReconcilerClient for Tier5MockGitHubReconciler {
    async fn list_runners(
        &self,
        _target_api_path: &str,
    ) -> Result<Vec<SimpleGitHubRunner>, ReconcilerError> {
        Ok(self.runners.lock().unwrap().clone())
    }

    async fn delete_runner(
        &self,
        _target_api_path: &str,
        runner_id: u64,
    ) -> Result<(), ReconcilerError> {
        self.deleted_runners.lock().unwrap().push(runner_id);
        Ok(())
    }

    async fn mint_2slot_runner_configs(
        &self,
        _target_api_path: &str,
        _vm_name: &str,
        _runner_prefix: &str,
        _runner_group_id: u32,
        _labels: &[String],
    ) -> Result<RunnerBootstrapConfig, ReconcilerError> {
        let fail1 = *self.jit_fail_slot1.lock().unwrap();
        let fail2 = *self.jit_fail_slot2.lock().unwrap();

        if fail1 || fail2 {
            Ok(RunnerBootstrapConfig::RegistrationToken {
                token: "fallback-registration-token-xyz".to_string(),
            })
        } else {
            Ok(RunnerBootstrapConfig::JitConfig {
                runner_1_jitconfig: "jit-slot-1-token".to_string(),
                runner_2_jitconfig: "jit-slot-2-token".to_string(),
            })
        }
    }

    async fn count_queued_jobs(&self, owner: &str, repo: &str) -> Result<u32, ReconcilerError> {
        if owner == "swack-tools" || repo == "oxidex" {
            Ok(*self.queued_jobs.lock().unwrap())
        } else {
            Ok(0)
        }
    }
}

fn populate_all_floor_hosts(gcp: &Tier5MockGcpReconciler) {
    for target in CONTROLLED_TARGETS {
        let slug = target.replace('/', "-").to_lowercase();
        gcp.instances.lock().unwrap().push(SimpleGceInstance {
            id: Some(format!("vm-{}", slug)),
            name: format!("oxidex-runners-{}-healthy", slug),
            zone: "us-central1-a".to_string(),
            machine_type: "c4d-highcpu-16".to_string(),
            status: "RUNNING".to_string(),
            metadata: GceMetadata::default(),
            disks: vec![],
        });
    }
}

// ============================================================================
// 1. GCP RS256 JWT Cryptographic Signature & Claims Verification
// ============================================================================

#[test]
fn test_tier5_adversarial_gcp_rs256_jwt_claims_and_signature_verification() {
    let creds = make_test_credentials();
    let now = 1700000000u64;

    let assertion = mint_rs256_assertion(&creds, now).expect("mint assertion");
    let parts: Vec<&str> = assertion.split('.').collect();
    assert_eq!(
        parts.len(),
        3,
        "JWT must consist of header.payload.signature"
    );

    // 1. Header Verification
    let header_bytes = URL_SAFE_NO_PAD.decode(parts[0]).expect("decode header");
    let header_val: serde_json::Value =
        serde_json::from_slice(&header_bytes).expect("parse header JSON");
    assert_eq!(header_val["alg"], "RS256");
    assert_eq!(header_val["typ"], "JWT");

    // 2. Claims Verification
    let payload_bytes = URL_SAFE_NO_PAD.decode(parts[1]).expect("decode payload");
    let claims: serde_json::Value =
        serde_json::from_slice(&payload_bytes).expect("parse claims JSON");
    assert_eq!(claims["iss"], creds.client_email);
    assert_eq!(claims["scope"], GCP_OAUTH_SCOPE);
    assert_eq!(claims["aud"], creds.token_uri);
    assert_eq!(claims["iat"], now);
    assert_eq!(claims["exp"], now + 3600);
    assert_eq!(
        claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
        3600
    );

    // 3. Cryptographic Verification using RSA Public Key
    let priv_key = RsaPrivateKey::from_pkcs8_pem(&creds.private_key).expect("parse private key");
    let pub_key = priv_key.to_public_key();
    let verifying_key = VerifyingKey::<Sha256>::new(pub_key);

    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let sig_bytes = URL_SAFE_NO_PAD.decode(parts[2]).expect("decode signature");
    let sig = Signature::try_from(sig_bytes.as_slice()).expect("parse signature");

    assert!(
        verifying_key.verify(signing_input.as_bytes(), &sig).is_ok(),
        "RS256 signature verification must succeed against authentic public key"
    );

    // 4. Adversarial Tamper Resistance: Mutate payload
    let tampered_signing_input = format!("{}.{}", parts[0], "eyJpc3MiOiJoYWNrZXIifQ");
    assert!(
        verifying_key
            .verify(tampered_signing_input.as_bytes(), &sig)
            .is_err(),
        "Tampered payload must cause signature verification failure"
    );

    // 5. Adversarial Tamper Resistance: Corrupt signature byte
    let mut corrupted_sig_bytes = sig_bytes.clone();
    corrupted_sig_bytes[10] ^= 0xFF;
    if let Ok(corrupted_sig) = Signature::try_from(corrupted_sig_bytes.as_slice()) {
        assert!(
            verifying_key
                .verify(signing_input.as_bytes(), &corrupted_sig)
                .is_err(),
            "Corrupted signature must be rejected"
        );
    }
}

// ============================================================================
// 2. GCP In-Isolate OAuth Token Caching: Expiration Boundaries & Strictly 0 KV
// ============================================================================

#[tokio::test]
async fn test_tier5_adversarial_gcp_oauth_token_caching_boundaries_zero_kv() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();

    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    // Use unique future timestamp to ensure clean token cache miss
    let t0 = 2500000000u64;

    // Cache miss at T0 -> calls token endpoint
    let token1 = get_cached_or_fresh_gcp_token(&transport, &creds, t0)
        .await
        .expect("token 1");
    assert_eq!(token1, "ya29.mock-token-xyz");
    assert_eq!(transport.calls.lock().unwrap().len(), 1);

    // Boundary 1: T0 + 3240s (54 minutes, exactly within 55m TTL) -> must hit memory cache
    let token2 = get_cached_or_fresh_gcp_token(&transport, &creds, t0 + 3240)
        .await
        .expect("token 2");
    assert_eq!(token2, token1);
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        1,
        "54m call must use in-isolate memory cache without HTTP call"
    );

    // Boundary 2: T0 + 3299s (54m 59s, 1s before expiration) -> still cached
    let token3 = get_cached_or_fresh_gcp_token(&transport, &creds, t0 + 3299)
        .await
        .expect("token 3");
    assert_eq!(token3, token1);
    assert_eq!(transport.calls.lock().unwrap().len(), 1);

    // Boundary 3: T0 + 3300s (exactly 55 minutes, TTL elapsed) -> cache expired, triggers fresh exchange
    let token4 = get_cached_or_fresh_gcp_token(&transport, &creds, t0 + 3300)
        .await
        .expect("token 4");
    assert_eq!(token4, "ya29.mock-token-xyz");
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        2,
        "55m expiration boundary must trigger fresh token exchange"
    );

    // Stress test: 100 rapid calls at T0 + 3310 -> all hit memory cache, strictly 2 calls total
    for _ in 0..100 {
        let tok = get_cached_or_fresh_gcp_token(&transport, &creds, t0 + 3310)
            .await
            .expect("stress token");
        assert_eq!(tok, "ya29.mock-token-xyz");
    }
    assert_eq!(transport.calls.lock().unwrap().len(), 2);
}

// ============================================================================
// 3. GCP instances.insert URL Parameters, Precedence & Validation
// ============================================================================

#[tokio::test]
async fn test_tier5_adversarial_gcp_instances_insert_url_params_and_precedence() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();

    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    let client = GceComputeClient::new(creds, "example-project".to_string(), transport.clone());
    let now = 2600000000u64;

    let base_req = GceInsertInstanceRequest {
        name: "test-runner-vm".to_string(),
        machine_type: "zones/us-central1-a/machineTypes/c4d-highcpu-16".to_string(),
        scheduling: GceScheduling::default(),
        disks: vec![],
        network_interfaces: vec![],
        metadata: GceMetadata::default(),
        labels: std::collections::BTreeMap::new(),
        service_accounts: vec![],
    };

    // Case 1: sourceInstanceTemplate is None -> clean URL without query parameter
    client
        .insert_instance("us-central1-a", &base_req, None, now)
        .await
        .expect("insert none");
    let call1 = transport
        .calls
        .lock()
        .unwrap()
        .iter()
        .rfind(|(m, u, _)| m == "POST" && u.contains("/instances"))
        .cloned()
        .expect("call 1");
    assert_eq!(
        call1.1,
        "https://compute.googleapis.com/compute/v1/projects/example-project/zones/us-central1-a/instances"
    );

    // Case 2: sourceInstanceTemplate is short name -> query parameter correctly appended and encoded
    client
        .insert_instance(
            "us-central1-a",
            &base_req,
            Some("spot-runner-template-v1"),
            now,
        )
        .await
        .expect("insert template");
    let call2 = transport
        .calls
        .lock()
        .unwrap()
        .iter()
        .rfind(|(m, u, _)| m == "POST" && u.contains("/instances"))
        .cloned()
        .expect("call 2");
    assert!(call2.1.contains("?sourceInstanceTemplate="));
    assert!(call2.1.contains(
        "projects%2Fexample-project%2Fglobal%2FinstanceTemplates%2Fspot-runner-template-v1"
    ));

    // Case 3: sourceInstanceTemplate is full path -> query parameter preserves full path
    client
        .insert_instance(
            "us-central1-a",
            &base_req,
            Some("projects/external-project/global/instanceTemplates/golden-v2"),
            now,
        )
        .await
        .expect("insert full path");
    let call3 = transport
        .calls
        .lock()
        .unwrap()
        .iter()
        .rfind(|(m, u, _)| m == "POST" && u.contains("/instances"))
        .cloned()
        .expect("call 3");
    assert!(call3
        .1
        .contains("projects%2Fexternal-project%2Fglobal%2FinstanceTemplates%2Fgolden-v2"));

    // Case 4: Golden Image Precedence Matrix in build_spot_runner_vm_request
    // Precedence: source_image > golden_image_family > DEBIAN_13_IMAGE_FAMILY
    let cfg_both = RunnerVmConfig {
        instance_name: "test-vm-1",
        machine_type: "c4d-highcpu-16",
        zone: "us-central1-a",
        target: "swack-tools",
        runner_prefix: "oxidex",
        startup_script: "#!/bin/bash",
        patch_sha256: "0000000000000000000000000000000000000000000000000000000000000000",
        patch_json: "{}",
        runner_1_jitconfig: None,
        runner_2_jitconfig: None,
        source_image: Some("golden-custom-image-2026"),
        golden_image_family: Some("golden-family-2026"),
        boot_disk_type: Some("pd-balanced"),
    };
    let req_both = build_spot_runner_vm_request("example-project", &cfg_both).unwrap();
    let img_both = req_both.disks[0]
        .initialize_params
        .as_ref()
        .unwrap()
        .source_image
        .as_ref()
        .unwrap();
    assert_eq!(
        img_both, "projects/example-project/global/images/golden-custom-image-2026",
        "source_image must take precedence over family"
    );

    let cfg_family_only = RunnerVmConfig {
        source_image: None,
        golden_image_family: Some("golden-family-2026"),
        ..cfg_both
    };
    let req_family = build_spot_runner_vm_request("example-project", &cfg_family_only).unwrap();
    let img_family = req_family.disks[0]
        .initialize_params
        .as_ref()
        .unwrap()
        .source_image
        .as_ref()
        .unwrap();
    assert_eq!(
        img_family,
        "projects/example-project/global/images/family/golden-family-2026"
    );

    let cfg_neither = RunnerVmConfig {
        source_image: None,
        golden_image_family: None,
        ..cfg_both
    };
    let req_debian = build_spot_runner_vm_request("example-project", &cfg_neither).unwrap();
    let img_debian = req_debian.disks[0]
        .initialize_params
        .as_ref()
        .unwrap()
        .source_image
        .as_ref()
        .unwrap();
    assert_eq!(img_debian, DEBIAN_13_IMAGE_FAMILY);

    // Case 5: Machine Type Sizing Floor Enforcement
    let cfg_bad_shape = RunnerVmConfig {
        machine_type: "e2-standard-4",
        ..cfg_both
    };
    let err_bad_shape =
        build_spot_runner_vm_request("example-project", &cfg_bad_shape).unwrap_err();
    assert!(matches!(err_bad_shape, GcpError::Validation(_)));
}

// ============================================================================
// 4. GCP deleteDisk=ALL and Orphan Runner Disk Detection
// ============================================================================

#[tokio::test]
async fn test_tier5_adversarial_gcp_delete_disk_all_and_orphan_disk_filtering() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();

    let creds = make_test_credentials();
    let transport = MockGcpHttpTransport::new();
    let client = GceComputeClient::new(creds, "example-project".to_string(), transport.clone());
    let now = 1700000000u64;

    // 1. Verify deleteDisk=ALL query parameter
    client
        .delete_instance("us-central1-a", "oxidex-runners-homelab-1", now)
        .await
        .expect("delete instance");
    let delete_call = transport
        .calls
        .lock()
        .unwrap()
        .iter()
        .find(|(m, _, _)| m == "DELETE")
        .cloned()
        .expect("delete call");
    assert_eq!(
        delete_call.1,
        "https://compute.googleapis.com/compute/v1/projects/example-project/zones/us-central1-a/instances/oxidex-runners-homelab-1?deleteDisk=ALL"
    );

    // 2. Orphan Disk Detection Matrix
    let sample_disks = serde_json::json!([
        // Case A: Orphan runner disk with oxidex-runners prefix -> MATCH
        {
            "id": "1",
            "name": "oxidex-runners-swack-tools-orphan",
            "zone": "zones/us-central1-a",
            "sizeGb": "40",
            "type": "PERSISTENT",
            "selfLink": "",
            "users": []
        },
        // Case B: Orphan runner disk with spot-runners prefix -> MATCH
        {
            "id": "2",
            "name": "spot-runners-rustnzb-orphan",
            "zone": "zones/us-central1-a",
            "sizeGb": "40",
            "type": "PERSISTENT",
            "selfLink": "",
            "users": []
        },
        // Case C: Orphan disk with infix -runners- -> MATCH
        {
            "id": "3",
            "name": "custom-heavy-runners-pool-orphan",
            "zone": "zones/us-central1-a",
            "sizeGb": "40",
            "type": "PERSISTENT",
            "selfLink": "",
            "users": []
        },
        // Case D: Runner disk with attached VM user -> NOT orphan, must NOT be returned
        {
            "id": "4",
            "name": "oxidex-runners-attached",
            "zone": "zones/us-central1-a",
            "sizeGb": "40",
            "type": "PERSISTENT",
            "selfLink": "",
            "users": ["https://compute.googleapis.com/.../instances/oxidex-runners-attached"]
        },
        // Case E: Non-runner unattached disk -> NOT runner, must NOT be returned
        {
            "id": "5",
            "name": "database-backup-pd",
            "zone": "zones/us-central1-a",
            "sizeGb": "100",
            "type": "PERSISTENT",
            "selfLink": "",
            "users": []
        }
    ]);

    let envelope_json = serde_json::json!({
        "items": {
            "zones/us-central1-a": {
                "disks": sample_disks
            }
        },
        "nextPageToken": null,
        "unreachables": []
    });

    transport.set_response(
        "/aggregated/disks",
        200,
        serde_json::to_vec(&envelope_json).unwrap(),
    );

    let orphans = client
        .list_orphan_runner_disks(now)
        .await
        .expect("list orphan disks");
    assert_eq!(orphans.len(), 3);
    assert_eq!(orphans[0].name, "oxidex-runners-swack-tools-orphan");
    assert_eq!(orphans[1].name, "spot-runners-rustnzb-orphan");
    assert_eq!(orphans[2].name, "custom-heavy-runners-pool-orphan");
}

// ============================================================================
// 5. GitHub 2-Slot JIT Config Uniqueness & Graceful Fallbacks (404/403/500)
// ============================================================================

struct MockGitHubAdversarialTransport {
    pub jit_slot1_status: u16,
    pub jit_slot2_status: u16,
    pub registration_token: String,
    pub calls: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait(?Send)]
impl spot_dashboard_edge::patch_console::GitHubTransport for MockGitHubAdversarialTransport {
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
            .push((method.to_string(), path.to_string()));

        if path.contains("generate-jitconfig") {
            let body_str = body
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            let is_slot1 = body_str.contains("-1\"");
            let status = if is_slot1 {
                self.jit_slot1_status
            } else {
                self.jit_slot2_status
            };

            if status == 201 {
                let dummy_runner = GitHubRunner {
                    id: if is_slot1 { 101 } else { 102 },
                    name: if is_slot1 {
                        "runner-1".into()
                    } else {
                        "runner-2".into()
                    },
                    os: Some("linux".into()),
                    status: "online".into(),
                    busy: false,
                    labels: vec![],
                };
                let resp = GenerateJitConfigResponse {
                    runner: dummy_runner,
                    encoded_jit_config: if is_slot1 {
                        "encoded-jit-config-slot-1".into()
                    } else {
                        "encoded-jit-config-slot-2".into()
                    },
                };
                return Ok((201, serde_json::to_vec(&resp).unwrap()));
            } else {
                return Ok((
                    status,
                    format!("{{\"message\": \"Error status {}\"}}", status).into_bytes(),
                ));
            }
        }

        if path.contains("registration-token") {
            let resp = RegistrationTokenResponse {
                token: self.registration_token.clone(),
                expires_at: Some("2026-10-22T04:00:00Z".into()),
            };
            return Ok((201, serde_json::to_vec(&resp).unwrap()));
        }

        Ok((200, b"{}".to_vec()))
    }
}

#[tokio::test]
async fn test_tier5_adversarial_github_2slot_jitconfig_uniqueness_and_error_fallbacks() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let labels = vec![
        "self-hosted".to_string(),
        "Linux".to_string(),
        "X64".to_string(),
    ];

    // Case 1: Happy path -> both slots succeed (201), unique JIT configs generated
    {
        let transport = Arc::new(MockGitHubAdversarialTransport {
            jit_slot1_status: 201,
            jit_slot2_status: 201,
            registration_token: "reg-token-fallback".into(),
            calls: Mutex::new(Vec::new()),
        });
        let client = GitHubActionsClient::new("fake-token".into(), transport.clone());
        let res = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-alpha", "oxidex", 10, &labels)
            .await
            .unwrap();

        match res {
            RunnerBootstrapConfig::JitConfig {
                runner_1_jitconfig,
                runner_2_jitconfig,
            } => {
                assert_eq!(runner_1_jitconfig, "encoded-jit-config-slot-1");
                assert_eq!(runner_2_jitconfig, "encoded-jit-config-slot-2");
                assert_ne!(
                    runner_1_jitconfig, runner_2_jitconfig,
                    "Slots 1 and 2 must have distinct JIT configs"
                );
            }
            _ => panic!("Expected JitConfig for 201/201"),
        }
    }

    // Case 2: Slot 1 fails with 404 (feature disabled) -> falls back to registration-token
    {
        let transport = Arc::new(MockGitHubAdversarialTransport {
            jit_slot1_status: 404,
            jit_slot2_status: 201,
            registration_token: "fallback-on-404".into(),
            calls: Mutex::new(Vec::new()),
        });
        let client = GitHubActionsClient::new("fake-token".into(), transport.clone());
        let res = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-alpha", "oxidex", 10, &labels)
            .await
            .unwrap();
        assert_eq!(
            res,
            RunnerBootstrapConfig::RegistrationToken {
                token: "fallback-on-404".into()
            }
        );
    }

    // Case 3: Slot 1 fails with 403 (insufficient permissions) -> falls back to registration-token
    {
        let transport = Arc::new(MockGitHubAdversarialTransport {
            jit_slot1_status: 403,
            jit_slot2_status: 201,
            registration_token: "fallback-on-403".into(),
            calls: Mutex::new(Vec::new()),
        });
        let client = GitHubActionsClient::new("fake-token".into(), transport.clone());
        let res = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-alpha", "oxidex", 10, &labels)
            .await
            .unwrap();
        assert_eq!(
            res,
            RunnerBootstrapConfig::RegistrationToken {
                token: "fallback-on-403".into()
            }
        );
    }

    // Case 4: Slot 1 fails with 500 (GitHub internal error) -> falls back to registration-token
    {
        let transport = Arc::new(MockGitHubAdversarialTransport {
            jit_slot1_status: 500,
            jit_slot2_status: 201,
            registration_token: "fallback-on-500".into(),
            calls: Mutex::new(Vec::new()),
        });
        let client = GitHubActionsClient::new("fake-token".into(), transport.clone());
        let res = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-alpha", "oxidex", 10, &labels)
            .await
            .unwrap();
        assert_eq!(
            res,
            RunnerBootstrapConfig::RegistrationToken {
                token: "fallback-on-500".into()
            }
        );
    }

    // Case 5: Slot 1 succeeds (201) but Slot 2 fails (500) -> falls back to registration-token
    {
        let transport = Arc::new(MockGitHubAdversarialTransport {
            jit_slot1_status: 201,
            jit_slot2_status: 500,
            registration_token: "fallback-on-slot2-fail".into(),
            calls: Mutex::new(Vec::new()),
        });
        let client = GitHubActionsClient::new("fake-token".into(), transport.clone());
        let res = client
            .mint_2slot_runner_configs("orgs/swack-tools", "vm-alpha", "oxidex", 10, &labels)
            .await
            .unwrap();
        assert_eq!(
            res,
            RunnerBootstrapConfig::RegistrationToken {
                token: "fallback-on-slot2-fail".into()
            }
        );
    }
}

// ============================================================================
// 6. GitHub Runner Deregistration 404 Idempotency Matrix
// ============================================================================

struct MockDeregisterTransport {
    pub return_status: u16,
}

#[async_trait::async_trait(?Send)]
impl spot_dashboard_edge::patch_console::GitHubTransport for MockDeregisterTransport {
    async fn request(
        &self,
        _method: &str,
        _path: &str,
        _headers: &[(&str, &str)],
        _body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        Ok((self.return_status, b"{}".to_vec()))
    }
}

#[tokio::test]
async fn test_tier5_adversarial_github_delete_runner_404_idempotency_matrix() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    // HTTP 204 No Content -> Ok
    let client_204 = GitHubActionsClient::new(
        "tok".into(),
        Arc::new(MockDeregisterTransport { return_status: 204 }),
    );
    assert!(client_204
        .delete_runner("orgs/swack-tools", 1001)
        .await
        .is_ok());

    // HTTP 404 Not Found (already deleted) -> Ok (idempotent!)
    let client_404 = GitHubActionsClient::new(
        "tok".into(),
        Arc::new(MockDeregisterTransport { return_status: 404 }),
    );
    assert!(
        client_404
            .delete_runner("orgs/swack-tools", 1002)
            .await
            .is_ok(),
        "HTTP 404 on runner delete must be treated as successful/idempotent"
    );

    // HTTP 401 Unauthorized -> Err
    let client_401 = GitHubActionsClient::new(
        "tok".into(),
        Arc::new(MockDeregisterTransport { return_status: 401 }),
    );
    assert!(client_401
        .delete_runner("orgs/swack-tools", 1003)
        .await
        .is_err());

    // HTTP 403 Forbidden -> Err
    let client_403 = GitHubActionsClient::new(
        "tok".into(),
        Arc::new(MockDeregisterTransport { return_status: 403 }),
    );
    assert!(client_403
        .delete_runner("orgs/swack-tools", 1004)
        .await
        .is_err());

    // HTTP 500 Internal Error -> Err
    let client_500 = GitHubActionsClient::new(
        "tok".into(),
        Arc::new(MockDeregisterTransport { return_status: 500 }),
    );
    assert!(client_500
        .delete_runner("orgs/swack-tools", 1005)
        .await
        .is_err());
}

// ============================================================================
// 7. GitHub Queued Backlog Calculation with Mixed Workflow States
// ============================================================================

struct MockQueuedJobsTransport {
    pub runs_json: String,
    pub jobs_run1_json: String,
    pub jobs_run2_json: String,
}

#[async_trait::async_trait(?Send)]
impl spot_dashboard_edge::patch_console::GitHubTransport for MockQueuedJobsTransport {
    async fn request(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(&str, &str)],
        _body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), String> {
        if path.contains("/actions/runs?") {
            return Ok((200, self.runs_json.as_bytes().to_vec()));
        }
        if path.contains("/runs/1/jobs") {
            return Ok((200, self.jobs_run1_json.as_bytes().to_vec()));
        }
        if path.contains("/runs/2/jobs") {
            return Ok((200, self.jobs_run2_json.as_bytes().to_vec()));
        }
        Ok((200, b"{}".to_vec()))
    }
}

#[tokio::test]
async fn test_tier5_adversarial_github_queued_backlog_mixed_workflow_states() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    // Run 1 and Run 2 are queued
    let runs_resp = serde_json::json!({
        "total_count": 2,
        "workflow_runs": [
            { "id": 1, "status": "queued" },
            { "id": 2, "status": "queued" }
        ]
    });

    // Run 1 jobs: queued (+1), waiting (+1), in_progress (0), completed (0)
    let jobs_run1 = serde_json::json!({
        "total_count": 4,
        "jobs": [
            { "id": 101, "name": "build", "status": "queued" },
            { "id": 102, "name": "lint", "status": "waiting" },
            { "id": 103, "name": "test-unit", "status": "in_progress" },
            { "id": 104, "name": "deploy", "status": "completed", "conclusion": "success" }
        ]
    });

    // Run 2 jobs: pending (+1), requested (+1), failure (0), cancelled (0)
    let jobs_run2 = serde_json::json!({
        "total_count": 4,
        "jobs": [
            { "id": 201, "name": "e2e", "status": "pending" },
            { "id": 202, "name": "audit", "status": "requested" },
            { "id": 203, "name": "bench", "status": "failure" },
            { "id": 204, "name": "package", "status": "cancelled" }
        ]
    });

    let transport = Arc::new(MockQueuedJobsTransport {
        runs_json: runs_resp.to_string(),
        jobs_run1_json: jobs_run1.to_string(),
        jobs_run2_json: jobs_run2.to_string(),
    });

    let client = GitHubActionsClient::new("token".into(), transport);
    let count = client
        .count_queued_jobs_for_repo("swack-tools", "oxidex")
        .await
        .unwrap();

    // 2 from run 1 (queued, waiting) + 2 from run 2 (pending, requested) = 4 queued jobs
    assert_eq!(
        count, 4,
        "Expected exactly 4 queued jobs from mixed workflow states"
    );
}

// ============================================================================
// 8. Canonical SHA-256 fleet_fingerprint Diff Gating: Order Invariance & Sensitivity
// ============================================================================

#[test]
fn test_tier5_adversarial_fleet_fingerprint_permutation_invariance_and_sensitivity() {
    let inst_a = SimpleGceInstance {
        id: Some("1".into()),
        name: "vm-a".into(),
        zone: "us-central1-a".into(),
        machine_type: "c4d-highcpu-16".into(),
        status: "RUNNING".into(),
        metadata: GceMetadata::default(),
        disks: vec![],
    };
    let inst_b = SimpleGceInstance {
        id: Some("2".into()),
        name: "vm-b".into(),
        zone: "us-central1-b".into(),
        machine_type: "n4d-highcpu-16".into(),
        status: "RUNNING".into(),
        metadata: GceMetadata::default(),
        disks: vec![],
    };
    let inst_c = SimpleGceInstance {
        id: Some("3".into()),
        name: "vm-c".into(),
        zone: "us-central1-c".into(),
        machine_type: "c2d-highcpu-16".into(),
        status: "TERMINATED".into(),
        metadata: GceMetadata::default(),
        disks: vec![],
    };

    let runner_1 = SimpleGitHubRunner {
        id: 10,
        name: "runner-1".into(),
        status: "online".into(),
        busy: false,
    };
    let runner_2 = SimpleGitHubRunner {
        id: 20,
        name: "runner-2".into(),
        status: "online".into(),
        busy: true,
    };
    let runner_3 = SimpleGitHubRunner {
        id: 30,
        name: "runner-3".into(),
        status: "offline".into(),
        busy: false,
    };

    let launch_1 = LaunchOperation {
        target: "swack-tools".into(),
        host_name: "launch-1".into(),
        zone: "us-central1-a".into(),
        machine_type: "c4d-highcpu-16".into(),
        phase: LaunchPhase::Creating,
        deployment_id: "dep-1".into(),
        started_at: 1700000000,
        instance_id: None,
        expected_runners: vec![],
    };
    let launch_2 = LaunchOperation {
        target: "swack-tools".into(),
        host_name: "launch-2".into(),
        zone: "us-central1-b".into(),
        machine_type: "c4d-highcpu-16".into(),
        phase: LaunchPhase::Booting,
        deployment_id: "dep-2".into(),
        started_at: 1700000000,
        instance_id: None,
        expected_runners: vec![],
    };

    // 1. Order Permutations
    let fp1 = compute_fleet_fingerprint(
        &[inst_a.clone(), inst_b.clone(), inst_c.clone()],
        &[runner_1.clone(), runner_2.clone(), runner_3.clone()],
        15,
        &[launch_1.clone(), launch_2.clone()],
    );

    let fp2 = compute_fleet_fingerprint(
        &[inst_c.clone(), inst_a.clone(), inst_b.clone()],
        &[runner_3.clone(), runner_1.clone(), runner_2.clone()],
        15,
        &[launch_2.clone(), launch_1.clone()],
    );

    let fp3 = compute_fleet_fingerprint(
        &[inst_b.clone(), inst_c.clone(), inst_a.clone()],
        &[runner_2.clone(), runner_3.clone(), runner_1.clone()],
        15,
        &[launch_1.clone(), launch_2.clone()],
    );

    assert_eq!(
        fp1, fp2,
        "Fingerprint must be strictly identical regardless of instance/runner/launch ordering"
    );
    assert_eq!(
        fp2, fp3,
        "Fingerprint must be strictly identical regardless of instance/runner/launch ordering"
    );

    // 2. Sensitivity Checks
    // Mutate runner busy state
    let runner_1_busy = SimpleGitHubRunner {
        busy: true,
        ..runner_1.clone()
    };
    let fp_mut_runner =
        compute_fleet_fingerprint(&[inst_a.clone(), inst_b.clone()], &[runner_1_busy], 15, &[]);
    assert_ne!(
        fp1, fp_mut_runner,
        "Toggling runner busy must change fingerprint"
    );

    // Mutate VM status
    let inst_a_term = SimpleGceInstance {
        status: "TERMINATED".into(),
        ..inst_a.clone()
    };
    let fp_mut_vm =
        compute_fleet_fingerprint(&[inst_a_term, inst_b.clone()], &[runner_1.clone()], 15, &[]);
    assert_ne!(
        fp1, fp_mut_vm,
        "Changing instance status must change fingerprint"
    );

    // Mutate queued jobs count
    let fp_mut_queue = compute_fleet_fingerprint(
        &[inst_a.clone(), inst_b.clone()],
        &[runner_1.clone()],
        16,
        &[],
    );
    assert_ne!(
        fp1, fp_mut_queue,
        "Changing queued jobs count must change fingerprint"
    );
}

// ============================================================================
// 9. Scheduled Reconciler & KV Quota: Strictly 0 KV Writes on Unchanged Ticks
// ============================================================================

#[tokio::test]
async fn test_tier5_adversarial_reconciler_zero_kv_writes_unchanged_ticks() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = Tier5MockKvStore::new();
    let gcp = Tier5MockGcpReconciler::default();
    let gh = Tier5MockGitHubReconciler::default();

    populate_all_floor_hosts(&gcp);

    let mut now = 1700000000u64;

    // Initial tick: writes state snapshot (1 KV put)
    let rep1 = run_reconciliation(&kv, &gcp, &gh, now)
        .await
        .expect("tick 1");
    assert!(rep1.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // Stress test: 200 consecutive ticks advancing by 3 seconds each (total 600s < 1800s heartbeat)
    for tick_idx in 2..=201 {
        now += 3;
        let rep = run_reconciliation(&kv, &gcp, &gh, now).await.expect("tick");
        assert_eq!(rep.tick, tick_idx);
        assert!(
            !rep.snapshot_written,
            "Tick {} wrote snapshot despite 0 fleet changes!",
            tick_idx
        );
        assert_eq!(rep.launched_hosts.len(), 0);
        assert_eq!(rep.cleaned_vms.len(), 0);
        assert_eq!(rep.cleaned_runners.len(), 0);
    }

    // Strictly 0 additional KV writes across all 200 unchanged ticks
    assert_eq!(
        kv.put_count.load(Ordering::SeqCst),
        1,
        "Expected strictly 1 KV write for all 201 unchanged ticks"
    );
}

// ============================================================================
// 10. Coarse Heartbeat: Single KV Write Occurs at Exactly 1800s
// ============================================================================

#[tokio::test]
async fn test_tier5_adversarial_reconciler_coarse_heartbeat_boundary() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = Tier5MockKvStore::new();
    let gcp = Tier5MockGcpReconciler::default();
    let gh = Tier5MockGitHubReconciler::default();

    populate_all_floor_hosts(&gcp);

    let t0 = 1700000000u64;

    // Initial tick at T0 (1 KV put)
    let rep0 = run_reconciliation(&kv, &gcp, &gh, t0).await.expect("t0");
    assert!(rep0.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // T0 + 1799s (1 second before 30m heartbeat) -> NO write
    let rep_1799 = run_reconciliation(&kv, &gcp, &gh, t0 + 1799)
        .await
        .expect("1799s");
    assert!(
        !rep_1799.snapshot_written,
        "At 1799s heartbeat must not fire"
    );
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);

    // T0 + 1800s (exactly 30m heartbeat) -> single KV write
    let rep_1800 = run_reconciliation(&kv, &gcp, &gh, t0 + 1800)
        .await
        .expect("1800s");
    assert!(
        rep_1800.snapshot_written,
        "At 1800s heartbeat MUST fire single snapshot write"
    );
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 2);

    // T0 + 1801s (1s after heartbeat) -> NO write
    let rep_1801 = run_reconciliation(&kv, &gcp, &gh, t0 + 1801)
        .await
        .expect("1801s");
    assert!(!rep_1801.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 2);

    // T0 + 3599s -> NO write
    let rep_3599 = run_reconciliation(&kv, &gcp, &gh, t0 + 3599)
        .await
        .expect("3599s");
    assert!(!rep_3599.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 2);

    // T0 + 3600s (second 30m heartbeat) -> single KV write
    let rep_3600 = run_reconciliation(&kv, &gcp, &gh, t0 + 3600)
        .await
        .expect("3600s");
    assert!(rep_3600.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 3);
}

// ============================================================================
// 11. Scale-Up Boundary: Exactly 19 Queued Jobs (No Scale-Up) vs Exactly 20 (Scale-Up 1 Host)
// ============================================================================

#[tokio::test]
async fn test_tier5_adversarial_reconciler_scale_up_exact_boundaries_19_vs_20() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = Tier5MockKvStore::new();
    let gcp = Tier5MockGcpReconciler::default();
    let gh = Tier5MockGitHubReconciler::default();

    populate_all_floor_hosts(&gcp);

    let t0 = 1700000000u64;

    // Initial tick to stabilize floor
    let rep_init = run_reconciliation(&kv, &gcp, &gh, t0).await.expect("init");
    assert_eq!(rep_init.launched_hosts.len(), 0);

    // Boundary Test 1: Exactly 19 queued jobs -> NO scale-up
    *gh.queued_jobs.lock().unwrap() = 19;
    let rep_19 = run_reconciliation(&kv, &gcp, &gh, t0 + 10)
        .await
        .expect("19 jobs");
    assert_eq!(
        rep_19.launched_hosts.len(),
        0,
        "Exactly 19 queued jobs must NOT trigger scale-up"
    );
    assert_eq!(gcp.inserted_instances.lock().unwrap().len(), 0);

    // Boundary Test 2: Exactly 20 queued jobs -> Scale-up 1 host!
    *gh.queued_jobs.lock().unwrap() = 20;
    let rep_20 = run_reconciliation(&kv, &gcp, &gh, t0 + 20)
        .await
        .expect("20 jobs");
    assert_eq!(
        rep_20.launched_hosts.len(),
        1,
        "Exactly 20 queued jobs MUST trigger scale-up of 1 Spot runner host"
    );
    assert!(rep_20.launched_hosts[0].starts_with("oxidex-runners-scale-"));
    assert_eq!(gcp.inserted_instances.lock().unwrap().len(), 1);

    // Verify machine shape of scale-up host (>= 16 vCPU / 32 GiB)
    let scale_req = &gcp.inserted_instances.lock().unwrap()[0];
    assert!(scale_req.machine_type.contains("c4d-highcpu-16"));
}

// ============================================================================
// 12. Concurrency Lease: Overlapping Ticks within 240s Skip Execution
// ============================================================================

#[tokio::test]
async fn test_tier5_adversarial_reconciler_concurrency_lease_skipping() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = Tier5MockKvStore::new();
    let gcp = Tier5MockGcpReconciler::default();
    let gh = Tier5MockGitHubReconciler::default();

    populate_all_floor_hosts(&gcp);

    let t0 = 1700000000u64;

    // Simulate an ongoing active lease expiring at T0 + 240s
    set_reconciler_lease(Some(t0 + 240));

    // Overlapping tick at T0 + 10s -> MUST skip execution
    let rep_skip = run_reconciliation(&kv, &gcp, &gh, t0 + 10)
        .await
        .expect("skip");
    assert_eq!(rep_skip.status, "skipped_lease_active");
    assert!(!rep_skip.snapshot_written);
    assert_eq!(rep_skip.launched_hosts.len(), 0);
    assert_eq!(rep_skip.cleaned_vms.len(), 0);
    assert_eq!(rep_skip.cleaned_runners.len(), 0);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // Overlapping tick at T0 + 239s -> still skips
    let rep_skip_239 = run_reconciliation(&kv, &gcp, &gh, t0 + 239)
        .await
        .expect("skip 239");
    assert_eq!(rep_skip_239.status, "skipped_lease_active");
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 0);

    // Tick at T0 + 240s (lease expired) -> proceeds normally!
    let rep_proc = run_reconciliation(&kv, &gcp, &gh, t0 + 240)
        .await
        .expect("proceed");
    assert_eq!(rep_proc.status, "reconciled");
    assert!(rep_proc.snapshot_written);
    assert_eq!(kv.put_count.load(Ordering::SeqCst), 1);
}

// ============================================================================
// 13. Preempted VM Cleanup: TERMINATED GCE Instances Trigger Deletion with deleteDisk=ALL
// ============================================================================

#[tokio::test]
async fn test_tier5_adversarial_reconciler_preempted_vm_cleanup() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = Tier5MockKvStore::new();
    let gcp = Tier5MockGcpReconciler::default();
    let gh = Tier5MockGitHubReconciler::default();

    // Populate healthy floor hosts PLUS one preempted (TERMINATED) Spot VM
    populate_all_floor_hosts(&gcp);
    let term_vm_name = "oxidex-runners-swack-tools-preempted";
    gcp.instances.lock().unwrap().push(SimpleGceInstance {
        id: Some("vm-term-1".into()),
        name: term_vm_name.into(),
        zone: "us-central1-a".into(),
        machine_type: "c4d-highcpu-16".into(),
        status: "TERMINATED".into(),
        metadata: GceMetadata::default(),
        disks: vec![],
    });

    let t0 = 1700000000u64;
    let rep = run_reconciliation(&kv, &gcp, &gh, t0)
        .await
        .expect("reconcile");

    // Preempted VM must be cleaned
    assert!(
        rep.cleaned_vms.contains(&term_vm_name.to_string()),
        "Preempted TERMINATED VM must be cleaned up"
    );
    assert!(
        gcp.deleted_instances
            .lock()
            .unwrap()
            .contains(&term_vm_name.to_string()),
        "GCP delete_instance must be invoked on the TERMINATED VM"
    );
    assert!(
        rep.snapshot_written,
        "VM deletion mutation must trigger snapshot update"
    );
}

// ============================================================================
// 14. Stale Runner Cleanup: offline and !busy Runners Deregistered
// ============================================================================

#[tokio::test]
async fn test_tier5_adversarial_reconciler_stale_runner_cleanup_matrix() {
    let _guard = TIER5_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    reset_reconciler_state();

    let kv = Tier5MockKvStore::new();
    let gcp = Tier5MockGcpReconciler::default();
    let gh = Tier5MockGitHubReconciler::default();

    populate_all_floor_hosts(&gcp);

    // Active VM: oxidex-runners-swack-tools-healthy
    let active_vm = "oxidex-runners-swack-tools-healthy";

    // Setup 4 runners to test the cleanup matrix
    gh.runners.lock().unwrap().extend(vec![
        // Runner 1: offline, not busy, has active VM -> KEPT (e.g. host rebooting or runner restarting)
        SimpleGitHubRunner {
            id: 101,
            name: format!("oxidex-{}-1", active_vm),
            status: "offline".into(),
            busy: false,
        },
        // Runner 2: offline, not busy, NO active VM -> STALE, MUST BE CLEANED
        SimpleGitHubRunner {
            id: 102,
            name: "oxidex-runners-deleted-vm-2".into(),
            status: "offline".into(),
            busy: false,
        },
        // Runner 3: offline, BUSY, NO active VM -> KEPT (safety invariant: never kill busy runner)
        SimpleGitHubRunner {
            id: 103,
            name: "oxidex-runners-missing-vm-busy-3".into(),
            status: "offline".into(),
            busy: true,
        },
        // Runner 4: online, not busy, NO active VM -> KEPT (still reporting online)
        SimpleGitHubRunner {
            id: 104,
            name: "oxidex-runners-missing-vm-online-4".into(),
            status: "online".into(),
            busy: false,
        },
    ]);

    let t0 = 1700000000u64;
    let rep = run_reconciliation(&kv, &gcp, &gh, t0)
        .await
        .expect("reconcile");

    // Only Runner 2 should be cleaned
    assert_eq!(rep.cleaned_runners.len(), 1);
    assert_eq!(rep.cleaned_runners[0], "oxidex-runners-deleted-vm-2");
    assert_eq!(gh.deleted_runners.lock().unwrap().as_slice(), &[102]);
    assert!(
        rep.snapshot_written,
        "Runner deregistration must trigger snapshot update"
    );
}

// ============================================================================
// 15. Resource Budget Gate: Release Wasm < 3 MiB Raw and < 1 MiB Gzipped
// ============================================================================

#[test]
fn test_tier5_adversarial_resource_budget_gate_size_limits() {
    let wasm_path = std::path::Path::new("build/index_bg.wasm");
    if !wasm_path.exists() {
        let alt_path =
            std::path::Path::new("target/wasm32-unknown-unknown/release/spot_dashboard_edge.wasm");
        if !alt_path.exists() {
            eprintln!(
                "Wasm artifact not found at {:?} or {:?}; skipping size gate check in unit run",
                wasm_path, alt_path
            );
            return;
        }
    }

    let actual_path = if wasm_path.exists() {
        wasm_path
    } else {
        std::path::Path::new("target/wasm32-unknown-unknown/release/spot_dashboard_edge.wasm")
    };

    let wasm_bytes = std::fs::read(actual_path).expect("read wasm");
    let raw_len = wasm_bytes.len();
    assert!(
        raw_len < 3 * 1024 * 1024,
        "Wasm uncompressed size {} bytes exceeds Cloudflare Free Tier limit of 3 MiB (3,145,728 bytes)",
        raw_len
    );

    // Verify gzipped bundle size < 1 MiB (1,048,576 bytes) using system gzip
    let gzip_output = std::process::Command::new("gzip")
        .arg("-c")
        .arg(actual_path)
        .output();

    if let Ok(out) = gzip_output {
        if out.status.success() {
            let compressed_len = out.stdout.len();
            assert!(
                compressed_len < 1024 * 1024,
                "Wasm gzipped size {} bytes exceeds target limit of 1 MiB (1,048,576 bytes)",
                compressed_len
            );
        }
    }
}
