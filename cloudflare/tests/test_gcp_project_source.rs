//! The GCP project is never compiled in: the dashboard header shows the deploy-time
//! `GCP_PROJECT` value, and the reconciler refuses a service account key without `project_id`.
use spot_dashboard_edge::reconciler::{reconcile_worker_fleet, refresh_fleet_inventory};
use spot_dashboard_edge::state::{KvStoreBackend, StateError};
use spot_dashboard_edge::ui::render_dashboard_html;
use std::collections::HashMap;
use std::sync::Mutex;

struct MemKv(Mutex<HashMap<String, String>>);

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for MemKv {
    async fn get(&self, key: &str) -> Result<Option<String>, StateError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    async fn put(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
}

const KEY_WITHOUT_PROJECT: &str =
    r#"{"client_email":"sa@example.iam.gserviceaccount.com","private_key":"unused"}"#;

#[test]
fn header_uses_configured_project_and_has_no_compiled_default() {
    let unset = render_dashboard_html("", "");
    assert!(unset.contains("GCP Project: not configured"));
    assert!(!unset.contains("homelab"));

    let set = render_dashboard_html("my-gcloud-default", "example-org");
    assert!(set.contains("GCP Project: my-gcloud-default · GitHub Org: example-org"));
    assert!(!set.contains("GCP Project: not configured"));

    let escaped = render_dashboard_html("<x>", "");
    assert!(escaped.contains("GCP Project: &lt;x&gt; · GitHub Org: not configured"));
}

#[tokio::test]
async fn reconciler_rejects_service_account_key_without_project_id() {
    let kv = MemKv(Mutex::new(HashMap::new()));
    let err = reconcile_worker_fleet(
        &kv,
        Some(KEY_WITHOUT_PROJECT),
        Some("gh-token"),
        1_792_000_000,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("no project_id"), "{err}");

    let err = refresh_fleet_inventory(&kv, Some(KEY_WITHOUT_PROJECT), 1_792_000_000)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no project_id"), "{err}");
}
