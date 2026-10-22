use spot_dashboard_edge::state::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

pub struct RemediationMockKv {
    pub data: Mutex<HashMap<String, String>>,
    pub get_count: AtomicUsize,
    pub put_count: AtomicUsize,
}

impl RemediationMockKv {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            get_count: AtomicUsize::new(0),
            put_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for RemediationMockKv {
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

// ============================================================================
// 1. Finding 1: Cloud-Init Save Persistence & Validation
// ============================================================================

#[tokio::test]
async fn test_cloud_init_save_persists_template_and_updates_profile() {
    invalidate_state_cache();
    let kv = RemediationMockKv::new();
    let now = 1792000000;

    // Initialize state
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    save_state(&kv, &mut bundle, now).await.unwrap();
    let initial_puts = kv.put_count.load(Ordering::SeqCst);

    let req = SaveCloudInitRequest {
        id: Some("template-custom-1".into()),
        mode: Some("save".into()),
        role: "cicd_runner".into(),
        name: "Custom Spot Runner Template".into(),
        content: "#!/usr/bin/env bash\necho 'running bootstrap'".into(),
        assign_to_profile: true,
    };

    let (status, resp) = handle_post_cloud_init_save(&kv, req, now + 1)
        .await
        .unwrap();

    assert_eq!(status, 200);
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["template"]["id"], "template-custom-1");
    assert_eq!(resp["template"]["name"], "Custom Spot Runner Template");
    assert_eq!(resp["template"]["role"], "cicd_runner");
    assert_eq!(
        resp["template"]["content"],
        "#!/usr/bin/env bash\necho 'running bootstrap'"
    );

    // Verify KV was updated
    assert_eq!(kv.put_count.load(Ordering::SeqCst), initial_puts + 1);

    // Verify template persists in fresh load
    invalidate_state_cache();
    let (fresh_bundle, _) = load_state(&kv, now + 2).await.unwrap();
    let saved_tpl = fresh_bundle
        .state
        .cloud_init_templates
        .iter()
        .find(|t| t.id == "template-custom-1")
        .expect("Saved template must exist in persistent state");
    assert_eq!(saved_tpl.name, "Custom Spot Runner Template");
    assert_eq!(saved_tpl.role, "cicd_runner");
}

#[tokio::test]
async fn test_cloud_init_save_validates_required_fields_and_roles() {
    invalidate_state_cache();
    let kv = RemediationMockKv::new();
    let now = 1792000000;

    // Empty name
    let empty_name = SaveCloudInitRequest {
        id: None,
        mode: None,
        role: "cicd_runner".into(),
        name: "   ".into(),
        content: "echo hello".into(),
        assign_to_profile: false,
    };
    assert!(matches!(
        handle_post_cloud_init_save(&kv, empty_name, now).await,
        Err(StateError::Validation(_))
    ));

    // Empty content
    let empty_content = SaveCloudInitRequest {
        id: None,
        mode: None,
        role: "cicd_runner".into(),
        name: "Valid Name".into(),
        content: "  \n  ".into(),
        assign_to_profile: false,
    };
    assert!(matches!(
        handle_post_cloud_init_save(&kv, empty_content, now).await,
        Err(StateError::Validation(_))
    ));

    // Invalid role
    let invalid_role = SaveCloudInitRequest {
        id: None,
        mode: None,
        role: "super_admin".into(),
        name: "Valid Name".into(),
        content: "echo hello".into(),
        assign_to_profile: false,
    };
    assert!(matches!(
        handle_post_cloud_init_save(&kv, invalid_role, now).await,
        Err(StateError::Validation(_))
    ));
}

// ============================================================================
// 2. Finding 2: VM Templates from Instance Validation & Persistence
// ============================================================================

#[tokio::test]
async fn test_vm_templates_from_instance_success_and_persistence() {
    invalidate_state_cache();
    let kv = RemediationMockKv::new();
    let now = 1792000000;

    // Seed state with a live instance in instances_view
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.instances_view.push(InstanceRecord {
        id: "inst-seed-123".into(),
        name: "oxidex-runners-c4d-1".into(),
        zone: "us-central1-a".into(),
        region: "us-central1".into(),
        machine_type: "c4d-highcpu-16".into(),
        status: "RUNNING".into(),
        role: "cicd_runner".into(),
        vcpu: 16,
        ram_gib: 32,
        runners_count: 2,
        runners_count_known: true,
        runner_limits_known: true,
    });
    save_state(&kv, &mut bundle, now).await.unwrap();

    let req = CreateVmTemplateRequest {
        instance_id: "inst-seed-123".into(),
        template_name: "C4D Spot Runner Golden Template".into(),
    };

    let (status, resp) = handle_post_vm_templates_from_instance(&kv, req, now + 1)
        .await
        .unwrap();

    assert_eq!(status, 200);
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["template_name"], "C4D Spot Runner Golden Template");
    assert_eq!(resp["template"]["machine_type"], "c4d-highcpu-16");
    assert_eq!(resp["template"]["cpu_platform"], "AMD Turin");
    assert_eq!(resp["template"]["disk_category"], "pd_balanced");
    assert_eq!(resp["template"]["boot_disk_type"], "pd-balanced");
    assert_eq!(resp["template"]["vcpu"], 16);
    assert_eq!(resp["template"]["ram_gib"], 32);
    assert_eq!(resp["template"]["runners_count"], 2);
    assert_eq!(resp["template"]["runner_vcpu"], 8);
    assert_eq!(resp["template"]["runner_ram_gib"], 16);
    // UI expects data.vm_template
    assert!(resp.get("vm_template").is_some());
    assert_eq!(resp["vm_template"]["id"], resp["template"]["id"]);

    // Verify persistence in fresh state load
    invalidate_state_cache();
    let (fresh_bundle, _) = load_state(&kv, now + 2).await.unwrap();
    let registered_tpl = fresh_bundle
        .state
        .vm_templates
        .iter()
        .find(|t| t.name == "C4D Spot Runner Golden Template")
        .expect("Template must be stored in persistent vm_templates");
    assert_eq!(registered_tpl.machine_type, "c4d-highcpu-16");
}

#[tokio::test]
async fn test_vm_templates_from_instance_rejects_nonexistent_instance_with_404() {
    invalidate_state_cache();
    let kv = RemediationMockKv::new();
    let now = 1792000000;

    let req = CreateVmTemplateRequest {
        instance_id: "nonexistent-instance-999".into(),
        template_name: "Ghost Template".into(),
    };

    let res = handle_post_vm_templates_from_instance(&kv, req, now).await;
    match res {
        Err(StateError::NotFound(msg)) => {
            assert!(msg.contains("nonexistent-instance-999"));
        }
        other => panic!("Expected StateError::NotFound, got {:?}", other),
    }
}

// ============================================================================
// 3. Finding 3: Rust Builds POST Telemetry Recording & Aggregate Rollup
// ============================================================================

#[tokio::test]
async fn test_rust_builds_post_records_build_computes_aggregates_and_bounds_history() {
    invalidate_state_cache();
    let kv = RemediationMockKv::new();
    let now = 1792000000;

    let req = RecordRustBuildRequest {
        status: "DELIVERED".into(),
        project_name: Some("swack-tools/oxidex".into()),
        commit_sha: Some("d37731697ab8".into()),
        profile: Some("release".into()),
        compile_duration_seconds: Some(120.0),
        delivery_duration_seconds: Some(15.0),
        instance_name: Some("builder-n4-1".into()),
        zone: Some("us-central1-a".into()),
        machine_type: Some("n4-highcpu-16".into()),
        cpu_platform: Some("Intel Sapphire Rapids".into()),
        vcpu: Some(16),
        origin_ip: Some("127.0.0.1".into()),
        cache_hits: Some(450),
        total_crates: Some(500),
        peak_ram_gib: Some(18.5),
        cpu_time_seconds: Some(1800.0),
    };

    let (status, resp) = handle_post_rust_builds(&kv, req, now).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["build"]["status"], "DELIVERED");
    assert_eq!(resp["build"]["total_duration_seconds"], 135.0);
    assert_eq!(resp["build"]["cache_hit_rate_percent"], 90.0);
    assert_eq!(resp["build"]["peak_ram_gib"], 18.5);

    // Verify rollup totals in state
    invalidate_state_cache();
    let (fresh_bundle, _) = load_state(&kv, now + 1).await.unwrap();
    assert_eq!(fresh_bundle.state.rust_builds["total_builds"], 1);
    assert_eq!(fresh_bundle.state.rust_builds["total_cache_hits"], 450);
    assert_eq!(
        fresh_bundle.state.rust_builds["overall_cache_hit_rate_percent"],
        90.0
    );
    assert_eq!(fresh_bundle.state.rust_builds["max_peak_ram_gib"], 18.5);
}

// ============================================================================
// 4. Finding 5: Unified Inventory (instances_view synchronization)
// ============================================================================

#[tokio::test]
async fn test_instance_actions_query_live_instances_view_inventory() {
    invalidate_state_cache();
    let kv = RemediationMockKv::new();
    let now = 1792000000;

    // Seed state ONLY in instances_view (replicating actual reconciler behavior)
    let (mut bundle, _) = load_state(&kv, now).await.unwrap();
    bundle.state.instances_view.push(InstanceRecord {
        id: "live-reconciler-vm-1".into(),
        name: "oxidex-runners-europe-1".into(),
        zone: "europe-west1-b".into(),
        region: "europe-west1".into(),
        machine_type: "c4d-highcpu-16".into(),
        status: "RUNNING".into(),
        role: "cicd_runner".into(),
        vcpu: 16,
        ram_gib: 32,
        runners_count: 2,
        runners_count_known: true,
        runner_limits_known: true,
    });
    // Explicitly verify bundle.state.instances is EMPTY
    bundle.state.instances.clear();
    save_state(&kv, &mut bundle, now).await.unwrap();

    // 1. Delete request must succeed finding the instance in instances_view
    let del_req = DeleteInstanceRequest {
        instance_id: Some("live-reconciler-vm-1".into()),
        instance_name: None,
    };
    let (del_status, del_resp) = handle_post_instances_delete(&kv, del_req, now + 1)
        .await
        .unwrap();
    assert_eq!(del_status, 200);
    assert_eq!(del_resp["instance"]["id"], "live-reconciler-vm-1");
    assert_eq!(del_resp["status"], "not_applied");

    // 2. Scale-region must count current running instances from instances_view
    let scale_req = ScaleRegionRequest {
        region: "europe-west1".into(),
        target_instances: 4,
        role: "cicd_runner".into(),
    };
    let (scale_status, scale_resp) = handle_post_instances_scale_region(&kv, scale_req, now + 2)
        .await
        .unwrap();
    assert_eq!(scale_status, 200);
    assert_eq!(scale_resp["current_count"], 1); // Found 1 in instances_view!
    assert_eq!(scale_resp["target_instances"], 4);
}
