//! The token a TCP listener requires, tested through the layer itself.
//!
//! A socket needs no token — its access control is the mode bits — so this layer
//! is only ever applied by the TCP listener. What is tested here is the decision:
//! no header, the wrong token, the right token, the scheme's case, because a
//! client that sends `bearer` is not an attacker and should not be treated as one,
//! and a rotation, because the file is read per request and a token replaced on
//! disk has to be the one that works next.

use acpilotd::AppState;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use tower::ServiceExt;

const TOKEN: &str = "8f5b9d0c2a7e41f3b6c8d9e0f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4";

/// A router guarded by a token file holding `TOKEN`, and the directory keeping it
/// alive for as long as the test needs it.
fn guarded() -> (tempfile::TempDir, Router) {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write");
    let router = acpilotd::protect(acpilotd::router_with(AppState::new()), &path);
    (directory, router)
}

/// Ask `router` for `/v1/health`, with `authorization` when there is one.
async fn ask(router: &Router, authorization: Option<&str>) -> Response {
    let mut request = Request::builder().uri("/v1/health");
    if let Some(value) = authorization {
        request = request.header(header::AUTHORIZATION, value);
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("router is infallible")
}

async fn status_of(uri: &str, authorization: Option<&str>) -> (StatusCode, serde_json::Value) {
    let (_directory, router) = guarded();
    let mut request = Request::builder().uri(uri);
    if let Some(value) = authorization {
        request = request.header(header::AUTHORIZATION, value);
    }
    let response = router
        .oneshot(request.body(Body::empty()).expect("request"))
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

async fn health_with(authorization: Option<&str>) -> (StatusCode, serde_json::Value) {
    status_of("/v1/health", authorization).await
}

#[tokio::test]
async fn a_request_with_no_token_is_401_in_the_envelope() {
    let (status, body) = health_with(None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "unauthorized");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("Authorization")),
        "{body}"
    );
}

#[tokio::test]
async fn a_wrong_token_is_refused_and_the_health_payload_is_not_leaked() {
    let (status, body) = health_with(Some("Bearer not-the-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["service"].is_null(), "no daemon details on a refusal");
}

#[tokio::test]
async fn the_right_token_is_let_through() {
    let (status, body) = health_with(Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["service"], "acpilotd");
}

#[tokio::test]
async fn the_docs_page_answers_without_a_token_so_it_can_ask_for_one() {
    // The route only exists with the `docs` feature, so a build without it answers
    // 404 — which is still the assertion: a 401 here would mean the page that asks
    // for the token was itself behind the token.
    let (status, _) = status_of("/docs", None).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_contract_is_not_open_along_with_the_page() {
    let (status, _) = status_of("/v1/openapi.json", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_scheme_is_case_insensitive() {
    let (status, _) = health_with(Some(&format!("bearer {TOKEN}"))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_bare_token_without_the_scheme_is_refused() {
    let (status, _) = health_with(Some(TOKEN)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_rotated_token_takes_effect_without_a_restart() {
    let (directory, router) = guarded();
    let path = directory.path().join("token");

    let before = ask(&router, Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(before.status(), StatusCode::OK);

    let fresh = acpilot_host::token::rotate(&path).expect("rotated");
    assert_ne!(fresh, TOKEN);

    let old = ask(&router, Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(
        old.status(),
        StatusCode::UNAUTHORIZED,
        "the token that may have leaked stops working on the next call"
    );
    let new = ask(&router, Some(&format!("Bearer {fresh}"))).await;
    assert_eq!(new.status(), StatusCode::OK, "and the new one works");
}

#[tokio::test]
async fn a_deleted_token_file_refuses_everything_rather_than_admitting_it() {
    let (directory, router) = guarded();
    std::fs::remove_file(directory.path().join("token")).expect("remove");

    let response = ask(&router, Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a missing file is no token, not any token"
    );
}
