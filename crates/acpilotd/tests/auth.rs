//! The token a TCP listener requires, tested through the layer itself.
//!
//! A socket needs no token — its access control is the mode bits — so this layer
//! is only ever applied by the TCP listener. What is tested here is the decision:
//! no header, the wrong token, the right token, and the scheme's case, because a
//! client that sends `bearer` is not an attacker and should not be treated as one.

use acpilotd::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

const TOKEN: &str = "8f5b9d0c2a7e41f3b6c8d9e0f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4";

async fn health_with(authorization: Option<&str>) -> (StatusCode, serde_json::Value) {
    let router = acpilotd::protect(acpilotd::router_with(AppState::new()), TOKEN);
    let mut request = Request::builder().uri("/v1/health");
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
async fn the_scheme_is_case_insensitive() {
    let (status, _) = health_with(Some(&format!("bearer {TOKEN}"))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_bare_token_without_the_scheme_is_refused() {
    let (status, _) = health_with(Some(TOKEN)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
