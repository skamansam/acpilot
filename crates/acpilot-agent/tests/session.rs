//! A session against a mock agent, over an in-memory channel.
//!
//! The mock is an ACP agent built from the same SDK the client uses, so this
//! exercises the real handshake, the real framing, and the real notification
//! path without a model, a network, or a child process. The engine phase's task P2-9's fixture does the
//! same thing as a real process (`tests/process.rs`), which is how the pipes and
//! the child's lifetime are tested; this one stays because a channel can be
//! observed from both ends — the cancel test reads what the *agent* saw — and
//! costs no process per assertion.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use acpilot_agent::session::{Session, SessionError, SessionOptions};
use acpilot_proto::event::SessionEventKind;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Channel, on_receive_notification, on_receive_request};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// What the mock saw that the client cannot observe from its own side.
#[derive(Default)]
struct Seen {
    cancels: Mutex<Vec<String>>,
    cancel_seen: Notify,
}

fn mock_agent(transport: Channel, seen: Arc<Seen>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = Agent
            .builder()
            .name("mock-agent")
            .on_receive_request(
                async |request: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(request.protocol_version)
                            .agent_capabilities(AgentCapabilities::new())
                            .agent_info(Implementation::new("mock-agent", "0.0.0")),
                    )
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async |_request: NewSessionRequest, responder, _cx| {
                    responder.respond(NewSessionResponse::new("acp-1"))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async |request: PromptRequest, responder, connection| {
                    for text in ["one", "two"] {
                        connection.send_notification(SessionNotification::new(
                            request.session_id.clone(),
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new(text)),
                            )),
                        ))?;
                    }
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                },
                on_receive_request!(),
            )
            .on_receive_notification(
                async move |cancel: CancelNotification, _cx| {
                    seen.cancels
                        .lock()
                        .expect("seen lock")
                        .push(cancel.session_id.0.to_string());
                    seen.cancel_seen.notify_one();
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_to(transport)
            .await;
        assert!(result.is_ok(), "the mock agent failed: {result:?}");
    })
}

async fn connected() -> (Session, Arc<Seen>, JoinHandle<()>) {
    let (client, agent) = Channel::duplex();
    let seen = Arc::new(Seen::default());
    let task = mock_agent(agent, Arc::clone(&seen));
    let session = Session::over(client, SessionOptions::new("session-1"))
        .await
        .expect("the mock agent initializes");
    (session, seen, task)
}

#[tokio::test]
async fn initialize_new_and_prompt_stream_ordered_chunks() {
    let (mut session, _seen, agent) = connected().await;

    assert_eq!(
        session.agent_info().map(|info| info.name.as_str()),
        Some("mock-agent")
    );

    let mut events = session.events();
    let created = session.new_session(".").await.expect("session/new works");
    assert_eq!(&*created.session_id.0, "acp-1");
    assert_eq!(session.acp_session_id(), Some("acp-1"));

    let response = session.prompt("hello").await.expect("session/prompt works");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    let first = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("event arrives")
        .expect("channel is open");
    let second = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("event arrives")
        .expect("channel is open");

    assert_eq!((first.seq, second.seq), (1, 2));
    assert_eq!(first.kind, SessionEventKind::MessageChunk);
    assert_eq!(first.payload["role"], "assistant");
    assert_eq!(first.payload["content"]["text"], "one");
    assert_eq!(second.payload["content"]["text"], "two");
    assert_eq!(first.session_id, "session-1");

    agent.abort();
}

#[tokio::test]
async fn cancel_reaches_the_agent_as_a_notification() {
    let (mut session, seen, agent) = connected().await;
    session.new_session(".").await.expect("session/new works");

    session.cancel().expect("cancel is a notification");

    tokio::time::timeout(Duration::from_secs(5), seen.cancel_seen.notified())
        .await
        .expect("the agent saw the cancel");
    assert_eq!(
        seen.cancels.lock().expect("seen lock").as_slice(),
        ["acp-1"]
    );

    agent.abort();
}

#[tokio::test]
async fn a_prompt_before_a_session_is_refused_and_a_second_turn_waits() {
    let (mut session, _seen, agent) = connected().await;

    assert!(matches!(
        session.prompt("too early").await,
        Err(SessionError::NoSession)
    ));

    session.new_session(".").await.expect("session/new works");
    let (first, second) = tokio::join!(session.prompt("one"), session.prompt("two"));
    assert!(first.is_ok(), "the first turn runs: {first:?}");
    assert!(
        matches!(second, Err(SessionError::Busy)),
        "a second concurrent turn is refused here, not by the agent"
    );

    // The refusal releases nothing, but the finished turn must release the
    // guard: a later prompt is accepted.
    let later = session.prompt("three").await;
    assert!(later.is_ok(), "the busy flag was released: {later:?}");

    agent.abort();
}
