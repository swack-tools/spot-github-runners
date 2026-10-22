use crate::admission::types::{AdmissionConfig, AdmissionError};

#[derive(Debug, Clone, PartialEq)]
pub struct AdmissionBudgetResult {
    pub reserve_gib: f64,
    pub work_gib: f64,
    pub concurrency: u32,
}

#[derive(Debug, Clone)]
pub struct RunnerTier {
    pub initial_slots: u32,
    pub maximum_slots: u32,
    pub ram_gib: f64,
}

pub fn admission_budget(
    tier: &RunnerTier,
    observed_overhead_gib: f64,
    workload_peak_gib: f64,
    slots: Option<u32>,
) -> Result<AdmissionBudgetResult, AdmissionError> {
    let slots = slots.unwrap_or(tier.initial_slots);
    if slots < 1
        || slots > tier.maximum_slots
        || observed_overhead_gib < 0.0
        || observed_overhead_gib >= tier.ram_gib
    {
        return Err(AdmissionError::InvalidBudget(
            "Invalid host overhead or runner slots".to_string(),
        ));
    }
    if !workload_peak_gib.is_finite() || workload_peak_gib <= 0.0 {
        return Err(AdmissionError::InvalidBudget(
            "Invalid workload memory reservation".to_string(),
        ));
    }

    let reserve = observed_overhead_gib.max(2.0) + 0.25 * (slots as f64);
    let work = tier.ram_gib - reserve;
    if work < workload_peak_gib {
        return Err(AdmissionError::InvalidBudget(
            "Workload cannot fit after host/listener reserve".to_string(),
        ));
    }

    let concurrency = (slots as f64).min((work / workload_peak_gib).floor()) as u32;
    Ok(AdmissionBudgetResult {
        reserve_gib: reserve,
        work_gib: work,
        concurrency,
    })
}

pub fn builder_memory_budget(total_gib: f64, available_gib: f64, working_gib: f64) -> f64 {
    let values = [total_gib, available_gib, working_gib];
    if values.iter().any(|&v| !v.is_finite() || v < 0.0) {
        return 0.0;
    }
    if available_gib > total_gib || working_gib > (total_gib - available_gib) {
        return 0.0;
    }

    let reserve = (total_gib - available_gib - working_gib).max(2.0);
    let envelope = total_gib - reserve;
    if envelope >= 28.0 {
        28.0
    } else {
        0.0
    }
}

pub fn parse_docker_memory_usage(usage: &str) -> Result<f64, AdmissionError> {
    // Expected format: "<number><unit> / <number><unit>" or just "<number><unit>"
    let token = usage.split('/').next().unwrap_or("").trim();
    let re = regex::Regex::new(r"^\s*([0-9.]+)\s*(B|kB|MB|GB|TB|KiB|MiB|GiB|TiB)\s*$")
        .map_err(|e| AdmissionError::System(e.to_string()))?;

    let caps = re.captures(token).ok_or_else(|| {
        AdmissionError::InvalidBudget(format!("Unrecognized Docker memory usage: '{token}'"))
    })?;

    let value: f64 = caps[1].parse().map_err(|_| {
        AdmissionError::InvalidBudget("Failed to parse numeric memory value".to_string())
    })?;
    let unit = &caps[2];

    let multiplier: f64 = match unit {
        "B" => 1.0,
        "kB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "TB" => 1_000_000_000_000.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => {
            return Err(AdmissionError::InvalidBudget(
                "Unsupported memory unit".to_string(),
            ))
        }
    };

    let bytes = value * multiplier;
    Ok(bytes / (1024.0 * 1024.0 * 1024.0)) // Return GiB
}

pub fn memory_envelope(
    config: &AdmissionConfig,
    total_gib: f64,
    used_gib: f64,
    container_usages: &[String],
) -> Result<f64, AdmissionError> {
    let mut working = 0.0;
    for usage in container_usages {
        working += parse_docker_memory_usage(usage)?;
    }
    let reserve = (used_gib - working).max(2.0) + 0.25 * (config.runner_names.len() as f64);
    let available_work = (total_gib - reserve).min(config.work_gib);
    Ok(available_work.max(0.0))
}
