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

/// One chunk of a `text/event-stream` body, as text.
///
/// The stream never ends, so a chunk that does not arrive within ten seconds means
/// a daemon that is not saying anything — which is the failure these tests are
/// about, and better as a named panic than a hung suite.
async fn chunk(stream: &mut axum::body::BodyDataStream) -> String {
    use futures::StreamExt as _;

    let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .expect("the stream says something")
        .expect("the stream is open")
        .expect("bytes");
    String::from_utf8_lossy(&chunk).into_owned()
}

/// Read frames until what has arrived contains `wanted`.
///
/// Returns everything read, because the assertion that follows is usually about a
/// frame *and* what was in it.
async fn until(stream: &mut axum::body::BodyDataStream, wanted: &str) -> String {
    let mut seen = String::new();
    while !seen.contains(wanted) {
        seen.push_str(&chunk(stream).await);
    }
    seen
}

/// A daemon serving a project whose `.acpilot/config.toml` says `theme_web` is `theme`.
fn serving(theme: &str) -> (tempfile::TempDir, std::path::PathBuf, AppState) {
    let directory = tempfile::tempdir().expect("temp dir");
    let config = directory.path().join(".acpilot");
    std::fs::create_dir_all(&config).expect("config dir");
    let settings = config.join("config.toml");
    std::fs::write(
        &settings,
        format!("[preferences]\ntheme_web = \"{theme}\"\n"),
    )
    .expect("settings");
    let state = acpilotd::state_for(None, directory.path()).expect("state");
    (directory, settings, state)
}

async fn stream_of(state: AppState) -> axum::body::BodyDataStream {
    let response = router_with(state)
        .oneshot(
            Request::builder()
                .uri("/v1/events")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router");
    response.into_body().into_data_stream()
}

#[tokio::test]
async fn the_event_stream_opens_with_the_preferences_the_daemon_would_apply() {
    // Preferences are state, not history: a client that connects should be told
    // what the daemon would draw with instead of waiting for a save that may never
    // come. This is that first frame.
    let (_directory, _settings, state) = serving("acpilot.paper");
    let mut stream = stream_of(state).await;

    let frame = chunk(&mut stream).await;
    assert!(frame.contains("event: prefs"), "{frame}");
    assert!(frame.contains("acpilot.paper"), "{frame}");
}

#[tokio::test]
async fn a_saved_settings_file_reaches_a_client_over_the_event_stream() {
    // The case the event exists for: a client that cannot see the daemon's disk.
    // The file is saved while the stream is open, so the only way the new theme can
    // arrive is the watcher, the announcement and the frame.
    let (_directory, settings, state) = serving("acpilot.paper");
    state.watch_preferences();
    let mut stream = stream_of(state).await;

    let first = until(&mut stream, "acpilot.paper").await;
    assert!(first.contains("event: prefs"), "{first}");

    std::fs::write(
        &settings,
        "[preferences]\ntheme_web = \"acpilot.night-flight\"\n",
    )
    .expect("save");

    let announced = until(&mut stream, "acpilot.night-flight").await;
    assert!(announced.contains("event: prefs"), "{announced}");
}

#[tokio::test]
async fn a_settings_file_that_does_not_parse_keeps_the_last_good_preferences() {
    // A half-written file must not become the palette a client draws with. It
    // resolves to nothing, so the watcher announces nothing — and the next good
    // save still arrives, which is what a stream that died on a typo would not do.
    let (_directory, settings, state) = serving("acpilot.paper");
    state.watch_preferences();
    let mut stream = stream_of(state.clone()).await;
    let first = until(&mut stream, "acpilot.paper").await;
    assert!(first.contains("event: prefs"), "{first}");

    std::fs::write(&settings, "[preferences\ntheme_web = 3\n").expect("a broken save");
    assert!(
        state.preferences().is_none(),
        "a file that does not parse is not a set of preferences"
    );

    std::fs::write(
        &settings,
        "[preferences]\ntheme_web = \"acpilot.night-flight\"\n",
    )
    .expect("a good save after it");
    let announced = until(&mut stream, "acpilot.night-flight").await;
    assert!(announced.contains("event: prefs"), "{announced}");
}

/// The body of `uri`, as text.
async fn body_of(state: AppState, uri: &str) -> String {
    let response = router_with(state)
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router");
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf-8")
}

/// The stream the web page listens on.
async fn palette_stream_of(state: AppState) -> axum::body::BodyDataStream {
    let response = router_with(state)
        .oneshot(
            Request::builder()
                .uri("/app/palette/events")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router");
    assert_eq!(response.status(), StatusCode::OK);
    // A page that connected to something other than a stream would sit with the
    // theme its HTML was built with until it was reloaded, which is the bug.
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream"),
        "the page is listening for events"
    );
    response.into_body().into_data_stream()
}

/// The JSON in the first `palette` frame of a stream that has just opened.
async fn first_palette(stream: &mut axum::body::BodyDataStream) -> serde_json::Value {
    let frame = chunk(stream).await;
    assert!(frame.contains("event: palette"), "{frame}");
    let data = frame
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap_or_else(|| panic!("a data line in {frame}"));
    serde_json::from_str(data).expect("the frame is JSON")
}

#[tokio::test]
async fn the_palette_stream_opens_with_the_colours_the_page_was_served_with() {
    // The page applies these to its own `:root`. If the stream and the HTML ever
    // disagreed, every load would flash one palette and settle on another.
    let (_directory, _settings, state) = serving("acpilot.paper");
    let html = body_of(state.clone(), "/app").await;
    let mut stream = palette_stream_of(state).await;
    let palette = first_palette(&mut stream).await;

    assert_eq!(palette["color_scheme"], "light");
    let bg = palette["vars"]["--bg"].as_str().expect("a background");
    assert!(
        html.contains(&format!("--bg: {bg}")),
        "the served page says `--bg: {bg}` too"
    );
    assert!(
        !html.contains("--bg: #0b1220"),
        "styled with the theme, not the page's own dark palette"
    );
}

#[tokio::test]
async fn a_saved_theme_repaints_the_page_without_a_reload() {
    // The point of the stream: the browser holds no token and can see no files, and
    // saving a theme from the TUI or the desktop app has to reach it anyway.
    let (_directory, settings, state) = serving("acpilot.paper");
    state.watch_preferences();
    let mut stream = palette_stream_of(state).await;

    let first = first_palette(&mut stream).await;
    assert_eq!(first["color_scheme"], "light");

    std::fs::write(
        &settings,
        "[preferences]\ntheme_web = \"acpilot.night-flight\"\n",
    )
    .expect("save");

    let announced = first_palette(&mut stream).await;
    assert_eq!(announced["color_scheme"], "dark");
    assert_eq!(
        announced["vars"]["--bg"], "#070d18",
        "the colour the page applies, not just an announcement"
    );
}

#[tokio::test]
async fn a_workspace_with_no_settings_still_answers_the_stream() {
    // A daemon nobody has saved preferences for has nothing to send, and a page
    // left waiting on a stream that never opens is worse than one with no stream
    // at all: the connection is the answer, the frames are the extras.
    let state = acpilotd::state_for(None, std::path::Path::new(".")).expect("state");
    if state.web_palette().is_some() {
        // A machine with a global settings file has preferences; the stream is
        // then exercised by the tests above. This one is about their absence.
        return;
    }
    let _stream = palette_stream_of(state).await;
}
