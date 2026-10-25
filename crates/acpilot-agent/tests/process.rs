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

use acpilot_agent::{
    AgentProcess, Broker, DecidedBy, Decision, Fs, MapResolver, Policy, Registry, Rule, Session,
    SessionOptions,
};
use acpilot_host::LocalHost;
use acpilot_proto::event::{SessionEvent, SessionEventKind};
use acpilot_proto::permission::PermissionOption;
use agent_client_protocol::schema::v1::StopReason;
use tokio::sync::broadcast::Receiver;
use tokio::time::{sleep, timeout};

/// The fixture agent, as Cargo built it for this test run.
const MOCK: &str = env!("CARGO_BIN_EXE_acpilot-mock-agent");

/// The longest a local pipe round trip may take before it is a finding rather
/// than a slow machine. The engine phase's P2-9 exit criterion: a turn against the fixture costs
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
    open_with(dir, None).await
}

/// The same, with a workspace the agent's `fs/*` requests can be served from.
async fn open_with(dir: &Path, fs: Option<Arc<Fs>>) -> Session {
    let registry =
        Registry::load(manifest(dir), Arc::new(MapResolver::new())).expect("the manifest loads");
    let spec = registry.get("mock").expect("the mock agent is declared");
    let process = AgentProcess::spawn(spec).expect("the fixture spawns");
    let mut options = SessionOptions::new("session-1");
    if let Some(fs) = fs {
        options = options.fs(fs);
    }
    Session::spawn(process, options)
        .await
        .expect("the fixture initializes")
}

/// A filesystem rooted at `dir`, on this machine.
fn workspace(dir: &Path) -> Arc<Fs> {
    Arc::new(Fs::new(Arc::new(LocalHost), dir).expect("the workspace resolves"))
}

/// How long a step in these tests may take before it is a finding.
const PATIENCE: Duration = Duration::from_secs(10);

/// The next event, or a failure that names the wait rather than hanging the run.
async fn next(events: &mut Receiver<SessionEvent>) -> SessionEvent {
    timeout(PATIENCE, events.recv())
        .await
        .expect("an event arrives")
        .expect("the stream is open")
}

/// The fixture reports each answer it heard as a notice; collect `count` of the
/// ones that start with `prefix`, in arrival order.
///
/// The prefix matters: the client records its own refusal notices, and a test
/// that took the first notice it saw would be asserting on its own log line
/// rather than on what the agent was told.
async fn reports(events: &mut Receiver<SessionEvent>, count: usize, prefix: &str) -> Vec<String> {
    let mut reports = Vec::new();
    while reports.len() < count {
        let event = next(events).await;
        if event.kind == SessionEventKind::Notice
            && let Some(message) = event.payload["message"].as_str()
            && message.starts_with(prefix)
        {
            reports.push(message.to_owned());
        }
    }
    reports
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
        (
            "fs",
            &[
                "fs/read_text_file",
                "fs/read_text_file",
                "fs/write_text_file",
            ][..],
        ),
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

/// The client serves `fs/*` from the session's workspace: a file inside it is
/// read, a file outside it is refused by name, and a write inside lands on disk.
#[tokio::test]
async fn files_inside_the_workspace_are_served_and_a_file_outside_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::write(dir.path().join("seed.txt"), "seeded\n").expect("write the seed");
    let mut session = open_with(dir.path(), Some(workspace(dir.path()))).await;
    let mut events = session.events();
    session
        .new_session(dir.path())
        .await
        .expect("session/new works");

    let response = session.prompt("fs").await.expect("prompt works");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    let report = reports(&mut events, 3, "fs/").await;
    assert!(
        report[0].starts_with("fs/read_text_file (inside) accepted"),
        "a file in the workspace is read: {report:?}"
    );
    assert!(
        report[1].contains("outside the workspace"),
        "a file outside it is refused for that reason: {report:?}"
    );
    assert!(
        report[2].starts_with("fs/write_text_file accepted"),
        "a write in the workspace is not queued for approval: {report:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("written-by-the-agent.txt")).expect("the agent's file"),
        "hello\n"
    );
}

/// A rule may be scoped to one agent, which only works if the subject says which
/// agent is asking. The session takes that from the spec its process was started
/// from, so a rule naming the manifest's `id` matches the agent it meant.
#[tokio::test]
async fn a_rule_naming_the_agent_decides_that_agents_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = Arc::new(Broker::new(
        Policy::with_rules(vec![Rule::new("allow-mock", Decision::Allow).agent("mock")])
            .with_tool(Decision::Deny),
        fs::canonicalize(dir.path()).expect("the workspace resolves"),
    ));
    let files = Arc::new(Fs::with_broker(Arc::new(LocalHost), Arc::clone(&broker)));
    let mut session = open_with(dir.path(), Some(Arc::clone(&files))).await;
    let mut events = session.events();
    session
        .new_session(dir.path())
        .await
        .expect("session/new works");

    session.prompt("permission").await.expect("the turn ends");

    let report = reports(&mut events, 1, "permission ").await;
    assert!(
        report[0].contains("selected allow"),
        "the rule allows it, so the agent's own allow option comes back: {report:?}"
    );
    let audit = broker.audit();
    assert_eq!(audit[0].agent.as_deref(), Some("mock"));
    assert_eq!(
        audit[0].decided_by,
        DecidedBy::Policy("allow-mock".to_owned())
    );
}

/// The shadow-write guard, with the two ends finally in the same test: the agent
/// is blocked mid-turn, the file is untouched, and the approval writes it.
///
/// This is the whole point of a pending request rather than a retry: a turn that
/// wanted three files must not land one of them and give up on the rest.
#[tokio::test]
async fn a_write_outside_the_workspace_holds_the_turn_until_it_is_approved() {
    let dir = tempfile::tempdir().expect("temp dir");
    let elsewhere = tempfile::tempdir().expect("temp dir");
    let files = workspace(dir.path());
    let mut session = open_with(dir.path(), Some(Arc::clone(&files))).await;
    session
        .new_session(dir.path())
        .await
        .expect("session/new works");
    let mut pending = files.broker().subscribe();

    let target = elsewhere.path().join("outside.txt");

    let approval = async {
        let request = timeout(PATIENCE, pending.recv())
            .await
            .expect("a request is announced")
            .expect("the stream is open");
        assert_eq!(request.session_id, "session-1");
        assert!(
            request.detail.contains("outside.txt"),
            "the person is told what would be written where: {}",
            request.detail
        );
        assert!(
            !target.exists(),
            "nothing is written while the request is pending"
        );
        assert!(
            files
                .broker()
                .answer(&request.id, PermissionOption::AllowOnce)
        );
    };
    let (response, ()) = tokio::join!(
        session.prompt(format!("write {}", target.display())),
        approval
    );
    response.expect("the turn ends after the approval");
    assert_eq!(
        fs::read_to_string(&target).expect("read the written file"),
        "hello\n"
    );
}

/// A refusal is an answer, not a failure: the write does not happen, the agent is
/// told so, and the turn still finishes.
#[tokio::test]
async fn a_denied_write_is_reported_and_the_turn_still_ends() {
    let dir = tempfile::tempdir().expect("temp dir");
    let elsewhere = tempfile::tempdir().expect("temp dir");
    let files = workspace(dir.path());
    let mut session = open_with(dir.path(), Some(Arc::clone(&files))).await;
    let mut events = session.events();
    session
        .new_session(dir.path())
        .await
        .expect("session/new works");
    let mut pending = files.broker().subscribe();

    let target = elsewhere.path().join("outside.txt");
    let denial = async {
        let request = timeout(PATIENCE, pending.recv())
            .await
            .expect("a request is announced")
            .expect("the stream is open");
        assert!(files.broker().answer(&request.id, PermissionOption::Deny));
    };
    let (response, ()) = tokio::join!(
        session.prompt(format!("write {}", target.display())),
        denial
    );
    assert_eq!(
        response
            .expect("a refused write does not end the session")
            .stop_reason,
        StopReason::EndTurn
    );

    let report = reports(&mut events, 1, "fs/").await;
    assert!(
        report[0].contains("was not approved"),
        "the agent is told why: {report:?}"
    );
    assert!(!target.exists(), "a denied write writes nothing");
}

/// Engine phase's task P2-2 and the engine phase's task P2-3 both recorded the same gap: their pipe wiring was type-checked
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
