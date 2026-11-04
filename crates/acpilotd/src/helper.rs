//! Talking to `acpilotd-helper` on a host this daemon cannot reach directly.
//!
//! The helper is one process per host, spoken to over stdin/stdout as
//! line-delimited JSON-RPC 2.0 (see `crates/acpilotd-helper`). This module owns the
//! daemon's half of that: start the transport, shake hands, and report what the far
//! side can do. It deliberately does not keep the process: a connection test is one
//! connection, and a session that keeps a helper alive is the remote-workspace work
//! that comes after this.
//!
//! # Why the version is checked
//!
//! The helper is uploaded and reused independently of the daemon, so a new daemon
//! beside an old helper is the expected failure. `hello` names the protocol this
//! build speaks and the helper refuses another by number; a handshake that answered
//! something unexpected is reported rather than parsed optimistically.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use acpilot_proto::host::HostCapabilities;
use serde_json::{Value, json};

/// The protocol this daemon speaks.
///
/// Kept in step with `crates/acpilotd-helper/src/main.rs`'s `PROTOCOL`: the two
/// values are the same contract from two sides, and a build that changes one
/// without the other is refused at the handshake instead of failing a call.
pub const PROTOCOL: u64 = 1;

/// Start `program` with `args`, ask it what it is, and end it.
///
/// Returns the capabilities protocol 1 defines rather than what was probed method
/// by method: the handshake is the answer to "does this helper work", and a probe
/// that called every method would be a second copy of the protocol here.
///
/// # Errors
///
/// The program could not start, wrote nothing, answered something that is not a
/// handshake, or speaks another protocol. The message names what happened; the
/// caller turns it into a `502` for the host.
pub fn probe(program: &str, args: &[String]) -> Result<HostCapabilities, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let result = handshake(&mut child);
    // The helper is a session, not a call, but this is a test rather than a
    // session: nothing will ever ask it again, and leaving it running would be a
    // process per connection test.
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn handshake(child: &mut Child) -> Result<HostCapabilities, String> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "the helper has no stdin".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "the helper has no stdout".to_owned())?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "hello",
        "params": { "protocol": PROTOCOL },
    });
    // A program that exited before reading is not a helper, and it answers
    // nothing — which the read below reports as itself. Treating the write's
    // `EPIPE` as the failure instead would name the symptom (a broken pipe) rather
    // than the fact (no handshake), and would report it twice for one cause.
    let _ = writeln!(stdin, "{request}").and_then(|()| stdin.flush());
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .map_err(|error| format!("the helper would not answer: {error}"))?;
    if line.trim().is_empty() {
        return Err("the helper answered nothing".to_owned());
    }
    let response: Value = serde_json::from_str(&line)
        .map_err(|error| format!("the helper answered something that is not JSON-RPC: {error}"))?;
    let protocol = response
        .get("result")
        .and_then(|result| result.get("protocol"))
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("the helper did not shake hands: {}", line.trim()))?;
    if protocol != PROTOCOL {
        return Err(format!(
            "this daemon speaks protocol {PROTOCOL}, the helper {protocol}"
        ));
    }
    // What protocol 1 defines: whole-file reads and writes, a listing, and a
    // program with piped stdio. No PTY and no language server yet, so a client is
    // not offered a pane on a host that would refuse to open one.
    Ok(HostCapabilities {
        filesystem: true,
        pty: false,
        spawn: true,
        lsp: false,
    })
}
