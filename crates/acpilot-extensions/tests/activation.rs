//! Activation and precedence: what one profile sees, and why.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use acpilot_extensions::activation::{ACTIVATION_FILE, ProfileExtensions};
use acpilot_extensions::effective::Layer;
use acpilot_extensions::store::Store;
use acpilot_extensions::{Activation, ExtensionError, Resolution, UiToken};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("acpilot-act-{}-{n}", std::process::id()));
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
        fs::write(path, contents).expect("write file");
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

fn manifest(id: &str, version: &str, kind: &str, path: &str) -> String {
    let publisher = id.split('.').next().expect("namespaced id");
    format!(
        "schema_version = 1\nid = \"{id}\"\nname = \"Test pack\"\nversion = \"{version}\"\npublisher = \"{publisher}\"\ndescription = \"A test extension.\"\n\n[[contributes]]\nkind = \"{kind}\"\npath = \"{path}\"\n"
    )
}

fn install_server_pack(store: &Store, id: &str, version: &str, server: &str, command: &str) {
    let source = TempDir::new();
    source.write(
        "extension.toml",
        &manifest(id, version, "language_servers", "lsp.toml"),
    );
    source.write(
        "lsp.toml",
        &format!(
            "schema_version = 1\n\n[[servers]]\nid = \"{server}\"\nname = \"{server}\"\ncommand = \"{command}\"\nargs = [\"--stdio\"]\nlanguages = [\"rust\"]\nroot_markers = [\"Cargo.toml\"]\n"
        ),
    );
    store
        .install_from_dir(source.path())
        .expect("server pack installs");
}

fn install_theme_pack(store: &Store, id: &str, version: &str, theme_id: &str, partial: &str) {
    let source = TempDir::new();
    source.write(
        "extension.toml",
        &manifest(id, version, "theme", "theme.toml"),
    );
    let body = if partial.is_empty() {
        format!(
            "schema_version = 1\nid = \"{theme_id}\"\nname = \"Pack theme\"\nappearance = \"dark\"\n\n[ui]\n{UI}\n\n[syntax]\n{SYNTAX}\n"
        )
    } else {
        format!(
            "schema_version = 1\nid = \"{theme_id}\"\nname = \"Pack theme\"\nappearance = \"dark\"\n{partial}\n"
        )
    };
    source.write("theme.toml", &body);
    store
        .install_from_dir(source.path())
        .expect("theme pack installs");
}

fn open(store: &Store, profile: &TempDir) -> ProfileExtensions {
    ProfileExtensions::open(store.clone(), profile.path()).expect("open profile")
}

fn effective(profile: &ProfileExtensions) -> Resolution {
    profile
        .effective()
        .unwrap_or_else(|errors| panic!("effective failed: {errors:?}"))
}

#[test]
fn activation_round_trips_and_sorts() {
    let dir = TempDir::new();
    let path = dir.path().join(ACTIVATION_FILE);
    let mut activation = Activation::default();

    assert!(activation.activate("acpilot.zeta").unwrap());
    assert!(activation.activate("acpilot.alpha").unwrap());
    assert!(!activation.activate("acpilot.alpha").unwrap(), "idempotent");
    assert_eq!(activation.active, vec!["acpilot.alpha", "acpilot.zeta"]);

    activation.save(&path).expect("saves");
    let loaded = Activation::load(&path).expect("loads");
    assert_eq!(loaded, activation);

    assert!(loaded.is_active("acpilot.alpha"));
    assert!(!loaded.is_active("acpilot.absent"));
}

#[test]
fn a_missing_activation_file_is_an_empty_one() {
    let dir = TempDir::new();
    let activation = Activation::load(&dir.path().join(ACTIVATION_FILE)).expect("loads");
    assert!(activation.active.is_empty());
    assert_eq!(activation.schema_version, 1);
}

#[test]
fn unknown_keys_are_refused() {
    let error = Activation::parse(
        "extensions.toml",
        "schema_version = 1\nactive = []\nother = true\n",
    )
    .expect_err("unknown key must fail");
    assert!(error.to_string().contains("other"), "{error}");
}

#[test]
fn duplicate_entries_are_refused() {
    let error = Activation::parse(
        "extensions.toml",
        "schema_version = 1\nactive = [\"acpilot.pack\", \"acpilot.pack\"]\n",
    )
    .expect_err("duplicate must fail");
    assert!(matches!(error, ExtensionError::Duplicate { .. }), "{error}");
}

#[test]
fn activating_something_absent_is_not_found() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    let profile_dir = TempDir::new();
    let mut profile = open(&store, &profile_dir);

    match profile.activate("acpilot.pack") {
        Err(ExtensionError::NotFound { .. }) => {}
        other => panic!("expected not-found, got {other:?}"),
    }
}

#[test]
fn activating_a_blocked_extension_is_refused() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    install_theme_pack(&store, "acpilot.pack", "1.0.0", "acpilot.pack-night", "");
    let theme_path = store
        .version_path("acpilot.pack", "1.0.0")
        .join("theme.toml");
    let original = fs::read_to_string(&theme_path).unwrap();
    fs::write(&theme_path, original.replace("#e8edf6", "#111111")).unwrap();

    let profile_dir = TempDir::new();
    let mut profile = open(&store, &profile_dir);
    match profile.activate("acpilot.pack") {
        Err(ExtensionError::BlockedActivation { id, blocked }) => {
            assert_eq!(id, "acpilot.pack");
            assert!(blocked.contains("acpilot.pack@1.0.0"), "{blocked}");
            assert!(blocked.contains("checksum mismatch"), "{blocked}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(
        !profile_dir.path().join(ACTIVATION_FILE).exists(),
        "a refused activation writes nothing"
    );
}

#[test]
fn deactivation_changes_the_answer_without_a_restart() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    install_server_pack(
        &store,
        "acpilot.pack",
        "1.0.0",
        "pack-analyzer",
        "pack-analyzer",
    );

    let profile_dir = TempDir::new();
    let mut profile = open(&store, &profile_dir);
    profile.activate("acpilot.pack").expect("activates");

    assert!(
        effective(&profile)
            .registry
            .language_servers()
            .iter()
            .any(|trace| trace.value.id == "pack-analyzer")
    );

    profile.deactivate("acpilot.pack").expect("deactivates");
    assert!(
        !effective(&profile)
            .registry
            .language_servers()
            .iter()
            .any(|trace| trace.value.id == "pack-analyzer")
    );

    profile.activate("acpilot.pack").expect("reactivates");
    assert!(
        effective(&profile)
            .registry
            .language_servers()
            .iter()
            .any(|trace| trace.value.id == "pack-analyzer")
    );
}

#[test]
fn reload_sees_an_external_edit() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    install_server_pack(
        &store,
        "acpilot.pack",
        "1.0.0",
        "pack-analyzer",
        "pack-analyzer",
    );

    let profile_dir = TempDir::new();
    let mut profile = open(&store, &profile_dir);
    assert!(
        !effective(&profile)
            .registry
            .language_servers()
            .iter()
            .any(|trace| trace.value.id == "pack-analyzer")
    );

    // Another process (or the user in an editor) writes the file.
    profile_dir.write(
        ACTIVATION_FILE,
        "schema_version = 1\nactive = [\"acpilot.pack\"]\n",
    );
    profile.reload().expect("reloads");

    assert!(
        effective(&profile)
            .registry
            .language_servers()
            .iter()
            .any(|trace| trace.value.id == "pack-analyzer")
    );
}

#[test]
fn profile_beats_extension_beats_builtin() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    install_server_pack(
        &store,
        "acpilot.pack",
        "1.0.0",
        "rust-analyzer",
        "pack-analyzer",
    );

    let profile_dir = TempDir::new();
    let mut profile = open(&store, &profile_dir);
    profile.activate("acpilot.pack").expect("activates");

    // Extension beats built-in.
    let resolution = effective(&profile);
    let trace = resolution
        .registry
        .language_servers()
        .iter()
        .find(|trace| trace.value.id == "rust-analyzer")
        .expect("built-in still present");
    assert_eq!(trace.value.command, "pack-analyzer");
    assert_eq!(trace.layers.len(), 2);
    assert!(matches!(
        trace.layers[0],
        Layer::Activated { ref extension, .. } if extension == "acpilot.pack"
    ));
    assert!(matches!(
        trace.layers[1],
        Layer::BuiltIn { ref extension, .. } if extension == "acpilot.language-servers"
    ));

    // Profile beats both.
    profile_dir.write(
        "lsp.toml",
        "schema_version = 1\n\n[[servers]]\nid = \"rust-analyzer\"\nname = \"rust-analyzer\"\ncommand = \"profile-analyzer\"\nargs = []\nlanguages = [\"rust\"]\nroot_markers = [\"Cargo.toml\"]\n",
    );
    let resolution = effective(&profile);
    let trace = resolution
        .registry
        .language_servers()
        .iter()
        .find(|trace| trace.value.id == "rust-analyzer")
        .expect("still present");
    assert_eq!(trace.value.command, "profile-analyzer");
    assert_eq!(trace.layers.len(), 3);
    assert!(matches!(
        trace.layers[0],
        Layer::Profile { file: "lsp.toml" }
    ));

    let explanation = resolution
        .registry
        .explain("rust-analyzer")
        .expect("explains");
    let profile_at = explanation
        .find("profile lsp.toml")
        .expect("profile in chain");
    let extension_at = explanation
        .find("extension acpilot.pack 1.0.0")
        .expect("extension in chain");
    let builtin_at = explanation
        .find("built-in acpilot.language-servers 1.0.0")
        .expect("built-in in chain");
    assert!(profile_at < extension_at, "{explanation}");
    assert!(extension_at < builtin_at, "{explanation}");
    assert!(explanation.contains("(wins)"), "{explanation}");
}

#[test]
fn duplicate_presets_inside_one_layer_are_errors() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    install_server_pack(&store, "acpilot.one", "1.0.0", "shared", "one-analyzer");
    install_server_pack(&store, "acpilot.two", "1.0.0", "shared", "two-analyzer");

    let profile_dir = TempDir::new();
    let mut profile = open(&store, &profile_dir);
    profile.activate("acpilot.one").expect("activates one");
    profile.activate("acpilot.two").expect("activates two");

    let errors = profile.effective().expect_err("a collision must fail");
    assert!(
        errors.iter().any(|error| matches!(
            error,
            ExtensionError::Duplicate {
                what: "language-server preset",
                ..
            }
        )),
        "{errors:?}"
    );
}

#[test]
fn a_theme_cannot_shadow_a_built_in() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    install_theme_pack(&store, "acpilot.pack", "1.0.0", "acpilot.night-flight", "");

    let profile_dir = TempDir::new();
    let mut profile = open(&store, &profile_dir);
    profile.activate("acpilot.pack").expect("activates");

    let errors = profile.effective().expect_err("shadowing must fail");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, ExtensionError::Duplicate { what: "theme", .. })),
        "{errors:?}"
    );
}

#[test]
fn an_activated_theme_may_extend_a_built_in() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    install_theme_pack(
        &store,
        "acpilot.pack",
        "1.0.0",
        "acpilot.pack-night",
        "extends = \"acpilot.night-flight\"\n\n[ui]\naccent = \"#ffd98a\"",
    );

    let profile_dir = TempDir::new();
    let mut profile = open(&store, &profile_dir);
    profile.activate("acpilot.pack").expect("activates");

    let resolution = effective(&profile);
    let theme = resolution
        .registry
        .themes()
        .iter()
        .find(|theme| theme.id == "acpilot.pack-night")
        .expect("activated theme present");
    assert_eq!(
        theme.palette.ui(UiToken::Accent).unwrap().to_string(),
        "#ffd98a"
    );
    assert_eq!(
        theme.palette.ui(UiToken::Text).unwrap().to_string(),
        "#e8edf6",
        "inherited from the built-in"
    );
    assert!(!theme.built_in);
    assert_eq!(theme.extension_id, "acpilot.pack");
}

#[test]
fn duplicate_presets_in_the_profile_fragment_are_errors() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    let profile_dir = TempDir::new();
    profile_dir.write(
        "lsp.toml",
        "schema_version = 1\n\n[[servers]]\nid = \"mine\"\nname = \"mine\"\ncommand = \"a\"\n\n[[servers]]\nid = \"mine\"\nname = \"mine\"\ncommand = \"b\"\n",
    );
    let profile = open(&store, &profile_dir);

    let errors = profile.effective().expect_err("duplicate must fail");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, ExtensionError::Duplicate { .. })),
        "{errors:?}"
    );
}

#[test]
fn a_missing_active_extension_is_reported_and_built_ins_still_work() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    let profile_dir = TempDir::new();
    profile_dir.write(
        ACTIVATION_FILE,
        "schema_version = 1\nactive = [\"acpilot.absent\"]\n",
    );
    let profile = open(&store, &profile_dir);

    let resolution = effective(&profile);
    assert_eq!(resolution.missing, vec!["acpilot.absent".to_owned()]);
    assert!(resolution.blocked.is_empty());
    assert!(
        resolution
            .registry
            .language_servers()
            .iter()
            .any(|trace| trace.value.id == "rust-analyzer")
    );
}

#[test]
fn a_blocked_active_extension_is_reported_and_invisible() {
    let store_root = TempDir::new();
    let store = Store::new(store_root.path());
    install_theme_pack(&store, "acpilot.pack", "1.0.0", "acpilot.pack-night", "");
    let theme_path = store
        .version_path("acpilot.pack", "1.0.0")
        .join("theme.toml");
    let original = fs::read_to_string(&theme_path).unwrap();
    fs::write(&theme_path, original.replace("#e8edf6", "#121212")).unwrap();

    let profile_dir = TempDir::new();
    // The file is written directly: activation via the CLI would refuse, but a
    // file edited by hand or left behind by an interrupted update must still be
    // treated as blocked rather than trusted.
    profile_dir.write(
        ACTIVATION_FILE,
        "schema_version = 1\nactive = [\"acpilot.pack\"]\n",
    );
    let profile = open(&store, &profile_dir);
    let resolution = effective(&profile);

    assert_eq!(resolution.blocked.len(), 1);
    assert!(resolution.blocked[0].is_blocked());
    assert!(
        !resolution
            .registry
            .themes()
            .iter()
            .any(|theme| theme.id == "acpilot.pack-night"),
        "a blocked theme must not reach the registry"
    );
    assert_eq!(resolution.registry.themes().len(), 2, "built-ins only");
}
