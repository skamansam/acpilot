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

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use acpilot_core::AcpilotError;
use acpilot_git::{ChangeKind, Repo, StatusEntry};
use acpilot_lsp::position::{LineIndex, Position};
use acpilot_proto::ErrorBody;
use acpilot_proto::file::{
    Diagnostic, DiagnosticSeverity, DiagnosticsRequest, FileChange, FileContent, FileDiagnostics,
    FileEntry, FileWriteRequest, GrepMatch, LineChange, LineMark, Reference, ReferencesRequest,
};
use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
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

/// The most paths a search returns.
///
/// A query that matches half a repository is the wrong query, not a listing: the
/// cap keeps one keystroke from walking a monorepo into a status line, and the walk
/// stops where it does rather than reading everything and throwing it away.
pub const MAX_SEARCH_RESULTS: usize = 200;

/// The most candidates a path search collects before ranking.
///
/// A fuzzy ranking has to see the whole answer before it can put the right file
/// first, so the cap cannot stop the walk the way a plain `contains` could. It
/// bounds what one keystroke's walk remembers instead: thirty times the number
/// returned, which is enough headroom for a broad query to still rank the file
/// somebody meant.
const MAX_SEARCH_CANDIDATES: usize = 6_000;

/// The most lines a content search returns.
///
/// The same rule as the path search's cap: a query that matches half a repository
/// is the wrong query, and the answer that says "200 of them" is more useful than
/// a response the length of the workspace.
pub const MAX_GREP_RESULTS: usize = 200;

/// The most uses a references search returns.
///
/// The content search's cap and its reason: a name used two hundred times is a name
/// whose answer is a rename rather than a list.
pub const MAX_REFERENCES: usize = 200;

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
    match source(&state, query.workspace.as_deref())? {
        Source::Local(root) => read_local(&root, &query),
        Source::Remote { host, root } => read_remote(&state, &host, &root, &query).await,
    }
}

/// [`read`], from a directory this daemon can open itself.
fn read_local(root: &Path, query: &FileQuery) -> Result<Json<FileContent>, ApiError> {
    let requested = query
        .path
        .as_deref()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            ApiError::new(AcpilotError::invalid("`path` is required"))
                .with_detail("path", "a read names the file to read")
        })?;
    let relative = relative_path(requested)?;
    let resolved = resolve_existing(root, &relative, requested)?;

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
    let path = display_path(root, &resolved);
    let marks = line_marks(root, &path);
    Ok(Json(FileContent {
        path,
        text,
        etag,
        marks,
    }))
}

/// The lines of one file that differ from the last commit, for a gutter.
///
/// Asked here rather than in the client for the same reason the tree's letters are:
/// git reads a repository on the host the files are on, and a client that ran `git`
/// itself would be answering about a different machine. Nothing is an error: a file
/// git does not track, a workspace that is not a repository and a host with no `git`
/// are all a gutter with no marks, which is what an untouched file draws too.
fn line_marks(root: &Path, path: &str) -> Vec<LineMark> {
    let Ok(repo) = Repo::discover(root) else {
        return Vec::new();
    };
    // `git diff` speaks paths from the repository's root, which is not always the
    // workspace's: a package opened inside a monorepo is the ordinary case.
    let (Ok(workspace), Ok(top)) = (root.canonicalize(), repo.root().canonicalize()) else {
        return Vec::new();
    };
    let Ok(prefix) = workspace.strip_prefix(&top) else {
        return Vec::new();
    };
    let relative = if prefix.as_os_str().is_empty() {
        path.to_owned()
    } else {
        format!("{}/{path}", prefix.to_string_lossy().replace('\\', "/"))
    };
    let Ok(changes) = repo.line_changes(&relative) else {
        return Vec::new();
    };
    changes
        .iter()
        .map(|(line, change)| LineMark {
            line: u32::try_from(line + 1).unwrap_or(u32::MAX),
            change: match change {
                acpilot_git::LineChange::Added => LineChange::Added,
                acpilot_git::LineChange::Modified => LineChange::Modified,
                acpilot_git::LineChange::Deleted => LineChange::Deleted,
            },
        })
        .collect()
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
    match source(&state, query.workspace.as_deref())? {
        Source::Local(root) => list_local(&state, &root, &query),
        Source::Remote { host, root } => list_remote(&state, &host, &root, &query).await,
    }
}

/// [`list`], from a directory this daemon can open itself.
fn list_local(
    state: &AppState,
    root: &Path,
    query: &FileQuery,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    let requested = query.path.as_deref().unwrap_or_default();
    let relative = relative_path(requested)?;
    let resolved = resolve_existing(root, &relative, requested)?;
    if !resolved.is_dir() {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "{requested} is not a directory; GET /v1/files reads a file"
        ))));
    }

    // The preference is the daemon's, so a tree looks the same in every client and
    // the patterns are matched where the ignore files are. No preferences means an
    // older or bare daemon, which honours `defaults.toml`'s answer.
    let honour = state
        .preferences()
        .is_none_or(|preferences| preferences.tree_respect_gitignore);
    // Built even when the caller asked for the hidden rows: a row an ignore rule
    // hides comes back *marked*, and that answer belongs to the matcher rather than
    // to a client that never saw the pattern.
    let rules = honour.then(|| ignore_rules(root, &resolved));
    let include_ignored = query.include_ignored.unwrap_or(false);
    // Asked once for the whole directory rather than once per row: git answers
    // about paths, and a row that asked its own question would be one `git status`
    // per file.
    let changed = changes(root);

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
        if !inside(root, &path) {
            continue;
        }
        let is_dir = metadata.is_dir();
        let ignored = rules
            .as_ref()
            .is_some_and(|rules| rules.matched_path_or_any_parents(&path, is_dir).is_ignore());
        if ignored && !include_ignored {
            continue;
        }
        // Git never lists its own directory and no `.gitignore` has to say so: the
        // objects, the refs and the index are the repository's bookkeeping, not the
        // project's files, and a tree that showed them would bury the project.
        // Showing the hidden rows does not change that: `.git` is not a file an
        // ignore rule hides, it is the repository itself.
        if honour && path.file_name().is_some_and(|name| name == ".git") {
            continue;
        }
        let relative = display_path(root, &path);
        let change = changed.get(&relative).copied();
        listing.push(FileEntry {
            path: relative,
            is_dir,
            size: if is_dir { 0 } else { metadata.len() },
            modified: modified(&metadata),
            ignored,
            status: change,
        });
    }
    listing.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Json(listing))
}

/// `GET /v1/files/search?workspace=&q=`.
///
/// The same ignore rules the tree follows, for the same reason twice over: a find
/// that returned what the tree hides would be a second answer to
/// `tree_respect_gitignore`, and the paths a person cannot see in the tree are not
/// the ones they are looking for. `.git` is never walked into, and hidden files
/// *are*: `.env` is a file a person searches for by name.
///
/// # Errors
///
/// No workspace to walk (`409`). A directory that cannot be read is skipped rather
/// than failing the search: the rest of the workspace is still findable.
async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    let root = workspace_root(&state, query.workspace.as_deref())?;
    let needle = query.q.as_deref().unwrap_or_default().trim().to_owned();
    if needle.is_empty() {
        // Nothing typed is nothing matched; a walk for "" would walk the repository.
        return Ok(Json(Vec::new()));
    }
    let honour = state
        .preferences()
        .is_none_or(|preferences| preferences.tree_respect_gitignore);
    // Ranked after the walk, not stopped by the cap: an initials match can be met
    // late in a directory walk, and a cap applied first would make the answer
    // depend on the order the filesystem hands directories back. The collection is
    // bounded instead, and the truncation happens once the scores are known.
    let mut found: Vec<(i32, FileEntry)> = Vec::new();
    for entry in walk(&root, honour) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if !inside(&root, path) {
            continue;
        }
        let relative = display_path(&root, path);
        let Some(score) = fuzzy_score(&relative, &needle) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        found.push((
            score,
            FileEntry {
                path: relative,
                is_dir: false,
                size: metadata.len(),
                modified: modified(&metadata),
                // A search never returns a hidden row: the walk did not enter one.
                ignored: false,
                // And no letters: a find is a list of paths rather than a tree of a
                // directory, so its cost is the walk rather than one `git status`
                // per keystroke.
                status: None,
            },
        ));
        if found.len() == MAX_SEARCH_CANDIDATES {
            break;
        }
    }
    found.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.path.cmp(&right.path))
    });
    found.truncate(MAX_SEARCH_RESULTS);
    Ok(Json(found.into_iter().map(|(_, entry)| entry).collect()))
}

/// `GET /v1/files/grep?workspace=&q=`.
///
/// Contents, not paths: every line of every text file the tree would show that
/// contains `q`, ignoring case, with the line's own text so a list can be read
/// before it is chosen from.
///
/// The walk is the path search's ([`walk`]), because a content find that read what
/// the tree hides would be a second answer to `tree_respect_gitignore`. A binary
/// file is skipped instead of decoded: the NUL test is the one a read uses, and
/// line numbers counted through replacement characters would point at text nobody
/// can see. Files over the read cap are skipped for the same reason a read refuses
/// them — a search is not a way around the route's limits.
///
/// # Errors
///
/// No workspace to walk (`409`). A file that cannot be read is skipped rather than
/// failing the search: the rest of the workspace is still greppable.
async fn grep(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<GrepMatch>>, ApiError> {
    let root = workspace_root(&state, query.workspace.as_deref())?;
    let needle = query.q.as_deref().unwrap_or_default().trim().to_lowercase();
    if needle.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let honour = state
        .preferences()
        .is_none_or(|preferences| preferences.tree_respect_gitignore);
    let mut found = Vec::new();
    'files: for entry in walk(&root, honour) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if !inside(&root, path) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let relative = display_path(&root, path);
        for (index, line) in text.lines().enumerate() {
            if !line.to_lowercase().contains(&needle) {
                continue;
            }
            found.push(GrepMatch {
                path: relative.clone(),
                line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                text: line.trim_end().to_owned(),
            });
            if found.len() == MAX_GREP_RESULTS {
                break 'files;
            }
        }
    }
    found.sort_by(|left, right| left.path.cmp(&right.path).then(left.line.cmp(&right.line)));
    Ok(Json(found))
}

/// `GET /v1/files/references?workspace=&q=`.
///
/// Where one identifier is used, in code. The walk is the content search's ([`walk`]),
/// so the ignore rules cannot become two answers to one preference, and the text rules
/// are its rules too — a file over the cap, or one that is not UTF-8, is skipped rather
/// than served around the route's limits. What a row is, though, is the grammar's
/// answer rather than a substring's: [`acpilot_text::references::uses`] lists the
/// leaves the parse calls identifiers, so a comment that mentions the name and a string
/// that spells it are not uses, and a file this build ships no grammar for is skipped
/// rather than searched as prose.
///
/// The parse happens here because the text is here: a client that filtered its own rows
/// would need the contents of every file the walk read, and the grammar crates are
/// already in the build the editor links.
///
/// # Errors
///
/// No workspace to walk (`409`). A file that cannot be read is skipped rather than
/// failing the search: the rest of the workspace is still searchable.
async fn references(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<Reference>>, ApiError> {
    let needle = query.q.as_deref().unwrap_or_default().trim().to_owned();
    let at = query.path.as_deref().zip(query.offset);
    let rows = find_references(&state, query.workspace.as_deref(), &needle, at, None).await?;
    Ok(Json(rows))
}

/// `POST /v1/files/references`.
///
/// The query-string question with the buffer attached, for the panel that asked it:
/// a file edited and not saved is the file the person is reading, and a list of uses
/// read from the last save would miss one typed a moment ago and keep one deleted a
/// moment ago. The walk is still the walk — every other file is read where it is,
/// because only the open buffer differs from its file.
///
/// The caret is optional here and the text is not: a name and a buffer is a question
/// worth answering, and it is answered by the parse tree, because a language server
/// needs a position to resolve.
///
/// # Errors
///
/// The parse-tree question's errors, plus a body that is not a references request
/// (`400`) and a path that is absolute or escapes (`400`).
async fn references_in_buffer(
    State(state): State<AppState>,
    body: Result<Json<ReferencesRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Vec<Reference>>, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a references request: {rejection}"
        )))
    })?;
    // Checked here rather than in the core, so a path that escapes is a `400` on every
    // request that names one — including the one whose text this route was handed.
    let _ = relative_path(&request.path)?;
    let buffer = request
        .text
        .as_deref()
        .map(|text| (request.path.as_str(), text));
    let at = request.offset.map(|offset| (request.path.as_str(), offset));
    let rows = find_references(
        &state,
        request.workspace.as_deref(),
        request.q.trim(),
        at,
        buffer,
    )
    .await?;
    Ok(Json(rows))
}

/// `POST /v1/files/diagnostics`.
///
/// What a language server says is wrong with one file. The text travels for the same
/// reason it does with references: an edited buffer is the file the person is reading,
/// and problems reported about the last save are problems about a version nobody has
/// open.
///
/// An answer with no `server` is not a clean file: it is a language this build has no
/// server for, a server that is not installed, or one that would not start. A pane
/// draws those differently because they are different claims — "nothing is wrong" and
/// "nobody checked" are not the same sentence.
///
/// # Errors
///
/// No workspace (`409`), a body that is not a diagnostics request (`400`), and a path
/// that is absolute or escapes (`400`). A file that cannot be read is answered with no
/// server rather than an error: the question was about the language, and the file is
/// the reason it could not be put.
async fn diagnostics(
    State(state): State<AppState>,
    body: Result<Json<DiagnosticsRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<FileDiagnostics>, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a diagnostics request: {rejection}"
        )))
    })?;
    let relative = relative_path(&request.path)?;
    let source = source(&state, request.workspace.as_deref())?;
    let (root, place) = place_of(&state, &source);
    let Some(language) = crate::lsp::language_of(&request.path) else {
        return Ok(Json(nothing_asked()));
    };
    let (resolved, text) = match request.text {
        // A buffer that is not on disk yet is still a document a server can judge:
        // the path has already been refused if it escapes, so joining it is the whole
        // answer, and a file being written for the first time is precisely the one
        // whose problems are worth hearing about.
        Some(text) => (
            match &source {
                Source::Local(_) => resolve_existing(&root, &relative, &request.path)
                    .unwrap_or_else(|_| root.join(&relative)),
                Source::Remote { .. } => root.join(&relative),
            },
            text,
        ),
        None => match &source {
            Source::Local(_) => {
                let resolved = resolve_existing(&root, &relative, &request.path)?;
                let metadata =
                    std::fs::metadata(&resolved).map_err(|source| io_error(&resolved, source))?;
                if metadata.is_dir() || metadata.len() > MAX_FILE_BYTES {
                    return Ok(Json(nothing_asked()));
                }
                let bytes =
                    std::fs::read(&resolved).map_err(|source| io_error(&resolved, source))?;
                let Ok(text) = String::from_utf8(bytes) else {
                    return Ok(Json(nothing_asked()));
                };
                (resolved, text)
            }
            // The file is on the host, and so is the server: this daemon only needs the
            // text to know which buffer the question is about, and it is the host's copy.
            Source::Remote { host, root: at } => {
                let Some(text) = remote_text(&state, host, at, &relative).await? else {
                    return Ok(Json(nothing_asked()));
                };
                (root.join(&relative), text)
            }
        },
    };
    let connections = std::sync::Arc::clone(state.hosts().connections());
    let Some(answer) = state
        .language_servers()
        .diagnostics(language, &place, &connections, &resolved, &text)
        .await
    else {
        return Ok(Json(nothing_asked()));
    };
    tracing::debug!(
        server = answer.program,
        "diagnostics from a language server"
    );
    let problems = FileDiagnostics {
        server: Some(answer.program.to_owned()),
        encoding: Some(match answer.encoding {
            acpilot_lsp::PositionEncoding::Utf8 => acpilot_proto::lsp::PositionEncoding::Utf8,
            acpilot_lsp::PositionEncoding::Utf16 => acpilot_proto::lsp::PositionEncoding::Utf16,
        }),
        items: answer.items.iter().map(problem_row).collect(),
    };
    // Remembered so a client that connects after this answer is still told about the
    // file: the stream's snapshot is state, and this is the newest fact in it. No
    // version travels, because the question named text rather than a version and the
    // server never said which number it judged.
    state.record_diagnostics(acpilot_proto::file::DiagnosticsNotice {
        path: request.path.clone(),
        version: None,
        problems: problems.clone(),
    });
    Ok(Json(problems))
}

/// The answer when no server was asked, which is not the answer to a clean file.
fn nothing_asked() -> FileDiagnostics {
    FileDiagnostics {
        server: None,
        encoding: None,
        items: Vec::new(),
    }
}

/// One problem, in the wire's own counting: lines and columns from 1.
///
/// Shared with the daemon's push path: a published problem and an answered one are the
/// same problem, and two conversions would be two chances to count differently.
pub(crate) fn problem_row(problem: &acpilot_lsp::session::Problem) -> Diagnostic {
    Diagnostic {
        line: problem.line.saturating_add(1),
        column: problem.character.saturating_add(1),
        end_line: problem.end_line.saturating_add(1),
        end_column: problem.end_character.saturating_add(1),
        severity: match problem.severity {
            acpilot_lsp::session::Severity::Error => DiagnosticSeverity::Error,
            acpilot_lsp::session::Severity::Warning => DiagnosticSeverity::Warning,
            acpilot_lsp::session::Severity::Information => DiagnosticSeverity::Information,
            acpilot_lsp::session::Severity::Hint => DiagnosticSeverity::Hint,
        },
        message: problem.message.clone(),
        source: problem.source.clone(),
        code: problem.code.clone(),
    }
}

/// The answer to both forms of the question: where is this name used.
///
/// `at` is the caret, when the caller knows one, and `buffer` is the open file's text
/// as the editor holds it. Neither is required: a request without a caret is a
/// spelling search, and one without a buffer is about the files on disk.
async fn find_references(
    state: &AppState,
    workspace: Option<&str>,
    needle: &str,
    at: Option<(&str, usize)>,
    buffer: Option<(&str, &str)>,
) -> Result<Vec<Reference>, ApiError> {
    let source = source(state, workspace)?;
    let (root, _) = place_of(state, &source);
    // An identifier is one word. Anything else cannot be a use of a name, and a walk
    // that read every file to prove it would be a search for nothing.
    if !is_identifier(needle) {
        return Ok(Vec::new());
    }
    // A request that names a caret can be answered by a language server, which knows
    // scope: two functions called `new` are two functions, and a field is not every
    // `id` in the repository. `None` means nothing answered, and the parse tree below
    // is the answer for a workspace whose language has no server installed.
    if let Some((path, offset)) = at
        && let Some(rows) = ask_server(state, &source, path, offset, buffer).await?
    {
        return Ok(rows);
    }
    // The parse-tree fallback reads this daemon's own disk. A remote workspace's files
    // are not here, and an empty list would say "no uses" about a workspace nobody
    // searched.
    if matches!(source, Source::Remote { .. }) {
        return Err(ApiError::new(AcpilotError::conflict(
            "no language server on this workspace's host answered; the parse-tree search reads this daemon's own disk",
        )));
    }
    let honour = state
        .preferences()
        .is_none_or(|preferences| preferences.tree_respect_gitignore);
    let mut found: Vec<Reference> = Vec::new();
    'files: for entry in walk(&root, honour) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if !inside(&root, path) {
            continue;
        }
        let relative = display_path(&root, path);
        // A file no grammar parses is prose or data: searching it for a name would
        // list the documentation as if it were code.
        let language = shipped_language(&relative);
        if language.is_empty() {
            continue;
        }
        // The buffer the caller sent *is* this file, when it names it: a panel answers
        // about what the pane shows, not about the last save, and the difference is
        // exactly the edit somebody just made.
        let text = match buffer.filter(|(buffered, _)| *buffered == relative.as_str()) {
            Some((_, text)) => Cow::Borrowed(text),
            None => {
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if metadata.len() > MAX_FILE_BYTES {
                    continue;
                }
                let Ok(bytes) = std::fs::read(path) else {
                    continue;
                };
                if bytes.contains(&0) {
                    continue;
                }
                let Ok(text) = String::from_utf8(bytes) else {
                    continue;
                };
                Cow::Owned(text)
            }
        };
        // Split rather than `lines()`: the parser counts rows the way the rope does,
        // and the row a use is on has to be the row whose text the row draws.
        let lines: Vec<&str> = text.split('\n').collect();
        for (line, column) in parsed_uses(language, &text, needle) {
            found.push(Reference {
                path: relative.clone(),
                line: u32::try_from(line + 1).unwrap_or(u32::MAX),
                column: u32::try_from(column + 1).unwrap_or(u32::MAX),
                text: lines
                    .get(line)
                    .map_or_else(String::new, |line| line.trim_end().to_owned()),
            });
            if found.len() == MAX_REFERENCES {
                break 'files;
            }
        }
    }
    found.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.column.cmp(&right.column))
    });
    Ok(found)
}

/// Whether `name` is one identifier.
///
/// The daemon's own answer rather than the grammar crate's, because this check has
/// to run in a build that does not link the grammars: a references request is a
/// question about one name, and a query that is a sentence or empty is not one.
fn is_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(first) if first.is_alphabetic() || first == '_')
        && characters.all(|character| character.is_alphanumeric() || character == '_')
}

/// The grammar this build ships for `path`, or empty when the fallback is compiled
/// out.
///
/// A daemon built with `--no-default-features` has no parser at all: references then
/// come from a language server or not at all, and the dep is what keeps 2.2 MB of
/// grammar tables out of the binary.
#[cfg(feature = "parse-references")]
fn shipped_language(path: &str) -> &'static str {
    acpilot_text::language_for(path)
}

#[cfg(not(feature = "parse-references"))]
fn shipped_language(_path: &str) -> &'static str {
    ""
}

/// The parse tree's uses of `needle`, as `(line, character column)`.
#[cfg(feature = "parse-references")]
fn parsed_uses(language: &str, text: &str, needle: &str) -> Vec<(usize, usize)> {
    acpilot_text::references::uses(language, text, needle)
        .into_iter()
        .map(|hit| (hit.line, hit.column))
        .collect()
}

#[cfg(not(feature = "parse-references"))]
fn parsed_uses(_language: &str, _text: &str, _needle: &str) -> Vec<(usize, usize)> {
    Vec::new()
}

/// Ask a language server where the name at `offset` in `path` is used.
///
/// `None` when there is nothing to ask: a language this build has no server for, no
/// server installed, or one that did not answer. The rows are read from the files the
/// server named, because a list of `path:line` cannot be chosen from, and a server can
/// name files outside the workspace — a dependency in a package cache — which are
/// dropped: a row the tree cannot open is not a row.
async fn ask_server(
    state: &AppState,
    source: &Source,
    path: &str,
    offset: usize,
    buffer: Option<(&str, &str)>,
) -> Result<Option<Vec<Reference>>, ApiError> {
    let Some(language) = crate::lsp::language_of(path) else {
        return Ok(None);
    };
    let relative = relative_path(path)?;
    let (root, place) = place_of(state, source);
    // The buffer is the document the server opens: asking it about the saved text
    // while the pane shows an edited one would resolve the caret against a version
    // nobody is looking at, and the caret's offset is the edited one's.
    let (resolved, text) = match buffer.filter(|(buffered, _)| *buffered == path) {
        Some((_, text)) => (
            // A file that is not on disk yet — a buffer opened before its first save —
            // is still a document the server can resolve a symbol in. The path has
            // already been refused if it escapes, so joining it is the whole answer.
            match source {
                Source::Local(_) => resolve_existing(&root, &relative, path)
                    .unwrap_or_else(|_| root.join(&relative)),
                Source::Remote { .. } => root.join(&relative),
            },
            text.to_owned(),
        ),
        None => match source {
            Source::Local(_) => {
                let resolved = resolve_existing(&root, &relative, path)?;
                let metadata =
                    std::fs::metadata(&resolved).map_err(|source| io_error(&resolved, source))?;
                if metadata.is_dir() || metadata.len() > MAX_FILE_BYTES {
                    return Ok(None);
                }
                let bytes =
                    std::fs::read(&resolved).map_err(|source| io_error(&resolved, source))?;
                if bytes.contains(&0) {
                    return Ok(None);
                }
                let Ok(text) = String::from_utf8(bytes) else {
                    return Ok(None);
                };
                (resolved, text)
            }
            Source::Remote { host, root: at } => {
                let Some(text) = remote_text(state, host, at, &relative).await? else {
                    return Ok(None);
                };
                (root.join(&relative), text)
            }
        },
    };
    let connections = std::sync::Arc::clone(state.hosts().connections());
    let Some(answer) = state
        .language_servers()
        .references(language, &place, &connections, &resolved, &text, offset)
        .await
    else {
        return Ok(None);
    };
    tracing::debug!(server = answer.program, "references from a language server");
    match source {
        Source::Local(root) => Ok(Some(rows_from(root, &answer))),
        Source::Remote { host, root, .. } => {
            Ok(Some(rows_from_remote(state, host, root, &answer).await?))
        }
    }
}

/// The root path and the machine a session for `source` runs on.
fn place_of(state: &AppState, source: &Source) -> (PathBuf, crate::lsp::Place) {
    match source {
        Source::Local(root) => (
            root.clone(),
            crate::lsp::Place {
                root: root.clone(),
                at: crate::lsp::Where::Local,
            },
        ),
        Source::Remote { host, root } => (
            PathBuf::from(root),
            crate::lsp::Place {
                root: PathBuf::from(root),
                at: crate::lsp::Where::Remote {
                    transport: transport_for(state, host),
                    host: host.id.clone(),
                },
            },
        ),
    }
}

/// The transport a host's helper is reached with.
fn transport_for(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
) -> crate::helper::Transport {
    let address = host
        .id
        .split_once(':')
        .map(|(_, address)| address.to_owned())
        .unwrap_or_else(|| host.id.clone());
    crate::helper::Transport::new(host.kind, address, state.hosts().ssh_program())
}

/// One file's text, read through the host's helper.
///
/// `None` is the host refusing — a missing file, a directory, bytes that are not text —
/// which is the same "nothing to ask about" a local read answers with.
async fn remote_text(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    root: &str,
    relative: &Path,
) -> Result<Option<String>, ApiError> {
    let path = relative.to_string_lossy().replace('\\', "/");
    let answer = ask(
        state,
        host,
        "fs.read",
        serde_json::json!({ "path": remote_path(root, &path) }),
    )
    .await?;
    let Ok(read) = answer else {
        return Ok(None);
    };
    Ok(read
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned))
}

/// [`rows_from`], for locations on a host: the text of each row is read there.
///
/// Sequential on purpose: a references answer is at most [`MAX_REFERENCES`] rows, and a
/// burst of transport calls to draw one list would cost more than it saves.
async fn rows_from_remote(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    root: &str,
    answer: &crate::lsp::Answer,
) -> Result<Vec<Reference>, ApiError> {
    let at = Path::new(root);
    let mut texts: HashMap<std::path::PathBuf, String> = HashMap::new();
    let mut found: Vec<Reference> = Vec::new();
    for location in &answer.locations {
        let Ok(relative) = location.path.strip_prefix(at) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.is_empty() {
            continue;
        }
        if !texts.contains_key(&location.path) {
            let relative = Path::new(&relative);
            let Some(text) = remote_text(state, host, root, relative).await? else {
                continue;
            };
            texts.insert(location.path.clone(), text);
        }
        let text = &texts[&location.path];
        let index = LineIndex::new(text);
        let Some(bytes) = index.offset(
            text,
            Position::new(location.line, location.character),
            answer.encoding,
        ) else {
            continue;
        };
        let line_start = text[..bytes].rfind('\n').map_or(0, |at| at + 1);
        let column = u32::try_from(text[line_start..bytes].chars().count() + 1).unwrap_or(u32::MAX);
        let line = text
            .split('\n')
            .nth(location.line as usize)
            .unwrap_or_default()
            .trim_end();
        found.push(Reference {
            path: relative,
            line: location.line + 1,
            column,
            text: line.to_owned(),
        });
        if found.len() == MAX_REFERENCES {
            break;
        }
    }
    found.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.column.cmp(&right.column))
    });
    found.dedup_by(|left, right| {
        left.path == right.path && left.line == right.line && left.column == right.column
    });
    Ok(found)
}

/// The server's locations as rows a pane can draw.
///
/// The encoding matters twice: the server's `character` is counted in it, and the
/// row's `column` is a character column because a pane places a caret on a glyph.
fn rows_from(root: &Path, answer: &crate::lsp::Answer) -> Vec<Reference> {
    let mut texts: HashMap<std::path::PathBuf, String> = HashMap::new();
    let mut found: Vec<Reference> = Vec::new();
    for location in &answer.locations {
        let Ok(relative) = location.path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.is_empty() {
            continue;
        }
        let text = texts
            .entry(location.path.clone())
            .or_insert_with(|| std::fs::read_to_string(&location.path).unwrap_or_default());
        let index = LineIndex::new(text);
        let Some(bytes) = index.offset(
            text,
            Position::new(location.line, location.character),
            answer.encoding,
        ) else {
            continue;
        };
        let line_start = text[..bytes].rfind('\n').map_or(0, |at| at + 1);
        let column = u32::try_from(text[line_start..bytes].chars().count() + 1).unwrap_or(u32::MAX);
        let line = text
            .split('\n')
            .nth(location.line as usize)
            .unwrap_or_default()
            .trim_end();
        found.push(Reference {
            path: relative,
            line: location.line + 1,
            column,
            text: line.to_owned(),
        });
        if found.len() == MAX_REFERENCES {
            break;
        }
    }
    found.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.column.cmp(&right.column))
    });
    found.dedup_by(|left, right| {
        left.path == right.path && left.line == right.line && left.column == right.column
    });
    found
}

/// Every changed path under `root`, as the letter a tree row draws.
///
/// Asked once per listing, on the host the files are on, so a client cannot be
/// wrong about a workspace it cannot see. Two things are deliberately not errors:
/// a workspace that is not a repository, and a host with no `git` on it. Both are a
/// tree without letters, and the file routes' job is to list what is there — the
/// commit writer is where a missing `git` is worth reporting, because it is where
/// something was supposed to happen.
///
/// A directory takes the most urgent change beneath it, so a collapsed row is not a
/// row that hides what is inside it. A path git reports that is no longer on disk
/// cannot appear: the listing is the filesystem, and a deleted file has no row to
/// mark (the source-control pane, P6.5, is where it does).
fn changes(root: &Path) -> BTreeMap<String, FileChange> {
    let Ok(repo) = Repo::discover(root) else {
        return BTreeMap::new();
    };
    let Ok(status) = repo.status() else {
        return BTreeMap::new();
    };
    // The repository can sit above the workspace — one package of a monorepo — and
    // then git speaks paths the workspace cannot name. Canonicalised on both sides
    // because the two spellings came from different tools.
    let prefix = root
        .canonicalize()
        .ok()
        .zip(repo.root().canonicalize().ok())
        .and_then(|(root, top)| {
            root.strip_prefix(top)
                .ok()
                .map(|prefix| prefix.to_string_lossy().replace('\\', "/"))
        })
        .filter(|prefix| !prefix.is_empty());

    let mut changes: BTreeMap<String, FileChange> = BTreeMap::new();
    for entry in &status.files {
        let Some(relative) = workspace_relative(&entry.path, prefix.as_deref()) else {
            continue;
        };
        let change = change_of(entry);
        let mut path = Some(relative.as_str());
        while let Some(current) = path {
            let slot = changes.entry(current.to_owned()).or_insert(change);
            if urgency(change) > urgency(*slot) {
                *slot = change;
            }
            path = current.rfind('/').map(|at| &current[..at]);
        }
    }
    changes
}

/// A repository-relative path as this workspace spells it, or `None` when the
/// change is outside the workspace (the repository is bigger than the workspace it
/// was opened from).
fn workspace_relative(path: &str, prefix: Option<&str>) -> Option<String> {
    let Some(prefix) = prefix else {
        return Some(path.to_owned());
    };
    let rest = path.strip_prefix(prefix)?.strip_prefix('/')?;
    (!rest.is_empty()).then(|| rest.to_owned())
}

/// One path's change, from the two halves git reports.
///
/// A conflict outranks everything: git will not commit through one, and a row that
/// said `M` would hide the only state that has to be resolved by hand. After that a
/// file git has never seen is untracked whatever its contents say, an added path is
/// added whether or not it has been edited since, and anything else that changed
/// changed.
fn change_of(entry: &StatusEntry) -> FileChange {
    let sides = [entry.index, entry.worktree];
    if sides.contains(&ChangeKind::Unmerged) {
        FileChange::Conflicted
    } else if sides.contains(&ChangeKind::Untracked) {
        // Either side, because porcelain reports an untracked path as `?.` — the
        // working tree's answer with nothing in the index — and a check on the index
        // alone read every such file as modified.
        FileChange::Untracked
    } else if sides.contains(&ChangeKind::Added) {
        FileChange::Added
    } else if sides.contains(&ChangeKind::Deleted) {
        FileChange::Deleted
    } else {
        FileChange::Modified
    }
}

/// How much a change asks to be seen first, for a directory's one letter.
///
/// An edit to a tracked file outranks an untracked one because a change to a tracked
/// file is what a commit is about; the untracked file is not part of the project
/// yet.
fn urgency(change: FileChange) -> u8 {
    match change {
        FileChange::Conflicted => 5,
        FileChange::Deleted => 4,
        FileChange::Added => 3,
        FileChange::Modified => 2,
        FileChange::Untracked => 1,
    }
}

/// A walk of the workspace that follows what the tree follows.
///
/// One walk for both searches: `.gitignore` applies only when
/// `tree_respect_gitignore` is on, `.ignore` and the two per-machine excludes
/// never, `.git` is never entered, and hidden files are walked because `.env` is a
/// file people search for by name. Two copies of these rules would be two answers
/// to one setting. `require_git(false)` is what makes a `.gitignore` mean the same
/// thing in a workspace that is not a checkout yet, which is every project before
/// its first commit.
fn walk(root: &Path, honour: bool) -> ignore::Walk {
    let mut builder = WalkBuilder::new(root);
    builder
        .git_ignore(honour)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .filter_entry(|entry| {
            !(entry.file_type().is_some_and(|kind| kind.is_dir())
                && entry.file_name() == std::ffi::OsStr::new(".git"))
        });
    builder.build()
}

/// How well a path answers a query, or `None` when it does not.
///
/// Initials are the point: `aptui` finds `crates/acpilot-tui/src/app.rs`, which a
/// substring match cannot. The characters have to appear in order, and a character
/// at the start of a segment (`/`, `_`, `-`, `.`, the beginning) scores above one
/// inside a word, so the first row is usually the file somebody meant. A run of
/// consecutive characters scores highest of all, which keeps the old behaviour —
/// typing a plain substring — ranking above an initials match of the same length.
///
/// The last term is a length penalty, because `src/app.rs` and
/// `examples/old/src/app.rs` match the same letters and the shorter one is the
/// answer more often than not.
#[must_use]
fn fuzzy_score(path: &str, query: &str) -> Option<i32> {
    let haystack = path.to_lowercase();
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let mut score = 0;
    let mut chars = haystack.char_indices().peekable();
    let mut previous_end = None;
    for wanted in needle.chars() {
        let mut matched = None;
        for (index, candidate) in chars.by_ref() {
            if candidate == wanted {
                matched = Some(index);
                break;
            }
        }
        let index = matched?;
        score += 1;
        if previous_end.is_some_and(|end| end == index) {
            score += 5;
        }
        let before = haystack[..index].chars().next_back();
        if index == 0 || before.is_some_and(|character| "/_- .".contains(character)) {
            score += 8;
        }
        previous_end = Some(index + wanted.len_utf8());
    }
    Some(score - i32::try_from(haystack.len() / 12).unwrap_or(i32::MAX))
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
    match source(&state, request.workspace.as_deref())? {
        Source::Local(root) => write_local(root, request),
        Source::Remote { host, root } => write_remote(&state, &host, &root, request).await,
    }
}

/// [`write`], through this daemon's own filesystem.
fn write_local(root: PathBuf, request: FileWriteRequest) -> Result<Json<FileContent>, ApiError> {
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
    let path = display_path(&root, &target);
    // Asked again after the write: what is on disk is now this text, so the marks are
    // this text's, not the version the buffer was edited from.
    let marks = line_marks(&root, &path);
    Ok(Json(FileContent {
        path,
        text,
        etag,
        marks,
    }))
}

/// The query string the delete route takes.
#[derive(Debug, Deserialize)]
struct DeleteQuery {
    /// Workspace id, when the client names one.
    workspace: Option<String>,
    /// Workspace-relative path of the file to remove.
    path: Option<String>,
}

/// `DELETE /v1/files?workspace=&path=`.
///
/// Files only. A directory needs a decision about what happens to its contents,
/// and "deleted the tree because the path was a folder" is how an editor loses a
/// day's work; `409` says so rather than choosing.
///
/// # Errors
///
/// No workspace (`409`), a path that escapes (`400`/`403`), no such file (`404`),
/// or a directory (`409`).
async fn delete(
    State(state): State<AppState>,
    Query(query): Query<DeleteQuery>,
) -> Result<StatusCode, ApiError> {
    let requested = query
        .path
        .as_deref()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            ApiError::new(AcpilotError::invalid("`path` is required"))
                .with_detail("path", "a delete names the file to remove")
        })?
        .to_owned();
    match source(&state, query.workspace.as_deref())? {
        Source::Local(root) => delete_local(&root, &requested),
        Source::Remote { host, root } => delete_remote(&state, &host, &root, &requested).await,
    }
}

/// [`delete`], through this daemon's own filesystem.
fn delete_local(root: &Path, requested: &str) -> Result<StatusCode, ApiError> {
    let relative = relative_path(requested)?;
    let resolved = resolve_existing(root, &relative, requested)?;
    let metadata = std::fs::metadata(&resolved).map_err(|source| io_error(&resolved, source))?;
    if metadata.is_dir() {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "{requested} is a directory; this route removes files"
        ))));
    }
    std::fs::remove_file(&resolved).map_err(|source| io_error(&resolved, source))?;
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `POST /v1/files/directory`.
#[derive(Debug, Deserialize)]
struct DirectoryRequest {
    /// Workspace id, when the client names one.
    #[serde(default)]
    workspace: Option<String>,
    /// Workspace-relative path of the directory to create.
    path: String,
}

/// `POST /v1/files/directory` — make a directory, and any parents it needs.
///
/// Parents are created because a "new folder" dialog names the folder somebody
/// wants, not every level between it and the root. The deepest part of the path
/// that already exists is the part that is resolved and checked: a symlink in the
/// middle is how `create_dir_all` lands outside the workspace.
///
/// # Errors
///
/// No workspace (`409`), a path that escapes (`400`/`403`), or something already
/// at that path (`409`).
async fn create_directory(
    State(state): State<AppState>,
    body: Result<Json<DirectoryRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a directory request: {rejection}"
        )))
    })?;
    match source(&state, request.workspace.as_deref())? {
        Source::Local(root) => directory_local(&root, &request.path),
        Source::Remote { host, root } => {
            directory_remote(&state, &host, &root, &request.path).await
        }
    }
}

/// [`create_directory`], through this daemon's own filesystem.
fn directory_local(root: &Path, requested: &str) -> Result<StatusCode, ApiError> {
    let relative = relative_path(requested)?;
    if relative.as_os_str().is_empty() {
        return Err(
            ApiError::new(AcpilotError::invalid("a directory needs a name"))
                .with_detail("path", "send a path below the workspace root"),
        );
    }
    let absolute = root.join(&relative);
    if absolute.exists() {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "{requested} already exists"
        ))));
    }
    let mut existing = absolute.as_path();
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| outside(requested, root))?;
    }
    let resolved = std::fs::canonicalize(existing).map_err(|source| io_error(existing, source))?;
    if !resolved.starts_with(root) {
        return Err(outside(requested, root));
    }
    std::fs::create_dir_all(&absolute).map_err(|source| io_error(&absolute, source))?;
    Ok(StatusCode::CREATED)
}

/// The ignore rules that apply to one directory.
///
/// The root's `.gitignore` and every `.gitignore` between it and the directory being
/// listed, because rules are usually nested: the root says `target/` and a vendored
/// directory three levels down has its own.
///
/// `.git/info/exclude` and the global excludes file are deliberately not read: they
/// are per-machine settings, and a tree that differed between two people reading the
/// same repository would be a tree nobody could describe.
fn ignore_rules(root: &Path, listed: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let mut directory = Some(listed);
    while let Some(current) = directory {
        let file = current.join(".gitignore");
        if file.is_file() {
            // A pattern that does not parse is skipped by the builder rather than
            // failing the listing: a tree that refused to draw over one bad line
            // would hide the working tree behind a typo.
            let _ = builder.add(&file);
        }
        if current == root {
            break;
        }
        directory = current.parent().filter(|parent| parent.starts_with(root));
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// The query string both read routes take.
#[derive(Debug, Deserialize)]
struct FileQuery {
    /// Workspace id, when the client names one. The daemon serves exactly one.
    workspace: Option<String>,
    /// Workspace-relative path. Empty or absent means the root, for a listing.
    path: Option<String>,
    /// Whether to return the rows an ignore rule hides, marked with `ignored`.
    ///
    /// Absent is off: a tree asks for the working tree, and one toggle in the TUI is
    /// the only thing that asks for the rest.
    include_ignored: Option<bool>,
}

/// The search route's query string.
#[derive(Debug, Deserialize)]
struct SearchQuery {
    /// Workspace id, when the client names one. The daemon serves exactly one.
    workspace: Option<String>,
    /// What a path has to contain, ignoring case. Empty matches nothing.
    q: Option<String>,
    /// The file the caret is in, for a references request that can be answered by a
    /// language server rather than by spelling.
    path: Option<String>,
    /// The caret's offset in that file, counted in characters, which is what a server
    /// needs to resolve a name to a symbol instead of to a string.
    offset: Option<usize>,
}

/// Where the files a request names are.
pub(crate) enum Source {
    /// A directory this daemon can open itself.
    Local(PathBuf),
    /// A root on a host this daemon reaches through its helper.
    Remote {
        /// The host whose helper is asked.
        host: acpilot_proto::host::HostSummary,
        /// The root, spelled the way that host spells it.
        root: String,
    },
}

/// The workspace `workspace` names, in the form a route can read from.
///
/// A workspace this daemon opened is found by id, wherever it lives: the row records the
/// host, and a row for another host is read through that host's helper rather than
/// through this machine's filesystem. Nothing named is the workspace the daemon was
/// started on, which is the only answer an older client can ask for.
///
/// # Errors
///
/// A workspace id that was never opened (`404`, naming the one this daemon serves), or
/// no workspace at all (`409`).
pub(crate) fn source(state: &AppState, workspace: Option<&str>) -> Result<Source, ApiError> {
    let named = workspace.filter(|id| !id.is_empty());
    let opened = named.and_then(|id| {
        state
            .workspaces()
            .list()
            .into_iter()
            .find(|row| row.id == id)
    });
    let Some(row) = opened else {
        return workspace_root(state, named).map(Source::Local);
    };
    if row.host_id == crate::hosts::LOCAL_HOST {
        return Ok(Source::Local(PathBuf::from(&row.root)));
    }
    let host = state
        .hosts()
        .get(&row.host_id)
        .ok_or_else(|| ApiError::new(AcpilotError::not_found(format!("host {}", row.host_id))))?;
    Ok(Source::Remote {
        host,
        root: row.root,
    })
}

/// [`write`], through a host this daemon reaches with its helper.
///
/// The token a client sends back is the one [`read_remote`] produced — this daemon's hash
/// of the text — so `if_match` is checked by reading the file again over the same transport
/// that is about to write it. A failed read is also how a create is told from an overwrite:
/// the helper has no `stat`, and "is it there" is the same question as "read it".
async fn write_remote(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    root: &str,
    request: FileWriteRequest,
) -> Result<Json<FileContent>, ApiError> {
    // Hashed before the text is moved into the request: the token a client stores is this
    // daemon's own, and it is the hash of the bytes this call is about to send.
    let etag = etag_of(request.text.as_bytes());
    let bytes = request.text.into_bytes();
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(too_large(&request.path, bytes.len() as u64));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ApiError::new(AcpilotError::invalid("the text is not UTF-8")))?;
    let relative = relative_path(&request.path)?;
    let path = relative.to_string_lossy().replace('\\', "/");
    let target = remote_path(root, &path);
    let current = ask(
        state,
        host,
        "fs.read",
        serde_json::json!({ "path": target }),
    )
    .await?;

    match (&request.if_match, current) {
        // Nothing there is a create; something there is the silent replace this route
        // refuses without a token, exactly as the local write does.
        (None, Ok(_)) => {
            return Err(ApiError::new(AcpilotError::conflict(format!(
                "{} already exists; read it and send its `etag` as `if_match`",
                request.path
            ))));
        }
        (None, Err(_)) => {}
        (Some(expected), Ok(read)) => {
            let read = read
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if &etag_of(read.as_bytes()) != expected {
                return Err(ApiError::new(AcpilotError::conflict(format!(
                    "{} changed since that `etag`; read it again",
                    request.path
                ))));
            }
        }
        (Some(_), Err(detail)) => {
            return Err(ApiError::new(AcpilotError::not_found(format!(
                "file {} on {}",
                request.path, host.id
            )))
            .with_detail("transport", detail));
        }
    }

    let written = ask(
        state,
        host,
        "fs.write",
        serde_json::json!({ "path": target, "text": text.as_str() }),
    )
    .await?;
    written.map_err(|detail| {
        ApiError::new(AcpilotError::not_found(format!(
            "path {} on {}",
            request.path, host.id
        )))
        .with_detail("transport", detail)
    })?;
    Ok(Json(FileContent {
        path,
        text,
        etag,
        marks: Vec::new(),
    }))
}

/// [`delete`], through a host this daemon reaches with its helper.
///
async fn delete_remote(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    root: &str,
    requested: &str,
) -> Result<StatusCode, ApiError> {
    let relative = relative_path(requested)?;
    let path = relative.to_string_lossy().replace('\\', "/");
    let answer = ask(
        state,
        host,
        "fs.remove",
        serde_json::json!({ "path": remote_path(root, &path) }),
    )
    .await?;
    let removed = answer.map_err(|detail| {
        ApiError::new(AcpilotError::not_found(format!(
            "path {requested} on {}",
            host.id
        )))
        .with_detail("transport", detail)
    })?;
    // The host answered that it reached the path and did not remove it, which is a
    // directory: refusing silently here would delete a tree the caller did not name.
    if removed.get("removed").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "{requested} is a directory; this route removes files"
        ))));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// [`create_directory`], through a host this daemon reaches with its helper.
///
async fn directory_remote(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    root: &str,
    requested: &str,
) -> Result<StatusCode, ApiError> {
    let relative = relative_path(requested)?;
    if relative.as_os_str().is_empty() {
        return Err(
            ApiError::new(AcpilotError::invalid("a directory needs a name"))
                .with_detail("path", "send a path below the workspace root"),
        );
    }
    let path = relative.to_string_lossy().replace('\\', "/");
    let answer = ask(
        state,
        host,
        "fs.mkdir",
        serde_json::json!({ "path": remote_path(root, &path) }),
    )
    .await?;
    let made = answer.map_err(|detail| {
        ApiError::new(AcpilotError::not_found(format!(
            "path {requested} on {}",
            host.id
        )))
        .with_detail("transport", detail)
    })?;
    if made.get("created").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "{requested} already exists"
        ))));
    }
    Ok(StatusCode::CREATED)
}

/// Ask `host`'s helper `method`, and tell its refusal from a transport that will not answer.
///
/// The outer error is how this daemon failed to reach the host: a helper that would not
/// start, a handshake that ran out of time, a host nobody has tested yet. The inner error
/// is the host answering — its own words for why it would not do as it was asked, which is
/// the only thing that can say a path is missing from a machine this daemon cannot see.
async fn ask(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    method: &str,
    params: serde_json::Value,
) -> Result<Result<serde_json::Value, String>, ApiError> {
    let capabilities = host.capabilities.ok_or_else(|| {
        ApiError::new(AcpilotError::conflict(format!(
            "host {} has not been tested yet; POST /v1/hosts/{}/test first",
            host.id, host.id
        )))
    })?;
    if !capabilities.filesystem {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "the helper on {} does not read files",
            host.id
        ))));
    }
    let address = host
        .id
        .split_once(':')
        .map(|(_, address)| address.to_owned())
        .unwrap_or_else(|| host.id.clone());
    let transport = crate::helper::Transport::new(host.kind, address, state.hosts().ssh_program());
    let unavailable = |detail: String| {
        ApiError::new(AcpilotError::HostUnavailable {
            host: host.id.clone(),
        })
        .with_detail("transport", detail)
    };
    let connections = std::sync::Arc::clone(state.hosts().connections());
    let key = host.id.clone();
    let method = method.to_owned();
    let asked =
        tokio::task::spawn_blocking(move || connections.request(&key, &transport, &method, params));
    let answered = tokio::time::timeout(crate::hosts::HANDSHAKE_TIMEOUT, asked)
        .await
        .map_err(|_| {
            unavailable(format!(
                "{} did not answer within {} seconds",
                host.id,
                crate::hosts::HANDSHAKE_TIMEOUT.as_secs()
            ))
        })?;
    match answered {
        Err(join) => Err(unavailable(join.to_string())),
        Ok(answered) => Ok(answered),
    }
}

/// [`read`], from a root on the far side of a transport.
///
/// No ETag the host computed, because the helper reads a file rather than opening one:
/// the token is this daemon's own hash of the text it was handed, which is enough to tell
/// a client that what it is looking at is what it read.
async fn read_remote(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    root: &str,
    query: &FileQuery,
) -> Result<Json<FileContent>, ApiError> {
    let requested = query
        .path
        .as_deref()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            ApiError::new(AcpilotError::invalid("`path` is required"))
                .with_detail("path", "a read names the file to read")
        })?;
    let relative = relative_path(requested)?;
    let path = relative.to_string_lossy().replace('\\', "/");
    let answer = ask(
        state,
        host,
        "fs.read",
        serde_json::json!({ "path": remote_path(root, &path) }),
    )
    .await?;
    let read = answer.map_err(|message| read_refusal(host, requested, &message))?;
    let text = read
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            ApiError::new(AcpilotError::HostUnavailable {
                host: host.id.clone(),
            })
            .with_detail(
                "transport",
                format!("the helper answered fs.read with {read}, which is not a file"),
            )
        })?;
    // Checked after the transport rather than before it: the helper has no cheap way to
    // say how big a file is, and a cap that refused nothing would be a comment.
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(too_large(requested, text.len() as u64));
    }
    Ok(Json(FileContent {
        path,
        text: text.to_owned(),
        etag: etag_of(text.as_bytes()),
        // No gutter marks: `git` runs on the host the files are on, and this daemon has
        // not asked the far one yet.
        marks: Vec::new(),
    }))
}

/// [`list`], from a root on the far side of a transport.
///
/// The far host lists one directory, and the two questions a tree row asks about it are
/// asked *there*: `git check-ignore` for the hidden rows and `git status` for the letters.
/// Both are the same programs, reading the same repository, that the local listing uses —
/// a matcher of this daemon's own would be a second answer to which files a person's
/// commits hide, and the two would disagree the first time a pattern was subtle.
///
/// Every one of those answers is optional. A host with no `git`, a workspace that is not a
/// repository, and a helper from before `fs.ignored` all leave a row without a letter and
/// a listing that shows everything: a tree that cannot know is a tree without marks, not a
/// tree that claims every path is clean and visible.
async fn list_remote(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    root: &str,
    query: &FileQuery,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    let requested = query.path.as_deref().unwrap_or_default();
    let relative = relative_path(requested)?;
    let separated = relative.to_string_lossy().replace('\\', "/");
    let answer = ask(
        state,
        host,
        "fs.list",
        serde_json::json!({ "path": remote_path(root, &separated) }),
    )
    .await?;
    let listed = answer.map_err(|message| {
        ApiError::new(AcpilotError::not_found(format!(
            "path {} on {}",
            empty_as_root(requested),
            host.id
        )))
        .with_detail("transport", message)
    })?;
    let entries = listed
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            ApiError::new(AcpilotError::HostUnavailable {
                host: host.id.clone(),
            })
            .with_detail(
                "transport",
                format!("the helper answered fs.list with {listed}, which is not a listing"),
            )
        })?;
    let prefix = if separated.is_empty() {
        String::new()
    } else {
        format!("{separated}/")
    };
    let honour = state
        .preferences()
        .is_none_or(|preferences| preferences.tree_respect_gitignore);
    let include_ignored = query.include_ignored.unwrap_or(false);
    let mut listing: Vec<FileEntry> = entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name").and_then(serde_json::Value::as_str)?;
            Some(FileEntry {
                path: format!("{prefix}{name}"),
                is_dir: entry
                    .get("is_dir")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                size: entry
                    .get("size")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                modified: None,
                ignored: false,
                status: None,
            })
        })
        // `.git` is the repository's bookkeeping rather than the project's files, and no
        // ignore rule has to say so — the same rule the local listing follows.
        .filter(|row| {
            !(honour
                && row
                    .path
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name == ".git"))
        })
        .collect();
    // Asked once for the whole directory rather than once per row: a row that asked its
    // own question would be one `git` process start on the far host per file.
    let changed = changes_remote(state, host, root).await;
    let ignored = if honour {
        ignored_remote(state, host, root, &listing).await
    } else {
        std::collections::HashSet::new()
    };
    listing.retain(|row| include_ignored || !ignored.contains(&row.path));
    for row in &mut listing {
        row.ignored = ignored.contains(&row.path);
        row.status = changed.get(&row.path).copied();
    }
    listing.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Json(listing))
}

/// Every changed path under a root on the far side, as the letter a tree row draws.
///
/// The same two `git` questions the local [`changes`] answers, asked through the helper's
/// `exec` because `git` has to read the repository the files are in. An empty map is the
/// answer for every way that can fail — no `git` on the host, not a repository — for the
/// same reason the local one is: a tree without letters is not an error a listing carries.
async fn changes_remote(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    root: &str,
) -> BTreeMap<String, FileChange> {
    let Some(top) = git_remote(state, host, &["-C", root, "rev-parse", "--show-toplevel"]).await
    else {
        return BTreeMap::new();
    };
    let top = top.trim();
    if top.is_empty() {
        return BTreeMap::new();
    }
    // Run at the workspace root rather than the repository's: paths are reported relative
    // to the repository either way, and the root is the path this daemon knows.
    let Some(status) = git_remote(
        state,
        host,
        &[
            "-C",
            root,
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ],
    )
    .await
    else {
        return BTreeMap::new();
    };
    let Ok(status) = acpilot_git::parse_status(status.as_bytes()) else {
        return BTreeMap::new();
    };
    let prefix = remote_prefix(root, top);
    let mut changes: BTreeMap<String, FileChange> = BTreeMap::new();
    for entry in &status.files {
        let Some(relative) = workspace_relative(&entry.path, prefix.as_deref()) else {
            continue;
        };
        let change = change_of(entry);
        let mut path = Some(relative.as_str());
        while let Some(current) = path {
            let slot = changes.entry(current.to_owned()).or_insert(change);
            if urgency(change) > urgency(*slot) {
                *slot = change;
            }
            path = current.rfind('/').map(|at| &current[..at]);
        }
    }
    changes
}

/// The workspace root's path inside its repository, when the repository is bigger.
///
/// The same arithmetic [`changes`] does with `canonicalize`, done on the far host's own
/// spellings: both paths came from that host's `git`, so they already agree on separators
/// and on whether a `..` was collapsed.
fn remote_prefix(root: &str, top: &str) -> Option<String> {
    let normalise = |path: &str| path.trim_end_matches(['/', '\\']).replace('\\', "/");
    let (root, top) = (normalise(root), normalise(top));
    if root == top {
        return None;
    }
    root.strip_prefix(&format!("{top}/")).map(str::to_owned)
}

/// Which of `rows` an ignore rule on the far host hides.
///
/// One question for the whole directory, and an empty answer for every way it can go
/// unanswered: a helper that does not offer `fs.ignored` refuses by name, and a refusal
/// here reads as "nothing is hidden", which is the listing that shows every file rather
/// than the one that silently loses rows.
async fn ignored_remote(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    root: &str,
    rows: &[FileEntry],
) -> std::collections::HashSet<String> {
    let paths: Vec<&str> = rows.iter().map(|row| row.path.as_str()).collect();
    if paths.is_empty() {
        return std::collections::HashSet::new();
    }
    let Ok(Ok(answer)) = ask(
        state,
        host,
        "fs.ignored",
        serde_json::json!({ "root": root, "paths": paths }),
    )
    .await
    else {
        return std::collections::HashSet::new();
    };
    answer
        .get("ignored")
        .and_then(serde_json::Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// One `git` on the far host, and what it printed.
///
/// `None` is every way that question can go unanswered — no `git` on the host, a directory
/// that is not a repository, a program that exited non-zero — because each of them is a
/// tree without letters rather than an error a listing should carry.
async fn git_remote(
    state: &AppState,
    host: &acpilot_proto::host::HostSummary,
    args: &[&str],
) -> Option<String> {
    let answered = ask(
        state,
        host,
        "exec",
        serde_json::json!({ "program": "git", "args": args }),
    )
    .await
    .ok()?;
    let answer = answered.ok()?;
    if answer.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        return None;
    }
    answer
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// A refusal from the far side of a read, in this API's terms.
///
/// The helper's words travel as the detail rather than being dropped: "permission denied"
/// and "no such file" are both a path that could not be read here, and only the host can
/// say which.
fn read_refusal(
    host: &acpilot_proto::host::HostSummary,
    requested: &str,
    message: &str,
) -> ApiError {
    if message.contains("not UTF-8 text") {
        return binary(requested);
    }
    ApiError::new(AcpilotError::not_found(format!(
        "path {requested} on {}",
        host.id
    )))
    .with_detail("transport", message.to_owned())
}

/// The far host's spelling of a workspace-relative path.
///
/// The separator follows the root, because the root is the host's spelling: a Windows host
/// names `C:\\src` and joins with a backslash, and a path sent with the wrong one is a file
/// that is not there.
fn remote_path(root: &str, relative: &str) -> String {
    let root = root.trim_end_matches(['/', '\\']);
    if relative.is_empty() {
        return root.to_owned();
    }
    let separator = if root.contains('\\') { '\\' } else { '/' };
    let relative = if separator == '\\' {
        relative.replace('/', "\\")
    } else {
        relative.to_owned()
    };
    format!("{root}{separator}{relative}")
}

/// The token a client holds for text read across a transport.
///
/// Shorter than [`etag`] because there is no stat to fold in: the text is the version, and
/// a hash of it is the whole answer.
fn etag_of(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hash: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{:x}-{hash}", bytes.len())
}

/// The root, checked against the id the client named.
///
/// An empty id means "the one you serve", which is the only value a v1 client
/// can usefully send before workspaces are plural. A different id is a `404`
/// naming both, because a client that thinks it is editing directory A while
/// the daemon serves B has no way to notice otherwise. A workspace that was opened on
/// another host is a `409` here: it is known, and the routes that walk this machine's
/// filesystem are not the ones that read it.
pub(crate) fn workspace_root(
    state: &AppState,
    workspace: Option<&str>,
) -> Result<PathBuf, ApiError> {
    if let Some(id) = workspace.filter(|id| !id.is_empty())
        && let Some(row) = state
            .workspaces()
            .list()
            .into_iter()
            .find(|row| row.id == id)
        && row.host_id != crate::hosts::LOCAL_HOST
    {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "{id} is on {}, and this route walks this daemon's own filesystem rather than asking that host",
            row.host_id
        ))));
    }
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
        .route("/v1/files", get(read).put(write).delete(delete))
        .route("/v1/files/directory", post(create_directory))
        .route("/v1/files/list", get(list))
        .route("/v1/files/search", get(search))
        .route("/v1/files/grep", get(grep))
        .route(
            "/v1/files/references",
            get(references).post(references_in_buffer),
        )
        .route("/v1/files/diagnostics", post(diagnostics))
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
    get,
    path = "/v1/files/search",
    tag = "files",
    summary = "Find files by path",
    params(
        ("workspace" = Option<String>, Query, description = "Workspace id; empty or absent means the one this daemon serves"),
        ("q" = String, Query, description = "A path query: a substring, or initials matched in order, ignoring case")
    ),
    responses(
        (status = 200, description = "Matching files, best match first, at most MAX_SEARCH_RESULTS of them", body = Vec<FileEntry>),
        (status = 409, description = "This daemon has no workspace to walk", body = ErrorBody)
    )
)]
fn search_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/files/grep",
    tag = "files",
    summary = "Find lines by content",
    params(
        ("workspace" = Option<String>, Query, description = "Workspace id; empty or absent means the one this daemon serves"),
        ("q" = String, Query, description = "Text a line has to contain, ignoring case")
    ),
    responses(
        (status = 200, description = "Matching lines, sorted by path and line, at most MAX_GREP_RESULTS of them", body = Vec<GrepMatch>),
        (status = 409, description = "This daemon has no workspace to walk", body = ErrorBody)
    )
)]
fn grep_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/files/references",
    tag = "files",
    summary = "Find the code uses of an identifier",
    params(
        ("workspace" = Option<String>, Query, description = "Workspace id; empty or absent means the one this daemon serves"),
        ("q" = String, Query, description = "The identifier to look for: one word, found where the parse tree calls it a name")
    ),
    responses(
        (status = 200, description = "Uses in code, sorted by path, line and column, at most MAX_REFERENCES of them", body = Vec<Reference>),
        (status = 409, description = "This daemon has no workspace to walk", body = ErrorBody)
    )
)]
fn references_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/files/diagnostics",
    tag = "files",
    summary = "What the language server says is wrong with a file",
    request_body = DiagnosticsRequest,
    responses(
        (status = 200, description = "The problems, and the server that found them; `server` absent means nothing was asked", body = FileDiagnostics),
        (status = 400, description = "A body that is not a diagnostics request, or a path that escapes", body = ErrorBody),
        (status = 409, description = "This daemon has no workspace", body = ErrorBody)
    )
)]
fn diagnostics_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/files/references",
    tag = "files",
    summary = "Find the code uses of an identifier in an open buffer",
    request_body = ReferencesRequest,
    responses(
        (status = 200, description = "Uses in code, sorted by path, line and column, at most MAX_REFERENCES of them", body = Vec<Reference>),
        (status = 400, description = "A body that is not a references request, or a path that escapes", body = ErrorBody),
        (status = 409, description = "This daemon has no workspace to walk", body = ErrorBody)
    )
)]
fn references_in_buffer_doc() {}

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
        (
            "/v1/files/search",
            HttpMethod::Get,
            <__path_search_doc as Path>::operation(),
        ),
        (
            "/v1/files/grep",
            HttpMethod::Get,
            <__path_grep_doc as Path>::operation(),
        ),
        (
            "/v1/files/references",
            HttpMethod::Get,
            <__path_references_doc as Path>::operation(),
        ),
        (
            "/v1/files/references",
            HttpMethod::Post,
            <__path_references_in_buffer_doc as Path>::operation(),
        ),
        (
            "/v1/files/diagnostics",
            HttpMethod::Post,
            <__path_diagnostics_doc as Path>::operation(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn a_nested_gitignore_applies_to_the_directory_it_sits_in() {
        let root = tempfile::tempdir().expect("a root");
        fs::create_dir_all(root.path().join("vendor/pkg")).expect("dirs");
        fs::write(root.path().join("vendor/pkg/.gitignore"), "*.min.js\n").expect("ignore");

        let rules = ignore_rules(root.path(), &root.path().join("vendor/pkg"));
        assert!(
            rules.matched("vendor/pkg/app.min.js", false).is_ignore(),
            "the nested file's rule applies to the directory it sits in"
        );
        assert!(
            !rules.matched("vendor/pkg/app.js", false).is_ignore(),
            "and nothing else"
        );
        assert!(
            !rules.matched("src/main.js", false).is_ignore(),
            "a nested file does not reach outside its directory"
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
