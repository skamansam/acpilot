//! `GET /v1/files/references` with a real language server, when one is installed.
//!
//! Ignored by default: it needs `rust-analyzer` on `PATH`, which a CI runner does not
//! promise. Run it with
//!
//! ```sh
//! cargo test -p acpilotd --test lsp_references -- --ignored
//! ```
//!
//! The fixture is the point of the test. Two modules define a function with the same
//! name and each calls its own; the caret is on the *call in the first module*. A
//! scoped answer lists the definition and the call on line 1 and nothing on line 2,
//! while the parse-tree fallback — which is a spelling search with a parser's help —
//! lists all four. That difference is the whole reason this route asks a server first.

use std::fs;

use acpilotd::{AppState, Workspace};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

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

async fn get(directory: &std::path::Path, uri: &str) -> (StatusCode, serde_json::Value) {
    let state = AppState::new().with_workspace(Workspace {
        id: "app".to_owned(),
        root: directory.canonicalize().expect("resolved"),
    });
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
#[ignore = "needs rust-analyzer on PATH"]
async fn a_language_server_answers_with_the_uses_in_scope() {
    let directory = workspace();
    // The first `dup()` call, which is in module `a` on line 1.
    let offset = SOURCE.find("dup();").expect("a call");

    let (status, body) = get(
        directory.path(),
        &format!("/v1/files/references?q=dup&path=src/lib.rs&offset={offset}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let rows = body.as_array().expect("a list of rows");
    assert!(
        rows.len() >= 2,
        "the definition and the call is the least a scoped answer has: {body}"
    );
    for row in rows {
        assert_eq!(
            row["path"], "src/lib.rs",
            "the fixture is one file, as the server sees it: {body}"
        );
        assert_eq!(
            row["line"], 1,
            "a use in module `b` is a different function and must not be listed: {body}"
        );
        assert!(
            row["text"]
                .as_str()
                .is_some_and(|text| text.contains("dup")),
            "a row carries the line it is on: {body}"
        );
    }
    let columns: Vec<u64> = rows
        .iter()
        .filter_map(|row| row["column"].as_u64())
        .collect();
    assert!(
        columns.windows(2).all(|pair| pair[0] <= pair[1]),
        "rows are sorted by column: {body}"
    );
}

#[tokio::test]
#[ignore = "needs rust-analyzer on PATH"]
#[cfg(feature = "parse-references")]
async fn a_language_with_no_server_falls_back_to_the_parse_tree() {
    // Markdown has no server this build knows, so the question goes to the parse-tree
    // search over the workspace. That is the contrast the other test needs: four rows,
    // because spelling cannot tell module `a`'s `dup` from module `b`'s, and the prose
    // in the `.md` is not a row at all.
    let directory = workspace();
    fs::write(
        directory.path().join("NOTES.md"),
        "dup() is mentioned here\n",
    )
    .expect("notes");
    let (status, body) = get(
        directory.path(),
        "/v1/files/references?q=dup&path=NOTES.md&offset=0",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body.as_array().expect("a list of rows");
    assert_eq!(
        rows.len(),
        4,
        "both definitions and both calls, and nothing from the prose: {body}"
    );
    let lines: Vec<u64> = rows.iter().filter_map(|row| row["line"].as_u64()).collect();
    assert_eq!(lines, vec![1, 1, 2, 2], "{body}");
    assert!(
        rows.iter().all(|row| row["path"] == "src/lib.rs"),
        "a `.md` is skipped rather than searched as prose: {body}"
    );
}
