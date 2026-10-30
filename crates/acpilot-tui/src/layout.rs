//! The widths the dividers were dragged to, remembered between launches.
//!
//! A width is not a setting anyone edits: it is a drag, and doing the same drag on
//! every launch is the small tax that makes a UI feel unfinished. It is still not a
//! *preference*, so it is not written to `config.toml` — that file belongs to the
//! person and may be hand-edited, and a client that rewrote it to remember a column
//! would be editing something it does not own. It goes under the data directory
//! instead, in a file only this client writes and only this client reads.
//!
//! Nothing here is allowed to fail a launch. A missing file is a first launch; a
//! file that does not parse, or one with a width no pane could use, is the widths
//! it does describe — refusing to start over a remembered column would be worse
//! than forgetting it.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The files tree's column, in cells, when nobody has dragged the divider.
pub const DEFAULT_FILES: u16 = 26;

/// The sidebar's column — sessions, transcript, composer — when nobody has.
pub const DEFAULT_SIDEBAR: u16 = 44;

/// The narrowest the tree may be drawn: a row of file names and its marker.
pub const MIN_FILES: u16 = 16;

/// The widest the tree may be drawn, so a drag cannot leave no editor.
pub const MAX_FILES: u16 = 60;

/// The narrowest the sidebar may be drawn: a line of a transcript and its border.
pub const MIN_SIDEBAR: u16 = 24;

/// The widest the sidebar may be drawn.
pub const MAX_SIDEBAR: u16 = 80;

/// The widths of the two dividers, in columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    /// The files tree's column.
    pub files: u16,
    /// The sidebar's column.
    pub sidebar: u16,
}

impl Default for Layout {
    fn default() -> Self {
        Self::new(DEFAULT_FILES, DEFAULT_SIDEBAR)
    }
}

impl Layout {
    /// Two widths, held to what a pane can actually be drawn at.
    ///
    /// The clamp is here rather than at the caller because a width can arrive from
    /// a file somebody typed, and a bound that only the mouse respects is a bound a
    /// hand-edited file walks around.
    #[must_use]
    pub fn new(files: u16, sidebar: u16) -> Self {
        Self {
            files: files.clamp(MIN_FILES, MAX_FILES),
            sidebar: sidebar.clamp(MIN_SIDEBAR, MAX_SIDEBAR),
        }
    }

    /// Read the widths a previous launch left, or the defaults.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<Self>(&text).ok())
            .map_or_else(Self::default, |layout| {
                Self::new(layout.files, layout.sidebar)
            })
    }

    /// Write the widths where the next launch will find them.
    ///
    /// The parent directory is created here because the first launch on a machine
    /// is exactly when it does not exist yet.
    ///
    /// # Errors
    ///
    /// The file or its directory could not be written. The one caller reports it
    /// and carries on: a width that cannot be remembered is not a reason to end a
    /// session.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_toml())
    }

    /// The file's text: two keys and what they mean, so a person who opens it knows
    /// what they are looking at.
    #[must_use]
    pub fn to_toml(&self) -> String {
        format!(
            "# Written by acpilot tui: the widths the dividers were dragged to.\n\
             # Editing this is fine; a width outside the pane's range is clamped.\n\
             files = {}\nsidebar = {}\n",
            self.files, self.sidebar
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_with_no_file_is_the_default_widths() {
        let directory = tempfile::tempdir().expect("temp dir");
        let layout = Layout::load(&directory.path().join("absent.toml"));
        assert_eq!(layout, Layout::default());
        assert_eq!(layout.files, DEFAULT_FILES);
    }

    #[test]
    fn the_widths_a_drag_left_are_the_widths_the_next_launch_opens() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("nested").join("layout.toml");
        let saved = Layout::new(38, 60);
        saved.save(&path).expect("a file");
        assert_eq!(Layout::load(&path), saved);
    }

    #[test]
    fn a_file_that_makes_no_sense_is_the_defaults_rather_than_a_refusal() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("layout.toml");
        std::fs::write(&path, "not = [valid").expect("a file");
        assert_eq!(Layout::load(&path), Layout::default());
    }

    #[test]
    fn a_width_no_pane_could_use_is_clamped_on_the_way_in() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("layout.toml");
        std::fs::write(&path, "files = 4\nsidebar = 500\n").expect("a file");
        let layout = Layout::load(&path);
        assert_eq!(layout.files, MIN_FILES);
        assert_eq!(layout.sidebar, MAX_SIDEBAR);
    }
}
