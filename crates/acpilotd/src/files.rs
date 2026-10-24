//! Files: reading and writing one workspace, with optimistic concurrency.
//!
//! Nothing here is on a keystroke path. The editor owns its buffer and saves
//! through `PUT /v1/files`, which is why the route takes a whole file and an
//! `if_match` token rather than a patch: a save that silently overwrote someone
//! else's change would be the one bug this API exists to prevent.
//!
//! # The path rules are the feature
//!
//! Every path is relative to the workspace root, and three separate things
//! disagree about what a path means: the client that wrote it, the string
//! comparison that decides whether it is "inside", and the filesystem that
//! resolves it. So a path is never joined blindly — components are checked here
//! (`..` is refused, not resolved), and the result is canonicalised and compared
//! against the canonical root, which is what catches a symlink pointing out of
//! the workspace. A prefix check on strings alone would pass `link/secret`.
//!
//! # What is deliberately not here
//!
//! No directory creation, no deletion, no watch. The service phase's P3-5 asks
//! for read, list and write with an ETag; the editor phase (P6) is where a
//! buffer meets a file, and it will say what else a save needs.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use acpilot_core::AcpilotError;
use acpilot_proto::ErrorBody;
use acpilot_proto::file::{FileContent, FileEntry, FileWriteRequest};
use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::{ApiError, AppState};

/// The largest file this API moves, in bytes.
///
/// Ten mebibytes is a source file by a wide margin and small enough that a JSON
/// body holding one cannot be mistaken for a stream. Anything larger is a job
/// for the terminal or a future upload route, not for a route whose contract is
/// "here is the text".
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// `GET /v1/files?workspace=&path=`.
///
/// # Errors
///
/// No workspace to read from (`409`), a path that is absolute or escapes (`400`),
/// a path that is not there (`404`), a directory (`409`, naming the list route),
/// a file over the cap or not UTF-8 text (`400`).
async fn read(
    State(state): State<AppState>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileContent>, ApiError> {
    let root = workspace_root(&state, query.workspace.as_deref())?;
    let requested = query
        .path
        .as_deref()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            ApiError::new(AcpilotError::invalid("`path` is required"))
                .with_detail("path", "a read names the file to read")
        })?;
    let relative = relative_path(requested)?;
    let resolved = resolve_existing(&root, &relative, requested)?;

    let metadata = std::fs::metadata(&resolved).map_err(|source| io_error(&resolved, source))?;
    if metadata.is_dir() {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "{requested} is a directory; GET /v1/files/list lists one"
        ))));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(too_large(requested, metadata.len()));
    }
    let bytes = std::fs::read(&resolved).map_err(|source| io_error(&resolved, source))?;
    // Checked again after the read: the file can grow between the two calls, and
    // the cap is about what this route returns, not what the metadata claimed.
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(too_large(requested, bytes.len() as u64));
    }
    let etag = etag(&metadata, &bytes);
    let text = text_of(bytes, requested)?;
    Ok(Json(FileContent {
        path: display_path(&root, &resolved),
        text,
        etag,
    }))
}

/// `GET /v1/files/list?workspace=&path=`.
///
/// # Errors
///
/// The same path rules as a read, plus `404` for a directory that is not there
/// and `409` for a path that is a file.
async fn list(
    State(state): State<AppState>,
    Query(query): Query<FileQuery>,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    let root = workspace_root(&state, query.workspace.as_deref())?;
    let requested = query.path.as_deref().unwrap_or_default();
    let relative = relative_path(requested)?;
    let resolved = resolve_existing(&root, &relative, requested)?;
    if !resolved.is_dir() {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "{requested} is not a directory; GET /v1/files reads a file"
        ))));
    }

    let entries = std::fs::read_dir(&resolved).map_err(|source| io_error(&resolved, source))?;
    let mut listing = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| io_error(&resolved, source))?;
        // A broken symlink has a name and no target. Listing the name with no
        // size or time is the honest answer; failing the whole directory for one
        // stale link is not.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let path = entry.path();
        if !inside(&root, &path) {
            continue;
        }
        listing.push(FileEntry {
            path: display_path(&root, &path),
            is_dir: metadata.is_dir(),
            size: if metadata.is_dir() { 0 } else { metadata.len() },
            modified: modified(&metadata),
        });
    }
    listing.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Json(listing))
}

/// `PUT /v1/files`.
///
/// `if_match` is the whole concurrency story: absent means "create", and a file
/// that already exists is a conflict rather than a silent replace; present means
/// "replace only if it is still the version I read", and a different token is a
/// conflict. Neither is retried or resolved here — the client decides.
///
/// # Errors
///
/// No workspace (`409`), a path that escapes (`400`/`403`), a missing parent
/// directory (`404`), a directory in the way, a missing or stale `if_match`
/// (`409`), or text over the cap (`400`).
async fn write(
    State(state): State<AppState>,
    body: Result<Json<FileWriteRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<FileContent>, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a file write: {rejection}"
        )))
    })?;
    let root = workspace_root(&state, None)?;
    let relative = relative_path(&request.path)?;
    let bytes = request.text.into_bytes();
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(too_large(&request.path, bytes.len() as u64));
    }
    let absolute = root.join(&relative);

    // The parent is resolved, not created: a save into a directory that is not
    // there is a mistake to report, and building the tree would hide it.
    let name = absolute
        .file_name()
        .ok_or_else(|| {
            ApiError::new(AcpilotError::invalid(format!(
                "{} names no file",
                request.path
            )))
            .with_detail("path", "a write names the file to write")
        })?
        .to_owned();
    let parent = absolute.parent().unwrap_or(&root);
    let parent = std::fs::canonicalize(parent).map_err(|_| {
        ApiError::new(AcpilotError::not_found(format!(
            "directory {}",
            display_path(&root, parent)
        )))
    })?;
    if !parent.starts_with(&root) {
        return Err(outside(&request.path, &root));
    }
    if !parent.is_dir() {
        return Err(ApiError::new(AcpilotError::not_found(format!(
            "directory {}",
            display_path(&root, &parent)
        ))));
    }
    let target = parent.join(&name);

    match &request.if_match {
        None => {
            if target.exists() {
                return Err(ApiError::new(AcpilotError::conflict(format!(
                    "{} already exists; read it and send its `etag` as `if_match`",
                    request.path
                ))));
            }
        }
        Some(expected) => {
            let metadata = std::fs::metadata(&target).map_err(|_| {
                ApiError::new(AcpilotError::not_found(format!("file {}", request.path)))
            })?;
            if metadata.is_dir() {
                return Err(ApiError::new(AcpilotError::conflict(format!(
                    "{} is a directory",
                    request.path
                ))));
            }
            // A symlink whose target is outside the workspace is refused before
            // it is read, let alone written through.
            let resolved =
                std::fs::canonicalize(&target).map_err(|source| io_error(&target, source))?;
            if !resolved.starts_with(&root) {
                return Err(outside(&request.path, &root));
            }
            if metadata.len() > MAX_FILE_BYTES {
                return Err(too_large(&request.path, metadata.len()));
            }
            let current = std::fs::read(&resolved).map_err(|source| io_error(&resolved, source))?;
            if &etag(&metadata, &current) != expected {
                return Err(ApiError::new(AcpilotError::conflict(format!(
                    "{} changed since that `etag`; read it again",
                    request.path
                ))));
            }
        }
    }

    std::fs::write(&target, &bytes).map_err(|source| io_error(&target, source))?;
    let metadata = std::fs::metadata(&target).map_err(|source| io_error(&target, source))?;
    let etag = etag(&metadata, &bytes);
    let text = String::from_utf8(bytes)
        .map_err(|_| ApiError::new(AcpilotError::invalid("the text is not UTF-8")))?;
    Ok(Json(FileContent {
        path: display_path(&root, &target),
        text,
        etag,
    }))
}

/// The query string both read routes take.
#[derive(Debug, Deserialize)]
struct FileQuery {
    /// Workspace id, when the client names one. The daemon serves exactly one.
    workspace: Option<String>,
    /// Workspace-relative path. Empty or absent means the root, for a listing.
    path: Option<String>,
}

/// The root, checked against the id the client named.
///
/// An empty id means "the one you serve", which is the only value a v1 client
/// can usefully send before workspaces are plural. A different id is a `404`
/// naming both, because a client that thinks it is editing directory A while
/// the daemon serves B has no way to notice otherwise.
fn workspace_root(state: &AppState, workspace: Option<&str>) -> Result<PathBuf, ApiError> {
    let served = state.workspace().ok_or_else(|| {
        ApiError::new(AcpilotError::conflict(
            "this daemon was started without a workspace, so it has no files to serve",
        ))
    })?;
    if let Some(id) = workspace.filter(|id| !id.is_empty())
        && id != served.id
    {
        return Err(ApiError::new(AcpilotError::not_found(format!(
            "workspace {id}; this daemon serves {}",
            served.id
        ))));
    }
    Ok(served.root.clone())
}

/// A client's path as components, with every escape refused rather than resolved.
///
/// `..` is not joined and then canonicalised: it is a `400` here, so the answer
/// does not depend on whether the directory it climbs out of happens to exist.
fn relative_path(requested: &str) -> Result<PathBuf, ApiError> {
    let path = Path::new(requested);
    if path.is_absolute() {
        return Err(ApiError::new(AcpilotError::invalid(format!(
            "{requested} is absolute; paths are relative to the workspace root"
        )))
        .with_detail("path", "send a path relative to the workspace root"));
    }
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            _ => {
                return Err(ApiError::new(AcpilotError::invalid(format!(
                    "{requested} leaves the workspace"
                )))
                .with_detail("path", "`..` and roots are not part of a workspace path"));
            }
        }
    }
    Ok(relative)
}

/// The canonical path of something that must exist, and must be inside `root`.
fn resolve_existing(root: &Path, relative: &Path, requested: &str) -> Result<PathBuf, ApiError> {
    let absolute = root.join(relative);
    let resolved = std::fs::canonicalize(&absolute).map_err(|_| {
        ApiError::new(AcpilotError::not_found(format!(
            "path {}",
            empty_as_root(requested)
        )))
    })?;
    if !resolved.starts_with(root) {
        return Err(outside(requested, root));
    }
    Ok(resolved)
}

/// Whether `path` resolves to somewhere inside `root`.
///
/// A path that does not resolve (a broken symlink) is inside by its name only,
/// which is what a listing needs: it reports the entry it found.
fn inside(root: &Path, path: &Path) -> bool {
    match std::fs::canonicalize(path) {
        Ok(resolved) => resolved.starts_with(root),
        Err(_) => !path.is_absolute() || path.starts_with(root),
    }
}

/// The `/`-separated path of `path` relative to `root`, on every platform.
fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The token a client sends back as `if_match`.
///
/// Modification time, size and a hash of the bytes: the first two make a change
/// visible without reading the file, and the hash catches the case they miss —
/// an edit that preserved both, which is what a fast tool does.
fn etag(metadata: &std::fs::Metadata, bytes: &[u8]) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        });
    let digest = Sha256::digest(bytes);
    let hash: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{modified:x}-{:x}-{hash}", metadata.len())
}

/// RFC 3339, when the host reports a modification time.
fn modified(metadata: &std::fs::Metadata) -> Option<String> {
    let since = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let millis = i64::try_from(since.as_millis()).ok()?;
    Some(jiff::Timestamp::from_millisecond(millis).ok()?.to_string())
}

/// UTF-8 text, or a refusal that says which file it refused.
///
/// A NUL byte is checked separately from UTF-8 validity: a UTF-16 file with an
/// ASCII body is valid UTF-8 in the bytes it shares, and it is still not text
/// this API can hand back.
fn text_of(bytes: Vec<u8>, requested: &str) -> Result<String, ApiError> {
    let text = String::from_utf8(bytes).map_err(|_| binary(requested))?;
    if text.contains('\0') {
        return Err(binary(requested));
    }
    Ok(text)
}

fn binary(requested: &str) -> ApiError {
    ApiError::new(AcpilotError::invalid(format!(
        "{requested} is not UTF-8 text; this API moves text, not bytes"
    )))
    .with_detail("path", "a binary file is refused rather than mangled")
}

fn too_large(requested: &str, size: u64) -> ApiError {
    ApiError::new(AcpilotError::invalid(format!(
        "{requested} is {size} bytes; the cap is {MAX_FILE_BYTES}"
    )))
    .with_detail("path", format!("at most {MAX_FILE_BYTES} bytes"))
}

fn outside(requested: &str, root: &Path) -> ApiError {
    ApiError::new(AcpilotError::denied(format!(
        "{requested} resolves outside the workspace at {}",
        root.display()
    )))
    .with_detail("path", "the path must stay inside the workspace")
}

fn empty_as_root(requested: &str) -> &str {
    if requested.is_empty() { "." } else { requested }
}

fn io_error(path: &Path, source: io::Error) -> ApiError {
    ApiError::new(AcpilotError::Io(io::Error::new(
        source.kind(),
        format!("{}: {source}", path.display()),
    )))
}

/// The file routes, ready to merge into the daemon's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/files", get(read).put(write))
        .route("/v1/files/list", get(list))
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/files",
    tag = "files",
    summary = "Read a file",
    params(
        ("workspace" = Option<String>, Query, description = "Workspace id; empty or absent means the one this daemon serves"),
        ("path" = String, Query, description = "Path relative to the workspace root")
    ),
    responses(
        (status = 200, description = "The text and the token to write it back", body = FileContent),
        (status = 400, description = "A path that escapes, a binary file, or one over the cap", body = ErrorBody),
        (status = 403, description = "A path that resolves outside the workspace", body = ErrorBody),
        (status = 404, description = "No such path", body = ErrorBody),
        (status = 409, description = "A directory, which the list route serves", body = ErrorBody)
    )
)]
fn read_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/files/list",
    tag = "files",
    summary = "List a directory",
    params(
        ("workspace" = Option<String>, Query, description = "Workspace id; empty or absent means the one this daemon serves"),
        ("path" = Option<String>, Query, description = "Directory relative to the workspace root; empty or absent means the root")
    ),
    responses(
        (status = 200, description = "Entries, sorted by path", body = Vec<FileEntry>),
        (status = 400, description = "A path that escapes", body = ErrorBody),
        (status = 404, description = "No such directory", body = ErrorBody),
        (status = 409, description = "A file, which the read route serves", body = ErrorBody)
    )
)]
fn list_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    put,
    path = "/v1/files",
    tag = "files",
    summary = "Write a file",
    request_body = FileWriteRequest,
    responses(
        (status = 200, description = "The text as written, with its new token", body = FileContent),
        (status = 400, description = "Not UTF-8, over the cap, or a path that escapes", body = ErrorBody),
        (status = 403, description = "A path that resolves outside the workspace", body = ErrorBody),
        (status = 404, description = "The parent directory does not exist, or `if_match` names a file that is gone", body = ErrorBody),
        (status = 409, description = "The file exists without `if_match`, or its token changed", body = ErrorBody)
    )
)]
fn write_doc() {}

/// The OpenAPI operations this module adds.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    use utoipa::Path;
    vec![
        (
            "/v1/files",
            HttpMethod::Get,
            <__path_read_doc as Path>::operation(),
        ),
        (
            "/v1/files",
            HttpMethod::Put,
            <__path_write_doc as Path>::operation(),
        ),
        (
            "/v1/files/list",
            HttpMethod::Get,
            <__path_list_doc as Path>::operation(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dot_dot_path_is_refused_before_it_is_joined() {
        for path in ["../secret", "a/../../b", "/etc/passwd", "a/./../b"] {
            let error = relative_path(path).expect_err(path);
            assert_eq!(
                error.status(),
                axum::http::StatusCode::BAD_REQUEST,
                "{path}"
            );
        }
    }

    #[test]
    fn a_plain_path_keeps_its_components() {
        assert_eq!(
            relative_path("src/main.rs").expect("relative"),
            PathBuf::from("src/main.rs")
        );
        assert_eq!(
            relative_path("./src/./lib.rs").expect("relative"),
            PathBuf::from("src/lib.rs")
        );
    }

    #[test]
    fn a_path_is_displayed_with_forward_slashes() {
        let root = Path::new("/work/app");
        assert_eq!(
            display_path(root, Path::new("/work/app/src/lib.rs")),
            "src/lib.rs"
        );
    }
}
