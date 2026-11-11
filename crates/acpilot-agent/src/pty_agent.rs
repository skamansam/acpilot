//! The program-as-agent adapter: a PTY, a screen, and a transcript parser.
//!
//! Most agents do not speak ACP. This module drives one that does not: it starts
//! the program on a pseudo-terminal through the [`Host`] seam, writes a prompt,
//! and reads the answer off the screen. The design phase's task P0-2 probe drove
//! `freebuff` and Claude Code through a real turn each with one driver, and every
//! rule below is one of the failures that probe paid for
//! (`docs/notes/pty-agents.md`).
//!
//! # What the driver owns, so a profile never repeats it
//!
//! * **A terminal that answers.** Both measured programs probe the terminal
//!   before they settle — `CSI ? u` (kitty keyboard), `CSI > 0 q` (XTVERSION),
//!   `CSI 6 n` (cursor position), `CSI c` (DA1). A bare PTY answers nothing, and
//!   a program waiting on a reply looks exactly like a hang.
//! * **The two-write submit.** `text\r` in one write is one paste as far as
//!   Freebuff is concerned: the words land in the box, nothing is submitted, and
//!   the screen at rest looks like a finished turn. The text and the Enter go in
//!   separate writes with [`DEFAULT_SUBMIT_DELAY`] between them.
//! * **Bracketed paste.** A prompt with a newline in it submits on the first
//!   newline unless it is framed, so every prompt is framed `CSI 200~ … CSI 201~`.
//!   A program without paste handling shows the markers in its input row, which
//!   is visible and fixable rather than a silently truncated turn.
//! * **Capture at the boundary.** Both programs run on the alternate screen, so
//!   after the child exits the terminal holds nothing at all. The screen is read
//!   the moment the turn ends, never after `exit`.
//!
//! # How a turn ends
//!
//! The program's own sentinel — the input row back at rest — is the primary
//! signal, and the profile's `idle_timeout` is only the backstop. Idle alone is
//! a lie at every threshold below five seconds: the model thinking produced
//! silent gaps of 2.4–3.4 s in the middle of live turns, and a 2 s timeout cut
//! every measured turn. The sentinel is what keeps a turn fast; the backstop is
//! what catches the sentinel changing in a new version of the program.
//!
//! A sentinel is only accepted once it has been *absent* since the prompt was
//! submitted, because the sentinel is on screen before the prompt is even typed.
//! It must also hold still for [`SENTINEL_SETTLE`], so a program that redraws its
//! input row while it works cannot end a turn on the frame before the answer.
//!
//! # What a parser is allowed to do
//!
//! The screen is raw material, not a transcript: Freebuff renders ads, a
//! spinner and its own thinking text in the same region as the answer, and
//! Claude prefixes the answer with a bullet and puts a status bar under it.
//! [`transcript`] cuts the chrome and yields [`Message`]s — the user's own line,
//! the model's thought and its answer — and [`Turn`] keeps the raw screen beside
//! them, so a parser regression is a diff instead of a mystery.
//!
//! Two parsers ship, both named by the manifest: `plain-transcript` (screen text)
//! and `ansi-transcript` (the same text with the cell attributes kept, so colour
//! survives). The escape hatch `parser = "script:<path>"` is *recorded* by the
//! loader and refused here by name: running a user's script needs a policy
//! decision about executing extension code, and this build has none.
//!
//! # What is not here
//!
//! No ACP. A PTY agent produces a transcript, not a typed event stream: plans,
//! diffs and rewinds stay a native ACP privilege. Nothing in the daemon
//! dispatches `transport = "pty"` to this module yet — the supervisor refuses the
//! transport rather than starting it as if it spoke ACP, and the session that
//! carries these turns over `GET /v1/sessions/{id}/events` is the next step.

use std::io::{self, Read};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use acpilot_host::{ExecCommand, Exit, Host, Pty, Size};
use thiserror::Error;

use crate::manifest::{AgentSpec, Parser, PtyProfile, Transport};

/// The grid a PTY agent is drawn on when the caller does not say.
///
/// Wide enough that both measured programs lay their input row out on one line,
/// which is what the sentinel check reads. A caller with a real window should
/// pass its own size.
pub const SCREEN_SIZE: Size = Size::new(120, 40);

/// How long the screen must be still before startup counts as finished.
///
/// Neither program has a ready signal. Both animate something during startup
/// (ads in Freebuff, "Auto-updating…" in Claude), so this is a quiet window
/// rather than an event; [`PtyAgent::wait_ready`] reports whether it was reached.
pub const READY_QUIET: Duration = Duration::from_millis(1_200);

/// The cap on [`PtyAgent::wait_ready`]: 8.2 s was the slowest startup measured,
/// and a program that never goes quiet must not stall a caller forever.
pub const READY_CAP: Duration = Duration::from_secs(20);

/// How long the sentinel must hold still before a turn is declared over.
///
/// Long enough that an input row redrawn mid-turn cannot fire it, short enough
/// that it costs nothing next to the seconds an idle timeout would.
pub const SENTINEL_SETTLE: Duration = Duration::from_millis(150);

/// The hard cap on one turn. [`DEFAULT_IDLE_TIMEOUT`] is the normal backstop;
/// this exists for a program that animates forever and therefore never goes
/// quiet at all.
pub const TURN_DEADLINE: Duration = Duration::from_secs(120);

/// How long [`PtyAgent::exit`] waits for a clean exit before killing the program.
pub const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the driver looks at the screen while it waits.
const POLL: Duration = Duration::from_millis(20);

/// How many bytes of the prompt must appear on a row for it to be the user's own
/// line. The whole prompt is too strict: a narrow input row wraps it.
const ECHO_PROBE: usize = 24;

/// The submit key. Claude and Freebuff both take `\r`; `\n` is the terminal's
/// line discipline answering, which is not the same keystroke.
const SUBMIT: &[u8] = b"\r";

const PASTE_OPEN: &str = "\u{1b}[200~";
const PASTE_CLOSE: &str = "\u{1b}[201~";

/// Terminal probes a TUI sends before it will settle, with the answer an
/// xterm-compatible terminal gives. Taken from the design phase's P0-2 probe,
/// which is where all four sequences were observed on the wire.
const QUERIES: [(&[u8], Query); 7] = [
    (b"\x1b[?u", Query::KittyKeyboard),
    (b"\x1b[>0q", Query::Version),
    (b"\x1b[>c", Query::SecondaryDeviceAttributes),
    (b"\x1b[c", Query::DeviceAttributes),
    (b"\x1b[6n", Query::CursorPosition),
    (b"\x1b[5n", Query::StatusReport),
    (b"\x1b[18t", Query::TextAreaSize),
];

/// Everything that can go wrong before a single byte is on the screen.
#[derive(Debug, Error)]
pub enum PtyAgentError {
    /// The spec names a transport this adapter does not drive.
    ///
    /// Reported rather than guessed at: starting an ACP agent on a PTY, or a PTY
    /// agent on pipes, fails later in a way that blames the program.
    #[error("agent {id:?}: transport {transport:?} is not driven by the PTY adapter")]
    NotPty {
        /// Agent identifier.
        id: String,
        /// Transport as the manifest spells it.
        transport: &'static str,
    },

    /// The manifest names something this build will not run.
    #[error("agent {id:?}: {message}")]
    Unsupported {
        /// Agent identifier.
        id: String,
        /// What is not supported, and what ships instead.
        message: String,
    },

    /// The terminal could not be opened, or the program could not be started on it.
    #[error("agent {id:?}: cannot start {command:?}: {source}")]
    Spawn {
        /// Agent identifier.
        id: String,
        /// Command that was attempted, rendered for a human.
        command: String,
        /// The host's complaint.
        #[source]
        source: acpilot_host::HostError,
    },

    /// The terminal opened but its output side was already taken.
    #[error("agent {id:?}: the PTY has no reader left")]
    NoReader {
        /// Agent identifier.
        id: String,
    },

    /// The reading thread could not be started, so the screen would never fill.
    #[error("agent {id:?}: cannot read the terminal: {source}")]
    Reader {
        /// Agent identifier.
        id: String,
        /// The thread spawn failure.
        #[source]
        source: io::Error,
    },

    /// Writing to the program failed, which usually means it exited.
    #[error("agent {id:?}: cannot write to the terminal: {source}")]
    Write {
        /// Agent identifier.
        id: String,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
}

/// Who said one block of a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// The user's own line, echoed by the program.
    User,
    /// The model thinking. Freebuff labels it `• Thinking`; keeping it separate
    /// is cheap here and expensive to recover later.
    Thought,
    /// The answer.
    Agent,
}

/// One block of a transcript: consecutive screen rows with the same author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Who said it.
    pub role: MessageRole,
    /// The text, chrome removed and trailing blanks trimmed.
    pub text: String,
    /// The same text with the cell attributes kept as SGR sequences, when the
    /// profile asked for `ansi-transcript`. `None` for `plain-transcript`.
    pub ansi: Option<String>,
}

/// Why a turn ended, so a caller can tell a sentinel from a timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnd {
    /// The input row came back at rest: the program said it had finished.
    Sentinel,
    /// The idle backstop fired. The screen may be a complete answer or a
    /// truncated one; [`Turn::screen`] is what to look at.
    Idle,
    /// The program exited mid-turn.
    Exited,
    /// [`TURN_DEADLINE`] passed. A program that animates forever never goes quiet.
    Deadline,
}

/// One turn: the messages extracted from the screen, and the screen they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// The transcript, oldest first.
    pub messages: Vec<Message>,
    /// The screen exactly as the boundary found it — chrome, status bar and all.
    ///
    /// Kept next to the messages on purpose: when a program changes its layout,
    /// the difference between "the parser is wrong" and "the prompt changed" is
    /// this field.
    pub screen: String,
    /// What ended the turn.
    pub end: TurnEnd,
    /// Time from writing the prompt to the boundary.
    pub elapsed: Duration,
}

impl Turn {
    /// Every answer message joined with newlines, which is what a caller that
    /// only wants the reply needs.
    #[must_use]
    pub fn answer(&self) -> String {
        self.messages
            .iter()
            .filter(|message| message.role == MessageRole::Agent)
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// How a program left, as far as the PTY could tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitRecord {
    /// Whether the process was seen to end.
    pub exited: bool,
    /// The exit code, when the transport reports one.
    pub code: Option<i32>,
    /// Whether the driver had to kill it.
    pub killed: bool,
}

/// A program driven as an agent, one turn at a time.
///
/// Dropping this kills the program: an adapter that outlived its owner would
/// leave a TUI attached to a workspace nobody owns.
pub struct PtyAgent {
    spec: Arc<AgentSpec>,
    profile: PtyProfile,
    pty: Pty,
    screen: vt100::Parser,
    chunks: Receiver<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
    /// When the last byte arrived, from the driver's own clock. Reset on submit,
    /// because the idle backstop measures the turn rather than the process.
    last_data: Instant,
    exited: Option<Exit>,
    size: Size,
}

impl std::fmt::Debug for PtyAgent {
    /// No screen contents and no secrets: a log line needs the agent, the pid and
    /// the grid. The screen is the user's conversation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyAgent")
            .field("id", &self.spec.id)
            .field("pid", &self.pty.process_id())
            .field("size", &self.size)
            .field("exited", &self.exited.is_some())
            .finish_non_exhaustive()
    }
}

impl PtyAgent {
    /// Start the PTY agent described by `spec` on `host`.
    ///
    /// The host decides where the program runs, so a remote workspace opens its
    /// agent on the remote box without this module knowing. `size` is the grid
    /// the program is told it has; [`SCREEN_SIZE`] is the default a caller with no
    /// window passes.
    pub fn start(spec: Arc<AgentSpec>, host: &dyn Host, size: Size) -> Result<Self, PtyAgentError> {
        let Transport::Pty { command, profile } = &spec.transport else {
            return Err(PtyAgentError::NotPty {
                id: spec.id.clone(),
                transport: spec.transport.name(),
            });
        };

        // Owned before the spec is moved into the agent, so the profile the driver
        // reads every turn does not borrow the spec it belongs to.
        let profile = profile.clone();

        if let Parser::Script(path) = &profile.parser {
            return Err(PtyAgentError::Unsupported {
                id: spec.id.clone(),
                message: format!(
                    "transcript parser {} is not runnable in this build; \
                     `plain-transcript` and `ansi-transcript` ship",
                    path.display()
                ),
            });
        }

        // A TUI that thinks it is on a dumb terminal draws nothing, which is the
        // whole reason the transport is a PTY. The rest of the environment is
        // the daemon's; `env` in the manifest is added on top.
        let mut exec = ExecCommand::new(&command.program)
            .args(&command.args)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor");
        for (name, value) in &spec.env {
            exec = exec.env(name, value.expose());
        }

        let mut pty = host
            .spawn_pty(&exec, size)
            .map_err(|source| PtyAgentError::Spawn {
                id: spec.id.clone(),
                command: command.command_line(),
                source,
            })?;

        let Some(reader) = pty.take_reader() else {
            let _ = pty.kill();
            return Err(PtyAgentError::NoReader {
                id: spec.id.clone(),
            });
        };

        let (sender, chunks) = mpsc::channel();
        let thread = thread::Builder::new()
            .name(format!("pty-agent-{}", spec.id))
            .spawn(move || {
                let mut reader = reader;
                let mut buffer = [0u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if sender.send(buffer[..read].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        // A read error is the end of the stream either way; the
                        // screen keeps what arrived before it.
                        Err(_) => break,
                    }
                }
            });

        let reader = match thread {
            Ok(reader) => reader,
            Err(source) => {
                let _ = pty.kill();
                return Err(PtyAgentError::Reader {
                    id: spec.id.clone(),
                    source,
                });
            }
        };

        let screen = vt100::Parser::new(
            NonZeroU16::new(size.rows.max(1)).expect("clamped to at least one row"),
            NonZeroU16::new(size.cols.max(1)).expect("clamped to at least one column"),
            0,
        );

        Ok(Self {
            spec,
            profile,
            pty,
            screen,
            chunks,
            reader: Some(reader),
            last_data: Instant::now(),
            exited: None,
            size,
        })
    }

    /// The spec this agent was started from.
    pub fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    /// The agent's identifier.
    pub fn id(&self) -> &str {
        &self.spec.id
    }

    /// The process id, when the host reports one.
    pub fn pid(&self) -> Option<u32> {
        self.pty.process_id()
    }

    /// The grid the program was told it has.
    pub fn size(&self) -> Size {
        self.size
    }

    /// The screen as text: trailing blanks dropped, rows joined with newlines.
    pub fn screen_text(&self) -> String {
        rows(self.screen.screen())
            .iter()
            .map(StyledLine::plain_trimmed)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether a row on the screen is the input row back at rest.
    pub fn at_rest(&self) -> bool {
        let wanted = self.profile.sentinel.trim();
        !wanted.is_empty()
            && rows(self.screen.screen())
                .iter()
                .any(|row| row.trimmed() == wanted)
    }

    /// Send keys exactly as typed, for a startup gate.
    ///
    /// Gates are per-program (Claude's trust dialog, Freebuff's "wrong
    /// directory" modal) and the manifest has no field for them yet, so the
    /// caller that knows the program sends the answer here and then calls
    /// [`Self::wait_ready`] again.
    pub fn send(&mut self, keys: impl AsRef<[u8]>) -> Result<(), PtyAgentError> {
        self.write(keys.as_ref())
    }

    /// Wait for the screen to stop changing, which is the closest thing either
    /// measured program has to "ready".
    ///
    /// Returns whether quiet was reached: a program that animates forever
    /// (Freebuff's ads) exhausts [`READY_CAP`] and returns `false`. That is a
    /// report, not a failure — the caller can still type — so it is not an error.
    pub fn wait_ready(&mut self) -> Result<bool, PtyAgentError> {
        let started = Instant::now();
        let mut saw_output = false;
        loop {
            if self.pump(POLL) {
                saw_output = true;
            }
            self.notice_exit();
            if saw_output && self.last_data.elapsed() >= READY_QUIET {
                return Ok(true);
            }
            if self.exited.is_some() || started.elapsed() >= READY_CAP {
                return Ok(false);
            }
        }
    }

    /// Write one prompt and read the turn it produces.
    ///
    /// The text is framed as a bracketed paste and the Enter goes in its own
    /// write after the profile's `submit_delay`; see the module note for why both
    /// matter. The screen is captured the moment the turn ends.
    pub fn prompt(&mut self, text: &str) -> Result<Turn, PtyAgentError> {
        let started = Instant::now();
        self.write(format!("{PASTE_OPEN}{text}{PASTE_CLOSE}").as_bytes())?;
        self.pump_for(self.profile.submit_delay);
        self.write(SUBMIT)?;
        // The idle backstop is about this turn, so its clock starts here rather
        // than at the last byte the program happened to send before the prompt.
        self.last_data = Instant::now();

        let end = self.run_turn(started);
        let screen = self.screen_text();
        let messages = transcript(self.screen.screen(), &self.profile, text);
        Ok(Turn {
            messages,
            screen,
            end,
            elapsed: started.elapsed(),
        })
    }

    /// Leave on our terms: the profile's `exit_command`, then a clean code.
    ///
    /// `Ctrl-D` is not universal (both measured programs ignored it), so a
    /// profile that names nothing gets it only as a last resort.
    pub fn exit(&mut self) -> ExitRecord {
        if let Some(command) = self.profile.exit_command.clone() {
            let _ = self.write(command.as_bytes());
            self.pump_for(Duration::from_millis(250));
            let _ = self.write(SUBMIT);
        } else {
            let _ = self.write(b"\x04");
        }

        let started = Instant::now();
        loop {
            match self.pty.try_wait() {
                Ok(Some(exit)) => {
                    self.exited = Some(exit);
                    return ExitRecord {
                        exited: true,
                        code: exit.code,
                        killed: false,
                    };
                }
                Ok(None) => {}
                Err(_) => break,
            }
            if started.elapsed() >= EXIT_TIMEOUT {
                let _ = self.pty.kill();
                let code = self
                    .pty
                    .try_wait()
                    .ok()
                    .flatten()
                    .and_then(|exit| exit.code);
                self.exited = Some(Exit {
                    code,
                    success: false,
                });
                return ExitRecord {
                    exited: code.is_some(),
                    code,
                    killed: true,
                };
            }
            self.pump(POLL);
        }

        ExitRecord {
            exited: false,
            code: None,
            killed: true,
        }
    }

    /// End the program and everything on its terminal.
    pub fn kill(&mut self) -> io::Result<()> {
        self.pty.kill()
    }

    // -- the loop ------------------------------------------------------------

    /// Run the turn until the sentinel, the backstop, an exit or the deadline.
    fn run_turn(&mut self, started: Instant) -> TurnEnd {
        // The sentinel is on screen before the prompt is typed; only a turn that
        // saw it go away may end on it coming back.
        let mut away = !self.at_rest();

        loop {
            self.pump(POLL);
            self.notice_exit();

            let at_rest = self.at_rest();
            if !at_rest {
                away = true;
            }
            let quiet = self.last_data.elapsed();

            if away && at_rest && quiet >= SENTINEL_SETTLE {
                return TurnEnd::Sentinel;
            }
            if self.exited.is_some() {
                return TurnEnd::Exited;
            }
            if quiet >= self.profile.idle_timeout {
                return TurnEnd::Idle;
            }
            if started.elapsed() >= TURN_DEADLINE {
                return TurnEnd::Deadline;
            }
        }
    }

    /// Read whatever is available within `timeout`. `true` when bytes arrived.
    fn pump(&mut self, timeout: Duration) -> bool {
        match self.chunks.recv_timeout(timeout) {
            Ok(bytes) => {
                self.screen.process(&bytes);
                self.last_data = Instant::now();
                self.answer_queries(&bytes);
                true
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => false,
        }
    }

    /// Read for a fixed window, so a submit-delay is spent reading rather than
    /// sleeping and no output is missed across it.
    fn pump_for(&mut self, window: Duration) {
        let started = Instant::now();
        while started.elapsed() < window {
            self.pump(POLL);
        }
    }

    /// Record the exit when the host reports one.
    fn notice_exit(&mut self) {
        if self.exited.is_some() {
            return;
        }
        if let Ok(Some(exit)) = self.pty.try_wait() {
            self.exited = Some(exit);
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), PtyAgentError> {
        self.pty
            .write_all(bytes)
            .map_err(|source| PtyAgentError::Write {
                id: self.spec.id.clone(),
                source,
            })
    }

    /// Answer the terminal probes a real terminal would answer.
    ///
    /// The replies are built before anything is written, so the screen is not
    /// borrowed while the terminal is.
    fn answer_queries(&mut self, bytes: &[u8]) {
        let mut replies: Vec<String> = Vec::new();
        for (sequence, query) in QUERIES {
            if !contains(bytes, sequence) {
                continue;
            }
            let screen = self.screen.screen();
            let reply = match query {
                Query::DeviceAttributes => "\u{1b}[?1;2c".to_owned(),
                Query::SecondaryDeviceAttributes => "\u{1b}[>0;370;0c".to_owned(),
                Query::Version => "\u{1b}P>|xterm(370)\u{1b}\\".to_owned(),
                Query::KittyKeyboard => "\u{1b}[?0u".to_owned(),
                Query::StatusReport => "\u{1b}[0n".to_owned(),
                Query::CursorPosition => {
                    let (row, col) = screen.cursor_position();
                    format!("\u{1b}[{};{}R", row + 1, col + 1)
                }
                Query::TextAreaSize => {
                    let (rows, cols) = screen.size();
                    format!("\u{1b}[8;{};{}t", rows, cols)
                }
            };
            replies.push(reply);
        }
        for reply in replies {
            let _ = self.pty.write_all(reply.as_bytes());
        }
    }
}

impl Drop for PtyAgent {
    fn drop(&mut self) {
        let _ = self.pty.kill();
        // The reader sees EOF when the master closes; joining would wait on a
        // grandchild that inherited the terminal, so it is detached.
        self.reader = None;
    }
}

/// A terminal query seen on the wire.
#[derive(Debug, Clone, Copy)]
enum Query {
    DeviceAttributes,
    SecondaryDeviceAttributes,
    Version,
    KittyKeyboard,
    StatusReport,
    CursorPosition,
    TextAreaSize,
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

// -- the screen --------------------------------------------------------------

/// One character cell with the attributes it was drawn in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Glyph {
    text: String,
    style: Style,
}

/// The attributes that survive into an `ansi-transcript`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    fg: vt100::Color,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Style {
    fn of(cell: &vt100::Cell) -> Self {
        Self {
            fg: cell.fgcolor(),
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    /// The SGR parameters for this style, empty for the default.
    fn sgr(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.bold {
            parts.push("1".to_owned());
        }
        if self.italic {
            parts.push("3".to_owned());
        }
        if self.underline {
            parts.push("4".to_owned());
        }
        if self.inverse {
            parts.push("7".to_owned());
        }
        match self.fg {
            vt100::Color::Default => {}
            vt100::Color::Idx(index) => parts.push(format!("38;5;{index}")),
            vt100::Color::Rgb(red, green, blue) => {
                parts.push(format!("38;2;{red};{green};{blue}"));
            }
        }
        parts.join(";")
    }
}

/// One screen row, trimmed of its trailing blanks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StyledLine {
    glyphs: Vec<Glyph>,
}

impl StyledLine {
    fn plain(&self) -> String {
        self.glyphs
            .iter()
            .map(|glyph| glyph.text.as_str())
            .collect()
    }

    fn plain_trimmed(&self) -> String {
        self.plain().trim_end().to_owned()
    }

    fn trimmed(&self) -> String {
        self.plain().trim().to_owned()
    }

    fn indent(&self) -> usize {
        let plain = self.plain();
        plain.len() - plain.trim_start().len()
    }

    fn is_blank(&self) -> bool {
        self.glyphs.iter().all(|glyph| glyph.text.trim().is_empty())
    }

    /// The same row with SGR runs, so colour and weight survive.
    fn ansi(&self) -> String {
        let mut out = String::new();
        let mut current: Option<Style> = None;
        for glyph in &self.glyphs {
            if current != Some(glyph.style) {
                if current.is_some() {
                    out.push_str("\u{1b}[0m");
                }
                let sgr = glyph.style.sgr();
                if !sgr.is_empty() {
                    out.push_str("\u{1b}[");
                    out.push_str(&sgr);
                    out.push('m');
                }
                current = Some(glyph.style);
            }
            out.push_str(&glyph.text);
        }
        if current.is_some_and(|style| style != Style::default()) {
            out.push_str("\u{1b}[0m");
        }
        out
    }
    /// The row with the layout's indentation removed from both ends.
    ///
    /// A transcript is text, not a layout: the two spaces both programs put in
    /// front of an answer are where the row sits, not part of what was said. The
    /// indentation is still read before this — it is what tells a thought body
    /// from the answer under it.
    fn trimmed_line(&self) -> Self {
        let mut glyphs = self.glyphs.clone();
        while glyphs
            .first()
            .is_some_and(|glyph| glyph.text.trim().is_empty())
        {
            glyphs.remove(0);
        }
        while glyphs
            .last()
            .is_some_and(|glyph| glyph.text.trim().is_empty())
        {
            glyphs.pop();
        }
        Self { glyphs }
    }

    /// Drop the program's leading bullet (`•`, `●`) and the space after it.
    ///
    /// The bullet is the first *non-space* glyph, not the first glyph: both
    /// programs indent their markers, and a version of this that looked at the
    /// row's first cell found a space and left the marker in the text.
    fn without_marker(&self) -> Self {
        let mut glyphs = self.glyphs.clone();
        let Some(index) = glyphs
            .iter()
            .position(|glyph| !glyph.text.trim().is_empty())
        else {
            return Self { glyphs };
        };
        if !is_bullet(&glyphs[index].text) {
            return Self { glyphs };
        }
        glyphs.remove(index);
        if glyphs
            .get(index)
            .is_some_and(|glyph| glyph.text.trim().is_empty())
        {
            glyphs.remove(index);
        }
        Self { glyphs }
    }
}

fn is_bullet(glyph: &str) -> bool {
    matches!(glyph, "•" | "●")
}

/// The spinners and turn summaries both measured programs draw above their input
/// row. Chrome, not transcript: `✻ Churned for 2s · done 2:46 PM` is the
/// program telling on itself, not something the model said.
fn is_chrome(trimmed: &str) -> bool {
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    if matches!(first, '✻' | '✽' | '✶' | '✳' | '✢' | '∗' | '⎘') {
        return true;
    }
    // A horizontal rule is a divider between blocks, not a line of either.
    trimmed.chars().all(|ch| {
        matches!(
            ch,
            '─' | '━'
                | '┄'
                | '┅'
                | '┈'
                | '┉'
                | '╌'
                | '╍'
                | '═'
                | '—'
                | '–'
                | '-'
                | '│'
        )
    })
}

fn rows(screen: &vt100::Screen) -> Vec<StyledLine> {
    let (height, width) = screen.size();
    let mut lines: Vec<StyledLine> = (0..height.get())
        .map(|row| StyledLine {
            glyphs: glyphs(screen, row, width.get()),
        })
        .collect();
    while lines.last().is_some_and(StyledLine::is_blank) {
        lines.pop();
    }
    lines
}

fn glyphs(screen: &vt100::Screen, row: u16, cols: u16) -> Vec<Glyph> {
    let mut out: Vec<Glyph> = Vec::with_capacity(usize::from(cols));
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        // A wide character's second cell has no contents of its own.
        let text = cell.contents();
        if text.is_empty() {
            continue;
        }
        out.push(Glyph {
            text: text.to_owned(),
            style: Style::of(cell),
        });
    }
    while out.last().is_some_and(|glyph| glyph.text.trim().is_empty()) {
        out.pop();
    }
    out
}

// -- the parsers -------------------------------------------------------------

/// Turn the screen at a turn boundary into messages.
///
/// The rules are the layout both measured programs share, and each one exists
/// because the screen alone cannot say it:
///
/// 1. Everything on or below the sentinel row is chrome — the input box, the
///    status bar, and Freebuff's rotating ads, which sit under both.
/// 2. The last row before the sentinel that repeats the prompt is the user's own
///    line. Everything after it is the answer. (A program whose answer repeats
///    the prompt verbatim would be ambiguous; neither measured one does.)
/// 3. A row that starts with `•` or `●` is the program's own marker. `• Thinking`
///    opens a thought block; the text of that block is the more-indented rows
///    under it, and the first row indented no further ends it — which is how
///    Freebuff puts the answer under its thought.
/// 4. Spinner rows and horizontal rules are chrome and are dropped.
///
/// A program that breaks any of these needs `parser = "script:<path>"`, which
/// this build records and refuses rather than running user code.
#[must_use]
pub fn transcript(screen: &vt100::Screen, profile: &PtyProfile, prompt: &str) -> Vec<Message> {
    let styled = matches!(profile.parser, Parser::AnsiTranscript);
    let lines = rows(screen);

    let sentinel = profile.sentinel.trim();
    let end = lines
        .iter()
        .rposition(|line| line.trimmed() == sentinel)
        .unwrap_or(lines.len());
    let region = &lines[..end];

    let probe: String = prompt
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(ECHO_PROBE)
        .collect::<String>()
        .trim()
        .to_owned();
    let echo = if probe.is_empty() {
        None
    } else {
        region
            .iter()
            .rposition(|line| line.plain().contains(&probe))
    };

    let mut messages: Vec<Message> = Vec::new();
    if let Some(index) = echo {
        push(&mut messages, MessageRole::User, &region[index], styled);
    }
    let start = echo.map_or(0, |index| index + 1);

    let mut thought_indent: Option<usize> = None;
    for line in &region[start..] {
        let trimmed = line.trimmed();
        if trimmed.is_empty() || is_chrome(&trimmed) {
            continue;
        }

        if trimmed.starts_with('•') || trimmed.starts_with('●') {
            let body = line.without_marker();
            let heading = body.trimmed().to_lowercase();
            if heading.starts_with("thinking") || heading == "thought" {
                // The heading is a label for the block, not something said.
                thought_indent = Some(line.indent());
                continue;
            }
            thought_indent = None;
            push(&mut messages, MessageRole::Agent, &body, styled);
            continue;
        }

        if let Some(indent) = thought_indent {
            if line.indent() > indent {
                push(&mut messages, MessageRole::Thought, line, styled);
                continue;
            }
            thought_indent = None;
        }

        push(&mut messages, MessageRole::Agent, line, styled);
    }

    messages
}

fn push(messages: &mut Vec<Message>, role: MessageRole, line: &StyledLine, styled: bool) {
    let line = line.trimmed_line();
    let text = line.plain();
    let ansi = styled.then(|| line.ansi());
    match messages.last_mut() {
        Some(last) if last.role == role => {
            last.text.push('\n');
            last.text.push_str(&text);
            if let (Some(whole), Some(more)) = (&mut last.ansi, ansi) {
                whole.push('\n');
                whole.push_str(&more);
            }
        }
        _ => messages.push(Message { role, text, ansi }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(parser: Parser, sentinel: &str) -> PtyProfile {
        PtyProfile {
            parser,
            prompt_row: None,
            sentinel: sentinel.to_owned(),
            exit_command: None,
            submit_delay: Duration::ZERO,
            idle_timeout: Duration::from_millis(5_000),
        }
    }

    fn screen(cols: u16, rows: u16, text: &str) -> vt100::Parser {
        let mut parser = vt100::Parser::new(
            NonZeroU16::new(rows).expect("rows"),
            NonZeroU16::new(cols).expect("cols"),
            0,
        );
        parser.process(text.as_bytes());
        parser
    }

    /// The Freebuff screen from the design phase's P0-2 note, verbatim.
    ///
    /// `concat!` rather than a continued literal: a backslash-newline in Rust
    /// strips the next line's leading whitespace, and the indentation is exactly
    /// what the thought-block rule reads.
    const FREEBUFF: &str = concat!(
        "   [02:48 PM]\r\n",
        "   Reply with exactly the word OK and nothing else. ⎘\r\n",
        "  • Thinking\r\n",
        "    The user wants a reply of exactly the word OK and nothing else.\r\n",
        "  OK\r\n",
        "                                                     ⎘ • 3s • △▽\r\n",
        "▍Enter a coding task",
    );

    /// The Claude Code screen from the same note.
    const CLAUDE: &str = concat!(
        "● OK\r\n",
        "\r\n",
        "✻ Churned for 2s · done 2:46 PM\r\n",
        "\r\n",
        "──────────────\r\n",
        "❯\r\n",
        "──────────────\r\n",
        "  acpilot | Opus 5.5 | 5% of 1M\r\n",
        "  ⏵⏵ auto mode on (shift+tab to cycle)",
    );

    #[test]
    fn a_freebuff_screen_is_a_user_line_a_thought_and_an_answer() {
        let parsed = screen(90, 20, FREEBUFF);
        let messages = transcript(
            parsed.screen(),
            &profile(Parser::PlainTranscript, "▍Enter a coding task"),
            "Reply with exactly the word OK and nothing else.",
        );
        let roles: Vec<MessageRole> = messages.iter().map(|message| message.role).collect();
        assert_eq!(
            roles,
            vec![MessageRole::User, MessageRole::Thought, MessageRole::Agent],
            "the echo, the thought and the answer: {messages:#?}"
        );
        assert!(
            messages[0].text.contains("Reply with exactly the word OK"),
            "the user's own line is kept: {messages:#?}"
        );
        assert_eq!(
            messages[1].text.trim(),
            "The user wants a reply of exactly the word OK and nothing else.",
            "the thought body, not the `• Thinking` label: {messages:#?}"
        );
        assert_eq!(
            messages[2].text.trim(),
            "OK",
            "the answer is the unmarked line: {messages:#?}"
        );
        assert!(
            messages.iter().all(|message| !message.text.contains("△▽")),
            "the status row and the ads under it are chrome: {messages:#?}"
        );
        assert!(
            messages.iter().all(|message| message.ansi.is_none()),
            "plain-transcript keeps no attributes"
        );
    }

    #[test]
    fn a_claude_screen_drops_the_spinner_and_the_status_bar() {
        let parsed = screen(90, 20, CLAUDE);
        let messages = transcript(
            parsed.screen(),
            &profile(Parser::PlainTranscript, "❯"),
            "Reply with exactly the word OK.",
        );
        assert_eq!(
            messages.len(),
            1,
            "one answer and nothing else: {messages:#?}"
        );
        assert_eq!(messages[0].role, MessageRole::Agent);
        assert_eq!(
            messages[0].text.trim(),
            "OK",
            "the bullet is the program's marker, not text: {messages:#?}"
        );
        assert!(
            !messages[0].text.contains('✻') && !messages[0].text.contains("────"),
            "the spinner row and the rule are chrome: {messages:#?}"
        );
    }

    #[test]
    fn the_ansi_parser_keeps_colour_and_the_plain_one_does_not() {
        let parsed = screen(
            40,
            10,
            "\x1b[31mred\x1b[0m plain\r\n\x1b[1;32mbold green\x1b[0m\r\n❯",
        );
        let styled = transcript(
            parsed.screen(),
            &profile(Parser::AnsiTranscript, "❯"),
            "prompt",
        );
        assert_eq!(styled.len(), 1);
        let ansi = styled[0].ansi.as_deref().expect("attributes kept");
        assert!(
            ansi.contains("38;5;1"),
            "SGR 31 is palette index 1: {ansi:?}"
        );
        assert!(ansi.contains("1;38;5;2"), "bold and green: {ansi:?}");
        assert!(
            ansi.ends_with("\u{1b}[0m"),
            "the run is closed so it cannot bleed: {ansi:?}"
        );
        assert_eq!(styled[0].text, "red plain\nbold green");

        let plain = transcript(
            parsed.screen(),
            &profile(Parser::PlainTranscript, "❯"),
            "prompt",
        );
        assert!(plain[0].ansi.is_none());
        assert_eq!(plain[0].text, "red plain\nbold green");
    }

    #[test]
    fn a_sentinel_that_never_appears_leaves_the_whole_screen_to_the_parser() {
        // No input row anywhere: the turn was ended by the backstop, so the
        // parser must still produce the answer rather than nothing.
        let parsed = screen(40, 10, "● the answer\r\n");
        let messages = transcript(
            parsed.screen(),
            &profile(Parser::PlainTranscript, "❯"),
            "prompt",
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text.trim(), "the answer");
    }

    #[test]
    fn a_prompt_that_the_program_never_echoed_still_yields_the_answer() {
        let parsed = screen(40, 10, "● OK\r\n❯");
        let messages = transcript(
            parsed.screen(),
            &profile(Parser::PlainTranscript, "❯"),
            "a prompt the program dropped",
        );
        assert_eq!(messages.len(), 1, "{messages:#?}");
        assert_eq!(messages[0].role, MessageRole::Agent);
    }
}
