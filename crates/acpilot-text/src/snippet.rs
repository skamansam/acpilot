//! Snippet templates: the text to insert and the tab stops it creates.
//!
//! The grammar is the one editors already use: `$1`, `${1}`, `${1:name}` and
//! `$0`. A placeholder with a default inserts that default and selects it. `$0`
//! is the final caret and always sorts last. A template with no `$0` ends after
//! its last character.

use std::collections::BTreeMap;
use std::ops::Range;

/// One snippet ready for insertion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnippetTemplate {
    /// The text the editor inserts.
    pub text: String,
    /// The stops in visit order, counted in characters.
    pub stops: Vec<Range<usize>>,
}

/// Expand one snippet body.
///
/// A repeated stop keeps its first position. That is the same rule the common
/// editors use for a linked placeholder, and it keeps the tab order stable.
#[must_use]
pub fn expand(body: &str) -> SnippetTemplate {
    let mut text = String::new();
    let mut stops: BTreeMap<usize, Range<usize>> = BTreeMap::new();
    let mut chars = body.chars().peekable();

    while let Some(character) = chars.next() {
        if character != '$' {
            text.push(character);
            continue;
        }
        match chars.peek().copied() {
            Some('$') | Some('\\') => {
                chars.next();
                text.push('$');
            }
            Some(next) if next.is_ascii_digit() => {
                let number = digits(&mut chars);
                let at = text.chars().count();
                stops.entry(number).or_insert(at..at);
            }
            Some('{') => {
                chars.next();
                let number = digits(&mut chars);
                let start = text.chars().count();
                match chars.peek().copied() {
                    Some('}') => {
                        chars.next();
                        stops.entry(number).or_insert(start..start);
                    }
                    Some(':') => {
                        chars.next();
                        let mut default = String::new();
                        while let Some(character) = chars.next() {
                            if character == '}' {
                                break;
                            }
                            default.push(character);
                        }
                        text.push_str(&default);
                        let end = text.chars().count();
                        stops.entry(number).or_insert(start..end);
                    }
                    _ => {
                        text.push_str("${");
                        text.push_str(&number.to_string());
                    }
                }
            }
            _ => text.push('$'),
        }
    }

    let mut ordered: Vec<Range<usize>> = stops
        .iter()
        .filter(|(number, _)| **number != 0)
        .map(|(_, range)| range.clone())
        .collect();
    let end = text.chars().count();
    ordered.push(stops.get(&0).cloned().unwrap_or(end..end));
    SnippetTemplate {
        text,
        stops: ordered,
    }
}

/// Read a run of digits, saturating rather than wrapping on a huge index.
fn digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> usize {
    let mut number = 0usize;
    while let Some(character) = chars.peek().copied() {
        if !character.is_ascii_digit() {
            break;
        }
        number = number
            .saturating_mul(10)
            .saturating_add(character as usize - '0' as usize);
        chars.next();
    }
    number
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_is_text_and_the_selected_stop() {
        let template = expand("fn ${1:name}() {\n    $0\n}");
        assert_eq!(template.text, "fn name() {\n    \n}");
        assert_eq!(template.stops, vec![3..7, 16..16]);
    }

    #[test]
    fn numbered_stops_sort_before_the_final_caret() {
        let template = expand("$2 ${1:one} $0");
        assert_eq!(template.text, " one ");
        assert_eq!(template.stops, vec![1..4, 0..0, 5..5]);
    }

    #[test]
    fn a_template_without_a_final_caret_ends_at_the_end() {
        let template = expand("plain");
        assert_eq!(template.text, "plain");
        assert_eq!(template.stops, vec![5..5]);
    }

    #[test]
    fn a_dollar_not_followed_by_a_stop_stays_literal() {
        let template = expand("cost $x and $$5");
        assert_eq!(template.text, "cost $x and $5");
        assert_eq!(template.stops, vec![14..14]);
    }
}
