//! Declaring a provider: what the route keeps, and what it asks the server.
//!
//! The probe is the interesting half, so the reachable rows here talk to a real
//! HTTP server on a loopback port serving LM Studio's `/api/v0/models`. A stub
//! returning a `Value` would test the mapping and skip the part this route adds:
//! that the answer arrives, is counted, and is written into the row.

use acpilot_profile::providers::{DeclaredProvider, ProvidersFile};
use acpilot_proto::model::ProviderKind;
use acpilotd::{AppState, router_with};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use axum::{Json, Router as AxumRouter};
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

fn app() -> Router {
    router_with(AppState::new())
}

/// A daemon that reads and writes the declarations in `path`.
fn app_with_file(path: &std::path::Path) -> Router {
    let providers = acpilotd::providers::Providers::load(path.to_path_buf())
        .expect("a fresh declaration loads");
    router_with(AppState::new().with_providers(providers))
}

fn declaration(id: &str, enabled: bool) -> DeclaredProvider {
    DeclaredProvider {
        id: id.to_owned(),
        kind: ProviderKind::LmStudio,
        label: Some("LM Studio".to_owned()),
        base_url: "http://127.0.0.1:1".to_owned(),
        credential_ref: None,
        enabled,
    }
}

/// A model server on an ephemeral loopback port, answering `models` as LM Studio does.
async fn serve_models(rows: Value) -> String {
    let app = AxumRouter::new().route(
        "/api/v0/models",
        get(move || {
            let rows = rows.clone();
            async move { Json(json!({ "object": "list", "data": rows })) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("a bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

#[tokio::test]
async fn a_reachable_provider_is_listed_with_what_the_server_said() {
    let base_url = serve_models(json!([
        { "id": "qwen2-7b-instruct", "type": "llm" },
        { "id": "text-embedding-nomic", "type": "embeddings" }
    ]))
    .await;
    let application = app();

    let (status, added) = send(
        &application,
        "POST",
        "/v1/providers",
        Some(json!({
            "id": "studio",
            "kind": "lm_studio",
            "label": "LM Studio",
            "base_url": base_url,
            "enabled": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["id"], "studio");
    assert_eq!(added["status"], "reachable");
    assert_eq!(added["model_count"], 2, "the count is what discovery saw");
    assert!(
        added["last_probed_at"].is_string(),
        "a row that was answered says when: {added}"
    );
    assert_eq!(
        added["credential_ref"],
        Value::Null,
        "a provider with no token has no keyring entry to name"
    );

    let (status, listed) = send(&application, "GET", "/v1/providers", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = listed.as_array().expect("a list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "studio");
    assert_eq!(rows[0]["base_url"], base_url);
}

#[tokio::test]
async fn an_unreachable_provider_is_a_row_that_says_why() {
    // Nothing listens on port 1, so the connection is refused at once.
    let application = app();
    let (status, added) = send(
        &application,
        "POST",
        "/v1/providers",
        Some(json!({
            "id": "absent",
            "kind": "open_ai_compatible",
            "label": "not running",
            "base_url": "http://127.0.0.1:1",
            "enabled": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["status"], "unreachable");
    assert_eq!(added["model_count"], 0);
    assert!(
        added["detail"]
            .as_str()
            .is_some_and(|detail| !detail.is_empty()),
        "a refusal without a reason leaves nothing to fix: {added}"
    );

    let (_, listed) = send(&application, "GET", "/v1/providers", None).await;
    assert_eq!(
        listed.as_array().map(Vec::len),
        Some(1),
        "the provider exists even though the server did not answer"
    );
}

#[tokio::test]
async fn a_disabled_provider_is_never_contacted() {
    let application = app();
    let (status, added) = send(
        &application,
        "POST",
        "/v1/providers",
        Some(json!({
            "id": "later",
            "kind": "open_ai_compatible",
            "label": "later",
            "base_url": "http://127.0.0.1:1",
            "enabled": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["status"], "disabled");
    assert_eq!(added["last_probed_at"], Value::Null);
}

#[tokio::test]
async fn one_id_is_one_provider() {
    let application = app();
    let body = json!({
        "id": "studio",
        "kind": "open_ai_compatible",
        "label": "studio",
        "base_url": "http://127.0.0.1:1",
        "enabled": false
    });
    let (status, _) = send(&application, "POST", "/v1/providers", Some(body.clone())).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, refused) = send(&application, "POST", "/v1/providers", Some(body)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "conflict");

    let (_, listed) = send(&application, "GET", "/v1/providers", None).await;
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn a_url_without_a_scheme_is_a_400_and_not_a_guess() {
    let application = app();
    let (status, refused) = send(
        &application,
        "POST",
        "/v1/providers",
        Some(json!({
            "id": "studio",
            "kind": "lm_studio",
            "label": "studio",
            "base_url": "127.0.0.1:1234",
            "enabled": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["code"], "invalid_request");
    assert_eq!(
        refused["details"][0]["field"], "base_url",
        "the refusal names the field to fix: {refused}"
    );

    let (_, listed) = send(&application, "GET", "/v1/providers", None).await;
    assert_eq!(
        listed.as_array().map(Vec::len),
        Some(0),
        "a refused request declares nothing"
    );
}

#[tokio::test]
async fn an_added_provider_is_written_to_the_profile() {
    let directory = tempfile::tempdir().expect("temp dir");
    let file = ProvidersFile::path_in(directory.path());
    let application = app_with_file(&file);

    let (status, _) = send(
        &application,
        "POST",
        "/v1/providers",
        Some(json!({
            "id": "later",
            "kind": "open_ai_compatible",
            "label": "later",
            "base_url": "http://127.0.0.1:1",
            "enabled": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let text = std::fs::read_to_string(&file).expect("the declaration was written");
    assert!(text.contains("schema_version = 1"), "{text}");
    assert!(text.contains("[[providers]]"), "{text}");
    assert!(text.contains("id = \"later\""), "{text}");
    // A credential is a reference even on disk: the token never enters the file.
    assert!(!text.contains("token"), "{text}");
}

#[tokio::test]
async fn a_restart_reads_the_declaration_back_and_claims_nothing_more() {
    let directory = tempfile::tempdir().expect("temp dir");
    let file = ProvidersFile::path_in(directory.path());
    let mut declarations = ProvidersFile::empty();
    declarations.providers.push(declaration("studio", true));
    declarations.providers.push(declaration("off", false));
    declarations.save(&file).expect("save");

    let application = app_with_file(&file);
    let (status, listed) = send(&application, "GET", "/v1/providers", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = listed.as_array().expect("a list");
    assert_eq!(rows.len(), 2, "both declarations came back");
    assert_eq!(rows[0]["id"], "studio");
    assert_eq!(rows[0]["label"], "LM Studio");
    assert_eq!(
        rows[0]["status"], "not_probed",
        "a restart tried nothing, so nothing failed"
    );
    assert_eq!(rows[0]["model_count"], 0);
    assert_eq!(rows[0]["last_probed_at"], Value::Null);
    assert!(
        rows[0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("acpilot models probe studio")),
        "the row says how to ask: {listed}"
    );
    assert_eq!(rows[1]["status"], "disabled");
}
