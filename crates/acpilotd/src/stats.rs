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
//! comfortable-looking fraction of a window nobody stated.
//!
//! A declared provider gets a row too, ahead of the roster and in declaration
//! order. It has no conversation, so it reports no context; it reports credits only
//! when this build knows the provider's balance endpoint — OpenRouter's
//! `GET /api/v1/credits` — because a guessed URL answered with `404` would be drawn
//! as `0.00`. The read is cached for a minute: the bar polls faster than a balance
//! changes, and a stats request is not a reason to hammer somebody's account. A
//! provider with no endpoint, a disabled one, or one whose read failed has no
//! `credits` key rather than a zero.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use acpilot_proto::model::ProviderStatus;
use acpilot_proto::stats::{CreditStats, IntegrationKind, IntegrationStats};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::AppState;
use crate::providers::{self, Credential};

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

/// How long a balance read is reused.
///
/// The bar polls every fifteen seconds and a credit balance moves when a turn is
/// paid for, not once a poll; a minute keeps the figure fresh without turning one
/// open window into sixty requests to somebody's account.
const BALANCE_TTL: Duration = Duration::from_secs(60);

/// What this daemon last read from a provider's balance endpoint.
#[derive(Debug)]
struct Reading {
    /// The declaration the reading belongs to. A provider re-pointed at another
    /// URL is asked again rather than shown the old server's balance.
    fingerprint: String,
    /// When the answer arrived, for `updated_at`.
    stamp: String,
    /// When it arrived, for the cache.
    at: Instant,
    /// `None` when the provider has no endpoint this build reads, or the read
    /// failed. Keeping the miss is what stops a broken endpoint being dialled on
    /// every poll.
    credits: Option<CreditStats>,
}

/// The balances this daemon has read, by provider id.
#[derive(Debug, Default)]
pub(crate) struct Balances {
    known: Mutex<HashMap<String, Reading>>,
}

impl Balances {
    /// The provider's balance, read or reused.
    async fn credits(
        &self,
        providers: &providers::Providers,
        summary: &acpilot_proto::model::ProviderSummary,
    ) -> Option<(CreditStats, String)> {
        let declaration = providers.declaration(&summary.id)?;
        let fingerprint = format!(
            "{}|{}",
            declaration.base_url,
            declaration.credential_ref.as_deref().unwrap_or("")
        );
        {
            let known = self.lock();
            if let Some(reading) = known.get(&summary.id)
                && reading.fingerprint == fingerprint
                && reading.at.elapsed() < BALANCE_TTL
            {
                return reading
                    .credits
                    .clone()
                    .map(|credits| (credits, reading.stamp.clone()));
            }
        }
        let stamp = jiff::Timestamp::now().to_string();
        let token = match Credential::resolve(providers.resolver(), &declaration.request()) {
            Credential::None => None,
            Credential::Bearer(token) => Some(token),
            Credential::Unusable(reason) => {
                // Sending anonymously would let the server take the blame for a
                // local keyring problem, so nothing is sent at all.
                tracing::debug!(provider = %summary.id, %reason, "no balance was read");
                self.remember(&summary.id, fingerprint, stamp, None);
                return None;
            }
        };
        let answer =
            acpilot_model::balance(&summary.id, &declaration.base_url, token.as_deref()).await;
        let credits = match answer {
            Ok(credits) => credits,
            Err(error) => {
                tracing::debug!(provider = %summary.id, %error, "no balance was read");
                None
            }
        };
        self.remember(&summary.id, fingerprint, stamp.clone(), credits.clone());
        credits.map(|credits| (credits, stamp))
    }

    fn remember(&self, id: &str, fingerprint: String, stamp: String, credits: Option<CreditStats>) {
        self.lock().insert(
            id.to_owned(),
            Reading {
                fingerprint,
                stamp,
                at: Instant::now(),
                credits,
            },
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Reading>> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Every integration this daemon knows about, with whatever it can report.
async fn collect(state: &AppState) -> Vec<IntegrationStats> {
    let mut rows = provider_rows(state).await;
    let Some(registry) = &state.agents else {
        // No `agents.toml` is no roster, not an empty roster: the bar draws nothing
        // rather than a row per agent nobody configured.
        return rows;
    };
    let now = jiff::Timestamp::now().to_string();
    rows.extend(registry.agents().iter().map(|spec| {
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
    }));
    rows
}

/// A row per declared provider, in declaration order.
///
/// A disabled provider is a row and nothing more: its declaration is the decision
/// not to contact it, so no balance is read. A provider that is not disabled is
/// asked only when its balance endpoint is one this build knows.
async fn provider_rows(state: &AppState) -> Vec<IntegrationStats> {
    let now = jiff::Timestamp::now().to_string();
    let mut rows = Vec::new();
    for summary in state.providers().list() {
        let reading = if summary.status == ProviderStatus::Disabled {
            None
        } else {
            state.balances.credits(state.providers(), &summary).await
        };
        let (credits, updated_at) = match reading {
            Some((credits, at)) => (Some(credits), at),
            None => (
                None,
                summary
                    .last_probed_at
                    .clone()
                    .unwrap_or_else(|| now.clone()),
            ),
        };
        rows.push(IntegrationStats {
            id: summary.id,
            kind: IntegrationKind::Provider,
            label: summary.label,
            // A provider has no conversation of its own, and `used_tokens: 0`
            // would be a measurement nobody made.
            context: None,
            credits,
            updated_at,
        });
    }
    rows
}

/// `GET /v1/stats`.
async fn list(State(state): State<AppState>) -> Json<Vec<IntegrationStats>> {
    Json(collect(&state).await)
}

/// `GET /app/stats` — the same rows, measured for a renderer that cannot measure.
async fn page(State(state): State<AppState>) -> Json<Vec<acpilot_stats::Row>> {
    Json(acpilot_stats::rows(
        &collect(&state).await,
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
    operation_id = "read_stats",
    path = "/v1/stats",
    tag = "stats",
    summary = "Context and credits per integration",
    responses((status = 200, description = "One row per integration, including the quiet ones", body = Vec<IntegrationStats>))
)]
fn list_doc() {}
