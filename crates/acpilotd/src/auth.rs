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
//! `/v1/auth/status`, `/v1/auth/login`, `/v1/auth/refresh`, `/v1/auth/logout`,
//! `/v1/auth/session` and the `/v1/auth/oidc/*` round trip answer without a token,
//! because they are where a token comes from. `status` carries no secret; the
//! others require the credential they exchange. `/login` is the browser form that
//! turns a password or an OIDC callback into the `HttpOnly` refresh cookie; the
//! page keeps only the access token.
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
use axum::extract::{Json, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::accounts::{Accounts, AuthClaims, AuthError, BrowserSession};

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
const OPEN: [&str; 5] = ["/", "/docs", "/app", "/app/palette/events", "/login"];

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

/// The account routes: status, login, refresh, logout, the browser session, and
/// the OIDC round trip.
///
/// They are mounted on the same router as everything else and are in [`is_open`],
/// because a login route behind a login is a loop. The JSON login is what the CLI
/// uses; the same answer also sets the browser's `HttpOnly` refresh cookie, so the
/// web page never stores a long-lived token.
pub fn routes() -> Router<crate::AppState> {
    Router::new()
        .route("/v1/auth/status", get(status))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/logout", post(logout))
        .route("/v1/auth/session", get(browser_session))
        .route("/v1/auth/oidc/providers", get(oidc_providers))
        .route("/v1/auth/oidc/start", get(oidc_start))
        .route("/v1/auth/oidc/callback", get(oidc_callback))
        .route("/login", get(login_page))
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
async fn login(headers: HeaderMap, Json(body): Json<LoginBody>) -> Response {
    let accounts = Accounts::default();
    let user = match accounts.authenticate(&body.name, &body.password) {
        Ok(user) => user,
        Err(error) => return failure(error),
    };
    match accounts.issue(&user) {
        Ok(session) => {
            let mut response = (StatusCode::OK, Json(session.clone())).into_response();
            set_session_cookies(&mut response, &session.refresh_token, &headers);
            response
        }
        Err(error) => failure(error),
    }
}

/// `GET /v1/auth/session`: rotate the cookie into a page session.
///
/// The page stores only `access_token`; the replacement refresh token goes back
/// into the `HttpOnly` cookie. A cookie that is missing, revoked or reused is a
/// `401`, which is the page's cue to show the login form.
async fn browser_session(headers: HeaderMap) -> Response {
    let Some(refresh_token) = cookie(&headers, SESSION_COOKIE) else {
        return unauthorized("this browser has no session; sign in at /login");
    };
    let accounts = Accounts::default();
    match accounts.refresh(refresh_token) {
        Ok(session) => {
            let mut response = (
                StatusCode::OK,
                Json(BrowserSession {
                    access_token: session.access_token,
                    expires_in: session.expires_in,
                    user: session.user,
                }),
            )
                .into_response();
            set_session_cookies(&mut response, &session.refresh_token, &headers);
            response
        }
        Err(error) => failure(error),
    }
}

/// `GET /v1/auth/oidc/providers`: the configured provider ids and issuers.
///
/// No client id, secret or endpoint: a page needs enough to draw a button, not
/// enough to impersonate the client.
async fn oidc_providers() -> Response {
    match crate::oidc::providers() {
        Ok(providers) => (
            StatusCode::OK,
            Json(crate::oidc::public_providers(&providers)),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody::new("auth_unavailable", error)),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct StartQuery {
    provider: Option<String>,
}

/// `GET /v1/auth/oidc/start`: begin authorization-code + PKCE.
async fn oidc_start(Query(query): Query<StartQuery>, headers: HeaderMap) -> Response {
    let configured = match crate::oidc::providers() {
        Ok(providers) => providers,
        Err(error) => return redirect_error(&error),
    };
    let Some(provider) = query
        .provider
        .as_deref()
        .or_else(|| configured.first().map(|provider| provider.id.as_str()))
    else {
        return redirect_error("no OIDC provider is configured in auth.toml");
    };
    match crate::oidc::start(provider, &base_url(&headers)).await {
        Ok(url) => Redirect::to(&url).into_response(),
        Err(error) => redirect_error(&error),
    }
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// `GET /v1/auth/oidc/callback`: verify the provider's answer and set the cookie.
async fn oidc_callback(Query(query): Query<CallbackQuery>, headers: HeaderMap) -> Response {
    if let Some(error) = query.error {
        return redirect_error(&query.error_description.unwrap_or(error));
    }
    let (Some(state), Some(code)) = (query.state.as_deref(), query.code.as_deref()) else {
        return redirect_error("the provider returned no authorization code");
    };
    match crate::oidc::callback(state, code, &base_url(&headers)).await {
        Ok(session) => {
            let mut response = Redirect::to("/login?oidc=1").into_response();
            set_session_cookies(&mut response, &session.refresh_token, &headers);
            response
        }
        Err(error) => redirect_error(&error),
    }
}

/// The cookie the browser keeps. The refresh token, never the access token.
const SESSION_COOKIE: &str = "acpilot_session";
const CSRF_COOKIE: &str = "acpilot_csrf";

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .into_iter()
        .flat_map(|cookies| cookies.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn set_session_cookies(response: &mut Response, refresh_token: &str, headers: &HeaderMap) {
    let secure = is_secure(headers);
    let suffix = if secure { "; Secure" } else { "" };
    if let Ok(value) = header::HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={refresh_token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000{suffix}"
    )) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    if let Ok(value) = header::HeaderValue::from_str(&format!(
        "{CSRF_COOKIE}={}; SameSite=Strict; Path=/; Max-Age=2592000{suffix}",
        random_hex(16).unwrap_or_default()
    )) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("https"))
        || headers
            .get(header::FORWARDED)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("proto=https"))
}

fn base_url(headers: &HeaderMap) -> String {
    let scheme = if is_secure(headers) { "https" } else { "http" };
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1");
    format!("{scheme}://{host}")
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|error| error.to_string())?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn redirect_error(message: &str) -> Response {
    let encoded: String = message
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            b' ' => vec!['+'],
            other => format!("%{other:02X}").chars().collect(),
        })
        .collect();
    Redirect::to(&format!("/login?error={encoded}")).into_response()
}

/// The login page: one HTML file, no asset directory, and the same policy as the
/// rest of the daemon's pages. It is open because a browser with no session has to
/// be able to reach the form; everything behind it needs the cookie or a bearer.
async fn login_page() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(LOGIN_PAGE),
    )
        .into_response()
}

const LOGIN_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Sign in · Acpilot</title>
<style>
:root{color-scheme:dark;--bg:#070d18;--panel:#101a2b;--text:#e7edf7;--muted:#8fa0b8;--accent:#5ab0ff;--danger:#ff7b72}
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--text);font:16px/1.5 system-ui,sans-serif}
main{width:min(26rem,92vw);background:var(--panel);border:1px solid #22314a;border-radius:14px;padding:1.6rem}
h1{font-size:1.3rem;margin:0 0 .3rem}p{color:var(--muted);margin:.4rem 0 1rem}
label{display:block;font-size:.85rem;color:var(--muted);margin:.8rem 0 .3rem}
input{width:100%;padding:.65rem .7rem;border-radius:.5rem;border:1px solid #2a3a55;background:#0a1322;color:var(--text);font:inherit}
button,a.button{display:inline-block;margin-top:1rem;padding:.6rem .9rem;border-radius:.5rem;border:1px solid #2a3a55;background:#13233c;color:var(--text);font:inherit;cursor:pointer;text-decoration:none}
button.primary{background:var(--accent);color:#04101f;border-color:var(--accent);font-weight:600}
.error{color:var(--danger)}.providers{margin-top:1rem}.providers a{display:block;margin-top:.5rem}
.hint{font-size:.8rem;color:var(--muted);margin-top:1rem}
</style></head><body><main>
<h1>Sign in to Acpilot</h1>
<p>This daemon is yours. The session stays in this browser; the page keeps only a short-lived access token.</p>
<p id="error" class="error" hidden></p>
<div id="providers" class="providers"></div>
<form id="password" hidden>
<label for="name">Account</label><input id="name" name="name" autocomplete="username" required>
<label for="password-input">Password</label><input id="password-input" name="password" type="password" autocomplete="current-password" required>
<button class="primary" type="submit">Sign in</button>
</form>
<details class="hint"><summary>Use the bootstrap token instead</summary>
<label for="token">Daemon token</label><input id="token" autocomplete="off" spellcheck="false">
<button type="button" id="use-token">Use token</button>
</details>
<p class="hint" id="hint"></p>
</main><script>
const errorBox=document.getElementById('error');
function show(message){errorBox.textContent=message;errorBox.hidden=!message}
function finish(session){sessionStorage.setItem('acpilot.token',session.access_token);location.href='/';}
async function restore(){try{const response=await fetch('/v1/auth/session',{credentials:'same-origin'});if(response.ok){finish(await response.json());return true;}}catch{}return false;}
async function render(){const response=await fetch('/v1/auth/oidc/providers');if(!response.ok)return;const providers=await response.json();const box=document.getElementById('providers');for(const provider of providers){const link=document.createElement('a');link.className='button';link.href='/v1/auth/oidc/start?provider='+encodeURIComponent(provider.id);link.textContent='Sign in with '+provider.id;box.append(link);}}
async function main(){const params=new URLSearchParams(location.search);if(params.get('error'))show(params.get('error'));if(await restore())return;await render();document.getElementById('password').hidden=false;const status=await fetch('/v1/auth/status').then(r=>r.json()).catch(()=>null);if(status&&status.bootstrap){document.getElementById('hint').textContent='No account exists yet. Run `acpilot auth init` on the daemon machine, or use the bootstrap token below.';}document.getElementById('password').addEventListener('submit',async event=>{event.preventDefault();show('');const response=await fetch('/v1/auth/login',{method:'POST',credentials:'same-origin',headers:{'content-type':'application/json'},body:JSON.stringify({name:document.getElementById('name').value,password:document.getElementById('password-input').value})});if(response.ok){finish(await response.json());}else{const body=await response.json().catch(()=>({}));show(body.message||'That account and password were not accepted.');}});document.getElementById('use-token').addEventListener('click',()=>{const token=document.getElementById('token').value.trim();if(token){sessionStorage.setItem('acpilot.token',token);location.href='/';}});}
main();
</script></body></html>"#;

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn a_cookie_is_found_by_its_name_not_position() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=night; acpilot_session=abc123; other=x"),
        );
        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("abc123"));
        assert_eq!(cookie(&headers, "missing"), None);
    }

    #[test]
    fn an_https_proxy_marks_the_refresh_cookie_secure() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let mut response = StatusCode::OK.into_response();
        set_session_cookies(&mut response, "refresh", &headers);
        let cookies: Vec<&str> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert!(
            cookies
                .iter()
                .any(|cookie| cookie.starts_with("acpilot_session=refresh"))
        );
        assert!(
            cookies
                .iter()
                .all(|cookie| cookie.contains("HttpOnly") || cookie.contains("acpilot_csrf"))
        );
        assert!(cookies.iter().all(|cookie| cookie.contains("Secure")));
        assert!(
            cookies
                .iter()
                .any(|cookie| cookie.contains("SameSite=Strict"))
        );
    }

    #[test]
    fn a_plain_http_request_does_not_set_secure() {
        let headers = HeaderMap::new();
        let mut response = StatusCode::OK.into_response();
        set_session_cookies(&mut response, "refresh", &headers);
        assert!(
            !response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(|cookie| cookie.contains("Secure"))
        );
    }

    #[test]
    fn the_login_page_names_both_ways_in() {
        assert!(LOGIN_PAGE.contains("/v1/auth/session"));
        assert!(LOGIN_PAGE.contains("/v1/auth/oidc/providers"));
        assert!(LOGIN_PAGE.contains("/v1/auth/login"));
    }
}
