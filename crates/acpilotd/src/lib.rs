//! The daemon: one process that owns sessions, agents, permissions and terminals.
//!
//! Every UI is a client of this API, including the local TUI and CLI, so there is
//! exactly one place where state lives and exactly one auth story. P1 establishes
//! the router, the error contract, and the OpenAPI surface; P3 fills in the rest.
//!
//! Note what is *not* here: no file contents on the keystroke path, no agent
//! logic, no UI. The daemon is a control plane.

use std::sync::Arc;

use acpilot_agent::supervisor::availability;
use acpilot_agent::{AgentSpec, Registry};
use acpilot_core::AcpilotError;
use acpilot_proto::agent::AgentSummary;
use acpilot_proto::{ApiDoc, ErrorBody};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
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
/// This newtype exists because the error type lives in `acpilot-core`, which must
/// not depend on `axum`. Mapping happens here, once, so every route returns the
/// same envelope.
#[derive(Debug)]
pub struct ApiError(pub AcpilotError);

impl From<AcpilotError> for ApiError {
    fn from(error: AcpilotError) -> Self {
        Self(error)
    }
}

impl ApiError {
    /// HTTP status for this failure class.
    ///
    /// `HostUnavailable`, `ModelUnavailable` and `AgentFailed` are `502`: the
    /// daemon is fine, the thing behind it is not. That distinction is what lets
    /// a client offer "retry" instead of "give up".
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self.0 {
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
        let body = ErrorBody::new(self.0.code(), self.0.to_string());
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
}

impl AppState {
    /// No profile loaded: the agent list is empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve this profile's agents.
    pub fn with_registry(registry: Arc<Registry>) -> Self {
        Self {
            agents: Some(registry),
        }
    }

    /// The registry behind the agent routes, when the daemon has one.
    pub fn registry(&self) -> Option<&Arc<Registry>> {
        self.agents.as_ref()
    }
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

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    let mut document = ApiDoc::openapi();
    document.paths.paths.insert(
        "/v1/agents".to_owned(),
        utoipa::openapi::path::PathItem::new(
            utoipa::openapi::path::HttpMethod::Get,
            <__path_agents_doc as utoipa::Path>::operation(),
        ),
    );
    Json(document)
}

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
            assert_eq!(ApiError(error).status(), expected, "{label}");
        }
    }
}
