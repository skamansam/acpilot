#![cfg(unix)]

//! The service phase's acceptance test: a scripted client against a real daemon.
//!
//! unix only, like `tests/socket.rs`: the daemon's default transport is a unix
//! socket, and the Windows named pipe is P10-5. The store and the settings cascade
//! have their own tests that run everywhere; what needs a socket is what waits.
//!
//! `TODO.md` states the phase's definition of done as two sentences — "a scripted
//! `curl` session (create → prompt → stream → permission → resolve) works, and SSE
//! replay after disconnect loses no events" — and this file is those two sentences
//! as tests, plus the one thing the storage task added to them: that a conversation
//! is still there after the process that ran it is gone.
//!
//! Everything here goes through the same socket a client uses: the mock agent is a
//! real child process, the permission request is really pending in the broker, and
//! the events are read from the stream rather than from the ring.
//!
//! It is deliberately not a unit test of any one route. A route can be right on its
//! own and still be unusable in sequence: a prompt that is accepted before the
//! subscription exists loses its first event, and a `resolve` that does not wake the
//! agent's wait is a `200` that changes nothing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use acpilot_agent::{Broker, MapResolver, Policy, Registry};
use acpilot_client::Client;
use acpilot_proto::permission::PermissionOption;
use acpilot_proto::session::{CreateSessionRequest, SessionStatus};
use acpilot_store::Store;
use acpilotd::{AppState, Target, serve};
use futures::StreamExt;
use tokio::sync::oneshot;

/// How long a test waits for the agent before calling it a hang.
const PATIENCE: Duration = Duration::from_secs(20);

/// The mock agent, built on demand.
///
/// `acpilot-mock-agent` is a `[[bin]]` with `test = false`, so `nextest` does not
/// guarantee it is built before the tests that need it; building it here once is
/// what makes this suite work from a clean `cargo nextest run` as well as from
/// `cargo xtask check`, which builds the workspace first.
fn mock_agent() -> &'static Path {
    static AGENT: OnceLock<PathBuf> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            let exe = std::env::current_exe().expect("the test's own path");
            let debug = exe.parent().and_then(Path::parent).expect("target/debug");
            let path = debug.join("acpilot-mock-agent");
            if !path.exists() {
                let status = std::process::Command::new(env!("CARGO"))
                    .args([
                        "build",
                        "-p",
                        "acpilot-agent",
                        "--bin",
                        "acpilot-mock-agent",
                    ])
                    .current_dir(env!("CARGO_MANIFEST_DIR"))
                    .status()
                    .expect("cargo is on PATH");
                assert!(status.success(), "the mock agent must build");
            }
            path
        })
        .as_path()
}

/// A daemon on a temporary socket, its store, and the client that drives it.
///
/// No `Drop`: the tests that want a second daemon in the same directory need to
/// take the directory back out, and a type that owns a drop cannot be destructured.
/// The server task is aborted explicitly by whoever is done with it.
struct Harness {
    directory: tempfile::TempDir,
    client: Client,
    stop: Option<oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Harness {
    async fn start() -> Self {
        Self::in_directory(tempfile::tempdir().expect("temp dir")).await
    }

    async fn in_directory(directory: tempfile::TempDir) -> Self {
        let (client, stop, server) = daemon_in(directory.path()).await;
        Self {
            directory,
            client,
            stop: Some(stop),
            server,
        }
    }

    async fn session(&self) -> String {
        self.client
            .create_session(&CreateSessionRequest {
                workspace_id: String::new(),
                agent_id: "mock".into(),
                title: None,
            })
            .await
            .expect("a session")
            .id
    }
}

/// Start a daemon serving `directory`, with everything a session needs.
///
/// One function for both daemons in the restart test, so the second cannot be
/// configured differently from the first and accidentally pass or fail for a reason
/// the test is not about.
async fn daemon_in(
    directory: &Path,
) -> (
    Client,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let workspace = directory.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");

    // The mock agent as a manifest entry, so the session route can find it: the
    // daemon lists what a profile declares and refuses to invent a command.
    let agents = directory.join("agents.toml");
    // The command is escaped rather than pasted: a Windows path in a TOML string is
    // `C:\\…` twice over, and the parse error names the column rather than the cause.
    let command = mock_agent().display().to_string().replace('\\', "\\\\");
    std::fs::write(
        &agents,
        format!(
            "schema_version = 1\n\n[[agents]]\nid = \"mock\"\nname = \"Mock\"\n\
             description = \"The fixture agent.\"\ntransport = \"acp-stdio\"\n\
             command = \"{command}\"\n"
        ),
    )
    .expect("agents.toml");
    let registry = Registry::load(&agents, Arc::new(MapResolver::new())).expect("registry");

    let store =
        Arc::new(Store::open(directory.join("acpilot.db")).expect("a database to record into"));
    // What `startup` does at boot: a session the last process left open is closed,
    // because nothing is running it. A second daemon that skipped this would list a
    // conversation as `busy` with nobody behind it.
    store
        .close_open_sessions()
        .expect("the previous run is closed");
    let state = AppState::new()
        .with_registry(Arc::new(registry))
        .with_broker(Arc::new(Broker::with_timeout(
            Policy::default(),
            &workspace,
            None,
        )))
        .with_workspace(acpilotd::workspace_at(&workspace).expect("workspace"))
        .with_store(store)
        .with_transcripts(directory.join("transcripts"));

    let socket = directory.join("acpilotd.sock");
    let (stop, stopped) = oneshot::channel::<()>();
    let router = acpilotd::router_with(state);
    let target = Target::Socket(socket.clone());
    let server = tokio::spawn(async move {
        serve(router, target, async {
            let _ = stopped.await;
        })
        .await
    });

    let client = Client::new(socket.display().to_string());
    for _ in 0..200 {
        if client.health().await.is_ok() {
            return (client, stop, server);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the daemon never answered on {}", socket.display());
}

/// The next event's sequence, or a panic naming what was being waited for.
async fn next_event(
    stream: &mut futures::stream::BoxStream<
        'static,
        Result<acpilot_client::SessionStream, acpilot_client::ClientError>,
    >,
) -> u64 {
    let frame = tokio::time::timeout(PATIENCE, stream.next())
        .await
        .expect("an event inside the patience window")
        .expect("the stream is still open")
        .expect("a frame");
    match frame {
        acpilot_client::SessionStream::Event(event) => event.seq,
        other => panic!("expected an event, got {other:?}"),
    }
}

/// Wait until the session reports `status`, or fail saying what it reports instead.
async fn wait_for_status(client: &Client, id: &str, status: SessionStatus) {
    for _ in 0..400 {
        let summary = client.session(id).await.expect("the session");
        if summary.status == status {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let observed = client.session(id).await.expect("the session").status;
    panic!("session {id} is {observed:?}, not {status:?}");
}

#[tokio::test]
async fn a_scripted_session_runs_from_create_to_a_resolved_permission() {
    let harness = Harness::start().await;
    let session = harness.session().await;

    // Subscribed before the prompt: a client that prompts first and subscribes
    // second is the case the ring exists for, but it would make this test's first
    // assertion a race.
    let mut events = harness.client.session_events(&session, 0);
    // And the profile stream, because a permission request only becomes pending
    // when somebody is listening to answer it: a daemon with no client refuses
    // rather than parking a turn forever (that is `DecidedBy::Unattended`). The
    // first frame is the current preferences, so reading one proves the daemon
    // subscribed the broker before the agent asks anything.
    let mut announcements = harness.client.profile_events();
    tokio::time::timeout(PATIENCE, announcements.next())
        .await
        .expect("the profile stream opens")
        .expect("a first frame")
        .expect("the current preferences");

    harness
        .client
        .prompt(&session, "permission")
        .await
        .expect("the turn is accepted");

    // The agent asks for a shell command. It is pending until somebody answers,
    // which is the contract `GET /v1/permissions?status=pending` exists for.
    let mut request = None;
    for _ in 0..400 {
        let pending = harness
            .client
            .pending_permissions()
            .await
            .expect("the pending list");
        if let Some(first) = pending.into_iter().next() {
            request = Some(first);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let request = request.expect("the agent's permission request reached the daemon");
    assert_eq!(request.session_id, session);
    assert!(
        !request.options.is_empty(),
        "the request offers at least one option: {request:?}"
    );

    let decision = harness
        .client
        .resolve(&request.id, PermissionOption::AllowOnce)
        .await
        .expect("the decision is accepted");
    assert_eq!(decision.option, PermissionOption::AllowOnce);

    // The decision wakes the agent's wait and the turn finishes: a `resolve` that
    // returned but did not reach the agent would leave it busy forever, which is
    // the failure this asserts against.
    wait_for_status(&harness.client, &session, SessionStatus::Idle).await;

    // At least one event arrived on the stream for that turn: the transcript is
    // the channel the client reads, not the pending list.
    let first = next_event(&mut events).await;
    assert!(first >= 1);

    // A second turn proves the session is still usable after a permission.
    harness
        .client
        .prompt(&session, "stream 2")
        .await
        .expect("a second prompt is accepted");
    let mut chunks = 0;
    while chunks < 2 {
        next_event(&mut events).await;
        chunks += 1;
    }
}

#[tokio::test]
async fn a_stream_replays_after_a_drop_with_no_gap() {
    let harness = Harness::start().await;
    let session = harness.session().await;
    harness
        .client
        .prompt(&session, "stream 6")
        .await
        .expect("the turn is accepted");

    // Read the first two, then hang up. The rest has already been recorded, which
    // is the interesting case: the ring knows more than the client does.
    let mut first = harness.client.session_events(&session, 0);
    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.push(next_event(&mut first).await);
    }
    let last = *seen.last().expect("two events");
    drop(first);

    // Reconnect with `Last-Event-ID`, the header a browser's `EventSource` sends.
    let mut rest = harness.client.session_events(&session, last);
    while seen.len() < 6 {
        seen.push(next_event(&mut rest).await);
    }

    assert_eq!(
        seen,
        (1..=6).collect::<Vec<_>>(),
        "every sequence once, in order, across the reconnect"
    );
    assert!(
        harness.directory.path().join("acpilot.db").exists(),
        "the conversation was recorded as it streamed"
    );
}

#[tokio::test]
async fn a_conversation_is_still_there_after_the_daemon_that_ran_it() {
    let harness = Harness::start().await;
    let session = harness.session().await;
    harness
        .client
        .prompt(&session, "stream 3")
        .await
        .expect("the turn is accepted");
    let mut events = harness.client.session_events(&session, 0);
    let mut seen = Vec::new();
    while seen.len() < 3 {
        seen.push(next_event(&mut events).await);
    }
    drop(events);

    // Stop the first daemon but keep its directory: the database is the only thing
    // that carries the conversation across, which is the whole point of the task.
    let Harness {
        directory,
        client,
        stop,
        server,
    } = harness;
    drop(client);
    if let Some(stop) = stop {
        let _ = stop.send(());
    }
    server.abort();

    let (client, stop, server) = daemon_in(directory.path()).await;

    let sessions = client.sessions().await.expect("the session list");
    let stored = sessions
        .iter()
        .find(|summary| summary.id == session)
        .expect("the conversation a previous process ran");
    assert_eq!(
        stored.status,
        SessionStatus::Closed,
        "nothing is running it, so it is not busy"
    );

    // And its events are readable from the database: the same sequences, in order,
    // on the same route a live session uses. The stream ends because there is no
    // live feed to wait on, which is the difference between history and a session.
    let mut replayed = client.session_events(&session, 0);
    let mut again = Vec::new();
    while let Some(frame) = tokio::time::timeout(PATIENCE, replayed.next())
        .await
        .expect("the replay finishes rather than hanging")
    {
        if let acpilot_client::SessionStream::Event(event) = frame.expect("a frame") {
            again.push(event.seq);
        }
    }
    assert_eq!(again, seen, "the stored conversation, replayed");

    let _ = stop.send(());
    server.abort();
}
