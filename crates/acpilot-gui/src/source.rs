//! The source-control pane's state, without a window.
//!
//! The desktop surface reads the same two routes as the terminal — `GET
//! /v1/git/status` and `GET /v1/git/diff?scope=all` — and cuts the patch with
//! [`acpilot_git::patch_section`], so the two panes cannot disagree about which
//! lines belong to a file. `s`/`u` move the selected path across the index through
//! the daemon; [`Composer`]'s `confirming` state is what makes the commit a second
//! decision rather than a keystroke.
//!
//! Plain data on purpose: the state a test needs to read is here, and [`crate::app`]
//! only has to lay it out.

use acpilot_proto::git::{GitChangeKind, GitCommit, GitDiff, GitStatus, GitStatusEntry};
use acpilot_proto::session::SessionSummary;

/// What the pane knows about the repository right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Branch state and every changed path.
    pub status: GitStatus,
    /// The whole uncommitted diff, as the daemon served it.
    pub diff: GitDiff,
    /// The sessions a generated message may be asked of. The open one is preferred;
    /// a window with no session pane uses the first one the daemon lists.
    pub sessions: Vec<SessionSummary>,
    /// Which changed path is selected.
    pub selected: usize,
    /// The first diff line on screen.
    pub scroll: usize,
    /// Whether the first answer has arrived.
    pub loading: bool,
    /// Why the answer did not arrive.
    pub error: Option<String>,
}

impl Source {
    /// A pane waiting for the daemon.
    #[must_use]
    pub fn loading() -> Self {
        Self {
            status: GitStatus {
                branch: None,
                upstream: None,
                ahead: 0,
                behind: 0,
                head: None,
                files: Vec::new(),
            },
            diff: GitDiff {
                patch: String::new(),
                files: Vec::new(),
            },
            sessions: Vec::new(),
            selected: 0,
            scroll: 0,
            loading: true,
            error: None,
        }
    }

    /// Keep the selected path across a refresh, and clamp a path that vanished.
    pub fn apply(&mut self, status: GitStatus, diff: GitDiff, sessions: Vec<SessionSummary>) {
        let chosen = self.selected_path().map(str::to_owned);
        self.selected = chosen
            .and_then(|path| status.files.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| self.selected.min(status.files.len().saturating_sub(1)));
        self.status = status;
        self.diff = diff;
        self.sessions = sessions;
        self.loading = false;
        self.error = None;
        self.scroll = 0;
    }

    /// The daemon refused the read.
    pub fn failed(&mut self, message: impl Into<String>) {
        self.loading = false;
        self.error = Some(message.into());
    }

    /// How many paths the next commit would contain.
    #[must_use]
    pub fn staged(&self) -> usize {
        self.status
            .files
            .iter()
            .filter(|entry| staged_kind(entry.index))
            .count()
    }

    /// The selected row, when there is one.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&GitStatusEntry> {
        self.status.files.get(self.selected)
    }

    /// Whether the selected path has a working-tree change to add.
    #[must_use]
    pub fn can_stage_selected(&self) -> bool {
        self.selected_entry().is_some_and(|entry| {
            !matches!(
                entry.worktree,
                GitChangeKind::Unmodified | GitChangeKind::Ignored
            )
        })
    }

    /// Whether the selected path is in the next commit and can be removed.
    #[must_use]
    pub fn can_unstage_selected(&self) -> bool {
        self.selected_entry()
            .is_some_and(|entry| staged_kind(entry.index))
    }

    /// The path whose diff is on the right.
    #[must_use]
    pub fn selected_path(&self) -> Option<&str> {
        self.status
            .files
            .get(self.selected)
            .map(|entry| entry.path.as_str())
    }

    /// The selected file's section of the patch, header included.
    #[must_use]
    pub fn section(&self) -> Vec<&str> {
        let Some(path) = self.selected_path() else {
            return Vec::new();
        };
        acpilot_git::patch_section(&self.diff.patch, path)
    }

    /// Move the changed-file selection and put the diff back at its top.
    pub fn step(&mut self, delta: isize) {
        let last = self.status.files.len().saturating_sub(1);
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(last)
        };
        self.scroll = 0;
    }

    /// Scroll the diff by a page, stopping at its end.
    pub fn page(&mut self, delta: isize) {
        let total = self.section().len();
        let next = if delta < 0 {
            self.scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll.saturating_add(delta as usize)
        };
        self.scroll = next.min(total.saturating_sub(1));
    }

    /// The session a draft may be generated from: the first one listed, because a
    /// window with no session pane has no other way to name one.
    #[must_use]
    pub fn session(&self) -> Option<&str> {
        self.sessions.first().map(|session| session.id.as_str())
    }
}

/// Whether the index has something to commit for this kind.
#[must_use]
pub fn staged_kind(kind: GitChangeKind) -> bool {
    !matches!(kind, GitChangeKind::Unmodified | GitChangeKind::Untracked)
}

/// The one-letter mark a source-control list draws for a side of a change.
#[must_use]
pub fn letter(kind: GitChangeKind) -> char {
    use GitChangeKind::{
        Added, Copied, Deleted, Ignored, Modified, Renamed, TypeChanged, Unmerged, Unmodified,
        Untracked,
    };
    match kind {
        Unmodified | Ignored => '·',
        Modified => 'M',
        Added => 'A',
        Deleted => 'D',
        Renamed => 'R',
        Copied => 'C',
        TypeChanged => 'T',
        Unmerged => 'U',
        Untracked => '?',
    }
}

/// One changed row, ready for the list: the index's mark, the worktree's, and the
/// path. The two marks are what make "staged" visible rather than inferred.
#[must_use]
pub fn row(entry: &GitStatusEntry) -> String {
    format!(
        "{}{} {}",
        letter(entry.index),
        letter(entry.worktree),
        entry.path
    )
}

/// A commit message being written or generated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    /// The whole message, subject and body: git takes one string, and splitting it
    /// into fields is how the two disagree.
    pub text: String,
    /// Whether a session is drafting.
    pub generating: bool,
    /// Whether the commit request is in flight.
    pub committing: bool,
    /// Whether `ctrl-s` is waiting for a `y`.
    pub confirming: bool,
    /// The last refusal, shown in the composer.
    pub error: Option<String>,
}

impl Composer {
    /// The message if it can be committed.
    ///
    /// Whitespace is not a message: the daemon refuses it, and asking is cheaper than
    /// a round trip that teaches the same thing.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        let text = self.text.trim();
        (!text.is_empty()).then_some(self.text.as_str())
    }

    /// The subject that a successful commit will report, for the note beside it.
    #[must_use]
    pub fn subject(&self) -> &str {
        self.text.lines().next().unwrap_or("")
    }

    /// Apply the daemon's answer to a draft.
    pub fn draft(&mut self, draft: &acpilot_proto::git::CommitDraft) {
        self.text = draft.message.clone();
        self.generating = false;
        self.confirming = false;
        self.error = None;
    }

    /// Apply the daemon's confirmation.
    pub fn committed(&mut self, commit: &GitCommit) -> String {
        format!("committed {} · {}", short(&commit.id), commit.subject)
    }

    /// Apply a refusal to whichever request was in flight.
    pub fn failed(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.generating = false;
        self.committing = false;
        self.confirming = false;
        self.error = Some(message);
    }
}

/// The first seven characters of a commit id, or the whole id when it is shorter.
#[must_use]
pub fn short(id: &str) -> String {
    id.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(files: Vec<GitStatusEntry>) -> GitStatus {
        GitStatus {
            branch: Some("main".to_owned()),
            upstream: None,
            ahead: 0,
            behind: 0,
            head: None,
            files,
        }
    }

    fn entry(path: &str, index: GitChangeKind, worktree: GitChangeKind) -> GitStatusEntry {
        GitStatusEntry {
            path: path.to_owned(),
            old_path: None,
            index,
            worktree,
        }
    }

    fn session(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_owned(),
            workspace_id: "acpilot".to_owned(),
            agent_id: "mock".to_owned(),
            title: "one".to_owned(),
            status: acpilot_proto::session::SessionStatus::Idle,
            created_at: "2026-11-07T12:00:00Z".to_owned(),
            updated_at: "2026-11-07T12:00:00Z".to_owned(),
        }
    }

    #[test]
    fn the_selected_path_survives_a_refresh_and_clamps_when_it_is_gone() {
        let mut source = Source::loading();
        source.apply(
            status(vec![entry(
                "a.rs",
                GitChangeKind::Modified,
                GitChangeKind::Unmodified,
            )]),
            GitDiff {
                patch: "diff --git a/a.rs b/a.rs\n@@ -1 +1 @@\n-a\n+b\n".to_owned(),
                files: Vec::new(),
            },
            vec![session("s1")],
        );
        assert_eq!(source.selected_path(), Some("a.rs"));
        assert_eq!(source.staged(), 1);
        assert_eq!(source.session(), Some("s1"));
        assert!(
            !source.can_stage_selected(),
            "a path whose only change is in the index has nothing left to add"
        );
        assert!(source.can_unstage_selected());

        source.step(1);
        source.scroll = 3;
        source.apply(
            status(vec![
                entry("a.rs", GitChangeKind::Modified, GitChangeKind::Unmodified),
                entry("b.rs", GitChangeKind::Added, GitChangeKind::Unmodified),
            ]),
            GitDiff::default(),
            Vec::new(),
        );
        assert_eq!(
            source.selected_path(),
            Some("a.rs"),
            "the path on screen keeps its row"
        );
        assert_eq!(source.scroll, 0, "a new patch starts at its top");

        source.step(9);
        assert_eq!(source.selected_path(), Some("b.rs"));
        assert!(
            !source.can_stage_selected(),
            "an added path has nothing in the working tree left to stage"
        );
        source.apply(
            status(vec![entry(
                "new.rs",
                GitChangeKind::Unmodified,
                GitChangeKind::Untracked,
            )]),
            GitDiff::default(),
            Vec::new(),
        );
        assert!(
            source.can_stage_selected(),
            "an untracked file can be staged"
        );
        assert!(!source.can_unstage_selected());
        source.apply(status(Vec::new()), GitDiff::default(), Vec::new());
        assert_eq!(source.selected_path(), None);
    }

    #[test]
    fn a_row_shows_both_sides_of_a_change() {
        let staged = entry(
            "src/main.rs",
            GitChangeKind::Modified,
            GitChangeKind::Unmodified,
        );
        assert_eq!(row(&staged), "M· src/main.rs");
        let both = entry("new.rs", GitChangeKind::Added, GitChangeKind::Modified);
        assert_eq!(
            row(&both),
            "AM new.rs",
            "an edit after staging is its own mark"
        );
        assert_eq!(letter(GitChangeKind::Untracked), '?');
    }

    #[test]
    fn a_commit_needs_a_message_and_a_confirmation() {
        let mut composer = Composer::default();
        assert_eq!(composer.message(), None);
        composer.text = "  \n".to_owned();
        assert_eq!(composer.message(), None);
        composer.text = "feat: ✨ keep it confirmed\n\nbody".to_owned();
        assert_eq!(
            composer.message(),
            Some("feat: ✨ keep it confirmed\n\nbody"),
            "the whole message, not the trimmed copy"
        );
        assert_eq!(composer.subject(), "feat: ✨ keep it confirmed");
        assert!(!composer.confirming, "typing is not confirmation");
    }

    #[test]
    fn a_refusal_clears_every_in_flight_state() {
        let mut composer = Composer {
            generating: true,
            committing: true,
            confirming: true,
            ..Composer::default()
        };
        composer.failed("the index is empty");
        assert!(!composer.generating && !composer.committing && !composer.confirming);
        assert_eq!(composer.error.as_deref(), Some("the index is empty"));
    }
}
