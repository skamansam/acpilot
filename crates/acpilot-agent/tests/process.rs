//! A session against a real child process.
//!
//! `tests/session.rs` drives the same client over an in-memory channel, which is
//! the right way to test the handshake and the translation. It cannot test the
//! part that only exists with a process: a supervisor's two pipes taken once,
//! stderr attached, and a child that dies when the session does. That is what
//! `tests/fixtures/mock_agent.rs` is for — a real ACP agent, speaking over real
//! stdin and stdout, telling the client what to do from the prompt text.
//!
//! The fixture is built as `acpilot-mock-agent` and named here with
//! `CARGO_BIN_EXE_`, so the path comes from Cargo rather than from a target
//! directory this test guessed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use acpilot_agent::{AgentProcess, MapResolver, Registry, Session, SessionOptions};
use acpilot_proto::event::{SessionEvent, SessionEventKind};
use agent_client_protocol::schema::v1::StopReason;
use tokio::sync::broadcast::Receiver;
use tokio::time::{sleep, timeout};

/// The fixture agent, as Cargo built it for this test run.
const MOCK: &str = env!("CARGO_BIN_EXE_acpilot-mock-agent");

/// The longest a local pipe round trip may take before it is a finding rather
/// than a slow machine. P2-9's exit criterion: a turn against the fixture costs
/// milliseconds, because nothing in it is a model.
const TURN_BUDGET: Duration = Duration::from_millis(200);

/// A one-agent manifest naming the fixture, so the spec comes from a file the
/// loader validated rather than from a struct the test assembled.
fn manifest(dir: &Path) -> PathBuf {
    let path = dir.join("agents.toml");
    let command = serde_json::to_string(MOCK).expect("a path quotes as a string");
    fs::write(
        &path,
        format!(
            "schema_version = 1\n\n\
             [[agents]]\n\
             id = \"mock\"\n\
             name = \"Mock agent\"\n\
             transport = \"acp-stdio\"\n\
             command = {command}\n"
        ),
    )
    .expect("write the manifest");
    path
}

/// Load the manifest, start the child, and open a session over its pipes.
async fn open(dir: &Path) -> Session {
    let registry =
        Registry::load(manifest(dir), Arc::new(MapResolver::new())).expect("the manifest loads");
    let spec = registry.get("mock").expect("the mock agent is declared");
    let process = AgentProcess::spawn(spec).expect("the fixture spawns");
    Session::spawn(process, SessionOptions::new("session-1"))
        .await
        .expect("the fixture initializes")
}

/// The next event, or a failure that names the wait rather than hanging the run.
async fn next(events: &mut Receiver<SessionEvent>) -> SessionEvent {
    timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("an event arrives")
        .expect("the stream is open")
}

#[tokio::test]
async fn a_session_over_a_real_process_streams_chunks_in_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut session = open(dir.path()).await;
    let mut events = session.events();

    let created = session
        .new_session(dir.path())
        .await
        .expect("session/new works");
    assert_eq!(&*created.session_id.0, "acp-1");

    let started = Instant::now();
    let response = session.prompt("stream 3").await.expect("prompt works");
    let mut texts = Vec::new();
    let mut seqs = Vec::new();
    while texts.len() < 3 {
        let event = next(&mut events).await;
        if event.kind == SessionEventKind::MessageChunk {
            seqs.push(event.seq);
            texts.push(
                event.payload["content"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
        }
    }
    let elapsed = started.elapsed();

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(texts, ["chunk 1", "chunk 2", "chunk 3"]);
    assert_eq!(
        seqs,
        [1, 2, 3],
        "numbers are assigned once, in arrival order"
    );
    assert!(
        elapsed < TURN_BUDGET,
        "three chunks over a local pipe took {elapsed:?}, over the {TURN_BUDGET:?} budget"
    );
}

#[tokio::test]
async fn a_permission_request_is_refused_with_the_agents_own_reject_option() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut session = open(dir.path()).await;
    let mut events = session.events();
    session
        .new_session(dir.path())
        .await
        .expect("session/new works");

    let response = session.prompt("permission").await.expect("prompt works");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    // Two notices: the refusal this client recorded, and the fixture reporting
    // which option it actually received.
    let refusal = next(&mut events).await;
    let reported = next(&mut events).await;
    assert_eq!(refusal.kind, SessionEventKind::Notice);
    assert_eq!(refusal.payload["title"], "Permission request denied");
    assert_eq!(reported.kind, SessionEventKind::Notice);

    let report = reported.payload["message"]
        .as_str()
        .expect("the fixture says what it heard");
    assert!(
        report.starts_with("permission selected reject"),
        "the client answered with the agent's reject_once option, got: {report}"
    );
}

#[tokio::test]
async fn file_and_terminal_requests_are_refused_because_nothing_advertises_them() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut session = open(dir.path()).await;
    let mut events = session.events();
    session
        .new_session(dir.path())
        .await
        .expect("session/new works");

    for (prompt, expected) in [
        ("fs", &["fs/read_text_file", "fs/write_text_file"][..]),
        ("terminal", &["terminal/create"][..]),
    ] {
        session.prompt(prompt).await.expect("the turn ends");

        let mut reports = Vec::new();
        while reports.len() < expected.len() {
            let event = next(&mut events).await;
            if event.kind == SessionEventKind::Notice
                && let Some(message) = event.payload["message"].as_str()
            {
                reports.push(message.to_owned());
            }
        }

        for (report, method) in reports.iter().zip(expected) {
            assert!(
                report.starts_with(method) && report.contains("refused"),
                "the client refused `{method}` rather than claiming it: {report}"
            );
        }
    }
}

#[tokio::test]
async fn a_hung_turn_stays_pending_and_does_not_wedge_the_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut session = open(dir.path()).await;
    let mut events = session.events();
    session
        .new_session(dir.path())
        .await
        .expect("session/new works");

    let hung = timeout(Duration::from_millis(500), session.prompt("hang")).await;
    assert!(hung.is_err(), "the fixture never answers this prompt");

    // The dropped future released the turn, so the session is usable again —
    // an agent that stops talking must not lock its own client.
    let response = session
        .prompt("stream 1")
        .await
        .expect("the next turn runs");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    let event = next(&mut events).await;
    assert_eq!(event.payload["content"]["text"], "chunk 1");
}

/// P2-2 and P2-3 both recorded the same gap: their pipe wiring was type-checked
/// and never run. This is the run.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_the_session_kills_the_agent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut session = open(dir.path()).await;
    session
        .new_session(dir.path())
        .await
        .expect("session/new works");

    let pid = session.pid().expect("a spawned session owns its process");
    assert!(
        running(pid),
        "the fixture is running before the session drops"
    );

    drop(session);

    let deadline = Instant::now() + Duration::from_secs(5);
    while running(pid) && Instant::now() < deadline {
        sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !running(pid),
        "the agent outlived the session that started it"
    );
}

/// Whether `pid` is a live process. A zombie counts as dead: it has stopped
/// running and is waiting only to be reaped, which is not the same as an agent
/// still holding a workspace.
#[cfg(target_os = "linux")]
fn running(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `pid (comm) state ...`; `comm` may contain spaces and brackets, so the
    // state is what follows the last ')'.
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    !matches!(rest.trim_start().chars().next(), Some('Z') | None)
}
