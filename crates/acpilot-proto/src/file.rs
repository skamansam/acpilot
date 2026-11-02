//! Files: listings and contents, with optimistic concurrency.
//!
//! Edits do not flow through this API on the keystroke path — the editor owns
//! the buffer. These types describe saves and reads, and the ETag that keeps a
//! save from silently overwriting someone else's change.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FileEntry {
    /// Path relative to the workspace root, using `/` as the separator everywhere.
    pub path: String,
    /// Whether this is a directory.
    pub is_dir: bool,
    /// Size in bytes; zero for directories.
    pub size: u64,
    /// RFC 3339 modification time, when the host reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// Whether an ignore rule hides this entry.
    ///
    /// Never true unless the caller asked for hidden rows (`include_ignored` on the
    /// list route): a tree's "show hidden" toggle is the only thing that asks, and
    /// the flag travels with the row because a pane draws a hidden file as a hidden
    /// one rather than pretending it was never ignored.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ignored: bool,
    /// How this path differs from the last commit, when it does.
    ///
    /// Absent for a clean path, for a workspace that is not a repository, and for a
    /// host with no `git` on it. A tree whose letters are unknowable draws no
    /// letters rather than claiming everything is committed, and the field travels
    /// with the row so a listing is one answer rather than a listing plus a second
    /// question about the same directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<FileChange>,
}

/// What happened to a path since the last commit.
///
/// One letter per path, not git's two columns: a tree row is a name, and which half
/// of a change is staged is a question for the source-control pane, which reads the
/// index and the working tree separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    /// Tracked, with edits.
    Modified,
    /// In the index and not in the last commit.
    Added,
    /// Not tracked and not ignored.
    Untracked,
    /// Gone from the working tree.
    Deleted,
    /// A merge conflict, which git will not commit through.
    Conflicted,
}

impl FileChange {
    /// The letter a row shows, which is git's own letter for the same state.
    #[must_use]
    pub fn letter(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Untracked => "?",
            Self::Deleted => "D",
            Self::Conflicted => "!",
        }
    }
}

/// Serde's `skip_serializing_if` for an absent flag: a listing of ordinary files
/// costs no bytes saying that none of them is hidden.
fn is_false(flag: &bool) -> bool {
    !*flag
}

/// One line a content search matched.
///
/// The line's text travels with the match because a jump list has to be read
/// before it is chosen from: a list of `src/main.rs:12` is a list nobody can pick
/// the right row out of without opening every one of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GrepMatch {
    /// Workspace-relative path of the file the line is in.
    pub path: String,
    /// 1-based line number, as an editor counts lines.
    pub line: u32,
    /// The line as it reads, with trailing whitespace removed.
    pub text: String,
}

/// One use of an identifier, for the references panel.
///
/// `column` is a character column rather than a byte offset: a pane places a caret on
/// the glyph, and a byte column drifts left of it the first time a line holds a `日`.
/// The line's text travels with the row for the same reason a content search's does —
/// a list of `src/app.rs:12` cannot be chosen from without opening every one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Reference {
    /// Workspace-relative path of the file the use is in.
    pub path: String,
    /// 1-based line number, as an editor counts lines.
    pub line: u32,
    /// 1-based character column the name starts at.
    pub column: u32,
    /// The line as it reads, with trailing whitespace removed.
    pub text: String,
}

/// What happened to one line since the last commit, for a gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineChange {
    /// A line the committed file did not have.
    Added,
    /// A line that replaced something.
    Modified,
    /// The line above a gap: lines were removed here.
    Deleted,
}

/// One marked line, as a gutter counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LineMark {
    /// 1-based line number in the file as it is now.
    pub line: u32,
    /// What changed on it.
    pub change: LineChange,
}

/// File contents plus the token needed to write them back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FileContent {
    /// Workspace-relative path.
    pub path: String,
    /// UTF-8 contents. Binary files are refused rather than mangled.
    pub text: String,
    /// Opaque validator; send it back as `if_match` to write.
    pub etag: String,
    /// The lines that differ from the last commit, for a gutter.
    ///
    /// With the text rather than behind its own route: a pane opens a file once, and
    /// a gutter that asked a second question about the same file would be a second
    /// round trip whose answer could disagree with the text it is drawn beside. Empty
    /// for a file git does not track, for a workspace that is not a repository, and on
    /// a host with no `git`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<LineMark>,
}

/// Request body for `POST /v1/files/references`.
///
/// The query-string form of the same question, plus the buffer the editor is holding.
/// A file edited and not saved is the file the person is reading, so the search has
/// to run over the text that was sent rather than over what the last save left on
/// disk — otherwise a use typed a moment ago is missing from the list and a use
/// deleted a moment ago is still in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReferencesRequest {
    /// Workspace id, when the client names one. The daemon serves exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The identifier to look for: one word.
    pub q: String,
    /// The file the caret is in, workspace-relative.
    pub path: String,
    /// The caret's offset in that file, counted in characters, which is what a server
    /// resolves a name to a symbol with.
    ///
    /// Characters rather than bytes because a caret is a glyph: the daemon converts to
    /// the server's own unit, and a byte offset would drift left of the caret the first
    /// time a line held a `日`. Absent means no caret: the text is still the version the
    /// search reads, and no server is asked, because resolving a name needs a place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// The buffer as the editor holds it, for the file `path` names.
    ///
    /// Sent for the open file only: the rest of the workspace is read where it is,
    /// because a client for a remote daemon does not have it. Sending it is not a
    /// save — nothing here writes — and the daemon never asks for a second copy of a
    /// file it can read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// How bad a problem a language server reported is.
///
/// The protocol's four levels, named rather than numbered: a client that switched on
/// `2` would be a client that changed meaning when a server did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// The code does not compile.
    Error,
    /// It compiles and probably should not.
    Warning,
    /// A remark.
    Information,
    /// A suggestion.
    Hint,
}

/// One problem a language server reported about a file.
///
/// Lines and columns are 1-based here, as everywhere else on this wire, and the
/// columns are counted in characters in [`FileDiagnostics::encoding`] — the daemon
/// converts the server's own numbers once, where the text is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Diagnostic {
    /// 1-based line the problem starts on.
    pub line: u32,
    /// 1-based character column it starts at.
    pub column: u32,
    /// 1-based line it ends on.
    pub end_line: u32,
    /// 1-based character column it ends at.
    pub end_column: u32,
    /// How bad it is.
    pub severity: DiagnosticSeverity,
    /// What the server said.
    pub message: String,
    /// Who said it, when the server names itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The server's code for it, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// Request body for `POST /v1/files/diagnostics`.
///
/// The path is the question; the text is the version it is about. A buffer edited and
/// not saved is the text the person is reading, and problems reported about the last
/// save are problems about a file nobody is looking at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DiagnosticsRequest {
    /// Workspace id, when the client names one. The daemon serves exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The file to ask about, workspace-relative.
    pub path: String,
    /// The buffer as the editor holds it. Absent means the file on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// What a language server says is wrong with a file.
///
/// `server` absent means nothing was asked: no server for the language, none
/// installed, or one that would not start. That is deliberately different from an
/// empty `items`, which is a server saying the file is clean — a pane that drew both
/// as "no problems" would be claiming a check that never happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FileDiagnostics {
    /// The command that answered, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// The encoding the columns are counted in, when a server answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<crate::lsp::PositionEncoding>,
    /// The problems, in the server's order.
    #[serde(default)]
    pub items: Vec<Diagnostic>,
}

/// What a language server pushed about a file, for a client that never asked.
///
/// The same answer `POST /v1/files/diagnostics` returns, minus the request: a server
/// that finishes an index publishes afterwards, and a pane that only heard about the
/// problems it asked for would draw a stale gutter until somebody typed. `version`
/// travels because the protocol allows a server to say which text a publish is about,
/// and a client that knows its own version can tell a late answer from a current one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DiagnosticsNotice {
    /// Workspace-relative path the problems are about.
    pub path: String,
    /// The document version the server named, when it named one.
    ///
    /// Absent for the servers that leave it out of the publish, which is most of them;
    /// a client that needs a version asks, and the answer carries none either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    /// The problems, and the server that found them.
    pub problems: FileDiagnostics,
}

/// Request body for `PUT /v1/files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FileWriteRequest {
    /// Workspace-relative path.
    pub path: String,
    /// New UTF-8 contents.
    pub text: String,
    /// The `etag` the caller last read, or absent to create a new file.
    ///
    /// A mismatch is a `409`, never a silent overwrite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub if_match: Option<String>,
}
