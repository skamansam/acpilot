//! The bridge's remote half: pairing, tokens, scopes and revocation over HTTP.
//!
//! The bridge is its own listener, so these tests drive its router the way the
//! daemon serves it — `/pair` open, `/acp` behind a driver token. The cockpit's
//! routes are on the daemon's router, which is what the revocation test uses: the
//! person who revokes a driver is holding the cockpit, not the driver's token.
//!
//! What is not stubbed is the ledger: codes, hashes, scopes and revocation are the
//! real [`Drivers`], because a test that paired against a stub would prove the stub.

mod common;

use std::sync::Arc;
use std::time::Duration;

use acpilot_agent::{Broker, PermissionKind, Policy, Subject};
use acpilotd::config::BridgeSettings;
use acpilotd::drivers::Drivers;
use acpilotd::{AppState, Workspace, acp_http, router_with};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A daemon state with the bridge enabled and a memory ledger.
fn state() -> AppState {
    AppState::new()
        .with_drivers(Arc::new(Drivers::memory()))
        .with_bridge(BridgeSettings {
            enabled: true,
            bind: Some("127.0.0.1:0".to_owned()),
            public_url: None,
        })
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(
                    body.map_or_else(String::new, |value| value.to_string()),
                ))
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Claim `code` and answer with the token the driver would keep.
async fn claim(app: &Router, code: &str, scopes: &[&str]) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        "/pair",
        None,
        Some(json!({
            "code": code,
            "name": "devin",
            "requested_scopes": scopes,
        })),
    )
    .await
}

#[tokio::test]
async fn a_code_is_single_use_and_an_expired_one_is_refused() {
    let state = state();
    let code = state.drivers().issue(false, None);
    let app = acp_http::router(state);

    let (status, paired) = claim(&app, &code.code, &["observe", "prompt"]).await;
    assert_eq!(status, StatusCode::OK, "{paired}");
    let token = paired["token"].as_str().expect("a token");
    assert_eq!(paired["driver"]["name"], "devin");
    assert!(
        paired["driver"]["scopes"]
            .as_array()
            .is_some_and(|scopes| scopes == &["observe", "prompt"]),
        "{paired}"
    );

    // The code was removed when it was claimed: a second driver holding the same
    // piece of paper gets nothing.
    let (status, body) = claim(&app, &code.code, &["observe"]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_pairing");

    // A code that outlived its life is refused by name rather than as unknown: a
    // driver that waited too long is told to ask for another.
    let expired = AppState::new()
        .with_drivers(Arc::new(Drivers::memory().with_pairing_ttl(Duration::ZERO)))
        .with_bridge(BridgeSettings {
            enabled: true,
            bind: Some("127.0.0.1:0".to_owned()),
            public_url: None,
        });
    let stale = expired.drivers().issue(false, None);
    let (status, body) = claim(&acp_http::router(expired), &stale.code, &["observe"]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "pairing_expired");

    // The token minted above is a real credential, not a string in a response.
    let (status, _) = send(
        &app,
        "POST",
        "/acp",
        Some(token),
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_request_without_a_driver_token_is_refused_in_the_envelope() {
    let app = acp_http::router(state());
    let call = Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }));

    let (status, body) = send(&app, "POST", "/acp", None, call.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "unauthorized");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("driver token")),
        "{body}"
    );

    let (status, _) = send(&app, "POST", "/acp", Some("not-a-token"), call).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_driver_cannot_grant_itself_permission_answers() {
    let state = state();
    let code = state.drivers().issue(false, None);
    let app = acp_http::router(state);
    let (status, paired) = claim(&app, &code.code, &["observe", "answer_permissions"]).await;
    assert_eq!(status, StatusCode::OK, "{paired}");
    assert!(
        !paired["driver"]["scopes"]
            .as_array()
            .expect("scopes")
            .iter()
            .any(|scope| scope == "answer_permissions"),
        "the code was issued without the grant, so the request cannot become one: {paired}"
    );
    let token = paired["token"].as_str().expect("a token");

    let (status, body) = send(
        &app,
        "POST",
        "/acp",
        Some(token),
        Some(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "session/answer_permission",
            "params": { "requestId": "r-1", "option": "allow_once" }
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "JSON-RPC failures are answers, not HTTP errors"
    );
    assert_eq!(body["error"]["code"], -32003, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("answer_permissions")),
        "{body}"
    );
}

#[tokio::test]
async fn a_driver_with_the_grant_answers_a_pending_question() {
    let directory = tempfile::tempdir().expect("temp dir");
    let broker = Arc::new(Broker::with_timeout(
        Policy::shipped(),
        std::fs::canonicalize(directory.path()).expect("the workspace resolves"),
        None,
    ));
    let state = state().with_broker(Arc::clone(&broker));
    let code = state.drivers().issue(true, None);
    let app = acp_http::router(state);
    let (status, paired) = claim(&app, &code.code, &["observe", "answer_permissions"]).await;
    assert_eq!(status, StatusCode::OK, "{paired}");
    let token = paired["token"].as_str().expect("a token");

    // A question the way an agent raises one: the broker announces it and a task
    // waits for the answer this driver is about to give.
    let mut announcements = broker.subscribe();
    let asking = Arc::clone(&broker);
    let waiting = tokio::spawn(async move {
        asking
            .ask(
                "session-1",
                Subject::new(PermissionKind::Terminal, "cargo test"),
            )
            .await
    });
    let request = tokio::time::timeout(Duration::from_secs(5), announcements.recv())
        .await
        .expect("the request is announced")
        .expect("the stream is open");

    let (status, body) = send(
        &app,
        "POST",
        "/acp",
        Some(token),
        Some(json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "session/answer_permission",
            "params": { "requestId": request.id, "option": "allow_once" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["accepted"], true, "{body}");

    let answer = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the asker is released")
        .expect("the asker did not panic");
    assert!(answer.allowed(), "the driver's answer reached the broker");
}

#[tokio::test]
async fn a_driver_opens_a_session_and_prompts_a_real_agent() {
    let directory = tempfile::tempdir().expect("temp dir");
    let profile = directory.path().join("profile");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&profile).expect("profile");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let fixture = crate::common::mock_agent();
    assert!(fixture.exists(), "the fixture agent is built: {fixture:?}");
    std::fs::write(
        profile.join("agents.toml"),
        format!(
            "schema_version = 1\n\n[[agents]]\nid = \"mock\"\nname = \"Mock\"\n\
             transport = \"acp-stdio\"\ncommand = '{}'\n",
            fixture.display().to_string().replace('\\', "/")
        ),
    )
    .expect("agents.toml");
    let registry = acpilot_agent::Registry::load(
        profile.join("agents.toml"),
        Arc::new(acpilot_agent::MapResolver::new()),
    )
    .expect("a registry");
    let workspace = std::fs::canonicalize(&workspace).expect("the workspace resolves");
    let state = AppState::new()
        .with_registry(Arc::new(registry))
        .with_workspace(Workspace {
            id: "work".to_owned(),
            profile: String::new(),
            root: workspace.clone(),
        })
        .with_broker(Arc::new(Broker::new(Policy::shipped(), workspace.clone())))
        .with_drivers(Arc::new(Drivers::memory()))
        .with_bridge(BridgeSettings {
            enabled: true,
            bind: Some("127.0.0.1:0".to_owned()),
            public_url: None,
        });
    let code = state.drivers().issue(false, None);
    let app = acp_http::router(state);
    let (status, paired) = claim(&app, &code.code, &["observe", "prompt"]).await;
    assert_eq!(status, StatusCode::OK, "{paired}");
    let token = paired["token"].as_str().expect("a token");

    let (status, session) = send(
        &app,
        "POST",
        "/acp",
        Some(token),
        Some(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace, "mcpServers": [] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let session_id = session["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("a session id: {session}"))
        .to_owned();

    let (status, answer) = send(
        &app,
        "POST",
        "/acp",
        Some(token),
        Some(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "stream 2" }]
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
}

#[tokio::test]
async fn a_scripted_driver_waits_while_the_cockpit_answers_the_permission() {
    let directory = tempfile::tempdir().expect("temp dir");
    let profile = directory.path().join("profile");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&profile).expect("profile");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let fixture = crate::common::mock_agent();
    std::fs::write(
        profile.join("agents.toml"),
        format!(
            "schema_version = 1\n\n[[agents]]\nid = \"mock\"\nname = \"Mock\"\n\
             transport = \"acp-stdio\"\ncommand = '{}'\n",
            fixture.display().to_string().replace('\\', "/")
        ),
    )
    .expect("agents.toml");
    let registry = acpilot_agent::Registry::load(
        profile.join("agents.toml"),
        Arc::new(acpilot_agent::MapResolver::new()),
    )
    .expect("a registry");
    let workspace = std::fs::canonicalize(&workspace).expect("the workspace resolves");
    let broker = Arc::new(Broker::new(Policy::shipped(), workspace.clone()));
    let state = AppState::new()
        .with_registry(Arc::new(registry))
        .with_workspace(Workspace {
            id: "work".to_owned(),
            profile: String::new(),
            root: workspace.clone(),
        })
        .with_broker(Arc::clone(&broker))
        .with_drivers(Arc::new(Drivers::memory()))
        .with_bridge(BridgeSettings {
            enabled: true,
            bind: Some("127.0.0.1:0".to_owned()),
            public_url: None,
        });
    let code = state.drivers().issue(false, None);
    let mut driver_stream = state.drivers().subscribe();
    let app = acp_http::router(state.clone());
    let cockpit = router_with(state);
    let (status, paired) = claim(&app, &code.code, &["observe", "prompt"]).await;
    assert_eq!(status, StatusCode::OK, "{paired}");
    let token = paired["token"].as_str().expect("a token").to_owned();
    let driver = paired["driver"]["id"].as_str().expect("an id").to_owned();

    let (status, session) = send(
        &app,
        "POST",
        "/acp",
        Some(&token),
        Some(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace, "mcpServers": [] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let session_id = session["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();

    // The script prompts a turn the agent raises a permission question for. The
    // daemon routes it to the person, because this driver was paired without the
    // grant; the script's request stays open until the cockpit answers.
    let prompted = tokio::spawn({
        let app = app.clone();
        let token = token.clone();
        let params = json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": "permission" }]
        });
        async move {
            send(
                &app,
                "POST",
                "/acp",
                Some(&token),
                Some(json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": params })),
            )
            .await
        }
    });

    let request = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(request) = broker.pending().into_iter().next() {
                return request;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the agent's question reaches the broker");

    // What the script received while it waited: the explanation, and never the
    // question itself.
    let mut question = false;
    let mut notice = false;
    while let Ok(message) = driver_stream.try_recv() {
        if message.driver != driver {
            continue;
        }
        if message.message["method"] == "session/request_permission" {
            question = true;
        }
        if message.message["params"]["update"]["sessionUpdate"] == "notice" {
            notice = serde_json::to_string(&message.message)
                .is_ok_and(|text| text.contains("answer_permissions"));
        }
    }
    assert!(!question, "a driver without the grant is never asked");
    assert!(notice, "the driver is told why it is waiting");

    // The person answers in the cockpit; the script's turn finishes as `end_turn`.
    let (status, body) = send(
        &cockpit,
        "POST",
        &format!("/v1/permissions/{}/resolve", request.id),
        None,
        Some(json!({ "option": "allow_once" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, answer) = tokio::time::timeout(Duration::from_secs(20), prompted)
        .await
        .expect("the prompt finishes")
        .expect("the task did not panic");
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
}

#[tokio::test]
async fn a_driver_reads_the_tool_calls_a_plan_and_a_mode_change() {
    let directory = tempfile::tempdir().expect("temp dir");
    let profile = directory.path().join("profile");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&profile).expect("profile");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let fixture = crate::common::mock_agent();
    std::fs::write(
        profile.join("agents.toml"),
        format!(
            "schema_version = 1\n\n[[agents]]\nid = \"mock\"\nname = \"Mock\"\n\
             transport = \"acp-stdio\"\ncommand = '{}'\n",
            fixture.display().to_string().replace('\\', "/")
        ),
    )
    .expect("agents.toml");
    let registry = acpilot_agent::Registry::load(
        profile.join("agents.toml"),
        Arc::new(acpilot_agent::MapResolver::new()),
    )
    .expect("a registry");
    let workspace = std::fs::canonicalize(&workspace).expect("the workspace resolves");
    let broker = Arc::new(Broker::new(Policy::shipped(), workspace.clone()));
    let state = AppState::new()
        .with_registry(Arc::new(registry))
        .with_workspace(Workspace {
            id: "work".to_owned(),
            profile: String::new(),
            root: workspace.clone(),
        })
        .with_broker(Arc::clone(&broker))
        .with_drivers(Arc::new(Drivers::memory()))
        .with_bridge(BridgeSettings {
            enabled: true,
            bind: Some("127.0.0.1:0".to_owned()),
            public_url: None,
        });
    let code = state.drivers().issue(false, None);
    let mut stream = state.drivers().subscribe();
    let app = acp_http::router(state.clone());
    let (status, paired) = claim(&app, &code.code, &["observe", "prompt"]).await;
    assert_eq!(status, StatusCode::OK, "{paired}");
    let token = paired["token"].as_str().expect("a token").to_owned();
    let driver = paired["driver"]["id"].as_str().expect("an id").to_owned();

    let (status, session) = send(
        &app,
        "POST",
        "/acp",
        Some(&token),
        Some(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace.clone(), "mcpServers": [] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let session_id = session["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("a session id: {session}"))
        .to_owned();

    let (status, answer) = send(
        &app,
        "POST",
        "/acp",
        Some(&token),
        Some(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "tools" }]
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");

    // Every update the paired driver was sent during the turn. The agent emitted
    // these through a real session process, so the payloads are the ones the event
    // log actually writes rather than ones this test made up.
    let mut updates = Vec::new();
    while let Ok(message) = stream.try_recv() {
        if message.driver != driver {
            continue;
        }
        if let Some(update) = message.message["params"].get("update") {
            updates.push(update.clone());
        }
    }
    let find = |name: &str| {
        updates
            .iter()
            .find(|update| update["sessionUpdate"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("`{name}` in {updates:?}"))
    };

    let call = find("tool_call");
    assert_eq!(call["toolCallId"], "tool-1");
    assert_eq!(call["title"], "Read seed.txt");
    assert_eq!(call["kind"], "read");
    assert_eq!(
        call["locations"][0]["path"],
        workspace.join("seed.txt").display().to_string(),
        "the driver is told which file the call touches"
    );
    assert_eq!(find("tool_call_update")["status"], "completed");
    assert_eq!(find("plan")["entries"][0]["content"], "Read seed.txt");
    assert_eq!(find("current_mode_update")["currentModeId"], "code");
}

#[tokio::test]
async fn a_revoked_driver_is_refused_by_its_next_request() {
    let state = state();
    let code = state.drivers().issue(false, None);
    let app = acp_http::router(state.clone());
    let (status, paired) = claim(&app, &code.code, &["observe"]).await;
    assert_eq!(status, StatusCode::OK, "{paired}");
    let token = paired["token"].as_str().expect("a token").to_owned();
    let id = paired["driver"]["id"].as_str().expect("an id").to_owned();

    let initialize = Some(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "clientInfo": { "name": "devin", "version": "2.1" } }
    }));
    let (status, body) = send(&app, "POST", "/acp", Some(&token), initialize.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["agentInfo"]["name"], "acpilotd");

    // The cockpit revokes; the driver's next call is a stranger.
    let cockpit = router_with(state);
    let (status, body) = send(
        &cockpit,
        "POST",
        &format!("/v1/acp/drivers/{id}/revoke"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "revoked");

    let (status, body) = send(&app, "POST", "/acp", Some(&token), initialize).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the old token has nothing left to verify against"
    );
    assert_eq!(body["code"], "unauthorized");

    // The cockpit keeps the row: the audit question outlives the credential.
    let (status, rows) = send(&cockpit, "GET", "/v1/acp/drivers", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows[0]["status"], "revoked");
}
