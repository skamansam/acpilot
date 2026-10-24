//! A deliberately small LSP client: spawn, request, notify, listen.
//!
//! The design phase's task P0-7 exists to find out what real servers do before the language-intelligence phase (P7) commits to a loop.
//! `async-lsp` is the candidate library; this module is the control group, small
//! enough to read in one sitting and generic enough that the language-intelligence phase (P7) can keep it if the
//! library's transport abstraction turns out to cost more than it saves.
//!
//! It does not implement document sync, cancellation, or progress bookkeeping.
//! It does answer server-to-client requests instead of ignoring them, because a
//! server blocked on `client/registerCapability` never answers `initialize` and
//! the resulting hang looks like a slow index.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

use crate::wire::{self, FramingError};

/// How many stderr lines are retained for a failure report.
const STDERR_TAIL: usize = 200;

/// Client failures. A protocol error from the server is distinct from a broken
/// transport: one is an answer, the other is a dead process.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server binary could not be started.
    #[error("could not start the language server: {0}")]
    Spawn(#[source] std::io::Error),
    /// The transport failed.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// The server exited with this request still outstanding.
    #[error("the server exited while {0} was pending")]
    Closed(String),
    /// The server returned a JSON-RPC error.
    #[error("server error for {method}: {message} ({code})")]
    Server {
        /// Method that failed.
        method: String,
        /// JSON-RPC error code.
        code: i64,
        /// Server-supplied message.
        message: String,
    },
    /// No answer within the deadline. A wedged server is a status-line warning,
    /// not an editor that never repaints.
    #[error("{0} did not answer within {1:?}")]
    Timeout(String, Duration),
}

#[derive(Debug, Clone)]
struct RpcError {
    code: i64,
    message: String,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A running language server and its message pump.
pub struct Server {
    child: Child,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    pending: Pending,
    notifications: mpsc::UnboundedReceiver<Value>,
    requests: mpsc::UnboundedReceiver<Value>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    next_id: i64,
}

impl Server {
    /// Start `program` with `args` in `root`, with stdio framed by [`wire`].
    pub fn spawn(program: &str, args: &[String], root: &Path) -> Result<Self, ClientError> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(ClientError::Spawn)?;

        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let stdin = Arc::new(AsyncMutex::new(stdin));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
        let (notifications, notification_rx) = mpsc::unbounded_channel();
        let (requests, request_rx) = mpsc::unbounded_channel();

        tokio::spawn(pump(
            BufReader::new(stdout),
            Arc::clone(&stdin),
            Arc::clone(&pending),
            notifications,
            requests,
        ));
        tokio::spawn(collect_stderr(
            BufReader::new(stderr),
            Arc::clone(&stderr_tail),
        ));

        Ok(Self {
            child,
            stdin,
            pending,
            notifications: notification_rx,
            requests: request_rx,
            stderr: stderr_tail,
            next_id: 1,
        })
    }

    /// Send a request and wait for its response.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id, sender);

        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = send(&self.stdin, &message).await {
            lock(&self.pending).remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(timeout, receiver).await {
            Err(_) => {
                lock(&self.pending).remove(&id);
                Err(ClientError::Timeout(method.to_owned(), timeout))
            }
            Ok(Err(_)) => Err(ClientError::Closed(method.to_owned())),
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(ClientError::Server {
                method: method.to_owned(),
                code: error.code,
                message: error.message,
            }),
        }
    }

    /// Poll for a notification without awaiting one.
    pub fn try_next_notification(&mut self) -> Option<Value> {
        self.notifications.try_recv().ok()
    }

    /// Poll for an auto-answered server request without awaiting one.
    pub fn try_next_server_request(&mut self) -> Option<Value> {
        self.requests.try_recv().ok()
    }

    /// Send a notification; there is no response to wait for.
    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), ClientError> {
        send(
            &self.stdin,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        )
        .await
    }

    /// The next server-to-client notification, or `None` once the pump stops.
    pub async fn next_notification(&mut self) -> Option<Value> {
        self.notifications.recv().await
    }

    /// Server-to-client requests that were answered automatically (and logged).
    pub async fn next_server_request(&mut self) -> Option<Value> {
        self.requests.recv().await
    }

    /// The last [`STDERR_TAIL`] stderr lines, oldest first.
    #[must_use]
    pub fn stderr_tail(&self) -> Vec<String> {
        lock(&self.stderr).iter().cloned().collect()
    }

    /// Whether the server process has exited.
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Ask the server to shut down, then insist.
    ///
    /// The protocol's exit is two messages: `shutdown` (a request) and `exit` (a
    /// notification). A server that ignores both gets killed, because a probe
    /// that cannot exit is a probe that cannot be run in a loop.
    pub async fn shutdown(&mut self) {
        let _ = self
            .request("shutdown", Value::Null, Duration::from_secs(5))
            .await;
        let _ = self.notify("exit", Value::Null).await;

        match tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await {
            Ok(Ok(_)) => {}
            _ => {
                let _ = self.child.kill().await;
            }
        }
    }
}

async fn send(stdin: &AsyncMutex<ChildStdin>, message: &Value) -> Result<(), ClientError> {
    let body = serde_json::to_vec(message).expect("a serde_json::Value is always serialisable");
    let mut stdin = stdin.lock().await;
    wire::write_message(&mut *stdin, &body).await?;
    Ok(())
}

/// Read framed messages and route them until the server closes stdout.
async fn pump<R>(
    mut reader: R,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    pending: Pending,
    notifications: mpsc::UnboundedSender<Value>,
    requests: mpsc::UnboundedSender<Value>,
) where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        match wire::read_message(&mut reader).await {
            Ok(Some(body)) => {
                let Ok(message) = serde_json::from_slice::<Value>(&body) else {
                    eprintln!(
                        "acpilot-lsp: dropping a message that is not JSON ({} bytes)",
                        body.len()
                    );
                    continue;
                };
                match (message.get("method"), message.get("id")) {
                    (Some(method), Some(id)) => {
                        let method = method.as_str().unwrap_or_default();
                        let _ = requests.send(message.clone());
                        let response = answer(method, id, message.get("params"));
                        if let Err(error) = send(&stdin, &response).await {
                            eprintln!("acpilot-lsp: failed to answer {method}: {error}");
                            break;
                        }
                    }
                    (Some(_), None) => {
                        // A closed UI is allowed to stop listening; the server
                        // is not allowed to block on it.
                        let _ = notifications.send(message);
                    }
                    (None, Some(id)) => {
                        let Some(id) = id.as_i64() else { continue };
                        let Some(sender) = lock(&pending).remove(&id) else {
                            continue;
                        };
                        let outcome = if let Some(error) = message.get("error") {
                            Err(RpcError {
                                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                                message: error
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("no message")
                                    .to_owned(),
                            })
                        } else {
                            Ok(message.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = sender.send(outcome);
                    }
                    (None, None) => {}
                }
            }
            Ok(None) => break,
            Err(error) => {
                eprintln!("acpilot-lsp: framing failed: {error}");
                break;
            }
        }
    }

    for (_, sender) in lock(&pending).drain() {
        let _ = sender.send(Err(RpcError {
            code: 0,
            message: "the server closed the connection".to_owned(),
        }));
    }
}

/// Answer the handful of requests every server sends during startup.
///
/// Returning `MethodNotFound` is the correct answer to anything else: it tells
/// the server what we cannot do, rather than leaving it waiting.
fn answer(method: &str, id: &Value, params: Option<&Value>) -> Value {
    let result = match method {
        "workspace/configuration" => {
            let count = params
                .and_then(|params| params.get("items"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Value::Array(vec![Value::Null; count])
        }
        "client/registerCapability"
        | "client/unregisterCapability"
        | "window/workDoneProgress/create"
        | "window/showMessageRequest" => Value::Null,
        _ => {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("{method} is not supported")}
            });
        }
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

async fn collect_stderr<R>(mut stderr: R, tail: Arc<Mutex<VecDeque<String>>>)
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        match stderr.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let mut tail = lock(&tail);
                if tail.len() == STDERR_TAIL {
                    tail.pop_front();
                }
                tail.push_back(line.trim_end().to_owned());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_configuration_with_one_null_per_item() {
        let response = answer(
            "workspace/configuration",
            &json!(7),
            Some(&json!({"items": [{"section": "a"}, {"section": "b"}]})),
        );
        assert_eq!(response["id"], 7);
        assert_eq!(response["result"], json!([null, null]));
    }

    #[test]
    fn refuses_unknown_server_requests() {
        let response = answer("window/showDocument", &json!("x"), None);
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn registration_is_acknowledged() {
        let response = answer("client/registerCapability", &json!(1), None);
        assert_eq!(response["result"], Value::Null);
    }
}
