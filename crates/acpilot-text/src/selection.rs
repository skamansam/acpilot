//! Cursors, selections, and the motions that move them.
//!
//! A cursor is one char offset; a selection is an anchor that stays where the
//! drag started and a head that follows the caret; a set of them is every place
//! a keystroke lands. Keeping all three here — rather than in the pane that
//! draws them — is what makes multi-cursor editing one code path instead of one
//! per frontend, and it is why this module has to decide the questions a pane
//! would otherwise answer differently in each of three UIs:
//!
//! - **Where the ends of a line are.** The end is before the line break, so End
//!   followed by typing inserts before the newline rather than on the next line.
//! - **What a word is.** Alphanumeric or `_` ([`crate::buffer::is_word`]), which
//!   is the same answer the undo history uses to decide where a step ends.
//! - **What happens when two carets collide.** Selections that overlap or touch
//!   are merged, because two replacements over one range is text corruption and
//!   not a rendering question.

use std::ops::Range;

use crate::buffer::{Buffer, LineColumn, is_word};
use crate::edit::Replacement;

/// A caret: a position between two characters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor {
    /// The char offset the caret sits at.
    pub at: usize,
}

impl Cursor {
    /// A caret at an offset.
    #[must_use]
    pub const fn new(at: usize) -> Self {
        Self { at }
    }

    /// The same caret, moved inside `0..=len`.
    #[must_use]
    pub fn clamped(self, len: usize) -> Self {
        Self {
            at: self.at.min(len),
        }
    }
}

/// A selection: where it was anchored, and where the caret is now.
///
/// An empty selection is a caret. Direction is preserved (`anchor > head` means
/// the caret is to the left of where the selection started) so that extending a
/// selection with a motion key keeps growing the side it was growing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    /// The end that does not move while a selection is extended.
    pub anchor: Cursor,
    /// The end the caret is at.
    pub head: Cursor,
}

impl Selection {
    /// A caret with nothing selected.
    #[must_use]
    pub const fn caret(at: usize) -> Self {
        Self {
            anchor: Cursor::new(at),
            head: Cursor::new(at),
        }
    }

    /// A selection from `anchor` to `head`.
    #[must_use]
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self {
            anchor: Cursor::new(anchor),
            head: Cursor::new(head),
        }
    }

    /// Whether nothing is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The characters this selection covers, whichever way it faces.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        let (start, end) = if self.anchor <= self.head {
            (self.anchor.at, self.head.at)
        } else {
            (self.head.at, self.anchor.at)
        };
        start..end
    }

    /// The earlier end.
    #[must_use]
    pub fn start(&self) -> Cursor {
        self.anchor.min(self.head)
    }

    /// The later end.
    #[must_use]
    pub fn end(&self) -> Cursor {
        self.anchor.max(self.head)
    }
}

/// What a motion key asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    /// One character back.
    Left,
    /// One character forward.
    Right,
    /// A line up, keeping the column where it can.
    Up,
    /// A line down, keeping the column where it can.
    Down,
    /// The start of the line, before its indentation.
    LineStart,
    /// The end of the line, before its line break.
    LineEnd,
    /// Backwards to the start of a word.
    WordLeft,
    /// Forwards to the end of a word.
    WordRight,
    /// The start of the buffer.
    FileStart,
    /// The end of the buffer.
    FileEnd,
}

/// Every place a keystroke lands in one buffer, in order and without overlaps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selections {
    items: Vec<Selection>,
    primary: usize,
}

impl Selections {
    /// One caret.
    #[must_use]
    pub fn caret(at: usize) -> Self {
        Self {
            items: vec![Selection::caret(at)],
            primary: 0,
        }
    }

    /// Everything in the buffer, selected.
    #[must_use]
    pub fn all(buffer: &Buffer) -> Self {
        Self {
            items: vec![Selection::new(0, buffer.char_len())],
            primary: 0,
        }
    }

    /// A set of selections, sorted and merged, with the primary one remembered.
    #[must_use]
    pub fn new(items: Vec<Selection>, primary: usize) -> Self {
        let mut selections = Self { items, primary };
        selections.normalize(usize::MAX);
        selections
    }

    /// The selections, in order.
    #[must_use]
    pub fn items(&self) -> &[Selection] {
        &self.items
    }

    /// The selection a bare keystroke applies to.
    ///
    /// There is always at least one: [`Selections::normalize`] refuses to leave
    /// the set empty, and every constructor goes through it or is a single caret.
    #[must_use]
    pub fn primary(&self) -> &Selection {
        let index = self.primary.min(self.items.len().saturating_sub(1));
        &self.items[index]
    }

    /// The indexes of every caret, which is where an insertion goes.
    pub fn cursors(&self) -> impl Iterator<Item = usize> + '_ {
        self.items.iter().map(|selection| selection.head.at)
    }

    /// The ranges every selection covers.
    #[must_use]
    pub fn ranges(&self) -> Vec<Range<usize>> {
        self.items
            .iter()
            .map(Selection::range)
            .filter(|range| !range.is_empty())
            .collect()
    }

    /// Sort, drop carets that sit on each other, and merge what overlaps.
    ///
    /// `len` clamps every position into the buffer; pass [`usize::MAX`] when the
    /// selections were just built from offsets that are already in range.
    ///
    /// Merging is the safety property, not tidiness: two replacements over one
    /// range would apply twice and corrupt the text, so a set that is not
    /// normalised is a set that cannot be edited with.
    pub fn normalize(&mut self, len: usize) {
        if self.items.is_empty() {
            self.items.push(Selection::caret(0));
            self.primary = 0;
            return;
        }
        let primary_head = self
            .items
            .get(self.primary)
            .map_or(0, |selection| selection.head.at);
        self.items
            .iter_mut()
            .for_each(|selection| *selection = clamp(selection, len));
        self.items
            .sort_by_key(|selection| (selection.start(), selection.end()));

        let mut merged: Vec<Selection> = Vec::with_capacity(self.items.len());
        for selection in self.items.drain(..) {
            match merged.last_mut() {
                // Touching ranges are one: nothing between them can be edited
                // apart, and keeping both would mean two carets drawn in the same
                // cell after the next keystroke.
                Some(last) if last.end() >= selection.start() => {
                    *last = Selection::new(last.start().at, last.end().max(selection.end()).at);
                }
                _ => merged.push(selection),
            }
        }
        self.items = merged;
        // Inclusive at both ends: a caret's own range is empty, so an exclusive
        // `contains` would never find the selection it came from.
        self.primary = self
            .items
            .iter()
            .position(|selection| {
                let range = selection.range();
                range.start <= primary_head && primary_head <= range.end
            })
            .unwrap_or(0);
    }

    /// Move every caret by one motion.
    ///
    /// `extend` keeps the anchors, which is what Shift does; without it each
    /// selection collapses to a caret at the new position.
    pub fn move_by(&mut self, motion: Motion, buffer: &Buffer, extend: bool) {
        self.normalize(buffer.char_len());
        for selection in &mut self.items {
            let target = Cursor::new(target(motion, selection.head.at, buffer));
            selection.head = target;
            if !extend {
                selection.anchor = target;
            }
        }
        self.normalize(buffer.char_len());
    }

    /// Move every position through an edit that has just been applied.
    ///
    /// `replacements` must be the ones the buffer applied, ascending and not
    /// overlapping. A position inside a replaced range lands at the end of the
    /// new text, which is what makes the caret follow a typed character and what
    /// stops a stale offset from pointing into the middle of an emoji.
    pub fn map_through(&mut self, replacements: &[Replacement]) {
        for selection in &mut self.items {
            selection.anchor.at = map_offset(selection.anchor.at, replacements);
            selection.head.at = map_offset(selection.head.at, replacements);
        }
        self.items
            .iter_mut()
            .for_each(|selection| *selection = clamp(selection, usize::MAX));
    }
}

/// Move one offset through replacements, in ascending order.
fn map_offset(offset: usize, replacements: &[Replacement]) -> usize {
    let mut mapped = offset as isize;
    for replacement in replacements {
        let start = replacement.range.start as isize;
        let end = replacement.range.end as isize;
        let inserted = replacement.inserted() as isize;
        if (offset as isize) < start {
            break;
        } else if (offset as isize) >= end {
            mapped += inserted - (end - start);
        } else {
            // Inside the replaced range: collapse to the end of what replaced it.
            mapped = mapped - offset as isize + start + inserted;
            break;
        }
    }
    mapped.max(0) as usize
}

/// Keep a selection's ends inside the buffer.
fn clamp(selection: &Selection, len: usize) -> Selection {
    Selection::new(
        selection.anchor.clamped(len).at,
        selection.head.clamped(len).at,
    )
}

/// Where a motion lands from an offset.
fn target(motion: Motion, at: usize, buffer: &Buffer) -> usize {
    let len = buffer.char_len();
    match motion {
        Motion::Left => at.saturating_sub(1),
        Motion::Right => (at + 1).min(len),
        Motion::Up => vertical(at, -1, buffer),
        Motion::Down => vertical(at, 1, buffer),
        Motion::LineStart => buffer.line_range(buffer.line_column(at).line).start,
        Motion::LineEnd => buffer.line_range(buffer.line_column(at).line).end,
        Motion::WordLeft => word_left(at, buffer),
        Motion::WordRight => word_right(at, buffer),
        Motion::FileStart => 0,
        Motion::FileEnd => len,
    }
}

/// A line up or down, keeping the column, or staying put at the first and last
/// lines.
///
/// There is no goal column: a pane that wants one remembers it between motions,
/// because it is a property of a key sequence rather than of a cursor.
fn vertical(at: usize, by: isize, buffer: &Buffer) -> usize {
    let position = buffer.line_column(at);
    let line = position.line as isize + by;
    if line < 0 || line as usize >= buffer.line_count() {
        return at;
    }
    buffer.offset(LineColumn::new(line as usize, position.column))
}

/// Forwards to the end of this word, or the start of the next one when the caret
/// is between words.
fn word_right(at: usize, buffer: &Buffer) -> usize {
    let len = buffer.char_len();
    let mut index = at;
    if buffer
        .char_at(index)
        .is_some_and(|character| !is_word(character))
    {
        while index < len
            && buffer
                .char_at(index)
                .is_some_and(|character| !is_word(character))
        {
            index += 1;
        }
    }
    while index < len && buffer.char_at(index).is_some_and(is_word) {
        index += 1;
    }
    index
}

/// Backwards to the start of this word, or of the one before it when the caret
/// is between words.
fn word_left(at: usize, buffer: &Buffer) -> usize {
    let mut index = at;
    while index > 0
        && buffer
            .char_at(index - 1)
            .is_some_and(|character| !is_word(character))
    {
        index -= 1;
    }
    while index > 0 && buffer.char_at(index - 1).is_some_and(is_word) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer() -> Buffer {
        Buffer::from_text("one two\nthree\n")
    }

    fn at(selections: &Selections) -> Vec<usize> {
        selections.cursors().collect()
    }

    #[test]
    fn a_selection_knows_which_way_it_faces() {
        let forward = Selection::new(1, 4);
        let backward = Selection::new(4, 1);
        assert_eq!(forward.range(), 1..4);
        assert_eq!(backward.range(), 1..4);
        assert_eq!(forward.head.at, 4, "the caret is where the head is");
        assert_eq!(backward.head.at, 1);
        assert!(Selection::caret(3).is_empty());
        assert_eq!(backward.start().at, 1);
        assert_eq!(backward.end().at, 4);
    }

    #[test]
    fn a_normalised_set_is_sorted_and_has_no_two_carets_in_one_place() {
        let mut selections = Selections::new(
            vec![
                Selection::caret(5),
                Selection::caret(1),
                Selection::caret(5),
                Selection::new(8, 9),
            ],
            2,
        );
        assert_eq!(at(&selections), vec![1, 5, 9]);
        assert_eq!(
            selections.primary().head.at,
            5,
            "the primary survived the sort"
        );
        selections.normalize(usize::MAX);
        assert_eq!(
            at(&selections),
            vec![1, 5, 9],
            "normalising twice is the same"
        );
    }

    #[test]
    fn overlapping_selections_become_one() {
        let selections = Selections::new(
            vec![
                Selection::new(2, 5),
                Selection::new(4, 9),
                Selection::caret(20),
            ],
            1,
        );
        assert_eq!(selections.items().len(), 2);
        assert_eq!(selections.items()[0].range(), 2..9);
        assert_eq!(selections.items()[0].head.at, 9);
        assert_eq!(
            selections.primary().range(),
            2..9,
            "the merged one holds the caret"
        );
    }

    #[test]
    fn a_set_that_was_emptied_becomes_one_caret_rather_than_no_selection() {
        let mut selections = Selections {
            items: Vec::new(),
            primary: 0,
        };
        selections.normalize(usize::MAX);
        assert_eq!(at(&selections), vec![0]);
    }

    #[test]
    fn motions_walk_characters_lines_and_words() {
        let buffer = buffer();
        let mut selections = Selections::caret(0);

        selections.move_by(Motion::Right, &buffer, false);
        assert_eq!(at(&selections), vec![1]);
        selections.move_by(Motion::Left, &buffer, false);
        assert_eq!(at(&selections), vec![0]);
        selections.move_by(Motion::Left, &buffer, false);
        assert_eq!(
            at(&selections),
            vec![0],
            "the first character is the end of it"
        );

        selections.move_by(Motion::WordRight, &buffer, false);
        assert_eq!(at(&selections), vec![3], "the end of `one`");
        selections.move_by(Motion::WordRight, &buffer, false);
        assert_eq!(at(&selections), vec![7], "the end of `two`");
        selections.move_by(Motion::WordLeft, &buffer, false);
        assert_eq!(at(&selections), vec![4], "the start of `two`");
        selections.move_by(Motion::WordLeft, &buffer, false);
        assert_eq!(at(&selections), vec![0]);

        selections.move_by(Motion::LineEnd, &buffer, false);
        assert_eq!(at(&selections), vec![7], "before the newline, not after it");
        selections.move_by(Motion::LineStart, &buffer, false);
        assert_eq!(at(&selections), vec![0]);
        selections.move_by(Motion::FileEnd, &buffer, false);
        assert_eq!(at(&selections), vec![14]);
        selections.move_by(Motion::FileStart, &buffer, false);
        assert_eq!(at(&selections), vec![0]);
    }

    #[test]
    fn a_line_motion_keeps_the_column_and_stops_at_the_ends() {
        let buffer = buffer();
        let mut selections = Selections::caret(5);
        selections.move_by(Motion::Down, &buffer, false);
        assert_eq!(at(&selections), vec![13], "column 5 of `three`");
        selections.move_by(Motion::Down, &buffer, false);
        assert_eq!(
            at(&selections),
            vec![14],
            "the last line is empty, so the column clamps into it"
        );
        selections.move_by(Motion::Down, &buffer, false);
        assert_eq!(at(&selections), vec![14], "there is no line 3");
        selections.move_by(Motion::Up, &buffer, false);
        assert_eq!(
            at(&selections),
            vec![8],
            "and back up to the column it has now"
        );
        selections.move_by(Motion::Up, &buffer, false);
        selections.move_by(Motion::Up, &buffer, false);
        assert_eq!(
            at(&selections),
            vec![0],
            "there is no goal column, so going up again stops at the left edge"
        );

        let mut selections = Selections::caret(3);
        selections.move_by(Motion::Down, &buffer, false);
        selections.move_by(Motion::Down, &buffer, false);
        assert_eq!(at(&selections), vec![buffer.char_len()]);
    }

    #[test]
    fn extending_keeps_the_anchor_and_typing_collapses_it() {
        let buffer = buffer();
        let mut selections = Selections::caret(0);
        selections.move_by(Motion::Right, &buffer, true);
        selections.move_by(Motion::Right, &buffer, true);
        assert_eq!(selections.items()[0].range(), 0..2);
        assert_eq!(selections.items()[0].head.at, 2);

        selections.move_by(Motion::Left, &buffer, true);
        assert_eq!(
            selections.items()[0].range(),
            0..1,
            "back over the same ground"
        );

        selections.move_by(Motion::Right, &buffer, false);
        assert_eq!(selections.items()[0].range(), 2..2);
    }

    #[test]
    fn a_position_inside_replaced_text_lands_at_the_end_of_it() {
        let replacement = Replacement::of(2..4, "x");
        assert_eq!(map_offset(1, std::slice::from_ref(&replacement)), 1);
        assert_eq!(map_offset(2, std::slice::from_ref(&replacement)), 3);
        assert_eq!(map_offset(3, std::slice::from_ref(&replacement)), 3);
        assert_eq!(map_offset(4, std::slice::from_ref(&replacement)), 3);
        assert_eq!(map_offset(5, std::slice::from_ref(&replacement)), 4);
    }

    #[test]
    fn several_replacements_shift_in_order() {
        let replacements = [Replacement::of(0..1, "aaa"), Replacement::of(5..6, "")];
        // Nothing moves before the first edit; the second one shifts what follows
        // it, and both shifts add up for an offset past the end of them.
        assert_eq!(map_offset(0, &replacements), 3);
        assert_eq!(
            map_offset(3, &replacements),
            5,
            "before the deletion, so only the insertion counts"
        );
        assert_eq!(map_offset(9, &replacements), 10);
    }

    #[test]
    fn a_typed_character_leaves_the_caret_after_it() {
        let mut selections = Selections::caret(2);
        selections.map_through(&[Replacement::of(2..2, "x")]);
        assert_eq!(at(&selections), vec![3]);
        assert!(selections.items()[0].is_empty());
    }

    #[test]
    fn a_selection_that_covered_what_was_deleted_becomes_a_caret() {
        let mut selections = Selections::new(vec![Selection::new(1, 4)], 0);
        selections.map_through(&[Replacement::of(1..4, "")]);
        assert_eq!(at(&selections), vec![1]);
        assert!(selections.items()[0].is_empty());
    }
}
