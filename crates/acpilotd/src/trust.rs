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
    program_fingerprint(&command.program)
}

/// The bytes of a program, hashed by the same rule as an agent's.
///
/// Separate from [`fingerprint`] because the pin is about a *program*: a language
/// server is a command a preset names, not a command line an agent declares, and both
/// have to agree byte for byte or approving one would not approve the other.
#[must_use]
pub fn program_fingerprint(program: &str) -> Option<String> {
    let path = resolve(program)?;
    let bytes = std::fs::read(path).ok()?;
    Some(hex(&Sha256::digest(bytes)))
}

/// The language-server pins a person has recorded, by program.
///
/// An unreadable pin list is an empty one, for the reason [`pins`] gives: a database
/// that cannot answer should not stop a client seeing which servers exist.
pub fn lsp_pins(state: &AppState) -> HashMap<String, String> {
    let Some(store) = state.store() else {
        return HashMap::new();
    };
    match store.lsp_server_pins() {
        Ok(pins) => pins
            .into_iter()
            .map(|pin| (pin.program, pin.fingerprint))
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "the language-server pins could not be read");
            HashMap::new()
        }
    }
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
pub fn summary(spec: &AgentSpec, pin: Option<&str>, extension: Option<&str>) -> AgentSummary {
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
        extension: extension.map(str::to_owned),
        protocol_version: None,
        accepts_system_prompt: accepts_system_prompt(spec.command()),
    }
}

/// Whether this program accepts ACP's `systemPrompt`, when the daemon can tell.
///
/// A handshake is the only proof for a general agent, so most rows answer `None`.
/// The recipe agent is the exception: this build ships it, and it resolves skills
/// from its recipe at `session/new` rather than reading the field. Saying so lets
/// a client skip a picker whose answer the agent would ignore.
#[must_use]
pub fn accepts_system_prompt(command: Option<&CommandSpec>) -> Option<bool> {
    let command = command?;
    is_recipe_program(&command.program).then_some(false)
}

/// Whether `program` names the recipe agent this build ships.
///
/// A manifest may write the shipped command bare, as a path to it, or with the
/// platform's executable suffix. All three run the same agent, so all three must
/// answer the same way. Comparing the whole string made the other spellings look
/// like agents that take a per-session selection.
fn is_recipe_program(program: &str) -> bool {
    let Some(name) = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    name == acpilot_agent::promote::COMMAND
        || name == format!("{}.exe", acpilot_agent::promote::COMMAND)
}

/// The agent routes that are about trust.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/agents/{id}/trust", post(trust))
}

/// The language-server route that is about trust.
///
/// Keyed by language rather than by program because a language is what a client lists
/// and what a URL can hold: a program is a path, and a path in a segment is a route
/// that breaks the first time somebody installs a server somewhere with a slash in it.
/// The pin itself is stored against the program, so approving `typescript` also
/// approves `javascript` when one server answers for both.
pub fn lsp_routes() -> Router<AppState> {
    Router::new().route(
        "/v1/lsp/servers/{language}/trust",
        post(trust_language_server),
    )
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
    document.paths.paths.insert(
        "/v1/lsp/servers/{language}/trust".to_owned(),
        utoipa::openapi::path::PathItem::new(
            utoipa::openapi::path::HttpMethod::Post,
            <__path_trust_language_server_doc as utoipa::Path>::operation(),
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
    if state.registry().is_none() && state.profile_root().is_none() {
        return Err(ApiError::new(AcpilotError::conflict(
            "this daemon has no profile, so it has no agents to trust",
        )));
    }
    let (spec, extension) = crate::agent_specs(&state)
        .into_iter()
        .find(|(spec, _)| spec.id == id)
        .ok_or_else(|| {
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
    Ok(Json(summary(
        &spec,
        Some(&fingerprint),
        extension.as_deref(),
    )))
}

/// `POST /v1/lsp/servers/{language}/trust` — record the program a person approved.
///
/// The answer is the row itself, with the new state on it, so a client that cleared
/// the question does not have to list again to find out what it did. A language this
/// build cannot start is `404` rather than a pin recorded for nothing, and a program
/// that is not on this host is `400` for the reason the agent route gives: there is
/// nothing to approve.
async fn trust_language_server(
    State(state): State<AppState>,
    ApiPath(language): ApiPath<String>,
) -> Result<Json<acpilot_proto::lsp::LspServerSummary>, ApiError> {
    state.refresh_language_servers().await;
    let server = state
        .language_servers()
        .status()
        .await
        .into_iter()
        .find(|server| server.language == language)
        .ok_or_else(|| {
            ApiError::new(AcpilotError::not_found(format!(
                "a language server for {language} in this build"
            )))
        })?;
    let store = state.store().ok_or_else(|| {
        ApiError::new(AcpilotError::conflict(
            "this daemon has no store, so a pin recorded now would be forgotten",
        ))
    })?;
    let fingerprint = program_fingerprint(&server.program).ok_or_else(|| {
        ApiError::new(AcpilotError::invalid(format!(
            "the language server for {language} has no program on this host to approve"
        )))
    })?;
    store
        .save_lsp_server_pin(&server.program, &fingerprint)
        .map_err(|error| {
            ApiError::new(AcpilotError::Io(std::io::Error::other(format!(
                "the pin could not be recorded: {error}"
            ))))
        })?;
    Ok(Json(crate::lsp_summary(&server, Some(&fingerprint))))
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

/// OpenAPI registration for the language-server route, from the handler's metadata.
/// The function body is never called.
#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/lsp/servers/{language}/trust",
    tag = "lsp",
    summary = "Record that a language server's program is approved",
    params(("language" = String, Path, description = "LSP language id")),
    responses((
        status = 200,
        description = "The row, now approved, with the pin that made it so",
        body = acpilot_proto::lsp::LspServerSummary
    ))
)]
fn trust_language_server_doc() {}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn command(program: &str) -> CommandSpec {
        CommandSpec {
            program: program.to_owned(),
            args: Vec::new(),
        }
    }

    #[test]
    fn the_recipe_agent_is_the_one_row_that_ignores_the_system_prompt() {
        // The daemon ships this program, so it knows the answer without a
        // handshake. Every other program is only known once it has spoken.
        assert_eq!(
            accepts_system_prompt(Some(&command(acpilot_agent::promote::COMMAND))),
            Some(false)
        );
        assert_eq!(
            accepts_system_prompt(Some(&command("claude-code-acp"))),
            None
        );
        assert_eq!(accepts_system_prompt(None), None);
    }

    #[test]
    fn the_recipe_agent_is_recognised_through_a_path_or_an_executable_suffix() {
        // A manifest may install the shipped binary anywhere, and Windows spells
        // the program with `.exe`. Both are the same agent and the same answer.
        for program in [
            "/opt/acpilot/bin/acpilot-agent-lm",
            "C:/acpilot/bin/acpilot-agent-lm.exe",
        ] {
            assert_eq!(
                accepts_system_prompt(Some(&command(program))),
                Some(false),
                "{program}"
            );
        }
        for program in [
            "/opt/acpilot/bin/acpilot-agent-lm-other",
            "/opt/acpilot/bin/other",
        ] {
            assert_eq!(
                accepts_system_prompt(Some(&command(program))),
                None,
                "{program} is a different program"
            );
        }
    }
}
