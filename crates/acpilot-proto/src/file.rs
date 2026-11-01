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
