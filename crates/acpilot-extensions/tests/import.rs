//! TextMate import: what maps, what is inherited, and what the importer refuses.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use acpilot_extensions::builtin;
use acpilot_extensions::loader::load_dir;
use acpilot_extensions::manifest::Manifest;
use acpilot_extensions::theme::{SyntaxToken, Theme, UiToken};
use acpilot_extensions::theme_import::from_tmtheme;
use acpilot_extensions::{Registry, ThemeAppearance};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("acpilot-tm-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn theme(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>name</key><string>Hangar</string>
  <key>settings</key>
  <array>
    <dict>
      <key>settings</key>
      <dict>
        <key>background</key><string>{background}</string>
        <key>foreground</key><string>#e6edf3</string>
        <key>caret</key><string>#ffcc66</string>
        <key>gutterForeground</key><string>#7d8590</string>
        <key>lineHighlight</key><string>#161b22</string>
      </dict>
    </dict>
    {body}
  </array>
</dict>
</plist>
"#,
        background = "#0b0f14"
    )
}

const RULES: &str = r#"
    <dict><key>scope</key><string>comment</string><key>settings</key><dict><key>foreground</key><string>#8b949e</string></dict></dict>
    <dict><key>scope</key><string>keyword, storage</string><key>settings</key><dict><key>foreground</key><string>#ff7b72</string></dict></dict>
    <dict><key>scope</key><string>string</string><key>settings</key><dict><key>foreground</key><string>#a5d6ff</string></dict></dict>
    <dict><key>scope</key><string>entity.name.function</string><key>settings</key><dict><key>foreground</key><string>#d2a8ff</string></dict></dict>
    <dict><key>scope</key><string>constant.numeric</string><key>settings</key><dict><key>foreground</key><string>#79c0ff</string></dict></dict>
"#;

#[test]
fn imports_a_dark_theme_and_inherits_the_rest() {
    let imported = from_tmtheme("acpilot.hangar", "1.0.0", &theme(RULES)).expect("imports");

    assert_eq!(imported.id, "acpilot.hangar");
    assert_eq!(imported.name, "Hangar");
    assert_eq!(imported.appearance, ThemeAppearance::Dark);

    let parsed = Theme::parse("theme.toml", &imported.theme_toml).expect("theme is valid");
    assert_eq!(parsed.extends.as_deref(), Some("acpilot.night-flight"));
    assert_eq!(
        parsed.ui.get(&UiToken::Accent).unwrap().to_string(),
        "#ffcc66"
    );
    assert_eq!(
        parsed
            .syntax
            .get(&SyntaxToken::Comment)
            .unwrap()
            .to_string(),
        "#8b949e"
    );

    let manifest = Manifest::parse("extension.toml", &imported.manifest_toml).expect("valid");
    assert_eq!(manifest.id, "acpilot.hangar");
    assert_eq!(manifest.publisher, "acpilot");
    assert_eq!(manifest.version, "1.0.0");
    assert_eq!(manifest.contributes.len(), 1);
    assert_eq!(manifest.contributes[0].path, "theme.toml");
}

#[test]
fn the_imported_theme_resolves_against_the_built_in_base() {
    let imported = from_tmtheme("acpilot.hangar", "1.0.0", &theme(RULES)).expect("imports");

    let dir = TempDir::new();
    dir.write("extension.toml", &imported.manifest_toml);
    dir.write("theme.toml", &imported.theme_toml);
    let extension = load_dir(dir.path()).expect("generated extension loads");

    let mut extensions = builtin::load_all().expect("built-ins load");
    extensions.push(extension);
    let registry = Registry::new(extensions).expect("no collisions");

    let resolved = registry.theme("acpilot.hangar").expect("theme registered");
    assert_eq!(resolved.appearance, ThemeAppearance::Dark);
    assert!(!resolved.built_in);
    assert_eq!(resolved.extension_id, "acpilot.hangar");
    assert_eq!(
        resolved
            .palette
            .syntax(SyntaxToken::Punctuation)
            .unwrap()
            .to_string(),
        "#97a5b8",
        "punctuation was not in the file, so it comes from night-flight"
    );
    assert_eq!(
        resolved.palette.ui(UiToken::Text).unwrap().to_string(),
        "#e6edf3"
    );
}

#[test]
fn a_light_background_inherits_the_paper_theme() {
    let mut body = theme(RULES);
    body = body
        .replace("#0b0f14", "#fbf7ef")
        .replace("#e6edf3", "#1a2333");
    let imported = from_tmtheme("acpilot.daylight", "1.0.0", &body).expect("imports");

    assert_eq!(imported.appearance, ThemeAppearance::Light);
    let parsed = Theme::parse("theme.toml", &imported.theme_toml).expect("valid");
    assert_eq!(parsed.extends.as_deref(), Some("acpilot.paper"));
}

#[test]
fn later_scope_rules_win() {
    let body = r#"
    <dict><key>scope</key><string>comment</string><key>settings</key><dict><key>foreground</key><string>#8b949e</string></dict></dict>
    <dict><key>scope</key><string>comment.line</string><key>settings</key><dict><key>foreground</key><string>#aaaaaa</string></dict></dict>
"#;
    let imported = from_tmtheme("acpilot.hangar", "1.0.0", &theme(body)).expect("imports");
    let parsed = Theme::parse("theme.toml", &imported.theme_toml).expect("valid");
    assert_eq!(
        parsed
            .syntax
            .get(&SyntaxToken::Comment)
            .unwrap()
            .to_string(),
        "#aaaaaa"
    );
}

#[test]
fn short_and_alpha_colours_are_understood() {
    let body = r#"
    <dict><key>scope</key><string>string</string><key>settings</key><dict><key>foreground</key><string>#ABC</string></dict></dict>
    <dict><key>scope</key><string>keyword</string><key>settings</key><dict><key>foreground</key><string>#ff7b72ff</string></dict></dict>
"#;
    let imported = from_tmtheme("acpilot.hangar", "1.0.0", &theme(body)).expect("imports");
    let parsed = Theme::parse("theme.toml", &imported.theme_toml).expect("valid");
    assert_eq!(
        parsed.syntax.get(&SyntaxToken::String).unwrap().to_string(),
        "#aabbcc"
    );
    assert_eq!(
        parsed
            .syntax
            .get(&SyntaxToken::Keyword)
            .unwrap()
            .to_string(),
        "#ff7b72"
    );
    assert!(
        imported
            .warnings
            .iter()
            .any(|warning| warning.contains("alpha")),
        "{:?}",
        imported.warnings
    );
}

#[test]
fn a_below_aa_pair_is_reported_not_hidden() {
    let body = r#"
    <dict><key>scope</key><string>comment</string><key>settings</key><dict><key>foreground</key><string>#151a1f</string></dict></dict>
"#;
    let imported = from_tmtheme("acpilot.hangar", "1.0.0", &theme(body)).expect("imports");
    assert!(
        imported
            .warnings
            .iter()
            .any(|warning| warning.contains("comment on bg") && warning.contains("below AA")),
        "{:?}",
        imported.warnings
    );
}

#[test]
fn a_built_in_id_is_refused() {
    let error = from_tmtheme("acpilot.night-flight", "1.0.0", &theme(RULES))
        .expect_err("built-in ids are reserved");
    assert!(error.to_string().contains("built-in"), "{error}");
}

#[test]
fn a_bare_id_is_refused() {
    let error = from_tmtheme("hangar", "1.0.0", &theme(RULES)).expect_err("must be namespaced");
    assert!(error.to_string().contains("publisher.name"), "{error}");
}

#[test]
fn a_document_that_is_not_a_theme_is_refused() {
    let error = from_tmtheme("acpilot.hangar", "1.0.0", "<plist><dict></dict></plist>")
        .expect_err("no settings");
    assert!(error.to_string().contains("settings"), "{error}");

    let error = from_tmtheme("acpilot.hangar", "1.0.0", "<plist><dict><key>a</key>")
        .expect_err("truncated");
    assert!(error.to_string().contains("ended"), "{error}");
}
