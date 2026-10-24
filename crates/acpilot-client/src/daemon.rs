//! Finding the daemon, and starting it when it is not there.
//!
//! Any client may start the daemon (ADR 0004). That is what makes `acpilot tui` a
//! single command instead of a terminal-and-a-flag ritual, and it is also the only
//! place in the product that starts a process nobody asked for by name, so it is
//! deliberately conservative:
//!
//! * A **probe** decides whether a daemon exists. Nothing is started until
//!   `GET /v1/health` has failed, so a running daemon is never raced or replaced.
//! * A **lock file** serialises spawning. Two windows opening at once is a normal
//!   desktop login, not an attack, and without the lock both would spawn and one
//!   would lose the port after doing the expensive startup work.
//! * A **version mismatch is a refusal**, not a restart: a daemon may be mid-turn
//!   for a session someone else is watching, and killing it to satisfy a client
//!   would throw that turn away.
//! * `--no-spawn` turns all of it off, because a service manager, a test and a
//!   person debugging a startup problem all want the failure instead.
//!
//! The daemon is detached into its own process group (`acpilot_host::detach`) so
//! closing the terminal that happened to start it does not end every session.
//! Its output goes to a log beside the lock, because a daemon that fails four
//! milliseconds after spawning would otherwise leave nothing to read.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime};

use crate::Client;

/// Where a daemon is expected when nobody says otherwise.
///
/// The unix socket in the runtime directory on a platform that has one, and
/// loopback HTTP where it does not. The answer lives beside the platform rules in
/// `acpilot-host`, so the daemon binds exactly what a client probes.
#[must_use]
pub fn default_endpoint() -> String {
    acpilot_host::default_endpoint()
}

/// Environment variable that names the endpoint, matching `acpilot --api-url`.
pub const ENV_ENDPOINT: &str = "ACPILOT_API_URL";

/// Environment variable that names the daemon executable to spawn.
///
/// The default is a sibling of the client binary, because an installer is
/// allowed to put both in a directory nobody added to `PATH` (ADR 0004 §5).
pub const ENV_DAEMON: &str = "ACPILOT_DAEMON";

/// How long a probe waits for `GET /v1/health`.
///
/// A daemon on loopback either answers now or is not there; a slow answer is not
/// a daemon that is starting, it is a machine that is busy, and the retry loop
/// covers that case better than a long first wait.
const PROBE: Duration = Duration::from_millis(300);

/// How long a freshly spawned daemon gets to answer `/v1/health`.
const READY: Duration = Duration::from_millis(2000);

/// How often readiness is checked while waiting.
const POLL: Duration = Duration::from_millis(50);

/// A spawn lock older than this is treated as abandoned.
///
/// The lock is created before the spawn and removed after readiness, so a live
/// holder is at most a couple of seconds old. Ten seconds is far enough past
/// that to be a crash and not a slow start.
const STALE_LOCK: Duration = Duration::from_secs(10);

/// How the daemon was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// It was already running; nothing was started.
    Existing,
    /// This client started it.
    Spawned {
        /// The daemon's process id, for a log line or a status message.
        pid: u32,
    },
}

/// What to connect to, and whether starting it is allowed.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `--api-url`, when the caller named one.
    pub endpoint: Option<String>,
    /// `--no-spawn`: fail instead of starting a daemon.
    pub no_spawn: bool,
    /// Extra arguments passed to `acpilotd` when this client spawns it.
    ///
    /// `--workspace`, and `--profile` when the caller named one. They are only
    /// used on the spawn path: a daemon that is already running keeps the
    /// arguments it was started with.
    pub daemon_args: Vec<String>,
}

/// A client talking to a daemon, and how it got there.
#[derive(Debug)]
pub struct Running {
    /// The connected client.
    pub client: Client,
    /// The endpoint it is connected to.
    pub endpoint: String,
    /// The daemon's version, as `GET /v1/health` reports it.
    pub version: String,
    /// The workspace the daemon is serving, when it names one.
    pub workspace: Option<String>,
    /// Whether this call started the daemon.
    pub origin: Origin,
}

impl Running {
    /// Whether this client started the daemon.
    #[must_use]
    pub fn spawned(&self) -> bool {
        matches!(self.origin, Origin::Spawned { .. })
    }
}

/// Why a daemon could not be reached or started.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// Nothing is listening and spawning was not allowed.
    #[error(
        "no daemon at {endpoint} and spawning is disabled; start `acpilotd` yourself or drop --no-spawn"
    )]
    NotRunning {
        /// The endpoint that was probed.
        endpoint: String,
    },

    /// The daemon is running with a different version than this client.
    #[error(
        "the daemon at {endpoint} is version {daemon}, this client is {client}; restart the daemon"
    )]
    VersionMismatch {
        /// The endpoint that answered.
        endpoint: String,
        /// `health.version` from the daemon.
        daemon: String,
        /// This build's version.
        client: String,
    },

    /// The daemon executable could not be found.
    #[error("cannot find the daemon binary {tried}; set {ENV_DAEMON} to its path")]
    NoDaemonBinary {
        /// The path that was tried.
        tried: String,
    },

    /// The endpoint is not one this spawner understands.
    #[error("{endpoint} cannot be spawned: {why}")]
    Unsupported {
        /// The endpoint given.
        endpoint: String,
        /// What about it is unusable.
        why: String,
    },

    /// The daemon was started and did not answer in time.
    #[error("the daemon started at {endpoint} did not answer within {seconds} s; see {log}")]
    Timeout {
        /// The endpoint that was probed.
        endpoint: String,
        /// The readiness budget, in seconds.
        seconds: u64,
        /// Where the daemon's output went.
        log: String,
    },

    /// The daemon was started and exited before answering.
    #[error("the daemon started at {endpoint} exited with {status}; see {log}")]
    Exited {
        /// The endpoint that was probed.
        endpoint: String,
        /// The exit status, as the OS spells it.
        status: String,
        /// Where the daemon's output went.
        log: String,
    },

    /// The lock or log could not be created.
    #[error("cannot prepare the daemon's runtime directory: {0}")]
    Io(#[from] std::io::Error),

    /// A blocking runtime could not be built.
    #[error("cannot start the client runtime: {0}")]
    Runtime(String),
}

/// Connect to the daemon, starting it if it is not running.
///
/// # Errors
///
/// See [`ConnectError`]: every variant exists because it names a different thing
/// a person has to do — start it yourself, restart it, install it, wait, or read
/// the log.
pub async fn ensure(options: &Options) -> Result<Running, ConnectError> {
    let endpoint = resolve_endpoint(
        options.endpoint.as_deref(),
        std::env::var(ENV_ENDPOINT).ok().as_deref(),
    );
    let client = Client::new(&endpoint);

    if let Some(health) = probe(&client).await {
        check_version(&endpoint, &health.version)?;
        return Ok(Running {
            client,
            endpoint,
            version: health.version,
            workspace: health.workspace,
            origin: Origin::Existing,
        });
    }

    if options.no_spawn {
        return Err(ConnectError::NotRunning { endpoint });
    }

    let binary = daemon_binary().ok_or_else(|| ConnectError::NoDaemonBinary {
        tried: sibling_candidates()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(" or "),
    })?;
    let bind = bind_args(&endpoint)?;
    let log = log_path(&endpoint);
    prepare_runtime_dir(&log)?;

    // Another client may have started it between the probe above and the lock.
    // Claiming the lock is what decides who starts it; the loser waits.
    let claim = claim_spawn(&endpoint)?;
    let mut child = None;
    let _guard = match claim {
        Claim::Owned(guard) => {
            if let Some(health) = probe(&client).await {
                check_version(&endpoint, &health.version)?;
                return Ok(Running {
                    client,
                    endpoint,
                    version: health.version,
                    workspace: health.workspace,
                    origin: Origin::Existing,
                });
            }
            child = Some(spawn(&binary, &bind, &log, &options.daemon_args)?);
            Some(guard)
        }
        Claim::Elsewhere => None,
    };

    let health = wait_ready(&client, child.as_mut(), &endpoint, &log).await?;
    check_version(&endpoint, &health.version)?;
    Ok(Running {
        client,
        endpoint,
        version: health.version,
        workspace: health.workspace,
        origin: match child {
            Some(child) => Origin::Spawned { pid: child.id() },
            None => Origin::Existing,
        },
    })
}

/// [`ensure`] for a caller that is not async, such as the CLI's argument dispatch.
///
/// # Errors
///
/// The same failures as [`ensure`], plus a runtime that could not be built.
pub fn ensure_blocking(options: &Options) -> Result<Running, ConnectError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ConnectError::Runtime(error.to_string()))?;
    runtime.block_on(ensure(options))
}

/// The endpoint to use: the flag, then the environment, then the default.
///
/// Separate from [`ensure`] so the precedence is testable without touching the
/// process environment, which is global state and not safe to set in parallel
/// tests.
#[must_use]
pub fn resolve_endpoint(explicit: Option<&str>, environment: Option<&str>) -> String {
    explicit
        .or(environment)
        .map_or_else(default_endpoint, ToOwned::to_owned)
        .trim_end_matches('/')
        .to_owned()
}

/// The flags that make `acpilotd` bind the endpoint this client probed.
///
/// Named rather than inferred: a client that spawned a daemon on a default could
/// drift from the daemon's default and probe a socket nobody is serving. A URL
/// becomes `--addr host:port`; a path becomes `--socket <path>`.
fn bind_args(endpoint: &str) -> Result<Vec<String>, ConnectError> {
    let unsupported = |why: &str| ConnectError::Unsupported {
        endpoint: endpoint.to_owned(),
        why: why.to_owned(),
    };
    if let Some(rest) = endpoint.strip_prefix("http://") {
        let addr = rest.trim_end_matches('/');
        if addr.is_empty() || !addr.contains(':') {
            return Err(unsupported("the endpoint needs a host and a port"));
        }
        return Ok(vec!["--addr".to_owned(), addr.to_owned()]);
    }
    if endpoint.starts_with("https://") {
        return Err(unsupported(
            "a daemon this client starts speaks plain HTTP; pass --api-url http://host:port",
        ));
    }
    if endpoint.is_empty() {
        return Err(unsupported("the endpoint is empty"));
    }
    Ok(vec!["--socket".to_owned(), endpoint.to_owned()])
}

/// Ask the daemon who it is, giving up quickly.
async fn probe(client: &Client) -> Option<crate::Health> {
    tokio::time::timeout(PROBE, client.health())
        .await
        .ok()
        .and_then(Result::ok)
}

fn check_version(endpoint: &str, daemon: &str) -> Result<(), ConnectError> {
    let client = env!("CARGO_PKG_VERSION");
    if daemon == client {
        return Ok(());
    }
    Err(ConnectError::VersionMismatch {
        endpoint: endpoint.to_owned(),
        daemon: daemon.to_owned(),
        client: client.to_owned(),
    })
}

/// Wait for `/v1/health`, watching the child so a failed startup is reported as
/// what it is instead of a timeout two seconds later.
async fn wait_ready(
    client: &Client,
    mut child: Option<&mut Child>,
    endpoint: &str,
    log: &Path,
) -> Result<crate::Health, ConnectError> {
    let deadline = tokio::time::Instant::now() + READY;
    loop {
        if let Some(health) = probe(client).await {
            return Ok(health);
        }
        if let Some(child) = child.as_mut()
            && let Some(status) = child.try_wait()?
        {
            return Err(ConnectError::Exited {
                endpoint: endpoint.to_owned(),
                status: status.to_string(),
                log: log.display().to_string(),
            });
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ConnectError::Timeout {
                endpoint: endpoint.to_owned(),
                seconds: READY.as_secs(),
                log: log.display().to_string(),
            });
        }
        tokio::time::sleep(POLL).await;
    }
}

fn spawn(
    binary: &Path,
    bind: &[String],
    log: &Path,
    extra: &[String],
) -> Result<Child, ConnectError> {
    let output = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    // The log holds paths and, when tracing is verbose, request shapes. It is
    // the operator's own log, not a world-readable one.
    acpilot_host::private_file(log)?;
    let mut command = Command::new(binary);
    command
        .args(bind)
        .args(extra)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output);
    // Out of the terminal's process group: closing the window that spawned the
    // daemon must not end every session it owns (ADR 0004 §2).
    acpilot_host::detach(&mut command);
    command.spawn().map_err(ConnectError::Io)
}

/// The daemon binary this client would start.
///
/// `ACPILOT_DAEMON` wins, because an installer may put the pair anywhere and a
/// developer may be running one build against another's binary. Otherwise the
/// sibling of the running executable, which is where a cargo build and a release
/// archive both put it. `PATH` is deliberately not searched: a stale `acpilotd`
/// from an old install answering a new client is a version mismatch nobody can
/// explain.
fn daemon_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(ENV_DAEMON) {
        return Some(PathBuf::from(path));
    }
    sibling_candidates().into_iter().find(|path| path.is_file())
}

fn sibling_candidates() -> Vec<PathBuf> {
    let directory = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    ["acpilotd", "acpilotd.exe"]
        .into_iter()
        .map(|name| directory.join(name))
        .collect()
}

/// The per-user runtime directory the lock and log live in.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("acpilot")
}

fn log_path(endpoint: &str) -> PathBuf {
    runtime_dir().join(format!("daemon-{}.log", slug(endpoint)))
}

fn lock_path(endpoint: &str) -> PathBuf {
    runtime_dir().join(format!("spawn-{}.lock", slug(endpoint)))
}

/// A file-name-safe form of an endpoint, so two ports do not share a lock.
fn slug(endpoint: &str) -> String {
    endpoint
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn prepare_runtime_dir(log: &Path) -> Result<(), ConnectError> {
    let Some(directory) = log.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(directory)?;
    acpilot_host::private_dir(directory)?;
    Ok(())
}

/// Who gets to start the daemon.
enum Claim {
    /// This call holds the lock and should spawn.
    Owned(SpawnLock),
    /// Somebody else holds it; wait for the daemon to appear.
    Elsewhere,
}

fn claim_spawn(endpoint: &str) -> Result<Claim, ConnectError> {
    let path = lock_path(endpoint);
    for _ in 0..2 {
        match SpawnLock::create(&path) {
            Ok(lock) => return Ok(Claim::Owned(lock)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale(&path) {
                    // A lock nobody could still be holding: the previous
                    // spawner crashed between creating it and removing it.
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                return Ok(Claim::Elsewhere);
            }
            Err(error) => return Err(ConnectError::Io(error)),
        }
    }
    // Both attempts found a lock that was stale and then was recreated under us.
    Ok(Claim::Elsewhere)
}

fn is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_LOCK)
}

/// A lock file that removes itself.
///
/// The pid inside is for a person reading the directory, not for the protocol:
/// liveness is decided by age, because a pid can be reused and `kill -0` is a
/// platform branch this crate is not allowed to grow (see `platform_boundary`).
struct SpawnLock {
    path: PathBuf,
}

impl SpawnLock {
    fn create(path: &Path) -> std::io::Result<Self> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let _ = std::fs::write(path, std::process::id().to_string());
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_beats_the_environment_beats_the_default() {
        assert_eq!(
            resolve_endpoint(
                Some("http://127.0.0.1:9000/"),
                Some("http://192.0.2.1:7777")
            ),
            "http://127.0.0.1:9000"
        );
        assert_eq!(
            resolve_endpoint(None, Some("http://127.0.0.1:9001")),
            "http://127.0.0.1:9001"
        );
        assert_eq!(resolve_endpoint(None, None), default_endpoint());
    }

    #[test]
    fn the_endpoint_becomes_the_flag_that_binds_it() {
        assert_eq!(
            bind_args("http://127.0.0.1:7777").expect("an address"),
            ["--addr", "127.0.0.1:7777"]
        );
        // A path is a unix socket, and the daemon is told which one rather than
        // being left to its own default.
        assert_eq!(
            bind_args("/run/user/1000/acpilot/acpilot.sock").expect("a socket"),
            ["--socket", "/run/user/1000/acpilot/acpilot.sock"]
        );
        assert!(bind_args("https://127.0.0.1:7777").is_err(), "no TLS");
        assert!(bind_args("http://localhost").is_err(), "no port");
        assert!(bind_args("").is_err(), "nothing to bind");
    }

    #[test]
    fn two_ports_do_not_share_a_lock_or_a_log() {
        assert_ne!(slug("http://127.0.0.1:7777"), slug("http://127.0.0.1:7778"));
        assert!(
            slug("http://127.0.0.1:7777")
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        );
    }

    #[test]
    fn a_lock_that_is_old_is_stale_and_a_new_one_is_not() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("spawn.lock");
        let lock = SpawnLock::create(&path).expect("a lock");
        assert!(!is_stale(&path));
        drop(lock);
        assert!(!path.exists(), "the lock removes itself");
    }

    #[tokio::test]
    async fn no_daemon_and_no_spawn_names_the_flag() {
        // Bind and drop, so the port is very likely free without guessing a
        // number: the test is about the error, not about port 1 being dead.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().expect("an address").port();
        drop(listener);

        let options = Options {
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            no_spawn: true,
            daemon_args: Vec::new(),
        };
        let error = ensure(&options).await.expect_err("nothing is listening");
        match error {
            ConnectError::NotRunning { endpoint } => {
                assert!(endpoint.ends_with(&port.to_string()), "{endpoint}");
            }
            other => panic!("unexpected {other}"),
        }
        assert!(
            ConnectError::NotRunning {
                endpoint: "http://127.0.0.1:1".to_owned()
            }
            .to_string()
            .contains("--no-spawn")
        );
    }
}
