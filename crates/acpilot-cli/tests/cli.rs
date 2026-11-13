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
fn config_explain_names_the_winning_layer() {
    let config = TempDir::new();
    let workspace = TempDir::new();
    fs::write(
        config.path().join("config.toml"),
        "[preferences]\ntab_width = 2\n",
    )
    .expect("global settings");
    fs::create_dir_all(workspace.path().join(".acpilot")).expect("project marker");
    fs::write(
        workspace.path().join(".acpilot/config.toml"),
        "[preferences]\ntab_width = 8\n",
    )
    .expect("project settings");

    let output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .arg("config")
        .arg("explain")
        .arg("tab_width")
        .arg(workspace.path())
        .env("ACPILOT_HOME", config.path())
        .output()
        .expect("run acpilot config explain");

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("preferences.tab_width = 8"), "{text}");
    assert!(text.contains("from"), "{text}");
    assert!(
        text.contains(".acpilot/config.toml") || text.contains(".acpilot\\config.toml"),
        "the project file is named as the winner: {text}"
    );
}

#[test]
fn a_marker_naming_a_missing_profile_exits_three_with_the_fix() {
    let config = TempDir::new();
    let workspace = TempDir::new();
    fs::create_dir_all(workspace.path().join(".acpilot")).expect("project marker");
    fs::write(
        workspace.path().join(".acpilot/config.toml"),
        "profile = \"work\"\n",
    )
    .expect("project settings");

    let output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .arg("config")
        .arg("explain")
        .arg("tab_width")
        .arg(workspace.path())
        .env("ACPILOT_HOME", config.path())
        .output()
        .expect("run acpilot config explain");

    assert_eq!(
        output.status.code(),
        Some(3),
        "a missing profile is not found, not a generic failure: {}",
        stderr(&output)
    );
    let text = stderr(&output);
    assert!(
        text.contains("acpilot profile new work"),
        "the hint is actionable: {text}"
    );
    assert!(
        text.contains(".acpilot/config.toml") || text.contains(".acpilot\\config.toml"),
        "{text}"
    );
    assert!(
        !config.path().join("profiles/default").exists(),
        "a missing marker must not fall back to default"
    );
}

#[test]
fn profile_rm_purges_the_config_state_and_index_row() {
    let home = TempDir::new();
    let profile = home.path().join("profiles").join("work");
    fs::create_dir_all(profile.join("sessions/s1")).expect("profile state");
    fs::write(profile.join("config.toml"), "schema_version = 1\n").expect("config");
    fs::write(profile.join("profile.sqlite"), "a database").expect("database");
    fs::write(profile.join("sessions/s1/session.jsonl"), "{}").expect("transcript");
    let index = acpilot_store::Index::open(home.path().join("index.sqlite")).expect("index");
    index
        .remember_workspace("work", "/a/workspace")
        .expect("index row");

    let output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "rm", "work", "--purge"])
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile rm");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains(&profile.display().to_string()),
        "the removed path is printed: {}",
        stdout(&output)
    );
    assert!(!profile.exists(), "the profile directory is gone");
    assert!(
        index.profiles().expect("profiles").is_empty(),
        "the index row is gone with it"
    );
}

#[test]
fn profile_rm_of_a_missing_profile_is_not_found() {
    let home = TempDir::new();
    let output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "rm", "work", "--purge"])
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile rm");

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("config.toml"),
        "the refusal says what is missing: {}",
        stderr(&output)
    );
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
        // Pin the health probe to a port with no listener, so a daemon running
        // on the test machine cannot change the report.
        .env("ACPILOT_API_URL", "http://127.0.0.1:1")
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
    assert!(text.contains("daemon"), "{text}");
    assert!(
        text.contains("not reachable"),
        "no daemon is reported, not demanded"
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
        .env("ACPILOT_API_URL", "http://127.0.0.1:1")
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
        .env("ACPILOT_API_URL", "http://127.0.0.1:1")
        .output()
        .expect("run acpilot");

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("Mute"), "{text}");
    assert!(text.contains("handshake"), "{text}");
    assert!(text.contains("Problems"), "{text}");
}

#[test]
fn serve_picks_a_port_and_prints_both_facts_in_one_panel() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let config = TempDir::new();
    let runtime = TempDir::new();
    let workspace = TempDir::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .arg("serve")
        .arg("--workspace")
        .arg(workspace.path())
        .env("ACPILOT_HOME", config.path())
        // The token is written under the runtime directory; a real one would be
        // both a side effect and a secret this test should not touch.
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("PATH", workspace.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn acpilot serve");

    let stdout = child.stdout.take().expect("stdout");
    let mut panel = String::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.expect("stdout line");
        panel.push_str(&line);
        panel.push('\n');
        if line.contains("Ctrl-C") {
            break;
        }
    }
    // Stopped before any assertion, so a failed expectation cannot leave a server
    // holding the port for the rest of the suite.
    let _ = child.kill();
    let _ = child.wait();

    assert!(panel.contains("Local"), "{panel}");
    assert!(panel.contains("http://127.0.0.1:"), "{panel}");
    let token_line = panel
        .lines()
        .find(|line| line.contains("Token       "))
        .expect("a token line");
    let token = token_line.split_whitespace().last().expect("a token value");
    assert_eq!(token.len(), 64, "{panel}");
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()), "{panel}");

    let stored = fs::read_to_string(runtime.path().join("acpilot/token")).expect("token file");
    assert_eq!(
        stored.trim(),
        token,
        "the panel and the file are one secret"
    );
}

#[test]
fn serve_uses_a_token_it_was_given() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let config = TempDir::new();
    let runtime = TempDir::new();
    let workspace = TempDir::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .arg("serve")
        .arg("--workspace")
        .arg(workspace.path())
        .arg("--token")
        .arg("chosen-token")
        .env("ACPILOT_HOME", config.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("PATH", workspace.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn acpilot serve");

    let stdout = child.stdout.take().expect("stdout");
    let mut panel = String::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.expect("stdout line");
        panel.push_str(&line);
        panel.push('\n');
        if line.contains("Ctrl-C") {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    assert!(panel.contains("Token       chosen-token"), "{panel}");
    let stored = fs::read_to_string(runtime.path().join("acpilot/token")).expect("token file");
    assert_eq!(stored.trim(), "chosen-token");
}

#[test]
fn secret_set_with_empty_stdin_fails_before_it_reaches_the_keyring() {
    // `acpilot()` pipes an empty stdin. The command must refuse that rather than
    // store a zero-length credential, and it must do so before any keyring call.
    // The config root is a temp one so profile resolution's first-run default is
    // a test side effect rather than a write into the runner's home directory.
    let config = TempDir::new();
    let output = acpilot_with_env(
        &[
            "secret",
            "set",
            "anthropic",
            "--config",
            config.path().to_str().expect("utf-8 path"),
        ],
        "ACPILOT_HOME",
        config.path().to_str().expect("utf-8 path"),
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("empty"), "{}", stderr(&output));
}

#[test]
fn profile_export_redacts_by_default_and_gates_secrets() {
    let home = TempDir::new();
    let profile = home.path().join("profiles").join("work");
    fs::create_dir_all(&profile).expect("profile directory");
    fs::write(profile.join("config.toml"), "schema_version = 1\n").expect("config");
    fs::write(
        profile.join("agents.toml"),
        "schema_version = 1\n\n[[agents]]\nid = \"claude\"\ntransport = \"acp-stdio\"\n\
         command = \"npx\"\nenv = { ANTHROPIC_API_KEY = \"keyring:anthropic\" }\n",
    )
    .expect("manifest");
    let output_path = home.path().join("work.json");

    let output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "export", "work", "--output"])
        .arg(&output_path)
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile export");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let document = fs::read_to_string(&output_path).expect("export file");
    assert!(document.contains("keyring:anthropic"), "{document}");
    assert!(!document.contains("\"secrets\""), "{document}");

    let without_risk = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args([
            "profile",
            "export",
            "work",
            "--with-secrets",
            "--output",
            home.path()
                .join("secret.json")
                .to_str()
                .expect("utf-8 path"),
        ])
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile export");
    assert_eq!(
        without_risk.status.code(),
        Some(2),
        "{}",
        stderr(&without_risk)
    );
    assert!(
        stderr(&without_risk).contains("--i-understand-the-risk"),
        "{}",
        stderr(&without_risk)
    );

    let without_output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args([
            "profile",
            "export",
            "work",
            "--with-secrets",
            "--i-understand-the-risk",
        ])
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile export");
    assert_eq!(
        without_output.status.code(),
        Some(2),
        "{}",
        stderr(&without_output)
    );
    assert!(
        stderr(&without_output).contains("--output"),
        "{}",
        stderr(&without_output)
    );
}

#[test]
fn a_secret_uses_the_environment_profile_and_does_not_fall_back() {
    let config = TempDir::new();
    let output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args([
            "secret",
            "set",
            "anthropic",
            "--config",
            config.path().to_str().expect("utf-8 path"),
        ])
        .env("ACPILOT_HOME", config.path())
        .env("ACPILOT_PROFILE", "work")
        .output()
        .expect("run acpilot secret set");

    assert_eq!(
        output.status.code(),
        Some(3),
        "a missing environment profile is not found: {}",
        stderr(&output)
    );
    assert!(stderr(&output).contains("work"), "{}", stderr(&output));
    assert!(
        !config.path().join("profiles").join("default").exists(),
        "a missing named profile must not create or use default"
    );
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

#[test]
fn a_profile_is_created_listed_shown_used_and_diffed_without_a_daemon() {
    let home = TempDir::new();

    let created = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "new", "work", "--label", "Work"])
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile new");
    assert!(created.status.success(), "{}", stderr(&created));
    assert!(
        stdout(&created).contains("created work"),
        "{}",
        stdout(&created)
    );
    assert!(
        home.path().join("profiles/work/config.toml").is_file(),
        "the profile has the config.toml `Profile::open` requires"
    );

    let listed = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "ls"])
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile ls");
    assert!(listed.status.success(), "{}", stderr(&listed));
    let text = stdout(&listed);
    assert!(
        text.contains("* work"),
        "the first profile is active: {text}"
    );
    assert!(
        text.contains("Work"),
        "the label is drawn, not the id: {text}"
    );

    let shown = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "show", "work"])
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile show");
    assert!(shown.status.success(), "{}", stderr(&shown));
    let text = stdout(&shown);
    assert!(text.contains("profile    work"), "{text}");
    assert!(text.contains("present"), "the roots are reported: {text}");

    let second = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "new", "personal"])
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile new");
    assert!(second.status.success(), "{}", stderr(&second));

    let diff = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "diff", "work", "personal"])
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile diff");
    assert!(diff.status.success(), "{}", stderr(&diff));
    let text = stdout(&diff);
    assert!(text.contains("work -> personal"), "{text}");
    assert!(
        text.contains("no effective difference"),
        "two fresh profiles hold the defaults: {text}"
    );

    let used = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "use", "personal"])
        .env("ACPILOT_HOME", home.path())
        .output()
        .expect("run acpilot profile use");
    assert!(used.status.success(), "{}", stderr(&used));
    assert!(
        stdout(&used).contains("active profile is now personal"),
        "{}",
        stdout(&used)
    );
}

#[test]
fn a_profile_export_imports_and_a_second_import_is_a_conflict() {
    let source = TempDir::new();
    let created = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "new", "work"])
        .env("ACPILOT_HOME", source.path())
        .output()
        .expect("run acpilot profile new");
    assert!(created.status.success(), "{}", stderr(&created));

    let file = source.path().join("work.json");
    let exported = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args([
            "profile",
            "export",
            "work",
            "--output",
            file.to_str().expect("utf-8 path"),
        ])
        .env("ACPILOT_HOME", source.path())
        .output()
        .expect("run acpilot profile export");
    assert!(exported.status.success(), "{}", stderr(&exported));

    let target = TempDir::new();
    let imported = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "import", file.to_str().expect("utf-8 path")])
        .env("ACPILOT_HOME", target.path())
        .output()
        .expect("run acpilot profile import");
    assert!(imported.status.success(), "{}", stderr(&imported));
    assert!(
        target.path().join("profiles/work/config.toml").is_file(),
        "the imported profile is one `Profile::open` accepts"
    );

    let again = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(["profile", "import", file.to_str().expect("utf-8 path")])
        .env("ACPILOT_HOME", target.path())
        .output()
        .expect("run acpilot profile import twice");
    assert_eq!(
        again.status.code(),
        Some(4),
        "an existing profile is a conflict: {}",
        stderr(&again)
    );
}

#[test]
fn agents_rm_removes_a_promoted_entry_and_refuses_a_hand_written_one() {
    let config = TempDir::new();
    let workspace = TempDir::new();
    let profile = config.path().join("profiles/work");
    fs::create_dir_all(&profile).expect("profile directory");
    fs::write(profile.join("config.toml"), "schema_version = 1\n").expect("profile config");
    fs::write(
        profile.join("agents.toml"),
        "schema_version = 1\n\n[[agents]]\n\
         # acpilot:promoted recipe=\"recipes/demo.toml\" mtime=\"2026-11-12T12:00:00Z\"\n\
         id = \"demo\"\ntransport = \"acp-stdio\"\ncommand = \"acpilot-agent-lm\"\n\
         args = [\"--recipe\", \"demo\"]\n",
    )
    .expect("manifest");

    let output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args([
            "--profile",
            profile.to_str().expect("path"),
            "agents",
            "rm",
            "demo",
        ])
        .current_dir(workspace.path())
        .env("ACPILOT_HOME", config.path())
        .output()
        .expect("run acpilot");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !profile.join("agents.toml").exists(),
        "the last promoted entry takes the file with it"
    );

    // A hand-written entry is not promotion's to remove: the id is refused and
    // the file is left exactly as it was.
    let hand_written = "schema_version = 1\n\n[[agents]]\nid = \"mine\"\n\
                        transport = \"acp-stdio\"\ncommand = \"my-agent\"\n";
    fs::write(profile.join("agents.toml"), hand_written).expect("manifest");
    let output = Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args([
            "--profile",
            profile.to_str().expect("path"),
            "agents",
            "rm",
            "mine",
        ])
        .current_dir(workspace.path())
        .env("ACPILOT_HOME", config.path())
        .output()
        .expect("run acpilot");
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("written by hand"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(profile.join("agents.toml")).expect("manifest"),
        hand_written
    );
}
