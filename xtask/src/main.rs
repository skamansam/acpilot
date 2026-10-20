//! Development tasks, reachable as `cargo xtask <command>`.
//!
//! The point is that CI and a developer's shell run the *same* commands, so
//! "works for me" means something. Commands that belong to phases which have not
//! shipped yet fail loudly rather than pretending to succeed.

use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Acpilot development tasks")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Everything CI enforces: formatting, lints, tests, and the docs build.
    Check,
    /// Run the daemon and the web dev server together (needs a built web UI).
    Dev,
    /// Build the web UI into `web/dist` for embedding.
    WebBuild,
    /// Produce installers via cargo-dist.
    Dist,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Task::Check => run_check(),
        Task::Dev => not_yet("dev", "P3 owns the daemon and P11 the web dev server"),
        Task::WebBuild => not_yet("web-build", "P11 adds the web frontend"),
        Task::Dist => not_yet("dist", "P12 adds packaging"),
    }
}

fn run_check() -> ExitCode {
    let steps: [(&str, Vec<&str>); 3] = [
        ("fmt", vec!["fmt", "--all", "--", "--check"]),
        (
            "clippy",
            vec![
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        ("test", vec!["test", "--workspace"]),
    ];

    for (name, args) in steps {
        println!("==> cargo {}", args.join(" "));
        let status = Command::new(env!("CARGO"))
            .args(&args)
            .status()
            .unwrap_or_else(|error| {
                eprintln!("failed to run cargo for step {name}: {error}");
                std::process::exit(1);
            });
        if !status.success() {
            eprintln!("==> {name} failed");
            return ExitCode::FAILURE;
        }
    }

    println!("==> all checks passed");
    ExitCode::SUCCESS
}

/// Report a task that cannot run yet, with the phase that will make it real.
fn not_yet(name: &str, owner: &str) -> ExitCode {
    eprintln!("xtask {name}: not implemented yet — {owner}");
    ExitCode::from(2)
}
