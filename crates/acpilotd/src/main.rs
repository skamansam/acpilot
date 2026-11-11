//! `acpilotd` — the daemon process.
//!
//! It listens on a unix socket in the runtime directory by default, made private
//! to the user who started it (ADR 0004 §3), and on TCP only when `--addr` asks
//! when `[daemon] addr` asks — because the filesystem is the socket's access
//! control and a port has no such thing.
//!
//! Settings come from two files and then the flags (see [`acpilotd::config`]): the
//! user's `<config>/config.toml` and the profile's, under a `[daemon]` table, so a
//! machine that always listens on one port is a line in a file rather than a flag
//! in a service unit. A flag that was typed wins.
//!
//! One daemon per endpoint. The lock is the bind itself — a socket path or a port
//! cannot be held twice — and [`acpilotd::pidfile`] is what turns the kernel's
//! refusal into a sentence naming the process that already has it.

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "acpilotd",
    // The tag a release was published as, not the manifest's number — the same
    // constant `acpilot` reports, so a client and its daemon cannot disagree.
    version = acpilot_core::VERSION,
    about = "Acpilot daemon"
)]
struct Cli {
    /// TCP address to bind instead of the unix socket.
    ///
    /// Opt-in, and not the default: a socket's directory is `0700`, so another
    /// local user cannot reach it, and a port cannot inherit that. Every request
    /// on this listener therefore needs `Authorization: Bearer <token>`, with the
    /// token in a `0600` file beside the socket — which is also why asking for a
    /// port is deliberate.
    #[arg(long, env = "ACPILOT_ADDR", value_name = "ADDR")]
    addr: Option<String>,

    /// Unix socket to bind instead of the default one in the runtime directory.
    ///
    /// Two daemons cannot share a path: a socket that still answers is refused
    /// rather than replaced, and one left by a dead daemon is removed.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Profile directory to load (the one holding `config.toml`).
    ///
    /// Its `[policy]` table becomes the rule set a running broker answers from,
    /// its `agents.toml`, if present, is the agent list `/v1/agents` serves, and
    /// its data directory is where this profile's conversations are kept.
    #[arg(long, value_name = "DIR")]
    profile: Option<PathBuf>,

    /// Workspace root the policy treats as "inside".
    ///
    /// Resolved once at startup, because the broker's inside/outside answer is a
    /// prefix comparison and a root that is itself a symlink would answer it
    /// wrongly for everything under it.
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,

    /// SQLite file to keep sessions in.
    ///
    /// Defaults to `profile.sqlite` in the profile's data directory, or the shared
    /// data directory when the daemon has no profile. One database per profile is
    /// what keeps a client's conversation out of a weekend project's.
    #[arg(long, value_name = "PATH")]
    database: Option<PathBuf>,

    /// Where to write this process's pid.
    #[arg(long, value_name = "PATH")]
    pidfile: Option<PathBuf>,

    /// Run attached: log to stderr only, and write no JSON log file.
    ///
    /// The daemon never detaches. This flag is what a service unit or a container
    /// sets when the journal, not a file under the data directory, is where the
    /// log should be read.
    #[arg(long)]
    foreground: bool,

    /// Print the OpenAPI document and exit, without binding a socket.
    #[arg(long)]
    print_openapi: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if cli.print_openapi {
        println!("{}", acpilot_proto::openapi_json());
        return Ok(());
    }

    let config = acpilotd::config::resolve(acpilotd::config::Flags {
        addr: cli.addr,
        socket: cli.socket,
        profile: cli.profile,
        workspace: cli.workspace,
        database: cli.database,
        pidfile: cli.pidfile,
        foreground: cli.foreground,
    })?;

    logging(&config);

    // Before the bind: a second daemon that fails on the port would otherwise have
    // to explain `EADDRINUSE` on its own, and the pidfile knows who has it.
    let pidfile = acpilotd::pidfile::acquire(&config.pidfile, &config.target)?;

    // The state and the background work are one call so that the embedded daemon
    // a client runs in its own process (P6-11) gets exactly what this binary gives.
    // Every profile under the config root, plus the one the flags named: the
    // daemon answers `GET /v1/profiles`, and a request names its profile in
    // `X-Acpilot-Profile`. A profile that does not load costs its own row, not the
    // others — except the started one, whose refusal is the launch's.
    let profiles = std::sync::Arc::new(acpilotd::profiles::Profiles::discover(
        config.profile.as_deref(),
        &config.workspace,
        config.cors,
    )?);
    tracing::info!(
        endpoint = %config.target,
        database = %config.database.display(),
        pid = std::process::id(),
        "starting"
    );

    acpilotd::serve(
        acpilotd::profiles::router(profiles.clone()),
        config.target.clone(),
        acpilotd::shutdown(),
    )
    .await?;

    // The language servers are processes, and the router's drop is not their end:
    // a task holding an `AppState` clone keeps the registry alive past `serve`. So
    // the daemon stops them here, on the way out, with the same `shutdown`/`exit`
    // pair and the same kill a probe in `acpilot-lsp` uses — one per profile.
    profiles.shutdown_language_servers().await;

    // Removed before the return, so a clean exit leaves no record saying a daemon
    // is running; the drop is idempotent, but the order here is the readable one.
    drop(pidfile);
    Ok(())
}

/// Set up logging: pretty to stderr, JSON to a file unless `--foreground`.
///
/// Both, not either: a person watching a terminal wants lines they can read, and a
/// bug report wants the timestamps and fields a JSON line keeps. `--foreground`
/// turns the second off because the journal already has it.
fn logging(config: &acpilotd::config::Config) {
    use tracing_subscriber::prelude::*;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "acpilotd=info,tower_http=info".into());
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    if config.foreground {
        tracing_subscriber::registry()
            .with(stderr)
            .with(filter)
            .init();
        return;
    }

    let path = acpilot_profile::data_dir()
        .join("logs")
        .join("acpilotd.jsonl");
    match FileWriter::open(&path) {
        Ok(writer) => {
            tracing_subscriber::registry()
                .with(stderr)
                .with(tracing_subscriber::fmt::layer().json().with_writer(writer))
                .with(filter)
                .init();
            tracing::info!(log = %path.display(), "logging to a file as well as stderr");
        }
        Err(error) => {
            // A home directory that cannot take a log file is not a reason to refuse
            // to run: the daemon's job is the sessions, and stderr still has them.
            eprintln!("acpilotd: cannot write {}: {error}", path.display());
            tracing_subscriber::registry()
                .with(stderr)
                .with(filter)
                .init();
        }
    }
}

/// The JSON log file, appended and shared.
///
/// `Arc<Mutex<File>>` rather than an appender crate: one file, one process, and a
/// write per event is already what a synchronous subscriber does.
#[derive(Clone)]
struct FileWriter(std::sync::Arc<std::sync::Mutex<std::fs::File>>);

impl FileWriter {
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self(std::sync::Arc::new(std::sync::Mutex::new(file))))
    }
}

impl std::io::Write for FileWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let mut file = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::io::Write::write(&mut *file, buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut file = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::io::Write::flush(&mut *file)
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
