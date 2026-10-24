//! Terminals: the PTYs the daemon owns, and the stream a pane attaches to.
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

use acpilot_core::AcpilotError;
use acpilot_host::{ExecCommand, Host, LocalHost, Pty, Size};
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

/// One running terminal.
pub struct Terminal {
    id: TerminalId,
    session_id: SessionId,
    /// The command line, for display. Never re-parsed.
    command: String,
    /// Writing, resizing, and the exit status all need the handle exclusively.
    pty: Mutex<Pty>,
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
            pty: Mutex::new(pty),
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
        let mut pty = self.pty.lock().unwrap_or_else(PoisonError::into_inner);
        pty.write_all(bytes)
            .map_err(|source| ApiError::new(AcpilotError::Io(source)))
    }

    /// Tell the program the pane changed shape, and re-lay the screen.
    ///
    /// # Errors
    ///
    /// A zero dimension, or a backend that refused the resize.
    pub fn resize(&self, size: TerminalSize) -> Result<(), ApiError> {
        let (rows, cols) = dimensions(size)?;
        // The PTY first, then the screen: the output thread takes them in the
        // same order when it reports an exit, so these two cannot deadlock.
        {
            let mut pty = self.pty.lock().unwrap_or_else(PoisonError::into_inner);
            pty.resize(Size::new(size.cols, size.rows))
                .map_err(|error| host_failure(&self.command, &error.to_string()))?;
        }
        let mut screen = self.screen.lock().unwrap_or_else(PoisonError::into_inner);
        screen.parser.set_size(rows, cols);
        Ok(())
    }

    /// What the API reports about it.
    #[must_use]
    pub fn summary(&self) -> TerminalSummary {
        let size = self
            .pty
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .size();
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
    let code = {
        let mut pty = terminal.pty.lock().unwrap_or_else(PoisonError::into_inner);
        match pty.try_wait() {
            Ok(Some(exit)) => {
                // `try_wait` has reaped the child, so a second call cannot see it;
                // the panes are told from this one.
                exit.code
            }
            // No exit status readable: either the child outlived the terminal
            // (it cannot) or the backend cannot report one. Not a reason to
            // withhold the news that it ended.
            Ok(None) | Err(_) => None,
        }
    };
    let screen = terminal
        .screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let _ = screen.output.send(Output::Exit { code });
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
