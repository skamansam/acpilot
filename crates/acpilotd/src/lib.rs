//! The daemon: one process that owns sessions, agents, permissions and terminals.
//!
//! Every UI is a client of this API, including the local TUI and CLI, so there is
//! exactly one place where state lives and exactly one auth story. P1 establishes
//! the router, the error contract, and the OpenAPI surface; P3 fills in the rest.
//!
//! Note what is *not* here: no file contents on the keystroke path, no agent
//! logic, no UI. The daemon is a control plane.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use acpilot_agent::supervisor::availability;
use acpilot_agent::{AgentSpec, Broker, PermissionOption, Registry};
use acpilot_core::AcpilotError;
use acpilot_proto::agent::AgentSummary;
use acpilot_proto::permission::{PermissionDecision, PermissionRequest};
use acpilot_proto::{ApiDoc, ErrorBody, ErrorDetail};
use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use utoipa::OpenApi as _;

#[cfg(feature = "docs")]
use axum::response::Html;

/// Service name reported by the health endpoint.
pub const SERVICE: &str = "acpilotd";

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

async fn health() -> Json<Health> {
    Json(Health {
        ok: true,
        service: SERVICE,
        version: env!("CARGO_PKG_VERSION"),
        api: acpilot_proto::API_PREFIX,
    })
}

/// State the routes share.
///
/// The registry is optional for now: the daemon has no profile resolution yet
/// (P4 owns it), so `acpilot serve` starts with none and the routes that need an
/// agent list answer from whatever the daemon was handed. Tests construct a real
/// [`Registry`], which is the same path the daemon will use once profiles land.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    agents: Option<Arc<Registry>>,
    broker: Option<Arc<Broker>>,
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
    let workspace = std::fs::canonicalize(workspace).map_err(|source| StateError::Workspace {
        path: workspace.to_path_buf(),
        source,
    })?;
    tracing::info!(
        profile = %profile.root().display(),
        workspace = %workspace.display(),
        rules = policy.rules().len(),
        "policy loaded"
    );
    let broker = Arc::new(Broker::with_timeout(policy, workspace, None));
    Ok(AppState::new().with_broker(broker))
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

/// Swagger UI, loaded from a CDN against this daemon's own OpenAPI route.
///
/// The page is served only with the `docs` feature so a production build has no
/// third-party script tag in it.
#[cfg(feature = "docs")]
async fn docs() -> Html<&'static str> {
    Html(
        r##"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>Acpilot API</title>
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css" />
  </head>
  <body>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
    <script>
      window.ui = SwaggerUIBundle({ url: "/v1/openapi.json", dom_id: "#swagger-ui" });
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
        .route("/v1/openapi.json", get(openapi))
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
