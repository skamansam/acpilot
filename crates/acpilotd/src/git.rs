//! Git over the workspace: status, diff, tree, log, and the index's two writes.
//!
//! Every answer comes from the host's own `git`, through `acpilot-git`, so the
//! source-control pane agrees with the command line about what is staged. The
//! repository is discovered from the workspace root, and the paths in every answer
//! are relative to the **repository root** — which is what git reports, and the one
//! spelling a patch and a status can share. A workspace opened inside a monorepo
//! therefore sees the repository's paths, not just its own directory; the tree route
//! is where a client asks what belongs to the workspace it is drawing.
//!
//! # What these routes write
//!
//! `stage` and `unstage` move paths across the index; `discard` restores the working
//! tree from the index, and `checkout` restores both from `HEAD`. Every write answers
//! with the status read after git ran. None of them makes a commit or changes config:
//! the committed message is P6.5-3's, and the composer's confirmation is what reaches
//! it. The destructive routes are reached only after a client's own confirmation.
//!
//! A workspace on another host is refused with `409`, like every other route that
//! walks this machine's filesystem. Its tree still draws the letters through
//! `GET /v1/files/list`, because that route asks the host; a diff over there needs a
//! helper method that does not exist yet, and a plausible patch about a different
//! machine would be worse than the refusal.
//!
//! A host without `git` is `502 host_unavailable` with the tool named in `details`,
//! rather than a `500` that reads as a bug in the daemon. "Not a repository" is a
//! `404`: there is no repository to describe, which is the same answer a route that
//! read one file would give.

use std::collections::BTreeSet;

use acpilot_core::AcpilotError;
use acpilot_git::{ChangeKind, DiffScope, GitError, Repo, Status};
use acpilot_proto::ErrorBody;
use acpilot_proto::git::{
    GitChangeKind, GitCommit, GitDiff, GitDiffFile, GitHunkRequest, GitIndexRequest, GitStatus,
    GitStatusEntry, GitTreeEntry,
};
use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use serde::Deserialize;
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::{ApiError, AppState};

/// How many commits `/v1/git/log` returns when the caller does not say.
///
/// A source-control pane shows a handful and scrolls; fifty is enough to fill one and
/// small enough that a first paint is not a walk of the whole history.
pub const DEFAULT_LOG_LIMIT: usize = 50;

/// The most commits one call returns.
///
/// The same rule as the search caps: a request for more than this is a request for a
/// history view, which pages, rather than for the pane that asked. Clamped rather
/// than refused because the caller's intent is clear and an error would only make it
/// ask again with a number it had to learn.
pub const MAX_LOG_LIMIT: usize = 500;

/// The query string every git route accepts.
#[derive(Debug, Deserialize)]
struct GitQuery {
    /// Workspace id, when the client names one. The daemon serves exactly one.
    workspace: Option<String>,
}

/// The diff route's query string.
#[derive(Debug, Deserialize)]
struct DiffQuery {
    /// Workspace id, when the client names one.
    workspace: Option<String>,
    /// `unstaged` (default), `staged` or `all`.
    scope: Option<String>,
}

/// The log route's query string.
#[derive(Debug, Deserialize)]
struct LogQuery {
    /// Workspace id, when the client names one.
    workspace: Option<String>,
    /// How many commits, newest first. Defaults to [`DEFAULT_LOG_LIMIT`].
    limit: Option<usize>,
}

/// `GET /v1/git/status?workspace=`.
///
/// # Errors
///
/// No workspace (`409`), a workspace on another host (`409`), a directory that is not
/// inside a repository (`404`), or no `git` on the host (`502`).
async fn status(
    State(state): State<AppState>,
    Query(query): Query<GitQuery>,
) -> Result<Json<GitStatus>, ApiError> {
    let repo = repository(&state, query.workspace.as_deref())?;
    let status = repo.status().map_err(git_error)?;
    Ok(Json(status_json(status)))
}

/// `POST /v1/git/stage`.
///
/// Adds each path to the index, including an untracked file or a deletion, and
/// answers with the status read after git ran. The path check is in
/// [`Repo::stage`], before git sees a `../` a client should not have sent.
///
/// # Errors
///
/// `400` for no paths or one that is not repository-relative; the path and host
/// failures of [`status`]; `409` when git refuses a path or the index is locked.
async fn stage(
    State(state): State<AppState>,
    Json(request): Json<GitIndexRequest>,
) -> Result<Json<GitStatus>, ApiError> {
    set_index(&state, request, true)
}

/// `POST /v1/git/stage-hunk`.
///
/// Applies one hunk from the path's **unstaged** diff to the index and answers
/// with the status read afterwards. A hunk is named by its zero-based position in
/// that diff, and the patch is cut from git's own output: the request never carries
/// patch text that could stage something the working tree does not contain.
///
/// # Errors
///
/// `400` for a path that is not repository-relative; the path and host failures of
/// [`status`]; `409` when the path has no unstaged hunk, the index is past the end
/// of that diff, or git refuses the patch.
async fn stage_hunk(
    State(state): State<AppState>,
    Json(request): Json<GitHunkRequest>,
) -> Result<Json<GitStatus>, ApiError> {
    let repo = repository(&state, request.workspace.as_deref())?;
    repo.stage_hunk(&request.path, request.hunk)
        .map_err(index_error)?;
    let after = repo.status().map_err(git_error)?;
    Ok(Json(status_json(after)))
}

/// `POST /v1/git/unstage`.
///
/// Removes each path from the next commit and leaves the working tree alone. In a
/// repository with no commit, the index is emptied with `git rm --cached` because
/// there is no `HEAD` to reset to.
///
/// # Errors
///
/// The same failures as [`stage`].
async fn unstage(
    State(state): State<AppState>,
    Json(request): Json<GitIndexRequest>,
) -> Result<Json<GitStatus>, ApiError> {
    set_index(&state, request, false)
}

/// `POST /v1/git/discard`.
///
/// Restores each path's working-tree half from the index and answers the status read
/// afterwards. A staged edit stays staged, and an untracked path is removed by git's
/// clean because the index has no version to restore.
///
/// # Errors
///
/// `400` for no paths or one that is not repository-relative; the path and host
/// failures of [`status`]; `409` when there is nothing to restore or git refuses.
async fn discard(
    State(state): State<AppState>,
    Json(request): Json<GitIndexRequest>,
) -> Result<Json<GitStatus>, ApiError> {
    restore_paths(&state, request, false)
}

/// `POST /v1/git/checkout`.
///
/// Restores both the index and the working tree for each path from `HEAD`. A staged
/// addition is removed, as is the new half of a rename; an untracked path is refused
/// because it has no `HEAD` version.
///
/// # Errors
///
/// The same failures as [`discard`], plus `409` when there is no `HEAD`.
async fn checkout(
    State(state): State<AppState>,
    Json(request): Json<GitIndexRequest>,
) -> Result<Json<GitStatus>, ApiError> {
    restore_paths(&state, request, true)
}

/// Stage or unstage the request's paths, and answer with the new index.
///
/// A rename has two paths: the row names the new one and carries the old one beside
/// it. Moving only the new path across the index would leave the other half staged,
/// so the old path travels with the row the person selected.
fn set_index(
    state: &AppState,
    request: GitIndexRequest,
    staged: bool,
) -> Result<Json<GitStatus>, ApiError> {
    if request.paths.is_empty() {
        return Err(ApiError::new(AcpilotError::invalid("`paths` is empty"))
            .with_detail("paths", "at least one repository-relative path"));
    }

    let repo = repository(state, request.workspace.as_deref())?;
    let before = repo.status().map_err(git_error)?;
    let mut paths: BTreeSet<String> = request.paths.iter().cloned().collect();
    for entry in &before.files {
        if request.paths.iter().any(|path| path == &entry.path)
            && let Some(old_path) = &entry.orig_path
        {
            paths.insert(old_path.clone());
        }
    }
    let paths: Vec<String> = paths.into_iter().collect();

    if staged {
        repo.stage(&paths).map_err(index_error)?;
    } else {
        repo.unstage(&paths).map_err(index_error)?;
    }
    let after = repo.status().map_err(git_error)?;
    Ok(Json(status_json(after)))
}

/// Discard or check out the request's paths, and answer with the new status.
///
/// The repository resolves a rename's two halves itself: discarding only touches the
/// working-tree path, while checking out restores the old name and removes the new
/// one. The daemon does not expand `old_path` here, because the same expansion would
/// ask `git restore` to name a path that is not in the working tree.
fn restore_paths(
    state: &AppState,
    request: GitIndexRequest,
    checkout: bool,
) -> Result<Json<GitStatus>, ApiError> {
    if request.paths.is_empty() {
        return Err(ApiError::new(AcpilotError::invalid("`paths` is empty"))
            .with_detail("paths", "at least one repository-relative path"));
    }

    let repo = repository(state, request.workspace.as_deref())?;
    if checkout {
        repo.checkout(&request.paths).map_err(index_error)?;
    } else {
        repo.discard(&request.paths).map_err(index_error)?;
    }
    let after = repo.status().map_err(git_error)?;
    Ok(Json(status_json(after)))
}

/// `GET /v1/git/diff?workspace=&scope=`.
///
/// `scope` is `unstaged` (working tree against the index) when it is absent,
/// `staged` (index against `HEAD`), or `all` (working tree against `HEAD`). An
/// unknown scope is `400` with the three spellings in `details`, rather than a
/// default that quietly answers a different question.
///
/// # Errors
///
/// The path and host failures of [`status`], plus `400` for an unknown scope.
async fn diff(
    State(state): State<AppState>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<GitDiff>, ApiError> {
    let scope = scope_of(query.scope.as_deref())?;
    let repo = repository(&state, query.workspace.as_deref())?;
    let diff = repo.diff(scope).map_err(git_error)?;
    Ok(Json(GitDiff {
        patch: diff.patch,
        files: diff
            .files
            .into_iter()
            .map(|file| GitDiffFile {
                path: file.path,
                old_path: file.old_path,
                change: change_kind(file.change),
            })
            .collect(),
    }))
}

/// `GET /v1/git/tree?workspace=`.
///
/// Every path in the working tree, tracked or not, sorted, with the index's answer
/// for each. This is the listing a source-control tree folds into directories; the
/// file tree a client draws from filesystem rows is `GET /v1/files/list`.
///
/// # Errors
///
/// The path and host failures of [`status`].
async fn tree(
    State(state): State<AppState>,
    Query(query): Query<GitQuery>,
) -> Result<Json<Vec<GitTreeEntry>>, ApiError> {
    let repo = repository(&state, query.workspace.as_deref())?;
    let entries = repo.tree().map_err(git_error)?;
    Ok(Json(
        entries
            .into_iter()
            .map(|entry| GitTreeEntry {
                path: entry.path,
                tracked: entry.tracked,
            })
            .collect(),
    ))
}

/// `GET /v1/git/log?workspace=&limit=`.
///
/// Newest first, in topological order so a graph can be drawn without re-sorting.
/// `limit` defaults to [`DEFAULT_LOG_LIMIT`] and is capped at [`MAX_LOG_LIMIT`]; zero
/// is an empty answer, which is the same thing a repository with no commits gives.
///
/// # Errors
///
/// The path and host failures of [`status`].
async fn log(
    State(state): State<AppState>,
    Query(query): Query<LogQuery>,
) -> Result<Json<Vec<GitCommit>>, ApiError> {
    let repo = repository(&state, query.workspace.as_deref())?;
    let commits = repo.log(log_limit(query.limit)).map_err(git_error)?;
    Ok(Json(
        commits
            .into_iter()
            .map(|commit| GitCommit {
                id: commit.id,
                parents: commit.parents,
                author: commit.author,
                email: commit.email,
                authored_at: commit.authored_at,
                subject: commit.subject,
            })
            .collect(),
    ))
}

/// The repository containing this daemon's workspace.
fn repository(state: &AppState, workspace: Option<&str>) -> Result<Repo, ApiError> {
    let root = crate::files::workspace_root(state, workspace)?;
    Repo::discover(&root).map_err(git_error)
}

/// The scope a client asked for.
fn scope_of(scope: Option<&str>) -> Result<DiffScope, ApiError> {
    match scope.unwrap_or("unstaged") {
        "unstaged" => Ok(DiffScope::Unstaged),
        "staged" => Ok(DiffScope::Staged),
        "all" => Ok(DiffScope::All),
        other => Err(ApiError::new(AcpilotError::invalid(format!(
            "unknown diff scope {other:?}"
        )))
        .with_detail("scope", "unstaged, staged or all")),
    }
}

/// How many commits to ask for: the default, a caller's number, capped.
fn log_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT)
}

/// A status in the wire's spelling.
fn status_json(status: Status) -> GitStatus {
    GitStatus {
        branch: status.branch,
        upstream: status.upstream,
        ahead: status.ahead,
        behind: status.behind,
        head: status.head,
        files: status.files.into_iter().map(status_entry).collect(),
    }
}

/// One status row in the wire's spelling.
fn status_entry(entry: acpilot_git::StatusEntry) -> GitStatusEntry {
    GitStatusEntry {
        path: entry.path,
        old_path: entry.orig_path,
        index: change_kind(entry.index),
        worktree: change_kind(entry.worktree),
    }
}

/// A git change kind in the wire's spelling.
fn change_kind(kind: ChangeKind) -> GitChangeKind {
    match kind {
        ChangeKind::Unmodified => GitChangeKind::Unmodified,
        ChangeKind::Modified => GitChangeKind::Modified,
        ChangeKind::Added => GitChangeKind::Added,
        ChangeKind::Deleted => GitChangeKind::Deleted,
        ChangeKind::Renamed => GitChangeKind::Renamed,
        ChangeKind::Copied => GitChangeKind::Copied,
        ChangeKind::TypeChanged => GitChangeKind::TypeChanged,
        ChangeKind::Unmerged => GitChangeKind::Unmerged,
        ChangeKind::Untracked => GitChangeKind::Untracked,
        ChangeKind::Ignored => GitChangeKind::Ignored,
    }
}

/// A git failure in this API's terms.
///
/// The two that a caller can act on are named: no repository is `404`, and no `git`
/// on the host is `502 host_unavailable` with the missing tool in `details`, because
/// installing it is the fix. Everything else is the host's own failure and travels
/// with git's words rather than being flattened into "git failed".
pub(crate) fn git_error(error: GitError) -> ApiError {
    match error {
        GitError::GitMissing(source) => ApiError::new(AcpilotError::HostUnavailable {
            host: crate::hosts::LOCAL_HOST.to_owned(),
        })
        .with_detail(
            "git",
            format!("git is not installed or not on PATH: {source}"),
        ),
        GitError::NotARepository { path } => ApiError::new(AcpilotError::not_found(format!(
            "a git repository containing {}",
            path.display()
        )))
        .with_detail("path", "open a directory inside a repository"),
        GitError::Command {
            command,
            status,
            stderr,
        } => ApiError::new(AcpilotError::Io(std::io::Error::other(format!(
            "git {command} exited {status}: {stderr}"
        )))),
        GitError::EmptyMessage => {
            ApiError::new(AcpilotError::invalid("a commit message cannot be empty"))
                .with_detail("message", "a subject line, and a body when there is one")
        }
        GitError::NoPaths => ApiError::new(AcpilotError::invalid("no paths were named"))
            .with_detail("paths", "at least one repository-relative path"),
        GitError::NoChanges => ApiError::new(AcpilotError::conflict(
            "the named paths have no changes to restore",
        ))
        .with_detail("paths", "a path with a working-tree or index change"),
        GitError::NoHunks { path } => ApiError::new(AcpilotError::conflict(format!(
            "{path:?} has no unstaged hunks to stage"
        )))
        .with_detail("hunk", "a hunk in the path's unstaged diff"),
        GitError::HunkNotFound { path, hunk } => ApiError::new(AcpilotError::conflict(format!(
            "{path:?} has no unstaged hunk {hunk}"
        )))
        .with_detail(
            "hunk",
            "a zero-based index into the path's current unstaged diff",
        ),
        GitError::NoHead => {
            ApiError::new(AcpilotError::conflict("there is no HEAD to check out from"))
                .with_detail("paths", "commit something before checking a path out")
        }
        GitError::NotInHead { path } => ApiError::new(AcpilotError::conflict(format!(
            "{path:?} is untracked; discard it instead"
        )))
        .with_detail("paths", "checkout needs a version in HEAD"),
        GitError::InvalidPath { path } => ApiError::new(AcpilotError::invalid(format!(
            "not a repository-relative path: {path:?}"
        )))
        .with_detail(
            "paths",
            "a path from GET /v1/git/status, relative to the repository root",
        ),
        GitError::Parse { what, reason } => ApiError::new(AcpilotError::Protocol {
            message: format!("could not parse git {what}: {reason}"),
        }),
        GitError::Io(source) => ApiError::new(AcpilotError::Io(source)),
    }
}

/// A failure from an index write.
///
/// Git refusing an `add` or a `reset` is a conflict the person can fix — a locked
/// index, a pathspec that does not match — rather than a daemon fault, so it gets
/// the same `409` shape as a refused commit and git's own words in `details`.
fn index_error(error: GitError) -> ApiError {
    match error {
        GitError::Command {
            command,
            status,
            stderr,
        } => {
            let reason = stderr.trim();
            ApiError::new(AcpilotError::conflict(format!(
                "git {command} exited {status}: {reason}"
            )))
            .with_detail("paths", reason.to_owned())
        }
        other => git_error(other),
    }
}

/// The git routes, ready to merge into the daemon's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/git/status", get(status))
        .route("/v1/git/diff", get(diff))
        .route("/v1/git/tree", get(tree))
        .route("/v1/git/log", get(log))
        .route("/v1/git/stage", post(stage))
        .route("/v1/git/stage-hunk", post(stage_hunk))
        .route("/v1/git/unstage", post(unstage))
        .route("/v1/git/discard", post(discard))
        .route("/v1/git/checkout", post(checkout))
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/git/status",
    tag = "git",
    summary = "Branch state and changed paths",
    params(
        ("workspace" = Option<String>, Query, description = "Workspace id; empty or absent means the one this daemon serves")
    ),
    responses(
        (status = 200, description = "Branch, upstream, ahead/behind and both sides of every change; paths are repository-relative", body = GitStatus),
        (status = 404, description = "The workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "No workspace, or one on another host", body = ErrorBody),
        (status = 502, description = "`git` is not installed on the host", body = ErrorBody)
    )
)]
fn git_status_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/git/diff",
    tag = "git",
    summary = "A unified diff and the files it touches",
    params(
        ("workspace" = Option<String>, Query, description = "Workspace id; empty or absent means the one this daemon serves"),
        ("scope" = Option<String>, Query, description = "`unstaged` (default), `staged` or `all`")
    ),
    responses(
        (status = 200, description = "The patch and its file list; paths are repository-relative", body = GitDiff),
        (status = 400, description = "An unknown scope", body = ErrorBody),
        (status = 404, description = "The workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "No workspace, or one on another host", body = ErrorBody),
        (status = 502, description = "`git` is not installed on the host", body = ErrorBody)
    )
)]
fn git_diff_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/git/tree",
    tag = "git",
    summary = "Every path in the working tree, tracked or not",
    params(
        ("workspace" = Option<String>, Query, description = "Workspace id; empty or absent means the one this daemon serves")
    ),
    responses(
        (status = 200, description = "Paths sorted, each with whether the index knows it; paths are repository-relative", body = Vec<GitTreeEntry>),
        (status = 404, description = "The workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "No workspace, or one on another host", body = ErrorBody),
        (status = 502, description = "`git` is not installed on the host", body = ErrorBody)
    )
)]
fn git_tree_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/git/log",
    tag = "git",
    summary = "Recent commits, newest first",
    params(
        ("workspace" = Option<String>, Query, description = "Workspace id; empty or absent means the one this daemon serves"),
        ("limit" = Option<usize>, Query, description = "How many commits, defaults to 50 and is capped at 500")
    ),
    responses(
        (status = 200, description = "Commits in topological order, each with its parents", body = Vec<GitCommit>),
        (status = 404, description = "The workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "No workspace, or one on another host", body = ErrorBody),
        (status = 502, description = "`git` is not installed on the host", body = ErrorBody)
    )
)]
fn git_log_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/git/stage",
    tag = "git",
    summary = "Put repository paths in the index",
    request_body = GitIndexRequest,
    responses(
        (status = 200, description = "The status read after git staged the paths; paths are repository-relative", body = GitStatus),
        (status = 400, description = "No paths, or one that is not repository-relative", body = ErrorBody),
        (status = 404, description = "The workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "No workspace, one on another host, or git refused the index write", body = ErrorBody),
        (status = 502, description = "`git` is not installed on the host", body = ErrorBody)
    )
)]
fn git_stage_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/git/stage-hunk",
    tag = "git",
    summary = "Put one unstaged hunk in the index",
    request_body = GitHunkRequest,
    responses(
        (status = 200, description = "The status read after git applied the hunk; other hunks stay unstaged", body = GitStatus),
        (status = 400, description = "A path that is not repository-relative", body = ErrorBody),
        (status = 404, description = "The workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "No workspace, one on another host, no unstaged hunk at that index, or git refused the patch", body = ErrorBody),
        (status = 502, description = "`git` is not installed on the host", body = ErrorBody)
    )
)]
fn git_stage_hunk_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/git/unstage",
    tag = "git",
    summary = "Remove repository paths from the index",
    request_body = GitIndexRequest,
    responses(
        (status = 200, description = "The status read after git unstaged the paths; the working tree is untouched", body = GitStatus),
        (status = 400, description = "No paths, or one that is not repository-relative", body = ErrorBody),
        (status = 404, description = "The workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "No workspace, one on another host, or git refused the index write", body = ErrorBody),
        (status = 502, description = "`git` is not installed on the host", body = ErrorBody)
    )
)]
fn git_unstage_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/git/discard",
    tag = "git",
    summary = "Restore repository paths' working-tree half from the index",
    request_body = GitIndexRequest,
    responses(
        (status = 200, description = "The status read after git restored the paths; staged changes stay staged", body = GitStatus),
        (status = 400, description = "No paths, or one that is not repository-relative", body = ErrorBody),
        (status = 404, description = "The workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "No workspace, one on another host, nothing to restore, or git refused the write", body = ErrorBody),
        (status = 502, description = "`git` is not installed on the host", body = ErrorBody)
    )
)]
fn git_discard_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/git/checkout",
    tag = "git",
    summary = "Restore repository paths' index and working tree from HEAD",
    request_body = GitIndexRequest,
    responses(
        (status = 200, description = "The status read after git checked the paths out; staged additions are removed", body = GitStatus),
        (status = 400, description = "No paths, or one that is not repository-relative", body = ErrorBody),
        (status = 404, description = "The workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "No workspace, one on another host, no HEAD, an untracked path, or git refused the write", body = ErrorBody),
        (status = 502, description = "`git` is not installed on the host", body = ErrorBody)
    )
)]
fn git_checkout_doc() {}

/// This module's operations, for the OpenAPI document.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    use utoipa::Path;
    vec![
        (
            "/v1/git/status",
            HttpMethod::Get,
            <__path_git_status_doc as Path>::operation(),
        ),
        (
            "/v1/git/diff",
            HttpMethod::Get,
            <__path_git_diff_doc as Path>::operation(),
        ),
        (
            "/v1/git/tree",
            HttpMethod::Get,
            <__path_git_tree_doc as Path>::operation(),
        ),
        (
            "/v1/git/log",
            HttpMethod::Get,
            <__path_git_log_doc as Path>::operation(),
        ),
        (
            "/v1/git/stage",
            HttpMethod::Post,
            <__path_git_stage_doc as Path>::operation(),
        ),
        (
            "/v1/git/stage-hunk",
            HttpMethod::Post,
            <__path_git_stage_hunk_doc as Path>::operation(),
        ),
        (
            "/v1/git/unstage",
            HttpMethod::Post,
            <__path_git_unstage_doc as Path>::operation(),
        ),
        (
            "/v1/git/discard",
            HttpMethod::Post,
            <__path_git_discard_doc as Path>::operation(),
        ),
        (
            "/v1/git/checkout",
            HttpMethod::Post,
            <__path_git_checkout_doc as Path>::operation(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_scope_names_the_three_it_accepts() {
        let error = scope_of(Some("everything")).expect_err("not a scope");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(error.details.len(), 1);
        assert_eq!(error.details[0].field, "scope");
        assert!(
            error.details[0].message.contains("unstaged"),
            "the answer to a typo is the spelling to use: {}",
            error.details[0].message
        );
    }

    #[test]
    fn the_default_scope_is_the_working_tree_against_the_index() {
        assert_eq!(scope_of(None).expect("a default"), DiffScope::Unstaged);
        assert_eq!(scope_of(Some("staged")).expect("staged"), DiffScope::Staged);
        assert_eq!(scope_of(Some("all")).expect("all"), DiffScope::All);
    }

    #[test]
    fn a_log_limit_is_bounded_rather_than_unbounded() {
        assert_eq!(log_limit(None), DEFAULT_LOG_LIMIT);
        assert_eq!(log_limit(Some(7)), 7);
        assert_eq!(
            log_limit(Some(usize::MAX)),
            MAX_LOG_LIMIT,
            "a number past the cap asks for the cap, not for the whole history"
        );
    }
}
