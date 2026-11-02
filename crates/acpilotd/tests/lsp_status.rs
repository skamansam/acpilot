//! `GET /v1/lsp/servers`, and a server that stops when nobody asks.
//!
//! The route is the registry's report: one row per language this build can start,
//! because "no server for this language" is exactly what a references list answered by
//! the parse tree means. Two of the tests need no server at all; the third needs
//! `rust-analyzer` and says so.

use std::fs;
use std::time::Duration;

use acpilotd::{AppState, Workspace};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn get(state: AppState, uri: &str) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    let response = acpilotd::router_with(state)
        .oneshot(request)
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
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn every_language_this_build_can_start_is_a_row() {
    let (status, body) = get(AppState::new(), "/v1/lsp/servers").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let rows = body.as_array().expect("a list of rows");
    let languages: Vec<&str> = rows
        .iter()
        .filter_map(|row| row["languages"][0].as_str())
        .collect();
    assert_eq!(
        languages,
        vec!["rust", "python", "typescript", "javascript"],
        "the languages this build has a server for, in the order it tried them: {body}"
    );

    for row in rows {
        assert_eq!(
            row["status"], "stopped",
            "a daemon that has asked nothing has started nothing: {body}"
        );
        assert!(
            row["encoding"].is_null(),
            "an encoding is what a handshake agreed, and there was none: {body}"
        );
        assert!(
            row["round_trip_ms"].is_null(),
            "no question, no round trip: {body}"
        );
        assert!(
            row["command"]
                .as_str()
                .is_some_and(|command| !command.is_empty()),
            "a row says which program it would run: {body}"
        );
    }
}

#[tokio::test]
async fn the_contract_lists_the_route() {
    let (status, body) = get(AppState::new(), "/v1/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["paths"]["/v1/lsp/servers"]["get"]["summary"], "What the language servers are doing",
        "a route a generated client cannot see is a route only this repository knows"
    );
    assert!(
        body["components"]["schemas"]["LspServerSummary"].is_object(),
        "the row type is in the document"
    );
}

/// A crate whose two modules both define and call `dup`.
const SOURCE: &str = "pub mod a { pub fn dup() {} pub fn call() { dup(); } }\n\
                      pub mod b { pub fn dup() {} pub fn call() { dup(); } }\n";

fn workspace() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("a workspace");
    fs::create_dir(directory.path().join("src")).expect("src");
    fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(directory.path().join("src/lib.rs"), SOURCE).expect("lib");
    directory
}

#[tokio::test]
#[ignore = "needs rust-analyzer on PATH"]
async fn a_server_nobody_has_asked_is_stopped_and_starts_again() {
    let directory = workspace();
    let root = directory.path().canonicalize().expect("resolved");
    let state = AppState::new().with_workspace(Workspace {
        id: "app".to_owned(),
        root,
    });
    let offset = SOURCE.find("dup();").expect("a call");
    let question = format!("/v1/files/references?q=dup&path=src/lib.rs&offset={offset}");

    // The first question starts the server, and pays for its index. How many rows
    // come back is the index's business — a cold server can answer with the
    // definition before it has found the call — so one row is enough to say it
    // answered at all, which is what this test is about.
    let (status, body) = get(state.clone(), &question).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.as_array().is_some_and(|rows| !rows.is_empty()),
        "the server answered: {body}"
    );

    let running = state
        .language_servers()
        .status()
        .await
        .into_iter()
        .find(|server| server.language == "rust")
        .expect("a rust row");
    let session = running.running.as_ref().expect("a session is running");
    assert!(session.requests >= 1, "the question was counted");
    assert!(
        session.round_trip.is_some(),
        "the answer took a measurable time"
    );

    // Nothing has asked for zero seconds, which is every server: what an idle sweep
    // does to one that has sat for five minutes.
    let stopped = state.language_servers().stop_idle(Duration::ZERO).await;
    assert_eq!(stopped, vec!["rust"], "the session that answered");
    assert!(
        state
            .language_servers()
            .stop_idle(Duration::ZERO)
            .await
            .is_empty(),
        "a stopped session is gone rather than stopped again"
    );

    let stopped_row = state
        .language_servers()
        .status()
        .await
        .into_iter()
        .find(|server| server.language == "rust")
        .expect("a rust row");
    assert!(
        stopped_row.running.is_none(),
        "the row says stopped, which is what a client draws"
    );

    // And the next question starts a fresh one rather than finding a hole: the entry
    // was removed, so this is the same path the first question took. The session is
    // the evidence rather than the row count, which the new index is still deciding.
    let (status, body) = get(state.clone(), &question).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.as_array().is_some_and(|rows| !rows.is_empty()),
        "a restarted server answers at all: {body}"
    );
    let restarted = state
        .language_servers()
        .status()
        .await
        .into_iter()
        .find(|server| server.language == "rust")
        .expect("a rust row")
        .running
        .expect("the second question started a new session");
    assert!(restarted.requests >= 1, "and it answered one question");
    state.language_servers().shutdown().await;
}
