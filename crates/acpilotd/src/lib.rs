//! The daemon: one process that owns sessions, agents, permissions and terminals.
//!
//! Every UI is a client of this API, including the local TUI and CLI, so there is
//! exactly one place where state lives and exactly one auth story. The foundation phase (P1) establishes
//! the router, the error contract, and the OpenAPI surface; the service phase (P3) fills in the rest.
//!
//! Note what is *not* here: no file contents on the keystroke path, no agent
//! logic, no UI. The daemon is a control plane.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use acpilot_agent::Terminals as AgentTerminals;
use acpilot_agent::{AgentSpec, Broker, IDLE_TIMEOUT, KeyringResolver, PermissionOption, Registry};
use acpilot_core::AcpilotError;
use acpilot_host::LocalHost;
use acpilot_proto::agent::AgentSummary;
use acpilot_proto::permission::{PermissionDecision, PermissionRequest};
use acpilot_proto::profile::Preferences;
use acpilot_proto::{ApiDoc, ErrorBody, ErrorDetail};
use acpilot_store::Store;
use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{
    DefaultPredicate, NotForContentType, Predicate, SizeAbove,
};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi as _;

mod auth;
mod base64;
// The daemon's settings: two files, then the flags (P3-1). Public because the
// binary resolves them before it has an `AppState` to attach them to.
pub mod config;
mod files;
pub mod helper;
pub mod hosts;
mod lsp;
// The in-process listener: the same router, served by a client instead of a
// process (P6-11).
pub mod embedded;
mod lsp_remote;
pub mod pidfile;
mod platform;
mod project;
mod pty_remote;
mod sessions;
mod stats;
mod terminals;
// What a person approved: the fingerprint a `POST /v1/agents/{id}/trust` records and
// the `untrusted` state it is the only way out of.
mod trust;
// What a conversation costs, counted from the events its agent streamed. Not a
// route and not a renderer: the sessions engine counts, `/v1/stats` reports.
mod usage;
mod workspaces;

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

/// How often the store drops events past its retention window.
///
/// Six hours against a thirty-day window: the sweep is a single indexed delete, and
/// a daemon that runs for a week without one is the case this is sized for. The
/// first tick is immediate, so every start is also a sweep.
pub const COMPACT_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

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
    /// How this daemon was installed, when a marker says (ADR 0010).
    ///
    /// A client whose version does not match reports the difference and needs to know
    /// who is allowed to reconcile it: a package manager that owns these files, or the
    /// program itself when the copy is the user's. Absent means no marker was found,
    /// which is treated as nobody's to replace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<acpilot_host::install::Install>,
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
        // Asked of the binary that is running, not of `PATH`: a second copy on this
        // machine is a different installation and says so for itself.
        install: std::env::current_exe()
            .ok()
            .and_then(|exe| acpilot_host::install::install_of(&exe)),
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
    /// The hosts this daemon knows, the one it runs on first.
    ///
    /// A registry rather than a table read per request: the local host's
    /// capabilities are a fact about this build, and a list that changed under a
    /// client between two reads would be a dialog that flickers.
    hosts: Arc<hosts::Hosts>,
    /// The workspaces clients have opened, and the check that each root was there.
    ///
    /// In memory like the host list: a workspace is a host and a path, and both are
    /// already durable facts. The daemon still serves one workspace's files; this is
    /// the registry that makes naming another one possible at all.
    workspaces: Arc<workspaces::Workspaces>,
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
    /// The SQLite record: sessions, their events and their decisions, kept after
    /// the process that streamed them is gone (the service phase's task P3-7).
    ///
    /// `None` in a test or an embedded daemon that was given no path, for the same
    /// reason transcripts are: a library value does not write into somebody's home
    /// directory uninvited.
    store: Option<Arc<Store>>,
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
    /// Answer cross-origin requests (`[daemon] cors`).
    ///
    /// Off unless the daemon was told the browser is somewhere else: the page is
    /// served from this origin, and the one other case (Vite on another port)
    /// proxies `/v1`, so CORS is an explicit decision rather than a default.
    cors: bool,
    /// Decisions already delivered, by request id.
    ///
    /// The broker answers each question once and forgets it; the API promises
    /// that a retried `resolve` returns the same body and that a *different*
    /// option after a decision is a conflict. Remembering the decision is the
    /// only way to tell those two apart, and it is the daemon's to remember:
    /// the broker's job is to answer, not to be a history.
    decided: Arc<Mutex<HashMap<String, PermissionDecision>>>,
    /// The language servers this daemon may ask about a file, one per language and
    /// started on demand.
    ///
    /// One registry rather than one per request: a server indexes a project once, and
    /// a daemon that spawned one per references request would pay that index every
    /// keystroke.
    language_servers: Arc<lsp::Servers>,
    /// Problems a language server published without being asked.
    ///
    /// A server with an index speaks on its own, and the pane that is reading the file
    /// did not ask it to: the frame this announces is how a gutter that was waiting on
    /// a background check fills in.
    diagnostics_changes: broadcast::Sender<acpilot_proto::file::DiagnosticsNotice>,
    /// The newest problems per file, so the stream can start a late-joining client
    /// with what is already known.
    ///
    /// Both halves write here: an answer to `POST /v1/files/diagnostics` and a publish
    /// the timer drained. That is what makes the frame state as well as an event — a
    /// client that connects after a server saw a problem is told it on connect rather
    /// than drawing an empty gutter until the next save.
    diagnostics: Arc<Mutex<DiagnosticsStore>>,
}

/// How many files the daemon keeps the last problems for.
///
/// A snapshot for a client that connects late, not a database: a workspace with more
/// files than this has more of them than anybody is looking at, and one that falls out
/// is asked about again when it is opened. Eviction is in announcement order, so the
/// file whose problems just moved is the last to go.
pub const DIAGNOSTICS_SNAPSHOT: usize = 64;

/// The newest problems the daemon knows about, by workspace-relative path.
#[derive(Debug, Default)]
struct DiagnosticsStore {
    notices: HashMap<String, acpilot_proto::file::DiagnosticsNotice>,
    /// The files in the order they were first announced, oldest first.
    order: VecDeque<String>,
}

impl DiagnosticsStore {
    /// Keep `notice`, dropping the oldest file when the store is full.
    fn record(&mut self, notice: acpilot_proto::file::DiagnosticsNotice) {
        if !self.notices.contains_key(&notice.path) {
            self.order.push_back(notice.path.clone());
            while self.order.len() > DIAGNOSTICS_SNAPSHOT {
                let Some(oldest) = self.order.pop_front() else {
                    break;
                };
                self.notices.remove(&oldest);
            }
        }
        self.notices.insert(notice.path.clone(), notice);
    }
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
            hosts: Arc::new(hosts::Hosts::default()),
            workspaces: Arc::default(),
            sessions: Sessions::default(),
            terminals: Terminals::default(),
            transcripts: None,
            store: None,
            cors: false,
            agent_terminals: Arc::new(AgentTerminals::new(Arc::new(LocalHost))),
            preference_changes: broadcast::channel(EVENT_BUFFER).0,
            decided: Arc::default(),
            language_servers: Arc::new(lsp::Servers::new()),
            diagnostics_changes: broadcast::channel(EVENT_BUFFER).0,
            diagnostics: Arc::default(),
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

    /// The language servers behind `GET /v1/files/references`.
    ///
    /// A registry rather than a server: which language server a file wants, and
    /// whether one is installed, is decided per request, and the sessions it has
    /// started are the daemon's for as long as it runs.
    pub fn language_servers(&self) -> &lsp::Servers {
        &self.language_servers
    }

    /// Subscribe to the problems the language servers publish on their own.
    ///
    /// The same broadcast the profile-wide stream reads, exposed so a test can watch
    /// the daemon's half without a socket.
    #[must_use]
    pub fn subscribe_diagnostics(
        &self,
    ) -> broadcast::Receiver<acpilot_proto::file::DiagnosticsNotice> {
        self.diagnostics_changes.subscribe()
    }

    /// Keep what a server said about a file, for a client that connects later.
    ///
    /// The answer to a question and a publish nobody asked for are the same fact about
    /// a file, so they land in the same place: the newest one wins, and a client that
    /// opens after both is told what the daemon knows instead of starting empty.
    pub fn record_diagnostics(&self, notice: acpilot_proto::file::DiagnosticsNotice) {
        let mut store = self
            .diagnostics
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        store.record(notice);
    }

    /// The problems the daemon knows about, one per file, sorted by path.
    ///
    /// Sorted rather than in announcement order because this is what the stream's
    /// first frames are built from: two connections opening the same daemon should not
    /// see the same snapshot in two different orders.
    #[must_use]
    pub fn diagnostics_snapshot(&self) -> Vec<acpilot_proto::file::DiagnosticsNotice> {
        let store = self
            .diagnostics
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut notices: Vec<acpilot_proto::file::DiagnosticsNotice> =
            store.notices.values().cloned().collect();
        notices.sort_by(|left, right| left.path.cmp(&right.path));
        notices
    }

    /// Take what the servers published since this was last called, and announce the
    /// documents whose problems changed.
    ///
    /// A publish outside the workspace is dropped rather than announced: a server may
    /// mention a file it read from a dependency path, and a client that cannot open it
    /// has nothing to do with the notice. Returns how many were announced, which is
    /// the only thing the sweep's log line needs.
    pub async fn drain_language_server_diagnostics(&self) -> usize {
        let Some(workspace) = self.workspace() else {
            return 0;
        };
        let pushed = self.language_servers.drain_published().await;
        let mut announced = 0;
        for pushed in pushed {
            let Ok(relative) = pushed.path.strip_prefix(&workspace.root) else {
                tracing::debug!(
                    path = %pushed.path.display(),
                    "a language server published about a file outside the workspace"
                );
                continue;
            };
            let notice = acpilot_proto::file::DiagnosticsNotice {
                path: relative.to_string_lossy().replace('\\', "/"),
                version: pushed.version,
                problems: acpilot_proto::file::FileDiagnostics {
                    server: Some(pushed.program.to_owned()),
                    encoding: Some(match pushed.encoding {
                        acpilot_lsp::PositionEncoding::Utf8 => {
                            acpilot_proto::lsp::PositionEncoding::Utf8
                        }
                        acpilot_lsp::PositionEncoding::Utf16 => {
                            acpilot_proto::lsp::PositionEncoding::Utf16
                        }
                    }),
                    items: pushed.items.iter().map(files::problem_row).collect(),
                },
            };
            // Kept before it is announced, so a client that connects mid-sweep finds
            // the file in the snapshot rather than racing the frame.
            self.record_diagnostics(notice.clone());
            // Nobody listening is not a failure: a daemon with no client attached has
            // nowhere to send an event, and the next client is told on connect from the
            // snapshot.
            let _ = self.diagnostics_changes.send(notice);
            announced += 1;
        }
        announced
    }

    /// Ask the language servers what they published, on a timer.
    ///
    /// Spawned by [`startup`] rather than by [`AppState::new`], like the idle sweep: a
    /// state constructed in a test should not start a clock, and one daemon wants one
    /// of these.
    ///
    /// # Panics
    ///
    /// If there is no tokio runtime, like every other `tokio::spawn`.
    pub fn watch_language_server_diagnostics(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(lsp::DIAGNOSTICS_POLL);
            loop {
                tick.tick().await;
                let announced = state.drain_language_server_diagnostics().await;
                if announced > 0 {
                    tracing::debug!(announced, "language server published problems");
                }
            }
        });
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
    ///
    /// The same [`acpilot_extensions::AppliedTheme`] the window and the terminal
    /// draw with: the browser is a surface, not a third interpretation of a theme
    /// file.
    #[must_use]
    pub fn web_palette(&self) -> Option<acpilot_extensions::AppliedTheme> {
        use acpilot_proto::profile::ThemeSurface;

        let settings = self.settings()?;
        let extensions = acpilot_extensions::builtin::load_all().ok()?;
        acpilot_extensions::AppliedTheme::from_extensions(
            &extensions,
            settings.preferences.theme_for(ThemeSurface::Web),
        )
        .ok()
    }

    /// Where transcripts are kept, when the daemon keeps them.
    pub fn transcripts(&self) -> Option<&std::path::Path> {
        self.transcripts.as_deref()
    }

    /// Keep sessions, events and decisions in this database.
    #[must_use]
    pub fn with_store(mut self, store: Arc<Store>) -> Self {
        match store.hosts() {
            Ok(records) => self.hosts.seed(records),
            Err(error) => tracing::warn!(%error, "the recorded hosts could not be read"),
        }
        self.store = Some(store);
        self
    }

    /// The hosts this daemon knows, the one it runs on first.
    pub fn hosts(&self) -> &hosts::Hosts {
        &self.hosts
    }

    /// The workspaces clients have opened on those hosts.
    pub fn workspaces(&self) -> &workspaces::Workspaces {
        &self.workspaces
    }

    /// The database behind the session routes, when the daemon opened one.
    pub fn store(&self) -> Option<&Arc<Store>> {
        self.store.as_ref()
    }

    /// Answer cross-origin requests.
    #[must_use]
    pub fn with_cors(mut self, cors: bool) -> Self {
        self.cors = cors;
        self
    }

    /// Whether this daemon answers a browser on another origin.
    pub fn cors(&self) -> bool {
        self.cors
    }

    /// Drop events past the retention window, on a timer.
    ///
    /// Run once at boot as well, because the interval's first tick is immediate and
    /// a daemon that is restarted daily would otherwise never reach a sweep.
    ///
    /// # Panics
    ///
    /// If there is no tokio runtime, like every other `tokio::spawn`.
    pub fn compact_store_on_a_timer(&self) {
        let Some(store) = self.store.clone() else {
            return;
        };
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(COMPACT_INTERVAL);
            loop {
                tick.tick().await;
                let before = jiff::Timestamp::now()
                    - jiff::SignedDuration::from_hours(acpilot_store::RETENTION_DAYS * 24);
                match store.compact(before) {
                    Ok(0) => {}
                    Ok(removed) => {
                        tracing::info!(removed, "compacted events past the retention window");
                    }
                    // A sweep that fails is retried in six hours: the events it
                    // would have dropped are not being read by anything.
                    Err(error) => tracing::warn!(%error, "the retention sweep failed"),
                }
            }
        });
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
    /// chose an id, say) becomes a **sessionless** pane rather than being dropped:
    /// no session owns it, so it lists under no session filter, and a client that
    /// asks for panes with no session can still find it.
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
                        let session_id = sessions.id_for_acp(handle.session_id());
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
    pub fn reap_idle_language_servers(&self) {
        let servers = Arc::clone(&self.language_servers);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(lsp::IDLE_SWEEP);
            // An interval's first tick fires immediately, and nothing can be idle
            // before a server has been started.
            tick.tick().await;
            loop {
                tick.tick().await;
                let stopped = servers.stop_idle(lsp::IDLE_TIMEOUT).await;
                if !stopped.is_empty() {
                    tracing::info!(?stopped, "stopped the language servers nothing had asked");
                }
            }
        });
    }

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

/// The state a launch serves, with the background work a daemon is.
///
/// One function for the binary and for embedded mode, because the parts after the
/// state are not optional: the terminal reaper ends the processes an agent opened
/// and never released, the mirror makes an agent's terminal watchable, and the
/// settings watch is how a client behind a socket sees a theme saved on this
/// machine. A second launch path that remembered only some of them would be a
/// daemon that behaves differently depending on who started it — which is exactly
/// what P6-11's single code path is for.
///
/// # Errors
///
/// The profile or the workspace could not be read (see [`state_for`]).
pub fn startup(
    profile: Option<&std::path::Path>,
    workspace: &std::path::Path,
) -> Result<AppState, StateError> {
    let state = state_for(profile, workspace)
        // Conversations outlive the process that streamed them, which is the point
        // of a transcript: a client that reconnects after a restart can read what
        // happened. Where that lives is the data directory, not the socket's.
        .map(|state| state.with_transcripts(transcript_dir()))?;
    // And the queryable half of the same record: a transcript a client cannot ask
    // about by id is a file, not a history. A store that cannot be opened costs the
    // history and one warning, not the daemon — a full disk is bad enough without
    // taking today's sessions with it.
    let state = open_store(state, profile);
    // Once at boot and then every six hours, because the first tick is immediate.
    state.compact_store_on_a_timer();
    // A terminal is a process; one an agent opened and never released has to end
    // on its own, because `terminal/release` is optional in ACP.
    state.reap_terminals_on_a_timer();
    // A kept language server is that same trade one layer up: a session is kept for
    // the next question, and a person who walks away has no next question. A stopped
    // server starts again on the next ask, so this is a cost, not a behaviour.
    state.reap_idle_language_servers();
    // And a problem a server finds on its own reaches the pane reading the file. The
    // transport queues a publish whether or not anybody is asking, so this is how a
    // gutter that was waiting on a background index fills in without a keystroke.
    state.watch_language_server_diagnostics();
    // And one an agent opened should be watchable: the same PTY, seen through a
    // pane a client can attach to.
    state.mirror_agent_terminals();
    // A saved `config.toml` is the theme a client is drawing with, and a client
    // behind a socket cannot see this machine's files: the daemon watches them and
    // announces the new preferences on `GET /v1/events`.
    state.watch_preferences();
    Ok(state)
}

/// Open the profile's database and attach it to `state`.
///
/// The path decision is [`acpilot_profile::Profile::database_path`]'s for a named
/// profile and the shared data directory's otherwise, so two ways of starting the
/// daemon cannot disagree about where a conversation lives.
///
/// A session from a previous process is marked `closed` here: nothing is running it,
/// and a list that says `busy` is a client waiting for a turn that will never come.
fn open_store(state: AppState, profile: Option<&std::path::Path>) -> AppState {
    let path = profile
        .and_then(|directory| acpilot_profile::Profile::open(directory).ok())
        .map_or_else(
            || acpilot_profile::data_dir().join(acpilot_store::FILE_NAME),
            |profile| profile.database_path(),
        );
    let store = match Store::open(&path) {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "sessions will not outlive this process"
            );
            return state;
        }
    };
    if let Some(workspace) = state.workspace()
        && let Err(error) =
            store.remember_workspace(&workspace.id, &workspace.root.to_string_lossy())
    {
        tracing::warn!(%error, "the workspace was not recorded");
    }
    match store.close_open_sessions() {
        Ok(0) => {}
        Ok(closed) => tracing::info!(
            closed,
            "sessions from a previous run are closed; nothing is running them"
        ),
        Err(error) => tracing::warn!(%error, "the stored sessions were not closed"),
    }
    tracing::info!(path = %path.display(), "sessions are kept in a database");
    state.with_store(Arc::new(store))
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
/// Availability is a `PATH` lookup on the host the daemon runs on, and the pin a
/// person recorded decides whether a binary that changed is `untrusted` (see
/// [`trust`]). Nothing is spawned, so listing agents cannot start one; a
/// handshake-dependent status is reported by the session routes, not here.
fn summarize(spec: &AgentSpec, pins: &HashMap<String, String>) -> AgentSummary {
    trust::summary(spec, pins.get(&spec.id).map(String::as_str))
}

/// `GET /v1/agents`.
///
/// An empty list means the daemon has no registry loaded; there is no profile to
/// answer 404 for yet.
async fn agents(State(state): State<AppState>) -> Json<Vec<AgentSummary>> {
    let pins = trust::pins(&state);
    let summaries = state.agents.as_ref().map_or_else(Vec::new, |registry| {
        registry
            .agents()
            .iter()
            .map(|spec| summarize(spec, &pins))
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
    // Then what the servers already said, so a client that connects after a publish is
    // not drawing an empty gutter until it asks: the frames are state as well as
    // events. They have no id, like the preferences frame — they are the answer to a
    // question rather than a change.
    for notice in state.diagnostics_snapshot() {
        let _ = sender.send(diagnostics_frame(None, &notice)).await;
    }
    let requests = state.broker.as_ref().map(|broker| broker.subscribe());
    let resolved = state
        .broker
        .as_ref()
        .map(|broker| broker.subscribe_resolved());
    let diagnostics = state.subscribe_diagnostics();
    tokio::spawn(pump(
        state,
        requests,
        resolved,
        preferences,
        diagnostics,
        sender,
    ));
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

/// One `diagnostics` frame: problems a server published about a file nobody asked
/// about.
///
/// `id` is the frame's place in the stream for the same reason [`prefs_frame`] carries
/// one: `Last-Event-ID` and a `resync` name a frame, and a client that reconnects has
/// something to say about where it was. Nothing is replayed — the frame is an event,
/// and a client that missed it asks about the file it opens.
fn diagnostics_frame(id: Option<u64>, notice: &acpilot_proto::file::DiagnosticsNotice) -> Event {
    let mut frame = Event::default().event("diagnostics");
    if let Some(id) = id {
        frame = frame.id(id.to_string());
    }
    frame.data(serde_json::json!({ "diagnostics": notice }).to_string())
}

/// What the pump has to say next.
enum Step {
    /// Time to prove the connection is still there.
    Beat,
    /// The broker announced something, or the subscription ended.
    Permission(Result<PermissionRequest, broadcast::error::RecvError>),
    /// Somebody answered a request, or that subscription ended.
    Resolved(Result<(String, PermissionOption), broadcast::error::RecvError>),
    /// The settings settled, or that subscription ended.
    Preferences(Result<Preferences, broadcast::error::RecvError>),
    /// A language server published problems, or that subscription ended.
    Diagnostics(Result<acpilot_proto::file::DiagnosticsNotice, broadcast::error::RecvError>),
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
    mut resolved: Option<broadcast::Receiver<(String, PermissionOption)>>,
    mut preferences: broadcast::Receiver<Preferences>,
    mut diagnostics: broadcast::Receiver<acpilot_proto::file::DiagnosticsNotice>,
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

        let answered = async {
            match resolved.as_mut() {
                Some(resolved) => resolved.recv().await,
                None => {
                    std::future::pending::<
                        Result<(String, PermissionOption), broadcast::error::RecvError>,
                    >()
                    .await
                }
            }
        };

        let step = tokio::select! {
            _ = beat.tick() => Step::Beat,
            preferences = preferences.recv() => Step::Preferences(preferences),
            requests = announced => Step::Permission(requests),
            answers = answered => Step::Resolved(answers),
            published = diagnostics.recv() => Step::Diagnostics(published),
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
            // The request is gone from the pending list by now, so the frame names
            // it rather than carrying it: a client closes the dialog it is showing
            // for that id, and one that was not showing it needs nothing.
            Step::Resolved(Ok((request_id, option))) => {
                seq += 1;
                Event::default()
                    .event("permission")
                    .id(seq.to_string())
                    .data(
                        serde_json::json!({
                            "type": "resolved",
                            "request_id": request_id,
                            "option": option,
                        })
                        .to_string(),
                    )
            }
            Step::Resolved(Err(broadcast::error::RecvError::Lagged(_))) => Event::default()
                .event("resync")
                .data(serde_json::json!({ "latest_seq": seq }).to_string()),
            Step::Diagnostics(Ok(notice)) => {
                seq += 1;
                diagnostics_frame(Some(seq), &notice)
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
            // A lagged diagnostics receiver is the same situation the permission half
            // handles: the frame is gone, and the client asks about the file it opens.
            Step::Diagnostics(Err(broadcast::error::RecvError::Lagged(_))) => Event::default()
                .event("resync")
                .data(serde_json::json!({ "latest_seq": seq }).to_string()),
            Step::Permission(Err(broadcast::error::RecvError::Closed))
            | Step::Resolved(Err(broadcast::error::RecvError::Closed))
            | Step::Preferences(Err(broadcast::error::RecvError::Closed))
            | Step::Diagnostics(Err(broadcast::error::RecvError::Closed)) => break,
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
    // Trust is a route of its own module: what a pin is and where it lives is
    // documented there, and the document only needs the operation.
    trust::register(&mut document);
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
    // The language servers are the daemon's own processes, so the route is the
    // registry's report rather than a module's: one row per language, running or not.
    paths.insert(
        "/v1/lsp/servers".to_owned(),
        utoipa::openapi::path::PathItem::new(
            utoipa::openapi::path::HttpMethod::Get,
            <__path_lsp_servers_doc as utoipa::Path>::operation(),
        ),
    );
    // Sessions, terminals and files add several methods per path, so they append
    // rather than overwrite.
    let module_operations = sessions::operations()
        .into_iter()
        .chain(terminals::operations())
        .chain(files::operations())
        .chain(project::operations())
        .chain(stats::operations());
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
    summary = "Follow permission requests, settings changes and problems a server found",
    responses((
        status = 200,
        description = "text/event-stream: a `prefs` frame on connect and after every settled save, `permission` frames as requests appear, `diagnostics` frames when a language server publishes problems nobody asked about, `heartbeat` every 15 s, `resync` after a gap"
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

/// What the page may load, and what may be done to it.
///
/// `default-src 'none'` is the starting point, and every entry after it is a
/// permission the one-file bundle needs: its script and its stylesheet are inline
/// (there is no second asset to name), and its requests are same-origin (the API,
/// the two streams and the terminal socket). `frame-ancestors 'none'` matters
/// most: a page that can be framed is a page whose token field can be clickjacked
/// out of a person, and `/` is unauthenticated precisely so it can be typed. The
/// policy is on the HTML only — JSON needs none of this, and a CSP on a stream
/// says nothing about a stream.
const PAGE_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; \
                       style-src 'unsafe-inline'; img-src 'self' data:; \
                       font-src 'self'; connect-src 'self'; base-uri 'none'; \
                       form-action 'none'; frame-ancestors 'none'";

/// The web app: the SPA at the daemon's root, and again at `/app`.
///
/// `/` is the entry point a person types. `/app` stays because it is where the
/// page has been published — in the docs, in links from a terminal, and in the
/// paths the page itself asks for (`/app/stats`, `/app/palette/events`). Two
/// spellings of one page, not two pages: the handler and the headers are the same
/// function, so they cannot disagree.
///
/// Served without a token, like `/docs`, because a browser cannot attach a header
/// to a typed address — and this page holds no data at all: one HTML file, the
/// version, and two buttons. What it proves is that the daemon can be reached from
/// a browser at all, which is the first question when the web UI is the one that
/// is misbehaving. The editor, transcript and terminal are the web phase's (P11).
async fn web_app(State(state): State<AppState>) -> Response {
    let html = match state.web_palette() {
        Some(theme) => acpilot_web::themed_page(acpilot_core::VERSION, &theme),
        None => acpilot_web::page(acpilot_core::VERSION),
    };
    page_response(html)
}

/// The page with the headers it is served under.
///
/// One function so a second page route cannot be added with the policy missing.
/// `no-referrer` is the second half of the token story: the page holds the secret
/// in session storage, and any outbound link must not be able to learn which
/// daemon address it was read on.
fn page_response(html: String) -> Response {
    let mut response = Html(html).into_response();
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(PAGE_CSP),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    response
}

/// The web page's colours, as they change.
///
/// A browser repaints this page by listening here, so a theme saved from the TUI,
/// the desktop app or another tab applies without a reload. `EventSource` cannot
/// set a header, which is why this stream is one of the unauthenticated ones — and
/// why it must not be given the token some other way: a secret in a query string
/// ends up in the daemon's own logs, the browser's history and any `Referer`.
///
/// What it carries instead is the palette the page was already served with
/// inline: the same names, the same colours, and nothing about any session. That
/// is the whole reason the colours can travel without a token — the page at `/app`
/// is public for the same reason, and this stream adds no fact to it.
///
/// The first frame is the palette in force right now, sent before the subscription
/// is waited on, so a page that connects after a save lands on the current theme
/// rather than the one its HTML was built with.
async fn web_palette_events(State(state): State<AppState>) -> Response {
    let (sender, receiver) = mpsc::channel(EVENT_BUFFER);
    // Subscribed before the current answer is read, so a save landing between the
    // two is announced rather than silently skipped.
    let mut changes = state.subscribe_preferences();
    if let Some(frame) = palette_frame(&state) {
        let _ = sender.send(frame).await;
    }
    tokio::spawn(async move {
        loop {
            match changes.recv().await {
                Ok(_) => {}
                // A page that fell behind gets the next palette, which is the whole
                // of the state: skipping a beat loses nothing.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
            match palette_frame(&state) {
                // A daemon that loses its settings mid-stream sends nothing: the
                // page keeps the last colours it had, which are still the ones its
                // stylesheet defaults to.
                None => {}
                Some(frame) => {
                    if sender.send(frame).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
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

/// One `palette` frame: the colours the web page should be painted with now.
fn palette_frame(state: &AppState) -> Option<Event> {
    let theme = state.web_palette()?;
    Some(
        Event::default()
            .event("palette")
            .data(acpilot_web::palette_json(&theme)),
    )
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

/// `GET /v1/lsp/servers`.
///
/// One row per language this build can start, not one per server that happens to be
/// running: a language with no session is the row that explains why a caret's
/// question was answered by the parse tree, and a list of only the running ones could
/// not. Running or stopped, the encoding and the last round trip are here because the
/// alternative is a client remembering what the daemon did — and a second client
/// remembering it differently.
async fn lsp_servers(
    State(state): State<AppState>,
) -> Json<Vec<acpilot_proto::lsp::LspServerSummary>> {
    use acpilot_proto::lsp::{LspServerStatus, LspServerSummary, PositionEncoding};

    let rows = state
        .language_servers()
        .status()
        .await
        .into_iter()
        .map(|server| LspServerSummary {
            id: server.program.to_owned(),
            languages: vec![server.language.to_owned()],
            command: server.program.to_owned(),
            status: if server.running.is_some() {
                LspServerStatus::Ready
            } else {
                LspServerStatus::Stopped
            },
            encoding: server
                .running
                .as_ref()
                .map(|running| match running.encoding {
                    acpilot_lsp::PositionEncoding::Utf8 => PositionEncoding::Utf8,
                    acpilot_lsp::PositionEncoding::Utf16 => PositionEncoding::Utf16,
                }),
            // Milliseconds, because a round trip is a number a person compares with
            // a keystroke and not with a clock.
            round_trip_ms: server
                .running
                .and_then(|running| running.round_trip)
                .and_then(|round_trip| u64::try_from(round_trip.as_millis()).ok()),
        })
        .collect();
    Json(rows)
}

/// The `GET /v1/lsp/servers` operation, for the OpenAPI document.
#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/lsp/servers",
    tag = "lsp",
    summary = "What the language servers are doing",
    responses(
        (status = 200, description = "One row per language this build can start: running or stopped, the encoding it negotiated, and the last references round trip", body = Vec<acpilot_proto::lsp::LspServerSummary>)
    )
)]
fn lsp_servers_doc() {}

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
    // Read before `with_state` moves it: the layer decision is made once, when the
    // router is built, rather than per request.
    let cors = state.cors();
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/agents", get(agents))
        .merge(trust::routes())
        .route("/v1/permissions", get(permissions))
        .route("/v1/permissions/{id}/resolve", post(resolve))
        .route("/v1/events", get(events))
        .route("/v1/lsp/servers", get(lsp_servers))
        .route("/v1/openapi.json", get(openapi))
        .route("/", get(web_app))
        .route("/app", get(web_app))
        .route("/app/palette/events", get(web_palette_events))
        .merge(files::routes())
        .merge(hosts::routes())
        .merge(sessions::routes())
        .merge(stats::routes())
        .merge(project::routes())
        .merge(terminals::routes())
        .merge(workspaces::routes())
        .with_state(state);

    #[cfg(feature = "docs")]
    let app = app.route("/docs", get(docs));

    // One span per request, from the method to the status: a slow route is visible
    // in the log without a line in every handler.
    let app = app.layer(TraceLayer::new_for_http());
    // Compression for the JSON, never for the event streams. A compressed
    // `text/event-stream` is held until the compressor has enough bytes, which for a
    // transcript means a stream that arrives in bursts; `SizeAbove` leaves the small
    // answers alone, where the framing costs more than the body.
    let app = app.layer(
        CompressionLayer::new().compress_when(
            DefaultPredicate::new()
                .and(NotForContentType::new("text/event-stream"))
                .and(SizeAbove::new(1024)),
        ),
    );
    if cors {
        app.layer(CorsLayer::permissive())
    } else {
        app
    }
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
        let theme = state.web_palette().expect("a theme is named");
        assert_eq!(
            theme.color(acpilot_extensions::UiToken::Bg).to_string(),
            "#fbf7ef",
            "paper's background, from the project's settings"
        );

        // Editing the file is the next page load's colours: no restart, no cache.
        std::fs::write(
            &config,
            "[preferences]\ntheme_web = \"acpilot.night-flight\"\n",
        )
        .expect("project settings");
        let theme = state.web_palette().expect("a theme is named");
        assert_eq!(
            theme.color(acpilot_extensions::UiToken::Bg).to_string(),
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
