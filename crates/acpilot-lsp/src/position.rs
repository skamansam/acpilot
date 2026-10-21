//! LSP positions are not byte offsets, and that is the whole problem.
//!
//! The protocol negotiates one of three position encodings during `initialize`.
//! A buffer is UTF-8 bytes; LSP positions are `(line, character)` where
//! `character` counts either bytes, UTF-16 code units, or code points from the
//! start of the line. Apply a UTF-16 column to a UTF-8 rope and it works for
//! every ASCII file — right up to the first emoji, at which point every
//! highlight and every diagnostic is one column off, forever.
//!
//! So conversion is a first-class type here, not an arithmetic detail at a call
//! site. [`LineIndex`] is built from the document text, [`Position`] is the wire
//! shape, and the round-trip properties are tested with arbitrary Unicode:
//! emoji (a surrogate pair in UTF-16), CJK (three bytes in UTF-8, one unit in
//! UTF-16), combining marks (two code points, one glyph), and CRLF.
//!
//! The index is rebuilt when the document changes, so its lifetime is one edit.
//! That is the honest cost: an incremental index is a P7 optimisation, and
//! [`position`](LineIndex::position) only scans the current line, not the file.

use std::fmt;

/// A position encoding the server and client agree on.
///
/// UTF-32 is in the specification but no server in the P0-7 survey advertised
/// it, and an editor whose buffer is UTF-8 would only gain a third conversion
/// path. It can be added without changing the API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    /// `character` counts UTF-8 bytes.
    Utf8,
    /// `character` counts UTF-16 code units. The LSP default when neither side
    /// says anything, which is why not saying anything is a bug.
    Utf16,
}

impl PositionEncoding {
    /// Parse the name exactly as it appears on the wire.
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        match name {
            "utf-8" => Some(Self::Utf8),
            "utf-16" => Some(Self::Utf16),
            _ => None,
        }
    }

    /// The name used in `general.positionEncodings` and in server capabilities.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
        }
    }

    /// What a server means when it advertises nothing.
    #[must_use]
    pub fn negotiated(advertised: Option<&str>) -> Self {
        advertised.and_then(Self::from_wire).unwrap_or(Self::Utf16)
    }

    /// Number of units a string occupies in this encoding.
    #[must_use]
    pub fn units(self, text: &str) -> usize {
        match self {
            Self::Utf8 => text.len(),
            Self::Utf16 => text.encode_utf16().count(),
        }
    }
}

impl fmt::Display for PositionEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// A zero-based line and encoding-specific character offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based column, counted in the negotiated encoding.
    pub character: u32,
}

impl Position {
    /// Construct a position.
    #[must_use]
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// Byte offsets of every line start in one document.
///
/// Lines are separated by `\n`. A `\r` before it stays part of the line's
/// content because that is what the bytes say: a client that silently drops it
/// disagrees with a server that does not, and the difference is a column on
/// every line of a Windows file.
#[derive(Clone, Debug)]
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    /// Index the line starts of `text`.
    ///
    /// The index borrows nothing; it is a few `usize`s per line. Pass the same
    /// text to [`position`](Self::position) and [`offset`](Self::offset): the
    /// index cannot know whether the document changed underneath it, and a stale
    /// index is how a diagnostic lands on the wrong line.
    #[must_use]
    pub fn new(text: &str) -> Self {
        let mut starts = Vec::with_capacity(text.lines().count() + 1);
        starts.push(0);
        starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
        Self { starts }
    }

    /// Number of lines. A document ending in `\n` has a final empty line.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Convert a UTF-8 byte offset to a wire position.
    ///
    /// Returns `None` for an offset past the end of `text` or inside a UTF-8
    /// sequence — positions exist between characters, not inside them.
    #[must_use]
    pub fn position(
        &self,
        text: &str,
        offset: usize,
        encoding: PositionEncoding,
    ) -> Option<Position> {
        if offset > text.len() || !text.is_char_boundary(offset) {
            return None;
        }

        let line = self
            .starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let start = *self.starts.get(line)?;
        let character = encoding.units(text.get(start..offset)?);

        Some(Position::new(
            u32::try_from(line).ok()?,
            u32::try_from(character).ok()?,
        ))
    }

    /// Convert a wire position to a UTF-8 byte offset in `text`.
    ///
    /// Returns `None` when the line does not exist, the character is past the
    /// end of that line, or a UTF-16 character offset lands in the middle of a
    /// surrogate pair. Reporting a position and receiving a different one back
    /// is worse than an error: it silently corrupts every edit.
    #[must_use]
    pub fn offset(
        &self,
        text: &str,
        position: Position,
        encoding: PositionEncoding,
    ) -> Option<usize> {
        let start = *self.starts.get(position.line as usize)?;
        let rest = text.get(start..)?;
        let line = &rest[..rest.find('\n').unwrap_or(rest.len())];

        let byte = match encoding {
            PositionEncoding::Utf8 => {
                let character = position.character as usize;
                if character > line.len() || !line.is_char_boundary(character) {
                    return None;
                }
                character
            }
            PositionEncoding::Utf16 => {
                let target = position.character as usize;
                let mut units = 0;
                let mut bytes = 0;
                for character in line.chars() {
                    if units == target {
                        break;
                    }
                    units += character.len_utf16();
                    bytes += character.len_utf8();
                }
                if units != target {
                    return None;
                }
                bytes
            }
        };

        Some(start + byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Every (character, byte) pair that names a position inside one line.
    fn positions_in(line: &str, encoding: PositionEncoding) -> Vec<(usize, usize)> {
        match encoding {
            PositionEncoding::Utf8 => (0..=line.len())
                .filter(|&bytes| line.is_char_boundary(bytes))
                .map(|bytes| (bytes, bytes))
                .collect(),
            PositionEncoding::Utf16 => {
                let mut positions = vec![(0, 0)];
                let (mut units, mut bytes) = (0, 0);
                for character in line.chars() {
                    units += character.len_utf16();
                    bytes += character.len_utf8();
                    positions.push((units, bytes));
                }
                positions
            }
        }
    }

    fn text() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just('\n'),
                Just('\r'),
                Just('a'),
                Just(' '),
                Just('é'),
                Just('日'),
                Just('😀'),
                Just('\u{301}'),   // combining acute
                Just('\u{1F1EF}'), // regional indicator
                any::<char>(),
            ],
            0..80,
        )
        .prop_map(String::from_iter)
    }

    #[test]
    fn ascii_offsets_agree_across_encodings() {
        let text = "hello\nworld\n";
        let index = LineIndex::new(text);
        for offset in [0, 5, 6, 11, 12] {
            assert_eq!(
                index.position(text, offset, PositionEncoding::Utf8),
                index.position(text, offset, PositionEncoding::Utf16),
            );
        }
        assert_eq!(
            index.position(text, 6, PositionEncoding::Utf8),
            Some(Position::new(1, 0))
        );
    }

    #[test]
    fn utf16_counts_code_units_not_bytes_or_glyphs() {
        let text = "a😀b日e\u{301}";
        let index = LineIndex::new(text);

        // 😀 is two UTF-16 code units and four UTF-8 bytes.
        assert_eq!(
            index.position(text, 5, PositionEncoding::Utf8),
            Some(Position::new(0, 5))
        );
        assert_eq!(
            index.position(text, 5, PositionEncoding::Utf16),
            Some(Position::new(0, 3))
        );

        let emoji = index.position(text, 1, PositionEncoding::Utf16).unwrap();
        assert_eq!(emoji, Position::new(0, 1));
        // The middle of the surrogate pair is not a position.
        assert_eq!(
            index.offset(text, Position::new(0, 2), PositionEncoding::Utf16),
            None
        );
        assert_eq!(
            index.offset(text, Position::new(0, 3), PositionEncoding::Utf16),
            Some(5)
        );

        // 日: one UTF-16 unit, three UTF-8 bytes, starting at byte 6.
        assert_eq!(
            index.offset(text, Position::new(0, 4), PositionEncoding::Utf16),
            Some(6)
        );
        // Byte 8 is inside 日 and therefore not a position.
        assert_eq!(index.position(text, 8, PositionEncoding::Utf16), None);
        // e + combining acute are two UTF-16 units, but one grapheme — no
        // grapheme counting here, because LSP does not define one.
        assert_eq!(
            index.offset(text, Position::new(0, 5), PositionEncoding::Utf16),
            Some(9)
        );
        assert_eq!(
            index.offset(text, Position::new(0, 7), PositionEncoding::Utf16),
            Some(12)
        );
    }

    #[test]
    fn crlf_is_not_silently_rewritten() {
        let text = "a\r\nb";
        let index = LineIndex::new(text);
        // The `\r` is part of line 0, and the position just before `\n` is after it.
        assert_eq!(
            index.offset(text, Position::new(0, 2), PositionEncoding::Utf8),
            Some(2)
        );
        assert_eq!(
            index.position(text, 2, PositionEncoding::Utf8),
            Some(Position::new(0, 2))
        );
        assert_eq!(
            index.position(text, 3, PositionEncoding::Utf8),
            Some(Position::new(1, 0))
        );
    }

    #[test]
    fn rejects_positions_that_do_not_exist() {
        let text = "abc\ndef";
        let index = LineIndex::new(text);
        assert_eq!(index.position(text, 8, PositionEncoding::Utf8), None);
        assert_eq!(
            index.offset(text, Position::new(2, 0), PositionEncoding::Utf8),
            None
        );
        assert_eq!(
            index.offset(text, Position::new(0, 9), PositionEncoding::Utf8),
            None
        );
        assert_eq!(
            index.offset(text, Position::new(0, 1), PositionEncoding::Utf16),
            Some(1)
        );
    }

    proptest! {
        #[test]
        fn offset_round_trips_through_every_char_boundary(text in text(), encoding in prop::sample::select(vec![PositionEncoding::Utf8, PositionEncoding::Utf16])) {
            let index = LineIndex::new(&text);
            for offset in (0..=text.len()).filter(|&offset| text.is_char_boundary(offset)) {
                let position = index.position(&text, offset, encoding)
                    .expect("a char boundary is always a position");
                prop_assert_eq!(
                    index.offset(&text, position, encoding),
                    Some(offset),
                    "offset {} encoding {}", offset, encoding
                );
            }
        }

        #[test]
        fn every_line_position_maps_back(text in text(), encoding in prop::sample::select(vec![PositionEncoding::Utf8, PositionEncoding::Utf16])) {
            let index = LineIndex::new(&text);
            for line in 0..index.line_count() {
                let start = index.starts[line];
                let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
                for (character, bytes) in positions_in(&text[start..end], encoding) {
                    let position = Position::new(line as u32, character as u32);
                    prop_assert_eq!(
                        index.offset(&text, position, encoding),
                        Some(start + bytes),
                        "line {:?} char {} encoding {}",
                        &text[start..end],
                        character,
                        encoding
                    );
                    prop_assert_eq!(
                        index.position(&text, start + bytes, encoding),
                        Some(position)
                    );
                }
            }
        }

        #[test]
        fn positions_stay_inside_their_line(text in text(), encoding in prop::sample::select(vec![PositionEncoding::Utf8, PositionEncoding::Utf16])) {
            let index = LineIndex::new(&text);
            for offset in (0..=text.len()).filter(|&offset| text.is_char_boundary(offset)) {
                let position = index.position(&text, offset, encoding).unwrap();
                prop_assert!((position.line as usize) < index.line_count());
                let line_start = index.starts[position.line as usize];
                prop_assert!(line_start <= offset);
                prop_assert!(!text[line_start..offset].contains('\n'));
            }
        }
    }
}
