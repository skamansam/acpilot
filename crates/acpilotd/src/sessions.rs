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
//! # The ring is not the record
//!
//! The ring is what a client catches up from while the session is live, and it is
//! deliberately small. What survives the process is the append-only transcript the
//! engine phase's task P2-8 added: every record is appended to
//! `<transcripts>/<id>/session.jsonl` as it is recorded, with the permission
//! decisions flushed at the end of each turn. A write failure costs the file, not
//! the conversation — it is logged once and the session carries on, because losing
//! the record is bad and ending the turn to say so is worse.
//!
//! # What is deliberately not here
//!
//! A restart still forgets the *list* of sessions: the transcripts are on disk, but
//! no route enumerates them, and `GET /v1/sessions/{id}/events` answers 404 for a
//! session this process did not start. Resuming a stored conversation is the
//! service phase's task P3-7, which is where the SQLite table and the session row
//! land. No fork, no prompt queue, no per-session policy — each is a feature with a
//! task id, not a shortcut taken while wiring the routes.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use acpilot_agent::session::{Session, SessionOptions};
use acpilot_agent::supervisor::AgentProcess;
use acpilot_agent::transcript::{FILE_NAME, Meta as TranscriptMeta, Transcript};
use acpilot_agent::{AgentSpec, Broker, Fs, PermissionOption};
use acpilot_core::AcpilotError;
use acpilot_host::LocalHost;
use acpilot_proto::ErrorBody;
use acpilot_proto::event::{SessionEvent, SessionEventKind};
use acpilot_proto::session::{
    CancelAccepted, CreateSessionRequest, PromptAccepted, SessionId, SessionStatus, SessionSummary,
};
use acpilot_proto::session::{ModeState, SetModeRequest};
use acpilot_proto::stats::ContextStats;
use acpilot_store::{Grant, Store};
use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as ApiPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::usage::Usage;
use crate::{ApiError, AppState, HEARTBEAT};

/// How many events a session keeps for replay.
///
/// Big enough for a long turn's transcript, small enough that a hundred idle
/// sessions are still measured in megabytes. A client that falls further behind
/// than this is told to start again rather than handed a partial log.
pub const REPLAY_CAPACITY: usize = 4096;

/// How long [`Sessions::ask`] waits for an agent's answer.
///
/// Longer than a turn usually takes and shorter than a client's patience: a model
/// writing one commit message is seconds, not minutes, and a hung agent should end
/// the request with a cancel rather than hold the connection forever.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

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

    /// Send one prompt and wait for the agent's answer.
    ///
    /// The prompt route returns as soon as the turn is accepted because a client
    /// watches the transcript. Commit-message generation is the one caller that
    /// needs the text of the answer, not a subscription: it renders the staged
    /// diff into the prompt, sends it to the agent the person is already talking
    /// to, and returns what the agent said for validation. The events are still
    /// recorded and streamed to every other client; this only also collects them.
    ///
    /// A session that is busy is refused rather than queued: the answer would be
    /// to a question the client did not ask, and the ticket's rule is to reject
    /// rather than repair.
    pub async fn ask(&self, id: &str, text: String) -> Result<String, ApiError> {
        let record = self.handle(id).ok_or_else(|| unknown(id))?;
        match record.status() {
            SessionStatus::Busy | SessionStatus::AwaitingPermission => {
                return Err(ApiError::new(AcpilotError::conflict(format!(
                    "session {id} is still answering a turn"
                ))));
            }
            SessionStatus::Closed => {
                return Err(ApiError::new(AcpilotError::conflict(format!(
                    "session {id} is closed"
                ))));
            }
            SessionStatus::Idle | SessionStatus::Failed => {}
        }

        let mut events = record.subscribe();
        record.set_status(SessionStatus::Busy);
        record.set_turn(Some(format!("{id}:assistant-managed")));
        if record.commands.try_send(Command::Prompt(text)).is_err() {
            record.set_turn(None);
            record.set_status(SessionStatus::Idle);
            return Err(ApiError::new(AcpilotError::conflict(format!(
                "session {id} could not accept another turn"
            ))));
        }

        let deadline = tokio::time::Instant::now() + ANSWER_TIMEOUT;
        let mut answer = String::new();
        let outcome = loop {
            let status = record.status();
            if !matches!(
                status,
                SessionStatus::Busy | SessionStatus::AwaitingPermission
            ) {
                break status;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                let _ = record.commands.try_send(Command::Cancel);
                return Err(ApiError::new(AcpilotError::AgentFailed {
                    message: format!(
                        "the agent did not answer within {} seconds",
                        ANSWER_TIMEOUT.as_secs()
                    ),
                }));
            }
            tokio::select! {
                event = events.recv() => match event {
                    Ok(event) => {
                        if event.kind == SessionEventKind::MessageChunk
                            && event.payload.get("role").and_then(serde_json::Value::as_str)
                                == Some("assistant")
                            && let Some(text) = event
                                .payload
                                .get("content")
                                .and_then(|content| content.get("text"))
                                .and_then(serde_json::Value::as_str)
                        {
                            answer.push_str(text);
                        }
                    }
                    // The ring is smaller than a chatty turn can be. The status
                    // still settles, and the answer this returns is the text that
                    // was not lost; a hole is a failed generation, not a reason to
                    // hold the request open.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        break SessionStatus::Closed;
                    }
                },
                // A turn that finished between two events has no further event to
                // wake this on, so the status is also read on a short timer.
                () = tokio::time::sleep(remaining.min(Duration::from_millis(50))) => {}
            }
        };

        match outcome {
            SessionStatus::Failed | SessionStatus::Closed => {
                Err(ApiError::new(AcpilotError::AgentFailed {
                    message: format!("session {id} failed before it wrote a message"),
                }))
            }
            _ if answer.trim().is_empty() => Err(ApiError::new(AcpilotError::AgentFailed {
                message: format!("session {id} answered with no text"),
            })),
            _ => Ok(answer),
        }
    }

    /// The id a client holds for the conversation the agent calls `acp_id`.
    ///
    /// A scan rather than a second map: a daemon has one session per open
    /// conversation, and a second index would be a second thing to keep in step
    /// with creation, removal and a failed start.
    #[must_use]
    pub fn id_for_acp(&self, acp_id: &str) -> Option<SessionId> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .find(|record| record.answers_to(acp_id))
            .map(|record| record.id.clone())
    }

    /// What the newest conversation with `agent` costs, and when it last spoke.
    ///
    /// One reading per agent, because the bar lists integrations rather than
    /// conversations: three sessions with the same agent are one row, and the newest
    /// is the one a person is watching. The timestamp is the session's own — the last
    /// event it recorded — so a conversation that stopped goes old on the bar instead
    /// of looking fresh for as long as the roster can be read. A session whose stamp
    /// cannot be read loses to one that can, the same rule the bar's own staleness
    /// check follows.
    #[must_use]
    pub fn reading(&self, agent: &str) -> Option<(ContextStats, String)> {
        let records: Vec<Arc<Record>> = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|record| record.agent_id == agent)
            .cloned()
            .collect();
        let mut newest: Option<(jiff::Timestamp, String, ContextStats)> = None;
        for record in records {
            let Some(context) = record.context() else {
                continue;
            };
            let at = record.summary().updated_at;
            let observed = at
                .parse::<jiff::Timestamp>()
                .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
            if newest
                .as_ref()
                .is_none_or(|(current, _, _)| observed >= *current)
            {
                newest = Some((observed, at, context));
            }
        }
        newest.map(|(_, at, context)| (context, at))
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
    /// What the agent called this session, once `session/new` returned.
    ///
    /// The idle may not echo the id we proposed — ACP lets the agent choose — so
    /// anything keyed by the agent's session id (its terminals, its permission
    /// requests) has to be mapped back to the id a client holds.
    acp_id: Mutex<Option<String>>,
    /// The append-only file this session's events and grants are written to.
    ///
    /// `None` when the daemon was given no transcript directory (a test, or an
    /// embedded use that keeps its own record). A write failure is logged once and
    /// then ignored: a full disk must not end the conversation it was recording.
    transcript: Option<Arc<Transcript>>,
    /// Whether a failed transcript write has already been reported for this
    /// session, so a broken disk is one warning rather than one per event.
    transcript_warned: AtomicBool,
    /// The database this session's events and decisions are written to.
    ///
    /// `None` when the daemon opened no store. The file beside it is the record
    /// that outlives the schema; this is the one a route can query by id, which is
    /// what makes a conversation from yesterday reachable after a restart.
    store: Option<Arc<Store>>,
    /// A failed store write is one warning, like a failed transcript write.
    store_warned: AtomicBool,
    /// The broker whose decisions belong in this session's transcript.
    broker: Option<Arc<Broker>>,
    /// The last audit sequence written, so a turn appends what is new and not the
    /// whole profile's history again.
    grants_written: Mutex<u64>,
    /// What this conversation costs, counted as its events arrive.
    ///
    /// Held against the session rather than against the agent, because two
    /// conversations with one agent are two contexts; [`Sessions::reading`] picks
    /// the newest for the stats bar.
    usage: Mutex<Usage>,
    /// Answers already given for a client's `idempotency_key`.
    ///
    /// A retried prompt is one turn: the key is the client's, and the same key in
    /// the same session returns the acceptance it already got rather than starting a
    /// second agent run. Bounded by the session's life, which is the life of the
    /// thing the keys were about.
    prompts: Mutex<HashMap<String, PromptAccepted>>,
    /// The modes the agent offered, and the one it is in.
    ///
    /// Seeded when the session opens and refined by the agent's own `mode_changed`
    /// events, so the route can refuse an id the agent never advertised without
    /// asking the agent and waiting for a turn's worth of stream.
    modes: Mutex<Option<acpilot_proto::session::ModeState>>,
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
    /// Switch the agent's mode.
    Mode(String),
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
        broker: Option<Arc<Broker>>,
        transcripts: Option<&Path>,
        store: Option<Arc<Store>>,
    ) -> (Arc<Self>, mpsc::Receiver<Command>) {
        let now = jiff::Timestamp::now().to_string();
        let (commands, mailbox) = mpsc::channel(COMMAND_BUFFER);
        let transcript = transcripts.and_then(|directory| {
            let meta = TranscriptMeta {
                version: acpilot_agent::transcript::VERSION,
                session: id.clone(),
                workspace: workspace_id.clone(),
                agent: agent_id.clone(),
                created_at: now.clone(),
            };
            match Transcript::create(directory.join(&id).join(FILE_NAME), &meta) {
                Ok(transcript) => Some(Arc::new(transcript)),
                Err(error) => {
                    // A session that cannot keep a file still runs: the transcript
                    // is the record, not the conversation, and refusing to start
                    // would trade a missing file for an unusable daemon.
                    tracing::warn!(%error, session = %id, "no transcript for this session");
                    None
                }
            }
        });
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
                acp_id: Mutex::new(None),
                transcript,
                transcript_warned: AtomicBool::new(false),
                store,
                store_warned: AtomicBool::new(false),
                broker,
                grants_written: Mutex::new(0),
                usage: Mutex::new(Usage::default()),
                prompts: Mutex::new(HashMap::new()),
                modes: Mutex::new(None),
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

    /// Remember what the agent called this session.
    fn set_acp_id(&self, acp_id: Option<&str>) {
        *self.acp_id.lock().unwrap_or_else(PoisonError::into_inner) = acp_id.map(str::to_owned);
    }

    /// Whether the agent knows this conversation by `acp_id`.
    fn answers_to(&self, acp_id: &str) -> bool {
        self.acp_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_deref()
            == Some(acp_id)
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
        {
            let mut state = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            state.status = status;
        }
        // The database learns the new status as it happens: a restart that closed
        // every session would otherwise be the only way a stored row moves, and a
        // client reading history mid-run would see `busy` for a finished turn.
        if let Some(store) = &self.store
            && let Err(error) =
                store.set_status(&self.id, &status, &jiff::Timestamp::now().to_string())
            && !self.store_warned.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(%error, session = %self.id, "the database did not record a status");
        }
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
        // A mode change the agent announced is the newest truth about its modes:
        // taking it from the event rather than polling keeps a second client's
        // picker right after the first switched.
        if matches!(event.kind, SessionEventKind::ModeChanged)
            && let Some(mode_id) = event.payload.get("mode_id").and_then(|id| id.as_str())
        {
            let mut modes = self.modes.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(state) = modes.as_mut() {
                state.mode = mode_id.to_owned();
            }
        }
        // Counted before the event is visible, so a client that sees a chunk and
        // asks the stats route in the same breath is not told it has not arrived.
        self.usage
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observe(&event);
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
        let _ = log.live.send(event.clone());
        drop(log);
        // The file is written outside the ring's lock: a slow disk must not block
        // a subscriber, and a subscriber must not hold up the file.
        if let Some(transcript) = &self.transcript
            && let Err(error) = transcript.event(&event)
            && !self.transcript_warned.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(%error, session = %self.id, "the transcript is no longer being written");
        }
        if let Some(store) = &self.store
            && let Err(error) = store.append_event(&event)
            && !self.store_warned.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(%error, session = %self.id, "the database is no longer being written");
        }
    }

    /// Append the permission decisions this session made since the last call.
    ///
    /// The broker keeps one audit log for the whole profile, so what is new is
    /// "after the last sequence we wrote", not "everything for this session": a
    /// second conversation's grants must not be appended twice. Called when a turn
    /// ends rather than per event, because a decision is already on the broker's
    /// log by the time the call that needed it returns.
    fn flush_grants(&self) {
        let Some(broker) = &self.broker else {
            return;
        };
        if self.transcript.is_none() && self.store.is_none() {
            return;
        }
        let mut written = self
            .grants_written
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let cutoff = *written;
        for entry in broker
            .audit_for(&self.id)
            .into_iter()
            .filter(|entry| entry.seq > cutoff)
        {
            *written = entry.seq;
            // Both records are written from the same audit entry, so the file and
            // the database cannot tally a decision differently. The database copy
            // is the one retention keeps: a decision is an audit row, not volume.
            if let Some(store) = &self.store {
                let grant = Grant {
                    session_id: self.id.clone(),
                    seq: entry.seq,
                    at: entry.at.clone(),
                    tool: entry.tool.clone().unwrap_or_else(|| entry.title.clone()),
                    decision: option_text(&entry.option),
                    decided_by: entry.decided_by.to_string(),
                };
                if let Err(error) = store.record_grant(&grant)
                    && !self.store_warned.swap(true, Ordering::Relaxed)
                {
                    tracing::warn!(%error, session = %self.id, "a decision was not recorded");
                }
            }
            if let Some(transcript) = &self.transcript
                && let Err(error) = transcript.grant(entry)
                && !self.transcript_warned.swap(true, Ordering::Relaxed)
            {
                tracing::warn!(%error, session = %self.id, "a grant was not written to the transcript");
            }
        }
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

    /// Count a prompt on its way to the agent.
    ///
    /// Called by the owner task when it sends, not by a route: the prompt is part of
    /// the conversation whether or not the agent ever echoes it back.
    fn count_prompt(&self, text: &str) {
        self.usage
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .prompt(text);
    }

    /// What this conversation costs, when it has anything to report.
    #[must_use]
    pub fn context(&self) -> Option<ContextStats> {
        self.usage
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .context()
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

    /// The acceptance a retried prompt already got, when this key was seen.
    fn remembered(&self, key: &str) -> Option<PromptAccepted> {
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    /// Remember what a `idempotency_key` was answered with.
    fn remember(&self, key: &str, accepted: &PromptAccepted) {
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), accepted.clone());
    }

    /// What the agent offers, and the mode it is in.
    fn set_modes(&self, modes: Option<acpilot_proto::session::ModeState>) {
        *self.modes.lock().unwrap_or_else(PoisonError::into_inner) = modes;
    }

    /// The modes this agent advertised, when it advertised any.
    #[must_use]
    pub fn modes(&self) -> Option<acpilot_proto::session::ModeState> {
        self.modes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Follow a mode switch the agent accepted.
    fn set_mode(&self, mode: &str) {
        let mut modes = self.modes.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(state) = modes.as_mut() {
            state.mode = mode.to_owned();
        }
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
    let fs = Arc::new(Fs::with_broker(Arc::new(LocalHost), Arc::clone(broker)));
    let options = options_for(state, &id, &spec.id, fs);
    // The session opens before its record exists, so an agent that will not start
    // leaves no transcript behind: a file with a `meta` line and no conversation is
    // worse than no file, and it would hold the id forever.
    let session = start(Arc::clone(&spec), options, workspace.root.as_path()).await?;

    let (record, mailbox) = Record::new(
        id.clone(),
        workspace.id.clone(),
        spec.id.clone(),
        Some(Arc::clone(broker)),
        state.transcripts(),
        state.store().cloned(),
    );
    if let Some(title) = &request.title {
        record.title_from(title);
    }
    // The agent may have chosen its own name for the conversation; a terminal it
    // opens is keyed by that, so the pane store needs the way back.
    record.set_acp_id(session.acp_session_id());
    // What the agent says it can do, before the first client asks. Copied while
    // the session is still here: the owner task takes it below.
    record.set_modes(session.modes());

    // The session is moved onto its own thread before the route can return, so
    // no client ever sees a session id that has nobody listening on it.
    spawn_owner(session, Arc::clone(&record), mailbox);
    state.sessions.insert(Arc::clone(&record));
    // Stored before the route answers, so a client that creates a session and asks
    // about it after a crash finds it rather than a gap between two records.
    if let Some(store) = state.store()
        && let Err(error) = store.save_session(&record.summary())
    {
        tracing::warn!(%error, session = %id, "the session was not recorded");
    }
    Ok(record.summary())
}

/// The options a session is opened with.
///
/// One function so the two registries a session is handed are visible together: a
/// filesystem answers `fs/*` and the daemon's agent-terminal registry answers
/// `terminal/*`. Each is also the switch that advertises the capability, so a
/// session can never claim one it cannot serve.
///
/// The terminal registry is the daemon's, not a fresh one: a terminal an agent
/// opens is owned by this session id, and one store means the idle sweep and the
/// session's own release see the same processes.
fn options_for(state: &AppState, id: &str, agent: &str, fs: Arc<Fs>) -> SessionOptions {
    SessionOptions::new(id.to_owned())
        .agent(agent.to_owned())
        .fs(fs)
        .terminals(Arc::clone(state.agent_terminals()))
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
            runtime.block_on(drive(session, record.clone(), mailbox));
            // The session is over: whatever it decided on the way out belongs in
            // its file, and nothing else will ask the broker for it again.
            record.flush_grants();
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
                // A mode switch between turns is the ordinary case: nothing is
                // in flight, so the agent can answer at once.
                Some(Command::Mode(mode)) => match session.set_mode(mode.clone()).await {
                    Ok(()) => record.set_mode(&mode),
                    Err(error) => record.notice("Mode not changed", &error.to_string()),
                },
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
    record.count_prompt(&text);
    let turn = session.prompt(text);
    tokio::pin!(turn);
    loop {
        tokio::select! {
            outcome = &mut turn => {
                record.set_turn(None);
                // The turn is over, so every permission it asked about has an
                // answer; the file learns them now rather than one write per ask.
                record.flush_grants();
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
                // Switching modes mid-turn is not refused by ACP, but an agent
                // that changed its rules halfway through a turn would make the
                // transcript a lie about which rules produced which part. The
                // client can ask again when the turn ends.
                Some(Command::Mode(_)) => record.notice(
                    "Mode refused",
                    "a turn is in flight; the mode was not changed",
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
///
/// "Never reused" now outlives the process: a transcript on disk is the record of
/// a session id that a client may still hold, so a restarted daemon's first
/// conversation does not open at `s1` if `s1` already has a file. The count of live
/// sessions is only a starting point, not the answer.
fn next_id(state: &AppState) -> SessionId {
    let mut number = state.sessions.list().len() + 1;
    let recorded: Vec<String> = state
        .store()
        .and_then(|store| store.session_ids().ok())
        .unwrap_or_default();
    loop {
        let id = format!("s{number}");
        let live = state.sessions.get(&id).is_some();
        // `<transcript>/<id>/session.jsonl` is how a stored conversation is named;
        // its directory is the one thing a restart still has to compare with, and
        // the database is the second because a transcript can be turned off.
        let stored = state
            .transcripts()
            .is_some_and(|directory| directory.join(&id).exists())
            || recorded.iter().any(|recorded| recorded == &id);
        if !live && !stored {
            return id;
        }
        number += 1;
    }
}

/// `GET /v1/sessions`.
pub async fn list(State(state): State<AppState>) -> Json<Vec<SessionSummary>> {
    let mut summaries = state.sessions.list();
    // Sessions this process did not start are still this profile's sessions. They
    // are listed from the store rather than hidden, because a conversation that is
    // not in the list is one a client cannot open at all.
    if let Some(store) = state.store() {
        match store.sessions() {
            Ok(stored) => {
                for summary in stored {
                    if !summaries.iter().any(|live| live.id == summary.id) {
                        summaries.push(summary);
                    }
                }
                summaries.sort_by(|left, right| {
                    left.created_at
                        .cmp(&right.created_at)
                        .then_with(|| left.id.cmp(&right.id))
                });
            }
            Err(error) => tracing::warn!(%error, "the stored sessions could not be listed"),
        }
    }
    Json(pending_aware(summaries, &state))
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
    let summary = match state.sessions.get(&id) {
        Some(summary) => summary,
        None => state
            .store()
            .and_then(|store| store.session(&id).ok().flatten())
            .ok_or_else(|| unknown(&id))?,
    };
    Ok(Json(pending_aware(vec![summary], &state).remove(0)))
}

/// `DELETE /v1/sessions/{id}`.
pub async fn remove(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<SessionSummary>, ApiError> {
    let summary = state.sessions.get(&id).ok_or_else(|| unknown(&id))?;
    state.sessions.remove(&id);
    // And its panes: a shell nobody can list is a process nobody can find.
    state.terminals().close_session(&id);
    // Deleting ends the session in the record as well: leaving it `idle` would keep
    // it in every later list, which is the opposite of what the caller asked for.
    if let Some(store) = state.store()
        && let Err(error) = store.set_status(
            &id,
            &SessionStatus::Closed,
            &jiff::Timestamp::now().to_string(),
        )
    {
        tracing::warn!(%error, session = %id, "the deletion was not recorded");
    }
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
    // Before the busy check, not after: a client that retried the request it sent
    // a moment ago must get the same answer even though that answer started a turn
    // which is, by now, in flight.
    if let Some(key) = request.idempotency_key.as_deref()
        && let Some(accepted) = record.remembered(key)
    {
        return Ok((StatusCode::ACCEPTED, Json(accepted)));
    }
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

    let accepted = PromptAccepted {
        session_id: id,
        turn_id,
        first_seq,
    };
    if let Some(key) = request.idempotency_key.as_deref() {
        record.remember(key, &accepted);
    }
    // `202`: the turn was accepted and has not run. The transcript arrives on
    // the session's stream, which is where every client reads it.
    Ok((StatusCode::ACCEPTED, Json(accepted)))
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

/// `POST /v1/sessions/{id}/mode` — switch the agent's mode.
///
/// The agent is the authority: the list answered here is what it advertised at
/// `session/new`, and an id it did not advertise is `400` with the alternatives in
/// `details` rather than a request that fails later inside the agent. An agent that
/// offers no modes at all is `409`: there is nothing to switch, and a `200` for a
/// mode nobody can enter would be worse than saying so.
///
/// The answer is the state the switch was sent to; the agent's own `mode_changed`
/// event on the session's stream is what confirms it, and the session's copy follows
/// that event.
pub async fn set_mode(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    body: Result<Json<SetModeRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<ModeState>, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a mode request: {rejection}"
        )))
    })?;
    let record = state.sessions.handle(&id).ok_or_else(|| unknown(&id))?;
    if record.status() == SessionStatus::Closed {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "session {id} has ended; there is no agent to switch"
        ))));
    }
    if record.status() == SessionStatus::Busy {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "session {id} has a turn in flight; the mode can change when it ends"
        ))));
    }
    let mode = request.mode.trim();
    if mode.is_empty() {
        return Err(ApiError::new(AcpilotError::invalid("`mode` is empty"))
            .with_detail("mode", "send the id of one of the agent's modes"));
    }
    let Some(modes) = record.modes() else {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "the agent for session {id} offers no modes to switch"
        ))));
    };
    if !modes.available.iter().any(|offered| offered.id == mode) {
        let ids: Vec<&str> = modes
            .available
            .iter()
            .map(|offered| offered.id.as_str())
            .collect();
        return Err(ApiError::new(AcpilotError::invalid(format!(
            "{mode} is not a mode the agent offers"
        )))
        .with_detail("mode", format!("the agent offers {}", ids.join(", "))));
    }
    if record
        .commands
        .try_send(Command::Mode(mode.to_owned()))
        .is_err()
    {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "session {id} is not ready for a mode change"
        ))));
    }
    Ok(Json(ModeState {
        mode: mode.to_owned(),
        available: modes.available,
    }))
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
    let after = query
        .after
        .or_else(|| {
            headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .unwrap_or(0);

    // A conversation this process did not start has no live feed: its events are a
    // file, and the stream is the replay of it. That is the difference between a
    // history and a session, and it is why this branch ends instead of waiting.
    let Some(record) = state.sessions.handle(&id) else {
        return stored_events(&state, &id, after);
    };

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

/// `WS /v1/sessions/{id}/stream?after=N` — the same feed over a socket.
///
/// The SSE route is what a browser takes; this is the one a client that already
/// speaks WebSocket uses, and it starts at the same place: `after`, or the
/// sequence the ring and the database agree on when it is absent. The frames are
/// the event JSON the SSE `data:` lines carry, one per frame, with no `id`
/// alongside — a client that needs the sequence reads it from the event.
///
/// A stored conversation is replayed and the socket closed, exactly as its SSE
/// stream ends: a history has no live feed to wait on.
pub async fn stream(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    Query(query): Query<AfterQuery>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    // Both refusals happen before the upgrade, so they are the API's usual envelope
    // rather than a close frame a client has to interpret.
    if state.sessions.handle(&id).is_none() {
        let Some(store) = state.store() else {
            return Err(unknown(&id));
        };
        if store.session(&id).map_err(store_failed)?.is_none() {
            return Err(unknown(&id));
        }
    }
    let after = query.after.unwrap_or(0);
    Ok(upgrade.on_upgrade(move |socket| serve_stream(state, id, after, socket)))
}

/// Send the replay, then follow the feed until the socket goes away.
async fn serve_stream(state: AppState, id: String, after: u64, mut socket: WebSocket) {
    let Some(record) = state.sessions.handle(&id) else {
        // A stored conversation: everything is already written down, so there is
        // nothing to follow afterwards.
        if let Some(store) = state.store()
            && let Ok(events) = store.events_after(&id, after)
        {
            for event in events {
                if send_event(&mut socket, &event).await.is_err() {
                    return;
                }
            }
        }
        return;
    };

    let mut last = after;
    for event in record.replay_after(after) {
        last = event.seq;
        if send_event(&mut socket, &event).await.is_err() {
            return;
        }
    }

    let mut live = record.subscribe();
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = beat.tick() => {
                // A ping rather than a text frame: a client's `onmessage` should
                // see events, and the transport answers a ping for us.
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            event = live.recv() => match event {
                Ok(event) if event.seq > last => {
                    last = event.seq;
                    if send_event(&mut socket, &event).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let resync = serde_json::json!({ "type": "resync", "latest_seq": last });
                    if socket.send(Message::text(resync.to_string())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            frame = socket.recv() => match frame {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                // Anything else from a client is a frame this route does not
                // speak: pings are answered by the transport, and a client that
                // sends commands here is using the wrong route.
                Some(Ok(_)) => {}
            },
        }
    }
}

/// One event as a socket frame.
async fn send_event(socket: &mut WebSocket, event: &SessionEvent) -> Result<(), axum::Error> {
    socket
        .send(Message::text(
            serde_json::to_string(event).expect("a session event is always serialisable"),
        ))
        .await
}

/// Replay a stored conversation from the database, then end the stream.
///
/// Nothing is replayed twice and nothing is invented: `after` is the same sequence
/// the live stream takes, so a client reconnecting with a `Last-Event-ID` from a
/// session that has since restarted sees the same gap-free answer.
fn stored_events(state: &AppState, id: &str, after: u64) -> Result<Response, ApiError> {
    let Some(store) = state.store() else {
        return Err(unknown(id));
    };
    if store.session(id).map_err(store_failed)?.is_none() {
        return Err(unknown(id));
    }
    let events = store.events_after(id, after).map_err(store_failed)?;
    let frames = events
        .into_iter()
        .map(|event| Ok::<_, std::convert::Infallible>(frame(&event)));
    Ok(Sse::new(futures::stream::iter(frames)).into_response())
}

/// A store failure as the API's own error: the daemon understood the request and
/// could not answer it, which is a `500` rather than a `404`.
fn store_failed(error: acpilot_store::StoreError) -> ApiError {
    ApiError::new(AcpilotError::from(std::io::Error::other(error.to_string())))
}

/// The wire spelling of a decision, so the database says `allow_once` where the
/// transcript says the same thing.
fn option_text(option: &PermissionOption) -> String {
    serde_json::to_value(option)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{option:?}"))
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
        .route("/v1/sessions/{id}/mode", post(set_mode))
        .route("/v1/sessions/{id}/events", get(events))
        .route("/v1/sessions/{id}/stream", get(stream))
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    operation_id = "list_sessions",
    path = "/v1/sessions",
    tag = "sessions",
    summary = "List sessions",
    responses((status = 200, description = "Every session, oldest first", body = Vec<SessionSummary>))
)]
fn list_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    operation_id = "create_session",
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
    operation_id = "session_events",
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
    use acpilot_agent::permission::{Broker, Policy};
    use acpilot_agent::transcript::Entry;

    use super::*;

    #[test]
    fn titles_cut_at_a_word_boundary() {
        let long = "refactor the permission broker so every refusal names the rule that caused it";
        let title = derive_title(long).expect("a title");
        assert!(title.ends_with('…'), "{title}");
        assert!(title.chars().count() <= TITLE_LIMIT + 1, "{title}");
        assert!(!title.contains("  "), "{title}");
    }

    #[tokio::test]
    async fn a_transcript_holds_the_events_and_grants_a_session_made() {
        use acpilot_agent::permission::Subject;
        use acpilot_proto::permission::PermissionKind;

        let transcripts = tempfile::tempdir().expect("temp dir");
        let workspace = tempfile::tempdir().expect("workspace");
        let broker = Arc::new(Broker::with_timeout(
            Policy::default(),
            workspace.path(),
            None,
        ));
        let (record, _mailbox) = Record::new(
            "s1".to_owned(),
            "work".to_owned(),
            "mock".to_owned(),
            Some(Arc::clone(&broker)),
            Some(transcripts.path()),
            None,
        );

        record.notice("Test", "one");
        record.notice("Test", "two");
        // A write inside the workspace, which the shipped policy allows: the
        // interesting half is that the decision reaches the file, not who made it.
        let file = workspace.path().join("notes.md");
        broker
            .ask(
                "s1",
                Subject::new(PermissionKind::FileWrite, "Write notes.md").path(&file),
            )
            .await;
        record.flush_grants();

        let path = transcripts.path().join("s1").join(FILE_NAME);
        let entries = acpilot_agent::transcript::read(&path).expect("the transcript reads back");
        assert!(
            matches!(entries.first(), Some(Entry::Meta(meta)) if meta.session == "s1" && meta.agent == "mock"),
            "the first line says whose conversation this is: {entries:?}"
        );
        let replayed = acpilot_agent::transcript::replay(&path, 1).expect("it replays");
        assert_eq!(
            replayed.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![2],
            "a client that saw sequence 1 is owed the rest"
        );
        let Some(Entry::Grant(grant)) = entries.last() else {
            panic!("the decision is the last record: {entries:?}");
        };
        assert_eq!(grant.session, "s1");
        assert_eq!(grant.decided_by, "default");
        assert!(
            grant
                .path
                .as_deref()
                .is_some_and(|path| path.ends_with("notes.md")),
            "the grant names the file it was about: {grant:?}"
        );
    }

    #[test]
    fn a_stored_transcript_keeps_its_session_id_from_being_reused() {
        let transcripts = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(transcripts.path().join("s1")).expect("a stored session");
        let state = AppState::new().with_transcripts(transcripts.path());
        assert_eq!(
            next_id(&state),
            "s2",
            "a restarted daemon must not open a new conversation on top of a stored one"
        );
        assert_eq!(
            next_id(&AppState::new()),
            "s1",
            "with nowhere to keep a transcript, the count of live sessions is the answer"
        );
    }

    #[test]
    fn a_session_is_handed_the_daemon_terminal_registry() {
        let state = AppState::new();
        let broker = Arc::new(Broker::with_timeout(
            Policy::default(),
            std::env::temp_dir(),
            None,
        ));
        let fs = Arc::new(Fs::with_broker(Arc::new(LocalHost), broker));
        let options = options_for(&state, "s1", "mock", fs);

        assert_eq!(options.id, "s1");
        assert_eq!(options.agent.as_deref(), Some("mock"));
        assert!(options.fs.is_some(), "a session gets a filesystem");
        let terminals = options.terminals.expect("a session advertises `terminal`");
        assert!(
            Arc::ptr_eq(&terminals, state.agent_terminals()),
            "the registry is the daemon's, so a session's terminals are swept and released with it"
        );
    }

    #[test]
    fn a_retried_prompt_remembers_what_it_was_answered() {
        let (record, _mailbox) = Record::new(
            "s1".into(),
            "default".into(),
            "mock".into(),
            None,
            None,
            None,
        );
        let accepted = PromptAccepted {
            session_id: "s1".into(),
            turn_id: "s1:1".into(),
            first_seq: 1,
        };
        record.remember("key-1", &accepted);
        assert_eq!(record.remembered("key-1"), Some(accepted));
        assert!(
            record.remembered("key-2").is_none(),
            "a different key is a different prompt"
        );
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
        let (record, _mailbox) = Record::new(
            "s1".into(),
            "default".into(),
            "mock".into(),
            None,
            None,
            None,
        );
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
        let (record, _mailbox) = Record::new(
            "s1".into(),
            "default".into(),
            "mock".into(),
            None,
            None,
            None,
        );
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
        let (record, _mailbox) = Record::new(
            "s1".into(),
            "default".into(),
            "claude".into(),
            None,
            None,
            None,
        );
        assert_eq!(record.summary().title, "claude session");
        record.title_from("update the readme");
        assert_eq!(record.summary().title, "update the readme");
        // A second prompt does not rename a conversation.
        record.title_from("something else");
        assert_eq!(record.summary().title, "update the readme");
    }

    #[test]
    fn a_conversation_is_counted_as_it_is_recorded() {
        let (record, _mailbox) = Record::new(
            "s1".into(),
            "default".into(),
            "mock".into(),
            None,
            None,
            None,
        );
        assert!(
            record.context().is_none(),
            "a session nobody has spoken to reports nothing rather than zero"
        );

        record.count_prompt("explain this function");
        record.record(stamped(
            "s1",
            SessionEventKind::MessageChunk,
            serde_json::json!({ "role": "assistant", "content": { "text": "it sorts a list" } }),
            "2026-10-30T12:00:00Z",
        ));

        // 21 characters of prompt and 14 of answer, four to a token.
        let context = record.context().expect("a reading");
        assert_eq!(context.used_tokens, 9);
        assert_eq!(
            context.source,
            acpilot_proto::stats::StatSource::Estimated,
            "a count, not a measurement"
        );
        assert_eq!(context.window_tokens, None);
    }

    #[test]
    fn an_agents_reading_is_its_newest_conversation() {
        let sessions = Sessions::new();
        let (older, _a) = Record::new(
            "s1".into(),
            "default".into(),
            "mock".into(),
            None,
            None,
            None,
        );
        let (newer, _b) = Record::new(
            "s2".into(),
            "default".into(),
            "mock".into(),
            None,
            None,
            None,
        );
        let (elsewhere, _c) = Record::new(
            "s3".into(),
            "default".into(),
            "elsewhere".into(),
            None,
            None,
            None,
        );
        older.count_prompt("the first conversation");
        newer.count_prompt("the second conversation, which is longer");
        elsewhere.count_prompt("somebody else's conversation entirely");
        // Stamped explicitly, so the order is the clock's and not the map's.
        older.record(stamped(
            "s1",
            SessionEventKind::Notice,
            serde_json::json!({ "title": "test" }),
            "2026-10-30T12:00:00.000000000Z",
        ));
        newer.record(stamped(
            "s2",
            SessionEventKind::Notice,
            serde_json::json!({ "title": "test" }),
            "2026-10-30T12:05:00.000000000Z",
        ));
        elsewhere.record(stamped(
            "s3",
            SessionEventKind::Notice,
            serde_json::json!({ "title": "test" }),
            "2026-10-30T12:09:00.000000000Z",
        ));
        sessions.insert(older);
        sessions.insert(newer);
        sessions.insert(elsewhere);

        let (context, at) = sessions.reading("mock").expect("a reading");
        assert_eq!(
            at, "2026-10-30T12:05:00.000000000Z",
            "the newest conversation with this agent, and its own clock"
        );
        assert!(
            context.used_tokens >= 8,
            "the second conversation's prompt, not the first's: {context:?}"
        );
        assert!(sessions.reading("nobody").is_none());
    }

    fn stamped(
        id: &str,
        kind: SessionEventKind,
        payload: serde_json::Value,
        at: &str,
    ) -> SessionEvent {
        SessionEvent {
            seq: 1,
            session_id: id.to_owned(),
            kind,
            at: at.to_owned(),
            payload,
        }
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
