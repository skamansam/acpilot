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

    let secret = acpilot(&["secret", "--help"]);
    assert!(secret.status.success());
    insta::assert_snapshot!("secret_help", stdout(&secret));

    let serve = acpilot(&["serve", "--help"]);
    assert!(serve.status.success());
    insta::assert_snapshot!("serve_help", stdout(&serve));
}

#[test]
fn doctor_reports_a_fresh_machine_and_writes_nothing() {
    let config = TempDir::new();
    let workspace = TempDir::new();
    let output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .arg("doctor")
        .arg(workspace.path())
        .env("ACPILOT_HOME", config.path())
        // An empty PATH makes detection deterministic: no agent or language
        // server is found, which is the fresh-machine answer this asserts.
        .env("PATH", workspace.path())
        .output()
        .expect("run acpilot");

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("default") && text.contains("not created yet (not required)"),
        "{text}"
    );
    assert!(text.contains("declared   none"), "{text}");
    assert!(
        text.contains("temporary root, nothing is written"),
        "{text}"
    );
    assert!(text.contains("Language servers"), "{text}");
    assert!(
        !config.path().join("profiles").exists(),
        "doctor must not create a profile"
    );
}

#[test]
fn doctor_fails_only_when_a_declared_agent_cannot_run() {
    let config = TempDir::new();
    let workspace = TempDir::new();
    let profile = config.path().join("profiles/default");
    fs::create_dir_all(&profile).expect("profile directory");
    fs::write(profile.join("config.toml"), "schema_version = 1\n").expect("profile config");
    fs::write(
        profile.join("agents.toml"),
        "schema_version = 1\n\n[[agents]]\nid = \"ghost\"\nname = \"Ghost\"\n\
         transport = \"acp-stdio\"\ncommand = \"acpilot-doctor-no-such-program\"\n",
    )
    .expect("agent manifest");

    let output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .arg("doctor")
        .arg(workspace.path())
        .env("ACPILOT_HOME", config.path())
        .env("PATH", workspace.path())
        .output()
        .expect("run acpilot");

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("Ghost"), "{text}");
    assert!(text.contains("not on PATH"), "{text}");
    assert!(text.contains("Problems"), "{text}");
}

#[test]
fn doctor_reports_a_declared_agent_that_cannot_speak_acp() {
    let config = TempDir::new();
    let workspace = TempDir::new();
    let profile = config.path().join("profiles/default");
    fs::create_dir_all(&profile).expect("profile directory");
    fs::write(profile.join("config.toml"), "schema_version = 1\n").expect("profile config");

    // This binary is on disk but does not speak ACP: with no subcommand it
    // prints help and exits. A handshake against it must fail, and that failure
    // is a declared agent that cannot run — not a machine with nothing set up.
    let program = env!("CARGO_BIN_EXE_acpilot");
    fs::write(
        profile.join("agents.toml"),
        format!(
            "schema_version = 1\n\n[[agents]]\nid = \"mute\"\nname = \"Mute\"\n\
             transport = \"acp-stdio\"\ncommand = {program:?}\n"
        ),
    )
    .expect("agent manifest");

    let output = Command::new(program)
        .arg("doctor")
        .arg(workspace.path())
        .env("ACPILOT_HOME", config.path())
        .env("PATH", workspace.path())
        .output()
        .expect("run acpilot");

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("Mute"), "{text}");
    assert!(text.contains("handshake"), "{text}");
    assert!(text.contains("Problems"), "{text}");
}

#[test]
fn secret_set_with_empty_stdin_fails_before_it_reaches_the_keyring() {
    // `acpilot()` pipes an empty stdin. The command must refuse that rather than
    // store a zero-length credential, and it must do so before any keyring call.
    let output = acpilot(&["secret", "set", "anthropic"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("empty"), "{}", stderr(&output));
}

#[test]
fn a_secret_name_that_is_a_path_is_a_usage_error() {
    for args in [
        vec!["secret", "set", "../escape"],
        vec!["secret", "rm", "work/api key"],
    ] {
        let output = acpilot(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(!stderr(&output).is_empty(), "{args:?} says why");
    }
}

#[test]
fn secret_list_does_not_need_a_keyring_to_report_a_missing_manifest() {
    let config = TempDir::new();
    let output = acpilot(&[
        "secret",
        "list",
        "--config",
        config.path().to_str().expect("utf-8 path"),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("agents.toml"),
        "{}",
        stdout(&output)
    );
}
