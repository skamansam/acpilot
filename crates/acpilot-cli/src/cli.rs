//! The argument surface, kept out of `main` so it can be tested.
//!
//! Every command here is either a filesystem operation or (later) a call into
//! `acpilot-proto`; none of them reimplements daemon policy.

use std::path::PathBuf;

use acpilot_agent::keyring::KeyringResolver;
use acpilot_core::error::exit;
use acpilot_extensions::ExtensionError;
use acpilot_proto::API_PREFIX;
use clap::{Args, Parser, Subcommand};

use crate::ext::{self, Paths};
use crate::secret;

/// `acpilot` command line.
#[derive(Debug, Parser)]
#[command(
    name = "acpilot",
    version,
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
    /// Open the terminal UI against a running daemon.
    ///
    /// The TUI is a client, not a second engine: it opens sessions, sends
    /// prompts and answers permissions over the same API the CLI and the GUI
    /// use, so nothing it does can drift from what the daemon records.
    Tui {
        /// Endpoint of the daemon: a URL, or a unix socket path.
        ///
        /// Defaults to the socket in the runtime directory (or loopback HTTP on
        /// Windows), which is where `acpilotd` listens when given no flags.
        #[arg(long, env = "ACPILOT_API_URL", value_name = "URL|PATH")]
        api_url: Option<String>,
        /// Fail if no daemon is running instead of starting one.
        ///
        /// For tests, CI and anyone debugging a startup problem: the error is
        /// more useful than a daemon this client started out of sight.
        #[arg(long)]
        no_spawn: bool,
        /// Profile directory the spawned daemon serves.
        ///
        /// Only used when this command has to start the daemon; a daemon that is
        /// already running keeps the profile it was started with, and naming a
        /// different one here is an error rather than a silent mismatch.
        #[arg(long, value_name = "DIR")]
        profile: Option<PathBuf>,
        /// Workspace root the spawned daemon serves. Defaults to this directory.
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
            println!("acpilot {}", env!("CARGO_PKG_VERSION"));
            println!("api      {API_PREFIX}");
            exit::SUCCESS
        }
        Some(Command::Ext { command }) => run_ext(command),
        Some(Command::Secret { command }) => run_secret(command),
        Some(Command::Tui {
            api_url,
            no_spawn,
            profile,
            workspace,
            token,
        }) => run_tui(api_url, no_spawn, profile, workspace, token),

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

/// Open the TUI, starting the daemon if this is the first client (ADR 0004).
///
/// The flags are the daemon's, not the TUI's: they only matter on the spawn
/// path. A daemon that is already running was started with its own, and
/// pretending otherwise would show one project's sessions under another's name —
/// so a mismatch is refused rather than ignored.
fn run_tui(
    api_url: Option<String>,
    no_spawn: bool,
    profile: Option<PathBuf>,
    workspace: Option<PathBuf>,
    token: Option<String>,
) -> u8 {
    let workspace = match workspace {
        Some(directory) => directory,
        None => match std::env::current_dir() {
            Ok(directory) => directory,
            Err(error) => {
                eprintln!("acpilot tui: cannot read the working directory: {error}");
                return exit::FAILURE;
            }
        },
    };
    let mut daemon_args = vec!["--workspace".to_owned(), workspace.display().to_string()];
    if let Some(profile) = &profile {
        daemon_args.push("--profile".to_owned());
        daemon_args.push(profile.display().to_string());
    }

    let token = token.or_else(local_token);
    let options = acpilot_client::daemon::Options {
        endpoint: api_url,
        no_spawn,
        token: token.clone(),
        daemon_args,
    };
    let running = match acpilot_client::daemon::ensure_blocking(&options) {
        Ok(running) => running,
        Err(error) => {
            eprintln!("acpilot tui: {error}");
            return exit::FAILURE;
        }
    };

    if !running.spawned() {
        if profile.is_some() {
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

    match acpilot_tui::run(&running.endpoint, token.as_deref()) {
        Ok(()) => exit::SUCCESS,
        Err(error) => {
            eprintln!("acpilot tui: {error}");
            exit::FAILURE
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
    std::fs::read_to_string(acpilot_host::token_path())
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
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
