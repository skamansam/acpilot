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
}
