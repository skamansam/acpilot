//! The argument surface, kept out of `main` so it can be tested.
//!
//! Every command here is either a filesystem operation or (later) a call into
//! `acpilot-proto`; none of them reimplements daemon policy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use acpilot_agent::keyring::KeyringResolver;
use acpilot_core::error::exit;
use acpilot_extensions::ExtensionError;
use acpilot_profile::catalog::{ProfileRecord, Registry};
use acpilot_profile::launch::ProfileResolver;
use acpilot_profile::project::Root;
use acpilot_profile::providers::{DeclaredProvider, ProvidersFile};
use acpilot_profile::settings::{Cascade, Settings};
use acpilot_profile::{ProfileId, ProfileRoots};
use acpilot_proto::API_PREFIX;
use acpilot_proto::model::{AddProviderRequest, ProviderStatus, ProviderSummary};
use acpilot_proto::profile::ThemeSurface;
use acpilot_proto::workspace::WorkspaceOpenRequest;
use clap::{Args, Parser, Subcommand};

use crate::doctor;
use crate::export;
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
    /// Profile name, or a profile directory.
    ///
    /// Accepted before or after the subcommand, because every verb that has an
    /// opinion about identity reads one choice. A single segment is a profile
    /// name under `<config>/profiles`; a path opens that directory. `tui` and
    /// `serve` use it only when they start the daemon: a daemon already running
    /// keeps the profile it was started with, and naming a different one there is
    /// an error rather than a silent mismatch. A name that does not resolve is an
    /// error, never a quieter fallback: guessing sends a work prompt with a
    /// personal key.
    #[arg(long, global = true, env = "ACPILOT_PROFILE", value_name = "NAME|DIR")]
    pub profile: Option<String>,
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
    /// Open, list, and remove workspaces.
    Workspace {
        /// Which workspace verb.
        #[command(subcommand)]
        command: WorkspaceCommand,
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
    /// Probe a declared model server, and print what it answered.
    ///
    /// Reads the profile's `providers.toml`; starts no daemon and needs none. A
    /// probe is the only thing that contacts a model server, so it is a command a
    /// person runs rather than something a startup does on their behalf. A server
    /// that is down is reported with its reason, not retried in the background.
    Models {
        /// Which models verb.
        #[command(subcommand)]
        command: ModelsCommand,
    },
    /// List and remove the agents a profile declares.
    Agents {
        /// Which agents verb.
        #[command(subcommand)]
        command: AgentsCommand,
    },
}

/// Everything under `acpilot models`.
#[derive(Debug, Subcommand)]
pub enum ModelsCommand {
    /// Ask a declared provider what it has.
    ///
    /// With no id, every enabled provider is asked. A disabled provider is
    /// reported as disabled and is not contacted, even when it is named: that is
    /// the decision the file records.
    Probe {
        /// Provider id from the profile's `providers.toml`.
        id: Option<String>,
    },
}

/// Everything under `acpilot agents`.
#[derive(Debug, Subcommand)]
pub enum AgentsCommand {
    /// Remove the `agents.toml` entry promotion wrote for a recipe.
    ///
    /// This is the inverse of `POST /v1/recipes/{id}/promote`: the block leaves,
    /// and when it was the only agent the file is removed rather than left as an
    /// empty manifest the loader would refuse. An entry a person wrote by hand is
    /// refused, because promotion never owned it.
    Rm {
        /// Agent id, as the manifest spells it.
        id: String,
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
    },
}

/// Everything under `acpilot workspace`.
#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// Open a directory as a workspace, or return the row it already has.
    ///
    /// The daemon checks that the root is a directory on its host first. A root
    /// another profile opened is refused unless `--steal` says to transfer it.
    Open {
        /// Directory to open.
        path: PathBuf,
        /// Take the root from the profile that opened it first.
        ///
        /// The daemon records the new owner and logs the transfer. Without this,
        /// the open is a `409 profile_mismatch` naming the current owner.
        #[arg(long)]
        steal: bool,
        /// Endpoint of the daemon: a URL, or a unix socket path.
        #[arg(long, env = "ACPILOT_API_URL", value_name = "URL|PATH")]
        api_url: Option<String>,
        /// Bearer token for a daemon on a TCP endpoint.
        #[arg(long, env = "ACPILOT_TOKEN", value_name = "TOKEN")]
        token: Option<String>,
    },
}

/// Everything under `acpilot profile`.
///
/// Every verb here is a filesystem operation: a profile is a directory and a row
/// in the registry, so none of them needs a daemon, and one that did would fail
/// on the machine that most needs it — a first install with nothing running.
#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// Create a profile: its directories, its `config.toml` and its registry row.
    ///
    /// The profile is usable from the next command. The first profile created on
    /// a machine becomes the active one, because that is what a launch would have
    /// created on its own.
    New {
        /// Profile name: lower-case letters, digits and hyphens, 2 to 32 characters.
        name: String,
        /// Human label to draw. The name when absent.
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        /// Profile whose preferences this one inherits beneath its own.
        #[arg(long, value_name = "NAME")]
        extends: Option<String>,
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// List the profiles on this machine, marking the active one.
    Ls {
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Print one profile's registry row and the directories it owns.
    Show {
        /// Profile name under `<config>/profiles`.
        name: String,
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Make a profile the one a launch picks when nothing names another.
    ///
    /// Writes `active_profile` in the registry, which is the layer a launch reads
    /// after the flag, the environment and the project's marker. A daemon that is
    /// already running keeps the profile it was started with.
    Use {
        /// Profile name under `<config>/profiles`.
        name: String,
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Import a profile from a document `acpilot profile export` wrote.
    ///
    /// Configuration files only. A `secrets` map is reported and skipped, because
    /// a credential travels through `acpilot secret set` and never through a
    /// document that may have been copied or committed.
    Import {
        /// File written by `acpilot profile export`.
        file: PathBuf,
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Print every effective setting of two profiles, with the file that set it.
    ///
    /// The answer to "why is it doing that?": a key that differs names the value
    /// on each side and the layer that won, which is the same cascade a launch
    /// reads rather than a second implementation of it.
    Diff {
        /// Left profile.
        a: String,
        /// Right profile.
        b: String,
        #[command(flatten)]
        config: ConfigArgs,
    },
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
    /// Write this profile's configuration to a file, or print it.
    ///
    /// Keyring references stay references. `--with-secrets` adds a separate
    /// `secrets` map instead of rewriting a manifest into a plaintext credential,
    /// and requires an output file so the values never land in a terminal log.
    Export {
        /// Profile name under `<config>/profiles`.
        name: String,
        /// File to write. Without it the document is printed to stdout.
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Include the keyring values this profile's agents reference.
        #[arg(long)]
        with_secrets: bool,
        /// Acknowledge that the export file will contain plaintext credentials.
        #[arg(long)]
        i_understand_the_risk: bool,
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
        profile: SecretProfileArgs,
    },
    /// List the secrets this profile references, and whether they are set.
    List {
        #[command(flatten)]
        profile: SecretProfileArgs,
    },
    /// Remove a secret.
    Rm {
        /// Entry name, `name` or `profile/name`, with or without `keyring:`.
        name: String,
        #[command(flatten)]
        profile: SecretProfileArgs,
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
        config: ConfigArgs,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Withdraw approval from one profile.
    Deactivate {
        /// Extension id, `publisher.name`.
        id: String,
        #[command(flatten)]
        config: ConfigArgs,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Explain how a preset or agent resolves for one profile.
    Why {
        /// Preset or agent id, for example `rust-analyzer`.
        id: String,
        #[command(flatten)]
        config: ConfigArgs,
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

/// Where a secret verb reads its config root.
///
/// The profile itself is the global `--profile`, or the five-layer launch
/// resolution when that is absent: `acpilot secret set` writes into the active
/// profile rather than into `default`.
#[derive(Debug, Args)]
pub struct SecretProfileArgs {
    #[command(flatten)]
    pub config: ConfigArgs,
}

/// Run a parsed command and return the process exit code.
pub fn run(cli: Cli) -> u8 {
    let Cli { profile, command } = cli;
    match command {
        Some(Command::Version) => {
            println!("acpilot {}", acpilot_core::VERSION);
            println!("api      {API_PREFIX}");
            exit::SUCCESS
        }
        Some(Command::Ext { command }) => run_ext(command, profile.as_deref()),
        Some(Command::Secret { command }) => run_secret(command, profile.as_deref()),
        Some(Command::Token { command }) => run_token(command),
        Some(Command::Config { command }) => run_config(command, profile.as_deref()),
        Some(Command::Profile { command }) => run_profile(command),
        Some(Command::Workspace { command }) => run_workspace(command, profile.as_deref()),
        Some(Command::Serve {
            http: _,
            bind,
            token,
            workspace,
        }) => run_serve(ServeFlags {
            bind,
            token,
            profile: profile.map(PathBuf::from),
            workspace,
        }),
        Some(Command::Tui {
            path,
            api_url,
            no_spawn,
            workspace,
            token,
            demo,
            no_mouse,
        }) => run_tui(TuiFlags {
            path,
            api_url,
            no_spawn,
            profile: profile.map(PathBuf::from),
            workspace,
            token,
            demo,
            mouse: !no_mouse,
        }),
        Some(Command::Models { command }) => run_models(command, profile.as_deref()),
        Some(Command::Agents { command }) => run_agents(command, profile.as_deref()),
        Some(Command::Doctor {
            path,
            api_url,
            token,
        }) => run_doctor(
            path.as_deref(),
            profile
                .as_deref()
                .unwrap_or(acpilot_profile::launch::DEFAULT_PROFILE),
            api_url.as_deref(),
            token,
        ),

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

/// Open a workspace against a running daemon, starting one when it is not there.
fn run_workspace(command: WorkspaceCommand, profile: Option<&str>) -> u8 {
    let WorkspaceCommand::Open {
        path,
        steal,
        api_url,
        token,
    } = command;
    let root = match std::fs::canonicalize(&path) {
        Ok(root) if root.is_dir() => root,
        Ok(_) => {
            eprintln!(
                "acpilot workspace open: {} is a file, and a workspace is a directory",
                path.display()
            );
            return exit::USAGE;
        }
        Err(error) => {
            eprintln!("acpilot workspace open: {}: {error}", path.display());
            return exit::NOT_FOUND;
        }
    };
    let root = root.to_string_lossy().into_owned();
    // `--profile` is a name at this surface, and `acpilotd` wants the directory
    // that name resolves to. Resolving here keeps `workspace open --profile work`
    // from starting a daemon that treats `work` as a relative path.
    let profile_directory = match profile {
        Some(name) => match acpilot_profile::Profile::named(name) {
            Ok(profile) => Some(profile.root().to_path_buf()),
            Err(error) => {
                eprintln!("acpilot workspace open: {error}");
                return exit::NOT_FOUND;
            }
        },
        None => None,
    };
    let mut daemon_args = Vec::new();
    if let Some(directory) = &profile_directory {
        daemon_args.push("--profile".to_owned());
        daemon_args.push(directory.display().to_string());
    }
    daemon_args.push("--workspace".to_owned());
    daemon_args.push(root.clone());
    let options = acpilot_client::daemon::Options {
        endpoint: api_url,
        no_spawn: false,
        token: token.or_else(acpilot_host::token::local),
        daemon_args,
    };
    let running = match acpilot_client::daemon::ensure_blocking(&options) {
        Ok(running) => running,
        Err(error) => {
            eprintln!("acpilot workspace open: {error}");
            return exit::FAILURE;
        }
    };
    let request = WorkspaceOpenRequest {
        host_id: "local".to_owned(),
        root,
        name: None,
        steal,
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("acpilot workspace open: {error}");
            return exit::FAILURE;
        }
    };
    match runtime.block_on(running.client.open_workspace(profile, &request)) {
        Ok(opened) => {
            println!(
                "opened {} (profile {})",
                opened.id,
                if opened.profile.is_empty() {
                    "none"
                } else {
                    &opened.profile
                }
            );
            exit::SUCCESS
        }
        Err(acpilot_client::ClientError::Api {
            status: 409,
            message,
            ..
        }) => {
            eprintln!("acpilot workspace open: {message}");
            if !steal {
                eprintln!("  pass --steal to transfer the root from its current owner");
            }
            exit::CONFLICT
        }
        Err(error) => {
            eprintln!("acpilot workspace open: {error}");
            if matches!(error, acpilot_client::ClientError::Api { status: 404, .. }) {
                exit::NOT_FOUND
            } else {
                exit::FAILURE
            }
        }
    }
}

/// Where a profile verb looks for profiles and the registry.
fn profile_config_root(config: &ConfigArgs) -> PathBuf {
    config
        .config
        .clone()
        .unwrap_or_else(acpilot_profile::config_dir)
}

/// The registry file: `config.toml` at the config root.
fn profile_registry_path(root: &Path) -> PathBuf {
    root.join(acpilot_profile::CONFIG_FILE)
}

/// The directories `id` owns, under the root this command was given.
///
/// The platform's split (config, data, cache, state, runtime) when the root is the
/// platform's own; one root with the same shape when `--config` names another,
/// which is what a portable install and a test own. `profile rm` uses this too, so
/// a removal takes away exactly what a creation made.
fn profile_roots(root: &Path, id: &ProfileId) -> ProfileRoots {
    if root == acpilot_profile::config_dir() {
        ProfileRoots::for_id(id)
    } else {
        ProfileRoots::under(root, id)
    }
}

/// `acpilot profile new`.
fn create_profile(
    name: &str,
    label: Option<String>,
    extends: Option<String>,
    config: &ConfigArgs,
) -> u8 {
    let id = match ProfileId::user(name) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("acpilot profile new: {error}");
            return exit::USAGE;
        }
    };
    let root = profile_config_root(config);
    let registry_path = profile_registry_path(&root);
    let mut registry = match Registry::load(&registry_path) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("acpilot profile new: {error}");
            return exit::FAILURE;
        }
    };
    let extends = match extends.as_deref().map(ProfileId::parse).transpose() {
        Ok(extends) => extends,
        Err(error) => {
            eprintln!("acpilot profile new: {error}");
            return exit::USAGE;
        }
    };
    if extends.as_ref() == Some(&id) {
        eprintln!("acpilot profile new: a profile cannot extend itself");
        return exit::USAGE;
    }
    if let Some(parent) = &extends {
        // A row or a directory is enough: a profile written by hand is one a new
        // profile may inherit from, and `profile rm` leaves no row behind.
        let row = registry.profile(parent).is_some();
        let directory = root.join("profiles").join(parent.as_str()).is_dir();
        if !row && !directory {
            eprintln!("acpilot profile new: no profile `{parent}` to extend");
            return exit::NOT_FOUND;
        }
    }
    let roots = profile_roots(&root, &id);
    if registry.profile(&id).is_some() || roots.config_file().exists() {
        eprintln!("acpilot profile new: profile `{id}` already exists");
        return exit::CONFLICT;
    }
    if let Err(error) = roots.create() {
        eprintln!("acpilot profile new: {}: {error}", roots.config.display());
        return exit::FAILURE;
    }
    if let Err(error) = std::fs::write(roots.config_file(), "schema_version = 1\n") {
        let _ = std::fs::remove_dir_all(&roots.config);
        eprintln!(
            "acpilot profile new: {}: {error}",
            roots.config_file().display()
        );
        return exit::FAILURE;
    }
    let label = label
        .map(|label| label.trim().to_owned())
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| id.as_str().to_owned());
    registry.insert(
        id.clone(),
        ProfileRecord {
            label,
            extends,
            ..ProfileRecord::default()
        },
    );
    if registry.active_profile.is_none() {
        registry.active_profile = Some(id.clone());
    }
    if let Err(error) = registry.save(&registry_path) {
        let _ = std::fs::remove_dir_all(&roots.config);
        eprintln!("acpilot profile new: {error}");
        return exit::FAILURE;
    }
    println!("created {id} at {}", roots.config.display());
    exit::SUCCESS
}

/// `acpilot profile ls`.
fn list_profiles(config: &ConfigArgs) -> u8 {
    let root = profile_config_root(config);
    let registry = match Registry::load(&profile_registry_path(&root)) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("acpilot profile ls: {error}");
            return exit::FAILURE;
        }
    };
    if registry.profiles.is_empty() {
        println!("no profiles in {}", root.display());
        println!("  acpilot profile new work creates one");
        return exit::SUCCESS;
    }
    for (id, record) in &registry.profiles {
        let marker = if registry.active_profile.as_ref() == Some(id) {
            "*"
        } else {
            " "
        };
        let extends = record
            .extends
            .as_ref()
            .map_or_else(String::new, |parent| format!("  extends {parent}"));
        println!(
            "{marker} {:<16} {}{extends}",
            id.as_str(),
            record.label_or(id)
        );
    }
    exit::SUCCESS
}

/// `acpilot profile show`.
fn show_profile(name: &str, config: &ConfigArgs) -> u8 {
    let id = match ProfileId::parse(name) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("acpilot profile show: {error}");
            return exit::USAGE;
        }
    };
    let root = profile_config_root(config);
    let registry = match Registry::load(&profile_registry_path(&root)) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("acpilot profile show: {error}");
            return exit::FAILURE;
        }
    };
    let roots = profile_roots(&root, &id);
    let record = registry.profile(&id);
    if record.is_none() && !roots.config_file().exists() {
        eprintln!(
            "acpilot profile show: no profile `{id}`; create it with `acpilot profile new {id}`"
        );
        return exit::NOT_FOUND;
    }
    println!("profile    {id}");
    match record {
        Some(record) => {
            println!("label      {}", record.label_or(&id));
            println!(
                "extends    {}",
                record.extends.as_ref().map_or("none", ProfileId::as_str)
            );
            println!(
                "created    {}",
                record.created_at.as_deref().unwrap_or("not recorded")
            );
        }
        None => println!("registry   no row · the directory exists without one"),
    }
    let active = registry.active_profile.as_ref() == Some(&id);
    println!("active     {active}");
    for (label, path) in [
        ("config", &roots.config),
        ("data", &roots.data),
        ("cache", &roots.cache),
        ("state", &roots.state),
        ("runtime", &roots.runtime),
    ] {
        let state = if path.exists() { "present" } else { "missing" };
        println!("{label:<10} {} · {state}", path.display());
    }
    exit::SUCCESS
}

/// `acpilot profile use`.
fn use_profile(name: &str, config: &ConfigArgs) -> u8 {
    let id = match ProfileId::parse(name) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("acpilot profile use: {error}");
            return exit::USAGE;
        }
    };
    let root = profile_config_root(config);
    let registry_path = profile_registry_path(&root);
    let mut registry = match Registry::load(&registry_path) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("acpilot profile use: {error}");
            return exit::FAILURE;
        }
    };
    if registry.profile(&id).is_none() {
        eprintln!(
            "acpilot profile use: no profile `{id}`; create it with `acpilot profile new {id}`"
        );
        return exit::NOT_FOUND;
    }
    if registry.active_profile.as_ref() == Some(&id) {
        println!("{id} is already the active profile");
        return exit::SUCCESS;
    }
    registry.active_profile = Some(id.clone());
    if let Err(error) = registry.save(&registry_path) {
        eprintln!("acpilot profile use: {error}");
        return exit::FAILURE;
    }
    println!("active profile is now {id}");
    exit::SUCCESS
}

/// `acpilot profile import`.
fn import_profile(file: &Path, config: &ConfigArgs) -> u8 {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("acpilot profile import: {}: {error}", file.display());
            return exit::NOT_FOUND;
        }
    };
    let document: serde_json::Value = match serde_json::from_str(&text) {
        Ok(document) => document,
        Err(error) => {
            eprintln!(
                "acpilot profile import: {}: not an export document: {error}",
                file.display()
            );
            return exit::USAGE;
        }
    };
    let Some(name) = document.get("profile").and_then(serde_json::Value::as_str) else {
        eprintln!("acpilot profile import: the document names no profile");
        return exit::USAGE;
    };
    let id = match ProfileId::user(name) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("acpilot profile import: {error}");
            return exit::USAGE;
        }
    };
    let Some(files) = document.get("files").and_then(serde_json::Value::as_object) else {
        eprintln!("acpilot profile import: the document has no files table");
        return exit::USAGE;
    };
    let root = profile_config_root(config);
    let registry_path = profile_registry_path(&root);
    let mut registry = match Registry::load(&registry_path) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("acpilot profile import: {error}");
            return exit::FAILURE;
        }
    };
    let roots = profile_roots(&root, &id);
    if registry.profile(&id).is_some() || roots.config_file().exists() {
        eprintln!("acpilot profile import: profile `{id}` already exists");
        return exit::CONFLICT;
    }
    if let Err(error) = roots.create() {
        eprintln!(
            "acpilot profile import: {}: {error}",
            roots.config.display()
        );
        return exit::FAILURE;
    }
    let mut written = 0;
    for (name, value) in files {
        // The allow-list is the export's own, so an import cannot write a database,
        // a lock or a file a newer binary invented into a profile directory.
        if !acpilot_profile::export::FILES.contains(&name.as_str()) {
            let _ = std::fs::remove_dir_all(&roots.config);
            eprintln!("acpilot profile import: `{name}` is not a configuration file");
            return exit::USAGE;
        }
        let Some(value) = value.as_str() else {
            let _ = std::fs::remove_dir_all(&roots.config);
            eprintln!("acpilot profile import: `{name}` is not text");
            return exit::USAGE;
        };
        if let Err(error) = std::fs::write(roots.config.join(name), value) {
            let _ = std::fs::remove_dir_all(&roots.config);
            eprintln!("acpilot profile import: `{name}`: {error}");
            return exit::FAILURE;
        }
        written += 1;
    }
    if document.get("secrets").is_some() {
        eprintln!(
            "acpilot profile import: `secrets` are not imported; set each one with `acpilot secret set`"
        );
    }
    if !roots.config_file().exists()
        && let Err(error) = std::fs::write(roots.config_file(), "schema_version = 1\n")
    {
        let _ = std::fs::remove_dir_all(&roots.config);
        eprintln!("acpilot profile import: {error}");
        return exit::FAILURE;
    }
    registry.insert(id.clone(), ProfileRecord::new(id.as_str()));
    if registry.active_profile.is_none() {
        registry.active_profile = Some(id.clone());
    }
    if let Err(error) = registry.save(&registry_path) {
        let _ = std::fs::remove_dir_all(&roots.config);
        eprintln!("acpilot profile import: {error}");
        return exit::FAILURE;
    }
    println!("imported {id} from {} · {written} files", file.display());
    exit::SUCCESS
}

/// One effective setting: the value, and the layer that set it.
type Effective = BTreeMap<String, (String, String)>;

/// Every preference of a cascade, with the layer that won it.
fn effective(cascade: &Cascade) -> Result<Effective, String> {
    let settings = cascade.settings().map_err(|error| error.to_string())?;
    let mut values = Effective::new();
    let document = serde_json::to_value(&settings.preferences)
        .map_err(|error| format!("cannot read the preferences: {error}"))?;
    let mut keys = Vec::new();
    flatten_keys("preferences", &document, &mut keys);
    for key in keys {
        if let Ok(explanation) = cascade.explain(&key) {
            values.insert(
                explanation.key.clone(),
                (explanation.render_value(), explanation.origin.clone()),
            );
        }
    }
    if let Some(profile) = &settings.profile {
        values.insert(
            "profile".to_owned(),
            (profile.clone(), "a settings file".to_owned()),
        );
    }
    Ok(values)
}

/// The dotted paths of every scalar in a settings document.
fn flatten_keys(prefix: &str, value: &serde_json::Value, keys: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(table) => {
            for (name, value) in table {
                flatten_keys(&format!("{prefix}.{name}"), value, keys);
            }
        }
        _ => keys.push(prefix.to_owned()),
    }
}

/// `acpilot profile diff`.
fn diff_profiles(a: &str, b: &str, config: &ConfigArgs) -> u8 {
    let root = profile_config_root(config);
    let profiles = root.join("profiles");
    let left = match acpilot_profile::Profile::named_in(&profiles, a) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("acpilot profile diff: {error}");
            return exit::NOT_FOUND;
        }
    };
    let right = match acpilot_profile::Profile::named_in(&profiles, b) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("acpilot profile diff: {error}");
            return exit::NOT_FOUND;
        }
    };
    let project = match Root::resolve(None) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("acpilot profile diff: {error}");
            return exit::FAILURE;
        }
    };
    let user = acpilot_profile::user_config_path();
    let project_config = project
        .project()
        .map(acpilot_profile::project::Project::config_path);
    // The same cascade a launch reads, so this answers the question a launch
    // would: an explanation built from a second copy of the rules would agree
    // with itself and disagree with the daemon.
    let left_values = match Cascade::for_launch(Some(&user), project_config.as_deref(), Some(&left))
        .and_then(|cascade| cascade.settings().map(|_| cascade))
        .map_err(|error| error.to_string())
        .and_then(|cascade| effective(&cascade))
    {
        Ok(values) => values,
        Err(message) => {
            eprintln!("acpilot profile diff: {message}");
            return exit::FAILURE;
        }
    };
    let right_values =
        match Cascade::for_launch(Some(&user), project_config.as_deref(), Some(&right))
            .and_then(|cascade| cascade.settings().map(|_| cascade))
            .map_err(|error| error.to_string())
            .and_then(|cascade| effective(&cascade))
        {
            Ok(values) => values,
            Err(message) => {
                eprintln!("acpilot profile diff: {message}");
                return exit::FAILURE;
            }
        };
    println!("{a} -> {b}");
    let mut keys: Vec<&String> = left_values.keys().chain(right_values.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut differences = 0;
    for key in keys {
        let left = left_values.get(key);
        let right = right_values.get(key);
        if left == right {
            continue;
        }
        differences += 1;
        println!("  {key}");
        let (value, origin) =
            left.map_or(("not set", "-"), |pair| (pair.0.as_str(), pair.1.as_str()));
        println!("    {a}: {value}\n       from {origin}");
        let (value, origin) =
            right.map_or(("not set", "-"), |pair| (pair.0.as_str(), pair.1.as_str()));
        println!("    {b}: {value}\n       from {origin}");
    }
    if differences == 0 {
        println!("  no effective difference");
    }
    exit::SUCCESS
}

/// Remove a profile's config and state, or move them to the OS trash.
fn run_profile(command: ProfileCommand) -> u8 {
    match command {
        ProfileCommand::New {
            name,
            label,
            extends,
            config,
        } => create_profile(&name, label, extends, &config),
        ProfileCommand::Ls { config } => list_profiles(&config),
        ProfileCommand::Show { name, config } => show_profile(&name, &config),
        ProfileCommand::Use { name, config } => use_profile(&name, &config),
        ProfileCommand::Import { file, config } => import_profile(&file, &config),
        ProfileCommand::Diff { a, b, config } => diff_profiles(&a, &b, &config),
        ProfileCommand::Export {
            name,
            output,
            with_secrets,
            i_understand_the_risk,
            config,
        } => {
            if with_secrets && !i_understand_the_risk {
                eprintln!(
                    "acpilot profile export: --with-secrets requires --i-understand-the-risk"
                );
                return exit::USAGE;
            }
            if with_secrets && output.is_none() {
                eprintln!(
                    "acpilot profile export: --with-secrets requires --output FILE so the values \
                     are not written to the terminal"
                );
                return exit::USAGE;
            }
            let id = match ProfileId::parse(&name) {
                Ok(id) => id,
                Err(error) => {
                    eprintln!("acpilot profile export: {error}");
                    return exit::USAGE;
                }
            };
            let root = config.config.unwrap_or_else(acpilot_profile::config_dir);
            let profile =
                match acpilot_profile::Profile::open(root.join("profiles").join(id.as_str())) {
                    Ok(profile) => profile,
                    Err(error) => {
                        eprintln!("acpilot profile export: {error}");
                        return exit::NOT_FOUND;
                    }
                };
            let resolver = KeyringResolver::new(id.as_str());
            let document = match export::document(&profile, with_secrets, &resolver) {
                Ok(document) => document,
                Err(message) => {
                    eprintln!("acpilot profile export: {message}");
                    return exit::FAILURE;
                }
            };
            match output {
                Some(path) => match export::write(&document, &path, with_secrets) {
                    Ok(()) => {
                        println!("exported {id} to {}", path.display());
                        exit::SUCCESS
                    }
                    Err(message) => {
                        eprintln!("acpilot profile export: {message}");
                        exit::FAILURE
                    }
                },
                None => {
                    print!("{document}");
                    exit::SUCCESS
                }
            }
        }
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
fn run_config(command: ConfigCommand, profile: Option<&str>) -> u8 {
    let ConfigCommand::Explain { key, path } = command;
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
        .flag(profile.map(Path::new))
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

/// `acpilot agents rm`.
fn run_agents(command: AgentsCommand, profile: Option<&str>) -> u8 {
    match command {
        AgentsCommand::Rm { id } => {
            let profile = match models_profile(profile) {
                Ok(profile) => profile,
                Err(message) => {
                    eprintln!("acpilot agents rm: {message}");
                    return exit::NOT_FOUND;
                }
            };
            let path = acpilot_agent::promote::path_in(profile.root());
            match acpilot_agent::promote::demote(&path, &id) {
                Ok(()) => {
                    println!("removed {id} from {}", path.display());
                    exit::SUCCESS
                }
                Err(error) => {
                    // Nothing to remove is a different answer from a refusal, and
                    // a script should be able to tell them apart.
                    let code = match &error {
                        acpilot_agent::promote::PromoteError::Missing { .. } => exit::NOT_FOUND,
                        _ => exit::FAILURE,
                    };
                    eprintln!("acpilot agents rm: {error}");
                    code
                }
            }
        }
    }
}

/// `acpilot models probe`.
fn run_models(command: ModelsCommand, profile: Option<&str>) -> u8 {
    let ModelsCommand::Probe { id } = command;
    let profile = match models_profile(profile) {
        Ok(profile) => profile,
        Err(message) => {
            eprintln!("acpilot models probe: {message}");
            return exit::NOT_FOUND;
        }
    };
    let path = profile.providers_path();
    let declarations = match ProvidersFile::load(&path) {
        Ok(declarations) => declarations,
        Err(error) => {
            eprintln!("acpilot models probe: {error}");
            return exit::FAILURE;
        }
    };
    let targets = match probe_targets(&declarations, id.as_deref()) {
        Ok(targets) => targets,
        Err(message) => {
            eprintln!("acpilot models probe: {message}");
            return exit::NOT_FOUND;
        }
    };
    if targets.is_empty() {
        println!("no providers declared in {}", path.display());
        println!("  add a `[[providers]]` table to that file, then ask it here");
        return exit::SUCCESS;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("acpilot models probe: {error}");
            return exit::FAILURE;
        }
    };
    // The keyring namespace is the profile's directory name, the same rule the
    // daemon applies: two spellings would let one surface read a token the other
    // cannot, and the difference would look like a server problem.
    let keyring = profile
        .root()
        .file_name()
        .and_then(|name| name.to_str())
        .map(acpilot_agent::KeyringResolver::new);
    let state_path = acpilot_profile::providers::ProvidersState::path_in(profile.root());
    let mut remembered = acpilot_profile::providers::ProvidersState::load(&state_path);
    let mut failed = false;
    for declaration in targets {
        let request = AddProviderRequest::from(declaration);
        if let Some(refusal) = acpilotd::providers::refuse(&request) {
            eprintln!("acpilot models probe: {}", refusal.message);
            eprintln!("  {}: {}", refusal.field, refusal.detail);
            failed = true;
            continue;
        }
        // The daemon's own probe, not a copy: a second mapper would be a second
        // opinion about what a refused credential means.
        let credential = acpilotd::providers::Credential::resolve(keyring.as_ref(), &request);
        let row = runtime.block_on(acpilotd::providers::probe(&request, credential));
        println!("{}", render_provider(&row));
        if let Some(detail) = &row.detail {
            println!("  {detail}");
        }
        // Written where the daemon reads it, so a restart shows this answer. A
        // verb that asked and then forgot would make every start look unprobed.
        remembered.record(declaration, &row);
        if matches!(
            row.status,
            ProviderStatus::Unreachable | ProviderStatus::Unauthorized
        ) {
            failed = true;
        }
    }
    if !remembered.probes.is_empty()
        && let Err(error) = remembered.save(&state_path)
    {
        eprintln!("acpilot models probe: the answers were not remembered: {error}");
    }
    if failed { exit::FAILURE } else { exit::SUCCESS }
}

/// The providers a probe command should ask.
fn probe_targets<'a>(
    declarations: &'a ProvidersFile,
    id: Option<&str>,
) -> Result<Vec<&'a DeclaredProvider>, String> {
    match id {
        Some(id) => declarations
            .providers
            .iter()
            .find(|provider| provider.id == id)
            .map(|provider| vec![provider])
            .ok_or_else(|| format!("no provider `{id}` is declared in this profile")),
        // A disabled provider is a decision the file records, so an unqualified
        // probe skips it rather than contacting a server somebody turned off.
        None => Ok(declarations
            .providers
            .iter()
            .filter(|provider| provider.enabled)
            .collect()),
    }
}

/// The profile whose `providers.toml` a models verb reads.
fn models_profile(flag: Option<&str>) -> Result<acpilot_profile::Profile, String> {
    match flag {
        // A path is a profile directory and a single segment is a name under
        // `<config>/profiles`: the rule the global flag documents, so `--profile
        // work` and `--profile ./work` cannot mean two directories.
        Some(name) if Path::new(name).components().count() > 1 => {
            acpilot_profile::Profile::open(name).map_err(|error| error.to_string())
        }
        Some(name) => acpilot_profile::Profile::named(name).map_err(|error| error.to_string()),
        None => {
            let registry = acpilot_profile::Registry::load(&profile_registry_path(
                &acpilot_profile::config_dir(),
            ))
            .map_err(|error| error.to_string())?;
            let id = registry.active_profile.ok_or_else(|| {
                "no active profile; pass --profile NAME or run `acpilot profile use NAME`"
                    .to_owned()
            })?;
            acpilot_profile::Profile::named(id.as_str()).map_err(|error| error.to_string())
        }
    }
}

/// One provider as a line a terminal can compare with the file it came from.
fn render_provider(row: &ProviderSummary) -> String {
    let count = if row.status == ProviderStatus::Reachable {
        format!(
            "{} model{}",
            row.model_count,
            if row.model_count == 1 { "" } else { "s" }
        )
    } else {
        "-".to_owned()
    };
    format!(
        "{:<16} {:<12} {:<10} {}",
        row.id,
        status_word(row.status),
        count,
        row.base_url
    )
}

/// The word a person reads for a status, matching the wire spelling.
fn status_word(status: ProviderStatus) -> &'static str {
    match status {
        ProviderStatus::Disabled => "disabled",
        ProviderStatus::NotProbed => "not_probed",
        ProviderStatus::Reachable => "reachable",
        ProviderStatus::Unreachable => "unreachable",
        ProviderStatus::Unauthorized => "unauthorized",
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

fn run_ext(command: ExtCommand, profile: Option<&str>) -> u8 {
    // `ext` verbs act on one profile; without the global flag that is `default`,
    // the same identity the launch resolution falls back to.
    let named = profile.unwrap_or(acpilot_profile::launch::DEFAULT_PROFILE);
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
        ExtCommand::Activate { id, config, store } => {
            let paths = match Paths::resolve(store.store, config.config) {
                Ok(paths) => paths,
                Err(error) => return fail(&error),
            };
            match ext::activate(&paths, named, &id) {
                Ok(line) => {
                    println!("{line}");
                    exit::SUCCESS
                }
                Err(error) => fail(&error),
            }
        }
        ExtCommand::Deactivate { id, config, store } => {
            let paths = match Paths::resolve(store.store, config.config) {
                Ok(paths) => paths,
                Err(error) => return fail(&error),
            };
            match ext::deactivate(&paths, named, &id) {
                Ok(line) => {
                    println!("{line}");
                    exit::SUCCESS
                }
                Err(error) => fail(&error),
            }
        }
        ExtCommand::Why { id, config, store } => {
            let paths = match Paths::resolve(store.store, config.config) {
                Ok(paths) => paths,
                Err(error) => return fail(&error),
            };
            match ext::why(&paths, named, &id) {
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

type ResolvedSecretProfile = Result<(String, PathBuf), (String, u8)>;

/// The profile a secret verb acts on, through the same order a launch uses.
///
/// The flag may be a name; an explicit directory is accepted only when its last
/// segment is a valid profile id, because that segment is the keyring namespace.
fn resolve_secret_profile(config: &SecretProfileArgs, flag: Option<&str>) -> ResolvedSecretProfile {
    let root = Root::resolve(None).map_err(|error| (error.to_string(), exit::FAILURE))?;
    let config_root = config
        .config
        .config
        .clone()
        .unwrap_or_else(acpilot_profile::config_dir);
    let user_config = config_root.join(acpilot_profile::CONFIG_FILE);
    let resolved =
        ProfileResolver::new_in(config_root.join("profiles"), Some(user_config.as_path()))
            .flag(flag.map(Path::new))
            .process_environment()
            .resolve(&root)
            .map_err(|error| {
                let code = if error.is_missing() {
                    exit::NOT_FOUND
                } else {
                    exit::FAILURE
                };
                (error.to_string(), code)
            })?;
    let id = resolved.profile().id().ok_or_else(|| {
        (
            format!(
                "{} is not a named profile; secrets are namespaced by profile name",
                resolved.profile().root().display()
            ),
            exit::USAGE,
        )
    })?;
    Ok((
        id.as_str().to_owned(),
        resolved.profile().root().to_path_buf(),
    ))
}

fn run_secret(command: SecretCommand, flag: Option<&str>) -> u8 {
    match command {
        SecretCommand::Set { name, profile } => {
            let reference = match secret::reference(&name) {
                Ok(reference) => reference,
                Err(message) => {
                    eprintln!("error: {message}");
                    return exit::USAGE;
                }
            };
            let (profile, _) = match resolve_secret_profile(&profile, flag) {
                Ok(resolved) => resolved,
                Err((message, code)) => {
                    eprintln!("acpilot secret set: {message}");
                    return code;
                }
            };
            let value = match secret::read_value(std::io::stdin().lock()) {
                Ok(value) => value,
                Err(message) => {
                    eprintln!("acpilot secret set: {message}");
                    return exit::FAILURE;
                }
            };
            let resolver = KeyringResolver::new(profile);
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
            let (profile, directory) = match resolve_secret_profile(&profile, flag) {
                Ok(resolved) => resolved,
                Err((message, code)) => {
                    eprintln!("acpilot secret list: {message}");
                    return code;
                }
            };
            let resolver = KeyringResolver::new(profile);
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
            let (profile, _) = match resolve_secret_profile(&profile, flag) {
                Ok(resolved) => resolved,
                Err((message, code)) => {
                    eprintln!("acpilot secret rm: {message}");
                    return code;
                }
            };
            let resolver = KeyringResolver::new(profile);
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
