//! The editor pane's seed, and the diff review that hangs off it.
//!
//! The lines are the world the design is argued with; the *buffer* is real. Since
//! P6-4 the TUI's editor pane builds an `acpilot_text::Document` from these lines,
//! parses it with the language's grammar and draws the result, so what is fixed
//! here is only what a file has before it is read: a path, a language, lines with
//! a per-line git marker, and a cursor. Reading a real file is P6-6.

/// The buffer the editor pane draws.
#[derive(Debug, Clone)]
pub struct EditorBuffer {
    /// Path relative to the workspace root, shown in the tab.
    pub path: String,
    /// Language id, as a grammar registry would name it (P6-2).
    pub language: String,
    /// The lines, in order, without their newlines.
    ///
    /// Text only: which of them changed is not the buffer's business, it is
    /// git's. The gutter's markers come from [`Diff::patch`] through
    /// `acpilot_git::line_changes`, so the diff the permission card shows and the
    /// marks beside the code cannot disagree about what the agent did.
    pub lines: Vec<String>,
    /// Where the cursor sits: line, then column, both zero-based.
    pub cursor: (usize, usize),
}

impl EditorBuffer {
    /// The tab's label: the file's own name.
    #[must_use]
    pub fn tab(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// The cursor as a person counts: line 1, column 1.
    #[must_use]
    pub fn position(&self) -> (usize, usize) {
        (self.cursor.0 + 1, self.cursor.1 + 1)
    }
}

/// The file the buffer and the diff are both about.
const DIFF_PATH: &str = "crates/acpilot-tui/src/app.rs";

/// What a diff does to a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// Unchanged, kept for context.
    Context,
    /// Added by the agent.
    Added,
    /// Removed by the agent.
    Removed,
}

/// One line of a diff.
#[derive(Debug, Clone)]
pub struct DiffLine {
    /// What the line is.
    pub kind: DiffKind,
    /// Its text, without the `+`/`-` marker the renderer draws.
    pub text: String,
}

/// A hunk: one contiguous change with the context around it.
#[derive(Debug, Clone)]
pub struct Hunk {
    /// The `@@` header, which is the only part of a diff that says where it lands.
    pub header: String,
    /// The lines, in file order.
    pub lines: Vec<DiffLine>,
}

/// The diff an agent's write would make.
#[derive(Debug, Clone)]
pub struct Diff {
    /// The file it changes.
    pub path: String,
    /// The hunks, in file order.
    pub hunks: Vec<Hunk>,
}

impl Diff {
    /// The hunks as unified diff text, the way `git diff` writes one.
    ///
    /// Rendered rather than stored, because the hunks are what the review draws
    /// and the patch is what a parser reads: `acpilot_git::line_changes` turns
    /// this back into the lines that changed, so the gutter is derived from the
    /// diff instead of from a second list of line numbers that could drift.
    #[must_use]
    pub fn patch(&self) -> String {
        let mut patch = format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n",
            path = self.path
        );
        for hunk in &self.hunks {
            patch.push_str(&hunk.header);
            patch.push('\n');
            for line in &hunk.lines {
                let sigil = match line.kind {
                    DiffKind::Context => ' ',
                    DiffKind::Added => '+',
                    DiffKind::Removed => '-',
                };
                patch.push(sigil);
                patch.push_str(&line.text);
                patch.push('\n');
            }
        }
        patch
    }
}

/// The buffer the design is argued with: the pane code it is about.
#[must_use]
pub fn buffer() -> EditorBuffer {
    let lines = [
        "use acpilot_proto::terminal::{TerminalSize, TerminalSummary};",
        "",
        "/// How many lines of history a pane keeps behind its live screen.",
        "const SCROLLBACK: usize = 10_000;",
        "",
        "/// Whether a pane is following the program or being read.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub enum PaneMode {",
        "    Live,",
        "    Copy,",
        "}",
        "",
        "impl TerminalPane {",
        "    /// Move the copy cursor, scrolling when it reaches an edge.",
        "    fn copy_move(&mut self, rows: i32, cols: i32) {",
        "        let (height, width) = self.grid_size();",
        "        let (mut row, mut col) = self.cursor;",
        "        if rows < 0 && row == 0 && self.scroll < self.scrollable() {",
        "            self.set_scroll(self.scroll + 1);",
        "        } else {",
        "            row = row.saturating_sub(1);",
        "        }",
        "        self.cursor = (row.min(last_row), col.min(last_col));",
        "    }",
        "}",
    ];
    EditorBuffer {
        path: DIFF_PATH.to_owned(),
        language: "rust".to_owned(),
        lines: lines.iter().map(|line| (*line).to_owned()).collect(),
        cursor: (16, 8),
    }
}

/// The diff for the same file the permission asks about.
///
/// Its hunks are what the editor's gutter marks: the same edit, seen twice.
#[must_use]
pub fn diff() -> Diff {
    Diff {
        path: DIFF_PATH.to_owned(),
        hunks: vec![
            Hunk {
                header: "@@ -8,6 +8,12 @@ pub struct TerminalPane {".to_owned(),
                lines: vec![
                    line(DiffKind::Context, "    pub exit: Option<Option<i32>>,"),
                    line(
                        DiffKind::Added,
                        "    /// Following the program, or reading behind it.",
                    ),
                    line(DiffKind::Added, "    pub mode: PaneMode,"),
                    line(
                        DiffKind::Added,
                        "    /// How many lines behind the live screen the view sits.",
                    ),
                    line(DiffKind::Added, "    pub scroll: usize,"),
                    line(DiffKind::Context, "}"),
                ],
            },
            Hunk {
                header: "@@ -126,7 +132,9 @@ impl TerminalPane {".to_owned(),
                lines: vec![
                    line(
                        DiffKind::Context,
                        "    fn set_scroll(&mut self, offset: usize) {",
                    ),
                    line(DiffKind::Removed, "        self.scroll = offset;"),
                    line(
                        DiffKind::Added,
                        "        self.scroll = offset.min(self.history());",
                    ),
                    line(
                        DiffKind::Added,
                        "        self.parser.screen_mut().set_scrollback(self.scroll);",
                    ),
                    line(DiffKind::Context, "    }"),
                ],
            },
        ],
    }
}

fn line(kind: DiffKind, text: &str) -> DiffLine {
    DiffLine {
        kind,
        text: text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_counts_like_an_editor_and_labels_like_a_tab() {
        let buffer = buffer();
        assert_eq!(buffer.tab(), "app.rs");
        assert_eq!(buffer.position(), (17, 9));
        assert!(buffer.lines.iter().any(|line| line.contains("copy_move")));
        assert!(
            buffer.lines.iter().all(|line| !line.contains('\n')),
            "a line of the buffer is a line, not a file"
        );
    }

    #[test]
    fn the_diff_renders_as_a_patch_a_parser_can_read() {
        let diff = diff();
        let patch = diff.patch();
        assert!(
            patch.starts_with("diff --git a/crates/acpilot-tui/src/app.rs"),
            "{patch}"
        );
        assert!(patch.contains("+++ b/crates/acpilot-tui/src/app.rs"));
        assert!(patch.contains("@@ -8,6 +8,12 @@ pub struct TerminalPane {"));
        assert!(patch.contains("+    pub mode: PaneMode,\n"), "{patch}");
        assert!(
            patch.contains("-        self.scroll = offset;\n"),
            "{patch}"
        );
        assert!(
            patch
                .lines()
                .any(|line| line == "     pub exit: Option<Option<i32>>,"),
            "context keeps its leading space: {patch}"
        );
    }

    #[test]
    fn a_diff_has_both_directions_of_change() {
        let diff = diff();
        let kinds: Vec<DiffKind> = diff
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter().map(|line| line.kind))
            .collect();
        assert!(kinds.contains(&DiffKind::Added));
        assert!(kinds.contains(&DiffKind::Removed));
        assert!(kinds.contains(&DiffKind::Context));
    }
}
