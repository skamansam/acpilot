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

use acpilot_proto::agent::AgentSummary;
use acpilot_proto::event::SessionEvent;
use acpilot_proto::permission::{PermissionDecision, PermissionOption, PermissionRequest};
use acpilot_proto::session::{
    CancelAccepted, CreateSessionRequest, PromptAccepted, PromptRequest, SessionSummary,
};
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
    base: String,
}

impl Client {
    /// A client for the daemon at `base`, for example `http://127.0.0.1:7777`.
    ///
    /// A trailing slash is dropped so every path is built the same way.
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        let base = base.into();
        Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    /// The base URL this client was built with.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
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

    /// `GET /v1/sessions/{id}`.
    pub async fn session(&self, id: &str) -> Result<SessionSummary, ClientError> {
        self.get(&format!("/v1/sessions/{id}")).await
    }

    /// `POST /v1/sessions`.
    pub async fn create_session(
        &self,
        request: &CreateSessionRequest,
    ) -> Result<SessionSummary, ClientError> {
        let request = self.http.post(self.url("/v1/sessions")).json(request);
        self.send(request, "/v1/sessions").await
    }

    /// `DELETE /v1/sessions/{id}`.
    pub async fn delete_session(&self, id: &str) -> Result<SessionSummary, ClientError> {
        let path = format!("/v1/sessions/{id}");
        let request = self.http.delete(self.url(&path));
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
        let request = self.http.post(self.url(&path)).json(&body);
        self.send(request, &path).await
    }

    /// `POST /v1/sessions/{id}/cancel`.
    pub async fn cancel(&self, id: &str) -> Result<CancelAccepted, ClientError> {
        let path = format!("/v1/sessions/{id}/cancel");
        let request = self.http.post(self.url(&path));
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
            .post(self.url(&path))
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
            .get(self.url(&path))
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
            .get(self.url(path))
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
        let request = self.http.get(self.url(path));
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
            serde_json::from_value(value["request"].clone())
                .map(ProfileStream::Permission)
                .map_err(|error| {
                    ClientError::Stream(format!("a permission request did not parse: {error}"))
                })
        }
        "heartbeat" => Ok(ProfileStream::Heartbeat),
        "resync" => Ok(ProfileStream::Resync),
        other => Ok(ProfileStream::Other {
            event: other.to_owned(),
            data: frame.data.clone(),
        }),
    }
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
