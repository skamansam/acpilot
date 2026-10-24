//! The supervisor against real processes rather than a mock.
//!
//! A supervisor that only works against a fake child is not evidence about the
//! thing that actually goes wrong: stderr arriving in chunks, a wrapper process,
//! an exit code. So these tests start `/bin/sh`, which every CI runner and every
//! developer machine has, and assert on what it printed and how it exited.
//!
//! The file is unix-only because the shell it uses is. The Windows path in
//! `supervisor/platform/windows.rs` is type-checked by the CI target build; its
//! runtime behaviour needs a Windows runner, which is the engine phase's task P2-2's remaining gap and is
//! recorded in TODO.md rather than hidden behind a test that never runs.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use acpilot_agent::manifest;
use acpilot_agent::supervisor::availability;
use acpilot_agent::{
    AgentError, AgentProcess, AgentSpec, MAX_RESTARTS, MapResolver, Restart, Supervisor,
};
use acpilot_proto::agent::AgentStatus;

fn load(agent: &str) -> Arc<AgentSpec> {
    let resolver = MapResolver::new().with("anthropic", "sk-ant-test");
    let manifest = format!("schema_version = 1\n{agent}");
    manifest::parse(Path::new("agents.toml"), &manifest, &resolver)
        .expect("manifest should load")
        .remove(0)
        .into()
}

fn echo_agent(id: &str, command: &str, restart: &str) -> Arc<AgentSpec> {
    load(&format!(
        "[[agents]]\nid = \"{id}\"\ntransport = \"acp-stdio\"\ncommand = \"/bin/sh\"\n\
         args = ['-c', {command:?}]\n{restart}"
    ))
}

fn wait_for_stderr(process: &AgentProcess, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if process.stderr().text().contains(needle) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "stderr never contained {needle:?}; it was {:?}",
        process.stderr().text()
    );
}

#[test]
fn stderr_is_captured_while_the_agent_is_still_running() {
    let spec = echo_agent("chatty", r#"printf "ready\n" >&2; sleep 30"#, "");
    let mut process = AgentProcess::spawn(spec).expect("spawned");
    wait_for_stderr(&process, "ready");
    assert!(!process.stderr().truncated());
    process.kill().expect("killed");
    let status = process.wait().expect("waited");
    assert!(!status.success(), "a killed agent is not a clean exit");
}

#[test]
fn env_values_from_the_spec_reach_the_child() {
    let spec = load(
        "[[agents]]\nid = \"greeter\"\ntransport = \"acp-stdio\"\ncommand = \"/bin/sh\"\n\
         args = ['-c', 'printf %s \"$GREETING\" >&2']\n\
         env = { GREETING = \"hello\", ANTHROPIC_API_KEY = \"keyring:anthropic\" }\n",
    );
    let mut process = AgentProcess::spawn(spec).expect("spawned");
    let status = process.wait().expect("waited");
    assert!(status.success());
    assert_eq!(process.stderr().text(), "hello");
    assert!(
        !format!("{process:?}").contains("sk-ant-test"),
        "the debug view must not print the resolved secret"
    );
}

#[test]
fn a_missing_command_names_the_agent_and_the_command() {
    let spec = load(
        "[[agents]]\nid = \"ghost\"\ntransport = \"acp-stdio\"\n\
         command = \"acpilot-no-such-program-anywhere\"\n",
    );
    let error = AgentProcess::spawn(spec).expect_err("no such program");
    let AgentError::Spawn {
        id,
        command,
        source,
    } = &error
    else {
        panic!("expected a spawn error, got {error:?}");
    };
    assert_eq!(id, "ghost");
    assert_eq!(command, "acpilot-no-such-program-anywhere");
    assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
    assert!(error.to_string().contains("\"ghost\""), "{error}");
}

#[test]
fn transports_this_build_does_not_drive_are_refused_by_name() {
    let pty = load(
        "[[agents]]\nid = \"p\"\ntransport = \"pty\"\ncommand = \"/bin/sh\"\n\
         parser = \"ansi-transcript\"\nsentinel = \"ready\"\n",
    );
    let error = AgentProcess::spawn(pty).expect_err("pty is P2-7");
    assert!(matches!(
        error,
        AgentError::NotSupervisable {
            transport: "pty",
            ..
        }
    ));
    assert!(error.to_string().contains("\"pty\""), "{error}");

    let http = load(
        "[[agents]]\nid = \"h\"\ntransport = \"http-jsonrpc\"\n\
         url = \"https://example.test/acp\"\n",
    );
    let error = AgentProcess::spawn(http).expect_err("http has no local process");
    assert!(matches!(
        error,
        AgentError::NotSupervisable {
            transport: "http-jsonrpc",
            ..
        }
    ));
}

#[test]
fn never_is_the_default_and_does_not_restart() {
    let mut supervisor = Supervisor::start(echo_agent("crash", "exit 7", "")).expect("started");
    let outcome = supervisor.wait().expect("waited");
    assert_eq!(outcome.status.code(), Some(7));
    assert_eq!(outcome.restarts, 0);
    assert_eq!(supervisor.restarts(), 0);
}

#[test]
fn on_crash_restarts_up_to_the_limit_and_then_stops() {
    let spec = echo_agent(
        "crash",
        r#"printf "attempt\n" >&2; exit 3"#,
        "restart = \"on-crash\"\n",
    );
    let mut supervisor = Supervisor::start(spec).expect("started");
    let outcome = supervisor.wait().expect("waited");

    assert_eq!(outcome.status.code(), Some(3));
    assert_eq!(outcome.restarts, MAX_RESTARTS);
    assert_eq!(supervisor.restarts(), MAX_RESTARTS);
    // The stderr buffer is shared across attempts, so the first crash is still
    // readable after the last restart.
    assert_eq!(supervisor.stderr().text().matches("attempt").count(), 4);
}

#[test]
fn a_clean_exit_is_never_restarted() {
    let spec = echo_agent(
        "done",
        r#"printf "done\n" >&2; exit 0"#,
        "restart = \"on-crash\"\n",
    );
    let mut supervisor = Supervisor::start(spec).expect("started");
    let outcome = supervisor.wait().expect("waited");
    assert!(outcome.status.success());
    assert_eq!(outcome.restarts, 0);
    assert_eq!(supervisor.stderr().text(), "done\n");
}

#[test]
fn availability_is_a_path_lookup_and_nothing_else() {
    let shell = load("[[agents]]\nid = \"sh\"\ntransport = \"acp-stdio\"\ncommand = \"sh\"\n");
    assert_eq!(availability(&shell), AgentStatus::Available);
    assert_eq!(shell.restart, Restart::Never);

    let ghost = load(
        "[[agents]]\nid = \"ghost\"\ntransport = \"acp-stdio\"\n\
         command = \"acpilot-no-such-program-anywhere\"\n",
    );
    assert_eq!(availability(&ghost), AgentStatus::Missing);

    let remote = load(
        "[[agents]]\nid = \"remote\"\ntransport = \"http-jsonrpc\"\n\
         url = \"https://example.test/acp\"\n",
    );
    assert_eq!(availability(&remote), AgentStatus::Available);
}
