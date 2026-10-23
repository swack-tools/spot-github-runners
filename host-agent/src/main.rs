use std::path::Path;
use std::process::ExitCode;

use host_agent::{
    admission, builder_entrypoint, clean_target, docker_admission, docker_wrapper, host_launcher,
    toolchain_probe, worktree_commands,
};

fn is_builder_environment() -> bool {
    Path::new("/etc/oxidex-remote-build.json").exists()
        || Path::new("/etc/example-rust-project-remote-build.json").exists()
        || std::env::var_os("OXIDEX_BUILDER").is_some()
        || std::env::var_os("EXAMPLE_RUST_PROJECT_BUILDER").is_some()
        || std::env::var_os("OXIDEX_BUILDER_DOCKER_ADMISSION").is_some()
}

fn print_usage() {
    eprintln!(
        "host-agent 0.1.0 — Pure-Rust runner admission, target cleanup & builder runtime\n\n\
         USAGE:\n    \
             host-agent <SUBCOMMAND> [ARGS...]\n    \
             <symlink_alias> [ARGS...]\n\n\
         SUBCOMMANDS:\n    \
             clean-target        Descriptor-relative safe Cargo target cleanup\n    \
             admission           Memory lease reservation, daemon & headroom\n    \
             docker-wrapper      Runner GHA network interception & socat relay\n    \
             docker-admission    Builder strict resource and mount admission\n    \
             host-launcher       Dedicated builder root launcher & source cleanup\n    \
             builder-entrypoint  Rust 1.99.0 toolchain verification & entrypoint\n    \
             worktree-commands   Dual worktree runner with cgroup v2 metrics\n    \
             toolchain-probe     Toolchain, browser & cgroup verification probe\n"
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().map(String::as_str).unwrap_or("host-agent");
    let exec_name = Path::new(argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("host-agent");

    let result = match exec_name {
        "gha-runner-clean-target" | "clean-target" => clean_target::run(&args[1..]),
        "gha-host-admission" | "host-admission" => admission::run(&args[1..]),
        "docker" => {
            if is_builder_environment() {
                docker_admission::run(&args[1..])
            } else {
                docker_wrapper::run(&args[1..])
            }
        }
        "oxidex-remote-build" | "example-rust-project-remote-build" | "host-launcher" => {
            host_launcher::run(&args[1..])
        }
        "oxidex-build-entrypoint"
        | "example-rust-project-build-entrypoint"
        | "builder-entrypoint" => builder_entrypoint::run(&args[1..]),
        "oxidex-worktree-commands"
        | "example-rust-project-worktree-commands"
        | "worktree-commands" => worktree_commands::run(&args[1..]),
        "oxidex-toolchain-probe" | "example-rust-project-toolchain-probe" | "toolchain-probe" => {
            toolchain_probe::run(&args[1..])
        }
        _ => {
            if args.len() < 2 {
                print_usage();
                return ExitCode::from(1);
            }
            match args[1].as_str() {
                "clean-target" => clean_target::run(&args[2..]),
                "admission" | "host-admission" => admission::run(&args[2..]),
                "docker-wrapper" => docker_wrapper::run(&args[2..]),
                "docker-admission" => docker_admission::run(&args[2..]),
                "host-launcher" | "oxidex-remote-build" | "example-rust-project-remote-build" => {
                    host_launcher::run(&args[2..])
                }
                "builder-entrypoint"
                | "oxidex-build-entrypoint"
                | "example-rust-project-build-entrypoint" => builder_entrypoint::run(&args[2..]),
                "worktree-commands"
                | "oxidex-worktree-commands"
                | "example-rust-project-worktree-commands" => worktree_commands::run(&args[2..]),
                "toolchain-probe"
                | "oxidex-toolchain-probe"
                | "example-rust-project-toolchain-probe" => toolchain_probe::run(&args[2..]),
                "--help" | "-h" | "help" => {
                    print_usage();
                    return ExitCode::SUCCESS;
                }
                "--version" | "-V" => {
                    println!("host-agent 0.1.0");
                    return ExitCode::SUCCESS;
                }
                unknown => {
                    eprintln!("host-agent: unknown subcommand '{unknown}'");
                    print_usage();
                    return ExitCode::from(1);
                }
            }
        }
    };

    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("host-agent error: {err}");
            ExitCode::from(1)
        }
    }
}
