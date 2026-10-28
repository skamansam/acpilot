//! The editor pane: a real document, its parse, and what a key does to it.
//!
//! The design pass drew the editor from a mock (`acpilot_demo::editor`); this is
//! the pane behind that drawing, and the shape it draws is unchanged — a tab, a
//! gutter with line numbers and git markers, a current line, a caret — because
//! the mock was designed to be replaced by exactly this.
//!
//! What it holds is [`acpilot_text::Document`]: a rope, a caret set and an undo
//! history, the same one the desktop app and the browser will hold. Scrolling,
//! clipping and caret arithmetic are [`acpilot_text::pane`]'s, so this module
//! decides only what a *key* means and what a *frame* looks like.
//!
//! Three separations worth keeping:
//!
//! - **Movement is an intent, not a byte.** [`EditorPane::move_caret`] takes rows
//!   and columns so the Vim layer (P6-5) and the arrow keys reach the same code,
//!   and `h j k l` here is a keymap decision, not an editor one.
//! - **Painting answers in tokens, not colours.** A [`Run`] names a
//!   [`SyntaxToken`] and the pane's theme turns it into a terminal colour, which
//!   is the same rule the highlighter and the theme files already follow.
//! - **The prompt is a mode of the pane, not of the app.** `/` and `Ctrl-G` are
//!   the editor's, and while one is up every key is text: a find box that let `j`
//!   through would move the caret out from under the query being typed.
//!
//! Insert mode, saving through the daemon and the diff review are P6-5, P7-10 and
//! P6-9; the pane is read-only until then, and says so rather than pretending.

use acpilot_demo::EditorBuffer;
use acpilot_git::{LineChange, LineChanges};
// The theme's token set, not this crate's: a run names a token and the theme
// decides the colour, exactly as a theme file and the highlighter already agree
// to. `acpilot-extensions` re-exports the palette's, so this is the same type
// `Highlights` answers with.
use acpilot_extensions::theme::SyntaxToken;
use acpilot_text::highlight::{Grammars, Highlights, Span};
use acpilot_text::pane::{Cell, Find, Hit, Row, Viewport, bracket_at, column_at, parse_position};
use acpilot_text::{Document, LineColumn, Selection, Selections};
use std::ops::Range;

/// Which prompt is up, and what has been typed into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// `/` — a search, applied as it is typed.
    Find,
    /// `Ctrl-G` — a line and column, applied on Enter.
    Line,
}

/// The one-line box the editor takes its next question in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    /// What it is asking.
    pub kind: PromptKind,
    /// What has been typed.
    pub text: String,
    /// Where the caret was when the box opened, so `Esc` puts it back.
    origin: LineColumn,
}

/// What a cell is painted on top of, beyond its syntax.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    /// Nothing special.
    #[default]
    Plain,
    /// Part of a search match.
    Match,
    /// Part of the match the caret is on.
    CurrentMatch,
    /// One of the two brackets under the caret.
    Bracket,
    /// Covered by a selection a drag made.
    Selected,
}

/// One run of characters painted the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// The characters, tabs expanded.
    pub text: String,
    /// What they are, syntactically, if the grammar names them.
    pub token: Option<SyntaxToken>,
    /// What they are on top of that.
    pub state: State,
}

/// One line of the buffer as it will be painted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaintedRow {
    /// The line's number in the buffer, zero-based.
    pub line: usize,
    /// Its runs, left to right.
    pub runs: Vec<Run>,
    /// How many cells the runs cover, so a pane can paint the rest of the row
    /// with the current-line background instead of stopping at the last glyph.
    pub width: usize,
}

/// The editor pane: one document, a viewport, and the question it is asking.
pub struct EditorPane {
    path: String,
    language: String,
    document: Document,
    highlights: Option<Highlights>,
    viewport: Viewport,
    marks: LineChanges,
    find: Find,
    prompt: Option<Prompt>,
    message: String,
    rows: Vec<Row>,
    bracket: Option<(usize, usize)>,
    /// Where a drag started, while one is in progress.
    ///
    /// Held as an offset rather than as a `Selection` because the selection itself
    /// lives in the document, where an edit would move it: the anchor is the one end
    /// that must not move, and the caret is the other.
    anchor: Option<usize>,
}

/// A pane's debug shape: the file, the caret and the window.
///
/// Hand-written because a parse tree and a query cursor are not printable, and
/// because the two lines that *are* worth printing in a bug report are the ones
/// here.
impl std::fmt::Debug for EditorPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorPane")
            .field("path", &self.path)
            .field("language", &self.language)
            .field("caret", &self.caret())
            .field("viewport", &self.viewport)
            .field("rows", &self.rows.len())
            .finish_non_exhaustive()
    }
}

impl EditorPane {
    /// A pane over the seeded buffer, with its grammar compiled if this build
    /// ships one.
    ///
    /// The seed is the demo's: the same file, the same lines and the same cursor
    /// the design pass was argued with. What changed is that the text is now a
    /// rope the pane can edit rather than strings the pane drew, and that the
    /// gutter's markers are passed in — they are git's answer about a diff, not
    /// the buffer's opinion about itself.
    #[must_use]
    pub fn seeded(seed: &EditorBuffer, marks: LineChanges, grammars: &mut Grammars) -> Self {
        let text: Vec<&str> = seed.lines.iter().map(String::as_str).collect();
        let document = Document::from_bytes(text.join("\n").as_bytes())
            .expect("the seeded buffer is text the demo wrote");
        let highlights = grammars.get(&seed.language).map(Highlights::new);
        let caret = LineColumn::new(seed.cursor.0, seed.cursor.1);
        let mut pane = Self {
            path: seed.path.clone(),
            language: seed.language.clone(),
            document,
            highlights,
            viewport: Viewport::new(0, 0),
            marks,
            find: Find::new(),
            prompt: None,
            message: String::new(),
            rows: Vec::new(),
            bracket: None,
            anchor: None,
        };
        let at = pane.offset(caret);
        pane.document.set_caret(at);
        pane.settle();
        pane
    }

    /// The file's path, as the title and the status line spell it.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The tab's label: the file's own name.
    #[must_use]
    pub fn tab(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// The language the pane highlights with, or claims to.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    /// The document itself, for reading.
    #[must_use]
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// The search, whether or not its box is up.
    #[must_use]
    pub fn find(&self) -> &Find {
        &self.find
    }

    /// The rows on screen, as the viewport last measured them.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Where the window is on the document.
    #[must_use]
    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// The box that is asking a question, if one is.
    #[must_use]
    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    /// What the last key did, said in words for the status line.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The caret, zero-based.
    #[must_use]
    pub fn caret(&self) -> LineColumn {
        self.document
            .buffer()
            .line_column(self.document.selections().primary().head.at)
    }

    /// The caret as a person counts it: line 1, column 1.
    #[must_use]
    pub fn position(&self) -> (usize, usize) {
        let caret = self.caret();
        (caret.line + 1, caret.column + 1)
    }

    /// Where the caret is drawn, as `(row of the viewport, cell column)`, or
    /// `None` when it is outside the window.
    #[must_use]
    pub fn cursor(&self) -> Option<(usize, usize)> {
        self.viewport.caret(self.document.buffer(), self.caret())
    }

    /// The two matched brackets under the caret, when there is a pair.
    #[must_use]
    pub fn bracket(&self) -> Option<(usize, usize)> {
        self.bracket
    }

    /// The git marker for a line, or nothing for one that did not change.
    ///
    /// The letters are the gutter's, not git's: `+` for a line that is new, `~`
    /// for one that replaced something, `-` for the line above a gap. A single
    /// letter per column, because the gutter is one cell wide and a word would
    /// push the text out from under the numbers.
    #[must_use]
    pub fn mark(&self, line: usize) -> Option<char> {
        self.marks.get(line).map(|change| match change {
            LineChange::Added => '+',
            LineChange::Modified => '~',
            LineChange::Deleted => '-',
        })
    }

    /// What git says changed about this file, for the title and tests.
    #[must_use]
    pub fn changes(&self) -> &LineChanges {
        &self.marks
    }

    /// How many cells the gutter needs: the widest line number, a space, and the
    /// marker column.
    #[must_use]
    pub fn gutter(&self) -> usize {
        let digits = self.document.buffer().line_count().to_string().len().max(3);
        digits + 3
    }

    /// Tell the pane how much room it has.
    ///
    /// The text width is the pane's width minus the gutter, because the gutter is
    /// the pane's own furniture: a long line clipped at the pane's edge rather
    /// than at the gutter's would hide two columns behind the numbers.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let text = cols.saturating_sub(self.gutter()).max(1);
        let rows = rows.max(1);
        // Every frame calls this, and a frame that changed nothing should not
        // rebuild forty rows of cells: the size is the only thing a resize is
        // about, and a caret that moved has already built its own.
        if self.viewport.rows() == rows && self.viewport.cols() == text {
            return;
        }
        self.viewport.resize(rows, text);
        self.rebuild();
    }

    /// Reparse and re-measure after the text changed.
    pub fn settle(&mut self) {
        if let Some(highlights) = &mut self.highlights {
            highlights.sync(self.document.buffer());
        }
        self.rebuild();
    }

    /// Re-measure the window and the bracket under the caret.
    fn rebuild(&mut self) {
        self.viewport.reveal(self.document.buffer(), self.caret());
        self.rows = self.viewport.visible(self.document.buffer());
        self.bracket = bracket_at(self.document.buffer(), self.caret_offset());
    }

    /// The char offset a line and column name, clamped to the line.
    fn offset(&self, at: LineColumn) -> usize {
        self.document.buffer().offset(at)
    }

    /// The caret's offset in the buffer.
    fn caret_offset(&self) -> usize {
        self.document.selections().primary().head.at
    }

    /// Move the caret, clamped to the text, and scroll the least that shows it.
    ///
    /// A column past the end of the line lands at the line's end: arrows in a
    /// terminal editor move the caret, they do not add spaces. There is no goal
    /// column here either — remembering one across a run of Up and Down is a
    /// property of the key sequence (P6-5), not of the caret.
    pub fn move_caret(&mut self, rows: i32, cols: i32) {
        let caret = self.caret();
        let lines = self.document.buffer().line_count();
        let line = step(caret.line, rows).min(lines.saturating_sub(1));
        let width = self.document.buffer().line_range(line).len();
        let column = step(caret.column, cols).min(width);
        self.place(LineColumn::new(line, column));
    }

    /// Move a page at a time, keeping the caret's row on screen.
    pub fn page(&mut self, down: bool) {
        let rows = i32::try_from(self.viewport.rows().saturating_sub(1)).unwrap_or(i32::MAX);
        self.move_caret(if down { rows } else { -rows }, 0);
    }

    /// Scroll the window without moving the caret, which is what a wheel does.
    pub fn scroll(&mut self, lines: i32) {
        self.viewport
            .scroll_lines(self.document.buffer(), lines as isize);
        self.rows = self.viewport.visible(self.document.buffer());
    }

    /// Put the caret where a click landed, and anchor a selection there.
    ///
    /// `row` and `cell` are in the pane's own coordinates — the lines the viewport
    /// is showing and the cells the text was drawn in — because the border and the
    /// gutter are the pane's furniture and a caret placed from the window's
    /// coordinates is off by both. The column is the character under the cell, so
    /// a click inside a tab or a wide glyph lands on its left edge.
    ///
    /// A press is also the anchor a drag grows from, so the selection this starts is
    /// empty until the pointer moves: a click that selected nothing leaves no
    /// highlight behind, and a drag that wandered off resume where it began.
    ///
    /// Ignored while a prompt is up: the box has the keyboard, and a click that
    /// moved the caret out from under the query being typed would be a pane
    /// arguing with itself.
    pub fn click(&mut self, row: usize, cell: usize) {
        if self.prompt.is_some() {
            return;
        }
        let at = self.offset_at(row, cell);
        self.anchor = Some(at);
        self.document
            .set_selections(Selections::new(vec![Selection::new(at, at)], 0));
        self.rebuild();
        self.message.clear();
    }

    /// Grow the selection to the cell under the pointer.
    ///
    /// The head follows the pointer and the anchor stays where the press put it, so
    /// a drag can go back and forth over the same line and the selection follows it
    /// rather than accumulating. A drag that has left the pane is clamped to the edge
    /// it left by, which is what a pointer that has gone somewhere else should do.
    pub fn extend_to(&mut self, row: usize, cell: usize) {
        let Some(anchor) = self.anchor else {
            return;
        };
        let at = self.offset_at(row, cell);
        self.document
            .set_selections(Selections::new(vec![Selection::new(anchor, at)], 0));
        self.rebuild();
    }

    /// The characters a drag selected, or nothing when it selected none.
    #[must_use]
    pub fn selection(&self) -> Option<Range<usize>> {
        let range = self.document.selections().primary().range();
        (!range.is_empty()).then_some(range)
    }

    /// Whether a drag is holding a run of characters.
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.selection().is_some()
    }

    /// How many lines the selection touches, counting both ends.
    #[must_use]
    pub fn selected_lines(&self) -> usize {
        let Some(range) = self.selection() else {
            return 0;
        };
        let buffer = self.document.buffer();
        let start = buffer.line_column(range.start).line;
        let mut end = buffer.line_column(range.end).line;
        // A selection that stops at the very start of a line does not touch that
        // line: the newline before it is the last character selected, and a newline
        // belongs to the line above it.
        if end > start && buffer.line_range(end).start == range.end {
            end -= 1;
        }
        end - start + 1
    }

    /// Take the selection off the screen, leaving the caret where the head was.
    pub fn clear_selection(&mut self) {
        if self.anchor.take().is_none() && self.selection().is_none() {
            return;
        }
        let head = self.caret_offset();
        self.document.set_caret(head);
        self.rebuild();
        self.message.clear();
    }

    /// Copy what a drag selected, and say how much went.
    ///
    /// The text is the buffer's own slice, so the characters copied are the ones the
    /// frame painted — no cell arithmetic, no reconstruction from tabs and widths.
    /// The selection is dropped afterwards: what was copied is on the clipboard, and
    /// a highlight that stays after the copy is a pane still asking a question.
    pub fn yank(&mut self) -> Option<String> {
        let range = self.selection()?;
        let text = self.document.buffer().slice(range);
        let lines = self.selected_lines();
        self.anchor = None;
        let head = self.caret_offset();
        self.document.set_caret(head);
        self.rebuild();
        self.message = format!("copied {lines} line(s)");
        Some(text)
    }

    /// The character offset a pane cell names.
    ///
    /// One place, because a click and a drag land by the same arithmetic: the row is
    /// counted from the top of the window, and the cell from the left of the text
    /// (the horizontal scroll is the viewport's, not the pane's).
    fn offset_at(&self, row: usize, cell: usize) -> usize {
        let lines = self.document.buffer().line_count();
        let line = (self.viewport.top() + row).min(lines.saturating_sub(1));
        let cell = self.viewport.left() + cell;
        let column = column_at(
            self.document.buffer(),
            line,
            cell,
            self.viewport.tab_width(),
        );
        self.offset(LineColumn::new(line, column))
    }

    /// Put the caret at the start or the end of its line.
    pub fn line_edge(&mut self, end: bool) {
        let caret = self.caret();
        let width = self.document.buffer().line_range(caret.line).len();
        self.place(LineColumn::new(caret.line, if end { width } else { 0 }));
    }

    /// Put the caret somewhere and show it.
    ///
    /// A motion collapses a selection rather than extending it: dragging is how this
    /// pane selects, and Shift-extension is the keymap layer's to add (P6-5).
    fn place(&mut self, at: LineColumn) {
        let offset = self.offset(at);
        self.anchor = None;
        self.document.set_caret(offset);
        self.rebuild();
    }

    /// Open the search box, seeded with the last query.
    pub fn search(&mut self) {
        let text = self.find.query().to_owned();
        self.open(PromptKind::Find, text);
        if !self.find.is_empty() {
            self.message = matches(&self.find);
        }
    }

    /// Open the go-to-line box.
    pub fn go_to_line(&mut self) {
        self.open(PromptKind::Line, String::new());
        self.message = "line[:column]".to_owned();
    }

    /// Open a prompt, remembering where the caret was.
    fn open(&mut self, kind: PromptKind, text: String) {
        self.prompt = Some(Prompt {
            kind,
            text,
            origin: self.caret(),
        });
    }

    /// Whether a prompt has the keyboard.
    #[must_use]
    pub fn is_prompting(&self) -> bool {
        self.prompt.is_some()
    }

    /// Type a character into the open prompt.
    ///
    /// A find box searches as it is typed, which is the only version of `/` that
    /// is worth having: the matches are on screen before Enter is pressed, so a
    /// query that finds nothing says so while it is still being fixed.
    pub fn prompt_insert(&mut self, character: char) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        prompt.text.push(character);
        if prompt.kind == PromptKind::Find {
            let text = prompt.text.clone();
            let from = self.caret_offset();
            self.find.set_query(self.document.buffer(), &text, from);
            self.jump_to_current();
            self.message = matches(&self.find);
        }
    }

    /// Take the last character back out of the open prompt.
    pub fn prompt_backspace(&mut self) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        prompt.text.pop();
        if prompt.kind == PromptKind::Find {
            let text = prompt.text.clone();
            let from = self.caret_offset();
            self.find.set_query(self.document.buffer(), &text, from);
            self.jump_to_current();
            self.message = matches(&self.find);
        }
    }

    /// Run what the prompt asks for. Returns whether it was answered: `false`
    /// means the box is still up, because what was typed was not an answer.
    pub fn prompt_apply(&mut self) -> bool {
        let Some(prompt) = self.prompt.take() else {
            return true;
        };
        match prompt.kind {
            PromptKind::Find => {
                self.message = matches(&self.find);
                true
            }
            PromptKind::Line => match parse_position(&prompt.text) {
                Some(at) => {
                    self.place(at);
                    self.message = format!("line {}", at.line + 1);
                    true
                }
                None => {
                    // Left open on purpose: a typo is fixed in the box, not by
                    // opening it again.
                    self.message = format!("not a line: {}", prompt.text);
                    self.prompt = Some(prompt);
                    false
                }
            },
        }
    }

    /// Close the prompt and undo what it did.
    ///
    /// `Esc` in a find box puts the caret back where it started, because a search
    /// that was abandoned should not have moved anything — the box is a
    /// question, and "no" is an answer.
    pub fn prompt_cancel(&mut self) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        if prompt.kind == PromptKind::Find {
            self.find = Find::new();
            self.place(prompt.origin);
        }
        self.message.clear();
    }

    /// Step to the next or previous match of the current query.
    pub fn step_match(&mut self, forward: bool) {
        if self.find.is_empty() {
            self.message = "no search — press /".to_owned();
            return;
        }
        match self.find.step(forward) {
            Some(at) => {
                let position = self.document.buffer().line_column(at);
                self.place(position);
                self.message = matches(&self.find);
            }
            None => self.message = "no matches".to_owned(),
        }
    }

    /// Put the caret on the current match without re-searching.
    fn jump_to_current(&mut self) {
        if let Some(range) = self.find.current() {
            let position = self.document.buffer().line_column(range.start);
            let offset = self.offset(position);
            self.document.set_caret(offset);
            self.rebuild();
        }
    }

    /// The caret's line, for the current-line highlight.
    #[must_use]
    pub fn current_line(&self) -> usize {
        self.caret().line
    }

    /// Paint the rows on screen.
    ///
    /// The parse is asked for the bytes of the visible lines only, then each cell
    /// is given its token, its match state and its bracket state and neighbours
    /// with the same answer are joined into one [`Run`]. A frame therefore costs
    /// what the screen costs, not what the file costs.
    pub fn paint(&mut self) -> Vec<PaintedRow> {
        let Some(highlights) = &mut self.highlights else {
            return self.plain();
        };
        let (first, last) = match (self.rows.first(), self.rows.last()) {
            (Some(first), Some(last)) => (first.line, last.line),
            _ => return Vec::new(),
        };
        let buffer = self.document.buffer();
        let range = buffer.line_range(first).start..buffer.line_range(last).end;
        let spans: Vec<Span> = highlights.spans(buffer, range).to_vec();
        let bracket = self.bracket;
        let find = &self.find;
        let selection = self.selection();
        self.rows
            .iter()
            .map(|row| PaintedRow {
                line: row.line,
                runs: runs(row, &spans, bracket, find, selection.as_ref()),
                width: row.width(),
            })
            .collect()
    }

    /// Every row with no token at all: a language this build has no grammar for.
    fn plain(&self) -> Vec<PaintedRow> {
        let bracket = self.bracket;
        let find = &self.find;
        let selection = self.selection();
        self.rows
            .iter()
            .map(|row| PaintedRow {
                line: row.line,
                runs: runs(row, &[], bracket, find, selection.as_ref()),
                width: row.width(),
            })
            .collect()
    }
}

/// One row's cells, joined into runs of one token and one state.
fn runs(
    row: &Row,
    spans: &[Span],
    bracket: Option<(usize, usize)>,
    find: &Find,
    selection: Option<&Range<usize>>,
) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::with_capacity(row.cells.len());
    for cell in &row.cells {
        let (token, state) = appearance(cell, spans, bracket, find, selection);
        match runs.last_mut() {
            Some(run) if run.token == token && run.state == state => run.text.push(cell.ch),
            _ => runs.push(Run {
                text: cell.ch.to_string(),
                token,
                state,
            }),
        }
    }
    runs
}

/// The token and the state one cell is painted with.
fn appearance(
    cell: &Cell,
    spans: &[Span],
    bracket: Option<(usize, usize)>,
    find: &Find,
    selection: Option<&Range<usize>>,
) -> (Option<SyntaxToken>, State) {
    // A blank past the end of a line belongs to no character, so it cannot be part
    // of a selection: dragging over the end of a line copies the line's text, which
    // is what a selection means.
    let Some(at) = cell.at else {
        return (None, State::Plain);
    };
    let token = {
        let index = spans.partition_point(|span| span.range.end <= at);
        spans
            .get(index)
            .filter(|span| span.range.start <= at)
            .map(|span| span.token)
    };
    let state = if selection.is_some_and(|range| range.contains(&at)) {
        State::Selected
    } else if bracket.is_some_and(|(open, close)| at == open || at == close) {
        State::Bracket
    } else {
        match find.hit(at) {
            Hit::Match => State::Match,
            Hit::Current => State::CurrentMatch,
            Hit::No => State::Plain,
        }
    };
    (token, state)
}

/// How a search reads on the status line: `3/7`, or `no matches`.
fn matches(find: &Find) -> String {
    match (find.position(), find.matches().len()) {
        (_, 0) => "no matches".to_owned(),
        (Some(at), total) => format!("{at}/{total}"),
        (None, total) => format!("{total} matches"),
    }
}

/// `value` moved by `delta`, stopping at zero.
fn step(value: usize, delta: i32) -> usize {
    if delta >= 0 {
        value.saturating_add(delta.unsigned_abs() as usize)
    } else {
        value.saturating_sub(delta.unsigned_abs() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_git::line_changes;

    /// A patch that replaces line 2 of the test file, which is the line the
    /// helper's gutter has to mark: the marks are the diff's, not the buffer's.
    const PATCH: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -2,1 +2,1 @@\n-old\n+new\n";

    fn pane(lines: &[&str], cursor: (usize, usize)) -> EditorPane {
        let seed = EditorBuffer {
            path: "src/main.rs".to_owned(),
            language: "rust".to_owned(),
            lines: lines.iter().map(|text| (*text).to_owned()).collect(),
            cursor,
        };
        let mut grammars = Grammars::new();
        let mut pane = EditorPane::seeded(&seed, line_changes(PATCH, "src/main.rs"), &mut grammars);
        pane.resize(10, 60);
        pane
    }

    fn text(pane: &EditorPane) -> String {
        pane.document().text()
    }

    #[test]
    fn the_seeded_buffer_is_a_document_with_a_cursor_the_status_line_can_read() {
        let pane = pane(&["fn main() {", "    let x = 1;", "}"], (1, 4));
        assert_eq!(pane.tab(), "main.rs");
        assert_eq!(pane.position(), (2, 5));
        assert_eq!(text(&pane), "fn main() {\n    let x = 1;\n}");
        assert_eq!(pane.mark(1), Some('~'), "the line the patch replaced");
        assert_eq!(pane.mark(0), None);
    }

    #[test]
    fn the_gutter_marks_are_gits_answer_about_the_diff() {
        // A patch that adds two lines and removes one, read for the file it names:
        // one removal paired with one addition is a change, the addition left
        // over is new, and the file the patch is about is the only one it answers
        // for.
        let patch =
            "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,3 @@\n one\n-two\n+TWO\n+three\n";
        let marks = line_changes(patch, "src/main.rs");
        assert_eq!(marks.get(1), Some(LineChange::Modified), "{marks:?}");
        assert_eq!(marks.get(2), Some(LineChange::Added), "{marks:?}");
        assert!(
            line_changes(patch, "src/other.rs").is_empty(),
            "a patch says which file it is about"
        );
    }

    #[test]
    fn movement_stops_at_the_ends_of_the_text_and_of_the_line() {
        let mut pane = pane(&["one", "three", "x"], (0, 0));
        pane.move_caret(0, 99);
        assert_eq!(pane.caret(), LineColumn::new(0, 3), "no spaces are added");
        pane.move_caret(1, 0);
        assert_eq!(pane.caret(), LineColumn::new(1, 3));
        pane.move_caret(-99, -99);
        assert_eq!(pane.caret(), LineColumn::new(0, 0));
        pane.move_caret(99, 0);
        assert_eq!(pane.caret().line, 2, "the last line, not past it");
    }

    #[test]
    fn a_caret_moved_off_the_right_scrolls_the_view() {
        let long = "x".repeat(120);
        let mut pane = pane(&[long.as_str(), "short"], (0, 0));
        pane.move_caret(0, 100);
        assert!(pane.viewport().left() > 0, "the window followed the caret");
        assert_eq!(pane.cursor().map(|(row, _)| row), Some(0));
        assert_eq!(
            pane.cursor().map(|(_, column)| column),
            Some(pane.viewport().cols() - 1),
            "the caret sits at the right edge"
        );
        assert_eq!(pane.position(), (1, 101));
    }

    #[test]
    fn only_the_visible_rows_are_painted_and_the_current_line_is_known() {
        let lines: Vec<String> = (0..100).map(|line| format!("line {line}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let pane = pane(&refs, (0, 0));
        assert_eq!(pane.rows().len(), 10);
        assert_eq!(pane.current_line(), 0);
        assert_eq!(pane.rows()[3].text(), "line 3");
    }

    #[test]
    fn scrolling_moves_the_window_without_moving_the_caret() {
        let lines: Vec<String> = (0..50).map(|line| format!("l{line}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut pane = pane(&refs, (0, 0));
        pane.scroll(5);
        assert_eq!(pane.viewport().top(), 5);
        assert_eq!(pane.caret().line, 0, "the caret stayed put");
        assert_eq!(pane.rows()[0].text(), "l5");
    }

    #[test]
    fn a_click_puts_the_caret_on_the_cell_it_landed_on() {
        let mut pane = pane(&["\tlet x = 1;", "short"], (0, 0));
        // A cell inside the tab is the tab's own character, so the caret lands on
        // its left edge: a click is a cell and a caret is a character.
        pane.click(0, 2);
        assert_eq!(pane.caret(), LineColumn::new(0, 0));
        pane.click(0, 4);
        assert_eq!(pane.caret(), LineColumn::new(0, 1), "the `l` of `let`");
        // Past the end of a line is the end of the line, and a row past the last
        // line is the last line: a click is never a panic.
        pane.click(1, 40);
        assert_eq!(pane.caret(), LineColumn::new(1, 5));
        pane.click(99, 0);
        assert_eq!(pane.caret(), LineColumn::new(1, 0));
    }

    #[test]
    fn a_click_is_a_cell_of_the_text_not_of_the_pane() {
        // The pane is scrolled right, so the cells it was given start at the
        // column the window does — a click placed from the window's own column
        // would put the caret two cells left of the glyph under the pointer.
        let long = "0123456789".repeat(12);
        let mut pane = pane(&[long.as_str(), "short"], (0, 0));
        pane.move_caret(0, 100);
        let left = pane.viewport().left();
        assert!(left > 0, "the caret scrolled the window");
        pane.click(0, 2);
        assert_eq!(pane.caret(), LineColumn::new(0, left + 2));
    }

    #[test]
    fn a_drag_selects_the_run_it_covered_and_y_takes_it() {
        let mut pane = pane(&["one two", "three"], (0, 0));
        pane.click(0, 0);
        pane.extend_to(0, 3);
        assert_eq!(pane.selection(), Some(0..3), "one word, no newline");
        assert_eq!(pane.selected_lines(), 1);

        let copied = pane.yank().expect("a selection to copy");
        assert_eq!(copied, "one");
        assert!(!pane.has_selection(), "the copy is the end of the question");
        assert_eq!(pane.message(), "copied 1 line(s)");
        assert_eq!(pane.caret(), LineColumn::new(0, 3), "the caret is the head");
    }

    #[test]
    fn a_selection_across_lines_copies_both_of_them() {
        let mut pane = pane(&["one two", "three"], (0, 0));
        pane.click(0, 4);
        pane.extend_to(1, 2);
        assert_eq!(pane.selection(), Some(4..10));
        assert_eq!(pane.selected_lines(), 2);
        assert_eq!(pane.yank().as_deref(), Some("two\nth"));
        assert_eq!(pane.message(), "copied 2 line(s)");
    }

    #[test]
    fn a_selection_that_stops_at_a_line_start_does_not_count_that_line() {
        let mut pane = pane(&["one", "two", "three"], (0, 0));
        pane.click(0, 0);
        // The end of the first line is the start of the second, and the newline
        // between them is the last character selected.
        pane.extend_to(1, 0);
        assert_eq!(pane.selected_lines(), 1);
        assert_eq!(pane.yank().as_deref(), Some("one\n"));
    }

    #[test]
    fn a_drag_back_up_keeps_the_anchor_and_the_selection_shrinks() {
        // A pointer that goes back over what it just selected deselects it, which is
        // the difference between a selection and a smear.
        let mut pane = pane(&["abcdef"], (0, 0));
        pane.click(0, 4);
        pane.extend_to(0, 1);
        assert_eq!(pane.selection(), Some(1..4));
        assert_eq!(pane.yank().as_deref(), Some("bcd"));
    }

    #[test]
    fn a_click_that_selected_nothing_leaves_no_selection() {
        let mut pane = pane(&["one two"], (0, 0));
        pane.click(0, 2);
        assert!(!pane.has_selection(), "a press is not a selection yet");
        pane.clear_selection();
        assert_eq!(pane.caret(), LineColumn::new(0, 2), "and the caret stays");
    }

    #[test]
    fn a_drag_past_the_end_of_the_pane_selects_to_the_edge() {
        let mut pane = pane(&["one", "two", "three"], (0, 0));
        pane.click(0, 0);
        pane.extend_to(9, 99);
        assert_eq!(
            pane.selection(),
            Some(0..13),
            "the last line's text is the edge"
        );
        assert!(pane.cursor().is_some(), "and the head is on screen");
    }

    #[test]
    fn a_selected_cell_is_painted_as_selected() {
        let mut pane = pane(&["let x = 1;", "more"], (0, 0));
        pane.click(0, 0);
        pane.extend_to(0, 2);
        let painted = pane.paint();
        let first = &painted[0];
        let states: Vec<State> = first.runs.iter().map(|run| run.state).collect();
        assert_eq!(states.first(), Some(&State::Selected), "{states:?}");
        assert!(
            states.contains(&State::Plain),
            "only the selection is marked: {states:?}"
        );
        let joined: String = first.runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(joined, "let x = 1;", "painting never loses a character");
        assert!(
            painted[1].runs.iter().all(|run| run.state == State::Plain),
            "the line after the selection is untouched"
        );
    }

    #[test]
    fn the_gutter_is_wide_enough_for_the_longest_line_number() {
        let lines: Vec<String> = (0..1500).map(|line| format!("l{line}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let pane = pane(&refs, (0, 0));
        assert_eq!(
            pane.gutter(),
            7,
            "four digits, a space and the marker column"
        );
    }

    #[test]
    fn a_search_highlights_its_matches_and_steps_through_them() {
        let mut pane = pane(&["let a = 1;", "let b = 2;", "let a = 3;"], (0, 0));
        pane.search();
        for character in "let".chars() {
            pane.prompt_insert(character);
        }
        assert_eq!(pane.find().matches().len(), 3);
        assert_eq!(pane.message(), "1/3");
        assert_eq!(pane.caret(), LineColumn::new(0, 0));
        pane.prompt_apply();
        assert!(!pane.is_prompting());
        pane.step_match(true);
        assert_eq!(pane.caret().line, 1, "the caret went to the second match");
        assert_eq!(pane.message(), "2/3");
        pane.step_match(true);
        pane.step_match(true);
        assert_eq!(pane.caret().line, 0, "and wrapped");
    }

    #[test]
    fn escapes_puts_the_caret_back_where_the_search_started() {
        let mut pane = pane(&["one", "two", "one"], (2, 0));
        pane.search();
        pane.prompt_insert('t');
        pane.prompt_insert('w');
        assert_eq!(pane.caret().line, 1, "the caret jumped to the match");
        pane.prompt_cancel();
        assert_eq!(pane.caret(), LineColumn::new(2, 0));
        assert!(pane.find().is_empty(), "the abandoned search is gone");
    }

    #[test]
    fn go_to_line_reads_a_position_and_says_so_when_it_cannot() {
        let lines: Vec<String> = (0..40).map(|line| format!("l{line}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut pane = pane(&refs, (0, 0));
        pane.go_to_line();
        for character in "30:3".chars() {
            pane.prompt_insert(character);
        }
        assert!(pane.prompt_apply());
        assert_eq!(pane.position(), (30, 3));
        assert_eq!(pane.viewport().top(), 20, "the line is on screen");

        pane.go_to_line();
        for character in "nope".chars() {
            pane.prompt_insert(character);
        }
        assert!(!pane.prompt_apply(), "the box stays open");
        assert!(pane.is_prompting());
        assert_eq!(pane.message(), "not a line: nope");
    }

    #[test]
    fn the_bracket_under_the_caret_and_its_match_are_both_known() {
        let mut pane = pane(&["fn main() {", "    a(b)", "}"], (0, 10));
        assert_eq!(
            pane.bracket(),
            Some((10, 21)),
            "the braces, from the opener"
        );
        pane.place(LineColumn::new(1, 7));
        assert_eq!(pane.bracket(), Some((17, 19)), "`(b)` seen from the closer");
        pane.place(LineColumn::new(1, 8));
        assert_eq!(
            pane.bracket(),
            Some((17, 19)),
            "the caret just past the closer is still on it"
        );
    }

    #[test]
    fn a_rust_buffer_is_painted_in_tokens_and_every_cell_keeps_its_character() {
        let mut pane = pane(&["fn main() {", "    let x = 1; // hi", "}"], (0, 0));
        let painted = pane.paint();
        assert_eq!(painted.len(), 3);
        let first = &painted[0];
        assert!(
            first
                .runs
                .iter()
                .any(|run| run.text == "fn" && run.token == Some(SyntaxToken::Keyword)),
            "{:?}",
            first.runs
        );
        let joined: String = first.runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(joined, "fn main() {", "painting never loses a character");
        assert!(
            painted[1]
                .runs
                .iter()
                .any(|run| run.token == Some(SyntaxToken::Comment)),
            "{:?}",
            painted[1].runs
        );
    }

    #[test]
    fn a_match_is_painted_as_a_match_and_the_current_one_apart() {
        let mut pane = pane(&["one two", "two one"], (0, 0));
        pane.search();
        for character in "two".chars() {
            pane.prompt_insert(character);
        }
        pane.prompt_apply();
        let painted = pane.paint();
        let states: Vec<State> = painted
            .iter()
            .flat_map(|row| row.runs.iter().map(|run| run.state))
            .collect();
        assert!(states.contains(&State::CurrentMatch), "{states:?}");
        assert!(states.contains(&State::Match), "{states:?}");
    }

    #[test]
    fn a_language_with_no_grammar_is_still_drawn() {
        let mut grammars = Grammars::new();
        let seed = EditorBuffer {
            path: "Makefile".to_owned(),
            language: "make".to_owned(),
            lines: vec!["all:\n\techo hi".to_owned()],
            cursor: (0, 0),
        };
        let mut pane = EditorPane::seeded(&seed, LineChanges::default(), &mut grammars);
        pane.resize(4, 40);
        let painted = pane.paint();
        assert_eq!(painted.len(), 2, "the seeded line held a newline");
        assert!(painted[0].runs.iter().all(|run| run.token.is_none()));
    }
}
