//! The argument surface, kept out of `main` so it can be tested.
//!
//! Every command here is either a filesystem operation or (later) a call into
//! `acpilot-proto`; none of them reimplements daemon policy.

use std::path::{Path, PathBuf};

use acpilot_agent::keyring::KeyringResolver;
use acpilot_core::error::exit;
use acpilot_extensions::ExtensionError;
use acpilot_profile::launch::ProfileResolver;
use acpilot_profile::project::Root;
use acpilot_profile::settings::Settings;
use acpilot_profile::{ProfileId, ProfileRoots};
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
    /// Explain which settings file a value came from.
    Config {
        /// Which config verb.
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Create, inspect, and remove profiles.
    Profile {
        /// Which profile verb.
        #[command(subcommand)]
        command: ProfileCommand,
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
        /// Profile name, or a profile directory.
        ///
        /// Only used when this command runs the daemon itself; a daemon that is
        /// already running keeps the profile it was started with, and naming a
        /// different one here is an error rather than a silent mismatch. Without
        /// it, resolution follows `ACPILOT_PROFILE`, the project's marker, the
        /// global `active_profile`, and finally `default`.
        #[arg(long, value_name = "NAME|DIR")]
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
    /// Serve the daemon and the web UI over HTTP, printing its address and token.
    ///
    /// One process, serving the same state `/v1` answers from: a browser at the
    /// printed URL talks to the daemon a TUI would attach to, not to a copy of it.
    /// Every request needs `Authorization: Bearer <token>` except the page itself,
    /// which a browser cannot give a header to when the address is typed.
    ///
    /// A loopback port is picked by the operating system unless `--bind` names one,
    /// and a token is generated when `--token` does not supply one. Both are printed
    /// when the listener is up, in one panel, because a URL without the token stops
    /// at the page's token field and a token without the port names nothing.
    Serve {
        /// Accepted for compatibility; `serve` already listens on loopback.
        #[arg(long, hide = true)]
        http: bool,
        /// The address to bind, for example `0.0.0.0:7777`.
        ///
        /// Without it the OS picks a free loopback port and the panel prints it.
        #[arg(long, value_name = "ADDR")]
        bind: Option<String>,
        /// Use this bearer token instead of generating one.
        ///
        /// It is written to the runtime token file the daemon reads. Without it a
        /// token is generated on first use and reused, so a browser keeps working
        /// across restarts.
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
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

/// Everything under `acpilot config`.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the value of one setting and the layer that won.
    ///
    /// The layers are the defaults, the global `config.toml`, the profile's
    /// `config.toml` and `profile.toml`, the project's `.acpilot/config.toml`, the
    /// `ACPILOT_*` environment, and the flags this command was given. A bare key
    /// is a preference: `tab_width` means `preferences.tab_width`.
    Explain {
        /// Dotted key, for example `preferences.tab_width` or `profile`.
        key: String,
        /// A directory, or a file inside one. Defaults to this directory.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
        /// Profile whose files are part of the cascade.
        #[arg(long, env = "ACPILOT_PROFILE", value_name = "NAME")]
        profile: Option<String>,
    },
}

/// Everything under `acpilot profile`.
///
/// `new`, `list` and `use` are P4-9; `rm` is here because P4-4 owns deleting the
/// state a profile created, and a removal that forgets the database is not one.
#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// Move a profile and its state to the OS trash, or delete them with `--purge`.
    ///
    /// The config directory and every separate state root are moved, so the
    /// database and transcripts do not outlive the identity they belonged to.
    /// Without `--purge` the paths are recoverable from the platform trash.
    Rm {
        /// Profile name under `<config>/profiles`.
        name: String,
        /// Delete outright instead of moving to the OS trash.
        #[arg(long)]
        purge: bool,
        /// Where profiles live, when it is not the default config root.
        #[command(flatten)]
        config: ConfigArgs,
    },
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
        Some(Command::Config { command }) => run_config(command),
        Some(Command::Profile { command }) => run_profile(command),
        Some(Command::Serve {
            http: _,
            bind,
            token,
            profile,
            workspace,
        }) => run_serve(ServeFlags {
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
    let daemon = probe_daemon(
        &endpoint,
        token.or_else(acpilot_host::token::local).as_deref(),
    );
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

/// Remove a profile's config and state, or move them to the OS trash.
fn run_profile(command: ProfileCommand) -> u8 {
    match command {
        ProfileCommand::Rm {
            name,
            purge,
            config,
        } => {
            let id = match ProfileId::parse(&name) {
                Ok(id) => id,
                Err(error) => {
                    eprintln!("acpilot profile rm: {error}");
                    return exit::USAGE;
                }
            };
            let root = config.config.unwrap_or_else(acpilot_profile::config_dir);
            let directory = root.join("profiles").join(id.as_str());
            let roots = ProfileRoots::for_id(&id);

            // Config and data are separate on a platform that draws that line and
            // the same directory under ACPILOT_HOME. Sorting and deduplicating is
            // what stops the shared case from trying to remove one directory twice.
            // The id is parsed rather than the config read: a profile whose
            // `config.toml` is broken is exactly one a person may want gone.
            let mut paths = vec![
                directory.clone(),
                roots.data,
                roots.cache,
                roots.state,
                roots.runtime,
            ];
            paths.sort();
            paths.dedup();
            if !paths.iter().any(|path| path.exists()) {
                eprintln!(
                    "acpilot profile rm: {} does not exist",
                    directory.join(acpilot_profile::CONFIG_FILE).display()
                );
                return exit::NOT_FOUND;
            }

            let mut removed = 0;
            for path in paths.into_iter().filter(|path| path.exists()) {
                let outcome = if purge {
                    std::fs::remove_dir_all(&path).map_err(|error| error.to_string())
                } else {
                    trash::delete(&path).map_err(|error| error.to_string())
                };
                if let Err(error) = outcome {
                    eprintln!("acpilot profile rm: {}: {error}", path.display());
                    return exit::FAILURE;
                }
                removed += 1;
                if purge {
                    println!("removed {}", path.display());
                } else {
                    println!("moved {} to the trash", path.display());
                }
            }
            if removed == 0 {
                eprintln!("acpilot profile rm: {name} has no files to remove");
                return exit::NOT_FOUND;
            }

            // The index is disposable, so a failure here is a warning rather than
            // a failed removal: the profile is already gone, and discovery rebuilds
            // the rows it needs from the directories that remain.
            match acpilot_store::Index::open(acpilot_profile::index_path())
                .and_then(|index| index.forget_profile(id.as_str()))
            {
                Ok(_) => {}
                Err(error) => {
                    eprintln!("acpilot profile rm: the profile index was not updated: {error}")
                }
            }
            exit::SUCCESS
        }
    }
}

/// Explain one setting's winner, from the same cascade a launch reads.
fn run_config(command: ConfigCommand) -> u8 {
    let ConfigCommand::Explain { key, path, profile } = command;
    let root = match Root::resolve(path.as_deref()) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("acpilot config explain: {error}");
            return exit::FAILURE;
        }
    };
    // The same five layers a launch reads, answered by the same resolver. A
    // name that does not resolve is exit 3 with the command that fixes it, not
    // a quiet read of `default`.
    let resolved = match ProfileResolver::new()
        .flag(profile.as_deref().map(Path::new))
        .process_environment()
        .resolve(&root)
    {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("acpilot config explain: {error}");
            return if error.is_missing() {
                exit::NOT_FOUND
            } else {
                exit::FAILURE
            };
        }
    };
    let user = acpilot_profile::user_config_path();
    let cascade = match acpilot_profile::settings::Cascade::for_launch(
        Some(user.as_path()),
        root.project()
            .map(acpilot_profile::project::Project::config_path)
            .as_deref(),
        Some(resolved.profile()),
    ) {
        Ok(cascade) => cascade,
        Err(error) => {
            eprintln!("acpilot config explain: {error}");
            return exit::FAILURE;
        }
    };
    match cascade.explain(&key) {
        Ok(explanation) => {
            println!("{} = {}", explanation.key, explanation.render_value());
            println!("  from {}", explanation.origin);
            exit::SUCCESS
        }
        Err(error) => {
            eprintln!("acpilot config explain: {error}");
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
    // One resolution order for the profile, so the theme and the daemon cannot
    // read two different identities. The flag may still name a directory; a name
    // comes from the environment, the project marker, `active_profile`, or the
    // created `default`.
    let resolved = match ProfileResolver::new()
        .flag(profile.as_deref())
        .process_environment()
        .resolve(&root)
    {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("acpilot tui: {error}");
            return if error.is_missing() {
                exit::NOT_FOUND
            } else {
                exit::FAILURE
            };
        }
    };
    let profile = resolved.profile().clone();
    // The theme is resolved after the profile is known, because `profile.toml` is
    // a layer of the same cascade: reading it before that would draw the global
    // theme for the first frame and the profile's after a save.
    let settings = match root.settings_with_profile(Some(&profile)) {
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
    // The automatic default is not a disagreement with a running daemon: it is
    // the profile every machine has. A flag, environment variable, marker or
    // `active_profile` is a choice the daemon may not share.
    let implied = !resolved.is_default();

    let token = token.or_else(acpilot_host::token::local);
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
        profile: Some(profile.root().to_path_buf()),
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
        profile: Some(profile.root().to_path_buf()),
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

/// The address `acpilot serve` asks for when no `--bind` names one.
///
/// Port zero is a request, not an address: the OS picks a free loopback port and
/// the listener it hands back is the one the panel prints.
const DEFAULT_SERVE_ADDR: &str = "127.0.0.1:0";

/// What `acpilot serve` was asked for.
struct ServeFlags {
    /// The address to bind.
    bind: Option<String>,
    /// A token to use instead of the one on disk.
    token: Option<String>,
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
        bind,
        token,
        profile,
        workspace,
    } = flags;
    let addr = bind.unwrap_or_else(|| DEFAULT_SERVE_ADDR.to_owned());

    // Before the bind, like the daemon does it: a listener that is up for even a
    // moment without an answer to "who are you" is the hole the token closes.
    let token_path = acpilot_host::token::path();
    let secret = match token {
        Some(token) => {
            // A header value cannot carry whitespace, and an empty token would
            // accept `Authorization: Bearer ` — which is every request from a
            // client that forgot the secret.
            let token = token.trim();
            if token.is_empty() || token.chars().any(char::is_whitespace) {
                eprintln!(
                    "acpilot serve: --token must be a non-empty bearer token without whitespace"
                );
                return exit::USAGE;
            }
            if let Err(error) = acpilot_host::token::store(&token_path, token) {
                eprintln!(
                    "acpilot serve: cannot write {}: {error}",
                    token_path.display()
                );
                return exit::FAILURE;
            }
            token.to_owned()
        }
        None => match acpilot_host::token::load_or_create(&token_path) {
            Ok(secret) => secret,
            Err(error) => {
                eprintln!(
                    "acpilot serve: cannot read or write {}: {error}",
                    token_path.display()
                );
                return exit::FAILURE;
            }
        },
    };

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

    let started = std::time::Instant::now();
    runtime.block_on(async move {
        let served = acpilotd::serve_with_listening(
            acpilotd::profiles::router(profiles.clone()),
            acpilotd::Target::Tcp(addr),
            acpilotd::shutdown(),
            move |bound| {
                print!(
                    "{}",
                    serve_panel(&bound, &secret, &token_path, started.elapsed())
                )
            },
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

/// The address and token, in the shape a dev server prints.
///
/// One panel, because the two facts only help together: a URL without the token
/// stops at the page's token field, and a token without the port names nothing.
/// Plain text, so it reads the same in a terminal, a CI log and a scrollback;
/// colour would be the only thing lost when either is redirected.
fn serve_panel(
    bound: &acpilotd::Bound,
    token: &str,
    token_path: &std::path::Path,
    ready_in: std::time::Duration,
) -> String {
    let local = match bound {
        acpilotd::Bound::Tcp(addr) => format!("http://{}/", browse_at(&addr.to_string())),
        acpilotd::Bound::Socket(path) => path.display().to_string(),
    };
    let mut panel = format!(
        "\n┌  acpilot {} · ready in {} ms\n│\n│  Local       {local}\n",
        acpilot_core::VERSION,
        ready_in.as_millis()
    );
    // A wildcard bind is the right thing to listen on and the wrong thing to
    // type, so when the two differ the panel says both rather than guessing which
    // one the person needs.
    if let acpilotd::Bound::Tcp(addr) = bound {
        let addr = addr.to_string();
        if browse_at(&addr) != addr {
            panel.push_str(&format!("│  Bound       {addr}\n"));
        }
    }
    panel.push_str(&format!(
        "│  Token       {token}\n│  Token file  {}\n│\n│  Open the page and paste the token when it asks.\n└  Ctrl-C to stop\n\n",
        token_path.display()
    ));
    panel
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

    #[test]
    fn the_serve_panel_prints_the_bound_port_and_the_token() {
        let bound = acpilotd::Bound::Tcp("127.0.0.1:54321".parse().expect("address"));
        let panel = serve_panel(
            &bound,
            "abc123",
            std::path::Path::new("/run/user/1000/acpilot/token"),
            std::time::Duration::from_millis(12),
        );

        assert!(panel.contains("http://127.0.0.1:54321/"), "{panel}");
        assert!(panel.contains("Token       abc123"), "{panel}");
        assert!(panel.contains("ready in 12 ms"), "{panel}");
        assert!(
            !panel.contains("Bound"),
            "loopback is the address and what it bound in one: {panel}"
        );
    }

    #[test]
    fn a_wildcard_panel_names_what_it_bound_as_well_as_what_to_open() {
        let bound = acpilotd::Bound::Tcp("0.0.0.0:54321".parse().expect("address"));
        let panel = serve_panel(
            &bound,
            "abc123",
            std::path::Path::new("/run/user/1000/acpilot/token"),
            std::time::Duration::from_millis(1),
        );

        assert!(
            panel.contains("Local       http://127.0.0.1:54321/"),
            "{panel}"
        );
        assert!(panel.contains("Bound       0.0.0.0:54321"), "{panel}");
    }
}
