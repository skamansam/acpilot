//! Watching a terminal an agent opened, against a real PTY and a real shell.
//!
//! What is tested here is the shape a client pane needs: the terminal is
//! announced when it opens, the screen's bytes arrive as they are produced, a
//! keystroke reaches the program, and the exit is something a watcher is told
//! rather than something it polls.
//!
//! Unix only, and in `tests/` rather than the module's own test block: these
//! spawn `/bin/sh`, and the workspace's rule is that a branch naming an operating
//! system lives in a platform file — `acpilot-host/tests/platform_boundary.rs`
//! enforces it. Windows gets the same code through ConPTY; the compile there is
//! what CI checks today.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use acpilot_agent::Terminals;
use acpilot_agent::terminal::TerminalHandle;
use acpilot_host::LocalHost;
use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, KillTerminalRequest, SessionId as AgentSessionId,
};
use tokio::time::timeout;

/// A real program on a real PTY, and a watcher that saw it start.
async fn watched(script: &str) -> (Terminals, TerminalHandle) {
    let terminals = Terminals::new(Arc::new(LocalHost));
    let mut opened = terminals.subscribe_opened();
    let request = CreateTerminalRequest::new(AgentSessionId::new("s1"), "/bin/sh")
        .args(vec!["-c".to_owned(), script.to_owned()]);
    terminals.create(&request).expect("the terminal starts");
    let handle = timeout(Duration::from_secs(5), opened.recv())
        .await
        .expect("the terminal is announced")
        .expect("the registry is open");
    (terminals, handle)
}

/// Everything the watched program prints, from `text` on.
async fn printed(handle: &TerminalHandle, text: &str) -> String {
    let (backlog, mut live) = handle.watch();
    let mut seen = String::from_utf8_lossy(&backlog).into_owned();
    while !seen.contains(text) {
        let chunk = timeout(Duration::from_secs(5), live.recv())
            .await
            .expect("bytes arrive")
            .expect("the stream is open");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    seen
}

#[tokio::test]
async fn a_watcher_sees_a_terminal_open_and_what_it_printed() {
    let (_terminals, handle) = watched("printf hello").await;
    assert_eq!(handle.session_id(), "s1");
    assert_eq!(&*handle.id().0, "terminal-1");
    assert!(
        handle.command().contains("printf hello"),
        "the pane has a title: {}",
        handle.command()
    );

    printed(&handle, "hello").await;

    let mut exit = handle.exited();
    if (*exit.borrow()).is_none() {
        timeout(Duration::from_secs(5), exit.changed())
            .await
            .expect("the exit arrives")
            .expect("the stream is open");
    }
    assert!((*exit.borrow()).is_some(), "the program ended");
    assert!(!handle.alive(), "a terminal that ended is not alive");
}

#[tokio::test]
async fn a_handle_can_type_into_the_program() {
    let (_terminals, handle) = watched("read line; printf 'got %s' \"$line\"").await;
    handle
        .write(b"hi\n")
        .expect("the keystroke reaches the PTY");

    printed(&handle, "got hi").await;
}

#[tokio::test]
async fn a_watched_terminal_is_still_the_agents_terminal() {
    let (terminals, handle) = watched("sleep 30").await;
    // The agent's own view is untouched by a watcher: same id, same session, and
    // `terminal/kill` through the ACP path ends the program the pane is watching.
    assert_eq!(
        terminals.live(),
        1,
        "one terminal, no second process for the pane"
    );
    terminals
        .kill(&KillTerminalRequest::new(
            AgentSessionId::new("s1"),
            handle.id().clone(),
        ))
        .expect("the terminal is killed");
    let mut exit = handle.exited();
    if (*exit.borrow()).is_none() {
        timeout(Duration::from_secs(5), exit.changed())
            .await
            .expect("the exit arrives")
            .expect("the stream is open");
    }
    assert!((*exit.borrow()).is_some(), "the program ended");
}
