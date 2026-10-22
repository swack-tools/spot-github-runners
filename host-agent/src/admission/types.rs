use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdmissionAction {
    Acquire,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdmissionKind {
    Runner,
    Builder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionRequest {
    pub action: AdmissionAction,
    #[serde(default = "default_admission_kind")]
    pub kind: AdmissionKind,
}

fn default_admission_kind() -> AdmissionKind {
    AdmissionKind::Runner
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmissionResponse {
    #[serde(default)]
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AdmissionResponse {
    pub fn ok(accepted: bool) -> Self {
        Self {
            accepted,
            code: None,
            error: None,
        }
    }

    pub fn orphan(message: &str) -> Self {
        Self {
            accepted: false,
            code: Some("orphaned-runner-hook".to_string()),
            error: Some(format!("ORPHAN_RUNNER_HOOK: {}", message)),
        }
    }

    pub fn err(message: &str) -> Self {
        Self {
            accepted: false,
            code: None,
            error: Some(message.chars().take(200).collect()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleHeavyRunnerPolicy {
    pub name: String,
    pub memory_gib: u64,
    pub cpus: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionConfig {
    #[serde(default)]
    pub role: Option<String>,
    pub work_gib: f64,
    pub runner_gib: f64,
    pub builder_gib: f64,
    #[serde(default)]
    pub runner_names: Vec<String>,
    #[serde(default)]
    pub heavy_runner: Option<SingleHeavyRunnerPolicy>,
    #[serde(default)]
    pub heavy_runners: Option<Vec<String>>,
}

#[derive(Error, Debug)]
pub enum AdmissionError {
    #[error("Orphan runner hook: {0}")]
    OrphanRunnerHook(String),

    #[error("Unauthorized admission peer: UID {0}")]
    UnauthorizedPeer(u32),

    #[error("Invalid memory budget or headroom configuration: {0}")]
    InvalidBudget(String),

    #[error("Transport error: {0}")]
    Transport(String),

    #[error("Server error: {0}")]
    Server(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("System error: {0}")]
    System(String),
}
