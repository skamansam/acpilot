//! The daemon: one process that owns sessions, agents, permissions and terminals.
//!
//! Every UI is a client of this API, including the local TUI and CLI, so there is
//! exactly one place where state lives and exactly one auth story. The foundation phase (P1) establishes
//! the router, the error contract, and the OpenAPI surface; the service phase (P3) fills in the rest.
//!
//! Note what is *not* here: no file contents on the keystroke path, no agent
//! logic, no UI. The daemon is a control plane.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use acpilot_agent::Terminals as AgentTerminals;
use acpilot_agent::supervisor::availability;
use acpilot_agent::{AgentSpec, Broker, IDLE_TIMEOUT, KeyringResolver, PermissionOption, Registry};
use acpilot_core::AcpilotError;
use acpilot_host::LocalHost;
use acpilot_proto::agent::AgentSummary;
use acpilot_proto::permission::{PermissionDecision, PermissionRequest};
use acpilot_proto::profile::Preferences;
use acpilot_proto::{ApiDoc, ErrorBody, ErrorDetail};
use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use utoipa::OpenApi as _;

mod auth;
mod files;
mod platform;
mod sessions;
mod terminals;

pub use auth::protect;
pub use files::MAX_FILE_BYTES;
pub use platform::{Target, resolve_target, serve, shutdown};
pub use sessions::{REPLAY_CAPACITY, Sessions};
pub use terminals::{Output, Terminal, Terminals};

use axum::response::Html;

/// Service name reported by the health endpoint.
pub const SERVICE: &str = "acpilotd";

/// How often a quiet stream says it is still there (`docs/api.md`: every 15 s,
/// so a client that sees nothing for three intervals reconnects).
const HEARTBEAT: Duration = Duration::from_secs(15);

/// Frames buffered for a client slower than the broker. Past this the stream
/// says `resync` rather than growing without bound, which is the same trade the
/// broker's own announcement channel makes.
const EVENT_BUFFER: usize = 32;

/// How often the daemon sweeps terminals no agent has touched.
///
/// A minute against [`IDLE_TIMEOUT`]'s half hour: a process an agent opened and
/// forgot is gone soon after it stops being useful, and the sweep is a lock and a
/// timestamp comparison per terminal. `terminal/release` is optional in ACP, so
/// without this a command that outlived one tool call would outlive the daemon.
pub const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// Health payload. Deliberately minimal: a health check should not be able to fail
/// because a subsystem is slow.
#[derive(Debug, Serialize)]
pub struct Health {
    /// Always true when the daemon is able to answer.
    pub ok: bool,
    /// Service name, so a client can tell it reached the right port.
    pub service: &'static str,
    /// Daemon version.
    pub version: &'static str,
    /// Control-plane API version prefix.
    pub api: &'static str,
    /// The workspace this daemon serves, when it has one.
    ///
    /// A client that names a directory needs to know whether the daemon it found
    /// is already serving that directory, and a client that asks the daemon to
    /// start should not have to trust its own flags (ADR 0004 §5). The field is
    /// the workspace id, not a path: the API does not hand out host paths for
    /// something a client should have named.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// An [`AcpilotError`] rendered as an HTTP response.
///
/// This wrapper exists because the error type lives in `acpilot-core`, which must
/// not depend on `axum`. Mapping happens here, once, so every route returns the
/// same envelope. `details` is the field-level half of that envelope: a `400`
/// about one option is more useful when it says which values were allowed.
#[derive(Debug)]
pub struct ApiError {
    /// The failure itself.
    pub error: AcpilotError,
    /// Field-level entries for the envelope; empty for most failures.
    pub details: Vec<ErrorDetail>,
}

impl From<AcpilotError> for ApiError {
    fn from(error: AcpilotError) -> Self {
        Self::new(error)
    }
}

impl ApiError {
    /// A failure with no field-level detail.
    #[must_use]
    pub fn new(error: AcpilotError) -> Self {
        Self {
            error,
            details: Vec::new(),
        }
    }

    /// Attach one field-level entry, naming the input and what was expected.
    #[must_use]
    pub fn with_detail(mut self, field: impl Into<String>, message: impl Into<String>) -> Self {
        self.details.push(ErrorDetail {
            field: field.into(),
            message: message.into(),
        });
        self
    }

    /// HTTP status for this failure class.
    ///
    /// `InvalidRequest` is `400`: the daemon understood the request and cannot
    /// use it. `HostUnavailable`, `ModelUnavailable` and `AgentFailed` are `502`:
    /// the daemon is fine, the thing behind it is not. That distinction is what
    /// lets a client offer "retry" instead of "give up".
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self.error {
            AcpilotError::InvalidRequest { .. } => StatusCode::BAD_REQUEST,
            AcpilotError::NotFound { .. } => StatusCode::NOT_FOUND,
            AcpilotError::Conflict { .. } => StatusCode::CONFLICT,
            AcpilotError::PermissionDenied { .. } => StatusCode::FORBIDDEN,
            AcpilotError::HostUnavailable { .. }
            | AcpilotError::ModelUnavailable { .. }
            | AcpilotError::AgentFailed { .. } => StatusCode::BAD_GATEWAY,
            AcpilotError::Protocol { .. } | AcpilotError::Io(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = ErrorBody {
            details: self.details,
            ..ErrorBody::new(self.error.code(), self.error.to_string())
        };
        (status, Json(body)).into_response()
    }
}

async fn health(State(state): State<AppState>) -> Json<Health> {
    Json(Health {
        ok: true,
        service: SERVICE,
        version: env!("CARGO_PKG_VERSION"),
        api: acpilot_proto::API_PREFIX,
        workspace: state.workspace().map(|workspace| workspace.id.clone()),
    })
}

/// State the routes share.
///
/// The registry is optional for now: the daemon has no profile resolution yet
/// (the profiles phase, P4, owns it), so `acpilot serve` starts with none and the routes that need an
/// agent list answer from whatever the daemon was handed. Tests construct a real
/// The one workspace this daemon serves.
///
/// Workspaces become first-class and plural in the service phase (P3); until
/// then the daemon is started against a root and every session opens there, so
/// a client is told the id rather than allowed to name a directory the agent
/// will not actually edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Identifier the API uses, derived from the directory's basename.
    pub id: String,
    /// Resolved root the broker compares with and the agent is given.
    pub root: std::path::PathBuf,
}

/// [`Registry`], which is the same path the daemon will use once profiles land.
#[derive(Debug, Clone)]
pub struct AppState {
    agents: Option<Arc<Registry>>,
    broker: Option<Arc<Broker>>,
    /// The workspace sessions open in, when the daemon was given one.
    workspace: Option<Workspace>,
    /// Every session this daemon owns.
    sessions: Sessions,
    /// Every terminal this daemon owns.
    terminals: Terminals,
    /// Where each session's `session.jsonl` is kept, when the daemon keeps one.
    ///
    /// `None` in tests and in an embedded daemon that keeps its own record: a
    /// transcript directory is a decision about a filesystem, and the default for
    /// a library value is not to write into somebody's home directory.
    transcripts: Option<std::path::PathBuf>,
    /// The terminals *agents* ask their client to open.
    ///
    /// A different store from [`Self::terminals`], which is the panes a client
    /// attaches to: these are served by the same `acpilot-agent` code a headless
    /// session uses, and one registry here is enough because terminal ownership
    /// is keyed by session id. It runs on this daemon's host — the same [`Host`]
    /// the file methods use, so a remote workspace (P3-9) moves both at once.
    ///
    /// [`Host`]: acpilot_host::Host
    agent_terminals: Arc<AgentTerminals>,
    /// Settings saves, announced to every client on the profile-wide stream.
    ///
    /// A channel rather than a value: the stream is the reader, there may be none,
    /// and a daemon whose events nobody is watching should not accumulate anything
    /// it has to remember to drop. `AppState` always holds a sender, so the channel
    /// is never closed while the daemon runs.
    preference_changes: broadcast::Sender<Preferences>,
    /// Decisions already delivered, by request id.
    ///
    /// The broker answers each question once and forgets it; the API promises
    /// that a retried `resolve` returns the same body and that a *different*
    /// option after a decision is a conflict. Remembering the decision is the
    /// only way to tell those two apart, and it is the daemon's to remember:
    /// the broker's job is to answer, not to be a history.
    decided: Arc<Mutex<HashMap<String, PermissionDecision>>>,
}

impl Default for AppState {
    /// No profile loaded: the agent list is empty and nothing is pending.
    ///
    /// Written out rather than derived because the agent-terminal registry is a
    /// [`LocalHost`] and the host is not a default: this is where the daemon says
    /// which machine an agent's commands run on.
    fn default() -> Self {
        Self {
            agents: None,
            broker: None,
            workspace: None,
            sessions: Sessions::default(),
            terminals: Terminals::default(),
            transcripts: None,
            agent_terminals: Arc::new(AgentTerminals::new(Arc::new(LocalHost))),
            preference_changes: broadcast::channel(EVENT_BUFFER).0,
            decided: Arc::default(),
        }
    }
}

impl AppState {
    /// No profile loaded: the agent list is empty and nothing is pending.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve this profile's agents.
    #[must_use]
    pub fn with_registry(mut self, registry: Arc<Registry>) -> Self {
        self.agents = Some(registry);
        self
    }

    /// Decide permission requests with this broker.
    ///
    /// One broker per daemon: the sessions it decides for, the pending list the
    /// API serves and the audit log a person reads are then one of each, rather
    /// than one per route that happened to build its own.
    #[must_use]
    pub fn with_broker(mut self, broker: Arc<Broker>) -> Self {
        self.broker = Some(broker);
        self
    }

    /// The registry behind the agent routes, when the daemon has one.
    pub fn registry(&self) -> Option<&Arc<Registry>> {
        self.agents.as_ref()
    }

    /// The broker behind the permission routes, when the daemon has one.
    pub fn broker(&self) -> Option<&Arc<Broker>> {
        self.broker.as_ref()
    }

    /// Serve this workspace, and open every session in it.
    #[must_use]
    pub fn with_workspace(mut self, workspace: Workspace) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// The workspace sessions open in.
    pub fn workspace(&self) -> Option<&Workspace> {
        self.workspace.as_ref()
    }

    /// The session store the session routes drive.
    pub fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// The terminal store the terminal routes drive.
    pub fn terminals(&self) -> &Terminals {
        &self.terminals
    }

    /// Keep each session's transcript under `directory`.
    ///
    /// A session gets its own subdirectory, so two conversations never share a
    /// file and a stored transcript can grow siblings (P3-7's database). The
    /// directory is created per session when the session starts.
    #[must_use]
    pub fn with_transcripts(mut self, directory: impl Into<std::path::PathBuf>) -> Self {
        self.transcripts = Some(directory.into());
        self
    }

    /// The settings this daemon would apply right now, resolved from the cascade.
    ///
    /// Read rather than cached, for the same reason the palette is: a settings file
    /// edited while the daemon runs should be the answer the next caller gets. A
    /// daemon with no workspace, or with a file that does not parse, answers
    /// [`None`] — which is "carry on with what you have", not an error.
    #[must_use]
    pub fn settings(&self) -> Option<acpilot_profile::settings::Settings> {
        let root = acpilot_profile::project::Root::resolve(Some(&self.workspace()?.root)).ok()?;
        root.settings().ok()
    }

    /// The preferences a client of this daemon would draw with, resolved now.
    #[must_use]
    pub fn preferences(&self) -> Option<Preferences> {
        Some(self.settings()?.preferences)
    }

    /// Subscribe to settings saves, for whoever is serving the profile-wide stream.
    #[must_use]
    pub fn subscribe_preferences(&self) -> broadcast::Receiver<Preferences> {
        self.preference_changes.subscribe()
    }

    /// Announce the settings a person saves, for as long as the daemon runs.
    ///
    /// The daemon watches rather than each client because a client may not be able
    /// to see this machine's files: a window, a browser, or anything reaching the
    /// daemon over TCP. `GET /v1/events` carries the answer, and a client applies a
    /// frame the same way it applies the settings it reads at launch.
    ///
    /// A daemon with no workspace, or whose settings no watcher can reach, still
    /// serves the first frame of a stream from [`Self::preferences`]: the fallback
    /// is a client that never hears about a save, not a launcher that fails.
    pub fn watch_preferences(&self) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        let Ok(root) = acpilot_profile::project::Root::resolve(Some(&workspace.root)) else {
            return;
        };
        // Both files the cascade reads, so a save in either is a change: a project
        // can be light while the user's own settings stay night flight.
        let mut paths = vec![acpilot_profile::user_config_path()];
        if let Some(project) = root.project() {
            paths.push(project.config_path());
        }
        let Some(changes) = acpilot_profile::watch::changes(paths) else {
            tracing::warn!(
                "settings are not being watched; clients will only see the preferences \
                             they read at connect"
            );
            return;
        };
        let state = self.clone();
        std::thread::spawn(move || {
            while changes.recv().is_ok() {
                // A file that does not parse, or names something no extension
                // provides, answers `None`: the last good preferences stay in
                // force and the stream says nothing rather than something wrong.
                let Some(preferences) = state.preferences() else {
                    continue;
                };
                let _ = state.preference_changes.send(preferences);
            }
        });
    }

    /// The palette the web page is drawn with, resolved now.
    ///
    /// Read per request rather than at startup: the browser page is the one
    /// surface whose theme can be re-read for free, and a settings file edited
    /// while the daemon runs should be the colours on the next page load instead of
    /// a reason to restart. `None` leaves the page's own palette, which is what a
    /// daemon with no workspace or no settings has always served.
    #[must_use]
    pub fn web_palette(&self) -> Option<acpilot_extensions::Palette> {
        use acpilot_proto::profile::ThemeSurface;

        let settings = self.settings()?;
        let extensions = acpilot_extensions::builtin::load_all().ok()?;
        let resolved = acpilot_extensions::ThemeRegistry::new(&extensions)
            .complete(settings.preferences.theme_for(ThemeSurface::Web))
            .ok()?;
        Some(resolved.palette)
    }

    /// Where transcripts are kept, when the daemon keeps them.
    pub fn transcripts(&self) -> Option<&std::path::Path> {
        self.transcripts.as_deref()
    }

    /// The terminals an agent's `terminal/*` requests are served from.
    ///
    /// One for the whole daemon, because the registry is keyed by session id and
    /// a session can only reach its own. Cloned into a session's options when the
    /// session starts, so the capability it advertises and the handler that serves
    /// it are the same switch.
    pub fn agent_terminals(&self) -> &Arc<AgentTerminals> {
        &self.agent_terminals
    }

    /// Give every terminal an agent opens a pane of its own, from now on.
    ///
    /// The agent's terminal is the process; the pane is the screen a person can
    /// attach to and type into. They are the same PTY — the pane reads the bytes
    /// the agent's reader already got and writes to the same descriptor — so a
    /// shell the agent started can be watched and driven from a client without the
    /// daemon owning a second process for it.
    ///
    /// A session the daemon cannot map (its `session/new` failed after the agent
    /// chose an id, say) is filed under the agent's own id rather than dropped: a
    /// pane with an unfamiliar owner is more useful than a shell nobody can see.
    ///
    /// # Panics
    ///
    /// If there is no tokio runtime, like every other `tokio::spawn`.
    pub fn mirror_agent_terminals(&self) {
        let panes = self.terminals.clone();
        let sessions = self.sessions.clone();
        let mut opened = self.agent_terminals.subscribe_opened();
        tokio::spawn(async move {
            loop {
                match opened.recv().await {
                    Ok(handle) => {
                        let session_id = sessions
                            .id_for_acp(handle.session_id())
                            .unwrap_or_else(|| handle.session_id().to_owned());
                        panes.adopt(session_id, handle);
                    }
                    // A mirror that falls 256 terminals behind loses those
                    // panes, not the processes: every terminal it skipped still
                    // works for the agent, which is the owner, and the next one
                    // opened is adopted as normal. Said out loud rather than
                    // silently, because a shell with no pane is something the
                    // log should be able to explain.
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "pane mirror fell behind");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// Kill agent terminals nobody has touched in [`IDLE_TIMEOUT`], on a timer.
    ///
    /// A terminal is a process, and a process with no reader keeps running. The
    /// session releases its own terminals when it ends, so this is the backstop
    /// for a session that stays open while an agent forgets one — which is the
    /// normal case, because `terminal/release` is optional in ACP.
    ///
    /// Spawned by the daemon binary rather than by [`AppState::new`] so the state
    /// stays constructible outside a runtime; calling it twice would give every
    /// terminal two sweepers, which is harmless but pointless.
    ///
    /// # Panics
    ///
    /// If there is no tokio runtime, like every other `tokio::spawn`.
    pub fn reap_terminals_on_a_timer(&self) {
        let terminals = Arc::clone(&self.agent_terminals);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REAP_INTERVAL);
            // The first tick of an interval fires immediately; nothing can be
            // idle before a session has run, so it is skipped.
            tick.tick().await;
            loop {
                tick.tick().await;
                let reaped = terminals.reap(IDLE_TIMEOUT);
                if reaped > 0 {
                    tracing::info!(reaped, "released terminals no agent had touched");
                }
            }
        });
    }

    fn decision(&self, id: &str) -> Option<PermissionDecision> {
        self.decided
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    fn remember(&self, id: &str, decision: PermissionDecision) {
        self.decided
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_owned(), decision);
    }
}

/// Why a profile could not become daemon state.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The profile directory, or the `[policy]` in it, is not usable.
    #[error(transparent)]
    Profile(#[from] acpilot_profile::ProfileError),

    /// The profile's `agents.toml` exists and does not load.
    #[error("the profile's agents.toml is not usable: {0}")]
    Agents(#[from] acpilot_agent::AgentError),

    /// The workspace root could not be resolved, so "inside" has no meaning.
    #[error("cannot resolve workspace {}: {source}", path.display())]
    Workspace {
        /// The path that was given.
        path: std::path::PathBuf,
        /// Why it did not resolve.
        source: std::io::Error,
    },
}

/// Daemon state with a broker running `profile`'s `[policy]`.
///
/// The broker waits for as long as a request is pending: the API's contract is
/// that a request stays pending until it is resolved, because the client that
/// answers it is the one that just listed it. [`acpilot_agent::ASK_TIMEOUT`]
/// exists for a session with no client at all, which a daemon with a permissions
/// route is not.
///
/// The workspace is resolved once, here: the broker's inside/outside answer is a
/// prefix comparison, and a root that is itself a symlink would answer it wrongly
/// for everything under it.
pub fn state_for_profile(
    directory: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<AppState, StateError> {
    let profile = acpilot_profile::Profile::open(directory)?;
    let policy = profile.policy()?;
    let workspace = workspace_at(workspace)?;
    tracing::info!(
        profile = %profile.root().display(),
        workspace = %workspace.root.display(),
        rules = policy.rules().len(),
        "policy loaded"
    );
    let broker = Arc::new(Broker::with_timeout(policy, workspace.root.clone(), None));
    let state = AppState::new()
        .with_broker(broker)
        .with_workspace(workspace);
    Ok(match load_agents(&profile)? {
        Some(registry) => state.with_registry(registry),
        None => state,
    })
}

/// The state the daemon binary builds from its two flags.
///
/// One function rather than two lines in `main` because the flags are a promise:
/// a client that spawns the daemon with `--workspace <dir>` expects
/// `GET /v1/health` to name that directory whether or not a profile came with it
/// (ADR 0004 §2), and that is testable here and not in a process.
pub fn state_for(
    profile: Option<&std::path::Path>,
    workspace: &std::path::Path,
) -> Result<AppState, StateError> {
    match profile {
        Some(directory) => state_for_profile(directory, workspace),
        None => Ok(AppState::new().with_workspace(workspace_at(workspace)?)),
    }
}

/// Where the daemon keeps the transcripts of its sessions.
///
/// `<data>/acpilot/transcripts`, moved with the rest of Acpilot's state by
/// `ACPILOT_HOME`. A caller can point [`AppState::with_transcripts`] somewhere
/// else; this is the answer for a daemon started by its own binary.
#[must_use]
pub fn transcript_dir() -> std::path::PathBuf {
    acpilot_profile::data_dir().join("transcripts")
}

/// The root a daemon was started against, resolved and named once.
///
/// A daemon without a profile still serves a root: the client that spawned it
/// named one, and `GET /v1/health` reports it so that client can tell whether the
/// daemon it found is the one for this directory (ADR 0004 §2). Resolving here is
/// what keeps "inside the workspace" a prefix comparison rather than a guess.
pub fn workspace_at(root: &std::path::Path) -> Result<Workspace, StateError> {
    let root = std::fs::canonicalize(root).map_err(|source| StateError::Workspace {
        path: root.to_path_buf(),
        source,
    })?;
    let id = root
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "workspace".to_owned(), str::to_owned);
    Ok(Workspace { id, root })
}

/// The profile's agents, when it has an `agents.toml`.
///
/// No file means no agent list, not an empty one: the file is the source, and
/// writing one on the daemon's behalf would be configuration nobody asked for.
/// A file that exists and does not load fails the startup, exactly as a broken
/// `config.toml` does — including a secret reference this host cannot resolve,
/// because a daemon serving half a profile is the failure profiles exist to
/// prevent.
fn load_agents(profile: &acpilot_profile::Profile) -> Result<Option<Arc<Registry>>, StateError> {
    let path = profile.agents_path();
    if !path.is_file() {
        return Ok(None);
    }
    // Secrets are namespaced by profile name, and the directory is the name: the profiles phase's task P4-2
    // resolves `<name>` to exactly this path. A path with no last segment names
    // no profile, and then nothing has been stored under that name either.
    let resolver = KeyringResolver::new(profile.name().unwrap_or_default());
    let registry = Registry::load(path, Arc::new(resolver))?;
    tracing::info!(
        path = %registry.path().display(),
        agents = registry.agents().len(),
        "agents loaded"
    );
    Ok(Some(Arc::new(registry)))
}

/// One agent as the API reports it.
///
/// Availability is a `PATH` lookup on the host the daemon runs on (see
/// [`availability`]). Nothing is spawned, so listing agents cannot start one; a
/// handshake-dependent status is reported by the session routes, not here.
fn summarize(spec: &AgentSpec) -> AgentSummary {
    AgentSummary {
        id: spec.id.clone(),
        name: spec.name.clone(),
        transport: spec.transport_kind(),
        status: availability(spec),
        // An HTTP agent has no command; the empty string is the honest answer
        // rather than a URL in a field that means "what will be spawned".
        command: spec
            .command()
            .map_or_else(String::new, acpilot_agent::CommandSpec::command_line),
        protocol_version: None,
    }
}

/// `GET /v1/agents`.
///
/// An empty list means the daemon has no registry loaded; there is no profile to
/// answer 404 for yet.
async fn agents(State(state): State<AppState>) -> Json<Vec<AgentSummary>> {
    let summaries = state.agents.as_ref().map_or_else(Vec::new, |registry| {
        registry
            .agents()
            .iter()
            .map(|spec| summarize(spec))
            .collect()
    });
    Json(summaries)
}

/// OpenAPI registration for the route above.
///
/// The document lives in `acpilot-proto` and cannot name a daemon handler without
/// depending on it, so the path is registered here, from the handler's own
/// metadata. The function body is never called.
#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/agents",
    tag = "agents",
    summary = "List agents in the active profile",
    responses((
        status = 200,
        description = "Every configured agent, with its availability on this host",
        body = Vec<AgentSummary>
    ))
)]
fn agents_doc() {}

/// `GET /v1/permissions?status=pending[&session=]`.
///
/// Pending requests are state, not history: a client that reconnects fetches
/// this instead of replaying `permission.requested` frames. Only `pending` is a
/// state v1 can list; anything else is refused rather than answered with an
/// empty list that would look like "nothing to do".
async fn permissions(
    State(state): State<AppState>,
    Query(query): Query<PendingQuery>,
) -> Result<Json<Vec<PermissionRequest>>, ApiError> {
    if let Some(status) = query.status.as_deref()
        && status != "pending"
    {
        return Err(ApiError::new(AcpilotError::invalid(format!(
            "status is `{status}`; v1 lists only `pending`, and a resolved request is \
             history rather than a state"
        ))));
    }
    let Some(broker) = &state.broker else {
        return Ok(Json(Vec::new()));
    };
    let mut pending = broker.pending();
    if let Some(session) = &query.session {
        pending.retain(|request| &request.session_id == session);
    }
    Ok(Json(pending))
}

#[derive(Debug, Deserialize)]
struct PendingQuery {
    status: Option<String>,
    session: Option<String>,
}

/// `GET /v1/events` — the profile-wide stream.
///
/// The stream announces a request the moment one appears, so a UI can offer a
/// prompt without polling `GET /v1/permissions`, and the preferences the moment a
/// settings file settles, so a client that cannot see this daemon's disk — a
/// window, a browser, anything over TCP — follows `theme` without a restart. Both
/// are announced here rather than on a session stream because both are
/// profile-wide: a client watching every session is exactly the client that must
/// not miss one.
///
/// Preferences are also the stream's first frame, because they are state and not
/// history: the daemon has one current answer, and a client that connects should
/// be told what it is rather than learn it only when somebody edits a file.
/// Requests are not replayed — one that arrived while a client was away is
/// recovered by fetching the pending list, which is state; replaying old prompts
/// would ask a person to answer something already answered (`docs/api.md`).
async fn events(State(state): State<AppState>) -> Response {
    let (sender, receiver) = mpsc::channel(EVENT_BUFFER);
    // Subscribed before the current answer is read, so a save landing between the
    // two is announced rather than silently skipped.
    let preferences = state.subscribe_preferences();
    if let Some(current) = state.preferences() {
        // The channel is fresh, so this cannot fail; if it somehow did, the pump
        // below is the next thing to notice.
        let _ = sender.send(prefs_frame(None, &current)).await;
    }
    let requests = state.broker.as_ref().map(|broker| broker.subscribe());
    tokio::spawn(pump(state, requests, preferences, sender));
    Sse::new(futures::stream::unfold(
        receiver,
        |mut receiver| async move {
            receiver
                .recv()
                .await
                .map(|frame| (Ok::<_, Infallible>(frame), receiver))
        },
    ))
    .into_response()
}

/// One `prefs` frame: the preferences this daemon would apply right now.
///
/// `id` is the frame's place in the stream, so `Last-Event-ID` and a `resync` have
/// something to name. The first frame of a connection has none: it is the answer
/// to a question, not a change.
fn prefs_frame(id: Option<u64>, preferences: &Preferences) -> Event {
    let mut frame = Event::default().event("prefs");
    if let Some(id) = id {
        frame = frame.id(id.to_string());
    }
    frame.data(serde_json::json!({ "preferences": preferences }).to_string())
}

/// What the pump has to say next.
enum Step {
    /// Time to prove the connection is still there.
    Beat,
    /// The broker announced something, or the subscription ended.
    Permission(Result<PermissionRequest, broadcast::error::RecvError>),
    /// The settings settled, or that subscription ended.
    Preferences(Result<Preferences, broadcast::error::RecvError>),
}

/// Turn announcements and a clock into SSE frames.
///
/// `requests` is `None` when the daemon has no broker: no request can ever be
/// announced, so that half of the stream stays quiet instead of closing. A closing
/// stream would make every client reconnect on the same two seconds the reconnect
/// advice names, forever, for a state the daemon could simply describe as quiet.
/// Preferences are never optional — every daemon has settings — which is why they
/// are the half that always has something to say.
async fn pump(
    state: AppState,
    mut requests: Option<broadcast::Receiver<PermissionRequest>>,
    mut preferences: broadcast::Receiver<Preferences>,
    sender: mpsc::Sender<Event>,
) {
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Announced frames only: the stream has no replay, so this number is the id of
    // the last frame it sent, which is what a `resync` names.
    let mut seq = 0_u64;

    loop {
        // A daemon without a broker has nothing to poll, and `pending` keeps the
        // branch parked rather than busy.
        let announced = async {
            match requests.as_mut() {
                Some(requests) => requests.recv().await,
                None => {
                    std::future::pending::<Result<PermissionRequest, broadcast::error::RecvError>>()
                        .await
                }
            }
        };

        let step = tokio::select! {
            _ = beat.tick() => Step::Beat,
            preferences = preferences.recv() => Step::Preferences(preferences),
            requests = announced => Step::Permission(requests),
        };

        let frame = match step {
            Step::Beat => Event::default()
                .event("heartbeat")
                .data(serde_json::json!({ "seq": seq }).to_string()),
            Step::Permission(Ok(request)) => {
                seq += 1;
                Event::default()
                    .event("permission")
                    .id(seq.to_string())
                    .data(
                        serde_json::json!({ "type": "requested", "request": request }).to_string(),
                    )
            }
            Step::Preferences(Ok(preferences)) => {
                seq += 1;
                prefs_frame(Some(seq), &preferences)
            }
            // The client fell behind the announcement channel. It cannot catch up
            // from the stream, so it is told where it now is and refetches what is
            // state: the pending list, and — for preferences, which this daemon can
            // answer directly — the current preferences.
            Step::Permission(Err(broadcast::error::RecvError::Lagged(_))) => Event::default()
                .event("resync")
                .data(serde_json::json!({ "latest_seq": seq }).to_string()),
            Step::Preferences(Err(broadcast::error::RecvError::Lagged(_))) => {
                match state.preferences() {
                    Some(current) => {
                        seq += 1;
                        prefs_frame(Some(seq), &current)
                    }
                    None => Event::default()
                        .event("resync")
                        .data(serde_json::json!({ "latest_seq": seq }).to_string()),
                }
            }
            Step::Permission(Err(broadcast::error::RecvError::Closed))
            | Step::Preferences(Err(broadcast::error::RecvError::Closed)) => break,
        };

        // A closed channel means the client hung up; there is nobody to tell.
        if sender.send(frame).await.is_err() {
            break;
        }
    }
}

/// `POST /v1/permissions/{id}/resolve`.
///
/// The body is read as a JSON value rather than as the option enum, so an
/// unknown option is our `400 invalid_request` naming the allowed set instead of
/// the framework's rejection page — the envelope is the contract, including when
/// the caller is wrong.
async fn resolve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Result<Json<PermissionDecision>, ApiError> {
    let Json(body) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not JSON: {rejection}"
        )))
    })?;
    let Some(option) = body
        .get("option")
        .and_then(|value| serde_json::from_value::<PermissionOption>(value.clone()).ok())
    else {
        return Err(
            ApiError::new(AcpilotError::invalid("`option` is missing or unknown"))
                .with_detail("option", "expected one of allow_once, allow_always, deny"),
        );
    };

    if let Some(previous) = state.decision(&id) {
        if previous.option == option {
            return Ok(Json(previous));
        }
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "permission request {id} was already decided as {}; the agent has acted on that \
             answer",
            option_str(previous.option)
        ))));
    }

    let Some(broker) = &state.broker else {
        return Err(unknown(&id));
    };
    let Some(request) = broker
        .pending()
        .into_iter()
        .find(|request| request.id == id)
    else {
        return Err(unknown(&id));
    };
    if !request.options.contains(&option) {
        return Err(ApiError::new(AcpilotError::invalid(format!(
            "permission request {id} does not offer that option"
        )))
        .with_detail("option", allowed(&request.options)));
    }
    if !broker.answer(&id, option) {
        // Between the list and the answer somebody else took it, or the wait
        // expired: either way this caller is not the one who decided.
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "permission request {id} was answered by someone else"
        ))));
    }

    let decision = PermissionDecision {
        option,
        decided_by: "user".to_owned(),
        at: jiff::Timestamp::now().to_string(),
    };
    state.remember(&id, decision.clone());
    Ok(Json(decision))
}

fn unknown(id: &str) -> ApiError {
    ApiError::new(AcpilotError::not_found(format!("permission request {id}")))
}

fn allowed(options: &[PermissionOption]) -> String {
    let listed: Vec<&str> = options.iter().copied().map(option_str).collect();
    format!("expected one of {}", listed.join(", "))
}

/// The spelling the wire uses for an option, for a message a person reads.
fn option_str(option: PermissionOption) -> &'static str {
    match option {
        PermissionOption::AllowOnce => "allow_once",
        PermissionOption::AllowAlways => "allow_always",
        PermissionOption::Deny => "deny",
    }
}

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    let mut document = ApiDoc::openapi();
    let paths = &mut document.paths.paths;
    paths.insert(
        "/v1/agents".to_owned(),
        utoipa::openapi::path::PathItem::new(
            utoipa::openapi::path::HttpMethod::Get,
            <__path_agents_doc as utoipa::Path>::operation(),
        ),
    );
    paths.insert(
        "/v1/permissions".to_owned(),
        utoipa::openapi::path::PathItem::new(
            utoipa::openapi::path::HttpMethod::Get,
            <__path_permissions_doc as utoipa::Path>::operation(),
        ),
    );
    paths.insert(
        "/v1/permissions/{id}/resolve".to_owned(),
        utoipa::openapi::path::PathItem::new(
            utoipa::openapi::path::HttpMethod::Post,
            <__path_resolve_doc as utoipa::Path>::operation(),
        ),
    );
    paths.insert(
        "/v1/events".to_owned(),
        utoipa::openapi::path::PathItem::new(
            utoipa::openapi::path::HttpMethod::Get,
            <__path_events_doc as utoipa::Path>::operation(),
        ),
    );
    // Sessions, terminals and files add several methods per path, so they append
    // rather than overwrite.
    let module_operations = sessions::operations()
        .into_iter()
        .chain(terminals::operations())
        .chain(files::operations());
    for (path, method, operation) in module_operations {
        let item = paths.entry(path.to_owned()).or_default();
        match method {
            utoipa::openapi::path::HttpMethod::Get => item.get = Some(operation),
            utoipa::openapi::path::HttpMethod::Post => item.post = Some(operation),
            utoipa::openapi::path::HttpMethod::Put => item.put = Some(operation),
            utoipa::openapi::path::HttpMethod::Delete => item.delete = Some(operation),
            _ => {}
        }
    }
    Json(document)
}

/// OpenAPI registration for the pending list.
#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/permissions",
    tag = "permissions",
    summary = "List requests waiting for an answer",
    params(
        ("status" = Option<String>, Query, description = "Only `pending` is a state; anything else is an error"),
        ("session" = Option<String>, Query, description = "Only requests from this session")
    ),
    responses(
        (status = 200, description = "Every pending request, oldest first", body = Vec<PermissionRequest>),
        (status = 400, description = "A status this build cannot list", body = ErrorBody)
    )
)]
fn permissions_doc() {}

/// OpenAPI registration for the profile-wide stream.
#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/events",
    tag = "events",
    summary = "Follow permission requests and settings changes",
    responses((
        status = 200,
        description = "text/event-stream: a `prefs` frame on connect and after every settled save, `permission` frames as requests appear, `heartbeat` every 15 s, `resync` after a gap"
    ))
)]
fn events_doc() {}

/// OpenAPI registration for the answer.
#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/permissions/{id}/resolve",
    tag = "permissions",
    summary = "Answer a pending request",
    params(("id" = String, Path, description = "Request id, as listed")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "The decision, recorded", body = PermissionDecision),
        (status = 400, description = "An option this request does not offer", body = ErrorBody),
        (status = 404, description = "No such request", body = ErrorBody),
        (status = 409, description = "Already decided differently, or answered elsewhere", body = ErrorBody)
    )
)]
fn resolve_doc() {}

/// The web shell: a page that says which build is answering, and can leave.
///
/// Served without a token, like `/docs`, because a browser cannot attach a header
/// to a typed address — and this page holds no data at all: one HTML file, the
/// version, and two buttons. What it proves is that the daemon can be reached from
/// a browser at all, which is the first question when the web UI is the one that
/// is misbehaving. The editor, transcript and terminal are the web phase's (P11).
async fn web_app(State(state): State<AppState>) -> Html<String> {
    Html(match state.web_palette() {
        Some(palette) => acpilot_web::themed_page(acpilot_core::VERSION, &palette),
        None => acpilot_web::page(acpilot_core::VERSION),
    })
}

/// Swagger UI, loaded from a CDN against this daemon's own OpenAPI route.
///
/// The page is served only with the `docs` feature so a production build has no
/// third-party script tag in it.
///
/// It has a token field because on a TCP listener every route — the contract this
/// page renders included — needs `Authorization: Bearer`, and a browser has no
/// way to attach one to a typed address. The field is the one path to the
/// document: the page fetches `/v1/openapi.json` itself with that header and hands
/// the parsed document to Swagger, and every "Try it out" call carries it too.
/// Nothing is stored, so a reload asks again rather than leaving a bearer token in
/// a web storage area for the life of the tab.
#[cfg(feature = "docs")]
async fn docs() -> Html<&'static str> {
    Html(
        r##"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>Acpilot API</title>
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css" />
    <style>
      #token-bar {
        display: flex;
        gap: 0.5rem;
        align-items: center;
        padding: 0.75rem 1rem;
        font: 13px/1.4 system-ui, sans-serif;
        background: #fafafa;
        border-bottom: 1px solid #ddd;
      }
      #token-bar input {
        flex: 1 1 28rem;
        font: inherit;
        padding: 0.35rem 0.5rem;
      }
      #status[data-refused] {
        color: #b00020;
      }
    </style>
  </head>
  <body>
    <div id="token-bar">
      <label for="token">Token</label>
      <input id="token" type="password" autocomplete="off" spellcheck="false"
             placeholder="the token acpilotd logged at startup" />
      <button id="load" type="button">Load</button>
      <span id="status"></span>
    </div>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
    <script>
      const field = document.getElementById("token");
      const status = document.getElementById("status");

      function say(message, refused) {
        status.textContent = message;
        if (refused) {
          status.setAttribute("data-refused", "");
        } else {
          status.removeAttribute("data-refused");
        }
      }

      async function load() {
        const token = field.value.trim();
        if (!token) {
          say("paste the token to load the contract", true);
          return;
        }
        const authorization = "Bearer " + token;
        const response = await fetch("/v1/openapi.json", {
          headers: { Authorization: authorization },
        });
        if (!response.ok) {
          say(
            response.status === 401
              ? "the daemon refused that token"
              : "cannot read the contract: " + response.status,
            true,
          );
          return;
        }
        const document = await response.json();
        say("");
        window.ui = SwaggerUIBundle({
          spec: document,
          dom_id: "#swagger-ui",
          requestInterceptor: (request) => {
            request.headers.Authorization = authorization;
            return request;
          },
        });
      }

      document.getElementById("load").addEventListener("click", load);
      field.addEventListener("keydown", (event) => {
        if (event.key === "Enter") load();
      });
    </script>
  </body>
</html>"##,
    )
}

/// Build the HTTP router with no profile loaded.
///
/// Kept separate from `main` so tests can exercise routes without binding a port.
pub fn router() -> Router {
    router_with(AppState::default())
}

/// Build the HTTP router over `state`.
///
/// The daemon's `main` calls [`router`] today because profile resolution is not
/// built; a caller that has loaded a profile passes it here.
pub fn router_with(state: AppState) -> Router {
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/agents", get(agents))
        .route("/v1/permissions", get(permissions))
        .route("/v1/permissions/{id}/resolve", post(resolve))
        .route("/v1/events", get(events))
        .route("/v1/openapi.json", get(openapi))
        .route("/app", get(web_app))
        .merge(files::routes())
        .merge(sessions::routes())
        .merge(terminals::routes())
        .with_state(state);

    #[cfg(feature = "docs")]
    let app = app.route("/docs", get(docs));

    app
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_web_page_theme_is_read_when_the_page_is_asked_for() {
        let directory = tempfile::tempdir().expect("temp dir");
        let marker = directory.path().join(".acpilot");
        std::fs::create_dir_all(&marker).expect("marker directory");
        let config = marker.join("config.toml");
        std::fs::write(&config, "[preferences]\ntheme_web = \"acpilot.paper\"\n")
            .expect("project settings");

        let state = AppState::new().with_workspace(Workspace {
            id: "thing".to_owned(),
            root: directory.path().to_path_buf(),
        });
        let palette = state.web_palette().expect("a theme is named");
        assert_eq!(
            palette
                .ui(acpilot_extensions::UiToken::Bg)
                .expect("a background")
                .to_string(),
            "#fbf7ef",
            "paper's background, from the project's settings"
        );

        // Editing the file is the next page load's colours: no restart, no cache.
        std::fs::write(
            &config,
            "[preferences]\ntheme_web = \"acpilot.night-flight\"\n",
        )
        .expect("project settings");
        let palette = state.web_palette().expect("a theme is named");
        assert_eq!(
            palette
                .ui(acpilot_extensions::UiToken::Bg)
                .expect("a background")
                .to_string(),
            "#070d18",
            "night flight, because the file changed"
        );
    }

    #[test]
    fn a_daemon_with_no_workspace_serves_the_pages_own_palette() {
        assert!(AppState::new().web_palette().is_none());
    }

    #[test]
    fn statuses_match_the_documented_contract() {
        let cases = [
            (AcpilotError::not_found("x"), StatusCode::NOT_FOUND),
            (AcpilotError::conflict("x"), StatusCode::CONFLICT),
            (AcpilotError::denied("x"), StatusCode::FORBIDDEN),
            (
                AcpilotError::HostUnavailable { host: "h".into() },
                StatusCode::BAD_GATEWAY,
            ),
            (
                AcpilotError::ModelUnavailable {
                    provider: "lmstudio".into(),
                    message: "connection refused".into(),
                },
                StatusCode::BAD_GATEWAY,
            ),
            (
                AcpilotError::AgentFailed {
                    message: "m".into(),
                },
                StatusCode::BAD_GATEWAY,
            ),
            (
                AcpilotError::Protocol {
                    message: "m".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                AcpilotError::Io(std::io::Error::other("disk")),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (error, expected) in cases {
            let label = error.to_string();
            assert_eq!(ApiError::new(error).status(), expected, "{label}");
        }
    }
}
