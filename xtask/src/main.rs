//! Development tasks, reachable as `cargo xtask <command>`.
//!
//! The point is that CI and a developer's shell run the *same* commands, so
//! "works for me" means something. Commands that belong to phases which have not
//! shipped yet fail loudly rather than pretending to succeed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::Duration;

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
    /// workspace tests, the desktop shell behind its `gui` feature, the web UI's
    /// types, tests and embedded-page drift, and the design phase's P0-5 probe.
    /// `wsl-bench` needs a WSL2 runner and stays out.
    Check {
        /// Leave the web UI out. The Rust-only loop: fmt, clippy, nextest, the
        /// shell build and the probe, without Node or a browser test.
        #[arg(long)]
        no_web: bool,
    },
    /// The checks of the design phase's P0-5 probe alone. CI's `probes` job runs this, so the six
    /// steps are written once — here — and cannot drift from what a developer
    /// runs locally.
    Probe,
    /// The web UI's half of the gate alone: types, both test runs, and the embedded
    /// page compared with its source.
    ///
    /// CI's `web` job runs this, and `cargo xtask check` runs it last, so the
    /// commands are written once — here — and a runner cannot check something a
    /// developer's shell does not. It is the mirror of `check --no-web`, which is
    /// everything except this.
    WebCheck,
    /// The desktop shell's half of the gate alone: its lint, its link, and its
    /// tests.
    ///
    /// CI's `gui` job runs this, and `cargo xtask check` builds the shell but stops
    /// there — `gui-tests` is a second gpui build, and the lint wants it too, since
    /// the harness changes what `cfg(test)` compiles under `--all-targets`. Before
    /// this task, three of that job's steps were raw cargo lines; now the feature
    /// pair and the order are written once.
    GuiCheck,
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
    /// Regenerate the site's showcase: the screens the TUI paints, the desktop
    /// shell's tables, and the WebAssembly build of the palette solver.
    ///
    /// The frames come from the client itself (`acpilot-showcase` drives `App` and
    /// paints through `ui::draw`), so this is the command to run after touching a
    /// pane, a key or a token — the drift test that runs in `cargo test` fails until
    /// it has.
    Showcase,
    /// Write the published `agents.toml` JSON Schema.
    Schema,
    /// Run the daemon and the web dev server together, watching `web/` for edits.
    ///
    /// The page comes from Vite rather than from the daemon's embedded copy: the
    /// point of this task is editing a `.svelte` file and seeing it, so nothing is
    /// built and nothing has to be committed before it can be looked at.
    Dev,
    /// Build the web UI into `crates/acpilot-web/assets/`, where the daemon embeds it.
    ///
    /// `npm run build` under a name a newcomer can find without reading the
    /// workflow. The output is committed — `cargo build` needs no Node — and CI
    /// fails when it drifts from `web/`, which is why the task ends by saying
    /// whether it did.
    WebBuild,
    /// Package a release: the tarballs, checksums, installers and SBOM cargo-dist
    /// produces from `dist-workspace.toml`.
    ///
    /// The artifacts are the ones a release uploads, built locally: a change to the
    /// config or to what a binary reports can be looked at without pushing a tag and
    /// waiting for a runner. Only the host's target is built unless `--target` says
    /// otherwise, because the other six need a cross toolchain this task does not
    /// pretend to have.
    Dist {
        /// Target triples to build for; the host's own target when none are given.
        #[arg(long, value_name = "TRIPLE")]
        target: Vec<String>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Task::Check { no_web } => run_check(no_web),
        Task::WebCheck => {
            if check_web() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Task::GuiCheck => {
            if check_gui() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
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
        Task::Showcase => run_showcase(),
        Task::Schema => run_schema(),
        Task::Dev => run_dev(),
        Task::WebBuild => run_web_build(),
        Task::Dist { target } => run_dist(&target),
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

/// The address `cargo xtask dev` binds the daemon to.
///
/// `web/vite.config.ts` proxies `/v1` to exactly this address, so the fact is
/// written in two languages and a change to either is a dev loop where every
/// request misses. [`tests::the_dev_server_proxies_to_the_address_the_dev_task_binds`]
/// is what keeps the halves together.
const DEV_ADDR: &str = "127.0.0.1:7777";

/// The web UI's source: `web/`, beside the workspace.
fn web_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../web")
}

/// A file of the repository, as the repository knows it: relative to the root.
fn path_in_repo(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

/// `npm`, under the name Windows gives it: the launcher there is a `.cmd`, and
/// `Command` does not resolve a bare name to one.
fn npm() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
}

/// What `cargo run` needs to start the daemon the dev server proxies to.
fn dev_daemon_arguments() -> [&'static str; 6] {
    ["run", "-p", "acpilotd", "--", "--addr", DEV_ADDR]
}

/// Install `web/`'s dependencies when they are not there yet.
///
/// `npm ci` rather than `npm install`: the lockfile is the dependency list, and a
/// task that quietly rewrote it would change what CI installs. Only when there is
/// no `node_modules` — a rebuild after a `.svelte` edit should not pay for the
/// install again — so a changed lockfile is the one case this does not cover; that
/// is `npm install` in `web/`, run once by the person who changed it.
fn install_web_dependencies() -> bool {
    let dir = web_dir();
    if dir.join("node_modules").is_dir() {
        return true;
    }

    let mut install = Command::new(npm());
    install.args(["ci"]).current_dir(&dir);
    if run_step("install (web)", install) {
        return true;
    }
    eprintln!(
        "    the web UI needs Node 22 and npm; `web/package.json` is the list of what it \
         installs, and `web/package-lock.json` is the versions"
    );
    false
}

/// The desktop shell's half of the gate: the lint that compiles it, the link, and
/// the tests that need gpui's headless harness.
///
/// This is the whole of CI's `gui` job, which is why it is a task: the workspace
/// gate cannot cover the shell at all (`gui` is off by default so a plain `cargo
/// test` needs no graphics stack), so the job is the only place the shell is
/// checked — and before this, its steps were three raw cargo lines that a
/// developer had to reproduce by hand. A build rather than a `check` between the
/// two, because the Linux link is exactly where gpui's missing `xkbcommon-x11`
/// symlink shows up and a `check` neither links nor tests the binary.
fn check_gui() -> bool {
    let mut clippy = Command::new(env!("CARGO"));
    clippy.args(GUI_CLIPPY);
    if !run_step("clippy (gui)", clippy) {
        warn_about_graphics(App::Gui);
        return false;
    }

    if !build(App::Gui, false) {
        return false;
    }

    test(App::Gui)
}

/// The clippy run for the shell.
///
/// Both features, because `gui-tests` decides what the test targets compile and
/// `--all-targets` lints them: linting `gui` alone would miss the harness code and
/// linting `gui-tests` alone would not be the shell a release builds.
const GUI_CLIPPY: [&str; 9] = [
    "clippy",
    "-p",
    "acpilot-gui",
    "--features",
    "gui,gui-tests",
    "--all-targets",
    "--",
    "-D",
    "warnings",
];

/// The web UI's half of the gate: types, both test runs, and the built page.
///
/// Commands that CI's `web` job does not repeat: the job runs this task, so there
/// is one list of what the web UI is checked with rather than two that agree until
/// somebody edits one. The page is built last and then compared with the
/// committed copy: the daemon embeds that file, so a `web/` change that was never
/// built is a daemon serving yesterday's page.
fn check_web() -> bool {
    if !install_web_dependencies() {
        return false;
    }

    for (label, args) in WEB_UNIT_STEPS {
        let mut command = Command::new(npm());
        command.args(args).current_dir(web_dir());
        if !run_step(label, command) {
            return false;
        }
    }

    // `npm run test:live` spawns the daemon this checkout built, so one has to
    // exist: CI keeps the build in a step of its own for the same reason, and
    // neither the test nor this task runs `cargo` from JavaScript.
    let mut daemon = Command::new(env!("CARGO"));
    daemon.args(WEB_LIVE_DAEMON_BUILD);
    if !run_step("build (daemon, for the live web test)", daemon) {
        return false;
    }

    let (label, args) = WEB_LIVE_STEP;
    let mut live = Command::new(npm());
    live.args(args).current_dir(web_dir());
    if !run_step(label, live) {
        return false;
    }

    let mut build = Command::new(npm());
    build.args(WEB_PAGE_BUILD).current_dir(web_dir());
    if !run_step("build (web, for the embedded page)", build) {
        return false;
    }

    check_embedded_page()
}

/// The npm scripts the gate runs before the live one, in order.
///
/// Named here rather than written out at each call so a test can hold them against
/// `web/package.json`: a renamed script would otherwise fail as `Missing script`,
/// which reads like a broken checkout.
const WEB_UNIT_STEPS: [(&str, &[&str]); 2] = [
    ("types (web)", &["run", "check"]),
    ("test (web)", &["test"]),
];

/// The live test, after the daemon build it needs. Separate from the pair above
/// because of that build step, not because of the command.
const WEB_LIVE_STEP: (&str, &[&str]) = ("test (web, live)", &["run", "test:live"]);

/// What the live web test needs beside it: the daemon, from this checkout.
const WEB_LIVE_DAEMON_BUILD: [&str; 3] = ["build", "-p", "acpilotd"];

/// The build that writes the page the daemon embeds.
const WEB_PAGE_BUILD: [&str; 2] = ["run", "build"];

/// Fail when the committed page is not what `web/` builds.
///
/// The page is committed so `cargo build` needs no Node, and CI fails when a
/// rebuild changes it. Answering the same question here is the whole point of
/// putting the web's checks in the gate: a `.svelte` edit is caught before a push,
/// and the rebuilt file is already in the working tree to commit.
fn check_embedded_page() -> bool {
    let page = embedded_page_path();
    if !page.is_file() {
        eprintln!(
            "==> drift (web): {} does not exist after a successful build",
            page.display()
        );
        return false;
    }

    match embedded_page_drift() {
        Some(false) => {
            eprintln!(
                "==> drift (web): the committed page no longer matches `web/`; the build \
                 rewrote it, so commit crates/acpilot-web/assets/index.html"
            );
            false
        }
        Some(true) => {
            println!("    the committed page matches what `web/` builds");
            true
        }
        // A directory without git cannot answer the question, and failing a check
        // for that would be failing it for where the clone lives.
        None => {
            println!("    not a git checkout: the committed page cannot be compared");
            true
        }
    }
}

/// Rebuild the page the daemon embeds.
fn run_web_build() -> ExitCode {
    if !install_web_dependencies() {
        return ExitCode::FAILURE;
    }

    let mut build = Command::new(npm());
    build.args(["run", "build"]).current_dir(web_dir());
    if !run_step("build (web)", build) {
        return ExitCode::FAILURE;
    }

    report_embedded_page();
    ExitCode::SUCCESS
}

/// Say what the build wrote and whether git still has to be told.
///
/// The built page is committed so `cargo build` needs no Node, and CI fails when it
/// no longer matches `web/`; this is the moment that difference exists, so it is the
/// moment to name it rather than to read it out of a red job later.
fn report_embedded_page() {
    let page = embedded_page_path();
    // Canonicalised for the message only: `xtask/../crates/...` is a path nobody
    // types, and the same file is written by `npm run build` in `web/`.
    let shown = fs::canonicalize(&page).unwrap_or_else(|_| page.clone());
    match fs::metadata(&page) {
        Ok(metadata) => println!("wrote {} ({} bytes)", shown.display(), metadata.len()),
        Err(error) => {
            eprintln!(
                "{}: {error} — the build reported success without writing the page",
                shown.display()
            );
            return;
        }
    }

    match embedded_page_drift() {
        Some(true) => println!("    the committed page is unchanged"),
        Some(false) => {
            println!("    the committed page changed — commit it: CI fails on drift");
        }
        // Not a checkout, or no git: the reminder still holds, it just cannot be
        // answered here.
        None => println!(
            "    commit {} when it changes: CI rebuilds it and fails on drift",
            EMBEDDED_PAGE
        ),
    }
}

/// The built page, in the crate that `include_str!`s it.
fn embedded_page_path() -> PathBuf {
    path_in_repo(EMBEDDED_PAGE)
}

/// The page, as the repository knows it: `web/`'s source path, relative to the root.
const EMBEDDED_PAGE: &str = "crates/acpilot-web/assets/index.html";

/// Whether the committed page still matches a fresh build; `None` when there is no
/// git to ask.
///
/// `git status` rather than a byte comparison against a saved copy: whether the file
/// differs from the index is the question the next commit answers.
fn embedded_page_drift() -> Option<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--", EMBEDDED_PAGE])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

/// Build the artifacts a release uploads, with cargo-dist.
///
/// Two prerequisites are checked rather than assumed, because both fail in a way
/// whose error does not say what to do: `dist` itself, and the tool the configured
/// SBOM needs. Naming the install command here is the whole difference between a
/// five-minute fix and a walk through cargo-dist's changelog.
///
/// The version is pinned in `dist-workspace.toml` and CI fetches that one, so a local
/// `dist` of another version is refused rather than used: generated archives,
/// installer scripts and manifests are the output of a specific dist, and "the
/// release built on the runner" and "the release built here" have to be the same
/// thing for `xtask dist` to mean anything.
fn run_dist(targets: &[String]) -> ExitCode {
    let Some(pinned) = configured_dist_version() else {
        eprintln!(
            "==> dist: dist-workspace.toml has no `cargo-dist-version`; the version is \
             what makes CI and a local build agree"
        );
        return ExitCode::FAILURE;
    };

    match installed_dist_version() {
        Some(found) if found == pinned => {}
        Some(found) => {
            eprintln!(
                "==> dist: cargo-dist {found} is installed and dist-workspace.toml pins \
                 {pinned}; install the pinned one:\n    {}",
                dist_install_hint(&pinned)
            );
            return ExitCode::FAILURE;
        }
        None => {
            eprintln!(
                "==> dist: cargo-dist is not on PATH; it is what builds these artifacts:\n    {}",
                dist_install_hint(&pinned)
            );
            return ExitCode::FAILURE;
        }
    }

    // `cargo-cyclonedx = true` in the config, so every build runs the tool: dist's
    // own error for a missing one is the tool's, not the task's.
    if !cyclonedx_installed() {
        eprintln!(
            "==> dist: cargo-cyclonedx is not on PATH, and dist-workspace.toml asks for an \
             SBOM per release:\n    cargo install cargo-cyclonedx --locked"
        );
        return ExitCode::FAILURE;
    }

    // Two runs, because they make different things and the second one is seconds of
    // work: the archives belong to a target and are built by the machine that names
    // it, while the installer scripts, the formula and the checksum file are made
    // once per program. `host` mode alone builds only the installers for *this*
    // machine's OS, which would leave the PowerShell script out of a run on Linux.
    let mut local = Command::new("dist");
    local.arg("build");
    for target in targets {
        local.args(["--target", target]);
    }
    if !run_step("build (dist, archives)", local) {
        return ExitCode::FAILURE;
    }

    let mut global = Command::new("dist");
    global.args(["build", "--artifacts=global"]);
    if !run_step("build (dist, installers)", global) {
        return ExitCode::FAILURE;
    }

    if !build_sboms() {
        return ExitCode::FAILURE;
    }

    report_dist_artifacts();
    ExitCode::SUCCESS
}

/// Generate the SBOMs the config asks for, and put them where dist declares them.
///
/// `cargo-cyclonedx = true` makes dist *attach* a `.cdx.xml` per program to the
/// release and marks the artifact as already generated; generating it is a step in
/// the CI job cargo-dist writes, not something `dist build` does. This repeats that
/// step locally, so a run here has the same files a run on a runner would.
fn build_sboms() -> bool {
    let mut command = Command::new(env!("CARGO"));
    command.args(["cyclonedx", "-q"]);
    if !run_step("sbom (cargo cyclonedx)", command) {
        return false;
    }

    // cargo-cyclonedx writes beside each crate's manifest, and dist expects every
    // artifact under `target/distrib/`. Moved rather than copied: a `.cdx.xml` left
    // in a crate directory is a file that shows up in `git status` and, eventually,
    // in a commit.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut found = Vec::new();
    collect_sboms(&root, &mut found);
    if found.is_empty() {
        eprintln!(
            "==> sbom: cargo cyclonedx wrote no .cdx.xml, and the config asks for one \
             per program"
        );
        return false;
    }
    let dir = dist_dir();
    if let Err(error) = fs::create_dir_all(&dir) {
        eprintln!("{}: {error}", dir.display());
        return false;
    }
    // `cargo cyclonedx` writes one per workspace member, and the release attaches
    // the three declared ones. The libraries' SBOMs are neither attached nor useful
    // here, so they are deleted rather than left beside a manifest for `git status`
    // to find, or moved into a directory that is supposed to be a release's files.
    let mut kept = 0;
    for source in &found {
        let Some(name) = source.file_name() else {
            continue;
        };
        let name = name.to_string_lossy();
        let shipped = name
            .strip_suffix(".cdx.xml")
            .is_some_and(|package| SHIPPED_PACKAGES.contains(&package));
        let result = if shipped {
            kept += 1;
            fs::rename(source, dir.join(name.as_ref()))
        } else {
            fs::remove_file(source)
        };
        if let Err(error) = result {
            eprintln!("{}: {error}", source.display());
            return false;
        }
    }
    if kept != SHIPPED_PACKAGES.len() {
        eprintln!(
            "==> sbom: {kept} of {} declared SBOMs were written; the names cargo-cyclonedx \
             uses and the package names dist declares have to agree",
            SHIPPED_PACKAGES.len()
        );
        return false;
    }
    // Canonicalised for the message: the directory is reached through `xtask/..`,
    // which is not a path anyone types.
    let shown = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    println!("    {kept} SBOMs moved into {}", shown.display());
    true
}

/// Every `.cdx.xml` under `dir`, skipping the directories that hold build output.
fn collect_sboms(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name == "target" || name == "node_modules" || name == ".git" {
                continue;
            }
            collect_sboms(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "xml")
            && name.to_string_lossy().ends_with(".cdx.xml")
        {
            out.push(path);
        }
    }
}

/// The packages a release ships: the CLI/TUI, the daemon it spawns, and the desktop
/// shell. Named here as well as in each manifest because two things depend on the
/// list agreeing — the unit test below, and which SBOMs are kept.
const SHIPPED_PACKAGES: [&str; 3] = ["acpilot-cli", "acpilotd", "acpilot-gui"];

/// The cargo-dist version `dist-workspace.toml` pins, read from the file itself.
///
/// Parsed by line rather than by a TOML parser: `xtask` needs one key from one file
/// it also reads comments out of, and a dependency for that would be a dependency in
/// every developer's build.
fn configured_dist_version() -> Option<String> {
    let text = fs::read_to_string(dist_config_path()).ok()?;
    let value = text.lines().map(str::trim).find_map(|line| {
        // `cargo-dist-version = "0.32.0"`, minus the key and the quotes. Trimmed
        // before the `=`, or the space before it makes the remainder start with one.
        let rest = line.strip_prefix("cargo-dist-version")?.trim();
        Some(rest.strip_prefix('=')?.trim().trim_matches('"'))
    })?;
    Some(value.to_owned())
}

/// The version `dist --version` reports, if it is on PATH.
fn installed_dist_version() -> Option<String> {
    let output = Command::new("dist").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    // `cargo-dist 0.32.0` — the version is the last word, because the program's own
    // name has a hyphen in it and splitting on the first would keep it.
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .last()
        .map(str::to_owned)
}

/// The command that installs the pinned dist, printed whenever it is missing or of
/// the wrong version.
fn dist_install_hint(pinned: &str) -> String {
    format!("cargo install cargo-dist --locked --version {pinned}")
}

fn cyclonedx_installed() -> bool {
    Command::new(env!("CARGO"))
        .args(["cyclonedx", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Where dist writes, relative to the workspace root.
fn dist_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/distrib")
}

fn dist_config_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../dist-workspace.toml")
}

/// List what the build produced, so the next step is a `scp`, not a search.
fn report_dist_artifacts() {
    let dir = dist_dir();
    let mut entries: Vec<(String, u64)> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            // The per-archive staging directories (the unpacked tree an archive is
            // made from) are dist's workspace, not artifacts.
            .filter(|entry| !entry.path().is_dir())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let size = entry.metadata().ok()?.len();
                Some((name, size))
            })
            .collect(),
        Err(error) => {
            eprintln!(
                "{}: {error} — dist reported success without writing an artifact",
                dir.display()
            );
            return;
        }
    };
    entries.sort();
    if entries.is_empty() {
        eprintln!(
            "{} is empty — dist reported success without writing an artifact",
            dir.display()
        );
        return;
    }
    let shown = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    println!("{} artifacts in {}", entries.len(), shown.display());
    for (name, size) in entries {
        println!("    {name} ({size} bytes)");
    }
    println!("    the archives, checksums and SBOMs are what a release uploads; the scripts");
    println!("    and the formula are what a person runs to install it");
}

/// Run the daemon and the Vite dev server together until one of them stops.
///
/// Neither is useful alone: the page is served from `web/` by Vite, and every call
/// it makes is proxied to the daemon on [`DEV_ADDR`]. They are spawned instead of
/// being handed to a shell so that stopping one stops both — a dev loop left with a
/// stray daemon holding a port is the failure that costs an afternoon.
fn run_dev() -> ExitCode {
    // Built before anything is spawned: twenty seconds of cargo output printed over
    // a page that answers no route until it finishes is the launch people abandon.
    if !build(App::Daemon, false) || !install_web_dependencies() {
        return ExitCode::FAILURE;
    }

    let mut daemon_command = Command::new(env!("CARGO"));
    daemon_command.args(dev_daemon_arguments());
    let mut daemon = match daemon_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("==> dev: cannot start the daemon: {error}");
            return ExitCode::FAILURE;
        }
    };

    let mut web_command = Command::new(npm());
    web_command.args(["run", "dev"]).current_dir(web_dir());
    let mut web = match web_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            stop(&mut daemon);
            eprintln!("==> dev: cannot start the web dev server: {error}");
            return ExitCode::FAILURE;
        }
    };

    println!("==> dev");
    println!("    page     the address Vite prints below (http://localhost:5173 unless taken)");
    println!("    daemon   http://{DEV_ADDR} — every request needs the token it logs");
    println!("    Ctrl-C stops both");

    let ended = match supervise(&mut daemon, &mut web) {
        Ok(ended) => ended,
        Err(error) => {
            eprintln!("==> dev: {error}");
            stop(&mut daemon);
            stop(&mut web);
            return ExitCode::FAILURE;
        }
    };
    let (name, status) = ended;
    println!("==> dev: the {name} exited ({status})");
    stop(&mut daemon);
    stop(&mut web);

    // A signal is how this task is meant to end: Ctrl-C reaches both children
    // because they share the terminal's process group, and `code()` is `None` for a
    // process that was terminated rather than one that returned.
    if status.success() || status.code().is_none() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Wait until one of the two children exits, and say which.
///
/// Polled rather than blocking on one of them: a daemon that cannot bind its port
/// (already in use, a token file it may not read) leaves a page whose every request
/// misses, and that is worth stopping for instead of debugging in two terminals.
fn supervise(daemon: &mut Child, web: &mut Child) -> std::io::Result<(&'static str, ExitStatus)> {
    loop {
        if let Some(status) = web.try_wait()? {
            return Ok(("web dev server", status));
        }
        if let Some(status) = daemon.try_wait()? {
            return Ok(("daemon", status));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Stop a child that is still running, and reap it either way.
///
/// `wait` matters as much as `kill`: a child that is not reaped is a zombie holding
/// the port it bound, which is the next run's "address already in use".
fn stop(child: &mut Child) {
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn run_check(no_web: bool) -> ExitCode {
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

    // The shell sits behind a `gui` feature that is off by default, so neither the
    // clippy nor the test run above compiles it: without this step, a refactor
    // that breaks the desktop build is found by CI's `gui` job and nowhere else.
    // A build rather than a `check`, because linking is where gpui's missing
    // Linux symlink shows up, and it is the same call `cargo xtask build gui`
    // makes. The shell's *tests* stay out of the gate: they need `gui-tests` on
    // top, and `cargo xtask test gui` is that run.
    if !build(App::Gui, false) {
        return ExitCode::FAILURE;
    }

    // The web UI is a second toolchain, and its checks are a task of their own so
    // that CI's `web` job runs exactly these: a `check` that stopped at `cargo`
    // would be one that says yes while the page the daemon serves no longer
    // matches its source. `--no-web` is the Rust-only loop; it says so rather
    // than pretending the Node steps passed.
    if no_web {
        println!("==> web skipped (--no-web): types, tests and the embedded page were not checked");
    } else if !check_web() {
        eprintln!("==> web failed");
        return ExitCode::FAILURE;
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

/// Write the showcase's data and the wasm solver the page runs.
///
/// Two kinds of artefact, one command: the JSON documents are captured from the
/// running client, and the `.wasm` module is the product's solver compiled for a
/// browser. They are regenerated together because they are shown together — a page
/// whose frames are current but whose palette solver is not would be a demo that is
/// half the build it claims to be.
fn run_showcase() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");

    for (relative, text) in acpilot_showcase::documents() {
        let path = root.join(relative);
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

    if !write_wasm(&root) {
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// Build the palette solver for `wasm32-unknown-unknown` and copy it into the site.
///
/// Not committed: a `.wasm` binary is a build output, and the one thing worse than
/// committing one is committing one that no longer matches its source. The docs
/// workflow builds it the same way, so the deployed page and a local preview are the
/// same artefact from the same command.
fn write_wasm(root: &Path) -> bool {
    let mut build = Command::new(env!("CARGO"));
    build.args([
        "build",
        "-p",
        "acpilot-palette-wasm",
        "--release",
        "--target",
        "wasm32-unknown-unknown",
    ]);
    if !run_step("wasm", build) {
        eprintln!("    install the target with `rustup target add wasm32-unknown-unknown`");
        return false;
    }

    let built = root.join("target/wasm32-unknown-unknown/release/acpilot_palette_wasm.wasm");
    let published = root.join("site/public/showcase/acpilot-palette.wasm");
    if let Some(parent) = published.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        eprintln!("{}: {error}", parent.display());
        return false;
    }
    match fs::copy(&built, &published) {
        Ok(bytes) => {
            println!("wrote {} ({bytes} bytes)", published.display());
            true
        }
        Err(error) => {
            eprintln!(
                "{}: {error} (build {} first)",
                published.display(),
                built.display()
            );
            false
        }
    }
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
    fn the_dev_server_proxies_to_the_address_the_dev_task_binds() {
        // The address is one fact in two languages: here, where the daemon binds,
        // and in `web/vite.config.ts`, where Vite forwards `/v1`. A task that binds
        // somewhere else is a page whose every request misses, and the failure
        // looks like a broken API rather than a mistyped port.
        let vite =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../web/vite.config.ts"))
                .expect("web/vite.config.ts configures the dev server");
        assert!(
            vite.contains(&format!("http://{DEV_ADDR}")),
            "vite.config.ts does not proxy to {DEV_ADDR}"
        );
        assert_eq!(dev_daemon_arguments().last(), Some(&DEV_ADDR));
    }

    #[test]
    fn the_web_build_writes_where_the_daemon_looks_for_the_page() {
        // `acpilot_web` reads `assets/index.html` through `include_str!`; a build
        // pointed anywhere else is a page the daemon never serves.
        let source = web_dir().join("vite.config.ts");
        let vite = fs::read_to_string(&source).expect("web/vite.config.ts");
        assert!(
            vite.contains("../crates/acpilot-web/assets"),
            "the build no longer lands in the crate that embeds it"
        );
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

    #[test]
    fn the_shipped_programs_are_the_ones_a_release_installs() {
        // cargo-dist ships a package only when its manifest says so, and the three
        // that do are the three a person ends up running. Stated as an exact set
        // because the failure this guards against is an addition: a test fixture
        // that grew a binary (`acpilot-mock-agent`) or a developer tool
        // (`acpilot-terminal-colours`) joining a release because nobody looked.
        let mut shipped = Vec::new();
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates");
        for entry in fs::read_dir(&crates).expect("crates/ is a directory") {
            let manifest = entry.expect("a readable entry").path().join("Cargo.toml");
            let Ok(text) = fs::read_to_string(&manifest) else {
                continue;
            };
            let ships = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.starts_with('#'))
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| pair == ["[package.metadata.dist]", "dist = true"]);
            if ships {
                let name = text
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("name = "))
                    .expect("every manifest names its package")
                    .trim_matches('"')
                    .to_owned();
                shipped.push(name);
            }
        }
        shipped.sort();
        let mut expected = SHIPPED_PACKAGES.to_vec();
        expected.sort_unstable();
        assert_eq!(
            shipped, expected,
            "a release ships the CLI, the daemon and the desktop shell, and nothing else"
        );
    }

    #[test]
    fn the_web_check_runs_scripts_the_web_package_declares() {
        // A renamed npm script is a `check` that fails with `Missing script`, which
        // reads like a broken checkout rather than a stale command name.
        let manifest = fs::read_to_string(web_dir().join("package.json"))
            .expect("web/package.json lists the scripts");
        for (_, args) in WEB_UNIT_STEPS.into_iter().chain([WEB_LIVE_STEP]) {
            // `npm test` is the bare script; `npm run <name>` names one.
            let script = if args[0] == "run" { args[1] } else { "test" };
            assert!(
                manifest.contains(&format!("\"{script}\":")),
                "web/package.json no longer declares `{script}`"
            );
        }
        assert!(
            manifest.contains("\"build\":"),
            "web/package.json no longer builds"
        );
    }

    #[test]
    fn the_web_job_runs_the_task_that_checks_the_web_ui() {
        // The job is one `cargo xtask` line now, so the subcommand's name is the
        // only thing holding the two together: renaming it without the workflow
        // is a red runner that says `unrecognized subcommand`, not a smaller gate.
        let workflow = fs::read_to_string(path_in_repo(".github/workflows/ci.yml"))
            .expect("CI is a workflow file");
        assert!(
            workflow.contains("run: cargo xtask web-check"),
            "CI's web job no longer runs `cargo xtask web-check`"
        );
        Cli::try_parse_from(["xtask", "web-check"]).expect("`web-check` is a task");
    }

    #[test]
    fn the_gui_job_runs_the_task_that_checks_the_shell() {
        // Same reason as the web job: one `cargo xtask` line means a renamed
        // subcommand is a red runner rather than a job that quietly checks less.
        let workflow = fs::read_to_string(path_in_repo(".github/workflows/ci.yml"))
            .expect("CI is a workflow file");
        assert!(
            workflow.contains("run: cargo xtask gui-check"),
            "CI's gui job no longer runs `cargo xtask gui-check`"
        );
        let cli = Cli::try_parse_from(["xtask", "gui-check"]).expect("`gui-check` is a task");
        assert!(matches!(cli.command, Task::GuiCheck));
    }

    #[test]
    fn the_gui_check_lints_the_shell_with_the_harness_it_tests_with() {
        // `test gui` runs with `gui,gui-tests`, so the lint has to see the same
        // feature set or the test code is never linted at all.
        assert!(GUI_CLIPPY.contains(&"--all-targets"));
        assert!(GUI_CLIPPY.contains(&"gui,gui-tests"));
        assert_eq!(
            GUI_CLIPPY[1..3],
            ["-p", "acpilot-gui"],
            "the lint is the shell's, not the workspace's"
        );
        assert_eq!(GUI_CLIPPY.last(), Some(&"warnings"));
        // The run it is paired with is the one the tests use.
        assert!(test_commands(App::Gui)[0].contains(&"gui,gui-tests"));
    }

    #[test]
    fn check_can_leave_the_web_ui_out() {
        // The Rust-only loop: the Node steps are a second toolchain, and a check
        // that always paid for it is one people work around instead of running.
        let cli = Cli::try_parse_from(["xtask", "check", "--no-web"]).expect("--no-web parses");
        assert!(matches!(cli.command, Task::Check { no_web: true }));
        let cli = Cli::try_parse_from(["xtask", "check"]).expect("check needs no flag");
        assert!(matches!(cli.command, Task::Check { no_web: false }));
    }

    #[test]
    fn the_live_web_test_gets_the_daemon_it_expects() {
        // The test spawns `target/debug/acpilotd` from this checkout and never runs
        // cargo itself, so the build step is the only thing that can put it there.
        assert_eq!(WEB_LIVE_DAEMON_BUILD, ["build", "-p", "acpilotd"]);
        let test = fs::read_to_string(web_dir().join("src/App.live.test.ts"))
            .expect("the live test names the daemon it runs");
        assert!(
            test.contains("'target', 'debug'") && test.contains("acpilotd"),
            "the live test no longer looks in target/debug for acpilotd"
        );
    }

    #[test]
    fn the_pinned_dist_version_is_what_the_install_hint_asks_for() {
        // The pin is the reproducibility claim: CI fetches this version, so a local
        // build of another one is a different release with the same name.
        let pinned = configured_dist_version().expect("dist-workspace.toml pins cargo-dist");
        // A full version, not a requirement: `0.32` and `^0.32.0` cannot be
        // installed by the hint, and a pin nobody can install is not a pin.
        assert!(
            pinned
                .split('.')
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())),
            "the pin is a full version, not a requirement: {pinned}"
        );
        assert!(
            dist_install_hint(&pinned).ends_with(&pinned),
            "the hint installs the pinned version"
        );
    }
}
