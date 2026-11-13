//! The agent as a client sees it: a real process, two pipes, and a model server
//! on loopback.
//!
//! This is the test that would catch the wiring a unit test cannot: the recipe is
//! found through `ACPILOT_HOME`, the binary speaks JSON-RPC on stdio, a tool call
//! comes back as an `fs/read_text_file` request to this client, and the answer is
//! streamed as ACP updates. The mock provider answers from a script, so a repair
//! and a step limit are the same harness with different answers.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

/// A scripted provider: one answer per request, repeating the last when the script
/// runs out.
#[derive(Clone)]
struct Mock {
    answers: Arc<Mutex<Vec<Value>>>,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

async fn chat(State(mock): State<Mock>, Json(body): Json<Value>) -> Json<Value> {
    mock.bodies.lock().expect("bodies").push(body);
    let call = mock.calls.fetch_add(1, Ordering::SeqCst);
    let answers = mock.answers.lock().expect("answers");
    let answer = answers
        .get(call)
        .or_else(|| answers.last())
        .cloned()
        .unwrap_or_else(|| final_answer("the script is empty"));
    Json(answer)
}

/// A tool call answer, with the arguments exactly as written.
fn tool_answer(name: &str, arguments: &str) -> Value {
    json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "function": { "name": name, "arguments": arguments }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 12, "completion_tokens": 4 }
    })
}

/// A final text answer.
fn final_answer(text: &str) -> Value {
    json!({
        "choices": [{
            "message": { "content": text },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 20, "completion_tokens": 6 }
    })
}

/// Start the scripted provider and return its base URL.
fn provider(answers: Vec<Value>) -> (Mock, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let mock = Mock {
        answers: Arc::new(Mutex::new(answers)),
        calls: Arc::new(AtomicUsize::new(0)),
        bodies: Arc::new(Mutex::new(Vec::new())),
    };
    let state = mock.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/chat/completions", post(chat))
                    .with_state(state),
            )
            .await
            .expect("mock provider");
        });
    });
    (mock, format!("http://127.0.0.1:{port}"))
}

/// Write one JSON-RPC line.
fn send(stdin: &mut ChildStdin, message: &Value) {
    writeln!(stdin, "{message}").expect("write request");
    stdin.flush().expect("flush");
}

/// A client that answers what the agent asks and records what it announces.
struct Harness {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    updates: Vec<Value>,
}

impl Harness {
    fn spawn(home: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_acpilot-agent-lm"))
            .arg("--recipe")
            .arg("demo")
            .arg("--profile")
            .arg("work")
            .env("ACPILOT_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the agent");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
            updates: Vec::new(),
        }
    }

    /// Read one line, answering the one agent→client request this recipe produces.
    fn line(&mut self) -> Value {
        let mut text = String::new();
        self.reader
            .read_line(&mut text)
            .expect("read a protocol line");
        assert!(
            !text.trim().is_empty(),
            "the agent closed stdout before answering"
        );
        let value: Value = serde_json::from_str(text.trim()).expect("a JSON-RPC line");
        if value.get("method").is_some() {
            if value.get("id").is_some() {
                // `fs/read_text_file`, the only request these recipes can make.
                assert_eq!(value["method"], "fs/read_text_file");
                send(
                    &mut self.stdin,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": value["id"].clone(),
                        "result": { "content": "hello\n" }
                    }),
                );
            } else {
                self.updates.push(value);
            }
            return self.line();
        }
        value
    }

    fn request(&mut self, id: u32, method: &str, params: Value) -> Value {
        send(
            &mut self.stdin,
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        );
        loop {
            let value = self.line();
            if value.get("id").and_then(Value::as_u64) == Some(u64::from(id)) {
                assert!(value.get("error").is_none(), "request failed: {value}");
                return value["result"].clone();
            }
        }
    }

    /// Open a session and run one prompt, in that order.
    fn prompt(&mut self, home: &std::path::Path, text: &str) -> Value {
        self.request(1, "initialize", json!({ "protocolVersion": 1 }));
        let session = self.request(2, "session/new", json!({ "cwd": home, "mcpServers": [] }));
        self.request(
            3,
            "session/prompt",
            json!({
                "sessionId": session["sessionId"],
                "prompt": [{ "type": "text", "text": text }]
            }),
        )
    }

    /// The `update` objects the agent announced.
    fn updates(&self) -> Vec<&Value> {
        self.updates
            .iter()
            .filter_map(|update| update.get("params").and_then(|params| params.get("update")))
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn profile(home: &std::path::Path, base_url: &str, max_steps: u32) {
    let root = home.join("profiles").join("work");
    std::fs::create_dir_all(root.join("recipes")).expect("recipe directory");
    std::fs::write(root.join("config.toml"), "schema_version = 1\n").expect("config");
    std::fs::write(
        root.join("providers.toml"),
        format!(
            "schema_version = 1\n\n[[providers]]\nid = \"mock\"\nkind = \"open_ai_compatible\"\nlabel = \"Mock\"\nbase_url = \"{base_url}\"\n"
        ),
    )
    .expect("providers");
    std::fs::write(
        root.join("recipes").join("demo.toml"),
        format!(
            r#"schema_version = 1
id = "demo"
label = "Demo"
provider_id = "mock"
model_id = "mock-1"
system_prompt = "You are a test agent."
context = {{ window = 4096, reserve_output = 256, strategy = "truncate_oldest", max_corpus_bytes = 1024 }}
tools = {{ read_files = true, write_files = false, run_commands = false, search = false, max_steps = {max_steps} }}
"#
        ),
    )
    .expect("recipe");
}

#[test]
fn a_recipe_becomes_an_agent_that_reads_through_the_client() {
    let home = tempfile::tempdir().expect("home");
    let (mock, base_url) = provider(vec![
        tool_answer("read_file", r#"{"path":"/work/seed.txt"}"#),
        final_answer("the file says hello"),
    ]);
    profile(home.path(), &base_url, 3);

    let mut client = Harness::spawn(home.path());
    let prompt = client.prompt(home.path(), "read the seed file");

    assert_eq!(
        prompt["stopReason"], "end_turn",
        "the second answer ends the turn: {prompt}"
    );
    assert_eq!(
        prompt["_meta"]["acpilot"]["steps"], 1,
        "one tool call ran: {prompt}"
    );

    let updates = client.updates();
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|update| update.get("sessionUpdate").and_then(Value::as_str))
        .collect();
    assert!(
        kinds.contains(&"tool_call"),
        "the read is announced: {kinds:?}"
    );
    assert!(
        kinds.contains(&"tool_call_update"),
        "and its result: {kinds:?}"
    );
    assert!(
        kinds.contains(&"usage_update"),
        "the counts are reported: {kinds:?}"
    );
    assert!(
        kinds.contains(&"agent_message_chunk"),
        "the answer is streamed: {kinds:?}"
    );
    assert!(
        updates.iter().any(|update| update["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("hello"))),
        "the answer the model gave is what was streamed"
    );

    let bodies = mock.bodies.lock().expect("bodies");
    assert_eq!(bodies.len(), 2, "one call, then one after the tool result");
    let tool_message = bodies[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the tool result was sent back to the model");
    assert!(
        tool_message["content"]
            .as_str()
            .is_some_and(|content| content.contains("hello")),
        "the result carries the file: {tool_message}"
    );
}

#[test]
fn one_malformed_call_is_repaired_before_it_costs_the_turn() {
    let home = tempfile::tempdir().expect("home");
    let (mock, base_url) = provider(vec![
        tool_answer("read_file", "{path: "),
        tool_answer("read_file", r#"{"path":"/work/seed.txt"}"#),
        final_answer("repaired and read"),
    ]);
    profile(home.path(), &base_url, 3);

    let mut client = Harness::spawn(home.path());
    let prompt = client.prompt(home.path(), "read the seed file");
    assert_eq!(
        prompt["stopReason"], "end_turn",
        "a repaired call continues the turn: {prompt}"
    );

    let updates = client.updates();
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "notice"
                && update["title"] == "Tool call repaired"),
        "the repair is visible rather than silent: {updates:?}"
    );
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "tool_call_update"
                && update["status"] == "completed"),
        "the repaired call ran: {updates:?}"
    );

    let bodies = mock.bodies.lock().expect("bodies");
    assert_eq!(bodies.len(), 3, "two attempts, then the final answer");
    let repair = bodies[1]["messages"]
        .as_array()
        .expect("messages")
        .last()
        .expect("a repair message");
    assert!(
        repair["content"]
            .as_str()
            .is_some_and(|content| content.contains("not accepted")),
        "the model is told what was wrong with its own call: {repair}"
    );
}

#[test]
fn the_step_limit_ends_the_turn_with_the_protocols_own_reason() {
    let home = tempfile::tempdir().expect("home");
    let (mock, base_url) = provider(vec![tool_answer(
        "read_file",
        r#"{"path":"/work/seed.txt"}"#,
    )]);
    profile(home.path(), &base_url, 1);

    let mut client = Harness::spawn(home.path());
    let prompt = client.prompt(home.path(), "read forever");
    assert_eq!(
        prompt["stopReason"], "max_turn_requests",
        "the cap is reported to the client: {prompt}"
    );

    let updates = client.updates();
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "notice"
                && update["title"] == "Step limit reached"
                && update["description"]
                    .as_str()
                    .is_some_and(|text| text.contains("allows 1"))),
        "the notice says how many steps the recipe allows: {updates:?}"
    );
    assert_eq!(
        prompt["_meta"]["acpilot"]["steps"], 1,
        "one step ran before the cap"
    );
    assert_eq!(
        mock.bodies.lock().expect("bodies").len(),
        2,
        "the second call is requested, then refused"
    );
}

#[test]
fn search_is_never_offered_and_the_agent_says_why() {
    let home = tempfile::tempdir().expect("home");
    let (_mock, base_url) = provider(vec![final_answer("done")]);
    profile(home.path(), &base_url, 2);
    // Turn the search ceiling on in the recipe that was just written.
    let recipe = home.path().join("profiles/work/recipes/demo.toml");
    let text = std::fs::read_to_string(&recipe).expect("recipe");
    std::fs::write(&recipe, text.replace("search = false", "search = true")).expect("recipe");

    let mut client = Harness::spawn(home.path());
    client.prompt(home.path(), "search the docs");
    let updates = client.updates();
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "notice"
                && update["title"] == "Search is not offered"),
        "the missing callback is stated, not faked: {updates:?}"
    );
}
