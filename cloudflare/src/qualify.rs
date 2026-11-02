//! Spot price estimates and the Qualifying Instances Matrix.
//!
//! Prices come from Google's Spot list prices per vCPU-hour and per GiB-hour of RAM
//! ([`spot_rates`]), so every machine type of a known series gets the same kind of estimate.
//! [`qualify`] filters a catalog of offers (machine type and zone) by the sizing an administrator
//! enters on the Machine Qualification tab, works out how many runners fit each host, prices each
//! host and the hosts the runner floor needs, and sorts the cheapest first.

use serde_json::{json, Map, Value};

/// When [`spot_rates`] was read from the Cloud Billing Catalog API (service `6F81-5844-456A`).
pub const SPOT_PRICES_AS_OF: &str = "2026-11-01";
/// Where the rates apply. Other regions differ by up to about a third.
pub const SPOT_PRICES_REGION: &str = "us-central1";
pub const PRICING_SOURCE: &str = "gce_spot_list_price";
const HOURS_PER_MONTH: f64 = 730.0;

/// Spot list price of one vCPU-hour and one GiB-hour of RAM for a machine series, in
/// [`SPOT_PRICES_REGION`] on [`SPOT_PRICES_AS_OF`].
pub fn spot_rates(series: &str) -> Option<(f64, f64)> {
    Some(match series {
        "e2" => (0.01309, 0.001754),
        "t2d" => (0.0165, 0.002212),
        "t2a" => (0.01345, 0.001836),
        "c4a" => (0.01525, 0.001736),
        "n4a" => (0.01002, 0.001141),
        "c2d" => (0.01357, 0.001819),
        // Billed per GB; converted to GiB.
        "c4d" => (0.01452, 0.001554 * 1.073_741_824),
        "n4" => (0.01845, 0.002094),
        "c4" => (0.02074, 0.002357),
        "n4d" => (0.01745, 0.001986),
        _ => return None,
    })
}

/// Monthly price of one GiB of a disk type (us-central1).
pub fn disk_gib_month_usd(disk_type: &str) -> f64 {
    match disk_type {
        "hyperdisk-balanced" => 0.08,
        "hyperdisk-throughput" => 0.025,
        "pd-ssd" => 0.17,
        "pd-standard" => 0.04,
        _ => 0.10, // pd-balanced
    }
}

pub fn disk_hourly_usd(disk_type: &str, gib: u32) -> f64 {
    disk_gib_month_usd(disk_type) * gib as f64 / HOURS_PER_MONTH
}

/// vCPUs and GiB of RAM of a predefined machine type, from its name.
pub fn machine_shape(machine_type: &str) -> Option<(u32, f64)> {
    let mut parts = machine_type.split('-');
    let series = parts.next()?;
    let class = parts.next()?;
    let vcpu: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || vcpu == 0 {
        return None;
    }
    let per_vcpu = match (series, class) {
        ("c4d", "standard") => 3.875,
        ("c4d", "highcpu") => 1.875,
        ("c4d", "highmem") => 7.75,
        ("c4", "standard") => 3.75,
        ("c4", "highcpu") => 2.0,
        ("c4", "highmem") => 7.75,
        (_, "standard") => 4.0,
        (_, "highcpu") => 2.0,
        (_, "highmem") => 8.0,
        _ => return None,
    };
    Some((vcpu, vcpu as f64 * per_vcpu))
}

/// Estimated Spot price of a machine type's vCPUs and RAM per hour, without disks.
pub fn compute_hourly_usd(machine_type: &str) -> Option<f64> {
    let series = machine_type.split('-').next()?;
    let (cpu_rate, ram_rate) = spot_rates(series)?;
    let (vcpu, ram) = machine_shape(machine_type)?;
    Some(round4(vcpu as f64 * cpu_rate + ram * ram_rate))
}

/// The disk type a machine series must use: C4, C4A, C4D, N4, N4A and N4D take only Hyperdisk.
pub fn default_disk_type(machine_type: &str) -> &'static str {
    if crate::gcp::machine_type_requires_hyperdisk(machine_type) {
        "hyperdisk-balanced"
    } else {
        "pd-balanced"
    }
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// The `pricing` object the dashboard shows for one host.
pub fn pricing_json(compute_hourly: f64, disk_hourly: f64) -> Value {
    let total = compute_hourly + disk_hourly;
    json!({
        "compute_hourly_usd": round4(compute_hourly),
        "compute_daily_usd": round2(compute_hourly * 24.0),
        "compute_monthly_usd": round2(compute_hourly * HOURS_PER_MONTH),
        "disk_hourly_usd": round4(disk_hourly),
        "disk_daily_usd": round2(disk_hourly * 24.0),
        "disk_monthly_usd": round2(disk_hourly * HOURS_PER_MONTH),
        "tax_hourly_usd": 0.0,
        "tax_daily_usd": 0.0,
        "tax_monthly_usd": 0.0,
        "tax_rate_percent": 0.0,
        "total_hourly_usd": round4(total),
        "total_daily_usd": round2(total * 24.0),
        "total_monthly_usd": round2(total * HOURS_PER_MONTH),
        "pricing_source": PRICING_SOURCE,
        "pricing_region": SPOT_PRICES_REGION,
        "pricing_as_of": SPOT_PRICES_AS_OF,
    })
}

/// Region names and rough latency from the dashboard's home region, for the small-host offers.
const OFFER_REGIONS: &[(&str, &str, &str, f64)] = &[
    ("us-central1", "Council Bluffs, Iowa", "North America", 18.0),
    (
        "us-east1",
        "Moncks Corner, South Carolina",
        "North America",
        30.0,
    ),
    ("us-east4", "Ashburn, Virginia", "North America", 25.0),
    ("us-west1", "The Dalles, Oregon", "North America", 28.0),
    ("europe-west1", "St. Ghislain, Belgium", "Europe", 95.0),
    ("europe-west4", "Eemshaven, Netherlands", "Europe", 100.0),
];

/// 2 and 4 vCPU machine types runner pools can launch, the zones that offer them
/// (`gcloud compute machine-types list`, 2026-11-01), CPU platform, and benchmark score.
const SMALL_OFFERS: &[(&str, &str, u32, &[&str])] = &[
    (
        "n4a",
        "Google Axion (Neoverse N3)",
        1080,
        &[
            "us-central1-a",
            "us-central1-b",
            "us-central1-c",
            "us-east1-b",
            "us-east1-c",
            "us-east4-a",
            "us-west1-a",
            "us-west1-b",
            "europe-west1-b",
            "europe-west4-a",
        ],
    ),
    (
        "c4a",
        "Google Axion (Neoverse V2)",
        1180,
        &[
            "us-central1-a",
            "us-central1-b",
            "us-central1-c",
            "us-east1-b",
            "us-east1-c",
            "us-east4-a",
            "us-west1-a",
            "us-west1-c",
            "europe-west1-b",
            "europe-west4-a",
        ],
    ),
    (
        "t2a",
        "Ampere Altra (Neoverse N1)",
        920,
        &[
            "us-central1-a",
            "us-central1-b",
            "us-central1-f",
            "europe-west4-a",
        ],
    ),
    (
        "t2d",
        "AMD Milan",
        1300,
        &[
            "us-central1-a",
            "us-central1-b",
            "us-central1-c",
            "us-east1-b",
            "us-east1-c",
            "us-east4-a",
            "us-west1-a",
            "us-west1-b",
            "europe-west1-b",
            "europe-west4-a",
        ],
    ),
    (
        "e2",
        "Intel or AMD (E2)",
        950,
        &[
            "us-central1-a",
            "us-central1-b",
            "us-central1-c",
            "us-east1-b",
            "us-east1-c",
            "us-east4-a",
            "us-west1-a",
            "us-west1-b",
            "europe-west1-b",
            "europe-west4-a",
        ],
    ),
];

/// Offers for the 2 and 4 vCPU machine types that runner pools can launch.
pub fn small_host_offers() -> Vec<Value> {
    let mut out = Vec::new();
    for (series, platform, bench, zones) in SMALL_OFFERS {
        for size in [2u32, 4] {
            let machine_type = format!("{}-standard-{}", series, size);
            let Some((vcpu, ram)) = machine_shape(&machine_type) else {
                continue;
            };
            let disk_type = default_disk_type(&machine_type);
            for zone in *zones {
                let region = zone.rsplit_once('-').map_or(*zone, |x| x.0);
                let (location, continent, latency) = OFFER_REGIONS
                    .iter()
                    .find(|r| r.0 == region)
                    .map(|r| (r.1, r.2, r.3))
                    .unwrap_or((region, "", 50.0));
                out.push(json!({
                    "machine_type": machine_type,
                    "family": series,
                    "arch": if crate::runner_pools::is_arm64_machine_type(&machine_type) { "arm64" } else { "x86_64" },
                    "cpu_platform": platform,
                    "benchmark_score": bench,
                    "vcpu": vcpu,
                    "ram_gib": ram,
                    "region": region,
                    "zone": zone,
                    "display_location": location,
                    "continent": continent,
                    "latency_ms": latency,
                    "disk_category": if disk_type.starts_with("hyperdisk") { "hyperdrive" } else { "normal" },
                    "data_disk_type": disk_type,
                    "boot_disk_type": disk_type,
                }));
            }
        }
    }
    out
}

/// Sizing the matrix filters by. Missing or non-finite values take the defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Requirements {
    pub min_runners: u32,
    pub runner_vcpu: u32,
    pub runner_ram_gib: f64,
    pub min_benchmark_score: f64,
    pub min_instance_vcpu: u32,
    pub min_instance_ram_gib: f64,
    pub disk_category: String,
    pub data_disk_gb: u32,
    pub max_hourly_usd: f64,
    pub max_daily_usd: f64,
    pub max_monthly_usd: f64,
    pub include_disk_costs: bool,
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key)
        .and_then(|x| x.as_f64())
        .filter(|x| x.is_finite())
}

impl Requirements {
    pub fn from_json(v: &Value) -> Self {
        let budget = v.get("budget").cloned().unwrap_or(Value::Null);
        let pos = |x: Option<f64>, d: f64| x.filter(|x| *x >= 0.0).unwrap_or(d);
        Requirements {
            min_runners: pos(num(v, "min_runners"), 1.0).clamp(1.0, 500.0) as u32,
            runner_vcpu: pos(num(v, "runner_vcpu"), 2.0).clamp(1.0, 128.0) as u32,
            runner_ram_gib: pos(num(v, "runner_ram_gib"), 4.0).clamp(0.5, 1024.0),
            min_benchmark_score: pos(num(v, "min_benchmark_score"), 0.0),
            min_instance_vcpu: pos(num(v, "min_instance_vcpu"), 1.0).clamp(1.0, 512.0) as u32,
            min_instance_ram_gib: pos(num(v, "min_instance_ram_gib"), 0.0),
            disk_category: v
                .get("disk_category")
                .and_then(|x| x.as_str())
                .unwrap_or("any")
                .to_string(),
            data_disk_gb: pos(num(v, "data_disk_gb"), 100.0).clamp(10.0, 65_536.0) as u32,
            max_hourly_usd: pos(num(&budget, "max_hourly_usd"), f64::INFINITY),
            max_daily_usd: pos(num(&budget, "max_daily_usd"), f64::INFINITY),
            max_monthly_usd: pos(num(&budget, "max_monthly_usd"), f64::INFINITY),
            include_disk_costs: budget
                .get("include_disk_costs")
                .and_then(|x| x.as_bool())
                .unwrap_or(true),
        }
    }

    fn to_json(&self, original: &Value) -> Value {
        let mut out = original.as_object().cloned().unwrap_or_default();
        let cap = |x: f64| if x.is_finite() { json!(x) } else { Value::Null };
        out.insert("min_runners".into(), json!(self.min_runners));
        out.insert("runner_vcpu".into(), json!(self.runner_vcpu));
        out.insert("runner_ram_gib".into(), json!(self.runner_ram_gib));
        out.insert(
            "min_benchmark_score".into(),
            json!(self.min_benchmark_score),
        );
        out.insert("min_instance_vcpu".into(), json!(self.min_instance_vcpu));
        out.insert(
            "min_instance_ram_gib".into(),
            json!(self.min_instance_ram_gib),
        );
        out.insert("disk_category".into(), json!(self.disk_category));
        out.insert("data_disk_gb".into(), json!(self.data_disk_gb));
        let mut budget: Map<String, Value> = original
            .get("budget")
            .and_then(|b| b.as_object())
            .cloned()
            .unwrap_or_default();
        budget.insert("max_hourly_usd".into(), cap(self.max_hourly_usd));
        budget.insert("max_daily_usd".into(), cap(self.max_daily_usd));
        budget.insert("max_monthly_usd".into(), cap(self.max_monthly_usd));
        budget.insert("include_disk_costs".into(), json!(self.include_disk_costs));
        out.insert("budget".into(), Value::Object(budget));
        Value::Object(out)
    }
}

fn disk_matches(category: &str, disk_type: &str) -> bool {
    match category {
        "hyperdrive" => disk_type.starts_with("hyperdisk"),
        "normal" => disk_type.starts_with("pd-"),
        _ => true,
    }
}

/// Filters `catalog` by `requirements_json` and prices what's left. `role` is `cicd_runner` or
/// `rust_builder`. Rows keep their own fields; sizing, prices and `price_qualifies` are worked out
/// here. For runners, a row must be something a pool can launch: machine types that can't host
/// runners and zones this project can't use are left out, so every listed row can be launched.
pub fn qualify(role: &str, requirements_json: &Value, catalog: &[Value]) -> Value {
    let req = Requirements::from_json(requirements_json);
    let reserve = if role == "rust_builder" {
        4.0
    } else {
        crate::runner_pools::HOST_RESERVED_RAM_GIB as f64
    };
    let mut seen = std::collections::HashSet::new();
    let mut hardware: Vec<Value> = Vec::new();
    let (mut insufficient, mut below_bench, mut bad_disk, mut over_budget) = (0, 0, 0, 0);
    let mut cannot_host = 0;
    for row in catalog {
        let Some(obj) = row.as_object() else { continue };
        let mt = obj
            .get("machine_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let zone = obj.get("zone").and_then(|v| v.as_str()).unwrap_or("");
        if mt.is_empty() || !seen.insert(format!("{}|{}", mt, zone)) {
            continue;
        }
        if role == "cicd_runner"
            && (crate::runner_pools::machine_shape(mt).is_none()
                || !crate::runner_pools::is_known_gce_zone(zone))
        {
            cannot_host += 1;
            continue;
        }
        let shape = machine_shape(mt);
        let vcpu = num(row, "vcpu")
            .map(|v| v as u32)
            .or(shape.map(|s| s.0))
            .unwrap_or(0);
        let ram = num(row, "ram_gib").or(shape.map(|s| s.1)).unwrap_or(0.0);
        let by_cpu = vcpu / req.runner_vcpu;
        let by_ram = ((ram - reserve).max(0.0) / req.runner_ram_gib).floor() as u32;
        let units = by_cpu.min(by_ram);
        if vcpu < req.min_instance_vcpu || ram + 1e-9 < req.min_instance_ram_gib || units == 0 {
            insufficient += 1;
            continue;
        }
        if num(row, "benchmark_score").unwrap_or(0.0) < req.min_benchmark_score {
            below_bench += 1;
            continue;
        }
        let disk_type = obj
            .get("data_disk_type")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| default_disk_type(mt))
            .to_string();
        if !disk_matches(&req.disk_category, &disk_type) {
            bad_disk += 1;
            continue;
        }
        let stored_compute = row
            .get("pricing")
            .and_then(|p| num(p, "compute_hourly_usd"));
        let Some(compute) = compute_hourly_usd(mt).or(stored_compute) else {
            continue;
        };
        let disk = disk_hourly_usd(&disk_type, req.data_disk_gb);
        let host_hourly = compute + disk;
        let budget_host_hourly = if req.include_disk_costs {
            host_hourly
        } else {
            compute
        };
        let hosts = req.min_runners.div_ceil(units);
        let floor_hourly = budget_host_hourly * hosts as f64;
        let fits = floor_hourly <= req.max_hourly_usd + 1e-9
            && floor_hourly * 24.0 <= req.max_daily_usd + 1e-9
            && floor_hourly * HOURS_PER_MONTH <= req.max_monthly_usd + 1e-9;
        let mut out = obj.clone();
        out.insert("vcpu".into(), json!(vcpu));
        out.insert("ram_gib".into(), json!(ram));
        out.insert(
            "arch".into(),
            json!(if crate::runner_pools::is_arm64_machine_type(mt) {
                "arm64"
            } else {
                "x86_64"
            }),
        );
        out.insert("data_disk_type".into(), json!(disk_type));
        out.insert("data_disk_gb".into(), json!(req.data_disk_gb));
        out.insert("units_per_instance".into(), json!(units));
        out.insert("unit_vcpu".into(), json!(req.runner_vcpu));
        out.insert("unit_ram_gib".into(), json!(req.runner_ram_gib));
        out.insert("instances_needed_for_floor".into(), json!(hosts));
        out.insert("total_units_at_floor".into(), json!(hosts * units));
        out.insert("pricing".into(), pricing_json(compute, disk));
        out.insert("evaluated_hourly_usd".into(), json!(round4(host_hourly)));
        out.insert(
            "evaluated_daily_usd".into(),
            json!(round2(host_hourly * 24.0)),
        );
        out.insert(
            "evaluated_monthly_usd".into(),
            json!(round2(host_hourly * HOURS_PER_MONTH)),
        );
        out.insert("fleet_floor_hourly_usd".into(), json!(round4(floor_hourly)));
        out.insert(
            "fleet_floor_daily_usd".into(),
            json!(round2(floor_hourly * 24.0)),
        );
        out.insert(
            "fleet_floor_monthly_usd".into(),
            json!(round2(floor_hourly * HOURS_PER_MONTH)),
        );
        out.insert("price_qualifies".into(), json!(fits));
        if role == "cicd_runner" {
            // What one host adds to the pools' hourly budget check, as the server works it out.
            out.insert(
                "pool_host_hourly_usd".into(),
                json!(crate::state::machine_type_hourly_usd(mt)),
            );
        }
        if !fits {
            over_budget += 1;
        }
        hardware.push(Value::Object(out));
    }
    hardware.sort_by(|a, b| {
        let pa = num(a, "evaluated_hourly_usd").unwrap_or(f64::MAX);
        let pb = num(b, "evaluated_hourly_usd").unwrap_or(f64::MAX);
        pa.partial_cmp(&pb).unwrap_or(std::cmp::Ordering::Equal)
    });
    let spot: Vec<Value> = hardware
        .iter()
        .filter(|r| r.get("price_qualifies").and_then(|v| v.as_bool()) == Some(true))
        .cloned()
        .collect();
    json!({
        "role": role,
        "requirements": req.to_json(requirements_json),
        "hardware_qualifying_count": hardware.len(),
        "spot_price_qualifying_count": spot.len(),
        "hardware_qualifying": hardware,
        "spot_price_qualifying": spot,
        "disqualified_counts": {
            "insufficient_vcpu_or_ram": insufficient,
            "below_benchmark_score": below_bench,
            "incompatible_disk": bad_disk,
            "exceeds_spot_budget": over_budget,
            "exceeds_max_instances": 0,
            "region_disabled": 0,
            "cannot_host_runners": cannot_host,
        },
        "pricing_source": PRICING_SOURCE,
        "pricing_region": SPOT_PRICES_REGION,
        "pricing_as_of": SPOT_PRICES_AS_OF,
    })
}

/// Every offer the overview holds for `role` (both lists), plus the small runner hosts.
pub fn catalog_from_overview(overview: &Value, role: &str) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(q) = overview.get("qualifications").and_then(|q| q.get(role)) {
        for key in ["hardware_qualifying", "spot_price_qualifying"] {
            if let Some(list) = q.get(key).and_then(|v| v.as_array()) {
                out.extend(list.iter().cloned());
            }
        }
    }
    if role == "cicd_runner" {
        out.extend(small_host_offers());
    }
    out
}
