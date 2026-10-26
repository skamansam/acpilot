//! A pane over a terminal an agent opened, through the daemon's own store.
//!
//! The ACP side is the agent engine's: `terminal/create` starts a PTY and keeps a
//! ring an agent polls. This is the other half — the screen a client attaches to,
//! the keystrokes that reach the same process, and the end of the program closing
//! the pane rather than leaving a dead shell in the list.
//!
//! Unix only, and in `tests/` rather than the module's test block: these spawn
//! `/bin/sh`, and a `cfg` naming an operating system belongs in a platform file —
//! `acpilot-host/tests/platform_boundary.rs` enforces it.

#![cfg(unix)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use acpilot_agent::terminal::TerminalHandle;
use acpilotd::{AppState, Output, Terminal};
use agent_client_protocol::schema::v1::{CreateTerminalRequest, SessionId as AgentSessionId};

/// Ask the agent side for a terminal the way an agent would, and hand back the
/// view a pane gets: the open announcement, which is what the daemon mirrors.
async fn an_agents_terminal(state: &AppState, script: &str) -> TerminalHandle {
    let mut opened = state.agent_terminals().subscribe_opened();
    state
        .agent_terminals()
        .create(
            &CreateTerminalRequest::new(AgentSessionId::new("acp-1"), "/bin/sh")
                .args(vec!["-c".to_owned(), script.to_owned()]),
        )
        .expect("the agent's terminal starts");
    tokio::time::timeout(Duration::from_secs(5), opened.recv())
        .await
        .expect("the terminal is announced")
        .expect("the registry is open")
}

/// Wait for the pane's screen to show `text`, or fail saying it never did.
async fn screen_shows(pane: &Arc<Terminal>, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (replay, _) = pane.attach();
        if String::from_utf8_lossy(&replay).contains(text) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the pane never showed {text:?}: {}",
            String::from_utf8_lossy(&replay)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn an_agent_terminal_becomes_a_pane_a_client_can_attach_to() {
    let state = AppState::new();
    let handle = an_agents_terminal(&state, "printf hello").await;
    let summary = state.terminals().adopt("s1".to_owned(), handle);

    assert_eq!(summary.id, "agent-terminal-1");
    assert_eq!(summary.session_id, "s1");
    assert!(
        summary.command.contains("printf hello"),
        "the pane has a title: {}",
        summary.command
    );
    // The list a pane UI reads finds it under the session the API knows, not the
    // id the agent chose for the conversation.
    assert!(
        state
            .terminals()
            .list(Some("s1"))
            .iter()
            .any(|terminal| terminal.id == summary.id),
        "the pane is listed for its session"
    );

    let pane = state
        .terminals()
        .get(&summary.id)
        .expect("the pane is registered");
    screen_shows(&pane, "hello").await;
}

#[tokio::test]
async fn typing_into_the_pane_reaches_the_agents_process() {
    let state = AppState::new();
    // `cat` echoes what it reads, so a byte typed into the pane is visible in the
    // screen the pane replays — through the agent's PTY, not a second one.
    let handle = an_agents_terminal(&state, "exec cat").await;
    let summary = state.terminals().adopt("s1".to_owned(), handle);
    let pane = state
        .terminals()
        .get(&summary.id)
        .expect("the pane is registered");

    pane.write(b"typed\r")
        .expect("the pane writes to the agent's process");
    screen_shows(&pane, "typed").await;
}

#[tokio::test]
async fn a_pane_ends_when_the_agents_program_does() {
    let state = AppState::new();
    // The program waits for a line, so the pane is certainly alive when it is
    // adopted: a command that prints and exits is over before the daemon has a
    // pane for it, which is a different case (and its own test).
    let handle = an_agents_terminal(&state, "read line; printf bye").await;
    let summary = state.terminals().adopt("s1".to_owned(), handle);
    let pane = state
        .terminals()
        .get(&summary.id)
        .expect("the pane is registered");
    assert!(pane.summary().alive, "the program is waiting for a line");
    pane.write(b"go\r").expect("the pane reaches the program");

    let deadline = Instant::now() + Duration::from_secs(5);
    while pane.summary().alive {
        assert!(
            Instant::now() < deadline,
            "the pane outlived the program it was watching"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Writing to a pane whose program has ended is a broken pipe, not a panic and
    // not a silent success.
    assert!(pane.write(b"x").is_err(), "there is nothing to type into");
}

#[tokio::test]
async fn the_daemon_mirrors_a_terminal_an_agent_opens() {
    let state = AppState::new();
    state.mirror_agent_terminals();
    // Nothing else adopts here: this is the daemon noticing on its own, which is
    // what makes the shell an agent asked for show up in a client's list.
    an_agents_terminal(&state, "printf mirrored").await;

    let deadline = Instant::now() + Duration::from_secs(5);
    let pane = loop {
        if let Some(pane) = state.terminals().get("agent-terminal-1") {
            break pane;
        }
        assert!(
            Instant::now() < deadline,
            "the agent's terminal never became a pane"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // No session in this daemon answers to the agent's id, so the pane is filed
    // under that id rather than dropped — see `mirror_agent_terminals`.
    assert_eq!(pane.session_id(), "acp-1");
    screen_shows(&pane, "mirrored").await;
}

#[tokio::test]
async fn a_pane_opened_after_the_program_ended_still_hears_the_exit() {
    let state = AppState::new();
    let handle = an_agents_terminal(&state, "printf bye").await;
    // Wait for the program to end before the daemon makes a pane of it: an agent
    // asking for a terminal a moment after the command it ran is the ordinary
    // case, and its exit is already recorded by then.
    let mut exit = handle.exited();
    while (*exit.borrow()).is_none() {
        tokio::time::timeout(Duration::from_secs(5), exit.changed())
            .await
            .expect("the program ends")
            .expect("the terminal is open");
    }

    // Nothing is awaited between the adoption and this attach, so the pane's pump
    // has not run yet and every frame it publishes is still to come.
    let summary = state.terminals().adopt("s1".to_owned(), handle);
    let pane = state
        .terminals()
        .get(&summary.id)
        .expect("the pane is registered");
    let (replay, mut frames) = pane.attach();

    let mut seen = String::from_utf8_lossy(&replay).into_owned();
    let code = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match frames.recv().await {
                Ok(Output::Data(chunk)) => {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                }
                Ok(Output::Exit { code }) => return code,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("the pane closed without saying the program ended");
                }
            }
        }
    })
    .await
    .expect("the pane is told the program ended");
    assert!(
        seen.contains("bye"),
        "the screen keeps what the program printed: {seen:?}"
    );
    assert_eq!(code, Some(0), "the exit code reaches the pane");
}
