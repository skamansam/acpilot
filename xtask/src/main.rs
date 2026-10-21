//! Development tasks, reachable as `cargo xtask <command>`.
//!
//! The point is that CI and a developer's shell run the *same* commands, so
//! "works for me" means something. Commands that belong to phases which have not
//! shipped yet fail loudly rather than pretending to succeed.

use std::fs;
use std::path::Path;
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
    /// Regenerate the theme data the documentation site renders.
    DocsData,
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
        Task::DocsData => run_docs_data(),
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
        // Same command as `.github/workflows/ci.yml`, not `cargo test`: a local
        // run that differs from CI is a place for "works for me" to hide.
        // nextest runs each test in its own process and does not run doctests,
        // which is also what CI does today.
        (
            "test",
            vec!["nextest", "run", "--workspace", "--no-fail-fast"],
        ),
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

/// Write `site/src/data/themes.json` from the built-in extensions.
///
/// The site does not parse TOML at build time and does not reimplement contrast:
/// it renders the palettes and ratios the binary actually ships, so a gallery
/// image cannot disagree with the gate that enforces it.
fn run_docs_data() -> ExitCode {
    let registry = match acpilot_extensions::Registry::built_in() {
        Ok(registry) => registry,
        Err(errors) => {
            for error in errors {
                eprintln!("{error}");
            }
            return ExitCode::FAILURE;
        }
    };

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../site/src/data/themes.json");
    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        eprintln!("{}: {error}", parent.display());
        return ExitCode::FAILURE;
    }
    if let Err(error) = fs::write(&path, acpilot_extensions::docs::themes_json(&registry)) {
        eprintln!("{}: {error}", path.display());
        return ExitCode::FAILURE;
    }

    println!("wrote {}", path.display());
    ExitCode::SUCCESS
}

/// Report a task that cannot run yet, with the phase that will make it real.
fn not_yet(name: &str, owner: &str) -> ExitCode {
    eprintln!("xtask {name}: not implemented yet — {owner}");
    ExitCode::from(2)
}
