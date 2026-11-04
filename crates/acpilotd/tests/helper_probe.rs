#![cfg(unix)]

//! The daemon's half of the helper handshake, against programs that answer for it.
//!
//! `acpilotd-helper` itself is tested where it lives; what this covers is the other
//! end — that a helper which shakes hands is believed, one that speaks another
//! protocol is refused by number, and one that says nothing is a failure rather than
//! an empty success — and then what a kept connection can ask it: a request is
//! answered, another request's answer is refused rather than taken, a refusal keeps
//! its code, and a helper that stops reading is a failure inside the timeout rather
//! than a daemon hung on a pipe. A shell standing in for the helper is what makes the failures
//! reproducible without a second host.

use std::time::Duration;

use acpilotd::helper::{Connection, Connections, probe};
use serde_json::json;

fn speak(line: &str) -> (String, Vec<String>) {
    (
        "sh".to_owned(),
        vec!["-c".to_owned(), format!("printf '%s\\n' '{line}'")],
    )
}

#[test]
fn a_helper_from_before_the_method_list_gets_protocol_1s_capabilities() {
    let (program, args) =
        speak(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol":1,"os":"linux","arch":"x86_64"}}"#);
    let capabilities = probe(&program, &args).expect("a helper that answered");
    assert!(capabilities.filesystem, "protocol 1 reads and writes files");
    assert!(capabilities.spawn, "protocol 1 runs programs");
    assert!(
        !capabilities.pty,
        "protocol 1 has no PTY, so a client is not offered one"
    );
    assert!(!capabilities.lsp);
}

#[test]
fn a_helper_that_lists_its_methods_is_believed() {
    let (program, args) = speak(
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocol":1,"methods":["hello","fs.read","fs.write","fs.list"]}}"#,
    );
    let capabilities = probe(&program, &args).expect("a helper that answered");
    assert!(capabilities.filesystem, "it offered fs.*");
    assert!(
        !capabilities.spawn,
        "it did not offer exec, so a client is not invited to start an agent on it"
    );
    assert!(!capabilities.pty);
    assert!(!capabilities.lsp);
}

#[test]
fn a_helper_that_lists_no_methods_offers_nothing() {
    let (program, args) = speak(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol":1,"methods":[]}}"#);
    let capabilities = probe(&program, &args).expect("a helper that answered");
    assert!(
        !capabilities.filesystem && !capabilities.pty && !capabilities.spawn && !capabilities.lsp,
        "an empty list is what the helper said, not a reason to fall back"
    );
}

#[test]
fn a_helper_that_offers_a_pty_is_offered_as_one() {
    let (program, args) =
        speak(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol":1,"methods":["pty.open","exec"]}}"#);
    let capabilities = probe(&program, &args).expect("a helper that answered");
    assert!(capabilities.pty, "a helper that grew a PTY says so");
    assert!(capabilities.spawn);
    assert!(!capabilities.filesystem, "no fs.* in its list");
}

#[test]
fn a_helper_that_speaks_another_protocol_is_refused_by_number() {
    let (program, args) = speak(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol":9}}"#);
    let error = probe(&program, &args).expect_err("another protocol is not this one");
    assert!(error.contains("protocol 1"), "{error}");
    assert!(error.contains('9'), "{error}");
}

#[test]
fn a_helper_that_says_nothing_is_a_failure() {
    let (program, args) = ("sh".to_owned(), vec!["-c".to_owned(), "exit 0".to_owned()]);
    let error = probe(&program, &args).expect_err("silence is not a handshake");
    assert!(error.contains("answered nothing"), "{error}");
}

#[test]
fn a_second_test_of_the_same_host_asks_the_helper_that_answered_the_first() {
    // The stand-in answers every hello it reads, but exits silently if it is
    // started a second time. Reuse is therefore the only way the second call can
    // succeed, and a daemon that dialled again would fail with "answered
    // nothing" rather than pass for the wrong reason.
    let directory = tempfile::tempdir().expect("temp dir");
    let marker = directory.path().join("dialled");
    // The stand-in answers each handshake with the id it was asked: the daemon checks
    // the id now, so a fixed one would be a refused answer rather than a reuse.
    let answer = r#"{"jsonrpc":"2.0","id":%s,"result":{"protocol":1}}"#;
    let script = format!(
        "if [ -e '{marker}' ]; then exit 0; fi\n: > '{marker}'\n\
         n=0\nwhile read -r _; do n=$((n+1)); printf '{answer}\\n' \"$n\"; done",
        marker = marker.display(),
    );
    let program = "sh".to_owned();
    let args = vec!["-c".to_owned(), script];
    let connections = Connections::new();

    let first = connections
        .probe_with("ssh:box", &program, &args)
        .expect("the first test dials");
    assert!(first.filesystem);
    let second = connections
        .probe_with("ssh:box", &program, &args)
        .expect("the second test reuses the helper");
    assert!(second.spawn);
}

#[test]
fn a_helper_that_is_not_there_names_the_program() {
    let error = probe("/nonexistent/acpilotd-helper", &[]).expect_err("no such program");
    assert!(error.contains("/nonexistent/acpilotd-helper"), "{error}");
}

#[test]
fn a_request_gets_the_answer_to_that_request() {
    let (program, args) = answering(
        r#"n=0
while read -r _; do
  n=$((n+1))
  if [ "$n" = 1 ]; then printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocol":1,"methods":["fs.list"]}}';
  else printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"entries":["a.txt"]}}'; fi
done"#,
    );
    let mut connection = Connection::dial(&program, &args).expect("the handshake");
    let result = connection
        .request("fs.list", json!({}))
        .expect("the second request is answered");
    assert_eq!(result["entries"][0], "a.txt", "{result}");
}

#[test]
fn another_requests_answer_is_refused_rather_than_taken() {
    let (program, args) = answering(
        r#"n=0
while read -r _; do
  n=$((n+1))
  if [ "$n" = 1 ]; then printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocol":1}}';
  else printf '%s\n' '{"jsonrpc":"2.0","id":7,"result":{"entries":[]}}'; fi
done"#,
    );
    let mut connection = Connection::dial(&program, &args).expect("the handshake");
    let error = connection
        .request("fs.list", json!({}))
        .expect_err("an answer to somebody else's request");
    assert!(error.contains("request 7"), "{error}");
    assert!(error.contains("not 2"), "{error}");
}

#[test]
fn a_refusal_keeps_the_code_the_helper_gave_it() {
    let (program, args) = answering(
        r#"n=0
while read -r _; do
  n=$((n+1))
  if [ "$n" = 1 ]; then printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocol":1}}';
  else printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"no such file: /nope"}}'; fi
done"#,
    );
    let mut connection = Connection::dial(&program, &args).expect("the handshake");
    let error = connection
        .request("fs.read", json!({ "path": "/nope" }))
        .expect_err("a missing file is a refusal");
    assert!(error.contains("fs.read"), "{error}");
    assert!(error.contains("no such file: /nope"), "{error}");
    assert!(error.contains("-32000"), "{error}");
}

#[test]
fn a_helper_that_stops_answering_fails_inside_the_timeout() {
    let (program, args) = answering(
        r#"n=0
while read -r _; do
  n=$((n+1))
  if [ "$n" = 1 ]; then printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocol":1}}';
  else exec sleep 30; fi
done"#,
    );
    let mut connection = Connection::dial_within(&program, &args, Duration::from_millis(300))
        .expect("the handshake");
    let error = connection
        .request("exec", json!({}))
        .expect_err("a helper that stopped reading");
    assert!(error.contains("did not answer within"), "{error}");
}

/// A shell that stands in for a helper, with the loop the tests above describe.
fn answering(script: &str) -> (String, Vec<String>) {
    ("sh".to_owned(), vec!["-c".to_owned(), script.to_owned()])
}
