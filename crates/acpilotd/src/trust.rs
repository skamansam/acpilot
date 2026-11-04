//! What a person approved, and the state that says they have not approved it yet.
//!
//! `AgentStatus::Untrusted` means "ran once, but its binary changed since". The
//! daemon cannot invent that state and cannot clear it on its own: the fingerprint
//! has to be recorded when somebody says the new binary is the one they meant, and
//! `POST /v1/agents/{id}/trust` is the only route that writes it.
//!
//! # What a fingerprint is
//!
//! The bytes of the program an agent will run, hashed with SHA-256 — not the command
//! line. An agent whose arguments changed is the same program; one whose binary was
//! replaced under the same path is not, which is exactly the case the state exists
//! for. A program this daemon cannot resolve has no fingerprint, and an agent whose
//! binary is gone is reported as `missing` rather than `untrusted`: there is nothing
//! to approve.
//!
//! # Where the pin lives
//!
//! The store, because a pin a restart forgets is a dialog every start. A daemon with
//! no store refuses the route instead of answering `200` for a promise it cannot
//! keep.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use acpilot_agent::supervisor::availability;
use acpilot_agent::{AgentSpec, CommandSpec};
use acpilot_core::AcpilotError;
use acpilot_proto::agent::{AgentStatus, AgentSummary};
use axum::Json;
use axum::Router;
use axum::extract::{Path as ApiPath, State};
use axum::routing::post;
use sha2::{Digest, Sha256};

use crate::{ApiError, AppState};

/// The pins a person has recorded, by agent id.
///
/// An unreadable pin list is an empty one: status is a convenience, and a database
/// that cannot answer should not stop a client seeing which agents exist.
pub fn pins(state: &AppState) -> HashMap<String, String> {
    let Some(store) = state.store() else {
        return HashMap::new();
    };
    match store.agent_pins() {
        Ok(pins) => pins
            .into_iter()
            .map(|pin| (pin.agent_id, pin.fingerprint))
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "the agent pins could not be read");
            HashMap::new()
        }
    }
}

/// The bytes the agent's command runs, hashed.
///
/// `None` when the agent is not a program (an HTTP agent) or when the program is not
/// on this host: a fingerprint of nothing would be a pin nobody could compare.
#[must_use]
pub fn fingerprint(spec: &AgentSpec) -> Option<String> {
    let command = spec.command()?;
    let path = resolve(&command.program)?;
    let bytes = std::fs::read(path).ok()?;
    Some(hex(&Sha256::digest(bytes)))
}

/// The status a pin implies, on top of what this host knows about the program.
#[must_use]
pub fn status(spec: &AgentSpec, pin: Option<&str>) -> AgentStatus {
    let available = availability(spec);
    if available != AgentStatus::Available {
        return available;
    }
    match pin {
        Some(pin) if fingerprint(spec).as_deref() != Some(pin) => AgentStatus::Untrusted,
        _ => available,
    }
}

/// One agent as the API reports it, with its pin applied.
#[must_use]
pub fn summary(spec: &AgentSpec, pin: Option<&str>) -> AgentSummary {
    AgentSummary {
        id: spec.id.clone(),
        name: spec.name.clone(),
        transport: spec.transport_kind(),
        status: status(spec, pin),
        // An HTTP agent has no command; the empty string is the honest answer
        // rather than a URL in a field that means "what will be spawned".
        command: spec
            .command()
            .map_or_else(String::new, CommandSpec::command_line),
        protocol_version: None,
    }
}

/// The agent routes that are about trust.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/agents/{id}/trust", post(trust))
}

/// Add the route to the served OpenAPI document.
///
/// The document lives in `acpilot-proto` and cannot name a daemon handler without
/// depending on it, so each module inserts its own paths from the handler's metadata.
pub fn register(document: &mut utoipa::openapi::OpenApi) {
    document.paths.paths.insert(
        "/v1/agents/{id}/trust".to_owned(),
        utoipa::openapi::path::PathItem::new(
            utoipa::openapi::path::HttpMethod::Post,
            <__path_trust_doc as utoipa::Path>::operation(),
        ),
    );
}

/// `POST /v1/agents/{id}/trust` — record the binary a person approved.
///
/// The answer is the agent's own summary, so a client that cleared the dialog does
/// not have to list again to find out what it did. The pin is the same fingerprint
/// `GET /v1/agents` compares against, which is what makes this the only way out of
/// `untrusted`.
async fn trust(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<AgentSummary>, ApiError> {
    let registry = state.registry().ok_or_else(|| {
        ApiError::new(AcpilotError::conflict(
            "this daemon has no profile, so it has no agents to trust",
        ))
    })?;
    let spec = registry.get(&id).ok_or_else(|| {
        ApiError::new(AcpilotError::not_found(format!(
            "agent {id} in this profile"
        )))
    })?;
    let store = state.store().ok_or_else(|| {
        ApiError::new(AcpilotError::conflict(
            "this daemon has no store, so a pin recorded now would be forgotten",
        ))
    })?;
    let fingerprint = fingerprint(&spec).ok_or_else(|| {
        ApiError::new(AcpilotError::invalid(format!(
            "agent {id} has no program on this host to approve"
        )))
    })?;
    store.save_agent_pin(&id, &fingerprint).map_err(|error| {
        ApiError::new(AcpilotError::Io(std::io::Error::other(format!(
            "the pin could not be recorded: {error}"
        ))))
    })?;
    Ok(Json(summary(&spec, Some(&fingerprint))))
}

/// OpenAPI registration for the route above.
///
/// The document lives in `acpilot-proto` and cannot name a daemon handler without
/// depending on it, so the path is registered here, from the handler's own metadata.
/// The function body is never called.
#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/agents/{id}/trust",
    tag = "agents",
    summary = "Record that this agent's binary is approved",
    params(("id" = String, Path, description = "Agent id")),
    responses((
        status = 200,
        description = "The agent, now available, with the pin that made it so",
        body = AgentSummary
    ))
)]
fn trust_doc() {}

/// Where a command's program is on this host.
///
/// The lookup mirrors what starting the program does, without a platform branch:
/// `PATH` is searched in order, and the executable suffix is tried where the platform
/// has one (it is empty on unix, so the second candidate is the first one again and
/// the check is skipped).
fn resolve(program: &str) -> Option<PathBuf> {
    let direct = Path::new(program);
    if program.contains('/') || program.contains('\\') {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search).find_map(|directory| {
        let plain = directory.join(program);
        if plain.is_file() {
            return Some(plain);
        }
        let suffix = std::env::consts::EXE_SUFFIX;
        if suffix.is_empty() {
            return None;
        }
        let suffixed = directory.join(format!("{program}{suffix}"));
        suffixed.is_file().then_some(suffixed)
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
