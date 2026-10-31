//! The file tree, flattened.
//!
//! A tree is a walk wherever it is read from; what a pane needs is a list it can
//! draw. Each node says how deep it sits and whether a directory's children follow
//! it, so the renderer indents and never recurses — the same shape P6-6 has to
//! produce from `ignore` and the filesystem, and therefore the same shape the
//! design is judged on.

/// A file's relation to the last commit, as the tree decorates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitStatus {
    /// Unchanged since the last commit.
    Clean,
    /// Tracked, with edits.
    Modified,
    /// Staged for the next commit.
    Added,
    /// Not in the index at all.
    Untracked,
    /// Gone from the working tree.
    Deleted,
    /// Merging, and not resolved.
    Conflicted,
}

impl GitStatus {
    /// The letter a gutter or tree row shows, or nothing for a clean file.
    #[must_use]
    pub fn letter(self) -> Option<&'static str> {
        match self {
            Self::Clean => None,
            Self::Modified => Some("M"),
            Self::Added => Some("A"),
            Self::Untracked => Some("?"),
            Self::Deleted => Some("D"),
            Self::Conflicted => Some("!"),
        }
    }
}

/// One row of the tree.
#[derive(Debug, Clone)]
pub struct FileNode {
    /// Path relative to the workspace root, so a row can be opened by name.
    pub path: String,
    /// How many levels deep the row is drawn.
    pub depth: u8,
    /// Whether the row is a directory.
    pub dir: bool,
    /// For a directory, whether its children follow it. Ignored for a file.
    pub expanded: bool,
    /// How it differs from the last commit.
    pub status: GitStatus,
}

impl FileNode {
    /// The entry's own name: the last path component, which is what a row shows.
    #[must_use]
    pub fn label(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// The workspace this design is argued with.
///
/// It is the repository itself: a design review that has to imagine the tree is a
/// review of the screenshot, not of the product.
#[must_use]
pub fn files() -> Vec<FileNode> {
    vec![
        file("Cargo.toml", 0, GitStatus::Modified),
        dir("crates", 0, true),
        dir("crates/acpilot-tui", 1, true),
        file("crates/acpilot-tui/Cargo.toml", 2, GitStatus::Clean),
        dir("crates/acpilot-tui/src", 2, true),
        file("crates/acpilot-tui/src/app.rs", 3, GitStatus::Modified),
        file("crates/acpilot-tui/src/backend.rs", 3, GitStatus::Modified),
        file("crates/acpilot-tui/src/ui.rs", 3, GitStatus::Modified),
        dir("crates/acpilot-tui/tests", 2, false),
        dir("crates/acpilot-demo", 1, false),
        dir("crates/acpilotd", 1, false),
        dir("docs", 0, false),
        file("README.md", 0, GitStatus::Clean),
        file("TODO.md", 0, GitStatus::Untracked),
    ]
}

fn file(path: &str, depth: u8, status: GitStatus) -> FileNode {
    FileNode {
        path: path.to_owned(),
        depth,
        dir: false,
        expanded: false,
        status,
    }
}

fn dir(path: &str, depth: u8, expanded: bool) -> FileNode {
    FileNode {
        path: path.to_owned(),
        depth,
        dir: true,
        expanded,
        status: GitStatus::Clean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_is_labelled_with_its_own_name() {
        let node = file("crates/acpilot-tui/src/ui.rs", 3, GitStatus::Modified);
        assert_eq!(node.label(), "ui.rs");
        assert_eq!(node.status.letter(), Some("M"));
        assert_eq!(GitStatus::Clean.letter(), None, "a clean file says nothing");
    }
}
