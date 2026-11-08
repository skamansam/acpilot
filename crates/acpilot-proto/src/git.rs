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
//! The writes are named and narrow: staging and unstaging paths move the index, and
//! committing writes the commit the person confirmed. Checking out, resetting the
//! working tree and changing config remain outside this surface.

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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
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

/// The effective commit-message prompt, as a client should show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CommitPrompt {
    /// The template before its placeholders are filled.
    pub template: String,
    /// Where the template came from.
    pub source: CommitPromptSource,
}

/// Which prompt a draft would be generated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommitPromptSource {
    /// The template in `acpilot-core`.
    Default,
    /// The profile's `commit-prompt.txt`.
    Profile,
}

/// A request to draft a commit message from what is staged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CommitMessageRequest {
    /// Workspace id, when the client names one. The daemon serves exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The active session whose agent writes the message.
    pub session_id: String,
    /// A template for this request only, instead of the profile's or the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// A request to move repository-relative paths across the index or restore them.
///
/// The paths are the ones `GET /v1/git/status` reports, so a client never has to
/// translate a workspace path into the repository's spelling. The answer is the new
/// status, read after git ran, rather than the request echoed back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GitIndexRequest {
    /// Workspace id, when the client names one. The daemon serves exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Paths relative to the repository root.
    pub paths: Vec<String>,
}

/// A request to commit what the index holds.
///
/// The message is the whole thing, subject and body, with the blank line between
/// them: a client that split it into fields would have to put it back together, and
/// the subject git derives is the first line either way. Nothing here stages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GitCommitRequest {
    /// Workspace id, when the client names one. The daemon serves exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The whole commit message.
    pub message: String,
}

/// A generated message the person can edit before anything is committed.
///
/// This is a draft: the daemon neither stages nor commits, and the message is not
/// stored. The transcript of the session that wrote it is the record of how it was
/// produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CommitDraft {
    /// The whole message: subject, a blank line, then the body when there is one.
    pub message: String,
    /// The first line, after validation.
    pub subject: String,
    /// Everything after the blank line, empty when there is no body.
    pub body: String,
    /// The Conventional Commit type.
    pub kind: String,
    /// The scope, when the subject names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Whether the subject marks a breaking change.
    pub breaking: bool,
    /// The leading gitmoji, when the description starts with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// What wrote it, for example `session:<id>`.
    pub generator: String,
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
    fn an_index_request_names_the_paths_and_may_omit_the_workspace() {
        let request: GitIndexRequest =
            serde_json::from_value(serde_json::json!({ "paths": ["src/main.rs"] }))
                .expect("paths are the required half");
        assert_eq!(request.paths, ["src/main.rs"]);
        assert!(request.workspace.is_none());
        let json = serde_json::to_value(&request).expect("an index request is JSON");
        assert!(
            json.get("workspace").is_none(),
            "the daemon's own workspace costs no bytes saying so"
        );
    }

    #[test]
    fn a_commit_request_carries_the_whole_message() {
        let request = GitCommitRequest {
            workspace: None,
            message: "feat(git): ✨ commit\n\nThe body is part of it.".to_owned(),
        };
        let json = serde_json::to_value(&request).expect("a request is JSON");
        assert_eq!(json["message"], request.message);
        assert!(
            json.get("workspace").is_none(),
            "the daemon's own workspace costs no bytes saying so"
        );
    }

    #[test]
    fn a_prompt_names_whether_it_is_the_profile_s_own() {
        let prompt = CommitPrompt {
            template: "write it".to_owned(),
            source: CommitPromptSource::Profile,
        };
        let json = serde_json::to_value(&prompt).expect("a prompt is JSON");
        assert_eq!(json["source"], "profile");
    }

    #[test]
    fn a_draft_round_trips_without_claiming_a_scope_it_does_not_have() {
        let draft = CommitDraft {
            message: "fix: 🐛 keep the exit code".to_owned(),
            subject: "fix: 🐛 keep the exit code".to_owned(),
            body: String::new(),
            kind: "fix".to_owned(),
            scope: None,
            breaking: false,
            emoji: Some("🐛".to_owned()),
            generator: "session:s1".to_owned(),
        };
        let json = serde_json::to_value(&draft).expect("a draft is JSON");
        assert!(json.get("scope").is_none());
        let again: CommitDraft = serde_json::from_value(json).expect("it comes back");
        assert_eq!(again, draft);
    }

    #[test]
    fn a_request_may_omit_the_workspace_and_the_override() {
        let request: CommitMessageRequest =
            serde_json::from_value(serde_json::json!({ "session_id": "s1" }))
                .expect("only the session is required");
        assert_eq!(request.session_id, "s1");
        assert!(request.workspace.is_none());
        assert!(request.prompt.is_none());
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
