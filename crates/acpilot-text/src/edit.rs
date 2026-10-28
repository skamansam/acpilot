//! The four edits a buffer performs, each as a set of replacements.
//!
//! One shape for "what changed" is what makes the rest of the crate small:
//! typing a character, deleting a word, replacing a selection and indenting nine
//! lines are all *replacements of a char range with text*, so the buffer has one
//! mutation, the history has one record, and a multi-cursor insert is a longer
//! list rather than a second code path.

use std::ops::Range;

use crate::buffer::Buffer;

/// One replacement of a range of characters with text.
///
/// Ranges are in char offsets, empty for an insertion and with empty `text` for
/// a deletion. A replacement whose range and text are both empty is nothing at
/// all, and [`Edit::replacements`] drops it rather than making its caller ask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replacement {
    /// The characters this takes out.
    pub range: Range<usize>,
    /// The characters it puts in their place.
    pub text: String,
}

impl Replacement {
    /// A replacement of `range` with `text`.
    #[must_use]
    pub fn of(range: Range<usize>, text: impl Into<String>) -> Self {
        Self {
            range,
            text: text.into(),
        }
    }

    /// How many characters this puts in.
    #[must_use]
    pub fn inserted(&self) -> usize {
        self.text.chars().count()
    }

    /// How many characters this takes out.
    #[must_use]
    pub fn removed(&self) -> usize {
        self.range.end - self.range.start
    }

    /// How much the text's length changes by.
    #[must_use]
    pub fn delta(&self) -> isize {
        self.inserted() as isize - self.removed() as isize
    }

    /// Whether this would leave the text exactly as it is.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.range.is_empty() && self.text.is_empty()
    }
}

/// How one indent level is spelled.
///
/// The buffer does not read preferences itself: the tab width and
/// "insert spaces" flag come from the settings cascade (P4-7), and a crate that
/// reached for a config file would be a crate the browser cannot use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndentStyle {
    /// `n` spaces, the usual reading of `tab_width` with `insert_spaces = true`.
    Spaces(usize),
    /// One tab character.
    Tabs,
}

impl IndentStyle {
    /// The characters one indent step inserts.
    #[must_use]
    pub fn unit(self) -> String {
        match self {
            Self::Spaces(width) => " ".repeat(width),
            Self::Tabs => "\t".to_owned(),
        }
    }

    /// How many leading spaces one level is worth.
    fn width(self) -> usize {
        match self {
            // Zero would make outdent impossible rather than a no-op, so a
            // nonsense preference degrades to "one space per level".
            Self::Spaces(width) => width.max(1),
            Self::Tabs => 1,
        }
    }
}

/// Which way an indent edit goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndentMode {
    /// Add one level at the start of every line.
    In,
    /// Take one level off the start of every line, where there is one.
    Out,
}

/// One edit: what a keystroke, a paste or a menu item asks for, before it is
/// turned into replacements against a particular buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Put `text` in at a char offset, moving everything after it along.
    Insert {
        /// Where the text goes.
        at: usize,
        /// What goes there.
        text: String,
    },
    /// Take a range out.
    Delete {
        /// What goes.
        range: Range<usize>,
    },
    /// Take a range out and put text in its place. Deleting and then inserting
    /// is one replacement on purpose: two would be two undo steps and a
    /// selection that moves in between.
    Replace {
        /// What goes.
        range: Range<usize>,
        /// What takes its place.
        text: String,
    },
    /// Add or remove one level of indentation on each line.
    ///
    /// Lines rather than a character range, because Tab on a selection means
    /// "this block", and a caller that converted lines to offsets would decide
    /// for itself whether a selection ending at column zero includes that line.
    Indent {
        /// The zero-based lines, half-open; out of range lines are ignored.
        lines: Range<usize>,
        /// Which way.
        mode: IndentMode,
        /// What one level is.
        style: IndentStyle,
    },
}

impl Edit {
    /// Insert `text` at `at`.
    #[must_use]
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self::Insert {
            at,
            text: text.into(),
        }
    }

    /// Delete `range`.
    #[must_use]
    pub fn delete(range: Range<usize>) -> Self {
        Self::Delete { range }
    }

    /// Replace `range` with `text`.
    #[must_use]
    pub fn replace(range: Range<usize>, text: impl Into<String>) -> Self {
        Self::Replace {
            range,
            text: text.into(),
        }
    }

    /// Indent the given lines one level in or out.
    #[must_use]
    pub fn indent(lines: Range<usize>, mode: IndentMode, style: IndentStyle) -> Self {
        Self::Indent { lines, mode, style }
    }

    /// The replacements this edit is, against `buffer`.
    ///
    /// Ascending, not overlapping, clamped to the buffer, and with the no-ops
    /// removed — which is the contract [`Buffer::apply`] and
    /// [`crate::Change::between`] rely on, so an edit computed against a text
    /// that has since shrunk lands at the end rather than panicking.
    #[must_use]
    pub fn replacements(&self, buffer: &Buffer) -> Vec<Replacement> {
        let len = buffer.char_len();
        let mut out = match self {
            Self::Insert { at, text } => vec![Replacement::of(*at..*at, text.clone())],
            Self::Delete { range } => vec![Replacement::of(range.clone(), String::new())],
            Self::Replace { range, text } => vec![Replacement::of(range.clone(), text.clone())],
            Self::Indent { lines, mode, style } => indentation(buffer, lines, *mode, *style),
        };
        for replacement in &mut out {
            let start = replacement.range.start.min(len);
            let end = replacement.range.end.clamp(start, len);
            replacement.range = start..end;
        }
        out.retain(|replacement| !replacement.is_noop());
        out.sort_by_key(|replacement| replacement.range.start);
        out
    }
}

/// The replacements that indent or outdent a run of lines.
fn indentation(
    buffer: &Buffer,
    lines: &Range<usize>,
    mode: IndentMode,
    style: IndentStyle,
) -> Vec<Replacement> {
    if lines.start >= lines.end {
        return Vec::new();
    }
    let last = lines.end.min(buffer.line_count());
    (lines.start..last)
        .filter_map(|line| {
            let range = buffer.line_range(line);
            match mode {
                // A blank line is indented too: Tab on an empty line is how a
                // person gets somewhere to type, and it leaves no trailing
                // whitespace to clean up because the line was empty.
                IndentMode::In => Some(Replacement::of(range.start..range.start, style.unit())),
                IndentMode::Out => {
                    let remove = outdent_length(buffer, range.start, style);
                    (remove > 0).then(|| Replacement::of(range.start..range.start + remove, ""))
                }
            }
        })
        .collect()
}

/// How many characters come off the line that starts at `start`.
///
/// A tab is one level whatever the style is — a file indented with tabs is not
/// outdented four characters at a time — and a line with no indentation gives
/// nothing back rather than eating the word that starts it.
fn outdent_length(buffer: &Buffer, start: usize, style: IndentStyle) -> usize {
    let mut run = 0;
    while let Some(character) = buffer.char_at(start + run) {
        if character == ' ' || character == '\t' {
            run += 1;
        } else {
            break;
        }
    }
    if run == 0 {
        return 0;
    }
    if buffer.char_at(start) == Some('\t') {
        return 1;
    }
    run.min(style.width())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer() -> Buffer {
        Buffer::from_text("one\ntwo\n\tthree\n    four\n\nfive")
    }

    #[test]
    fn an_insert_is_one_empty_range() {
        assert_eq!(
            Edit::insert(3, "x").replacements(&Buffer::from_text("one")),
            vec![Replacement::of(3..3, "x")]
        );
        assert_eq!(
            Edit::insert(0, "x").replacements(&Buffer::new()),
            vec![Replacement::of(0..0, "x")]
        );
    }

    #[test]
    fn an_edit_against_a_shorter_text_lands_at_the_end() {
        let buffer = Buffer::from_text("abc");
        assert_eq!(
            Edit::insert(99, "x").replacements(&buffer),
            vec![Replacement::of(3..3, "x")]
        );
        assert_eq!(
            Edit::delete(1..99).replacements(&buffer),
            vec![Replacement::of(1..3, "")]
        );
    }

    #[test]
    fn a_reversed_or_empty_range_is_nothing_at_all() {
        let buffer = Buffer::from_text("abc");
        // A reversed range is what a stale selection hands in; it is nothing.
        let (from, to) = (2usize, 1usize);
        assert!(Edit::delete(from..to).replacements(&buffer).is_empty());
        assert!(Edit::delete(1..1).replacements(&buffer).is_empty());
        assert!(Edit::insert(1, "").replacements(&buffer).is_empty());
    }

    #[test]
    fn indenting_adds_one_level_and_skips_no_line_it_was_given() {
        let buffer = Buffer::from_text("one\ntwo\nthree");
        assert_eq!(
            Edit::indent(0..3, IndentMode::In, IndentStyle::Spaces(2)).replacements(&buffer),
            vec![
                Replacement::of(0..0, "  "),
                Replacement::of(4..4, "  "),
                Replacement::of(8..8, "  "),
            ]
        );
        assert_eq!(
            Edit::indent(0..1, IndentMode::In, IndentStyle::Tabs).replacements(&buffer),
            vec![Replacement::of(0..0, "\t")]
        );
    }

    #[test]
    fn a_line_past_the_end_is_not_indented() {
        let buffer = Buffer::from_text("one\n");
        assert_eq!(
            Edit::indent(0..9, IndentMode::In, IndentStyle::Tabs).replacements(&buffer),
            vec![Replacement::of(0..0, "\t"), Replacement::of(4..4, "\t")]
        );
    }

    #[test]
    fn outdenting_takes_one_level_and_never_a_word() {
        let buffer = buffer();
        assert_eq!(
            Edit::indent(0..6, IndentMode::Out, IndentStyle::Spaces(4)).replacements(&buffer),
            vec![
                // the tab on line 2, and four of the spaces on line 3
                Replacement::of(8..9, ""),
                Replacement::of(15..19, ""),
            ],
            "lines with no indentation are left alone"
        );
    }

    #[test]
    fn outdenting_takes_only_one_level_at_a_time() {
        let buffer = Buffer::from_text("        deep");
        assert_eq!(
            Edit::indent(0..1, IndentMode::Out, IndentStyle::Spaces(4)).replacements(&buffer),
            vec![Replacement::of(0..4, "")]
        );
        assert_eq!(
            Edit::indent(0..1, IndentMode::Out, IndentStyle::Spaces(8)).replacements(&buffer),
            vec![Replacement::of(0..8, "")]
        );
    }

    #[test]
    fn a_tab_is_one_level_whatever_the_style_says() {
        let buffer = Buffer::from_text("\t\ttabbed");
        assert_eq!(
            Edit::indent(0..1, IndentMode::Out, IndentStyle::Spaces(4)).replacements(&buffer),
            vec![Replacement::of(0..1, "")]
        );
    }

    #[test]
    fn a_blank_line_outdents_to_nothing_rather_than_eating_its_neighbour() {
        let buffer = Buffer::from_text("one\n    \ntwo");
        assert_eq!(
            Edit::indent(0..3, IndentMode::Out, IndentStyle::Spaces(4)).replacements(&buffer),
            vec![Replacement::of(4..8, "")]
        );
    }

    #[test]
    fn a_nonsense_tab_width_still_outdents_one_space() {
        let buffer = Buffer::from_text("  two");
        assert_eq!(
            Edit::indent(0..1, IndentMode::Out, IndentStyle::Spaces(0)).replacements(&buffer),
            vec![Replacement::of(0..1, "")]
        );
    }
}
