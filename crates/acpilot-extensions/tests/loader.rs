//! The manifest matrix: the rules that must fail loudly, one test each.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use acpilot_extensions::loader::{Contribution, load_dir};
use acpilot_extensions::{ExtensionError, Registry, UiToken};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A directory that removes itself. No dev-dependency for six lines.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("acpilot-ext-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write file");
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

const UI: &str = r##"bg = "#070d18"
bg_panel = "#0c1424"
text = "#e8edf6"
text_muted = "#97a5b8"
accent = "#f7c65c"
accent_high = "#ffe3a8"
border = "#24334a"
success = "#59c98b"
warning = "#f7c65c"
danger = "#ff7a7a""##;

const SYNTAX: &str = r##"comment = "#7d8ca3"
keyword = "#f7c65c"
string = "#8ad6a0"
number = "#e0a3ff"
type = "#8fc7ff"
function = "#ffd9a0"
variable = "#e8edf6"
constant = "#ffb3c1"
operator = "#b9c4d4"
punctuation = "#97a5b8""##;

fn theme(id: &str) -> String {
    format!(
        "schema_version = 1\nid = \"{id}\"\nname = \"Test theme\"\nappearance = \"dark\"\n\n[ui]\n{UI}\n\n[syntax]\n{SYNTAX}\n"
    )
}

fn child_theme(id: &str, base: &str) -> String {
    format!(
        "schema_version = 1\nid = \"{id}\"\nname = \"Child\"\nappearance = \"dark\"\nextends = \"{base}\"\n\n[ui]\naccent = \"#ffe3a8\"\n"
    )
}

fn manifest(id: &str, kind: &str, path: &str) -> String {
    let publisher = id.split('.').next().expect("namespaced id");
    // Escape backslashes so a path written in the test reaches the loader
    // verbatim instead of being read as a TOML escape sequence.
    let path = path.replace('\\', "\\\\");
    format!(
        "schema_version = 1\nid = \"{id}\"\nname = \"Test pack\"\nversion = \"1.0.0\"\npublisher = \"{publisher}\"\ndescription = \"A test extension.\"\n\n[[contributes]]\nkind = \"{kind}\"\npath = \"{path}\"\n"
    )
}

fn assert_unsafe_path(error: ExtensionError, expected: &str) {
    match error {
        ExtensionError::UnsafePath { path, .. } => assert_eq!(path, expected),
        other => panic!("expected an unsafe-path error, got {other}"),
    }
}

#[test]
fn loads_a_theme_extension() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "theme", "theme.toml"),
    );
    dir.write("theme.toml", &theme("acpilot.test"));

    let extension = load_dir(dir.path()).expect("loads");
    assert_eq!(extension.manifest.id, "acpilot.test");
    assert!(!extension.built_in);
    assert!(extension.warnings.is_empty());
    match &extension.contributions[0] {
        Contribution::Theme(theme) => {
            assert_eq!(theme.id, "acpilot.test");
            assert_eq!(theme.ui.len(), 10);
        }
        other => panic!("expected a theme, got {other:?}"),
    }
}

#[test]
fn unknown_manifest_keys_are_refused() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &format!(
            "{}\noops = true\n",
            manifest("acpilot.test", "theme", "theme.toml")
        ),
    );
    dir.write("theme.toml", &theme("acpilot.test"));

    let error = load_dir(dir.path()).expect_err("unknown key must fail");
    let message = error.to_string();
    assert!(message.contains("oops"), "{message}");
}

#[test]
fn a_newer_schema_names_both_versions() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "theme", "theme.toml"),
    );
    dir.write(
        "theme.toml",
        &theme("acpilot.test").replace("schema_version = 1", "schema_version = 2"),
    );
    match load_dir(dir.path()) {
        Err(ExtensionError::SchemaVersion {
            found, expected, ..
        }) => {
            assert_eq!(found, 2);
            assert_eq!(expected, 1);
        }
        other => panic!("expected a schema-version error, got {other:?}"),
    }
}

#[test]
fn a_bare_extension_id_is_refused() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "theme", "theme.toml")
            .replace("id = \"acpilot.test\"", "id = \"test\""),
    );
    dir.write("theme.toml", &theme("acpilot.test"));

    assert!(matches!(
        load_dir(dir.path()),
        Err(ExtensionError::InvalidId { .. })
    ));
}

#[test]
fn the_id_must_sit_under_the_publisher() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("evil.test", "theme", "theme.toml")
            .replace("publisher = \"evil\"", "publisher = \"acpilot\""),
    );
    dir.write("theme.toml", &theme("acpilot.test"));

    let error = load_dir(dir.path()).expect_err("must fail");
    assert!(error.to_string().contains("publisher"), "{error}");
}

#[test]
fn a_theme_from_another_publisher_is_refused() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "theme", "theme.toml"),
    );
    dir.write("theme.toml", &theme("someone-else.theme"));

    match load_dir(dir.path()) {
        Err(ExtensionError::InvalidId { what, .. }) => assert_eq!(what, "theme id"),
        other => panic!("expected an invalid theme id, got {other:?}"),
    }
}

#[test]
fn contribution_paths_may_not_escape_the_directory() {
    for path in [
        "../outside.toml",
        "themes/../../outside.toml",
        "/etc/passwd",
        "C:/windows/win.ini",
        "themes\\theme.toml",
        "themes//theme.toml",
        "./theme.toml",
    ] {
        let dir = TempDir::new();
        dir.write("extension.toml", &manifest("acpilot.test", "theme", path));
        dir.write("theme.toml", &theme("acpilot.test"));

        let error = load_dir(dir.path()).expect_err(path);
        assert_unsafe_path(error, path);
    }
}

#[test]
fn a_missing_contribution_is_an_error() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "theme", "themes/missing.toml"),
    );

    match load_dir(dir.path()) {
        Err(ExtensionError::MissingContribution { path, kind }) => {
            assert_eq!(path, "themes/missing.toml");
            assert_eq!(kind, "theme");
        }
        other => panic!("expected a missing contribution, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_directory_is_refused() {
    use std::os::unix::fs::symlink;

    let outside = TempDir::new();
    outside.write("theme.toml", &theme("acpilot.test"));

    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "theme", "linked.toml"),
    );
    symlink(
        outside.path().join("theme.toml"),
        dir.path().join("linked.toml"),
    )
    .expect("create symlink");

    let error = load_dir(dir.path()).expect_err("a link out must fail");
    assert_unsafe_path(error, "linked.toml");
}

#[test]
fn theme_tokens_are_a_closed_set() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "theme", "theme.toml"),
    );
    dir.write(
        "theme.toml",
        &theme("acpilot.test").replace("text_muted", "text_mutedd"),
    );

    let error = load_dir(dir.path()).expect_err("typo must fail");
    let message = error.to_string();
    assert!(message.contains("text_mutedd"), "{message}");
}

#[test]
fn a_base_theme_must_be_complete() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "theme", "theme.toml"),
    );
    dir.write(
        "theme.toml",
        &theme("acpilot.test").replace("danger = \"#ff7a7a\"\n", ""),
    );

    let error = load_dir(dir.path()).expect_err("missing token must fail");
    let message = error.to_string();
    assert!(message.contains("danger"), "{message}");
    assert!(message.contains("extends"), "{message}");
}

#[test]
fn a_child_theme_inherits_and_overrides() {
    let base = TempDir::new();
    base.write(
        "extension.toml",
        &manifest("acpilot.base", "theme", "base.toml"),
    );
    base.write("base.toml", &theme("acpilot.base"));

    let child = TempDir::new();
    child.write(
        "extension.toml",
        &manifest("acpilot.child", "theme", "child.toml"),
    );
    child.write("child.toml", &child_theme("acpilot.child", "acpilot.base"));

    let registry = Registry::new(vec![
        load_dir(base.path()).expect("base loads"),
        load_dir(child.path()).expect("child loads"),
    ])
    .expect("registry merges");

    let child = registry.theme("acpilot.child").expect("child resolved");
    assert_eq!(
        child
            .palette
            .ui(UiToken::Accent)
            .expect("own accent")
            .to_string(),
        "#ffe3a8"
    );
    assert_eq!(
        child
            .palette
            .ui(UiToken::Text)
            .expect("inherited text")
            .to_string(),
        "#e8edf6"
    );
    assert_eq!(child.palette.syntax.len(), 10, "syntax inherited whole");
    assert_eq!(child.extension_id, "acpilot.child");
}

#[test]
fn extending_a_theme_nobody_provides_is_an_error() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.child", "theme", "child.toml"),
    );
    dir.write(
        "child.toml",
        &child_theme("acpilot.child", "acpilot.missing"),
    );

    let errors = Registry::new(vec![load_dir(dir.path()).expect("loads")])
        .expect_err("unknown base must fail");
    assert!(
        errors[0].to_string().contains("acpilot.missing"),
        "{:?}",
        errors
    );
}

#[test]
fn an_extends_cycle_is_refused() {
    let first = TempDir::new();
    first.write(
        "extension.toml",
        &manifest("acpilot.first", "theme", "one.toml"),
    );
    first.write("one.toml", &child_theme("acpilot.first", "acpilot.second"));

    let second = TempDir::new();
    second.write(
        "extension.toml",
        &manifest("acpilot.second", "theme", "two.toml"),
    );
    second.write("two.toml", &child_theme("acpilot.second", "acpilot.first"));

    let errors = Registry::new(vec![
        load_dir(first.path()).expect("loads"),
        load_dir(second.path()).expect("loads"),
    ])
    .expect_err("a cycle must fail");
    assert!(
        errors
            .iter()
            .any(|error| error.to_string().contains("cycle")),
        "{errors:?}"
    );
}

#[test]
fn duplicate_theme_ids_are_refused() {
    let first = TempDir::new();
    first.write(
        "extension.toml",
        &manifest("acpilot.one", "theme", "theme.toml"),
    );
    first.write("theme.toml", &theme("acpilot.dup"));

    let second = TempDir::new();
    second.write(
        "extension.toml",
        &manifest("acpilot.two", "theme", "theme.toml"),
    );
    second.write("theme.toml", &theme("acpilot.dup"));

    let errors = Registry::new(vec![
        load_dir(first.path()).expect("loads"),
        load_dir(second.path()).expect("loads"),
    ])
    .expect_err("a theme collision must fail");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, ExtensionError::Duplicate { what: "theme", .. })),
        "{errors:?}"
    );
}

#[test]
fn deferred_kinds_are_loaded_faithfully_but_not_read() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "grammars", "grammars/rust"),
    );
    dir.write("grammars/rust/manifest.toml", "name = \"rust\"\n");

    let extension = load_dir(dir.path()).expect("grammar path exists");
    assert_eq!(extension.warnings.len(), 1);
    assert!(extension.warnings[0].contains("P4.6-8"));
    match &extension.contributions[0] {
        Contribution::Unloaded { kind, path, .. } => {
            assert_eq!(*kind, acpilot_proto::extension::ExtensionKind::Grammars);
            assert_eq!(path, "grammars/rust");
        }
        other => panic!("expected an unloaded contribution, got {other:?}"),
    }
}

#[test]
fn min_acpilot_is_enforced() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "theme", "theme.toml").replace(
            "version = \"1.0.0\"",
            "version = \"1.0.0\"\nmin_acpilot = \"99.0.0\"",
        ),
    );
    dir.write("theme.toml", &theme("acpilot.test"));

    match load_dir(dir.path()) {
        Err(ExtensionError::NeedsNewerAcpilot { required, .. }) => assert_eq!(required, "99.0.0"),
        other => panic!("expected a min_acpilot error, got {other:?}"),
    }
}

#[test]
fn a_short_version_is_refused() {
    let dir = TempDir::new();
    dir.write(
        "extension.toml",
        &manifest("acpilot.test", "theme", "theme.toml").replace("1.0.0", "1.0"),
    );
    dir.write("theme.toml", &theme("acpilot.test"));

    match load_dir(dir.path()) {
        Err(ExtensionError::InvalidId { what, .. }) => assert_eq!(what, "version"),
        other => panic!("expected a version error, got {other:?}"),
    }
}

#[test]
fn duplicate_contribution_paths_are_refused() {
    let dir = TempDir::new();
    let manifest = manifest("acpilot.test", "theme", "theme.toml")
        + "\n[[contributes]]\nkind = \"theme\"\npath = \"theme.toml\"\n";
    dir.write("extension.toml", &manifest);
    dir.write("theme.toml", &theme("acpilot.test"));

    assert!(matches!(
        load_dir(dir.path()),
        Err(ExtensionError::DuplicateContribution { .. })
    ));
}
