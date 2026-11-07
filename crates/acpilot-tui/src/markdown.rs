//! Enough markdown to read an agent's reply.
//!
//! The transcript is a conversation, not a document: what a pane needs from a
//! reply is its shape — a heading is a heading, a bullet is a bullet, a fenced
//! block is code — so a person can find the paragraph they meant without a
//! renderer's worth of HTML. This module turns one entry's text into rows of
//! styled segments and decides nothing about colour: the token is
//! [`Emphasis`], and the pane maps it through the theme like every other style.
//!
//! # What is deliberately absent
//!
//! No tables, no nested lists, no HTML, no reference links. An agent that writes
//! them gets their markers as text, which is honest — a table silently dropped is
//! a table the person did not know was there. Fences and inline spans are the two
//! things a reply to a coding question cannot do without.
//!
//! The parse is line-oriented on purpose: rows are counted with
//! [`super::App::transcript_len`] without building them, so one line of markdown
//! is one row here, and a renderer that joined or split lines would make the
//! length a second answer.

/// How one run of a row is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    /// Ordinary prose.
    Plain,
    /// `**strong**`.
    Bold,
    /// `*italic*` or `_italic_`.
    Italic,
    /// A backtick span, or a line inside a fence.
    Code,
    /// `[text](url)`: the text, with the address as the target a terminal has no
    /// way to open.
    Link,
    /// A heading, with its hashes taken off.
    Heading,
    /// A quoted line, with its `>` turned into a bar.
    Quote,
}

/// One run of text with one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// What is drawn.
    pub text: String,
    /// How it is drawn.
    pub emphasis: Emphasis,
}

impl Segment {
    /// A run of ordinary text.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            emphasis: Emphasis::Plain,
        }
    }
}

/// One drawn row of a rendered entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Its runs, in order. Never empty: a blank line is one empty plain segment.
    pub segments: Vec<Segment>,
}

impl Line {
    /// The row as plain text, which is what a yank and a search read.
    #[must_use]
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }

    fn one(segment: Segment) -> Self {
        Self {
            segments: vec![segment],
        }
    }
}

/// Render one entry's text.
///
/// The number of lines returned is always the number of lines in `text`: a
/// marker is rewritten in place rather than dropped or moved, which is what lets
/// the pane count rows without rendering them.
#[must_use]
pub fn render(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut fenced = false;
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            // The fence itself is drawn rather than hidden: a code block that
            // began without a marker is indistinguishable from indented prose,
            // and the language is the one useful thing the marker carries.
            let marker = trimmed.trim_end();
            lines.push(Line::one(Segment {
                text: format!("  {marker}"),
                emphasis: Emphasis::Code,
            }));
            fenced = !fenced;
            continue;
        }
        if fenced {
            lines.push(Line::one(Segment {
                text: format!("  {line}"),
                emphasis: Emphasis::Code,
            }));
            continue;
        }
        lines.push(render_line(line));
    }
    lines
}

/// One line outside a fence: a block marker and then inline spans.
fn render_line(line: &str) -> Line {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    if let Some(rest) = heading(trimmed) {
        return Line::one(Segment {
            text: rest.to_owned(),
            emphasis: Emphasis::Heading,
        });
    }
    if let Some(rest) = trimmed.strip_prefix("> ") {
        let mut segments = vec![Segment {
            text: format!("{indent}│ "),
            emphasis: Emphasis::Quote,
        }];
        segments.extend(inline(rest));
        return Line { segments };
    }
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        let mut segments = vec![Segment {
            text: format!("{indent}• "),
            emphasis: Emphasis::Plain,
        }];
        segments.extend(inline(rest));
        return Line { segments };
    }
    if let Some((number, rest)) = ordered(trimmed) {
        let mut segments = vec![Segment::plain(format!("{indent}{number}. "))];
        segments.extend(inline(rest));
        return Line { segments };
    }
    Line {
        segments: inline(line),
    }
}

/// The text of a `#` heading, hashes and the space after them removed.
fn heading(line: &str) -> Option<&str> {
    let hashes = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix(' ')
}

/// The number of an `1.` item, when the line is one.
fn ordered(line: &str) -> Option<(u32, &str)> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 4 {
        return None;
    }
    let rest = line[digits..].strip_prefix(". ")?;
    Some((line[..digits].parse().ok()?, rest))
}

/// Split one line into plain, code, bold, italic and link runs.
fn inline(line: &str) -> Vec<Segment> {
    let characters: Vec<char> = line.chars().collect();
    let mut segments: Vec<Segment> = Vec::new();
    let mut plain = String::new();
    let mut at = 0;
    while at < characters.len() {
        let rest: String = characters[at..].iter().collect();
        if characters[at] == '`'
            && let Some(end) = rest[1..].find('`')
            && end > 0
        {
            push(&mut segments, &mut plain, Emphasis::Plain);
            let code: String = characters[at + 1..at + 1 + end].iter().collect();
            segments.push(Segment {
                text: code,
                emphasis: Emphasis::Code,
            });
            at += end + 2;
            continue;
        }
        if let Some((text, span)) = link(&rest) {
            push(&mut segments, &mut plain, Emphasis::Plain);
            segments.push(Segment {
                text,
                emphasis: Emphasis::Link,
            });
            at += span;
            continue;
        }
        for (marker, emphasis, width) in [
            ("**", Emphasis::Bold, 2),
            ("*", Emphasis::Italic, 1),
            ("_", Emphasis::Italic, 1),
        ] {
            if rest.starts_with(marker)
                && let Some(end) = rest[width..].find(marker)
                && end > 0
            {
                push(&mut segments, &mut plain, Emphasis::Plain);
                segments.push(Segment {
                    text: rest[width..width + end].to_owned(),
                    emphasis,
                });
                at += width + end + width;
                break;
            }
        }
        // A marker that did not open a span is a character: `2 * 3` is not an
        // italic 3, and a reply about shell globs is full of `*`.
        if segments.last().is_some_and(|segment| {
            segment.emphasis != Emphasis::Plain
                && characters[at..]
                    .iter()
                    .collect::<String>()
                    .contains(&segment.text)
        }) {
            continue;
        }
        plain.push(characters[at]);
        at += 1;
    }
    push(&mut segments, &mut plain, Emphasis::Plain);
    if segments.is_empty() {
        segments.push(Segment::plain(""));
    }
    segments
}

/// Add the pending plain text as one run, if there is any.
fn push(segments: &mut Vec<Segment>, plain: &mut String, emphasis: Emphasis) {
    if !plain.is_empty() {
        segments.push(Segment {
            text: std::mem::take(plain),
            emphasis,
        });
    }
}

/// A `[text](address)`, as its text and how many characters it used.
fn link(text: &str) -> Option<(String, usize)> {
    let rest = text.strip_prefix('[')?;
    let close = rest.find(']')?;
    let after = rest[close + 1..].strip_prefix('(')?;
    let end = after.find(')')?;
    if end == 0 {
        return None;
    }
    Some((rest[..close].to_owned(), close + end + 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &Line) -> Vec<(String, Emphasis)> {
        line.segments
            .iter()
            .map(|segment| (segment.text.clone(), segment.emphasis))
            .collect()
    }

    #[test]
    fn a_heading_loses_its_hashes_and_is_one_run() {
        let lines = render("## What changed\nthen prose");
        assert_eq!(
            texts(&lines[0]),
            vec![("What changed".to_owned(), Emphasis::Heading)]
        );
        assert_eq!(lines.len(), 2, "a heading is one row");
    }

    #[test]
    fn bullets_and_numbered_items_keep_their_place() {
        let lines = render("- one\n  - two\n1. first");
        assert_eq!(
            texts(&lines[0]),
            vec![
                ("• ".to_owned(), Emphasis::Plain),
                ("one".to_owned(), Emphasis::Plain)
            ]
        );
        assert_eq!(lines[1].text(), "  • two");
        assert_eq!(lines[2].text(), "1. first");
    }

    #[test]
    fn a_fence_marks_its_block_and_every_line_in_it_is_code() {
        let lines = render("```rust\nlet x = 1;\n```");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text(), "  ```rust");
        assert_eq!(
            texts(&lines[1]),
            vec![("  let x = 1;".to_owned(), Emphasis::Code)]
        );
        assert_eq!(lines[2].text(), "  ```");
    }

    #[test]
    fn inline_code_bold_italic_and_a_link_are_their_own_runs() {
        let line = render("use `Keymap` and **this** *one* [docs](https://x)").remove(0);
        assert_eq!(
            texts(&line),
            vec![
                ("use ".to_owned(), Emphasis::Plain),
                ("Keymap".to_owned(), Emphasis::Code),
                (" and ".to_owned(), Emphasis::Plain),
                ("this".to_owned(), Emphasis::Bold),
                (" ".to_owned(), Emphasis::Plain),
                ("one".to_owned(), Emphasis::Italic),
                (" ".to_owned(), Emphasis::Plain),
                ("docs".to_owned(), Emphasis::Link),
            ],
            "segments: {line:?}"
        );
    }

    #[test]
    fn a_marker_that_opens_nothing_is_a_character() {
        // A reply about globs is full of `*`, and losing one changes the answer.
        let line = render("2 * 3 and a lone `").remove(0);
        assert_eq!(line.text(), "2 * 3 and a lone `");
        assert!(line.segments.iter().all(|s| s.emphasis == Emphasis::Plain));
    }

    #[test]
    fn prose_with_nothing_to_mark_keeps_every_character() {
        let line = render("the quick brown fox").remove(0);
        assert_eq!(line.text(), "the quick brown fox");
    }

    #[test]
    fn the_renderer_never_changes_the_number_of_rows() {
        for text in [
            "",
            "one",
            "one\ntwo",
            "# head\n\n```\nx\n```\n> quote",
            "trailing\n",
        ] {
            assert_eq!(
                render(text).len(),
                text.split('\n').count(),
                "{text:?} is counted without rendering it"
            );
        }
    }
}
