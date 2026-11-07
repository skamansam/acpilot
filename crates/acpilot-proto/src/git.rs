//! Git: the source-control pane's answers.
//!
//! The daemon asks the host's own `git` and serves what it said, so these types are
//! the shape of that answer rather than a second model of a repository. Paths are
//! relative to the **repository root**, which is what `git` reports; opening one is
//! `GET /v1/files` only when the workspace is the repository's root, and a client
//! that needs the difference asks `GET /v1/files/list` for the tree it is drawing.
//!
//! Both halves of a change travel on one row: which side is staged is the question
//! a source-control pane exists to answer, and a single letter would merge an edit
//! that is in the index with one that is not.
//!
//! Nothing here is a write. Staging, committing and checking out are later, reviewed
//! actions with their own permission path, and a type that could express one would
//! be a wire shape the daemon cannot yet honour.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What happened to a path, on one side of the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitChangeKind {
    /// Nothing changed on this side.
    Unmodified,
    /// Contents changed.
    Modified,
    /// Added.
    Added,
    /// Removed.
    Deleted,
    /// Moved from the old path.
    Renamed,
    /// Duplicated from the old path.
    Copied,
    /// A file type changed, for example a symlink became a regular file.
    TypeChanged,
    /// A merge conflict, which git will not commit through.
    Unmerged,
    /// Not tracked and not ignored.
    Untracked,
    /// Matched by an ignore rule.
    Ignored,
}

/// One changed path, with the index's answer and the working tree's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GitStatusEntry {
    /// Path relative to the repository root, with `/` as the separator.
    pub path: String,
    /// The path it was renamed or copied from, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// State in the index: what the next commit would contain.
    pub index: GitChangeKind,
    /// State in the working tree: what the next `git add` would pick up.
    pub worktree: GitChangeKind,
}

/// Branch state and every changed path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GitStatus {
    /// Current branch, absent on a detached `HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Upstream branch, when one is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Commits ahead of the upstream. Always on the wire: zero is the answer.
    pub ahead: u32,
    /// Commits behind the upstream.
    pub behind: u32,
    /// The `HEAD` commit, absent before the first commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Every changed path, as git lists them.
    pub files: Vec<GitStatusEntry>,
}

/// One file in a diff, independent of the patch text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GitDiffFile {
    /// Path after the change, relative to the repository root.
    pub path: String,
    /// The path before a rename or copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// What happened to it.
    pub change: GitChangeKind,
}

/// A unified diff and the files it touches.
///
/// The patch is colour-free and external-diff-free, because a person reads it in a
/// pane rather than in a terminal that would render either. The file list is what a
/// commit composer iterates without parsing headers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GitDiff {
    /// The unified diff text.
    pub patch: String,
    /// The files the patch touches.
    pub files: Vec<GitDiffFile>,
}

/// One path in the working tree, tracked or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GitTreeEntry {
    /// Path relative to the repository root, with `/` as the separator.
    pub path: String,
    /// Whether the index knows about it.
    pub tracked: bool,
}

/// One commit, with enough structure to draw a graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GitCommit {
    /// Full commit id.
    pub id: String,
    /// Parent ids, in order. Empty for a root commit.
    pub parents: Vec<String>,
    /// Author name.
    pub author: String,
    /// Author email.
    pub email: String,
    /// Author timestamp, RFC 3339 with the author's offset.
    pub authored_at: String,
    /// First line of the message.
    pub subject: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_halves_of_a_change_travel_on_one_row() {
        let row = GitStatusEntry {
            path: "src/main.rs".to_owned(),
            old_path: None,
            index: GitChangeKind::Unmodified,
            worktree: GitChangeKind::Modified,
        };
        let json = serde_json::to_value(&row).expect("a row is JSON");
        assert_eq!(json["index"], "unmodified");
        assert_eq!(
            json["worktree"], "modified",
            "an edit that is not staged is still the working tree's"
        );
        assert!(
            json.get("old_path").is_none(),
            "a path that did not move costs no bytes saying so"
        );
    }

    #[test]
    fn a_status_always_names_its_ahead_and_behind() {
        let status = GitStatus {
            branch: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            head: None,
            files: Vec::new(),
        };
        let json = serde_json::to_value(&status).expect("status is JSON");
        assert_eq!(json["ahead"], 0, "zero is an answer, not an absence");
        assert_eq!(json["behind"], 0);
        assert!(
            json.get("branch").is_none(),
            "a detached HEAD has no branch to name"
        );
    }

    #[test]
    fn a_root_commit_says_it_has_no_parents() {
        let commit = GitCommit {
            id: "a".to_owned(),
            parents: Vec::new(),
            author: "A".to_owned(),
            email: "a@example.com".to_owned(),
            authored_at: "2026-11-07T00:00:00Z".to_owned(),
            subject: "init".to_owned(),
        };
        let json = serde_json::to_value(&commit).expect("a commit is JSON");
        assert_eq!(
            json["parents"],
            serde_json::json!([]),
            "a root commit's empty parent list is how a graph draws the start"
        );
    }
}
