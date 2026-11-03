//! The Qualifying Instances Matrix: Spot list prices, sizing filters, and the small runner hosts.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};
use spot_dashboard_edge::qualify::*;
use spot_dashboard_edge::state::{
    default_dashboard_bundle, handle_qualify, invalidate_state_cache, save_state, KvStoreBackend,
    StateError,
};

#[derive(Default)]
struct Kv {
    data: Mutex<HashMap<String, String>>,
}

#[async_trait::async_trait(?Send)]
impl KvStoreBackend for Kv {
    async fn get(&self, key: &str) -> Result<Option<String>, StateError> {
        Ok(self.data.lock().unwrap().get(key).cloned())
    }
    async fn put(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.data
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
}

static LOCK: Mutex<()> = Mutex::new(());
const NOW: u64 = 1_793_500_000;

fn two_vcpu_runners() -> Value {
    json!({
        "role": "cicd_runner",
        "min_runners": 1,
        "runner_vcpu": 2,
        "runner_ram_gib": 6,
        "min_benchmark_score": 1000,
        "min_instance_vcpu": 2,
        "min_instance_ram_gib": 8,
        "disk_category": "any",
        "data_disk_gb": 100,
        "budget": {"max_hourly_usd": 5.0, "max_daily_usd": 20.0, "max_monthly_usd": 100.0, "include_disk_costs": true}
    })
}

fn rows(q: &Value, key: &str) -> Vec<Value> {
    q[key].as_array().cloned().unwrap_or_default()
}

#[test]
fn prices_come_from_spot_rates_per_vcpu_and_gib() {
    // n4a-standard-2: 2 vCPU x 0.01002 + 8 GiB x 0.001141.
    assert_eq!(compute_hourly_usd("n4a-standard-2"), Some(0.0292));
    assert_eq!(compute_hourly_usd("c4a-highcpu-16"), Some(0.3145));
    assert_eq!(machine_shape("c4d-highcpu-16"), Some((16, 30.0)));
    assert_eq!(machine_shape("e2-standard-2"), Some((2, 8.0)));
    assert_eq!(compute_hourly_usd("zz9-standard-2"), None);
    assert_eq!(compute_hourly_usd("custom-8-32768"), None);
    // 100 GiB of Hyperdisk Balanced at $0.08/GiB-month.
    assert!((disk_hourly_usd("hyperdisk-balanced", 100) - 8.0 / 730.0).abs() < 1e-9);
    assert_eq!(default_disk_type("e2-standard-2"), "pd-balanced");
    assert_eq!(default_disk_type("n4a-standard-2"), "hyperdisk-balanced");
}

#[test]
fn two_vcpu_sizing_lists_small_hosts_cheapest_first() {
    let q = qualify("cicd_runner", &two_vcpu_runners(), &small_host_offers());
    let hw = rows(&q, "hardware_qualifying");
    assert!(!hw.is_empty());
    // Sorted by price per host.
    let prices: Vec<f64> = hw
        .iter()
        .map(|r| r["evaluated_hourly_usd"].as_f64().unwrap())
        .collect();
    assert!(prices.windows(2).all(|w| w[0] <= w[1]), "{:?}", prices);
    let cheapest = &hw[0];
    assert_eq!(cheapest["machine_type"], "n4a-standard-2");
    assert_eq!(cheapest["units_per_instance"], 1);
    assert!(cheapest["pool_host_hourly_usd"].as_f64().unwrap() > 0.0);
    assert_eq!(cheapest["arch"], "arm64");
    // A 4 vCPU host fits two of these runners.
    let four = hw
        .iter()
        .find(|r| r["machine_type"] == "t2d-standard-4")
        .unwrap();
    assert_eq!(four["units_per_instance"], 2);
    // Benchmark floor drops E2 (950) and T2A (920).
    assert!(!hw
        .iter()
        .any(|r| r["machine_type"].as_str().unwrap().starts_with("e2-")));
    assert!(!hw
        .iter()
        .any(|r| r["machine_type"].as_str().unwrap().starts_with("t2a-")));
    assert!(
        q["disqualified_counts"]["below_benchmark_score"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(q["requirements"]["min_benchmark_score"], 1000.0);
    assert_eq!(q["requirements"]["budget"]["max_hourly_usd"], 5.0);
}

#[test]
fn host_size_disk_and_budget_filters_apply() {
    let mut req = two_vcpu_runners();
    req["min_instance_vcpu"] = json!(4);
    let q = qualify("cicd_runner", &req, &small_host_offers());
    assert!(rows(&q, "hardware_qualifying")
        .iter()
        .all(|r| r["vcpu"].as_u64().unwrap() >= 4));

    let mut req = two_vcpu_runners();
    req["disk_category"] = json!("normal");
    let q = qualify("cicd_runner", &req, &small_host_offers());
    assert!(rows(&q, "hardware_qualifying")
        .iter()
        .all(|r| r["data_disk_type"].as_str().unwrap().starts_with("pd-")));

    // Ten runners at $0.10/hr: nothing fits the budget, but the hardware list stays.
    let mut req = two_vcpu_runners();
    req["min_runners"] = json!(10);
    req["budget"]["max_hourly_usd"] = json!(0.10);
    let q = qualify("cicd_runner", &req, &small_host_offers());
    assert!(!rows(&q, "hardware_qualifying").is_empty());
    assert!(rows(&q, "spot_price_qualifying").is_empty());
    assert_eq!(q["spot_price_qualifying_count"], 0);
}

#[test]
fn bad_sizing_values_fall_back_to_defaults() {
    let req =
        json!({"runner_vcpu": "lots", "runner_ram_gib": -3, "min_runners": null, "budget": "none"});
    let q = qualify("cicd_runner", &req, &small_host_offers());
    assert_eq!(q["requirements"]["runner_vcpu"], 2);
    assert_eq!(q["requirements"]["runner_ram_gib"], 4.0);
    assert_eq!(q["requirements"]["budget"]["max_hourly_usd"], Value::Null);
    assert!(!rows(&q, "spot_price_qualifying").is_empty());
}

#[test]
fn every_small_offer_is_launchable_in_a_known_zone() {
    for row in small_host_offers() {
        let mt = row["machine_type"].as_str().unwrap();
        let zone = row["zone"].as_str().unwrap();
        assert!(
            spot_dashboard_edge::runner_pools::machine_shape(mt).is_some(),
            "{}",
            mt
        );
        assert!(
            spot_dashboard_edge::runner_pools::is_known_gce_zone(zone),
            "{}",
            zone
        );
    }
}

#[tokio::test]
async fn the_qualify_handler_uses_the_stored_catalog_and_saves_the_sizing() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    let kv = Kv::default();
    let mut b = default_dashboard_bundle(NOW).unwrap();
    b.state.overview = Some(json!({
        "qualifications": {
            "cicd_runner": {
                "requirements": {"runner_vcpu": 4, "runner_ram_gib": 16, "min_instance_vcpu": 8},
                "hardware_qualifying": [{
                    "machine_type": "c2d-standard-8", "zone": "europe-north1-a", "region": "europe-north1",
                    "vcpu": 8, "ram_gib": 32, "benchmark_score": 1311, "data_disk_type": "hyperdisk-balanced",
                    "pricing": {"compute_hourly_usd": 0.036}
                }],
                "spot_price_qualifying": []
            },
            "rust_builder": {"hardware_qualifying": [], "spot_price_qualifying": []}
        }
    }));
    save_state(&kv, &mut b, NOW).await.unwrap();

    // A preview doesn't save; it sees the stored offer (repriced) and the small hosts.
    let preview = handle_qualify(&kv, "cicd_runner", &two_vcpu_runners(), false, NOW)
        .await
        .unwrap();
    let hw = rows(&preview["qualification"], "hardware_qualifying");
    let c2d = hw
        .iter()
        .find(|r| r["machine_type"] == "c2d-standard-8")
        .unwrap();
    assert_eq!(
        c2d["pricing"]["compute_hourly_usd"],
        compute_hourly_usd("c2d-standard-8").unwrap()
    );
    assert_eq!(c2d["units_per_instance"], 4);
    assert!(hw.iter().any(|r| r["machine_type"] == "n4a-standard-2"));

    // Saving the profile stores the sizing; the overview then opens with it.
    handle_qualify(&kv, "cicd_runner", &two_vcpu_runners(), true, NOW)
        .await
        .unwrap();
    invalidate_state_cache();
    let (_, ov) = spot_dashboard_edge::state::handle_api_overview(&kv, Default::default(), NOW)
        .await
        .unwrap();
    let q = &ov["qualifications"]["cicd_runner"];
    assert_eq!(q["requirements"]["runner_vcpu"], 2);
    assert_eq!(
        rows(q, "hardware_qualifying")[0]["machine_type"],
        "n4a-standard-2"
    );
    // The sizing form reloads from the saved profile; the other role keeps a default profile.
    let runner = &ov["profiles"]["cicd_runner"];
    assert_eq!(runner["runner_vcpu"], 2);
    assert_eq!(runner["runner_ram_gib"], 6);
    assert_eq!(runner["budget"]["max_daily_usd"], 20.0);
    assert_eq!(ov["profiles"]["rust_builder"]["runner_vcpu"], 16);
}

#[tokio::test]
async fn a_fresh_install_has_profiles_and_sizing_for_both_roles() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    invalidate_state_cache();
    let kv = Kv::default();
    let (_, ov) = spot_dashboard_edge::state::handle_api_overview(&kv, Default::default(), NOW)
        .await
        .unwrap();
    for role in ["cicd_runner", "rust_builder"] {
        let profile = &ov["profiles"][role];
        assert!(
            profile["min_benchmark_score"].is_number(),
            "{role}: {profile}"
        );
        assert!(profile["budget"]["max_hourly_usd"].is_number());
        let reqs = &ov["qualifications"][role]["requirements"];
        assert!(reqs["min_benchmark_score"].is_number(), "{role}: {reqs}");
        assert!(reqs["budget"].is_object());
    }
    // Saving a profile on a fresh install keeps the full overview shape.
    handle_qualify(&kv, "cicd_runner", &two_vcpu_runners(), true, NOW)
        .await
        .unwrap();
    invalidate_state_cache();
    let (_, saved) = spot_dashboard_edge::state::handle_api_overview(&kv, Default::default(), NOW)
        .await
        .unwrap();
    assert!(saved["regions"].is_array(), "{}", saved["regions"]);
    assert!(saved["instances_view"]["instances"].is_array());
    assert_eq!(saved["profiles"]["cicd_runner"]["runner_vcpu"], 2);
    assert_eq!(
        saved["qualifications"]["cicd_runner"]["requirements"]["runner_vcpu"],
        2
    );
    // The default runner sizing finds the small hosts.
    let hw = rows(&ov["qualifications"]["cicd_runner"], "hardware_qualifying");
    assert!(
        hw.iter().any(|r| r["machine_type"] == "e2-standard-4"),
        "{hw:?}"
    );
}

#[test]
fn a_saved_profile_takes_only_well_formed_fields() {
    use spot_dashboard_edge::state::{default_profile, profile_with_sizing};
    let base = default_profile("cicd_runner");
    let p = profile_with_sizing(
        &base,
        &json!({
            "role": "rust_builder",
            "runner_vcpu": 2,
            "runner_ram_gib": 4.4,
            "min_benchmark_score": "lots",
            "data_disk_gb": -5,
            "disk_category": "normal",
            "budget": {"max_hourly_usd": 0.25, "include_disk_costs": false},
            "surprise": true
        }),
    );
    assert_eq!(p.role, "cicd_runner");
    assert_eq!(p.runner_vcpu, 2);
    assert_eq!(p.runner_ram_gib, 4);
    assert_eq!(p.min_benchmark_score, base.min_benchmark_score);
    assert_eq!(p.data_disk_gb, base.data_disk_gb);
    assert_eq!(p.disk_category, "normal");
    assert_eq!(p.budget.max_hourly_usd, 0.25);
    assert!(!p.budget.include_disk_costs);
    assert_eq!(p.budget.max_daily_usd, base.budget.max_daily_usd);
}

#[test]
fn only_the_newest_matrix_preview_draws_the_table() {
    // Every keystroke in the sizing form starts a preview. An earlier, slower answer must not
    // replace the rows for what the form says now.
    let html = spot_dashboard_edge::ui::DASHBOARD_HTML;
    let start = html
        .find("async function previewQualification(role)")
        .expect("previewQualification");
    let body = &html[start..start + 900];
    let seq = body
        .find("const seq = ++QUAL_PREVIEW_SEQ[role];")
        .expect("sequence taken");
    let fetch = body.find("await fetch(\"/api/qualify\"").expect("fetch");
    let guard = body
        .find("if (seq !== QUAL_PREVIEW_SEQ[role]) return;")
        .expect("stale answers dropped");
    let draw = body.find("renderQualificationTable()").expect("draw");
    assert!(seq < fetch && fetch < guard && guard < draw);
}

#[test]
fn runner_rows_are_only_machines_a_pool_can_launch() {
    let offer = |mt: &str, zone: &str| json!({ "machine_type": mt, "zone": zone, "benchmark_score": 2434.0 });
    let catalog = vec![
        offer("c4-standard-16", "northamerica-northeast1-a"),
        offer("m3-standard-16", "us-central1-a"),
        offer("c4d-highcpu-16", "not-a-zone1-z"),
    ];
    let reqs = json!({ "min_runners": 1, "runner_vcpu": 2, "runner_ram_gib": 4 });

    let q = qualify("cicd_runner", &reqs, &catalog);
    let hw = q["hardware_qualifying"].as_array().unwrap();
    assert_eq!(hw.len(), 1, "{:?}", hw);
    assert_eq!(hw[0]["machine_type"], "c4-standard-16");
    assert_eq!(q["hardware_qualifying_count"], 1);
    assert_eq!(q["disqualified_counts"]["cannot_host_runners"], 2);
    // Priced from the C4 Spot list rates, not a stored estimate: 16 vCPU and 60 GiB.
    let compute = hw[0]["pricing"]["compute_hourly_usd"].as_f64().unwrap();
    assert!((compute - 0.4733).abs() < 1e-4, "{}", compute);
    assert!(hw[0]["pool_host_hourly_usd"].as_f64().unwrap() >= compute);

    // Builders launch a fixed shape, so their matrix keeps its own rows.
    let q = qualify("rust_builder", &reqs, &catalog);
    assert_eq!(q["disqualified_counts"]["cannot_host_runners"], 0);
    assert!(q["hardware_qualifying_count"].as_u64().unwrap() >= 2);
}
