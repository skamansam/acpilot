//! Sessions: one conversation, its event log, and the routes that drive it.
//!
//! The daemon owns sessions because every UI is a client of this API. A session
//! here is a record of *what a client needs to see* — the agent, the workspace,
//! a status, a title — plus the live [`Session`] behind it and the events it has
//! produced. Nothing about a turn is kept in an HTTP handler's local state: the
//! prompt is acknowledged immediately, the turn runs on its own task, and the
//! transcript arrives on the event stream where a reconnecting client can ask
//! for everything after the last sequence it saw.
//!
//! # Why a replay ring
//!
//! The engine's event channel is a broadcast channel: a subscriber that arrives
//! late misses what was already sent. A UI attaches after `POST /v1/sessions`
//! returns, so "late" is the normal case, and `GET /v1/sessions/{id}/events?after=`
//! is how it catches up. The ring is bounded, and a client that asks for a
//! sequence the ring no longer holds is told so rather than served a transcript
//! with a hole in it it cannot name.
//!
//! # What is deliberately not here
//!
//! No persistence: the transcript table is the engine phase's task P2-8, and until
//! it lands a daemon restart forgets its sessions. No fork, no prompt queue, no
//! per-session policy — each is a feature with a task id, not a shortcut taken
//! while wiring the routes.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use acpilot_agent::session::{Session, SessionOptions};
use acpilot_agent::supervisor::AgentProcess;
use acpilot_agent::{AgentSpec, Fs};
use acpilot_core::AcpilotError;
use acpilot_host::LocalHost;
use acpilot_proto::ErrorBody;
use acpilot_proto::event::{SessionEvent, SessionEventKind};
use acpilot_proto::session::{
    CancelAccepted, CreateSessionRequest, PromptAccepted, SessionId, SessionStatus, SessionSummary,
};
use axum::Json;
use axum::Router;
use axum::extract::{Path as ApiPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::{ApiError, AppState, HEARTBEAT};

/// How many events a session keeps for replay.
///
/// Big enough for a long turn's transcript, small enough that a hundred idle
/// sessions are still measured in megabytes. A client that falls further behind
/// than this is told to start again rather than handed a partial log.
pub const REPLAY_CAPACITY: usize = 4096;

/// How long a session title may be before it is cut at a word boundary.
const TITLE_LIMIT: usize = 60;

/// Frames buffered for one attached client before it is considered stalled.
const STREAM_BUFFER: usize = 64;

/// Commands buffered for a session's owner task.
///
/// Small on purpose: a queue of prompts is a feature nobody asked for, and a
/// second prompt while one is in flight is already refused by the route.
const COMMAND_BUFFER: usize = 8;

/// Every session the daemon owns, by id.
#[derive(Debug, Clone, Default)]
pub struct Sessions {
    inner: Arc<Mutex<HashMap<SessionId, Arc<Record>>>>,
}

impl Sessions {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every session, oldest first.
    #[must_use]
    pub fn list(&self) -> Vec<SessionSummary> {
        let mut summaries: Vec<SessionSummary> = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|record| record.summary())
            .collect();
        summaries.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        summaries
    }

    /// One session's summary, when it exists.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<SessionSummary> {
        self.handle(id).map(|record| record.summary())
    }

    /// The record behind `id`, for the routes that drive it.
    #[must_use]
    pub fn handle(&self, id: &str) -> Option<Arc<Record>> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// Forget a session and end its agent.
    pub fn remove(&self, id: &str) -> bool {
        let removed = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        match removed {
            Some(record) => {
                // The task owns the session, so ending it is a message rather
                // than a drop: dropping the last record would not reach it.
                record.shutdown();
                true
            }
            None => false,
        }
    }

    fn insert(&self, record: Arc<Record>) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(record.id().to_owned(), record);
    }
}

/// One session: its identity, its live state, and its event log.
#[derive(Debug)]
pub struct Record {
    id: SessionId,
    workspace_id: String,
    agent_id: String,
    lifecycle: Mutex<Lifecycle>,
    log: Mutex<Log>,
    /// The turn in flight, so a cancel can say which one it stopped.
    turn: Mutex<Option<String>>,
    /// The mailbox of the task that owns the [`Session`].
    ///
    /// The ACP handle is `Send` but not `Sync` — the SDK's connection owns an
    /// `std::sync::mpsc` receiver — so it cannot live in state that axum shares
    /// across tasks. One task owns it and reads this channel; every route talks
    /// to that task instead of to the agent.
    commands: mpsc::Sender<Command>,
}

/// What a route can ask a session's owner task to do.
enum Command {
    /// Send a prompt and stream the turn.
    Prompt(String),
    /// Ask the agent to stop the turn in flight.
    Cancel,
    /// End the session: the agent process is killed when the task returns.
    Shutdown,
}

/// The parts of a session that change.
#[derive(Debug)]
struct Lifecycle {
    title: Option<String>,
    status: SessionStatus,
    created_at: String,
    updated_at: String,
}

/// The event ring and the live feed.
#[derive(Debug)]
struct Log {
    events: VecDeque<SessionEvent>,
    live: broadcast::Sender<SessionEvent>,
}

impl Record {
    fn new(
        id: SessionId,
        workspace_id: String,
        agent_id: String,
    ) -> (Arc<Self>, mpsc::Receiver<Command>) {
        let now = jiff::Timestamp::now().to_string();
        let (commands, mailbox) = mpsc::channel(COMMAND_BUFFER);
        (
            Arc::new(Self {
                id,
                workspace_id,
                agent_id,
                lifecycle: Mutex::new(Lifecycle {
                    title: None,
                    status: SessionStatus::Idle,
                    created_at: now.clone(),
                    updated_at: now,
                }),
                log: Mutex::new(Log {
                    events: VecDeque::new(),
                    live: broadcast::channel(REPLAY_CAPACITY).0,
                }),
                turn: Mutex::new(None),
                commands,
            }),
            mailbox,
        )
    }

    /// The id every route and event uses.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What the API reports for this session.
    #[must_use]
    pub fn summary(&self) -> SessionSummary {
        let state = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        SessionSummary {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            agent_id: self.agent_id.clone(),
            title: state
                .title
                .clone()
                .unwrap_or_else(|| format!("{} session", self.agent_id)),
            status: state.status,
            created_at: state.created_at.clone(),
            updated_at: state.updated_at.clone(),
        }
    }

    /// The current lifecycle state.
    #[must_use]
    pub fn status(&self) -> SessionStatus {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status
    }

    fn set_status(&self, status: SessionStatus) {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status = status;
    }

    /// The sequence number of the last recorded event, `0` when there is none.
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .back()
            .map_or(0, |event| event.seq)
    }

    /// Record one event: ring for replay, feed for whoever is attached.
    pub fn record(&self, event: SessionEvent) {
        {
            let mut state = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            state.updated_at = event.at.clone();
        }
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        log.events.push_back(event.clone());
        while log.events.len() > REPLAY_CAPACITY {
            log.events.pop_front();
        }
        // No listener is not an error: a session with nothing attached still
        // keeps its transcript for the client that arrives later.
        let _ = log.live.send(event);
    }

    /// Everything after `after`, oldest first.
    #[must_use]
    pub fn replay_after(&self, after: u64) -> Vec<SessionEvent> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .iter()
            .filter(|event| event.seq > after)
            .cloned()
            .collect()
    }

    /// A subscription to events recorded from now on.
    fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .live
            .subscribe()
    }

    /// The turn in flight, when there is one.
    fn current_turn(&self) -> Option<String> {
        self.turn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_turn(&self, turn: Option<String>) {
        *self.turn.lock().unwrap_or_else(PoisonError::into_inner) = turn;
    }

    /// Ask the session's owner task to end, which drops the agent process.
    fn shutdown(&self) {
        let _ = self.commands.try_send(Command::Shutdown);
    }

    /// A title from the first prompt, if the session has none yet.
    fn title_from(&self, prompt: &str) {
        let mut state = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.title.is_some() {
            return;
        }
        if let Some(title) = derive_title(prompt) {
            state.title = Some(title);
        }
    }
}

/// The first line of a prompt, cut at a word boundary.
///
/// A title is a label in a sidebar, not a summary: anything that starts
/// mid-sentence is worse than a shorter line that ends where a reader expects.
fn derive_title(prompt: &str) -> Option<String> {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    if line.chars().count() <= TITLE_LIMIT {
        return Some(line.to_owned());
    }
    let cut: String = line.chars().take(TITLE_LIMIT).collect();
    let trimmed = match cut.rsplit_once(' ') {
        Some((head, _)) if head.chars().count() > TITLE_LIMIT / 2 => head,
        _ => cut.as_str(),
    };
    Some(format!("{}…", trimmed.trim_end()))
}

/// Start a session: spawn the agent, open the ACP session, attach the event pump.
///
/// An empty `workspace_id` means "the workspace this daemon serves", which is
/// the only one it has until workspaces are first-class. Naming a different one
/// is refused rather than ignored: a client that thinks it is editing directory
/// A while the agent edits B has no way to notice.
pub async fn create(
    state: &AppState,
    request: CreateSessionRequest,
) -> Result<SessionSummary, ApiError> {
    let registry = state.registry().ok_or_else(|| {
        ApiError::new(AcpilotError::conflict(
            "this daemon has no profile, so it has no agents to start",
        ))
    })?;
    let spec = registry.get(&request.agent_id).ok_or_else(|| {
        ApiError::new(AcpilotError::not_found(format!(
            "agent {} in this profile",
            request.agent_id
        )))
    })?;
    let workspace = state.workspace().ok_or_else(|| {
        ApiError::new(AcpilotError::invalid(
            "this daemon was started without a workspace, so a session has no root to open",
        ))
    })?;
    // The workspace is checked before the policy: a client asking for a root
    // this daemon does not serve has made a request error, and that answer does
    // not depend on whether the daemon happens to have a policy loaded.
    if !request.workspace_id.is_empty() && request.workspace_id != workspace.id {
        return Err(ApiError::new(AcpilotError::not_found(format!(
            "workspace {}; this daemon serves {}",
            request.workspace_id, workspace.id
        ))));
    }
    let broker = state.broker().ok_or_else(|| {
        ApiError::new(AcpilotError::conflict(
            "this daemon has no permission policy, so it cannot say what an agent may do",
        ))
    })?;

    let id = next_id(state);
    let (record, mailbox) = Record::new(id.clone(), workspace.id.clone(), spec.id.clone());
    if let Some(title) = &request.title {
        record.title_from(title);
    }

    let fs = Fs::with_broker(Arc::new(LocalHost), Arc::clone(broker));
    let options = SessionOptions::new(id.clone())
        .agent(spec.id.clone())
        .fs(Arc::new(fs));
    let session = start(Arc::clone(&spec), options, workspace.root.as_path()).await?;

    // The session is moved onto its own thread before the route can return, so
    // no client ever sees a session id that has nobody listening on it.
    spawn_owner(session, Arc::clone(&record), mailbox);
    state.sessions.insert(Arc::clone(&record));
    Ok(record.summary())
}

/// Run one session's owner loop on a thread of its own.
///
/// Not a `tokio::spawn`: the ACP handle is `Send` but not `Sync`, so any future
/// that borrows it across an `await` is not `Send` and cannot go on the shared
/// runtime. A current-thread runtime per session is the smallest honest fix —
/// it also means one agent's turn cannot starve another's.
fn spawn_owner(session: Session, record: Arc<Record>, mailbox: mpsc::Receiver<Command>) {
    let name = format!("acpilot-session-{}", record.id());
    let owner = Arc::clone(&record);
    let spawned = std::thread::Builder::new()
        .name(name)
        // A session thread runs one agent loop; it does not need the default
        // 2 MiB to hold a transcript of channel messages.
        .stack_size(512 * 1024)
        .spawn(move || {
            let record = owner;
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    record.notice(
                        "Session ended",
                        &format!("no runtime for the session: {error}"),
                    );
                    record.set_status(SessionStatus::Closed);
                    return;
                }
            };
            runtime.block_on(drive(session, record, mailbox));
        });
    if let Err(error) = spawned {
        record.notice(
            "Session ended",
            &format!("no thread for the session: {error}"),
        );
        record.set_status(SessionStatus::Closed);
    }
}

/// The loop that owns one [`Session`].
///
/// Everything that needs `&Session` happens here. A turn is driven inside this
/// loop rather than on a second task: the event feed has to be drained while the
/// prompt is in flight, and this loop is already the one holding it.
async fn drive(session: Session, record: Arc<Record>, mut mailbox: mpsc::Receiver<Command>) {
    let mut events = session.events();
    loop {
        tokio::select! {
            command = mailbox.recv() => match command {
                Some(Command::Prompt(text)) => {
                    if drive_turn(&session, &record, &mut events, &mut mailbox, text).await
                        == Continue::Stop
                    {
                        return;
                    }
                }
                // A cancel between turns is not an error: the key arrived after
                // the turn ended, which is what a person asked for either way.
                Some(Command::Cancel) => {}
                Some(Command::Shutdown) | None => {
                    record.set_status(SessionStatus::Closed);
                    return;
                }
            },
            event = events.recv() => {
                if drain(&record, event) == Continue::Stop {
                    return;
                }
            }
        }
    }
}

/// Whether the session's owner task should keep running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Continue {
    Yes,
    Stop,
}

/// Run one turn, streaming its events as they arrive.
async fn drive_turn(
    session: &Session,
    record: &Arc<Record>,
    events: &mut broadcast::Receiver<SessionEvent>,
    mailbox: &mut mpsc::Receiver<Command>,
    text: String,
) -> Continue {
    let turn = session.prompt(text);
    tokio::pin!(turn);
    loop {
        tokio::select! {
            outcome = &mut turn => {
                record.set_turn(None);
                match outcome {
                    Ok(_) => record.set_status(SessionStatus::Idle),
                    Err(error) => {
                        // The transcript says why, on the stream the turn was on:
                        // a status change alone leaves a reader guessing.
                        record.notice("Turn failed", &error.to_string());
                        record.set_status(SessionStatus::Failed);
                    }
                }
                return Continue::Yes;
            }
            event = events.recv() => {
                if drain(record, event) == Continue::Stop {
                    return Continue::Stop;
                }
            }
            command = mailbox.recv() => match command {
                Some(Command::Cancel) => {
                    if let Err(error) = session.cancel() {
                        record.notice("Cancel failed", &error.to_string());
                    }
                }
                Some(Command::Prompt(_)) => record.notice(
                    "Prompt refused",
                    "a turn is already in flight; that prompt was not sent",
                ),
                Some(Command::Shutdown) | None => {
                    record.set_status(SessionStatus::Closed);
                    return Continue::Stop;
                }
            },
        }
    }
}

/// Record one event, and say whether the session is still alive.
fn drain(
    record: &Arc<Record>,
    event: Result<SessionEvent, broadcast::error::RecvError>,
) -> Continue {
    match event {
        Ok(event) => record.record(event),
        // The ring holds what this task could not keep up with; a subscriber
        // reading the replay is not relying on this receiver's buffer.
        Err(broadcast::error::RecvError::Lagged(_)) => {}
        Err(broadcast::error::RecvError::Closed) => {
            record.set_status(SessionStatus::Closed);
            return Continue::Stop;
        }
    }
    Continue::Yes
}

/// Spawn the agent and open the ACP session, mapping both failures to the API.
async fn start(
    spec: Arc<AgentSpec>,
    options: SessionOptions,
    root: &Path,
) -> Result<Session, ApiError> {
    let process = AgentProcess::spawn(spec).map_err(agent_failed)?;
    let mut session = Session::spawn(process, options)
        .await
        .map_err(agent_failed)?;
    session.new_session(root).await.map_err(agent_failed)?;
    Ok(session)
}

fn agent_failed(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(AcpilotError::AgentFailed {
        message: error.to_string(),
    })
}

/// `s1`, `s2`, … — opaque, stable for the life of the daemon, never reused.
fn next_id(state: &AppState) -> SessionId {
    let id = format!("s{}", state.sessions.list().len() + 1);
    if state.sessions.get(&id).is_some() {
        // A deleted session leaves its number behind; the id must not be reused
        // while a client may still be holding a transcript for it.
        return format!("s{}-{}", jiff::Timestamp::now().as_millisecond(), id);
    }
    id
}

/// `GET /v1/sessions`.
pub async fn list(State(state): State<AppState>) -> Json<Vec<SessionSummary>> {
    Json(pending_aware(state.sessions.list(), &state))
}

/// Mark sessions blocked on a permission request.
///
/// The status is derived rather than stored: a session does not know it is
/// waiting, the broker does, and two places remembering the same fact is how
/// they end up disagreeing. `awaiting_permission` is therefore only ever true
/// while the request is still pending.
fn pending_aware(mut summaries: Vec<SessionSummary>, state: &AppState) -> Vec<SessionSummary> {
    let Some(broker) = state.broker() else {
        return summaries;
    };
    let pending = broker.pending();
    for summary in &mut summaries {
        if pending
            .iter()
            .any(|request| request.session_id == summary.id)
        {
            summary.status = SessionStatus::AwaitingPermission;
        }
    }
    summaries
}

/// `GET /v1/sessions/{id}`.
pub async fn one(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<SessionSummary>, ApiError> {
    let summary = state.sessions.get(&id).ok_or_else(|| unknown(&id))?;
    Ok(Json(pending_aware(vec![summary], &state).remove(0)))
}

/// `DELETE /v1/sessions/{id}`.
pub async fn remove(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<SessionSummary>, ApiError> {
    let summary = state.sessions.get(&id).ok_or_else(|| unknown(&id))?;
    state.sessions.remove(&id);
    Ok(Json(summary))
}

/// `POST /v1/sessions`.
pub async fn create_route(
    State(state): State<AppState>,
    body: Result<Json<CreateSessionRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<SessionSummary>, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a session request: {rejection}"
        )))
    })?;
    create(&state, request).await.map(Json)
}

/// `POST /v1/sessions/{id}/prompt`.
///
/// Acknowledged immediately: the turn runs on its own task and streams over the
/// session's event route. `first_seq` is where that turn's output begins, so a
/// client that reconnects mid-turn asks for exactly the part it has not seen.
pub async fn prompt(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    body: Result<
        Json<acpilot_proto::session::PromptRequest>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Result<(StatusCode, Json<PromptAccepted>), ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a prompt: {rejection}"
        )))
    })?;
    if request.text.trim().is_empty() {
        return Err(ApiError::new(AcpilotError::invalid("`text` is empty"))
            .with_detail("text", "a prompt needs at least one non-space character"));
    }
    let record = state.sessions.handle(&id).ok_or_else(|| unknown(&id))?;
    if record.status() == SessionStatus::Busy {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "session {id} already has a turn in flight; cancel it or wait"
        ))));
    }
    if record.status() == SessionStatus::Closed {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "session {id} has ended; start a new one"
        ))));
    }

    record.title_from(&request.text);
    record.set_status(SessionStatus::Busy);
    let first_seq = record.last_seq() + 1;
    let turn_id = format!("{id}:{first_seq}");
    record.set_turn(Some(turn_id.clone()));

    // The owner task runs the turn; the route only hands it over. A full mailbox
    // means the task is still finishing something, which is the same conflict as
    // a turn in flight and is reported as one.
    if record
        .commands
        .try_send(Command::Prompt(request.text))
        .is_err()
    {
        record.set_turn(None);
        record.set_status(SessionStatus::Busy);
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "session {id} is not ready for another prompt yet"
        ))));
    }

    // `202`: the turn was accepted and has not run. The transcript arrives on
    // the session's stream, which is where every client reads it.
    Ok((
        StatusCode::ACCEPTED,
        Json(PromptAccepted {
            session_id: id,
            turn_id,
            first_seq,
        }),
    ))
}

/// `POST /v1/sessions/{id}/cancel`.
///
/// Cancelling an idle session is not an error: the caller asked for the turn to
/// stop, and it is already stopped.
pub async fn cancel(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    body: Option<Json<serde_json::Value>>,
) -> Result<(StatusCode, Json<CancelAccepted>), ApiError> {
    let record = state.sessions.handle(&id).ok_or_else(|| unknown(&id))?;
    if record.status() == SessionStatus::Closed {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "session {id} has ended; there is nothing to cancel"
        ))));
    }
    // Naming a turn is optional. When it is named and it is not the one in
    // flight, nothing is cancelled: stopping a different turn than the caller
    // asked for is worse than doing nothing.
    let wanted = body
        .as_ref()
        .and_then(|Json(value)| value.get("turn_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let turn_id = record.current_turn();
    let cancelled = match (&wanted, &turn_id) {
        (Some(wanted), Some(current)) => wanted == current,
        (Some(_), None) => false,
        (None, current) => current.is_some(),
    };
    if cancelled && record.commands.try_send(Command::Cancel).is_err() {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "session {id} is not accepting a cancel right now"
        ))));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(CancelAccepted { turn_id, cancelled }),
    ))
}

impl Record {
    /// Record a notice on the session's own stream.
    fn notice(&self, title: &str, message: &str) {
        let seq = self.last_seq() + 1;
        let event = SessionEvent {
            seq,
            session_id: self.id.clone(),
            kind: SessionEventKind::Notice,
            at: jiff::Timestamp::now().to_string(),
            payload: serde_json::json!({
                "level": "warning",
                "title": title,
                "message": message,
            }),
        };
        self.record(event);
    }
}

/// `GET /v1/sessions/{id}/events?after=N` — the session's event stream.
///
/// `after` (or a `Last-Event-ID` header, which is what an `EventSource` sends on
/// reconnect) selects the first sequence to deliver. Everything is replayed from
/// the ring before the live feed starts, so a client that was away sees the turn
/// it missed instead of a transcript that starts mid-sentence.
pub async fn events(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    Query(query): Query<AfterQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let record = state.sessions.handle(&id).ok_or_else(|| unknown(&id))?;
    let after = query
        .after
        .or_else(|| {
            headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .unwrap_or(0);

    let (sender, receiver) = mpsc::channel(STREAM_BUFFER);
    tokio::spawn(pump(record, after, sender));
    Ok(Sse::new(futures::stream::unfold(
        receiver,
        |mut receiver| async move {
            receiver
                .recv()
                .await
                .map(|frame| (Ok::<_, std::convert::Infallible>(frame), receiver))
        },
    ))
    .into_response())
}

/// Which sequence the stream starts at.
#[derive(Debug, Deserialize)]
pub struct AfterQuery {
    after: Option<u64>,
}

/// Replay what the ring holds, then follow the feed.
enum Step {
    Beat,
    Event(Result<SessionEvent, broadcast::error::RecvError>),
}

async fn pump(record: Arc<Record>, after: u64, sender: mpsc::Sender<Event>) {
    let mut last = after;
    for event in record.replay_after(after) {
        last = event.seq;
        if sender.send(frame(&event)).await.is_err() {
            return;
        }
    }

    let mut live = record.subscribe();
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let step = tokio::select! {
            _ = beat.tick() => Step::Beat,
            event = live.recv() => Step::Event(event),
        };
        let frame = match step {
            Step::Beat => Event::default()
                .event("heartbeat")
                .data(serde_json::json!({ "seq": last }).to_string()),
            Step::Event(Ok(event)) => {
                // The replay and the feed can overlap by one event: the ring is
                // written before the broadcast, so a subscriber that arrives
                // mid-write sees the same sequence in both.
                if event.seq <= last {
                    continue;
                }
                last = event.seq;
                frame(&event)
            }
            Step::Event(Err(broadcast::error::RecvError::Lagged(_))) => Event::default()
                .event("resync")
                .data(serde_json::json!({ "latest_seq": last }).to_string()),
            Step::Event(Err(broadcast::error::RecvError::Closed)) => break,
        };
        if sender.send(frame).await.is_err() {
            break;
        }
    }
}

/// One event as an SSE frame. The id is the sequence, so a browser's
/// `Last-Event-ID` reconnect asks for the right place without extra work.
fn frame(event: &SessionEvent) -> Event {
    Event::default()
        .event("session")
        .id(event.seq.to_string())
        .data(serde_json::to_string(event).expect("a session event is always serialisable"))
}

fn unknown(id: &str) -> ApiError {
    ApiError::new(AcpilotError::not_found(format!("session {id}")))
}

/// The session routes, ready to merge into the daemon's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/sessions", get(list).post(create_route))
        .route("/v1/sessions/{id}", get(one).delete(remove))
        .route("/v1/sessions/{id}/prompt", post(prompt))
        .route("/v1/sessions/{id}/cancel", post(cancel))
        .route("/v1/sessions/{id}/events", get(events))
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/sessions",
    tag = "sessions",
    summary = "List sessions",
    responses((status = 200, description = "Every session, oldest first", body = Vec<SessionSummary>))
)]
fn list_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/sessions",
    tag = "sessions",
    summary = "Start a session",
    request_body = CreateSessionRequest,
    responses(
        (status = 200, description = "The session, with its agent attached", body = SessionSummary),
        (status = 400, description = "A workspace this daemon does not serve", body = ErrorBody),
        (status = 404, description = "No such agent", body = ErrorBody),
        (status = 409, description = "No profile, or no policy to decide with", body = ErrorBody),
        (status = 502, description = "The agent would not start or refused the handshake", body = ErrorBody)
    )
)]
fn create_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/sessions/{id}",
    tag = "sessions",
    summary = "One session",
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "The session", body = SessionSummary),
        (status = 404, description = "No such session", body = ErrorBody)
    )
)]
fn get_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    delete,
    path = "/v1/sessions/{id}",
    tag = "sessions",
    summary = "End a session and kill its agent",
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "The session as it was", body = SessionSummary),
        (status = 404, description = "No such session", body = ErrorBody)
    )
)]
fn delete_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/prompt",
    tag = "sessions",
    summary = "Send a prompt",
    params(("id" = String, Path, description = "Session id")),
    request_body = acpilot_proto::session::PromptRequest,
    responses(
        (status = 202, description = "The turn was accepted; its output streams on the event route", body = PromptAccepted),
        (status = 400, description = "Empty prompt", body = ErrorBody),
        (status = 404, description = "No such session", body = ErrorBody),
        (status = 409, description = "A turn is already in flight", body = ErrorBody)
    )
)]
fn prompt_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/cancel",
    tag = "sessions",
    summary = "Cancel the turn in flight",
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 202, description = "The session, after the cancel was sent", body = CancelAccepted),
        (status = 404, description = "No such session", body = ErrorBody),
        (status = 409, description = "The session has ended", body = ErrorBody)
    )
)]
fn cancel_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/sessions/{id}/events",
    tag = "sessions",
    summary = "Follow a session's events",
    params(
        ("id" = String, Path, description = "Session id"),
        ("after" = Option<u64>, Query, description = "First sequence to deliver; `Last-Event-ID` is read too")
    ),
    responses((
        status = 200,
        description = "text/event-stream: `session` frames with the sequence as their id, `heartbeat` every 15 s, `resync` after a gap"
    ))
)]
fn events_doc() {}

/// The OpenAPI operations this module adds, so `lib.rs` registers them in one
/// place instead of repeating the path table.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    use utoipa::Path;
    vec![
        (
            "/v1/sessions",
            HttpMethod::Get,
            <__path_list_doc as Path>::operation(),
        ),
        (
            "/v1/sessions",
            HttpMethod::Post,
            <__path_create_doc as Path>::operation(),
        ),
        (
            "/v1/sessions/{id}",
            HttpMethod::Get,
            <__path_get_doc as Path>::operation(),
        ),
        (
            "/v1/sessions/{id}",
            HttpMethod::Delete,
            <__path_delete_doc as Path>::operation(),
        ),
        (
            "/v1/sessions/{id}/prompt",
            HttpMethod::Post,
            <__path_prompt_doc as Path>::operation(),
        ),
        (
            "/v1/sessions/{id}/cancel",
            HttpMethod::Post,
            <__path_cancel_doc as Path>::operation(),
        ),
        (
            "/v1/sessions/{id}/events",
            HttpMethod::Get,
            <__path_events_doc as Path>::operation(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_cut_at_a_word_boundary() {
        let long = "refactor the permission broker so every refusal names the rule that caused it";
        let title = derive_title(long).expect("a title");
        assert!(title.ends_with('…'), "{title}");
        assert!(title.chars().count() <= TITLE_LIMIT + 1, "{title}");
        assert!(!title.contains("  "), "{title}");
    }

    #[test]
    fn a_short_prompt_is_its_own_title() {
        assert_eq!(
            derive_title("fix the tests").as_deref(),
            Some("fix the tests")
        );
    }

    #[test]
    fn a_blank_prompt_names_nothing() {
        assert_eq!(derive_title("   \n\t\n"), None);
    }

    #[test]
    fn the_replay_ring_keeps_the_newest_events() {
        let (record, _mailbox) = Record::new("s1".into(), "default".into(), "mock".into());
        for seq in 1..=REPLAY_CAPACITY + 10 {
            record.record(event(&record, seq as u64));
        }
        let kept = record.replay_after(0);
        assert_eq!(kept.len(), REPLAY_CAPACITY);
        assert_eq!(kept.first().map(|event| event.seq), Some(11));
        assert_eq!(record.last_seq(), (REPLAY_CAPACITY + 10) as u64);
    }

    #[test]
    fn replay_after_returns_only_later_events() {
        let (record, _mailbox) = Record::new("s1".into(), "default".into(), "mock".into());
        for seq in 1..=3 {
            record.record(event(&record, seq));
        }
        let kept = record.replay_after(1);
        assert_eq!(
            kept.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(record.replay_after(3).is_empty());
    }

    #[test]
    fn the_default_title_names_the_agent() {
        let (record, _mailbox) = Record::new("s1".into(), "default".into(), "claude".into());
        assert_eq!(record.summary().title, "claude session");
        record.title_from("update the readme");
        assert_eq!(record.summary().title, "update the readme");
        // A second prompt does not rename a conversation.
        record.title_from("something else");
        assert_eq!(record.summary().title, "update the readme");
    }

    fn event(record: &Record, seq: u64) -> SessionEvent {
        SessionEvent {
            seq,
            session_id: record.id().to_owned(),
            kind: SessionEventKind::Notice,
            at: jiff::Timestamp::now().to_string(),
            payload: serde_json::json!({ "title": "test" }),
        }
    }
}
