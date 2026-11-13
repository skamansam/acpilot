//! The model-backed ACP agent: a recipe becomes a process that reads files,
//! writes them and runs commands — but only through the client that spawned it.
//!
//! The crate holds no filesystem handle for the workspace. A recipe is read at
//! startup, and after that every fact about the world arrives through an ACP
//! request (`fs/read_text_file`, `fs/write_text_file`, `terminal/*`,
//! `session/request_permission`). That is why it is a separate binary rather than
//! a daemon module: promotion (P4.5-9) writes it into `agents.toml` as an
//! ordinary `acp-stdio` agent, and the permission broker it already has applies
//! without a second implementation.
//!
//! Three refusals shape the loop:
//!
//! - A tool call that is not valid JSON gets **one** repair request. A second
//!   failure is a `Notice` and a failed tool call, never a guessed argument.
//! - `max_steps` ends the turn with `MaxTurnRequests`, and the notice says how
//!   many steps ran.
//! - A cancel is racing the HTTP request itself, so the connection is dropped
//!   when it arrives rather than after the answer is read.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use acpilot_agent::{KeyringResolver, SecretRef, SecretResolver};
use acpilot_model::context::estimate_tokens;
use acpilot_model::{
    Backend, ChatMessage, ChatRequest, ChatResponse, Provider, Role, ToolCall, Usage,
};
use acpilot_profile::Profile;
use acpilot_profile::providers::ProvidersFile;
use acpilot_profile::recipes::Recipes;
use acpilot_proto::model::ModelRecipe;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ContentChunk, CreateTerminalRequest, ErrorCode,
    NewSessionRequest, Notice, NoticeSeverity, PermissionOption, PermissionOptionKind,
    PromptRequest, PromptResponse, ReadTextFileRequest, ReleaseTerminalRequest,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionNotification,
    SessionUpdate, StopReason, TerminalOutputRequest, TextContent, ToolCall as AcpToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
    WaitForTerminalExitRequest, WriteTextFileRequest,
};
use agent_client_protocol::{Client, ConnectionTo};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Notify;

pub mod tools;

pub use tools::{ParseError, SEARCH_UNAVAILABLE, ToolInvocation};

/// Why a recipe could not become a running agent.
#[derive(Debug, Error)]
pub enum AgentError {
    /// No profile was named and none was in the environment.
    #[error("no profile: pass --profile or set ACPILOT_RECIPE_PROFILE")]
    NoProfile,

    /// The profile directory is not usable.
    #[error("profile `{profile}`: {message}")]
    Profile {
        /// Name that was resolved.
        profile: String,
        /// Why it failed.
        message: String,
    },

    /// `providers.toml` could not be read.
    #[error("providers.toml: {0}")]
    Providers(String),

    /// The recipe directory could not be read or a file failed its rules.
    #[error("recipes: {0}")]
    Recipes(String),

    /// The recipe id is not in the profile.
    #[error("recipe `{id}` is not in this profile's recipes directory")]
    UnknownRecipe {
        /// The id that was asked for.
        id: String,
    },

    /// The recipe names a provider that is missing or disabled.
    #[error("recipe `{recipe}` names provider `{provider}`, which {why}")]
    ProviderRefused {
        /// Recipe id.
        recipe: String,
        /// Provider id.
        provider: String,
        /// `is not declared` or `is declared and disabled`.
        why: &'static str,
    },

    /// The keyring reference could not be resolved.
    #[error("credential `{reference}`: {message}")]
    Credential {
        /// Reference as written.
        reference: String,
        /// Why it failed.
        message: String,
    },
}

/// One recipe and the backend that serves it.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The validated recipe.
    pub recipe: ModelRecipe,
    /// The adapter for its provider.
    pub backend: Backend,
}

/// Resolve a profile, a recipe and the provider behind it.
///
/// The token is read once, at startup. A missing keyring entry is a refusal
/// before the connection is announced, not a 401 in the middle of a turn.
///
/// # Errors
///
/// [`AgentError`] names the file or the reference that failed.
pub fn load(profile_name: &str, recipe_id: &str) -> Result<Loaded, AgentError> {
    let profile = Profile::named(profile_name).map_err(|error| AgentError::Profile {
        profile: profile_name.to_owned(),
        message: error.to_string(),
    })?;
    let providers = ProvidersFile::load_in(profile.root())
        .map_err(|error| AgentError::Providers(error.to_string()))?;
    let recipes = Recipes::load_in(&profile.recipes_dir(), &providers, &[])
        .map_err(|error| AgentError::Recipes(error.to_string()))?;
    let recipe = recipes
        .get(recipe_id)
        .cloned()
        .ok_or_else(|| AgentError::UnknownRecipe {
            id: recipe_id.to_owned(),
        })?;

    let declaration =
        providers
            .find(&recipe.provider_id)
            .ok_or_else(|| AgentError::ProviderRefused {
                recipe: recipe.id.clone(),
                provider: recipe.provider_id.clone(),
                why: "is not declared",
            })?;
    if !declaration.enabled {
        return Err(AgentError::ProviderRefused {
            recipe: recipe.id.clone(),
            provider: recipe.provider_id.clone(),
            why: "is declared and disabled",
        });
    }

    let token = match &declaration.credential_ref {
        None => None,
        Some(reference) => {
            let secret = SecretRef::parse(reference).map_err(|error| AgentError::Credential {
                reference: reference.clone(),
                message: error.to_string(),
            })?;
            let resolver = KeyringResolver::new(profile_name);
            let value = resolver
                .resolve(&secret)
                .map_err(|error| AgentError::Credential {
                    reference: reference.clone(),
                    message: error.to_string(),
                })?;
            Some(value.expose().to_owned())
        }
    };

    let provider = Provider::new(
        declaration.id.clone(),
        declaration.kind,
        declaration.base_url.clone(),
    )
    .with_token(token);
    let backend = Backend::new(provider).map_err(|error| AgentError::Profile {
        profile: profile_name.to_owned(),
        message: error.to_string(),
    })?;

    Ok(Loaded { recipe, backend })
}

/// One session's state: where it starts, what was said, and whether it was
/// cancelled.
#[derive(Debug)]
pub struct SessionState {
    /// The workspace the client named at `session/new`.
    pub cwd: PathBuf,
    history: Mutex<Vec<ChatMessage>>,
    cancel: AtomicBool,
    wake: Notify,
}

impl SessionState {
    /// A session rooted at `cwd`.
    #[must_use]
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            history: Mutex::new(Vec::new()),
            cancel: AtomicBool::new(false),
            wake: Notify::new(),
        }
    }

    /// Ask the running turn to stop, and wake it if it is waiting on the model.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    /// Add the user's message to the conversation.
    pub fn push_user(&self, text: String) {
        self.history
            .lock()
            .expect("history lock")
            .push(ChatMessage {
                role: Role::User,
                content: text,
            });
    }

    fn snapshot(&self) -> Vec<ChatMessage> {
        self.history.lock().expect("history lock").clone()
    }

    fn push(&self, message: ChatMessage) {
        self.history.lock().expect("history lock").push(message);
    }

    /// The conversation as it stands, for a test or a log line.
    #[must_use]
    pub fn history(&self) -> Vec<ChatMessage> {
        self.snapshot()
    }
}

/// What one turn did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnResult {
    /// Why it stopped, as ACP spells it.
    pub stop: StopReason,
    /// Steps that ran a tool call.
    pub steps: u32,
    /// The last counts the provider reported.
    pub usage: Usage,
}

/// Run one prompt to completion.
///
/// The caller has already put the user's message into the session.
///
/// # Errors
///
/// An ACP notification failed to send, or the model server could not be reached.
pub async fn turn(
    session: &SessionState,
    session_id: &SessionId,
    loaded: &Loaded,
    connection: &ConnectionTo<Client>,
) -> Result<TurnResult, agent_client_protocol::Error> {
    session.cancel.store(false, Ordering::SeqCst);
    let recipe = &loaded.recipe;

    if recipe.tools.search {
        notice(
            connection,
            session_id,
            NoticeSeverity::Info,
            "Search is not offered",
            SEARCH_UNAVAILABLE,
        )?;
    }

    let mut steps: u32 = 0;
    let mut usage = Usage::default();

    loop {
        if session.cancel.load(Ordering::SeqCst) {
            return Ok(TurnResult {
                stop: StopReason::Cancelled,
                steps,
                usage,
            });
        }

        {
            let mut history = session.history.lock().expect("history lock");
            let dropped = trim_history(
                &mut history,
                recipe.context.window,
                recipe.context.reserve_output,
            );
            drop(history);
            if dropped > 0 {
                notice(
                    connection,
                    session_id,
                    NoticeSeverity::Warning,
                    "Older turns dropped",
                    &format!(
                        "{dropped} messages were dropped so the conversation fits the {}-token window",
                        recipe.context.window
                    ),
                )?;
            }
        }

        let request = ChatRequest {
            model: recipe.model_id.clone(),
            system_prompt: Some(recipe.system_prompt.clone()),
            messages: session.snapshot(),
            params: recipe.sampling.clone(),
            tools: tools::schema(&recipe.tools),
            json_schema: None,
        };

        let answer = tokio::select! {
            biased;
            () = session.wake.notified() => {
                session.cancel.store(true, Ordering::SeqCst);
                return Ok(TurnResult { stop: StopReason::Cancelled, steps, usage });
            }
            result = loaded.backend.chat(&request) => result.map_err(|error| {
                agent_client_protocol::Error::new(
                    i32::from(ErrorCode::InternalError),
                    error.to_string(),
                )
            })?,
        };
        usage = answer.usage;

        if !answer.content.is_empty() {
            chunk(connection, session_id, &answer.content)?;
        }

        let used =
            u64::from(answer.usage.prompt_tokens) + u64::from(answer.usage.completion_tokens);
        connection.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::UsageUpdate(UsageUpdate::new(used, u64::from(recipe.context.window))),
        ))?;

        if answer.tool_calls.is_empty() {
            session.push(ChatMessage {
                role: Role::Assistant,
                content: answer.content.clone(),
            });
            return Ok(TurnResult {
                stop: stop_reason(&answer),
                steps,
                usage,
            });
        }

        // The model has to see its own call before it sees the result, or the
        // next request reads like a tool answer to nothing.
        session.push(ChatMessage {
            role: Role::Assistant,
            content: call_summary(&answer),
        });

        for (index, call) in answer.tool_calls.iter().enumerate() {
            if session.cancel.load(Ordering::SeqCst) {
                return Ok(TurnResult {
                    stop: StopReason::Cancelled,
                    steps,
                    usage,
                });
            }
            if steps >= recipe.tools.max_steps {
                notice(
                    connection,
                    session_id,
                    NoticeSeverity::Warning,
                    "Step limit reached",
                    &format!(
                        "the turn stopped after {} steps because the recipe allows {}; raise `tools.max_steps` to allow more",
                        steps, recipe.tools.max_steps
                    ),
                )?;
                return Ok(TurnResult {
                    stop: StopReason::MaxTurnRequests,
                    steps,
                    usage,
                });
            }
            steps += 1;

            let call_id = format!("{session_id}-call-{steps}-{index}");
            let result = run_call(
                session, session_id, loaded, connection, &call_id, call, &request,
            )
            .await?;
            session.push(ChatMessage {
                role: Role::Tool,
                content: format!("{}: {result}", call.name),
            });
        }
    }
}

/// Turn one model tool call into a client request, with one repair attempt.
async fn run_call(
    session: &SessionState,
    session_id: &SessionId,
    loaded: &Loaded,
    connection: &ConnectionTo<Client>,
    call_id: &str,
    call: &ToolCall,
    base: &ChatRequest,
) -> Result<String, agent_client_protocol::Error> {
    let raw_input = serde_json::from_str::<Value>(&call.arguments).ok();
    let exposure = &loaded.recipe.tools;

    let invocation = match tools::parse_call(&call.name, &call.arguments, exposure) {
        Ok(invocation) => invocation,
        Err(first) => {
            notice(
                connection,
                session_id,
                NoticeSeverity::Warning,
                "Tool call repaired",
                &format!("`{}` was refused once: {first}", call.name),
            )?;
            let repaired = repair(session, loaded, connection, base, call, &first).await?;
            if repaired.is_err() {
                let failed = repaired.expect_err("the branch above is an error");
                notice(
                    connection,
                    session_id,
                    NoticeSeverity::Error,
                    "Tool call dropped",
                    &format!("`{}` still did not parse: {failed}", call.name),
                )?;
                announce(
                    connection,
                    session_id,
                    CallState {
                        id: call_id,
                        title: &format!("Refused {}", call.name),
                        kind: ToolKind::Other,
                        status: ToolCallStatus::Failed,
                    },
                    raw_input.clone(),
                    Some(Value::String(failed.to_string())),
                )?;
                return Ok(format!("refused: {failed}"));
            }
            repaired.expect("the branch above is a success")
        }
    };

    announce(
        connection,
        session_id,
        CallState {
            id: call_id,
            title: &invocation.title(),
            kind: kind(&invocation),
            status: ToolCallStatus::InProgress,
        },
        raw_input,
        None,
    )?;

    let (output, ok) = execute(session, session_id, connection, call_id, &invocation).await?;
    announce(
        connection,
        session_id,
        CallState {
            id: call_id,
            title: &invocation.title(),
            kind: kind(&invocation),
            status: if ok {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            },
        },
        None,
        Some(Value::String(output.clone())),
    )?;
    Ok(output)
}

/// Ask the model once to re-emit a malformed call.
async fn repair(
    session: &SessionState,
    loaded: &Loaded,
    connection: &ConnectionTo<Client>,
    base: &ChatRequest,
    call: &ToolCall,
    error: &ParseError,
) -> Result<Result<ToolInvocation, ParseError>, agent_client_protocol::Error> {
    let mut messages = base.messages.clone();
    messages.push(ChatMessage {
        role: Role::Assistant,
        content: format!("Called `{}` with arguments {}", call.name, call.arguments),
    });
    messages.push(ChatMessage {
        role: Role::User,
        content: tools::repair_message(&call.name, &call.arguments, error),
    });

    let request = ChatRequest {
        messages,
        ..base.clone()
    };
    let answer = tokio::select! {
        biased;
        () = session.wake.notified() => {
            session.cancel.store(true, Ordering::SeqCst);
            return Ok(Err(ParseError::NotJson("the turn was cancelled".to_owned())));
        }
        result = loaded.backend.chat(&request) => result.map_err(|error| {
            agent_client_protocol::Error::new(
                i32::from(ErrorCode::InternalError),
                error.to_string(),
            )
        })?,
    };
    let _ = connection;
    match answer.tool_calls.first() {
        Some(repaired) => Ok(tools::parse_call(
            &repaired.name,
            &repaired.arguments,
            &loaded.recipe.tools,
        )),
        None => Ok(Err(ParseError::Schema(
            "the repair produced no tool call".to_owned(),
        ))),
    }
}

/// Run a parsed invocation through the client.
async fn execute(
    session: &SessionState,
    session_id: &SessionId,
    connection: &ConnectionTo<Client>,
    call_id: &str,
    invocation: &ToolInvocation,
) -> Result<(String, bool), agent_client_protocol::Error> {
    match invocation {
        ToolInvocation::ReadFile { path, line, limit } => {
            let request = ReadTextFileRequest::new(session_id.clone(), path)
                .line(*line)
                .limit(*limit);
            match connection.send_request(request).block_task().await {
                Ok(response) => Ok((response.content, true)),
                Err(error) => Ok((format!("the client refused: {error}"), false)),
            }
        }
        ToolInvocation::WriteFile { path, content } => {
            if !permitted(
                connection,
                session_id,
                call_id,
                &invocation.title(),
                ToolKind::Edit,
                None,
            )
            .await?
            {
                return Ok(("the user denied the write".to_owned(), false));
            }
            let request = WriteTextFileRequest::new(session_id.clone(), path, content);
            match connection.send_request(request).block_task().await {
                Ok(_) => Ok(("written".to_owned(), true)),
                Err(error) => Ok((format!("the client refused: {error}"), false)),
            }
        }
        ToolInvocation::RunCommand { command, args } => {
            let input = serde_json::json!({ "command": command, "args": args });
            if !permitted(
                connection,
                session_id,
                call_id,
                &invocation.title(),
                ToolKind::Execute,
                Some(input),
            )
            .await?
            {
                return Ok(("the user denied the command".to_owned(), false));
            }
            let request = CreateTerminalRequest::new(session_id.clone(), command)
                .args(args.clone())
                .cwd(Some(session.cwd.clone()));
            let created = match connection.send_request(request).block_task().await {
                Ok(response) => response,
                Err(error) => {
                    return Ok((format!("the client refused a terminal: {error}"), false));
                }
            };
            let terminal = created.terminal_id;
            let exit = connection
                .send_request(WaitForTerminalExitRequest::new(
                    session_id.clone(),
                    terminal.clone(),
                ))
                .block_task()
                .await;
            let output = connection
                .send_request(TerminalOutputRequest::new(
                    session_id.clone(),
                    terminal.clone(),
                ))
                .block_task()
                .await;
            let _ = connection
                .send_request(ReleaseTerminalRequest::new(session_id.clone(), terminal))
                .block_task()
                .await;

            let text = match output {
                Ok(response) => response.output,
                Err(error) => return Ok((format!("no output: {error}"), false)),
            };
            match exit {
                Ok(response) => {
                    let code = response
                        .exit_status
                        .exit_code
                        .map_or_else(|| "no exit code".to_owned(), |code| code.to_string());
                    Ok((format!("exit {code}\n{text}"), true))
                }
                Err(error) => Ok((format!("no exit status: {error}\n{text}"), false)),
            }
        }
    }
}

/// Ask the client's permission broker before a write or a command.
async fn permitted(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    call_id: &str,
    title: &str,
    kind: ToolKind,
    input: Option<Value>,
) -> Result<bool, agent_client_protocol::Error> {
    let fields = ToolCallUpdateFields::new()
        .title(title)
        .kind(kind)
        .raw_input(input);
    let request = RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(call_id.to_owned(), fields),
        vec![
            PermissionOption::new("allow", "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
        ],
    );
    match connection.send_request(request).block_task().await {
        Ok(response) => Ok(match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                selected.option_id.0.as_ref() == "allow"
            }
            _ => false,
        }),
        // A client that cannot answer is treated as a refusal: a broken channel
        // is not permission.
        Err(_) => Ok(false),
    }
}

/// The parts of a tool-call announcement that do not change between its start
/// and its result.
struct CallState<'a> {
    id: &'a str,
    title: &'a str,
    kind: ToolKind,
    status: ToolCallStatus,
}

/// Send or update a tool call on the session stream.
fn announce(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    call: CallState<'_>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
) -> Result<(), agent_client_protocol::Error> {
    let update = if raw_input.is_some() {
        SessionUpdate::ToolCall(
            AcpToolCall::new(call.id.to_owned(), call.title)
                .kind(call.kind)
                .status(call.status)
                .raw_input(raw_input),
        )
    } else {
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            call.id.to_owned(),
            ToolCallUpdateFields::new()
                .status(call.status)
                .raw_output(raw_output),
        ))
    };
    connection.send_notification(SessionNotification::new(session_id.clone(), update))
}

/// The ACP tool kind for an invocation, so a client draws the right glyph.
fn kind(invocation: &ToolInvocation) -> ToolKind {
    match invocation {
        ToolInvocation::ReadFile { .. } => ToolKind::Read,
        ToolInvocation::WriteFile { .. } => ToolKind::Edit,
        ToolInvocation::RunCommand { .. } => ToolKind::Execute,
    }
}

fn chunk(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    text: &str,
) -> Result<(), agent_client_protocol::Error> {
    connection.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        )))),
    ))
}

fn notice(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    severity: NoticeSeverity,
    title: &str,
    description: &str,
) -> Result<(), agent_client_protocol::Error> {
    connection.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::Notice(Notice::new(severity, title).description(description.to_owned())),
    ))?;
    Ok(())
}

/// The stop reason ACP should report for a final answer.
fn stop_reason(answer: &ChatResponse) -> StopReason {
    match answer.stop_reason.as_deref() {
        Some("length" | "max_tokens") => StopReason::MaxTokens,
        Some("content_filter" | "refusal") => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

/// What the assistant called, in the form the model reads back.
fn call_summary(answer: &ChatResponse) -> String {
    let mut text = answer.content.clone();
    for call in &answer.tool_calls {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!(
            "Called `{}` with arguments {}",
            call.name, call.arguments
        ));
    }
    text
}

/// Drop whole messages from the front until the conversation fits.
///
/// A dropped tail may not start with a tool result or an assistant turn: either
/// reads as an answer to a question that is no longer there. Whole pairs go.
fn trim_history(history: &mut Vec<ChatMessage>, window: u32, reserve: u32) -> u32 {
    let room = window.saturating_sub(reserve);
    let mut dropped = 0u32;
    loop {
        let used: u32 = history
            .iter()
            .map(|message| estimate_tokens(message.content.len()))
            .sum();
        if used <= room || history.is_empty() {
            return dropped;
        }
        history.remove(0);
        dropped += 1;
        while history
            .first()
            .is_some_and(|message| message.role != Role::User)
        {
            history.remove(0);
            dropped += 1;
        }
    }
}

/// The prompt text from an ACP prompt, ignoring non-text blocks.
#[must_use]
pub fn prompt_text(request: &PromptRequest) -> String {
    request
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The session id a client asked for, or a generated one.
#[must_use]
pub fn new_session_id(sequence: u64) -> String {
    format!("lm-{sequence}")
}

/// Everything the `session/new` handler needs, so `main` stays wiring.
#[must_use]
pub fn new_session(request: &NewSessionRequest, sequence: u64) -> (String, SessionState) {
    let id = new_session_id(sequence);
    (id, SessionState::new(request.cwd.clone()))
}

/// An ACP prompt response with the counts attached where a client can read them.
#[must_use]
pub fn prompt_response(result: &TurnResult) -> PromptResponse {
    PromptResponse::new(result.stop).meta(Some(serde_json::Map::from_iter([(
        "acpilot".to_owned(),
        serde_json::json!({
            "steps": result.steps,
            "prompt_tokens": result.usage.prompt_tokens,
            "completion_tokens": result.usage.completion_tokens,
            "reported": result.usage.reported,
        }),
    )])))
}

/// A cancel notification names the session it stops.
#[must_use]
pub fn cancelled_session(notification: &CancelNotification) -> &str {
    notification.session_id.0.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: Role, words: usize) -> ChatMessage {
        ChatMessage {
            role,
            content: "x".repeat(words * 4),
        }
    }

    #[test]
    fn a_conversation_that_fits_is_untouched() {
        let mut history = vec![message(Role::User, 10), message(Role::Assistant, 10)];
        assert_eq!(trim_history(&mut history, 1000, 100), 0);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn trimming_drops_whole_pairs_so_a_reply_never_answers_nothing() {
        let mut history = vec![
            message(Role::User, 100),
            message(Role::Assistant, 100),
            message(Role::User, 10),
        ];
        let dropped = trim_history(&mut history, 200, 0);
        assert_eq!(dropped, 2, "the oldest pair went together");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::User);
    }

    #[test]
    fn an_empty_conversation_is_not_a_reason_to_loop_forever() {
        let mut history = Vec::new();
        assert_eq!(trim_history(&mut history, 0, 100), 0);
    }

    #[test]
    fn every_finish_reason_a_client_draws_differently_is_mapped() {
        let mut answer = ChatResponse {
            content: String::new(),
            tool_calls: Vec::new(),
            stop_reason: Some("length".to_owned()),
            usage: Usage::default(),
            stats: acpilot_model::Stats::default(),
            unsupported_params: Vec::new(),
        };
        assert_eq!(stop_reason(&answer), StopReason::MaxTokens);
        answer.stop_reason = Some("content_filter".to_owned());
        assert_eq!(stop_reason(&answer), StopReason::Refusal);
        answer.stop_reason = Some("stop".to_owned());
        assert_eq!(stop_reason(&answer), StopReason::EndTurn);
        answer.stop_reason = None;
        assert_eq!(
            stop_reason(&answer),
            StopReason::EndTurn,
            "a server that says nothing still ended the turn"
        );
    }

    #[test]
    fn prompt_text_joins_the_text_blocks() {
        let request = PromptRequest::new(
            "s",
            vec![
                ContentBlock::Text(TextContent::new("one")),
                ContentBlock::Text(TextContent::new("two")),
            ],
        );
        assert_eq!(prompt_text(&request), "one\ntwo");
    }

    #[test]
    fn a_session_id_is_stable_for_a_sequence_number() {
        assert_eq!(new_session_id(7), "lm-7");
    }
}
