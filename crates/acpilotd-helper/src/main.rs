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
//! `fs.read`, `fs.write`, `fs.list` and `exec` — the operations `acpilot-host`'s
//! `Host` trait asks for. Text is UTF-8 on the wire; a binary read is refused rather
//! than mangled, because the caller can tell a refusal from an empty file and cannot
//! tell either from lossy bytes. There is no PTY yet: `exec` runs a program with
//! piped stdio, and a caller that wants a terminal gets `unsupported` naming that.
//!
//! No dependencies beyond `serde_json`: the helper is uploaded and run where nothing
//! can be installed, so its size and its build are part of its contract.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

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
struct Failure {
    code: i64,
    message: String,
}

impl Failure {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The method is not one this helper has.
    fn method(name: &str) -> Self {
        Self::new(-32601, format!("{name} is not a method this helper has"))
    }

    /// The parameters are missing or wrong.
    fn params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    /// The host refused the operation: a missing file, a directory that is a file,
    /// a program that would not start.
    fn host(message: impl Into<String>) -> Self {
        Self::new(-32000, message)
    }
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let response = answer(&line);
        if writeln!(stdout, "{response}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            break;
        }
    }
}

/// One line in, one line out. A line that is not a request is answered rather than
/// skipped: a caller waiting on a reply it will never get is worse than an error.
fn answer(line: &str) -> String {
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
    match call(&request.method, &request.params) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": request.id, "result": result }).to_string(),
        Err(failure) => error_line(request.id, &failure),
    }
}

fn error_line(id: Value, failure: &Failure) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": failure.code, "message": failure.message },
    })
    .to_string()
}

/// Every method this helper answers, as `hello` advertises them.
///
/// The daemon turns this list into capabilities, so a method added to [`call`]
/// without a line here is one the far side never offers, and a name here that
/// [`call`] does not answer is a promise that fails on first use. A test calls each
/// name to keep the list true, and the list is in the handshake so a daemon does not
/// have to assume what the helper it reached can do.
const METHODS: [&str; 5] = ["hello", "fs.read", "fs.write", "fs.list", "exec"];

fn call(method: &str, params: &Value) -> Result<Value, Failure> {
    match method {
        "hello" => hello(params),
        "fs.read" => read(params),
        "fs.write" => write(params),
        "fs.list" => list(params),
        "exec" => exec(params),
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
