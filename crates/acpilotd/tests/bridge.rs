//! The bridge's local transport: a real ACP client drives a session here.
//!
//! The daemon is the binary, spawned with `--acp`, exactly as a driver would. The
//! assertions go over the same stdio a cloud agent speaks, not through the API, so
//! this covers the framing, the handshake, `session/new`, a streamed turn and the
//! stop reason in one pass.

mod common;

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ClientCapabilities, ContentBlock, Implementation, InitializeRequest, NewSessionRequest,
    PromptRequest, SessionNotification, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, Client, ConnectionTo, on_receive_notification};

/// Long enough for a debug build of the daemon to start and one agent to answer.
const PATIENCE: Duration = Duration::from_secs(60);

#[tokio::test]
async fn a_local_driver_prompts_a_session_and_reads_the_stream() {
    let directory = tempfile::tempdir().expect("temp dir");
    let profile = directory.path().join("profile");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&profile).expect("profile");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(profile.join("config.toml"), "schema_version = 1\n").expect("config");

    // The driver and the daemon compare a canonical path, and this is also the
    // path a driver on macOS gets from the system: `/var` is `/private/var`.
    let workspace = std::fs::canonicalize(&workspace).expect("the workspace resolves");

    // A TOML *literal* string, so a Windows path is taken byte for byte. A basic
    // string needs every backslash doubled, and one that reaches `Command` with a
    // separator missing is `program not found` rather than a parse error.
    let fixture = common::mock_agent();
    assert!(fixture.exists(), "the fixture agent is built: {fixture:?}");
    let command = fixture.display();
    std::fs::write(
        profile.join("agents.toml"),
        format!(
            "schema_version = 1\n\n[[agents]]\nid = \"mock\"\nname = \"Mock\"\n\
             transport = \"acp-stdio\"\ncommand = '{command}'\n"
        ),
    )
    .expect("agents.toml");

    let line = format!(
        "{} --acp --profile {} --workspace {} --database {} --foreground",
        env!("CARGO_BIN_EXE_acpilotd"),
        profile.display(),
        workspace.display(),
        directory.path().join("profile.sqlite").display(),
    );
    let agent = AcpAgent::from_str(&line).expect("a bridge command");

    let chunks: Arc<Mutex<Vec<String>>> = Arc::default();
    let listening = Arc::clone(&chunks);
    let cwd = workspace.clone();

    let driven = tokio::time::timeout(
        PATIENCE,
        Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    // Read through JSON: the point is what a driver receives on the
                    // wire, not which Rust field the schema names today.
                    let value = serde_json::to_value(&notification).unwrap_or_default();
                    if value
                        .pointer("/update/sessionUpdate")
                        .and_then(serde_json::Value::as_str)
                        == Some("agent_message_chunk")
                        && let Some(text) = value
                            .pointer("/update/content/text")
                            .and_then(serde_json::Value::as_str)
                    {
                        listening
                            .lock()
                            .expect("the chunk list")
                            .push(text.to_owned());
                    }
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(agent, move |connection: ConnectionTo<Agent>| {
                let workspace = cwd.clone();
                async move {
                    let initialize = connection
                        .send_request(
                            InitializeRequest::new(ProtocolVersion::V1)
                                .client_capabilities(ClientCapabilities::new())
                                .client_info(Implementation::new("bridge-test", "0.0.0")),
                        )
                        .block_task()
                        .await?;
                    let initialize = serde_json::to_value(&initialize).unwrap_or_default();
                    assert_eq!(
                        initialize
                            .pointer("/agentInfo/name")
                            .and_then(serde_json::Value::as_str),
                        Some("acpilotd"),
                        "the driver is talking to the bridge: {initialize}"
                    );

                    let session = connection
                        .send_request(NewSessionRequest::new(workspace))
                        .block_task()
                        .await?;
                    let session = serde_json::to_value(&session).unwrap_or_default();
                    let session_id = session
                        .get("sessionId")
                        .and_then(serde_json::Value::as_str)
                        .expect("a session id")
                        .to_owned();

                    let response = connection
                        .send_request(PromptRequest::new(
                            agent_client_protocol::schema::v1::SessionId::new(session_id),
                            vec![ContentBlock::Text(TextContent::new("stream 2"))],
                        ))
                        .block_task()
                        .await?;
                    let response = serde_json::to_value(&response).unwrap_or_default();
                    assert_eq!(
                        response
                            .get("stopReason")
                            .and_then(serde_json::Value::as_str),
                        Some("end_turn")
                    );
                    Ok(())
                }
            }),
    )
    .await
    .expect("the bridge answers");
    driven.expect("the driver's calls succeed");

    // The response is sent after the bridge has seen the record go quiet, but the
    // client's own notification handler may still be a few milliseconds behind it.
    // Wait at the protocol's pace rather than the scheduler's before asserting.
    for _ in 0..40 {
        if chunks.lock().expect("the chunk list").len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        chunks.lock().expect("the chunk list").join("|"),
        "chunk 1|chunk 2",
        "the driver reads the agent's words as they arrive"
    );
}
