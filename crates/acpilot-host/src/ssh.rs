//! SSH hosts: one connection per host, one channel per exec or terminal.
//!
//! Every shape below comes from a measurement in `docs/notes/ssh-agent.md`, not
//! from how SSH is usually described:
//!
//! * **The exec channel is a byte stream, not a message channel.** 103 JSON lines
//!   written one at a time arrived in 20–33 `DATA` events, up to 37 in one event.
//!   So [`RemoteReader`] hands the caller bytes and the caller frames on `\n`;
//!   there is no message boundary to preserve here because there is none on the
//!   wire.
//! * **stderr arrives as `EXTENDED_DATA` with `ext = 1`**, which is why it gets a
//!   reader of its own instead of being folded into stdout. A frame stream
//!   polluted by a warning is the failure mode that looks like a protocol bug.
//! * **Resize is a channel message** (`window_change`), not a reconnect, so
//!   [`Pty::resize`](crate::Pty::resize) maps straight onto it.
//! * **The cost is the connection** — connect plus auth 70.7–113.9 ms, an exchange
//!   after it 43–51 ms — so a host is connected once, here, and channels are
//!   opened against the same session. Reconnecting per command would pay the
//!   handshake for every keystroke's worth of work.
//!
//! What this module deliberately does not do yet, because P8-1 owns it: ssh-agent
//! authentication, password authentication through the keyring, keepalives,
//! reconnect with backoff, and `ProxyJump`-style configuration. A host that
//! cannot connect returns an error rather than retrying.
//!
//! ## The command line is a shell line, and that is a limitation
//!
//! SSH `exec` takes one string. Arguments are POSIX-quoted here (`quote`) so the
//! far side's shell reconstructs the argv that was asked for; `cwd` and `env` are
//! honoured by prefixing `cd … && K=V …`, because the protocol has no fields for
//! them. That is correct for a POSIX host and meaningless for a Windows one: a
//! Windows SSH server needs `cmd /c` and different quoting, which is why the
//! escaping is one function with tests rather than sprinkled through the file.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use russh::keys::{PublicKeyOrCertificate, load_secret_key};
use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf, Sig, client};
use tokio::sync::mpsc as async_mpsc;

use crate::pty::{Pty, Terminal};
use crate::{Child, ChildBackend, ExecCommand, Exit, Host, HostError, HostKind, Size};

/// How long to wait for a connect attempt before giving up.
///
/// A dial to an unreachable host can hang for the kernel's TCP timeout, which is
/// minutes; a daemon cannot wait that long to answer its first request. P8-1 makes
/// this configurable and adds the retry policy around it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything needed to reach one machine.
#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// The private key. Key files now; ssh-agent is P8-1.
    pub key_path: PathBuf,
    pub key_passphrase: Option<String>,
    pub host_key: HostKeyPolicy,
}

impl SshConfig {
    /// A config for `user@host`, verifying the host key against
    /// `~/.ssh/known_hosts`.
    pub fn new(
        host: impl Into<String>,
        user: impl Into<String>,
        key_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            host: host.into(),
            port: 22,
            user: user.into(),
            key_path: key_path.into(),
            key_passphrase: None,
            host_key: HostKeyPolicy::known_hosts(default_known_hosts()),
        }
    }

    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    #[must_use]
    pub fn passphrase(mut self, passphrase: impl Into<String>) -> Self {
        self.key_passphrase = Some(passphrase.into());
        self
    }

    #[must_use]
    pub fn host_key(mut self, host_key: HostKeyPolicy) -> Self {
        self.host_key = host_key;
        self
    }
}

/// What to do with the key the server presents.
///
/// The default is `known_hosts` for the reason it exists: a key accepted without
/// comparison turns every later connection into a man-in-the-middle opportunity.
#[derive(Debug, Clone)]
pub enum HostKeyPolicy {
    /// Compare against a `known_hosts` file.
    KnownHosts {
        path: PathBuf,
        /// Append a key that is not recorded yet. A TOFU prompt in a UI maps to
        /// this; a script that cannot ask maps to `false` and fails instead.
        trust_on_first_use: bool,
    },
    /// Accept any key. Only for loopback fixtures: P0-4's `sshd.sh` uses a
    /// throwaway host key that no `known_hosts` could contain.
    AcceptAny,
}

impl HostKeyPolicy {
    #[must_use]
    pub fn known_hosts(path: impl Into<PathBuf>) -> Self {
        Self::KnownHosts {
            path: path.into(),
            trust_on_first_use: false,
        }
    }

    #[must_use]
    pub fn trust_on_first_use(path: impl Into<PathBuf>) -> Self {
        Self::KnownHosts {
            path: path.into(),
            trust_on_first_use: true,
        }
    }
}

/// `~/.ssh/known_hosts`, wherever `$HOME` is.
fn default_known_hosts() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".ssh").join("known_hosts")
}

/// A remote machine, connected once and kept.
pub struct SshHost {
    commands: async_mpsc::UnboundedSender<Command>,
    description: String,
}

impl std::fmt::Debug for SshHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshHost")
            .field("host", &self.description)
            .finish_non_exhaustive()
    }
}

impl SshHost {
    /// Connect and authenticate. Returns once the session is usable, so a caller
    /// that gets a host never gets a later surprise about credentials.
    pub fn connect(config: SshConfig) -> Result<Self, HostError> {
        let description = format!("{}@{}:{}", config.user, config.host, config.port);
        let (commands, command_rx) = async_mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let named = description.clone();

        let _ = named;
        thread::Builder::new()
            .name("acpilot-ssh".to_string())
            .spawn(move || serve(config, command_rx, ready_tx))
            .map_err(|error| HostError::Ssh {
                operation: "connect",
                message: format!("could not start the session thread: {error}"),
            })?;

        match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                commands,
                description,
            }),
            Ok(Err(message)) => Err(HostError::Ssh {
                operation: "connect",
                message,
            }),
            Err(_) => Err(HostError::Ssh {
                operation: "connect",
                message: format!("{description} did not answer within {CONNECT_TIMEOUT:?}"),
            }),
        }
    }

    fn open(&self, command: &ExecCommand, pty: Option<Size>) -> Result<RemoteChannel, HostError> {
        let (reply, answer) = mpsc::channel();
        let line = command_line(command);
        let request = match pty {
            Some(size) => Command::Pty { line, size, reply },
            None => Command::Exec { line, reply },
        };
        let operation = if pty.is_some() { "pty" } else { "exec" };
        self.commands.send(request).map_err(|_| HostError::Ssh {
            operation,
            message: format!("the session to {} has closed", self.description),
        })?;
        match answer.recv() {
            Ok(Ok(channel)) => Ok(channel),
            Ok(Err(message)) => Err(HostError::Ssh { operation, message }),
            Err(_) => Err(HostError::Ssh {
                operation,
                message: format!("the session to {} died mid-request", self.description),
            }),
        }
    }
}

impl Host for SshHost {
    fn kind(&self) -> HostKind {
        HostKind::Ssh
    }

    fn spawn(&self, command: &ExecCommand) -> Result<Child, HostError> {
        let channel = self.open(command, None)?;
        Ok(Child::new(
            command.program.to_string_lossy().into_owned(),
            Box::new(RemoteChild {
                outgoing: channel.outgoing.clone(),
                exit: channel.exit,
            }),
            Some(Box::new(RemoteWriter {
                outgoing: channel.outgoing.clone(),
            })),
            Some(Box::new(RemoteReader::new(channel.stdout))),
            Some(Box::new(RemoteReader::new(channel.stderr))),
        ))
    }

    fn spawn_pty(&self, command: &ExecCommand, size: Size) -> Result<Pty, HostError> {
        let channel = self.open(command, Some(size))?;
        Ok(Pty::new(
            Box::new(RemoteTerminal {
                outgoing: channel.outgoing,
                reader: Some(Box::new(RemoteReader::new(channel.stdout))),
                exit: channel.exit,
            }),
            size,
        ))
    }
}

/// Work for the session thread. A channel open is async, so each request is
/// handed to a task on the session's runtime and answered through `reply`.
enum Command {
    Exec {
        line: String,
        reply: Sender<Result<RemoteChannel, String>>,
    },
    Pty {
        line: String,
        size: Size,
        reply: Sender<Result<RemoteChannel, String>>,
    },
}

/// The session thread: connect once, then answer requests until the host is
/// dropped.
fn serve(
    config: SshConfig,
    mut commands: async_mpsc::UnboundedReceiver<Command>,
    ready: Sender<Result<(), String>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(format!("no async runtime: {error}")));
            return;
        }
    };

    runtime.block_on(async move {
        let session = match connect(&config).await {
            Ok(session) => {
                let _ = ready.send(Ok(()));
                session
            }
            Err(message) => {
                let _ = ready.send(Err(message));
                return;
            }
        };

        // Channel set-up is serialised: `Handle` owns its reply receiver and is
        // not cloneable, so a per-open task would need a shareable handle. Set-up
        // is a few round trips on an already-open connection (the whole 103-frame
        // exchange cost 43–51 ms in P0-4), while `drive` below is a task per
        // channel, so bytes after set-up flow concurrently.
        while let Some(command) = commands.recv().await {
            match command {
                Command::Exec { line, reply } => {
                    let _ = reply.send(open_channel(&session, line, None).await);
                }
                Command::Pty { line, size, reply } => {
                    let _ = reply.send(open_channel(&session, line, Some(size)).await);
                }
            }
        }

        let _ = session
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await;
    });
}

async fn connect(config: &SshConfig) -> Result<client::Handle<Handler>, String> {
    let key = load_secret_key(&config.key_path, config.key_passphrase.as_deref())
        .map_err(|error| format!("key {}: {error}", config.key_path.display()))?;

    let rejection = Arc::new(Mutex::new(None::<String>));
    let handler = Handler {
        host_key: config.host_key.clone(),
        host: config.host.clone(),
        port: config.port,
        rejection: Arc::clone(&rejection),
        description: format!("{}@{}:{}", config.user, config.host, config.port),
    };

    let mut session = client::connect(
        Arc::new(client::Config::default()),
        (config.host.as_str(), config.port),
        handler,
    )
    .await
    .map_err(
        |error| match rejection.lock().ok().and_then(|mut slot| slot.take()) {
            Some(reason) => format!("{reason} ({error})"),
            None => format!("{}: {error}", config.host),
        },
    )?;

    let hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(|error| error.to_string())?
        .flatten();
    let auth = session
        .authenticate_publickey(
            &config.user,
            russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
        )
        .await
        .map_err(|error| error.to_string())?;
    if !auth.success() {
        return Err(format!(
            "{} rejected the key for {}",
            config.host, config.user
        ));
    }

    Ok(session)
}

/// The client's side of the handshake: the one decision the library leaves to
/// us, which is whether the host is who it says it is.
struct Handler {
    host_key: HostKeyPolicy,
    host: String,
    port: u16,
    rejection: Arc<Mutex<Option<String>>>,
    description: String,
}

impl Handler {
    fn reject(&self, reason: impl Into<String>) {
        if let Ok(mut slot) = self.rejection.lock() {
            *slot = Some(reason.into());
        }
    }
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let (path, trust_on_first_use) = match &self.host_key {
            HostKeyPolicy::AcceptAny => return Ok(true),
            HostKeyPolicy::KnownHosts {
                path,
                trust_on_first_use,
            } => (path.clone(), *trust_on_first_use),
        };

        let PublicKeyOrCertificate::PublicKey { key, .. } = server_public_key else {
            // Host certificates are recorded differently in known_hosts, and
            // guessing at that is worse than refusing: P8-1 adds it.
            self.reject("the host presented a certificate, not a plain key");
            return Ok(false);
        };

        match russh::keys::check_known_hosts_path(&self.host, self.port, key, &path) {
            Ok(true) => Ok(true),
            Ok(false) if trust_on_first_use => {
                match russh::keys::known_hosts::learn_known_hosts_path(
                    &self.host, self.port, key, &path,
                ) {
                    Ok(()) => Ok(true),
                    Err(error) => {
                        self.reject(format!(
                            "could not record {} in {}: {error}",
                            self.description,
                            path.display()
                        ));
                        Ok(false)
                    }
                }
            }
            Ok(false) => {
                self.reject(format!(
                    "{} is not in {} (add it, or enable trust-on-first-use)",
                    self.description,
                    path.display()
                ));
                Ok(false)
            }
            Err(error) => {
                self.reject(format!(
                    "host key mismatch for {}: {error}",
                    self.description
                ));
                Ok(false)
            }
        }
    }
}

/// Open one session channel, set it up, and start the task that moves bytes.
async fn open_channel(
    session: &client::Handle<Handler>,
    line: String,
    pty: Option<Size>,
) -> Result<RemoteChannel, String> {
    let channel = session
        .channel_open_session()
        .await
        .map_err(|error| error.to_string())?;

    if let Some(size) = pty {
        channel
            .request_pty(
                true,
                "xterm-256color",
                u32::from(size.cols),
                u32::from(size.rows),
                0,
                0,
                &[],
            )
            .await
            .map_err(|error| format!("request_pty: {error}"))?;
    }

    channel
        .exec(true, line.into_bytes())
        .await
        .map_err(|error| format!("exec: {error}"))?;

    let (read, write) = channel.split();
    let (stdout, stdout_rx) = mpsc::channel();
    let (stderr, stderr_rx) = mpsc::channel();
    let (exit, exit_rx) = mpsc::channel();
    let (outgoing, outgoing_rx) = async_mpsc::unbounded_channel();

    tokio::spawn(drive(
        read,
        write,
        outgoing_rx,
        Streams {
            stdout,
            stderr,
            exit,
        },
    ));

    Ok(RemoteChannel {
        stdout: stdout_rx,
        stderr: stderr_rx,
        exit: exit_rx,
        outgoing,
    })
}

/// The senders a channel task writes into. Dropping them is how the readers
/// learn that the channel ended.
struct Streams {
    stdout: Sender<Vec<u8>>,
    stderr: Sender<Vec<u8>>,
    exit: Sender<Exit>,
}

/// Move bytes between one SSH channel and its readers, until either side ends.
async fn drive(
    mut read: ChannelReadHalf,
    write: ChannelWriteHalf<russh::client::Msg>,
    mut outgoing: async_mpsc::UnboundedReceiver<Outgoing>,
    streams: Streams,
) {
    let mut reported_exit = false;
    loop {
        tokio::select! {
            message = read.wait() => match message {
                None => break,
                Some(ChannelMsg::Data { data }) => {
                    let _ = streams.stdout.send(data.to_vec());
                }
                // Only ext = 1 is stderr. Other extended types are not ours to
                // interpret, and folding them into a reader would corrupt it.
                Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                    let _ = streams.stderr.send(data.to_vec());
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    reported_exit = true;
                    let _ = streams.exit.send(Exit {
                        code: Some(i32::try_from(exit_status).unwrap_or(i32::MAX)),
                        success: exit_status == 0,
                    });
                }
                _ => {}
            },
            command = outgoing.recv() => match command {
                None => {
                    let _ = write.close().await;
                    break;
                }
                Some(Outgoing::Data(bytes)) => {
                    if write.data_bytes(bytes).await.is_err() {
                        break;
                    }
                }
                Some(Outgoing::Resize(size)) => {
                    let _ = write
                        .window_change(u32::from(size.cols), u32::from(size.rows), 0, 0)
                        .await;
                }
                Some(Outgoing::Eof) => {
                    let _ = write.eof().await;
                }
                Some(Outgoing::Kill) => {
                    // `signal` is a request, and servers vary: OpenSSH acts on it
                    // for the signals it supports, otherwise the close below is
                    // what ends the channel. P0-4 never measured kill, so this is
                    // the honest best effort rather than a claim.
                    let _ = write.signal(Sig::KILL).await;
                    let _ = write.close().await;
                    break;
                }
            },
        }
    }

    if !reported_exit {
        // No exit status: the channel dropped or the close was ours. Say so
        // rather than pretending the program exited 0.
        let _ = streams.exit.send(Exit {
            code: None,
            success: false,
        });
    }
}

/// One open channel, split into the handles a `Child` or `Pty` needs.
struct RemoteChannel {
    stdout: Receiver<Vec<u8>>,
    stderr: Receiver<Vec<u8>>,
    exit: Receiver<Exit>,
    outgoing: async_mpsc::UnboundedSender<Outgoing>,
}

/// What a caller can do to a channel besides writing to it.
enum Outgoing {
    Data(Vec<u8>),
    /// End of input. Distinct from closing the channel: `cat` must see EOF and
    /// finish while the channel stays open to deliver what it printed. Dropping
    /// the writer sends this, because `drop(stdin)` is what a sync caller does.
    Eof,
    Resize(Size),
    Kill,
}

/// Blocking reads over the chunks the channel task delivers.
///
/// No OS pipe is involved: an async task writing into a pipe would block the
/// runtime, and buffering here costs one copy and no thread.
struct RemoteReader {
    chunks: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    offset: usize,
    closed: bool,
}

impl RemoteReader {
    fn new(chunks: Receiver<Vec<u8>>) -> Self {
        Self {
            chunks,
            chunk: Vec::new(),
            offset: 0,
            closed: false,
        }
    }
}

impl Read for RemoteReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.offset == self.chunk.len() {
            if self.closed {
                return Ok(0);
            }
            match self.chunks.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.offset = 0;
                }
                Err(_) => {
                    self.closed = true;
                    return Ok(0);
                }
            }
        }
        let available = &self.chunk[self.offset..];
        let written = available.len().min(out.len());
        out[..written].copy_from_slice(&available[..written]);
        self.offset += written;
        Ok(written)
    }
}

/// Writes become `DATA` messages in the order they were written.
struct RemoteWriter {
    outgoing: async_mpsc::UnboundedSender<Outgoing>,
}

impl Drop for RemoteWriter {
    fn drop(&mut self) {
        let _ = self.outgoing.send(Outgoing::Eof);
    }
}

impl Write for RemoteWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.outgoing
            .send(Outgoing::Data(buf.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the SSH channel closed"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // The channel task awaits every `data_bytes` before reading the next
        // command, so there is nothing buffered here to flush.
        Ok(())
    }
}

impl ChildBackend for RemoteChild {
    fn id(&self) -> Option<u32> {
        None
    }

    fn kill(&mut self) -> io::Result<()> {
        let _ = self.outgoing.send(Outgoing::Kill);
        Ok(())
    }

    fn wait(&mut self) -> io::Result<Exit> {
        self.exit
            .recv()
            .map_err(|_| io::Error::other("the SSH channel closed before an exit status arrived"))
    }

    fn try_wait(&mut self) -> io::Result<Option<Exit>> {
        match self.exit.try_recv() {
            Ok(exit) => Ok(Some(exit)),
            Err(TryRecvError::Empty) => Ok(None),
            // A closed channel with no status is a process that is no longer
            // running; reporting it as `None` would leave a supervisor waiting.
            Err(TryRecvError::Disconnected) => Ok(Some(Exit {
                code: None,
                success: false,
            })),
        }
    }
}

struct RemoteChild {
    outgoing: async_mpsc::UnboundedSender<Outgoing>,
    exit: Receiver<Exit>,
}

struct RemoteTerminal {
    outgoing: async_mpsc::UnboundedSender<Outgoing>,
    reader: Option<Box<dyn Read + Send>>,
    exit: Receiver<Exit>,
}

impl Terminal for RemoteTerminal {
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader.take()
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.outgoing
            .send(Outgoing::Data(bytes.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the SSH channel closed"))
    }

    fn resize(&mut self, size: Size) -> Result<(), HostError> {
        self.outgoing
            .send(Outgoing::Resize(size))
            .map_err(|_| HostError::Ssh {
                operation: "pty",
                message: "the SSH channel closed before the resize was sent".to_string(),
            })
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    fn kill(&mut self) -> io::Result<()> {
        let _ = self.outgoing.send(Outgoing::Kill);
        Ok(())
    }

    fn wait(&mut self) -> Result<Exit, HostError> {
        self.exit.recv().map_err(|_| HostError::Ssh {
            operation: "pty",
            message: "the SSH channel closed before an exit status arrived".to_string(),
        })
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, HostError> {
        match self.exit.try_recv() {
            Ok(exit) => Ok(Some(exit)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Ok(Some(Exit {
                code: None,
                success: false,
            })),
        }
    }
}

/// The one string SSH `exec` accepts, built from an argv.
///
/// `cwd` and `env` become shell prefixes because the protocol has no fields for
/// them; every word is POSIX-quoted so the far side reconstructs the argv rather
/// than re-splitting it.
fn command_line(command: &ExecCommand) -> String {
    let mut line = String::new();
    if let Some(cwd) = &command.cwd {
        line.push_str("cd ");
        line.push_str(&quote(&cwd.to_string_lossy()));
        line.push_str(" && ");
    }
    for (key, value) in &command.env {
        line.push_str(&quote(&format!(
            "{}={}",
            key.to_string_lossy(),
            value.to_string_lossy()
        )));
        line.push(' ');
    }
    line.push_str(&quote(&command.program.to_string_lossy()));
    for arg in &command.args {
        line.push(' ');
        line.push_str(&quote(&arg.to_string_lossy()));
    }
    line
}

/// Single-quote for a POSIX shell: everything is literal except `'`, which is
/// closed, escaped and reopened.
fn quote(word: &str) -> String {
    if word.is_empty() {
        return "''".to_string();
    }
    let mut quoted = String::with_capacity(word.len() + 2);
    quoted.push('\'');
    for character in word.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_survive_a_shell() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("two words"), "'two words'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("$HOME;rm -rf /"), "'$HOME;rm -rf /'");
    }

    #[test]
    fn a_command_line_is_cwd_then_env_then_argv() {
        let command = ExecCommand::new("/usr/bin/agent")
            .args(["--stdio", "a b"])
            .cwd("/home/dev/work")
            .env("RUST_LOG", "debug");
        assert_eq!(
            command_line(&command),
            "cd '/home/dev/work' && 'RUST_LOG=debug' '/usr/bin/agent' '--stdio' 'a b'"
        );
    }

    #[test]
    fn a_reader_reports_eof_when_the_channel_ends() {
        let (sender, receiver) = mpsc::channel();
        sender.send(b"one".to_vec()).unwrap();
        sender.send(b"two".to_vec()).unwrap();
        drop(sender);

        let mut reader = RemoteReader::new(receiver);
        let mut buffer = [0u8; 2];
        // Deliberately smaller than the first chunk: a read must not assume the
        // writer's boundaries.
        assert_eq!(reader.read(&mut buffer).unwrap(), 2);
        assert_eq!(&buffer, b"on");
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"etwo");
    }

    #[test]
    fn writing_after_the_channel_closes_is_an_error_not_a_silent_drop() {
        let (outgoing, receiver) = async_mpsc::unbounded_channel();
        drop(receiver);
        let mut writer = RemoteWriter { outgoing };
        assert_eq!(
            writer.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn the_default_policy_is_known_hosts_not_accept_any() {
        let config = SshConfig::new("example.com", "dev", "/tmp/key");
        assert!(matches!(
            config.host_key,
            HostKeyPolicy::KnownHosts {
                trust_on_first_use: false,
                ..
            }
        ));
        assert!(
            config
                .host_key_path()
                .is_none_or(|path| path.ends_with(".ssh/known_hosts"))
        );
    }

    impl SshConfig {
        fn host_key_path(&self) -> Option<&std::path::Path> {
            match &self.host_key {
                HostKeyPolicy::KnownHosts { path, .. } => Some(path.as_path()),
                HostKeyPolicy::AcceptAny => None,
            }
        }
    }
}
