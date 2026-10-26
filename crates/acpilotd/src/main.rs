//! `acpilotd` — the daemon process.
//!
//! It listens on a unix socket in the runtime directory by default, made private
//! to the user who started it (ADR 0004 §3), and on TCP only when `--addr` asks —
//! because the filesystem is the socket's access control and a port has no such
//! thing. Single-instance locking and the remaining service routes arrive in the
//! service phase (P3).
//!
//! Two flags are how a profile reaches the daemon today. `--profile` names the
//! directory holding a profile's `config.toml` and `agents.toml`: the `[policy]`
//! table (when it has one — a file without it means the shipped policy rather
//! than none) is the rule set every permission request is answered from, and the
//! agents file, when it exists, is what `GET /v1/agents` lists. `--workspace` is
//! the root the broker compares paths with, because "inside the workspace" is the
//! question the defaults are written in — the daemon serves one workspace until
//! workspaces are first-class in the service phase (P3).
//!
//! Profile *names* and the platform config directory are the profiles phase's P4-2: a name has to
//! resolve the same way for every Acpilot binary, and guessing here would be the
//! second resolver.

use std::path::PathBuf;

use acpilot_profile::project::Root;
use acpilot_proto::profile::ThemeSurface;
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
    /// and its `agents.toml`, if present, is the agent list `/v1/agents` serves.
    /// Without this flag the daemon runs the shipped policy and has no profile.
    #[arg(long, value_name = "DIR")]
    profile: Option<PathBuf>,

    /// Workspace root the policy treats as "inside".
    ///
    /// Resolved once at startup, because the broker's inside/outside answer is a
    /// prefix comparison and a root that is itself a symlink would answer it
    /// wrongly for everything under it.
    #[arg(long, value_name = "DIR", default_value = ".")]
    workspace: PathBuf,

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

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "acpilotd=info,tower_http=info".into()),
        )
        .init();

    let mut state = acpilotd::state_for(cli.profile.as_deref(), &cli.workspace)?
        // Conversations outlive the process that streamed them, which is the point
        // of a transcript: a client that reconnects after a restart can read what
        // happened. Where that lives is the data directory, not the socket's.
        .with_transcripts(acpilotd::transcript_dir());
    // The browser is the third surface reading the project's theme setting. It is
    // resolved here, once, from the same cascade the terminal and the window use:
    // a page with its own palette would be a fourth place a colour is written down.
    if let Some(palette) = web_palette(&cli.workspace) {
        state = state.with_web_palette(palette);
    }
    // A terminal is a process; one an agent opened and never released has to end
    // on its own, because `terminal/release` is optional in ACP.
    state.reap_terminals_on_a_timer();
    // And one an agent opened should be watchable: the same PTY, seen through a
    // pane a client can attach to.
    state.mirror_agent_terminals();

    let target = acpilotd::resolve_target(cli.addr, cli.socket)
        .map_err(|why| std::io::Error::new(std::io::ErrorKind::InvalidInput, why))?;
    tracing::info!(endpoint = %target, service = acpilotd::SERVICE, "starting");

    acpilotd::serve(acpilotd::router_with(state), target, acpilotd::shutdown()).await?;

    Ok(())
}

/// The palette the web page is drawn with, from the workspace's settings.
///
/// `None` leaves the page's own colours, and every failure is a warning rather than
/// a startup error: a theme that is not installed is a bad setting, and a daemon
/// that refused to serve the API because a browser page could not be coloured would
/// be a worse trade than a plain page.
fn web_palette(workspace: &std::path::Path) -> Option<acpilot_extensions::Palette> {
    let root = match Root::resolve(Some(workspace)) {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(%error, "the workspace does not resolve, so the page keeps its colours");
            return None;
        }
    };
    let settings = match root.settings() {
        Ok(settings) => settings,
        Err(error) => {
            tracing::warn!(%error, "the settings do not load, so the page keeps its colours");
            return None;
        }
    };
    let id = settings.preferences.theme_for(ThemeSurface::Web);
    let extensions = match acpilot_extensions::builtin::load_all() {
        Ok(extensions) => extensions,
        Err(errors) => {
            for error in errors {
                tracing::warn!(%error, "a built-in extension does not load");
            }
            return None;
        }
    };
    match acpilot_extensions::ThemeRegistry::new(&extensions).complete(id) {
        Ok(theme) => Some(theme.palette),
        Err(error) => {
            tracing::warn!(%error, theme = id, "the web theme does not resolve");
            None
        }
    }
}
