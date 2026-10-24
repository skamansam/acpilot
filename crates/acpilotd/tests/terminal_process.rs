//! A terminal, against a real PTY and a real shell.
//!
//! The properties worth pinning are the ones that make a terminal something a
//! pane can leave and come back to: output that arrived before the pane did is
//! still there, input reaches the program, a resize reaches it too, and closing
//! the pane does not end the process.
//!
//! unix only: these spawn `/bin/sh`, which is where the platform files put the
//! difference. Windows gets the same code through ConPTY, and the compile there
//! is what CI checks today.

#![cfg(unix)]

use std::time::{Duration, Instant};

use acpilot_host::ExecCommand;
use acpilot_proto::terminal::TerminalSize;
use acpilotd::{AppState, Terminal};
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

const SIZE: TerminalSize = TerminalSize { cols: 80, rows: 24 };

fn spawn(program: &str, args: &[&str]) -> std::sync::Arc<Terminal> {
    Terminal::spawn(
        "t1".to_owned(),
        "s1".to_owned(),
        ExecCommand::new(program).args(args),
        SIZE,
    )
    .expect("terminal started")
}

/// The screen once it contains `needle`, or a panic that shows what it held.
///
/// Polling the replay rather than reading the stream is deliberate: that is
/// exactly what a pane attaching late does, so the test exercises the path the
/// feature exists for.
fn wait_for(terminal: &Terminal, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (replay, receiver) = terminal.attach();
        drop(receiver);
        let screen = String::from_utf8_lossy(&replay).into_owned();
        if screen.contains(needle) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "the screen never showed {needle:?}; it held {screen:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_until_gone(terminal: &Terminal) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while terminal.summary().alive {
        assert!(
            Instant::now() < deadline,
            "the terminal was still alive after ten seconds"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_pane_that_arrives_late_sees_what_the_terminal_already_drew() {
    let terminal = spawn("sh", &["-c", "printf 'late pane sees this'"]);
    let screen = wait_for(&terminal, "late pane sees this");
    assert!(screen.contains("late pane sees this"), "{screen:?}");

    wait_until_gone(&terminal);
    assert!(!terminal.summary().alive);
    // The screen outlives the process: that is what a pane attaching after a
    // command finished has to be able to show.
    let (replay, _receiver) = terminal.attach();
    assert!(
        String::from_utf8_lossy(&replay).contains("late pane sees this"),
        "{:?}",
        String::from_utf8_lossy(&replay)
    );
}

#[test]
fn typed_bytes_reach_the_program() {
    let terminal = spawn("sh", &["-c", "read line; printf 'got:%s' \"$line\""]);
    // No newline is added by the daemon; the `\r` here is the key a terminal
    // sends, and the program sees the difference.
    terminal.write(b"hello\r").expect("write");
    wait_for(&terminal, "got:hello");
}

#[test]
fn a_resize_reaches_the_program() {
    let terminal = Terminal::spawn(
        "t1".to_owned(),
        "s1".to_owned(),
        ExecCommand::new("sh").args(["-c", "sleep 0.4; stty size"]),
        TerminalSize { cols: 40, rows: 10 },
    )
    .expect("terminal started");
    terminal
        .resize(TerminalSize {
            cols: 100,
            rows: 30,
        })
        .expect("resized");
    // `stty size` prints "<rows> <cols>", so this asserts both halves of the
    // resize reached the process rather than only the bookkeeping.
    wait_for(&terminal, "30 100");
    assert_eq!(
        terminal.summary().size,
        TerminalSize {
            cols: 100,
            rows: 30
        }
    );
}

#[test]
fn closing_a_pane_does_not_end_the_process() {
    let terminal = spawn("sh", &["-c", "printf started; sleep 0.4; printf finished"]);
    {
        // A pane attaching and leaving again: the drop is what a closed socket
        // does to its receiver.
        let (_replay, receiver) = terminal.attach();
        drop(receiver);
    }
    wait_for(&terminal, "started");
    assert!(
        terminal.summary().alive,
        "the process ended when the pane left"
    );
    wait_for(&terminal, "finished");
    wait_until_gone(&terminal);
}

#[test]
fn a_zero_dimension_never_starts_a_process() {
    let error = Terminal::spawn(
        "t1".to_owned(),
        "s1".to_owned(),
        ExecCommand::new("sh"),
        TerminalSize { cols: 0, rows: 0 },
    )
    .expect_err("refused");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_list_reports_what_a_pane_can_attach_to() {
    let terminal = spawn("sh", &["-c", "sleep 5"]);
    let state = AppState::new();
    state.terminals().insert(terminal);

    let response = acpilotd::router_with(state)
        .oneshot(
            Request::builder()
                .uri("/v1/terminals")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body[0]["id"], "t1");
    assert_eq!(body[0]["session_id"], "s1");
    assert_eq!(body[0]["alive"], true);
    assert_eq!(body[0]["size"]["cols"], 80);
    // The command line, not the program: it is what a pane shows to say which
    // process it is looking at.
    assert_eq!(body[0]["command"], "sh -c sleep 5");
}
