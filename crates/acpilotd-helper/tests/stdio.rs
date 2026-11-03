//! The helper as a daemon drives it: one process, JSON lines in both directions.
//!
//! A real child process rather than the library's functions, because the contract
//! here is a protocol — the handshake, one line per request, a refusal that still
//! answers — and none of that exists until the binary is started.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Helper {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_acpilotd-helper"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the helper starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// One round trip: a line there, a line back.
    fn ask(&mut self, method: &str, params: Value) -> Value {
        let request = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        writeln!(self.stdin, "{request}").expect("write");
        self.stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read");
        serde_json::from_str(&line).expect("a response")
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let response = self.ask(method, params);
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("expected a result, got {response}"))
    }

    fn error(&mut self, method: &str, params: Value) -> Value {
        let response = self.ask(method, params);
        response
            .get("error")
            .cloned()
            .unwrap_or_else(|| panic!("expected an error, got {response}"))
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_handshake_names_the_protocol_and_the_host() {
    let mut helper = Helper::start();
    let hello = helper.call("hello", json!({ "protocol": 1 }));
    assert_eq!(hello["protocol"], 1);
    assert_eq!(hello["os"], std::env::consts::OS);
    assert_eq!(hello["arch"], std::env::consts::ARCH);
    assert!(hello["pid"].as_u64().is_some());
}

#[test]
fn a_protocol_it_does_not_speak_is_refused_by_number() {
    let mut helper = Helper::start();
    let error = helper.error("hello", json!({ "protocol": 99 }));
    assert_eq!(error["code"], -32000);
    let message = error["message"].as_str().expect("a message");
    assert!(message.contains("99"), "{message}");
}

#[test]
fn a_file_is_written_read_and_listed() {
    let directory = tempfile::tempdir().expect("directory");
    let file = directory.path().join("note.txt");
    let path = file.to_string_lossy().replace('\\', "/");

    let mut helper = Helper::start();
    let written = helper.call("fs.write", json!({ "path": path, "text": "hello, host" }));
    assert_eq!(written["bytes"], 11);

    let read = helper.call("fs.read", json!({ "path": path }));
    assert_eq!(read["text"], "hello, host");
    assert_eq!(read["bytes"], 11);

    let listed = helper.call(
        "fs.list",
        json!({ "path": directory.path().to_string_lossy().replace('\\', "/") }),
    );
    let entries = listed["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "note.txt");
    assert_eq!(entries[0]["is_dir"], false);
}

#[test]
fn a_binary_file_is_refused_rather_than_mangled() {
    let directory = tempfile::tempdir().expect("directory");
    let file = directory.path().join("blob.bin");
    std::fs::write(&file, [0xff, 0xfe, 0x00]).expect("write");

    let mut helper = Helper::start();
    let error = helper.error(
        "fs.read",
        json!({ "path": file.to_string_lossy().replace('\\', "/") }),
    );
    assert_eq!(error["code"], -32000);
    assert!(
        error["message"]
            .as_str()
            .expect("a message")
            .contains("UTF-8")
    );
}

#[test]
fn an_unknown_method_is_refused_by_name() {
    let mut helper = Helper::start();
    let error = helper.error("pty.open", json!({}));
    assert_eq!(error["code"], -32601);
}

#[cfg(unix)]
#[test]
fn exec_reports_the_exit_code_and_what_the_program_printed() {
    let mut helper = Helper::start();
    let ran = helper.call(
        "exec",
        json!({ "program": "/bin/sh", "args": ["-c", "printf hi; exit 3"] }),
    );
    assert_eq!(ran["code"], 3);
    assert_eq!(ran["stdout"], "hi");
}

#[cfg(unix)]
#[test]
fn exec_of_a_missing_program_is_a_refusal() {
    let mut helper = Helper::start();
    let error = helper.error("exec", json!({ "program": "/nonexistent/program" }));
    assert_eq!(error["code"], -32000);
}
