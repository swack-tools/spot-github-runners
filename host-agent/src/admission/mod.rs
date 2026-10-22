pub mod budget;
pub mod client;
pub mod headroom;
pub mod peer;
pub mod pool;
pub mod server;
pub mod types;

use std::path::PathBuf;

use crate::platform::{DefaultSystemInspector, SystemInspector};

pub use budget::{
    admission_budget, builder_memory_budget, memory_envelope, parse_docker_memory_usage,
    AdmissionBudgetResult, RunnerTier,
};
pub use client::{acquire_build, acquire_runner, release_build, release_runner, request};
pub use peer::{legacy_owner_for_worker, owner_alive, peer_owner, runner_cgroup, worker_owner};
pub use pool::LeasePool;
pub use server::run_server;
pub use types::{
    AdmissionAction, AdmissionConfig, AdmissionError, AdmissionKind, AdmissionRequest,
    AdmissionResponse, SingleHeavyRunnerPolicy,
};

pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let sys = DefaultSystemInspector::default();
    run_with_sys(args, &sys)
}

pub fn run_with_sys(
    args: &[String],
    sys: &dyn SystemInspector,
) -> Result<i32, Box<dyn std::error::Error>> {
    if args.is_empty() {
        eprintln!(
            "host-admission: missing subcommand (serve, acquire, release, headroom, reconcile-builders)"
        );
        return Ok(1);
    }

    match args[0].as_str() {
        "serve" => {
            let mut socket_path = PathBuf::from("/run/gha-admission/socket");
            let mut config_path = PathBuf::from("/etc/gha-admission.json");
            let mut ledger_path = PathBuf::from("/var/lib/gha-admission/leases.json");

            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--socket-path" if i + 1 < args.len() => {
                        socket_path = PathBuf::from(&args[i + 1]);
                        i += 1;
                    }
                    "--config-path" if i + 1 < args.len() => {
                        config_path = PathBuf::from(&args[i + 1]);
                        i += 1;
                    }
                    "--ledger-path" if i + 1 < args.len() => {
                        ledger_path = PathBuf::from(&args[i + 1]);
                        i += 1;
                    }
                    _ => {}
                }
                i += 1;
            }

            run_server(&socket_path, &config_path, &ledger_path, sys)?;
            Ok(0)
        }
        "acquire" => {
            let is_builder = args.iter().any(|a| a == "--builder");
            let socket_path = args
                .iter()
                .position(|a| a == "--socket-path")
                .and_then(|i| args.get(i + 1).map(String::as_str));

            if is_builder {
                acquire_build(socket_path)
                    .map(|_| 0)
                    .or_else(|code| Ok(code))
            } else {
                match acquire_runner(socket_path) {
                    Ok(_) => Ok(0),
                    Err(AdmissionError::OrphanRunnerHook(msg)) => {
                        eprintln!("host-admission: {msg}");
                        Ok(75)
                    }
                    Err(err) => {
                        eprintln!("host-admission error: {err}");
                        Ok(1)
                    }
                }
            }
        }
        "release" => {
            let is_builder = args.iter().any(|a| a == "--builder");
            let socket_path = args
                .iter()
                .position(|a| a == "--socket-path")
                .and_then(|i| args.get(i + 1).map(String::as_str));

            if is_builder {
                release_build(socket_path);
                Ok(0)
            } else {
                release_runner(socket_path)
                    .map(|_| 0)
                    .or_else(|code| Ok(code))
            }
        }
        "headroom" => headroom::run_headroom(&args[1..], sys),
        "reconcile-builders" => {
            let has_lock = args.iter().any(|a| a == "--global-lock-held");
            if !has_lock {
                eprintln!("Root drain requires its held global lock (--global-lock-held)");
                return Ok(1);
            }
            if sys.builders_running(true) {
                Ok(75)
            } else {
                Ok(0)
            }
        }
        unknown => {
            eprintln!("Unknown admission subcommand: '{unknown}'");
            Ok(1)
        }
    }
}
