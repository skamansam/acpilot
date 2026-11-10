//! Several profiles in one daemon: what `GET /v1/profiles` lists, which profile
//! a request is dispatched to, and the two refusals.
//!
//! The states are built directly rather than through `discover`, because discovery
//! reads every profile under the machine's config root and attaches its database
//! path to a shared store cache — a test that used it would be testing the home
//! directory as much as the daemon. The routing is the same either way:
//! `from_states` is what `discover` calls once the states exist.

use std::sync::Arc;

use acpilot_store::Index;
use acpilotd::profiles::{self, Profiles};
use acpilotd::state_for_profile;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A profile directory the daemon can open, with one agent in it.
fn profile(root: &std::path::Path, name: &str, agent: &str) -> std::path::PathBuf {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).expect("profile directory");
    std::fs::write(directory.join("config.toml"), "schema_version = 1\n").expect("config.toml");
    let command = std::env::current_exe().expect("the test's own path");
    std::fs::write(
        directory.join("agents.toml"),
        format!(
            "schema_version = 1\n\n[[agents]]\nid = \"{agent}\"\nname = \"{agent}\"\n\
             transport = \"acp-stdio\"\ncommand = {command:?}\n"
        ),
    )
    .expect("agents.toml");
    directory
}

/// `GET`/`POST` one route, optionally naming a profile.
async fn send(
    app: Router,
    method: &str,
    uri: &str,
    profile: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    send_json(app, method, uri, profile, None).await
}

/// [`send`] with a JSON body.
async fn send_json(
    app: Router,
    method: &str,
    uri: &str,
    profile: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(name) = profile {
        request = request.header("X-Acpilot-Profile", name);
    }
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .oneshot(request.body(body).expect("request"))
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
async fn a_header_picks_the_profile_and_a_mutation_without_one_is_refused() {
    let root = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("workspace");
    let work = profile(root.path(), "work", "work-agent");
    let personal = profile(root.path(), "personal", "personal-agent");

    let work_state = state_for_profile(&work, workspace.path()).expect("work loads");
    let personal_state = state_for_profile(&personal, workspace.path()).expect("personal loads");
    let registry = Arc::new(Profiles::from_states(
        vec![
            ("work".to_owned(), work_state),
            ("personal".to_owned(), personal_state),
        ],
        "work".to_owned(),
    ));
    let app = profiles::router(Arc::clone(&registry));

    // The list is what the page's switcher draws, and the active row is the one a
    // headerless read is about.
    let (status, body) = send(app.clone(), "GET", "/v1/profiles", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().expect("rows").len(), 2);
    let work_row = body
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["id"] == "work")
        .expect("work is listed");
    assert_eq!(work_row["active"], true);
    assert_eq!(work_row["agent_count"], 1);

    // The header is the identity: the same route answers with a different state.
    let (status, body) = send(app.clone(), "GET", "/v1/agents", Some("personal")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["id"], "personal-agent");

    let (status, body) = send(app.clone(), "GET", "/v1/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body[0]["id"], "work-agent",
        "no header means the active profile"
    );

    // A mutation has to name its identity: guessing is how a prompt goes out
    // under the wrong key.
    let (status, body) = send(app.clone(), "POST", "/v1/sessions", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
    assert!(
        body["message"]
            .as_str()
            .expect("a message")
            .contains("X-Acpilot-Profile"),
        "{body}"
    );

    // A name this daemon does not serve is a 404 that says what it does serve,
    // rather than silently using the active profile.
    let (status, body) = send(app.clone(), "GET", "/v1/agents", Some("ghost")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let message = body["message"].as_str().expect("a message");
    assert!(
        message.contains("work") && message.contains("personal"),
        "the refusal names what it serves: {body}"
    );

    // Activation changes the default for headerless reads; streams bind at
    // connect, so they follow on their next connection rather than by a frame.
    let (status, body) = send(app.clone(), "POST", "/v1/profiles/personal/activate", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], true);

    let (status, body) = send(app, "GET", "/v1/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["profile"], "personal");

    assert_eq!(registry.active(), "personal");
}

#[tokio::test]
async fn opening_a_workspace_another_profile_owns_is_a_profile_mismatch() {
    let root = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("workspace");
    let work = profile(root.path(), "work", "work-agent");
    let personal = profile(root.path(), "personal", "personal-agent");
    let index = Index::in_memory().expect("an owner index");
    let work_state = state_for_profile(&work, workspace.path())
        .expect("work loads")
        .with_index(index.clone());
    let personal_state = state_for_profile(&personal, workspace.path())
        .expect("personal loads")
        .with_index(index.clone());
    let registry = Arc::new(Profiles::from_states(
        vec![
            ("work".to_owned(), work_state),
            ("personal".to_owned(), personal_state),
        ],
        "work".to_owned(),
    ));
    let app = profiles::router(registry);
    let body = serde_json::json!({
        "host_id": "local",
        "root": workspace.path().to_string_lossy(),
    });

    let (status, opened) = send_json(
        app.clone(),
        "POST",
        "/v1/workspaces",
        Some("work"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    assert_eq!(opened["profile"], "work");
    assert_eq!(
        opened["id"],
        format!("work:local:{}", workspace.path().display())
    );

    let (status, refused) = send_json(
        app.clone(),
        "POST",
        "/v1/workspaces",
        Some("personal"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "profile_mismatch");
    assert_eq!(refused["details"][0]["field"], "owner");
    assert_eq!(refused["details"][0]["message"], "work");

    let mut stolen = body;
    stolen["steal"] = serde_json::Value::Bool(true);
    let (status, opened) = send_json(
        app,
        "POST",
        "/v1/workspaces",
        Some("personal"),
        Some(stolen),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    assert_eq!(opened["profile"], "personal");
    assert_eq!(
        index
            .workspace_owner(&format!("local:{}", workspace.path().display()))
            .expect("owner"),
        Some("personal".to_owned()),
        "an explicit steal moves the index row"
    );
}

#[tokio::test]
async fn a_profile_that_cannot_load_does_not_take_the_others_with_it() {
    let root = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("workspace");
    let good = profile(root.path(), "good", "good-agent");
    // A config that does not parse: `Profile::open` reads it, so this directory
    // is a name the daemon cannot serve rather than one that answers empty.
    let broken = root.path().join("broken");
    std::fs::create_dir_all(&broken).expect("broken directory");
    std::fs::write(broken.join("config.toml"), "[policy\n").expect("broken config");

    let good_state = state_for_profile(&good, workspace.path()).expect("the good profile loads");
    let registry = Arc::new(Profiles::from_states(
        vec![("good".to_owned(), good_state)],
        "good".to_owned(),
    ));
    let app = profiles::router(registry);

    let (status, body) = send(app, "GET", "/v1/profiles", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().expect("rows").len(), 1);
    assert_eq!(body[0]["id"], "good");
}
