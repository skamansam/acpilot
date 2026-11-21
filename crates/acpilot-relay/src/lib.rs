//! The chat relay's group, membership and presence routes (D22).
//!
//! The relay is a separate process from the daemon, with its own data directory
//! and one SQLite file per group. It does not run agents, read profiles, or know a
//! workspace. A group is a UUID; a member is a device key and a display name.
//!
//! # Trust
//!
//! v1 is server-trusted: the operator can read what the relay stores. The shape
//! that makes E2EE additive later is kept: a device key signs every request, the
//! group id in a path is checked against the roster before it is used, and a
//! message body has a `kind` and an opaque body rather than a relay-specific
//! message struct. Presence is ephemeral and never written to a group database.
//!
//! # Signed requests
//!
//! A request carries `X-Acpilot-Device`, `X-Acpilot-Timestamp` (Unix seconds) and
//! `X-Acpilot-Signature`. The signature is ed25519 over:
//!
//! ```text
//! METHOD\nPATH\nTIMESTAMP\nSHA256(BODY)
//! ```
//!
//! A timestamp more than five minutes from the relay's clock is refused. The
//! device must be in the group's active roster; a revoked key cannot post, read or
//! publish presence.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// How long a presence row lives after the device last published it.
pub const PRESENCE_TTL: Duration = Duration::from_secs(45);

/// How far a signed request's timestamp may be from the relay's clock.
pub const CLOCK_SKEW: i64 = 300;

/// Why a relay operation failed.
#[derive(Debug, Error)]
pub enum RelayError {
    /// The request is malformed or a signature does not cover it.
    #[error("{0}")]
    BadRequest(String),
    /// The device is not a member, or its signature is not its own.
    #[error("{0}")]
    Unauthorized(String),
    /// The device is known but revoked.
    #[error("{0}")]
    Forbidden(String),
    /// The group or member does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The relay's disk or database refused.
    #[error("the relay's state could not be read or written: {0}")]
    State(String),
}

impl RelayError {
    fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::State(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "invalid_request",
            Self::Unauthorized(_) => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::NotFound(_) => "not_found",
            Self::State(_) => "relay_unavailable",
        }
    }
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        (
            self.status(),
            axum::Json(json!({ "error": self.code(), "message": self.to_string() })),
        )
            .into_response()
    }
}

/// The relay's root and its in-memory presence table.
#[derive(Clone)]
pub struct Relay {
    root: PathBuf,
    presence: Arc<Mutex<HashMap<(String, String), PresenceEntry>>>,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Relay")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Relay {
    /// Open a relay rooted at `root`, creating the index on first use.
    ///
    /// # Errors
    ///
    /// The root or index could not be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, RelayError> {
        let root = root.into();
        std::fs::create_dir_all(root.join("groups")).map_err(state)?;
        let connection = Connection::open(root.join("relay.sqlite")).map_err(state)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE IF NOT EXISTS groups (
                     id         TEXT PRIMARY KEY,
                     name       TEXT NOT NULL,
                     created_at INTEGER NOT NULL
                 );",
            )
            .map_err(state)?;
        Ok(Self {
            root,
            presence: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// The group's directory, after checking the id is a UUID.
    fn group_dir(&self, group: &str) -> Result<PathBuf, RelayError> {
        let id = uuid::Uuid::parse_str(group)
            .map_err(|_| RelayError::BadRequest(format!("`{group}` is not a group id")))?;
        Ok(self.root.join("groups").join(id.to_string()))
    }

    /// The group database, opened after the directory is made.
    fn group_db(&self, group: &str) -> Result<Connection, RelayError> {
        let directory = self.group_dir(group)?;
        std::fs::create_dir_all(&directory).map_err(state)?;
        let connection = Connection::open(directory.join("group.sqlite")).map_err(state)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA foreign_keys = ON;
                 CREATE TABLE IF NOT EXISTS meta (
                     key   TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS members (
                     device       TEXT PRIMARY KEY,
                     person       TEXT NOT NULL,
                     display_name TEXT NOT NULL,
                     inviter      TEXT NOT NULL,
                     joined_at    INTEGER NOT NULL,
                     revoked_at   INTEGER
                 );",
            )
            .map_err(state)?;
        Ok(connection)
    }

    fn index(&self) -> Result<Connection, RelayError> {
        Connection::open(self.root.join("relay.sqlite")).map_err(state)
    }

    fn group_exists(&self, group: &str) -> Result<bool, RelayError> {
        let connection = self.index()?;
        let exists: Option<String> = connection
            .query_row(
                "SELECT id FROM groups WHERE id = ?1",
                params![group],
                |row| row.get(0),
            )
            .optional()
            .map_err(state)?;
        Ok(exists.is_some())
    }

    fn active_member(&self, group: &str, device: &str) -> Result<Option<Member>, RelayError> {
        let connection = self.group_db(group)?;
        connection
            .query_row(
                "SELECT device, person, display_name, inviter, joined_at, revoked_at \
                 FROM members WHERE device = ?1 AND revoked_at IS NULL",
                params![device],
                member_row,
            )
            .optional()
            .map_err(state)
    }

    fn member(&self, group: &str, device: &str) -> Result<Option<Member>, RelayError> {
        let connection = self.group_db(group)?;
        connection
            .query_row(
                "SELECT device, person, display_name, inviter, joined_at, revoked_at \
                 FROM members WHERE device = ?1",
                params![device],
                member_row,
            )
            .optional()
            .map_err(state)
    }

    fn members(&self, group: &str) -> Result<Vec<Member>, RelayError> {
        let connection = self.group_db(group)?;
        let mut statement = connection
            .prepare(
                "SELECT device, person, display_name, inviter, joined_at, revoked_at \
                 FROM members ORDER BY joined_at, device",
            )
            .map_err(state)?;
        let rows = statement
            .query_map([], member_row)
            .map_err(state)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(state)?;
        Ok(rows)
    }

    fn insert_member(&self, group: &str, invite: &UnsignedInvite) -> Result<(), RelayError> {
        let connection = self.group_db(group)?;
        connection
            .execute(
                "INSERT INTO members (device, person, display_name, inviter, joined_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    invite.device,
                    invite.person,
                    invite.display_name,
                    invite.inviter,
                    jiff::Timestamp::now().as_second()
                ],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(code, _)
                    if code.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    RelayError::BadRequest("that device is already a member".to_owned())
                }
                other => state(other),
            })?;
        connection
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('name', ?1)",
                params![invite.name],
            )
            .map_err(state)?;
        Ok(())
    }

    fn revoke(&self, group: &str, device: &str) -> Result<bool, RelayError> {
        let connection = self.group_db(group)?;
        let changed = connection
            .execute(
                "UPDATE members SET revoked_at = ?1 \
                 WHERE device = ?2 AND revoked_at IS NULL",
                params![jiff::Timestamp::now().as_second(), device],
            )
            .map_err(state)?;
        Ok(changed > 0)
    }

    fn publish(&self, group: &str, member: &Member, presence: Presence) {
        let mut table = self.presence.lock().unwrap_or_else(PoisonError::into_inner);
        table.insert(
            (group.to_owned(), member.device.clone()),
            PresenceEntry {
                presence,
                at: Instant::now(),
            },
        );
    }

    fn presence(&self, group: &str) -> Vec<Presence> {
        let now = Instant::now();
        let table = self.presence.lock().unwrap_or_else(PoisonError::into_inner);
        let mut rows: Vec<Presence> = table
            .iter()
            .filter(|((known, _), entry)| {
                known == group && now.duration_since(entry.at) <= PRESENCE_TTL
            })
            .map(|(_, entry)| entry.presence.clone())
            .collect();
        rows.sort_by(|left, right| left.device.cmp(&right.device));
        rows
    }

    fn forget(&self, group: &str, device: &str) {
        self.presence
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(group.to_owned(), device.to_owned()));
    }
}

/// The HTTP router.
pub fn router(relay: Relay) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/groups", post(create_group))
        .route("/v1/groups/{group}/join", post(join_group))
        .route("/v1/groups/{group}/members", get(list_members))
        .route("/v1/groups/{group}/leave", post(leave_group))
        .route(
            "/v1/groups/{group}/presence",
            get(read_presence).post(write_presence),
        )
        .with_state(relay)
}

/// `GET /v1/health`: the relay is up. No group, member or message is named.
async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(json!({ "ok": true, "service": "acpilot-relay", "api": "v1" }))
}

/// A create body: a self-signed first invite.
#[derive(Debug, Deserialize)]
struct CreateBody {
    invite: UnsignedInvite,
    signature: String,
}

/// `POST /v1/groups`: make a group and put its first device on the roster.
async fn create_group(
    State(relay): State<Relay>,
    body: Bytes,
) -> Result<axum::Json<serde_json::Value>, RelayError> {
    let create: CreateBody = serde_json::from_slice(&body)
        .map_err(|error| RelayError::BadRequest(format!("the create body is not JSON: {error}")))?;
    if create.invite.inviter != create.invite.device {
        return Err(RelayError::BadRequest(
            "the first invite must be signed by the device that creates the group".to_owned(),
        ));
    }
    verify_invite(&create.invite, &create.signature)?;
    check_invite_time(&create.invite)?;
    if relay.group_exists(&create.invite.group)? {
        return Err(RelayError::BadRequest(
            "that group already exists; join it with a signed invite".to_owned(),
        ));
    }
    relay.group_db(&create.invite.group)?;
    relay.insert_member(&create.invite.group, &create.invite)?;
    relay
        .index()?
        .execute(
            "INSERT INTO groups (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![
                create.invite.group,
                create.invite.name,
                jiff::Timestamp::now().as_second()
            ],
        )
        .map_err(state)?;
    let members = relay.members(&create.invite.group)?;
    Ok(axum::Json(json!({
        "group": create.invite.group,
        "invite": { "invite": create.invite, "signature": create.signature },
        "members": members,
    })))
}

/// A join body: an invite the inviter signed.
#[derive(Debug, Deserialize)]
struct JoinBody {
    invite: UnsignedInvite,
    signature: String,
}

/// `POST /v1/groups/{group}/join`: add the invited device to the roster.
async fn join_group(
    State(relay): State<Relay>,
    AxumPath(group): AxumPath<String>,
    body: Bytes,
) -> Result<axum::Json<serde_json::Value>, RelayError> {
    let join: JoinBody = serde_json::from_slice(&body)
        .map_err(|error| RelayError::BadRequest(format!("the join body is not JSON: {error}")))?;
    if join.invite.group != group {
        return Err(RelayError::BadRequest(
            "the invite names a different group".to_owned(),
        ));
    }
    verify_invite(&join.invite, &join.signature)?;
    check_invite_time(&join.invite)?;
    if !relay.group_exists(&group)? {
        return Err(RelayError::NotFound(format!("no group `{group}`")));
    }
    if relay.active_member(&group, &join.invite.inviter)?.is_none() {
        return Err(RelayError::Forbidden(
            "the invite's signer is not an active member".to_owned(),
        ));
    }
    if relay.member(&group, &join.invite.device)?.is_some() {
        return Err(RelayError::BadRequest(
            "that device already joined this group".to_owned(),
        ));
    }
    relay.insert_member(&group, &join.invite)?;
    let members = relay.members(&group)?;
    Ok(axum::Json(json!({ "group": group, "members": members })))
}

/// `GET /v1/groups/{group}/members`: the signed roster, including revocations.
async fn list_members(
    State(relay): State<Relay>,
    AxumPath(group): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::Json<serde_json::Value>, RelayError> {
    let path = format!("/v1/groups/{group}/members");
    let member = authenticate(&relay, &group, &Method::GET, &path, &headers, &body)?;
    Ok(axum::Json(json!({
        "group": group,
        "members": relay.members(&group)?,
        "caller": member.device,
    })))
}

/// `POST /v1/groups/{group}/leave`: revoke this device and drop its presence.
async fn leave_group(
    State(relay): State<Relay>,
    AxumPath(group): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, RelayError> {
    let path = format!("/v1/groups/{group}/leave");
    let member = authenticate(&relay, &group, &Method::POST, &path, &headers, &body)?;
    if !relay.revoke(&group, &member.device)? {
        return Err(RelayError::Forbidden(
            "this device is not an active member".to_owned(),
        ));
    }
    relay.forget(&group, &member.device);
    Ok(StatusCode::NO_CONTENT)
}

/// A cursor frame a device publishes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Presence {
    /// The device that is publishing.
    pub device: String,
    /// The person id that device belongs to.
    pub person: String,
    /// The workspace the cursor is in.
    pub workspace: String,
    /// The file, workspace-relative.
    pub path: String,
    /// Zero-based line.
    pub line: u32,
    /// Zero-based character.
    pub character: u32,
    /// The file version the cursor was drawn over.
    pub etag: String,
    /// When the frame was accepted, Unix seconds.
    pub at: i64,
}

#[derive(Debug, Clone)]
struct PresenceEntry {
    presence: Presence,
    at: Instant,
}

/// What a device publishes: the cursor, without the relay's answer fields.
#[derive(Debug, Deserialize)]
struct PresenceInput {
    workspace: String,
    path: String,
    line: u32,
    character: u32,
    etag: String,
}

/// `POST /v1/groups/{group}/presence`: publish one ephemeral cursor frame.
async fn write_presence(
    State(relay): State<Relay>,
    AxumPath(group): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, RelayError> {
    let path = format!("/v1/groups/{group}/presence");
    let member = authenticate(&relay, &group, &Method::POST, &path, &headers, &body)?;
    let input: PresenceInput = serde_json::from_slice(&body).map_err(|error| {
        RelayError::BadRequest(format!("the presence body is not JSON: {error}"))
    })?;
    if input.path.is_empty() || input.workspace.is_empty() {
        return Err(RelayError::BadRequest(
            "a presence frame names a workspace and a path".to_owned(),
        ));
    }
    relay.publish(
        &group,
        &member,
        Presence {
            device: member.device.clone(),
            person: member.person.clone(),
            workspace: input.workspace,
            path: input.path,
            line: input.line,
            character: input.character,
            etag: input.etag,
            at: jiff::Timestamp::now().as_second(),
        },
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/groups/{group}/presence`: the active roster's cursors, stale ones gone.
async fn read_presence(
    State(relay): State<Relay>,
    AxumPath(group): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::Json<serde_json::Value>, RelayError> {
    let path = format!("/v1/groups/{group}/presence");
    authenticate(&relay, &group, &Method::GET, &path, &headers, &body)?;
    Ok(axum::Json(json!({
        "group": group,
        "presence": relay.presence(&group),
    })))
}

/// An invite before its signature is attached.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsignedInvite {
    /// The group UUID.
    pub group: String,
    /// The group's human name.
    pub name: String,
    /// The relay's public base URL, for the joiner's client.
    pub relay: String,
    /// The joining person's random UUID.
    pub person: String,
    /// The joining person's display name.
    pub display_name: String,
    /// The joining device's 64-hex ed25519 public key.
    pub device: String,
    /// The inviting device's key; equal to `device` for the first invite.
    pub inviter: String,
    /// When the invite was made, Unix seconds.
    pub issued_at: i64,
    /// When it stops being accepted, Unix seconds.
    pub expires_at: i64,
}

/// A roster row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    /// 64-hex ed25519 public key.
    pub device: String,
    /// Random person UUID.
    pub person: String,
    /// Display name.
    pub display_name: String,
    /// The device that invited this one.
    pub inviter: String,
    /// When it joined, Unix seconds.
    pub joined_at: i64,
    /// When it left, Unix seconds; `None` while active.
    pub revoked_at: Option<i64>,
}

fn member_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Member> {
    Ok(Member {
        device: row.get(0)?,
        person: row.get(1)?,
        display_name: row.get(2)?,
        inviter: row.get(3)?,
        joined_at: row.get(4)?,
        revoked_at: row.get(5)?,
    })
}

/// The bytes an invite signs.
fn invite_message(invite: &UnsignedInvite) -> Result<Vec<u8>, RelayError> {
    serde_json::to_vec(invite)
        .map_err(|error| RelayError::BadRequest(format!("the invite cannot be encoded: {error}")))
}

fn verify_invite(invite: &UnsignedInvite, signature: &str) -> Result<(), RelayError> {
    let key = public_key(&invite.inviter)?;
    let bytes = decode_hex(signature, 64)
        .map_err(|error| RelayError::BadRequest(format!("the invite signature: {error}")))?;
    let mut raw = [0u8; 64];
    raw.copy_from_slice(&bytes);
    key.verify_strict(&invite_message(invite)?, &Signature::from_bytes(&raw))
        .map_err(|_| {
            RelayError::Unauthorized(
                "the invite is not signed by the key it names as its inviter".to_owned(),
            )
        })
}

fn check_invite_time(invite: &UnsignedInvite) -> Result<(), RelayError> {
    let now = jiff::Timestamp::now().as_second();
    if invite.expires_at <= now {
        return Err(RelayError::BadRequest("that invite has expired".to_owned()));
    }
    if invite.issued_at > now + CLOCK_SKEW {
        return Err(RelayError::BadRequest(
            "that invite is dated in the future".to_owned(),
        ));
    }
    if !valid_group_name(&invite.name) {
        return Err(RelayError::BadRequest(
            "a group name is 1 to 80 characters and not only whitespace".to_owned(),
        ));
    }
    public_key(&invite.device)?;
    Ok(())
}

fn valid_group_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && name.chars().count() <= 80
}

/// Check a request's device signature against the group's active roster.
fn authenticate(
    relay: &Relay,
    group: &str,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Member, RelayError> {
    let device = header(headers, "x-acpilot-device")?;
    let timestamp = header(headers, "x-acpilot-timestamp")?;
    let signature = header(headers, "x-acpilot-signature")?;
    let timestamp: i64 = timestamp.parse().map_err(|_| {
        RelayError::Unauthorized("the request timestamp is not Unix seconds".to_owned())
    })?;
    let now = jiff::Timestamp::now().as_second();
    if (now - timestamp).abs() > CLOCK_SKEW {
        return Err(RelayError::Unauthorized(
            "the request timestamp is outside the five-minute window".to_owned(),
        ));
    }
    let Some(member) = relay.active_member(group, device)? else {
        let known = relay.member(group, device)?;
        return Err(if known.is_some() {
            RelayError::Forbidden("this device has left the group".to_owned())
        } else {
            RelayError::Unauthorized("this device is not a member of the group".to_owned())
        });
    };
    let key = public_key(device)?;
    let bytes = decode_hex(signature, 64)
        .map_err(|error| RelayError::Unauthorized(format!("the request signature: {error}")))?;
    let mut raw = [0u8; 64];
    raw.copy_from_slice(&bytes);
    let message = signable(method, path, timestamp, body);
    key.verify_strict(&message, &Signature::from_bytes(&raw))
        .map_err(|_| {
            RelayError::Unauthorized("the request signature is not this device's".to_owned())
        })?;
    Ok(member)
}

fn signable(method: &Method, path: &str, timestamp: i64, body: &[u8]) -> Vec<u8> {
    format!(
        "{}\n{}\n{}\n{}",
        method.as_str(),
        path,
        timestamp,
        hex(&Sha256::digest(body))
    )
    .into_bytes()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, RelayError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| RelayError::Unauthorized(format!("the request needs `{name}`")))
}

fn public_key(hex_text: &str) -> Result<VerifyingKey, RelayError> {
    let bytes = decode_hex(hex_text, 32)
        .map_err(|error| RelayError::Unauthorized(format!("the device key: {error}")))?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&key)
        .map_err(|error| RelayError::Unauthorized(format!("the device key: {error}")))
}

fn decode_hex(text: &str, length: usize) -> Result<Vec<u8>, String> {
    let text = text.trim();
    if text.len() != length * 2 {
        return Err(format!(
            "expected {} hex characters and got {}",
            length * 2,
            text.len()
        ));
    }
    let mut bytes = Vec::with_capacity(length);
    for pair in text.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(pair).map_err(|error| format!("not hex: {error}"))?;
        bytes.push(u8::from_str_radix(pair, 16).map_err(|error| format!("not hex: {error}"))?);
    }
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn state(error: impl std::fmt::Display) -> RelayError {
    RelayError::State(error.to_string())
}

/// Sign an invite or request the way a device does.
///
/// Public because it is the exact canonical form a client must reproduce, and a
/// test that reconstructed it independently would be testing its own copy.
#[must_use]
pub fn sign_invite(key: &SigningKey, invite: &UnsignedInvite) -> String {
    let bytes = serde_json::to_vec(invite).expect("an invite is serialisable");
    hex(&key.sign(&bytes).to_bytes())
}

/// Sign a request for the relay's canonical form.
#[must_use]
pub fn sign_request(
    key: &SigningKey,
    method: &Method,
    path: &str,
    timestamp: i64,
    body: &[u8],
) -> String {
    hex(&key
        .sign(&signable(method, path, timestamp, body))
        .to_bytes())
}

/// The public half of a signing key, as the roster spells it.
#[must_use]
pub fn device_key(key: &SigningKey) -> String {
    hex(&key.verifying_key().to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn keys(seed: u8) -> (SigningKey, String) {
        let key = SigningKey::from_bytes(&[seed; 32]);
        let device = device_key(&key);
        (key, device)
    }

    fn invite(
        group: &str,
        name: &str,
        person: &str,
        device: &str,
        inviter: &str,
    ) -> UnsignedInvite {
        let now = jiff::Timestamp::now().as_second();
        UnsignedInvite {
            group: group.to_owned(),
            name: name.to_owned(),
            relay: "https://relay.example".to_owned(),
            person: person.to_owned(),
            display_name: person.to_owned(),
            device: device.to_owned(),
            inviter: inviter.to_owned(),
            issued_at: now,
            expires_at: now + 600,
        }
    }

    fn signed(
        method: Method,
        path: &str,
        key: &SigningKey,
        body: &[u8],
    ) -> axum::http::Request<Body> {
        let timestamp = jiff::Timestamp::now().as_second();
        let signature = sign_request(key, &method, path, timestamp, body);
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("x-acpilot-device", device_key(key))
            .header("x-acpilot-timestamp", timestamp.to_string())
            .header("x-acpilot-signature", signature)
            .body(Body::from(body.to_vec()))
            .expect("request")
    }

    async fn json(response: Response) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn a_group_is_created_joined_and_listed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let relay = Relay::open(directory.path()).expect("relay");
        let app = router(relay);
        let (owner, owner_key) = keys(1);
        let (guest, guest_key) = keys(2);
        let group = uuid::Uuid::now_v7().to_string();
        let first = invite(
            &group,
            "weekend project",
            "person-a",
            &owner_key,
            &owner_key,
        );
        let create = json!({ "invite": first, "signature": sign_invite(&owner, &first) });
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::post("/v1/groups")
                    .header("content-type", "application/json")
                    .body(Body::from(create.to_string()))
                    .expect("request"),
            )
            .await
            .expect("create");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{:?}",
            json(response).await
        );

        let second = invite(
            &group,
            "weekend project",
            "person-b",
            &guest_key,
            &owner_key,
        );
        let join = json!({ "invite": second, "signature": sign_invite(&owner, &second) });
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::post(format!("/v1/groups/{group}/join"))
                    .header("content-type", "application/json")
                    .body(Body::from(join.to_string()))
                    .expect("request"),
            )
            .await
            .expect("join");
        assert_eq!(response.status(), StatusCode::OK);

        let path = format!("/v1/groups/{group}/members");
        let response = app
            .clone()
            .oneshot(signed(Method::GET, &path, &guest, b""))
            .await
            .expect("members");
        assert_eq!(response.status(), StatusCode::OK);
        let body = json(response).await;
        assert_eq!(body["members"].as_array().expect("rows").len(), 2);

        let presence = json!({
            "workspace": "thing",
            "path": "src/lib.rs",
            "line": 3,
            "character": 7,
            "etag": "abc"
        });
        let presence_path = format!("/v1/groups/{group}/presence");
        let response = app
            .clone()
            .oneshot(signed(
                Method::POST,
                &presence_path,
                &guest,
                presence.to_string().as_bytes(),
            ))
            .await
            .expect("presence");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let response = app
            .clone()
            .oneshot(signed(Method::GET, &presence_path, &owner, b""))
            .await
            .expect("read presence");
        assert_eq!(response.status(), StatusCode::OK);
        let body = json(response).await;
        assert_eq!(body["presence"].as_array().expect("presence rows").len(), 1);
        assert_eq!(body["presence"][0]["path"], "src/lib.rs");
        assert_eq!(body["presence"][0]["device"], guest_key);
    }

    #[tokio::test]
    async fn a_revoked_device_cannot_publish_presence() {
        let directory = tempfile::tempdir().expect("temp dir");
        let relay = Relay::open(directory.path()).expect("relay");
        let app = router(relay);
        let (owner, owner_key) = keys(3);
        let group = uuid::Uuid::now_v7().to_string();
        let first = invite(&group, "project", "person-a", &owner_key, &owner_key);
        let create = json!({ "invite": first, "signature": sign_invite(&owner, &first) });
        app.clone()
            .oneshot(
                axum::http::Request::post("/v1/groups")
                    .header("content-type", "application/json")
                    .body(Body::from(create.to_string()))
                    .expect("request"),
            )
            .await
            .expect("create");

        let presence = json!({
            "workspace": "thing",
            "path": "src/lib.rs",
            "line": 3,
            "character": 7,
            "etag": "abc"
        });
        let path = format!("/v1/groups/{group}/presence");
        let response = app
            .clone()
            .oneshot(signed(
                Method::POST,
                &path,
                &owner,
                presence.to_string().as_bytes(),
            ))
            .await
            .expect("presence");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = app
            .clone()
            .oneshot(signed(
                Method::POST,
                &format!("/v1/groups/{group}/leave"),
                &owner,
                b"",
            ))
            .await
            .expect("leave");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = app
            .clone()
            .oneshot(signed(
                Method::POST,
                &path,
                &owner,
                presence.to_string().as_bytes(),
            ))
            .await
            .expect("presence after leave");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_unsigned_read_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let relay = Relay::open(directory.path()).expect("relay");
        let app = router(relay);
        let group = uuid::Uuid::now_v7().to_string();
        let response = app
            .oneshot(
                axum::http::Request::get(format!("/v1/groups/{group}/members"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("members");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
