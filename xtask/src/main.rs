//! Development tasks, reachable as `cargo xtask <command>`.
//!
//! The point is that CI and a developer's shell run the *same* commands, so
//! "works for me" means something. Commands that belong to phases which have not
//! shipped yet fail loudly rather than pretending to succeed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Acpilot development tasks")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Everything CI enforces that can run locally: formatting, lints, the
    /// workspace tests, and the design phase's P0-5 probe. `wsl-bench` needs a WSL2 runner and
    /// stays out.
    Check,
    /// The checks of the design phase's P0-5 probe alone. CI's `probes` job runs this, so the six
    /// steps are written once — here — and cannot drift from what a developer
    /// runs locally.
    Probe,
    /// Build a client, or the daemon it talks to, without running anything.
    ///
    /// `build` rather than `cargo build` for two reasons a newcomer pays for
    /// once each: the TUI is a library inside the `acpilot` binary *and* spawns
    /// `acpilotd` as a sibling, so building it means building two packages; and
    /// the desktop app sits behind a `gui` feature that a plain `cargo run`
    /// forgets. Both facts live here instead of in a wiki page.
    Build {
        /// What to build: `tui`, `gui`, `daemon`, or `all`.
        #[arg(value_enum)]
        app: App,
        /// An optimised build. Off by default: a debug build is what a
        /// development loop wants, and it is several times faster to produce.
        #[arg(long)]
        release: bool,
    },
    /// Run a client, or the daemon, in the foreground as a debug build.
    ///
    /// Arguments after `--` go to the program, so the flags are the program's
    /// own: `cargo xtask run tui -- .` opens this directory, and
    /// `cargo xtask run daemon -- --profile DIR` is the daemon's flag, not this
    /// task's.
    Run {
        /// What to run: `tui`, `gui`, or `daemon`.
        #[arg(value_enum)]
        app: App,
        /// Arguments for the program, verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run one app's tests.
    ///
    /// `test gui` is the reason this exists: the shell's tests need the `gui`
    /// feature *and* gpui's headless `test-support` harness, a pair the
    /// workspace gate deliberately does not compile. `test tui` and `test
    /// daemon` are the same packages the build task names, so a change to one
    /// frontend has a single command that covers it and its daemon.
    Test {
        /// What to test: `tui`, `gui`, `daemon`, or `all`.
        #[arg(value_enum)]
        app: App,
    },
    /// Regenerate the theme data the documentation site renders.
    DocsData,
    /// Write the published `agents.toml` JSON Schema.
    Schema,
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
        Task::Build { app, release } => {
            if build(app, release) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Task::Run { app, args } => run_app(app, &args),
        Task::Test { app } => {
            if test(app) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Task::DocsData => run_docs_data(),
        Task::Schema => run_schema(),
        Task::Dev => not_yet("dev", "P3 owns the daemon and P11 the web dev server"),
        Task::WebBuild => not_yet("web-build", "P11 adds the web frontend"),
        Task::Dist => not_yet("dist", "P12 adds packaging"),
    }
}

/// The programs a developer builds and runs, named the way they are talked
/// about: the TUI, the desktop shell, the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum App {
    /// The terminal UI: the `acpilot` binary's `tui` subcommand.
    Tui,
    /// The desktop shell, which only compiles behind `--features gui`.
    Gui,
    /// The daemon on its own, for a client to attach to or a debugger to hold.
    Daemon,
    /// Both clients and the daemon they talk to. A build target, not something
    /// to run: there is one foreground process per terminal.
    All,
}

impl App {
    fn label(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Gui => "gui",
            Self::Daemon => "daemon",
            Self::All => "all",
        }
    }
}

/// The cargo invocations that produce `app`.
///
/// Two facts that a plain `cargo build -p acpilot` hides, and that cost a
/// newcomer an afternoon each:
///
/// * the TUI is a library inside the `acpilot` binary, and `acpilot tui` starts
///   `acpilotd` as a *sibling binary* rather than linking it, so building the
///   TUI means building two packages — a client whose daemon is missing only
///   finds that out at launch;
/// * the desktop app is behind a `gui` feature that is off by default, so that
///   the workspace gate never compiles a renderer or needs a graphics stack.
fn build_commands(app: App) -> Vec<Vec<&'static str>> {
    match app {
        App::Tui => vec![vec!["-p", "acpilot-cli", "-p", "acpilotd"]],
        App::Gui => vec![vec!["-p", "acpilot-gui", "--features", "gui"]],
        App::Daemon => vec![vec!["-p", "acpilotd"]],
        // One invocation per feature set: `--features gui` applies to every
        // selected package, and the CLI does not have that feature.
        App::All => vec![
            vec!["-p", "acpilot-cli", "-p", "acpilotd"],
            vec!["-p", "acpilot-gui", "--features", "gui"],
        ],
    }
}

/// Build every package `app` needs, stopping at the first failure.
fn build(app: App, release: bool) -> bool {
    for arguments in build_commands(app) {
        let mut command = Command::new(env!("CARGO"));
        command.arg("build").args(&arguments);
        if release {
            command.arg("--release");
        }
        if !run_step(&format!("build ({})", app.label()), command) {
            warn_about_graphics(app);
            return false;
        }
    }
    true
}

/// The cargo invocations that test `app`.
///
/// `gui` is the one that is not the workspace gate: the shell's tests need the
/// `gui` feature and gpui's headless `test-support` harness, a pair the plain
/// gate does not compile on purpose. The TUI's own tests live in its crate, not
/// in the `acpilot` binary that links it, so the TUI set names that crate too.
fn test_commands(app: App) -> Vec<Vec<&'static str>> {
    match app {
        App::Tui => vec![vec![
            "-p",
            "acpilot-cli",
            "-p",
            "acpilot-tui",
            "-p",
            "acpilotd",
        ]],
        // `--no-fail-fast` for the same reason the workspace run has it: a
        // second failing test is another thing to fix in the same edit, and
        // CI's gui job has always run this way.
        App::Gui => vec![vec![
            "-p",
            "acpilot-gui",
            "--features",
            "gui,gui-tests",
            "--no-fail-fast",
        ]],
        App::Daemon => vec![vec!["-p", "acpilotd"]],
        // The workspace first — the same run the gate does — and then the shell,
        // which the gate left out.
        App::All => vec![
            vec!["--workspace", "--no-fail-fast"],
            vec![
                "-p",
                "acpilot-gui",
                "--features",
                "gui,gui-tests",
                "--no-fail-fast",
            ],
        ],
    }
}

fn test(app: App) -> bool {
    for arguments in test_commands(app) {
        let mut command = Command::new(env!("CARGO"));
        command.args(["nextest", "run"]).args(&arguments);
        if !run_step(&format!("test ({})", app.label()), command) {
            warn_about_graphics(app);
            return false;
        }
    }
    true
}

/// Say where gpui's Linux libraries come from when a build or a test links it.
///
/// The failure is almost never the code: gpui links a dev symlink that is its
/// own package on Debian/Ubuntu, and `unable to find library -lxkbcommon-x11`
/// does not say where to get it. The list is in `crates/acpilot-gui/Cargo.toml`
/// and ADR 0009 §5; naming it here is the afternoon it saves.
fn warn_about_graphics(app: App) {
    if cfg!(target_os = "linux") && matches!(app, App::Gui | App::All) {
        eprintln!(
            "==> the desktop shell needs gpui's system libraries — on Debian/Ubuntu: \
             libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libwayland-dev \
             libfreetype-dev libfontconfig1-dev pkg-config clang"
        );
    }
}

/// What `cargo run` needs for `app`: the package, its features, and the
/// subcommand that reaches the program when the package ships several.
///
/// [`None`] for [`App::All`], which is a build target.
fn run_package(app: App) -> Option<&'static [&'static str]> {
    match app {
        App::Tui => Some(&["-p", "acpilot-cli"]),
        App::Gui => Some(&["-p", "acpilot-gui", "--features", "gui"]),
        App::Daemon => Some(&["-p", "acpilotd"]),
        App::All => None,
    }
}

fn run_entry(app: App) -> &'static [&'static str] {
    match app {
        App::Tui => &["tui"],
        App::Gui | App::Daemon | App::All => &[],
    }
}

fn run_app(app: App, args: &[String]) -> ExitCode {
    let Some(package) = run_package(app) else {
        eprintln!("xtask run: name one of tui, gui or daemon — `all` is a build target");
        return ExitCode::from(2);
    };

    // Built before `cargo run` rather than left to it: the TUI starts `acpilotd`
    // as the sibling of its own executable, and running the client alone would
    // start a daemon only if one had been built at some point before. Cargo
    // makes the second build of the same target a no-op.
    if !build(app, false) {
        return ExitCode::FAILURE;
    }

    let mut command = Command::new(env!("CARGO"));
    command.arg("run").args(package).arg("--");
    command.args(run_entry(app)).args(args);
    if run_step(&format!("run ({})", app.label()), command) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
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

    // The workspace is not the whole gate: the design phase's P0-5 probe is deliberately not a
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

/// The design phase's P0-5 probe: a standalone crate plus a helper that cargo never compiles.
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

/// Write every published JSON Schema under `site/public/schema/`.
///
/// The files are in `public/` because that is where the docs site serves them
/// from, and each schema's `$id` is that URL: publishing the docs publishes the
/// schema. Generated rather than hand-edited, so an artifact cannot describe a
/// file the loader refuses — a drift test in each crate compares this output with
/// the committed copy.
fn run_schema() -> ExitCode {
    let schemas = [
        (
            acpilot_agent::SCHEMA_PATH,
            acpilot_agent::schema_json_text(),
        ),
        (
            acpilot_extensions::SCHEMA_PATH,
            acpilot_extensions::schema_json_text(),
        ),
    ];

    for (relative, text) in schemas {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(relative);
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            eprintln!("{}: {error}", parent.display());
            return ExitCode::FAILURE;
        }
        if let Err(error) = fs::write(&path, &text) {
            eprintln!("{}: {error}", path.display());
            return ExitCode::FAILURE;
        }
        println!("wrote {} ({} bytes)", path.display(), text.len());
    }

    ExitCode::SUCCESS
}

/// Report a task that cannot run yet, with the phase that will make it real.
fn not_yet(name: &str, owner: &str) -> ExitCode {
    eprintln!("xtask {name}: not implemented yet — {owner}");
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn building_the_tui_builds_the_daemon_it_spawns() {
        // `acpilot tui` looks for `acpilotd` beside its own executable, so a TUI
        // build that left the daemon behind would launch and then fail.
        assert_eq!(
            build_commands(App::Tui),
            vec![vec!["-p", "acpilot-cli", "-p", "acpilotd"]]
        );
    }

    #[test]
    fn the_gui_is_built_and_run_behind_its_feature() {
        assert_eq!(
            build_commands(App::Gui),
            vec![vec!["-p", "acpilot-gui", "--features", "gui"]]
        );
        assert_eq!(
            run_package(App::Gui),
            Some(&["-p", "acpilot-gui", "--features", "gui"][..])
        );
    }

    #[test]
    fn all_builds_both_clients_and_is_not_something_to_run() {
        assert_eq!(build_commands(App::All).len(), 2);
        assert_eq!(run_package(App::All), None);
    }

    #[test]
    fn testing_the_gui_asks_for_the_harness_the_gate_leaves_out() {
        assert_eq!(
            test_commands(App::Gui),
            vec![vec![
                "-p",
                "acpilot-gui",
                "--features",
                "gui,gui-tests",
                "--no-fail-fast",
            ]]
        );
        // `all` is the gate plus the shell: two runs, in that order.
        let all = test_commands(App::All);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], vec!["--workspace", "--no-fail-fast"]);
        assert!(all[1].contains(&"gui,gui-tests"));
    }

    #[test]
    fn testing_the_tui_covers_its_own_crate_and_the_daemon() {
        let commands = test_commands(App::Tui);
        assert_eq!(commands.len(), 1);
        assert!(commands[0].contains(&"acpilot-tui"));
        assert!(commands[0].contains(&"acpilotd"));
    }

    #[test]
    fn running_the_tui_asks_the_cli_for_its_tui_subcommand() {
        assert_eq!(run_package(App::Tui), Some(&["-p", "acpilot-cli"][..]));
        assert_eq!(run_entry(App::Tui), &["tui"]);
        // The daemon and the GUI are their own binaries; nothing goes between
        // `--` and the caller's arguments.
        assert!(run_entry(App::Daemon).is_empty());
        assert!(run_entry(App::Gui).is_empty());
    }
}
