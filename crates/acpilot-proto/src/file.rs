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

/// File contents plus the token needed to write them back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FileContent {
    /// Workspace-relative path.
    pub path: String,
    /// UTF-8 contents. Binary files are refused rather than mangled.
    pub text: String,
    /// Opaque validator; send it back as `if_match` to write.
    pub etag: String,
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
