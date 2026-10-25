//! The client half of ACP's terminal methods.
//!
//! An agent that wants to run a command does not run it: it asks its *client* to,
//! because the client is the side that knows which machine the workspace is on and
//! which processes it is allowed to start. `terminal/create` therefore arrives
//! here, and the pseudo-terminal it gets back is [`Host::spawn_pty`] — the same
//! seam `fs/*` uses, so a remote workspace opens a terminal on the remote box in
//! P3-9 without this module knowing.
//!
//! # Output is a bounded ring, not a transcript
//!
//! A command that prints without pause must not exhaust memory, so the last
//! [`MAX_OUTPUT`] bytes are kept and the rest are dropped from the front. The
//! protocol says truncation must land on a character boundary, which is why the
//! ring drops any partial UTF-8 sequence left at its head: an agent that pastes
//! the output into a message would otherwise send bytes no string can hold.
//!
//! The buffer is a ring rather than a growing `String` because the reader is a
//! thread waiting on a file descriptor: whatever it appends has to be cheap, and
//! "cheap" for an unbounded stream means dropping the oldest bytes as they arrive.
//!
//! # A terminal belongs to one session
//!
//! Every request carries a session id, and a terminal id alone is not enough to
//! reach one. A session that asks about another session's terminal gets a refusal,
//! not a peek: terminal output is command output, and two agents sharing a
//! workspace must not share each other's.
//!
//! # Release is not optional
//!
//! `terminal/create` has no matching obligation in the protocol, so a client that
//! only implemented `output` would leak a process per tool call. [`Terminals::reap`]
//! exists for that: the daemon calls it on a timer, and a terminal nobody has
//! touched for [`IDLE_TIMEOUT`] is killed and forgotten.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use acpilot_host::{ExecCommand, Host, HostError, Pty, Size};
use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, CreateTerminalResponse, ErrorCode, KillTerminalRequest,
    KillTerminalResponse, ReleaseTerminalRequest, ReleaseTerminalResponse, SessionId,
    TerminalExitStatus, TerminalId, TerminalOutputRequest, TerminalOutputResponse,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse,
};
use thiserror::Error;
use tokio::sync::watch;

/// How much output a terminal keeps. One mebibyte is roughly a screenful of
/// compiler errors or a few thousand lines of test output, which is what an agent
/// actually reads back; beyond that the oldest bytes are the least interesting.
pub const MAX_OUTPUT: usize = 1024 * 1024;

/// How long a terminal may go untouched before [`Terminals::reap`] kills it.
///
/// Long enough that an agent can start a server, do something else, and come back
/// to it; short enough that a forgotten process does not outlive the session it
/// belongs to by much.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What went wrong creating or reading a terminal.
#[derive(Debug, Error)]
pub enum TerminalError {
    /// A terminal id that no live terminal has, or one this session does not own.
    #[error("no terminal {id} in this session")]
    Unknown {
        /// The id from the request.
        id: TerminalId,
    },

    /// The working directory the agent asked for is not absolute.
    #[error("{} is not an absolute working directory", path.display())]
    NotAbsolute {
        /// The path the agent sent, unchanged.
        path: PathBuf,
    },

    /// The host could not start the program.
    #[error("cannot start {}: {source}", command)]
    Spawn {
        /// The command line, for the message.
        command: String,
        /// Why the host refused.
        #[source]
        source: HostError,
    },

    /// The pseudo-terminal itself failed after it was created.
    #[error("terminal {id}: {source}")]
    Pty {
        /// The terminal that failed.
        id: TerminalId,
        /// Why, as the host reported it.
        #[source]
        source: HostError,
    },
}

impl TerminalError {
    /// The JSON-RPC code an agent should see.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            // The request names something that is not there, which is what the
            // agent can fix; a failure inside the host is not.
            Self::Unknown { .. } | Self::NotAbsolute { .. } => ErrorCode::InvalidParams,
            Self::Spawn { .. } | Self::Pty { .. } => ErrorCode::InternalError,
        }
    }

    pub fn into_protocol(self) -> agent_client_protocol::Error {
        let code = self.code();
        agent_client_protocol::Error::new(i32::from(code), self.to_string())
    }
}

/// The output a terminal keeps, bounded and valid UTF-8 at its head.
#[derive(Debug)]
struct Ring {
    bytes: VecDeque<u8>,
    limit: usize,
    truncated: bool,
}

impl Ring {
    fn new(limit: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        if self.bytes.len() <= self.limit {
            return;
        }
        let excess = self.bytes.len() - self.limit;
        self.bytes.drain(..excess);
        // The front may now be the middle of a multi-byte character. Dropping the
        // continuation bytes is the only way to keep what is left a `String`; the
        // protocol asks for a boundary rather than for the newest possible byte.
        while matches!(self.bytes.front(), Some(byte) if byte & 0b1100_0000 == 0b1000_0000) {
            self.bytes.pop_front();
        }
        self.truncated = true;
    }

    fn text(&self) -> String {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// One live terminal: the process, its output, and how it ended.
struct Handle {
    id: TerminalId,
    session: SessionId,
    pty: Mutex<Pty>,
    output: Mutex<Ring>,
    exit: watch::Sender<Option<acpilot_host::Exit>>,
    exit_rx: Mutex<watch::Receiver<Option<acpilot_host::Exit>>>,
    last_used: Mutex<Instant>,
}

impl Handle {
    fn touch(&self) {
        *self.last_used.lock().expect("terminal clock") = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_used.lock().expect("terminal clock").elapsed()
    }

    fn exit_status(&self) -> Option<acpilot_host::Exit> {
        *self.exit_rx.lock().expect("terminal exit").borrow()
    }

    /// The process has closed its output, so its exit status is now readable.
    ///
    /// Called from the reader thread, which is why it takes the lock rather than
    /// borrowing `&mut self`: the thread reads while the session writes.
    fn record_exit(self: &Arc<Self>) {
        let exit = self.pty.lock().expect("terminal pty").wait();
        if let Ok(exit) = exit {
            // A send with no receivers left is not a failure: it means nobody is
            // waiting for a terminal that has already ended.
            let _ = self.exit.send(Some(exit));
        }
    }
}

/// Every terminal this client is serving, keyed by session and id.
///
/// One of these per client, not per session: the ids are handed out here, and the
/// ownership check needs the session id that created each one.
pub struct Terminals {
    host: Arc<dyn Host>,
    limit: usize,
    inner: Mutex<Inner>,
}

struct Inner {
    next: u64,
    entries: HashMap<u64, Arc<Handle>>,
}

impl std::fmt::Debug for Terminals {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.inner.lock().expect("terminal registry").entries.len();
        formatter
            .debug_struct("Terminals")
            .field("live", &entries)
            .field("limit", &self.limit)
            .finish()
    }
}

impl Terminals {
    /// A registry that starts processes on `host`.
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self::with_limit(host, MAX_OUTPUT)
    }

    /// The same, with a smaller cap, so a test does not have to print a mebibyte
    /// to watch the ring drop the front.
    #[must_use]
    pub fn with_limit(host: Arc<dyn Host>, limit: usize) -> Self {
        Self {
            host,
            limit,
            inner: Mutex::new(Inner {
                next: 0,
                entries: HashMap::new(),
            }),
        }
    }

    /// `terminal/create`: start the command and hand back an id.
    ///
    /// # Errors
    ///
    /// A relative `cwd`, or a host that cannot start the program.
    pub fn create(
        &self,
        request: &CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, TerminalError> {
        if let Some(cwd) = &request.cwd
            && !cwd.is_absolute()
        {
            return Err(TerminalError::NotAbsolute { path: cwd.clone() });
        }

        let mut command = ExecCommand::new(&request.command);
        command.args = request.args.iter().map(Into::into).collect();
        command.cwd = request.cwd.clone();
        command.env = request
            .env
            .iter()
            .map(|entry| (entry.name.clone().into(), entry.value.clone().into()))
            .collect();

        let mut pty = self
            .host
            .spawn_pty(&command, Size::DEFAULT)
            .map_err(|source| TerminalError::Spawn {
                command: command.command_line(),
                source,
            })?;
        // The reader is taken before the terminal is stored, so the unnecessary
        // blocking read cannot be taken twice by two different callers.
        let mut reader = pty.take_reader().ok_or_else(|| TerminalError::Spawn {
            command: command.command_line(),
            source: acpilot_host::HostError::Pty {
                command: command.command_line(),
                message: "the pseudo-terminal has no reader".to_owned(),
            },
        })?;

        let (exit, exit_rx) = watch::channel(None::<acpilot_host::Exit>);
        // The agent's own limit is a request, not a permission: a caller that asks
        // for less gets less, and one that asks for more gets the client's cap.
        let limit = request
            .output_byte_limit
            .map(|asked| usize::try_from(asked).unwrap_or(usize::MAX).min(self.limit))
            .unwrap_or(self.limit);

        let id = {
            let mut inner = self.inner.lock().expect("terminal registry");
            inner.next += 1;
            TerminalId::from(format!("terminal-{}", inner.next))
        };

        let handle = Arc::new(Handle {
            id: id.clone(),
            session: request.session_id.clone(),
            pty: Mutex::new(pty),
            output: Mutex::new(Ring::new(limit)),
            exit,
            exit_rx: Mutex::new(exit_rx),
            last_used: Mutex::new(Instant::now()),
        });

        {
            let mut inner = self.inner.lock().expect("terminal registry");
            // The key is the counter's value, so a released id is never reused:
            // an agent that kept an old id must get "no such terminal", never a
            // different program's output.
            let key = inner.next;
            inner.entries.insert(key, Arc::clone(&handle));
        }

        let reader_handle = Arc::clone(&handle);
        // A blocking read belongs on a thread of its own: this is a file
        // descriptor that may be quiet for minutes, and parking a runtime worker
        // on it would be parking a thread the rest of the daemon needs.
        std::thread::Builder::new()
            .name(format!("acp-terminal-{}", &*handle.id.0))
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                let reader = &mut reader as &mut dyn Read;
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if let Ok(mut output) = reader_handle.output.lock() {
                                output.push(&buffer[..read]);
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        // A read that fails means the console is gone, which is
                        // also what a closed pseudo-terminal reports; the exit
                        // status below is the answer that matters.
                        Err(_) => break,
                    }
                }
                reader_handle.record_exit();
            })
            .map_err(|source| TerminalError::Spawn {
                command: command.command_line(),
                source: acpilot_host::HostError::Pty {
                    command: command.command_line(),
                    message: source.to_string(),
                },
            })?;

        Ok(CreateTerminalResponse::new(id))
    }

    /// `terminal/output`: what the terminal has printed so far, and whether it has
    /// finished.
    ///
    /// # Errors
    ///
    /// No terminal with that id in that session.
    pub fn output(
        &self,
        request: &TerminalOutputRequest,
    ) -> Result<TerminalOutputResponse, TerminalError> {
        let handle = self.lookup(&request.session_id, &request.terminal_id)?;
        handle.touch();
        let output = handle.output.lock().expect("terminal output");
        let mut response = TerminalOutputResponse::new(output.text(), output.truncated);
        if let Some(exit) = handle.exit_status() {
            response = response.exit_status(exit_status(exit));
        }
        Ok(response)
    }

    /// `terminal/wait_for_exit`: the exit status, once the program has one.
    ///
    /// # Errors
    ///
    /// No terminal with that id in that session.
    pub async fn wait_for_exit(
        &self,
        request: &WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, TerminalError> {
        let handle = self.lookup(&request.session_id, &request.terminal_id)?;
        handle.touch();
        if let Some(exit) = handle.exit_status() {
            return Ok(WaitForTerminalExitResponse::new(exit_status(exit)));
        }

        // A clone of the receiver, so the lock is not held across the wait: an
        // `output` call on the same terminal must not queue behind this one.
        let mut receiver = handle.exit_rx.lock().expect("terminal exit").clone();
        loop {
            if let Some(exit) = *receiver.borrow_and_update() {
                handle.touch();
                return Ok(WaitForTerminalExitResponse::new(exit_status(exit)));
            }
            if receiver.changed().await.is_err() {
                // The sender is gone, which means the handle was dropped while
                // this was waiting: the terminal no longer exists to report on.
                return Err(TerminalError::Unknown {
                    id: request.terminal_id.clone(),
                });
            }
        }
    }

    /// `terminal/kill`: stop the program, keep the output.
    ///
    /// # Errors
    ///
    /// No terminal with that id in that session.
    pub fn kill(
        &self,
        request: &KillTerminalRequest,
    ) -> Result<KillTerminalResponse, TerminalError> {
        let handle = self.lookup(&request.session_id, &request.terminal_id)?;
        handle.touch();
        handle
            .pty
            .lock()
            .expect("terminal pty")
            .kill()
            .map_err(|source| TerminalError::Pty {
                id: request.terminal_id.clone(),
                source: acpilot_host::HostError::Pty {
                    command: request.terminal_id.to_string(),
                    message: source.to_string(),
                },
            })?;
        Ok(KillTerminalResponse::new())
    }

    /// `terminal/release`: stop the program and forget the terminal.
    ///
    /// # Errors
    ///
    /// No terminal with that id in that session.
    pub fn release(
        &self,
        request: &ReleaseTerminalRequest,
    ) -> Result<ReleaseTerminalResponse, TerminalError> {
        let handle = self.lookup(&request.session_id, &request.terminal_id)?;
        self.forget(&handle);
        Ok(ReleaseTerminalResponse::new())
    }

    /// Kill every terminal in `session`, for a session that is ending.
    pub fn release_session(&self, session: &SessionId) -> usize {
        let doomed: Vec<Arc<Handle>> = {
            let inner = self.inner.lock().expect("terminal registry");
            inner
                .entries
                .values()
                .filter(|handle| &handle.session == session)
                .cloned()
                .collect()
        };
        for handle in &doomed {
            self.forget(handle);
        }
        doomed.len()
    }

    /// Kill terminals nobody has touched for `idle`; returns how many went.
    ///
    /// Called from a timer rather than from a task per terminal: the point is to
    /// bound the number of processes, and a task each would be another thing to
    /// keep alive.
    pub fn reap(&self, idle: Duration) -> usize {
        let idle_handles: Vec<Arc<Handle>> = {
            let inner = self.inner.lock().expect("terminal registry");
            inner
                .entries
                .values()
                .filter(|handle| handle.idle_for() >= idle)
                .cloned()
                .collect()
        };
        for handle in &idle_handles {
            self.forget(handle);
        }
        idle_handles.len()
    }

    /// How many terminals are live, for a status line and for tests.
    #[must_use]
    pub fn live(&self) -> usize {
        self.inner.lock().expect("terminal registry").entries.len()
    }

    fn lookup(&self, session: &SessionId, id: &TerminalId) -> Result<Arc<Handle>, TerminalError> {
        let inner = self.inner.lock().expect("terminal registry");
        inner
            .entries
            .values()
            .find(|handle| &handle.id == id && &handle.session == session)
            .cloned()
            .ok_or_else(|| TerminalError::Unknown { id: id.clone() })
    }

    fn forget(&self, handle: &Arc<Handle>) {
        {
            let mut inner = self.inner.lock().expect("terminal registry");
            inner.entries.retain(|_, entry| !Arc::ptr_eq(entry, handle));
        }
        // Kill before dropping the last reference: the reader thread holds its own
        // `Arc`, so dropping the map's entry alone would leave the process running
        // with nobody able to reach it.
        if let Ok(mut pty) = handle.pty.lock() {
            let _ = pty.kill();
        }
    }
}

fn exit_status(exit: acpilot_host::Exit) -> TerminalExitStatus {
    // A PTY reports a code even when the process was killed, so `signal` stays
    // unset: ACP has a field for it, and filling it with a guess would be worse
    // than leaving it empty. A code that does not fit in `u32` — a Windows
    // status — is reported as no code rather than as a truncated one.
    let mut status = TerminalExitStatus::new();
    if let Some(code) = exit.code.and_then(|code| u32::try_from(code).ok()) {
        status = status.exit_code(code);
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_end_and_says_it_truncated() {
        let mut ring = Ring::new(8);
        ring.push(b"0123456789");
        assert_eq!(ring.text(), "23456789");
        assert!(
            ring.truncated,
            "the front was dropped and the caller is told"
        );
    }

    #[test]
    fn truncation_never_leaves_half_a_character() {
        // `é` is two bytes. A cap of 5 over "aééé" drops the first byte of the
        // last character, so the boundary has to move right by one: the tail must
        // still be a `String` the agent can send.
        let mut ring = Ring::new(5);
        ring.push("aééé".as_bytes());
        let text = ring.text();
        assert!(text.is_char_boundary(0));
        assert!(text.ends_with('é'), "{text:?}");
        assert!(
            !text.starts_with('\u{fffd}'),
            "no replacement character at the head"
        );
    }

    #[test]
    fn a_smaller_limit_than_the_default_is_honoured() {
        let mut ring = Ring::new(4);
        ring.push(b"abcdef");
        assert_eq!(ring.text(), "cdef");
    }

    #[test]
    fn an_empty_ring_is_an_empty_string_and_not_truncated() {
        let ring = Ring::new(4);
        assert_eq!(ring.text(), "");
        assert!(!ring.truncated);
    }
}
