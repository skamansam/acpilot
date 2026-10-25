//! The half of the TUI that talks to the daemon.
//!
//! The UI thread never awaits. A worker thread owns a small Tokio runtime, makes
//! the calls, and turns them into [`Update`]s; the UI turns key presses into
//! [`Request`]s. That split is what keeps a slow daemon from freezing the
//! drawing loop, and it is the same shape a GUI or web client would use, because
//! all of them are clients of the same API.
//!
//! Nothing here decides policy. A prompt goes to the daemon because the daemon
//! runs the turn; a permission answer goes to the daemon because the broker owns
//! the pending list and the audit row. The TUI is a view with a keyboard.

use std::time::Duration;

use acpilot_client::{Client, ClientError, ProfileStream, SessionStream};
use acpilot_proto::agent::AgentSummary;
use acpilot_proto::event::SessionEvent;
use acpilot_proto::permission::{PermissionOption, PermissionRequest};
use acpilot_proto::session::{CreateSessionRequest, SessionSummary};
use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// How long the session list waits before asking again.
///
/// Status is not on the event stream — a turn ending changes a summary, not the
/// transcript — so a poll is how a finished turn stops looking busy. One second
/// is faster than a person reads the status line, and the request is tiny.
const POLL: Duration = Duration::from_secs(1);

/// How long to wait before reopening a stream that failed.
const RECONNECT: Duration = Duration::from_secs(2);

/// What the UI asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Re-read sessions, agents and pending permissions.
    Refresh,
    /// Open a session, replaying its transcript from the beginning.
    Select(String),
    /// Send a prompt to the open session.
    Prompt(String),
    /// Cancel the turn in flight.
    Cancel,
    /// Start a session with this agent and open it.
    New(String),
    /// End a session.
    Delete(String),
    /// Answer a pending request.
    Resolve {
        /// Request id, as listed.
        id: String,
        /// What to answer.
        option: PermissionOption,
    },
    /// Stop the worker.
    Quit,
}

/// What the worker tells the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// The daemon answered, and says which version it is.
    Health {
        /// Version string.
        version: String,
    },
    /// The profile's agents.
    Agents(Vec<AgentSummary>),
    /// The session list.
    Sessions(Vec<SessionSummary>),
    /// A session was opened; its transcript starts empty.
    Opened {
        /// Session id.
        id: String,
    },
    /// One event on the open session.
    Event(SessionEvent),
    /// The pending permission list, as the daemon has it now.
    Permissions(Vec<PermissionRequest>),
    /// A turn was accepted.
    Prompted {
        /// Session id.
        session_id: String,
        /// Sequence the turn's output starts at.
        first_seq: u64,
        /// Identifier for the turn.
        turn_id: String,
    },
    /// Something the user should read, but which is not a transcript event.
    Status(String),
}

/// The worker's handle: where requests go, and the thread running it.
pub struct Worker {
    requests: UnboundedSender<Request>,
}

impl Worker {
    /// Send a request. A closed channel means the worker is gone, which the UI
    /// treats as a failed daemon and shows in the status line.
    pub fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }
}

/// Start the worker for the daemon at `daemon`, with `token` when it has one.
pub fn spawn(daemon: String, token: Option<String>, updates: UnboundedSender<Update>) -> Worker {
    let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("acpilot-tui-client".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    let client = match token {
                        Some(token) => Client::with_token(daemon, token),
                        None => Client::new(daemon),
                    };
                    runtime.block_on(run(client, receiver, updates))
                }
                Err(error) => {
                    let _ = updates.send(Update::Status(format!(
                        "no runtime for the daemon client: {error}"
                    )));
                }
            }
        })
        .expect("the TUI client thread starts");
    Worker { requests }
}

async fn run(
    client: Client,
    mut requests: UnboundedReceiver<Request>,
    updates: UnboundedSender<Update>,
) {
    match client.health().await {
        Ok(health) => {
            let _ = updates.send(Update::Health {
                version: health.version,
            });
        }
        Err(error) => {
            let _ = updates.send(Update::Status(format!(
                "cannot reach {}: {error}",
                client.base()
            )));
        }
    }
    refresh(&client, &updates).await;

    // The profile-wide stream is what raises a permission card without the user
    // pressing anything. It runs for the life of the TUI.
    {
        let client = client.clone();
        let updates = updates.clone();
        tokio::spawn(async move { follow_permissions(client, updates).await });
    }

    let mut open: Option<(String, tokio::task::JoinHandle<()>)> = None;
    let mut poll = tokio::time::interval(POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = poll.tick() => {
                match client.sessions().await {
                    Ok(sessions) => { let _ = updates.send(Update::Sessions(sessions)); }
                    // A daemon that went away is reported once per poll; the
                    // status line is a line, not a log.
                    Err(error) => { let _ = updates.send(Update::Status(describe(&error))); }
                }
            }
            request = requests.recv() => match request {
                None | Some(Request::Quit) => {
                    if let Some((_, task)) = open.take() {
                        task.abort();
                    }
                    return;
                }
                Some(Request::Refresh) => refresh(&client, &updates).await,
                Some(Request::Select(id)) => {
                    if let Some((existing, _)) = &open
                        && existing == &id
                    {
                        continue;
                    }
                    if let Some((_, task)) = open.take() {
                        task.abort();
                    }
                    let _ = updates.send(Update::Opened { id: id.clone() });
                    let task = {
                        let client = client.clone();
                        let updates = updates.clone();
                        let id = id.clone();
                        tokio::spawn(async move { follow_session(client, id, updates).await })
                    };
                    open = Some((id, task));
                }
                Some(Request::Prompt(text)) => {
                    let Some((id, _)) = &open else {
                        let _ = updates.send(Update::Status("no session is open".to_owned()));
                        continue;
                    };
                    match client.prompt(id, text).await {
                        Ok(accepted) => {
                            let _ = updates.send(Update::Prompted {
                                session_id: accepted.session_id,
                                first_seq: accepted.first_seq,
                                turn_id: accepted.turn_id,
                            });
                            refresh_sessions(&client, &updates).await;
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::Cancel) => {
                    let Some((id, _)) = &open else { continue };
                    match client.cancel(id).await {
                        Ok(outcome) if outcome.cancelled => {
                            let turn = outcome.turn_id.unwrap_or_default();
                            let _ = updates.send(Update::Status(format!("cancel sent for {turn}")));
                        }
                        Ok(_) => {
                            let _ = updates.send(Update::Status("no turn was running".to_owned()));
                        }
                        Err(error) => { let _ = updates.send(Update::Status(describe(&error))); }
                    }
                }
                Some(Request::New(agent_id)) => {
                    let request = CreateSessionRequest {
                        // The daemon serves one workspace; an empty id means it.
                        workspace_id: String::new(),
                        agent_id,
                        title: None,
                    };
                    match client.create_session(&request).await {
                        Ok(summary) => {
                            let _ = updates.send(Update::Opened {
                                id: summary.id.clone(),
                            });
                            if let Some((_, task)) = open.take() {
                                task.abort();
                            }
                            let task = {
                                let client = client.clone();
                                let updates = updates.clone();
                                let id = summary.id.clone();
                                tokio::spawn(async move { follow_session(client, id, updates).await })
                            };
                            open = Some((summary.id.clone(), task));
                            refresh_sessions(&client, &updates).await;
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::Delete(id)) => {
                    if let Some((existing, _)) = &open
                        && existing == &id
                        && let Some((_, task)) = open.take()
                    {
                        task.abort();
                    }
                    match client.delete_session(&id).await {
                        Ok(summary) => {
                            let _ = updates.send(Update::Status(format!("ended {}", summary.id)));
                            refresh_sessions(&client, &updates).await;
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::Resolve { id, option }) => {
                    match client.resolve(&id, option).await {
                        Ok(decision) => {
                            let _ = updates.send(Update::Status(format!(
                                "answered {}: {}",
                                id, decision.decided_by
                            )));
                            refresh_permissions(&client, &updates).await;
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                            refresh_permissions(&client, &updates).await;
                        }
                    }
                }
            }
        }
    }
}

/// Re-read everything the UI shows at startup.
async fn refresh(client: &Client, updates: &UnboundedSender<Update>) {
    match client.agents().await {
        Ok(agents) => {
            let _ = updates.send(Update::Agents(agents));
        }
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
        }
    }
    refresh_sessions(client, updates).await;
    refresh_permissions(client, updates).await;
}

async fn refresh_sessions(client: &Client, updates: &UnboundedSender<Update>) {
    match client.sessions().await {
        Ok(sessions) => {
            let _ = updates.send(Update::Sessions(sessions));
        }
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
        }
    }
}

async fn refresh_permissions(client: &Client, updates: &UnboundedSender<Update>) {
    match client.pending_permissions().await {
        Ok(pending) => {
            let _ = updates.send(Update::Permissions(pending));
        }
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
        }
    }
}

/// Follow one session, reconnecting with the last sequence seen.
async fn follow_session(client: Client, id: String, updates: UnboundedSender<Update>) {
    let mut after = 0;
    loop {
        let frame = client.session_events(&id, after);
        tokio::pin!(frame);
        while let Some(item) = frame.next().await {
            match item {
                Ok(SessionStream::Event(event)) => {
                    after = event.seq;
                    if updates.send(Update::Event(event)).is_err() {
                        return;
                    }
                }
                Ok(SessionStream::Heartbeat { seq }) => after = after.max(seq),
                Ok(SessionStream::Resync { latest_seq }) => {
                    // The stream cannot replay what it dropped; asking again
                    // from where it now is is the only honest recovery.
                    after = latest_seq;
                    break;
                }
                Ok(SessionStream::Other { .. }) => {}
                Err(error) => {
                    let _ = updates.send(Update::Status(format!(
                        "event stream: {}",
                        describe(&error)
                    )));
                    break;
                }
            }
        }
        tokio::time::sleep(RECONNECT).await;
        if after == 0 {
            // A first connection that never worked: say so once, then keep
            // trying quietly rather than filling the status line every 2 s.
            let _ = updates.send(Update::Status(format!(
                "waiting for session {id} on the daemon"
            )));
            return;
        }
    }
}

/// Follow the daemon's permission announcements.
async fn follow_permissions(client: Client, updates: UnboundedSender<Update>) {
    loop {
        let frames = client.profile_events();
        tokio::pin!(frames);
        while let Some(item) = frames.next().await {
            match item {
                Ok(ProfileStream::Permission(request)) => {
                    let _ = updates.send(Update::Status(format!(
                        "{} wants to {}",
                        request.session_id,
                        request.title.to_lowercase()
                    )));
                }
                Ok(ProfileStream::Resync) | Ok(ProfileStream::Heartbeat) => {
                    let _ = refresh_permissions(&client, &updates).await;
                }
                Ok(ProfileStream::Other { .. }) => {}
                Err(error) => {
                    let _ = updates.send(Update::Status(format!(
                        "permission stream: {}",
                        describe(&error)
                    )));
                    break;
                }
            }
            // The announcement says a request exists; the pending list is the
            // state. Refetching keeps the modal and the list from disagreeing.
            let _ = refresh_permissions(&client, &updates).await;
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

/// A failure as a status line, with its code when the daemon sent one.
fn describe(error: &ClientError) -> String {
    match error.code() {
        Some(code) => format!("{code}: {error}"),
        None => error.to_string(),
    }
}
