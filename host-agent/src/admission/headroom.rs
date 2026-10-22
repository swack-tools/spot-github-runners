use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::admission::budget::builder_memory_budget;
use crate::platform::SystemInspector;

pub fn run_headroom(
    args: &[String],
    sys: &dyn SystemInspector,
) -> Result<i32, Box<dyn std::error::Error>> {
    let mut json_output = false;
    let mut check_builder = false;
    let mut ledger_path = PathBuf::from("/var/lib/gha-admission/leases.json");

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json_output = true,
            "--check-builder" => check_builder = true,
            "--ledger-path" if i + 1 < args.len() => {
                ledger_path = PathBuf::from(&args[i + 1]);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    let (mem_total, mem_available) = sys
        .read_meminfo()
        .map_err(|e| format!("Cannot read meminfo: {e}"))?;

    let active_leases: HashMap<String, f64> = if ledger_path.exists() {
        let content = fs::read_to_string(&ledger_path)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        HashMap::new()
    };

    let leased_sum: f64 = active_leases.values().sum();
    let budget = builder_memory_budget(mem_total, mem_available, 0.0);

    if check_builder {
        if budget >= 28.0 {
            println!("OK: 28 GiB builder fits");
            Ok(0)
        } else {
            println!("HOLD: Actual host cannot fit 28GiB builder with host reserve");
            Ok(75)
        }
    } else if json_output {
        let record = serde_json::json!({
            "mem_total_gib": mem_total,
            "mem_available_gib": mem_available,
            "leased_gib": leased_sum,
            "builder_budget_gib": budget,
            "active_leases": active_leases,
        });
        println!("{}", serde_json::to_string_pretty(&record)?);
        Ok(0)
    } else {
        println!(
            "Host Memory: total={:.2} GiB, available={:.2} GiB, leased={:.2} GiB, builder_budget={:.2} GiB",
            mem_total, mem_available, leased_sum, budget
        );
        Ok(0)
    }
}
