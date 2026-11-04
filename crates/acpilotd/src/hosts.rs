//! Hosts: which machine a workspace's files and processes live on.
//!
//! A host is a transport, not a place a session points at yet: a session still
//! opens on the daemon's own machine, and this registry is what a client reads to
//! know that. The local host is always listed and always the first row, because
//! every other answer in this API happens on it.
//!
//! # What a capability means
//!
//! [`HostCapabilities`] answers "can the daemon do this *here*", which is what a
//! client needs before offering to start an agent. For the local host the answer
//! is a fact about this build rather than a probe: the daemon is running on the
//! machine, so a false field would mean a platform it does not run on. For a
//! remote host the answer is absent until a connection test has succeeded — an
//! optimistic `true` would be a client offering to spawn an agent on a machine
//! nobody has reached.
//!
//! # What is not here
//!
//! Latency is not reported: a handshake is one line each way, and a number from it
//! would say more about where the helper runs than about the host. A remote host's
//! capabilities are the ones its helper named in the handshake, so a client is only
//! offered what the far side said it could do — a build with a PTY is offered a pane,
//! one without `exec` is never offered as a place to start an agent.

use std::sync::{Arc, Mutex, PoisonError};

use acpilot_core::AcpilotError;
use acpilot_proto::host::{CreateHostRequest, HostCapabilities, HostKind, HostSummary};
use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use crate::{ApiError, AppState};

/// The id the daemon's own host is listed under.
pub const LOCAL_HOST: &str = "local";

/// Every host this daemon knows, the one it runs on first.
///
/// In memory rather than read per request: the local host's capabilities are a
/// fact about this build, and a list that changed between two reads would be a
/// dialog that flickers. The store is where a *remote* host is remembered, and it
/// is seeded at startup, so a restart does not forget what somebody added.
#[derive(Debug)]
pub struct Hosts {
    known: Mutex<Vec<HostSummary>>,
    /// The helper connections this daemon keeps, one per remote host.
    ///
    /// A connection rather than a probe per request: a helper is a session, and
    /// testing the same host twice should ask the process that already answered
    /// rather than boot the transport again (see `crate::helper`). Here rather
    /// than on the daemon because it is a fact about a host, and the host list is
    /// what a connection test starts from.
    connections: Arc<crate::helper::Connections>,
    /// Where `ssh` is, when it is not on `PATH`.
    ///
    /// `ACPILOT_SSH` is how a daemon says so and [`Self::prefer_ssh`] is how a test
    /// does: the environment belongs to the process, and a suite that wrote to it
    /// would be deciding the transport for every test beside it.
    ssh: Mutex<Option<String>>,
}

impl Default for Hosts {
    fn default() -> Self {
        Self {
            known: Mutex::new(vec![local_summary()]),
            connections: Arc::new(crate::helper::Connections::new()),
            ssh: Mutex::new(None),
        }
    }
}

impl Hosts {
    /// Every host, the local one first.
    #[must_use]
    pub fn list(&self) -> Vec<HostSummary> {
        self.lock().clone()
    }

    /// The helper connections this daemon keeps, one per remote host.
    #[must_use]
    pub fn connections(&self) -> &Arc<crate::helper::Connections> {
        &self.connections
    }

    /// Say where `ssh` is without moving the process's environment.
    ///
    /// A test's transport is a stand-in program, and `ACPILOT_SSH` is read from the
    /// environment only as the daemon's own answer: a suite that set the variable
    /// would be choosing the transport for every test in its process.
    pub fn prefer_ssh(&self, program: Option<String>) {
        *self.ssh.lock().unwrap_or_else(PoisonError::into_inner) = program;
    }

    /// Say where a host should put the helper it is given, without touching this
    /// process's environment.
    ///
    /// The same seam as [`Self::prefer_ssh`], for the same reason: an install directory
    /// is a fact about this daemon (or this test), and `ACPILOT_HELPER_DIR` is only the
    /// default nobody typed.
    pub fn prefer_install_dir(&self, dir: Option<String>) {
        self.connections.prefer_install_dir(dir);
    }

    /// Say which helper to put on a host, overriding the one this build carries.
    ///
    /// A test seam: the bundled bytes are compiled in, so a suite cannot produce them,
    /// and a build that carries nothing still has to prove the install path works.
    pub fn prefer_bundle(&self, bytes: Option<Vec<u8>>) {
        self.connections.prefer_bundle(bytes);
    }

    /// The `ssh` this daemon should run: the one it was told about, else the one the
    /// environment names, else `PATH`'s.
    pub(crate) fn ssh_program(&self) -> String {
        self.ssh
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .or_else(|| std::env::var("ACPILOT_SSH").ok())
            .unwrap_or_else(|| "ssh".to_owned())
    }

    /// The host with this id, when it is one this daemon knows.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<HostSummary> {
        self.lock().iter().find(|host| host.id == id).cloned()
    }

    /// Add a host. `false` means the id was already known, which is a conflict
    /// rather than an update: two rows with one id is not a list a client can use.
    pub fn insert(&self, host: HostSummary) -> bool {
        let mut known = self.lock();
        if known.iter().any(|existing| existing.id == host.id) {
            return false;
        }
        known.push(host);
        true
    }

    /// Replace a host's row, for a test that learned something about it.
    pub fn replace(&self, host: HostSummary) {
        let mut known = self.lock();
        match known.iter_mut().find(|existing| existing.id == host.id) {
            Some(existing) => *existing = host,
            None => known.push(host),
        }
    }

    /// Add the hosts a previous daemon recorded, skipping ones already known.
    ///
    /// A stored row is listed as **not connected**: whether a host answers is a
    /// fact about a connection, and nothing has opened one yet in this process.
    pub fn seed(&self, records: impl IntoIterator<Item = acpilot_store::HostRecord>) {
        let mut known = self.lock();
        for record in records {
            if known.iter().any(|existing| existing.id == record.id) {
                continue;
            }
            known.push(HostSummary {
                label: record.address.clone().unwrap_or_else(|| record.id.clone()),
                id: record.id,
                kind: record.kind,
                connected: false,
                capabilities: None,
            });
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<HostSummary>> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// This machine, as this build can report it.
fn local_summary() -> HostSummary {
    HostSummary {
        id: LOCAL_HOST.to_owned(),
        label: "this machine".to_owned(),
        kind: HostKind::Local,
        connected: true,
        capabilities: Some(HostCapabilities {
            filesystem: true,
            pty: true,
            spawn: true,
            lsp: true,
        }),
    }
}

/// The wire spelling of a kind, which is also the scheme of a host id.
fn kind_text(kind: HostKind) -> &'static str {
    match kind {
        HostKind::Local => "local",
        HostKind::Ssh => "ssh",
        HostKind::Wsl => "wsl",
    }
}

/// The host routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/hosts", get(list).post(create))
        .route("/v1/hosts/{id}/test", post(test))
}

async fn list(State(state): State<AppState>) -> Json<Vec<HostSummary>> {
    Json(state.hosts().list())
}

/// `POST /v1/hosts` — name a machine this daemon should be able to reach.
async fn create(
    State(state): State<AppState>,
    body: Result<Json<CreateHostRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<HostSummary>), ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a host request: {rejection}"
        )))
    })?;
    if request.kind == HostKind::Local {
        return Err(ApiError::new(AcpilotError::conflict(
            "the daemon's own host is always listed as `local`",
        )));
    }
    let address = request
        .address
        .as_deref()
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .ok_or_else(|| {
            ApiError::new(AcpilotError::invalid("a remote host needs an address"))
                .with_detail("address", "`user@machine` for ssh, a distribution for wsl")
        })?;
    // `kind:address` rather than `kind://address`: the id is a path segment on
    // `/v1/hosts/{id}/test`, and `//` splits into two of them — the route would
    // never match the host this call just added.
    let id = format!("{}:{address}", kind_text(request.kind));
    let host = HostSummary {
        label: address.to_owned(),
        id: id.clone(),
        kind: request.kind,
        connected: false,
        capabilities: None,
    };
    if !state.hosts().insert(host.clone()) {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "host {id} is already known"
        ))));
    }
    if let Some(store) = state.store() {
        store
            .save_host(&acpilot_store::HostRecord {
                id,
                kind: request.kind,
                address: Some(address.to_owned()),
            })
            .map_err(|error| {
                ApiError::new(AcpilotError::Io(std::io::Error::other(format!(
                    "the host could not be recorded: {error}"
                ))))
            })?;
    }
    Ok((StatusCode::CREATED, Json(host)))
}

/// `POST /v1/hosts/{id}/test` — ask a host whether it answers.
///
/// The local host answers with this build's capabilities. A remote one is refused
/// rather than faked: a test that returns `connected: false` for a host whose
/// transport does not exist yet would be indistinguishable from a host that is
/// merely asleep, and the client would wait.
async fn test(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<HostSummary>, ApiError> {
    let host = state
        .hosts()
        .get(&id)
        .ok_or_else(|| ApiError::new(AcpilotError::not_found(format!("host {id}"))))?;
    match host.kind {
        HostKind::Local => {
            let probed = local_summary();
            state.hosts().replace(probed.clone());
            Ok(Json(probed))
        }
        HostKind::Ssh | HostKind::Wsl => remote(&state, &host, id).await,
    }
}

/// How long a transport gets before the test is reported as failed.
///
/// A handshake is one line each way once the transport is up; the budget is for
/// the transport starting (a distro a Windows build has to boot, an ssh connection
/// that has to negotiate). A test that never answers is a failure the caller can
/// act on, not a request to hold open forever.
pub(crate) const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Test a host whose transport is a process: `ssh` or `wsl.exe`.
///
/// The helper is started over the transport and asked to shake hands. What comes
/// back is protocol 1's capability set, not a promise about this machine: a host
/// whose helper answers is a host this daemon can reach, and the capabilities say
/// what it can then do.
async fn remote(
    state: &AppState,
    host: &HostSummary,
    id: String,
) -> Result<Json<HostSummary>, ApiError> {
    let address = id
        .split_once(':')
        .map(|(_, address)| address.to_owned())
        .unwrap_or_else(|| id.clone());
    let transport = crate::helper::Transport::new(host.kind, address, state.hosts().ssh_program());
    // The connection the daemon already has for this host, or a new one it keeps:
    // testing the same host twice asks the helper that answered the first time.
    let connections = Arc::clone(state.hosts().connections());
    let key = id.clone();
    let probe = tokio::task::spawn_blocking(move || connections.probe(&key, &transport));
    let failed = |detail: String| {
        ApiError::new(AcpilotError::HostUnavailable { host: id.clone() })
            .with_detail("transport", detail)
    };
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, probe).await {
        Err(_) => Err(failed(format!(
            "{} did not answer within {} seconds",
            transport_name(host.kind),
            HANDSHAKE_TIMEOUT.as_secs()
        ))),
        Ok(Err(error)) => Err(failed(error.to_string())),
        Ok(Ok(Err(detail))) => Err(failed(detail)),
        Ok(Ok(Ok(capabilities))) => {
            let connected = HostSummary {
                connected: true,
                capabilities: Some(capabilities),
                ..host.clone()
            };
            state.hosts().replace(connected.clone());
            Ok(Json(connected))
        }
    }
}

/// Which of the three ways to reach a helper a command is for.
///
/// One enum rather than three functions, because the three differ by the *last* argument
/// of the same command: a transport that grows a fourth way to start a program only has
/// to say which of these it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The helper the host already has, named on its `PATH`.
    Path,
    /// The helper this daemon installed in `dir`.
    Installed,
    /// A shell reading the install script from stdin.
    Install,
}

/// The program and arguments that run the helper on a host.
///
/// `ACPILOT_SSH` and `ACPILOT_WSL` move the transport itself: a box where `ssh` is
/// not on `PATH`, or a Windows build whose `wsl.exe` lives somewhere unusual, would
/// otherwise have no way to say where it is. The helper's name is the far side's —
/// `acpilotd-helper` on the host's `PATH` — not this daemon's.
///
/// `dir` is where an installed helper lives, as a shell spells it: `$HOME/.local/bin` by
/// default, or an absolute directory when a test would rather not write into a
/// developer's home.
pub(crate) fn remote_command(
    kind: HostKind,
    address: &str,
    ssh: &str,
    step: Step,
    dir: &str,
) -> (String, Vec<String>) {
    match kind {
        // `-e`, never `--`: P0-5 measured `wsl.exe --` routing the command through
        // the user's shell, which ate backslashes and reported a missing program as
        // 127. `-e` calls `execvpe` directly and reports the failure as itself.
        HostKind::Ssh => {
            let mut args = vec![
                "-o".to_owned(),
                "BatchMode=yes".to_owned(),
                address.to_owned(),
            ];
            match step {
                Step::Path => args.push(crate::helper::INSTALL_NAME.to_owned()),
                // `ssh` hands the rest of the line to the remote user's shell, so the
                // launch is quoted once here and expanded (`$HOME`) there.
                Step::Installed => args.push(format!(
                    "sh -c '{}'",
                    launch_script(dir).replace('\'', "'\\''")
                )),
                Step::Install => args.push("sh -s".to_owned()),
            }
            (ssh.to_owned(), args)
        }
        HostKind::Wsl => {
            let program = std::env::var("ACPILOT_WSL").unwrap_or_else(|_| "wsl.exe".to_owned());
            let mut args = vec!["-d".to_owned(), address.to_owned(), "-e".to_owned()];
            match step {
                Step::Path => args.push(crate::helper::INSTALL_NAME.to_owned()),
                // `-e` does not expand `~` or `$HOME` (P0-5), so the one program it
                // starts is a shell that does.
                Step::Installed => {
                    args.push("sh".to_owned());
                    args.push("-c".to_owned());
                    args.push(launch_script(dir));
                }
                Step::Install => {
                    args.push("sh".to_owned());
                    args.push("-s".to_owned());
                }
            }
            (program, args)
        }
        HostKind::Local => match step {
            Step::Path => (crate::helper::INSTALL_NAME.to_owned(), Vec::new()),
            Step::Installed => ("sh".to_owned(), vec!["-c".to_owned(), launch_script(dir)]),
            Step::Install => ("sh".to_owned(), vec!["-s".to_owned()]),
        },
    }
}

/// The program a shell should `exec` to start the installed helper on a host.
fn launch_script(dir: &str) -> String {
    format!("exec \"{}\"", helper_path(dir))
}

/// Where an installed helper lives, as the host's shell spells it.
///
/// A relative directory is joined to `$HOME` so the default is per-user; an absolute one
/// is used as it stands, which is what a test's temporary directory needs.
fn helper_path(dir: &str) -> String {
    let root = if dir.starts_with('/') || dir.starts_with('~') {
        dir.to_owned()
    } else {
        format!("$HOME/{dir}")
    };
    format!("{root}/{}", crate::helper::INSTALL_NAME)
}

fn transport_name(kind: HostKind) -> &'static str {
    match kind {
        HostKind::Ssh => "ssh",
        HostKind::Wsl => "wsl.exe",
        HostKind::Local => "the local host",
    }
}
