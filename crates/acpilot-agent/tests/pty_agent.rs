//! The PTY adapter against a real program on a real PTY.
//!
//! The unit tests in `pty_agent.rs` parse captured screens; these drive a
//! process. The fixture is `tests/fixtures/tui_agent.sh`, a shell script that
//! does the one thing a TUI does and a shell does not do by accident: it takes
//! its input row away while it works and puts it back when it is done.
//!
//! Unix-only because the fixture is a `/bin/sh` script. The Windows half is the
//! ConPTY path in `acpilot-host`: it is the platform's own pseudo-console, and it
//! has no runner here, which is the same gap the supervisor records rather than
//! hides behind a test that never runs.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use acpilot_agent::pty_agent::{PtyAgent, SCREEN_SIZE, TurnEnd};
use acpilot_agent::{AgentSpec, MapResolver, MessageRole, PtyAgentError, manifest};
use acpilot_host::LocalHost;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tui_agent.sh")
}

/// A `pty` agent running the fixture in `mode`.
fn spec(mode: &str, extra: &str) -> Arc<AgentSpec> {
    let script = fixture();
    let text = format!(
        "schema_version = 1\n\n\
         [[agents]]\n\
         id = \"tui\"\n\
         transport = \"pty\"\n\
         command = \"sh\"\n\
         args = [{script:?}, {mode:?}]\n\
         parser = \"plain-transcript\"\n\
         sentinel = \"▍Enter a coding task\"\n\
         exit_command = \"/exit\"\n\
         submit_delay_ms = 50\n\
         {extra}\n"
    );
    manifest::parse(Path::new("agents.toml"), &text, &MapResolver::new())
        .expect("the fixture manifest is valid")
        .remove(0)
        .into()
}

#[test]
fn a_turn_ends_on_the_sentinel_and_the_screen_becomes_a_transcript() {
    let spec = spec("turn", "idle_timeout_ms = 5000");
    assert!(
        matches!(spec.transport, acpilot_agent::Transport::Pty { .. }),
        "the fixture is a PTY agent, which is the transport this test exists for"
    );

    let mut agent = PtyAgent::start(Arc::clone(&spec), &LocalHost, SCREEN_SIZE).expect("started");
    assert!(
        agent.wait_ready().expect("a ready screen"),
        "the fixture drew its input row and stopped"
    );
    assert!(agent.at_rest(), "the row it drew is the sentinel");

    let turn = agent.prompt("hello world").expect("a turn");

    assert_eq!(
        turn.end,
        TurnEnd::Sentinel,
        "the input row came back, so the turn is over in {:?}: {}",
        turn.elapsed,
        turn.screen
    );
    let roles: Vec<MessageRole> = turn.messages.iter().map(|message| message.role).collect();
    assert_eq!(
        roles,
        vec![MessageRole::User, MessageRole::Thought, MessageRole::Agent],
        "the echoed prompt, the thought and the answer: {:#?}",
        turn.messages
    );
    assert!(
        turn.messages[0].text.contains("hello world"),
        "the user's line survives the clear: {:#?}",
        turn.messages
    );
    assert_eq!(
        turn.messages[1].text.trim(),
        "Thinking about the request.",
        "the thought body: {:#?}",
        turn.messages
    );
    assert_eq!(turn.answer(), "OK: got it");
    assert!(
        turn.screen.contains("✻"),
        "the raw screen keeps the chrome, which is what a parser regression is a diff of: {}",
        turn.screen
    );
    assert!(
        turn.messages
            .iter()
            .all(|message| !message.text.contains('✻')),
        "the transcript drops it: {:#?}",
        turn.messages
    );
    assert!(
        turn.screen.contains("▍Enter a coding task"),
        "the raw screen is what the boundary saw: {}",
        turn.screen
    );

    let exit = agent.exit();
    assert!(exit.exited, "`/exit` ended the fixture");
    assert!(!exit.killed, "a clean exit is not a kill");
}

#[test]
fn a_program_that_never_redraws_its_row_ends_on_the_idle_backstop() {
    // The sentinel is the primary signal and this fixture withholds it, so the
    // only thing that can end the turn is the profile's idle timeout. A short one
    // is right here: this is a test of the backstop, not of a turn.
    let spec = spec("silent", "idle_timeout_ms = 300");
    let mut agent = PtyAgent::start(Arc::clone(&spec), &LocalHost, SCREEN_SIZE).expect("started");
    agent.wait_ready().expect("a ready screen");

    let turn = agent.prompt("hello world").expect("a turn");

    assert_eq!(
        turn.end,
        TurnEnd::Idle,
        "nothing came back to rest: {}",
        turn.screen
    );
    assert_eq!(turn.answer().trim(), "partial answer");
    // The turn ended without the process ending: the adapter is still usable.
    assert!(agent.pid().is_some());
}

#[test]
fn an_acp_agent_is_refused_by_name_instead_of_started_on_a_terminal() {
    let text = "schema_version = 1\n\n\
                [[agents]]\n\
                id = \"stdio\"\n\
                transport = \"acp-stdio\"\n\
                command = \"sh\"\n";
    let spec: Arc<AgentSpec> = manifest::parse(Path::new("agents.toml"), text, &MapResolver::new())
        .expect("valid")
        .remove(0)
        .into();

    let error = PtyAgent::start(spec, &LocalHost, SCREEN_SIZE).expect_err("refused");
    let message = error.to_string();
    assert!(
        matches!(error, PtyAgentError::NotPty { .. }),
        "the transport is the complaint: {message}"
    );
    assert!(
        message.contains("acp-stdio"),
        "it names the transport the manifest wrote: {message}"
    );
}

#[test]
fn a_script_parser_is_refused_rather_than_run() {
    let text = "schema_version = 1\n\n\
                [[agents]]\n\
                id = \"tui\"\n\
                transport = \"pty\"\n\
                command = \"sh\"\n\
                args = [\"-c\", \"true\"]\n\
                parser = \"script:/tmp/transcript.awk\"\n\
                sentinel = \"ready\"\n";
    let spec: Arc<AgentSpec> = manifest::parse(Path::new("agents.toml"), text, &MapResolver::new())
        .expect("the manifest only records the path")
        .remove(0)
        .into();

    let error = PtyAgent::start(spec, &LocalHost, SCREEN_SIZE).expect_err("refused");
    let message = error.to_string();
    assert!(
        matches!(error, PtyAgentError::Unsupported { .. }),
        "a user script needs a policy decision, not a spawn: {message}"
    );
    assert!(
        message.contains("transcript.awk"),
        "the message names the script a person wrote: {message}"
    );
    assert!(
        message.contains("plain-transcript"),
        "and what ships instead: {message}"
    );
}
