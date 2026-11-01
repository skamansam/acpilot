//! The file tree, flattened.
//!
//! A tree is a walk wherever it is read from; what a pane needs is a list it can
//! draw. Each node says how deep it sits and whether a directory's children follow
//! it, so the renderer indents and never recurses — the same shape P6-6 has to
//! produce from `ignore` and the filesystem, and therefore the same shape the
//! design is judged on.

use acpilot_proto::file::FileChange;

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
    /// Whether an ignore rule hides this row, which the tree draws greyed.
    pub ignored: bool,
    /// How it differs from the last commit, or `None` when it does not.
    ///
    /// The wire's own type rather than a second enum with the same five states:
    /// the daemon answers this per row, and a fixture that spelled it differently
    /// would be a design the listing could not produce.
    pub status: Option<FileChange>,
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
        file("Cargo.toml", 0, Some(FileChange::Modified)),
        dir("crates", 0, true),
        dir("crates/acpilot-tui", 1, true),
        file("crates/acpilot-tui/Cargo.toml", 2, None),
        dir("crates/acpilot-tui/src", 2, true),
        file(
            "crates/acpilot-tui/src/app.rs",
            3,
            Some(FileChange::Modified),
        ),
        file(
            "crates/acpilot-tui/src/backend.rs",
            3,
            Some(FileChange::Modified),
        ),
        file(
            "crates/acpilot-tui/src/ui.rs",
            3,
            Some(FileChange::Modified),
        ),
        dir("crates/acpilot-tui/tests", 2, false),
        dir("crates/acpilot-demo", 1, false),
        dir("crates/acpilotd", 1, false),
        dir("docs", 0, false),
        file("README.md", 0, None),
        file("TODO.md", 0, Some(FileChange::Untracked)),
    ]
}

fn file(path: &str, depth: u8, status: Option<FileChange>) -> FileNode {
    FileNode {
        path: path.to_owned(),
        depth,
        dir: false,
        expanded: false,
        ignored: false,
        status,
    }
}

fn dir(path: &str, depth: u8, expanded: bool) -> FileNode {
    FileNode {
        path: path.to_owned(),
        depth,
        dir: true,
        expanded,
        ignored: false,
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_is_labelled_with_its_own_name() {
        let node = file(
            "crates/acpilot-tui/src/ui.rs",
            3,
            Some(FileChange::Modified),
        );
        assert_eq!(node.label(), "ui.rs");
        assert_eq!(
            node.status.map(FileChange::letter),
            Some("M"),
            "the row's letter is the wire type's"
        );
        assert_eq!(
            file("README.md", 0, None).status,
            None,
            "a clean file says nothing"
        );
    }
}
