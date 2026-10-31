//! Terminals: the PTYs the daemon owns, the ones an agent asks for, and the
//! stream a pane attaches to.
//!
//! A terminal is not a stream of bytes with no memory. A pane that attaches
//! after a command has already printed its output must see that output, and a
//! pane that closes must not end the process — those two rules are what make a
//! terminal something you can come back to rather than something you watch from
//! the start or lose.
//!
//! So each terminal keeps a [`vt100::Parser`] of its screen, fed by the same
//! thread that feeds subscribers. The screen is what an attach replays, and the
//! parser is also what makes a resize meaningful: bytes arriving after a resize
//! are laid out at the new width, not into a buffer that still thinks it is 80
//! columns wide.
//!
//! A terminal an agent asked its client for is the same kind of thing from a
//! pane's side, so it gets the same screen: [`Terminal::adopt`] builds one over
//! the PTY the agent's process already has. The process stays the agent's and the
//! pane ends with it — what a person watches and what an agent polls are the same
//! bytes, which is the point.
//!
//! # Why one lock over the screen and the channel
//!
//! Replay and subscription have to be one step. If a handler took the screen
//! first and subscribed second, output in between would be in neither; subscribed
//! first and replay second, it would be in both and the pane would show a line
//! twice. They are taken together here, and the output thread sends under the
//! same lock, so a subscriber is never late for a byte that is already on the
//! screen and never early for one that is not.
//!
//! # What is deliberately not here
//!
//! No persistence, no scrollback history beyond the visible screen, and no
//! closing of a terminal when its session ends. Each is a task id in `TODO.md`
//! (P2-8 for transcripts, session close for the broker's pending work), not a
//! shortcut taken while wiring the routes.

use std::collections::HashMap;
use std::io::{self, Read};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use acpilot_agent::terminal::TerminalHandle as AgentTerminal;
use acpilot_core::AcpilotError;
use acpilot_host::{ExecCommand, Exit, Host, LocalHost, Pty, Size};
use acpilot_proto::ErrorBody;
use acpilot_proto::session::SessionId;
use acpilot_proto::terminal::{
    CreateTerminalRequest, TerminalControl, TerminalId, TerminalNotice, TerminalSize,
    TerminalSummary,
};
use axum::Json;
use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Path as ApiPath, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use tokio::sync::broadcast;
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::{ApiError, AppState};

/// Frames buffered for one attached pane.
///
/// A pane that falls further behind than this is not slowly catching up, it has
/// stopped reading; it is told so and reattaches, which costs one screen instead
/// of an unbounded queue per terminal.
const OUTPUT_BUFFER: usize = 256;

/// How much is read from the PTY at a time. One read is one frame, which is why
/// it is not smaller: a pane that redraws 40 times for one screenful is a pane
/// that flickers.
const READ: usize = 8192;

/// Rows of scrollback the parser keeps.
///
/// Zero on purpose: an attach replays the visible screen, and a scrollback is
/// memory per terminal with nothing reading it until transcripts persist (P2-8).
const SCROLLBACK: usize = 0;

/// What the output thread publishes.
///
/// `Exit` is a frame rather than a closed channel because the sender lives as
/// long as the terminal does: a pane that attached after the process died must
/// still learn that it died, and how.
#[derive(Debug, Clone)]
pub enum Output {
    /// Bytes exactly as the terminal produced them.
    Data(Vec<u8>),
    /// The process ended, with its code when the host reported one.
    Exit {
        /// Exit code, if any.
        code: Option<i32>,
    },
}

/// The screen, and the channel that keeps attached panes up to date with it.
///
/// One mutex covers both; see the module docs for why that is not laziness.
struct Screen {
    parser: vt100::Parser,
    output: broadcast::Sender<Output>,
}

/// Where a pane's keystrokes go, and where its size is set.
///
/// Two kinds, one shape. A pane the daemon started owns its PTY; a pane of an
/// agent's terminal borrows the one the agent's process already has, and the
/// `Option` is how the borrow ends when the program does — a handle to a dead
/// process would keep its ring and its reader alive for nothing.
enum Driver {
    /// A PTY this daemon started (`POST /v1/terminals`).
    Pty(Mutex<Pty>),
    /// A terminal an agent asked its client for, seen through a pane.
    Agent(Mutex<Option<AgentTerminal>>),
}

impl Driver {
    fn pty(&self) -> Option<std::sync::MutexGuard<'_, Pty>> {
        match self {
            Self::Pty(pty) => Some(pty.lock().unwrap_or_else(PoisonError::into_inner)),
            Self::Agent(_) => None,
        }
    }
}

/// One running terminal.
pub struct Terminal {
    id: TerminalId,
    session_id: SessionId,
    /// The command line, for display. Never re-parsed.
    command: String,
    driver: Driver,
    screen: Mutex<Screen>,
    alive: AtomicBool,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Terminal")
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("command", &self.command)
            .field("alive", &self.alive.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Terminal {
    /// Start `command` on a new PTY and pump its output into `self`.
    ///
    /// # Errors
    ///
    /// A zero dimension, a PTY the host cannot create, or a reader the backend
    /// cannot hand over. The last two are the daemon's host failing rather than
    /// the request being wrong, and both name the command in the message.
    pub fn spawn(
        id: TerminalId,
        session_id: SessionId,
        command: ExecCommand,
        size: TerminalSize,
    ) -> Result<Arc<Self>, ApiError> {
        let (rows, cols) = dimensions(size)?;
        let command_line = command.command_line();
        let mut pty = LocalHost::new()
            .spawn_pty(&command, Size::new(size.cols, size.rows))
            .map_err(|error| host_failure(&command_line, &error.to_string()))?;
        let reader = pty
            .take_reader()
            .ok_or_else(|| host_failure(&command_line, "the terminal has no output side"))?;

        let (output, _) = broadcast::channel(OUTPUT_BUFFER);
        let terminal = Arc::new(Self {
            id,
            session_id,
            command: command_line,
            driver: Driver::Pty(Mutex::new(pty)),
            screen: Mutex::new(Screen {
                parser: vt100::Parser::new(rows, cols, SCROLLBACK),
                output,
            }),
            alive: AtomicBool::new(true),
        });

        let pump = Arc::clone(&terminal);
        std::thread::Builder::new()
            .name(format!("acpilot-terminal-{}", terminal.id))
            // One read loop and a parser; the default 2 MiB is more than this
            // needs, and a daemon with a dozen panes is a dozen of them.
            .stack_size(256 * 1024)
            .spawn(move || pump_output(pump, reader))
            .map_err(|source| ApiError::new(AcpilotError::Io(source)))?;

        Ok(terminal)
    }

    /// A pane of an agent's terminal: the same screen and attach machinery, over
    /// a process this daemon did not start.
    ///
    /// The process stays the agent's. Ending the pane does not end it — writing to
    /// the socket never did — and the program ending closes the pane, because a
    /// screen nobody can type into is not a pane any more. The handle is dropped
    /// at that point, so the agent's runtime can forget it.
    ///
    /// # Panics
    ///
    /// If there is no tokio runtime: the pump that feeds the screen is a task.
    #[must_use]
    pub fn adopt(session_id: SessionId, handle: AgentTerminal) -> Arc<Self> {
        let size = handle.size();
        // An agent's PTY always has a size; `vt100` insists on a non-zero one, so
        // a report of zero becomes the smallest screen rather than a panic.
        let rows = NonZeroU16::new(size.rows).unwrap_or(NonZeroU16::MIN);
        let cols = NonZeroU16::new(size.cols).unwrap_or(NonZeroU16::MIN);
        let terminal = Arc::new(Self {
            id: format!("agent-{}", handle.id()),
            session_id,
            command: handle.command().to_owned(),
            driver: Driver::Agent(Mutex::new(Some(handle.clone()))),
            screen: Mutex::new(Screen {
                parser: vt100::Parser::new(rows, cols, SCROLLBACK),
                output: broadcast::channel(OUTPUT_BUFFER).0,
            }),
            alive: AtomicBool::new(handle.alive()),
        });
        tokio::spawn(pump_agent(Arc::clone(&terminal), handle));
        terminal
    }

    /// The terminal's identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The session that owns it.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The screen as it is now, and everything from this moment on.
    ///
    /// The pair is taken under one lock, so the first live frame follows the
    /// replay exactly: nothing is lost between them and nothing is repeated.
    #[must_use]
    pub fn attach(&self) -> (Vec<u8>, broadcast::Receiver<Output>) {
        let screen = self.screen.lock().unwrap_or_else(PoisonError::into_inner);
        // `contents_formatted` is the screen drawn with the escape sequences that
        // reproduce it, which is what a pane needs: it is not a transcript, it is
        // a state.
        let replay = screen.parser.screen().contents_formatted().into_bytes();
        let receiver = screen.output.subscribe();
        (replay, receiver)
    }

    /// Bytes as if typed. No newline is added: `text` and `text\r` differ.
    ///
    /// # Errors
    ///
    /// The write to the PTY failed, which in practice means the process is gone.
    pub fn write(&self, bytes: &[u8]) -> Result<(), ApiError> {
        match &self.driver {
            Driver::Pty(pty) => pty
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write_all(bytes)
                .map_err(|source| ApiError::new(AcpilotError::Io(source))),
            // A pane of an agent's terminal: the same bytes, one process further
            // away. `None` is a program that has already ended, which is the same
            // answer `write` on a closed PTY gives.
            Driver::Agent(handle) => {
                let handle = handle.lock().unwrap_or_else(PoisonError::into_inner);
                match handle.as_ref() {
                    Some(handle) => handle
                        .write(bytes)
                        .map_err(|source| ApiError::new(AcpilotError::Io(source))),
                    None => Err(ApiError::new(AcpilotError::Io(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        format!("{} has ended", self.command),
                    )))),
                }
            }
        }
    }

    /// Tell the program the pane changed shape, and re-lay the screen.
    ///
    /// # Errors
    ///
    /// A zero dimension, or a backend that refused the resize.
    pub fn resize(&self, size: TerminalSize) -> Result<(), ApiError> {
        let (rows, cols) = dimensions(size)?;
        // The terminal first, then the screen: the output pump takes them in the
        // same order when it reports an exit, so these two cannot deadlock.
        match &self.driver {
            Driver::Pty(pty) => pty
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .resize(Size::new(size.cols, size.rows))
                .map_err(|error| host_failure(&self.command, &error.to_string()))?,
            Driver::Agent(handle) => {
                // The actor that owns the process sets the size; the pane only
                // relays the shape, because the program is not this socket's.
                if let Some(handle) = handle
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                {
                    handle
                        .resize(Size::new(size.cols, size.rows))
                        .map_err(|error| host_failure(&self.command, &error.to_string()))?;
                }
            }
        }
        let mut screen = self.screen.lock().unwrap_or_else(PoisonError::into_inner);
        screen.parser.set_size(rows, cols);
        Ok(())
    }

    /// What the API reports about it.
    #[must_use]
    pub fn summary(&self) -> TerminalSummary {
        let size = match &self.driver {
            Driver::Pty(pty) => pty.lock().unwrap_or_else(PoisonError::into_inner).size(),
            // The handle knows the PTY's size; once it is gone, the screen is the
            // last thing that knew it, which is still the truth a pane needs.
            Driver::Agent(handle) => match handle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            {
                Some(handle) => handle.size(),
                None => {
                    let screen = self.screen.lock().unwrap_or_else(PoisonError::into_inner);
                    let (rows, cols) = screen.parser.screen().size();
                    Size::new(cols.get(), rows.get())
                }
            },
        };
        TerminalSummary {
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            size: TerminalSize {
                cols: size.cols,
                rows: size.rows,
            },
            alive: self.alive.load(Ordering::Acquire),
            command: self.command.clone(),
        }
    }
}

/// Read the PTY, feed the screen, tell the panes, and report the exit.
fn pump_output(terminal: Arc<Terminal>, mut reader: Box<dyn Read + Send>) {
    let mut buffer = [0u8; READ];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let mut screen = terminal
                    .screen
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                screen.parser.process(&buffer[..read]);
                // A send with no panes attached is not an error: the screen above
                // is what a late pane will see.
                let _ = screen.output.send(Output::Data(buffer[..read].to_vec()));
            }
            // A signal interrupted the read; the terminal is still there.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // `EIO` is how a closed PTY reports EOF on unix. There is no error
            // worth surfacing: the exit status below is the real answer.
            Err(_) => break,
        }
    }

    terminal.alive.store(false, Ordering::Release);
    let code = match terminal.driver.pty() {
        Some(mut pty) => match pty.try_wait() {
            Ok(Some(exit)) => {
                // `try_wait` has reaped the child, so a second call cannot see it;
                // the panes are told from this one.
                exit.code
            }
            // No exit status readable: either the child outlived the terminal
            // (it cannot) or the backend cannot report one. Not a reason to
            // withhold the news that it ended.
            Ok(None) | Err(_) => None,
        },
        // `pump_output` only ever runs for a PTY this daemon started.
        None => None,
    };
    let screen = terminal
        .screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let _ = screen.output.send(Output::Exit { code });
}

/// Feed a pane from an agent's terminal until the program ends.
///
/// The screen is built here rather than in the agent runtime because a screen is a
/// client's idea of a terminal, and the agent already has the bytes: `output` is a
/// ring it polls, not a screen it draws. Bytes are published under the same lock
/// as the parser, for the reason the module docs give — replay and subscription are
/// one step, and a pane must not see a line twice or miss one.
async fn pump_agent(terminal: Arc<Terminal>, handle: AgentTerminal) {
    // Backlog first, then live: the two are one sequence, taken under the reader's
    // lock, so a pane made a second after a command printed its usage still shows
    // the usage.
    let (backlog, mut bytes) = handle.watch();
    let mut exit = handle.exited();
    if !backlog.is_empty() {
        feed(&terminal, &backlog);
    }
    // Checked at the top of every pass, not only when the watch changes: a
    // program that ended before the pane existed already recorded its exit, and
    // waiting for a change to a value that is already there waits forever. The
    // check comes after the subscription, so an exit from here on is a change this
    // receiver still sees.
    while (*exit.borrow()).is_none() {
        tokio::select! {
            chunk = bytes.recv() => match chunk {
                Ok(chunk) => feed(&terminal, &chunk),
                // The pane is behind, not gone: the screen it will be replayed is
                // already ahead of the bytes it missed.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // A closed watch is the terminal's handle being gone, which ends the
            // pane the same way an exit does.
            changed = exit.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
    // The exit and the last bytes can both be ready, and a `select!` that picks
    // the exit would draw the screen without the program's final line. Its bytes
    // were published before the exit was recorded, so whatever is still queued is
    // the tail of its output: read it before the screen stops changing.
    drain(&terminal, &mut bytes);

    // Drop the handle before saying the pane is over, so "not alive" means there
    // is nowhere left to type: holding it would keep the agent's ring and reader
    // alive for a screen that no longer changes, and a write to a closed PTY can
    // still be buffered rather than refused.
    if let Driver::Agent(slot) = &terminal.driver {
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
    terminal.alive.store(false, Ordering::Release);
    let code = (*exit.borrow()).and_then(|exit: Exit| exit.code);
    let screen = terminal
        .screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let _ = screen.output.send(Output::Exit { code });
}

/// Everything the pane has been sent but not drawn yet, without waiting.
fn drain(terminal: &Terminal, frames: &mut broadcast::Receiver<Vec<u8>>) {
    loop {
        match frames.try_recv() {
            Ok(chunk) => feed(terminal, &chunk),
            // Behind the program's last moments: what it left on the screen is
            // what the exit is about, and the missed bytes are older than that.
            Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                return;
            }
        }
    }
}

/// Draw one chunk into the pane's screen and tell whoever is attached.
fn feed(terminal: &Terminal, chunk: &[u8]) {
    let mut screen = terminal
        .screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    screen.parser.process(chunk);
    let _ = screen.output.send(Output::Data(chunk.to_vec()));
}

/// Every terminal the daemon owns, by id.
#[derive(Debug, Clone, Default)]
pub struct Terminals {
    inner: Arc<Mutex<HashMap<TerminalId, Arc<Terminal>>>>,
}

impl Terminals {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many terminals are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the daemon has no terminals.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// A pane over a terminal an agent opened, kept in the list.
    ///
    /// The id is the agent's, prefixed: `agent-terminal-1` cannot collide with a
    /// pane the daemon started (`t1`, `t2`), and a client that saw both can tell
    /// which is which without a second field.
    pub fn adopt(&self, session_id: SessionId, handle: AgentTerminal) -> TerminalSummary {
        let terminal = Terminal::adopt(session_id, handle);
        let summary = terminal.summary();
        self.insert(terminal);
        summary
    }

    /// Keep `terminal`.
    pub fn insert(&self, terminal: Arc<Terminal>) {
        self.lock().insert(terminal.id().to_owned(), terminal);
    }

    /// One terminal, when it exists.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<Terminal>> {
        self.lock().get(id).cloned()
    }

    /// Every terminal, oldest id first, optionally only one session's.
    ///
    /// The map is copied before anything is summarised: a summary locks the
    /// terminal's PTY, and holding the registry's lock while doing that would
    /// make every list wait on every pane's last write.
    #[must_use]
    pub fn list(&self, session: Option<&str>) -> Vec<TerminalSummary> {
        let terminals: Vec<Arc<Terminal>> = self.lock().values().cloned().collect();
        let mut summaries: Vec<TerminalSummary> = terminals
            .iter()
            .filter(|terminal| session.is_none_or(|id| terminal.session_id == id))
            .map(|terminal| terminal.summary())
            .collect();
        summaries.sort_by(|a, b| a.id.cmp(&b.id));
        summaries
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TerminalId, Arc<Terminal>>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `POST /v1/terminals`.
///
/// The terminal is started before the route answers, so the id it returns is one
/// a pane can attach to immediately. A command that fails to start is an error
/// here rather than an empty terminal the client discovers later.
///
/// # Errors
///
/// No workspace to start in (`409`), no such session (`404`), arguments without a
/// command or a zero dimension (`400`), a directory that is not inside the
/// workspace (`403`) or does not exist (`404`), or a PTY the host could not make
/// (`500`).
pub async fn create(
    state: &AppState,
    request: CreateTerminalRequest,
) -> Result<TerminalSummary, ApiError> {
    // The request is checked before the daemon's state is: a size the parser
    // cannot hold would otherwise fail inside `vt100` after a process was
    // already running, and a body that makes no sense is a `400` whatever this
    // daemon happens to have loaded.
    dimensions(request.size)?;
    let command = match request.command {
        Some(program) => ExecCommand::new(program).args(request.args),
        None => {
            if !request.args.is_empty() {
                return Err(
                    ApiError::new(AcpilotError::invalid("`args` without `command`"))
                        .with_detail("args", "arguments belong to the command they are passed to"),
                );
            }
            acpilot_host::default_shell()
        }
    };

    let workspace = state.workspace().ok_or_else(|| {
        ApiError::new(AcpilotError::conflict(
            "this daemon was started without a workspace, so a terminal has no directory to start in",
        ))
    })?;
    if state.sessions.get(&request.session_id).is_none() {
        return Err(ApiError::new(AcpilotError::not_found(format!(
            "session {}",
            request.session_id
        ))));
    }

    let cwd = resolve_cwd(&workspace.root, request.cwd.as_deref())?;
    let command = command.cwd(cwd);

    let terminal = Terminal::spawn(next_id(state), request.session_id, command, request.size)?;
    let summary = terminal.summary();
    state.terminals().insert(terminal);
    Ok(summary)
}

/// `GET /v1/terminals?session=`.
async fn list(
    State(state): State<AppState>,
    Query(query): Query<TerminalQuery>,
) -> Json<Vec<TerminalSummary>> {
    Json(state.terminals().list(query.session.as_deref()))
}

/// The query string of the list route.
#[derive(Debug, Deserialize)]
struct TerminalQuery {
    /// Only this session's terminals.
    session: Option<String>,
}

/// `POST /v1/terminals`.
async fn create_route(
    State(state): State<AppState>,
    body: Result<Json<CreateTerminalRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<TerminalSummary>, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a terminal request: {rejection}"
        )))
    })?;
    create(&state, request).await.map(Json)
}

/// `WS /v1/terminals/{id}/attach`.
///
/// An unknown id is a `404` before the upgrade, so a pane that attached to a
/// terminal that is gone is told that rather than shown an empty screen.
async fn attach(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let terminal = state
        .terminals()
        .get(&id)
        .ok_or_else(|| ApiError::new(AcpilotError::not_found(format!("terminal {id}"))))?;
    Ok(upgrade
        .on_upgrade(move |socket| pump(socket, terminal))
        .into_response())
}

/// One pane's lifetime: replay, then live frames, then the exit.
///
/// Closing this socket never ends the terminal — that is the point of attaching
/// rather than owning. The process ends when it ends, or when something else
/// kills it.
async fn pump(mut socket: WebSocket, terminal: Arc<Terminal>) {
    let (replay, mut output) = terminal.attach();
    let attached = TerminalNotice::Attached {
        terminal: terminal.summary(),
        replay_bytes: replay.len(),
    };
    if send_notice(&mut socket, &attached).await.is_err() {
        return;
    }
    if socket.send(Message::Binary(replay.into())).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { return };
                match message {
                    Message::Binary(bytes) => {
                        if let Err(error) = terminal.write(&bytes) {
                            close_with(&mut socket, &error.error.to_string()).await;
                            return;
                        }
                    }
                    Message::Text(text) => {
                        match serde_json::from_str::<TerminalControl>(&text) {
                            Ok(TerminalControl::Resize { cols, rows }) => {
                                if let Err(error) = terminal.resize(TerminalSize { cols, rows }) {
                                    close_with(&mut socket, &error.error.to_string()).await;
                                    return;
                                }
                            }
                            Ok(TerminalControl::Detach) => {
                                let _ = socket.send(Message::Close(None)).await;
                                return;
                            }
                            Err(_) => {
                                close_with(
                                    &mut socket,
                                    "a text frame on a terminal must be a control frame",
                                )
                                .await;
                                return;
                            }
                        }
                    }
                    Message::Close(_) => return,
                    // Ping/pong are the transport's; the socket answers them.
                    _ => {}
                }
            }
            frame = output.recv() => match frame {
                Ok(Output::Data(bytes)) => {
                    if socket.send(Message::Binary(bytes.into())).await.is_err() {
                        return;
                    }
                }
                Ok(Output::Exit { code }) => {
                    let _ = send_notice(&mut socket, &TerminalNotice::Exit { code }).await;
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
                // A pane that has stopped reading is told to attach again, which
                // costs it one screen rather than the daemon an unbounded queue.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    close_with(&mut socket, "fell behind; attach again for the current screen").await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Send one control frame as JSON text.
async fn send_notice(socket: &mut WebSocket, notice: &TerminalNotice) -> Result<(), ()> {
    let text = serde_json::to_string(notice).map_err(|_| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

/// Close with a reason, ignoring a socket that is already gone.
async fn close_with(socket: &mut WebSocket, reason: &str) {
    let frame = CloseFrame {
        code: close_code::ERROR,
        reason: reason.to_owned().into(),
    };
    let _ = socket.send(Message::Close(Some(frame))).await;
}

/// Rows and columns as `vt100` takes them, or a `400` naming which is wrong.
fn dimensions(size: TerminalSize) -> Result<(NonZeroU16, NonZeroU16), ApiError> {
    match (NonZeroU16::new(size.rows), NonZeroU16::new(size.cols)) {
        (Some(rows), Some(cols)) => Ok((rows, cols)),
        _ => Err(ApiError::new(AcpilotError::invalid(
            "a terminal needs at least one row and one column",
        ))
        .with_detail("size", format!("{}x{}", size.cols, size.rows))),
    }
}

/// The directory the terminal starts in, checked against the workspace.
///
/// A shell can `cd` anywhere once it runs — the check is about what the daemon
/// starts it in, not about what it can reach. Both spellings are accepted: a
/// relative path is joined to the root, an absolute one is checked, and either
/// way the answer is a resolved path so a symlink cannot point the start
/// directory outside the workspace the daemon says it serves.
fn resolve_cwd(root: &Path, requested: Option<&str>) -> Result<PathBuf, ApiError> {
    let Some(requested) = requested else {
        return Ok(root.to_path_buf());
    };
    if requested.trim().is_empty() {
        return Ok(root.to_path_buf());
    }
    let relative = Path::new(requested);
    let candidate = if relative.is_absolute() {
        relative.to_path_buf()
    } else {
        root.join(relative)
    };
    let resolved = std::fs::canonicalize(&candidate)
        .map_err(|_| ApiError::new(AcpilotError::not_found(format!("directory {requested}"))))?;
    if !resolved.starts_with(root) {
        return Err(ApiError::new(AcpilotError::denied(format!(
            "cwd {requested} is outside the workspace at {}",
            root.display()
        ))));
    }
    if !resolved.is_dir() {
        return Err(ApiError::new(AcpilotError::not_found(format!(
            "directory {requested}"
        ))));
    }
    Ok(resolved)
}

/// A terminal id that has not been used.
///
/// The same scheme as sessions' `s1`, `s2`: readable in a log, and a deleted
/// terminal leaves its number behind rather than handing a new process an id a
/// client may still hold a screen for.
fn next_id(state: &AppState) -> TerminalId {
    let id = format!("t{}", state.terminals().len() + 1);
    if state.terminals().get(&id).is_some() {
        return format!("t{}-{}", jiff::Timestamp::now().as_millisecond(), id);
    }
    id
}

/// A failed PTY operation, named with the command it belongs to.
///
/// `Io` is deliberate: the request was well formed and the daemon's host could
/// not do it, which is the same class as a file that could not be written.
fn host_failure(command: &str, message: &str) -> ApiError {
    ApiError::new(AcpilotError::Io(io::Error::other(format!(
        "terminal `{command}`: {message}"
    ))))
}

/// The terminal routes, ready to merge into the daemon's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/terminals", get(list).post(create_route))
        .route("/v1/terminals/{id}/attach", get(attach))
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/terminals",
    tag = "terminals",
    summary = "List terminals",
    params(("session" = Option<String>, Query, description = "Only this session's terminals")),
    responses((status = 200, description = "Every terminal the daemon owns", body = Vec<TerminalSummary>))
)]
fn list_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/terminals",
    tag = "terminals",
    summary = "Start a terminal",
    request_body = CreateTerminalRequest,
    responses(
        (status = 200, description = "The terminal, with its process running", body = TerminalSummary),
        (status = 400, description = "Arguments without a command, or a zero dimension", body = ErrorBody),
        (status = 403, description = "The starting directory is outside the workspace", body = ErrorBody),
        (status = 404, description = "No such session, or no such directory", body = ErrorBody),
        (status = 409, description = "This daemon has no workspace", body = ErrorBody),
        (status = 500, description = "The host could not start the program on a PTY", body = ErrorBody)
    )
)]
fn create_doc() {}

/// The OpenAPI operations this module adds, so `lib.rs` registers them in one
/// place instead of repeating the path table.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    use utoipa::Path;
    vec![
        (
            "/v1/terminals",
            HttpMethod::Get,
            <__path_list_doc as Path>::operation(),
        ),
        (
            "/v1/terminals",
            HttpMethod::Post,
            <__path_create_doc as Path>::operation(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_dimension_is_refused() {
        let error = dimensions(TerminalSize { cols: 0, rows: 24 }).expect_err("refused");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(error.details[0].field, "size");
    }

    #[test]
    fn a_control_frame_round_trips() {
        let json = r#"{"type":"resize","cols":100,"rows":30}"#;
        let control: TerminalControl = serde_json::from_str(json).expect("control frame");
        assert_eq!(
            control,
            TerminalControl::Resize {
                cols: 100,
                rows: 30
            }
        );
        assert_eq!(
            serde_json::to_string(&TerminalControl::Detach).expect("json"),
            r#"{"type":"detach"}"#
        );
    }
}
