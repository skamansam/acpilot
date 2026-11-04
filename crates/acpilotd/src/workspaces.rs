//! Workspaces: a root directory on a host, and whether it is really there.
//!
//! The daemon still serves one workspace's files (see `crate::files`), and this
//! registry is the step before that becomes plural: a client names a host and an
//! absolute root, and the daemon answers whether that root is a directory **on that
//! host**. For a remote host that question cannot be answered with `stat` — there is
//! no path across the transport — so the helper is asked to list it. That makes this
//! the first caller the helper's `request` has, which is the point: a transport with
//! nothing to carry is a protocol nobody has used.
//!
//! # Why opening is a check rather than a record
//!
//! A workspace row that named a directory nobody had looked at would be a session
//! started into a missing directory later, reported as an agent that failed. The
//! check happens once, here, and the refusal says which side of the transport said
//! no: a path that is not there is `404`, a host that cannot answer is `502` naming
//! the transport.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use acpilot_core::AcpilotError;
use acpilot_proto::host::{HostKind, HostSummary};
use acpilot_proto::workspace::{WorkspaceOpenRequest, WorkspaceSummary};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::{Value, json};

use crate::{ApiError, AppState};

/// Every workspace opened in this process, in the order they were opened.
///
/// In memory for the same reason the host list is: the store is where sessions and
/// hosts outlive a restart, and a workspace row records nothing a host and a root
/// pair does not already say.
#[derive(Debug, Default)]
pub struct Workspaces {
    known: Mutex<Vec<WorkspaceSummary>>,
}

impl Workspaces {
    /// Nothing has been opened yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every workspace, the daemon's own first when it was given one.
    ///
    /// The served workspace is added by the route rather than stored here: it is a
    /// fact about how the daemon was started, and a row copied into a list would be
    /// the same fact in two places.
    #[must_use]
    pub fn list(&self) -> Vec<WorkspaceSummary> {
        self.lock().clone()
    }

    /// Open `host_id` + `root`, or hand back the row that pair already has.
    ///
    /// Idempotent because a client opens its workspace on every launch: the second
    /// call is the same workspace, not a second one.
    pub fn open(&self, host_id: &str, root: &str, name: Option<String>) -> WorkspaceSummary {
        let id = format!("{host_id}:{root}");
        let mut known = self.lock();
        if let Some(existing) = known.iter().find(|workspace| workspace.id == id) {
            return existing.clone();
        }
        let summary = WorkspaceSummary {
            name: name.unwrap_or_else(|| basename(root)),
            id,
            host_id: host_id.to_owned(),
            root: root.to_owned(),
            session_count: 0,
        };
        known.push(summary.clone());
        summary
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<WorkspaceSummary>> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The last segment of a host path, which is what a workspace is called by default.
///
/// Both separators, because the root is spelled the way the host spells it and a
/// Windows host spells it with backslashes even when this daemon runs on a Unix.
fn basename(root: &str) -> String {
    let name = root
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(root);
    if name.is_empty() {
        root.to_owned()
    } else {
        name.to_owned()
    }
}

/// The workspace routes.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/workspaces", get(list).post(open))
}

/// `GET /v1/workspaces`.
async fn list(State(state): State<AppState>) -> Json<Vec<WorkspaceSummary>> {
    let mut rows = state.workspaces().list();
    if let Some(served) = state.workspace() {
        let root = served.root.display().to_string();
        let id = format!("{}:{root}", crate::hosts::LOCAL_HOST);
        if !rows.iter().any(|row| row.id == id) {
            rows.insert(
                0,
                WorkspaceSummary {
                    id,
                    name: served.id.clone(),
                    host_id: crate::hosts::LOCAL_HOST.to_owned(),
                    root,
                    session_count: 0,
                },
            );
        }
    }
    Json(rows)
}

/// `POST /v1/workspaces` — open `host_id` + `root`, after checking it is there.
///
/// # Errors
///
/// An empty or relative root (`400`), a host this daemon does not know (`404`), a
/// root that is not a directory on the host (`404`), a host that cannot answer
/// (`502`, naming the transport), or a host whose helper does not offer files
/// (`409`).
async fn open(
    State(state): State<AppState>,
    body: Result<Json<WorkspaceOpenRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<WorkspaceSummary>), ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a workspace request: {rejection}"
        )))
    })?;
    let root = request.root.trim();
    if root.is_empty() {
        return Err(
            ApiError::new(AcpilotError::invalid("a workspace needs a root"))
                .with_detail("root", "the absolute path of the directory on the host"),
        );
    }
    if !absolute(root) {
        return Err(ApiError::new(AcpilotError::invalid(format!(
            "{root} is not an absolute path"
        )))
        .with_detail("root", "a path on the host, not one relative to the daemon"));
    }
    let host = state.hosts().get(&request.host_id).ok_or_else(|| {
        ApiError::new(AcpilotError::not_found(format!("host {}", request.host_id)))
    })?;
    let summary = match host.kind {
        HostKind::Local => local(&state, root, request.name)?,
        HostKind::Ssh | HostKind::Wsl => remote(&state, &host, root, request.name).await?,
    };
    Ok((StatusCode::CREATED, Json(summary)))
}

/// A root this daemon can look at itself.
fn local(state: &AppState, root: &str, name: Option<String>) -> Result<WorkspaceSummary, ApiError> {
    let metadata = std::fs::metadata(Path::new(root)).map_err(|source| {
        let error = match source.kind() {
            std::io::ErrorKind::NotFound => {
                AcpilotError::not_found(format!("{root} is not there on this machine"))
            }
            _ => AcpilotError::Io(std::io::Error::new(
                source.kind(),
                format!("{root}: {source}"),
            )),
        };
        ApiError::new(error)
    })?;
    if !metadata.is_dir() {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "{root} is a file, and a workspace is a directory"
        ))));
    }
    Ok(state
        .workspaces()
        .open(crate::hosts::LOCAL_HOST, root, name))
}

/// A root on a host the daemon reaches through its helper.
async fn remote(
    state: &AppState,
    host: &HostSummary,
    root: &str,
    name: Option<String>,
) -> Result<WorkspaceSummary, ApiError> {
    // The capabilities are what the helper said in its handshake, so a host nobody has
    // tested has not said anything: asking it to list a directory would be a call that
    // dials, handshakes and then has to explain a failure the test route already
    // reports with an address.
    let capabilities = host.capabilities.ok_or_else(|| {
        ApiError::new(AcpilotError::conflict(format!(
            "host {} has not been tested yet; POST /v1/hosts/{}/test first",
            host.id, host.id
        )))
    })?;
    if !capabilities.filesystem {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "the helper on {} does not read files, so it cannot say whether {root} is there",
            host.id
        ))));
    }
    let address = host
        .id
        .split_once(':')
        .map(|(_, address)| address.to_owned())
        .unwrap_or_else(|| host.id.clone());
    let (program, args) =
        crate::hosts::remote_command(host.kind, &address, &state.hosts().ssh_program());
    let unavailable = |detail: String| {
        ApiError::new(AcpilotError::HostUnavailable {
            host: host.id.clone(),
        })
        .with_detail("transport", detail)
    };
    let connections = Arc::clone(state.hosts().connections());
    let key = host.id.clone();
    // Owned, because the call happens on a blocking thread: the root is the far
    // host's path and nothing borrows it across the hop.
    let path = root.to_owned();
    let asked = tokio::task::spawn_blocking(move || {
        connections.request(&key, &program, &args, "fs.list", json!({ "path": path }))
    });
    let answered = tokio::time::timeout(crate::hosts::HANDSHAKE_TIMEOUT, asked)
        .await
        .map_err(|_| {
            unavailable(format!(
                "{} did not answer within {} seconds",
                host.id,
                crate::hosts::HANDSHAKE_TIMEOUT.as_secs()
            ))
        })?;
    let listing = match answered {
        Err(join) => return Err(unavailable(join.to_string())),
        // The helper's own words, which name the path and why it is not a directory.
        Ok(Err(detail)) => {
            return Err(ApiError::new(AcpilotError::not_found(format!(
                "{root} is not a directory on {}",
                host.id
            )))
            .with_detail("transport", detail));
        }
        Ok(Ok(listing)) => listing,
    };
    if listing.get("entries").and_then(Value::as_array).is_none() {
        return Err(unavailable(format!(
            "the helper answered fs.list with {listing}, which is not a listing"
        )));
    }
    Ok(state.workspaces().open(&host.id, root, name))
}

/// Whether `root` is spelled as a path from the root of a filesystem.
///
/// Both separators: the path is the far host's, and a Windows host answers to
/// `C:\src` while the daemon asking may be running on anything.
fn absolute(root: &str) -> bool {
    root.starts_with('/')
        || root.starts_with('\\')
        || root.as_bytes().get(1).is_some_and(|byte| *byte == b':')
}
