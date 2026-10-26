//! The token a TCP listener requires.
//!
//! The default transport is a unix socket, and the filesystem is its access
//! control: the directory is `0700` and the socket `0600`, so another local user
//! cannot open it at all. A port inherits none of that, so it does not inherit the
//! exemption from authentication either (ADR 0004 §3, `docs/api.md`).
//!
//! The token itself lives in `acpilot_host::token`: a `0600` file beside the
//! socket, generated on first use, shared with the CLI that rotates it. This module
//! is the decision that reads it, and it reads the file per request rather than at
//! startup — that is what makes `acpilot token rotate` take effect on the next call
//! with nothing restarting, which is the only kind of rotation a person performs on
//! a token they think has leaked.
//!
//! `Authorization: Bearer <token>` on every request, including streams. A missing
//! or wrong token is `401 unauthorized`, in the same envelope as every other
//! failure, because a client that has to parse a `WWW-Authenticate` header to find
//! out what went wrong is a client with a second error path.
//!
//! One route answers without it: the Swagger page, because a browser cannot attach
//! a header to a typed address and that page is where the token gets pasted. It
//! holds no data — the document it renders and every call it makes still need the
//! token, and the page asks for one before it fetches either.

use std::path::PathBuf;
use std::sync::Arc;

use acpilot_proto::ErrorBody;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};

/// The paths that answer without a token: static HTML, and the two pages a person
/// needs before they have a token — the Swagger UI, where one gets pasted, and the
/// web shell, which is where a browser is pointed to find out what is listening.
///
/// `/app/palette/events` is the web shell's own repaint stream. It is open because
/// a browser cannot attach a header to an `EventSource`, and because it carries
/// nothing a token would protect: the palette the shell already serves inline.
/// Anything added to that frame is a change to this list's reason, not just to the
/// frame.
const OPEN: [&str; 3] = ["/docs", "/app", "/app/palette/events"];

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
        Arc::new(path.into()),
        require,
    ))
}

/// Reject a request that does not carry the token.
async fn require(
    State(path): State<Arc<PathBuf>>,
    request: Request,
    next: Next,
) -> Result<Response, Response> {
    if OPEN.contains(&request.uri().path()) {
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
        .map(|(_, token)| token.trim());
    // A file that cannot be read is "no token", not "any token": a daemon whose
    // runtime directory was removed under it must refuse rather than accept.
    let expected = tokio::fs::read_to_string(&*path)
        .await
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|token| !token.is_empty());
    match (presented, expected) {
        (Some(presented), Some(expected)) if acpilot_host::token::same(presented, &expected) => {
            Ok(next.run(request).await)
        }
        _ => Err(unauthorized()),
    }
}

/// The one failure this module produces, in the envelope every route uses.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(ErrorBody::new(
            "unauthorized",
            "this listener requires `Authorization: Bearer <token>`; the token is in the daemon's \
             runtime directory",
        )),
    )
        .into_response()
}
