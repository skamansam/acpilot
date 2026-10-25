//! The HTTP contract, exercised through the router itself rather than a live port.

use std::sync::Arc;

use acpilot_agent::{MapResolver, Registry};
use acpilot_core::AcpilotError;
use acpilotd::{ApiError, AppState, router, router_with};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn get(uri: &str) -> (StatusCode, serde_json::Value) {
    get_from(router(), uri).await
}

async fn get_from(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = app
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
async fn a_daemon_without_a_profile_still_names_the_workspace_it_serves() {
    // The client that spawns a daemon names a directory even when it has no
    // profile, and uses `health.workspace` to check that the daemon it found is
    // the one for that directory (ADR 0004 §2). Dropping the field unless a
    // profile came along would make that check silently pass.
    let directory = tempfile::tempdir().expect("temp dir");
    let name = directory
        .path()
        .canonicalize()
        .expect("resolved")
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a name")
        .to_owned();

    let state = acpilotd::state_for(None, directory.path()).expect("state");
    let (status, body) = get_from(router_with(state), "/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["workspace"], name);
}

#[tokio::test]
async fn health_is_json_and_reports_the_build_version() {
    let response = router()
        .oneshot(
            Request::builder()
                .uri("/v1/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type"),
        Some(&"application/json".parse().expect("header value")),
        "clients parse this without sniffing"
    );

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    // The payload is exactly these four keys; a client may read them by name.
    let object = body.as_object().expect("health is an object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["api", "ok", "service", "version"]);
}

#[tokio::test]
async fn wrong_methods_and_routes_do_not_fall_through_to_the_api() {
    let response = router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

    let (status, _) = get("/v1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = get("/").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn openapi_is_served_and_valid() {
    let (status, body) = get("/v1/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["openapi"], "3.1.0");
    assert_eq!(body["info"]["title"], "Acpilot API");
    // The daemon serves one path the proto document cannot name: the route it
    // implements itself. A client generated from this document must see it.
    assert_eq!(
        body["paths"]["/v1/agents"]["get"]["responses"]["200"]["description"],
        "Every configured agent, with its availability on this host"
    );
}

#[tokio::test]
async fn unknown_route_is_a_404_with_no_surprises() {
    let (status, _) = get("/v1/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn errors_use_the_shared_envelope() {
    let response = ApiError::new(AcpilotError::denied("write outside workspace")).into_response();
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

fn agents_state(manifest: &str) -> AppState {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("agents.toml");
    std::fs::write(&path, manifest).expect("write manifest");
    let registry = Registry::load(path, Arc::new(MapResolver::new())).expect("registry loads");
    AppState::new().with_registry(Arc::new(registry))
}

#[tokio::test]
async fn agents_without_a_profile_is_an_empty_list_not_an_error() {
    let (status, body) = get("/v1/agents").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]));
}

#[tokio::test]
async fn agents_are_served_from_the_registry_with_their_availability() {
    // An executable that certainly exists on this machine, and one that certainly
    // does not. Using the test binary keeps the test free of platform commands.
    let present = std::env::current_exe().expect("current test binary");
    let manifest = format!(
        "schema_version = 1\n\
         [[agents]]\nid = \"present\"\nname = \"Present\"\ntransport = \"acp-stdio\"\n\
         command = {present:?}\n\
         [[agents]]\nid = \"absent\"\ntransport = \"acp-stdio\"\n\
         command = \"acpilot-no-such-program-anywhere\"\n"
    );
    let state = agents_state(&manifest);

    let (status, body) = get_from(router_with(state), "/v1/agents").await;
    assert_eq!(status, StatusCode::OK);
    let agents = body.as_array().expect("a list");
    assert_eq!(agents.len(), 2, "declaration order is preserved");

    assert_eq!(agents[0]["id"], "present");
    assert_eq!(agents[0]["name"], "Present");
    assert_eq!(agents[0]["transport"], "acp_stdio");
    assert_eq!(agents[0]["status"], "available");
    assert!(
        agents[0]["command"]
            .as_str()
            .expect("command")
            .contains(&present.display().to_string()),
        "the API reports what will be spawned"
    );
    // No handshake has happened, so there is no protocol version to claim.
    assert!(agents[0].get("protocol_version").is_none());

    assert_eq!(agents[1]["id"], "absent");
    assert_eq!(
        agents[1]["status"], "missing",
        "a command that is not on PATH is the one status a list can prove"
    );
}

/// The router under a real listener, not `oneshot`.
///
/// `oneshot` proves the handler; this proves the server, the bind, and the HTTP
/// client all agree — the difference between a unit-tested route and `acpilot
/// serve` working on a laptop.
#[tokio::test]
async fn the_api_answers_over_a_real_socket() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        axum::serve(listener, router()).await.expect("server runs");
    });

    let base = format!("http://{address}");
    let health: serde_json::Value = reqwest::get(format!("{base}/v1/health"))
        .await
        .expect("health request")
        .json()
        .await
        .expect("health json");
    assert_eq!(health["ok"], true);

    let openapi = reqwest::get(format!("{base}/v1/openapi.json"))
        .await
        .expect("openapi request");
    assert_eq!(openapi.status(), 200);
    assert!(openapi.url().path().ends_with("/v1/openapi.json"));

    let missing = reqwest::get(format!("{base}/v1/does-not-exist"))
        .await
        .expect("missing request");
    assert_eq!(missing.status(), 404);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn a_profile_registry_is_served_over_a_real_socket() {
    let present = std::env::current_exe().expect("current test binary");
    let manifest = format!(
        "schema_version = 1\n\
         [[agents]]\nid = \"present\"\nname = \"Present\"\ntransport = \"acp-stdio\"\n\
         command = {present:?}\n\
         [[agents]]\nid = \"absent\"\ntransport = \"acp-stdio\"\n\
         command = \"acpilot-no-such-program-anywhere\"\n"
    );
    let state = agents_state(&manifest);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        axum::serve(listener, router_with(state))
            .await
            .expect("server runs");
    });

    // A client, not a `oneshot`: the daemon's JSON, the HTTP layer and the
    // availability check have to agree over the wire the CLI will use.
    let body: serde_json::Value = reqwest::get(format!("http://{address}/v1/agents"))
        .await
        .expect("agents request")
        .json()
        .await
        .expect("agents json");
    let agents = body.as_array().expect("a list");
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0]["id"], "present");
    assert_eq!(agents[0]["status"], "available");
    assert_eq!(agents[1]["id"], "absent");
    assert_eq!(agents[1]["status"], "missing");

    server.abort();
    let _ = server.await;
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
