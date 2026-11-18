//! The extension routes: install from a directory, list, activate, themes,
//! verify, and remove.
//!
//! The store is a real directory and the fixture is the shipped night-flight
//! extension, so an install here copies and hashes real bytes rather than a
//! fixture that stands in for a store the routes never see.

use std::fs;
use std::path::{Path, PathBuf};

use acpilotd::{AppState, router_with};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(Body::from(
            body.map_or_else(String::new, |value| value.to_string()),
        ))
        .expect("request");
    let response = app
        .clone()
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions/acpilot.night-flight")
}

/// A daemon with its own store, and a profile directory to activate into.
fn daemon(root: &Path) -> (Router, AppState) {
    let profile = root.join("config/profiles/work");
    let state = AppState::new()
        .with_extension_store(root.join("store"))
        .with_profile_name(Some("work".to_owned()))
        .with_profile_root(Some(profile));
    (router_with(state.clone()), state)
}

fn install() -> serde_json::Value {
    serde_json::json!({
        "id": "acpilot.night-flight",
        "path": fixture().display().to_string(),
        "activate": false,
    })
    .clone()
}

#[tokio::test]
async fn an_installed_extension_lists_activates_in_themes_and_removes() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (app, state) = daemon(directory.path());
    let mut notices = state.subscribe_extensions();

    let (status, installed) = send(&app, "POST", "/v1/extensions", Some(install())).await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    assert_eq!(installed["id"], "acpilot.night-flight");
    assert_eq!(installed["version"], "1.0.0");
    assert_eq!(installed["status"], "installed");
    assert_eq!(installed["kinds"][0], "theme");
    assert!(
        installed["checksum"]
            .as_str()
            .is_some_and(|checksum| checksum.starts_with("sha256:"))
    );
    assert_eq!(
        notices
            .try_recv()
            .expect("installed notice")
            .action
            .event_name(),
        "extension.installed"
    );

    let (status, rows) = send(&app, "GET", "/v1/extensions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows.as_array().expect("an array").len(), 1, "{rows}");
    assert_eq!(rows[0]["name"], "Night flight");

    let (status, detail) = send(&app, "GET", "/v1/extensions/acpilot.night-flight", None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["versions"][0]["version"], "1.0.0");
    assert!(
        detail["manifest"]
            .as_str()
            .is_some_and(|manifest| manifest.contains("schema_version")),
        "{detail}"
    );
    assert!(
        !detail["contributions"]
            .as_array()
            .expect("contributions")
            .is_empty(),
        "{detail}"
    );

    let (status, action) = send(
        &app,
        "POST",
        "/v1/extensions/acpilot.night-flight/activate",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{action}");
    assert_eq!(action["changed"], true);
    assert_eq!(action["profile"], "work");
    assert_eq!(
        notices
            .try_recv()
            .expect("activated notice")
            .action
            .event_name(),
        "extension.activated"
    );

    let (_, rows) = send(&app, "GET", "/v1/extensions", None).await;
    assert_eq!(rows[0]["status"], "active");
    assert_eq!(rows[0]["active_profiles"][0], "work");

    let (status, themes) = send(&app, "GET", "/v1/themes", None).await;
    assert_eq!(status, StatusCode::OK, "{themes}");
    let themes = themes.as_array().expect("a theme list");
    let night = themes
        .iter()
        .find(|theme| theme["id"] == "acpilot.night-flight")
        .expect("the activated theme is listed");
    assert_eq!(night["appearance"], "dark");
    assert_eq!(night["ui"]["bg"], "#070d18");
    assert!(
        night["syntax"]
            .as_object()
            .is_some_and(|tokens| !tokens.is_empty())
    );

    let (status, removed) = send(&app, "DELETE", "/v1/extensions/acpilot.night-flight", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{removed}");

    let (status, action) = send(
        &app,
        "POST",
        "/v1/extensions/acpilot.night-flight/deactivate",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{action}");
    assert_eq!(action["changed"], true);
    assert_eq!(
        notices
            .try_recv()
            .expect("deactivated notice")
            .action
            .event_name(),
        "extension.deactivated"
    );

    let (status, removed) = send(&app, "DELETE", "/v1/extensions/acpilot.night-flight", None).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["versions"][0], "1.0.0");
    assert_eq!(
        notices
            .try_recv()
            .expect("removed notice")
            .action
            .event_name(),
        "extension.removed"
    );

    let (status, gone) = send(&app, "GET", "/v1/extensions/acpilot.night-flight", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
}

#[tokio::test]
async fn editing_a_file_makes_verify_report_a_blocked_version() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (app, _) = daemon(directory.path());

    let (status, installed) = send(&app, "POST", "/v1/extensions", Some(install())).await;
    assert_eq!(status, StatusCode::OK, "{installed}");

    let manifest = directory
        .path()
        .join("store/acpilot.night-flight/1.0.0/extension.toml");
    let mut text = fs::read_to_string(&manifest).expect("read the manifest");
    text.push_str("\n# edited after install\n");
    fs::write(&manifest, text).expect("edit the manifest");

    let (status, verdict) = send(
        &app,
        "POST",
        "/v1/extensions/acpilot.night-flight/verify",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{verdict}");
    assert_eq!(verdict["ready"], false);
    assert_eq!(verdict["extensions"][0]["status"], "blocked");

    let (_, rows) = send(&app, "GET", "/v1/extensions", None).await;
    assert_eq!(rows[0]["status"], "blocked");
}

#[tokio::test]
async fn activating_something_that_is_not_installed_is_a_404() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (app, _) = daemon(directory.path());

    let (status, body) = send(
        &app,
        "POST",
        "/v1/extensions/acpilot.missing/activate",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}
