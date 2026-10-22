//! The registry as the daemon uses it: a directory it owns, a manifest that
//! changes underneath it, and a snapshot that is never half-updated.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use acpilot_agent::{AgentError, MapResolver, Parser, Registry, SecretResolver, Transport};
use acpilot_proto::agent::AgentTransport;

const FIRST: &str = r#"
schema_version = 1

[[agents]]
id = "claude"
name = "Claude Code"
transport = "acp-stdio"
command = "npx"
args = ["-y", "@zed-industries/claude-code-acp"]
env = { ANTHROPIC_API_KEY = "keyring:anthropic" }
"#;

const WITH_SECOND: &str = r#"
schema_version = 1

[[agents]]
id = "claude"
name = "Claude Code"
transport = "acp-stdio"
command = "npx"
args = ["-y", "@zed-industries/claude-code-acp"]
env = { ANTHROPIC_API_KEY = "keyring:anthropic" }

[[agents]]
id = "freebuff"
transport = "pty"
command = "freebuff"
parser = "ansi-transcript"
sentinel = "Enter a coding task"
idle_timeout_ms = 4000
"#;

fn resolver() -> Arc<dyn SecretResolver> {
    Arc::new(MapResolver::new().with("anthropic", "sk-ant-test"))
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("agents.toml")
}

fn write(dir: &Path, text: &str) {
    fs::write(manifest_path(dir), text).expect("write manifest");
}

fn load(dir: &Path) -> Registry {
    Registry::load(manifest_path(dir), resolver()).expect("registry should load")
}

/// Poll until `check` holds, or fail the test after five seconds.
///
/// Watching is the one thing here that depends on the operating system telling
/// us something, so a test cannot wait on a duration it guessed.
fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn an_acp_stdio_agent_loads_with_its_secret_resolved() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), FIRST);
    let registry = load(dir.path());

    let agents = registry.agents();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].id, "claude");
    assert_eq!(agents[0].transport_kind(), AgentTransport::AcpStdio);
    assert_eq!(agents[0].env["ANTHROPIC_API_KEY"].expose(), "sk-ant-test");
    assert_eq!(registry.revision(), 1);
    assert_eq!(registry.last_error(), None);
    assert_eq!(registry.path(), manifest_path(dir.path()));
}

#[test]
fn all_three_transports_reach_the_spec() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        r#"
schema_version = 1

[[agents]]
id = "claude"
transport = "acp-stdio"
command = "npx"

[[agents]]
id = "freebuff"
transport = "pty"
command = "freebuff"
parser = "plain-transcript"
sentinel = "ready"

[[agents]]
id = "remote"
transport = "http-jsonrpc"
url = "https://devin.internal/acp"
auth = "keyring:anthropic"
"#,
    );
    let registry = load(dir.path());

    assert_eq!(
        registry.get("claude").expect("claude").transport_kind(),
        AgentTransport::AcpStdio
    );
    let pty = registry.get("freebuff").expect("freebuff");
    assert_eq!(pty.transport_kind(), AgentTransport::Pty);
    let Transport::Pty { profile, command } = &pty.transport else {
        panic!("expected a pty agent");
    };
    assert_eq!(profile.parser, Parser::PlainTranscript);
    assert_eq!(
        profile.idle_timeout,
        Duration::from_millis(5_000),
        "default"
    );
    assert_eq!(command.program, "freebuff");

    let remote = registry.get("remote").expect("remote");
    assert_eq!(remote.transport_kind(), AgentTransport::HttpJsonRpc);
    assert_eq!(remote.command(), None);
    assert_eq!(registry.get("nobody"), None);
}

#[test]
fn a_manifest_that_does_not_validate_does_not_load_at_all() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "schema_version = 1\n\n[[agents]]\nid = \"x\"\ntransport = \"pty\"\n",
    );
    let error = Registry::load(manifest_path(dir.path()), resolver()).expect_err("should fail");
    let message = error.to_string();
    assert!(message.contains("requires \"command\""), "{message}");
}

#[test]
fn a_missing_file_is_reported_with_its_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let error = Registry::load(manifest_path(dir.path()), resolver()).expect_err("should fail");
    let message = error.to_string();
    assert!(message.contains("agents.toml"), "{message}");
    assert!(matches!(error, AgentError::Io { .. }), "{error:?}");
}

#[test]
fn reload_reports_whether_anything_changed() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), FIRST);
    let registry = load(dir.path());

    assert!(!registry.reload().expect("identical reload"), "same bytes");
    assert_eq!(registry.revision(), 1);

    write(dir.path(), WITH_SECOND);
    assert!(registry.reload().expect("changed reload"));
    assert_eq!(registry.revision(), 2);
    assert_eq!(registry.agents().len(), 2);

    // Whitespace-only edits are a save, not a change: the snapshot is rebuilt
    // and compares equal, so nothing downstream is asked to refresh.
    write(dir.path(), &format!("{WITH_SECOND}\n\n"));
    assert!(!registry.reload().expect("whitespace reload"));
    assert_eq!(registry.revision(), 2);
}

#[test]
fn a_broken_edit_keeps_the_last_good_snapshot_and_the_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), FIRST);
    let registry = load(dir.path());

    write(
        dir.path(),
        "schema_version = 1\n\n[[agents]]\nid = \"claude\"\ntransport = \"nope\"\n",
    );
    let error = registry
        .reload()
        .expect_err("an unknown transport is not a reload");
    assert!(error.to_string().contains("acp-stdio"), "{error}");

    // Still serving the file that worked.
    assert_eq!(registry.get("claude").expect("claude").name, "Claude Code");
    assert_eq!(registry.revision(), 1);
    let reason = registry.last_error().expect("a reason is kept");
    assert!(reason.contains("agents.toml"), "{reason}");

    // A later good save clears it, and is a real change.
    write(dir.path(), WITH_SECOND);
    assert!(registry.reload().expect("fixed reload"));
    assert_eq!(registry.last_error(), None);
    assert_eq!(registry.revision(), 2);
}

#[test]
fn a_file_deleted_out_from_under_a_running_registry_does_not_lose_the_agents() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), FIRST);
    let registry = load(dir.path());

    fs::remove_file(manifest_path(dir.path())).expect("remove manifest");
    let error = registry.reload().expect_err("a missing file is a failure");
    assert!(matches!(error, AgentError::Io { .. }), "{error:?}");
    assert!(registry.get("claude").is_some());
    assert!(registry.last_error().is_some());
}

#[test]
fn watching_reloads_a_plain_save() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), FIRST);
    let registry = Arc::new(load(dir.path()));
    let _watcher = registry.watch().expect("watch");

    write(dir.path(), WITH_SECOND);

    wait_until("the second agent to appear", || {
        registry.get("freebuff").is_some()
    });
    assert_eq!(registry.revision(), 2);
}

#[test]
fn watching_survives_an_atomic_save() {
    // What most editors do: write a sibling file, then rename it over the
    // target. The inode changes, which is why the watcher holds the directory.
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), FIRST);
    let registry = Arc::new(load(dir.path()));
    let _watcher = registry.watch().expect("watch");

    let temporary = dir.path().join(".agents.toml.swp");
    fs::write(&temporary, WITH_SECOND).expect("write temporary");
    fs::rename(&temporary, manifest_path(dir.path())).expect("rename over the manifest");

    wait_until("the renamed manifest to be picked up", || {
        registry.get("freebuff").is_some()
    });
    assert_eq!(registry.revision(), 2);
}

#[test]
fn watching_ignores_other_files_in_the_profile_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), FIRST);
    fs::write(dir.path().join("lsp.toml"), "").expect("write lsp.toml");
    let registry = Arc::new(load(dir.path()));
    let _watcher = registry.watch().expect("watch");

    fs::write(dir.path().join("lsp.toml"), "schema_version = 1\n").expect("rewrite lsp.toml");
    // There is no event to wait for, so wait for the opposite: a revision that
    // does not move. The sleep is the assertion's budget, not a guess at timing
    // — half a second is many orders of magnitude past one directory event.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(registry.revision(), 1);
}

#[test]
fn watching_a_broken_save_still_picks_up_the_next_good_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), FIRST);
    let registry = Arc::new(load(dir.path()));
    let _watcher = registry.watch().expect("watch");

    write(dir.path(), "schema_version = 99\n");
    wait_until("the error to be recorded", || {
        registry.last_error().is_some()
    });
    assert!(registry.get("claude").is_some());

    write(dir.path(), WITH_SECOND);
    wait_until("the next good save to load", || {
        registry.get("freebuff").is_some()
    });
    assert_eq!(registry.last_error(), None);
}

#[test]
fn dropping_the_watcher_stops_the_watching() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), FIRST);
    let registry = Arc::new(load(dir.path()));
    let watcher = registry.watch().expect("watch");
    drop(watcher);

    write(dir.path(), WITH_SECOND);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(registry.revision(), 1, "no watcher, no reload");
    // The registry is still usable without one.
    assert!(registry.reload().expect("manual reload"));
}
