//! The listener a client runs in its own process (P6-11).
//!
//! The point of embedded mode is that it is not a second API: the test talks to it
//! with the same `acpilot_client::Client` the TUI uses, and expects the same
//! answers a daemon process would give — including the refusal when the token is
//! missing, because a loopback port inherits none of a unix socket's filesystem
//! access control.

use std::time::Duration;

use acpilotd::embedded::Embedded;

/// A state with nothing in it: these tests are about the listener, not a profile.
fn state(directory: &std::path::Path) -> acpilotd::AppState {
    acpilotd::AppState::new()
        .with_workspace(acpilotd::workspace_at(directory).expect("a workspace"))
}

#[tokio::test]
async fn an_embedded_listener_answers_the_client_a_daemon_would() {
    let workspace = tempfile::tempdir().expect("a workspace");
    let embedded = Embedded::start(state(workspace.path())).expect("a listener");

    let client = acpilot_client::Client::with_token(embedded.endpoint(), embedded.token());
    let health = client.health().await.expect("health");
    assert_eq!(health.service, "acpilotd");
    assert_eq!(health.api, acpilot_proto::API_PREFIX);
    // The same version constant the binary reports, so a client and the daemon it
    // embedded cannot disagree about the protocol.
    assert_eq!(health.version, acpilot_core::VERSION);

    // A port has no filesystem boundary to inherit, so the generated token is the
    // only thing standing between it and anything else on the machine.
    let anonymous = acpilot_client::Client::new(embedded.endpoint());
    let error = anonymous.health().await.expect_err("a token is required");
    assert_eq!(error.code(), Some("unauthorized"));
}

#[tokio::test]
async fn dropping_the_listener_stops_the_port() {
    let workspace = tempfile::tempdir().expect("a workspace");
    let embedded = Embedded::start(state(workspace.path())).expect("a listener");
    let endpoint = embedded.endpoint().to_owned();
    let client = acpilot_client::Client::with_token(&endpoint, embedded.token());
    client.health().await.expect("the listener is up");

    drop(embedded);

    // Polled rather than awaited: the listener stops accepting as soon as it is
    // told, and the thread that served it ends when the process does. A raw
    // connection is used rather than the client, because the client's pool would
    // keep answering over the connection it already has.
    let address = endpoint.trim_start_matches("http://").to_owned();
    let mut refused = false;
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(&address).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        refused,
        "the port still accepted after the listener was dropped"
    );
}
