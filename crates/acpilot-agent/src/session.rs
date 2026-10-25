//! One ACP session: the connection, its lifecycle calls, and its event stream.
//!
//! This is the only place that speaks ACP. The supervisor owns the process and
//! its stderr ring; this module takes the child's two protocol pipes and runs
//! the connection over them, so supervision and protocol stay separately
//! testable. The process is held here too, which means dropping a [`Session`]
//! kills the agent instead of leaving it attached to a workspace nobody owns.
//!
//! # What is wrapped
//!
//! `initialize` runs once when the connection opens; `session/new`,
//! `session/load`, `session/prompt` and `session/cancel` are the methods. A
//! session can only prompt after a new or load call, and only one turn at a
//! time — a second concurrent prompt is refused here rather than by whatever a
//! particular agent decides to do with it.
//!
//! # Events
//!
//! Every `session/update` becomes one [`SessionEvent`] with a sequence number
//! assigned here, starting at 1 and never reused. The number is assigned before
//! the event is broadcast, so a slow subscriber drops events but cannot renumber
//! them; the transcript (P2-8) is what makes a gap recoverable. The broadcast
//! channel is the daemon's feed (the service phase's task P3-3 adds replay on top).
//!
//! The kind is the closed [`SessionEventKind`] enum, but the *payload* is
//! deliberately explicit rather than a passthrough of the ACP object with its
//! camelCase keys:
//!
//! | update | kind | payload keys |
//! |---|---|---|
//! | `user_message_chunk` / `agent_message_chunk` | `message_chunk` | `role`, `content`, `message_id`? |
//! | `agent_thought_chunk` | `thought` | `content`, `message_id`? |
//! | `tool_call` / `tool_call_update` | `tool_call` | `tool_call_id`, `status`, `title`, `kind`, `name`?, `content`?, `locations`?, `raw_input`?, `raw_output`?, `patch`? |
//! | `plan` | `plan` | `entries` |
//! | `current_mode_update` | `mode_changed` | `mode_id` |
//! | `notice` | `notice` | `level`, `title`, `message`? |
//! | `compaction_update` | `compaction` | `compaction_id`, `status`, `summary`?, `error`? |
//! | `compaction_summary_chunk` | `compaction` | `compaction_id`, `content` |
//! | `available_commands_update` | `commands_changed` | `commands` |
//! | `config_option_update` | `notice` | `level`, `title`, `subject`, `config_options` |
//! | `session_info_update` | `notice` | `level`, `title`, `subject`, `session_title`?, `updated_at`? |
//! | `usage_update` | `notice` | `level`, `title`, `subject`, `used`, `size`, `cost`? |
//!
//! A tool-call update carries only the fields the agent sent, plus `"patch":
//! true`; the current status is tracked so the payload always says what the tool
//! call's status is now, not just what changed. The config, session-info and
//! usage updates have no kind of their own in the closed enum, so they arrive as
//! notices with a `subject` that says which one they are.
//!
//! # Permissions
//!
//! A permission request is classified (file read, file write, terminal, tool,
//! other) and handed to the [`crate::permission::Broker`], which answers from the
//! rules, from the defaults, or by asking a person. An allow selects the agent's
//! own allow option, a refusal its reject option, and each is recorded as a
//! `notice` so the transcript says what the agent was told. A session with no
//! broker refuses everything with the agent's own reject option: no grant is ever
//! implied by a session that has no policy.
//!
//! # Files
//!
//! `fs/read_text_file` and `fs/write_text_file` are served by [`crate::fs`] when
//! the session was given one, and that is the same condition as advertising the
//! capability at `initialize`: a session either has a workspace and says so, or
//! has none and refuses. A refusal is recorded as a `notice` before it is
//! returned, so an operator reading the event stream sees the same refusals the
//! agent does.
//!
//! # Requests this build cannot serve
//!
//! Terminals arrive with the engine phase's task P2-5, but an agent may ask before then. Each request is
//! answered with `MethodNotFound` and a message naming the gap. Answering is not
//! politeness: a request with no response leaves the agent waiting forever, which
//! is how the engine phase's P2-9 fixture found this, and an error is the one thing an agent can
//! act on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use acpilot_proto::event::{EventSeq, SessionEvent, SessionEventKind};
use acpilot_proto::permission::PermissionKind as RequestKind;
use acpilot_proto::session::SessionId;
use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ClientCapabilities, ClientSessionCapabilities,
    CompactionCapabilities, CompactionSummaryChunk, CompactionUpdate, ContentBlock, ContentChunk,
    CreateTerminalRequest, CurrentModeUpdate, FileSystemCapabilities, Implementation,
    InitializeRequest, InitializeResponse, KillTerminalRequest, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, Notice, NoticeCapabilities,
    NoticeSeverity, PermissionOptionKind, PromptRequest, PromptResponse, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionInfoUpdate, SessionNotification, SessionUpdate, TerminalOutputRequest, TextContent,
    ToolCall, ToolCallStatus, ToolCallUpdate, ToolKind, UsageUpdate, WaitForTerminalExitRequest,
    WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo};
use serde::Serialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

use crate::fs::Fs;
use crate::permission::{Broker, DecidedBy, Subject};
use crate::supervisor::{AgentProcess, StderrRing};
use crate::terminal::Terminals;

/// How many events a subscriber may fall behind before the oldest are dropped.
///
/// The daemon subscribes before a turn starts and drains promptly; this bound
/// exists so a stalled UI cannot make the agent's turn allocate without limit.
pub const EVENT_CAPACITY: usize = 256;

/// How a session presents itself to the agent at `initialize`.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Identifier the daemon uses for this session in [`SessionEvent`].
    pub id: SessionId,
    /// Client name reported to the agent.
    pub client_name: String,
    /// Client version reported to the agent.
    pub client_version: String,
    /// Broadcast capacity; see [`EVENT_CAPACITY`].
    pub event_capacity: usize,
    /// The workspace the agent's `fs/*` requests are served from.
    ///
    /// `None` means the session has no filesystem: the client does not advertise
    /// the capability, and a request that arrives anyway is refused. The two must
    /// agree, because advertising a capability this client cannot serve is the
    /// same bug as refusing one it advertised.
    pub fs: Option<Arc<Fs>>,
    /// The broker that decides what this agent may do.
    ///
    /// `None` means the session has no policy, which is not the same as a
    /// permissive one: every permission request is refused. [`SessionOptions::fs`]
    /// fills this in from the filesystem's broker, because a session that serves
    /// writes must be able to ask about them.
    pub permission: Option<Arc<Broker>>,
    /// Agent id this session runs, so a rule can be scoped to it.
    ///
    /// [`Session::spawn`] fills this from the process it was given; a session over
    /// another transport sets it here. `None` means a rule that names an agent
    /// never matches this session's requests, which is the honest answer when
    /// nobody said which agent is at the other end.
    pub agent: Option<String>,
    /// The terminals this session's agent may open.
    ///
    /// `None` means the session does not advertise `terminal` at all, and a
    /// request that arrives anyway is refused — the same rule [`Self::fs`]
    /// follows, because a capability and the handler that serves it are one
    /// decision.
    pub terminals: Option<Arc<Terminals>>,
}

impl SessionOptions {
    /// Options for `id` with the crate's own name and version.
    pub fn new(id: impl Into<SessionId>) -> Self {
        Self {
            id: id.into(),
            client_name: "acpilot".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            event_capacity: EVENT_CAPACITY,
            fs: None,
            permission: None,
            agent: None,
            terminals: None,
        }
    }

    /// Serve this session's `terminal/*` requests from `terminals`.
    #[must_use]
    pub fn terminals(mut self, terminals: Arc<Terminals>) -> Self {
        self.terminals = Some(terminals);
        self
    }

    /// Record which agent this session runs, for agent-scoped rules.
    #[must_use]
    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    /// Serve this session's `fs/*` requests from `fs`.
    ///
    /// The filesystem's broker comes with it unless one was set already: the file
    /// callbacks are decided by that broker's policy, and a session that served
    /// writes without it would be bypassing the policy it was given.
    #[must_use]
    pub fn fs(mut self, fs: Arc<Fs>) -> Self {
        self.permission
            .get_or_insert_with(|| Arc::clone(fs.broker()));
        self.fs = Some(fs);
        self
    }

    /// Decide this session's requests with `broker`.
    #[must_use]
    pub fn permission(mut self, broker: Arc<Broker>) -> Self {
        self.permission = Some(broker);
        self
    }
}

/// What went wrong opening or driving a session.
#[derive(Debug, Error)]
pub enum SessionError {
    /// A turn is already in flight.
    #[error("the previous prompt is still running; cancel it or wait for it to finish")]
    Busy,

    /// A prompt arrived before `session/new` or `session/load`.
    #[error("no ACP session yet: call new_session or load_session first")]
    NoSession,

    /// The pipes were already taken, or the connection ended before
    /// `initialize` answered.
    #[error("the agent connection is not available; it closed before the session opened")]
    Closed,

    /// `initialize` failed.
    #[error("cannot initialize the agent: {message}")]
    Initialize {
        /// The agent's or transport's message.
        message: String,
    },

    /// A request or notification failed.
    #[error("{operation}: {source}")]
    Protocol {
        /// Which ACP method was being called.
        operation: &'static str,
        /// The underlying failure, which already names the method on the wire.
        #[source]
        source: agent_client_protocol::Error,
    },
}

/// One live ACP connection and its active conversation.
///
/// The connection is kept in a background task; this value is the handle.
/// Requests are concurrent, so [`Session::cancel`] works while a prompt is in
/// flight.
pub struct Session {
    id: SessionId,
    acp_id: Option<String>,
    connection: ConnectionTo<Agent>,
    initialize: InitializeResponse,
    events: broadcast::Sender<SessionEvent>,
    busy: Arc<AtomicBool>,
    process: Option<AgentProcess>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Session {
    /// No pipe handles and no secrets: the id and the agent's identity are what
    /// a log line needs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("acp_session_id", &self.acp_id)
            .field(
                "agent",
                &self.initialize.agent_info.as_ref().map(|info| &info.name),
            )
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Open a session over an already-spawned agent.
    ///
    /// Takes the child's stdin and stdout. Both are handed out once; a second
    /// caller reading stdout would eat protocol frames. The process is owned by
    /// the session from here on, so it dies with it.
    pub async fn spawn(
        mut process: AgentProcess,
        mut options: SessionOptions,
    ) -> Result<Self, SessionError> {
        // The supervisor is the one place that knows which spec was started, so
        // the session's policy sees the same agent id the manifest declared.
        if options.agent.is_none() {
            options.agent = Some(process.id().to_owned());
        }
        let stdin = process.take_stdin();
        let stdout = process.take_stdout();
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            return Err(SessionError::Closed);
        };

        // `blocking` moves each pipe onto its own worker thread; the SDK's
        // transport is async but the pipes the supervisor produced are not.
        let transport = ByteStreams::new(
            blocking::Unblock::new(stdin),
            blocking::Unblock::new(stdout),
        );
        let mut session = Self::over(transport, options).await?;
        session.process = Some(process);
        Ok(session)
    }

    /// Open a session over any transport that can reach an ACP agent.
    ///
    /// This is what [`Session::spawn`] uses for local pipes and what an SSH or
    /// WSL host (P3-9) will supply later. It must be called inside a Tokio
    /// runtime: the connection outlives the call in a spawned task.
    pub async fn over<T>(transport: T, options: SessionOptions) -> Result<Self, SessionError>
    where
        T: ConnectTo<Client> + Send + 'static,
    {
        let (events, _) = broadcast::channel(options.event_capacity.max(1));
        let log = Arc::new(Mutex::new(EventLog::new(
            options.id.clone(),
            events.clone(),
        )));

        let (ready_tx, ready_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel();

        let notification_log = Arc::clone(&log);
        let permission_log = Arc::clone(&log);
        let read_log = Arc::clone(&log);
        let write_log = Arc::clone(&log);
        let read_files = options.fs.clone();
        let write_files = options.fs.clone();
        let permission_broker = options.permission.clone();
        let permission_agent = options.agent.clone();
        // The daemon's name for this session, which is what a pending request
        // carries: a person approving a write needs the conversation it belongs
        // to, not the agent's own id for it.
        let read_session = options.id.clone();
        let write_session = options.id.clone();
        let permission_session = options.id.clone();
        // One clone per handler: each closure is `move`, so a shared `Option` would
        // be moved away by the first one that registered.
        let create_terminals = options.terminals.clone();
        let output_terminals = options.terminals.clone();
        let wait_terminals = options.terminals.clone();
        let kill_terminals = options.terminals.clone();
        let release_terminals = options.terminals.clone();

        let mut capabilities = ClientCapabilities::new().session(
            ClientSessionCapabilities::new()
                .notices(NoticeCapabilities::default())
                .compaction(CompactionCapabilities::default()),
        );
        // The capability and the handler are one decision: a session with no
        // workspace says it has no filesystem, and refuses if asked anyway.
        if options.fs.is_some() {
            capabilities = capabilities.fs(FileSystemCapabilities::new()
                .read_text_file(true)
                .write_text_file(true));
        }
        // One switch for all five methods: an agent picks a terminal by asking for
        // one, so a client that advertised `terminal` but refused `create` would be
        // advertising a capability it cannot serve.
        if options.terminals.is_some() {
            capabilities = capabilities.terminal(true);
        }

        let initialize = InitializeRequest::new(ProtocolVersion::V1)
            .client_capabilities(capabilities)
            .client_info(Implementation::new(
                options.client_name.clone(),
                options.client_version.clone(),
            ));

        let client_name = options.client_name.clone();
        let task = tokio::spawn(async move {
            let result = Client
                .builder()
                .name(client_name)
                .on_receive_notification(
                    async move |notification: SessionNotification, _cx| {
                        record(&notification_log, &notification.update);
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |request: RequestPermissionRequest, responder, _cx| {
                        let Some(broker) = permission_broker.clone() else {
                            let (response, description) = deny(&request);
                            record_permission_denied(&permission_log, &description);
                            return responder.respond(response);
                        };
                        let classified = classify(&request);
                        let mut subject = Subject::new(classified.kind, classified.title.clone())
                            .tool(&classified.tool)
                            .detail(classified.detail.clone());
                        if let Some(path) = &classified.path {
                            subject = subject.path(path);
                        }
                        if let Some(agent) = permission_agent.as_deref() {
                            subject = subject.agent(agent);
                        }
                        let answer = broker.ask(&permission_session, subject).await;
                        let (response, description) =
                            selection(&request, answer.allowed(), &answer.decided_by);
                        record_permission(&permission_log, answer.allowed(), &description);
                        responder.respond(response)
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: ReadTextFileRequest, responder, _cx| {
                        let Some(fs) = read_files.clone() else {
                            return responder.respond_with_error(not_served("fs/read_text_file"));
                        };
                        match fs
                            .read(&read_session, &request.path, request.line, request.limit)
                            .await
                        {
                            Ok(content) => responder.respond(ReadTextFileResponse::new(content)),
                            Err(error) => {
                                record_warning(&read_log, "File read refused", &error.to_string());
                                responder.respond_with_error(error.into_protocol())
                            }
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: WriteTextFileRequest, responder, _cx| {
                        let Some(fs) = write_files.clone() else {
                            return responder.respond_with_error(not_served("fs/write_text_file"));
                        };
                        // One session per connection, so the daemon's id is the
                        // session this write belongs to whatever the request says.
                        match fs
                            .write(&write_session, &request.path, &request.content)
                            .await
                        {
                            Ok(()) => responder.respond(WriteTextFileResponse::new()),
                            Err(error) => {
                                record_warning(
                                    &write_log,
                                    "File write refused",
                                    &error.to_string(),
                                );
                                responder.respond_with_error(error.into_protocol())
                            }
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: CreateTerminalRequest, responder, _cx| {
                        let Some(terminals) = create_terminals.clone() else {
                            return responder.respond_with_error(not_served("terminal/create"));
                        };
                        match terminals.create(&request) {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error.into_protocol()),
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: TerminalOutputRequest, responder, _cx| {
                        let Some(terminals) = output_terminals.clone() else {
                            return responder.respond_with_error(not_served("terminal/output"));
                        };
                        match terminals.output(&request) {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error.into_protocol()),
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: WaitForTerminalExitRequest, responder, _cx| {
                        let Some(terminals) = wait_terminals.clone() else {
                            return responder
                                .respond_with_error(not_served("terminal/wait_for_exit"));
                        };
                        match terminals.wait_for_exit(&request).await {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error.into_protocol()),
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: KillTerminalRequest, responder, _cx| {
                        let Some(terminals) = kill_terminals.clone() else {
                            return responder.respond_with_error(not_served("terminal/kill"));
                        };
                        match terminals.kill(&request) {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error.into_protocol()),
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: ReleaseTerminalRequest, responder, _cx| {
                        let Some(terminals) = release_terminals.clone() else {
                            return responder.respond_with_error(not_served("terminal/release"));
                        };
                        match terminals.release(&request) {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error.into_protocol()),
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(
                    transport,
                    move |connection: ConnectionTo<Agent>| async move {
                        match connection
                            .send_request(initialize)
                            .block_task()
                            .await
                            .map_err(|error| error.to_string())
                        {
                            Ok(response) => {
                                // Nobody left to hand the connection to; ending
                                // this callback closes it, which is the right
                                // answer for a session that was never used.
                                if ready_tx.send(Ok((connection.clone(), response))).is_err() {
                                    return Ok(());
                                }
                            }
                            Err(message) => {
                                let _ = ready_tx.send(Err(message));
                                return Ok(());
                            }
                        }
                        // Keep the connection alive until the session is dropped or
                        // the agent closes its end, whichever happens first.
                        let _ = stop_rx.await;
                        Ok(())
                    },
                )
                .await;

            if let Err(error) = result {
                tracing::debug!(%error, "ACP connection ended with an error");
            }
        });

        let (connection, initialize) = match ready_rx.await {
            Ok(Ok(ready)) => ready,
            Ok(Err(message)) => {
                task.abort();
                return Err(SessionError::Initialize { message });
            }
            Err(_) => {
                task.abort();
                return Err(SessionError::Closed);
            }
        };

        Ok(Self {
            id: options.id,
            acp_id: None,
            connection,
            initialize,
            events,
            busy: Arc::new(AtomicBool::new(false)),
            process: None,
            stop: Some(stop_tx),
            task: Some(task),
        })
    }

    /// The daemon's identifier for this session.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The ACP session id, once `session/new` or `session/load` has run.
    pub fn acp_session_id(&self) -> Option<&str> {
        self.acp_id.as_deref()
    }

    /// The agent's reported name and version, when it sent one.
    pub fn agent_info(&self) -> Option<&Implementation> {
        self.initialize.agent_info.as_ref()
    }

    /// The capabilities the agent advertised at `initialize`.
    pub fn capabilities(&self) -> &AgentCapabilities {
        &self.initialize.agent_capabilities
    }

    /// The protocol version the agent agreed to.
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.initialize.protocol_version
    }

    /// A new subscription to this session's events.
    ///
    /// Subscribe before prompting: events are not replayed from a broadcast
    /// channel, so a subscriber that arrives late misses what was already sent.
    pub fn events(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// The agent's stderr, when this session owns the process.
    pub fn stderr(&self) -> Option<&StderrRing> {
        self.process.as_ref().map(AgentProcess::stderr)
    }

    /// The agent's process id, when this session owns the process.
    ///
    /// `None` for a session over a transport someone else started, such as an
    /// SSH host or an in-memory channel. A session record can name what it is
    /// attached to without holding a handle to it.
    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(AgentProcess::pid)
    }

    /// Create a new conversation at `cwd`.
    pub async fn new_session(
        &mut self,
        cwd: impl AsRef<Path>,
    ) -> Result<NewSessionResponse, SessionError> {
        let response = self
            .connection
            .send_request(NewSessionRequest::new(cwd.as_ref()))
            .block_task()
            .await
            .map_err(|source| SessionError::Protocol {
                operation: "session/new",
                source,
            })?;
        self.acp_id = Some(response.session_id.0.to_string());
        Ok(response)
    }

    /// Reattach to an existing conversation, replaying its history as events.
    pub async fn load_session(
        &mut self,
        session_id: impl Into<String>,
        cwd: impl AsRef<Path>,
    ) -> Result<LoadSessionResponse, SessionError> {
        let session_id = session_id.into();
        let response = self
            .connection
            .send_request(LoadSessionRequest::new(session_id.clone(), cwd.as_ref()))
            .block_task()
            .await
            .map_err(|source| SessionError::Protocol {
                operation: "session/load",
                source,
            })?;
        self.acp_id = Some(session_id);
        Ok(response)
    }

    /// Send one prompt and wait for the turn to end.
    ///
    /// The streamed output arrives on [`Session::events`], not in the response:
    /// the response only says why the turn stopped.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<PromptResponse, SessionError> {
        let acp_id = self.acp_id.clone().ok_or(SessionError::NoSession)?;
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(SessionError::Busy);
        }
        let _turn = Turn::new(Arc::clone(&self.busy));

        let request = PromptRequest::new(
            acp_id,
            vec![ContentBlock::Text(TextContent::new(text.into()))],
        );
        self.connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|source| SessionError::Protocol {
                operation: "session/prompt",
                source,
            })
    }

    /// Ask the agent to stop the turn in flight.
    ///
    /// This is a notification, so it can be sent while a prompt is still
    /// pending. The turn ends with [`StopReason::Cancelled`](agent_client_protocol::schema::v1::StopReason)
    /// and [`Session::prompt`] returns normally.
    pub fn cancel(&self) -> Result<(), SessionError> {
        let acp_id = self.acp_id.clone().ok_or(SessionError::NoSession)?;
        self.connection
            .send_notification(CancelNotification::new(acp_id))
            .map_err(|source| SessionError::Protocol {
                operation: "session/cancel",
                source,
            })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Signal the connection first so the transport closes its write half,
        // then end the agent. Killing first would also work, but it turns a
        // clean shutdown into a crash for no reason.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(process) = self.process.as_mut() {
            let _ = process.kill();
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Resets the busy flag however the turn ends, including a dropped future.
struct Turn(Arc<AtomicBool>);

impl Turn {
    fn new(busy: Arc<AtomicBool>) -> Self {
        Self(busy)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn record(log: &Arc<Mutex<EventLog>>, update: &SessionUpdate) {
    let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
    log.record(update);
}

fn record_permission_denied(log: &Arc<Mutex<EventLog>>, description: &str) {
    record_warning(log, "Permission request denied", description);
}

/// Record a refusal on the event stream, so the operator sees what the agent
/// sees. The agent gets the JSON-RPC error; this is the same decision in the
/// transcript.
fn record_warning(log: &Arc<Mutex<EventLog>>, title: &str, description: &str) {
    let notice = SessionUpdate::Notice(
        Notice::new(NoticeSeverity::Warning, title).description(description.to_owned()),
    );
    record(log, &notice);
}

/// The answer to a request this build cannot serve yet: a JSON-RPC
/// `MethodNotFound` whose message names the method, so an agent's log says which
/// one rather than only that something was refused.
fn not_served(method: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::new(
        i32::from(agent_client_protocol::schema::v1::ErrorCode::MethodNotFound),
        format!("acpilot does not implement `{method}` yet"),
    )
}

/// Pick the refusal that the agent offered, or cancel when it offered none.
fn deny(request: &RequestPermissionRequest) -> (RequestPermissionResponse, String) {
    let reject = request
        .options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::RejectOnce)
        .or_else(|| {
            request
                .options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::RejectAlways)
        });

    let title = request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| request.tool_call.tool_call_id.0.to_string());
    let description = format!("{title} was refused: this session has no permission policy yet");

    let outcome = match reject {
        Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        )),
        None => RequestPermissionOutcome::Cancelled,
    };
    (RequestPermissionResponse::new(outcome), description)
}

/// One line of the agent's request, in the broker's vocabulary.
struct Classified {
    kind: RequestKind,
    tool: String,
    path: Option<PathBuf>,
    title: String,
    detail: String,
}

/// Read a `session/request_permission` as a policy subject.
///
/// The tool kind is the agent's own description of what it is about to do, so it
/// is what the classification trusts; the title and raw input are what a person
/// is shown. A request with no kind is `Other`, which the shipped policy asks
/// about — "we did not recognise it" is not a reason to allow it.
fn classify(request: &RequestPermissionRequest) -> Classified {
    let fields = &request.tool_call.fields;
    let kind = match fields.kind {
        Some(ToolKind::Read | ToolKind::Search) => RequestKind::FileRead,
        Some(ToolKind::Edit | ToolKind::Delete | ToolKind::Move) => RequestKind::FileWrite,
        Some(ToolKind::Execute) => RequestKind::Terminal,
        Some(ToolKind::Fetch) => RequestKind::ToolCall,
        // `ToolKind` is non-exhaustive on purpose: an agent may send a kind
        // newer than this client, and an unrecognised one (like `None`) is
        // asked about rather than guessed at.
        _ => RequestKind::Other,
    };
    let tool = fields.kind.map_or_else(
        || "tool".to_owned(),
        |kind| format!("{kind:?}").to_lowercase(),
    );
    let path = fields
        .locations
        .as_ref()
        .and_then(|locations| locations.first())
        .map(|location| location.path.clone());

    let mut detail = fields
        .raw_input
        .as_ref()
        .map(serde_json::Value::to_string)
        .unwrap_or_default();
    if let Some(locations) = &fields.locations {
        if !detail.is_empty() {
            detail.push('\n');
        }
        let listed: Vec<String> = locations
            .iter()
            .map(|location| location.path.display().to_string())
            .collect();
        detail.push_str(&listed.join(", "));
    }

    Classified {
        kind,
        tool,
        path,
        title: request_title(request),
        detail,
    }
}

fn request_title(request: &RequestPermissionRequest) -> String {
    request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| request.tool_call.tool_call_id.0.to_string())
}

/// The agent's own option for the broker's answer, and a line for the notice.
///
/// The ids are the agent's, not ours: an agent that called its allow option
/// `yolo` must receive `yolo`. When the agent offers no option of the kind the
/// decision needs, the request is cancelled — approving something the agent
/// never offered is not ours to do.
fn selection(
    request: &RequestPermissionRequest,
    allowed: bool,
    decided_by: &DecidedBy,
) -> (RequestPermissionResponse, String) {
    let wanted = if allowed {
        [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]
    } else {
        [
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ]
    };
    let chosen = wanted
        .iter()
        .find_map(|kind| request.options.iter().find(|option| option.kind == *kind));

    let description = format!(
        "{}: {} by {decided_by}",
        request_title(request),
        if allowed { "allowed" } else { "refused" }
    );
    let outcome = match chosen {
        Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        )),
        None => RequestPermissionOutcome::Cancelled,
    };
    (RequestPermissionResponse::new(outcome), description)
}

/// Record an answer on the event stream, at the severity it deserves.
fn record_permission(log: &Arc<Mutex<EventLog>>, allowed: bool, description: &str) {
    let (severity, title) = if allowed {
        (NoticeSeverity::Info, "Permission granted")
    } else {
        (NoticeSeverity::Warning, "Permission request denied")
    };
    let notice =
        SessionUpdate::Notice(Notice::new(severity, title).description(description.to_owned()));
    record(log, &notice);
}

/// The session's event counter and its translation state.
struct EventLog {
    session_id: SessionId,
    next_seq: EventSeq,
    tool_status: HashMap<String, ToolCallStatus>,
    sender: broadcast::Sender<SessionEvent>,
}

impl EventLog {
    fn new(session_id: SessionId, sender: broadcast::Sender<SessionEvent>) -> Self {
        Self {
            session_id,
            next_seq: 1,
            tool_status: HashMap::new(),
            sender,
        }
    }

    fn record(&mut self, update: &SessionUpdate) -> SessionEvent {
        let (kind, payload) = translate(&mut self.tool_status, update);
        self.emit(kind, payload)
    }

    fn emit(&mut self, kind: SessionEventKind, payload: Value) -> SessionEvent {
        let seq = self.next_seq;
        self.next_seq += 1;
        let event = SessionEvent {
            seq,
            session_id: self.session_id.clone(),
            kind,
            at: jiff::Timestamp::now().to_string(),
            payload,
        };
        // A broadcast with no listeners is not an error: the transcript is the
        // durable record, and the daemon subscribes before it prompts.
        let _ = self.sender.send(event.clone());
        event
    }
}

/// One ACP update, as our kind and payload. See the module docs for the table.
fn translate(
    tool_status: &mut HashMap<String, ToolCallStatus>,
    update: &SessionUpdate,
) -> (SessionEventKind, Value) {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => {
            (SessionEventKind::MessageChunk, content_chunk("user", chunk))
        }
        SessionUpdate::AgentMessageChunk(chunk) => (
            SessionEventKind::MessageChunk,
            content_chunk("assistant", chunk),
        ),
        SessionUpdate::AgentThoughtChunk(chunk) => {
            (SessionEventKind::Thought, content_chunk("assistant", chunk))
        }
        SessionUpdate::ToolCall(call) => {
            tool_status.insert(call.tool_call_id.0.to_string(), call.status);
            (SessionEventKind::ToolCall, tool_call(call))
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            if let Some(status) = update.fields.status {
                tool_status.insert(id.clone(), status);
            }
            let status = tool_status.get(&id).copied();
            (
                SessionEventKind::ToolCall,
                tool_call_update(id, update, status),
            )
        }
        SessionUpdate::Plan(plan) => (SessionEventKind::Plan, json!({ "entries": plan.entries })),
        SessionUpdate::CurrentModeUpdate(mode) => {
            (SessionEventKind::ModeChanged, mode_changed(mode))
        }
        SessionUpdate::Notice(notice) => (SessionEventKind::Notice, notice_payload(notice)),
        SessionUpdate::CompactionUpdate(update) => {
            (SessionEventKind::Compaction, compaction(update))
        }
        SessionUpdate::CompactionSummaryChunk(chunk) => {
            (SessionEventKind::Compaction, compaction_chunk(chunk))
        }
        SessionUpdate::AvailableCommandsUpdate(update) => (
            SessionEventKind::CommandsChanged,
            json!({ "commands": update.available_commands }),
        ),
        SessionUpdate::ConfigOptionUpdate(update) => (
            SessionEventKind::Notice,
            json!({
                "level": "info",
                "title": "Configuration options updated",
                "subject": "config_options",
                "config_options": update.config_options,
            }),
        ),
        SessionUpdate::SessionInfoUpdate(update) => {
            (SessionEventKind::Notice, session_info(update))
        }
        SessionUpdate::UsageUpdate(update) => (SessionEventKind::Notice, usage(update)),
        // A variant this build was compiled before, or an extension the crate
        // has not opted into. Keeping the payload makes it visible instead of
        // silently dropping history.
        other => (
            SessionEventKind::Notice,
            json!({
                "level": "info",
                "title": "Unmapped agent update",
                "subject": "unmapped",
                "detail": serde_json::to_value(other).unwrap_or(Value::Null),
            }),
        ),
    }
}

fn content_chunk(role: &str, chunk: &ContentChunk) -> Value {
    let mut payload = Map::new();
    payload.insert("role".to_owned(), json!(role));
    payload.insert(
        "content".to_owned(),
        serde_json::to_value(&chunk.content).unwrap_or(Value::Null),
    );
    if let Some(id) = &chunk.message_id {
        payload.insert("message_id".to_owned(), json!(id.0));
    }
    Value::Object(payload)
}

fn tool_call(call: &ToolCall) -> Value {
    let mut payload = Map::new();
    payload.insert("tool_call_id".to_owned(), json!(call.tool_call_id.0));
    payload.insert("title".to_owned(), json!(call.title));
    payload.insert("status".to_owned(), name(call.status));
    payload.insert("kind".to_owned(), name(call.kind));
    if let Some(tool_name) = &call.name {
        payload.insert("name".to_owned(), json!(tool_name));
    }
    if !call.content.is_empty() {
        payload.insert("content".to_owned(), json!(call.content));
    }
    if !call.locations.is_empty() {
        payload.insert("locations".to_owned(), json!(call.locations));
    }
    if let Some(input) = &call.raw_input {
        payload.insert("raw_input".to_owned(), input.clone());
    }
    if let Some(output) = &call.raw_output {
        payload.insert("raw_output".to_owned(), output.clone());
    }
    Value::Object(payload)
}

fn tool_call_update(id: String, update: &ToolCallUpdate, status: Option<ToolCallStatus>) -> Value {
    let fields = &update.fields;
    let mut payload = Map::new();
    payload.insert("tool_call_id".to_owned(), json!(id));
    payload.insert("patch".to_owned(), json!(true));
    if let Some(status) = status {
        payload.insert("status".to_owned(), name(status));
    }
    if let Some(kind) = fields.kind {
        payload.insert("kind".to_owned(), name(kind));
    }
    if let Some(title) = &fields.title {
        payload.insert("title".to_owned(), json!(title));
    }
    if let Some(tool_name) = &fields.name {
        payload.insert("name".to_owned(), json!(tool_name));
    }
    if let Some(content) = &fields.content {
        payload.insert("content".to_owned(), json!(content));
    }
    if let Some(locations) = &fields.locations {
        payload.insert("locations".to_owned(), json!(locations));
    }
    if let Some(input) = &fields.raw_input {
        payload.insert("raw_input".to_owned(), input.clone());
    }
    if let Some(output) = &fields.raw_output {
        payload.insert("raw_output".to_owned(), output.clone());
    }
    Value::Object(payload)
}

fn mode_changed(update: &CurrentModeUpdate) -> Value {
    json!({ "mode_id": update.current_mode_id.0 })
}

fn notice_payload(notice: &Notice) -> Value {
    let mut payload = Map::new();
    payload.insert("level".to_owned(), name(notice.severity.clone()));
    payload.insert("title".to_owned(), json!(notice.title));
    if let Some(description) = &notice.description {
        payload.insert("message".to_owned(), json!(description));
    }
    Value::Object(payload)
}

fn compaction(update: &CompactionUpdate) -> Value {
    let mut payload = Map::new();
    payload.insert("compaction_id".to_owned(), json!(update.compaction_id.0));
    payload.insert("status".to_owned(), name(update.status.clone()));
    insert_maybe(&mut payload, "summary", &update.summary);
    insert_maybe(&mut payload, "error", &update.error);
    Value::Object(payload)
}

fn compaction_chunk(chunk: &CompactionSummaryChunk) -> Value {
    json!({
        "compaction_id": chunk.compaction_id.0,
        "content": chunk.content,
    })
}

fn session_info(update: &SessionInfoUpdate) -> Value {
    let mut payload = Map::new();
    payload.insert("level".to_owned(), json!("info"));
    payload.insert("title".to_owned(), json!("Session details updated"));
    payload.insert("subject".to_owned(), json!("session_info"));
    if let Some(title) = update.title.as_opt_ref() {
        payload.insert(
            "session_title".to_owned(),
            title.map_or(Value::Null, |title| json!(title)),
        );
    }
    if let Some(updated_at) = update.updated_at.as_opt_ref() {
        payload.insert(
            "updated_at".to_owned(),
            updated_at.map_or(Value::Null, |at| json!(at)),
        );
    }
    Value::Object(payload)
}

fn usage(update: &UsageUpdate) -> Value {
    let mut payload = Map::new();
    payload.insert("level".to_owned(), json!("info"));
    payload.insert("title".to_owned(), json!("Context usage"));
    payload.insert("subject".to_owned(), json!("usage"));
    payload.insert("used".to_owned(), json!(update.used));
    payload.insert("size".to_owned(), json!(update.size));
    if let Some(cost) = &update.cost {
        payload.insert("cost".to_owned(), json!(cost));
    }
    Value::Object(payload)
}

/// A patch field: omitted stays omitted, explicit null clears, a value replaces.
fn insert_maybe<T: Serialize>(
    payload: &mut Map<String, Value>,
    key: &str,
    value: &MaybeUndefined<T>,
) {
    match value {
        MaybeUndefined::Undefined => {}
        MaybeUndefined::Null => {
            payload.insert(key.to_owned(), Value::Null);
        }
        MaybeUndefined::Value(value) => {
            payload.insert(
                key.to_owned(),
                serde_json::to_value(value).unwrap_or(Value::Null),
            );
        }
    }
}

fn name<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use super::*;
    use agent_client_protocol::schema::v1::{
        AvailableCommandsUpdate, CompactionStatus, PermissionOption, Plan, ToolCallLocation,
        ToolCallUpdateFields, ToolKind,
    };

    fn log() -> EventLog {
        let (sender, _receiver) = broadcast::channel(32);
        EventLog::new("session-1".to_owned(), sender)
    }

    fn chunk(text: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
    }

    #[test]
    fn message_chunks_carry_their_role_and_a_monotonic_sequence() {
        let mut log = log();
        let user = log.record(&SessionUpdate::UserMessageChunk(chunk("hello")));
        let agent = log.record(&SessionUpdate::AgentMessageChunk(chunk("hi")));

        assert_eq!(user.seq, 1);
        assert_eq!(agent.seq, 2);
        assert_eq!(user.session_id, "session-1");
        assert_eq!(user.kind, SessionEventKind::MessageChunk);
        assert_eq!(user.payload["role"], "user");
        assert_eq!(user.payload["content"]["text"], "hello");
        assert_eq!(agent.payload["role"], "assistant");
        assert!(jiff::Timestamp::from_str(&user.at).is_ok());
    }

    #[test]
    fn thoughts_are_distinct_from_assistant_messages() {
        let mut log = log();
        let thought = log.record(&SessionUpdate::AgentThoughtChunk(chunk("thinking")));
        assert_eq!(thought.kind, SessionEventKind::Thought);
        assert_eq!(thought.payload["content"]["text"], "thinking");
    }

    #[test]
    fn a_tool_call_reports_its_status_and_later_patches_keep_the_latest_one() {
        let mut log = log();
        let started = log.record(&SessionUpdate::ToolCall(
            ToolCall::new("t1", "Read src/lib.rs")
                .kind(ToolKind::Read)
                .status(ToolCallStatus::InProgress),
        ));
        assert_eq!(started.kind, SessionEventKind::ToolCall);
        assert_eq!(started.payload["tool_call_id"], "t1");
        assert_eq!(started.payload["title"], "Read src/lib.rs");
        assert_eq!(started.payload["status"], "in_progress");
        assert_eq!(started.payload["kind"], "read");

        let patched = log.record(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t1",
            ToolCallUpdateFields::new().title("Read src/lib.rs (done)"),
        )));
        assert_eq!(patched.kind, SessionEventKind::ToolCall);
        assert_eq!(patched.payload["patch"], true);
        assert_eq!(patched.payload["title"], "Read src/lib.rs (done)");
        assert_eq!(
            patched.payload["status"], "in_progress",
            "an update without a status must not lose the status we already know"
        );
    }

    #[test]
    fn plan_mode_commands_and_notices_have_their_own_kinds() {
        let mut log = log();
        let plan = log.record(&SessionUpdate::Plan(Plan::new(vec![])));
        let mode = log.record(&SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
            "code",
        )));
        let commands = log.record(&SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(vec![]),
        ));
        let notice = log.record(&SessionUpdate::Notice(
            Notice::new(NoticeSeverity::Warning, "Careful").description("the sandbox is off"),
        ));

        assert_eq!(plan.kind, SessionEventKind::Plan);
        assert_eq!(plan.payload["entries"], json!([]));
        assert_eq!(mode.kind, SessionEventKind::ModeChanged);
        assert_eq!(mode.payload["mode_id"], "code");
        assert_eq!(commands.kind, SessionEventKind::CommandsChanged);
        assert_eq!(commands.payload["commands"], json!([]));
        assert_eq!(notice.kind, SessionEventKind::Notice);
        assert_eq!(notice.payload["level"], "warning");
        assert_eq!(notice.payload["title"], "Careful");
        assert_eq!(notice.payload["message"], "the sandbox is off");
    }

    #[test]
    fn compaction_is_one_kind_for_both_update_shapes() {
        let mut log = log();
        let started = log.record(&SessionUpdate::CompactionUpdate(CompactionUpdate::new(
            "c1",
            CompactionStatus::Completed,
        )));
        assert_eq!(started.kind, SessionEventKind::Compaction);
        assert_eq!(started.payload["compaction_id"], "c1");
        assert_eq!(started.payload["status"], "completed");

        let summary = log.record(&SessionUpdate::CompactionSummaryChunk(
            CompactionSummaryChunk::new("c1", ContentBlock::Text(TextContent::new("summary"))),
        ));
        assert_eq!(summary.kind, SessionEventKind::Compaction);
        assert_eq!(summary.payload["content"]["text"], "summary");
    }

    #[test]
    fn updates_without_a_kind_become_notices_with_a_subject() {
        let mut log = log();
        let info = log.record(&SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new()));
        assert_eq!(info.kind, SessionEventKind::Notice);
        assert_eq!(info.payload["subject"], "session_info");

        let usage = log.record(&SessionUpdate::UsageUpdate(UsageUpdate::new(120, 4096)));
        assert_eq!(usage.kind, SessionEventKind::Notice);
        assert_eq!(usage.payload["subject"], "usage");
        assert_eq!(usage.payload["used"], 120);
        assert_eq!(usage.payload["size"], 4096);
    }

    #[test]
    fn every_event_in_one_session_gets_a_new_sequence_number() {
        let mut log = log();
        let seqs: Vec<u64> = [
            SessionUpdate::AgentMessageChunk(chunk("one")),
            SessionUpdate::AgentThoughtChunk(chunk("two")),
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new("ask")),
            SessionUpdate::AgentMessageChunk(chunk("three")),
        ]
        .iter()
        .map(|update| log.record(update).seq)
        .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
    }

    fn permission_request(options: Vec<PermissionOption>) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            "s1",
            ToolCallUpdate::new("t1", ToolCallUpdateFields::new().title("Edit src/main.rs")),
            options,
        )
    }

    #[test]
    fn a_permission_request_is_refused_with_the_agents_own_reject_option() {
        let request = permission_request(vec![
            PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new("reject", "Deny", PermissionOptionKind::RejectOnce),
        ]);
        let (response, description) = deny(&request);
        match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(&*selected.option_id.0, "reject");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(description.contains("Edit src/main.rs"));
    }

    #[test]
    fn a_permission_request_without_a_reject_option_is_cancelled() {
        let request = permission_request(vec![PermissionOption::new(
            "allow",
            "Allow",
            PermissionOptionKind::AllowOnce,
        )]);
        let (response, _) = deny(&request);
        assert_eq!(response.outcome, RequestPermissionOutcome::Cancelled);
    }

    #[test]
    fn a_request_is_classified_by_its_tool_kind_and_its_first_location() {
        let path = PathBuf::from("/work/repo/src/main.rs");
        let request = RequestPermissionRequest::new(
            "s1",
            ToolCallUpdate::new(
                "t1",
                ToolCallUpdateFields::new()
                    .kind(ToolKind::Edit)
                    .title("Edit src/main.rs")
                    .locations(vec![ToolCallLocation::new(path.clone())])
                    .raw_input(serde_json::json!({ "path": "/work/repo/src/main.rs" })),
            ),
            vec![],
        );

        let classified = classify(&request);
        assert_eq!(classified.kind, RequestKind::FileWrite);
        assert_eq!(classified.tool, "edit");
        assert_eq!(classified.path, Some(path));
        assert_eq!(classified.title, "Edit src/main.rs");
        assert!(
            classified.detail.contains("main.rs"),
            "the detail shows a person the target: {}",
            classified.detail
        );

        let execute = RequestPermissionRequest::new(
            "s1",
            ToolCallUpdate::new("t2", ToolCallUpdateFields::new().kind(ToolKind::Execute)),
            vec![],
        );
        let classified = classify(&execute);
        assert_eq!(classified.kind, RequestKind::Terminal);
        assert_eq!(classified.tool, "execute");
        assert!(classified.path.is_none(), "a command names no path here");
        assert_eq!(
            classified.title, "t2",
            "a request without a title falls back to its id"
        );
    }

    #[test]
    fn an_allowed_request_selects_the_agents_own_allow_option() {
        let request = permission_request(vec![
            PermissionOption::new("yolo", "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new("nope", "Deny", PermissionOptionKind::RejectOnce),
        ]);

        let (response, description) = selection(&request, true, &DecidedBy::User);
        match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(&*selected.option_id.0, "yolo");
            }
            other => panic!("expected the agent's allow option, got {other:?}"),
        }
        assert!(
            description.contains("allowed") && description.contains("Edit src/main.rs"),
            "the notice says what was allowed and who allowed it: {description}"
        );

        let (response, description) =
            selection(&request, false, &DecidedBy::Policy("no-secrets".to_owned()));
        match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(&*selected.option_id.0, "nope");
            }
            other => panic!("expected the agent's reject option, got {other:?}"),
        }
        assert!(
            description.contains("refused") && description.contains("policy:no-secrets"),
            "the notice names the rule: {description}"
        );
    }

    #[test]
    fn an_allow_the_agent_never_offered_is_cancelled() {
        let request = permission_request(vec![PermissionOption::new(
            "nope",
            "Deny",
            PermissionOptionKind::RejectOnce,
        )]);
        let (response, _) = selection(&request, true, &DecidedBy::User);
        assert_eq!(response.outcome, RequestPermissionOutcome::Cancelled);
    }
}
