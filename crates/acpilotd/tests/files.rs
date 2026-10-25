//! The file routes, driven through the router against a real directory.
//!
//! The traversal tests are the point of this file. A path that leaves the
//! workspace is the one failure in this API that a well-meaning client can cause
//! by accident, and every spelling of it — `..`, an absolute path, a symlink — is
//! tried here rather than left to the string comparison's good intentions.

use std::fs;

use acpilotd::{AppState, Workspace};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app(state: AppState) -> axum::Router {
    acpilotd::router_with(state)
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(Body::from(
            body.map_or_else(String::new, |value| value.to_string()),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("router is infallible");
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

/// A daemon serving `directory` as the workspace named `app`.
fn serving(directory: &std::path::Path) -> AppState {
    AppState::new().with_workspace(Workspace {
        id: "app".to_owned(),
        root: directory.canonicalize().expect("resolved"),
    })
}

/// A workspace with a couple of files in it.
fn workspace() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temp dir");
    fs::create_dir(directory.path().join("src")).expect("src");
    fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").expect("main");
    fs::write(directory.path().join("README.md"), "# app\n").expect("readme");
    directory
}

#[tokio::test]
async fn a_read_returns_the_text_and_a_token() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?workspace=app&path=src/main.rs",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["path"], "src/main.rs");
    assert_eq!(body["text"], "fn main() {}\n");
    let etag = body["etag"].as_str().expect("an etag").to_owned();

    // The same bytes and the same file must produce the same token, or every
    // save would look like a conflict.
    let (_, again) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?workspace=app&path=src/main.rs",
        None,
    )
    .await;
    assert_eq!(again["etag"], etag);
}

#[tokio::test]
async fn a_directory_is_a_conflict_that_names_the_list_route() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=src",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("/v1/files/list")),
        "{body}"
    );
}

#[tokio::test]
async fn a_path_that_climbs_out_is_a_400_before_it_is_resolved() {
    let directory = workspace();
    for path in ["../secret.env", "src/../../secret.env", "src/./../../x"] {
        let (status, body) = send(
            app(serving(directory.path())),
            "GET",
            &format!("/v1/files?path={path}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["details"][0]["field"], "path", "{path}: {body}");
    }
}

#[tokio::test]
async fn an_absolute_path_is_refused() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=/etc/passwd",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("absolute")),
        "{body}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_workspace_is_a_403() {
    // The case a string prefix check cannot catch: the path stays inside, the
    // file does not.
    let directory = workspace();
    let outside = tempfile::tempdir().expect("outside");
    fs::write(outside.path().join("secret.env"), "TOKEN=1\n").expect("secret");
    std::os::unix::fs::symlink(
        outside.path().join("secret.env"),
        directory.path().join("link"),
    )
    .expect("symlink");

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=link",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn binary_content_is_refused_rather_than_mangled() {
    let directory = workspace();
    fs::write(directory.path().join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).expect("blob");
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=blob.bin",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("UTF-8")),
        "{body}"
    );
}

#[tokio::test]
async fn a_file_over_the_cap_is_refused() {
    let directory = workspace();
    let big = vec![b'a'; usize::try_from(acpilotd::MAX_FILE_BYTES).expect("fits") + 1];
    fs::write(directory.path().join("big.txt"), &big).expect("big");
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=big.txt",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_listing_is_sorted_and_uses_forward_slashes() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list?path=",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body.as_array().expect("an array");
    let paths: Vec<&str> = entries
        .iter()
        .map(|entry| entry["path"].as_str().expect("a path"))
        .collect();
    assert_eq!(paths, ["README.md", "src"]);
    assert_eq!(entries[1]["is_dir"], true);
    assert_eq!(entries[1]["size"], 0);
    assert_eq!(entries[0]["is_dir"], false);

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list?path=src",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["path"], "src/main.rs");
}

#[tokio::test]
async fn a_file_is_not_a_listing() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list?path=README.md",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn a_write_needs_no_token_to_create_and_one_to_replace() {
    let directory = workspace();
    let state = serving(directory.path());

    let (status, created) = send(
        app(state.clone()),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({ "path": "notes.txt", "text": "first\n" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(created["etag"].as_str().is_some());
    assert_eq!(
        fs::read_to_string(directory.path().join("notes.txt")).expect("written"),
        "first\n"
    );

    // The same body again: creating over a file that exists is a conflict, not a
    // silent replace.
    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({ "path": "notes.txt", "text": "second\n" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let etag = created["etag"].as_str().expect("etag").to_owned();
    let (status, replaced) = send(
        app(state.clone()),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({
            "path": "notes.txt",
            "text": "second\n",
            "if_match": etag
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_ne!(replaced["etag"], etag);
}

#[tokio::test]
async fn a_stale_token_is_a_conflict_and_a_missing_file_is_a_404() {
    let directory = workspace();
    let state = serving(directory.path());
    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({
            "path": "README.md",
            "text": "clobbered\n",
            "if_match": "0-0-nothing"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        fs::read_to_string(directory.path().join("README.md")).expect("read"),
        "# app\n",
        "the file must not have been touched"
    );

    let (status, _) = send(
        app(state),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({
            "path": "gone.txt",
            "text": "x",
            "if_match": "0-0-nothing"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_write_into_a_missing_directory_is_a_404_not_a_mkdir() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({ "path": "new/deep/file.txt", "text": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!directory.path().join("new").exists());
}

#[tokio::test]
async fn a_write_through_a_path_that_escapes_is_refused() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({ "path": "../escape.txt", "text": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(!directory.path().join("..").join("escape.txt").exists());
}

#[tokio::test]
async fn an_unknown_workspace_is_a_404_and_no_workspace_is_a_conflict() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?workspace=other&path=README.md",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, _) = send(
        app(AppState::new()),
        "GET",
        "/v1/files?path=README.md",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_contract_lists_the_file_routes() {
    let (status, body) = send(app(AppState::new()), "GET", "/v1/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["paths"]["/v1/files"]["get"].is_object());
    assert!(body["paths"]["/v1/files"]["put"].is_object());
    assert!(body["paths"]["/v1/files/list"]["get"].is_object());
}
