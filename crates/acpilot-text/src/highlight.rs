//! Syntax highlighting: one incremental parse per buffer, spans a pane paints.
//!
//! A pane does not ask "what colour is this file" — it asks "what colour is what
//! I can see", sixty times a second, while the text changes under it. That is the
//! shape of everything here: a [`Grammar`] is a language and its capture query,
//! [`Highlights`] owns the parse tree for one buffer and is told when the text
//! changed, and [`Highlights::spans`] answers for a char range only.
//!
//! Three decisions, each one the fix for a cost a frame-based design pays:
//!
//! - **The parse is incremental, except where reuse would lie.** `sync` diffs the
//!   new text against the text it last parsed, hands tree-sitter that edit and
//!   reparses from the old tree, so a keystroke re-parses a line rather than a
//!   file. The diff is a common prefix and suffix rather than a list of edits: it
//!   is coarser than the truth and never wrong, which is the right trade for
//!   something running on every keystroke of a large file. The exception is a
//!   tree with an ERROR in it: tree-sitter's incremental error recovery can shape
//!   error nodes differently from a fresh parse, so a broken buffer is parsed
//!   whole, and an edit that introduces an error is reparsed once to check it. A
//!   buffer that is mid-sentence is not the one whose frame cost matters, and the
//!   colour of the text is.
//! - **The query runs on the visible range.** The tree covers the whole buffer.
//!   The query cursor is given the bytes a pane is about to draw. A 60k-line
//!   file costs what a screen of it costs.
//! - **Captures become [`SyntaxToken`]s, not colours.** The theme owns the
//!   colours — one token set, from the same TOML the TUI, the window and the page
//!   read — so highlighting cannot disagree with theming about what a
//!   `keyword` is, and a theme file needs no knowledge of tree-sitter's capture
//!   names. [`CAPTURES`] is the one table that maps between the two.
//!
//! What is deliberately absent: injections (markdown inside a doc comment, HTML
//! inside a JS string), locals (`local.definition` and friends, which would let a
//! grammar dim an unused variable), and grammars that are not compiled in — those
//! arrive as extensions (P4.6-8), which is why [`Grammars`] takes a name rather
//! than owning a fixed list.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use acpilot_palette::SyntaxToken;
use thiserror::Error;
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator, Tree, WasmStore};

use crate::buffer::Buffer;

/// The capture names this editor understands, and the token each one paints.
///
/// Ordered by nothing in particular. Lookup strips the dotted tail, so a grammar
/// that says `function.method.builtin` is painted as a function and one that
/// says `keyword.control.conditional` as a keyword. A capture that matches no
/// row is left unhighlighted rather than guessed at: plain text is a better
/// answer than a wrong colour. A grammar that invents a capture name should
/// not paint something misleading.
///
/// The extra rows beyond one-per-token exist because grammars disagree about
/// names. `attribute` and `decorator` are the same idea. `character` and
/// `escape` are string matter. `tag` is a type name in markup.
pub const CAPTURES: &[(&str, SyntaxToken)] = &[
    ("comment", SyntaxToken::Comment),
    ("keyword", SyntaxToken::Keyword),
    ("string", SyntaxToken::String),
    ("character", SyntaxToken::String),
    ("escape", SyntaxToken::String),
    ("number", SyntaxToken::Number),
    ("type", SyntaxToken::Type),
    ("namespace", SyntaxToken::Type),
    ("tag", SyntaxToken::Type),
    ("constructor", SyntaxToken::Type),
    ("function", SyntaxToken::Function),
    ("method", SyntaxToken::Function),
    ("macro", SyntaxToken::Function),
    ("attribute", SyntaxToken::Function),
    ("decorator", SyntaxToken::Function),
    ("variable", SyntaxToken::Variable),
    ("parameter", SyntaxToken::Variable),
    ("property", SyntaxToken::Variable),
    ("constant", SyntaxToken::Constant),
    ("boolean", SyntaxToken::Constant),
    ("operator", SyntaxToken::Operator),
    ("punctuation", SyntaxToken::Punctuation),
    ("delimiter", SyntaxToken::Punctuation),
];

/// The token a tree-sitter capture name paints, if any.
///
/// Dotted names fall back to their parent: `punctuation.bracket` is punctuation
/// even though no row names it. `_private` captures, the convention for
/// captures that exist only to be referenced by another pattern, paint nothing.
#[must_use]
pub fn token_for(capture: &str) -> Option<SyntaxToken> {
    if capture.starts_with('_') {
        return None;
    }
    let mut name = capture;
    loop {
        if let Some((_, token)) = CAPTURES.iter().find(|(known, _)| *known == name) {
            return Some(*token);
        }
        let dot = name.rfind('.')?;
        name = &name[..dot];
    }
}

/// A language with its highlighting query, compiled and ready.
///
/// Compiling the query is the expensive part of a grammar — it happens once per
/// language per process, behind [`Grammars`] — while parsing is per buffer.
#[derive(Debug)]
pub struct Grammar {
    name: String,
    language: Language,
    query: Query,
    wasm: Option<Arc<tree_sitter::wasmtime::Engine>>,
    budget: Option<Duration>,
}

impl Grammar {
    /// A grammar from a language and its `highlights.scm`.
    ///
    /// # Errors
    ///
    /// [`GrammarError::Query`] when the query does not compile against the
    /// language. That usually means the grammar crate and the runtime disagree.
    /// It is a build problem rather than a file problem.
    pub fn new(
        name: impl Into<String>,
        language: Language,
        highlights: &str,
    ) -> Result<Self, GrammarError> {
        let name = name.into();
        let query = Query::new(&language, highlights).map_err(|source| GrammarError::Query {
            language: name.clone(),
            source,
        })?;
        Ok(Self {
            name,
            language,
            query,
            wasm: None,
            budget: None,
        })
    }

    /// A grammar loaded from a prebuilt tree-sitter wasm module.
    ///
    /// The module's exported language name must match `name`. The engine is kept
    /// so every buffer parser can set a store from it.
    ///
    /// # Errors
    ///
    /// The wasm does not load, or its highlight query does not compile.
    pub fn from_wasm(
        name: impl Into<String>,
        wasm: &[u8],
        highlights: &str,
    ) -> Result<Self, GrammarError> {
        let name = name.into();
        let engine = Arc::new(tree_sitter::wasmtime::Engine::default());
        let mut store = WasmStore::new(&engine).map_err(|error| GrammarError::Wasm {
            language: name.clone(),
            message: error.to_string(),
        })?;
        let language = store
            .load_language(&name, wasm)
            .map_err(|error| GrammarError::Wasm {
                language: name.clone(),
                message: error.to_string(),
            })?;
        let query = Query::new(&language, highlights).map_err(|source| GrammarError::Query {
            language: name.clone(),
            source,
        })?;
        Ok(Self {
            name,
            language,
            query,
            wasm: Some(engine),
            budget: Some(Duration::from_millis(50)),
        })
    }

    /// Whether this build has a native grammar under `name`.
    #[must_use]
    pub fn is_builtin(name: &str) -> bool {
        matches!(name, "rust" | "rs" | "python" | "py" | "json" | "toml")
    }

    /// The engine a wasm grammar was loaded with.
    fn wasm_engine(&self) -> Option<&Arc<tree_sitter::wasmtime::Engine>> {
        self.wasm.as_ref()
    }

    /// The longest parse this grammar may take, when it has a budget.
    fn budget(&self) -> Option<Duration> {
        self.budget
    }

    /// The grammar shipped with this build for a language name, if there is one.
    ///
    /// Accepted names include the usual short spellings, because the name
    /// arrives from a settings file, a file extension and a person's fingers.
    #[must_use]
    pub fn builtin(name: &str) -> Option<Self> {
        let (name, language, highlights) = match name {
            "rust" | "rs" => (
                "rust",
                tree_sitter_rust::LANGUAGE,
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            ),
            "python" | "py" => (
                "python",
                tree_sitter_python::LANGUAGE,
                tree_sitter_python::HIGHLIGHTS_QUERY,
            ),
            "json" => (
                "json",
                tree_sitter_json::LANGUAGE,
                tree_sitter_json::HIGHLIGHTS_QUERY,
            ),
            "toml" => (
                "toml",
                tree_sitter_toml_ng::LANGUAGE,
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            ),
            _ => return None,
        };
        Self::new(name, Language::new(language), highlights).ok()
    }

    /// The language's name, as this crate spells it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The language a path's extension names, as this crate spells it.
///
/// The registry is the authority on which grammars exist. The suffix table lives
/// beside it rather than in each client. The editor's title, the outline and a
/// references search all have to agree with the highlighter about what a `.rs`
/// is. A second copy is a second answer. An unknown suffix is `""`, which
/// [`Grammar::builtin`] answers with `None`.
#[must_use]
pub fn language_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("json") => "json",
        Some("toml") => "toml",
        _ => "",
    }
}

/// Why a grammar could not be prepared.
#[derive(Debug, Error)]
pub enum GrammarError {
    /// The highlighting query did not compile against the language.
    #[error("the `{language}` grammar's highlight query does not compile: {source}")]
    Query {
        /// The language whose grammar was being prepared.
        language: String,
        /// What tree-sitter said, which names the offending pattern.
        source: tree_sitter::QueryError,
    },

    /// The wasm grammar did not load.
    #[error("the `{language}` grammar's wasm does not load: {message}")]
    Wasm {
        /// The language whose grammar was being prepared.
        language: String,
        /// What tree-sitter said.
        message: String,
    },
}

/// One run of characters painted as one token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// Char offsets, like every other index in this crate.
    pub range: Range<usize>,
    /// What it is painted as.
    pub token: SyntaxToken,
}

/// The grammars this process has compiled, and the ones it can.
///
/// Lazy on purpose: compiling every query in the build costs tens of
/// milliseconds. A session that opens one Rust file should not pay for JSON.
/// A grammar is built the first time it is asked for and kept. A language
/// this build does not ship caches its absence. A directory of `.astro` files
/// does not retry the compiler once per file.
#[derive(Debug, Default)]
pub struct Grammars {
    built: BTreeMap<String, Option<Arc<Grammar>>>,
    wasm: BTreeMap<String, WasmGrammar>,
    extensions: BTreeMap<String, String>,
    failures: BTreeMap<String, String>,
}

/// A wasm grammar waiting for its first file.
#[derive(Clone)]
struct WasmGrammar {
    wasm: Vec<u8>,
    highlights: String,
    #[allow(dead_code, reason = "read by the indent engine when it lands")]
    indents: Option<String>,
}

impl std::fmt::Debug for WasmGrammar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmGrammar")
            .field("wasm_bytes", &self.wasm.len())
            .field("highlights_bytes", &self.highlights.len())
            .field("indents", &self.indents.as_ref().map(String::len))
            .finish()
    }
}

impl Grammars {
    /// A registry with nothing compiled yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an extension grammar before the first file asks for it.
    ///
    /// Returns `false` when a native grammar owns the name, or when one of the
    /// file extensions is already claimed. The caller can then name the pack.
    pub fn register_wasm(
        &mut self,
        name: &str,
        extensions: &[String],
        wasm: Vec<u8>,
        highlights: String,
        indents: Option<String>,
    ) -> bool {
        if Grammar::is_builtin(name) || self.wasm.contains_key(name) {
            return false;
        }
        if extensions.iter().any(|extension| {
            self.extensions
                .contains_key(&extension.to_ascii_lowercase())
        }) {
            return false;
        }
        for extension in extensions {
            self.extensions
                .insert(extension.to_ascii_lowercase(), name.to_owned());
        }
        self.wasm.insert(
            name.to_owned(),
            WasmGrammar {
                wasm,
                highlights,
                indents,
            },
        );
        self.failures.remove(name);
        self.built.remove(name);
        true
    }

    /// Drop every extension grammar and the suffixes it claimed.
    ///
    /// A deactivated pack must stop claiming its files. Native grammars stay
    /// compiled, because no pack owns them.
    pub fn clear_wasm(&mut self) {
        let names: Vec<String> = self.wasm.keys().cloned().collect();
        for name in names {
            self.built.remove(&name);
            self.failures.remove(&name);
        }
        self.wasm.clear();
        self.extensions.clear();
    }

    /// The language a path names, including extension grammars.
    #[must_use]
    pub fn language_for_path(&self, path: &str) -> String {
        let extension = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        self.extensions
            .get(&extension)
            .cloned()
            .or_else(|| language_for_extension(&extension).map(str::to_owned))
            .unwrap_or_default()
    }

    /// Why a language's wasm grammar did not load, if it failed.
    #[must_use]
    pub fn failure(&self, language: &str) -> Option<&str> {
        self.failures.get(language).map(String::as_str)
    }

    /// The grammar for a language name, compiling it on first use.
    ///
    /// `None` means "no highlighting for this language", which a pane paints as
    /// plain text in the theme's foreground.
    pub fn get(&mut self, language: &str) -> Option<Arc<Grammar>> {
        if let Some(cached) = self.built.get(language) {
            return cached.clone();
        }
        let built = match self.wasm.get(language).cloned() {
            Some(spec) => match Grammar::from_wasm(language, &spec.wasm, &spec.highlights) {
                Ok(grammar) => Some(Arc::new(grammar)),
                Err(error) => {
                    self.failures.insert(language.to_owned(), error.to_string());
                    None
                }
            },
            None => Grammar::builtin(language).map(Arc::new),
        };
        self.built.insert(language.to_owned(), built.clone());
        built
    }

    /// The grammar for a path, by the language its extension names.
    pub fn for_path(&mut self, path: &Path) -> Option<Arc<Grammar>> {
        let language = self.language_for_path(path.to_str()?);
        if language.is_empty() {
            None
        } else {
            self.get(&language)
        }
    }

    /// How many grammars have been compiled, for a diagnostics line.
    #[must_use]
    pub fn compiled(&self) -> usize {
        self.built
            .values()
            .filter(|grammar| grammar.is_some())
            .count()
    }
}

/// The language name an extension names, if this build ships one.
#[must_use]
pub fn language_for_extension(extension: &str) -> Option<&'static str> {
    match extension.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "py" | "pyi" => Some("python"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        _ => None,
    }
}

/// One buffer's parse tree, and the spans a pane asks for.
///
/// The text the tree was parsed from is kept: it is what the next `sync` diffs
/// against, and it is already a `String` because tree-sitter parses bytes. The
/// cost is one copy of the file per open tab, which a rope-backed buffer already
/// pays once.
pub struct Highlights {
    grammar: Arc<Grammar>,
    parser: Parser,
    tree: Option<Tree>,
    text: String,
    cursor: QueryCursor,
    spans: Vec<Span>,
    budget: Option<Duration>,
    timed_out: bool,
}

impl Highlights {
    /// A highlighter for one buffer, with nothing parsed yet.
    ///
    /// [`Self::sync`] is what parses, so a tab that has never been drawn costs
    /// nothing but a parser.
    #[must_use]
    pub fn new(grammar: Arc<Grammar>) -> Self {
        let budget = grammar.budget();
        let mut parser = Parser::new();
        // A wasm grammar needs a store from the same engine it was loaded with.
        // A store that will not start leaves the tree `None`, so the pane paints
        // plain text instead of refusing the file.
        if let Some(engine) = grammar.wasm_engine()
            && let Ok(store) = WasmStore::new(engine)
        {
            let _ = parser.set_wasm_store(store);
        }
        // The grammar holds the same language, so this cannot fail. If a future
        // grammar did, the tree stays `None` and the pane paints plain text.
        let _ = parser.set_language(&grammar.language);
        Self {
            grammar,
            parser,
            tree: None,
            text: String::new(),
            cursor: QueryCursor::new(),
            spans: Vec::new(),
            budget,
            timed_out: false,
        }
    }

    /// The grammar in use.
    #[must_use]
    pub fn grammar(&self) -> &Grammar {
        &self.grammar
    }

    /// Reparse the buffer if its text changed.
    ///
    /// Returns whether it did, so a caller can skip the repaint as well as the
    /// parse. Call this after an edit. [`Self::spans`] reads the tree as it is
    /// and does not look at the buffer's text, which is what keeps a frame free
    /// of a whole-file copy.
    pub fn sync(&mut self, buffer: &Buffer) -> bool {
        let text = buffer.text();
        if (self.tree.is_some() || self.timed_out) && text == self.text {
            return false;
        }
        let started = Instant::now();
        // The old tree has to be told what changed, not merely handed back to the
        // parser. A tree whose edit was never applied describes a document that
        // no longer exists, and tree-sitter will reuse it whole. The first
        // keystroke in an empty buffer came back as a file with no nodes in it.
        // `edit_between` answers with the span between the two texts, which is a
        // superset of the real edits: less to reuse, never wrong.
        // A tree with an ERROR in it is not reused. tree-sitter's incremental error
        // recovery can reshape error nodes differently from a fresh parse. No warning
        // or error flag marks it, only a different tree. The pane's contract is that
        // the colours describe the text under them. A buffer that is mid-sentence is
        // parsed from scratch, and one that is valid keeps the reuse that makes a
        // keystroke cheap.
        let mut old = self.tree.take();
        let stable = old
            .as_ref()
            .is_some_and(|tree| !tree.root_node().has_error());
        if stable && let Some(tree) = old.as_mut() {
            tree.edit(&edit_between(&self.text, &text));
        }
        let mut parsed = self
            .parser
            .parse(&text, if stable { old.as_ref() } else { None });
        // And an edit that introduces an error is checked against a fresh parse too:
        // the reuse came from a valid tree. The adjustment that shaped the new ERROR
        // nodes is tree-sitter's, not ours.
        if stable
            && parsed
                .as_ref()
                .is_some_and(|tree| tree.root_node().has_error())
        {
            parsed = self.parser.parse(&text, None);
        }
        self.timed_out = self.budget.is_some_and(|budget| started.elapsed() > budget);
        self.tree = if self.timed_out { None } else { parsed };
        self.text = text;
        true
    }

    /// The spans inside a char range, in order and without overlaps.
    ///
    /// A capture that starts outside the range is not visited. The tree covers
    /// the whole buffer, and the query only walks the bytes asked for. Where two
    /// captures cover one character the later one wins. That is how
    /// tree-sitter's queries are written: the general pattern comes first and the
    /// specific one overrides it.
    pub fn spans(&mut self, buffer: &Buffer, range: Range<usize>) -> &[Span] {
        self.spans.clear();
        let Some(tree) = self.tree.as_ref() else {
            return &self.spans;
        };
        let rope = buffer.rope();
        let len = buffer.char_len();
        let start = range.start.min(len);
        let end = range.end.clamp(start, len);
        if start == end {
            return &self.spans;
        }
        let bytes = self.text.as_bytes();
        self.cursor
            .set_byte_range(rope.char_to_byte(start)..rope.char_to_byte(end));
        let names = self.grammar.query.capture_names();
        let mut captures = self
            .cursor
            .captures(&self.grammar.query, tree.root_node(), bytes);
        // One node can match several patterns, and the cursor hands them over
        // together with the last pattern last. Only that one counts: a query that
        // gives a class name `@constructor` after giving every identifier
        // `@variable` means "not a variable". Painting the variable anyway
        // would undo the specificity the grammar asked for. An unmapped capture
        // therefore paints nothing and suppresses the broader one, which is what
        // tree-sitter's own highlighter does.
        let mut pending: Option<(tree_sitter::Node, Option<SyntaxToken>)> = None;
        while let Some((matched, index)) = captures.next() {
            let Some(capture) = matched.captures().get(*index) else {
                continue;
            };
            let token = names
                .get(capture.index as usize)
                .and_then(|name| token_for(name));
            if pending.is_some_and(|(node, _)| node == capture.node) {
                pending = Some((capture.node, token));
                continue;
            }
            if let Some((node, token)) = pending.replace((capture.node, token))
                && let Some(token) = token
            {
                push(&mut self.spans, rope, node, token);
            }
        }
        if let Some((node, Some(token))) = pending {
            push(&mut self.spans, rope, node, token);
        }
        &self.spans
    }

    /// The token at one char offset, or `None` inside an unhighlighted gap.
    #[must_use]
    pub fn token_at(&self, offset: usize) -> Option<SyntaxToken> {
        let index = self.spans.partition_point(|span| span.range.end <= offset);
        self.spans
            .get(index)
            .filter(|span| span.range.start <= offset)
            .map(|span| span.token)
    }
}

/// Turn a node into a span and paint it.
fn push(spans: &mut Vec<Span>, rope: &ropey::Rope, node: tree_sitter::Node, token: SyntaxToken) {
    let range = rope.byte_to_char(node.start_byte())..rope.byte_to_char(node.end_byte());
    if !range.is_empty() {
        paint(spans, Span { range, token });
    }
}

/// Put a span in, letting it win over whatever it covers.
///
/// The list is kept sorted and non-overlapping. This is the one place that
/// decides what "later wins" means: the covered part of an earlier span is
/// dropped. A span that only partly covers another keeps the ends it still
/// owns.
fn paint(spans: &mut Vec<Span>, span: Span) {
    let mut out = Vec::with_capacity(spans.len() + 1);
    let mut placed = false;
    for existing in spans.drain(..) {
        if existing.range.end <= span.range.start {
            out.push(existing);
            continue;
        }
        if existing.range.start >= span.range.end {
            if !placed {
                out.push(span.clone());
                placed = true;
            }
            out.push(existing);
            continue;
        }
        if existing.range.start < span.range.start {
            out.push(Span {
                range: existing.range.start..span.range.start,
                token: existing.token,
            });
        }
        if existing.range.end > span.range.end {
            if !placed {
                out.push(span.clone());
                placed = true;
            }
            out.push(Span {
                range: span.range.end..existing.range.end,
                token: existing.token,
            });
        }
    }
    if !placed {
        out.push(span);
    }
    *spans = out;
}

/// The one edit that turns `old` into `new`, for tree-sitter's incremental parse.
///
/// A common prefix and a common suffix rather than the truth. Three edits in one
/// keystroke's worth of paste are reported as one, which costs reuse and never
/// costs correctness. Both ends are moved back to char boundaries so the edit
/// cannot split a `😀` and hand tree-sitter a byte offset no line can explain.
fn edit_between(old: &str, new: &str) -> tree_sitter::InputEdit {
    let (old, new) = (old.as_bytes(), new.as_bytes());
    let limit = old.len().min(new.len());
    let mut prefix = 0;
    while prefix < limit && old[prefix] == new[prefix] {
        prefix += 1;
    }
    prefix = floor_boundary(old, prefix);
    let mut suffix = 0;
    while suffix < limit - prefix && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }
    tree_sitter::InputEdit {
        start_byte: prefix,
        old_end_byte: old.len() - suffix,
        new_end_byte: new.len() - suffix,
        start_position: point_at(old, prefix),
        old_end_position: point_at(old, old.len() - suffix),
        new_end_position: point_at(new, new.len() - suffix),
    }
}

/// The nearest char boundary at or before `index`.
///
/// `index == len` is the end of the text and is a boundary. Indexing there is
/// the panic this guard exists for, because the common case is the old text
/// being a prefix of the new one.
fn floor_boundary(bytes: &[u8], mut index: usize) -> usize {
    while index > 0 && bytes.get(index).is_some_and(|byte| byte & 0xC0 == 0x80) {
        index -= 1;
    }
    index
}

/// The row and byte column tree-sitter counts a point in.
fn point_at(bytes: &[u8], index: usize) -> tree_sitter::Point {
    let before = &bytes[..index.min(bytes.len())];
    let row = before.iter().filter(|byte| **byte == b'\n').count();
    let column = before
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(before.len(), |newline| before.len() - newline - 1);
    tree_sitter::Point::new(row, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of a span, so an assertion reads as the code it is about.
    fn painted(buffer: &Buffer, spans: &[Span]) -> Vec<(String, SyntaxToken)> {
        spans
            .iter()
            .map(|span| (buffer.slice(span.range.clone()), span.token))
            .collect()
    }

    fn highlights(language: &str, text: &str) -> (Buffer, Highlights) {
        let buffer = Buffer::from_text(text);
        let mut grammars = Grammars::new();
        let grammar = grammars.get(language).expect("a built-in grammar");
        let mut highlights = Highlights::new(grammar);
        highlights.sync(&buffer);
        (buffer, highlights)
    }

    /// The property test found this on Windows: a text that is mostly quotes and
    /// brackets, with a paste at offset 0. The incremental parse painted the closing
    /// quote as a string. A fresh parse of the same text did not, because tree-sitter
    /// reused the old file's ERROR node and reshaped around it differently.
    #[test]
    fn an_incremental_parse_agrees_after_a_paste_into_broken_text() {
        let before = "AaA?a/\"\n\"0]\\ \"%";
        let inserted = "fc/\n%\n[";
        let mut buffer = Buffer::from_text(before);
        let mut grammars = Grammars::new();
        let grammar = grammars.get("rust").expect("a grammar");
        let mut incremental = Highlights::new(grammar.clone());
        incremental.sync(&buffer);
        incremental.spans(&buffer, 0..buffer.char_len());

        buffer.apply(&[crate::edit::Replacement::of(0..0, inserted)]);
        incremental.sync(&buffer);
        let actual = incremental.spans(&buffer, 0..buffer.char_len()).to_vec();

        let mut fresh = Highlights::new(grammar);
        fresh.sync(&buffer);
        let expected = fresh.spans(&buffer, 0..buffer.char_len()).to_vec();
        assert_eq!(
            actual,
            expected,
            "incremental: {:?}\nfresh: {:?}",
            incremental
                .tree
                .as_ref()
                .map(|tree| tree.root_node().to_sexp()),
            fresh.tree.as_ref().map(|tree| tree.root_node().to_sexp())
        );
    }

    /// What `tree-sitter-highlight` makes of the same source, as text and token.
    ///
    /// The reference implementation is the oracle. This crate walks captures
    /// itself so the query can be limited to the visible range and the tree can
    /// be the incremental one. The price is having to agree with tree-sitter
    /// about precedence. A disagreement is a test failure, not a colour somebody
    /// notices in six months.
    fn reference_spans(language: &str, source: &str) -> Vec<(String, SyntaxToken)> {
        use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

        let (tree_language, query) = match language {
            "rust" => (
                Language::new(tree_sitter_rust::LANGUAGE),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            ),
            "python" => (
                Language::new(tree_sitter_python::LANGUAGE),
                tree_sitter_python::HIGHLIGHTS_QUERY,
            ),
            "json" => (
                Language::new(tree_sitter_json::LANGUAGE),
                tree_sitter_json::HIGHLIGHTS_QUERY,
            ),
            "toml" => (
                Language::new(tree_sitter_toml_ng::LANGUAGE),
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            ),
            other => panic!("no fixture language `{other}`"),
        };
        let mut config = HighlightConfiguration::new(tree_language, language, query, "", "")
            .expect("the query compiles");
        let names: Vec<&str> = CAPTURES.iter().map(|(name, _)| *name).collect();
        config.configure(&names);

        let mut highlighter = Highlighter::new();
        let events = highlighter
            .highlight(&config, source.as_bytes(), None, None, |_| None)
            .expect("highlighting starts");
        let buffer = Buffer::from_text(source);
        let rope = buffer.rope();
        let mut stack: Vec<SyntaxToken> = Vec::new();
        let mut spans: Vec<Span> = Vec::new();
        for event in events {
            match event.expect("an event") {
                HighlightEvent::HighlightStart(highlight) => {
                    stack.push(CAPTURES[highlight.0].1);
                }
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let Some(token) = stack.last().copied() else {
                        continue;
                    };
                    paint(
                        &mut spans,
                        Span {
                            range: rope.byte_to_char(start)..rope.byte_to_char(end),
                            token,
                        },
                    );
                }
            }
        }
        spans
            .into_iter()
            .map(|span| (buffer.slice(span.range), span.token))
            .collect()
    }

    #[test]
    fn the_capture_walk_agrees_with_the_reference_highlighter() {
        let fixtures = [
            (
                "rust",
                "use std::fmt;\n\n/// Doc.\npub fn main(x: u8) -> u32 {\n    let s = \"hi\\n\";\n    println!(\"{s} {} {}\n\", x, 0x1f);\n    // a comment\n    #[allow(dead_code)]\n    let ch = 'c';\n    x as u32\n}\n",
            ),
            (
                "python",
                "import os\n\nclass Thing(Base):\n    \"\"\"Doc.\"\"\"\n\n    def run(self, n=2):\n        return [x for x in range(n)]  # hi\n",
            ),
            (
                "json",
                "{\"name\": \"acpilot\", \"count\": 3, \"ok\": true, \"items\": [null, 1.5]}\n",
            ),
            (
                "toml",
                "# a comment\n[package]\nname = \"acpilot\"\nversion = \"0.1.0\"\nn = 12\nok = true\n",
            ),
        ];
        for (language, source) in fixtures {
            let expected = reference_spans(language, source);
            let (buffer, mut highlights) = highlights(language, source);
            let actual = painted(&buffer, highlights.spans(&buffer, 0..buffer.char_len()));
            assert_eq!(actual, expected, "{language} disagrees with the reference");
        }
    }

    #[test]
    fn a_capture_is_painted_as_the_token_its_dotted_name_says() {
        assert_eq!(token_for("keyword"), Some(SyntaxToken::Keyword));
        assert_eq!(token_for("keyword.control"), Some(SyntaxToken::Keyword));
        assert_eq!(
            token_for("punctuation.bracket"),
            Some(SyntaxToken::Punctuation)
        );
        assert_eq!(
            token_for("function.method.builtin"),
            Some(SyntaxToken::Function)
        );
        assert_eq!(token_for("string.special.path"), Some(SyntaxToken::String));
        assert_eq!(
            token_for("_private"),
            None,
            "captures that exist to be referenced"
        );
        assert_eq!(token_for("spell"), None, "a capture nobody mapped");
    }

    #[test]
    fn rust_is_highlighted_from_its_own_query() {
        let (buffer, mut highlights) = highlights("rust", "fn main() {\n    let x = 1; // hi\n}\n");
        let spans = highlights.spans(&buffer, 0..buffer.char_len());
        let painted = painted(&buffer, spans);
        assert!(
            painted.contains(&("fn".to_owned(), SyntaxToken::Keyword)),
            "{painted:?}"
        );
        assert!(
            painted.contains(&("main".to_owned(), SyntaxToken::Function)),
            "{painted:?}"
        );
        assert!(
            painted.contains(&("1".to_owned(), SyntaxToken::Constant)),
            "rust's query calls an integer literal a constant: {painted:?}"
        );
        assert!(
            painted.contains(&("// hi".to_owned(), SyntaxToken::Comment)),
            "{painted:?}"
        );
    }

    #[test]
    fn a_second_language_uses_its_own_grammar() {
        let (buffer, mut highlights) = highlights("python", "def f():\n    return \"s\"\n");
        let painted = painted(&buffer, highlights.spans(&buffer, 0..buffer.char_len()));
        assert!(
            painted.contains(&("def".to_owned(), SyntaxToken::Keyword)),
            "{painted:?}"
        );
        assert!(
            painted.contains(&("\"s\"".to_owned(), SyntaxToken::String)),
            "{painted:?}"
        );
    }

    #[test]
    fn only_the_visible_range_is_queried() {
        let text = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let (buffer, mut highlights) = highlights("rust", text);
        let second = buffer.line_range(1);
        let spans = highlights.spans(&buffer, second.clone());
        assert!(!spans.is_empty());
        for span in spans {
            assert!(
                span.range.start >= second.start && span.range.end <= second.end,
                "{:?} is outside {second:?}",
                span.range
            );
        }
        assert_eq!(
            highlights.token_at(second.start),
            Some(SyntaxToken::Keyword),
            "`fn` of the second line"
        );
    }

    #[test]
    fn an_edit_reparses_only_what_changed_and_says_the_same_thing() {
        let mut buffer = Buffer::from_text("fn one() {}\n");
        let mut grammars = Grammars::new();
        let grammar = grammars.get("rust").expect("a grammar");
        let mut highlights = Highlights::new(grammar.clone());
        highlights.sync(&buffer);
        let before = highlights.spans(&buffer, 0..buffer.char_len()).to_vec();
        assert!(!before.is_empty());

        // Type a second function, one character at a time. Each step is a parse,
        // and the spans at the end have to be what a fresh parse would give.
        for character in "fn two() {}".chars() {
            buffer.apply(&[crate::edit::Replacement::of(
                buffer.char_len()..buffer.char_len(),
                character.to_string(),
            )]);
            assert!(highlights.sync(&buffer));
        }
        let after = highlights.spans(&buffer, 0..buffer.char_len()).to_vec();
        let mut fresh = Highlights::new(grammar);
        fresh.sync(&buffer);
        assert_eq!(
            after,
            fresh.spans(&buffer, 0..buffer.char_len()).to_vec(),
            "an incremental parse is the parse"
        );
    }

    #[test]
    fn sync_is_a_no_op_when_the_text_did_not_change() {
        let (buffer, mut highlights) = highlights("rust", "fn a() {}\n");
        assert!(!highlights.sync(&buffer), "nothing to do");
        assert!(highlights.spans(&buffer, 0..0).is_empty());
    }

    #[test]
    fn a_grammar_is_compiled_once_and_an_unknown_language_is_remembered() {
        let mut grammars = Grammars::new();
        assert_eq!(grammars.compiled(), 0);
        let first = grammars.get("rust").expect("rust");
        assert_eq!(grammars.compiled(), 1);
        let second = grammars
            .get("rs")
            .expect("the short name is the same grammar");
        assert!(!Arc::ptr_eq(&first, &second), "two names, two compilations");
        let again = grammars.get("rust").expect("cached");
        assert!(Arc::ptr_eq(&first, &again), "a grammar is built once");
        assert!(grammars.get("astro").is_none());
        assert_eq!(grammars.compiled(), 2, "a miss is not a compilation");
    }

    #[test]
    fn a_path_names_its_language() {
        let mut grammars = Grammars::new();
        assert_eq!(
            grammars
                .for_path(Path::new("src/main.rs"))
                .map(|g| g.name().to_owned()),
            Some("rust".to_owned())
        );
        assert_eq!(
            grammars
                .for_path(Path::new("Cargo.toml"))
                .map(|g| g.name().to_owned()),
            Some("toml".to_owned())
        );
        assert!(grammars.for_path(Path::new("main.astro")).is_none());
        assert!(grammars.for_path(Path::new("Makefile")).is_none());
        assert_eq!(language_for_extension("PY"), Some("python"));
    }

    #[test]
    fn clearing_wasm_releases_the_pack_and_keeps_native_grammars() {
        let mut grammars = Grammars::new();
        grammars.get("rust").expect("the native grammar");
        assert!(grammars.register_wasm(
            "astro",
            &["astro".to_owned()],
            vec![1, 2, 3],
            "(identifier) @variable".to_owned(),
            None,
        ));
        assert_eq!(grammars.language_for_path("src/view.astro"), "astro");

        grammars.clear_wasm();
        assert_eq!(grammars.language_for_path("src/view.astro"), "");
        assert_eq!(grammars.compiled(), 1, "rust stays compiled");
        assert!(
            grammars.register_wasm(
                "astro",
                &["astro".to_owned()],
                vec![1, 2, 3],
                "(identifier) @variable".to_owned(),
                None,
            ),
            "the name and suffix are free again"
        );
    }

    #[test]
    fn an_edit_between_two_texts_covers_only_what_differs() {
        let edit = edit_between("one two", "one three two");
        assert_eq!(edit.start_byte, 5, "`hree ` went in after the shared `t`");
        assert_eq!(edit.old_end_byte, 5, "nothing of the old text goes");
        assert_eq!(
            edit.new_end_byte, 11,
            "`hree t` goes in; the `wo` after it is the shared suffix, and the edit \
             may not overlap itself"
        );
        assert_eq!(edit.start_position, tree_sitter::Point::new(0, 5));

        let edit = edit_between("a\nb\nc", "a\nB\nc");
        assert_eq!(edit.start_byte, 2, "the letter on the second line");
        assert_eq!(edit.old_end_byte, 3);
        assert_eq!(edit.new_end_byte, 3);
        assert_eq!(edit.start_position, tree_sitter::Point::new(1, 0));
    }

    #[test]
    fn an_edit_never_splits_a_character() {
        // `😀` is four bytes. The diff would otherwise stop inside it.
        let edit = edit_between("a😀b", "a😁b");
        assert!(edit.start_byte <= 1);
        assert!(edit.old_end_byte >= 5);
        assert!(edit.start_position.column <= 1);
    }

    #[test]
    fn a_gap_stays_a_gap() {
        let (buffer, mut highlights) = highlights("rust", "fn a() {}   \n");
        highlights.spans(&buffer, 0..buffer.char_len());
        assert_eq!(highlights.token_at(0), Some(SyntaxToken::Keyword));
        assert_eq!(highlights.token_at(buffer.char_len()), None);
    }

    proptest::proptest! {
        // A parse of a random string with four grammars is not free. The
        // property is about agreement rather than about a rare input: forty
        // cases is enough to catch a wrong reuse and keeps the gate quick.
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(40))]

        /// An incremental parse says what a parse says. This is the property the
        /// whole design rests on: a keystroke cannot leave the colours of the
        /// file describing the file as it was.
        #[test]
        fn incremental_highlighting_is_the_whole_parse(
            before in "[ -~\\n]{0,120}",
            insert_at in 0usize..40,
            inserted in "[ -~\\n]{0,20}",
        ) {
            let mut buffer = Buffer::from_text(&before);
            let mut grammars = Grammars::new();
            let grammar = grammars.get("rust").expect("a grammar");
            let mut incremental = Highlights::new(grammar.clone());
            incremental.sync(&buffer);
            incremental.spans(&buffer, 0..buffer.char_len());

            let at = insert_at.min(buffer.char_len());
            buffer.apply(&[crate::edit::Replacement::of(at..at, inserted.clone())]);
            incremental.sync(&buffer);
            let incremental_spans = incremental.spans(&buffer, 0..buffer.char_len()).to_vec();

            let mut fresh = Highlights::new(grammar);
            fresh.sync(&buffer);
            proptest::prop_assert_eq!(
                incremental_spans,
                fresh.spans(&buffer, 0..buffer.char_len()).to_vec()
            );
        }

        /// Whatever the text, every span is inside it, in order, and honest about
        /// what it covers.
        #[test]
        fn every_span_is_inside_the_text(text in "[ -~\\n]{0,200}") {
            let (buffer, mut highlights) = highlights("rust", &text);
            let spans = highlights.spans(&buffer, 0..buffer.char_len()).to_vec();
            let mut previous_end = 0;
            for span in &spans {
                proptest::prop_assert!(!span.range.is_empty());
                proptest::prop_assert!(span.range.start >= previous_end, "sorted and disjoint");
                proptest::prop_assert!(span.range.end <= buffer.char_len());
                previous_end = span.range.end;
            }
        }
    }
}
