//! The daemon on its default transport: a unix socket a client can reach.
//!
//! The point is not that HTTP works — `http.rs` covers that over TCP. It is that
//! the default is a socket, that it is private while it exists, and that a socket
//! left by a live daemon is refused rather than replaced.
//!
//! unix only: the transport does not exist on Windows yet (ADR 0004 §3 names a
//! named pipe, P10-5), and the platform file there says so.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use acpilotd::{AppState, Target, serve};
use tokio::sync::oneshot;

/// Serve on `target` until the returned sender is used.
fn spawn_server(
    target: Target,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let (stop, stopped) = oneshot::channel::<()>();
    let router = acpilotd::router_with(AppState::new());
    let handle = tokio::spawn(async move {
        serve(router, target, async {
            let _ = stopped.await;
        })
        .await
    });
    (stop, handle)
}

#[tokio::test]
async fn health_answers_over_a_private_socket() {
    let directory = tempfile::tempdir().expect("temp dir");
    // A nested path on purpose: the directory does not exist yet, and creating it
    // `0700` is the daemon's job rather than the caller's.
    let path = directory.path().join("runtime/acpilot.sock");
    let (stop, server) = spawn_server(Target::Socket(path.clone()));

    let client = acpilot_client::Client::new(path.display().to_string());
    let mut health = None;
    for _ in 0..100 {
        if let Ok(answer) = client.health().await {
            health = Some(answer);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let health = health.expect("health over the socket");
    assert_eq!(health.service, "acpilotd");

    let mode = |path: &std::path::Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&path), 0o600, "the socket must be private");
    assert_eq!(
        mode(path.parent().expect("a parent")),
        0o700,
        "the directory must be private"
    );

    let _ = stop.send(());
    server.await.expect("server task").expect("served");
    assert!(
        !path.exists(),
        "a socket left behind is a lie about a daemon that is gone"
    );
}

#[tokio::test]
async fn a_live_socket_is_refused_rather_than_replaced() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("acpilot.sock");
    let (stop, server) = spawn_server(Target::Socket(path.clone()));
    // Wait for the first daemon to be listening.
    let client = acpilot_client::Client::new(path.display().to_string());
    for _ in 0..100 {
        if client.health().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let error = serve(
        acpilotd::router_with(AppState::new()),
        Target::Socket(path.clone()),
        std::future::pending(),
    )
    .await
    .expect_err("a second daemon on one socket is refused");
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse, "{error}");
    assert!(
        error.to_string().contains("another daemon"),
        "the message must say what happened: {error}"
    );

    let _ = stop.send(());
    server.await.expect("server task").expect("served");
}
