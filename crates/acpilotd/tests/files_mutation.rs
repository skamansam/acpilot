//! Creating directories and removing files, over the router.
//!
//! Both routes are the ones where the workspace guard has to be re-stated rather
//! than inherited: a write resolves the parent it is about to write into, while a
//! delete resolves the file itself and a `mkdir -p` resolves the deepest part that
//! already exists. Each of those is a different way for a symlink to leave the
//! workspace, so each gets its own refusal test.

use std::path::Path;

use acpilotd::{AppState, router_with};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app(root: &Path) -> axum::Router {
    let workspace = acpilotd::workspace_at(root).expect("workspace");
    router_with(AppState::new().with_workspace(workspace))
}

async fn send_status(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> StatusCode {
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
    // Drained, so a route that failed mid-write is not reported as a status only.
    let _ = response.into_body().collect().await.expect("body");
    status
}

fn directory(path: &str) -> Option<serde_json::Value> {
    Some(serde_json::json!({ "path": path }))
}

#[tokio::test]
async fn a_directory_is_created_with_the_parents_it_needs() {
    let root = tempfile::tempdir().expect("workspace");
    let status = send_status(
        app(root.path()),
        "POST",
        "/v1/files/directory",
        directory("notes/2026/november"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        root.path().join("notes/2026/november").is_dir(),
        "a dialog names the folder somebody wants, not every level above it"
    );
}

#[tokio::test]
async fn a_directory_that_is_already_there_is_a_conflict() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(root.path().join("src")).expect("src");
    let status = send_status(
        app(root.path()),
        "POST",
        "/v1/files/directory",
        directory("src"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_directory_cannot_be_made_to_escape_the_workspace() {
    let root = tempfile::tempdir().expect("workspace");
    let outside = root.path().parent().expect("a parent");
    let status = send_status(
        app(root.path()),
        "POST",
        "/v1/files/directory",
        directory("../escaped"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        !outside.join("escaped").exists(),
        "the refusal is before the filesystem, not after it"
    );
}

#[tokio::test]
async fn deleting_a_file_removes_it() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(root.path().join("notes.md"), "hi").expect("notes");
    let status = send_status(app(root.path()), "DELETE", "/v1/files?path=notes.md", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!root.path().join("notes.md").exists());
}

#[tokio::test]
async fn deleting_a_directory_is_refused_rather_than_recursive() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(root.path().join("src")).expect("src");
    let status = send_status(app(root.path()), "DELETE", "/v1/files?path=src", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(root.path().join("src").is_dir(), "nothing was removed");
}

#[tokio::test]
async fn deleting_something_that_is_not_there_is_a_404() {
    let root = tempfile::tempdir().expect("workspace");
    let status = send_status(app(root.path()), "DELETE", "/v1/files?path=ghost.md", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_outside_the_workspace_is_a_400() {
    let root = tempfile::tempdir().expect("workspace");
    let status = send_status(
        app(root.path()),
        "DELETE",
        "/v1/files?path=../notes.md",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_workspace_is_refused() {
    let root = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret.md"), "not yours").expect("secret");
    std::os::unix::fs::symlink(outside.path(), root.path().join("link")).expect("symlink");
    // The delete resolves the file itself, so the link is caught here rather than
    // by the parent-resolution a write uses.
    let status = send_status(
        app(root.path()),
        "DELETE",
        "/v1/files?path=link/secret.md",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(outside.path().join("secret.md").exists());
}
