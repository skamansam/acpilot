//! Development tasks, reachable as `cargo xtask <command>`.
//!
//! The point is that CI and a developer's shell run the *same* commands, so
//! "works for me" means something. Commands that belong to phases which have not
//! shipped yet fail loudly rather than pretending to succeed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Acpilot development tasks")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Everything CI enforces that can run locally: formatting, lints, the
    /// workspace tests, and the P0-5 probe. `wsl-bench` needs a WSL2 runner and
    /// stays out.
    Check,
    /// The P0-5 probe's checks alone. CI's `probes` job runs this, so the six
    /// steps are written once — here — and cannot drift from what a developer
    /// runs locally.
    Probe,
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
        Task::Probe => {
            if check_probe() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
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

    // The workspace is not the whole gate: the P0-5 probe is deliberately not a
    // member, so `--workspace` never reaches it. Checking it here keeps one
    // command for "everything CI enforces".
    if !check_platforms() {
        return ExitCode::FAILURE;
    }

    if !check_probe() {
        return ExitCode::FAILURE;
    }

    println!("==> all checks passed");
    ExitCode::SUCCESS
}

/// The other half of each platform pair, type-checked from here.
///
/// Unix and macOS are built first and Windows last, which otherwise means a
/// Windows-only module can stop compiling without anyone finding out until a CI
/// runner says so. `cargo check` type-checks `cfg`'d code for another target
/// without linking, so the unix machine everyone is sitting at can catch it.
///
/// This is a convenience, not the authority: CI's matrix builds every platform
/// natively (`cargo clippy --all-targets` on windows-latest and macos-latest), and
/// that is what actually protects `main`. A target whose std is not installed is
/// skipped with a printed note — a developer is not expected to carry all three.
fn check_platforms() -> bool {
    const TARGETS: [&str; 3] = [
        "x86_64-pc-windows-msvc",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ];

    let installed = installed_targets();
    let mut ok = true;
    for target in TARGETS {
        if !installed.iter().any(|installed| installed == target) {
            println!("==> check (platform, {target}): skipped, target not installed");
            continue;
        }
        let mut command = Command::new(env!("CARGO"));
        command.args([
            "check",
            // The platform crate holds the only `cfg` branches, so it is the one
            // that has to compile everywhere. `--all-targets` includes the
            // Windows-only test module, which a plain check would skip.
            "-p",
            "acpilot-host",
            "--all-targets",
            // Optional transports are off: `russh`'s crypto backend compiles C
            // for the target, which needs that target's toolchain, and the SSH
            // module has no `cfg` in it — there is nothing platform-specific to
            // learn by cross-checking it. CI builds it natively on each OS.
            "--no-default-features",
            "--target",
            target,
        ]);
        ok &= run_step(&format!("check (platform, {target})"), command);
    }

    if installed.is_empty() {
        println!("==> check (platform): skipped, rustup did not list any targets");
    }
    ok
}

fn installed_targets() -> Vec<String> {
    let Ok(output) = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// The P0-5 probe: a standalone crate plus a helper that cargo never compiles.
///
/// Same commands as the `p0-5 probe` job in `.github/workflows/ci.yml`, spawned
/// directly rather than through a shell — a bash bundled with an editor is not
/// part of the toolchain, and several agents may run this.
fn check_probe() -> bool {
    let dir = probe_dir();
    let manifest = dir.join("Cargo.toml");
    let target = probe_target_dir(&dir);
    if let Some(target) = &target {
        println!("==> probe target dir: {}", target.display());
    }

    let cargo = env!("CARGO");
    let mut ok = true;

    let mut fmt = probe_command(cargo, target.as_deref());
    fmt.args(["fmt", "--manifest-path"])
        .arg(&manifest)
        .args(["--", "--check"]);
    ok &= run_step("fmt (probe)", fmt);

    // Cargo never sees the helper: on the Linux side it is deliberately not a
    // cargo project, so rustfmt is invoked directly, as CI does.
    let mut rustfmt = probe_command("rustfmt", None);
    rustfmt.args(["--check", "helper/acpilotd-helper.rs"]);
    ok &= run_step("rustfmt (helper)", rustfmt);

    let mut clippy = probe_command(cargo, target.as_deref());
    clippy
        .args(["clippy", "--manifest-path"])
        .arg(&manifest)
        .args(["--all-targets", "--", "-D", "warnings"]);
    ok &= run_step("clippy (probe)", clippy);

    let mut test = probe_command(cargo, target.as_deref());
    test.args(["test", "--manifest-path"]).arg(&manifest);
    ok &= run_step("test (probe)", test);

    let exe = if cfg!(windows) { ".exe" } else { "" };
    let helper_test = std::env::temp_dir().join(format!("acpilot-helper-test{exe}"));
    let mut build_test = probe_command("rustc", None);
    build_test
        .args(["helper/acpilotd-helper.rs", "--test", "-o"])
        .arg(&helper_test);
    if run_step("test (helper, build)", build_test) {
        ok &= run_step("test (helper, run)", Command::new(&helper_test));
    } else {
        ok = false;
    }

    match python() {
        None => println!("==> helper protocol: skipped (no python3 or python on PATH)"),
        Some(python) => {
            let helper = std::env::temp_dir().join(format!("acpilotd-helper{exe}"));
            let mut build = probe_command("rustc", None);
            build
                .args(["-O", "helper/acpilotd-helper.rs", "-o"])
                .arg(&helper);
            if run_step("build helper", build) {
                let mut protocol = probe_command(python, None);
                protocol.arg("helper/check_protocol.py").arg(&helper);
                ok &= run_step("helper protocol", protocol);
            } else {
                ok = false;
            }
        }
    }

    ok
}

fn probe_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../probes/wsl-helper")
}

fn probe_command(program: &str, target: Option<&Path>) -> Command {
    let mut command = Command::new(program);
    command.current_dir(probe_dir());
    if let Some(target) = target {
        command.env("CARGO_TARGET_DIR", target);
    }
    command
}

/// A checkout reached over `\\wsl.localhost` would otherwise build the probe
/// into the `target/` the Linux toolchain also owns, where the Windows build
/// fails on files it cannot replace. An explicit `CARGO_TARGET_DIR` wins.
fn probe_target_dir(dir: &Path) -> Option<PathBuf> {
    if std::env::var_os("CARGO_TARGET_DIR").is_some() {
        return None;
    }
    dir.to_string_lossy()
        .starts_with(r"\\")
        .then(|| std::env::temp_dir().join("acpilot-wsl-helper-target"))
}

fn run_step(name: &str, mut command: Command) -> bool {
    println!("==> {name}");
    match command.status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("==> {name} failed ({status})");
            false
        }
        Err(error) => {
            eprintln!("==> {name}: {error}");
            false
        }
    }
}

/// `python3` on Unix, `python` on Windows; whichever answers `--version`.
fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
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
