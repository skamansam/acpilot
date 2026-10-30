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

use acpilot_client::terminal::{Terminal as Attached, TerminalEvent};
use acpilot_client::{Client, ClientError, ProfileStream, SessionStream};
use acpilot_proto::agent::AgentSummary;
use acpilot_proto::event::SessionEvent;
use acpilot_proto::permission::{PermissionOption, PermissionRequest};
use acpilot_proto::session::{CreateSessionRequest, SessionSummary};
use acpilot_proto::stats::IntegrationStats;
use acpilot_proto::terminal::{TerminalSize, TerminalSummary};
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

/// How long the stats bar waits before asking again.
///
/// Slower than the session poll on purpose: a context window and a balance move
/// over a turn, not over a keystroke, and a row that arrives already knowing how
/// old it is (`updated_at`) needs no faster clock to be honest.
const STATS_POLL: Duration = Duration::from_secs(15);

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
    /// Attach to one of the open session's terminals, by id.
    OpenTerminal {
        /// The terminal to attach to, as the daemon listed it.
        id: String,
    },
    /// Bytes for the attached terminal, as if typed.
    TerminalWrite(Vec<u8>),
    /// The pane changed shape; tell the program.
    TerminalResize(TerminalSize),
    /// Leave the terminal. The program keeps running — a pane is a view.
    CloseTerminal,
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
    /// The integrations the bar draws, as the daemon reported them.
    Stats(Vec<IntegrationStats>),
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
    /// The open session's terminals, as the daemon lists them.
    Terminals(Vec<TerminalSummary>),
    /// The attach succeeded. The screen replay follows as [`Update::TerminalData`].
    TerminalOpened {
        /// The terminal, as the daemon sees it.
        terminal: TerminalSummary,
        /// How many bytes of screen replay follow.
        replay_bytes: usize,
    },
    /// Screen bytes: the replay, then every chunk as it is produced.
    TerminalData(Vec<u8>),
    /// The program ended.
    TerminalExited {
        /// Exit code, when the host reported one.
        code: Option<i32>,
    },
    /// The attach is over — after an exit, or because the socket went away.
    TerminalClosed,
    /// The daemon's preferences, resolved into the theme to draw with.
    ///
    /// Separate from [`Update::Theme`], which is a settings file this client can
    /// read itself: the daemon's answer is the authoritative one, so the first of
    /// these takes over from the file watcher rather than racing it.
    DaemonTheme(crate::Theme),
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
    /// The settings named a different theme, and it resolved.
    ///
    /// Not from the daemon: the watcher in [`crate::watch_theme`] sends this, and
    /// it arrives through the same channel as everything else so a theme change is
    /// one more thing the loop applies rather than a second path into the state.
    Theme(crate::theme::Theme),
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
pub fn spawn(
    daemon: String,
    token: Option<String>,
    terminal: Option<crate::TerminalColours>,
    updates: UnboundedSender<Update>,
) -> Worker {
    let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("acpilot-tui-client".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    let client = match &token {
                        Some(token) => Client::with_token(daemon, token),
                        None => Client::new(daemon),
                    };
                    // The attach needs the token too, and the client keeps it
                    // private: the worker is the one that resolves it, so it is
                    // passed along rather than looked up again.
                    runtime.block_on(run(client, token, terminal, receiver, updates))
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

/// What the worker sends the attach task.
#[derive(Debug)]
enum TerminalCommand {
    /// Bytes to type.
    Write(Vec<u8>),
    /// A new shape for the screen.
    Resize(TerminalSize),
    /// Leave, without ending the program.
    Close,
}

/// The task holding one attach, and the way to talk to it.
struct TerminalLink {
    commands: UnboundedSender<TerminalCommand>,
    task: tokio::task::JoinHandle<()>,
}

async fn run(
    client: Client,
    token: Option<String>,
    terminal: Option<crate::TerminalColours>,
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
    // pressing anything, and what says the settings changed when this client
    // cannot see the daemon's disk itself. It runs for the life of the TUI.
    {
        let client = client.clone();
        let updates = updates.clone();
        tokio::spawn(async move { follow_permissions(client, terminal, updates).await });
    }

    let mut open: Option<(String, tokio::task::JoinHandle<()>)> = None;
    // One attach at a time, like the pane. The task owns the socket; this is how
    // the loop types into it and how it is stopped.
    let mut terminal: Option<TerminalLink> = None;
    let mut poll = tokio::time::interval(POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stats = tokio::time::interval(STATS_POLL);
    stats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = poll.tick() => {
                match client.sessions().await {
                    Ok(sessions) => { let _ = updates.send(Update::Sessions(sessions)); }
                    // A daemon that went away is reported once per poll; the
                    // status line is a line, not a log.
                    Err(error) => { let _ = updates.send(Update::Status(describe(&error))); }
                }
                // The terminal list changes when an agent opens one, which is
                // exactly when a person wants to attach, so it is polled too.
                if let Some((id, _)) = &open {
                    refresh_terminals(&client, id, &updates).await;
                }
            }
            _ = stats.tick() => refresh_stats(&client, &updates).await,
            request = requests.recv() => match request {
                None | Some(Request::Quit) => {
                    if let Some((_, task)) = open.take() {
                        task.abort();
                    }
                    if let Some(link) = terminal.take() {
                        link.task.abort();
                    }
                    return;
                }
                // Which terminal is the app's question, not the worker's: it is
                // the app that has the list on screen and the picker in front of
                // it. A dead terminal still has a screen worth looking at, so a
                // closed one attaches the same way.
                Some(Request::OpenTerminal { id }) => {
                    if let Some(link) = terminal.take() {
                        link.task.abort();
                    }
                    let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
                    let task = tokio::spawn(follow_terminal(
                        client.base().to_owned(),
                        token.clone(),
                        id,
                        receiver,
                        updates.clone(),
                    ));
                    terminal = Some(TerminalLink { commands, task });
                }
                Some(Request::TerminalWrite(bytes)) => {
                    if let Some(link) = &terminal {
                        let _ = link.commands.send(TerminalCommand::Write(bytes));
                    }
                }
                Some(Request::TerminalResize(size)) => {
                    if let Some(link) = &terminal {
                        let _ = link.commands.send(TerminalCommand::Resize(size));
                    }
                }
                Some(Request::CloseTerminal) => {
                    if let Some(link) = terminal.take() {
                        let _ = link.commands.send(TerminalCommand::Close);
                    }
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
                    refresh_terminals(&client, &id, &updates).await;
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
                            refresh_terminals(&client, &summary.id, &updates).await;
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
    refresh_stats(client, updates).await;
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

/// Read the open session's terminals.
///
/// Sent as soon as a session opens as well as on every poll: an agent's shell is
/// exactly what a person presses `Ctrl-T` for, and a list that arrives a second
/// later is a pane that says the session has none.
async fn refresh_terminals(client: &Client, session: &str, updates: &UnboundedSender<Update>) {
    match client.terminals(Some(session)).await {
        Ok(terminals) => {
            let _ = updates.send(Update::Terminals(terminals));
        }
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
        }
    }
}

/// Read the integrations the stats bar draws.
///
/// A daemon that answers with no rows is a daemon with no roster, which the bar
/// says; a request that fails leaves the last rows on screen rather than blanking a
/// bar a person is reading — the status line already reports the failure.
async fn refresh_stats(client: &Client, updates: &UnboundedSender<Update>) {
    match client.stats().await {
        Ok(stats) => {
            let _ = updates.send(Update::Stats(stats));
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

/// Hold one terminal attach: type into it, resize it, and forward its screen.
///
/// A task rather than a branch of the worker's loop because the socket is one
/// more source of frames and the worker is already selecting over three; giving
/// the attach its own loop is what keeps a quiet terminal from delaying a
/// keystroke or a poll.
async fn follow_terminal(
    endpoint: String,
    token: Option<String>,
    id: String,
    mut commands: UnboundedReceiver<TerminalCommand>,
    updates: UnboundedSender<Update>,
) {
    let mut terminal = match Attached::open(&endpoint, token.as_deref(), &id).await {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
            let _ = updates.send(Update::TerminalClosed);
            return;
        }
    };

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(TerminalCommand::Write(bytes)) => {
                    if let Err(error) = terminal.write(&bytes).await {
                        let _ = updates.send(Update::Status(describe(&error)));
                        break;
                    }
                }
                Some(TerminalCommand::Resize(size)) => {
                    if let Err(error) = terminal.resize(size).await {
                        let _ = updates.send(Update::Status(describe(&error)));
                        break;
                    }
                }
                Some(TerminalCommand::Close) | None => break,
            },
            frame = terminal.next() => match frame {
                Ok(Some(TerminalEvent::Attached {
                    terminal,
                    replay_bytes,
                })) => {
                    let _ = updates.send(Update::TerminalOpened {
                        terminal,
                        replay_bytes,
                    });
                }
                Ok(Some(TerminalEvent::Data(bytes))) => {
                    let _ = updates.send(Update::TerminalData(bytes));
                }
                Ok(Some(TerminalEvent::Exit { code })) => {
                    // The daemon closes after this, so the exit is the last thing
                    // said; the close below is just the socket agreeing.
                    let _ = updates.send(Update::TerminalExited { code });
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = updates.send(Update::Status(describe(&error)));
                    break;
                }
            },
        }
    }
    let _ = updates.send(Update::TerminalClosed);
}

/// Follow the daemon's permission announcements and its settings.
async fn follow_permissions(
    client: Client,
    terminal: Option<crate::TerminalColours>,
    updates: UnboundedSender<Update>,
) {
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
                // The stream opens with the current preferences, so a terminal
                // that shares no filesystem with its daemon still starts with the
                // theme the daemon would draw with, and a save arrives as the next
                // one. A theme this build cannot resolve is said out loud rather
                // than ignored: it is a preference, and somebody chose it.
                Ok(ProfileStream::Preferences(preferences)) => {
                    match crate::theme_for(&preferences, terminal) {
                        Some(theme) => {
                            let _ = updates.send(Update::DaemonTheme(theme));
                        }
                        None => {
                            let _ = updates.send(Update::Status(
                                "the daemon's preferences name a theme this build cannot \
                                 resolve; keeping the colours on screen"
                                    .to_owned(),
                            ));
                        }
                    }
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

/// Start the design pass's worker: the seeded world instead of a daemon.
///
/// It speaks the same [`Update`] channel the real worker does, so the UI cannot
/// tell the difference — which is the point of the exercise. What changes is who
/// answers: the fixture replays a stored transcript when a session opens, replies
/// to a prompt from a script, and hands a terminal attach a screen it kept.
/// Everything a pane draws is still the pane's own code.
pub fn spawn_demo(demo: acpilot_demo::Demo, updates: UnboundedSender<Update>) -> Worker {
    let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("acpilot-tui-demo".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(run_demo(demo, receiver, updates)),
                Err(error) => {
                    let _ = updates.send(Update::Status(format!(
                        "no runtime for the seeded world: {error}"
                    )));
                }
            }
        })
        .expect("the demo thread starts");
    Worker { requests }
}

/// How long the seeded transcript takes between events.
///
/// Slow enough to see a chunk arrive and fast enough that a design review does
/// not wait: the point is that the panes work while events stream, not that the
/// fixture is quick.
const REPLAY: Duration = Duration::from_millis(45);

async fn run_demo(
    demo: acpilot_demo::Demo,
    mut requests: UnboundedReceiver<Request>,
    updates: UnboundedSender<Update>,
) {
    announce(&demo, &updates);
    // Sequences continue past the seeded transcript, so a scripted reply can never
    // collide with a replayed event and be dropped as a duplicate.
    let mut seq = demo
        .sessions
        .iter()
        .flat_map(|fixture| fixture.events.iter().map(|event| event.seq))
        .max()
        .unwrap_or(0);

    while let Some(request) = requests.recv().await {
        match request {
            Request::Quit => return,
            Request::Select(id) => {
                let Some(fixture) = demo
                    .sessions
                    .iter()
                    .find(|fixture| fixture.summary.id == id)
                else {
                    let _ = updates.send(Update::Status(format!(
                        "the design pass has no session {id}"
                    )));
                    continue;
                };
                let _ = updates.send(Update::Opened {
                    id: fixture.summary.id.clone(),
                });
                for event in &fixture.events {
                    let _ = updates.send(Update::Event(event.clone()));
                    tokio::time::sleep(REPLAY).await;
                }
            }
            Request::Prompt(_) => {
                // The composer works, the turn does not: the reply is a script, so
                // the pane shows what streaming looks like without pretending a
                // model answered.
                let session = demo
                    .sessions
                    .first()
                    .map(|fixture| fixture.summary.id.clone())
                    .unwrap_or_default();
                for event in acpilot_demo::scripted_reply(seq + 1, &session) {
                    seq = event.seq;
                    let _ = updates.send(Update::Event(event));
                    tokio::time::sleep(REPLAY).await;
                }
                let _ = updates.send(Update::Status(
                    "design pass · the reply is scripted; a real turn needs the daemon".to_owned(),
                ));
            }
            Request::OpenTerminal { id } => {
                let terminal = demo
                    .terminals
                    .iter()
                    .find(|terminal| terminal.id == id)
                    .or_else(|| demo.terminals.first());
                let Some(terminal) = terminal.cloned() else {
                    let _ = updates.send(Update::Status(
                        "the design pass has no terminal to attach".to_owned(),
                    ));
                    continue;
                };
                let screen = demo.terminal_screen.clone().into_bytes();
                let _ = updates.send(Update::TerminalOpened {
                    terminal,
                    replay_bytes: screen.len(),
                });
                let _ = updates.send(Update::TerminalData(screen));
            }
            Request::CloseTerminal => {
                let _ = updates.send(Update::TerminalClosed);
            }
            Request::Resolve { id, option } => {
                let _ = updates.send(Update::Status(format!(
                    "design pass · answered {id} with {option:?} · nothing was written"
                )));
                let _ = updates.send(Update::Permissions(Vec::new()));
            }
            Request::Refresh => announce(&demo, &updates),
            // A keystroke into the fixture's shell is not answered: the screen it
            // shows is the one it kept, and pretending to run it would be the one
            // thing a design pass must not do.
            Request::TerminalWrite(_) | Request::TerminalResize(_) => {}
            Request::New(_) | Request::Delete(_) | Request::Cancel => {
                let _ = updates.send(Update::Status(
                    "design pass · there is no daemon to ask".to_owned(),
                ));
            }
        }
    }
}

/// Tell the UI what the profile has, the way the daemon's first poll would.
fn announce(demo: &acpilot_demo::Demo, updates: &UnboundedSender<Update>) {
    let _ = updates.send(Update::Agents(demo.agents.clone()));
    let _ = updates.send(Update::Sessions(
        demo.sessions
            .iter()
            .map(|fixture| fixture.summary.clone())
            .collect(),
    ));
    let _ = updates.send(Update::Terminals(demo.terminals.clone()));
}

/// A failure as a status line, with its code when the daemon sent one.
fn describe(error: &ClientError) -> String {
    match error.code() {
        Some(code) => format!("{code}: {error}"),
        None => error.to_string(),
    }
}
