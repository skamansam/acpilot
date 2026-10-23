//! A tiny ACP agent, built as a real process so `Session::spawn` can be tested
//! against a child's pipes rather than an in-memory channel.
//!
//! The in-process mock in `tests/session.rs` proves the handshake and the
//! translation; this one proves the wiring that only exists with a process:
//! two pipes, a supervisor, a dropped child. It is a fixture, not an agent: it
//! has no model and no tools. What it does is decided by the prompt text, so a
//! test needs no environment variables and no arguments:
//!
//! | Prompt | What the agent does |
//! | --- | --- |
//! | `stream <n>` | emits `n` message chunks numbered from 1, then ends the turn |
//! | `permission` | asks the client to allow a shell command, reports the answer, ends the turn |
//! | `fs` | asks the client to read and then write a file, reports each answer, ends the turn |
//! | `terminal` | asks the client to open a terminal, reports the answer, ends the turn |
//! | `hang` | never answers, so the request stays pending |
//! | anything else | echoes the prompt as one chunk |
//!
//! Each answer it hears comes back to the test as a `Notice`, because the
//! client is the only side a test can observe: a fixture that decided the
//! outcome and kept it to itself would be asserting nothing.
//!
//! Requests to the client are sent from a task spawned on the connection, not
//! from inside the handler. Awaiting a request inside an incoming handler
//! deadlocks the dispatch loop, which is documented on `SentRequest::block_task`
//! and is why `permission` asks after the handler has returned.

use std::sync::atomic::{AtomicU32, Ordering};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, CreateTerminalRequest, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, Notice,
    NoticeSeverity, PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse,
    ReadTextFileRequest, RequestPermissionOutcome, RequestPermissionRequest, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCallUpdate,
    ToolCallUpdateFields, WriteTextFileRequest,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Stdio, on_receive_request};

static SESSIONS: AtomicU32 = AtomicU32::new(0);

#[tokio::main]
async fn main() {
    let result = Agent
        .builder()
        .name("acpilot-mock-agent")
        .on_receive_request(
            async |request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        // No filesystem, terminal, or load-session support, so a
                        // test never has to wonder what was advertised.
                        .agent_capabilities(AgentCapabilities::new())
                        .agent_info(Implementation::new("acpilot-mock-agent", "0.0.0")),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |_request: NewSessionRequest, responder, _cx| {
                let id = SESSIONS.fetch_add(1, Ordering::Relaxed) + 1;
                responder.respond(NewSessionResponse::new(format!("acp-{id}")))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |request: PromptRequest, responder, connection: ConnectionTo<Client>| {
                let session = request.session_id.clone();
                let asked = directive(&prompt_text(&request));
                match asked {
                    Directive::Stream(count) => {
                        for index in 1..=count {
                            connection
                                .send_notification(chunk(&session, format!("chunk {index}")))?;
                        }
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    }
                    Directive::Hang => {
                        // Dropping the responder leaves the request pending,
                        // which is what a hung agent looks like.
                        Ok(())
                    }
                    Directive::Permission | Directive::Fs | Directive::Terminal => {
                        let task = connection.clone();
                        connection.spawn(async move {
                            let report = match asked {
                                Directive::Permission => ask_permission(&task, &session).await,
                                Directive::Fs => exercise_files(&task, &session).await,
                                Directive::Terminal => exercise_terminal(&task, &session).await,
                                // `spawn` is only reached for the three above.
                                Directive::Stream(_) | Directive::Hang => Vec::new(),
                            };
                            for line in report {
                                task.send_notification(notice(&session, line))?;
                            }
                            responder.respond(PromptResponse::new(StopReason::EndTurn))
                        })
                    }
                }
            },
            on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await;

    if let Err(error) = result {
        eprintln!("acpilot-mock-agent: {error}");
        std::process::exit(1);
    }
}

/// What a prompt asks for. See the table in the module docs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Directive {
    Stream(usize),
    Permission,
    Fs,
    Terminal,
    Hang,
}

fn directive(text: &str) -> Directive {
    match text.trim() {
        "permission" => Directive::Permission,
        "fs" => Directive::Fs,
        "terminal" => Directive::Terminal,
        "hang" => Directive::Hang,
        other => match other.strip_prefix("stream ") {
            Some(count) => Directive::Stream(count.trim().parse().unwrap_or(1)),
            None => Directive::Stream(1),
        },
    }
}

fn prompt_text(request: &PromptRequest) -> String {
    request
        .prompt
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn chunk(session: &SessionId, text: String) -> SessionNotification {
    SessionNotification::new(
        session.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        )))),
    )
}

fn notice(session: &SessionId, text: String) -> SessionNotification {
    SessionNotification::new(
        session.clone(),
        SessionUpdate::Notice(Notice::new(NoticeSeverity::Info, "mock agent").description(text)),
    )
}

async fn ask_permission(connection: &ConnectionTo<Client>, session: &SessionId) -> Vec<String> {
    let request = RequestPermissionRequest::new(
        session.clone(),
        ToolCallUpdate::new(
            "tool-1",
            ToolCallUpdateFields::new().title("rm -rf ./build"),
        ),
        vec![
            PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
        ],
    );

    match connection.send_request(request).block_task().await {
        Ok(response) => {
            let answer = match response.outcome {
                RequestPermissionOutcome::Selected(selected) => {
                    format!("selected {}", selected.option_id.0)
                }
                RequestPermissionOutcome::Cancelled => "cancelled".to_owned(),
                _ => "unrecognised outcome".to_owned(),
            };
            vec![format!("permission {answer}")]
        }
        Err(error) => vec![format!("permission request failed: {error}")],
    }
}

async fn exercise_files(connection: &ConnectionTo<Client>, session: &SessionId) -> Vec<String> {
    let read = connection
        .send_request(ReadTextFileRequest::new(
            session.clone(),
            std::path::PathBuf::from("/etc/passwd"),
        ))
        .block_task()
        .await;
    let write = connection
        .send_request(WriteTextFileRequest::new(
            session.clone(),
            std::path::PathBuf::from("written-by-the-agent.txt"),
            "hello\n",
        ))
        .block_task()
        .await;

    vec![
        format!("fs/read_text_file {}", outcome(&read)),
        format!("fs/write_text_file {}", outcome(&write)),
    ]
}

async fn exercise_terminal(connection: &ConnectionTo<Client>, session: &SessionId) -> Vec<String> {
    let created = connection
        .send_request(CreateTerminalRequest::new(session.clone(), "/bin/sh"))
        .block_task()
        .await;
    vec![format!("terminal/create {}", outcome(&created))]
}

/// Say what the client answered, in the two words the tests care about.
///
/// The error text is carried too: a test that only checked "refused" would not
/// notice the client refusing with the wrong reason.
fn outcome<T>(result: &Result<T, agent_client_protocol::Error>) -> String {
    match result {
        Ok(_) => "accepted".to_owned(),
        Err(error) => format!("refused: {error}"),
    }
}
