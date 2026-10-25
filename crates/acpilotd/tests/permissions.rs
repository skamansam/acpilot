//! The permission routes, driven through the router and a real broker.
//!
//! The broker is the same type a session asks: the test raises a request the way
//! an agent does — by calling `ask` and leaving the task waiting — and then plays
//! the client, listing it and resolving it over HTTP. Nothing here stubs the
//! decision; the assertions are on what the broker recorded, so a route that only
//! looked like it answered would fail.

use std::sync::Arc;
use std::time::Duration;

use acpilot_agent::{Answer, Broker, DecidedBy, PermissionKind, PermissionOption, Policy, Subject};
use acpilot_core::AcpilotError;
use acpilot_proto::permission::PermissionRequest;
use acpilotd::{AppState, state_for_profile};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tower::ServiceExt;

const PATIENCE: Duration = Duration::from_secs(5);

fn broker(policy: Policy, workspace: &std::path::Path) -> Arc<Broker> {
    Arc::new(Broker::with_timeout(
        policy,
        std::fs::canonicalize(workspace).expect("the workspace resolves"),
        None,
    ))
}

fn app(broker: &Arc<Broker>) -> Router {
    acpilotd::router_with(AppState::new().with_broker(Arc::clone(broker)))
}

async fn get(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    read(response).await
}

async fn resolve(
    app: Router,
    id: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/permissions/{id}/resolve"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    read(response).await
}

async fn read(response: axum::response::Response) -> (StatusCode, serde_json::Value) {
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

/// Raise a request the way an agent does.
///
/// Returns the pending request and the task that is blocked on the answer. The
/// task is kept because the broker records the decision on its side of the wait:
/// a test that asserted the audit log before joining it would be racing the
/// thing it is testing.
async fn raise(
    broker: &Arc<Broker>,
    session: &str,
    title: &str,
) -> (PermissionRequest, JoinHandle<Answer>) {
    let mut announcement = broker.subscribe();
    let asker = Arc::clone(broker);
    let session = session.to_owned();
    let title = title.to_owned();
    let waiting = tokio::spawn(async move {
        asker
            .ask(&session, Subject::new(PermissionKind::Terminal, title))
            .await
    });
    let request = timeout(PATIENCE, announcement.recv())
        .await
        .expect("a request is announced")
        .expect("the stream is open");
    (request, waiting)
}

#[tokio::test]
async fn the_pending_list_is_what_a_client_needs_to_answer() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());
    let (request, _waiting) = raise(&broker, "session-1", "cargo fmt").await;

    let (status, body) = get(app(&broker), "/v1/permissions?status=pending").await;
    assert_eq!(status, StatusCode::OK);
    let pending = body.as_array().expect("a list");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["id"], request.id.as_str());
    assert_eq!(pending[0]["session_id"], "session-1");
    assert_eq!(pending[0]["kind"], "terminal");
    assert_eq!(pending[0]["title"], "cargo fmt");
    assert_eq!(
        pending[0]["options"],
        serde_json::json!(["allow_once", "allow_always", "deny"]),
        "the client picks from the object it listed, not from a guess"
    );
    assert!(
        pending[0]["created_at"]
            .as_str()
            .is_some_and(|at| at.contains('T')),
        "a timestamp a client can parse: {}",
        pending[0]["created_at"]
    );
}

#[tokio::test]
async fn a_resolve_answers_the_broker_as_the_user() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());
    let (request, waiting) = raise(&broker, "session-1", "cargo fmt").await;
    let app = app(&broker);

    let (status, decision) = resolve(
        app.clone(),
        &request.id,
        serde_json::json!({ "option": "allow_once" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decision["option"], "allow_once");
    assert_eq!(decision["decided_by"], "user");
    assert!(decision["at"].as_str().is_some());

    // The agent's side of the wait got the answer, not a refusal.
    let answer = waiting.await.expect("the ask finishes");
    assert!(answer.allowed());
    assert_eq!(answer.decided_by, DecidedBy::User);

    assert!(broker.pending().is_empty(), "the request was delivered");
    let audit = broker.audit();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].option, PermissionOption::AllowOnce);
    assert_eq!(audit[0].decided_by, DecidedBy::User);

    // And it is no longer listed: pending is state, not history.
    let (status, body) = get(app, "/v1/permissions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]));
}

#[tokio::test]
async fn resolving_twice_with_the_same_option_is_idempotent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());
    let (request, _waiting) = raise(&broker, "session-1", "cargo fmt").await;
    let app = app(&broker);

    let (first_status, first) = resolve(
        app.clone(),
        &request.id,
        serde_json::json!({ "option": "deny" }),
    )
    .await;
    let (second_status, second) =
        resolve(app, &request.id, serde_json::json!({ "option": "deny" })).await;

    assert_eq!(first_status, StatusCode::OK);
    assert_eq!(second_status, StatusCode::OK, "a retry is not an error");
    assert_eq!(first, second, "the same answer, including its timestamp");
}

#[tokio::test]
async fn a_different_decision_after_the_first_is_a_conflict() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());
    let (request, _waiting) = raise(&broker, "session-1", "cargo fmt").await;
    let app = app(&broker);

    let _ = resolve(
        app.clone(),
        &request.id,
        serde_json::json!({ "option": "allow_once" }),
    )
    .await;
    let (status, body) = resolve(app, &request.id, serde_json::json!({ "option": "deny" })).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "conflict");
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("already decided"),
        "the message says why retrying cannot work: {body}"
    );
}

#[tokio::test]
async fn an_unknown_request_is_a_404() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());

    let (status, body) = resolve(
        app(&broker),
        "perm-9999",
        serde_json::json!({ "option": "allow_once" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn an_option_this_build_does_not_accept_is_a_400_listing_the_ones_it_does() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());
    let (request, _waiting) = raise(&broker, "session-1", "cargo fmt").await;

    let (status, body) = resolve(
        app(&broker),
        &request.id,
        serde_json::json!({ "option": "yolo" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
    let details = body["details"].as_array().expect("field-level detail");
    assert_eq!(details[0]["field"], "option");
    assert!(
        details[0]["message"]
            .as_str()
            .expect("message")
            .contains("allow_once"),
        "the envelope names what would have worked: {body}"
    );
}

#[tokio::test]
async fn the_list_can_be_narrowed_to_one_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());
    raise(&broker, "session-1", "cargo fmt").await;
    raise(&broker, "session-2", "cargo test").await;
    let app = app(&broker);

    let (_, all) = get(app.clone(), "/v1/permissions").await;
    assert_eq!(all.as_array().expect("a list").len(), 2);

    let (status, only) = get(app, "/v1/permissions?session=session-2").await;
    assert_eq!(status, StatusCode::OK);
    let only = only.as_array().expect("a list");
    assert_eq!(only.len(), 1);
    assert_eq!(only[0]["session_id"], "session-2");
}

#[tokio::test]
async fn a_status_this_build_cannot_list_is_an_error_not_an_empty_list() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());

    let (status, body) = get(app(&broker), "/v1/permissions?status=resolved").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
}

#[tokio::test]
async fn a_daemon_without_a_broker_has_nothing_to_answer() {
    let (status, body) = get(acpilotd::router(), "/v1/permissions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]), "no profile is not an error");

    let (status, body) = resolve(
        acpilotd::router(),
        "perm-1",
        serde_json::json!({ "option": "deny" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn a_profiles_policy_reaches_the_running_broker() {
    let profile = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        profile.path().join("config.toml"),
        r#"
schema_version = 1

[policy]
default = "deny"

[[policy.rules]]
id = "allow-format"
decision = "allow"
tool = "execute"
"#,
    )
    .expect("write the profile");

    let state = state_for_profile(profile.path(), workspace.path()).expect("the profile loads");
    let broker = state.broker().expect("a profile means a broker");
    assert_eq!(
        broker.policy().rules().len(),
        1,
        "the rule from the file is in the running policy"
    );

    // The rule decides, and the decision names it: this is the whole point of
    // loading the file rather than shipping a default.
    let answer = broker
        .ask(
            "session-1",
            Subject::new(PermissionKind::Terminal, "cargo fmt").tool("execute"),
        )
        .await;
    assert!(answer.allowed());
    assert_eq!(
        answer.decided_by,
        DecidedBy::Policy("allow-format".to_owned())
    );

    // And the API serves that same broker: the rule already answered, so
    // nothing is pending and the list is empty rather than missing.
    let (status, body) = get(acpilotd::router_with(state), "/v1/permissions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]));
}

#[tokio::test]
async fn a_profiles_agents_are_served_beside_its_policy() {
    let profile = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        profile.path().join("config.toml"),
        "schema_version = 1\n\n[policy]\ndefault = \"deny\"\n",
    )
    .expect("write the profile");
    // An executable that certainly exists: the test binary, so availability is a
    // real `PATH` answer rather than a platform guess.
    let present = std::env::current_exe().expect("current test binary");
    std::fs::write(
        profile.path().join("agents.toml"),
        format!(
            "schema_version = 1\n\
             [[agents]]\nid = \"present\"\nname = \"Present\"\ntransport = \"acp-stdio\"\n\
             command = {present:?}\n"
        ),
    )
    .expect("write the agents");

    let state = state_for_profile(profile.path(), workspace.path()).expect("the profile loads");
    assert!(
        state.registry().is_some(),
        "an agents.toml in the profile is an agent list"
    );

    let (status, body) = get(acpilotd::router_with(state), "/v1/agents").await;
    assert_eq!(status, StatusCode::OK);
    let agents = body.as_array().expect("a list");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["id"], "present");
    assert_eq!(agents[0]["name"], "Present");
    assert_eq!(agents[0]["status"], "available");
}

#[test]
fn a_profile_without_an_agents_file_has_no_registry_rather_than_an_empty_one() {
    let profile = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("temp dir");
    std::fs::write(profile.path().join("config.toml"), "schema_version = 1\n")
        .expect("write the profile");

    let state = state_for_profile(profile.path(), workspace.path()).expect("the profile loads");
    assert!(
        state.registry().is_none(),
        "the daemon does not invent an agents.toml"
    );
    assert!(
        !profile.path().join("agents.toml").exists(),
        "opening a profile for serving does not write into it"
    );
}

#[test]
fn a_broken_agents_file_fails_the_startup_instead_of_hiding_the_profile() {
    let profile = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("temp dir");
    std::fs::write(profile.path().join("config.toml"), "schema_version = 1\n")
        .expect("write the profile");
    std::fs::write(
        profile.path().join("agents.toml"),
        "schema_version = 1\n[[agents]]\nid = \"ghost\"\ntransport = \"carrier-pigeon\"\n",
    )
    .expect("write the agents");

    let error = state_for_profile(profile.path(), workspace.path())
        .expect_err("an agent with no transport is not a profile the daemon can serve");
    assert!(
        matches!(error, acpilotd::StateError::Agents(_)),
        "the failure names the manifest, not the policy: {error:?}"
    );
    assert!(
        error.to_string().contains("agents.toml"),
        "the message says which file: {error}"
    );
}

#[tokio::test]
async fn a_profile_that_does_not_exist_fails_the_startup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let error =
        state_for_profile(dir.path(), dir.path()).expect_err("an empty directory is not it");
    assert!(
        error.to_string().contains("config.toml"),
        "the message names the file that is missing: {error}"
    );
}

#[tokio::test]
async fn openapi_documents_the_permission_routes() {
    let (status, body) = get(acpilotd::router(), "/v1/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["paths"]["/v1/permissions"]["get"]["responses"]["200"]["description"],
        "Every pending request, oldest first"
    );
    assert_eq!(
        body["paths"]["/v1/permissions/{id}/resolve"]["post"]["responses"]["200"]["description"],
        "The decision, recorded"
    );
    assert!(
        body["components"]["schemas"]["PermissionDecision"].is_object(),
        "a client generated from this document can name the answer type"
    );
}

#[tokio::test]
async fn a_client_answers_over_a_real_socket() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());
    let (request, waiting) = raise(&broker, "session-1", "cargo fmt").await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app(&broker))
            .await
            .expect("server runs");
    });
    let base = format!("http://{address}");

    let pending: serde_json::Value = reqwest::get(format!("{base}/v1/permissions?status=pending"))
        .await
        .expect("list request")
        .json()
        .await
        .expect("list json");
    assert_eq!(pending[0]["id"], request.id.as_str());

    let answer = reqwest::Client::new()
        .post(format!("{base}/v1/permissions/{}/resolve", request.id))
        .json(&serde_json::json!({ "option": "allow_once" }))
        .send()
        .await
        .expect("resolve request");
    assert_eq!(answer.status(), 200);
    let decision: serde_json::Value = answer.json().await.expect("decision json");
    assert_eq!(decision["decided_by"], "user");
    assert!(waiting.await.expect("the ask finishes").allowed());

    server.abort();
    let _ = server.await;
}

#[test]
fn a_bad_option_is_a_400_and_not_a_500() {
    // The mapping, not the route: `invalid` is the class the routes raise for a
    // request they cannot use, and it must not read as a daemon failure.
    assert_eq!(
        acpilotd::ApiError::new(AcpilotError::invalid("`option` is missing")).status(),
        StatusCode::BAD_REQUEST
    );
}
