//! The HTTP contract, exercised through the router itself rather than a live port.

use acpilot_core::AcpilotError;
use acpilotd::{ApiError, router};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn get(uri: &str) -> (StatusCode, serde_json::Value) {
    let response = router()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
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
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn health_reports_the_service_and_its_api_version() {
    let (status, body) = get("/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["service"], "acpilotd");
    assert_eq!(body["api"], "/v1");
}

#[tokio::test]
async fn openapi_is_served_and_valid() {
    let (status, body) = get("/v1/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["openapi"], "3.1.0");
    assert_eq!(body["info"]["title"], "Acpilot API");
}

#[tokio::test]
async fn unknown_route_is_a_404_with_no_surprises() {
    let (status, _) = get("/v1/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn errors_use_the_shared_envelope() {
    let response = ApiError(AcpilotError::denied("write outside workspace")).into_response();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["code"], "permission_denied");
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("outside workspace")
    );
    // Empty detail lists are omitted rather than serialised as `null`, so clients
    // can treat presence as meaningful.
    assert!(body.get("details").is_none());
}

#[cfg(feature = "docs")]
#[tokio::test]
async fn docs_page_points_at_our_openapi_route() {
    let response = router()
        .oneshot(
            Request::builder()
                .uri("/docs")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router");
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf-8");
    assert!(body.contains("/v1/openapi.json"));
}
