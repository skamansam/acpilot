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
    /// Notifications that arrived while a call was waiting for its answer.
    ///
    /// A terminal's output may be written before the response to the call that opened
    /// it: the reader thread starts as soon as the program does. Holding the lines here
    /// keeps one `call` = one line, and the terminal tests read them in order.
    pending: Vec<Value>,
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
            pending: Vec::new(),
        }
    }

    /// One round trip: a line there, the response line back.
    ///
    /// Notifications are set aside rather than returned: the caller asked for an
    /// answer, and a terminal's output is not one.
    fn ask(&mut self, method: &str, params: Value) -> Value {
        let request = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        writeln!(self.stdin, "{request}").expect("write");
        self.stdin.flush().expect("flush");
        loop {
            let value = self.line();
            if value.get("id").is_some() {
                return value;
            }
            self.pending.push(value);
        }
    }

    /// One line, held notifications first so a test sees the stream in order.
    fn line(&mut self) -> Value {
        if !self.pending.is_empty() {
            return self.pending.remove(0);
        }
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read");
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("not JSON: {error}: {line:?}"))
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
fn every_method_the_handshake_advertises_is_one_the_helper_answers() {
    // The daemon reads capabilities out of this list, so a name here that `call`
    // does not serve would be a capability advertised and then refused on first use.
    let mut helper = Helper::start();
    let hello = helper.call("hello", json!({ "protocol": 1 }));
    let methods: Vec<&str> = hello["methods"]
        .as_array()
        .expect("the handshake lists methods")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(methods.contains(&"exec"), "{methods:?}");
    for method in methods {
        if method == "hello" {
            continue;
        }
        let response = helper.ask(method, json!({}));
        assert_ne!(
            response.pointer("/error/code").and_then(Value::as_i64),
            Some(-32601),
            "{method} is advertised but not answered: {response}"
        );
    }
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
    // Not `pty.open`: that one exists now, and a test that named it would be asking for
    // a missing-parameter error while claiming to be about a method this helper has not
    // got.
    let error = helper.error("pty.teleport", json!({}));
    assert_eq!(error["code"], -32601);
}

/// The bytes a notification carries, decoded here rather than imported: the helper's
/// codec is inside the binary, and a test that called it would not be reading what
/// crossed the pipe.
///
/// Unix-gated with its tests: a Windows build runs neither, and a function no test uses
/// is `-D dead-code` on a runner that compiles this file.
#[cfg(unix)]
fn data(event: &Value) -> String {
    let encoded = event["params"]["data"].as_str().expect("a data payload");
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in encoded.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).expect("a byte"));
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(unix)]
#[test]
fn a_terminal_paints_and_then_says_how_it_ended() {
    let mut helper = Helper::start();
    helper.call("hello", json!({ "protocol": 1 }));
    let opened = helper.call(
        "pty.open",
        json!({
            "program": "/bin/sh",
            "args": ["-c", "printf 'hello from the box\\n'; exit 4"],
            "cols": 80,
            "rows": 24,
        }),
    );
    let id = opened["id"].clone();
    assert!(
        id.as_u64().is_some(),
        "the host names the terminal: {opened}"
    );
    let mut painted = String::new();
    loop {
        let event = helper.line();
        match event["method"].as_str() {
            Some("pty.data") => {
                assert_eq!(event["params"]["id"], id, "output belongs to its terminal");
                painted.push_str(&data(&event));
            }
            Some("pty.exit") => {
                assert_eq!(event["params"]["id"], id);
                assert_eq!(
                    event["params"]["code"], 4,
                    "the code the program exited with, not a generic failure"
                );
                break;
            }
            other => panic!("the helper said something else: {other:?}: {event}"),
        }
    }
    assert!(
        painted.contains("hello from the box"),
        "the bytes the program wrote came back: {painted:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_terminal_takes_input_and_a_new_size() {
    let mut helper = Helper::start();
    helper.call("hello", json!({ "protocol": 1 }));
    let opened = helper.call(
        "pty.open",
        // `read` blocks until a line arrives, which is what makes this a test of the
        // input direction rather than of a program that exited before it was typed at.
        json!({
            "program": "/bin/sh",
            "args": ["-c", "read line; printf 'got:%s\\n' \"$line\""],
            "cols": 80,
            "rows": 24,
        }),
    );
    let id = opened["id"].clone();
    let resized = helper.call("pty.resize", json!({ "id": id, "cols": 100, "rows": 30 }));
    assert_eq!(resized["cols"], 100);

    helper.call(
        "pty.write",
        // Written through the same encoder the daemon uses: bytes as a JSON string would
        // have to escape, which is why the channel is base64 in the first place.
        json!({ "id": id, "data": "aGVsbG8K" }),
    );

    let mut painted = String::new();
    loop {
        let event = helper.line();
        match event["method"].as_str() {
            Some("pty.data") => painted.push_str(&data(&event)),
            Some("pty.exit") => break,
            other => panic!("the helper said something else: {other:?}: {event}"),
        }
    }
    assert!(
        painted.contains("got:hello"),
        "the program read what the daemon typed: {painted:?}"
    );
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

#[cfg(unix)]
#[test]
fn ignored_paths_are_what_git_on_the_host_says() {
    // `git check-ignore` and not a matcher of our own: the rules a commit follows are the
    // repository's, and a second implementation would disagree with them the first time a
    // pattern was subtle. A host without `git` answers "nothing is ignored" instead, which
    // is the same answer this test would get and not the thing it is checking for.
    let directory = tempfile::tempdir().expect("directory");
    let Ok(status) = Command::new("git")
        .arg("-C")
        .arg(directory.path())
        .args(["init", "-q"])
        .status()
    else {
        return;
    };
    if !status.success() {
        return;
    }
    std::fs::write(directory.path().join(".gitignore"), "target\n").expect("write");
    let root = directory.path().to_string_lossy().replace('\\', "/");

    let mut helper = Helper::start();
    let listed = helper.call(
        "fs.ignored",
        json!({ "root": root, "paths": ["target", "src"] }),
    );
    assert_eq!(
        listed["ignored"],
        json!(["target"]),
        "the path the rule hides, and not the one it does not: {listed}"
    );
}

#[test]
fn with_no_repository_nothing_is_reported_as_ignored() {
    let directory = tempfile::tempdir().expect("directory");
    let root = directory.path().to_string_lossy().replace('\\', "/");

    let mut helper = Helper::start();
    let listed = helper.call("fs.ignored", json!({ "root": root, "paths": ["a.txt"] }));
    assert_eq!(
        listed["ignored"],
        json!([]),
        "a tree that is not a repository has no rules to hide anything with: {listed}"
    );
}

#[test]
fn a_file_is_removed_and_a_directory_is_answered_rather_than_removed() {
    let directory = tempfile::tempdir().expect("directory");
    let file = directory.path().join("note.txt");
    std::fs::write(&file, "bye").expect("write");
    let file = file.to_string_lossy().replace('\\', "/");
    let root = directory.path().to_string_lossy().replace('\\', "/");

    let mut helper = Helper::start();
    let removed = helper.call("fs.remove", json!({ "path": file }));
    assert_eq!(removed["removed"], true);
    assert!(
        !directory.path().join("note.txt").exists(),
        "the file is gone"
    );

    let refused = helper.call("fs.remove", json!({ "path": root }));
    assert_eq!(
        refused["removed"], false,
        "a folder is a decision, not a delete: {refused}"
    );
    assert!(directory.path().exists(), "and it is still there");
}

#[test]
fn a_missing_path_is_an_error_and_an_existing_directory_is_not_created() {
    let directory = tempfile::tempdir().expect("directory");
    let missing = directory.path().join("nothing-here");
    let missing = missing.to_string_lossy().replace('\\', "/");
    let root = directory.path().to_string_lossy().replace('\\', "/");

    let mut helper = Helper::start();
    let error = helper.error("fs.remove", json!({ "path": missing }));
    assert_eq!(error["code"], -32000);

    let deep = format!("{root}/a/b");
    let made = helper.call("fs.mkdir", json!({ "path": deep }));
    assert_eq!(made["created"], true);
    assert!(
        directory.path().join("a/b").is_dir(),
        "parents are made too"
    );

    let again = helper.call("fs.mkdir", json!({ "path": format!("{root}/a") }));
    assert_eq!(again["created"], false);
}
