//! The token a TCP listener requires.
//!
//! The default transport is a unix socket, and the filesystem is its access
//! control: the directory is `0700` and the socket `0600`, so another local user
//! cannot open it at all. A port inherits none of that, so it does not inherit the
//! exemption from authentication either (ADR 0004 §3, `docs/api.md`).
//!
//! The token lives in a `0600` file beside the socket, generated on first use. It
//! is not hashed: hashing protects a secret that is *stored where someone can read
//! it*, and this file is already readable only by its owner — a hash would add a
//! second secret that must also be protected, and no protection. What the file
//! buys is that the token survives a restart, so a client does not have to be told
//! a new one.
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

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acpilot_proto::ErrorBody;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};

/// Where the token lives: beside the socket, in the runtime directory.
#[must_use]
pub fn token_path() -> PathBuf {
    acpilot_host::runtime_dir().join("token")
}

/// The token, creating it when this is the first TCP daemon on this machine.
///
/// # Errors
///
/// The directory or file could not be created or made private, or the system
/// random source refused.
pub fn load_or_create(path: &Path) -> io::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let token = existing.trim();
        if !token.is_empty() {
            return Ok(token.to_owned());
        }
    }
    if let Some(directory) = path.parent()
        && !directory.as_os_str().is_empty()
    {
        std::fs::create_dir_all(directory)?;
        acpilot_host::private_dir(directory)?;
    }
    let token = generate()?;
    // Written first, narrowed second: the directory is already `0700`, so the
    // window between the two is not a window another user can use.
    std::fs::write(path, format!("{token}\n"))?;
    acpilot_host::private_file(path)?;
    Ok(token)
}

/// 32 bytes from the system's random source, as hex.
///
/// 256 bits because a token has no rate limit of its own: whatever can reach the
/// port can guess, and the only defence is that guessing is hopeless.
fn generate() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The one path that answers without a token. Static, and about 4 KB of HTML.
const OPEN: &str = "/docs";

/// Wrap `router` so every request needs `token`.
///
/// Applied by the TCP listener only: the socket's answer to "may I" is the mode
/// bits, and asking for a token there too would be a second lock on a door that
/// is already shut.
pub fn protect(router: Router, token: impl Into<String>) -> Router {
    router.layer(middleware::from_fn_with_state(
        Arc::new(token.into()),
        require,
    ))
}

/// Reject a request that does not carry the token.
async fn require(
    State(token): State<Arc<String>>,
    request: Request,
    next: Next,
) -> Result<Response, Response> {
    if request.uri().path() == OPEN {
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
    match presented {
        Some(presented) if same(presented, &token) => Ok(next.run(request).await),
        _ => Err(unauthorized()),
    }
}

/// Compare without stopping at the first differing byte.
///
/// The timing signal of a byte-by-byte comparison is small, and over a network
/// with noise it is very small — but "very small" is the answer that gets written
/// into an incident report rather than the one that prevents it.
fn same(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    presented
        .iter()
        .zip(expected)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_token_is_256_bits_of_hex() {
        let token = generate().expect("random source");
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|character| character.is_ascii_hexdigit()));
        assert_ne!(token, generate().expect("random source"));
    }

    #[test]
    fn the_comparison_is_exact() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
        assert!(!same("", "abc"));
    }

    #[test]
    fn an_existing_token_is_reused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("token");
        std::fs::write(&path, "  from-before  \n").expect("write");
        assert_eq!(load_or_create(&path).expect("loaded"), "from-before");
    }
}
