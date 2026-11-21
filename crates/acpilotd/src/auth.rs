//! What a TCP listener requires (ADR 0004 §3, ADR 0013).
//!
//! The default transport is a unix socket, and the filesystem is its access
//! control: the directory is `0700` and the socket `0600`, so another local user
//! cannot open it at all. A port inherits none of that, so it does not inherit the
//! exemption from authentication either.
//!
//! # Two modes, one boundary
//!
//! Before the first account exists the listener is in bootstrap mode: the static
//! `0600` token is accepted, exactly as it was before accounts. Creating the first
//! account (`acpilot auth init`, on the machine the daemon runs on) ends that mode
//! immediately, because this layer asks the account store on every request rather
//! than caching the answer at startup. From then on the bearer is a short-lived
//! Ed25519 JWT from `/v1/auth/login`, and the static token is no longer a
//! credential — a token that survived the policy change would be the policy change
//! not happening.
//!
//! The token itself still lives in `acpilot_host::token`: a `0600` file beside the
//! socket, generated on first use, shared with the CLI that rotates it. This module
//! reads it per request rather than at startup — that is what makes
//! `acpilot token rotate` take effect on the next call with nothing restarting.
//!
//! # The account routes
//!
//! `/v1/auth/status`, `/v1/auth/login`, `/v1/auth/refresh` and `/v1/auth/logout`
//! answer without a token, because they are where a token comes from. `status`
//! carries no secret; the other three require the credential they exchange in the
//! body. Everything else is closed.
//!
//! `Authorization: Bearer <token>` on every request, including streams. A missing
//! or wrong token is `401 unauthorized`, in the same envelope as every other
//! failure, because a client that has to parse a `WWW-Authenticate` header to find
//! out what went wrong is a client with a second error path.
//!
//! A WebSocket a **browser** opens is the one request that cannot carry that
//! header: `new WebSocket(url)` takes a URL and a protocol list, and nothing else.
//! The page therefore offers the token as a subprotocol —
//! `new WebSocket(url, ["acpilot.v1", token])` — and this module accepts a token
//! from `Sec-WebSocket-Protocol` when there is no `Authorization`. That is still a
//! header rather than a query string: it does not reach the daemon's log, the
//! browser's history or a `Referer`, which are the three places the alternative
//! would put a secret. The handshake answers with `acpilot.v1`, never with the
//! token echoed back.
//!
//! One route answers without it: the Swagger page, because a browser cannot attach
//! a header to a typed address and that page is where the token gets pasted. It
//! holds no data — the document it renders and every call it makes still need the
//! token, and the page asks for one before it fetches either.

use std::path::PathBuf;
use std::sync::Arc;

use acpilot_proto::ErrorBody;
use axum::Router;
use axum::extract::{Json, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::accounts::{Accounts, AuthClaims, AuthError};

/// The paths that answer without a token: static HTML, and the two pages a person
/// needs before they have a token — the Swagger UI, where one gets pasted, and the
/// web shell at `/` (also `/app`), which is where a browser is pointed to find out
/// what is listening.
///
/// `/app/palette/events` is the web shell's own repaint stream. It is open because
/// a browser cannot attach a header to an `EventSource`, and because it carries
/// nothing a token would protect: the palette the shell already serves inline.
/// Anything added to that frame is a change to this list's reason, not just to the
/// frame.
const OPEN: [&str; 4] = ["/", "/docs", "/app", "/app/palette/events"];

/// Whether `path` answers without a credential.
///
/// The account routes are open by necessity: a login route behind a login is a
/// loop. They carry no data a token would protect, and each one checks the
/// credential it exchanges in the body.
fn is_open(path: &str) -> bool {
    OPEN.contains(&path) || path.starts_with("/v1/auth/")
}

/// Wrap `router` so every request needs the token in `path`.
///
/// The path, not the token: reading it once here would make a rotation mean a
/// restart, and a restart ends every session the daemon is holding.
///
/// Applied by the TCP listener only: the socket's answer to "may I" is the mode
/// bits, and asking for a token there too would be a second lock on a door that
/// is already shut.
pub fn protect(router: Router, path: impl Into<PathBuf>) -> Router {
    router.layer(middleware::from_fn_with_state(
        Arc::new(Expected::File(path.into())),
        require,
    ))
}

/// Wrap `router` so every request needs `token`, held in memory.
///
/// For a listener that is not a service: the embedded daemon (P6-11) generates
/// its token in the process that embedded it, so there is no file to read and
/// nothing to rotate — the listener ends when that process does. Which paths stay
/// open, and what a refusal looks like, are the same middleware's answers.
pub fn protect_with_token(router: Router, token: String) -> Router {
    router.layer(middleware::from_fn_with_state(
        Arc::new(Expected::Token(token)),
        require,
    ))
}

/// Where a request's answer to "may I" comes from.
#[derive(Debug)]
enum Expected {
    /// A file, read per request so a rotation needs no restart.
    File(PathBuf),
    /// The secret this process generated, for a listener that outlives nothing.
    Token(String),
}

/// Reject a request that does not carry the token.
async fn require(
    State(expected): State<Arc<Expected>>,
    request: Request,
    next: Next,
) -> Result<Response, Response> {
    if is_open(request.uri().path()) {
        return Ok(next.run(request).await);
    }
    // The scheme is case-insensitive (RFC 7235), so `bearer` and `Bearer` are the
    // same word. The token is not.
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim().to_owned());

    // The embedded listener is handed a token and no data directory; it never
    // grows accounts, so its answer stays the token it was given.
    if let Expected::Token(expected) = &*expected {
        let accepted = presented
            .as_deref()
            .is_some_and(|token| acpilot_host::token::same(token, expected))
            || (presented.is_none() && offered_protocol(&request, expected));
        return if accepted {
            Ok(next.run(request).await)
        } else {
            Err(unauthorized(
                "this listener requires `Authorization: Bearer <token>`",
            ))
        };
    }

    // A file that cannot be read is "no token", not "any token": a daemon whose
    // runtime directory was removed under it must refuse rather than accept. The
    // account store is asked per request so `auth init` takes effect immediately.
    let accounts = Accounts::default();
    let has_accounts = match accounts.has_users() {
        Ok(has_accounts) => has_accounts,
        Err(error) => {
            tracing::warn!(%error, "the account store could not be read; refusing every request");
            return Err(unauthorized(
                "the account store is unreadable, so no credential can be checked",
            ));
        }
    };

    if has_accounts {
        let mut request = request;
        if let Some(claims) = presented
            .as_deref()
            .and_then(|token| accounts.verify_access(token).ok())
            .or_else(|| protocol_claims(&request, &accounts))
        {
            request.extensions_mut().insert(claims);
            return Ok(next.run(request).await);
        }
        return Err(unauthorized(
            "this daemon has an account; send the JWT from `POST /v1/auth/login` as \
             `Authorization: Bearer <token>`",
        ));
    }

    // Bootstrap: no account exists, so the static token is the whole boundary.
    let expected = match &*expected {
        Expected::File(path) => tokio::fs::read_to_string(path)
            .await
            .ok()
            .map(|text| text.trim().to_owned())
            .filter(|token| !token.is_empty()),
        Expected::Token(_) => unreachable!("handled above"),
    };
    let accepted = match (presented.as_deref(), expected.as_deref()) {
        (Some(presented), Some(expected)) => acpilot_host::token::same(presented, expected),
        // Only when no header offered one: a request that sends the wrong bearer
        // token must not be rescued by a subprotocol list it also happens to carry.
        (None, Some(expected)) => offered_protocol(&request, expected),
        _ => false,
    };
    if accepted {
        Ok(next.run(request).await)
    } else {
        Err(unauthorized(
            "this listener requires `Authorization: Bearer <token>`; the token is in the \
             daemon's runtime directory",
        ))
    }
}

/// A valid JWT offered as a WebSocket subprotocol, if one is.
///
/// Browsers cannot attach an `Authorization` header to a WebSocket, so the page
/// sends the token in the protocol list. Each candidate is verified; a wrong one is
/// skipped rather than ending the handshake, and the response still names only
/// `acpilot.v1`.
fn protocol_claims(request: &Request, accounts: &Accounts) -> Option<AuthClaims> {
    request
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .into_iter()
        .flat_map(|offered| offered.split(','))
        .map(str::trim)
        .filter(|name| *name != "acpilot.v1")
        .find_map(|name| accounts.verify_access(name).ok())
}

/// Whether a browser offered the token as a WebSocket subprotocol.
///
/// `Sec-WebSocket-Protocol` is a comma-separated list of names the client accepts,
/// so one of them may be the token: the page sends `["acpilot.v1", token]`. The
/// comparison is constant-time like the header's, and a value that is not valid
/// text — which for this header means not ASCII — is simply not the token.
fn offered_protocol(request: &Request, expected: &str) -> bool {
    request
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|offered| {
            offered
                .split(',')
                .map(str::trim)
                .any(|name| acpilot_host::token::same(name, expected))
        })
}

/// The one failure this module produces, in the envelope every route uses.
fn unauthorized(message: &'static str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorBody::new("unauthorized", message)),
    )
        .into_response()
}

/// The account routes: status, login, refresh, logout.
///
/// They are mounted on the same router as everything else and are in [`is_open`],
/// because a login route behind a login is a loop. A browser gets the same JSON as
/// the CLI; the page's cookie handling is a later client change, not a second
/// route.
pub fn routes() -> Router<crate::AppState> {
    Router::new()
        .route("/v1/auth/status", get(status))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/logout", post(logout))
}

/// What `/v1/auth/status` asks for.
#[derive(Debug, Deserialize)]
struct LoginBody {
    name: String,
    password: String,
}

/// What a refresh or logout carries.
#[derive(Debug, Deserialize)]
struct TokenBody {
    refresh_token: String,
}

/// `GET /v1/auth/status`: whether bootstrap mode is over.
///
/// No credential, and no account name: a caller learns whether there is an account,
/// which `auth init` already tells them, and nothing about who it is.
async fn status() -> Response {
    match Accounts::default().status() {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(error) => failure(error),
    }
}

/// `POST /v1/auth/login`: exchange a name and password for a session.
async fn login(Json(body): Json<LoginBody>) -> Response {
    let accounts = Accounts::default();
    let user = match accounts.authenticate(&body.name, &body.password) {
        Ok(user) => user,
        Err(error) => return failure(error),
    };
    match accounts.issue(&user) {
        Ok(session) => (StatusCode::OK, Json(session)).into_response(),
        Err(error) => failure(error),
    }
}

/// `POST /v1/auth/refresh`: rotate a refresh token into a new pair.
async fn refresh(Json(body): Json<TokenBody>) -> Response {
    match Accounts::default().refresh(&body.refresh_token) {
        Ok(session) => (StatusCode::OK, Json(session)).into_response(),
        Err(error) => failure(error),
    }
}

/// `POST /v1/auth/logout`: revoke one refresh token.
///
/// Always `204`: a caller who logs out with a token that is already gone got what
/// they asked for, and telling an unauthenticated caller whether a token existed is
/// a probe.
async fn logout(Json(body): Json<TokenBody>) -> Response {
    match Accounts::default().logout(&body.refresh_token) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failure(error),
    }
}

/// Render an [`AuthError`] in the envelope every route uses.
fn failure(error: AuthError) -> Response {
    (
        error.status(),
        Json(ErrorBody::new(error.code(), error.to_string())),
    )
        .into_response()
}
