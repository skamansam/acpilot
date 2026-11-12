//! The source-control pane's state, without a window.
//!
//! The desktop surface reads the same routes as the terminal — `GET
//! /v1/git/status` plus the unstaged and staged diffs — and cuts the patch with
//! [`acpilot_git::patch_section`], so the two panes cannot disagree about which
//! lines belong to a file. `s`/`u` move the selected path across the index through
//! the daemon, `h`/`l` choose an unstaged hunk and `shift-s` stages it, `d`/`x` arm
//! a discard or checkout, and [`Composer`]'s `confirming` state is what makes a
//! write a second decision rather than a keystroke.
//!
//! Plain data on purpose: the state a test needs to read is here, and [`crate::app`]
//! only has to lay it out.

use acpilot_proto::git::{GitChangeKind, GitCommit, GitDiff, GitStatus, GitStatusEntry};
use acpilot_proto::session::SessionSummary;

/// A row the source pane draws: a section heading or one diff line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLine {
    /// The text to draw.
    pub text: String,
    /// Whether this is an `unstaged` or `staged` heading.
    pub heading: bool,
    /// Whether this is the header of the hunk `shift-s` would stage.
    pub selected_hunk: bool,
}

/// What the pane knows about the repository right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Branch state and every changed path.
    pub status: GitStatus,
    /// Working tree against the index: the hunks that can still be staged.
    pub unstaged_diff: GitDiff,
    /// Index against `HEAD`: the hunks the next commit already holds.
    pub staged_diff: GitDiff,
    /// The sessions a generated message may be asked of, in the daemon's order.
    /// The first is the default; the composer can choose another.
    pub sessions: Vec<SessionSummary>,
    /// Which changed path is selected.
    pub selected: usize,
    /// Which unstaged hunk `shift-s` would stage, zero-based.
    pub hunk: usize,
    /// The first diff line on screen.
    pub scroll: usize,
    /// Whether the first answer has arrived.
    pub loading: bool,
    /// Why the answer did not arrive.
    pub error: Option<String>,
    /// A destructive restore waiting for `y`, with the path it was armed on.
    /// The path is held here rather than read again at confirmation, so moving
    /// the selection cannot change what `y` writes.
    pub pending: Option<PendingRestore>,
}

/// Which destructive reset is waiting for a confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restore {
    /// Put the working tree back to the index, keeping staged changes.
    Discard,
    /// Put both the index and the working tree back to `HEAD`.
    Checkout,
}

impl Restore {
    /// The verb a confirmation prompt uses.
    #[must_use]
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Discard => "discard",
            Self::Checkout => "check out",
        }
    }
}

/// A restore that has been asked for but not confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRestore {
    /// Which reset `y` will run.
    pub restore: Restore,
    /// The repository-relative path it was armed on.
    pub path: String,
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
            unstaged_diff: GitDiff {
                patch: String::new(),
                files: Vec::new(),
            },
            staged_diff: GitDiff {
                patch: String::new(),
                files: Vec::new(),
            },
            sessions: Vec::new(),
            selected: 0,
            hunk: 0,
            scroll: 0,
            loading: true,
            error: None,
            pending: None,
        }
    }

    /// Keep the selected path and hunk across a refresh, and clamp them when the
    /// diff changed underneath the pane.
    pub fn apply(
        &mut self,
        status: GitStatus,
        unstaged: GitDiff,
        staged: GitDiff,
        sessions: Vec<SessionSummary>,
    ) {
        let chosen = self.selected_path().map(str::to_owned);
        self.selected = chosen
            .and_then(|path| status.files.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| self.selected.min(status.files.len().saturating_sub(1)));
        self.status = status;
        self.unstaged_diff = unstaged;
        self.staged_diff = staged;
        self.sessions = sessions;
        self.loading = false;
        self.error = None;
        self.scroll = 0;
        self.hunk = self.hunk.min(self.hunk_count().saturating_sub(1));
        self.pending = None;
    }

    /// The daemon refused the read.
    pub fn failed(&mut self, message: impl Into<String>) {
        self.loading = false;
        self.error = Some(message.into());
        self.pending = None;
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

    /// Whether the selected path has an unstaged change, or is untracked.
    #[must_use]
    pub fn can_discard_selected(&self) -> bool {
        self.selected_entry().is_some_and(|entry| {
            !matches!(
                entry.worktree,
                GitChangeKind::Unmodified | GitChangeKind::Ignored
            )
        })
    }

    /// Whether the selected path has a version in `HEAD` to check out.
    ///
    /// An untracked path has neither an index nor a `HEAD` version, so checkout
    /// refuses it and discard is the action that can remove it.
    #[must_use]
    pub fn can_checkout_selected(&self) -> bool {
        self.status.head.is_some()
            && self.selected_entry().is_some_and(|entry| {
                staged_kind(entry.index)
                    || !matches!(
                        entry.worktree,
                        GitChangeKind::Unmodified
                            | GitChangeKind::Ignored
                            | GitChangeKind::Untracked
                    )
            })
    }

    /// Arm a restore on the selected path, if it can run.
    ///
    /// Returns why nothing was armed as an error string, which the pane shows
    /// without asking the daemon to refuse the same request again.
    pub fn arm(&mut self, restore: Restore) -> Result<(), String> {
        let Some(path) = self.selected_path().map(str::to_owned) else {
            return Err("no changed file is selected".to_owned());
        };
        let available = match restore {
            Restore::Discard => self.can_discard_selected(),
            Restore::Checkout => self.can_checkout_selected(),
        };
        if !available {
            return Err(match restore {
                Restore::Discard => format!("{path} has no unstaged change to discard"),
                Restore::Checkout if self.status.head.is_none() => {
                    format!("{path} cannot be checked out before the first commit")
                }
                Restore::Checkout => {
                    format!("{path} has no HEAD version to check out")
                }
            });
        }
        self.error = None;
        self.pending = Some(PendingRestore { restore, path });
        Ok(())
    }

    /// Take the armed restore, if there is one.
    pub fn confirm(&mut self) -> Option<PendingRestore> {
        self.pending.take()
    }

    /// Forget the armed restore without writing.
    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// The path whose diff is on the right.
    #[must_use]
    pub fn selected_path(&self) -> Option<&str> {
        self.status
            .files
            .get(self.selected)
            .map(|entry| entry.path.as_str())
    }

    /// The selected file's unstaged section, header included.
    #[must_use]
    pub fn unstaged_section(&self) -> Vec<&str> {
        let Some(path) = self.selected_path() else {
            return Vec::new();
        };
        acpilot_git::patch_section(&self.unstaged_diff.patch, path)
    }

    /// The selected file's staged section, header included.
    #[must_use]
    pub fn staged_section(&self) -> Vec<&str> {
        let Some(path) = self.selected_path() else {
            return Vec::new();
        };
        acpilot_git::patch_section(&self.staged_diff.patch, path)
    }

    /// How many unstaged hunks the selected file has.
    #[must_use]
    pub fn hunk_count(&self) -> usize {
        self.unstaged_section()
            .iter()
            .filter(|line| line.starts_with("@@"))
            .count()
    }

    /// The selected hunk, clamped to the current unstaged diff.
    #[must_use]
    pub fn selected_hunk(&self) -> usize {
        self.hunk.min(self.hunk_count().saturating_sub(1))
    }

    /// Whether `shift-s` has an unstaged hunk to stage.
    #[must_use]
    pub fn can_stage_hunk(&self) -> bool {
        self.hunk_count() > 0
    }

    /// Move the hunk selection and put its header at the top of the view.
    pub fn step_hunk(&mut self, delta: isize) {
        let count = self.hunk_count();
        if count == 0 {
            self.hunk = 0;
            return;
        }
        let last = count - 1;
        self.hunk = if delta < 0 {
            self.hunk.saturating_sub(delta.unsigned_abs())
        } else {
            (self.hunk + delta as usize).min(last)
        };
        if let Some(line) = self
            .source_lines()
            .iter()
            .position(|line| line.selected_hunk)
        {
            self.scroll = line;
        }
    }

    /// The rows the diff area draws: each non-empty half under its own heading.
    #[must_use]
    pub fn source_lines(&self) -> Vec<SourceLine> {
        let mut lines = Vec::new();
        let unstaged = self.unstaged_section();
        if !unstaged.is_empty() {
            let hunks = self.hunk_count();
            lines.push(SourceLine {
                text: format!(
                    "unstaged · {hunks} {} · h/l chooses · shift-s stages",
                    if hunks == 1 { "hunk" } else { "hunks" }
                ),
                heading: true,
                selected_hunk: false,
            });
            let chosen = self.selected_hunk();
            let mut hunk = 0;
            for line in unstaged {
                let header = line.starts_with("@@");
                lines.push(SourceLine {
                    text: line.to_owned(),
                    heading: false,
                    selected_hunk: header && hunk == chosen,
                });
                if header {
                    hunk += 1;
                }
            }
        }
        let staged = self.staged_section();
        if !staged.is_empty() {
            lines.push(SourceLine {
                text: "staged · already in the next commit".to_owned(),
                heading: true,
                selected_hunk: false,
            });
            lines.extend(staged.into_iter().map(|line| SourceLine {
                text: line.to_owned(),
                heading: false,
                selected_hunk: false,
            }));
        }
        lines
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
        self.hunk = 0;
    }

    /// Scroll the diff by a page, stopping at its end.
    pub fn page(&mut self, delta: isize) {
        let total = self.source_lines().len();
        let next = if delta < 0 {
            self.scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll.saturating_add(delta as usize)
        };
        self.scroll = next.min(total.saturating_sub(1));
    }

    /// The session a draft may be generated from.
    ///
    /// A chosen id wins while it is still listed; otherwise the first session the
    /// daemon sent is the default. The first is not a policy, only how a composer
    /// opens before someone chooses another with Tab.
    #[must_use]
    pub fn session(&self, chosen: Option<&str>) -> Option<&SessionSummary> {
        self.session_index(chosen)
            .and_then(|index| self.sessions.get(index))
    }

    /// Where the chosen session sits in the daemon's list, or the first row's
    /// index when nothing was chosen.
    #[must_use]
    pub fn session_index(&self, chosen: Option<&str>) -> Option<usize> {
        chosen
            .and_then(|id| self.sessions.iter().position(|session| session.id == id))
            .or_else(|| (!self.sessions.is_empty()).then_some(0))
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
    /// The session that drafts the next message, by id. `None` means the first
    /// session the daemon listed — the default until Tab makes it a choice.
    pub session_id: Option<String>,
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

    /// Move the draft's session by `delta`, wrapping at either end.
    ///
    /// An empty list clears the choice; the next draft then reports that no
    /// session is open rather than asking a session that has gone away.
    pub fn step_session(&mut self, source: &Source, delta: isize) {
        let Some(current) = source.session_index(self.session_id.as_deref()) else {
            self.session_id = None;
            return;
        };
        let next = (current as isize + delta).rem_euclid(source.sessions.len() as isize) as usize;
        self.session_id = Some(source.sessions[next].id.clone());
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
            profile: "acpilot".to_owned(),
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
            GitDiff::default(),
            vec![session("s1")],
        );
        assert_eq!(source.selected_path(), Some("a.rs"));
        assert_eq!(source.staged(), 1);
        assert_eq!(
            source.session(None).map(|session| session.id.as_str()),
            Some("s1")
        );
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
            GitDiff::default(),
            Vec::new(),
        );
        assert!(
            source.can_stage_selected(),
            "an untracked file can be staged"
        );
        assert!(!source.can_unstage_selected());
        source.apply(
            status(Vec::new()),
            GitDiff::default(),
            GitDiff::default(),
            Vec::new(),
        );
        assert_eq!(source.selected_path(), None);
    }

    #[test]
    fn a_draft_can_be_asked_of_any_listed_session() {
        let mut source = Source::loading();
        source.apply(
            status(Vec::new()),
            GitDiff::default(),
            GitDiff::default(),
            vec![session("one"), session("two"), session("three")],
        );
        let mut composer = Composer::default();
        assert_eq!(
            source
                .session(composer.session_id.as_deref())
                .map(|session| session.id.as_str()),
            Some("one"),
            "the first listed is the default, not the only choice"
        );

        composer.step_session(&source, 1);
        assert_eq!(composer.session_id.as_deref(), Some("two"));
        composer.step_session(&source, 1);
        assert_eq!(composer.session_id.as_deref(), Some("three"));
        composer.step_session(&source, 1);
        assert_eq!(
            composer.session_id.as_deref(),
            Some("one"),
            "tab wraps around the daemon's list"
        );
        composer.step_session(&source, -1);
        assert_eq!(
            composer.session_id.as_deref(),
            Some("three"),
            "shift-tab wraps the other way"
        );

        composer.session_id = Some("two".to_owned());
        source.apply(
            status(Vec::new()),
            GitDiff::default(),
            GitDiff::default(),
            vec![session("three"), session("one"), session("two")],
        );
        assert_eq!(
            source
                .session(composer.session_id.as_deref())
                .map(|session| session.id.as_str()),
            Some("two"),
            "the choice follows the id, not the row"
        );

        composer.session_id = Some("gone".to_owned());
        assert_eq!(
            source
                .session(composer.session_id.as_deref())
                .map(|session| session.id.as_str()),
            Some("three"),
            "a vanished choice falls back to the first still listed"
        );
        source.apply(
            status(Vec::new()),
            GitDiff::default(),
            GitDiff::default(),
            Vec::new(),
        );
        composer.step_session(&source, 1);
        assert_eq!(composer.session_id, None);
    }

    #[test]
    fn hunk_navigation_is_bounded_by_the_unstaged_diff() {
        let mut source = Source::loading();
        source.apply(
            status(vec![entry(
                "a.rs",
                GitChangeKind::Modified,
                GitChangeKind::Modified,
            )]),
            GitDiff {
                patch: "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b\n@@ -20 +20 @@\n-c\n+d\n"
                    .to_owned(),
                files: Vec::new(),
            },
            GitDiff::default(),
            Vec::new(),
        );
        assert_eq!(source.hunk_count(), 2);
        assert_eq!(source.selected_hunk(), 0);
        source.step_hunk(1);
        assert_eq!(source.selected_hunk(), 1);
        assert!(
            source
                .source_lines()
                .iter()
                .any(|line| line.selected_hunk && line.text.starts_with("@@ -20")),
            "the second hunk's header is the selected one"
        );
        source.step_hunk(9);
        assert_eq!(source.selected_hunk(), 1, "the selection clamps at the end");
        source.step_hunk(-9);
        assert_eq!(source.selected_hunk(), 0, "and at the start");
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
