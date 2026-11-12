//! Where a name is used, as the parse tree sees it.
//!
//! The references panel asks one question — where does this identifier appear in the
//! workspace — and a word-bounded search alone answers it badly: the same word is in
//! comments, in strings and in the documentation, and a list where most rows are prose
//! is a list nobody scans. So a row is a leaf the grammar calls an identifier, and a
//! `fn` inside a string stays a string, which is the same trick that keeps
//! [`crate::outline`] from listing one.
//!
//! What this is not is a language server's `textDocument/references` (P7-4). Scope is
//! not resolved: two different `new`s in one file are the same row, and a field called
//! `id` is found wherever any `id` is written. It is a search with a parser's help,
//! which is what the panel says it is until a server answers instead.
//!
//! A language whose grammar has no identifier node — JSON, TOML, where a key *is* a
//! string — has no references, and a file this build ships no grammar for is not
//! searched rather than searched badly: prose is not a use.

use std::ops::Range;

use tree_sitter::{Language, Node, Parser};

use crate::is_word;

/// One place a name is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    /// The zero-based line.
    pub line: usize,
    /// The zero-based character column, so a pane places a caret on the glyph rather
    /// than on the byte the rope stores.
    pub column: usize,
}

/// The identifier the caret is on, and where it is in the text.
///
/// A caret sits *between* characters, so after typing a word it is one past the last
/// one: the character before the caret is checked first, which is the position a
/// person means when they say "the word the caret is on". A word that starts with a
/// digit is not an identifier — `1_000` is a number — and asking about one is not a
/// search, so it is [`None`].
#[must_use]
pub fn identifier_at(text: &str, caret: usize) -> Option<(String, Range<usize>)> {
    let characters: Vec<char> = text.chars().collect();
    let caret = caret.min(characters.len());
    let at = if characters
        .get(caret)
        .is_some_and(|character| is_word(*character))
    {
        caret
    } else if caret > 0 && is_word(characters[caret - 1]) {
        caret - 1
    } else {
        return None;
    };
    let mut start = at;
    while start > 0 && is_word(characters[start - 1]) {
        start -= 1;
    }
    let mut end = at + 1;
    while end < characters.len() && is_word(characters[end]) {
        end += 1;
    }
    let name: String = characters[start..end].iter().collect();
    if name.starts_with(|character: char| character.is_ascii_digit()) {
        return None;
    }
    Some((name, start..end))
}

/// Every use of `name` in `text`, in source order.
///
/// `language` is the name [`crate::highlight::language_for`] read off the path, so a
/// search parses a file with the grammar the pane beside it highlights with.
#[must_use]
pub fn uses(language: &str, text: &str, name: &str) -> Vec<Use> {
    if name.is_empty() || !name.chars().all(is_word) {
        // A name that is not one word cannot be an identifier, and a search for `"a b"`
        // would walk the tree to prove it.
        return Vec::new();
    }
    let Some(language) = grammar(language) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    // Split rather than `lines()`: the tree counts rows the way the rope does, and a
    // trailing newline is a row the parse reports a position in.
    let lines: Vec<&str> = text.split('\n').collect();
    let mut found = Vec::new();
    walk(tree.root_node(), text.as_bytes(), &lines, name, &mut found);
    found
}

/// The node kinds that are a name in the source rather than text that reads like one.
///
/// Restricting the walk to these is the whole filter: a comment is a `line_comment`,
/// a string's contents are `string_content`, and neither is ever an `identifier`, so
/// nothing inside them can be listed however it is spelled. `type_identifier` and
/// `field_identifier` are the grammar's own answer for a type and a member, and the
/// shorthand in a struct literal is a third kind rather than an `identifier` in the
/// current Rust grammar.
const IDENTIFIERS: [&str; 4] = [
    "identifier",
    "field_identifier",
    "type_identifier",
    "shorthand_field_identifier",
];

fn walk(node: Node, source: &[u8], lines: &[&str], name: &str, found: &mut Vec<Use>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.child_count() == 0
            && IDENTIFIERS.contains(&child.kind())
            && child.utf8_text(source).is_ok_and(|text| text == name)
        {
            let start = child.start_position();
            found.push(Use {
                line: start.row,
                // tree-sitter counts columns in bytes; a pane draws in characters.
                column: column(lines, start.row, start.column),
            });
        }
        walk(child, source, lines, name, found);
    }
}

/// A byte column as a character column, in one line.
fn column(lines: &[&str], row: usize, byte: usize) -> usize {
    lines
        .get(row)
        .and_then(|line| line.get(..byte))
        .map_or(byte, |prefix| prefix.chars().count())
}

fn grammar(language: &str) -> Option<Language> {
    match language {
        "rust" | "rs" => Some(Language::new(tree_sitter_rust::LANGUAGE)),
        "python" | "py" => Some(Language::new(tree_sitter_python::LANGUAGE)),
        // Parsed, and answer nothing: a JSON key is a string and a TOML key a bare
        // key, so neither grammar has a node that says "this is a name".
        "json" => Some(Language::new(tree_sitter_json::LANGUAGE)),
        "toml" => Some(Language::new(tree_sitter_toml_ng::LANGUAGE)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_caret_finds_the_word_it_is_on_and_the_one_it_sits_after() {
        let text = "let parsed = parse_position(offset);";
        assert_eq!(
            identifier_at(text, 1).map(|(name, _)| name),
            Some("let".to_owned())
        );
        assert_eq!(
            identifier_at(text, 10).map(|(name, _)| name),
            Some("parsed".to_owned()),
            "a caret at the end of a word is still on it"
        );
        assert_eq!(
            identifier_at(text, 6).map(|(name, _)| name),
            Some("parsed".to_owned()),
            "a caret in the middle of one is on it too"
        );
        assert_eq!(
            identifier_at(text, 11),
            None,
            "between a word and punctuation"
        );
        assert_eq!(identifier_at("", 0), None);
    }

    #[test]
    fn a_number_is_not_an_identifier() {
        assert_eq!(identifier_at("1000", 2), None);
        assert_eq!(
            identifier_at("row_1", 5).map(|(name, _)| name),
            Some("row_1".to_owned())
        );
    }

    #[test]
    fn only_code_counts_and_the_columns_are_characters() {
        let text =
            "// handle in a comment\nfn handle() {}\nlet text = \"handle\";\nhandle(document);\n";
        let found = uses("rust", text, "handle");
        assert_eq!(
            found,
            vec![Use { line: 1, column: 3 }, Use { line: 3, column: 0 },],
            "the comment, the string and the definition's own name after `fn`"
        );
        // The definition is a use: a references list that hides the place a name is
        // defined is a list with a row missing.
        assert!(uses("rust", "fn handle() {}", "handle").len() == 1);
    }

    #[test]
    fn a_use_after_a_multi_byte_character_is_counted_in_characters() {
        let text = "let 日 = \"x\"; handle(日);\n";
        assert_eq!(
            uses("rust", text, "handle"),
            vec![Use {
                line: 0,
                column: 13
            }]
        );
    }

    #[test]
    fn a_name_that_is_a_keyword_is_never_a_use() {
        assert!(uses("rust", "fn f() { let x = 1; }", "let").is_empty());
        assert!(uses("rust", "fn f() { let x = 1; }", "fn").is_empty());
    }

    #[test]
    fn python_finds_its_own_uses() {
        let text = "def handle(x):\n    return handle(x)\n";
        assert_eq!(
            uses("python", text, "handle"),
            vec![
                Use { line: 0, column: 4 },
                Use {
                    line: 1,
                    column: 11
                }
            ]
        );
    }

    #[test]
    fn a_language_with_no_names_finds_nothing() {
        assert!(uses("json", "{\"handle\": 1}", "handle").is_empty());
        assert!(uses("markdown", "handle()", "handle").is_empty());
    }
}
