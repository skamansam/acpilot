//! The buffer, the carets and the history as one thing a pane holds.
//!
//! Every path into the text goes through [`Document`], so "the edit landed but
//! the undo history did not hear about it" is not a state this crate can be in.
//! The rules a pane would otherwise get wrong are decided here, once:
//!
//! - Typing with something selected replaces it, which is what every editor
//!   does and the only reading of a keystroke that does not need a special key.
//! - Every caret in the set gets the same treatment, and all of it is one undo
//!   step.
//! - The caret follows the text it typed and is placed at the seam of what undo
//!   removed.
//!
//! Time is a parameter, not a clock read inside, so the 500 ms grouping rule
//! ([`crate::GROUP_WINDOW`]) can be tested by passing the times it is about.

use std::time::Instant;

use crate::buffer::{Buffer, LineEnding, TextError};
use crate::edit::{Edit, IndentMode, IndentStyle, Replacement};
use crate::history::{Change, History};
use crate::selection::Selections;

/// A text buffer with its carets and its undo history.
#[derive(Debug)]
pub struct Document {
    buffer: Buffer,
    selections: Selections,
    history: History,
}

impl Document {
    /// A document over a buffer, with the caret at the start.
    #[must_use]
    pub fn new(buffer: Buffer) -> Self {
        Self {
            buffer,
            selections: Selections::caret(0),
            history: History::new(),
        }
    }

    /// A document over a file's bytes.
    ///
    /// # Errors
    ///
    /// [`TextError::NotUtf8`] when the bytes are not UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TextError> {
        Ok(Self::new(Buffer::from_bytes(bytes)?))
    }

    /// The text and its file shape, for reading.
    #[must_use]
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// The text as it will be written, `\n` throughout.
    #[must_use]
    pub fn text(&self) -> String {
        self.buffer.text()
    }

    /// The bytes to save.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.buffer.to_bytes()
    }

    /// How this document will be written.
    #[must_use]
    pub fn line_ending(&self) -> LineEnding {
        self.buffer.line_ending()
    }

    /// Every place a keystroke lands.
    #[must_use]
    pub fn selections(&self) -> &Selections {
        &self.selections
    }

    /// Move the carets. Offsets past the end are clamped rather than refused: a
    /// click that arrived with a stale offset should put the caret somewhere
    /// sensible.
    pub fn set_selections(&mut self, selections: Selections) {
        let len = self.buffer.char_len();
        self.selections = selections;
        self.selections.normalize(len);
    }

    /// Select everything.
    pub fn select_all(&mut self) {
        self.selections = Selections::all(&self.buffer);
    }

    /// Put one caret at an offset.
    pub fn set_caret(&mut self, at: usize) {
        self.selections = Selections::caret(at.min(self.buffer.char_len()));
    }

    /// Apply one edit, as one undo step when it continues the open one.
    ///
    /// Returns whether anything changed: an edit computed against an older text
    /// can turn out to be nothing at all, and that is not an error.
    pub fn apply_at(&mut self, edit: Edit, now: Instant) -> bool {
        let replacements = edit.replacements(&self.buffer);
        let Some(change) = Change::between(&self.buffer, replacements) else {
            return false;
        };
        self.commit(change, now);
        true
    }

    /// Type `text` at every caret, replacing whatever is selected.
    ///
    /// All the carets are one change, so one Ctrl-Z takes the whole thing back —
    /// undoing half of a multi-cursor edit is not a feature anybody wants.
    pub fn insert(&mut self, text: &str, now: Instant) -> bool {
        if text.is_empty() {
            return false;
        }
        self.selections.normalize(self.buffer.char_len());
        let replacements: Vec<Replacement> = self
            .selections
            .items()
            .iter()
            .map(|selection| Replacement::of(selection.range(), text))
            .collect();
        self.apply_replacements(replacements, now)
    }

    /// Delete the character before every caret, or the selection covering it.
    pub fn delete_backward(&mut self, now: Instant) -> bool {
        self.selections.normalize(self.buffer.char_len());
        let replacements: Vec<Replacement> = self
            .selections
            .items()
            .iter()
            .filter_map(|selection| {
                let range = selection.range();
                if range.is_empty() {
                    let head = selection.head.at;
                    // `then_some` would build the range before asking, and
                    // `head - 1` at the start of the buffer is an underflow.
                    head.checked_sub(1)
                        .map(|before| Replacement::of(before..head, ""))
                } else {
                    Some(Replacement::of(range, ""))
                }
            })
            .collect();
        self.apply_replacements(replacements, now)
    }

    /// Delete the character after every caret, or the selection covering it.
    pub fn delete_forward(&mut self, now: Instant) -> bool {
        self.selections.normalize(self.buffer.char_len());
        let len = self.buffer.char_len();
        let replacements: Vec<Replacement> = self
            .selections
            .items()
            .iter()
            .filter_map(|selection| {
                let range = selection.range();
                if range.is_empty() {
                    let head = selection.head.at;
                    (head < len).then(|| Replacement::of(head..head + 1, ""))
                } else {
                    Some(Replacement::of(range, ""))
                }
            })
            .collect();
        self.apply_replacements(replacements, now)
    }

    /// Indent or outdent every line a caret or selection touches.
    ///
    /// A selection that ends at the start of a line does not drag that line in,
    /// which is the difference between indenting a block and indenting a block
    /// plus whatever follows it.
    pub fn indent(&mut self, mode: IndentMode, style: IndentStyle, now: Instant) -> bool {
        self.selections.normalize(self.buffer.char_len());
        let mut first = usize::MAX;
        let mut last = 0;
        for selection in self.selections.items() {
            let range = selection.range();
            let start = self.buffer.line_column(range.start).line;
            let end_offset = if range.end > range.start {
                range.end - 1
            } else {
                range.end
            };
            first = first.min(start);
            last = last.max(self.buffer.line_column(end_offset).line);
        }
        if first > last {
            return false;
        }
        self.apply_at(Edit::indent(first..last + 1, mode, style), now)
    }

    /// Take the last step back, putting one caret where it removed text from.
    pub fn undo(&mut self) -> bool {
        match self.history.undo(&mut self.buffer) {
            Some(caret) => {
                self.selections = Selections::caret(caret.min(self.buffer.char_len()));
                true
            }
            None => false,
        }
    }

    /// Put the last undone step back.
    pub fn redo(&mut self) -> bool {
        match self.history.redo(&mut self.buffer) {
            Some(caret) => {
                self.selections = Selections::caret(caret.min(self.buffer.char_len()));
                true
            }
            None => false,
        }
    }

    /// Whether there is a step to take back.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// Whether there is a step to put back.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// How many undo steps are on the stack.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.history.depth()
    }

    /// The whole path of a change that is already in a set of replacements.
    fn apply_replacements(&mut self, replacements: Vec<Replacement>, now: Instant) -> bool {
        let Some(change) = Change::between(&self.buffer, replacements) else {
            return false;
        };
        self.commit(change, now);
        true
    }

    /// Write the change into the buffer, move the carets through it and record
    /// it — in that order, because the carets have to be mapped against the text
    /// as it is after the edit and the history has to hold the text as it was
    /// before.
    fn commit(&mut self, change: Change, now: Instant) {
        self.buffer.apply(change.forward());
        self.selections.map_through(change.forward());
        self.selections.normalize(self.buffer.char_len());
        self.history.record(change, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::LineEnding;
    use crate::selection::Selection;
    use proptest::prelude::*;
    use std::time::Duration;

    fn now() -> Instant {
        Instant::now()
    }

    fn document(text: &str) -> Document {
        Document::new(Buffer::from_text(text))
    }

    fn caret(document: &Document) -> usize {
        document.selections().primary().head.at
    }

    #[test]
    fn typing_replaces_what_is_selected() {
        let mut document = document("one two");
        document.set_selections(Selections::new(vec![Selection::new(0, 3)], 0));
        assert!(document.insert("three", now()));
        assert_eq!(document.text(), "three two");
        assert_eq!(caret(&document), 5, "the caret is past what was typed");
        assert!(document.undo());
        assert_eq!(document.text(), "one two");
    }

    #[test]
    fn every_caret_is_one_undo_step() {
        let start = now();
        let mut document = document("one\ntwo");
        document.set_selections(Selections::new(
            vec![Selection::caret(0), Selection::caret(4)],
            0,
        ));
        assert!(document.insert("#", start));
        assert_eq!(document.text(), "#one\n#two");
        assert_eq!(document.undo_depth(), 1, "one step, not two");
        assert_eq!(
            document.selections().cursors().collect::<Vec<_>>(),
            vec![1, 6],
            "each caret followed its own character"
        );
        assert!(document.undo());
        assert_eq!(document.text(), "one\ntwo");
    }

    #[test]
    fn carets_at_the_same_place_are_one_replacement() {
        let mut document = document("one");
        document.set_selections(Selections::new(
            vec![Selection::caret(1), Selection::caret(1)],
            0,
        ));
        assert!(document.insert("x", now()));
        assert_eq!(document.text(), "oxne", "not `oxxne`");
        assert_eq!(document.selections().items().len(), 1);
    }

    #[test]
    fn backspace_at_the_start_of_the_buffer_does_nothing() {
        let mut document = document("one");
        document.set_caret(0);
        assert!(!document.delete_backward(now()));
        assert_eq!(document.text(), "one");
        assert!(!document.can_undo(), "nothing happened, so nothing to undo");

        document.set_caret(3);
        assert!(document.delete_backward(now()));
        assert_eq!(document.text(), "on");
        assert_eq!(caret(&document), 2);
    }

    #[test]
    fn delete_forward_at_the_end_does_nothing_and_a_selection_goes_whole() {
        let mut document = document("one");
        document.set_caret(3);
        assert!(!document.delete_forward(now()));
        assert_eq!(document.text(), "one");

        document.set_selections(Selections::new(vec![Selection::new(0, 3)], 0));
        assert!(document.delete_forward(now()));
        assert_eq!(document.text(), "");
        assert_eq!(caret(&document), 0);
    }

    #[test]
    fn a_word_of_backspaces_is_one_step() {
        let start = now();
        let mut document = document("foo bar");
        document.set_caret(7);
        for index in 0..3 {
            assert!(document.delete_backward(start + Duration::from_millis(index * 100)));
        }
        assert_eq!(document.text(), "foo ");
        assert_eq!(document.undo_depth(), 1);
        assert!(document.undo());
        assert_eq!(document.text(), "foo bar");
        assert_eq!(
            caret(&document),
            6,
            "back at the seam the first backspace removed from"
        );
    }

    #[test]
    fn undo_and_redo_are_what_the_document_did_and_undid() {
        let start = now();
        let mut document = document("");
        assert!(document.insert("hello", start));
        assert!(document.undo());
        assert_eq!(document.text(), "");
        assert!(document.can_redo());
        assert!(document.redo());
        assert_eq!(document.text(), "hello");
        assert_eq!(caret(&document), 5);
        assert!(!document.redo());
        assert!(document.undo(), "the step is still there to take back");
        assert!(!document.can_undo());
        assert!(!document.undo());
    }

    #[test]
    fn indenting_uses_the_lines_the_selections_touch() {
        let mut document = document("one\ntwo\nthree");
        // A caret on line 0 indents line 0 only.
        document.set_caret(1);
        assert!(document.indent(IndentMode::In, IndentStyle::Spaces(2), now()));
        assert_eq!(document.text(), "  one\ntwo\nthree");

        // A selection over lines 1 and 2 indents both...
        document.set_selections(Selections::new(vec![Selection::new(6, 15)], 0));
        assert!(document.indent(IndentMode::In, IndentStyle::Tabs, now()));
        assert_eq!(document.text(), "  one\n\ttwo\n\tthree");

        // ...but one that stops at column zero of the last line does not drag it in.
        document.set_selections(Selections::new(vec![Selection::new(6, 11)], 0));
        assert!(document.indent(IndentMode::Out, IndentStyle::Tabs, now()));
        assert_eq!(document.text(), "  one\ntwo\n\tthree");
    }

    #[test]
    fn select_all_covers_a_text_that_has_since_changed() {
        let mut document = document("one");
        document.select_all();
        assert_eq!(document.selections().ranges(), vec![0..3]);
        document.insert("four", now());
        assert_eq!(document.text(), "four");
        document.select_all();
        document.set_caret(99);
        assert_eq!(caret(&document), 4);
    }

    #[test]
    fn a_documents_file_shape_survives_editing_it() {
        let mut document =
            Document::from_bytes(b"\xEF\xBB\xBFone\r\ntwo\r\n").expect("a Windows file");
        assert_eq!(document.line_ending(), LineEnding::CrLf);
        document.set_caret(3);
        assert!(document.insert("\n", now()));
        assert_eq!(document.text(), "one\n\ntwo\n");
        assert_eq!(document.to_bytes(), b"\xEF\xBB\xBFone\r\n\r\ntwo\r\n");
    }

    fn any_text() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just('\n'),
                Just(' '),
                Just('a'),
                Just('Z'),
                Just('_'),
                Just('é'),
                Just('日'),
                Just('😀'),
                Just('\u{301}'),
                any::<char>(),
            ],
            0..40,
        )
        .prop_map(String::from_iter)
    }

    fn any_op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (any::<u64>(), any_text()).prop_map(|(at, text)| Op::Insert { at, text }),
            (any::<u64>(), any::<u64>()).prop_map(|(from, to)| Op::Delete { from, to }),
            (any::<u64>(), any::<u64>(), any_text()).prop_map(|(from, to, text)| Op::Replace {
                from,
                to,
                text
            }),
            (any::<u64>(), any::<u64>(), any::<bool>()).prop_map(|(from, to, out)| Op::Indent {
                from,
                to,
                out
            }),
            any_text().prop_map(Op::Type),
            (any::<u64>(), any::<u64>()).prop_map(|(left, right)| Op::DeleteMany { left, right }),
        ]
    }

    #[derive(Debug, Clone)]
    enum Op {
        Insert {
            at: u64,
            text: String,
        },
        Delete {
            from: u64,
            to: u64,
        },
        Replace {
            from: u64,
            to: u64,
            text: String,
        },
        Indent {
            from: u64,
            to: u64,
            out: bool,
        },
        /// Typing at every caret, which is the multi-cursor path.
        Type(String),
        /// One backspace at each of up to three carets.
        DeleteMany {
            left: u64,
            right: u64,
        },
    }

    /// Apply one op, saying whether it changed anything.
    fn run(document: &mut Document, op: &Op, when: Instant) -> bool {
        let len = document.buffer().char_len();
        let offset = |value: u64| (value % (len as u64 + 2)) as usize;
        match op {
            Op::Insert { at, text } => {
                document.apply_at(Edit::insert(offset(*at), text.clone()), when)
            }
            Op::Delete { from, to } => {
                document.apply_at(Edit::delete(offset(*from)..offset(*to)), when)
            }
            Op::Replace { from, to, text } => document.apply_at(
                Edit::replace(offset(*from)..offset(*to), text.clone()),
                when,
            ),
            Op::Indent { from, to, out } => {
                let lines = document.buffer().line_count() + 1;
                let lines = (from % lines as u64) as usize..(to % lines as u64) as usize;
                let mode = if *out {
                    IndentMode::Out
                } else {
                    IndentMode::In
                };
                document.apply_at(Edit::indent(lines, mode, IndentStyle::Spaces(4)), when)
            }
            Op::Type(text) => document.insert(text, when),
            Op::DeleteMany { left, right } => {
                let carets = [offset(*left), offset(*right), offset(left ^ right)];
                document.set_selections(Selections::new(
                    carets.iter().map(|at| Selection::caret(*at)).collect(),
                    0,
                ));
                document.delete_backward(when)
            }
        }
    }

    proptest! {
        /// The invariant everything else rests on: what an edit did, undo takes
        /// back exactly, and redo puts it exactly back.
        #[test]
        fn an_edit_and_its_undo_are_the_identity(
            text in any_text(),
            ops in proptest::collection::vec(any_op(), 0..12),
        ) {
            let mut document = Document::new(Buffer::from_text(&text));
            let mut when = now();
            for op in &ops {
                // A gap wider than the grouping window, so every op is a step of
                // its own: undo then has to be the inverse of *this* op. That a
                // run of keystrokes groups into one step is the next test's job.
                when += Duration::from_secs(1);
                let before = document.text();
                if !run(&mut document, op, when) {
                    continue;
                }
                let after = document.text();
                prop_assert!(document.undo(), "an applied edit must be undoable");
                prop_assert_eq!(document.text(), before, "undo did not restore the text");
                prop_assert!(document.redo(), "an undone edit must be redoable");
                prop_assert_eq!(document.text(), after, "redo did not put the edit back");
            }
        }

        /// Undoing every step walks all the way back to the file that was opened.
        #[test]
        fn undoing_everything_is_the_text_that_was_opened(
            text in any_text(),
            ops in proptest::collection::vec(any_op(), 0..12),
        ) {
            let mut document = Document::new(Buffer::from_text(&text));
            // What the buffer holds, not the generated string: `\r` and `\r\n`
            // were normalised on the way in and that is the text undo returns to.
            let opened = document.text();
            let mut when = now();
            let mut applied = 0;
            for op in &ops {
                when += Duration::from_millis(120);
                if run(&mut document, op, when) {
                    applied += 1;
                }
            }
            if applied == 0 {
                return Ok(());
            }
            let depth = document.undo_depth();
            let mut undone = 0;
            while document.undo() {
                undone += 1;
            }
            prop_assert_eq!(undone, depth, "every step came back");
            prop_assert_eq!(document.text(), opened, "the file is as it was opened");
        }

        /// The carets are always inside the text, whatever was clicked.
        #[test]
        fn every_caret_stays_inside_the_text(
            text in any_text(),
            ops in proptest::collection::vec(any_op(), 0..12),
        ) {
            let mut document = Document::new(Buffer::from_text(&text));
            let mut when = now();
            for op in &ops {
                when += Duration::from_millis(120);
                run(&mut document, op, when);
                for selection in document.selections().items() {
                    prop_assert!(selection.head.at <= document.buffer().char_len());
                    prop_assert!(selection.anchor.at <= document.buffer().char_len());
                }
            }
        }

        /// Bytes that are not text are refused, not repaired; bytes that are
        /// text come back byte for byte, and no offset panics on the way.
        #[test]
        fn arbitrary_bytes_are_either_text_or_an_error(
            bytes in proptest::collection::vec(any::<u8>(), 0..80),
            offsets in proptest::collection::vec(any::<usize>(), 0..6),
        ) {
            match Document::from_bytes(&bytes) {
                Ok(document) => {
                    prop_assert!(document.buffer().char_len() <= bytes.len());
                    if !document.buffer().had_mixed_endings() {
                        prop_assert_eq!(document.to_bytes(), bytes, "a consistent file round-trips");
                    }
                    for offset in offsets {
                        // Reading at any offset is the property; the answers are
                        // checked below rather than discarded.
                        let position = document.buffer().line_column(offset);
                        prop_assert_eq!(document.buffer().line_column(offset), position);
                        let _ = document.buffer().slice(offset..offset.saturating_add(3));
                        let line = document.buffer().line(offset);
                        prop_assert!(!line.contains('\n'));
                        prop_assert!(document.buffer().offset(position) <= document.buffer().char_len());
                    }
                }
                Err(error) => {
                    let not_utf8 = matches!(error, TextError::NotUtf8 { .. });
                    prop_assert!(not_utf8, "only decoding may fail");
                }
            }
        }
    }
}
