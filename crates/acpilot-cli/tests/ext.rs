//! The `ext` verbs through the process boundary, because that is the interface
//! scripts and humans actually use.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("acpilot-cli-{}-{n}", std::process::id()));
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

fn acpilot(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(args)
        .output()
        .expect("run acpilot")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn install_server_pack(dir: &TempDir, version: &str, server: &str, command: &str) {
    dir.write(
        "extension.toml",
        &format!(
            "schema_version = 1\nid = \"acpilot.pack\"\nname = \"Pack\"\nversion = \"{version}\"\npublisher = \"acpilot\"\ndescription = \"Test pack.\"\n\n[[contributes]]\nkind = \"language_servers\"\npath = \"lsp.toml\"\n"
        ),
    );
    dir.write(
        "lsp.toml",
        &format!(
            "schema_version = 1\n\n[[servers]]\nid = \"{server}\"\nname = \"{server}\"\ncommand = \"{command}\"\nargs = []\nlanguages = [\"rust\"]\nroot_markers = [\"Cargo.toml\"]\n"
        ),
    );
}

#[test]
fn help_lists_the_ext_verbs() {
    let top = acpilot(&["--help"]);
    assert!(top.status.success());
    let top = text(&top);
    assert!(top.contains("ext"), "{top}");

    let ext = acpilot(&["ext", "--help"]);
    assert!(ext.status.success());
    let ext = text(&ext);
    for verb in ["add", "verify", "prune", "activate", "deactivate", "why"] {
        assert!(ext.contains(verb), "{verb} missing from:\n{ext}");
    }
}

#[test]
fn add_verify_activate_why_deactivate_round_trip() {
    let store = TempDir::new();
    let config = TempDir::new();
    let source = TempDir::new();
    install_server_pack(&source, "1.0.0", "rust-analyzer", "pack-analyzer");

    let store_arg = store.path().to_str().unwrap();
    let config_arg = config.path().to_str().unwrap();

    let added = acpilot(&[
        "ext",
        "add",
        "--from",
        source.path().to_str().unwrap(),
        "--store",
        store_arg,
    ]);
    assert!(added.status.success(), "{}", text(&added));
    assert!(text(&added).contains("installed acpilot.pack@1.0.0"));

    let verified = acpilot(&["ext", "verify", "--all", "--store", store_arg]);
    assert!(verified.status.success(), "{}", text(&verified));
    assert!(text(&verified).contains("ok      acpilot.pack@1.0.0"));

    let before = acpilot(&[
        "ext",
        "why",
        "rust-analyzer",
        "--profile",
        "work",
        "--store",
        store_arg,
        "--config",
        config_arg,
    ]);
    assert!(before.status.success(), "{}", text(&before));
    assert!(text(&before).contains("built-in acpilot.language-servers 1.0.0"));
    assert!(!text(&before).contains("acpilot.pack"), "{}", text(&before));

    let activated = acpilot(&[
        "ext",
        "activate",
        "acpilot.pack",
        "--profile",
        "work",
        "--store",
        store_arg,
        "--config",
        config_arg,
    ]);
    assert!(activated.status.success(), "{}", text(&activated));
    assert!(text(&activated).contains("activated acpilot.pack in profile work"));

    let after = acpilot(&[
        "ext",
        "why",
        "rust-analyzer",
        "--profile",
        "work",
        "--store",
        store_arg,
        "--config",
        config_arg,
    ]);
    assert!(after.status.success(), "{}", text(&after));
    let after = text(&after);
    assert!(after.contains("extension acpilot.pack 1.0.0"), "{after}");
    assert!(after.contains("pack-analyzer"), "{after}");
    assert!(after.contains("(wins)"), "{after}");

    let activation_file = config.path().join("profiles/work/extensions.toml");
    let contents = fs::read_to_string(&activation_file).expect("activation written");
    assert!(contents.contains("acpilot.pack"), "{contents}");

    let deactivated = acpilot(&[
        "ext",
        "deactivate",
        "acpilot.pack",
        "--profile",
        "work",
        "--store",
        store_arg,
        "--config",
        config_arg,
    ]);
    assert!(deactivated.status.success(), "{}", text(&deactivated));

    let restored = acpilot(&[
        "ext",
        "why",
        "rust-analyzer",
        "--profile",
        "work",
        "--store",
        store_arg,
        "--config",
        config_arg,
    ]);
    assert!(restored.status.success(), "{}", text(&restored));
    assert!(!text(&restored).contains("extension acpilot.pack"));
    assert!(text(&restored).contains("built-in acpilot.language-servers 1.0.0"));
}

#[test]
fn verify_fails_on_a_tampered_extension() {
    let store = TempDir::new();
    let source = TempDir::new();
    install_server_pack(&source, "1.0.0", "rust-analyzer", "pack-analyzer");
    let store_arg = store.path().to_str().unwrap();

    assert!(
        acpilot(&[
            "ext",
            "add",
            "--from",
            source.path().to_str().unwrap(),
            "--store",
            store_arg
        ])
        .status
        .success()
    );

    let lsp = store.path().join("acpilot.pack/1.0.0/lsp.toml");
    let original = fs::read_to_string(&lsp).unwrap();
    fs::write(&lsp, original.replace("pack-analyzer", "evil-analyzer")).unwrap();

    let verified = acpilot(&["ext", "verify", "--all", "--store", store_arg]);
    assert_eq!(verified.status.code(), Some(1), "{}", text(&verified));
    let verified = text(&verified);
    assert!(
        verified.contains("blocked acpilot.pack@1.0.0"),
        "{verified}"
    );
    assert!(verified.contains("checksum mismatch"), "{verified}");
}

#[test]
fn verify_without_all_is_a_usage_error() {
    let store = TempDir::new();
    let output = acpilot(&["ext", "verify", "--store", store.path().to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(text(&output).contains("--all"));
}

#[test]
fn why_for_an_unknown_preset_is_not_found() {
    let store = TempDir::new();
    let config = TempDir::new();
    let output = acpilot(&[
        "ext",
        "why",
        "no-such-preset",
        "--profile",
        "work",
        "--store",
        store.path().to_str().unwrap(),
        "--config",
        config.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
}

#[test]
fn activating_a_blocked_extension_is_refused() {
    let store = TempDir::new();
    let config = TempDir::new();
    let source = TempDir::new();
    install_server_pack(&source, "1.0.0", "rust-analyzer", "pack-analyzer");
    let store_arg = store.path().to_str().unwrap();
    assert!(
        acpilot(&[
            "ext",
            "add",
            "--from",
            source.path().to_str().unwrap(),
            "--store",
            store_arg
        ])
        .status
        .success()
    );

    let lsp = store.path().join("acpilot.pack/1.0.0/lsp.toml");
    let original = fs::read_to_string(&lsp).unwrap();
    fs::write(&lsp, original.replace("pack-analyzer", "other-analyzer")).unwrap();

    let output = acpilot(&[
        "ext",
        "activate",
        "acpilot.pack",
        "--profile",
        "work",
        "--store",
        store_arg,
        "--config",
        config.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(text(&output).contains("cannot activate acpilot.pack"));
    assert!(!config.path().join("profiles/work/extensions.toml").exists());
}

#[test]
fn prune_reports_then_removes_superseded_versions() {
    let store = TempDir::new();
    let config = TempDir::new();
    let store_arg = store.path().to_str().unwrap();
    let config_arg = config.path().to_str().unwrap();

    for version in ["1.0.0", "1.1.0"] {
        let source = TempDir::new();
        install_server_pack(&source, version, "rust-analyzer", "pack-analyzer");
        let added = acpilot(&[
            "ext",
            "add",
            "--from",
            source.path().to_str().unwrap(),
            "--store",
            store_arg,
        ]);
        assert!(added.status.success(), "{}", text(&added));
    }

    let dry = acpilot(&[
        "ext",
        "prune",
        "--dry-run",
        "--store",
        store_arg,
        "--config",
        config_arg,
    ]);
    assert!(dry.status.success(), "{}", text(&dry));
    assert!(text(&dry).contains("would remove acpilot.pack@1.0.0"));
    assert!(store.path().join("acpilot.pack/1.0.0").exists());

    let pruned = acpilot(&["ext", "prune", "--store", store_arg, "--config", config_arg]);
    assert!(pruned.status.success(), "{}", text(&pruned));
    assert!(text(&pruned).contains("removed acpilot.pack@1.0.0"));
    assert!(!store.path().join("acpilot.pack/1.0.0").exists());
    assert!(store.path().join("acpilot.pack/1.1.0").exists());
}

#[test]
fn prune_never_removes_the_only_installed_version() {
    let store = TempDir::new();
    let config = TempDir::new();
    let source = TempDir::new();
    install_server_pack(&source, "1.0.0", "rust-analyzer", "pack-analyzer");
    let store_arg = store.path().to_str().unwrap();
    let added = acpilot(&[
        "ext",
        "add",
        "--from",
        source.path().to_str().unwrap(),
        "--store",
        store_arg,
    ]);
    assert!(added.status.success(), "{}", text(&added));

    let pruned = acpilot(&[
        "ext",
        "prune",
        "--store",
        store_arg,
        "--config",
        config.path().to_str().unwrap(),
    ]);
    assert!(pruned.status.success(), "{}", text(&pruned));
    assert!(text(&pruned).contains("nothing to prune"));
    assert!(store.path().join("acpilot.pack/1.0.0").exists());
}

const TMTHEME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>name</key><string>Hangar</string>
  <key>settings</key>
  <array>
    <dict><key>settings</key><dict>
      <key>background</key><string>#0b0f14</string>
      <key>foreground</key><string>#e6edf3</string>
      <key>caret</key><string>#ffcc66</string>
      <key>gutterForeground</key><string>#7d8590</string>
      <key>lineHighlight</key><string>#161b22</string>
    </dict></dict>
    <dict><key>scope</key><string>comment</string><key>settings</key><dict><key>foreground</key><string>#151a1f</string></dict></dict>
    <dict><key>scope</key><string>keyword, storage</string><key>settings</key><dict><key>foreground</key><string>#ff7b72</string></dict></dict>
    <dict><key>scope</key><string>string</string><key>settings</key><dict><key>foreground</key><string>#a5d6ff</string></dict></dict>
  </array>
</dict>
</plist>
"#;

#[test]
fn import_theme_installs_a_converted_theme_with_warnings() {
    let store = TempDir::new();
    let config = TempDir::new();
    let source = TempDir::new();
    source.write("hangar.tmTheme", TMTHEME);
    let store_arg = store.path().to_str().unwrap();
    let config_arg = config.path().to_str().unwrap();

    let imported = acpilot(&[
        "ext",
        "import-theme",
        "--from",
        source.path().join("hangar.tmTheme").to_str().unwrap(),
        "--id",
        "acpilot.hangar",
        "--store",
        store_arg,
        "--config",
        config_arg,
        "--activate",
        "work",
    ]);
    assert!(imported.status.success(), "{}", text(&imported));
    let imported_text = text(&imported);
    assert!(
        imported_text.contains("installed acpilot.hangar@1.0.0"),
        "{imported_text}"
    );
    assert!(
        imported_text.contains("activated acpilot.hangar in profile work"),
        "{imported_text}"
    );
    assert!(imported_text.contains("warning:"), "{imported_text}");
    assert!(imported_text.contains("below AA"), "{imported_text}");

    let theme = fs::read_to_string(store.path().join("acpilot.hangar/1.0.0/theme.toml")).unwrap();
    assert!(
        theme.contains("extends = \"acpilot.night-flight\""),
        "{theme}"
    );
    assert!(theme.contains("appearance = \"dark\""), "{theme}");

    let verified = acpilot(&["ext", "verify", "--all", "--store", store_arg]);
    assert!(verified.status.success(), "{}", text(&verified));
    assert!(text(&verified).contains("ok      acpilot.hangar@1.0.0"));
}
