//! The bridge's remote transport: ACP over HTTP with JSON-RPC bodies.
//!
//! The local transport (see [`crate::bridge`]) owns a driver's pipes; this one
//! owns nothing, so it needs two things the pipes gave for free: proof of who the
//! driver is, and a way to send it words as they arrive.
//!
//! # Two listeners, two credentials
//!
//! The cockpit routes (`/v1/acp/...`) live on the daemon's own listener and are
//! behind the daemon token like every other route. The driver routes live on a
//! listener configured by `[acp.bridge]` and take the *driver's* token — the one a
//! pairing code minted. A driver never holds the daemon token, and the daemon token
//! never opens the bridge.
//!
//! # Claiming a pair
//!
//! `POST /pair` is the one open route: the code is the credential, it is single-use
//! and it expires (see [`crate::drivers`]). It answers with the driver's row and its
//! token, once.
//!
//! # Where the notifications go
//!
//! JSON-RPC over HTTP has a request and a response; ACP's `session/update` is a
//! notification the agent sends unprompted. `GET /acp/events` is the driver's
//! stream, filtered to that driver, and it is where a prompt's chunks, tool calls and
//! permission questions arrive while `POST /acp` still holds the prompt's response
//! open. A
//! driver that never opens it can still prompt and gets the answer at the end, but
//! it reads the turn as one silence.
//!
//! # A driver may type, not sign
//!
//! A permission request raised by a driver-started turn becomes a
//! `session/request_permission` notification only when the driver holds
//! `answer_permissions`. Without it, the driver is told — once per turn, as a
//! notice — that the person at the machine answers, and the request stays on the
//! daemon's own pending list where the TUI, the CLI and the page already read it.
//! It is the one place this bridge deliberately does less than ACP allows, and the
//! ticket says why: an agent with a driver's credit card is an agent nobody watched.

use std::convert::Infallible;
use std::time::Duration;

use acpilot_proto::bridge::{
    AcpBridgeSummary, AcpDriverSummary, CreatePairingRequest, PairDriverRequest,
};
use acpilot_proto::event::SessionEvent;
use acpilot_proto::permission::{PermissionOption, PermissionRequest};
use agent_client_protocol::v1::schema::{
    CancelNotification, NewSessionRequest, Notice, NoticeSeverity, PromptRequest, SessionId,
    SessionNotification, SessionUpdate, StopReason,
};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path as ApiPath, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::drivers::{Driver, PairError};
use crate::{AcpilotError, ApiError, AppState};

/// The cockpit routes, for the OpenAPI document.
pub fn operations() -> Vec<(
    &'static str,
    utoipa::openapi::path::HttpMethod,
    utoipa::openapi::path::Operation,
)> {
    use utoipa::openapi::path::HttpMethod;
    vec![
        (
            "/v1/acp/bridge",
            HttpMethod::Get,
            <__path_bridge_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/acp/drivers",
            HttpMethod::Get,
            <__path_drivers_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/acp/pairings",
            HttpMethod::Post,
            <__path_pairings_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/acp/drivers/{id}/revoke",
            HttpMethod::Post,
            <__path_revoke_doc as utoipa::Path>::operation(),
        ),
    ]
}

/// The cockpit's half: pairing, the connection panel and revocation.
///
/// On the daemon's listener, so the daemon token guards it like every other route.
/// `POST /v1/acp/pairings` is the human act — the person holding the cockpit decides
/// whether this driver may answer permission requests.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/acp/bridge", get(bridge))
        .route("/v1/acp/drivers", get(drivers))
        .route("/v1/acp/pairings", post(pairings))
        .route("/v1/acp/drivers/{id}/revoke", post(revoke))
}

/// The driver's half: pair, then speak JSON-RPC.
///
/// Served by `main` on `[acp.bridge].bind`, and only when that table enables it.
pub fn router(state: AppState) -> Router {
    let guarded = Router::new()
        .route("/acp", post(rpc))
        .route("/acp/events", get(events))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/pair", post(pair))
        .merge(guarded)
        .with_state(state)
}

/// `GET /v1/acp/bridge`.
async fn bridge(State(state): State<AppState>) -> Json<AcpBridgeSummary> {
    let settings = state.bridge();
    Json(state.drivers().bridge(
        settings.enabled,
        settings.bind.clone(),
        settings.public_url.clone(),
        settings.token_required(),
    ))
}

/// `GET /v1/acp/drivers`.
async fn drivers(State(state): State<AppState>) -> Json<Vec<AcpDriverSummary>> {
    Json(state.drivers().summaries())
}

/// `POST /v1/acp/pairings`.
async fn pairings(
    State(state): State<AppState>,
    body: Result<Json<CreatePairingRequest>, JsonRejection>,
) -> Result<Json<acpilot_proto::bridge::PairingCode>, ApiError> {
    let request = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a pairing request: {rejection}"
        )))
    })?;
    let settings = state.bridge();
    if !settings.enabled {
        return Err(ApiError::new(AcpilotError::conflict(
            "the ACP bridge is off; set `[acp.bridge] enabled = true` with a `bind` address",
        )));
    }
    let handshake = settings.public_url.clone().or_else(|| {
        settings
            .bind
            .as_deref()
            .map(|bind| format!("http://{bind}"))
    });
    Ok(Json(
        state
            .drivers()
            .issue(request.0.grant_answer_permissions, handshake),
    ))
}

/// `POST /v1/acp/drivers/{id}/revoke`.
async fn revoke(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<AcpDriverSummary>, ApiError> {
    state
        .drivers()
        .revoke(&id)
        .map(Json)
        .map_err(|message| ApiError::new(AcpilotError::not_found(message)))
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    operation_id = "acp_bridge",
    path = "/v1/acp/bridge",
    tag = "bridge",
    summary = "What the ACP bridge is doing",
    description = "Whether the driver listener is on, the address it bound, and how many drivers are connected or on record. Off unless `[acp.bridge] enabled = true` names a `bind` address.",
    responses(
        (status = 200, description = "The bridge's state", body = acpilot_proto::bridge::AcpBridgeSummary)
    )
)]
fn bridge_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    operation_id = "acp_drivers",
    path = "/v1/acp/drivers",
    tag = "bridge",
    summary = "The drivers paired with this daemon",
    description = "Including revoked drivers: the row answers who prompted a turn after the credential has stopped working.",
    responses(
        (status = 200, description = "One row per driver", body = Vec<acpilot_proto::bridge::AcpDriverSummary>)
    )
)]
fn drivers_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    operation_id = "pair_driver",
    path = "/v1/acp/pairings",
    tag = "bridge",
    summary = "Issue a one-time pairing code",
    description = "The code is shown to a person, typed into a driver, single-use and expires in two minutes. `grant_answer_permissions` is the person's decision, not the driver's: without it the driver's turns wait for a human to answer the agent's permission requests.",
    request_body = acpilot_proto::bridge::CreatePairingRequest,
    responses(
        (status = 200, description = "The code and when it expires", body = acpilot_proto::bridge::PairingCode),
        (status = 400, description = "The body is not a pairing request", body = acpilot_proto::ErrorBody),
        (status = 409, description = "The bridge is not enabled", body = acpilot_proto::ErrorBody)
    )
)]
fn pairings_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    operation_id = "revoke_driver",
    path = "/v1/acp/drivers/{id}/revoke",
    tag = "bridge",
    summary = "Revoke a driver's token",
    description = "The next request carrying the old token is `401`; the row stays so a transcript can still answer who prompted a turn.",
    params(("id" = String, Path, description = "Driver id")),
    responses(
        (status = 200, description = "The revoked row", body = acpilot_proto::bridge::AcpDriverSummary),
        (status = 404, description = "No such driver", body = acpilot_proto::ErrorBody)
    )
)]
fn revoke_doc() {}

/// A JSON-RPC request body.
#[derive(Debug, Deserialize)]
struct Rpc {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Why a call did not produce a result.
#[derive(Debug)]
struct RpcError {
    code: i32,
    message: String,
}

impl RpcError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    fn unknown(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("`{method}` is not a method this bridge serves"),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
        }
    }

    /// The driver is authenticated but lacks the scope the call needs.
    fn scope(scope: &str) -> Self {
        Self {
            code: -32003,
            message: format!(
                "this driver has no `{scope}` grant; the person at the machine decides this"
            ),
        }
    }
}

/// Reject a request that does not carry a driver token, and rate-limit one that does.
async fn authenticate(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim());
    let settings = state.bridge();
    let driver = match presented.and_then(|token| state.drivers().authenticate(token)) {
        Some(driver) => driver,
        // A loopback-only listener may skip the token, and the person at the
        // machine then holds every scope: there is no remote driver to limit.
        None if !settings.token_required() => Driver {
            id: "loopback".to_owned(),
            name: "loopback".to_owned(),
            scopes: vec![
                acpilot_proto::bridge::DriverScope::Observe,
                acpilot_proto::bridge::DriverScope::Prompt,
                acpilot_proto::bridge::DriverScope::Cancel,
                acpilot_proto::bridge::DriverScope::AnswerPermissions,
            ],
        },
        None => {
            return refusal(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "the bridge requires a driver token; claim a pairing code first",
            );
        }
    };
    if !state
        .drivers()
        .rate_allows(&driver.id, settings.rate_per_minute())
    {
        return refusal(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "this driver has made its limit of requests for the minute",
        );
    }
    request.extensions_mut().insert(driver);
    next.run(request).await
}

/// One refusal, in the envelope the rest of the API uses.
fn refusal(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(acpilot_proto::ErrorBody::new(code, message))).into_response()
}

/// `POST /pair`: claim a code and receive the token once.
async fn pair(
    State(state): State<AppState>,
    body: Result<Json<PairDriverRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => {
            return refusal(
                StatusCode::BAD_REQUEST,
                "invalid_pairing",
                &format!("the body is not a pair request: {rejection}"),
            );
        }
    };
    if request.name.trim().is_empty() {
        return refusal(
            StatusCode::BAD_REQUEST,
            "invalid_pairing",
            "a driver needs a name; it is what the cockpit lists",
        );
    }
    match state.drivers().pair(&request) {
        Ok(paired) => (StatusCode::OK, Json(paired)).into_response(),
        Err(PairError::Expired) => refusal(
            StatusCode::BAD_REQUEST,
            "pairing_expired",
            "the pairing code has expired; ask the cockpit for another",
        ),
        Err(PairError::Invalid) => refusal(
            StatusCode::BAD_REQUEST,
            "invalid_pairing",
            "that pairing code is not one this daemon issued, or it has been used",
        ),
        Err(PairError::State(message)) => refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_failed",
            &message,
        ),
    }
}

/// `POST /acp`: dispatch one JSON-RPC method.
async fn rpc(
    State(state): State<AppState>,
    Extension(driver): Extension<Driver>,
    body: Result<Json<Rpc>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = body else {
        return failure(&Value::Null, RpcError::invalid("the body is not JSON-RPC"));
    };
    let id = request.id.clone();
    let answer = dispatch(&state, &driver, &request).await;
    match answer {
        Ok(result) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response(),
        Err(error) => failure(&id, error),
    }
}

async fn dispatch(state: &AppState, driver: &Driver, request: &Rpc) -> Result<Value, RpcError> {
    match request.method.as_str() {
        "initialize" => {
            let client = request.params.pointer("/clientInfo").and_then(|info| {
                let name = info.get("name")?.as_str()?;
                let version = info
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Some(if version.is_empty() {
                    name.to_owned()
                } else {
                    format!("{name} {version}")
                })
            });
            state.drivers().note_client(&driver.id, client);
            Ok(json!({
                "protocolVersion": request
                    .params
                    .get("protocolVersion")
                    .cloned()
                    .unwrap_or(json!(1)),
                "agentCapabilities": {},
                "agentInfo": { "name": "acpilotd", "version": env!("CARGO_PKG_VERSION") }
            }))
        }
        "session/new" => {
            require(driver, acpilot_proto::bridge::DriverScope::Observe)?;
            let session: NewSessionRequest = serde_json::from_value(request.params.clone())
                .map_err(|error| RpcError::invalid(format!("session/new: {error}")))?;
            let summary = crate::bridge::open_session(state, &session, Some(&driver.id))
                .await
                .map_err(|error| RpcError::invalid(error.to_string()))?;
            state.drivers().claim_session(&summary.id, &driver.id);
            state.drivers().note_session(&driver.id);
            Ok(json!({ "sessionId": summary.id }))
        }
        "session/prompt" => {
            require(driver, acpilot_proto::bridge::DriverScope::Prompt)?;
            let prompt: PromptRequest = serde_json::from_value(request.params.clone())
                .map_err(|error| RpcError::invalid(format!("session/prompt: {error}")))?;
            let key = prompt.session_id.0.to_string();
            owned(state, driver, &key)?;
            let text = crate::bridge::prompt_text(&prompt);
            if text.trim().is_empty() {
                return Err(RpcError::invalid("a prompt needs at least one text block"));
            }
            state.drivers().note_prompt(&driver.id);
            let stop = turn(state, driver, &prompt.session_id, text).await?;
            Ok(json!({ "stopReason": stop }))
        }
        "session/cancel" => {
            require(driver, acpilot_proto::bridge::DriverScope::Cancel)?;
            let cancel: CancelNotification = serde_json::from_value(request.params.clone())
                .map_err(|error| RpcError::invalid(format!("session/cancel: {error}")))?;
            let key = cancel.session_id.0.to_string();
            owned(state, driver, &key)?;
            state.sessions().cancel(&key);
            Ok(json!({}))
        }
        "session/answer_permission" => {
            require(
                driver,
                acpilot_proto::bridge::DriverScope::AnswerPermissions,
            )?;
            let request_id = request
                .params
                .get("requestId")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid("`requestId` is missing"))?;
            if let Some(session) = request.params.get("sessionId").and_then(Value::as_str) {
                owned(state, driver, session)?;
            }
            let option = request
                .params
                .get("option")
                .and_then(Value::as_str)
                .and_then(option)
                .ok_or_else(|| {
                    RpcError::invalid("`option` must be allow_once, allow_always or deny")
                })?;
            let broker = state
                .broker()
                .ok_or_else(|| RpcError::internal("this daemon has no permission broker"))?;
            if !broker.answer(request_id, option) {
                return Err(RpcError::internal(
                    "that permission request is no longer pending",
                ));
            }
            Ok(json!({ "accepted": true }))
        }
        other => Err(RpcError::unknown(other)),
    }
}

/// Refuse a call that needs a scope this driver does not hold.
fn require(driver: &Driver, scope: acpilot_proto::bridge::DriverScope) -> Result<(), RpcError> {
    if driver.allows(scope) {
        return Ok(());
    }
    let name = match scope {
        acpilot_proto::bridge::DriverScope::Observe => "observe",
        acpilot_proto::bridge::DriverScope::Prompt => "prompt",
        acpilot_proto::bridge::DriverScope::Cancel => "cancel",
        acpilot_proto::bridge::DriverScope::AnswerPermissions => "answer_permissions",
    };
    Err(RpcError::scope(name))
}

/// Refuse a session this driver did not open.
fn owned(state: &AppState, driver: &Driver, session: &str) -> Result<(), RpcError> {
    if state.sessions().handle(session).is_none() {
        return Err(RpcError::invalid(format!(
            "no session `{session}` in this daemon"
        )));
    }
    if !state.drivers().owns_session(session, &driver.id) {
        return Err(RpcError::invalid(format!(
            "session `{session}` was opened by another driver; this one cannot type into it"
        )));
    }
    Ok(())
}

/// The ACP spelling of a permission option.
fn option(text: &str) -> Option<PermissionOption> {
    match text {
        "allow_once" => Some(PermissionOption::AllowOnce),
        "allow_always" => Some(PermissionOption::AllowAlways),
        "deny" => Some(PermissionOption::Deny),
        _ => None,
    }
}

/// One prompt: ask the session, stream its words, and route its permission questions.
///
/// The response is held until the turn ends, which is what gives the HTTP transport
/// the same shape as stdio: the chunks arrive on the event stream in the meantime,
/// and the permission question either reaches the driver or reaches the person at
/// the machine — never both.
async fn turn(
    state: &AppState,
    driver: &Driver,
    session_id: &SessionId,
    text: String,
) -> Result<StopReason, RpcError> {
    let key = session_id.0.to_string();
    let mut events = state
        .sessions()
        .subscribe(&key)
        .ok_or_else(|| RpcError::invalid(format!("no session `{key}` in this daemon")))?;
    let mut permissions = state.broker().map(|broker| broker.subscribe());
    let mut explained = false;
    let ask = state.sessions().ask_turn(&key, text);
    tokio::pin!(ask);

    let outcome = loop {
        tokio::select! {
            outcome = &mut ask => break outcome,
            event = events.recv() => match event {
                Ok(event) => stream(state, driver, session_id, event),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break Err(ApiError::new(
                    AcpilotError::AgentFailed { message: format!("session {key} closed mid-turn") },
                )),
            },
            request = next_permission(&mut permissions) => {
                if let Some(request) = request
                    && request.session_id == key
                {
                    route_permission(state, driver, session_id, &request, &mut explained);
                }
            }
        }
    };

    // The agent's last notification is dispatched on its own task, so the record can
    // grow for a moment after `ask` returns. The response is the driver's signal to
    // read the end of the stream, so wait for the record to go quiet first — the
    // same trade the stdio bridge measured.
    settle(state, driver, session_id, &mut events).await;

    match outcome {
        Ok(_) => Ok(StopReason::EndTurn),
        Err(error) => Err(RpcError::internal(error.error.to_string())),
    }
}

/// The next permission request, or pending forever when this daemon has no broker.
async fn next_permission(
    receiver: &mut Option<broadcast::Receiver<PermissionRequest>>,
) -> Option<PermissionRequest> {
    match receiver {
        Some(receiver) => loop {
            match receiver.recv().await {
                Ok(request) => return Some(request),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        },
        None => std::future::pending().await,
    }
}

/// What a driver sees of one turn as it happens.
///
/// The same translation the stdio bridge uses ([`crate::acp_update`]): text, thoughts,
/// tool calls, plans, mode changes and notices. Compaction and a changed command list
/// have no ACP spelling in this schema version and are dropped rather than approximated.
fn stream(state: &AppState, driver: &Driver, session_id: &SessionId, event: SessionEvent) {
    let Some(update) = crate::acp_update::update(&event) else {
        return;
    };
    send_update(state, driver, session_id, update);
}

/// Send one `session/update` to a driver's stream.
///
/// The ACP type carries the session id, so it is built from the same values the
/// protocol names rather than a hand-rolled envelope: a driver reading
/// `params.sessionId` and `params.update` sees exactly the schema's shape.
fn send_update(state: &AppState, driver: &Driver, session_id: &SessionId, update: SessionUpdate) {
    let notification = SessionNotification::new(session_id.clone(), update);
    let params = serde_json::to_value(&notification).unwrap_or_default();
    state.drivers().publish(
        &driver.id,
        json!({ "jsonrpc": "2.0", "method": "session/update", "params": params }),
    );
}

/// What a permission question becomes for the driver, or what explains its absence.
fn route_permission(
    state: &AppState,
    driver: &Driver,
    session_id: &SessionId,
    request: &PermissionRequest,
    explained: &mut bool,
) {
    if driver.allows(acpilot_proto::bridge::DriverScope::AnswerPermissions) {
        let message = json!({
            "jsonrpc": "2.0",
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id.0,
                "requestId": request.id,
                "kind": request.kind,
                "title": request.title,
                "detail": request.detail,
                "options": request.options,
            }
        });
        state.drivers().publish(&driver.id, message);
        return;
    }
    if *explained {
        return;
    }
    *explained = true;
    let notice = Notice::new(
        NoticeSeverity::Info,
        "Permission request routed to the machine",
    )
    .description(
        "This driver has no `answer_permissions` grant, so the person at the machine \
         answers the agent's permission requests. The turn waits for them.",
    );
    let message = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id.0,
            "update": serde_json::to_value(SessionUpdate::Notice(notice)).unwrap_or_default(),
        }
    });
    state.drivers().publish(&driver.id, message);
}

/// Forward what the turn recorded after its status settled.
async fn settle(
    state: &AppState,
    driver: &Driver,
    session_id: &SessionId,
    events: &mut broadcast::Receiver<SessionEvent>,
) {
    const QUIET: Duration = Duration::from_millis(100);
    const CAP: Duration = Duration::from_secs(2);
    let deadline = tokio::time::Instant::now() + CAP;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        match tokio::time::timeout(QUIET.min(remaining), events.recv()).await {
            Ok(Ok(event)) => stream(state, driver, session_id, event),
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return,
        }
    }
}

/// `GET /acp/events`: the driver's JSON-RPC notification stream.
async fn events(
    State(state): State<AppState>,
    Extension(driver): Extension<Driver>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let receiver = state.drivers().subscribe();
    let wanted = driver.id;
    let stream = futures::stream::unfold((receiver, wanted), |(mut receiver, wanted)| async move {
        loop {
            match receiver.recv().await {
                Ok(message) if message.driver == wanted => {
                    let data = serde_json::to_string(&message.message).unwrap_or_default();
                    return Some((
                        Ok(Event::default().event("message").data(data)),
                        (receiver, wanted),
                    ));
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// The one failure shape this module's JSON-RPC half produces.
fn failure(id: &Value, error: RpcError) -> Response {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": error.code, "message": error.message },
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::Drivers;

    #[test]
    fn a_driver_token_is_how_a_driver_gets_in() {
        let drivers = Drivers::memory();
        let code = drivers.issue(false, None);
        let paired = drivers
            .pair(&PairDriverRequest {
                code: code.code,
                name: "devin".to_owned(),
                requested_scopes: vec![
                    acpilot_proto::bridge::DriverScope::Observe,
                    acpilot_proto::bridge::DriverScope::AnswerPermissions,
                ],
            })
            .expect("paired");
        assert!(
            !paired
                .driver
                .scopes
                .contains(&acpilot_proto::bridge::DriverScope::AnswerPermissions)
        );
        let driver = drivers.authenticate(&paired.token).expect("authenticated");
        let error = require(
            &driver,
            acpilot_proto::bridge::DriverScope::AnswerPermissions,
        )
        .expect_err("no grant");
        assert_eq!(error.code, -32003);
        assert!(error.message.contains("answer_permissions"), "{error:?}");
    }

    #[test]
    fn the_option_spelling_is_the_one_the_protocol_uses() {
        assert_eq!(option("allow_once"), Some(PermissionOption::AllowOnce));
        assert_eq!(option("allow_always"), Some(PermissionOption::AllowAlways));
        assert_eq!(option("deny"), Some(PermissionOption::Deny));
        assert_eq!(option("yes"), None);
    }
}
