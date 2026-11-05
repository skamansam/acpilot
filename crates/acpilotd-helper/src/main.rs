//! Remote/WSL helper: the daemon's hands on a host it cannot reach directly.
//!
//! One process per host, spoken to over stdin/stdout as line-delimited JSON-RPC 2.0.
//! A session rather than a call: the design phase's measurement (P0-5, see
//! `docs/notes/wsl-helper.md`) put a fresh spawn at ~140–200 ms and a message on a
//! warm one at 0.1 ms, so a daemon that spawned per request would pay a 1,400× tax
//! on every turn.
//!
//! # Handshake
//!
//! The first call is `hello` with `{"protocol": 1}`. A helper that speaks a
//! different protocol refuses by name rather than half-answering: the two ends are
//! uploaded and reused independently, so a stale helper beside a new daemon is the
//! expected failure, and a version number in the message is what makes it fixable.
//!
//! # What it exposes
//!
//! `fs.read`, `fs.write`, `fs.list`, `fs.remove`, `fs.mkdir`, `fs.ignored`, `exec`, the
//! `pty.*` family and the `lsp.*` family — the operations `acpilot-host`'s `Host` trait
//! asks for, plus the one question a tree asks about a directory, the one thing a pane
//! cannot do without, and the one thing a language server cannot: a pipe that stays open.
//! Text is UTF-8 on the wire; a binary read is refused rather than mangled, because the
//! caller can tell a refusal from an empty file and cannot tell either from lossy bytes.
//! `exec` runs a program with piped stdio and answers when it is over.
//!
//! # Terminals are not calls
//!
//! `pty.open` starts a program on a real PTY and answers with an id; the output comes
//! back afterwards as the only other shape of line this protocol has, a notification
//! (`pty.data`, then `pty.exit`). The daemon opens **one helper per terminal**, so a
//! stream never queues behind a request and a request never waits behind output. See
//! `pty.rs` for why the id is this process's rather than a pid.
//!
//! `lsp.start` does the same for a language server over pipes rather than a PTY: the
//! daemon sends its `initialize` through `lsp.write` and reads the answers as
//! `lsp.data`, so a server on the host indexes the host's files. `docs/notes/ssh-agent.md`
//! says why it is pipes: a PTY would add echo and CRLF translation the protocol never
//! asked for.
//!
//! `serde_json` and `portable-pty`, and nothing else: the helper is uploaded and run
//! where nothing can be installed, so its size and its build are part of its contract.
//! The PTY crate is the daemon's own — the platform branch is inside it, and opening a
//! terminal by hand would be `unsafe`, which this workspace forbids.

mod base64;
mod lsp;
mod pty;

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

/// The protocol this helper speaks.
///
/// Bumped when a method's shape changes; a daemon and a helper that disagree refuse
/// to talk rather than fail one call at a time.
const PROTOCOL: u32 = 1;

/// One JSON-RPC request.
#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// A refusal, in the JSON-RPC error shape.
#[derive(Debug)]
pub(crate) struct Failure {
    code: i64,
    message: String,
}

impl Failure {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The method is not one this helper has.
    pub(crate) fn method(name: &str) -> Self {
        Self::new(-32601, format!("{name} is not a method this helper has"))
    }

    /// The parameters are missing or wrong.
    pub(crate) fn params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    /// The host refused the operation: a missing file, a directory that is a file,
    /// a program that would not start.
    pub(crate) fn host(message: impl Into<String>) -> Self {
        Self::new(-32000, message)
    }
}

fn main() {
    // Terminals and servers outlive any one request, and the reader thread of each
    // needs the map it was registered in, so the sessions are shared rather than
    // borrowed.
    let sessions = Arc::new(pty::Sessions::new());
    let servers = Arc::new(lsp::Sessions::new());
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let response = answer(&line, &sessions, &servers);
        sessions.send(&response);
    }
    // The daemon closed the pipe: the shells and servers it started have nobody left to
    // talk to, and a process whose helper died is one nobody can name, attach to, or
    // send a request to.
    sessions.close_all();
    servers.close_all();
}

/// One line in, one line out. A line that is not a request is answered rather than
/// skipped: a caller waiting on a reply it will never get is worse than an error.
fn answer(line: &str, sessions: &Arc<pty::Sessions>, servers: &Arc<lsp::Sessions>) -> Value {
    let request: Request = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(error) => {
            return error_line(
                Value::Null,
                &Failure::new(
                    -32700,
                    format!("the line is not a JSON-RPC request: {error}"),
                ),
            );
        }
    };
    match call(&request.method, &request.params, sessions, servers) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": request.id, "result": result }),
        Err(failure) => error_line(request.id, &failure),
    }
}

fn error_line(id: Value, failure: &Failure) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": failure.code, "message": failure.message },
    })
}

/// Every method this helper answers, as `hello` advertises them.
///
/// The daemon turns this list into capabilities, so a method added to [`call`]
/// without a line here is one the far side never offers, and a name here that
/// [`call`] does not answer is a promise that fails on first use. A test calls each
/// name to keep the list true, and the list is in the handshake so a daemon does not
/// have to assume what the helper it reached can do.
const METHODS: [&str; 16] = [
    "hello",
    "fs.read",
    "fs.write",
    "fs.list",
    "fs.remove",
    "fs.mkdir",
    "fs.ignored",
    "exec",
    "pty.open",
    "pty.write",
    "pty.resize",
    "pty.kill",
    "pty.close",
    "lsp.start",
    "lsp.write",
    "lsp.close",
];

fn call(
    method: &str,
    params: &Value,
    sessions: &Arc<pty::Sessions>,
    servers: &Arc<lsp::Sessions>,
) -> Result<Value, Failure> {
    match method {
        "hello" => hello(params),
        "fs.read" => read(params),
        "fs.write" => write(params),
        "fs.list" => list(params),
        "fs.remove" => remove(params),
        "fs.mkdir" => mkdir(params),
        "fs.ignored" => ignored(params),
        "exec" => exec(params),
        "pty.open" => sessions.open(params),
        "pty.write" => sessions.write(params),
        "pty.resize" => sessions.resize(params),
        "pty.kill" => sessions.kill(params),
        "pty.close" => sessions.close(params),
        "lsp.start" => servers.start(params),
        "lsp.write" => servers.write(params),
        "lsp.close" => servers.close(params),
        other => Err(Failure::method(other)),
    }
}

/// The handshake: which protocol this helper speaks, what it can do, and where it
/// is running.
fn hello(params: &Value) -> Result<Value, Failure> {
    let protocol = params
        .get("protocol")
        .and_then(Value::as_u64)
        .unwrap_or(u64::from(PROTOCOL));
    if protocol != u64::from(PROTOCOL) {
        return Err(Failure::new(
            -32000,
            format!("this helper speaks protocol {PROTOCOL}, not {protocol}"),
        ));
    }
    Ok(json!({
        "protocol": PROTOCOL,
        "methods": METHODS,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "pid": std::process::id(),
    }))
}

fn path_param(params: &Value) -> Result<PathBuf, Failure> {
    params
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Failure::params("`path` is required"))
}

fn read(params: &Value) -> Result<Value, Failure> {
    let path = path_param(params)?;
    let bytes = std::fs::read(&path).map_err(|error| refused(&path, &error))?;
    let text = String::from_utf8(bytes).map_err(|error| {
        Failure::host(format!(
            "{}: not UTF-8 text ({} bytes)",
            path.display(),
            error.as_bytes().len()
        ))
    })?;
    Ok(json!({ "path": display(&path), "text": text, "bytes": text.len() }))
}

fn write(params: &Value) -> Result<Value, Failure> {
    let path = path_param(params)?;
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::params("`text` is required"))?;
    std::fs::write(&path, text).map_err(|error| refused(&path, &error))?;
    Ok(json!({ "path": display(&path), "bytes": text.len() }))
}

fn list(params: &Value) -> Result<Value, Failure> {
    let path = path_param(params)?;
    let directory = std::fs::read_dir(&path).map_err(|error| refused(&path, &error))?;
    let mut entries = Vec::new();
    for entry in directory {
        let entry = entry.map_err(|error| refused(&path, &error))?;
        let metadata = entry.metadata().map_err(|error| refused(&path, &error))?;
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "path": display(&entry.path()),
            "is_dir": metadata.is_dir(),
            "size": if metadata.is_dir() { 0 } else { metadata.len() },
        }));
    }
    entries.sort_by(|left, right| {
        left["name"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["name"].as_str().unwrap_or_default())
    });
    Ok(json!({ "entries": entries }))
}

/// Remove one file.
///
/// A directory is answered rather than removed — `removed: false` — because what happens
/// to the tree inside it is a decision for the caller, and `false` says "the call reached
/// the file and this is not one" without pretending the path was missing. A missing path
/// is still an error, since nothing was reached.
fn remove(params: &Value) -> Result<Value, Failure> {
    let path = path_param(params)?;
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| refused(&path, &error))?;
    if metadata.is_dir() {
        return Ok(json!({ "path": display(&path), "removed": false }));
    }
    std::fs::remove_file(&path).map_err(|error| refused(&path, &error))?;
    Ok(json!({ "path": display(&path), "removed": true }))
}

/// Make a directory and any parents it needs.
///
/// An existing path is `created: false` rather than a quiet success: a caller that asked
/// for a new folder and got "fine" cannot tell whether it was already there, and the two
/// answers are different in every dialog that asked for one.
fn mkdir(params: &Value) -> Result<Value, Failure> {
    let path = path_param(params)?;
    if path.exists() {
        return Ok(json!({ "path": display(&path), "created": false }));
    }
    std::fs::create_dir_all(&path).map_err(|error| refused(&path, &error))?;
    Ok(json!({ "path": display(&path), "created": true }))
}

/// Which of `paths` under `root` an ignore rule hides.
///
/// `git check-ignore` rather than a matcher of our own: the rules live in the repository
/// — `.gitignore` files, `.git/info/exclude`, the user's global excludes — and a second
/// implementation would disagree with the one the person's commits already follow. A
/// workspace that is not a repository, a host with no `git`, and a `git` that cannot read
/// the tree all answer "nothing is hidden", because a listing without marks shows every
/// file rather than claiming none is ignored.
fn ignored(params: &Value) -> Result<Value, Failure> {
    // `root` rather than `path`: this is a question *about* a directory — which of these
    // names under the repository it is — and one named `path` would read as the thing being
    // listed rather than the tree the rules come from.
    let root = params
        .get("root")
        .and_then(Value::as_str)
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Failure::params("`root` is required"))?;
    let paths: Vec<String> = params
        .get("paths")
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if paths.is_empty() {
        return Ok(json!({ "ignored": [] }));
    }
    let mut child = match std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Ok(json!({ "ignored": [] })),
    };
    if let Some(mut stdin) = child.stdin.take() {
        for path in &paths {
            // `-z` makes the separator NUL on *both* sides, not just the output: a
            // newline here is a path git never sees, and the answer is an empty list
            // that looks exactly like a repository with no rules. A path cannot hold a
            // NUL, so this is unambiguous.
            let _ = stdin
                .write_all(path.as_bytes())
                .and_then(|()| stdin.write_all(b"\0"));
        }
    }
    let Ok(output) = child.wait_with_output() else {
        return Ok(json!({ "ignored": [] }));
    };
    let listed: Vec<String> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|field| std::str::from_utf8(field).ok())
        .filter(|field| !field.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(json!({ "ignored": listed }))
}

/// Run a program with piped stdio and answer with what it printed.
///
/// `code` is absent for a process a signal ended: a caller can tell "exited 0" from
/// "was killed", which a `code` of `-1` would hide.
fn exec(params: &Value) -> Result<Value, Failure> {
    let program = params
        .get("program")
        .and_then(Value::as_str)
        .filter(|program| !program.is_empty())
        .ok_or_else(|| Failure::params("`program` is required"))?;
    let mut command = std::process::Command::new(program);
    if let Some(args) = params.get("args").and_then(Value::as_array) {
        command.args(args.iter().filter_map(Value::as_str));
    }
    if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .map_err(|error| Failure::host(format!("{program}: {error}")))?;
    Ok(json!({
        "code": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

fn refused(path: &Path, error: &io::Error) -> Failure {
    Failure::host(format!("{}: {error}", path.display()))
}

/// A path as the wire spells it: forward slashes, because the caller compares these
/// with paths it knows and a Windows backslash would make two spellings of one file.
fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
