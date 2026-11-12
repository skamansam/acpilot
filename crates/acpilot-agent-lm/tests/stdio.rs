//! The agent as a client sees it: a real process, two pipes, and a model server
//! on loopback.
//!
//! This is the test that would catch the wiring a unit test cannot: the recipe is
//! found through `ACPILOT_HOME`, the binary speaks JSON-RPC on stdio, a tool call
//! comes back as an `fs/read_text_file` request to this client, and the answer is
//! streamed as ACP updates. The mock provider is HTTP because that is what
//! `acpilot-model` speaks.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

/// Where the model server listens, and what it was sent.
#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

/// One scripted answer, alternating tool call then final text.
async fn chat(State(mock): State<Mock>, Json(body): Json<Value>) -> Json<Value> {
    mock.bodies.lock().expect("bodies").push(body);
    let call = mock.calls.fetch_add(1, Ordering::SeqCst);
    let answer = if call == 0 {
        json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"/work/seed.txt\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 4 }
        })
    } else {
        json!({
            "choices": [{
                "message": { "content": "the file says hello" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 20, "completion_tokens": 6 }
        })
    };
    Json(answer)
}

/// Start the mock provider and return its base URL.
fn provider() -> (Mock, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let mock = Mock {
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

    /// Read one line, answering the one agent→client request this test produces.
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
                // `fs/read_text_file`, the only request this recipe can make.
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
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn profile(home: &std::path::Path, base_url: &str) {
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
        r#"schema_version = 1
id = "demo"
label = "Demo"
provider_id = "mock"
model_id = "mock-1"
system_prompt = "You are a test agent."
context = { window = 4096, reserve_output = 256, strategy = "truncate_oldest", max_corpus_bytes = 1024 }
tools = { read_files = true, write_files = false, run_commands = false, search = false, max_steps = 3 }
"#,
    )
    .expect("recipe");
}

#[test]
fn a_recipe_becomes_an_agent_that_reads_through_the_client() {
    let home = tempfile::tempdir().expect("home");
    let (mock, base_url) = provider();
    profile(home.path(), &base_url);

    let mut client = Harness::spawn(home.path());
    let initialized = client.request(1, "initialize", json!({ "protocolVersion": 1 }));
    assert!(
        initialized.get("agentInfo").is_some(),
        "the agent names itself: {initialized}"
    );

    let session = client.request(
        2,
        "session/new",
        json!({ "cwd": home.path(), "mcpServers": [] }),
    );
    let session_id = session["sessionId"].clone();
    assert_eq!(session_id, "lm-1", "the first session this process opened");

    let prompt = client.request(
        3,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": "read the seed file" }]
        }),
    );

    assert_eq!(
        prompt["stopReason"], "end_turn",
        "the second answer ends the turn: {prompt}"
    );
    assert_eq!(
        prompt["_meta"]["acpilot"]["steps"], 1,
        "one tool call ran: {prompt}"
    );

    let updates: Vec<&Value> = client
        .updates
        .iter()
        .filter_map(|update| update.get("params").and_then(|params| params.get("update")))
        .collect();
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
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        2,
        "the mock answered both requests"
    );
}
