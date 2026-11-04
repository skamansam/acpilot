#![cfg(unix)]

//! The daemon's half of the helper handshake, against programs that answer for it.
//!
//! `acpilotd-helper` itself is tested where it lives; what this covers is the other
//! end — that a helper which shakes hands is believed, one that speaks another
//! protocol is refused by number, and one that says nothing is a failure rather than
//! an empty success. A shell standing in for the helper is what makes the failures
//! reproducible without a second host.

use acpilotd::helper::probe;

fn speak(line: &str) -> (String, Vec<String>) {
    (
        "sh".to_owned(),
        vec!["-c".to_owned(), format!("printf '%s\\n' '{line}'")],
    )
}

#[test]
fn a_helper_that_shakes_hands_is_believed() {
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
fn a_helper_that_is_not_there_names_the_program() {
    let error = probe("/nonexistent/acpilotd-helper", &[]).expect_err("no such program");
    assert!(error.contains("/nonexistent/acpilotd-helper"), "{error}");
}
