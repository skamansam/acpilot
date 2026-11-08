//! A typed client for the daemon API.
//!
//! Every UI is a client of the same API, so the calls live here once rather than
//! once per frontend. The surface is deliberately thin: one method per route, the
//! wire types from `acpilot-proto`, and [a stream][Client::session_events] for the
//! parts of the API that are events rather than state.
//!
//! # Two kinds of answer
//!
//! A route either returns the type in `acpilot-proto` or the daemon's error
//! envelope. Both are surfaced as-is: [`ClientError::Api`] carries the status, the
//! stable code and the message the daemon wrote, so a caller can decide between
//! "retry", "ask the person" and "this will never work" without parsing prose.
//!
//! # Reconnecting
//!
//! Event streams take a sequence number and send it as `Last-Event-ID`, which is
//! what the daemon replays from. A stream that ends is *not* retried here: the
//! caller owns the policy (a TUI wants one quiet retry, a CLI wants to fail), and
//! a client that reconnected silently would hide a daemon that is flapping.

pub mod daemon;
mod platform;
pub mod terminal;

use acpilot_proto::agent::AgentSummary;
use acpilot_proto::event::SessionEvent;
use acpilot_proto::permission::{PermissionDecision, PermissionOption, PermissionRequest};
use acpilot_proto::profile::Preferences;
use acpilot_proto::session::{
    CancelAccepted, CreateSessionRequest, PromptAccepted, PromptRequest, SessionSummary,
};
use acpilot_proto::terminal::TerminalSummary;
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::{StreamExt, stream};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;

/// Why a call did not produce an answer.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The daemon could not be reached, or the connection failed mid-call.
    #[error("cannot reach the daemon at {url}: {source}")]
    Transport {
        /// The URL that was attempted.
        url: String,
        /// The underlying failure.
        #[source]
        source: reqwest::Error,
    },

    /// The daemon answered with its error envelope.
    #[error("{message}")]
    Api {
        /// HTTP status, for a caller that needs to distinguish 404 from 409.
        status: u16,
        /// Stable machine-readable code, for example `not_found`.
        code: String,
        /// What went wrong, as the daemon phrased it.
        message: String,
    },

    /// A response body was not the shape this client understands.
    #[error("cannot read the daemon's answer from {url}: {message}")]
    Decode {
        /// The URL that was called.
        url: String,
        /// What the parser said.
        message: String,
    },

    /// An event stream failed, or delivered a frame that did not parse.
    #[error("the event stream failed: {0}")]
    Stream(String),

    /// A terminal attach failed: it could not be opened, or the socket broke.
    ///
    /// Separate from [`Self::Stream`] because a terminal is not an event feed a
    /// caller can resync: a pane that lost its socket has to attach again.
    #[error("the terminal failed: {0}")]
    Terminal(String),
}

impl ClientError {
    /// The daemon's error code, when this is an API error.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }

    /// True when the object asked for does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Api { status: 404, .. })
    }
}

/// What the daemon reports at `GET /v1/health`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Health {
    /// Always true when the daemon answers.
    pub ok: bool,
    /// Service name, so a client can tell it reached the right port.
    pub service: String,
    /// Daemon version.
    pub version: String,
    /// API prefix, `/v1`.
    pub api: String,
    /// The workspace this daemon serves, when it has one.
    #[serde(default)]
    pub workspace: Option<String>,
}

/// One frame of a session's event stream.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionStream {
    /// One recorded event.
    Event(SessionEvent),
    /// The stream is quiet but alive; `seq` is where a reconnect should start.
    Heartbeat {
        /// Sequence of the last event delivered.
        seq: u64,
    },
    /// The client fell behind; refetch from `latest_seq` before continuing.
    Resync {
        /// Where to resume.
        latest_seq: u64,
    },
    /// A frame from a newer daemon that this client does not know.
    Other {
        /// Frame name.
        event: String,
        /// Frame body.
        data: String,
    },
}

/// One frame of the profile-wide event stream.
#[derive(Debug, Clone, PartialEq)]
pub enum ProfileStream {
    /// An agent is waiting for an answer.
    Permission(PermissionRequest),
    /// The settings a surface should now apply.
    ///
    /// The whole object, not a diff: it is state, so applying one is replacing
    /// what the client had. The first frame of a connection is one of these too,
    /// which is how a client that cannot read the daemon's files learns the
    /// current preferences at all.
    Preferences(Preferences),
    /// A request somebody else answered.
    ///
    /// The pending list is state, so this is not how a client learns what is
    /// waiting; it is how a client that is *showing* a question hears that another
    /// window answered it, and closes the dialog instead of asking again.
    Resolved {
        /// The request that was answered.
        request_id: String,
        /// What it was answered with.
        option: PermissionOption,
    },
    /// Problems a language server pushed about a file.
    ///
    /// Not an answer to anything the client asked: a server that finishes an index
    /// speaks again on its own, and this is how a pane hears it without waiting for the
    /// next save. The notice names the document and, when the server said, its version,
    /// so a client can tell a late publish from a current one.
    Diagnostics(acpilot_proto::file::DiagnosticsNotice),
    /// The stream is quiet but alive.
    Heartbeat,
    /// The client fell behind; refetch the pending list.
    Resync,
    /// A frame from a newer daemon that this client does not know.
    Other {
        /// Frame name.
        event: String,
        /// Frame body.
        data: String,
    },
}

/// A client for one daemon.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    /// What the caller named: a URL, or a unix socket path.
    base: String,
    /// What requests are addressed to. The same as `base` on TCP, and
    /// `http://localhost` over a socket, where the URL is only a `Host` header.
    target: String,
}

impl Client {
    /// A client for the daemon at `base`, with no token.
    ///
    /// `base` is `http://host:port`, or — on a platform with unix sockets — the
    /// path of the daemon's socket. A trailing slash is dropped so every path is
    /// built the same way. Right for a socket, which asks for no token; use
    /// [`Client::with_token`] for a TCP endpoint.
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        Self::build(base.into(), None)
    }

    /// A client for a daemon on a TCP endpoint, carrying its token.
    ///
    /// The token is the caller's to find — a flag, an environment variable, a file
    /// it read — and never looked up here, so the same call cannot mean two things
    /// in two processes. Ignored for a socket endpoint: that transport's access
    /// control is the filesystem, and a secret in it would be one that buys nothing.
    #[must_use]
    pub fn with_token(base: impl Into<String>, token: impl AsRef<str>) -> Self {
        Self::build(base.into(), Some(token.as_ref()))
    }

    fn build(base: String, token: Option<&str>) -> Self {
        let base = base.trim_end_matches('/').to_owned();
        let (target, http) = platform::http(&base, token);
        Self { http, base, target }
    }

    /// The endpoint this client was built with, as the caller wrote it.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The endpoint as a message should name it.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The URL a request is actually sent to.
    fn request_url(&self, path: &str) -> String {
        format!("{}{path}", self.target)
    }

    /// `GET /v1/health`.
    pub async fn health(&self) -> Result<Health, ClientError> {
        self.get("/v1/health").await
    }

    /// `GET /v1/agents`.
    pub async fn agents(&self) -> Result<Vec<AgentSummary>, ClientError> {
        self.get("/v1/agents").await
    }

    /// `GET /v1/sessions`.
    pub async fn sessions(&self) -> Result<Vec<SessionSummary>, ClientError> {
        self.get("/v1/sessions").await
    }

    /// `GET /v1/stats`.
    ///
    /// The contract's rows, not a drawn bar: `acpilot-stats` turns them into tracks,
    /// and that crate is a client-side dependency on purpose — the daemon reports
    /// figures and every surface measures them the same way.
    pub async fn stats(&self) -> Result<Vec<acpilot_proto::stats::IntegrationStats>, ClientError> {
        self.get("/v1/stats").await
    }

    /// `GET /v1/files/list?path=&include_ignored=`.
    ///
    /// One directory per call: the daemon serves a listing of a path, not a walk of
    /// the workspace, so a tree that opens a directory asks for exactly that one and
    /// pays for what a person is looking at.
    ///
    /// `include_ignored` asks for the rows an ignore rule hides, each marked with
    /// its `ignored` flag: a tree's "show hidden" toggle is the only caller that
    /// sends it, and the marking is what lets a pane draw those rows as the
    /// exception they are.
    pub async fn files(
        &self,
        path: &str,
        include_ignored: bool,
    ) -> Result<Vec<acpilot_proto::file::FileEntry>, ClientError> {
        self.get(&format!(
            "/v1/files/list?path={}&include_ignored={include_ignored}",
            query_value(path)
        ))
        .await
    }

    /// `GET /v1/files/search?q=`.
    ///
    /// Every file in the workspace whose path contains `q`, as the daemon walked
    /// it: the ignore rules are the daemon's, so a find cannot see what the tree
    /// hides — one `tree_respect_gitignore` decides both.
    pub async fn search_files(
        &self,
        query: &str,
    ) -> Result<Vec<acpilot_proto::file::FileEntry>, ClientError> {
        self.get(&format!("/v1/files/search?q={}", query_value(query)))
            .await
    }

    /// `GET /v1/files/grep?q=`.
    ///
    /// The other half of a find: `search_files` answers a path, this answers a word
    /// inside a file. The lines come back with their text and a 1-based number, so a
    /// jump list can be read and chosen from without opening every file first.
    pub async fn grep(
        &self,
        query: &str,
    ) -> Result<Vec<acpilot_proto::file::GrepMatch>, ClientError> {
        self.get(&format!("/v1/files/grep?q={}", query_value(query)))
            .await
    }

    /// `GET /v1/lsp/servers`.
    ///
    /// What the daemon knows about the language servers it can start: which are
    /// running, the encoding each negotiated, and the last references round trip. A
    /// language this build has no server for is not a row — the list is the daemon's
    /// answer to "what could answer a caret", not a catalogue of editors.
    pub async fn lsp_servers(
        &self,
    ) -> Result<Vec<acpilot_proto::lsp::LspServerSummary>, ClientError> {
        self.get("/v1/lsp/servers").await
    }

    /// Where the name is used in code.
    ///
    /// With a caret — the file, the byte `offset` and the buffer's own `text` — this is
    /// `POST /v1/files/references`, and the daemon asks a language server where one is
    /// installed, so the list is scoped: two functions called `new` are two functions.
    /// The text travels because an edited buffer is not the file on disk, and a panel
    /// that listed the saved file's uses would answer about a version nobody is
    /// reading. Without a caret this is the spelling search,
    /// `GET /v1/files/references?q=`, which is what a symbol with no server behind it
    /// still deserves.
    pub async fn references(
        &self,
        name: &str,
        at: Option<(&str, usize, &str)>,
    ) -> Result<Vec<acpilot_proto::file::Reference>, ClientError> {
        let Some((path, offset, text)) = at else {
            let url = format!("/v1/files/references?q={}", query_value(name));
            return self.get(&url).await;
        };
        let body = acpilot_proto::file::ReferencesRequest {
            workspace: None,
            q: name.to_owned(),
            path: path.to_owned(),
            offset: Some(offset),
            text: Some(text.to_owned()),
        };
        let request = self
            .http
            .post(self.request_url("/v1/files/references"))
            .json(&body);
        self.send(request, "/v1/files/references").await
    }

    /// `POST /v1/files/diagnostics`.
    ///
    /// What the daemon's language server says is wrong with a file. The text travels
    /// because an edited buffer is the file the person is reading, and the answer says
    /// which server answered — an empty list from no server at all is not a clean file,
    /// and a status line that drew them the same would be claiming a check nobody ran.
    pub async fn diagnostics(
        &self,
        path: &str,
        text: &str,
    ) -> Result<acpilot_proto::file::FileDiagnostics, ClientError> {
        let body = acpilot_proto::file::DiagnosticsRequest {
            workspace: None,
            path: path.to_owned(),
            text: Some(text.to_owned()),
        };
        let request = self
            .http
            .post(self.request_url("/v1/files/diagnostics"))
            .json(&body);
        self.send(request, "/v1/files/diagnostics").await
    }

    /// `GET /v1/project/map`.
    ///
    /// The map the agent wrote under `.acpilot/`, as areas with a summary and the
    /// areas they lean on. `written` is false when nobody has written one yet, which
    /// is the panel's first state rather than an error.
    pub async fn project_map(&self) -> Result<acpilot_proto::project::ProjectMap, ClientError> {
        self.get("/v1/project/map").await
    }

    /// `PUT /v1/files`.
    ///
    /// The write half of the read, and the reason the read returns a token: a save
    /// that found the file changed underneath it is a `409`, never a silent overwrite.
    /// The daemon serialises writes per file, so this is the only way a buffer becomes
    /// a file.
    pub async fn write_file(
        &self,
        request: &acpilot_proto::file::FileWriteRequest,
    ) -> Result<acpilot_proto::file::FileContent, ClientError> {
        let builder = self.http.put(self.request_url("/v1/files")).json(request);
        self.send(builder, "/v1/files").await
    }

    /// `GET /v1/files?path=`.
    ///
    /// The text plus the ETag a save sends back as `if_match`: the read half of the
    /// write, which is P7-10's.
    pub async fn file(&self, path: &str) -> Result<acpilot_proto::file::FileContent, ClientError> {
        self.get(&format!("/v1/files?path={}", query_value(path)))
            .await
    }

    /// `GET /v1/git/status`.
    ///
    /// Branch state and every changed path, with the index's answer and the working
    /// tree's on one row. The source-control pane reads this and never writes it.
    pub async fn git_status(&self) -> Result<acpilot_proto::git::GitStatus, ClientError> {
        self.get("/v1/git/status").await
    }

    /// `GET /v1/git/diff?scope=all`.
    ///
    /// The patch and its file list, both from one request: the pane draws the files
    /// from the list and the diff from the patch, and two reads could describe two
    /// different moments.
    pub async fn git_diff(&self, scope: &str) -> Result<acpilot_proto::git::GitDiff, ClientError> {
        self.get(&format!("/v1/git/diff?scope={scope}")).await
    }

    /// `POST /v1/git/stage`.
    ///
    /// Adds repository-relative paths to the index and answers with the status read
    /// after git ran. The working tree is untouched.
    pub async fn stage(
        &self,
        paths: &[String],
    ) -> Result<acpilot_proto::git::GitStatus, ClientError> {
        let body = acpilot_proto::git::GitIndexRequest {
            workspace: None,
            paths: paths.to_vec(),
        };
        let request = self
            .http
            .post(self.request_url("/v1/git/stage"))
            .json(&body);
        self.send(request, "/v1/git/stage").await
    }

    /// `POST /v1/git/unstage`.
    ///
    /// Removes repository-relative paths from the index and leaves their working-tree
    /// changes in place.
    pub async fn unstage(
        &self,
        paths: &[String],
    ) -> Result<acpilot_proto::git::GitStatus, ClientError> {
        let body = acpilot_proto::git::GitIndexRequest {
            workspace: None,
            paths: paths.to_vec(),
        };
        let request = self
            .http
            .post(self.request_url("/v1/git/unstage"))
            .json(&body);
        self.send(request, "/v1/git/unstage").await
    }

    /// `POST /v1/git/commit-message`.
    ///
    /// Asks the named session to draft a message from the staged diff. The answer is
    /// a draft: the daemon has not staged or committed anything.
    pub async fn commit_message(
        &self,
        session_id: &str,
    ) -> Result<acpilot_proto::git::CommitDraft, ClientError> {
        let body = acpilot_proto::git::CommitMessageRequest {
            workspace: None,
            session_id: session_id.to_owned(),
            prompt: None,
        };
        let request = self
            .http
            .post(self.request_url("/v1/git/commit-message"))
            .json(&body);
        self.send(request, "/v1/git/commit-message").await
    }

    /// `POST /v1/git/commit`.
    ///
    /// Commits the index with a message the person confirmed. It does not stage
    /// anything first: the index is what the person prepared through [`Self::stage`].
    pub async fn commit(
        &self,
        message: &str,
    ) -> Result<acpilot_proto::git::GitCommit, ClientError> {
        let body = acpilot_proto::git::GitCommitRequest {
            workspace: None,
            message: message.to_owned(),
        };
        let request = self
            .http
            .post(self.request_url("/v1/git/commit"))
            .json(&body);
        self.send(request, "/v1/git/commit").await
    }

    /// `GET /v1/terminals?session=`.
    ///
    /// `None` lists every terminal the daemon owns, which is what a debugger
    /// wants; a pane asks for the ones its session opened.
    pub async fn terminals(
        &self,
        session: Option<&str>,
    ) -> Result<Vec<TerminalSummary>, ClientError> {
        match session {
            Some(id) => self.get(&format!("/v1/terminals?session={id}")).await,
            None => self.get("/v1/terminals").await,
        }
    }

    /// `POST /v1/terminals`.
    ///
    /// Starts the program the request names — the host's own shell when it names
    /// none — and answers with the terminal already running, so the id can be
    /// attached to without racing the program that is supposed to paint it.
    ///
    /// `workspace` is which machine the shell runs on: a remote workspace's terminal
    /// runs on that workspace's host through its helper, so the shell sees the files
    /// the rest of the client is reading. The session that owns the pane is still
    /// this daemon's until sessions run on a host themselves.
    pub async fn create_terminal(
        &self,
        request: &acpilot_proto::terminal::CreateTerminalRequest,
    ) -> Result<TerminalSummary, ClientError> {
        let builder = self
            .http
            .post(self.request_url("/v1/terminals"))
            .json(request);
        self.send(builder, "/v1/terminals").await
    }

    /// `GET /v1/sessions/{id}`.
    pub async fn session(&self, id: &str) -> Result<SessionSummary, ClientError> {
        self.get(&format!("/v1/sessions/{id}")).await
    }

    /// `POST /v1/sessions`.
    pub async fn create_session(
        &self,
        request: &CreateSessionRequest,
    ) -> Result<SessionSummary, ClientError> {
        let request = self
            .http
            .post(self.request_url("/v1/sessions"))
            .json(request);
        self.send(request, "/v1/sessions").await
    }

    /// `DELETE /v1/sessions/{id}`.
    pub async fn delete_session(&self, id: &str) -> Result<SessionSummary, ClientError> {
        let path = format!("/v1/sessions/{id}");
        let request = self.http.delete(self.request_url(&path));
        self.send(request, &path).await
    }

    /// `POST /v1/sessions/{id}/prompt`.
    ///
    /// Returns as soon as the turn is accepted; the answer arrives on
    /// [`Client::session_events`].
    pub async fn prompt(
        &self,
        id: &str,
        text: impl Into<String>,
    ) -> Result<PromptAccepted, ClientError> {
        let path = format!("/v1/sessions/{id}/prompt");
        let body = PromptRequest {
            text: text.into(),
            idempotency_key: None,
        };
        let request = self.http.post(self.request_url(&path)).json(&body);
        self.send(request, &path).await
    }

    /// `POST /v1/sessions/{id}/cancel`.
    pub async fn cancel(&self, id: &str) -> Result<CancelAccepted, ClientError> {
        let path = format!("/v1/sessions/{id}/cancel");
        let request = self.http.post(self.request_url(&path));
        self.send(request, &path).await
    }

    /// `GET /v1/permissions?status=pending`.
    pub async fn pending_permissions(&self) -> Result<Vec<PermissionRequest>, ClientError> {
        self.get("/v1/permissions?status=pending").await
    }

    /// `POST /v1/permissions/{id}/resolve`.
    pub async fn resolve(
        &self,
        id: &str,
        option: PermissionOption,
    ) -> Result<PermissionDecision, ClientError> {
        let path = format!("/v1/permissions/{id}/resolve");
        let request = self
            .http
            .post(self.request_url(&path))
            .json(&serde_json::json!({ "option": option }));
        self.send(request, &path).await
    }

    /// Follow one session's events from `after`.
    ///
    /// `after` is the last sequence the caller has seen; the daemon replays from
    /// there, so a reconnect fills the gap instead of skipping it.
    pub fn session_events(
        &self,
        id: &str,
        after: u64,
    ) -> BoxStream<'static, Result<SessionStream, ClientError>> {
        let client = self.clone();
        let id = id.to_owned();
        // One fallible open, then the frames. A failed open becomes a stream of
        // exactly one error, so the caller handles both the same way.
        stream::once(async move { client.open_session_events(&id, after).await })
            .then(|opened| async move {
                match opened {
                    Ok(frames) => frames,
                    Err(error) => Box::pin(stream::once(async move { Err(error) })),
                }
            })
            .flatten()
            .boxed()
    }

    /// Follow new permission requests for the whole daemon.
    pub fn profile_events(&self) -> BoxStream<'static, Result<ProfileStream, ClientError>> {
        let client = self.clone();
        stream::once(async move { client.open_profile_events().await })
            .then(|opened| async move {
                match opened {
                    Ok(frames) => frames,
                    Err(error) => Box::pin(stream::once(async move { Err(error) })),
                }
            })
            .flatten()
            .boxed()
    }

    async fn open_session_events(
        &self,
        id: &str,
        after: u64,
    ) -> Result<BoxStream<'static, Result<SessionStream, ClientError>>, ClientError> {
        let path = format!("/v1/sessions/{id}/events");
        let response = self
            .http
            .get(self.request_url(&path))
            .header("Last-Event-ID", after.to_string())
            .send()
            .await
            .map_err(|source| self.transport(&path, source))?;
        if !response.status().is_success() {
            return Err(self.api_error(response, &path).await);
        }
        Ok(Box::pin(response.bytes_stream().eventsource().map(
            |frame| match frame {
                Ok(frame) => session_frame(&frame),
                Err(error) => Err(ClientError::Stream(error.to_string())),
            },
        )))
    }

    async fn open_profile_events(
        &self,
    ) -> Result<BoxStream<'static, Result<ProfileStream, ClientError>>, ClientError> {
        let path = "/v1/events";
        let response = self
            .http
            .get(self.request_url(path))
            .send()
            .await
            .map_err(|source| self.transport(path, source))?;
        if !response.status().is_success() {
            return Err(self.api_error(response, path).await);
        }
        Ok(Box::pin(response.bytes_stream().eventsource().map(
            |frame| match frame {
                Ok(frame) => profile_frame(&frame),
                Err(error) => Err(ClientError::Stream(error.to_string())),
            },
        )))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let request = self.http.get(self.request_url(path));
        self.send(request, path).await
    }

    async fn send<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        path: &str,
    ) -> Result<T, ClientError> {
        let response = request
            .send()
            .await
            .map_err(|source| self.transport(path, source))?;
        if !response.status().is_success() {
            return Err(self.api_error(response, path).await);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|source| self.transport(path, source))?;
        serde_json::from_slice(&bytes).map_err(|error| ClientError::Decode {
            url: self.url(path),
            message: error.to_string(),
        })
    }

    fn transport(&self, path: &str, source: reqwest::Error) -> ClientError {
        ClientError::Transport {
            url: self.url(path),
            source,
        }
    }

    /// Turn a failed response into the daemon's own error, when it sent one.
    async fn api_error(&self, response: reqwest::Response, path: &str) -> ClientError {
        let status = response.status();
        let url = self.url(path);
        let body = response.text().await.unwrap_or_default();
        let parsed: Option<acpilot_proto::ErrorBody> = serde_json::from_str(&body).ok();
        match parsed {
            Some(error) => ClientError::Api {
                status: status.as_u16(),
                code: error.code,
                message: error.message,
            },
            // A proxy or a wrong port answers with HTML; say where, not just what.
            None => ClientError::Api {
                status: status.as_u16(),
                code: status_code(status).to_owned(),
                message: format!("{url} answered {status} with a body this API does not define"),
            },
        }
    }
}

/// A query-string value, percent-encoded.
///
/// Hand-rolled for the same reason the clipboard's base64 is: one escape is not
/// worth a dependency, and a path with a space, a `#` or a `?` in it has to survive
/// the round trip. `/` stays as it is — a path reads better in a log than `%2F`.
fn query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn status_code(status: StatusCode) -> &'static str {
    status.canonical_reason().unwrap_or("unknown")
}

fn session_frame(frame: &eventsource_stream::Event) -> Result<SessionStream, ClientError> {
    match frame.event.as_str() {
        "session" => serde_json::from_str(&frame.data)
            .map(SessionStream::Event)
            .map_err(|error| {
                ClientError::Stream(format!("a session event did not parse: {error}"))
            }),
        "heartbeat" => Ok(SessionStream::Heartbeat {
            seq: seq_of(&frame.data),
        }),
        "resync" => Ok(SessionStream::Resync {
            latest_seq: seq_of(&frame.data),
        }),
        other => Ok(SessionStream::Other {
            event: other.to_owned(),
            data: frame.data.clone(),
        }),
    }
}

fn profile_frame(frame: &eventsource_stream::Event) -> Result<ProfileStream, ClientError> {
    match frame.event.as_str() {
        "permission" => {
            let value: serde_json::Value = serde_json::from_str(&frame.data).map_err(|error| {
                ClientError::Stream(format!("a permission frame did not parse: {error}"))
            })?;
            if value["type"].as_str() == Some("resolved") {
                let request_id = value["request_id"].as_str().unwrap_or_default().to_owned();
                let option = serde_json::from_value(value["option"].clone()).map_err(|error| {
                    ClientError::Stream(format!("a decision did not parse: {error}"))
                })?;
                return Ok(ProfileStream::Resolved { request_id, option });
            }
            serde_json::from_value(value["request"].clone())
                .map(ProfileStream::Permission)
                .map_err(|error| {
                    ClientError::Stream(format!("a permission request did not parse: {error}"))
                })
        }
        "prefs" => {
            let value: serde_json::Value = serde_json::from_str(&frame.data).map_err(|error| {
                ClientError::Stream(format!("a prefs frame did not parse: {error}"))
            })?;
            serde_json::from_value(value["preferences"].clone())
                .map(ProfileStream::Preferences)
                .map_err(|error| {
                    ClientError::Stream(format!("the preferences did not parse: {error}"))
                })
        }
        "heartbeat" => Ok(ProfileStream::Heartbeat),
        "diagnostics" => diagnostics_of(&frame.data).map(ProfileStream::Diagnostics),
        "resync" => Ok(ProfileStream::Resync),
        other => Ok(ProfileStream::Other {
            event: other.to_owned(),
            data: frame.data.clone(),
        }),
    }
}

/// The notice in a `diagnostics` frame's body.
///
/// The body wraps it in an object rather than sending it bare, the same way a `prefs`
/// frame names its preferences: a frame with a named field has somewhere to put the
/// next thing the daemon wants to say about it.
fn diagnostics_of(data: &str) -> Result<acpilot_proto::file::DiagnosticsNotice, ClientError> {
    let value: serde_json::Value = serde_json::from_str(data).map_err(|error| {
        ClientError::Stream(format!("a diagnostics frame did not parse: {error}"))
    })?;
    serde_json::from_value(value["diagnostics"].clone()).map_err(|error| {
        ClientError::Stream(format!("a diagnostics notice did not parse: {error}"))
    })
}

/// The `seq` or `latest_seq` in a control frame, `0` when it is missing.
fn seq_of(data: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|value| {
            value
                .get("seq")
                .or_else(|| value.get("latest_seq"))
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_slash_is_dropped() {
        assert_eq!(
            Client::new("http://127.0.0.1:7777/").base(),
            "http://127.0.0.1:7777"
        );
        assert_eq!(
            Client::new("http://127.0.0.1:7777").base(),
            "http://127.0.0.1:7777"
        );
    }

    #[test]
    fn a_path_survives_the_query_string() {
        assert_eq!(
            query_value("crates/acpilot-tui/src/app.rs"),
            "crates/acpilot-tui/src/app.rs"
        );
        assert_eq!(query_value("docs/a b.md"), "docs/a%20b.md");
        assert_eq!(query_value("a#b?c=d"), "a%23b%3Fc%3Dd");
        assert_eq!(
            query_value("caf\u{e9}.rs"),
            "caf%C3%A9.rs",
            "a byte, not a char"
        );
    }

    #[test]
    fn the_listing_and_read_routes_are_where_the_daemon_serves_them() {
        // The paths are the contract: a typo here is a 404 the daemon answers
        // perfectly correctly, so the strings are asserted rather than assumed.
        let client = Client::new("http://127.0.0.1:7777");
        assert_eq!(
            client.url("/v1/files/list?path=src"),
            "http://127.0.0.1:7777/v1/files/list?path=src"
        );
        assert_eq!(
            client.url("/v1/files?path=src/main.rs"),
            "http://127.0.0.1:7777/v1/files?path=src/main.rs"
        );
    }

    #[test]
    fn a_pushed_problem_reads_its_notice_out_of_the_frame() {
        let body = serde_json::json!({
            "diagnostics": {
                "path": "src/main.rs",
                "version": 7,
                "problems": {
                    "server": "rust-analyzer",
                    "encoding": "utf16",
                    "items": [{
                        "line": 3,
                        "column": 5,
                        "end_line": 3,
                        "end_column": 9,
                        "severity": "error",
                        "message": "cannot find value `nope`",
                        "code": "E0425"
                    }]
                }
            }
        })
        .to_string();
        let notice = diagnostics_of(&body).expect("a notice");
        assert_eq!(notice.path, "src/main.rs");
        assert_eq!(notice.version, Some(7));
        assert_eq!(notice.problems.server.as_deref(), Some("rust-analyzer"));
        assert_eq!(notice.problems.items.len(), 1);
        assert_eq!(notice.problems.items[0].line, 3);

        // A frame from a newer daemon that this build cannot read is an error rather
        // than a silent nothing: a gutter that quietly ignored it would look clean.
        assert!(
            diagnostics_of(r#"{"diagnostics":{"path":7}}"#).is_err(),
            "a notice this build cannot read is said out loud"
        );
    }

    #[test]
    fn control_frames_read_their_sequence() {
        assert_eq!(seq_of(r#"{"seq":12}"#), 12);
        assert_eq!(seq_of(r#"{"latest_seq":9}"#), 9);
        assert_eq!(seq_of("not json"), 0);
    }

    #[test]
    fn an_unknown_frame_is_forward_compatible() {
        let frame = eventsource_stream::Event {
            event: "session.renamed".to_owned(),
            data: "{}".to_owned(),
            id: String::new(),
            retry: None,
        };
        match session_frame(&frame).expect("frames parse") {
            SessionStream::Other { event, .. } => assert_eq!(event, "session.renamed"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
