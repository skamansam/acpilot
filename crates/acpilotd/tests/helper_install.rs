#![cfg(unix)]

//! Putting the helper on a host that has never seen it.
//!
//! A release build carries `acpilotd-helper` inside `acpilotd`; the first connection
//! writes it to `~/.local/bin` over the transport that is already there and starts that
//! copy. A developer build carries nothing, so the bytes are injected here — the install
//! path is the same code either way.
//!
//! The "host" is a shell that drops the ssh options and address and runs what the daemon
//! asked for on this machine: an install (`sh -s` reading its script from stdin) and a
//! launch (`sh -c 'exec …'`) both work, and the installed file is a real program the
//! daemon then talks to. Nothing is copied from a build directory, because the whole claim
//! is that the daemon can produce a helper for a host out of what it carries.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use acpilotd::{AppState, router_with};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A transport that is this machine: `ssh -o … box <command>` drops the three arguments in
/// front of the command and hands the rest to a shell, which is what `ssh` does with the
/// remote command line — one string, parsed by the far side rather than by this program.
const TRANSPORT: &str = r#"#!/bin/sh
shift 3
exec sh -c "$*"
"#;

/// The helper the daemon installs, as bytes.
///
/// A shell that speaks the protocol: it names the methods it has, prints one line when it
/// starts — so a test can count dials — and answers `fs.list` with no entries.
fn helper_script(log: &Path) -> String {
    format!(
        r#"#!/bin/sh
printf 'started\n' >> '{log}'
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"fs.list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"entries":[]}}}}\n' "$id"
      ;;
    *)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocol":1,"methods":["hello","fs.list"]}}}}\n' "$id"
      ;;
  esac
done
"#,
        log = log.display()
    )
}

/// A daemon whose `ssh` is this machine, whose helper it carries is [`helper_script`], and
/// whose install directory is a temporary one.
fn daemon(work: &Path, install: &Path, log: &Path) -> Router {
    let transport = work.join("ssh.sh");
    std::fs::write(&transport, TRANSPORT).expect("write the transport");
    std::fs::set_permissions(&transport, std::fs::Permissions::from_mode(0o755))
        .expect("chmod the transport");

    let state = AppState::new();
    state
        .hosts()
        .prefer_ssh(Some(transport.display().to_string()));
    state
        .hosts()
        .prefer_install_dir(Some(install.display().to_string()));
    state
        .hosts()
        .prefer_bundle(Some(helper_script(log).into_bytes()));
    router_with(state)
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
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
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Name a host and ask this daemon to test it — the two calls a client makes before it can
/// open a workspace anywhere but the machine it is on.
async fn add_and_test(app: &Router) -> (StatusCode, serde_json::Value) {
    let (status, host) = send(
        app,
        "POST",
        "/v1/hosts",
        Some(serde_json::json!({ "kind": "ssh", "address": "box" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{host}");
    test(app).await
}

async fn test(app: &Router) -> (StatusCode, serde_json::Value) {
    send(app, "POST", "/v1/hosts/ssh:box/test", None).await
}

#[tokio::test]
async fn the_daemon_puts_the_helper_it_carries_on_the_host() {
    let directory = tempfile::tempdir().expect("temp dir");
    let install = directory.path().join("bin");
    let log = directory.path().join("dials.log");
    let app = daemon(directory.path(), &install, &log);

    let (status, tested) = add_and_test(&app).await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    assert_eq!(tested["connected"], true, "{tested}");
    assert_eq!(
        tested["capabilities"]["filesystem"], true,
        "the helper that answered is the one that was installed: {tested}"
    );

    let installed = install.join("acpilotd-helper");
    assert!(
        installed.is_file(),
        "the helper is on the host at {}",
        installed.display()
    );
    let mode = std::fs::metadata(&installed)
        .expect("the installed helper")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755, "the host can execute what was written");
    assert_eq!(
        std::fs::read(&installed).expect("read the installed helper"),
        helper_script(&log).into_bytes(),
        "byte for byte: the base64 in the install script is not lossy"
    );
    assert_eq!(
        std::fs::read_to_string(&log)
            .expect("the dial log")
            .lines()
            .count(),
        1,
        "the installed copy is what answered, once"
    );
}

#[tokio::test]
async fn a_second_test_reuses_the_helper_rather_than_installing_it_again() {
    let directory = tempfile::tempdir().expect("temp dir");
    let install = directory.path().join("bin");
    let log = directory.path().join("dials.log");
    let app = daemon(directory.path(), &install, &log);

    let (status, first) = add_and_test(&app).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = test(&app).await;
    assert_eq!(status, StatusCode::OK, "{second}");

    assert_eq!(
        std::fs::read_to_string(&log)
            .expect("the dial log")
            .lines()
            .count(),
        1,
        "a kept connection is one helper, not one upload per test"
    );
}

#[tokio::test]
async fn a_host_that_will_not_take_the_helper_says_so_and_still_tries_the_one_on_its_path() {
    let directory = tempfile::tempdir().expect("temp dir");
    let log = directory.path().join("dials.log");
    // A directory no user can create: the install fails, and the PATH attempt is the one
    // that follows. Both reasons are reported, because either one could be the fix.
    let app = daemon(directory.path(), Path::new("/proc/acpilot-no-helper"), &log);

    let (status, body) = add_and_test(&app).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    // `details` is the API's field list, not a map: `[{ "field": "transport", "message": … }]`.
    let detail = body["details"]
        .as_array()
        .and_then(|details| details.first())
        .and_then(|detail| detail["message"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(
        detail.contains("putting acpilotd-helper in"),
        "the install's own refusal is reported: {body}"
    );
    assert!(
        detail.contains("PATH"),
        "and the attempt this daemon fell back to: {body}"
    );
    assert!(!log.exists(), "nothing was installed, so nothing started");
}
