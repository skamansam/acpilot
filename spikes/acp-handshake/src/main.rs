//! P0-1 — one small ACP client, two real agents.
//!
//! The spike answers the questions in `TODO.md` §P0-1 and nothing else:
//!
//! * what capabilities each agent advertises at `initialize`,
//! * which `session/update` kinds arrive during one prompt,
//! * which `stopReason` comes back,
//! * what a permission request looks like and what an auto-deny does to the turn,
//! * whether `session/load` works and how much history it replays.
//!
//! Everything is recorded, nothing is interpreted: the run report is the data the
//! note in `docs/notes/acp-transcripts.md` is written from.
//!
//! ```bash
//! cargo run -- --agent claude --json /tmp/claude.json
//! cargo run -- --agent opencode --json /tmp/opencode.json
//! cargo run -- --command 'my-agent --acp' --prompt 'list the files here'
//! ```

use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ClientCapabilities, ContentBlock, Implementation, InitializeRequest, LoadSessionRequest,
    NewSessionRequest, PermissionOptionKind, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SetSessionConfigOptionRequest, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo, LineDirection};
use serde::Serialize;
use serde_json::{Value, json};

/// A frame on the agent's stdio. Keeping the raw lines is what makes it possible
/// to check later that the SDK did not paper over a framing quirk.
#[derive(Debug, Clone, Serialize)]
struct WireLine {
    dir: &'static str,
    at_ms: u128,
    line: String,
}

#[derive(Debug, Clone, Serialize)]
struct Event {
    phase: &'static str,
    kind: String,
    at_ms: u128,
    payload: Value,
}

#[derive(Debug, Clone, Serialize)]
struct PermissionRecord {
    at_ms: u128,
    payload: Value,
    /// The id we answered with, or `null` for `Cancelled`.
    answered_with: Option<String>,
}

#[derive(Default)]
struct Run {
    agent: String,
    cwd: String,
    prompt: String,
    initialize: Option<Value>,
    initialize_error: Option<String>,
    config_updates: Vec<Value>,
    session_new: Option<Value>,
    session_new_error: Option<String>,
    events: Vec<Event>,
    permissions: Vec<PermissionRecord>,
    prompt_response: Option<Value>,
    prompt_error: Option<String>,
    load_requested: bool,
    load_response: Option<Value>,
    load_error: Option<Value>,
    /// `true` while `session/load` is in flight, so replayed updates are labelled.
    in_load: bool,
    load_replayed: usize,
    wire: Vec<WireLine>,
    wire_dropped: usize,
    agent_stderr: Vec<String>,
}

type Shared = Arc<Mutex<Run>>;

const WIRE_LIMIT: usize = 20_000;
const STDERR_LIMIT: usize = 500;

fn main() {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("acp-handshake: {message}\n\n{}", Args::USAGE);
            std::process::exit(2);
        }
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("acp-handshake: cannot start tokio: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = runtime.block_on(args.run()) {
        eprintln!("acp-handshake: {error}");
        std::process::exit(1);
    }
}

struct Args {
    agent: String,
    command: Option<String>,
    cwd: PathBuf,
    prompt: String,
    json: Option<PathBuf>,
    configs: Vec<(String, String)>,
    timeout: Duration,
}

impl Args {
    const USAGE: &'static str = "\
usage: acp-handshake [--agent claude|opencode] [--command 'CMD ARGS'] \\
                     [--cwd DIR] [--prompt TEXT] [--config ID=VALUE]... \
                     [--json PATH] [--timeout SECS]";

    fn parse(iter: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut args = Self {
            agent: "claude".to_owned(),
            command: None,
            cwd: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../fixtures/acp")
                .to_path_buf(),
            prompt: "list the files here".to_owned(),
            json: None,
            configs: Vec::new(),
            timeout: Duration::from_secs(300),
        };

        let mut iter = iter;
        while let Some(flag) = iter.next() {
            let mut value = |name: &str| {
                iter.next()
                    .ok_or_else(|| format!("{name} needs a value\n\n{}", Self::USAGE))
            };
            match flag.as_str() {
                "--agent" => args.agent = value("--agent")?,
                "--command" => args.command = Some(value("--command")?),
                "--cwd" => args.cwd = PathBuf::from(value("--cwd")?),
                "--prompt" => args.prompt = value("--prompt")?,
                "--json" => args.json = Some(PathBuf::from(value("--json")?)),
                "--config" => {
                    let pair = value("--config")?;
                    let (id, value) = pair
                        .split_once('=')
                        .ok_or_else(|| format!("--config takes ID=VALUE, got {pair}"))?;
                    args.configs.push((id.to_owned(), value.to_owned()));
                }
                "--timeout" => {
                    let secs = value("--timeout")?
                        .parse::<u64>()
                        .map_err(|_| "--timeout takes whole seconds".to_owned())?;
                    args.timeout = Duration::from_secs(secs);
                }
                "-h" | "--help" => {
                    println!("{}", Self::USAGE);
                    std::process::exit(0);
                }
                other => return Err(format!("unknown flag {other}")),
            }
        }
        Ok(args)
    }

    async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = self
            .cwd
            .canonicalize()
            .map_err(|error| format!("cwd {} is not usable: {error}", self.cwd.display()))?;
        if !cwd.is_dir() {
            return Err(format!("cwd {} is not a directory", cwd.display()).into());
        }

        // Agents inherit our working directory, so this is what `session/new`
        // and every tool call inside the agent see.
        std::env::set_current_dir(&cwd)?;

        let command = self
            .command
            .clone()
            .unwrap_or_else(|| match self.agent.as_str() {
                "claude" => "npx -y @agentclientprotocol/claude-agent-acp@latest".to_owned(),
                "opencode" => "opencode acp".to_owned(),
                other => other.to_owned(),
            });

        let run: Shared = Arc::new(Mutex::new(Run {
            agent: command.clone(),
            cwd: cwd.display().to_string(),
            prompt: self.prompt.clone(),
            ..Run::default()
        }));
        let started = Instant::now();

        let mut agent = AcpAgent::from_str(&command)?;
        {
            let wire = Arc::clone(&run);
            agent = agent.with_debug(move |line, direction| {
                let Ok(mut run) = wire.lock() else { return };
                match direction {
                    LineDirection::Stderr => {
                        eprintln!("[agent stderr] {line}");
                        if run.agent_stderr.len() < STDERR_LIMIT {
                            run.agent_stderr.push(line.to_owned());
                        }
                    }
                    _ => {
                        // stdin frames are what *we* sent; stdout is the agent.
                        let dir = match direction {
                            LineDirection::Stdin => "stdin",
                            _ => "stdout",
                        };
                        if run.wire.len() < WIRE_LIMIT {
                            run.wire.push(WireLine {
                                dir,
                                at_ms: started.elapsed().as_millis(),
                                line: line.to_owned(),
                            });
                        } else {
                            run.wire_dropped += 1;
                        }
                    }
                }
            });
        }

        let notification_run = Arc::clone(&run);
        let permission_run = Arc::clone(&run);
        let connection_run = Arc::clone(&run);
        let configs = self.configs.clone();
        let prompt = self.prompt.clone();
        let timeout = self.timeout;

        agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    let payload = serde_json::to_value(&notification).unwrap_or(Value::Null);
                    let kind = payload
                        .get("update")
                        .and_then(|update| update.get("sessionUpdate"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();

                    let (phase, at_ms) = {
                        let mut run = notification_run.lock().expect("run lock");
                        let phase = if run.in_load { "load" } else { "prompt" };
                        if run.in_load {
                            run.load_replayed += 1;
                        }
                        let at_ms = started.elapsed().as_millis();
                        run.events.push(Event {
                            phase,
                            kind: kind.clone(),
                            at_ms,
                            payload: payload.clone(),
                        });
                        (phase, at_ms)
                    };

                    println!("[{at_ms:>6}ms {phase:>6}] {kind} {}", compact(&payload));
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _cx| {
                    let payload = serde_json::to_value(&request).unwrap_or(Value::Null);
                    // Auto-deny: pick an explicit reject option when the agent
                    // offers one, otherwise cancel the request outright.
                    let reject = request
                        .options
                        .iter()
                        .find(|option| option.kind == PermissionOptionKind::RejectOnce)
                        .or_else(|| {
                            request
                                .options
                                .iter()
                                .find(|option| option.kind == PermissionOptionKind::RejectAlways)
                        });

                    let outcome = match reject {
                        Some(option) => RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(option.option_id.clone()),
                        ),
                        None => RequestPermissionOutcome::Cancelled,
                    };
                    let answered_with = reject.map(|option| option.option_id.0.to_string());

                    let at_ms = started.elapsed().as_millis();
                    println!(
                        "[{at_ms:>6}ms  perm] auto-deny -> {} {}",
                        answered_with.as_deref().unwrap_or("cancelled"),
                        compact(&payload)
                    );
                    if let Ok(mut run) = permission_run.lock() {
                        run.permissions.push(PermissionRecord {
                            at_ms,
                            payload,
                            answered_with,
                        });
                    }
                    responder.respond(RequestPermissionResponse::new(outcome))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, move |connection: ConnectionTo<Agent>| {
                // The report still owns the original handle after the
                // connection returns; this is the connection's own clone.
                let run = connection_run;
                async move {
                    let initialize = InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(ClientCapabilities::new())
                        .client_info(Implementation::new(
                            "acpilot-spike",
                            env!("CARGO_PKG_VERSION"),
                        ));
                    match with_timeout(
                        Duration::from_secs(60),
                        connection.send_request(initialize).block_task(),
                    )
                    .await
                    {
                        Ok(response) => match response {
                            Ok(response) => {
                                let value = serde_json::to_value(&response).unwrap_or(Value::Null);
                                println!("\ninitialize -> {}", pretty(&value));
                                if let Ok(mut run) = run.lock() {
                                    run.initialize = Some(value);
                                }
                            }
                            Err(error) => {
                                record_error(&run, |run| {
                                    run.initialize_error = Some(error_text(&error))
                                });
                                return Ok(());
                            }
                        },
                        Err(elapsed) => {
                            record_error(&run, |run| run.initialize_error = Some(elapsed));
                            return Ok(());
                        }
                    }

                    let new_session = match with_timeout(
                        Duration::from_secs(90),
                        connection
                            .send_request(NewSessionRequest::new(cwd.clone()))
                            .block_task(),
                    )
                    .await
                    {
                        Ok(Ok(response)) => response,
                        Ok(Err(error)) => {
                            record_error(&run, |run| {
                                run.session_new_error = Some(error_text(&error));
                            });
                            return Ok(());
                        }
                        Err(elapsed) => {
                            record_error(&run, |run| run.session_new_error = Some(elapsed));
                            return Ok(());
                        }
                    };

                    let session_id = new_session.session_id.clone();
                    let value = serde_json::to_value(&new_session).unwrap_or(Value::Null);
                    println!("\nsession/new -> {}", pretty(&value));
                    if let Ok(mut run) = run.lock() {
                        run.session_new = Some(value);
                    }

                    // A session is born with whatever model and mode the agent
                    // prefers; choosing another one is part of the handshake, and
                    // the agent may refuse the write without failing the session.
                    for (id, value) in configs {
                        let request = SetSessionConfigOptionRequest::new(
                            session_id.clone(),
                            id.clone(),
                            value.as_str(),
                        );
                        let outcome = with_timeout(
                            Duration::from_secs(30),
                            connection.send_request(request).block_task(),
                        )
                        .await;
                        let record = match outcome {
                            Ok(Ok(response)) => json!({
                                "id": id,
                                "value": value,
                                "response": serde_json::to_value(response).unwrap_or(Value::Null),
                            }),
                            Ok(Err(error)) => json!({
                                "id": id,
                                "value": value,
                                "error": error_text(&error),
                            }),
                            Err(elapsed) => json!({
                                "id": id,
                                "value": value,
                                "error": elapsed,
                            }),
                        };
                        println!("\nsession/set_config_option -> {}", pretty(&record));
                        if let Ok(mut run) = run.lock() {
                            run.config_updates.push(record);
                        }
                    }

                    let prompt_request = PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(prompt))],
                    );
                    match with_timeout(
                        timeout,
                        connection.send_request(prompt_request).block_task(),
                    )
                    .await
                    {
                        Ok(Ok(response)) => {
                            let value = serde_json::to_value(&response).unwrap_or(Value::Null);
                            println!("\nsession/prompt -> {}", pretty(&value));
                            if let Ok(mut run) = run.lock() {
                                run.prompt_response = Some(value);
                            }
                        }
                        Ok(Err(error)) => {
                            record_error(&run, |run| {
                                run.prompt_error = Some(error_text(&error));
                            });
                        }
                        Err(elapsed) => {
                            record_error(&run, |run| run.prompt_error = Some(elapsed));
                            return Ok(());
                        }
                    }

                    // `session/load` is attempted even when the agent did not
                    // advertise it: the error it returns is part of the answer.
                    if let Ok(mut run) = run.lock() {
                        run.load_requested = true;
                        run.in_load = true;
                    }
                    let load = with_timeout(
                        Duration::from_secs(60),
                        connection
                            .send_request(LoadSessionRequest::new(session_id, cwd))
                            .block_task(),
                    )
                    .await;

                    if let Ok(mut run) = run.lock() {
                        run.in_load = false;
                    }
                    match load {
                        Ok(Ok(response)) => {
                            let value = serde_json::to_value(&response).unwrap_or(Value::Null);
                            println!("\nsession/load -> {}", pretty(&value));
                            if let Ok(mut run) = run.lock() {
                                run.load_response = Some(value);
                            }
                        }
                        Ok(Err(error)) => {
                            let value = json!({
                                "display": error.to_string(),
                                "debug": format!("{error:?}"),
                            });
                            println!("\nsession/load -> error {}", pretty(&value));
                            if let Ok(mut run) = run.lock() {
                                run.load_error = Some(value);
                            }
                        }
                        Err(elapsed) => {
                            let value = json!(elapsed);
                            println!("\nsession/load -> error {}", pretty(&value));
                            if let Ok(mut run) = run.lock() {
                                run.load_error = Some(value);
                            }
                        }
                    }

                    Ok(())
                }
            })
            .await?;

        let run = Arc::try_unwrap(run)
            .map_err(|_| "run still shared")?
            .into_inner()
            .map_err(|_| "run lock poisoned")?;
        report(&run, started.elapsed(), self.json.as_deref())
    }
}

fn report(
    run: &Run,
    elapsed: Duration,
    json_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut kinds = std::collections::BTreeMap::new();
    for event in &run.events {
        *kinds.entry(event.kind.clone()).or_insert(0usize) += 1;
    }
    let stop_reason = run
        .prompt_response
        .as_ref()
        .and_then(|value| value.get("stopReason"))
        .cloned();

    let value = json!({
        "agent": run.agent,
        "cwd": run.cwd,
        "prompt": run.prompt,
        "elapsed_ms": elapsed.as_millis(),
        "initialize": run.initialize,
        "initialize_error": run.initialize_error,
        "session_new": run.session_new,
        "session_new_error": run.session_new_error,
        "config_updates": run.config_updates,
        "events": run.events,
        "event_kinds": kinds,
        "permissions": run.permissions,
        "prompt_response": run.prompt_response,
        "prompt_error": run.prompt_error,
        "stop_reason": stop_reason,
        "load": {
            "requested": run.load_requested,
            "response": run.load_response,
            "error": run.load_error,
            "replayed_updates": run.load_replayed,
        },
        "wire": {
            "lines": run.wire.len(),
            "dropped": run.wire_dropped,
            "frames": run.wire,
        },
        "agent_stderr": run.agent_stderr,
    });

    println!("\n=== summary ===");
    println!("agent: {}", run.agent);
    println!("elapsed: {} ms", elapsed.as_millis());
    println!("event kinds: {kinds:?}");
    println!(
        "permission requests: {} (all auto-denied)",
        run.permissions.len()
    );
    println!("stop reason: {stop_reason:?}");
    if run.load_requested {
        println!(
            "session/load: {} replayed updates, result {}",
            run.load_replayed,
            if run.load_response.is_some() {
                "ok"
            } else {
                "error"
            }
        );
    }
    println!("wire lines: {}", run.wire.len());

    match json_path {
        Some(path) => {
            std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
            println!("report: {}", path.display());
        }
        None => println!("\n{}", serde_json::to_string_pretty(&value)?),
    }
    Ok(())
}

fn record_error(run: &Shared, apply: impl FnOnce(&mut Run)) {
    let mut run = run.lock().expect("run lock");
    apply(&mut run);
}

fn error_text(error: &agent_client_protocol::Error) -> String {
    format!("{error} ({error:?})")
}

/// One request, one deadline. A spike that hangs is a spike that lies about
/// being finished.
async fn with_timeout<T>(
    timeout: Duration,
    future: impl std::future::Future<Output = T>,
) -> Result<T, String> {
    match tokio::time::timeout(timeout, future).await {
        Ok(value) => Ok(value),
        Err(_) => Err(format!("timed out after {} s", timeout.as_secs())),
    }
}

fn compact(value: &Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_default();
    if text.chars().count() > 300 {
        let head: String = text.chars().take(300).collect();
        format!("{head}…")
    } else {
        text
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}
