//! The project map route, driven through the router against a real directory.
//!
//! The map is the agent's document, and the two states that matter are the ones a
//! person meets first: a repository nobody has mapped yet, which must read as an
//! invitation rather than an error, and a map that does not parse, which must said be
//! outright because the alternative is a panel that says "no map" over a file that is
//! sitting right there.

use std::fs;

use acpilotd::{AppState, Workspace};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app(state: AppState) -> axum::Router {
    acpilotd::router_with(state)
}

async fn send(uri: &str, directory: &std::path::Path) -> (StatusCode, serde_json::Value) {
    let state = AppState::new().with_workspace(Workspace {
        id: "app".to_owned(),
        root: directory.canonicalize().expect("resolved"),
    });
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    let response = app(state)
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
async fn a_workspace_nobody_has_mapped_answers_with_an_empty_map() {
    let directory = tempfile::tempdir().expect("a workspace");
    let (status, body) = send("/v1/project/map", directory.path()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["written"], false,
        "absence is a state, not a 404: {body}"
    );
    assert_eq!(body["path"], ".acpilot/project-map.toml");
    assert_eq!(body["areas"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn the_map_is_served_as_areas_in_the_order_the_agent_wrote_them() {
    let directory = tempfile::tempdir().expect("a workspace");
    fs::create_dir(directory.path().join(".acpilot")).expect(".acpilot");
    fs::write(
        directory.path().join(".acpilot/project-map.toml"),
        "updated = \"2026-11-01\"\n\
         [[areas]]\nname = \"TUI shell\"\npath = \"crates/acpilot-tui\"\n\
         summary = \"the terminal frontend\"\ndepends_on = [\"acpilot-proto\", \"acpilot-client\"]\n\
         [[areas]]\nname = \"Daemon\"\npath = \"crates/acpilotd\"\nsummary = \"sessions, files, agents\"\n",
    )
    .expect("map");

    let (status, body) = send("/v1/project/map", directory.path()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["written"], true);
    assert_eq!(body["updated"], "2026-11-01");
    let names: Vec<&str> = body["areas"]
        .as_array()
        .expect("areas")
        .iter()
        .map(|area| area["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(names, vec!["TUI shell", "Daemon"], "written order: {body}");
    assert_eq!(
        body["areas"][0]["depends_on"],
        serde_json::json!(["acpilot-proto", "acpilot-client"])
    );
    assert!(
        body["areas"][1].get("depends_on").is_none(),
        "an area with no edges costs no bytes saying so: {body}"
    );
}

#[tokio::test]
async fn a_map_that_does_not_parse_is_a_bad_request_that_names_it() {
    let directory = tempfile::tempdir().expect("a workspace");
    fs::create_dir(directory.path().join(".acpilot")).expect(".acpilot");
    fs::write(
        directory.path().join(".acpilot/project-map.toml"),
        "areas = 3\n",
    )
    .expect("map");
    let (status, body) = send("/v1/project/map", directory.path()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["message"].as_str().expect("a message");
    assert!(
        message.contains("project-map.toml"),
        "a parse error has to name the file to fix: {message}"
    );
}

#[tokio::test]
async fn a_daemon_with_no_workspace_cannot_map_one() {
    let request = Request::builder()
        .method("GET")
        .uri("/v1/project/map")
        .body(Body::empty())
        .expect("request");
    let response = app(AppState::new())
        .oneshot(request)
        .await
        .expect("router is infallible");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}
