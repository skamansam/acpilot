//! The ACP bridge: this daemon as an ACP **agent**, for a local driver.
//!
//! Acpilot is normally the ACP client. It spawns an agent and answers its
//! callbacks. The bridge flips one seat (ADR 0002 §5): a driver connects with
//! `acpilotd --acp`, opens a session here, and prompts the agents that live here.
//! The files, the terminals and the permission broker stay where they are — a
//! driver changes who is typing, not what the machine allows.
//!
//! This module is the **local** half: ACP over stdin/stdout, where the pipes already
//! belong to one process and no token has to prove anything. The remote half — ACP
//! over HTTP/JSON-RPC, pairing codes, scopes and rate limits — is not here yet and
//! `TODO.md` says so; nothing in this file claims otherwise.
//!
//! # Which agent a session runs
//!
//! ACP's `session/new` names a directory, not an agent, and this daemon can serve
//! several. A profile that declares exactly one agent needs no answer. A profile
//! with several is refused with the list, unless the driver names one in the
//! protocol's own extension point: `_meta: { "acpilot": { "agent": "<id>" } }`.
//! Refusing is deliberate — picking the first would quietly run an agent the driver
//! did not ask for.
//!
//! # What a driver sees
//!
//! The assistant's text, chunk by chunk, and the daemon's notices. Everything else
//! in the session record — tool calls, plans, mode changes — is not translated yet,
//! so a driver reads a conversation that is thinner than the TUI's, never one that
//! is wrong. Tool-call updates are the next translation, and they are listed as open
//! on the ticket rather than approximated here.

use std::sync::Arc;

use acpilot_proto::event::{SessionEvent, SessionEventKind};
use acpilot_proto::session::{CreateSessionRequest, SessionSummary};
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, ErrorCode, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, Notice,
    NoticeSeverity, PromptRequest, PromptResponse, SessionId, SessionNotification, SessionUpdate,
    StopReason, TextContent,
};
use agent_client_protocol::{
    Agent, Client, ConnectionTo, Error, Stdio, on_receive_notification, on_receive_request,
};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::AppState;

/// Serve ACP over stdin/stdout, with `state` behind it.
///
/// # Errors
///
/// The transport ended badly — a closed stdin, a client that wrote something which
/// is not JSON-RPC. A turn that fails is answered as a JSON-RPC error and does not
/// end the bridge; one conversation ending is not the endpoint ending.
pub async fn serve_stdio(state: AppState) -> Result<(), Error> {
    let sessions = state.clone();
    let prompting = state.clone();
    let cancelling = state;

    Agent
        .builder()
        .name("acpilotd")
        .on_receive_request(
            async |request: InitializeRequest, responder, _cx| {
                // No `load_session` and no `session/list`: both are capabilities a
                // client draws a feature from, and this bridge does not quietly
                // accept a request it would answer with a lie.
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(AgentCapabilities::new())
                        .agent_info(Implementation::new("acpilotd", env!("CARGO_PKG_VERSION"))),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _cx| {
                match open_session(&sessions, &request).await {
                    Ok(summary) => {
                        let _ =
                            responder.respond(NewSessionResponse::new(SessionId::new(summary.id)));
                    }
                    Err(error) => {
                        let _ = responder.respond_with_error(error);
                    }
                }
                Ok(())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection: ConnectionTo<Client>| {
                let key = request.session_id.0.to_string();
                if prompting.sessions().handle(&key).is_none() {
                    let _ = responder
                        .respond_with_error(failure(format!("no session `{key}` in this daemon")));
                    return Ok(());
                }
                let text = prompt_text(&request);
                if text.trim().is_empty() {
                    let _ = responder
                        .respond_with_error(failure("a prompt needs at least one text block"));
                    return Ok(());
                }

                // The turn runs on the connection rather than inside this handler:
                // a handler that awaited the agent would stop dispatching the
                // `session/cancel` that is supposed to interrupt it. The same
                // reason `acpilot-agent-lm` spawns its turn.
                let state = prompting.clone();
                let session_id = request.session_id.clone();
                let task = connection.clone();
                let _ = connection.spawn(async move {
                    match turn(&state, &session_id, &task, text).await {
                        Ok(()) => responder.respond(PromptResponse::new(StopReason::EndTurn)),
                        Err(error) => responder.respond_with_error(error),
                    }
                });
                Ok(())
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _cx| {
                cancelling.sessions().cancel(&notification.session_id.0);
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

/// Open the session a driver asked for.
///
/// The directory has to be inside the workspace this daemon serves: an ACP
/// `session/new` is a promise that the agent sees that directory, and a bridge that
/// accepted one elsewhere would break the promise silently.
async fn open_session(
    state: &AppState,
    request: &NewSessionRequest,
) -> Result<SessionSummary, Error> {
    let workspace = state
        .workspace()
        .ok_or_else(|| failure("this daemon has no workspace to open sessions in"))?;
    if !request.cwd.starts_with(&workspace.root) {
        return Err(failure(format!(
            "`{}` is outside the workspace this daemon serves ({})",
            request.cwd.display(),
            workspace.root.display()
        )));
    }

    crate::sessions::create(
        state,
        CreateSessionRequest {
            // Empty means "the workspace this daemon serves", which is the one
            // `cwd` was just checked against.
            workspace_id: String::new(),
            agent_id: choose_agent(state, request)?,
            title: None,
        },
    )
    .await
    .map_err(|error| Error::new(i32::from(ErrorCode::InternalError), error.error.to_string()))
}

/// Which agent the new session runs.
///
/// One declared agent is the answer. Several need the driver to say which, through
/// `_meta.acpilot.agent`; a wrong name lists what is there rather than starting
/// something else.
fn choose_agent(state: &AppState, request: &NewSessionRequest) -> Result<String, Error> {
    let registry = state.registry().ok_or_else(|| {
        failure("this daemon was started without a profile, so it has no agents to start")
    })?;
    let agents = registry.agents();
    if agents.is_empty() {
        return Err(failure(
            "this profile declares no agents; promote a recipe or add one to agents.toml",
        ));
    }

    if let Some(named) = named_agent(request) {
        return if agents.iter().any(|agent| agent.id == named) {
            Ok(named)
        } else {
            Err(failure(format!(
                "`{named}` is not an agent of this profile; it declares {}",
                list(&agents)
            )))
        };
    }

    match agents.as_slice() {
        [only] => Ok(only.id.clone()),
        several => Err(failure(format!(
            "this profile declares {} agents ({}); name one with `_meta.acpilot.agent`",
            several.len(),
            list(&agents)
        ))),
    }
}

/// The agent a driver named in the protocol's extension point, if any.
///
/// Read from the request's JSON rather than a typed field because `_meta` is
/// ACP's shape for "something the protocol does not define", and acpilot's name
/// under it is an acpilot convention, not a protocol type.
fn named_agent(request: &NewSessionRequest) -> Option<String> {
    serde_json::to_value(request)
        .ok()?
        .pointer("/_meta/acpilot/agent")?
        .as_str()
        .map(str::to_owned)
}

/// The ids a refusal names, in declaration order.
fn list(agents: &[Arc<acpilot_agent::AgentSpec>]) -> String {
    agents
        .iter()
        .map(|agent| agent.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// One prompt: ask the session, and forward what it says while it says it.
///
/// Two readers, one turn: `Sessions::ask` collects the answer this responds with,
/// and this loop's own subscription turns the same events into `session/update`
/// notifications as they arrive, so the driver is not silent until the end.
async fn turn(
    state: &AppState,
    session_id: &SessionId,
    connection: &ConnectionTo<Client>,
    text: String,
) -> Result<(), Error> {
    let key = session_id.0.to_string();
    let mut events = state
        .sessions()
        .subscribe(&key)
        .ok_or_else(|| failure(format!("no session `{key}` in this daemon")))?;
    let ask = state.sessions().ask(&key, text);
    tokio::pin!(ask);
    let mut open = true;

    let outcome = loop {
        tokio::select! {
            outcome = &mut ask => break outcome,
            event = events.recv(), if open => match event {
                Ok(event) => forward(connection, session_id, event)?,
                // A chatty turn can outrun the ring; the missing updates are a
                // hole in the stream, not a reason to stop the turn. `ask` still
                // holds the full answer.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => open = false,
            },
        }
    };
    if outcome.is_ok() {
        settle(connection, session_id, &mut events, open).await?;
    }
    outcome
        .map(|_| ())
        .map_err(|error| Error::new(i32::from(ErrorCode::InternalError), error.error.to_string()))
}

/// Forward what the turn recorded after its status settled.
///
/// The daemon marks a turn idle as soon as the agent answers, and the agent's
/// last notifications are dispatched on another task, so the record can grow for a
/// moment after `ask` returns. The response is the driver's signal to stop reading;
/// a chunk after it is a word the driver never sees. So this waits for the record to
/// go quiet — 100 ms with nothing, capped at two seconds, so a stream that never
/// stops cannot hold the answer forever.
///
/// Measured against a real client rather than assumed: without this, a streamed
/// turn lost its last chunk about one run in six, and only under parallel load.
async fn settle(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    events: &mut broadcast::Receiver<SessionEvent>,
    mut open: bool,
) -> Result<(), Error> {
    const QUIET: std::time::Duration = std::time::Duration::from_millis(100);
    const CAP: std::time::Duration = std::time::Duration::from_secs(2);

    let deadline = tokio::time::Instant::now() + CAP;
    while open {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        match tokio::time::timeout(QUIET.min(remaining), events.recv()).await {
            Ok(Ok(event)) => forward(connection, session_id, event)?,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) => open = false,
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

/// One recorded event, as the driver's protocol spells it.
///
/// Only what this bridge can say exactly: assistant text and the daemon's notices.
/// Every other kind is dropped rather than translated into something it is not.
fn forward(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    event: SessionEvent,
) -> Result<(), Error> {
    let update = match event.kind {
        SessionEventKind::MessageChunk => {
            // The driver sent the prompt; echoing it back as an assistant chunk
            // would make it read as if the agent had said its own words.
            if event.payload.get("role").and_then(Value::as_str) != Some("assistant") {
                return Ok(());
            }
            let Some(text) = event
                .payload
                .get("content")
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
            else {
                return Ok(());
            };
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            )))
        }
        SessionEventKind::Notice => {
            let title = event
                .payload
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Acpilot");
            let description = event
                .payload
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            SessionUpdate::Notice(
                Notice::new(NoticeSeverity::Info, title).description(description.to_owned()),
            )
        }
        _ => return Ok(()),
    };
    connection.send_notification(SessionNotification::new(session_id.clone(), update))
}

/// A prompt as one string.
///
/// ACP carries a list of content blocks; the session engine takes text. Joining
/// text blocks with a newline is exact for the case the bridge supports, and an
/// image or audio block is dropped rather than rendered into a sentence about it.
fn prompt_text(request: &PromptRequest) -> String {
    request
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The one failure shape this module produces.
fn failure(message: impl Into<String>) -> Error {
    Error::new(i32::from(ErrorCode::InvalidRequest), message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_agent::{MapResolver, Registry};
    use std::path::Path;

    fn manifest(agents: &str) -> (tempfile::TempDir, Registry) {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("agents.toml");
        std::fs::write(&path, format!("schema_version = 1\n\n{agents}")).expect("the manifest");
        let registry = Registry::load(&path, Arc::new(MapResolver::new())).expect("a registry");
        (directory, registry)
    }

    fn entry(id: &str) -> String {
        format!(
            "[[agents]]\nid = \"{id}\"\nname = \"{id}\"\ntransport = \"acp-stdio\"\n\
             command = \"true\"\n"
        )
    }

    fn request(cwd: &Path) -> NewSessionRequest {
        NewSessionRequest::new(cwd)
    }

    fn state(registry: Registry, workspace: &Path) -> AppState {
        AppState::new()
            .with_registry(Arc::new(registry))
            .with_workspace(crate::Workspace {
                id: "work".into(),
                profile: String::new(),
                root: workspace.to_path_buf(),
            })
    }

    #[test]
    fn one_declared_agent_needs_no_naming() {
        let (directory, registry) = manifest(&entry("mock"));
        let state = state(registry, directory.path());
        assert_eq!(
            choose_agent(&state, &request(directory.path())).expect("the only agent"),
            "mock"
        );
    }

    #[test]
    fn several_agents_are_refused_with_the_list_and_the_way_to_choose() {
        let (directory, registry) = manifest(&format!("{}{}", entry("one"), entry("two")));
        let state = state(registry, directory.path());
        let error = choose_agent(&state, &request(directory.path())).expect_err("ambiguous");
        let message = error.to_string();
        assert!(message.contains("one, two"), "{message}");
        assert!(message.contains("_meta.acpilot.agent"), "{message}");
    }

    #[test]
    fn a_directory_outside_the_workspace_is_refused_before_an_agent_is_chosen() {
        let (directory, registry) = manifest(&entry("mock"));
        let state = state(registry, &directory.path().join("workspace"));
        let outside = tempfile::tempdir().expect("temp dir");
        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(open_session(&state, &request(outside.path())))
            .expect_err("outside the workspace");
        assert!(
            error.to_string().contains("outside the workspace"),
            "{error}"
        );
    }

    #[test]
    fn an_extension_agent_name_is_read_from_the_request_and_a_wrong_one_is_refused() {
        use agent_client_protocol::schema::v1::ImageContent;

        let (directory, registry) = manifest(&format!("{}{}", entry("one"), entry("two")));
        let state = state(registry, directory.path());

        // ACP carries the name in `_meta`, which the schema keeps as free JSON.
        // Round-tripping the request is the client's half of the same encoding.
        let named: NewSessionRequest = serde_json::from_value(serde_json::json!({
            "cwd": directory.path(),
            "mcpServers": [],
            "_meta": { "acpilot": { "agent": "two" } }
        }))
        .expect("the request parses");
        assert_eq!(named_agent(&named).as_deref(), Some("two"));
        assert_eq!(
            choose_agent(&state, &named).expect("the named agent"),
            "two"
        );

        let wrong: NewSessionRequest = serde_json::from_value(serde_json::json!({
            "cwd": directory.path(),
            "mcpServers": [],
            "_meta": { "acpilot": { "agent": "three" } }
        }))
        .expect("the request parses");
        let message = choose_agent(&state, &wrong)
            .expect_err("no such agent")
            .to_string();
        assert!(message.contains("one, two"), "{message}");

        // The image is dropped rather than described in words the agent would
        // then act on.
        let request = PromptRequest::new(
            SessionId::new("s".to_owned()),
            vec![
                ContentBlock::Text(TextContent::new("one")),
                ContentBlock::Image(ImageContent::new("image/png", "")),
                ContentBlock::Text(TextContent::new("two")),
            ],
        );
        assert_eq!(prompt_text(&request), "one\ntwo");
    }

    #[test]
    fn a_prompt_is_the_text_blocks_and_not_an_image_rendered_as_words() {
        use agent_client_protocol::schema::v1::ImageContent;

        let request = PromptRequest::new(
            SessionId::new("s".to_owned()),
            vec![
                ContentBlock::Text(TextContent::new("one")),
                ContentBlock::Image(ImageContent::new("image/png", "")),
                ContentBlock::Text(TextContent::new("two")),
            ],
        );
        assert_eq!(prompt_text(&request), "one\ntwo");
    }
}
