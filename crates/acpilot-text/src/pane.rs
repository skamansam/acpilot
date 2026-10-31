//! What a pane does with a document: which rows are on screen, and what is in
//! them.
//!
//! The terminal, the window and the browser each draw the same buffer three
//! different ways, and the one thing they must not disagree about is what a
//! person is looking at: which lines are on screen, what part of a long line
//! survived the horizontal scroll, where the caret sits in cells, what `/` found
//! and which bracket matches which. All of that is arithmetic over the rope, so
//! it lives here rather than three times in three renderers — a pane draws
//! [`Viewport::visible`] and paints what it is given.
//!
//! Three decisions the tests hold:
//!
//! - **A viewport is measured in cells, not characters.** A tab is spaces up to
//!   the next stop, `日` and `😀` are two columns, a combining mark is none. A
//!   caret placed by counting characters drifts left of the glyph it is on the
//!   first time a file has a tab in it, and every line after that is wrong.
//! - **Only the visible rows are built.** [`Viewport::visible`] walks the lines
//!   between `top` and `top + rows` and stops; a 60k-line file costs what forty
//!   rows cost. [`Viewport::reveal`] keeps the caret on screen by moving the
//!   viewport, which is the only thing a motion key has to do to the view.
//! - **A character clipped by the scroll is blank cells, not a missing cell.**
//!   The view starting inside a tab or inside `日` emits the blank remainder, so
//!   the text after it lands in the column it is actually in. The alternative —
//!   shifting the line left — is a cursor one column off for the rest of the
//!   line.
//!
//! What is not here: colours (a [`Cell`] carries the char offset and the pane
//! asks the highlighter), wrapping (long lines scroll, they do not fold — a soft
//! wrap is a layout a pane chooses, and it changes what "one row" means), and
//! diffing, which is P6-9.

use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use crate::buffer::{Buffer, LineColumn};

/// How many spaces a tab advances by when nobody says otherwise.
pub const TAB_WIDTH: usize = 4;

/// The part of a document a pane can see.
///
/// `top` is the first visible line and `left` the first visible cell column, so
/// two numbers describe the window and its size describes the rest. Both are
/// clamped by the buffer in the calls that move them: a viewport told to scroll
/// past the end of a file stops at the end of the file, rather than becoming a
/// screen of blank lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    top: usize,
    left: usize,
    rows: usize,
    cols: usize,
    tab_width: usize,
}

impl Viewport {
    /// A viewport of `rows` lines by `cols` cells, at the start of the document
    /// and with tabs four wide.
    #[must_use]
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            top: 0,
            left: 0,
            rows,
            cols,
            tab_width: TAB_WIDTH,
        }
    }

    /// Set how wide a tab draws.
    ///
    /// A property of the pane rather than of the buffer: the same file is four
    /// wide in one editor and eight in another, which is why the rope holds the
    /// `\t` and this holds what it means.
    #[must_use]
    pub fn with_tab_width(mut self, tab_width: usize) -> Self {
        self.tab_width = tab_width.max(1);
        self
    }

    /// Change the size, keeping the top-left corner.
    ///
    /// The corner is kept rather than the centre: a window that grew should show
    /// more of the file, not the same lines shifted. The pane calls [`Self::reveal`]
    /// afterwards, which is also what stops a viewport that is now taller than the
    /// document from sitting scrolled past the end of it.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;
    }

    /// The first visible line.
    #[must_use]
    pub const fn top(&self) -> usize {
        self.top
    }

    /// The first visible cell column.
    #[must_use]
    pub const fn left(&self) -> usize {
        self.left
    }

    /// How many lines fit.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// How many cells of text fit beside the gutter.
    #[must_use]
    pub const fn cols(&self) -> usize {
        self.cols
    }

    /// How wide a tab draws.
    #[must_use]
    pub const fn tab_width(&self) -> usize {
        self.tab_width
    }

    /// The line a row of the viewport names.
    #[must_use]
    pub const fn line_at(&self, row: usize) -> usize {
        self.top + row
    }

    /// The row a line lands on, when it is on screen at all.
    #[must_use]
    pub fn row_of(&self, line: usize) -> Option<usize> {
        line.checked_sub(self.top)
            .filter(|row| *row < self.rows.max(1))
    }

    /// The largest `top` that still fills the viewport, so scrolling stops with
    /// the last line at the bottom rather than in the middle.
    fn max_top(&self, buffer: &Buffer) -> usize {
        buffer.line_count().saturating_sub(self.rows.max(1))
    }

    /// Put a line at the top of the viewport, as far as the document allows.
    ///
    /// Returns whether anything moved, so a caller can skip a repaint.
    pub fn scroll_to_line(&mut self, buffer: &Buffer, line: usize) -> bool {
        let top = line.min(self.max_top(buffer));
        self.set_top(top)
    }

    /// Move the viewport by lines: positive is down, and both ends stop at the
    /// document rather than past it.
    pub fn scroll_lines(&mut self, buffer: &Buffer, delta: isize) -> bool {
        let top = shifted(self.top, delta).min(self.max_top(buffer));
        self.set_top(top)
    }

    /// Move the viewport by cells: positive is right. A horizontal scroll has no
    /// end to clamp to — the longest line is not known without walking the file —
    /// so it stops at the left edge and otherwise scrolls as asked.
    pub fn scroll_columns(&mut self, delta: isize) -> bool {
        let left = shifted(self.left, delta);
        let moved = left != self.left;
        self.left = left;
        moved
    }

    /// Scroll the least that puts a caret on screen.
    ///
    /// The whole point of the horizontal half: a caret moved right by a motion key
    /// has to be visible, and a pane that only scrolled vertically would type off
    /// the right edge of the window. Returns whether anything moved.
    pub fn reveal(&mut self, buffer: &Buffer, caret: LineColumn) -> bool {
        // A viewport that has not been measured yet — the frame a pane is created
        // in, before it is drawn once — shows nothing and scrolls to nothing. Its
        // first real size is what places the window, and a scroll computed against
        // no cells would leave the file opened mid-line.
        if self.rows == 0 || self.cols == 0 {
            return false;
        }
        let line = caret.line.min(buffer.line_count().saturating_sub(1));
        // A viewport that grew has to stop being scrolled past the end of the
        // document: the first real size a pane is given arrives after it was
        // created one row tall, and without this the editor opens near the bottom
        // of the file with a screen of blank rows under it.
        let mut moved = self.set_top(self.top.min(self.max_top(buffer)));
        let rows = self.rows.max(1);
        if line < self.top {
            moved |= self.set_top(line);
        } else if line >= self.top + rows {
            moved |= self.set_top(line + 1 - rows);
        }
        let column = caret_cells(buffer, LineColumn::new(line, caret.column), self.tab_width);
        let cols = self.cols.max(1);
        if column < self.left {
            self.left = column;
            moved = true;
        } else if column >= self.left + cols {
            self.left = column + 1 - cols;
            moved = true;
        }
        moved
    }

    /// The rows on screen, top to bottom, with the text that fits in each.
    ///
    /// One [`Row`] per line that exists: a document shorter than the viewport
    /// answers with fewer rows, so a pane draws the rest of its box empty rather
    /// than as lines that are not there.
    #[must_use]
    pub fn visible(&self, buffer: &Buffer) -> Vec<Row> {
        let lines = buffer.line_count();
        let mut rows = Vec::with_capacity(self.rows.min(lines));
        for row in 0..self.rows {
            let line = self.top + row;
            if line >= lines {
                break;
            }
            rows.push(self.row(buffer, line));
        }
        rows
    }

    /// One line as it will be drawn: the characters the window shows, and blank
    /// cells where it cut a tab or a wide character in half.
    fn row(&self, buffer: &Buffer, line: usize) -> Row {
        let range = buffer.line_range(line);
        let text = buffer.slice(range.clone());
        let right = self.left + self.cols;
        let mut cells = Vec::new();
        let mut column = 0;
        for (index, character) in text.chars().enumerate() {
            let width = cells_of(character, column, self.tab_width);
            if column >= right || column + width > right {
                break;
            }
            let at = range.start + index;
            if column + width <= self.left {
                // Entirely left of the window.
                column += width;
                continue;
            }
            if column < self.left {
                // The window starts inside this character: a tab, or a wide glyph
                // whose left half is scrolled away. What is visible of it is blank
                // cells, and the character itself is not drawn.
                for _ in self.left..(column + width) {
                    cells.push(Cell { ch: ' ', at: None });
                }
                column += width;
                continue;
            }
            if character == '\t' {
                for _ in 0..width {
                    cells.push(Cell { ch: ' ', at: None });
                }
            } else {
                cells.push(Cell {
                    ch: character,
                    at: Some(at),
                });
            }
            column += width;
        }
        Row { line, cells }
    }

    /// Where the caret is on screen: the row of the viewport and the cell column
    /// in it, or `None` when it is outside the window.
    ///
    /// A caret at the very end of a line wider than the window is not visible —
    /// [`Self::reveal`] is what puts it back — because reporting a column past
    /// the pane would place the terminal's cursor on top of another pane.
    #[must_use]
    pub fn caret(&self, buffer: &Buffer, caret: LineColumn) -> Option<(usize, usize)> {
        if self.rows == 0 || self.cols == 0 {
            return None;
        }
        let row = self.row_of(caret.line)?;
        if caret.line >= buffer.line_count() {
            return None;
        }
        let column = caret_cells(buffer, caret, self.tab_width);
        (column >= self.left && column < self.left + self.cols.max(1))
            .then(|| (row, column - self.left))
    }

    /// Move the top of the viewport, clamped by nothing: the caller has the buffer
    /// or has already decided.
    fn set_top(&mut self, top: usize) -> bool {
        let moved = top != self.top;
        self.top = top;
        moved
    }
}

/// One line on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The line's number in the buffer, zero-based.
    pub line: usize,
    /// The cells that fit, left to right.
    pub cells: Vec<Cell>,
}

impl Row {
    /// The row's text, with tabs expanded and anything clipped blank.
    ///
    /// For tests, the status line and anything that wants the line as a string
    /// rather than as styled runs; a pane paints [`Row::cells`], because a
    /// character's colour is a question only the highlighter can answer.
    #[must_use]
    pub fn text(&self) -> String {
        self.cells.iter().map(|cell| cell.ch).collect()
    }

    /// How many cells the row occupies.
    #[must_use]
    pub fn width(&self) -> usize {
        self.cells
            .iter()
            .map(|cell| cells_of(cell.ch, 0, TAB_WIDTH).max(1))
            .sum()
    }
}

/// One character on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The character to draw. A tab arrives as spaces, and a character the scroll
    /// cut in half arrives as blanks.
    pub ch: char,
    /// Its char offset in the buffer, or `None` for the cells a tab expanded
    /// into — there is no character there, so there is no token either.
    pub at: Option<usize>,
}

/// The cells a character occupies starting at `column`.
///
/// A tab is spaces to the next stop, which is why it depends on where it starts.
/// A character no font can draw alone — a control code, a combining mark — is
/// zero, and a wide character is two. This is the one place the editor decides
/// what a column is.
#[must_use]
pub fn cells_of(character: char, column: usize, tab_width: usize) -> usize {
    if character == '\t' {
        let tab_width = tab_width.max(1);
        return tab_width - column % tab_width;
    }
    if character.is_control() {
        return 0;
    }
    UnicodeWidthChar::width(character).unwrap_or(0)
}

/// How many cells of the line come before a caret: the column a cursor is drawn
/// at.
///
/// The column is clamped to the line, because a caret from an older text or a
/// stale click is the everyday case and the answer that helps is the end of the
/// line, not a panic.
#[must_use]
pub fn caret_cells(buffer: &Buffer, caret: LineColumn, tab_width: usize) -> usize {
    let range = buffer.line_range(caret.line);
    let text = buffer.slice(range);
    let mut column = 0;
    for character in text.chars().take(caret.column) {
        column += cells_of(character, column, tab_width);
    }
    column
}

/// The character column a cell on a line names: where a click puts a caret.
///
/// The inverse of [`caret_cells`], and the reason a click can trust its column: a
/// cell inside a tab or inside a wide character lands on that character's left
/// edge, and a cell past the end of the line is the end of the line. A click is
/// always a cell — never a character — because that is what a terminal reports.
#[must_use]
pub fn column_at(buffer: &Buffer, line: usize, cell: usize, tab_width: usize) -> usize {
    let text = buffer.slice(buffer.line_range(line));
    let mut column = 0;
    for (index, character) in text.chars().enumerate() {
        let width = cells_of(character, column, tab_width);
        if column + width > cell {
            return index;
        }
        column += width;
    }
    text.chars().count()
}

/// The position a go-to-line box names, zero-based.
///
/// Accepts what people type into one: `12`, ` 12 `, `12:5`, `12,5` and `12 5`,
/// with the line and column counted from one as every editor counts them. A
/// number that is not a line or that names position zero is `None` rather than
/// the start of the file, because a typo that silently jumps somewhere is worse
/// than one that does nothing.
#[must_use]
pub fn parse_position(input: &str) -> Option<LineColumn> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed
        .split([':', ',', ' ', '\t'])
        .filter(|part| !part.is_empty());
    let line = parts.next()?.parse::<usize>().ok()?;
    let column = match parts.next() {
        Some(part) => part.parse::<usize>().ok()?,
        None => 1,
    };
    if parts.next().is_some() || line == 0 || column == 0 {
        return None;
    }
    Some(LineColumn::new(line - 1, column - 1))
}

/// The bracket at or just left of an offset, and its match.
///
/// The caret may sit on a bracket or just after one — typing `)` in an editor
/// that only looked under the caret would never match anything, because the caret
/// is past the character that was just written.
///
/// The scan counts the same pair only, so `([)]` pairs `(` with `)`; it is a text
/// scan rather than a parse, so a bracket inside a string or a comment matches
/// too. That is what makes it work in a file with no grammar, and a grammar that
/// knows better is P7's business.
#[must_use]
pub fn bracket_at(buffer: &Buffer, at: usize) -> Option<(usize, usize)> {
    // The caret may be on the bracket, or just past the one that was typed.
    let after = at.checked_sub(1);
    let (offset, open, close, forward) =
        [Some(at), after].into_iter().flatten().find_map(|offset| {
            let (open, close, forward) = pair_of(buffer.char_at(offset)?)?;
            Some((offset, open, close, forward))
        })?;
    let mut depth = 0usize;
    let mut cursor = offset;
    loop {
        cursor = if forward {
            cursor.checked_add(1)?
        } else {
            cursor.checked_sub(1)?
        };
        // Looking right, the brackets that nest are the openers; looking left,
        // they are the closers. The other one ends the scan at depth zero.
        let (nested, terminal) = if forward {
            (open, close)
        } else {
            (close, open)
        };
        let character = buffer.char_at(cursor)?;
        if character == nested {
            depth += 1;
        } else if character == terminal {
            if depth == 0 {
                return if forward {
                    Some((offset, cursor))
                } else {
                    Some((cursor, offset))
                };
            }
            depth -= 1;
        }
    }
}

/// The pair a bracket belongs to, and whether its match is to the right.
fn pair_of(character: char) -> Option<(char, char, bool)> {
    match character {
        '(' => Some(('(', ')', true)),
        '[' => Some(('[', ']', true)),
        '{' => Some(('{', '}', true)),
        ')' => Some(('(', ')', false)),
        ']' => Some(('[', ']', false)),
        '}' => Some(('{', '}', false)),
        _ => None,
    }
}

/// What a cell is, as far as a search is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Not part of any match.
    No,
    /// Part of a match that is not the one being stepped through.
    Match,
    /// Part of the current match, which the caret is on.
    Current,
}

/// A search in one buffer: the query, where it matches, and which match is
/// current.
///
/// Smart case, like the terminal's find: a query in lowercase matches either
/// case, and one with a capital in it is taken at its word. That is the rule a
/// person's typing already follows — reaching for the shift key is how you say
/// "exactly this".
///
/// Matches are found per line and never span one, because a search for `foo` that
/// jumped the line break to find it would highlight the newline too, and no
/// editor shows that match as one thing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Find {
    query: String,
    matches: Vec<Range<usize>>,
    current: Option<usize>,
}

impl Find {
    /// An empty search.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What is being searched for.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Whether there is a query at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.query.is_empty()
    }

    /// Every match, in document order and never overlapping.
    #[must_use]
    pub fn matches(&self) -> &[Range<usize>] {
        &self.matches
    }

    /// The match the caret is on, if any.
    #[must_use]
    pub fn current(&self) -> Option<Range<usize>> {
        self.current
            .and_then(|index| self.matches.get(index))
            .cloned()
    }

    /// Which match is current, counting from one as the status line says it.
    #[must_use]
    pub fn position(&self) -> Option<usize> {
        self.current.map(|index| index + 1)
    }

    /// Set the query and rescan, choosing the first match that reaches `from`.
    ///
    /// A match the caret is already inside wins, which is what makes typing `foo`
    /// with the caret in the middle of `foo` select that one instead of the next
    /// one in the file. When there is none after the caret the search wraps to the
    /// first, because a find that answers "nothing" when the only match is above
    /// the caret is a find nobody trusts.
    ///
    pub fn set_query(&mut self, buffer: &Buffer, query: &str, from: usize) {
        self.query = query.to_owned();
        self.matches = scan(buffer, query);
        self.current = self
            .matches
            .iter()
            .position(|range| range.end > from)
            .or((!self.matches.is_empty()).then_some(0));
    }

    /// Rescan a text that has changed under the search, keeping the position of
    /// the current match where it can.
    pub fn rescan(&mut self, buffer: &Buffer) {
        self.matches = scan(buffer, &self.query);
        self.current = self
            .current
            .filter(|index| *index < self.matches.len())
            .or_else(|| (!self.matches.is_empty()).then_some(0));
    }

    /// Step to the next or previous match, wrapping, and answer with its start.
    ///
    /// Wrapping both ways: a search that stops at the last match makes `n` a key
    /// that sometimes does nothing, and the answer to "where else is it" is always
    /// somewhere.
    pub fn step(&mut self, forward: bool) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        let index = match self.current {
            Some(current) if forward => (current + 1) % self.matches.len(),
            Some(current) => (current + self.matches.len() - 1) % self.matches.len(),
            None if forward => 0,
            None => self.matches.len() - 1,
        };
        self.current = Some(index);
        self.matches.get(index).map(|range| range.start)
    }

    /// What a char offset is part of.
    ///
    /// A binary search rather than a walk per cell: a pane asks this for every
    /// character it draws, and the match list is sorted.
    #[must_use]
    pub fn hit(&self, offset: usize) -> Hit {
        let index = self.matches.partition_point(|range| range.end <= offset);
        match self.matches.get(index) {
            Some(range) if range.start <= offset => {
                if self.current == Some(index) {
                    Hit::Current
                } else {
                    Hit::Match
                }
            }
            _ => Hit::No,
        }
    }
}

/// Every match of a query in a text, in order.
fn scan(buffer: &Buffer, query: &str) -> Vec<Range<usize>> {
    let mut matches = Vec::new();
    if query.is_empty() {
        return matches;
    }
    let needle: Vec<char> = query.chars().collect();
    let exact = query.chars().any(char::is_uppercase);
    let len = needle.len();
    for line in 0..buffer.line_count() {
        let range = buffer.line_range(line);
        let haystack: Vec<char> = buffer.slice(range.clone()).chars().collect();
        let mut index = 0;
        while index + len <= haystack.len() {
            let hit = (0..len).all(|offset| {
                let (a, b) = (haystack[index + offset], needle[offset]);
                a == b || (!exact && a.to_lowercase().eq(b.to_lowercase()))
            });
            if hit {
                let start = range.start + index;
                matches.push(start..start + len);
                index += len;
            } else {
                index += 1;
            }
        }
    }
    matches
}

/// Move a `usize` by a signed delta, stopping at zero.
fn shifted(value: usize, delta: isize) -> usize {
    if delta >= 0 {
        value.saturating_add(delta.unsigned_abs())
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn viewport(rows: usize, cols: usize) -> Viewport {
        Viewport::new(rows, cols)
    }

    #[test]
    fn only_the_rows_on_screen_are_built() {
        let text: String = (0..500).map(|line| format!("line {line}\n")).collect();
        let buffer = Buffer::from_text(&text);
        let viewport = viewport(10, 40);
        let rows = viewport.visible(&buffer);
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0].text(), "line 0");
        assert_eq!(rows[9].text(), "line 9");
        assert_eq!(rows[9].line, 9);
    }

    #[test]
    fn a_short_document_answers_with_fewer_rows_than_the_box() {
        let buffer = Buffer::from_text("one\ntwo\n");
        let rows = viewport(20, 40).visible(&buffer);
        assert_eq!(rows.len(), 3, "two lines and the empty one after the break");
        assert_eq!(rows.last().map(|row| row.text()), Some(String::new()));
    }

    #[test]
    fn a_tab_is_spaces_to_the_next_stop_and_stops_where_it_starts() {
        let buffer = Buffer::from_text("a\tb");
        let row = &viewport(1, 20).visible(&buffer)[0];
        assert_eq!(row.text(), "a   b", "a tab is four wide from column zero");
        assert_eq!(row.cells[1].at, None, "the spaces have no character");
        assert_eq!(row.cells[4].at, Some(2), "`b` is the third character");

        let buffer = Buffer::from_text("あ\tb");
        let row = &viewport(1, 20).visible(&buffer)[0];
        assert_eq!(row.text(), "あ  b", "`あ` is two cells, so the tab is two");
    }

    #[test]
    fn a_wide_character_is_two_cells_and_the_caret_knows_it() {
        let buffer = Buffer::from_text("日x\n😀y");
        assert_eq!(
            caret_cells(&buffer, LineColumn::new(0, 1), TAB_WIDTH),
            2,
            "the caret after `日` is two cells in"
        );
        assert_eq!(caret_cells(&buffer, LineColumn::new(0, 2), TAB_WIDTH), 3);
        assert_eq!(caret_cells(&buffer, LineColumn::new(1, 1), TAB_WIDTH), 2);
    }

    #[test]
    fn scrolling_right_cuts_the_line_and_the_caret_follows_it() {
        let buffer = Buffer::from_text("abcdefghij\nsecond");
        let mut viewport = viewport(2, 5);
        assert_eq!(viewport.visible(&buffer)[0].text(), "abcde");
        assert!(viewport.scroll_columns(3));
        assert_eq!(viewport.left(), 3);
        assert_eq!(viewport.visible(&buffer)[0].text(), "defgh");
        assert_eq!(
            viewport.caret(&buffer, LineColumn::new(0, 0)),
            None,
            "the caret is left of the window"
        );
        assert_eq!(viewport.caret(&buffer, LineColumn::new(0, 3)), Some((0, 0)));
        assert_eq!(viewport.caret(&buffer, LineColumn::new(0, 7)), Some((0, 4)));
    }

    #[test]
    fn a_view_that_starts_inside_a_tab_draws_its_blank_remainder() {
        let buffer = Buffer::from_text("\tab\n");
        let mut viewport = viewport(1, 4);
        viewport.scroll_columns(2);
        let row = &viewport.visible(&buffer)[0];
        assert_eq!(row.text(), "  ab", "two of the tab's four cells are left");
        assert_eq!(row.cells[0].at, None);
        assert_eq!(row.cells[2].at, Some(1));
    }

    #[test]
    fn the_scroll_stops_with_the_last_line_at_the_bottom() {
        let buffer = Buffer::from_text("1\n2\n3\n4\n5");
        let mut viewport = viewport(2, 10);
        assert!(viewport.scroll_lines(&buffer, 100));
        assert_eq!(viewport.top(), 3, "the fourth of five lines, two rows tall");
        assert!(viewport.scroll_lines(&buffer, -100));
        assert_eq!(viewport.top(), 0);
        assert!(!viewport.scroll_lines(&buffer, -1), "nothing moved");
    }

    #[test]
    fn reveal_moves_the_viewport_the_least_it_can() {
        let text: String = (0..100).map(|line| format!("{line:03}\n")).collect();
        let buffer = Buffer::from_text(&text);
        let mut viewport = viewport(10, 20);
        // A caret below the box scrolls one line, not a page.
        assert!(viewport.reveal(&buffer, LineColumn::new(10, 0)));
        assert_eq!(viewport.top(), 1);
        // A caret inside the box moves nothing.
        assert!(!viewport.reveal(&buffer, LineColumn::new(5, 1)));
        assert_eq!(viewport.top(), 1);
        // A caret above it puts that line at the top.
        assert!(viewport.reveal(&buffer, LineColumn::new(0, 0)));
        assert_eq!(viewport.top(), 0);
    }

    #[test]
    fn reveal_scrolls_sideways_for_a_caret_off_the_right() {
        let buffer = Buffer::from_text(&format!("{}\n", "x".repeat(100)));
        let mut viewport = viewport(1, 20);
        assert!(viewport.reveal(&buffer, LineColumn::new(0, 40)));
        assert_eq!(viewport.left(), 21);
        assert_eq!(
            viewport.caret(&buffer, LineColumn::new(0, 40)),
            Some((0, 19))
        );
        // Scrolling back stops the moment the caret is on screen, not at zero:
        // jumping to column zero would move the text under a caret that was
        // already about to be visible.
        assert!(viewport.reveal(&buffer, LineColumn::new(0, 2)));
        assert_eq!(viewport.left(), 2);
    }

    #[test]
    fn a_clicked_cell_lands_on_the_character_under_it() {
        let buffer = Buffer::from_text("\tabc\n日本x\n");
        // A tab is four cells; a click anywhere inside it is the character before it.
        assert_eq!(column_at(&buffer, 0, 0, TAB_WIDTH), 0);
        assert_eq!(column_at(&buffer, 0, 3, TAB_WIDTH), 0);
        assert_eq!(column_at(&buffer, 0, 4, TAB_WIDTH), 1, "`a`");
        assert_eq!(column_at(&buffer, 0, 99, TAB_WIDTH), 4, "past the end");
        // Two wide characters, so cell 3 is the `x`.
        assert_eq!(column_at(&buffer, 1, 0, TAB_WIDTH), 0);
        assert_eq!(column_at(&buffer, 1, 1, TAB_WIDTH), 0, "both cells of `日`");
        assert_eq!(
            column_at(&buffer, 1, 2, TAB_WIDTH),
            1,
            "the first cell of `本`"
        );
        assert_eq!(
            column_at(&buffer, 1, 3, TAB_WIDTH),
            1,
            "its second cell too"
        );
        assert_eq!(column_at(&buffer, 1, 4, TAB_WIDTH), 2, "`x`");
        assert_eq!(
            column_at(&buffer, 99, 0, TAB_WIDTH),
            0,
            "a line that is not there"
        );
    }

    #[test]
    fn a_go_to_line_box_reads_what_people_type() {
        assert_eq!(parse_position("12"), Some(LineColumn::new(11, 0)));
        assert_eq!(parse_position(" 12 "), Some(LineColumn::new(11, 0)));
        assert_eq!(parse_position("12:5"), Some(LineColumn::new(11, 4)));
        assert_eq!(parse_position("12,5"), Some(LineColumn::new(11, 4)));
        assert_eq!(parse_position("12 5"), Some(LineColumn::new(11, 4)));
        assert_eq!(parse_position("0"), None, "lines are counted from one");
        assert_eq!(parse_position("12:0"), None);
        assert_eq!(parse_position(""), None);
        assert_eq!(parse_position("end"), None);
        assert_eq!(parse_position("1:2:3"), None);
    }

    #[test]
    fn a_bracket_matches_across_its_nesting() {
        let buffer = Buffer::from_text("fn main() { a(b[c]) }");
        assert_eq!(bracket_at(&buffer, 10), Some((10, 20)), "the braces");
        assert_eq!(bracket_at(&buffer, 13), Some((13, 18)), "`(` and `)`");
        assert_eq!(
            bracket_at(&buffer, 18),
            Some((13, 18)),
            "seen from the closer"
        );
        assert_eq!(
            bracket_at(&buffer, 19),
            Some((13, 18)),
            "the caret just past a bracket is on it"
        );
        assert_eq!(bracket_at(&buffer, 7), Some((7, 8)), "the empty call");
        assert_eq!(bracket_at(&buffer, 0), None, "`f` is not a bracket");
    }

    #[test]
    fn an_unclosed_bracket_has_no_match_rather_than_a_guess() {
        let buffer = Buffer::from_text("if (a && b");
        assert_eq!(bracket_at(&buffer, 3), None);
        let buffer = Buffer::from_text("}}");
        assert_eq!(bracket_at(&buffer, 0), None, "nothing opened");
    }

    #[test]
    fn search_finds_every_match_and_wraps_both_ways() {
        let buffer = Buffer::from_text("one two one\ntwo one\n");
        let mut find = Find::new();
        find.set_query(&buffer, "one", 0);
        assert_eq!(find.matches().len(), 3);
        assert_eq!(
            find.matches()[1],
            8..11,
            "the second `one` of the first line"
        );
        assert_eq!(find.position(), Some(1));
        assert_eq!(find.current(), Some(0..3));
        assert_eq!(find.step(true), Some(8));
        assert_eq!(find.step(true), Some(16));
        assert_eq!(find.step(true), Some(0), "wrapped to the top");
        assert_eq!(find.step(false), Some(16), "and back");
    }

    #[test]
    fn a_lowercase_query_ignores_case_and_a_capital_does_not() {
        let buffer = Buffer::from_text("Foo foo FOO");
        let mut find = Find::new();
        find.set_query(&buffer, "foo", 0);
        assert_eq!(find.matches().len(), 3);
        find.set_query(&buffer, "Foo", 0);
        assert_eq!(find.matches().len(), 1);
        find.set_query(&buffer, "", 0);
        assert!(find.matches().is_empty(), "the empty query matches nothing");
        assert_eq!(find.hit(0), Hit::No);
    }

    #[test]
    fn a_search_starts_at_the_caret_and_wraps_when_the_only_match_is_above_it() {
        let buffer = Buffer::from_text("one\ntwo\nthree\n");
        let mut find = Find::new();
        find.set_query(&buffer, "three", 0);
        assert_eq!(find.current(), Some(8..13), "the only match, below");
        // A match the caret is inside counts as the one at the caret.
        find.set_query(&buffer, "three", 10);
        assert_eq!(find.current(), Some(8..13));
        // Nothing after it: the search wraps to the first.
        find.set_query(&buffer, "one", 5);
        assert_eq!(find.current(), Some(0..3));
    }

    #[test]
    fn a_hit_tells_the_current_match_from_the_others() {
        let buffer = Buffer::from_text("foo bar foo");
        let mut find = Find::new();
        find.set_query(&buffer, "foo", 0);
        assert_eq!(find.hit(0), Hit::Current);
        assert_eq!(find.hit(1), Hit::Current);
        assert_eq!(find.hit(3), Hit::No);
        assert_eq!(find.hit(8), Hit::Match);
        assert_eq!(find.hit(99), Hit::No);
    }

    #[test]
    fn a_rescan_keeps_the_search_on_a_text_that_changed() {
        let mut buffer = Buffer::from_text("one two\n");
        let mut find = Find::new();
        find.set_query(&buffer, "two", 0);
        assert_eq!(find.current(), Some(4..7));
        buffer.apply(&[crate::edit::Replacement::of(0..0, "zero ".to_owned())]);
        find.rescan(&buffer);
        assert_eq!(find.current(), Some(9..12), "moved with the text");
    }

    proptest! {
        /// A row is never wider than the box it is drawn in, whatever the text.
        #[test]
        fn no_row_is_wider_than_its_viewport(
            text in "[\\t あ😀aé\\n]{0,120}",
            left in 0usize..40,
            cols in 1usize..30,
        ) {
            let buffer = Buffer::from_text(&text);
            let mut viewport = Viewport::new(5, cols);
            viewport.scroll_columns(left as isize);
            for row in viewport.visible(&buffer) {
                prop_assert!(row.cells.len() <= cols, "{:?} is {} cells", row.text(), row.cells.len());
                let width: usize = row.cells.iter().map(|cell| cells_of(cell.ch, 0, TAB_WIDTH).max(1)).sum();
                prop_assert!(width <= cols);
            }
        }

        /// The caret is on screen after `reveal`, for any caret and any viewport —
        /// the property that says horizontal scrolling works rather than
        /// sometimes works.
        #[test]
        fn reveal_always_puts_the_caret_on_screen(
            text in "[\\t あ😀aé\\n]{0,80}",
            line in 0usize..12,
            column in 0usize..60,
        ) {
            let buffer = Buffer::from_text(&text);
            let mut viewport = Viewport::new(4, 12);
            viewport.reveal(&buffer, LineColumn::new(line, column));
            let line = line.min(buffer.line_count().saturating_sub(1));
            let caret = LineColumn::new(line, column);
            prop_assert!(viewport.caret(&buffer, caret).is_some());
        }

        /// A click lands where it was aimed: the character it names starts at or
        /// before the cell, and the one after it starts past it. This is the
        /// property that makes the caret appear under the pointer in a file with
        /// tabs and wide glyphs in it.
        #[test]
        fn a_click_and_the_caret_agree_about_where_a_cell_is(
            text in "[\t あ😀aé\n]{0,80}",
            cell in 0usize..40,
        ) {
            let buffer = Buffer::from_text(&text);
            for line in 0..buffer.line_count() {
                let column = column_at(&buffer, line, cell, TAB_WIDTH);
                let length = buffer.line_range(line).len();
                prop_assert!(column <= length);
                prop_assert!(caret_cells(&buffer, LineColumn::new(line, column), TAB_WIDTH) <= cell);
                if column < length {
                    prop_assert!(
                        caret_cells(&buffer, LineColumn::new(line, column + 1), TAB_WIDTH) > cell
                    );
                }
            }
        }

        /// Every character of a visible row names the buffer character it came
        /// from, so a pane's colour is about the text it is drawn beside.
        #[test]
        fn every_cell_points_at_its_character(text in "[\\t あ😀aé\\n]{0,80}") {
            let buffer = Buffer::from_text(&text);
            let viewport = Viewport::new(20, 40);
            for row in viewport.visible(&buffer) {
                for cell in &row.cells {
                    if let Some(at) = cell.at {
                        prop_assert_eq!(buffer.char_at(at), Some(cell.ch));
                        prop_assert!(at >= buffer.line_range(row.line).start);
                        prop_assert!(at < buffer.line_range(row.line).end);
                    }
                }
            }
        }
    }
}
