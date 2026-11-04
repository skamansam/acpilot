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
//! capabilities are protocol 1's — files and programs, no PTY and no language
//! server yet — so a client is not offered a pane on a host that would refuse one.

use std::sync::{Mutex, PoisonError};

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
}

impl Default for Hosts {
    fn default() -> Self {
        Self {
            known: Mutex::new(vec![local_summary()]),
        }
    }
}

impl Hosts {
    /// Every host, the local one first.
    #[must_use]
    pub fn list(&self) -> Vec<HostSummary> {
        self.lock().clone()
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
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

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
    let (program, args) = remote_command(host.kind, &address);
    let probe = tokio::task::spawn_blocking(move || crate::helper::probe(&program, &args));
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

/// The program and arguments that run the helper on a host.
///
/// `ACPILOT_SSH` and `ACPILOT_WSL` move the transport itself: a box where `ssh` is
/// not on `PATH`, or a Windows build whose `wsl.exe` lives somewhere unusual, would
/// otherwise have no way to say where it is. The helper's name is the far side's —
/// `acpilotd-helper` on the host's `PATH` — not this daemon's.
fn remote_command(kind: HostKind, address: &str) -> (String, Vec<String>) {
    match kind {
        // `-e`, never `--`: P0-5 measured `wsl.exe --` routing the command through
        // the user's shell, which ate backslashes and reported a missing program as
        // 127. `-e` calls `execvpe` directly and reports the failure as itself.
        HostKind::Ssh => (
            std::env::var("ACPILOT_SSH").unwrap_or_else(|_| "ssh".to_owned()),
            vec![
                "-o".to_owned(),
                "BatchMode=yes".to_owned(),
                address.to_owned(),
                "acpilotd-helper".to_owned(),
            ],
        ),
        HostKind::Wsl => (
            std::env::var("ACPILOT_WSL").unwrap_or_else(|_| "wsl.exe".to_owned()),
            vec![
                "-d".to_owned(),
                address.to_owned(),
                "-e".to_owned(),
                "acpilotd-helper".to_owned(),
            ],
        ),
        HostKind::Local => ("acpilotd-helper".to_owned(), Vec::new()),
    }
}

fn transport_name(kind: HostKind) -> &'static str {
    match kind {
        HostKind::Ssh => "ssh",
        HostKind::Wsl => "wsl.exe",
        HostKind::Local => "the local host",
    }
}
