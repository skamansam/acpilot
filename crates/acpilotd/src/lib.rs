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

use acpilot_agent::supervisor::availability;
use acpilot_agent::{AgentSpec, Broker, KeyringResolver, PermissionOption, Registry};
use acpilot_core::AcpilotError;
use acpilot_proto::agent::AgentSummary;
use acpilot_proto::permission::{PermissionDecision, PermissionRequest};
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
pub use terminals::{Terminal, Terminals};

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
#[derive(Debug, Clone, Default)]
pub struct AppState {
    agents: Option<Arc<Registry>>,
    broker: Option<Arc<Broker>>,
    /// The workspace sessions open in, when the daemon was given one.
    workspace: Option<Workspace>,
    /// Every session this daemon owns.
    sessions: Sessions,
    /// Every terminal this daemon owns.
    terminals: Terminals,
    /// Decisions already delivered, by request id.
    ///
    /// The broker answers each question once and forgets it; the API promises
    /// that a retried `resolve` returns the same body and that a *different*
    /// option after a decision is a conflict. Remembering the decision is the
    /// only way to tell those two apart, and it is the daemon's to remember:
    /// the broker's job is to answer, not to be a history.
    decided: Arc<Mutex<HashMap<String, PermissionDecision>>>,
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
/// prompt without polling `GET /v1/permissions`. It is announced here rather
/// than on a session stream because the pending list is profile-wide: a client
/// watching every session is exactly the client that must not miss one.
///
/// Nothing is replayed. A request that arrived while the client was away is
/// recovered by fetching the pending list, which is state; replaying old prompts
/// would ask a person to answer something already answered (`docs/api.md`).
async fn events(State(state): State<AppState>) -> Response {
    let (sender, receiver) = mpsc::channel(EVENT_BUFFER);
    let requests = state.broker.as_ref().map(|broker| broker.subscribe());
    tokio::spawn(pump(requests, sender));
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

/// What the pump has to say next.
enum Step {
    /// Time to prove the connection is still there.
    Beat,
    /// The broker announced something, or the subscription ended.
    Announced(Result<PermissionRequest, broadcast::error::RecvError>),
}

/// Turn broker announcements and a clock into SSE frames.
///
/// `requests` is `None` when the daemon has no broker: no request can ever be
/// announced, so the stream only heartbeats instead of closing. A closing stream
/// would make every client reconnect on the same two seconds the reconnect
/// advice names, forever, for a state the daemon could simply describe as quiet.
async fn pump(
    mut requests: Option<broadcast::Receiver<PermissionRequest>>,
    sender: mpsc::Sender<Event>,
) {
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Announced requests only: the stream has no replay, so this number is the
    // id of the last frame it sent, which is what a `resync` names.
    let mut seq = 0_u64;

    loop {
        let step = match requests.as_mut() {
            Some(requests) => tokio::select! {
                _ = beat.tick() => Step::Beat,
                announced = requests.recv() => Step::Announced(announced),
            },
            None => {
                beat.tick().await;
                Step::Beat
            }
        };

        let frame = match step {
            Step::Beat => Event::default()
                .event("heartbeat")
                .data(serde_json::json!({ "seq": seq }).to_string()),
            Step::Announced(Ok(request)) => {
                seq += 1;
                Event::default()
                    .event("permission")
                    .id(seq.to_string())
                    .data(
                        serde_json::json!({ "type": "requested", "request": request }).to_string(),
                    )
            }
            // The client fell behind the announcement channel. It cannot catch
            // up from the stream, so it is told where it now is and refetches
            // the pending list — the same recovery as a reconnecting client.
            Step::Announced(Err(broadcast::error::RecvError::Lagged(_))) => Event::default()
                .event("resync")
                .data(serde_json::json!({ "latest_seq": seq }).to_string()),
            Step::Announced(Err(broadcast::error::RecvError::Closed)) => break,
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
    summary = "Follow new permission requests",
    responses((
        status = 200,
        description = "text/event-stream: `permission` frames as requests appear, `heartbeat` every 15 s, `resync` after a gap"
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
async fn web_app() -> Html<String> {
    Html(acpilot_web::page(acpilot_core::VERSION))
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
