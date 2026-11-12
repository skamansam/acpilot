//! The outline of one buffer: its definitions, in the order they appear.
//!
//! The parse tree the highlighter already builds is the source. A symbol is a node
//! whose kind is a definition in that language, named by the field the grammar gives
//! it, so nothing here guesses from the text: an `fn` inside a string is not a
//! function, and a comment that reads like one is a comment. A language this build
//! ships no grammar for is an empty outline rather than a list of promising lines.
//!
//! Depth is counted in definitions, not in blocks: a function inside a `mod` is one
//! level in, and a `let` binding inside a function is not a symbol at all.

use tree_sitter::{Language, Node, Parser};

/// One definition in a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// The name the grammar gave it.
    pub name: String,
    /// What it is, as the language spells it: `fn`, `struct`, `class`.
    pub kind: &'static str,
    /// The zero-based line the definition starts on.
    pub line: usize,
    /// How many definitions enclose it, for indenting a row.
    pub depth: u8,
}

/// Which of a language's node kinds are definitions, and where the name lives.
///
/// Three strings per entry: the node kind, the field holding its name, and the word a
/// row shows. `impl` names its target type rather than a `name` field, which is why the
/// field is per kind instead of one constant.
type Kinds = &'static [(&'static str, &'static str, &'static str)];

const RUST: Kinds = &[
    ("function_item", "name", "fn"),
    ("struct_item", "name", "struct"),
    ("enum_item", "name", "enum"),
    ("trait_item", "name", "trait"),
    ("union_item", "name", "union"),
    ("impl_item", "type", "impl"),
    ("mod_item", "name", "mod"),
    ("const_item", "name", "const"),
    ("static_item", "name", "static"),
    ("type_item", "name", "type"),
    ("macro_definition", "name", "macro"),
];

const PYTHON: Kinds = &[
    ("function_definition", "name", "def"),
    ("class_definition", "name", "class"),
];

/// The definitions in `text`, in source order.
///
/// `language` is the name the editor read off the path, so the outline and the
/// highlighting cannot disagree about which grammar a file wants.
#[must_use]
pub fn symbols(language: &str, text: &str) -> Vec<Symbol> {
    let Some((language, kinds)) = grammar(language) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect(tree.root_node(), text.as_bytes(), kinds, 0, &mut found);
    found
}

fn grammar(language: &str) -> Option<(Language, Kinds)> {
    match language {
        "rust" | "rs" => Some((Language::new(tree_sitter_rust::LANGUAGE), RUST)),
        "python" | "py" => Some((Language::new(tree_sitter_python::LANGUAGE), PYTHON)),
        _ => None,
    }
}

fn collect(node: Node, source: &[u8], kinds: Kinds, depth: u8, found: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let definition = kinds.iter().find(|(kind, _, _)| *kind == child.kind());
        if let Some((_, field, label)) = definition
            && let Some(name) = child
                .child_by_field_name(field)
                .and_then(|node| node.utf8_text(source).ok())
        {
            found.push(Symbol {
                name: name.to_owned(),
                kind: label,
                line: child.start_position().row,
                depth,
            });
        }
        // Only a definition indents what is inside it: a `let` in a function is not a
        // level, and an `if` block is even less of one.
        let inside = if definition.is_some() {
            depth.saturating_add(1)
        } else {
            depth
        };
        collect(child, source, kinds, inside, found);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rust_file_lists_what_it_defines_in_source_order() {
        let text = "mod inner {\n    fn nested() {}\n}\n\nfn top() {}\n\nstruct Thing;\nimpl Thing {\n    fn method() {}\n}\n";
        let found = symbols("rust", text);
        let rows: Vec<(&str, usize, u8)> = found
            .iter()
            .map(|symbol| (symbol.kind, symbol.line, symbol.depth))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("mod", 0, 0),
                ("fn", 1, 1),
                ("fn", 4, 0),
                ("struct", 6, 0),
                ("impl", 7, 0),
                ("fn", 8, 1),
            ]
        );
        assert_eq!(found[0].name, "inner");
        assert_eq!(found[3].name, "Thing");
        assert_eq!(found[4].name, "Thing", "an impl is named by its type");
    }

    #[test]
    fn a_definition_inside_a_string_or_a_comment_is_not_a_symbol() {
        let text = "// fn ghost() {}\nconst TEXT: &str = \"struct Fake;\";\nfn real() {}\n";
        let found = symbols("rust", text);
        let names: Vec<&str> = found.iter().map(|symbol| symbol.name.as_str()).collect();
        assert_eq!(names, vec!["TEXT", "real"]);
    }

    #[test]
    fn a_language_with_no_grammar_is_an_empty_outline() {
        assert!(symbols("plaintext", "fn main() {}\n").is_empty());
        assert!(symbols("markdown", "# A heading\n").is_empty());
    }

    #[test]
    fn python_defines_functions_and_classes() {
        let found = symbols(
            "python",
            "class Thing:\n    def method(self):\n        pass\n\ndef loose():\n    pass\n",
        );
        let names: Vec<(&str, usize, u8)> = found
            .iter()
            .map(|symbol| (symbol.kind, symbol.line, symbol.depth))
            .collect();
        assert_eq!(names, vec![("class", 0, 0), ("def", 1, 1), ("def", 4, 0)]);
    }
}
