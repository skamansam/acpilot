//! The argument surface, kept out of `main` so it can be tested.
//!
//! Every command here is either a filesystem operation or (later) a call into
//! `acpilot-proto`; none of them reimplements daemon policy.

use std::path::PathBuf;

use acpilot_agent::keyring::KeyringResolver;
use acpilot_core::error::exit;
use acpilot_extensions::ExtensionError;
use acpilot_profile::Profile;
use acpilot_profile::project::Root;
use acpilot_profile::settings::Settings;
use acpilot_proto::API_PREFIX;
use acpilot_proto::profile::ThemeSurface;
use clap::{Args, Parser, Subcommand};

use crate::doctor;
use crate::ext::{self, Paths};
use crate::secret;

/// `acpilot` command line.
#[derive(Debug, Parser)]
#[command(
    name = "acpilot",
    // `acpilot_core::VERSION`, not the manifest's number: a published build reports
    // the tag it was released as (see that constant).
    version = acpilot_core::VERSION,
    about = "Edit and run ACP agents over one daemon"
)]
pub struct Cli {
    /// Command to run.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print version and protocol information.
    Version,
    /// Install, verify, and approve extensions.
    Ext {
        /// Which ext verb.
        #[command(subcommand)]
        command: ExtCommand,
    },
    /// Store, list, and remove credentials in the OS keyring.
    Secret {
        /// Which secret verb.
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Show and rotate the token a TCP daemon requires.
    Token {
        /// Which token verb.
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Open the terminal UI against a running daemon, or run one here.
    ///
    /// The TUI is a client, not a second engine: it opens sessions, sends
    /// prompts and answers permissions over the same API the CLI and the GUI
    /// use, so nothing it does can drift from what the daemon records. When no
    /// daemon answers, the daemon's own core runs inside this process (P6-11), so
    /// the command needs nothing installed and nothing started — with the trade
    /// that the sessions it opens end with the window. A daemon that is running
    /// is always attached to first, and its sessions outlive every client.
    Tui {
        /// A directory, or a file inside one. Defaults to this directory.
        ///
        /// A directory is the workspace, as named. A file climbs its parents until
        /// it finds `.acpilot/config.toml`, because naming a file is naming
        /// something *in* a project rather than a directory that happens to sit
        /// inside one. With no project above it the file's own directory is a
        /// temporary root, and nothing is written: a project exists on disk when
        /// settings are saved, not when a file is opened.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
        /// Endpoint of the daemon: a URL, or a unix socket path.
        ///
        /// Defaults to the socket in the runtime directory (or loopback HTTP on
        /// Windows), which is where `acpilotd` listens when given no flags.
        #[arg(long, env = "ACPILOT_API_URL", value_name = "URL|PATH")]
        api_url: Option<String>,
        /// Fail if no daemon is running instead of running one in this process.
        ///
        /// For tests, CI and anyone debugging a startup problem: the error is
        /// more useful than a daemon this client started without being asked.
        #[arg(long)]
        no_spawn: bool,
        /// Profile directory the daemon serves.
        ///
        /// Only used when this command runs the daemon itself; a daemon that is
        /// already running keeps the profile it was started with, and naming a
        /// different one here is an error rather than a silent mismatch. Without
        /// it, the profile named by the project's settings is used, so the usual
        /// launch names no profile at all.
        #[arg(long, value_name = "DIR")]
        profile: Option<PathBuf>,
        /// Workspace root the daemon serves. Defaults to this directory.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        /// Bearer token for a daemon on a TCP endpoint.
        ///
        /// A socket needs none: its access control is the filesystem. Without this
        /// flag the file the daemon writes beside its socket is read, so a local
        /// port works with no typing; name a token here when the daemon is
        /// somewhere this machine's runtime directory cannot describe.
        #[arg(long, env = "ACPILOT_TOKEN", value_name = "TOKEN")]
        token: Option<String>,
        /// Draw the interface on seeded data, with no daemon and nothing read from
        /// disk.
        ///
        /// The design pass: every pane is filled from `acpilot-demo`, and every
        /// action that is not built answers with the task that would build it. It
        /// is how the layout is argued about before the backend for it exists, and
        /// it is the same world the desktop app draws when it gets its shell.
        #[arg(long)]
        demo: bool,
        /// Do not ask the terminal to report the mouse.
        ///
        /// Mouse reporting is on by default, and it costs the terminal's own
        /// click-drag selection: most terminals keep it on Shift-drag, but not all.
        /// With this flag the terminal keeps its selection, and the TUI is driven by
        /// keys alone — every mouse action has a key, so nothing is lost but the
        /// pointer.
        #[arg(long)]
        no_mouse: bool,
    },
    /// Serve the daemon and the web UI over HTTP, printing the token once.
    ///
    /// One process, serving the same state `/v1` answers from: a browser at the
    /// printed URL talks to the daemon a TUI would attach to, not to a copy of it.
    /// Every request needs `Authorization: Bearer <token>` except the page itself,
    /// which a browser cannot give a header to when the address is typed.
    ///
    /// The token is printed on the run that creates it, and only then unless
    /// `--token` asks for it again: a secret in a scrollback is still a secret, and
    /// printing it on every start puts it there once per restart.
    Serve {
        /// Listen on the default loopback port, `127.0.0.1:7777`.
        ///
        /// `--bind` chooses another address and implies this one.
        #[arg(long)]
        http: bool,
        /// The address to bind, for example `0.0.0.0:7777`.
        #[arg(long, value_name = "ADDR")]
        bind: Option<String>,
        /// Print the bearer token even on a run that did not create it.
        #[arg(long)]
        token: bool,
        /// Profile directory the daemon serves.
        #[arg(long, value_name = "DIR")]
        profile: Option<PathBuf>,
        /// Workspace root the daemon serves. Defaults to this directory.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
    },
    /// Report what this machine has, and what a project is missing.
    ///
    /// Reads `PATH`, the profile and the workspace; starts no daemon and writes
    /// nothing. A declared local agent is started once to answer ACP
    /// `initialize`, because a command on `PATH` is not proof that it speaks
    /// the protocol. A machine with no profile and no agent is healthy — that is
    /// the first-run state, and the editor works in it.
    Doctor {
        /// A directory, or a file inside one. Defaults to this directory.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
        /// Profile to report on, by name under the config root.
        ///
        /// The profile need not exist: a missing one is reported as not created,
        /// not as a failure.
        #[arg(long, env = "ACPILOT_PROFILE", default_value = "default")]
        profile: String,
        /// The daemon's endpoint, when it is not the default.
        ///
        /// `doctor` asks `GET /v1/health` here and starts nothing. The same
        /// variable, `ACPILOT_API_URL`, names the endpoint a launch would use.
        #[arg(long, env = "ACPILOT_API_URL", value_name = "URL|PATH")]
        api_url: Option<String>,
        /// Bearer token for a daemon on a TCP endpoint.
        ///
        /// Without this flag the token a local daemon wrote is read. A socket
        /// needs none. `doctor` reads it only to ask health; it writes nothing.
        #[arg(long, env = "ACPILOT_TOKEN", value_name = "TOKEN")]
        token: Option<String>,
    },
}

/// Everything under `acpilot token`.
#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Replace the token, refusing every client that holds the old one.
    ///
    /// Writes the file the daemon reads. Nothing restarts: the daemon reads it per
    /// request, so the new token applies from the next call — which is the point,
    /// since a rotation is something you do about a token you think leaked.
    Rotate,
}

/// Everything under `acpilot secret`.
///
/// `set` reads the value from stdin; there is deliberately no `--value`, because
/// an argument is visible in shell history and in `ps`.
#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// Store a secret. Reads the value from stdin.
    Set {
        /// Entry name, `name` or `profile/name`, with or without `keyring:`.
        name: String,
        #[command(flatten)]
        profile: ProfileArgs,
    },
    /// List the secrets this profile references, and whether they are set.
    List {
        #[command(flatten)]
        profile: ProfileArgs,
    },
    /// Remove a secret.
    Rm {
        /// Entry name, `name` or `profile/name`, with or without `keyring:`.
        name: String,
        #[command(flatten)]
        profile: ProfileArgs,
    },
}

/// Everything under `acpilot ext`.
#[derive(Debug, Subcommand)]
pub enum ExtCommand {
    /// Install an extension from a local directory.
    Add {
        /// Extension directory containing `extension.toml`.
        #[arg(long, value_name = "DIR")]
        from: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Import a TextMate `.tmTheme` as a theme extension and install it.
    ImportTheme {
        /// Path to a `.tmTheme` file.
        #[arg(long, value_name = "FILE")]
        from: PathBuf,
        /// Namespaced theme id to import it as, for example `acpilot.hangar`.
        #[arg(long)]
        id: String,
        /// Version to install it under.
        #[arg(long, default_value = "1.0.0")]
        version: String,
        /// Activate it in this profile after installing.
        #[arg(long, value_name = "PROFILE")]
        activate: Option<String>,
        #[command(flatten)]
        store: StoreArgs,
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Rehash every installed version and report any that are blocked.
    Verify {
        /// Verify everything. Required: a partial verify is not a safety check.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Remove versions no profile activates.
    Prune {
        /// Report what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        store: StoreArgs,
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Approve an installed extension for one profile.
    Activate {
        /// Extension id, `publisher.name`.
        id: String,
        #[command(flatten)]
        profile: ProfileArgs,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Withdraw approval from one profile.
    Deactivate {
        /// Extension id, `publisher.name`.
        id: String,
        #[command(flatten)]
        profile: ProfileArgs,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Explain how a preset or agent resolves for one profile.
    Why {
        /// Preset or agent id, for example `rust-analyzer`.
        id: String,
        #[command(flatten)]
        profile: ProfileArgs,
        #[command(flatten)]
        store: StoreArgs,
    },
}

/// Where the store lives.
#[derive(Debug, Args)]
pub struct StoreArgs {
    /// Extension store root. Defaults to the platform data directory.
    #[arg(long, value_name = "PATH")]
    pub store: Option<PathBuf>,
}

/// Where the config (and therefore profiles) live.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Acpilot config root; profiles live under `<config>/profiles`.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

/// Which profile to act on.
#[derive(Debug, Args)]
pub struct ProfileArgs {
    /// Profile to act on.
    #[arg(long, env = "ACPILOT_PROFILE", default_value = "default")]
    pub profile: String,
    #[command(flatten)]
    pub config: ConfigArgs,
}

/// Run a parsed command and return the process exit code.
pub fn run(cli: Cli) -> u8 {
    match cli.command {
        Some(Command::Version) => {
            println!("acpilot {}", acpilot_core::VERSION);
            println!("api      {API_PREFIX}");
            exit::SUCCESS
        }
        Some(Command::Ext { command }) => run_ext(command),
        Some(Command::Secret { command }) => run_secret(command),
        Some(Command::Token { command }) => run_token(command),
        Some(Command::Serve {
            http,
            bind,
            token,
            profile,
            workspace,
        }) => run_serve(ServeFlags {
            http,
            bind,
            token,
            profile,
            workspace,
        }),
        Some(Command::Tui {
            path,
            api_url,
            no_spawn,
            profile,
            workspace,
            token,
            demo,
            no_mouse,
        }) => run_tui(TuiFlags {
            path,
            api_url,
            no_spawn,
            profile,
            workspace,
            token,
            demo,
            mouse: !no_mouse,
        }),
        Some(Command::Doctor {
            path,
            profile,
            api_url,
            token,
        }) => run_doctor(path.as_deref(), &profile, api_url.as_deref(), token),

        None => {
            // `clap` prints usage on --help; with no subcommand, do the same
            // rather than pretending to do work.
            use clap::CommandFactory;
            let mut command = Cli::command();
            let _ = command.print_help();
            println!();
            exit::USAGE
        }
    }
}

/// Report what this machine has, and exit non-zero only when something that is
/// configured cannot run.
fn run_doctor(
    path: Option<&std::path::Path>,
    profile: &str,
    api_url: Option<&str>,
    token: Option<String>,
) -> u8 {
    let installed = |program: &str| acpilotd::installed(program);
    let endpoint = acpilot_client::daemon::resolve_endpoint(
        api_url,
        std::env::var(acpilot_client::daemon::ENV_ENDPOINT)
            .ok()
            .as_deref(),
    );
    let daemon = probe_daemon(&endpoint, token.or_else(local_token).as_deref());
    match doctor::collect(path, profile, &installed, &probe_agent, daemon) {
        Ok(report) => {
            print!("{}", report.render());
            if report.ok() {
                exit::SUCCESS
            } else {
                exit::FAILURE
            }
        }
        Err(message) => {
            eprintln!("acpilot doctor: {message}");
            exit::FAILURE
        }
    }
}

/// Ask `GET /v1/health` what is listening, without starting anything.
///
/// A local daemon answers immediately or is not there, so the bound is short: a
/// slow answer is a busy machine, not a daemon starting, and waiting longer would
/// make `doctor` feel hung. `Refused` is kept apart from `Down` because a `401`
/// proves something is listening — the fix is a token, not a launch.
fn probe_daemon(endpoint: &str, token: Option<&str>) -> doctor::Daemon {
    use std::time::Duration;

    /// A daemon on this machine either answers now or is not there.
    const TIMEOUT: Duration = Duration::from_secs(1);

    let client = match token {
        Some(token) => acpilot_client::Client::with_token(endpoint, token),
        None => acpilot_client::Client::new(endpoint),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return doctor::Daemon {
                endpoint: endpoint.to_owned(),
                state: doctor::DaemonState::Down {
                    reason: error.to_string(),
                },
            };
        }
    };
    let state =
        match runtime.block_on(async { tokio::time::timeout(TIMEOUT, client.health()).await }) {
            Ok(Ok(health)) => doctor::DaemonState::Running {
                version: health.version,
                workspace: health.workspace,
            },
            Ok(Err(acpilot_client::ClientError::Api { status: 401, .. })) => {
                doctor::DaemonState::Refused { status: 401 }
            }
            Ok(Err(error)) => doctor::DaemonState::Down {
                reason: error.to_string(),
            },
            Err(_) => doctor::DaemonState::Down {
                reason: format!("no answer within {} s", TIMEOUT.as_secs()),
            },
        };
    doctor::Daemon {
        endpoint: endpoint.to_owned(),
        state,
    }
}

/// Ask one declared agent to `initialize`.
///
/// This is the handshake half of first run: a command on `PATH` proves a file
/// exists, not that it speaks ACP. The session is opened with no workspace, no
/// broker and no terminals, so this client advertises none of them; `initialize`
/// starts no conversation and sends no prompt. The child is killed when the
/// session is dropped, including when the wait below expires.
fn probe_agent(spec: &acpilot_agent::AgentSpec) -> doctor::HandshakeOutcome {
    use std::sync::Arc;
    use std::time::Duration;

    /// Long enough for `npx -y` to unpack on a cold cache, short enough that a
    /// doctor run does not feel hung. There is no prompt to wait for.
    const TIMEOUT: Duration = Duration::from_secs(10);

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return doctor::HandshakeOutcome::failed(error.to_string()),
    };
    runtime.block_on(async {
        let process = match acpilot_agent::AgentProcess::spawn(Arc::new(spec.clone())) {
            Ok(process) => process,
            Err(error) => return doctor::HandshakeOutcome::failed(error.to_string()),
        };
        match tokio::time::timeout(
            TIMEOUT,
            acpilot_agent::Session::spawn(process, acpilot_agent::SessionOptions::new("doctor")),
        )
        .await
        {
            Ok(Ok(session)) => {
                let (name, version) = session.agent_info().map_or((None, None), |info| {
                    (Some(info.name.to_string()), Some(info.version.to_string()))
                });
                doctor::HandshakeOutcome::answered(
                    name,
                    version,
                    format!("{:?}", session.protocol_version()),
                )
            }
            Ok(Err(error)) => doctor::HandshakeOutcome::failed(error.to_string()),
            Err(_) => doctor::HandshakeOutcome::failed("no answer within 10 seconds"),
        }
    })
}

/// What `acpilot tui` was asked for.
///
/// A struct rather than seven arguments: one of them is about the daemon, one is
/// about the design pass and one is about the terminal, and a positional list of
/// booleans is where `demo` and `no_spawn` get swapped by the next person.
struct TuiFlags {
    /// The project to open.
    path: Option<PathBuf>,
    /// The daemon's endpoint, when it is not the default.
    api_url: Option<String>,
    /// Fail instead of starting a daemon.
    no_spawn: bool,
    /// The profile to serve.
    profile: Option<PathBuf>,
    /// The workspace root the spawned daemon serves.
    workspace: Option<PathBuf>,
    /// The bearer token a TCP daemon requires.
    token: Option<String>,
    /// Draw the seeded world instead of asking a daemon.
    demo: bool,
    /// Ask the terminal to report the mouse.
    mouse: bool,
}

/// Open the TUI, attaching to a daemon if one is running and hosting one if not
/// (P6-11).
///
/// The daemon's flags are here because they only matter on the launch path this
/// command owns. A daemon that is already running was started with its own, and
/// pretending otherwise would show one project's sessions under another's name —
/// so a mismatch is refused rather than ignored.
fn run_tui(flags: TuiFlags) -> u8 {
    let TuiFlags {
        path,
        api_url,
        no_spawn,
        profile,
        workspace,
        token,
        demo,
        mouse,
    } = flags;
    // The design pass comes first, before anything is resolved: it has no daemon
    // to start, no project to find and no profile to read, and a flag that still
    // touched the disk would be a flag that fails on a machine with neither.
    if demo {
        // The design pass reads nothing from disk, themes included: it draws in the
        // built-in default, which is the theme a person who set nothing has.
        let theme = match Settings::load(None, None) {
            Ok(settings) => match tui_theme(&settings) {
                Ok(theme) => theme,
                Err(message) => {
                    eprintln!("acpilot tui: {message}");
                    return exit::FAILURE;
                }
            },
            Err(error) => {
                eprintln!("acpilot tui: {error}");
                return exit::FAILURE;
            }
        };
        return match acpilot_tui::run_demo(acpilot_demo::demo(), theme, mouse) {
            Ok(()) => exit::SUCCESS,
            Err(error) => {
                eprintln!("acpilot tui: {error}");
                exit::FAILURE
            }
        };
    }

    // What the person named, or where they are: the workspace and the project are
    // implied by the argument rather than by two flags that have to agree with it.
    let root = match Root::resolve(path.as_deref()) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("acpilot tui: {error}");
            return exit::FAILURE;
        }
    };
    let workspace = workspace.unwrap_or_else(|| root.root().to_path_buf());
    // What the command was pointed at, before the workspace became a daemon
    // argument: a file is opened in the editor, and a directory names only a place.
    let initial = initial_file(path.as_deref(), &workspace);
    // The settings are read once, here: the profile they name and the theme they
    // name come from the same cascade, and reading it twice is how two keys end up
    // resolved from two different files.
    let settings = match root.settings() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("acpilot tui: {error}");
            return exit::FAILURE;
        }
    };
    let theme = match tui_theme(&settings) {
        Ok(theme) => theme,
        Err(message) => {
            eprintln!("acpilot tui: {message}");
            return exit::FAILURE;
        }
    };
    // The profile is the project's, unless the caller named one. A project that
    // names none means the daemon's default, which is a profile nobody has to
    // spell out on the command line.
    let profile = match profile {
        Some(directory) => Some(directory),
        None => match implied_profile(&settings) {
            Ok(profile) => profile,
            Err(code) => return code,
        },
    };
    let implied = profile.is_some();

    let token = token.or_else(local_token);
    let options = acpilot_client::daemon::Options {
        endpoint: api_url,
        no_spawn,
        token: token.clone(),
        // Unused here: this path never spawns a process — when no daemon answers,
        // the core below runs in this one. The field belongs to `ensure`, which is
        // what starts `acpilotd` for a caller that wants a separate process.
        daemon_args: Vec::new(),
    };
    let in_process = InProcess {
        profile: profile.clone(),
        workspace: workspace.clone(),
    };
    let running = match acpilot_client::daemon::ensure_or_embed_blocking(&options, &in_process) {
        Ok(running) => running,
        Err(error) => {
            eprintln!("acpilot tui: {error}");
            return exit::FAILURE;
        }
    };

    // Only a daemon someone else is holding can disagree about the profile or the
    // workspace: one this command started — in a process before, in this one now —
    // was given both just above.
    if running.attached() {
        if implied {
            eprintln!(
                "acpilot tui: a daemon is already running at {}; it serves the profile it was \
                 started with. Stop it, or pass a different --api-url for a second instance.",
                running.endpoint
            );
            return exit::CONFLICT;
        }
        if let Some(served) = &running.workspace
            && let Some(wanted) = expected_workspace_id(&workspace)
            && &wanted != served
        {
            eprintln!(
                "acpilot tui: the daemon at {} serves workspace `{served}`, not `{wanted}`. Stop \
                 it, or pass a different --api-url for a second instance.",
                running.endpoint
            );
            return exit::CONFLICT;
        }
    }

    // The files the theme came from: the client watches them, so an edit to either
    // is the colours on screen rather than a note to restart.
    let theme_files = acpilot_tui::ThemeFiles {
        user: Some(acpilot_profile::user_config_path()),
        project: root.project().map(|project| project.config_path()),
        // The terminal's own colours are asked for by the client, before it takes
        // the screen: one reader of one terminal.
        terminal: None,
    };
    // The token the launch settled on, which for an embedded daemon is the one it
    // generated — the caller could not have had it before the launch.
    let token = running.token().map(str::to_owned);
    match acpilot_tui::run(
        &running.endpoint,
        token.as_deref(),
        theme,
        theme_files,
        initial.as_deref(),
        mouse,
    ) {
        Ok(()) => exit::SUCCESS,
        Err(error) => {
            eprintln!("acpilot tui: {error}");
            exit::FAILURE
        }
    }
}

/// The file a launch named, relative to the workspace root.
///
/// The daemon addresses files by workspace-relative path, so an argument from the
/// shell has to be translated before it can be read: an absolute path and a relative
/// one are two spellings of one file. A directory names no file, and a file outside
/// the workspace — possible when `--workspace` names another tree — is not one this
/// launch can open.
fn initial_file(named: Option<&std::path::Path>, root: &std::path::Path) -> Option<String> {
    let named = named?;
    if named.is_dir() {
        return None;
    }
    let file = std::fs::canonicalize(named).ok()?;
    let relative = file.strip_prefix(root).ok()?;
    let parts: Vec<String> = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// The address `acpilot serve --http` listens on.
const DEFAULT_SERVE_ADDR: &str = "127.0.0.1:7777";

/// What `acpilot serve` was asked for.
struct ServeFlags {
    /// Listen on the default loopback port.
    http: bool,
    /// The address to bind.
    bind: Option<String>,
    /// Print the token even on a run that did not create it.
    token: bool,
    /// The profile the daemon serves.
    profile: Option<PathBuf>,
    /// The workspace root it serves.
    workspace: Option<PathBuf>,
}

/// Serve the daemon and the web UI over HTTP until interrupted.
///
/// The runtime is built here because the daemon's future is async and this binary
/// is not: `ensure_or_embed_blocking` would start the embedded listener, which
/// binds `127.0.0.1:0` with a token no other process can learn — exactly what a
/// browser on another machine cannot use.
fn run_serve(flags: ServeFlags) -> u8 {
    let ServeFlags {
        http,
        bind,
        token,
        profile,
        workspace,
    } = flags;
    let Some(addr) = bind.or_else(|| http.then(|| DEFAULT_SERVE_ADDR.to_owned())) else {
        eprintln!(
            "acpilot serve: pass --http to listen on {DEFAULT_SERVE_ADDR}, or --bind <ADDR> for \
             another address"
        );
        return exit::USAGE;
    };

    // Before the bind, like the daemon does it: a listener that is up for even a
    // moment without an answer to "who are you" is the hole the token closes. The
    // file is read first only to tell a first run from a later one.
    let token_path = acpilot_host::token::path();
    let existing = acpilot_host::token::read(&token_path).ok().flatten();
    let secret = match acpilot_host::token::load_or_create(&token_path) {
        Ok(secret) => secret,
        Err(error) => {
            eprintln!(
                "acpilot serve: cannot read or write {}: {error}",
                token_path.display()
            );
            return exit::FAILURE;
        }
    };
    if existing.is_none() {
        println!("acpilot serve: token {secret}");
        println!(
            "acpilot serve: this run created it, so it is printed once; it is also in {}",
            token_path.display()
        );
    } else if token {
        println!("acpilot serve: token {secret}");
    } else {
        println!(
            "acpilot serve: token in {} (--token prints it)",
            token_path.display()
        );
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("acpilot serve: cannot start a runtime: {error}");
            return exit::FAILURE;
        }
    };
    let workspace = workspace.unwrap_or_else(|| PathBuf::from("."));
    // Inside the runtime: startup spawns the compaction and reaping timers, once
    // per profile. `serve` hosts every profile the config root holds, the same
    // way `acpilotd` does; `--profile` chooses which one a headerless read is
    // about, not which one exists.
    let profiles = match runtime.block_on(async {
        acpilotd::profiles::Profiles::discover(profile.as_deref(), &workspace, false)
    }) {
        Ok(profiles) => std::sync::Arc::new(profiles),
        Err(error) => {
            eprintln!("acpilot serve: {error}");
            return exit::FAILURE;
        }
    };

    println!("acpilot serve: http://{}/", browse_at(&addr));
    if browse_at(&addr) != addr {
        println!("acpilot serve: bound on {addr}, so another machine reaches it at that port");
    }

    runtime.block_on(async move {
        let served = acpilotd::serve(
            acpilotd::profiles::router(profiles.clone()),
            acpilotd::Target::Tcp(addr),
            acpilotd::shutdown(),
        )
        .await;
        // Language servers are processes, and the router's drop is not their end.
        profiles.shutdown_language_servers().await;
        match served {
            Ok(()) => exit::SUCCESS,
            Err(error) => {
                eprintln!("acpilot serve: {error}");
                exit::FAILURE
            }
        }
    })
}

/// The address a browser can actually open.
///
/// `--bind 0.0.0.0` is the right address to listen on and the wrong one to type:
/// a wildcard is not a host. The port is what the line is about, so it is kept.
fn browse_at(addr: &str) -> String {
    match addr.rsplit_once(':') {
        Some((host, port)) if host == "0.0.0.0" || host == "::" || host == "[::]" => {
            format!("127.0.0.1:{port}")
        }
        _ => addr.to_owned(),
    }
}

/// The daemon this binary can run without a process (P6-11).
///
/// `acpilotd --profile DIR --workspace DIR` is the same launch written as flags;
/// this carries the two directories that launch would have passed, so embedded mode
/// serves exactly what a daemon started for this command would have served. It is a
/// value rather than a closure because the state is built only when nothing
/// answered — a run against a real daemon must not read a profile twice.
struct InProcess {
    /// The profile the daemon would have been told to serve.
    profile: Option<PathBuf>,
    /// The workspace root it would have been given.
    workspace: PathBuf,
}

impl acpilot_client::daemon::Embedded for InProcess {
    fn start(
        &self,
    ) -> Result<acpilot_client::daemon::Started, acpilot_client::daemon::ConnectError> {
        use acpilot_client::daemon::{ConnectError, Started};

        // The same startup the `acpilotd` binary runs, down to the terminal reaper:
        // a daemon that forgot one of them would behave differently depending on
        // who started it, which is the fork this task exists to prevent.
        let state = acpilotd::startup(self.profile.as_deref(), &self.workspace).map_err(|why| {
            ConnectError::Embedded {
                why: why.to_string(),
            }
        })?;
        let listener =
            acpilotd::embedded::Embedded::start(state).map_err(|why| ConnectError::Embedded {
                why: why.to_string(),
            })?;
        Ok(Started::new(
            listener.endpoint().to_owned(),
            listener.token().to_owned(),
            listener,
        ))
    }
}

/// The theme the terminal draws with, resolved from the settings' id.
///
/// The same extension the desktop app and the browser use, resolved by the same
/// code: `[preferences] theme_tui` overrides `theme`, and a project's
/// `.acpilot/config.toml` overrides either for that project alone. A theme that is
/// not installed is refused by name rather than drawn as the terminal's own
/// colours — a setting that silently does nothing is worse than a message.
fn tui_theme(settings: &Settings) -> Result<acpilot_tui::Theme, String> {
    let id = settings.preferences.theme_for(ThemeSurface::Tui);
    let extensions = acpilot_extensions::builtin::load_all().map_err(|errors| {
        let mut message = String::from("the built-in extensions do not load:");
        for error in errors {
            message.push_str("\n  ");
            message.push_str(&error.to_string());
        }
        message
    })?;
    let registry = acpilot_extensions::ThemeRegistry::new(&extensions);
    match registry.complete_with_warnings(id) {
        Ok((resolved, warnings)) => {
            // A theme that loads and breaks a rule is said out loud here, which is
            // the only place a theme is loaded by a person: the panel window is a
            // warning, not a refusal, so the theme still draws.
            for warning in warnings {
                eprintln!("acpilot tui: {warning}");
            }
            Ok(acpilot_tui::Theme::from_resolved(&resolved))
        }
        // Following the terminal makes the named theme the fallback for a terminal
        // that will not say what its colours are, so a name that is not installed
        // is a note rather than a refused launch.
        Err(error) if settings.preferences.theme_follow_terminal => {
            eprintln!(
                "acpilot tui: {error}; the terminal's own colours are followed, so this theme is \
                 only the fallback"
            );
            registry
                .complete("acpilot.night-flight")
                .map(|resolved| acpilot_tui::Theme::from_resolved(&resolved))
                .map_err(|error| format!("{error}, and no built-in dark theme to fall back to"))
        }
        Err(error) => Err(error.to_string()),
    }
}

/// The profile directory a root's settings name, when they name one.
///
/// A project that says `profile = "work"` means `<config>/profiles/work`. A name
/// that is not there is refused rather than ignored: starting a daemon on the
/// default profile because a name had a typo is how a work prompt goes out under a
/// personal key.
fn implied_profile(settings: &Settings) -> Result<Option<PathBuf>, u8> {
    let Some(name) = settings.profile.clone() else {
        return Ok(None);
    };
    match Profile::named(&name) {
        Ok(profile) => Ok(Some(profile.root().to_path_buf())),
        Err(error) => {
            eprintln!("acpilot tui: {error}");
            eprintln!(
                "acpilot tui: the project's settings name profile `{name}`; create {} or pass \
                 --profile <dir>",
                acpilot_profile::profiles_dir().join(&name).display()
            );
            Err(exit::CONFLICT)
        }
    }
}

/// The token a local daemon wrote, when there is one.
///
/// The client is given a token, never told to look for one — but `acpilot tui`
/// against a port on this machine should not require pasting a secret the daemon
/// already put in a `0600` file. Reading it here keeps that convenience in the
/// binary that has a user, rather than in a library that would then behave
/// differently depending on whose environment it runs in.
fn local_token() -> Option<String> {
    acpilot_host::token::read(&acpilot_host::token::path())
        .ok()
        .flatten()
}

/// The workspace id the daemon derives from a root: its directory name.
///
/// Duplicated knowledge, deliberately and narrowly: the alternative is a client
/// that cannot tell whether the daemon it found is serving the directory it was
/// asked about. If the derivation ever moves, this is the one place to follow.
fn expected_workspace_id(root: &std::path::Path) -> Option<String> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    root.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

fn run_ext(command: ExtCommand) -> u8 {
    match command {
        ExtCommand::Add { from, store } => {
            let paths = match Paths::resolve(store.store, None) {
                Ok(paths) => paths,
                Err(error) => return fail(&error),
            };
            match ext::add(&paths, &from) {
                Ok(line) => {
                    println!("{line}");
                    println!("approve it for a profile with `acpilot ext activate <id>`");
                    exit::SUCCESS
                }
                Err(error) => fail(&error),
            }
        }
        ExtCommand::ImportTheme {
            from,
            id,
            version,
            activate,
            store,
            config,
        } => {
            let paths = match Paths::resolve(store.store, config.config) {
                Ok(paths) => paths,
                Err(error) => return fail(&error),
            };
            match ext::import_theme(&paths, &from, &id, &version, activate.as_deref()) {
                Ok(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                    exit::SUCCESS
                }
                Err(error) => fail(&error),
            }
        }
        ExtCommand::Verify { all, store } => {
            if !all {
                eprintln!(
                    "acpilot ext verify: pass --all; verifying a subset is not a safety check"
                );
                return exit::USAGE;
            }
            let paths = match Paths::resolve(store.store, None) {
                Ok(paths) => paths,
                Err(error) => return fail(&error),
            };
            match ext::verify_all(&paths) {
                Ok((all_ready, lines)) => {
                    for line in lines {
                        println!("{line}");
                    }
                    if all_ready {
                        exit::SUCCESS
                    } else {
                        exit::FAILURE
                    }
                }
                Err(error) => fail(&error),
            }
        }
        ExtCommand::Prune {
            dry_run,
            store,
            config,
        } => {
            let paths = match Paths::resolve(store.store, config.config) {
                Ok(paths) => paths,
                Err(error) => return fail(&error),
            };
            match ext::prune(&paths, dry_run) {
                Ok(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                    exit::SUCCESS
                }
                Err(error) => fail(&error),
            }
        }
        ExtCommand::Activate { id, profile, store } => {
            let paths = match Paths::resolve(store.store, profile.config.config) {
                Ok(paths) => paths,
                Err(error) => return fail(&error),
            };
            match ext::activate(&paths, &profile.profile, &id) {
                Ok(line) => {
                    println!("{line}");
                    exit::SUCCESS
                }
                Err(error) => fail(&error),
            }
        }
        ExtCommand::Deactivate { id, profile, store } => {
            let paths = match Paths::resolve(store.store, profile.config.config) {
                Ok(paths) => paths,
                Err(error) => return fail(&error),
            };
            match ext::deactivate(&paths, &profile.profile, &id) {
                Ok(line) => {
                    println!("{line}");
                    exit::SUCCESS
                }
                Err(error) => fail(&error),
            }
        }
        ExtCommand::Why { id, profile, store } => {
            let paths = match Paths::resolve(store.store, profile.config.config) {
                Ok(paths) => paths,
                Err(error) => return fail(&error),
            };
            match ext::why(&paths, &profile.profile, &id) {
                Ok(text) => {
                    print!("{text}");
                    exit::SUCCESS
                }
                Err(error) => fail(&error),
            }
        }
    }
}

/// Run one `acpilot secret` verb against the OS keyring.
/// `acpilot token`: the one secret this product writes itself.
///
/// Printing the path rather than the token keeps it off a screen that is being
/// shared, and the file is `0600`: the person who needs it can `cat` it, and the
/// person who does not learn nothing from the command's output.
fn run_token(command: TokenCommand) -> u8 {
    match command {
        TokenCommand::Rotate => {
            let path = acpilot_host::token::path();
            match acpilot_host::token::rotate(&path) {
                Ok(_) => {
                    println!("wrote a new token to {}", path.display());
                    println!(
                        "the running daemon reads it on the next request, so nothing restarts and \
                         every client holding the old token is refused"
                    );
                    exit::SUCCESS
                }
                Err(error) => {
                    eprintln!(
                        "acpilot token rotate: cannot write {}: {error}",
                        path.display()
                    );
                    exit::FAILURE
                }
            }
        }
    }
}

fn run_secret(command: SecretCommand) -> u8 {
    match command {
        SecretCommand::Set { name, profile } => {
            let reference = match secret::reference(&name) {
                Ok(reference) => reference,
                Err(message) => {
                    eprintln!("error: {message}");
                    return exit::USAGE;
                }
            };
            let value = match secret::read_value(std::io::stdin().lock()) {
                Ok(value) => value,
                Err(message) => {
                    eprintln!("acpilot secret set: {message}");
                    return exit::FAILURE;
                }
            };
            let resolver = KeyringResolver::new(profile.profile);
            match secret::set(&resolver, &reference, &value) {
                Ok(account) => {
                    println!("stored {account} in the {} keyring", secret::service());
                    exit::SUCCESS
                }
                Err(message) => {
                    eprintln!("error: {message}");
                    exit::FAILURE
                }
            }
        }
        SecretCommand::List { profile } => {
            let directory =
                match secret::profile_dir(profile.config.config.as_ref(), &profile.profile) {
                    Ok(directory) => directory,
                    Err(message) => {
                        eprintln!("error: {message}");
                        return exit::FAILURE;
                    }
                };
            let resolver = KeyringResolver::new(profile.profile);
            match secret::list(&directory, &resolver) {
                Ok(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                    exit::SUCCESS
                }
                Err(message) => {
                    eprintln!("error: {message}");
                    exit::FAILURE
                }
            }
        }
        SecretCommand::Rm { name, profile } => {
            let reference = match secret::reference(&name) {
                Ok(reference) => reference,
                Err(message) => {
                    eprintln!("error: {message}");
                    return exit::USAGE;
                }
            };
            let resolver = KeyringResolver::new(profile.profile);
            let account = resolver.account(&reference);
            match secret::remove(&resolver, &reference) {
                Ok(true) => {
                    println!("removed {account} from the {} keyring", secret::service());
                    exit::SUCCESS
                }
                Ok(false) => {
                    eprintln!(
                        "no secret at {account} in the {} keyring",
                        secret::service()
                    );
                    exit::NOT_FOUND
                }
                Err(message) => {
                    eprintln!("error: {message}");
                    exit::FAILURE
                }
            }
        }
    }
}

/// Print an extension failure with the exit code scripts depend on.
fn fail(error: &ExtensionError) -> u8 {
    eprintln!("error: {error}");
    match error {
        ExtensionError::NotFound { .. } => exit::NOT_FOUND,
        ExtensionError::AlreadyInstalled { .. } | ExtensionError::Duplicate { .. } => {
            exit::CONFLICT
        }
        _ => exit::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_file_is_opened_relative_to_the_workspace() {
        let directory = tempfile::tempdir().expect("temp dir");
        let nested = directory.path().join("src");
        std::fs::create_dir(&nested).expect("src");
        let file = nested.join("main.rs");
        std::fs::write(&file, "fn main() {}").expect("file");
        let root = std::fs::canonicalize(directory.path()).expect("root");

        assert_eq!(
            initial_file(Some(&file), &root).as_deref(),
            Some("src/main.rs"),
            "the daemon addresses files by workspace-relative path"
        );
        assert_eq!(
            initial_file(Some(&root), &root),
            None,
            "a directory names no file"
        );
        assert_eq!(initial_file(None, &root), None);
    }

    #[test]
    fn a_wildcard_bind_is_printed_as_a_host_a_browser_can_open() {
        // The address to listen on and the address to type are not the same one:
        // `0.0.0.0` is a wildcard, not a host a browser resolves.
        assert_eq!(browse_at("0.0.0.0:7777"), "127.0.0.1:7777");
        assert_eq!(browse_at("[::]:7777"), "127.0.0.1:7777");
        assert_eq!(browse_at("127.0.0.1:7777"), "127.0.0.1:7777");
    }
}
