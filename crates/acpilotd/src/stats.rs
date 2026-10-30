//! Integration stats: the figures behind the bar, and the browser's copy of it.
//!
//! `GET /v1/stats` is one row per integration the daemon can see, in the shape the
//! contract fixes (`IntegrationStats`). [`acpilot_stats`] turns those rows into
//! tracks; the terminal does that locally, and the page cannot compile Rust, which
//! is what `/app/stats` is for — the same rows, already measured.
//!
//! # What this daemon can actually report
//!
//! The registry: one row per configured agent, so the bar lists the roster even
//! when nothing is running. For an agent with a conversation behind it, a context
//! reading comes from the sessions engine ([`crate::usage`]): the tokens the
//! conversation carries, estimated from the events the agent streamed, or the figure
//! the agent itself reported when it sends a usage update. A session that has done
//! nothing yet reports nothing, and the field is absent rather than a measured zero —
//! the bar draws a roster row that says so instead of a full or empty track.
//!
//! The window is absent unless the *agent* names one. Nothing in a profile states a
//! window yet, and a client must not substitute a default, so the bar draws an
//! estimate with an indeterminate track (`18.4k / ?`) rather than a
//! comfortable-looking fraction of a window nobody stated. A configured window is
//! part of the models phase's task (P4.5-12), which owns the provider store this
//! daemon does not have; provider rows and balance reads are that task's too.

use acpilot_proto::stats::{IntegrationKind, IntegrationStats};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::AppState;

/// The stats routes.
///
/// `/app/stats` is not a second API: it is the page's view of the same collector,
/// because a browser has no `acpilot-stats` to link. It needs the token like every
/// other route — it carries an agent roster, which is not the page's own business —
/// and the page sends it in a header for the same reason it sends the others'.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/stats", get(list))
        .route("/app/stats", get(page))
}

/// Every integration this daemon knows about, with whatever it can report.
fn collect(state: &AppState) -> Vec<IntegrationStats> {
    let Some(registry) = &state.agents else {
        // No `agents.toml` is no roster, not an empty roster: the bar draws nothing
        // rather than a row per agent nobody configured.
        return Vec::new();
    };
    let now = jiff::Timestamp::now().to_string();
    registry
        .agents()
        .iter()
        .map(|spec| {
            // The newest conversation with this agent, when it has one. The stamp is
            // the session's own rather than this request's: a roster read is not an
            // observation of a conversation, and a bar whose figures are always
            // seconds old is a bar that can never show that one went quiet.
            let reading = state.sessions.reading(&spec.id);
            IntegrationStats {
                id: spec.id.clone(),
                kind: IntegrationKind::Agent,
                label: spec.name.clone(),
                // See the module note: absent, not zero.
                context: reading.as_ref().map(|(context, _)| *context),
                credits: None,
                updated_at: reading.map_or_else(|| now.clone(), |(_, at)| at),
            }
        })
        .collect()
}

/// `GET /v1/stats`.
async fn list(State(state): State<AppState>) -> Json<Vec<IntegrationStats>> {
    Json(collect(&state))
}

/// `GET /app/stats` — the same rows, measured for a renderer that cannot measure.
async fn page(State(state): State<AppState>) -> Json<Vec<acpilot_stats::Row>> {
    Json(acpilot_stats::rows(
        &collect(&state),
        jiff::Timestamp::now(),
    ))
}

/// The OpenAPI operation for the route above.
///
/// Only the API route is registered: `/app/*` is the page's own surface, described
/// here and in `docs/api.md`, not something a generated client is pointed at.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    vec![(
        "/v1/stats",
        HttpMethod::Get,
        <__path_list_doc as utoipa::Path>::operation(),
    )]
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/stats",
    tag = "stats",
    summary = "Context and credits per integration",
    responses((status = 200, description = "One row per integration, including the quiet ones", body = Vec<IntegrationStats>))
)]
fn list_doc() {}
