//! Scripting contract: the process boundary scripts actually depend on.
//!
//! `ext.rs` covers what the extension verbs do. This suite covers how they say
//! it: exit codes, streams, environment handling, and help text that a completion
//! script or a human reads. Snapshots here are deliberate — changing help output
//! is a user-visible change and should show up as a diff.

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
        let path = std::env::temp_dir().join(format!("acpilot-cli-cli-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
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

fn acpilot_with_env(args: &[&str], key: &str, value: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(args)
        .env(key, value)
        .output()
        .expect("run acpilot")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn install_pack(dir: &TempDir) {
    fs::write(
        dir.path().join("extension.toml"),
        "schema_version = 1\nid = \"acpilot.pack\"\nname = \"Pack\"\nversion = \"1.0.0\"\npublisher = \"acpilot\"\ndescription = \"Test pack.\"\n\n[[contributes]]\nkind = \"language_servers\"\npath = \"lsp.toml\"\n",
    )
    .expect("write manifest");
    fs::write(
        dir.path().join("lsp.toml"),
        "schema_version = 1\n\n[[servers]]\nid = \"rust-analyzer\"\nname = \"rust-analyzer\"\ncommand = \"pack-analyzer\"\nargs = []\nlanguages = [\"rust\"]\nroot_markers = [\"Cargo.toml\"]\n",
    )
    .expect("write lsp preset");
}

#[test]
fn version_prints_the_build_and_the_api_prefix() {
    let output = acpilot(&["version"]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        format!("acpilot {}\napi      /v1\n", env!("CARGO_PKG_VERSION")),
        "scripts parse this; keep the two lines stable"
    );
}

#[test]
fn no_subcommand_is_a_usage_error_that_still_prints_help() {
    let output = acpilot(&[]);
    assert_eq!(output.status.code(), Some(2));
    let text = stdout(&output);
    assert!(text.contains("Usage: acpilot"), "{text}");
}

#[test]
fn an_unknown_subcommand_is_a_usage_error_on_stderr() {
    let output = acpilot(&["nope"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("unrecognized subcommand"),
        "{output:?}"
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn missing_and_duplicate_installs_have_distinct_codes() {
    let store = TempDir::new();
    let store_arg = store.path().to_str().expect("utf-8 path");

    let missing = acpilot(&[
        "ext",
        "add",
        "--from",
        store.path().join("does-not-exist").to_str().expect("utf-8"),
        "--store",
        store_arg,
    ]);
    assert_eq!(missing.status.code(), Some(1), "{}", stderr(&missing));
    assert!(stderr(&missing).starts_with("error: "), "{missing:?}");

    let source = TempDir::new();
    install_pack(&source);
    let first = acpilot(&[
        "ext",
        "add",
        "--from",
        source.path().to_str().expect("utf-8"),
        "--store",
        store_arg,
    ]);
    assert!(first.status.success(), "{}", stderr(&first));

    let again = acpilot(&[
        "ext",
        "add",
        "--from",
        source.path().to_str().expect("utf-8"),
        "--store",
        store_arg,
    ]);
    assert_eq!(
        again.status.code(),
        Some(4),
        "a duplicate is a conflict, not a generic failure: {}",
        stderr(&again)
    );
    assert!(stderr(&again).contains("already installed"), "{again:?}");
}

#[test]
fn the_profile_comes_from_the_environment_when_the_flag_is_absent() {
    let store = TempDir::new();
    let config = TempDir::new();
    let source = TempDir::new();
    install_pack(&source);
    let store_arg = store.path().to_str().expect("utf-8");
    let config_arg = config.path().to_str().expect("utf-8");

    assert!(
        acpilot(&[
            "ext",
            "add",
            "--from",
            source.path().to_str().expect("utf-8"),
            "--store",
            store_arg,
        ])
        .status
        .success()
    );

    let activated = acpilot_with_env(
        &[
            "ext",
            "activate",
            "acpilot.pack",
            "--store",
            store_arg,
            "--config",
            config_arg,
        ],
        "ACPILOT_PROFILE",
        "work",
    );
    assert!(activated.status.success(), "{}", stderr(&activated));
    assert!(stdout(&activated).contains("profile work"));
    assert!(
        config.path().join("profiles/work/extensions.toml").exists(),
        "the environment variable selects the profile, not `default`"
    );
    assert!(!config.path().join("profiles/default").exists());
}

#[test]
fn help_is_stable() {
    let top = acpilot(&["--help"]);
    assert!(top.status.success());
    insta::assert_snapshot!("help", stdout(&top));

    let ext = acpilot(&["ext", "--help"]);
    assert!(ext.status.success());
    insta::assert_snapshot!("ext_help", stdout(&ext));
}
