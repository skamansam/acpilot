//! The tower layers: compression where it helps, CORS only when asked.
//!
//! Both are easy to add and easy to get wrong in a way no route test notices. A
//! `CompressionLayer` over the session stream buffers a transcript until the
//! compressor is happy, so the events arrive in bursts instead of as they happen —
//! the stream still "works" in a test that only reads the first frame. And a
//! permissive `CorsLayer` on a daemon that guards a TCP listener with a token lets a
//! page from any origin read every response the token unlocks, which is a decision,
//! not a default.

use acpilotd::{AppState, router_with};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

fn request(uri: &str, origin: bool) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(uri)
        .header(header::ACCEPT_ENCODING, "gzip");
    if origin {
        builder = builder.header(header::ORIGIN, "http://localhost:5173");
    }
    builder.body(Body::empty()).expect("request")
}

#[tokio::test]
async fn json_is_compressed_when_the_client_asks_for_it() {
    let response = router_with(AppState::new())
        .oneshot(request("/v1/openapi.json", false))
        .await
        .expect("router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok()),
        Some("gzip"),
        "the contract is large enough that compressing it is a win"
    );
}

#[tokio::test]
async fn a_small_body_is_sent_as_it_is() {
    let response = router_with(AppState::new())
        .oneshot(request("/v1/health", false))
        .await
        .expect("router is infallible");
    assert!(
        response.headers().get(header::CONTENT_ENCODING).is_none(),
        "the framing costs more than the bytes at this size"
    );
}

#[tokio::test]
async fn the_event_stream_is_never_compressed() {
    // A daemon with no broker keeps the stream open and quiet, which is the state
    // that would be buffered forever by a compressor.
    let response = router_with(AppState::new())
        .oneshot(request("/v1/events", false))
        .await
        .expect("router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert!(
        response.headers().get(header::CONTENT_ENCODING).is_none(),
        "a compressed stream is a stream that arrives when the compressor says so"
    );
}

#[tokio::test]
async fn cors_is_off_unless_the_daemon_was_told_the_browser_is_elsewhere() {
    let refused = router_with(AppState::new())
        .oneshot(request("/v1/health", true))
        .await
        .expect("router is infallible");
    assert!(
        refused
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "same origin is the normal case"
    );

    let allowed = router_with(AppState::new().with_cors(true))
        .oneshot(request("/v1/health", true))
        .await
        .expect("router is infallible");
    assert_eq!(
        allowed
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok()),
        Some("*")
    );
}
