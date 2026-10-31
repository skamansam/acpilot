//! The rope, and the file's own shape kept beside it.
//!
//! A buffer is the text plus the two things about a file that are not text: how
//! its lines end and whether it starts with a byte-order mark. Holding those as
//! metadata rather than as content is what lets the rest of the crate count
//! characters and lines without a special case for `\r`, and it is what makes
//! saving a Windows file on Linux leave every line ending alone.
//!
//! Every index in this module is a **char offset**. Bytes are available through
//! [`Buffer::rope`] for the callers that genuinely work in bytes — highlighting
//! and LSP document sync — and the conversion to line/UTF-16 columns lives in
//! `acpilot-lsp::position`, against the same text.

use std::ops::Range;

use ropey::Rope;
use thiserror::Error;

use crate::edit::Replacement;

/// The UTF-8 byte-order mark.
const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// How a file's lines end, and therefore how they are written back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineEnding {
    /// `\n`, what the rope itself holds.
    #[default]
    Lf,
    /// `\r\n`.
    CrLf,
    /// A bare `\r`, the classic Mac ending. No longer written by anything, still
    /// read, because a file that exists deserves to survive being opened.
    Cr,
}

impl LineEnding {
    /// What one line break is written as.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }

    /// The ending most of `text` uses.
    ///
    /// The majority rather than the first, so one stray ending in a large file
    /// does not decide the whole file's fate. A tie goes to the ending that
    /// appears first, and text with no line break at all is [`LineEnding::Lf`].
    #[must_use]
    pub fn of(text: &str) -> Self {
        scan(text).ending()
    }
}

/// A zero-based line and a column counted in **characters** from that line's
/// start.
///
/// Not `acpilot_lsp::Position`: that one counts whatever encoding a language
/// server negotiated, where this one counts code points, which is what a pane
/// needs to place a cursor against a grid of cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct LineColumn {
    /// Zero-based line number.
    pub line: usize,
    /// Zero-based character offset within the line.
    pub column: usize,
}

impl LineColumn {
    /// Construct a position.
    #[must_use]
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

/// Whether a character is part of a word: what motion keys step over, and one
/// half of the rule that decides where an undo step ends.
#[must_use]
pub fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Why a file could not become a buffer.
#[derive(Debug, Error)]
pub enum TextError {
    /// The bytes are not UTF-8.
    ///
    /// Refused rather than replaced: the files API already declines binary
    /// content (P3-5), and a buffer that decoded with `U+FFFD` would save the
    /// replacement characters back over the file.
    #[error("not UTF-8 text: the bytes are invalid at offset {offset}")]
    NotUtf8 {
        /// Where decoding stopped, in bytes from the start of the file.
        offset: usize,
    },
}

/// One document's text, with the file's line ending and byte-order mark.
#[derive(Clone, Debug)]
pub struct Buffer {
    rope: Rope,
    line_ending: LineEnding,
    bom: bool,
    mixed_endings: bool,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    /// An empty buffer, as `\n` and no byte-order mark.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rope: Rope::new(),
            line_ending: LineEnding::Lf,
            bom: false,
            mixed_endings: false,
        }
    }

    /// A buffer from text that is already in memory.
    ///
    /// The text's own ending is detected and preserved, so a string with `\r\n`
    /// in it survives a round trip; the rope holds `\n` either way.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        let found = scan(text);
        Self {
            rope: Rope::from_str(&normalise(text)),
            line_ending: found.ending(),
            bom: false,
            mixed_endings: found.mixed(),
        }
    }

    /// A buffer from a file's bytes, byte-order mark and all.
    ///
    /// # Errors
    ///
    /// [`TextError::NotUtf8`] when the bytes are not UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TextError> {
        let (bom, rest) = match bytes.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, bytes),
        };
        let text = std::str::from_utf8(rest).map_err(|error| TextError::NotUtf8 {
            offset: error.valid_up_to(),
        })?;
        let mut buffer = Self::from_text(text);
        buffer.bom = bom;
        Ok(buffer)
    }

    /// The text as it will be written, which is `\n` throughout.
    #[must_use]
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// The bytes to save: the byte-order mark if the file had one, the text in
    /// the file's own line ending.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let text = self.rope.to_string();
        let body = match self.line_ending {
            LineEnding::Lf => text,
            LineEnding::CrLf => text.replace('\n', "\r\n"),
            LineEnding::Cr => text.replace('\n', "\r"),
        };
        let mut out = Vec::with_capacity(body.len() + BOM.len());
        if self.bom {
            out.extend_from_slice(BOM);
        }
        out.extend_from_slice(body.as_bytes());
        out
    }

    /// The rope itself, for callers that index in bytes or slice without
    /// copying. Nothing outside this crate should hold it across an edit: the
    /// rope's identity changes and a stale slice is a wrong answer rather than a
    /// panic.
    #[must_use]
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// The number of characters, which is the upper bound of every offset.
    #[must_use]
    pub fn char_len(&self) -> usize {
        self.rope.len_chars()
    }

    /// The number of lines. A text ending in `\n` has a final empty line, which
    /// is what a pane wants to draw a cursor on.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// One line without its line break. An out-of-range line is empty.
    #[must_use]
    pub fn line(&self, line: usize) -> String {
        let range = self.line_range(line);
        self.slice(range)
    }

    /// The characters of one line, excluding its line break. A line past the end
    /// is the empty range at zero.
    #[must_use]
    pub fn line_range(&self, line: usize) -> Range<usize> {
        if line >= self.line_count() {
            return 0..0;
        }
        let start = self.rope.line_to_char(line);
        let content = self.rope.line(line);
        let length = content.len_chars();
        let trimmed = if length > 0 && content.char(length - 1) == '\n' {
            length - 1
        } else {
            length
        };
        start..start + trimmed
    }

    /// Part of the text, with both ends clamped to the buffer.
    ///
    /// Clamping rather than panicking is the whole point of a char-indexed API:
    /// a click, a motion key and an edit computed against a slightly older text
    /// all arrive with an offset from the past, and the useful answer is the
    /// nearest one that exists.
    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> String {
        let len = self.char_len();
        let start = range.start.min(len);
        let end = range.end.clamp(start, len);
        self.rope.slice(start..end).to_string()
    }

    /// The character at an offset, or `None` past either end.
    #[must_use]
    pub fn char_at(&self, offset: usize) -> Option<char> {
        self.rope.get_char(offset)
    }

    /// The line and character column an offset names. An offset past the end is
    /// the end of the document.
    #[must_use]
    pub fn line_column(&self, offset: usize) -> LineColumn {
        let offset = offset.min(self.char_len());
        let line = self.rope.char_to_line(offset);
        LineColumn {
            line,
            column: offset - self.rope.line_to_char(line),
        }
    }

    /// The offset a line and column name, clamped to that line.
    ///
    /// The column is clamped to the line's length rather than carried into the
    /// next line, because every caller that wants to wrap already does it
    /// itself, and one that forgot would silently edit the wrong line.
    #[must_use]
    pub fn offset(&self, position: LineColumn) -> usize {
        let line = position.line.min(self.line_count().saturating_sub(1));
        let range = self.line_range(line);
        range.start + position.column.min(range.end - range.start)
    }

    /// Apply replacements that do not overlap each other.
    ///
    /// Applied from the back, so a replacement's offsets stay valid while the
    /// text before it changes length; the caller may hand them in any order, but
    /// not overlapping, which is what [`crate::Edit::replacements`] and
    /// [`crate::Change`] both guarantee.
    pub fn apply(&mut self, replacements: &[Replacement]) {
        let mut ordered: Vec<&Replacement> = replacements.iter().collect();
        ordered.sort_by_key(|replacement| std::cmp::Reverse(replacement.range.start));
        for replacement in ordered {
            let len = self.char_len();
            let start = replacement.range.start.min(len);
            let end = replacement.range.end.clamp(start, len);
            if start != end {
                self.rope.remove(start..end);
            }
            if !replacement.text.is_empty() {
                self.rope.insert(start, &replacement.text);
            }
        }
    }

    /// How this file's lines end, and how they will be written.
    #[must_use]
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Change how this file's lines will be written.
    ///
    /// The text in the buffer does not move; only the bytes that saving produces
    /// do, which is why converting a file is a deliberate call and never a side
    /// effect of reading it.
    pub fn set_line_ending(&mut self, ending: LineEnding) {
        self.line_ending = ending;
        self.mixed_endings = false;
    }

    /// Whether the file started with a byte-order mark.
    #[must_use]
    pub fn has_bom(&self) -> bool {
        self.bom
    }

    /// Add or remove the byte-order mark.
    pub fn set_bom(&mut self, bom: bool) {
        self.bom = bom;
    }

    /// Whether the bytes that were read mixed line endings.
    ///
    /// True means the file did not survive normalisation exactly: the endings
    /// were written back as the majority one. It is reported rather than
    /// refused, because refusing to open a file somebody's tooling wrote is
    /// worse than opening it and saying so.
    #[must_use]
    pub fn had_mixed_endings(&self) -> bool {
        self.mixed_endings
    }
}

/// What one pass over a file found about its line endings.
struct Endings {
    crlf: usize,
    cr: usize,
    lf: usize,
    first: LineEnding,
}

impl Endings {
    /// The ending most of the file uses; a tie goes to whichever appeared first.
    fn ending(&self) -> LineEnding {
        let most = self.crlf.max(self.cr).max(self.lf);
        if most == 0 {
            return self.first;
        }
        let first_count = match self.first {
            LineEnding::CrLf => self.crlf,
            LineEnding::Cr => self.cr,
            LineEnding::Lf => self.lf,
        };
        if first_count == most {
            self.first
        } else if self.crlf == most {
            LineEnding::CrLf
        } else if self.cr == most {
            LineEnding::Cr
        } else {
            LineEnding::Lf
        }
    }

    /// Whether more than one kind of ending is present.
    fn mixed(&self) -> bool {
        [self.crlf, self.cr, self.lf]
            .iter()
            .filter(|count| **count > 0)
            .count()
            > 1
    }
}

/// Count the line endings in `text`.
fn scan(text: &str) -> Endings {
    let bytes = text.as_bytes();
    let mut found = Endings {
        crlf: 0,
        cr: 0,
        lf: 0,
        first: LineEnding::Lf,
    };
    let mut seen = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                found.crlf += 1;
                if !seen {
                    found.first = LineEnding::CrLf;
                    seen = true;
                }
                index += 2;
                continue;
            }
            b'\r' => {
                found.cr += 1;
                if !seen {
                    found.first = LineEnding::Cr;
                    seen = true;
                }
            }
            b'\n' => {
                found.lf += 1;
                if !seen {
                    found.first = LineEnding::Lf;
                    seen = true;
                }
            }
            _ => {}
        }
        index += 1;
    }
    found
}

/// Rewrite every line ending as `\n`, which is what the rope holds.
fn normalise(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' {
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
            out.push('\n');
        } else {
            out.push(character);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Replacement;
    use proptest::prelude::*;

    #[test]
    fn a_rope_holds_only_line_feeds_whatever_the_file_said() {
        let buffer = Buffer::from_text("one\r\ntwo\rthree\nfour");
        assert_eq!(buffer.text(), "one\ntwo\nthree\nfour");
        assert_eq!(buffer.line_ending(), LineEnding::CrLf, "two of the three");
        assert!(buffer.had_mixed_endings());
        assert_eq!(buffer.line_count(), 4);
        assert_eq!(buffer.line(1), "two");
    }

    #[test]
    fn saving_writes_the_endings_the_file_had() {
        assert_eq!(
            Buffer::from_bytes(b"a\r\nb\r\n").expect("text").to_bytes(),
            b"a\r\nb\r\n"
        );
        assert_eq!(
            Buffer::from_bytes(b"a\nb\n").expect("text").to_bytes(),
            b"a\nb\n"
        );
        assert_eq!(
            Buffer::from_bytes(b"a\rb\r").expect("text").to_bytes(),
            b"a\rb\r"
        );
        assert_eq!(
            Buffer::from_bytes(b"no break").expect("text").to_bytes(),
            b"no break"
        );
    }

    #[test]
    fn a_byte_order_mark_is_a_flag_not_a_character() {
        let buffer = Buffer::from_bytes(b"\xEF\xBB\xBFone\ntwo").expect("text");
        assert!(buffer.has_bom());
        assert_eq!(buffer.text(), "one\ntwo", "no U+FEFF in front of the text");
        assert_eq!(buffer.char_len(), 7);
        assert_eq!(buffer.to_bytes(), b"\xEF\xBB\xBFone\ntwo");

        let mut plain = Buffer::from_text("one");
        assert!(!plain.has_bom());
        plain.set_bom(true);
        assert_eq!(plain.to_bytes(), b"\xEF\xBB\xBFone");
    }

    #[test]
    fn mixed_endings_are_reported_and_normalised_to_the_majority() {
        let buffer = Buffer::from_text("a\nb\nc\nlast\r\n");
        assert_eq!(buffer.line_ending(), LineEnding::Lf, "three against one");
        assert!(buffer.had_mixed_endings());
        assert_eq!(buffer.to_bytes(), b"a\nb\nc\nlast\n");

        let mut converted = buffer.clone();
        converted.set_line_ending(LineEnding::CrLf);
        assert!(!converted.had_mixed_endings(), "a decision was made");
        assert_eq!(converted.to_bytes(), b"a\r\nb\r\nc\r\nlast\r\n");
    }

    #[test]
    fn a_file_that_is_not_utf8_is_refused_at_the_byte_that_stopped_it() {
        let error = Buffer::from_bytes(b"ok\xFFmore").expect_err("invalid UTF-8");
        assert!(
            matches!(error, TextError::NotUtf8 { offset: 2 }),
            "the offset is where decoding stopped: {error}"
        );
    }

    #[test]
    fn an_offset_past_the_end_is_the_end() {
        let buffer = Buffer::from_text("one\ntwo\n");
        assert_eq!(buffer.slice(100..200), "");
        // Built from values so the assertion reads as it does on purpose: a
        // reversed range arrives from a stale click, not from a typo here.
        let (from, to) = (5usize, 1usize);
        assert_eq!(
            buffer.slice(from..to),
            "",
            "a reversed range is empty, not a panic"
        );
        assert_eq!(buffer.slice(0..100), "one\ntwo\n");
        assert_eq!(buffer.line(99), "");
        assert_eq!(buffer.line_range(99), 0..0);
        assert_eq!(buffer.char_at(99), None);
    }

    #[test]
    fn positions_count_characters_not_bytes() {
        let buffer = Buffer::from_text("a😀日\né");
        // a(0) 😀(1) 日(2) \n(3) é(4)
        assert_eq!(buffer.line_column(2), LineColumn::new(0, 2));
        assert_eq!(buffer.line_column(4), LineColumn::new(1, 0));
        assert_eq!(buffer.offset(LineColumn::new(0, 3)), 3);
        assert_eq!(
            buffer.offset(LineColumn::new(99, 0)),
            4,
            "a line that is not there"
        );
        assert_eq!(
            buffer.offset(LineColumn::new(0, 99)),
            3,
            "a column past the line stops at the line's end, not the next line"
        );
        assert_eq!(buffer.line_column(buffer.char_len()), LineColumn::new(1, 1));
    }

    #[test]
    fn replacements_are_applied_from_the_back_so_offsets_stay_valid() {
        let mut buffer = Buffer::from_text("hello world");
        buffer.apply(&[
            Replacement {
                range: 0..5,
                text: "goodbye".to_owned(),
            },
            Replacement {
                range: 6..11,
                text: "there".to_owned(),
            },
        ]);
        assert_eq!(buffer.text(), "goodbye there");
    }

    fn any_text() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just('\n'),
                Just('\r'),
                Just('a'),
                Just(' '),
                Just('\t'),
                Just('é'),
                Just('日'),
                Just('😀'),
                Just('\u{301}'),
                any::<char>(),
            ],
            0..60,
        )
        .prop_map(String::from_iter)
    }

    proptest! {
        /// A file whose endings agree comes back byte for byte — the property
        /// that makes "save in the editor" safe on somebody else's repository.
        #[test]
        fn a_consistent_file_round_trips_through_bytes(
            // Two lines at least, or there is no ending to detect.
            lines in proptest::collection::vec(any_line(), 2..6),
            ending in prop::sample::select(vec![LineEnding::Lf, LineEnding::CrLf, LineEnding::Cr]),
            bom in any::<bool>(),
        ) {
            let mut file = Vec::new();
            if bom {
                file.extend_from_slice(BOM);
            }
            file.extend_from_slice(lines[0].as_bytes());
            for line in &lines[1..] {
                file.extend_from_slice(ending.as_str().as_bytes());
                file.extend_from_slice(line.as_bytes());
            }
            let buffer = Buffer::from_bytes(&file).expect("generated text is UTF-8");
            prop_assert!(!buffer.had_mixed_endings());
            prop_assert_eq!(buffer.line_ending(), ending);
            // Not `has_bom() == bom`: a generated line may itself begin with
            // U+FEFF, and a leading U+FEFF *is* a byte-order mark however it got
            // there. The byte round trip below is the property that matters.
            prop_assert_eq!(buffer.to_bytes(), file);
        }

        /// No offset, however silly, can panic a read.
        #[test]
        fn every_offset_has_an_answer(text in any_text(), offset in any::<usize>()) {
            let buffer = Buffer::from_text(&text);
            let position = buffer.line_column(offset);
            let clamped = offset.min(buffer.char_len());
            prop_assert!(buffer.char_at(clamped).is_some() == (clamped < buffer.char_len()));
            prop_assert!(buffer.char_at(offset).is_none() == (offset >= buffer.char_len()));
            prop_assert!(buffer.line_column(clamped) == position);
            prop_assert!(buffer.offset(position) <= buffer.char_len());
            for line in [position.line, position.line + 1, line_count_saturating(&buffer)] {
                let range = buffer.line_range(line);
                prop_assert!(range.end <= buffer.char_len());
                prop_assert!(buffer.line(line).chars().all(|character| character != '\n'));
            }
        }
    }

    fn line_count_saturating(buffer: &Buffer) -> usize {
        buffer.line_count().saturating_sub(1)
    }

    /// Text that holds no line break, so a test can say what ending a file has.
    fn any_line() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just('a'),
                Just(' '),
                Just('\t'),
                Just('é'),
                Just('日'),
                Just('😀'),
                Just('\u{301}'),
                any::<char>(),
            ],
            0..40,
        )
        .prop_map(|characters| {
            characters
                .into_iter()
                .filter(|character| *character != '\r' && *character != '\n')
                .collect::<String>()
        })
    }
}
