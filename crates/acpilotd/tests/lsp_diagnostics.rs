//! `POST /v1/files/diagnostics` against a real language server, and what is kept.
//!
//! Ignored by default: it needs `rust-analyzer` on `PATH`, which a CI runner does not
//! promise. Run it with
//!
//! ```sh
//! cargo test -p acpilotd --test lsp_diagnostics -- --ignored
//! ```
//!
//! The claim is not that a server answers — `lsp_references.rs` and the TUI's own tests
//! cover that path. It is that an answer is *state*: the daemon keeps the newest
//! problems per file, so a client that connects after the answer (the reconnect a
//! laptop's sleep causes, the second window opened a minute later) is told what is
//! wrong instead of drawing an empty gutter until somebody saves. The store is what
//! `GET /v1/events` sends on connect, and it is the half of the push design that needs
//! no background index to be testable.
//!
//! The fixture's file does not *parse*, which is deliberate: a syntax error is what
//! rust-analyzer reports on its own, while a type mismatch comes from a `cargo check`
//! it may not have run. A cold server also publishes an empty list before its analysis
//! arrives, which is why the test asks more than once rather than trusting the first
//! answer.

use std::fs;

use acpilotd::{AppState, Workspace};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A file with an unclosed parameter list.
const SOURCE: &str = "pub fn broken( {\n";

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

async fn post(
    state: AppState,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
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
#[ignore = "needs rust-analyzer on PATH"]
async fn an_answer_is_kept_for_the_client_that_connects_after_it() {
    let directory = workspace();
    let state = AppState::new().with_workspace(Workspace {
        id: "app".to_owned(),
        profile: String::new(),
        root: directory.path().canonicalize().expect("resolved"),
    });

    // A cold server's first publish can be the empty one that arrives before its
    // analysis, so the question is asked again after a pause — which is exactly what
    // the pane does while the buffer sits still.
    let mut answer = serde_json::Value::Null;
    for attempt in 1..=20 {
        let (status, body) = post(
            state.clone(),
            "/v1/files/diagnostics",
            serde_json::json!({ "path": "src/lib.rs" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["server"], "rust-analyzer",
            "the server answered rather than being absent: {body}"
        );
        let found = body["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty());
        answer = body;
        if found {
            break;
        }
        assert!(attempt < 20, "twenty answers and no problem: {answer}",);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let items = answer["items"].as_array().expect("a list of problems");
    assert_eq!(
        items[0]["severity"], "error",
        "a file that does not parse has an error: {answer}"
    );

    // The half a late client sees: the same answer, kept, with the file's path as the
    // workspace spells it. No version, because the request named text rather than a
    // version and the server never said which number it judged.
    let snapshot = state.diagnostics_snapshot();
    assert_eq!(snapshot.len(), 1, "one file was asked about");
    assert_eq!(snapshot[0].path, "src/lib.rs");
    assert_eq!(
        snapshot[0].problems.items.len(),
        items.len(),
        "the snapshot is the answer that was served"
    );
    assert!(
        snapshot[0].version.is_none(),
        "an answered question has no version to name: {:?}",
        snapshot[0].version
    );

    // And an answer about fixed text replaces it, rather than the store keeping the
    // problems of a version nobody is looking at any more.
    let (status, fixed) = post(
        state.clone(),
        "/v1/files/diagnostics",
        serde_json::json!({ "path": "src/lib.rs", "text": "pub fn fixed() {}\n" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fixed}");
    assert!(
        fixed["items"]
            .as_array()
            .is_some_and(|items| items.is_empty()),
        "the text that parses has no problems: {fixed}"
    );
    let snapshot = state.diagnostics_snapshot();
    assert_eq!(snapshot.len(), 1, "the same file, not a second row");
    assert!(
        snapshot[0].problems.items.is_empty(),
        "the newest answer wins: {:?}",
        snapshot[0].problems.items
    );

    state.language_servers().shutdown().await;
}
