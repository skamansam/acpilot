//! Probe a real language server and print a machine-readable report.
//!
//! This is the P0-7 instrument, not a product. It starts a server, negotiates
//! position encodings, opens one file, waits for diagnostics, asks for the
//! definition of a marker, and converts every returned range back to a byte
//! offset. If `--expect` is given, the definition must land exactly on that
//! substring or the process exits non-zero — the point is to prove the column,
//! not to eyeball it.
//!
//! ```text
//! cargo run -p acpilot-lsp --example lsp_probe -- \
//!   --server rust-analyzer --root spikes/fixtures/lsp/rust \
//!   --file src/lib.rs --needle 'describe("x")' --expect describe \
//!   -- rust-analyzer
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use acpilot_lsp::client::{ClientError, Server};
use acpilot_lsp::position::{LineIndex, Position, PositionEncoding};
use serde_json::{Value, json};
use url::Url;

struct Args {
    label: String,
    root: PathBuf,
    file: PathBuf,
    language_id: String,
    needle: Option<String>,
    expect: Option<String>,
    init_options: Value,
    diagnostics_timeout: Duration,
    program: String,
    program_args: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(failed) => {
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("lsp_probe: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<bool, String> {
    let args = parse_args()?;
    let root = args
        .root
        .canonicalize()
        .map_err(|error| format!("{}: {error}", args.root.display()))?;
    let file = root.join(&args.file);
    let text =
        std::fs::read_to_string(&file).map_err(|error| format!("{}: {error}", file.display()))?;
    let uri =
        Url::from_file_path(&file).map_err(|()| format!("{} is not a file URI", file.display()))?;
    let root_uri =
        Url::from_file_path(&root).map_err(|()| format!("{} is not a file URI", root.display()))?;
    let index = LineIndex::new(&text);

    let mut server = Server::spawn(&args.program, &args.program_args, &root)
        .map_err(|error| error.to_string())?;

    let initialize = json!({
        "processId": std::process::id(),
        "clientInfo": {"name": "acpilot-lsp-probe", "version": "0.0.0"},
        "initializationOptions": args.init_options,
        "rootUri": root_uri.as_str(),
        "workspaceFolders": [{"uri": root_uri.as_str(), "name": root.file_name().and_then(|name| name.to_str())}],
        "capabilities": {
            "general": {"positionEncodings": ["utf-8", "utf-16"]},
            "textDocument": {
                "synchronization": {"dynamicRegistration": false, "didSave": false},
                "definition": {"linkSupport": false},
                "diagnostic": {},
                "publishDiagnostics": {"relatedInformation": true, "versionSupport": true}
            },
            "workspace": {"configuration": true, "workspaceFolders": true, "diagnostics": {}},
            "window": {"workDoneProgress": true}
        }
    });

    let started = Instant::now();
    let response = server
        .request("initialize", initialize, Duration::from_secs(60))
        .await
        .map_err(|error| format!("initialize: {error}"))?;
    let initialize_ms = elapsed_ms(started);
    let capabilities = response.get("capabilities").cloned().unwrap_or(Value::Null);
    let encoding =
        PositionEncoding::negotiated(capabilities.get("positionEncoding").and_then(Value::as_str));

    server
        .notify("initialized", json!({}))
        .await
        .map_err(|error| format!("initialized: {error}"))?;

    let opened = Instant::now();
    server
        .notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri.as_str(),
                "languageId": args.language_id,
                "version": 1,
                "text": text
            }}),
        )
        .await
        .map_err(|error| format!("didOpen: {error}"))?;

    let deadline = Instant::now() + args.diagnostics_timeout;
    let mut progress = 0_u64;
    let mut progress_kinds: BTreeMap<String, u64> = BTreeMap::new();
    let mut first_progress_ms = None;
    let mut log_messages = 0_u64;
    let mut publishes: Vec<Value> = Vec::new();
    let mut first_diagnostics_ms = None;

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout(deadline - now, server.next_notification()).await {
            Ok(Some(message)) => {
                let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                match method {
                    "$/progress" => {
                        progress += 1;
                        first_progress_ms.get_or_insert_with(|| elapsed_ms(opened));
                        let kind = message
                            .pointer("/params/value/kind")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_owned();
                        *progress_kinds.entry(kind).or_default() += 1;
                    }
                    "textDocument/publishDiagnostics" => {
                        if message.pointer("/params/uri").and_then(Value::as_str)
                            == Some(uri.as_str())
                        {
                            first_diagnostics_ms.get_or_insert_with(|| elapsed_ms(opened));
                            publishes.push(message.get("params").cloned().unwrap_or(Value::Null));
                            break;
                        }
                    }
                    "window/logMessage" | "window/showMessage" => log_messages += 1,
                    _ => {}
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    let mut pull_attempts = 0;
    let pull = if capabilities.get("diagnosticProvider").is_some() {
        let mut outcome = None;
        for _ in 0..3 {
            pull_attempts += 1;
            let result = server
                .request(
                    "textDocument/diagnostic",
                    json!({"textDocument": {"uri": uri.as_str()}}),
                    Duration::from_secs(30),
                )
                .await;
            // -32802 is RequestCancelled: rust-analyzer cancels a pull that
            // races its own project load, and answers on the next try.
            let cancelled = matches!(&result, Err(ClientError::Server { code: -32802, .. }));
            outcome = Some(result);
            if !cancelled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
        outcome
    } else {
        None
    };

    let needle = args
        .needle
        .as_ref()
        .map(|needle| {
            text.find(needle.as_str())
                .ok_or_else(|| format!("{needle:?} does not occur in {}", file.display()))
        })
        .transpose()?;
    let expect = args
        .expect
        .as_ref()
        .map(|expect| {
            text.find(expect.as_str())
                .ok_or_else(|| format!("{expect:?} does not occur in {}", file.display()))
        })
        .transpose()?;

    let mut failures: Vec<String> = Vec::new();
    let mut attempts = Vec::new();
    let mut locations: Vec<(String, Value)> = Vec::new();
    let mut requested = Value::Null;

    if let Some(offset) = needle {
        let position = index
            .position(&text, offset, encoding)
            .ok_or_else(|| format!("offset {offset} is not a position"))?;
        requested = json!({"line": position.line, "character": position.character});
        for _ in 0..20 {
            let attempt = Instant::now();
            let result = server
                .request(
                    "textDocument/definition",
                    json!({"textDocument": {"uri": uri.as_str()}, "position": requested}),
                    Duration::from_secs(30),
                )
                .await
                .map_err(|error| format!("definition: {error}"))?;
            let found = locations_of(&result);
            attempts.push(json!({"ms": elapsed_ms(attempt), "locations": found.len()}));
            if !found.is_empty() {
                locations = found;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    let expected = expect.map(|offset| json!({"offset": offset, "text": snippet(&text, offset)}));

    let mut resolved = Vec::new();
    for (target, range) in &locations {
        let start = position_of(range.get("start"));
        let end = position_of(range.get("end"));
        let offset = start.and_then(|position| index.offset(&text, position, encoding));
        let end_offset = end.and_then(|position| index.offset(&text, position, encoding));
        if start.is_none() || end.is_none() {
            failures.push(format!(
                "definition range {range} is not a valid position pair"
            ));
        }
        if target == uri.as_str() && offset.is_none() {
            failures.push(format!(
                "definition start {range} does not map into the opened document"
            ));
        }
        if let (Some(offset), Some(expected_offset)) = (offset, expect)
            && target == uri.as_str()
            && offset != expected_offset
        {
            failures.push(format!(
                "{target} definition starts at byte {offset} ({:?}), expected byte {expected_offset} ({:?})",
                snippet(&text, offset),
                snippet(&text, expected_offset),
            ));
        }
        resolved.push(json!({
            "uri": target,
            "range": range,
            "offset": offset,
            "end_offset": end_offset,
            "snippet": offset.map(|offset| snippet(&text, offset)),
        }));
    }
    if expect.is_some() && locations.is_empty() {
        failures.push("the server returned no definition".to_owned());
    }

    // Every diagnostic range is converted too: a server that reports a range in
    // the wrong encoding often reports a column inside a character.
    let mut published_diagnostics = Vec::new();
    for publish in &publishes {
        for diagnostic in publish
            .get("diagnostics")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let range = diagnostic.get("range").cloned().unwrap_or(Value::Null);
            let start = position_of(range.get("start"));
            let end = position_of(range.get("end"));
            let start_offset = start.and_then(|position| index.offset(&text, position, encoding));
            let end_offset = end.and_then(|position| index.offset(&text, position, encoding));
            if start_offset.is_none() || end_offset.is_none() {
                failures.push(format!(
                    "diagnostic range {range} does not map into the document"
                ));
            }
            published_diagnostics.push(json!({
                "severity": diagnostic.get("severity"),
                "message": diagnostic.get("message").and_then(Value::as_str).map(|message| message.chars().take(160).collect::<String>()),
                "range": range,
                "start_offset": start_offset,
                "end_offset": end_offset,
            }));
        }
    }

    let pull_report = match &pull {
        None => Value::Null,
        Some(Err(ClientError::Server { code, message, .. })) => json!({
            "supported": false,
            "error": {"code": code, "message": message},
        }),
        Some(Err(error)) => json!({"supported": false, "error": error.to_string()}),
        Some(Ok(result)) => {
            let items = result.get("items").and_then(Value::as_array);
            let mut converted = Vec::new();
            for item in items.into_iter().flatten() {
                let range = item.get("range").cloned().unwrap_or(Value::Null);
                let start = position_of(range.get("start"))
                    .and_then(|position| index.offset(&text, position, encoding));
                if start.is_none() {
                    failures.push(format!(
                        "pull diagnostic range {range} does not map into the document"
                    ));
                }
                converted.push(json!({
                    "severity": item.get("severity"),
                    "message": item.get("message").and_then(Value::as_str).map(|message| message.chars().take(160).collect::<String>()),
                    "range": range,
                    "start_offset": start,
                }));
            }
            json!({
                "supported": true,
                "kind": result.get("kind"),
                "result_id": result.get("resultId"),
                "count": converted.len(),
                "items": converted,
            })
        }
    };

    // Give the server a moment to finish the progress stream that the first
    // publish did not end, then count everything still queued.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut server_requests = Vec::new();
    while let Some(request) = server.try_next_server_request() {
        server_requests.push(request.get("method").cloned().unwrap_or(Value::Null));
    }
    while let Some(message) = server.try_next_notification() {
        match message.get("method").and_then(Value::as_str) {
            Some("$/progress") => {
                progress += 1;
                let kind = message
                    .pointer("/params/value/kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                *progress_kinds.entry(kind).or_default() += 1;
            }
            Some("window/logMessage" | "window/showMessage") => log_messages += 1,
            _ => {}
        }
    }

    let mut uri_notes = Vec::new();
    let mut seen: Vec<&str> = vec![uri.as_str(), root_uri.as_str()];
    seen.extend(locations.iter().map(|(target, _)| target.as_str()));
    for target in seen {
        if uri_notes.iter().any(|note: &Value| note["uri"] == target) {
            continue;
        }
        uri_notes.push(uri_note(target));
    }

    let report = json!({
        "server": args.label,
        "command": std::iter::once(args.program.clone()).chain(args.program_args.clone()).collect::<Vec<_>>(),
        "root": root,
        "file": file,
        "uri": uri.as_str(),
        "timing_ms": {
            "initialize": initialize_ms,
            "first_progress": first_progress_ms,
            "first_publish_diagnostics": first_diagnostics_ms,
        },
        "negotiation": {
            "offered": ["utf-8", "utf-16"],
            "advertised": capabilities.get("positionEncoding"),
            "used": encoding.as_wire(),
        },
        "capabilities": {
            "positionEncoding": capabilities.get("positionEncoding"),
            "definitionProvider": capabilities.get("definitionProvider"),
            "diagnosticProvider": capabilities.get("diagnosticProvider"),
            "textDocumentSync": capabilities.get("textDocumentSync"),
            "completionProvider": capabilities.get("completionProvider"),
            "hoverProvider": capabilities.get("hoverProvider"),
        },
        "diagnostics": {
            "publishes": publishes.len(),
            "published": published_diagnostics,
            "pull": pull_report,
            "pull_attempts": pull_attempts,
        },
        "definition": {
            "needle": args.needle,
            "requested": requested,
            "attempts": attempts,
            "expected": expected,
            "locations": resolved,
        },
        "progress": {
            "notifications": progress,
            "kinds": progress_kinds,
        },
        "server_requests": server_requests,
        "window_messages": log_messages,
        "uris": uri_notes,
        "failures": failures,
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("a Value is serialisable")
    );

    let failed = !report["failures"].as_array().is_some_and(Vec::is_empty);
    if failed {
        eprintln!(
            "lsp_probe: {} check(s) failed for {}",
            report["failures"].as_array().map_or(0, Vec::len),
            args.label
        );
    }

    server.shutdown().await;
    let stderr = server.stderr_tail();
    if !stderr.is_empty() {
        eprintln!(
            "--- {} stderr (last {} lines) ---",
            args.label,
            stderr.len()
        );
        for line in stderr {
            eprintln!("{line}");
        }
    }

    Ok(failed)
}

fn parse_args() -> Result<Args, String> {
    let mut label = None;
    let mut root = None;
    let mut file = None;
    let mut language_id = None;
    let mut needle = None;
    let mut expect = None;
    let mut init_options = Value::Null;
    let mut timeout = 15.0_f64;
    let mut program = None;
    let mut program_args = Vec::new();

    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--" => {
                program = arguments.next();
                program_args = arguments.collect();
                break;
            }
            "--server" => label = Some(value(&mut arguments, "--server")?),
            "--root" => root = Some(PathBuf::from(value(&mut arguments, "--root")?)),
            "--file" => file = Some(PathBuf::from(value(&mut arguments, "--file")?)),
            "--language" => language_id = Some(value(&mut arguments, "--language")?),
            "--needle" => needle = Some(value(&mut arguments, "--needle")?),
            "--expect" => expect = Some(value(&mut arguments, "--expect")?),
            "--init-options" => {
                let raw = value(&mut arguments, "--init-options")?;
                init_options = serde_json::from_str(&raw)
                    .map_err(|error| format!("--init-options is not JSON: {error}"))?;
            }
            "--timeout" => {
                timeout = value(&mut arguments, "--timeout")?
                    .parse()
                    .map_err(|_| "--timeout expects seconds".to_owned())?;
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    let program = program.ok_or("a server command is required after --")?;
    let root = root.ok_or("--root is required")?;
    let file = file.ok_or("--file is required")?;
    let label = label.unwrap_or_else(|| {
        Path::new(&program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&program)
            .to_owned()
    });
    let language_id = language_id.unwrap_or_else(|| language_of(&file).to_owned());

    Ok(Args {
        label,
        root,
        file,
        language_id,
        needle,
        expect,
        init_options,
        diagnostics_timeout: Duration::from_secs_f64(timeout),
        program,
        program_args,
    })
}

fn value(arguments: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{name} needs a value"))
}

fn language_of(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rs") => "rust",
        Some("ts") | Some("tsx") => "typescript",
        Some("js") | Some("jsx") => "javascript",
        Some("py") => "python",
        Some("go") => "go",
        Some("c") | Some("h") => "c",
        _ => "plaintext",
    }
}

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

fn position_of(value: Option<&Value>) -> Option<Position> {
    let value = value?;
    Some(Position::new(
        u32::try_from(value.get("line")?.as_u64()?).ok()?,
        u32::try_from(value.get("character")?.as_u64()?).ok()?,
    ))
}

fn locations_of(result: &Value) -> Vec<(String, Value)> {
    let items: Vec<&Value> = match result {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![result],
        _ => Vec::new(),
    };

    items
        .into_iter()
        .filter_map(|item| {
            let target = item
                .get("targetUri")
                .or_else(|| item.get("uri"))?
                .as_str()?
                .to_owned();
            let range = item
                .get("targetSelectionRange")
                .or_else(|| item.get("targetRange"))
                .or_else(|| item.get("range"))?
                .clone();
            Some((target, range))
        })
        .collect()
}

fn snippet(text: &str, offset: usize) -> String {
    text.get(offset..)
        .unwrap_or_default()
        .chars()
        .take(40)
        .collect::<String>()
        .replace('\n', "\\n")
}

fn uri_note(uri: &str) -> Value {
    match Url::parse(uri) {
        Ok(url) => json!({
            "uri": uri,
            "ok": true,
            "scheme": url.scheme(),
            "host": url.host_str(),
            "percent_encoded": url.path().contains('%'),
            "path": url.path(),
        }),
        Err(error) => json!({"uri": uri, "ok": false, "error": error.to_string()}),
    }
}
