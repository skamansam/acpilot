//! Undo and redo: one step per thing a person did.
//!
//! The history stores what an edit *was*, not a copy of the text, so a step
//! costs a few range-and-string pairs rather than a document. One path in and one
//! path out is also why a multi-cursor insert, an indent of nine lines and a
//! paste are each exactly one undo — they are each one [`Change`].
//!
//! A run of single characters is one step, which is what a person expects from
//! Ctrl-Z after typing a word. The run ends at the first of:
//!
//! - [`GROUP_WINDOW`] (500 ms) passing since the last character,
//! - the character class changing, where a "word" is [`crate::buffer::is_word`],
//!   so `foo bar` is three steps and undo takes `bar` first,
//! - the caret not being contiguous with where the last character went, which is
//!   how a click, an arrow key or a second cursor ends the run,
//! - anything that is not a single character: a paste, a deletion of a word, an
//!   indent.
//!
//! Time is passed in rather than read from the clock inside, because a rule about
//! 500 ms is otherwise untestable without sleeping for half a second.

use std::ops::Range;
use std::time::{Duration, Instant};

use crate::buffer::{Buffer, is_word};
use crate::edit::Replacement;

/// How long a run of single characters still counts as one step.
pub const GROUP_WINDOW: Duration = Duration::from_millis(500);

/// The default number of steps the history keeps.
pub const DEFAULT_LIMIT: usize = 256;

/// What a change was, for the purposes of grouping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavour {
    /// One character typed — or one pasted, in which case it is not an
    /// [`Flavour::Insert`]: only a single typed character groups.
    Insert,
    /// One character removed.
    Delete,
    /// Anything else: a paste, several replacements at once, an indent.
    Other,
}

/// One applied edit: what it replaced, and how to put that back.
///
/// The inverse is computed when the change is made, against the text that was
/// there, because by the time undo runs that text is gone.
#[derive(Clone, Debug)]
pub struct Change {
    /// In ascending order, against the text as it was before this change.
    forward: Vec<Replacement>,
    /// In ascending order, against the text as it was after this change.
    inverse: Vec<Replacement>,
    flavour: Flavour,
    /// The character at the seam: the one typed, or the one removed. `None` when
    /// the change is not a single character, which is what stops it grouping.
    edge: Option<char>,
}

impl Change {
    /// The change a set of replacements makes, or `None` when they change
    /// nothing.
    ///
    /// `replacements` must not overlap, and must be the ones the buffer is about
    /// to apply; they may be in any order.
    #[must_use]
    pub fn between(buffer: &Buffer, replacements: Vec<Replacement>) -> Option<Self> {
        let mut forward: Vec<Replacement> = replacements
            .into_iter()
            .filter(|replacement| !replacement.is_noop())
            .collect();
        if forward.is_empty() {
            return None;
        }
        forward.sort_by_key(|replacement| replacement.range.start);

        let mut inverse: Vec<Replacement> = Vec::with_capacity(forward.len());
        // The inverse is read against the text this change produced, so every
        // offset after an earlier replacement is shifted by what that one did.
        let mut shift = 0isize;
        for replacement in &forward {
            // Read before the edit lands: this is the text undo has to put back.
            let removed = buffer.slice(replacement.range.clone());
            let start = (replacement.range.start as isize + shift) as usize;
            match inverse.last_mut() {
                // Two deletions that collapse onto one point — a backspace run
                // over the end of a line, or two carets next to each other — go
                // back as one insertion: two insertions at the same offset have
                // no order, and whichever went second would put its text first.
                Some(last) if last.range.is_empty() && last.range.start == start => {
                    last.text.push_str(&removed);
                }
                _ => inverse.push(Replacement::of(
                    start..start + replacement.inserted(),
                    removed,
                )),
            }
            shift += replacement.delta();
        }
        let (flavour, edge) = classify(&forward, buffer);

        Some(Self {
            forward,
            inverse,
            flavour,
            edge,
        })
    }

    /// What the buffer has to do to make this change.
    #[must_use]
    pub fn forward(&self) -> &[Replacement] {
        &self.forward
    }

    /// What it has to do to take it back.
    #[must_use]
    pub fn inverse(&self) -> &[Replacement] {
        &self.inverse
    }

    /// What kind of change this is.
    #[must_use]
    pub fn flavour(&self) -> Flavour {
        self.flavour
    }

    /// How many characters this change put in.
    fn inserted(&self) -> usize {
        self.forward.iter().map(Replacement::inserted).sum()
    }

    /// The characters this change covers, in the text before it.
    fn span(&self) -> Range<usize> {
        let start = self
            .forward
            .first()
            .map_or(0, |replacement| replacement.range.start);
        let end = self
            .forward
            .last()
            .map_or(start, |replacement| replacement.range.end.max(start));
        start..end
    }
}

/// One single character is an insert or a delete; anything else is not.
fn classify(forward: &[Replacement], buffer: &Buffer) -> (Flavour, Option<char>) {
    let [only] = forward else {
        return (Flavour::Other, None);
    };
    if only.range.is_empty() {
        let mut characters = only.text.chars();
        return match (characters.next(), characters.next()) {
            (Some(character), None) => (Flavour::Insert, Some(character)),
            _ => (Flavour::Other, None),
        };
    }
    if only.text.is_empty() && only.removed() == 1 {
        return match buffer.char_at(only.range.start) {
            Some(character) => (Flavour::Delete, Some(character)),
            None => (Flavour::Other, None),
        };
    }
    (Flavour::Other, None)
}

/// One step: a change, plus the single characters that joined it.
#[derive(Debug)]
struct Group {
    changes: Vec<Change>,
    flavour: Flavour,
    /// The character at the seam of the latest change.
    edge: char,
    /// The range the latest change produced, in the text after it — where a
    /// continuation has to start. Empty for a deletion, whose caret is the seam.
    seam: Range<usize>,
    /// When the latest change arrived.
    at: Instant,
    /// Where the caret belongs after this whole step is undone.
    before: usize,
    /// And after it is redone.
    after: usize,
}

impl Group {
    fn new(change: Change, at: Instant) -> Self {
        let span = change.span();
        let after = match change.flavour {
            // A deletion leaves the caret where the text used to start.
            Flavour::Delete => span.start,
            _ => span.start + change.inserted(),
        };
        Self {
            flavour: change.flavour,
            edge: change.edge.unwrap_or(' '),
            seam: span.start..span.start + change.inserted(),
            at,
            before: span.start,
            after,
            changes: vec![change],
        }
    }

    /// Whether this change continues the step, and if so, take it.
    fn absorb(&mut self, change: &Change, at: Instant) -> bool {
        if self.flavour != change.flavour
            || change.flavour == Flavour::Other
            || at.saturating_duration_since(self.at) > GROUP_WINDOW
        {
            return false;
        }
        let Some(edge) = change.edge else {
            return false;
        };
        if is_word(edge) != is_word(self.edge) {
            return false;
        }
        let span = change.span();
        let contiguous = match change.flavour {
            // Typing, and a backspace run walking left, or Delete walking right.
            Flavour::Insert => span.start == self.seam.end,
            Flavour::Delete => span.start == self.seam.start || span.end == self.seam.start,
            Flavour::Other => false,
        };
        if !contiguous {
            return false;
        }
        let after = match change.flavour {
            Flavour::Delete => span.start,
            _ => span.start + change.inserted(),
        };
        // The seam is where the next character has to go, which is the text this
        // change put in, not the text it took out.
        self.seam = span.start..span.start + change.inserted();
        self.after = after;
        self.edge = edge;
        self.at = at;
        true
    }
}

/// The undo stack, and the steps that can be redone.
///
/// A new change drops what was undone: an editor that lets you undo, type, and
/// then redo somebody else's future is an editor that loses work.
#[derive(Debug)]
pub struct History {
    groups: Vec<Group>,
    undone: Vec<Group>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    /// An empty history keeping [`DEFAULT_LIMIT`] steps.
    #[must_use]
    pub fn new() -> Self {
        Self {
            groups: Vec::new(),
            undone: Vec::new(),
            limit: DEFAULT_LIMIT,
        }
    }

    /// An empty history keeping `limit` steps, at least one.
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            groups: Vec::new(),
            undone: Vec::new(),
            limit: limit.max(1),
        }
    }

    /// Record a change, joining the open step when it continues one.
    ///
    /// Returns whether it joined, which is what a caller needs to know to tell a
    /// new undo step from a longer one — a status line saying "3 steps" after
    /// typing three characters is a lie worth not telling.
    pub fn record(&mut self, change: Change, at: Instant) -> bool {
        self.undone.clear();
        if let Some(group) = self.groups.last_mut()
            && group.absorb(&change, at)
        {
            group.changes.push(change);
            return true;
        }
        self.groups.push(Group::new(change, at));
        while self.groups.len() > self.limit {
            self.groups.remove(0);
        }
        false
    }

    /// Take the last step back.
    ///
    /// Returns where the caret belongs afterwards — the seam the step started at
    /// — or `None` when there is nothing left to undo.
    pub fn undo(&mut self, buffer: &mut Buffer) -> Option<usize> {
        let group = self.groups.pop()?;
        for change in group.changes.iter().rev() {
            buffer.apply(&change.inverse);
        }
        let caret = group.before;
        self.undone.push(group);
        Some(caret)
    }

    /// Put the last undone step back.
    ///
    /// Returns where the caret belongs afterwards: past the text a step inserted,
    /// or at the point a step removed it from.
    pub fn redo(&mut self, buffer: &mut Buffer) -> Option<usize> {
        let group = self.undone.pop()?;
        for change in &group.changes {
            buffer.apply(&change.forward);
        }
        let caret = group.after;
        self.groups.push(group);
        Some(caret)
    }

    /// Whether there is a step to take back.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.groups.is_empty()
    }

    /// Whether there is a step to put back.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// How many steps are on the stack.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.groups.len()
    }

    /// How many steps this history keeps.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Forget everything, both directions.
    pub fn clear(&mut self) {
        self.groups.clear();
        self.undone.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Edit;

    fn base() -> Instant {
        Instant::now()
    }

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    /// Apply an edit and record it, the way the document does.
    ///
    /// Returns whether the change joined the open step — `false` when it started
    /// a new one, or when there was nothing to do.
    fn apply(buffer: &mut Buffer, history: &mut History, edit: Edit, when: Instant) -> bool {
        let replacements = edit.replacements(buffer);
        let Some(change) = Change::between(buffer, replacements) else {
            return false;
        };
        buffer.apply(change.forward());
        history.record(change, when)
    }

    fn typed(buffer: &mut Buffer, history: &mut History, text: &str, base: Instant) -> Vec<bool> {
        text.chars()
            .enumerate()
            .map(|(index, character)| {
                let place = buffer.char_len();
                apply(
                    buffer,
                    history,
                    Edit::insert(place, character.to_string()),
                    at(base, index as u64 * 100),
                )
            })
            .collect()
    }

    #[test]
    fn a_run_of_characters_is_one_step_until_the_clock_runs_out() {
        let base = base();
        let mut buffer = Buffer::new();
        let mut history = History::new();

        let joined = typed(&mut buffer, &mut history, "foo", base);
        assert_eq!(
            joined,
            vec![false, true, true],
            "one step, two continuations"
        );
        assert_eq!(history.depth(), 1);
        assert_eq!(buffer.text(), "foo");

        // A long pause ends the run.
        apply(
            &mut buffer,
            &mut history,
            Edit::insert(3, "x"),
            at(base, 900),
        );
        assert_eq!(history.depth(), 2);

        assert_eq!(
            history.undo(&mut buffer),
            Some(3),
            "the second run is undone"
        );
        assert_eq!(buffer.text(), "foo");
        assert_eq!(history.undo(&mut buffer), Some(0));
        assert_eq!(buffer.text(), "");
        assert_eq!(history.undo(&mut buffer), None);
    }

    #[test]
    fn a_word_boundary_ends_the_run_so_undo_takes_the_last_word_first() {
        let base = base();
        let mut buffer = Buffer::new();
        let mut history = History::new();
        typed(&mut buffer, &mut history, "foo bar", base);

        assert_eq!(history.depth(), 3, "`foo`, the space, and `bar`");
        assert_eq!(buffer.text(), "foo bar");
        assert_eq!(history.undo(&mut buffer), Some(4));
        assert_eq!(buffer.text(), "foo ");
        assert_eq!(history.undo(&mut buffer), Some(3));
        assert_eq!(buffer.text(), "foo");
        assert_eq!(history.undo(&mut buffer), Some(0));
        assert_eq!(buffer.text(), "");
    }

    #[test]
    fn a_caret_that_is_not_contiguous_starts_a_new_step() {
        let base = base();
        let mut buffer = Buffer::from_text("one two");
        let mut history = History::new();

        // Two characters at the very end, then two more somewhere else: not a
        // run, because nothing was typed between them.
        apply(&mut buffer, &mut history, Edit::insert(7, "!"), base);
        apply(
            &mut buffer,
            &mut history,
            Edit::insert(0, "x"),
            at(base, 50),
        );
        assert_eq!(history.depth(), 2);
        assert_eq!(buffer.text(), "xone two!");

        apply(
            &mut buffer,
            &mut history,
            Edit::insert(8, "?"),
            at(base, 100),
        );
        assert_eq!(history.depth(), 3, "the caret was not where it left off");
    }

    #[test]
    fn backspacing_a_word_is_one_step_and_the_space_is_another() {
        let base = base();
        let mut buffer = Buffer::from_text("foo bar");
        let mut history = History::new();

        for (index, place) in [6, 5, 4].into_iter().enumerate() {
            apply(
                &mut buffer,
                &mut history,
                Edit::delete(place..place + 1),
                at(base, index as u64 * 50),
            );
        }
        assert_eq!(history.depth(), 1, "`bar` went in one step");
        assert_eq!(buffer.text(), "foo ");

        apply(&mut buffer, &mut history, Edit::delete(3..4), at(base, 200));
        assert_eq!(history.depth(), 2, "the space is its own step");
        assert_eq!(buffer.text(), "foo");
    }

    #[test]
    fn redo_puts_back_what_undo_took_and_a_new_edit_drops_it() {
        let base = base();
        let mut buffer = Buffer::new();
        let mut history = History::new();
        typed(&mut buffer, &mut history, "hello", base);

        assert_eq!(history.undo(&mut buffer), Some(0));
        assert_eq!(buffer.text(), "");
        assert!(history.can_redo());
        assert_eq!(history.redo(&mut buffer), Some(5));
        assert_eq!(buffer.text(), "hello");
        assert_eq!(history.redo(&mut buffer), None);

        assert_eq!(history.undo(&mut buffer), Some(0));
        apply(
            &mut buffer,
            &mut history,
            Edit::insert(0, "z"),
            at(base, 900),
        );
        assert_eq!(buffer.text(), "z");
        assert!(!history.can_redo(), "the branch that was undone is gone");
    }

    #[test]
    fn a_step_that_was_not_one_character_never_groups() {
        let base = base();
        let mut buffer = Buffer::new();
        let mut history = History::new();

        apply(&mut buffer, &mut history, Edit::insert(0, "hello"), base);
        apply(
            &mut buffer,
            &mut history,
            Edit::insert(5, "world"),
            at(base, 10),
        );
        assert_eq!(history.depth(), 2, "two pastes, not one");

        let mut buffer = Buffer::from_text("one\ntwo");
        let mut history = History::new();
        apply(
            &mut buffer,
            &mut history,
            Edit::indent(
                0..2,
                crate::edit::IndentMode::In,
                crate::edit::IndentStyle::Tabs,
            ),
            base,
        );
        apply(
            &mut buffer,
            &mut history,
            Edit::indent(
                0..2,
                crate::edit::IndentMode::In,
                crate::edit::IndentStyle::Tabs,
            ),
            at(base, 10),
        );
        assert_eq!(history.depth(), 2);
        assert_eq!(buffer.text(), "\t\tone\n\t\ttwo");
    }

    #[test]
    fn a_multi_cursor_edit_is_one_step_even_though_it_is_several_replacements() {
        let base = base();
        let mut buffer = Buffer::from_text("one\ntwo");
        let mut history = History::new();

        let replacements = vec![Replacement::of(0..0, "#"), Replacement::of(4..4, "#")];
        let change = Change::between(&buffer, replacements).expect("a change");
        assert_eq!(change.flavour(), Flavour::Other);
        buffer.apply(change.forward());
        history.record(change, base);
        assert_eq!(buffer.text(), "#one\n#two");

        assert_eq!(history.depth(), 1);
        assert_eq!(history.undo(&mut buffer), Some(0));
        assert_eq!(buffer.text(), "one\ntwo");
    }

    #[test]
    fn undo_takes_the_caret_to_the_seam_it_removed() {
        let base = base();
        let mut buffer = Buffer::from_text("one two");
        let mut history = History::new();

        apply(&mut buffer, &mut history, Edit::insert(3, " really"), base);
        assert_eq!(buffer.text(), "one really two");
        assert_eq!(
            history.undo(&mut buffer),
            Some(3),
            "the paste starts at 3 and so does the caret"
        );
        assert_eq!(buffer.text(), "one two");
        assert_eq!(history.redo(&mut buffer), Some(10), "past what it put in");
        assert_eq!(buffer.text(), "one really two");

        // A deletion puts the caret back where the text reappears.
        let mut buffer = Buffer::from_text("one two");
        let mut history = History::new();
        apply(&mut buffer, &mut history, Edit::delete(3..10), base);
        assert_eq!(buffer.text(), "one");
        assert_eq!(history.undo(&mut buffer), Some(3));
        assert_eq!(buffer.text(), "one two");
        assert_eq!(
            history.redo(&mut buffer),
            Some(3),
            "at the point it went from"
        );
    }

    #[test]
    fn a_change_that_changes_nothing_is_not_a_step() {
        let base = base();
        let mut buffer = Buffer::from_text("one");
        let mut history = History::new();

        apply(&mut buffer, &mut history, Edit::delete(1..1), base);
        apply(&mut buffer, &mut history, Edit::insert(1, ""), base);
        assert_eq!(buffer.text(), "one");
        assert!(Change::between(&buffer, Vec::new()).is_none());
        assert_eq!(history.depth(), 0);
        assert!(!history.can_undo());
    }

    #[test]
    fn the_oldest_steps_fall_off_the_end_of_the_stack() {
        let base = base();
        let mut buffer = Buffer::new();
        let mut history = History::with_limit(2);

        for index in 0..5u64 {
            let end = buffer.char_len();
            apply(
                &mut buffer,
                &mut history,
                Edit::insert(end, "x"),
                at(base, index * 1_000),
            );
        }
        assert_eq!(history.depth(), 2);
        assert_eq!(history.limit(), 2);
        history.undo(&mut buffer);
        history.undo(&mut buffer);
        assert_eq!(buffer.text(), "xxx", "the first three are out of reach");
        assert!(!history.can_undo());

        history.clear();
        assert!(!history.can_undo() && !history.can_redo());
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_start_a_new_step() {
        let base = base();
        let mut buffer = Buffer::new();
        let mut history = History::new();
        apply(
            &mut buffer,
            &mut history,
            Edit::insert(0, "a"),
            at(base, 500),
        );
        apply(&mut buffer, &mut history, Edit::insert(1, "b"), at(base, 0));
        assert_eq!(history.depth(), 1, "no time passed, not negative time");
    }
}
