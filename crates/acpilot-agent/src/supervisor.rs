//! The supervisor: one agent process, its stderr, and the restart decision.
//!
//! The engine phase's task P2-1 made a manifest into an [`AgentSpec`]; this is the first code that acts
//! on one. It deliberately stops at the process boundary — no JSON-RPC, no
//! sessions — because those are the engine phase's P2-3, and a supervisor that also spoke the
//! protocol would be impossible to test without an agent.
//!
//! Three decisions are worth stating because each is a refusal:
//!
//! * **Only `acp-stdio` is spawned here.** A `pty` agent is driven through a
//!   screen and a parser (P2-7), and an HTTP agent has no local process at all.
//!   Starting either one as if it spoke ACP over pipes would fail on the first
//!   protocol line, after the process had already run.
//! * **stderr is a ring, not a file.** It is kept in memory, bounded, and
//!   readable while the agent runs, so a status line can say why an agent is
//!   unhappy without waiting for it to exit. stdout is left untouched: it belongs
//!   to the session, and consuming it here would eat the protocol.
//! * **Restarts are bounded and only on a crash.** `restart = "on-crash"` means
//!   up to [`MAX_RESTARTS`] attempts after a non-zero exit; a clean exit is never
//!   restarted, because "the agent finished" is not a failure. The default,
//!   `never`, is the default because the manifest examples write `on-crash`
//!   where they mean it.
//!
//! Killing is a tree kill (see the crate's `platform` seam): agents are commonly
//! started through a wrapper (`npx`, `uvx`), and stopping only the wrapper leaves
//! the real agent holding the workspace and the stderr pipe.

mod stderr;

use std::io::{self, Read};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use acpilot_proto::agent::AgentStatus;

use crate::error::AgentError;
use crate::manifest::{AgentSpec, CommandSpec, Restart, Transport};
use crate::platform;

pub use stderr::{STDERR_CAPACITY, StderrRing};

/// How many times `restart = "on-crash"` may bring an agent back.
///
/// Three is enough for a transient failure (a lock held for a second, a socket
/// not yet listening) and few enough that a genuinely broken agent does not loop
/// for the length of a session. The count is per [`Supervisor`], not per hour:
/// there is no window in which an attempt is forgotten.
pub const MAX_RESTARTS: u32 = 3;

/// How long `wait` waits for the stderr reader after the child exits.
///
/// The reader normally sees EOF immediately. A grandchild that inherited stderr
/// can hold the pipe open after its parent is gone, and a supervisor that joined
/// unconditionally would hang for as long as that process lives.
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Size of one read from the child's stderr.
const STDERR_CHUNK: usize = 4 * 1024;

/// A spawned `acp-stdio` agent.
///
/// The process is killed when this value is dropped, so a failed session cannot
/// leave an agent running. `stdout` is available to the caller and is *not*
/// drained here: an ACP session owns it, and reading it in two places would
/// interleave protocol frames.
pub struct AgentProcess {
    spec: Arc<AgentSpec>,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Arc<StderrRing>,
    stderr_done: Option<Receiver<()>>,
    reader: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for AgentProcess {
    /// No pipes and no secrets: the facts a log line needs are the agent, the
    /// pid, and how much stderr is being held.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentProcess")
            .field("id", &self.spec.id)
            .field("pid", &self.child.id())
            .field("stderr_bytes", &self.stderr.len())
            .field("stderr_truncated", &self.stderr.truncated())
            .finish_non_exhaustive()
    }
}

impl AgentProcess {
    /// Spawn the agent described by `spec`.
    ///
    /// Every secret in `spec.env` was resolved when the manifest loaded, so this
    /// is the one place a value reaches a child's environment. Nothing else about
    /// the spec is interpreted: the program and its arguments are passed verbatim.
    pub fn spawn(spec: Arc<AgentSpec>) -> Result<Self, AgentError> {
        Self::spawn_with(spec, Arc::new(StderrRing::new()))
    }

    fn spawn_with(spec: Arc<AgentSpec>, stderr: Arc<StderrRing>) -> Result<Self, AgentError> {
        let command = match &spec.transport {
            Transport::AcpStdio { command } => command,
            other => {
                return Err(AgentError::NotSupervisable {
                    id: spec.id.clone(),
                    transport: other.name(),
                });
            }
        };

        let mut child =
            build(command, spec.as_ref())
                .spawn()
                .map_err(|source| AgentError::Spawn {
                    id: spec.id.clone(),
                    command: command.command_line(),
                    source,
                })?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let Some(stderr_pipe) = child.stderr.take() else {
            let _ = platform::kill_tree(&mut child);
            return Err(AgentError::Stderr {
                id: spec.id.clone(),
                source: io::Error::other("stderr pipe missing after spawn"),
            });
        };

        let ring = Arc::clone(&stderr);
        let (done_sender, done_receiver) = mpsc::channel();
        let reader = thread::Builder::new()
            .name(format!("agent-{}-stderr", spec.id))
            .spawn(move || {
                let mut pipe = stderr_pipe;
                let mut chunk = [0u8; STDERR_CHUNK];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => ring.push(&chunk[..read]),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        // A read error is the end of the stream either way; the
                        // ring keeps whatever arrived before it.
                        Err(_) => break,
                    }
                }
                ring.close();
                // The receiver may be gone because the process was dropped;
                // there is nobody left to tell.
                let _ = done_sender.send(());
            });

        match reader {
            Ok(reader) => Ok(Self {
                spec,
                child,
                stdin,
                stdout,
                stderr,
                stderr_done: Some(done_receiver),
                reader: Some(reader),
            }),
            Err(source) => {
                let _ = platform::kill_tree(&mut child);
                Err(AgentError::Stderr {
                    id: spec.id.clone(),
                    source,
                })
            }
        }
    }

    /// The spec this process was started from.
    pub fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    /// The agent's identifier.
    pub fn id(&self) -> &str {
        &self.spec.id
    }

    /// The process id, for a session record or a diagnostic.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The command that was started, rendered for a human.
    pub fn command_line(&self) -> String {
        self.spec
            .command()
            .map_or_else(String::new, CommandSpec::command_line)
    }

    /// The stderr collected so far. Readable while the agent runs.
    pub fn stderr(&self) -> &StderrRing {
        &self.stderr
    }

    /// The child's stdin, once. The caller that takes it owns the protocol.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    /// The child's stdout, once. See the type note: do not read it twice.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// Whether the agent has exited, without blocking.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Block until the agent exits, then let the stderr reader finish.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait();
        self.finish_stderr();
        status
    }

    /// End the agent and everything it started.
    pub fn kill(&mut self) -> io::Result<()> {
        let result = platform::kill_tree(&mut self.child);
        self.finish_stderr();
        result
    }

    /// Wait briefly for the reader to drain, then detach it if it has not.
    fn finish_stderr(&mut self) {
        let Some(done) = self.stderr_done.take() else {
            return;
        };
        match done.recv_timeout(STDERR_DRAIN_TIMEOUT) {
            Ok(()) => {
                if let Some(reader) = self.reader.take() {
                    let _ = reader.join();
                }
            }
            // Something still holds the pipe. Detaching loses the thread's last
            // few bytes, not the daemon's ability to exit.
            Err(_) => self.reader = None,
        }
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        // Closing stdin is the polite way; a kill is the reliable one, and an
        // agent that ignores EOF must not outlive the session that started it.
        let _ = self.kill();
    }
}

/// One agent, started again after a crash when its manifest says so.
///
/// The supervisor owns one process at a time and the stderr buffer across
/// restarts, so the reason for the *first* crash is still readable after the
/// fourth attempt. It is not a thread: [`Supervisor::wait`] blocks on the
/// caller's thread, which is the session thread that would be waiting anyway.
#[derive(Debug)]
pub struct Supervisor {
    spec: Arc<AgentSpec>,
    process: AgentProcess,
    stderr: Arc<StderrRing>,
    restarts: u32,
}

impl Supervisor {
    /// Start the agent.
    pub fn start(spec: Arc<AgentSpec>) -> Result<Self, AgentError> {
        let stderr = Arc::new(StderrRing::new());
        let process = AgentProcess::spawn_with(Arc::clone(&spec), Arc::clone(&stderr))?;
        Ok(Self {
            spec,
            process,
            stderr,
            restarts: 0,
        })
    }

    /// The running process, for taking its pipes or asking whether it is alive.
    pub fn process(&mut self) -> &mut AgentProcess {
        &mut self.process
    }

    /// The stderr collected so far, including from attempts that crashed.
    pub fn stderr(&self) -> &StderrRing {
        &self.stderr
    }

    /// How many restarts have happened.
    pub fn restarts(&self) -> u32 {
        self.restarts
    }

    /// Block until the agent exits for good.
    ///
    /// A non-zero exit under `restart = "on-crash"` is followed by a new process
    /// on the same stderr buffer, up to [`MAX_RESTARTS`] times. A clean exit and
    /// the `never` policy both return immediately, so the caller can distinguish
    /// "finished" from "gave up" by `status.success()` and `restarts`.
    pub fn wait(&mut self) -> Result<Outcome, AgentError> {
        loop {
            let status = self.process.wait().map_err(|source| AgentError::Wait {
                id: self.spec.id.clone(),
                source,
            })?;

            let should_restart = matches!(self.spec.restart, Restart::OnCrash)
                && !status.success()
                && self.restarts < MAX_RESTARTS;
            if !should_restart {
                return Ok(Outcome {
                    status,
                    restarts: self.restarts,
                });
            }

            self.restarts += 1;
            let replacement =
                AgentProcess::spawn_with(Arc::clone(&self.spec), Arc::clone(&self.stderr))?;
            // Dropping the old process closes its pipes and reaps what is left.
            self.process = replacement;
        }
    }
}

/// How an agent's supervision ended.
#[derive(Debug)]
pub struct Outcome {
    /// The last exit status.
    pub status: ExitStatus,
    /// How many times it was restarted before this status.
    pub restarts: u32,
}

/// Whether an agent can be used right now, from this machine.
///
/// For a local command this is a `PATH` lookup on the host the daemon runs on. It
/// is deliberately not a handshake: no process is started, so a status list can
/// be built without side effects. `Unhealthy` and `Untrusted` are session facts
/// (the engine phase's task P2-2 trust pinning, the service phase (P3) health) and stay unreported until there is a session
/// to report them from.
///
/// An `http-jsonrpc` agent is `Available` when it is configured: reachability is
/// a network fact, and pretending a URL is missing because the daemon is offline
/// would be a different lie from the one it would be if it said `Available` after
/// a failed handshake. The handshake is where that answer belongs.
pub fn availability(spec: &AgentSpec) -> AgentStatus {
    match &spec.transport {
        Transport::AcpStdio { command } | Transport::Pty { command, .. } => {
            if platform::find_program(&command.program).is_some() {
                AgentStatus::Available
            } else {
                AgentStatus::Missing
            }
        }
        Transport::HttpJsonRpc { .. } => AgentStatus::Available,
    }
}

fn build(command: &CommandSpec, spec: &AgentSpec) -> Command {
    let mut process = Command::new(&command.program);
    process.args(&command.args);
    // `expose` is the only way out of a `Secret`, and this is the one place it is
    // called: a child's environment. The value never reaches a log line.
    for (name, value) in &spec.env {
        process.env(name, value.expose());
    }
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    platform::configure(&mut process);
    process
}
