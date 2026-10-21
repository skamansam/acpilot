//! Importing themes from other editors.
//!
//! TextMate `.tmTheme` files are XML plists: a global colour dict, then a list of
//! `{ scope, settings }` rules. That maps onto our token set well enough to be
//! useful, and badly enough that the importer is honest about it: whatever the
//! file does not define is inherited from the built-in theme with the same
//! appearance, and any pair that lands below WCAG AA comes back as a warning
//! rather than pretending the import is perfect.
//!
//! The parser is deliberately small: `dict`, `array`, `string`, `integer`,
//! `real`, `true`, `false`. No DTDs, no external entities — the file is read as
//! text and nothing in it is fetched or executed.

use std::collections::BTreeMap;
use std::str::FromStr;

use acpilot_proto::extension::ThemeAppearance;

use crate::builtin;
use crate::contrast::{AA_NORMAL_TEXT, audit};
use crate::error::ExtensionError;
use crate::manifest::{Contribution, Manifest, SCHEMA_VERSION, validate_namespaced_id};
use crate::theme::{Color, SyntaxToken, Theme, UiToken};

/// A theme converted from another editor, with the two files it needs.
#[derive(Debug, Clone)]
pub struct ImportedTheme {
    /// `extension.toml`, validated.
    pub manifest_toml: String,
    /// The theme file, validated.
    pub theme_toml: String,
    /// Id the extension will be installed under.
    pub id: String,
    /// Display name taken from the source, or the id.
    pub name: String,
    /// Dark or light, derived from the source background.
    pub appearance: ThemeAppearance,
    /// Things worth saying out loud: ignored colours, below-AA pairs.
    pub warnings: Vec<String>,
}

/// Convert a `.tmTheme` document into an installable theme extension.
///
/// `id` must be namespaced and must differ from the base it inherits, or the
/// extension would collide with a built-in.
pub fn from_tmtheme(id: &str, version: &str, text: &str) -> Result<ImportedTheme, ExtensionError> {
    validate_namespaced_id("theme id", id)?;

    let parsed = parse_plist(text).map_err(|message| ExtensionError::Import {
        path: "tmTheme".to_owned(),
        message,
    })?;
    let root = parsed.as_dict().ok_or_else(|| ExtensionError::Import {
        path: "tmTheme".to_owned(),
        message: "the document root is not a <dict>".to_owned(),
    })?;

    let mut warnings = Vec::new();
    let name = root
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(id)
        .to_owned();

    let mut ui: BTreeMap<UiToken, Color> = BTreeMap::new();
    let mut syntax: BTreeMap<SyntaxToken, Color> = BTreeMap::new();

    let entries = root
        .get("settings")
        .and_then(Value::as_array)
        .ok_or_else(|| ExtensionError::Import {
            path: "tmTheme".to_owned(),
            message: "no `settings` array: this is not a TextMate theme".to_owned(),
        })?;

    for (index, entry) in entries.iter().enumerate() {
        let Some(entry) = entry.as_dict() else {
            warnings.push(format!("settings[{index}] is not a <dict>; ignored"));
            continue;
        };
        let Some(settings) = entry.get("settings").and_then(Value::as_dict) else {
            warnings.push(format!("settings[{index}] has no settings; ignored"));
            continue;
        };

        match entry.get("scope").and_then(Value::as_str) {
            None | Some("") => {
                apply_globals(settings, &mut ui, &mut warnings);
            }
            Some(scopes) => apply_scope(scopes, settings, &mut syntax, &mut warnings),
        }
    }

    let appearance = match ui.get(&UiToken::Bg) {
        Some(background) if background.relative_luminance() > 0.5 => ThemeAppearance::Light,
        _ => ThemeAppearance::Dark,
    };
    let base = match appearance {
        ThemeAppearance::Dark => "acpilot.night-flight",
        ThemeAppearance::Light => "acpilot.paper",
    };
    if id == base {
        return Err(ExtensionError::Import {
            path: "tmTheme".to_owned(),
            message: format!("{id} is a built-in theme id; import it under your own publisher"),
        });
    }

    let theme = Theme {
        schema_version: SCHEMA_VERSION,
        id: id.to_owned(),
        name: name.clone(),
        appearance,
        extends: Some(base.to_owned()),
        ui,
        syntax,
    };
    theme.validate("theme.toml")?;

    let publisher = id
        .split('.')
        .next()
        .expect("a namespaced id has a publisher");
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        id: id.to_owned(),
        name: name.clone(),
        version: version.to_owned(),
        publisher: publisher.to_owned(),
        description: format!("Imported from a TextMate theme ({name})."),
        homepage: None,
        min_acpilot: None,
        contributes: vec![Contribution {
            kind: acpilot_proto::extension::ExtensionKind::Theme,
            path: "theme.toml".to_owned(),
        }],
    };
    manifest.validate("extension.toml")?;

    warnings.extend(contrast_warnings(&theme, base));

    let manifest_toml = toml::to_string(&manifest).map_err(|error| ExtensionError::Import {
        path: "extension.toml".to_owned(),
        message: error.to_string(),
    })?;
    let theme_toml = toml::to_string(&theme).map_err(|error| ExtensionError::Import {
        path: "theme.toml".to_owned(),
        message: error.to_string(),
    })?;

    Ok(ImportedTheme {
        manifest_toml,
        theme_toml,
        id: id.to_owned(),
        name,
        appearance,
        warnings,
    })
}

fn apply_globals(
    settings: &BTreeMap<String, Value>,
    ui: &mut BTreeMap<UiToken, Color>,
    warnings: &mut Vec<String>,
) {
    for (key, token) in [
        ("background", UiToken::Bg),
        ("foreground", UiToken::Text),
        ("caret", UiToken::Accent),
        ("gutterForeground", UiToken::TextMuted),
        ("lineHighlight", UiToken::BgPanel),
        ("invisibles", UiToken::Border),
    ] {
        let Some(raw) = settings.get(key).and_then(Value::as_str) else {
            continue;
        };
        match color(raw) {
            Ok((color, alpha)) => {
                ui.insert(token, color);
                if alpha {
                    warnings.push(format!("{token}: colour {raw:?} has alpha; RGB used"));
                }
            }
            Err(message) => warnings.push(format!("{token}: {message}")),
        }
    }
}

fn apply_scope(
    scopes: &str,
    settings: &BTreeMap<String, Value>,
    syntax: &mut BTreeMap<SyntaxToken, Color>,
    warnings: &mut Vec<String>,
) {
    let Some(raw) = settings.get("foreground").and_then(Value::as_str) else {
        return;
    };
    let color = match color(raw) {
        Ok((color, alpha)) => {
            if alpha {
                warnings.push(format!("{scopes}: colour {raw:?} has alpha; RGB used"));
            }
            color
        }
        Err(message) => {
            warnings.push(format!("{scopes}: {message}"));
            return;
        }
    };

    for selector in scopes.split(',') {
        let selector = selector.trim();
        if let Some(token) = token_for_scope(selector) {
            // Later rules override earlier ones, which is how TextMate resolves
            // specificity too.
            syntax.insert(token, color);
        }
    }
}

/// Which token a TextMate scope selector fills.
fn token_for_scope(scope: &str) -> Option<SyntaxToken> {
    const MAP: [(&str, SyntaxToken); 10] = [
        ("comment", SyntaxToken::Comment),
        ("keyword.operator", SyntaxToken::Operator),
        ("keyword", SyntaxToken::Keyword),
        ("string", SyntaxToken::String),
        ("constant.numeric", SyntaxToken::Number),
        ("constant", SyntaxToken::Constant),
        ("entity.name.type", SyntaxToken::Type),
        ("entity.name.class", SyntaxToken::Type),
        ("support.type", SyntaxToken::Type),
        ("support.class", SyntaxToken::Type),
    ];
    if let Some((_, token)) = MAP
        .iter()
        .find(|(prefix, _)| scope == *prefix || scope.starts_with(&format!("{prefix}.")))
    {
        return Some(*token);
    }

    for (prefix, token) in [
        ("entity.name.function", SyntaxToken::Function),
        ("support.function", SyntaxToken::Function),
        ("variable", SyntaxToken::Variable),
        ("punctuation", SyntaxToken::Punctuation),
        ("storage.type", SyntaxToken::Type),
    ] {
        if scope == prefix || scope.starts_with(&format!("{prefix}.")) {
            return Some(token);
        }
    }
    None
}

fn contrast_warnings(theme: &Theme, base_id: &str) -> Vec<String> {
    let Ok(built_ins) = builtin::load_all() else {
        return Vec::new();
    };
    let mut base = None;
    for extension in &built_ins {
        for contribution in &extension.contributions {
            if let crate::loader::Contribution::Theme(candidate) = contribution
                && candidate.id == base_id
            {
                base = Some(candidate.resolve(None, &extension.manifest.id, true));
            }
        }
    }
    let Some(base) = base else {
        return Vec::new();
    };

    let resolved = theme.resolve(Some(&base.palette), &theme.id, false);
    audit(&resolved)
        .failures(AA_NORMAL_TEXT)
        .into_iter()
        .map(|pair| {
            format!(
                "{} on {} is {:.2}:1, below AA {AA_NORMAL_TEXT:.2}:1",
                pair.foreground.as_str(),
                pair.surface.as_str(),
                pair.ratio
            )
        })
        .collect()
}

/// Parse `#rgb`, `#rrggbb`, or `#rrggbbaa`.
///
/// The flag is `true` when the alpha channel was dropped, so the caller can say
/// so rather than silently changing how a colour looks.
fn color(raw: &str) -> Result<(Color, bool), String> {
    let digits = raw
        .trim()
        .strip_prefix('#')
        .ok_or_else(|| format!("expected a #rrggbb colour, got {raw:?}"))?;
    let digits = digits.to_ascii_lowercase();
    match digits.len() {
        3 => {
            let expand: String = digits.chars().flat_map(|c| [c, c]).collect();
            Color::from_str(&format!("#{expand}"))
                .map(|color| (color, false))
                .map_err(|error| error.to_string())
        }
        6 => Color::from_str(&format!("#{digits}"))
            .map(|color| (color, false))
            .map_err(|error| error.to_string()),
        8 => composite_rgb(raw)
            .map(|color| (color, true))
            .ok_or_else(|| format!("expected a #rrggbb or #rrggbbaa colour, got {raw:?}")),
        _ => Err(format!("expected a #rrggbb colour, got {raw:?}")),
    }
}

/// RGB half of an eight-digit colour, ignoring alpha.
fn composite_rgb(raw: &str) -> Option<Color> {
    let digits = raw.trim().strip_prefix('#')?;
    if digits.len() != 8 {
        return None;
    }
    Color::from_str(&format!("#{}", &digits[..6])).ok()
}

// --- A very small plist reader -------------------------------------------

/// The plist values a theme can contain.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    String(String),
    Dict(BTreeMap<String, Value>),
    Array(Vec<Value>),
    Bool(bool),
    Integer(i64),
    Real(f64),
}

impl Value {
    fn as_dict(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Self::Dict(dict) => Some(dict),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Value]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Open(String),
    Close(String),
    Empty(String),
    Text(String),
}

fn parse_plist(text: &str) -> Result<Value, String> {
    let tokens = tokenize(text)?;
    let mut parser = Parser { tokens, at: 0 };
    let value = parser.value()?;
    Ok(value)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn value(&mut self) -> Result<Value, String> {
        loop {
            let Some(token) = self.tokens.get(self.at).cloned() else {
                return Err("the document ended before a value".to_owned());
            };
            self.at += 1;
            match token {
                Token::Text(text) if text.trim().is_empty() => continue,
                Token::Text(text) => return Err(format!("unexpected text {text:?}")),
                Token::Open(name) => return self.open(&name),
                Token::Empty(name) => return Self::empty(&name),
                Token::Close(name) => return Err(format!("unexpected </{name}>")),
            }
        }
    }

    fn open(&mut self, name: &str) -> Result<Value, String> {
        match name {
            "plist" => {
                let value = self.value()?;
                self.expect_close("plist")?;
                Ok(value)
            }
            "dict" => {
                let mut dict = BTreeMap::new();
                loop {
                    match self.tokens.get(self.at) {
                        Some(Token::Text(text)) if text.trim().is_empty() => self.at += 1,
                        Some(Token::Close(close)) if close == "dict" => {
                            self.at += 1;
                            return Ok(Value::Dict(dict));
                        }
                        Some(Token::Open(open)) if open == "key" => {
                            self.at += 1;
                            let key = self.text_until("key")?;
                            let value = self.value()?;
                            dict.insert(key, value);
                        }
                        other => return Err(format!("unexpected {other:?} in <dict>")),
                    }
                }
            }
            "array" => {
                let mut items = Vec::new();
                loop {
                    match self.tokens.get(self.at) {
                        Some(Token::Text(text)) if text.trim().is_empty() => self.at += 1,
                        Some(Token::Close(close)) if close == "array" => {
                            self.at += 1;
                            return Ok(Value::Array(items));
                        }
                        None => return Err("unterminated <array>".to_owned()),
                        _ => items.push(self.value()?),
                    }
                }
            }
            "string" => Ok(Value::String(self.text_until("string")?)),
            "integer" => Ok(Value::Integer(
                self.text_until("integer")?
                    .trim()
                    .parse()
                    .map_err(|_| "an <integer> is not a number".to_owned())?,
            )),
            "real" => Ok(Value::Real(
                self.text_until("real")?
                    .trim()
                    .parse()
                    .map_err(|_| "a <real> is not a number".to_owned())?,
            )),
            "date" | "data" => {
                self.text_until(name)?;
                Ok(Value::String(String::new()))
            }
            other => Err(format!("unsupported plist element <{other}>")),
        }
    }

    fn empty(name: &str) -> Result<Value, String> {
        match name {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            other => Err(format!("unsupported empty plist element <{other}/>")),
        }
    }

    fn text_until(&mut self, name: &str) -> Result<String, String> {
        let mut text = String::new();
        loop {
            match self.tokens.get(self.at) {
                Some(Token::Text(chunk)) => {
                    text.push_str(chunk);
                    self.at += 1;
                }
                Some(Token::Close(close)) if close == name => {
                    self.at += 1;
                    return Ok(text);
                }
                other => return Err(format!("unexpected {other:?} inside <{name}>")),
            }
        }
    }

    fn expect_close(&mut self, name: &str) -> Result<(), String> {
        loop {
            match self.tokens.get(self.at) {
                Some(Token::Text(text)) if text.trim().is_empty() => self.at += 1,
                Some(Token::Close(close)) if close == name => {
                    self.at += 1;
                    return Ok(());
                }
                other => return Err(format!("expected </{name}>, found {other:?}")),
            }
        }
    }
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let text = strip_comments(text)?;
    let mut tokens = Vec::new();
    let mut rest = text.as_str();

    while let Some(start) = rest.find('<') {
        let chunk = &rest[..start];
        if !chunk.trim().is_empty() {
            tokens.push(Token::Text(unescape(chunk)?));
        } else if !chunk.is_empty() {
            tokens.push(Token::Text(chunk.to_owned()));
        }
        let end = rest[start..]
            .find('>')
            .ok_or_else(|| "unterminated tag".to_owned())?
            + start;
        let inner = rest[start + 1..end].trim();
        rest = &rest[end + 1..];

        if inner.starts_with('?') || inner.starts_with('!') {
            continue;
        }
        if let Some(name) = inner.strip_prefix('/') {
            tokens.push(Token::Close(tag_name(name).to_owned()));
        } else if let Some(name) = inner.strip_suffix('/') {
            tokens.push(Token::Empty(tag_name(name).to_owned()));
        } else if inner.is_empty() {
            return Err("empty tag".to_owned());
        } else {
            tokens.push(Token::Open(tag_name(inner).to_owned()));
        }
    }

    if !rest.trim().is_empty() {
        return Err(format!("trailing text {rest:?}"));
    }
    Ok(tokens)
}

/// The element name, without attributes such as `<plist version="1.0">`.
fn tag_name(tag: &str) -> &str {
    tag.split_whitespace().next().unwrap_or("")
}

fn strip_comments(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        let end = after
            .find("-->")
            .ok_or_else(|| "unterminated comment".to_owned())?;
        rest = &after[end + 3..];
    }
    out.push_str(rest);
    Ok(out)
}

fn unescape(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after
            .find(';')
            .ok_or_else(|| "unterminated entity".to_owned())?;
        let entity = &after[..end];
        let character = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            other => {
                if let Some(hex) = other
                    .strip_prefix("#x")
                    .or_else(|| other.strip_prefix("#X"))
                {
                    let code = u32::from_str_radix(hex, 16)
                        .map_err(|_| format!("bad character reference &{other};"))?;
                    char::from_u32(code)
                        .ok_or_else(|| format!("bad character reference &{other};"))?
                } else if let Some(decimal) = other.strip_prefix('#') {
                    let code = decimal
                        .parse::<u32>()
                        .map_err(|_| format!("bad character reference &{other};"))?;
                    char::from_u32(code)
                        .ok_or_else(|| format!("bad character reference &{other};"))?
                } else {
                    return Err(format!("unknown entity &{other};"));
                }
            }
        };
        out.push(character);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_reader_handles_the_shapes_a_theme_uses() {
        let value = parse_plist(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <!-- a comment with > inside -->
            <plist version="1.0"><dict>
              <key>name</key><string>A &amp; B &#65;</string>
              <key>list</key><array><integer>1</integer><real>1.5</real><true/><false/></array>
            </dict></plist>"#,
        )
        .expect("parses");
        let root = value.as_dict().expect("root dict");
        assert_eq!(root.get("name").and_then(Value::as_str), Some("A & B A"));
        assert_eq!(
            root.get("list")
                .and_then(Value::as_array)
                .map(<[Value]>::len),
            Some(4)
        );
    }

    #[test]
    fn truncated_documents_are_errors() {
        assert!(parse_plist("<plist><dict><key>a</key>").is_err());
        assert!(parse_plist("<plist><dict></plist></dict>").is_err());
    }

    #[test]
    fn colours_accept_the_three_common_forms() {
        assert_eq!(color("#fff").unwrap().0.to_string(), "#ffffff");
        assert_eq!(color("#0A0B0C").unwrap().0.to_string(), "#0a0b0c");
        assert_eq!(color("#0a0b0cff").unwrap().0.to_string(), "#0a0b0c");
        assert!(color("#0a0b0cff").unwrap().1, "alpha is reported");
        assert!(!color("#0a0b0c").unwrap().1);
        assert!(color("red").is_err());
        assert!(color("#12345").is_err());
    }

    #[test]
    fn scopes_map_to_the_nearest_token() {
        assert_eq!(
            token_for_scope("comment.line.double-slash"),
            Some(SyntaxToken::Comment)
        );
        assert_eq!(
            token_for_scope("constant.numeric.integer"),
            Some(SyntaxToken::Number)
        );
        assert_eq!(
            token_for_scope("entity.name.function"),
            Some(SyntaxToken::Function)
        );
        assert_eq!(
            token_for_scope("keyword.operator.assignment"),
            Some(SyntaxToken::Operator)
        );
        assert_eq!(token_for_scope("meta.block"), None);
    }
}
