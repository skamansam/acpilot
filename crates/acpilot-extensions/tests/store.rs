//! The store's promises, one test each: atomic install, a checksum that cannot
//! drift silently, and a blocked version that no loader can reach.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use acpilot_extensions::ExtensionError;
use acpilot_extensions::store::{BlockReason, Store, StoreStatus, hash_dir};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("acpilot-store-{}-{n}", std::process::id()));
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

fn theme(id: &str) -> String {
    format!(
        "schema_version = 1\nid = \"{id}\"\nname = \"Test theme\"\nappearance = \"dark\"\n\n[ui]\n{UI}\n\n[syntax]\n{SYNTAX}\n"
    )
}

fn write_theme(source: &TempDir, id: &str, version: &str) {
    source.write(
        "extension.toml",
        &manifest(id, version, "theme", "theme.toml"),
    );
    source.write("theme.toml", &theme(id));
}

fn install(store: &Store, id: &str, version: &str) {
    let source = TempDir::new();
    write_theme(&source, id, version);
    store
        .install_from_dir(source.path())
        .expect("fixture installs");
}

#[test]
fn install_records_a_checksum_and_verifies() {
    let source = TempDir::new();
    write_theme(&source, "acpilot.test", "1.0.0");
    let root = TempDir::new();
    let store = Store::new(root.path());

    let installed = store.install_from_dir(source.path()).expect("installs");
    assert_eq!(installed.id, "acpilot.test");
    assert_eq!(installed.version, "1.0.0");
    assert!(installed.is_ready());
    assert_eq!(installed.recorded, installed.computed);
    assert!(
        installed
            .recorded
            .as_deref()
            .is_some_and(|checksum| checksum.starts_with("sha256:"))
    );
    assert_eq!(
        installed.path,
        root.path().join("acpilot.test").join("1.0.0")
    );
    assert!(store.record_path("acpilot.test", "1.0.0").is_file());
    assert_eq!(
        fs::read_to_string(store.record_path("acpilot.test", "1.0.0")).unwrap(),
        format!("{}\n", installed.recorded.unwrap())
    );

    let verified = store.verify("acpilot.test", "1.0.0").expect("verifies");
    assert!(verified.is_ready());
}

#[test]
fn the_checksum_format_is_pinned() {
    // Pinning the exact bytes means a release cannot change what a checksum
    // means without every existing install reporting a mismatch.
    let root = TempDir::new();
    root.write("a.txt", "hello");
    root.write("b/c.txt", "world");

    assert_eq!(
        hash_dir(root.path()).unwrap(),
        "sha256:dfe3513d5acdc5a4fb1459fe5e8f84a974977eb1d86efee806562a62e414a39c"
    );
}

#[test]
fn the_hash_does_not_depend_on_write_order() {
    let first = TempDir::new();
    first.write("one.txt", "1");
    first.write("nested/two.txt", "2");
    first.write("nested/three.txt", "3");

    let second = TempDir::new();
    second.write("nested/three.txt", "3");
    second.write("nested/two.txt", "2");
    second.write("one.txt", "1");

    assert_eq!(
        hash_dir(first.path()).unwrap(),
        hash_dir(second.path()).unwrap()
    );

    second.write("nested/two.txt", "2 ");
    assert_ne!(
        hash_dir(first.path()).unwrap(),
        hash_dir(second.path()).unwrap(),
        "a changed byte must change the checksum"
    );
}

#[test]
fn a_tampered_asset_is_blocked_and_invisible() {
    let root = TempDir::new();
    let store = Store::new(root.path());
    install(&store, "acpilot.test", "1.0.0");

    let theme_path = root
        .path()
        .join("acpilot.test")
        .join("1.0.0")
        .join("theme.toml");
    let original = fs::read_to_string(&theme_path).unwrap();
    fs::write(&theme_path, original.replace("#e8edf6", "#101010")).unwrap();

    let verified = store.verify("acpilot.test", "1.0.0").unwrap();
    assert!(verified.is_blocked());
    match &verified.status {
        StoreStatus::Blocked(BlockReason::Mismatch { recorded, computed }) => {
            assert_ne!(recorded, computed);
            assert!(recorded.starts_with("sha256:"));
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }

    let active = store
        .resolve_active(&["acpilot.test".to_owned()])
        .expect("resolution reports blocked rather than failing");
    assert!(active.extensions.is_empty(), "blocked must not load");
    assert_eq!(active.blocked.len(), 1);
    assert!(active.missing.is_empty());

    assert!(
        store.load_latest().expect("loads").is_empty(),
        "the latest-version loader must not see blocked bytes either"
    );
}

#[test]
fn a_missing_record_is_blocked() {
    let root = TempDir::new();
    let store = Store::new(root.path());
    install(&store, "acpilot.test", "1.0.0");
    fs::remove_file(store.record_path("acpilot.test", "1.0.0")).unwrap();

    let verified = store.verify("acpilot.test", "1.0.0").unwrap();
    assert_eq!(
        verified.status,
        StoreStatus::Blocked(BlockReason::Unrecorded)
    );
}

#[test]
fn a_malformed_record_is_blocked() {
    let root = TempDir::new();
    let store = Store::new(root.path());
    install(&store, "acpilot.test", "1.0.0");
    fs::write(
        store.record_path("acpilot.test", "1.0.0"),
        "not a checksum\n",
    )
    .unwrap();

    let verified = store.verify("acpilot.test", "1.0.0").unwrap();
    assert!(matches!(
        verified.status,
        StoreStatus::Blocked(BlockReason::BadRecord { .. })
    ));
}

#[test]
fn a_valid_hash_over_an_unloadable_tree_is_blocked() {
    let root = TempDir::new();
    let store = Store::new(root.path());
    install(&store, "acpilot.test", "1.0.0");

    // Recompute the record so the checksum matches: this isolates the manifest
    // check from the tamper check, which is the point of the test.
    let version = root.path().join("acpilot.test").join("1.0.0");
    fs::write(version.join("extension.toml"), "schema_version = 1\n").unwrap();
    let checksum = hash_dir(&version).unwrap();
    fs::write(
        store.record_path("acpilot.test", "1.0.0"),
        format!("{checksum}\n"),
    )
    .unwrap();

    let verified = store.verify("acpilot.test", "1.0.0").unwrap();
    assert!(matches!(
        verified.status,
        StoreStatus::Blocked(BlockReason::InvalidManifest { .. })
    ));
}

#[test]
fn reinstalling_a_version_is_refused_and_leaves_the_original() {
    let root = TempDir::new();
    let store = Store::new(root.path());
    install(&store, "acpilot.test", "1.0.0");

    let source = TempDir::new();
    write_theme(&source, "acpilot.test", "1.0.0");
    source.write(
        "theme.toml",
        &theme("acpilot.test").replace("#070d18", "#000000"),
    );

    match store.install_from_dir(source.path()) {
        Err(ExtensionError::AlreadyInstalled { id, version }) => {
            assert_eq!(id, "acpilot.test");
            assert_eq!(version, "1.0.0");
        }
        other => panic!("expected already-installed, got {other:?}"),
    }
    assert!(
        store.verify("acpilot.test", "1.0.0").unwrap().is_ready(),
        "the original bytes must be untouched"
    );
}

#[test]
fn an_invalid_extension_leaves_nothing_behind() {
    let source = TempDir::new();
    source.write("extension.toml", "schema_version = 1\n");
    let root = TempDir::new();
    let store = Store::new(root.path());

    assert!(store.install_from_dir(source.path()).is_err());
    assert!(
        !root.path().join("acpilot.test").exists(),
        "no id directory for a failed install"
    );
    let staging = root.path().join(".staging");
    if staging.exists() {
        assert_eq!(
            fs::read_dir(&staging).unwrap().count(),
            0,
            "staging directories clean themselves up"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_in_the_source_is_refused() {
    use std::os::unix::fs::symlink;

    let outside = TempDir::new();
    outside.write("secret", "do not ship this");
    let source = TempDir::new();
    source.write(
        "extension.toml",
        &manifest("acpilot.test", "1.0.0", "theme", "theme.toml"),
    );
    source.write("theme.toml", &theme("acpilot.test"));
    symlink(outside.path().join("secret"), source.path().join("link")).unwrap();

    let root = TempDir::new();
    let store = Store::new(root.path());
    assert!(matches!(
        store.install_from_dir(source.path()),
        Err(ExtensionError::UnsupportedEntry { .. })
    ));
    assert!(!root.path().join("acpilot.test").exists());
}

#[test]
fn remove_deletes_the_version_and_record() {
    let root = TempDir::new();
    let store = Store::new(root.path());
    install(&store, "acpilot.test", "1.0.0");

    store.remove("acpilot.test", "1.0.0").expect("removes");
    assert!(!store.version_path("acpilot.test", "1.0.0").exists());
    assert!(!store.record_path("acpilot.test", "1.0.0").exists());
    assert!(!root.path().join("acpilot.test").exists());
    assert!(matches!(
        store.remove("acpilot.test", "1.0.0"),
        Err(ExtensionError::NotFound { .. })
    ));
}

#[test]
fn prune_removes_superseded_versions_and_keeps_the_newest() {
    let root = TempDir::new();
    let store = Store::new(root.path());
    install(&store, "acpilot.test", "1.0.0");
    install(&store, "acpilot.test", "1.1.0");

    let keep = std::collections::BTreeSet::new();
    let plan = store.plan_prune(&keep).expect("plans");
    assert_eq!(plan, vec![("acpilot.test".to_owned(), "1.0.0".to_owned())]);
    assert!(
        store.version_path("acpilot.test", "1.0.0").exists(),
        "planning must not delete anything"
    );

    let removed = store.prune(&keep).expect("prunes");
    assert_eq!(removed, plan);
    assert!(!store.version_path("acpilot.test", "1.0.0").exists());
    assert!(store.verify("acpilot.test", "1.1.0").unwrap().is_ready());
}

#[test]
fn prune_never_removes_the_only_copy() {
    let root = TempDir::new();
    let store = Store::new(root.path());
    install(&store, "acpilot.test", "1.0.0");

    let keep = std::collections::BTreeSet::new();
    assert!(store.plan_prune(&keep).expect("plans").is_empty());
    assert!(store.prune(&keep).expect("prunes").is_empty());
    assert!(store.verify("acpilot.test", "1.0.0").unwrap().is_ready());
}

#[test]
fn activation_resolves_the_newest_ready_version() {
    let root = TempDir::new();
    let store = Store::new(root.path());
    install(&store, "acpilot.test", "1.0.0");
    install(&store, "acpilot.test", "1.1.0");

    let active = store
        .resolve_active(&["acpilot.test".to_owned()])
        .expect("resolves");
    assert_eq!(active.extensions.len(), 1);
    assert_eq!(active.extensions[0].manifest.version, "1.1.0");
}

#[test]
fn resolving_an_absent_id_reports_it_as_missing() {
    let root = TempDir::new();
    let store = Store::new(root.path());

    let active = store
        .resolve_active(&["acpilot.absent".to_owned()])
        .expect("resolves");
    assert!(active.extensions.is_empty());
    assert!(active.blocked.is_empty());
    assert_eq!(active.missing, vec!["acpilot.absent".to_owned()]);
}

#[test]
fn scan_reports_directories_that_are_not_store_entries() {
    let root = TempDir::new();
    let store = Store::new(root.path());
    install(&store, "acpilot.test", "1.0.0");
    fs::create_dir_all(root.path().join("acpilot.test").join("not-a-version")).unwrap();
    fs::create_dir_all(root.path().join("not-namespaced").join("1.0.0")).unwrap();

    let scan = store.scan().expect("scans");
    let blocked: Vec<_> = scan.iter().filter(|stored| stored.is_blocked()).collect();
    assert_eq!(blocked.len(), 2, "{scan:#?}");
    assert!(blocked.iter().all(|stored| matches!(
        stored.status,
        StoreStatus::Blocked(BlockReason::InvalidName { .. })
    )));
    assert!(
        scan.iter()
            .any(|stored| stored.label() == "acpilot.test@1.0.0")
    );
}

#[test]
fn a_missing_store_is_empty_not_broken() {
    let root = TempDir::new();
    let store = Store::new(root.path().join("does-not-exist-yet"));
    assert!(store.scan().expect("scans").is_empty());
    assert!(store.load_latest().expect("loads").is_empty());
}
