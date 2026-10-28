//! Git state for the editor and the commit writer.
//!
//! This crate shells out to the user's `git` instead of parsing the object store
//! itself. That is deliberate: `git` already honours their config, filters,
//! worktrees, submodules, signing setup and LFS, and an editor that disagrees
//! with the command line about what is staged is worse than one with no git
//! integration at all. The cost is that `git` must be on the host, which it
//! already is for anyone with a repository to open.
//!
//! Nothing here writes: no stage, commit, checkout or config change. The commit
//! writer is a later, reviewed step with its own permission path.
//!
//! Parsing is confined to stable porcelain/plumbing output:
//! `status --porcelain=v2 -z`, `diff --name-status -z`, `ls-files -z`, and a
//! `--format` string with unit separators for `log`.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which changes a diff should cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffScope {
    /// Working tree against the index.
    Unstaged,
    /// Index against `HEAD`.
    Staged,
    /// Working tree against `HEAD`, staged and unstaged together. Falls back to
    /// the staged diff in a repository with no commits yet.
    All,
}

/// What happened to one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// No change on this side.
    Unmodified,
    /// Contents changed.
    Modified,
    /// Added to the index or the working tree.
    Added,
    /// Removed.
    Deleted,
    /// Moved from the old path.
    Renamed,
    /// Duplicated from the old path.
    Copied,
    /// File type changed, for example a symlink became a regular file.
    TypeChanged,
    /// A merge conflict.
    Unmerged,
    /// Not tracked and not ignored.
    Untracked,
    /// Matched by an ignore rule.
    Ignored,
}

/// One path in `git status`, with both sides of the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Path relative to the repository root.
    pub path: String,
    /// Previous path for a rename or copy.
    pub orig_path: Option<String>,
    /// State in the index.
    pub index: ChangeKind,
    /// State in the working tree.
    pub worktree: ChangeKind,
}

impl StatusEntry {
    /// Whether something is staged for the next commit.
    #[must_use]
    pub fn is_staged(&self) -> bool {
        !matches!(self.index, ChangeKind::Unmodified | ChangeKind::Untracked)
    }

    /// Whether the working tree differs from the index.
    #[must_use]
    pub fn is_unstaged(&self) -> bool {
        !matches!(
            self.worktree,
            ChangeKind::Unmodified | ChangeKind::Untracked
        )
    }
}

/// Branch and working-tree state, as the source-control pane needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// Current branch, absent on a detached HEAD.
    pub branch: Option<String>,
    /// Upstream branch, when one is configured.
    pub upstream: Option<String>,
    /// Commits ahead of the upstream.
    pub ahead: u32,
    /// Commits behind the upstream.
    pub behind: u32,
    /// `HEAD` commit, absent before the first commit.
    pub head: Option<String>,
    /// Every changed path.
    pub files: Vec<StatusEntry>,
}

/// One file in a diff, independent of the patch text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    /// Path after the change.
    pub path: String,
    /// Path before a rename or copy.
    pub old_path: Option<String>,
    /// What happened.
    pub change: ChangeKind,
}

/// A patch plus its file list.
///
/// The patch is the text the UI renders; the list is what a permission prompt
/// or a commit message can iterate without parsing headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// Unified diff text, already colour-free and external-diff-free.
    pub patch: String,
    /// Files the patch touches.
    pub files: Vec<DiffFile>,
}

/// What one line of a file is, compared with the revision it was diffed against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineChange {
    /// The line is new.
    Added,
    /// The line replaced one or more lines that were removed: a removal and an
    /// addition at the same place, which is what a person calls a change.
    Modified,
    /// Nothing changed on this line, but something was removed at it. The marker
    /// sits on the line *above* the gap, because a gutter has to put it
    /// somewhere and a line that no longer exists cannot hold one.
    Deleted,
}

/// The lines of one file that differ from the revision, as a gutter needs them.
///
/// Keyed by the line number in the file as it is **now**, zero-based, so a pane
/// can ask once per row it draws. A file git does not track has no line changes —
/// the whole file is new, and [`Repo::status`] already says `Untracked`; a caller
/// that wants a gutter for one marks the file, not every line of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineChanges {
    marks: std::collections::BTreeMap<usize, LineChange>,
}

impl LineChanges {
    /// What changed on a line, if anything.
    #[must_use]
    pub fn get(&self, line: usize) -> Option<LineChange> {
        self.marks.get(&line).copied()
    }

    /// The changed lines, in order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, LineChange)> + '_ {
        self.marks.iter().map(|(line, change)| (*line, *change))
    }

    /// How many lines changed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.marks.len()
    }

    /// Whether the file is unchanged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }
}

impl FromIterator<(usize, LineChange)> for LineChanges {
    fn from_iter<I: IntoIterator<Item = (usize, LineChange)>>(lines: I) -> Self {
        let mut marks = std::collections::BTreeMap::new();
        for (line, change) in lines {
            // A line that was both removed and added is a change; a deletion on
            // the seam of an addition is the addition's business.
            marks
                .entry(line)
                .and_modify(|existing| {
                    if *existing == LineChange::Deleted {
                        *existing = change;
                    }
                })
                .or_insert(change);
        }
        Self { marks }
    }
}

/// The line changes in a patch, for one path.
///
/// The patch is unified diff text — what `git diff` prints, or what a permission
/// card is about to show — parsed for its hunk headers and its `+`/`-` runs.
/// Only the hunks whose file header is `path` are read, so a patch over the whole
/// working tree answers for one file; a patch with no file header at all (a bare
/// hunk) is taken to be that file's.
///
/// A removal followed by an addition at the same place is one [`LineChange::Modified`],
/// because a gutter that shows `-` then `+` for a line somebody edited says the
/// file lost a line it still has.
#[must_use]
pub fn line_changes(patch: &str, path: &str) -> LineChanges {
    let named = patch.lines().any(|line| line.starts_with("+++ "));
    // A bare hunk with no file header is the caller's file; anything else has to
    // say which file it is before its hunks count.
    let mut target = !named;
    let mut new_line = 0usize;
    let mut removed = 0usize;
    let mut seam = 0usize;
    let mut marks: Vec<(usize, LineChange)> = Vec::new();

    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("+++ ") {
            // `b/<path>`, or `/dev/null` for a deletion.
            let file = header.split('\t').next().unwrap_or(header);
            target = !named || file.strip_prefix("b/") == Some(path);
            if !target {
                // Whatever the previous file left unfinished is not this file's.
                removed = 0;
            }
            continue;
        }
        if line.starts_with("--- ") {
            continue;
        }
        if line.starts_with("@@") {
            if target {
                flush(&mut marks, &mut removed, seam);
                new_line = hunk_start(line).unwrap_or(0);
                seam = new_line;
            }
            continue;
        }
        if !target {
            continue;
        }
        match line.chars().next() {
            Some('+') => {
                // An addition this close to a removal is the other half of it.
                let change = if removed > 0 {
                    removed -= 1;
                    LineChange::Modified
                } else {
                    LineChange::Added
                };
                marks.push((new_line, change));
                new_line += 1;
            }
            Some('-') => {
                removed += 1;
                seam = new_line;
            }
            Some(' ') => {
                flush(&mut marks, &mut removed, seam);
                new_line += 1;
            }
            // A `\ No newline at end of file` marker, a `diff --git` line, an
            // index line: nothing that moves a line number.
            _ => {}
        }
    }
    flush(&mut marks, &mut removed, seam);
    marks.into_iter().collect()
}

/// Record the removals a run left unpaired, on the line above where they sat.
fn flush(marks: &mut Vec<(usize, LineChange)>, removed: &mut usize, seam: usize) {
    if *removed > 0 {
        marks.push((seam.saturating_sub(1), LineChange::Deleted));
        *removed = 0;
    }
}

/// The zero-based first line of the new file a hunk header names.
fn hunk_start(header: &str) -> Option<usize> {
    // `@@ -8,6 +8,12 @@ optional section heading`
    let plus = header.split_once('+')?.1;
    let start = plus.split([',', ' ']).next()?.parse::<usize>().ok()?;
    Some(start.saturating_sub(1))
}

/// One path in the working tree, tracked or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Path relative to the repository root.
    pub path: String,
    /// Whether the index knows about it.
    pub tracked: bool,
}

/// One commit, with enough structure to draw a graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
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

/// Failures that a caller can explain to a user.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// `git` is not installed, or not on `PATH`.
    #[error("git is not installed or not on PATH")]
    GitMissing(#[source] std::io::Error),
    /// The directory is not inside a repository.
    #[error("not a git repository: {}", path.display())]
    NotARepository {
        /// Where discovery started.
        path: PathBuf,
    },
    /// A git command failed.
    #[error("git {command} failed ({status}): {stderr}")]
    Command {
        /// The subcommand, for logs and error text.
        command: String,
        /// Exit status.
        status: i32,
        /// Git's own diagnosis.
        stderr: String,
    },
    /// Output did not have the documented shape.
    #[error("could not parse git {what}: {reason}")]
    Parse {
        /// Which output.
        what: &'static str,
        /// What was wrong.
        reason: String,
    },
    /// Reading a path failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// A repository rooted at a working-tree directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// Find the repository containing `start`.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, GitError> {
        let start = start.as_ref();
        let output = Command::new("git")
            .arg("-C")
            .arg(start)
            .args(["rev-parse", "--show-toplevel"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => GitError::GitMissing(error),
                _ => GitError::Io(error),
            })?;

        if !output.status.success() {
            return Err(GitError::NotARepository {
                path: start.to_path_buf(),
            });
        }

        let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if root.is_empty() {
            return Err(GitError::NotARepository {
                path: start.to_path_buf(),
            });
        }
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    /// Use an already-known working-tree root.
    ///
    /// The path is not validated here; every command reports a real failure if
    /// it is not a repository, which is more useful than a constructor that
    /// guesses.
    #[must_use]
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Working-tree root, as `git` spells it.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Current branch state and changed paths.
    pub fn status(&self) -> Result<Status, GitError> {
        let output = self.run([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ])?;
        parse_status(&output)
    }

    /// A patch and its file list.
    pub fn diff(&self, scope: DiffScope) -> Result<Diff, GitError> {
        if scope == DiffScope::All && !self.has_head()? {
            // No commit to compare against: the index and the working tree are
            // the two halves of "everything not yet committed".
            let mut patch = self.diff_patch(DiffScope::Staged)?;
            let mut files = self.diff_files(DiffScope::Staged)?;
            patch.push_str(&self.diff_patch(DiffScope::Unstaged)?);
            files.extend(self.diff_files(DiffScope::Unstaged)?);
            files.sort_by(|left, right| left.path.cmp(&right.path));
            return Ok(Diff { patch, files });
        }

        Ok(Diff {
            patch: self.diff_patch(scope)?,
            files: self.diff_files(scope)?,
        })
    }

    /// The lines of one file that differ from `HEAD`, for a gutter.
    ///
    /// `path` is relative to the repository root. Zero context lines and the
    /// whole change staged and unstaged together: the gutter shows what the file
    /// looks like now against what was committed, not one half of the edit.
    ///
    /// An untracked file answers with no changes — [`Self::status`] says why, and
    /// a whole file git has never seen has no line diff to draw.
    pub fn line_changes(&self, path: &str) -> Result<LineChanges, GitError> {
        let mut args = vec![
            "diff".to_owned(),
            "--no-color".to_owned(),
            "--no-ext-diff".to_owned(),
            "--unified=0".to_owned(),
        ];
        // Without a commit there is nothing to compare a working tree against, so
        // the index is the baseline: what is not yet committed is new.
        if self.has_head()? {
            args.push("HEAD".to_owned());
        } else {
            args.push("--cached".to_owned());
        }
        args.push("--".to_owned());
        args.push(path.to_owned());
        let patch = String::from_utf8_lossy(&self.run_owned(args)?).into_owned();
        Ok(line_changes(&patch, path))
    }

    /// Every path in the working tree, tracked or untracked, sorted.
    pub fn tree(&self) -> Result<Vec<TreeEntry>, GitError> {
        let tracked_output = self.run(["ls-files", "-z", "--cached"])?;
        let tracked: BTreeSet<String> = split_nul(&tracked_output)
            .map(|path| path.to_owned())
            .collect();

        let all_output = self.run([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])?;
        let paths: BTreeSet<String> = split_nul(&all_output).map(str::to_owned).collect();

        // `--cached --others` can list a path twice in a conflict state; the
        // set dedupes. Sorted output is stable for a tree view.
        Ok(paths
            .into_iter()
            .map(|path| TreeEntry {
                tracked: tracked.contains(&path),
                path,
            })
            .collect())
    }

    /// Recent commits, newest first, in topological order for a graph.
    pub fn log(&self, limit: usize) -> Result<Vec<Commit>, GitError> {
        if limit == 0 || !self.has_head()? {
            return Ok(Vec::new());
        }
        let limit = limit.to_string();
        let output = self.run([
            "log",
            "--no-color",
            "--topo-order",
            "-n",
            &limit,
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%s%x1e",
        ])?;
        parse_log(&output)
    }

    fn has_head(&self) -> Result<bool, GitError> {
        // Capture rather than inherit: `rev-parse` prints the id on success, and
        // the daemon's stdout is not a place for it.
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["rev-parse", "--verify", "--quiet", "HEAD"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => GitError::GitMissing(error),
                _ => GitError::Io(error),
            })?;
        Ok(output.status.success())
    }

    fn diff_patch(&self, scope: DiffScope) -> Result<String, GitError> {
        let mut args = vec![
            "diff".to_owned(),
            "--no-color".to_owned(),
            "--no-ext-diff".to_owned(),
            "--unified=3".to_owned(),
        ];
        match scope {
            DiffScope::Staged => args.push("--cached".to_owned()),
            DiffScope::All => args.push("HEAD".to_owned()),
            DiffScope::Unstaged => {}
        }
        args.push("--".to_owned());
        let output = self.run_owned(args)?;
        Ok(String::from_utf8_lossy(&output).into_owned())
    }

    fn diff_files(&self, scope: DiffScope) -> Result<Vec<DiffFile>, GitError> {
        let mut args = vec![
            "diff".to_owned(),
            "--name-status".to_owned(),
            "-z".to_owned(),
        ];
        match scope {
            DiffScope::Staged => args.push("--cached".to_owned()),
            DiffScope::All => args.push("HEAD".to_owned()),
            DiffScope::Unstaged => {}
        }
        args.push("--".to_owned());
        let output = self.run_owned(args)?;
        parse_name_status(&output)
    }

    fn run<const N: usize>(&self, args: [&str; N]) -> Result<Vec<u8>, GitError> {
        self.run_owned(args.iter().map(|arg| (*arg).to_owned()).collect())
    }

    fn run_owned(&self, args: Vec<String>) -> Result<Vec<u8>, GitError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args.iter().map(OsStr::new))
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => GitError::GitMissing(error),
                _ => GitError::Io(error),
            })?;

        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(GitError::Command {
            command: args.join(" "),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn split_nul(bytes: &[u8]) -> impl Iterator<Item = &str> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .filter_map(|field| std::str::from_utf8(field).ok())
}

fn parse_status(bytes: &[u8]) -> Result<Status, GitError> {
    let mut status = Status::default();
    let fields: Vec<&str> = split_nul(bytes).collect();
    let mut index = 0;

    while index < fields.len() {
        let field = fields[index];
        index += 1;

        if let Some(header) = field.strip_prefix("# ") {
            if let Some(value) = header.strip_prefix("branch.oid ") {
                if value != "(initial)" {
                    status.head = Some(value.to_owned());
                }
            } else if let Some(value) = header.strip_prefix("branch.head ") {
                if value != "(detached)" {
                    status.branch = Some(value.to_owned());
                }
            } else if let Some(value) = header.strip_prefix("branch.upstream ") {
                status.upstream = Some(value.to_owned());
            } else if let Some(value) = header.strip_prefix("branch.ab ") {
                let (ahead, behind) = value.split_once(' ').ok_or_else(|| GitError::Parse {
                    what: "status branch.ab",
                    reason: format!("expected `+N -M`, got {value:?}"),
                })?;
                status.ahead = ahead
                    .strip_prefix('+')
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| GitError::Parse {
                        what: "status ahead",
                        reason: value.to_owned(),
                    })?;
                status.behind = behind
                    .strip_prefix('-')
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| GitError::Parse {
                        what: "status behind",
                        reason: value.to_owned(),
                    })?;
            }
            continue;
        }

        match field.as_bytes().first() {
            Some(b'1') => {
                let mut parts = field.splitn(9, ' ');
                let _tag = parts.next();
                let xy = parts.next().unwrap_or_default();
                let _sub = parts.next();
                let _mode_head = parts.next();
                let _mode_index = parts.next();
                let _mode_worktree = parts.next();
                let _hash_head = parts.next();
                let _hash_index = parts.next();
                let path = parts.next().ok_or_else(|| GitError::Parse {
                    what: "status entry",
                    reason: field.to_owned(),
                })?;
                status.files.push(entry(path, None, xy)?);
            }
            Some(b'2') => {
                let mut parts = field.splitn(10, ' ');
                let _tag = parts.next();
                let xy = parts.next().unwrap_or_default();
                let _sub = parts.next();
                let _mode_head = parts.next();
                let _mode_index = parts.next();
                let _mode_worktree = parts.next();
                let _hash_head = parts.next();
                let _hash_index = parts.next();
                let _score = parts.next();
                let path = parts.next().ok_or_else(|| GitError::Parse {
                    what: "rename entry",
                    reason: field.to_owned(),
                })?;
                let orig = fields.get(index).ok_or_else(|| GitError::Parse {
                    what: "rename source",
                    reason: field.to_owned(),
                })?;
                index += 1;
                status
                    .files
                    .push(entry(path, Some((*orig).to_owned()), xy)?);
            }
            Some(b'u') => {
                let mut parts = field.splitn(11, ' ');
                let _tag = parts.next();
                let xy = parts.next().unwrap_or_default();
                for _ in 0..8 {
                    let _ = parts.next();
                }
                let path = parts.next().ok_or_else(|| GitError::Parse {
                    what: "unmerged entry",
                    reason: field.to_owned(),
                })?;
                status.files.push(entry(path, None, xy)?);
            }
            Some(b'?') => {
                let path = field.strip_prefix("? ").unwrap_or_default();
                status.files.push(StatusEntry {
                    path: path.to_owned(),
                    orig_path: None,
                    index: ChangeKind::Unmodified,
                    worktree: ChangeKind::Untracked,
                });
            }
            Some(b'!') => {
                let path = field.strip_prefix("! ").unwrap_or_default();
                status.files.push(StatusEntry {
                    path: path.to_owned(),
                    orig_path: None,
                    index: ChangeKind::Ignored,
                    worktree: ChangeKind::Ignored,
                });
            }
            // Unknown records are ignored rather than failing the whole pane.
            _ => {}
        }
    }

    Ok(status)
}

fn entry(path: &str, orig_path: Option<String>, xy: &str) -> Result<StatusEntry, GitError> {
    let mut chars = xy.chars();
    let index = chars.next().ok_or_else(|| GitError::Parse {
        what: "status xy",
        reason: xy.to_owned(),
    })?;
    let worktree = chars.next().ok_or_else(|| GitError::Parse {
        what: "status xy",
        reason: xy.to_owned(),
    })?;
    Ok(StatusEntry {
        path: path.to_owned(),
        orig_path,
        index: change_kind(index),
        worktree: change_kind(worktree),
    })
}

fn change_kind(code: char) -> ChangeKind {
    match code {
        'M' => ChangeKind::Modified,
        'A' => ChangeKind::Added,
        'D' => ChangeKind::Deleted,
        'R' => ChangeKind::Renamed,
        'C' => ChangeKind::Copied,
        'T' => ChangeKind::TypeChanged,
        'U' => ChangeKind::Unmerged,
        '?' => ChangeKind::Untracked,
        '!' => ChangeKind::Ignored,
        _ => ChangeKind::Unmodified,
    }
}

fn parse_name_status(bytes: &[u8]) -> Result<Vec<DiffFile>, GitError> {
    let fields: Vec<&str> = split_nul(bytes).collect();
    let mut files = Vec::new();
    let mut index = 0;

    while index < fields.len() {
        let status = fields[index];
        index += 1;
        let change = status
            .chars()
            .next()
            .map(change_kind)
            .unwrap_or(ChangeKind::Unmodified);

        if matches!(change, ChangeKind::Renamed | ChangeKind::Copied) {
            let old = fields.get(index).ok_or_else(|| GitError::Parse {
                what: "diff rename",
                reason: status.to_owned(),
            })?;
            let new = fields.get(index + 1).ok_or_else(|| GitError::Parse {
                what: "diff rename",
                reason: status.to_owned(),
            })?;
            index += 2;
            files.push(DiffFile {
                path: (*new).to_owned(),
                old_path: Some((*old).to_owned()),
                change,
            });
        } else {
            let path = fields.get(index).ok_or_else(|| GitError::Parse {
                what: "diff path",
                reason: status.to_owned(),
            })?;
            index += 1;
            files.push(DiffFile {
                path: (*path).to_owned(),
                old_path: None,
                change,
            });
        }
    }

    Ok(files)
}

fn parse_log(bytes: &[u8]) -> Result<Vec<Commit>, GitError> {
    let text = String::from_utf8_lossy(bytes);
    let mut commits = Vec::new();

    for record in text.split('\u{1e}') {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split('\u{1f}').collect();
        if fields.len() != 6 {
            return Err(GitError::Parse {
                what: "log record",
                reason: format!("expected 6 fields, got {}", fields.len()),
            });
        }
        commits.push(Commit {
            id: fields[0].to_owned(),
            parents: fields[1].split_whitespace().map(str::to_owned).collect(),
            author: fields[2].to_owned(),
            email: fields[3].to_owned(),
            authored_at: fields[4].to_owned(),
            subject: fields[5].to_owned(),
        });
    }

    Ok(commits)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "acpilot-git-{}-{}-{name}",
                std::process::id(),
                id
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("temp dir");
            let fixture = Self { root };
            fixture.git(&["init", "-q", "-b", "main"]);
            fixture.git(&["config", "user.email", "test@example.com"]);
            fixture.git(&["config", "user.name", "Test User"]);
            fixture
        }

        fn repo(&self) -> Repo {
            Repo::discover(&self.root).expect("discover")
        }

        fn git(&self, args: &[&str]) -> String {
            let mut command = Command::new("git");
            command.arg("-C").arg(&self.root).args(args);
            // Git exports the author and committer identity to hooks. Without
            // this, the suite commits as whoever is committing and only fails
            // when run from the pre-commit hook.
            for key in [
                "GIT_AUTHOR_NAME",
                "GIT_AUTHOR_EMAIL",
                "GIT_AUTHOR_DATE",
                "GIT_COMMITTER_NAME",
                "GIT_COMMITTER_EMAIL",
                "GIT_COMMITTER_DATE",
            ] {
                command.env_remove(key);
            }
            let output = command.output().expect("run git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn write(&self, path: &str, contents: &str) {
            let path = self.root.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            fs::write(path, contents).expect("write");
        }

        fn commit(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn status_reports_branch_and_both_sides_of_a_change() {
        let fixture = Fixture::new("status");
        fixture.write("one.txt", "one\n");
        fixture.write("two.txt", "two\n");
        fixture.commit("init");

        fixture.write("one.txt", "one changed\n");
        fixture.write("two.txt", "two changed\n");
        fixture.git(&["add", "two.txt"]);
        fixture.write("three.txt", "three\n");

        let status = fixture.repo().status().expect("status");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(status.head.is_some());

        let one = status
            .files
            .iter()
            .find(|entry| entry.path == "one.txt")
            .expect("one.txt");
        assert_eq!(one.index, ChangeKind::Unmodified);
        assert_eq!(one.worktree, ChangeKind::Modified);
        assert!(!one.is_staged());
        assert!(one.is_unstaged());

        let two = status
            .files
            .iter()
            .find(|entry| entry.path == "two.txt")
            .expect("two.txt");
        assert_eq!(two.index, ChangeKind::Modified);
        assert_eq!(two.worktree, ChangeKind::Unmodified);
        assert!(two.is_staged());
        assert!(!two.is_unstaged());

        let three = status
            .files
            .iter()
            .find(|entry| entry.path == "three.txt")
            .expect("three.txt");
        assert_eq!(three.worktree, ChangeKind::Untracked);
    }

    #[test]
    fn staged_rename_keeps_its_old_path() {
        let fixture = Fixture::new("rename");
        fixture.write("old.txt", "content\n");
        fixture.commit("init");
        fixture.git(&["mv", "old.txt", "new.txt"]);

        let status = fixture.repo().status().expect("status");
        let renamed = status
            .files
            .iter()
            .find(|entry| entry.path == "new.txt")
            .expect("new.txt");
        assert_eq!(renamed.index, ChangeKind::Renamed);
        assert_eq!(renamed.orig_path.as_deref(), Some("old.txt"));
        assert!(renamed.is_staged());

        let diff = fixture.repo().diff(DiffScope::Staged).expect("diff");
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "new.txt");
        assert_eq!(diff.files[0].old_path.as_deref(), Some("old.txt"));
        assert_eq!(diff.files[0].change, ChangeKind::Renamed);
    }

    #[test]
    fn diffs_are_split_by_scope_and_carry_a_patch() {
        let fixture = Fixture::new("diff");
        fixture.write("one.txt", "one\n");
        fixture.commit("init");

        fixture.write("one.txt", "one\nstaged\n");
        fixture.git(&["add", "one.txt"]);
        fixture.write("one.txt", "one\nstaged\nunstaged\n");

        let repo = fixture.repo();
        let staged = repo.diff(DiffScope::Staged).expect("staged");
        assert!(staged.patch.contains("+staged"), "{}", staged.patch);
        assert!(!staged.patch.contains("+unstaged"), "{}", staged.patch);
        assert_eq!(staged.files[0].path, "one.txt");
        assert_eq!(staged.files[0].change, ChangeKind::Modified);

        let unstaged = repo.diff(DiffScope::Unstaged).expect("unstaged");
        assert!(unstaged.patch.contains("+unstaged"), "{}", unstaged.patch);
        assert!(!unstaged.patch.contains("+staged"), "{}", unstaged.patch);

        let all = repo.diff(DiffScope::All).expect("all");
        assert!(all.patch.contains("+staged"), "{}", all.patch);
        assert!(all.patch.contains("+unstaged"), "{}", all.patch);
    }

    #[test]
    fn tree_lists_tracked_and_untracked_but_not_ignored() {
        let fixture = Fixture::new("tree");
        fixture.write(".gitignore", "*.log\n");
        fixture.write("src/lib.rs", "fn main() {}\n");
        fixture.commit("init");
        fixture.write("notes.txt", "notes\n");
        fixture.write("debug.log", "ignored\n");

        let tree = fixture.repo().tree().expect("tree");
        let paths: Vec<&str> = tree.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, [".gitignore", "notes.txt", "src/lib.rs"]);
        assert!(tree[0].tracked);
        assert!(!tree[1].tracked);
        assert!(tree[2].tracked);
    }

    #[test]
    fn log_is_newest_first_and_links_parents() {
        let fixture = Fixture::new("log");
        fixture.write("one.txt", "one\n");
        fixture.commit("first commit");
        fixture.write("one.txt", "two\n");
        fixture.commit("second commit");

        let commits = fixture.repo().log(10).expect("log");
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "second commit");
        assert_eq!(commits[1].subject, "first commit");
        assert_eq!(commits[0].parents, [commits[1].id.clone()]);
        assert!(commits[1].parents.is_empty());
        assert_eq!(commits[0].author, "Test User");
        assert!(commits[0].authored_at.contains('T'));
    }

    #[test]
    fn an_unborn_repository_has_no_log_and_still_diffs_what_is_staged() {
        let fixture = Fixture::new("unborn");
        fixture.write("new.txt", "new\n");
        fixture.git(&["add", "new.txt"]);

        let repo = fixture.repo();
        let status = repo.status().expect("status");
        assert!(status.head.is_none());
        assert_eq!(status.files[0].path, "new.txt");
        assert!(status.files[0].is_staged());

        assert!(repo.log(10).expect("log").is_empty());

        let diff = repo.diff(DiffScope::All).expect("all in an unborn repo");
        assert!(diff.patch.contains("+new"), "{}", diff.patch);
        assert_eq!(diff.files[0].path, "new.txt");
        assert_eq!(diff.files[0].change, ChangeKind::Added);
    }

    #[test]
    fn discovery_starts_in_a_subdirectory() {
        let fixture = Fixture::new("discover");
        fixture.write("src/lib.rs", "fn main() {}\n");
        fixture.commit("init");

        let repo = Repo::discover(fixture.root.join("src")).expect("discover from src");
        assert_eq!(
            fs::canonicalize(repo.root()).expect("root"),
            fs::canonicalize(&fixture.root).expect("fixture"),
        );
    }

    #[test]
    fn a_plain_directory_is_not_a_repository() {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("acpilot-git-{}-{id}-plain", std::process::id()));
        fs::create_dir_all(&path).expect("dir");
        let error = Repo::discover(&path).expect_err("not a repo");
        assert!(matches!(error, GitError::NotARepository { .. }), "{error}");
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn status_parses_ahead_behind_and_upstream() {
        let fixture = Fixture::new("upstream");
        fixture.write("one.txt", "one\n");
        fixture.commit("init");

        // A branch whose upstream exists and is one commit behind.
        fixture.git(&["checkout", "-q", "-b", "topic"]);
        fixture.write("topic.txt", "topic\n");
        fixture.commit("topic commit");
        fixture.git(&["branch", "--set-upstream-to=main", "topic"]);

        let status = fixture.repo().status().expect("status");
        assert_eq!(status.branch.as_deref(), Some("topic"));
        assert_eq!(status.upstream.as_deref(), Some("main"));
        assert_eq!(status.ahead, 1);
        assert_eq!(status.behind, 0);
    }

    /// A patch as `git diff` writes one, for the parser's own tests.
    fn patch(path: &str, hunk: &str) -> String {
        format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n{hunk}")
    }

    #[test]
    fn a_patch_marks_the_lines_it_adds_and_the_lines_it_changes() {
        let patch = patch(
            "app.rs",
            "@@ -1,4 +1,6 @@\n line one\n-old two\n+new two\n+extra\n line three\n line four\n",
        );
        let changes = line_changes(&patch, "app.rs");
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert_eq!(
            changes.get(1),
            Some(LineChange::Modified),
            "a removal and an addition at one place is one change"
        );
        assert_eq!(changes.get(2), Some(LineChange::Added));
        assert_eq!(changes.get(0), None);
        assert!(!changes.is_empty());
    }

    #[test]
    fn a_removal_with_nothing_added_marks_the_line_above_the_gap() {
        let patch = patch("app.rs", "@@ -2,3 +2,2 @@\n line two\n-gone\n line four\n");
        let changes = line_changes(&patch, "app.rs");
        assert_eq!(changes.get(1), Some(LineChange::Deleted), "the seam");
        assert_eq!(changes.len(), 1, "{changes:?}");
    }

    #[test]
    fn only_the_file_asked_about_is_read() {
        let both = format!(
            "{}{}",
            patch("one.rs", "@@ -1,1 +1,2 @@\n a\n+b\n"),
            patch("two.rs", "@@ -1,1 +3,2 @@\n c\n+d\n"),
        );
        let one = line_changes(&both, "one.rs");
        assert_eq!(one.get(1), Some(LineChange::Added));
        assert_eq!(one.len(), 1, "the other file's hunk is not ours");
        let two = line_changes(&both, "two.rs");
        assert_eq!(two.get(3), Some(LineChange::Added), "its own line numbers");
        assert!(line_changes(&both, "three.rs").is_empty());
    }

    #[test]
    fn a_bare_hunk_is_taken_to_be_the_file_asked_about() {
        let changes = line_changes("@@ -1,1 +5,2 @@\n a\n+b\n", "whatever.rs");
        assert_eq!(
            changes.get(5),
            Some(LineChange::Added),
            "a patch with no file header has one file"
        );
    }

    #[test]
    fn the_gutter_reads_the_lines_the_working_tree_changed() {
        let fixture = Fixture::new("lines");
        fixture.write("app.rs", "one\ntwo\nthree\nfour\n");
        fixture.commit("init");

        fixture.write("app.rs", "one\ntwo changed\nthree\nafter\nfour\n");
        let changes = fixture.repo().line_changes("app.rs").expect("changes");
        assert_eq!(changes.get(1), Some(LineChange::Modified), "{changes:?}");
        assert_eq!(changes.get(3), Some(LineChange::Added), "{changes:?}");
        assert_eq!(changes.get(0), None);

        // The staged half counts too: the gutter shows the file against HEAD, not
        // against whichever half of the edit git is holding.
        fixture.git(&["add", "app.rs"]);
        let staged = fixture.repo().line_changes("app.rs").expect("changes");
        assert_eq!(staged, changes);

        // A file git has never seen has no line diff, and says so by being empty.
        fixture.write("new.rs", "a\nb\n");
        assert!(
            fixture
                .repo()
                .line_changes("new.rs")
                .expect("changes")
                .is_empty()
        );
    }

    #[test]
    fn before_the_first_commit_the_index_is_the_baseline() {
        let fixture = Fixture::new("no-head");
        fixture.write("app.rs", "one\ntwo\n");
        fixture.git(&["add", "app.rs"]);

        let changes = fixture.repo().line_changes("app.rs").expect("changes");
        assert_eq!(changes.get(0), Some(LineChange::Added));
        assert_eq!(changes.get(1), Some(LineChange::Added));
        assert_eq!(changes.len(), 2, "{changes:?}");
    }
}
