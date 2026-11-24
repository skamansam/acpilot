//! Accounts, passwords, and the JWT a TCP client carries (ADR 0013).
//!
//! The socket's access control is the filesystem, so this module is about the
//! listener that has none. A person creates one local account on the machine the
//! daemon runs on; logging in returns a short-lived Ed25519-signed access token and
//! a long-lived refresh token, and the static daemon token stops being accepted as
//! soon as the account exists.
//!
//! # Why the database is not in a profile
//!
//! Profiles isolate work, not people (ADR 0007). An account that existed in one
//! profile and not another would make "who are you" depend on a header, and the
//! first cross-profile request would be a second identity model. `users.sqlite`
//! therefore sits in the data root, beside the global index, and a future shared
//! daemon changes policy rather than schema.
//!
//! # Why the signing key is a file
//!
//! The daemon is a long-running process that restarts; a key generated at startup
//! would invalidate every access token on every restart. The key lives beside the
//! database in a `0600` file and is read per request, the same choice the static
//! token already makes. It is not in the database because a database copied for a
//! backup should not be a database that can mint credentials.
//!
//! # OIDC
//!
//! OIDC is the second way to obtain the same JWT, and it lands in this store as
//! an identity row: `(issuer, subject)` names the account, never an email. The
//! browser round trip and JWKS verification live in [`crate::oidc`]; this module
//! owns the pending `state` row, the link, and the session it issues.

use std::path::{Path, PathBuf};

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use ed25519_dalek::{Signature, Signer, SigningKey};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// How long an access token is valid.
///
/// Fifteen minutes is short enough that a stolen one is a window rather than a
/// session, and long enough that a client is not refreshing on every keystroke.
pub const ACCESS_TTL_SECONDS: i64 = 15 * 60;

/// How long a refresh token is valid.
///
/// Thirty days, rotated on every use. A refresh token that is presented twice is
/// evidence of a copy, which is why the second presentation revokes the family
/// rather than issuing another pair.
pub const REFRESH_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

/// The shortest password this store accepts.
///
/// Twelve characters, not eight: the account is reachable from any network the
/// daemon is bound to, and there is no lockout counter to make a short password
/// expensive to guess.
pub const MIN_PASSWORD: usize = 12;

/// The longest password this store accepts. Bcrypt's old 72-byte limit is not
/// Argon2's, but a megabyte password is a denial-of-service request, not a secret.
pub const MAX_PASSWORD: usize = 1024;

/// The issuer claim this daemon writes, and the only one it accepts.
const ISSUER: &str = "acpilot";

/// The JWT header, base64url-encoded: `{"alg":"EdDSA","typ":"JWT"}`.
///
/// Constant because there is exactly one algorithm this issuer signs with. A token
/// naming another is refused before its signature is looked at, which is the
/// algorithm-confusion defence in one line.
const HEADER: &str = "eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9";

/// A person this daemon knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Stable id, the JWT's `sub`.
    pub id: String,
    /// The name a person types. Unique, case-insensitively.
    pub name: String,
}

/// A signed-in session: one access token and the refresh token that replaces it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// The bearer token a client sends on every request.
    pub access_token: String,
    /// The opaque token a client presents to `/v1/auth/refresh`.
    pub refresh_token: String,
    /// Access-token lifetime in seconds.
    pub expires_in: i64,
    /// Who the tokens are for.
    pub user: User,
}

/// A browser session: the access token the page keeps in memory, without the
/// refresh token, which stays in an `HttpOnly` cookie.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserSession {
    /// The bearer token for API calls.
    pub access_token: String,
    /// Access-token lifetime in seconds.
    pub expires_in: i64,
    /// Who the tokens are for.
    pub user: User,
}

/// A pending OIDC authorization, keyed by the `state` the provider returns.
#[derive(Debug, Clone)]
pub struct PendingOidc {
    /// The configured provider id.
    pub provider: String,
    /// The nonce the ID token must carry.
    pub nonce: String,
    /// The PKCE verifier; the challenge was sent on the authorization request.
    pub verifier: String,
    /// When this row stops being accepted, Unix seconds.
    pub expires_at: i64,
}

/// The claims this daemon reads out of an access token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthClaims {
    /// Issuer, always [`ISSUER`].
    pub iss: String,
    /// The user id.
    pub sub: String,
    /// The name at issue time, for a log line.
    pub name: String,
    /// Issued at, Unix seconds.
    pub iat: i64,
    /// Expires at, Unix seconds.
    pub exp: i64,
    /// The token id, so a logout or a password change can revoke this one.
    pub jti: String,
    /// The user's token epoch. A password change increments it, which revokes
    /// every access token issued before the change without waiting for `exp`.
    pub ver: i64,
}

/// How many accounts this daemon has, and whether it is still in bootstrap mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthStatus {
    /// Whether any account exists.
    pub accounts: bool,
    /// True while the static daemon token is still accepted.
    pub bootstrap: bool,
    /// How many accounts exist.
    pub users: u64,
}

/// Why an account operation failed.
#[derive(Debug, Error)]
pub enum AuthError {
    /// No account exists yet; `acpilot auth init` is the local-only way to make one.
    #[error("this daemon has no account yet; run `acpilot auth init` on the machine it runs on")]
    NoAccounts,
    /// First-account creation was attempted twice.
    #[error("this daemon already has an account; use `acpilot auth passwd` to change its password")]
    AlreadyInitialized,
    /// The name or password did not match.
    #[error("no account matches that name and password")]
    InvalidCredentials,
    /// The access token is past its `exp`.
    #[error("the access token has expired; refresh it or log in again")]
    Expired,
    /// The refresh token was already used, which means it was copied.
    #[error("that refresh token was already used; every token in its family has been revoked")]
    Reused,
    /// The refresh token was revoked by a logout or a password change.
    #[error("that refresh token has been revoked")]
    Revoked,
    /// An OIDC identity is already linked to an account.
    #[error("that provider identity is already linked")]
    AlreadyLinked,
    /// A caller's input was outside the contract.
    #[error("{0}")]
    Invalid(String),
    /// The database or key file refused.
    #[error("the account store could not be read or written: {0}")]
    State(String),
}

impl AuthError {
    /// The error code a client sees.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoAccounts => "auth_not_initialized",
            Self::AlreadyInitialized => "auth_already_initialized",
            Self::InvalidCredentials => "invalid_credentials",
            Self::AlreadyLinked => "identity_already_linked",
            Self::Expired => "token_expired",
            Self::Reused | Self::Revoked => "invalid_token",
            Self::Invalid(_) => "invalid_request",
            Self::State(_) => "auth_unavailable",
        }
    }

    /// The HTTP status for this failure.
    #[must_use]
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::NoAccounts | Self::AlreadyInitialized => StatusCode::CONFLICT,
            Self::InvalidCredentials | Self::Expired | Self::Reused | Self::Revoked => {
                StatusCode::UNAUTHORIZED
            }
            Self::AlreadyLinked => StatusCode::CONFLICT,
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::State(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// The users, refresh tokens, revocations and key for one daemon.
///
/// Cloning is free: the path is the handle, and each call opens the database. That
/// is deliberate — the daemon's middleware runs on every request, and a connection
/// held in a mutex would serialise every stream behind a login. SQLite is the lock.
#[derive(Debug, Clone)]
pub struct Accounts {
    path: PathBuf,
    key: PathBuf,
}

impl Default for Accounts {
    fn default() -> Self {
        Self::at(default_path())
    }
}

/// Where the account database lives: the data root, not a profile.
#[must_use]
pub fn default_path() -> PathBuf {
    acpilot_profile::data_dir().join("users.sqlite")
}

/// How long one open waits for a lock before SQLite reports it.
const OPEN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// How many times a busy store is retried before a request fails.
const OPEN_ATTEMPTS: usize = 40;

/// The pause between those attempts.
const OPEN_PAUSE: std::time::Duration = std::time::Duration::from_millis(25);

/// Whether `error` says another process holds the store, which is worth a retry.
///
/// A busy timeout covers a plain lock wait. A journal mode switch does not always
/// honour it, so the open that loses that race comes back as an error instead of
/// waiting. The store is shared with every other process that runs as this user,
/// which on a test machine is every test binary at once.
fn busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

impl Accounts {
    /// Use the database at `path`, with the signing key beside it.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let key = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("auth")
            .join("jwt.key");
        Self { path, key }
    }

    /// A handle for a test, with both files under one directory.
    #[must_use]
    pub fn in_directory(directory: impl AsRef<Path>) -> Self {
        Self::at(directory.as_ref().join("users.sqlite"))
    }

    /// The database path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open the database, creating the file and its directory on first use.
    ///
    /// The schema is idempotent, so this also migrates a database written by an
    /// older build that had fewer tables. `user_version` is not used yet: version 1
    /// is the only schema, and inventing a migration runner before there is a second
    /// version would be a second thing to keep right.
    fn connection(&self) -> Result<Connection, AuthError> {
        if let Some(directory) = self.path.parent()
            && !directory.as_os_str().is_empty()
        {
            std::fs::create_dir_all(directory).map_err(state)?;
            acpilot_host::private_dir(directory).map_err(state)?;
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.open_once() {
                Ok(connection) => {
                    acpilot_host::private_file(&self.path).map_err(state)?;
                    return Ok(connection);
                }
                Err(error) if attempt < OPEN_ATTEMPTS && busy(&error) => {
                    std::thread::sleep(OPEN_PAUSE);
                }
                Err(error) => return Err(state(error)),
            }
        }
    }

    /// One attempt at opening the store: the pragmas, then the schema.
    fn open_once(&self) -> rusqlite::Result<Connection> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(OPEN_WAIT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(SCHEMA)?;
        Ok(connection)
    }

    /// Whether an account exists, which is also whether bootstrap mode is over.
    ///
    /// # Errors
    ///
    /// The database could not be opened or read. The caller is expected to fail
    /// closed: an unreadable account store is not permission to accept the old
    /// static token.
    pub fn has_users(&self) -> Result<bool, AuthError> {
        let connection = self.connection()?;
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
            .map_err(state)?;
        Ok(count > 0)
    }

    /// How many accounts exist, for the status route.
    ///
    /// # Errors
    ///
    /// The database could not be opened.
    pub fn status(&self) -> Result<AuthStatus, AuthError> {
        let connection = self.connection()?;
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
            .map_err(state)?;
        let users = u64::try_from(count).unwrap_or(0);
        Ok(AuthStatus {
            accounts: users > 0,
            bootstrap: users == 0,
            users,
        })
    }

    /// Create the first account.
    ///
    /// Only the first: a second account is a policy change (a shared daemon), not a
    /// CLI verb. The name is trimmed and limited; the password is hashed with
    /// Argon2id before anything is written.
    ///
    /// # Errors
    ///
    /// An account already exists, the input is outside the contract, or the write
    /// failed.
    pub fn create_owner(&self, name: &str, password: &str) -> Result<User, AuthError> {
        let name = valid_name(name)?;
        valid_password(password)?;
        let mut connection = self.connection()?;
        let existing: i64 = connection
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
            .map_err(state)?;
        if existing > 0 {
            return Err(AuthError::AlreadyInitialized);
        }
        let hash = hash_password(password)?;
        let now = now();
        let user = User {
            id: format!("usr-{}", uuid::Uuid::now_v7()),
            name,
        };
        let transaction = connection.transaction().map_err(state)?;
        transaction
            .execute(
                "INSERT INTO users (id, name, password_hash, created_at, changed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params![user.id, user.name, hash, now],
            )
            .map_err(state)?;
        transaction
            .execute(
                "INSERT INTO auth_events (at, event, user_id, detail) VALUES (?1, 'created', ?2, NULL)",
                params![now, user.id],
            )
            .map_err(state)?;
        transaction.commit().map_err(state)?;
        Ok(user)
    }

    /// Check a name and password and return the account behind them.
    ///
    /// # Errors
    ///
    /// [`AuthError::NoAccounts`] before `init`, or
    /// [`AuthError::InvalidCredentials`] for a wrong name or password. The two are
    /// distinct so a person can be told to run `init`; neither says which half was
    /// wrong.
    pub fn authenticate(&self, name: &str, password: &str) -> Result<User, AuthError> {
        let connection = self.connection()?;
        if !self.has_users()? {
            return Err(AuthError::NoAccounts);
        }
        let found: Option<(String, String, String)> = connection
            .query_row(
                "SELECT id, name, password_hash FROM users WHERE name = ?1 COLLATE NOCASE",
                params![name.trim()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(state)?;
        let Some((id, stored_name, hash)) = found else {
            // Hash the presented password anyway. Skipping it for an unknown name
            // would make "no such user" measurably faster than "wrong password".
            let _ = hash_password(password);
            return Err(AuthError::InvalidCredentials);
        };
        let argon = hasher()?;
        let verified = argon
            .verify_password(password.as_bytes(), hash.as_str())
            .is_ok();
        if !verified {
            return Err(AuthError::InvalidCredentials);
        }
        Ok(User {
            id,
            name: stored_name,
        })
    }

    /// Mint an access token and a refresh token for `user`.
    ///
    /// # Errors
    ///
    /// The signing key or the database could not be written.
    pub fn issue(&self, user: &User) -> Result<Session, AuthError> {
        self.issue_in_family(user, &uuid::Uuid::now_v7().to_string())
    }

    /// Mint a fresh access token for `user` without creating a refresh token.
    fn issue_access(&self, user: &User) -> Result<String, AuthError> {
        let connection = self.connection()?;
        let epoch: i64 = connection
            .query_row(
                "SELECT token_epoch FROM users WHERE id = ?1",
                params![user.id],
                |row| row.get(0),
            )
            .optional()
            .map_err(state)?
            .ok_or(AuthError::InvalidCredentials)?;
        let now = now();
        let claims = AuthClaims {
            iss: ISSUER.to_owned(),
            sub: user.id.clone(),
            name: user.name.clone(),
            iat: now,
            exp: now + ACCESS_TTL_SECONDS,
            jti: uuid::Uuid::now_v7().to_string(),
            ver: epoch,
        };
        sign(&self.key, &claims)
    }

    /// Exchange a still-valid refresh token for an access token, without rotating it.
    ///
    /// The browser's refresh token is in an `HttpOnly` cookie; the page gets only the
    /// short-lived access token, which it keeps in memory. The token is not consumed
    /// here because a page load is not a refresh rotation; `/v1/auth/refresh` is the
    /// rotating path.
    ///
    /// # Errors
    ///
    /// Unknown, expired, revoked, or reused token; or the key could not be read.
    pub fn access_for_refresh(&self, token: &str) -> Result<BrowserSession, AuthError> {
        let connection = self.connection()?;
        let now = now();
        let row: Option<(String, i64, Option<i64>, Option<i64>)> = connection
            .query_row(
                "SELECT user_id, expires_at, used_at, revoked_at FROM refresh_tokens WHERE hash = ?1",
                params![digest(token.trim())],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(state)?;
        let Some((user_id, expires_at, used_at, revoked_at)) = row else {
            return Err(AuthError::InvalidCredentials);
        };
        if used_at.is_some() {
            return Err(AuthError::Reused);
        }
        if revoked_at.is_some() {
            return Err(AuthError::Revoked);
        }
        if expires_at <= now {
            return Err(AuthError::Expired);
        }
        let user: User = connection
            .query_row(
                "SELECT id, name FROM users WHERE id = ?1",
                params![user_id],
                |row| {
                    Ok(User {
                        id: row.get(0)?,
                        name: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(state)?
            .ok_or(AuthError::InvalidCredentials)?;
        Ok(BrowserSession {
            access_token: self.issue_access(&user)?,
            expires_in: ACCESS_TTL_SECONDS,
            user,
        })
    }

    /// Mint a session whose refresh token belongs to `family`.
    fn issue_in_family(&self, user: &User, family: &str) -> Result<Session, AuthError> {
        let now = now();
        let connection = self.connection()?;
        let access_token = self.issue_access(user)?;
        let refresh_token = random_hex()?;
        let hash = digest(&refresh_token);
        connection
            .execute(
                "INSERT INTO refresh_tokens \
                 (id, user_id, family, hash, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    uuid::Uuid::now_v7().to_string(),
                    user.id,
                    family,
                    hash,
                    now,
                    now + REFRESH_TTL_SECONDS
                ],
            )
            .map_err(state)?;
        Ok(Session {
            access_token,
            refresh_token,
            expires_in: ACCESS_TTL_SECONDS,
            user: user.clone(),
        })
    }

    /// Exchange a refresh token for a new pair, rotating the old one.
    ///
    /// Reuse is treated as compromise: if the row already has `used_at`, every
    /// token in its family is revoked and the request fails. That turns a copied
    /// token into a lockout, which is the only useful response when the daemon
    /// cannot tell the copy from the original.
    ///
    /// # Errors
    ///
    /// Unknown, expired, revoked, or reused token; or the write failed.
    pub fn refresh(&self, token: &str) -> Result<Session, AuthError> {
        let mut connection = self.connection()?;
        let hash = digest(token.trim());
        let transaction = connection.transaction().map_err(state)?;
        let found: Option<RefreshRow> = transaction
            .query_row(
                "SELECT id, user_id, family, expires_at, used_at, revoked_at \
                 FROM refresh_tokens WHERE hash = ?1",
                params![hash],
                |row| {
                    Ok(RefreshRow {
                        id: row.get(0)?,
                        user_id: row.get(1)?,
                        family: row.get(2)?,
                        expires_at: row.get(3)?,
                        used_at: row.get(4)?,
                        revoked_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(state)?;
        let Some(row) = found else {
            return Err(AuthError::InvalidCredentials);
        };
        if row.used_at.is_some() {
            transaction
                .execute(
                    "UPDATE refresh_tokens SET revoked_at = ?1 WHERE family = ?2 AND revoked_at IS NULL",
                    params![now(), row.family],
                )
                .map_err(state)?;
            transaction
                .execute(
                    "INSERT INTO auth_events (at, event, user_id, detail) VALUES (?1, 'reuse', ?2, ?3)",
                    params![now(), row.user_id, row.family],
                )
                .map_err(state)?;
            transaction.commit().map_err(state)?;
            return Err(AuthError::Reused);
        }
        if row.revoked_at.is_some() {
            return Err(AuthError::Revoked);
        }
        if row.expires_at <= now() {
            return Err(AuthError::Expired);
        }
        transaction
            .execute(
                "UPDATE refresh_tokens SET used_at = ?1 WHERE id = ?2",
                params![now(), row.id],
            )
            .map_err(state)?;
        let user: (String, String) = transaction
            .query_row(
                "SELECT id, name FROM users WHERE id = ?1",
                params![row.user_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(state)?;
        transaction.commit().map_err(state)?;
        self.issue_in_family(
            &User {
                id: user.0,
                name: user.1,
            },
            &row.family,
        )
    }

    /// Revoke one refresh token. Unknown tokens are a successful logout: the caller
    /// wanted it unusable, and it is.
    ///
    /// # Errors
    ///
    /// The database could not be written.
    pub fn logout(&self, token: &str) -> Result<(), AuthError> {
        let connection = self.connection()?;
        connection
            .execute(
                "UPDATE refresh_tokens SET revoked_at = ?1 WHERE hash = ?2 AND revoked_at IS NULL",
                params![now(), digest(token.trim())],
            )
            .map_err(state)?;
        Ok(())
    }

    /// Change a password, revoking every refresh token and every access token
    /// issued before the change.
    ///
    /// The access-token half is enforced by [`Accounts::verify_access`] comparing
    /// `iat` with `users.changed_at`, so a password change logs out sessions that
    /// have not refreshed yet rather than waiting for them to expire.
    ///
    /// # Errors
    ///
    /// The current password is wrong, the new one is outside the contract, or the
    /// write failed.
    pub fn change_password(&self, name: &str, current: &str, new: &str) -> Result<(), AuthError> {
        valid_password(new)?;
        let user = self.authenticate(name, current)?;
        let hash = hash_password(new)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(state)?;
        transaction
            .execute(
                "UPDATE users SET password_hash = ?1, changed_at = ?2, token_epoch = token_epoch + 1 \
                 WHERE id = ?3",
                params![hash, now(), user.id],
            )
            .map_err(state)?;
        transaction
            .execute(
                "UPDATE refresh_tokens SET revoked_at = ?1 WHERE user_id = ?2 AND revoked_at IS NULL",
                params![now(), user.id],
            )
            .map_err(state)?;
        transaction
            .execute(
                "INSERT INTO auth_events (at, event, user_id, detail) VALUES (?1, 'password', ?2, NULL)",
                params![now(), user.id],
            )
            .map_err(state)?;
        transaction.commit().map_err(state)?;
        Ok(())
    }

    /// Verify an access token and return its claims.
    ///
    /// The signature is checked before the claims are trusted, then `exp`, then the
    /// password-change cutoff, then the revocation list. A token whose user row is
    /// gone is refused.
    ///
    /// # Errors
    ///
    /// The token is malformed, signed by another key, expired, issued before the
    /// password changed, revoked, or names a user that no longer exists.
    pub fn verify_access(&self, token: &str) -> Result<AuthClaims, AuthError> {
        let token = token.trim();
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(signature)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(AuthError::InvalidCredentials);
        };
        if parts.next().is_some() || header != HEADER {
            return Err(AuthError::InvalidCredentials);
        }
        let signature = unb64url(signature)?;
        let signature: [u8; 64] = signature
            .try_into()
            .map_err(|_| AuthError::InvalidCredentials)?;
        let key = load_key(&self.key)?;
        let verifying = key.verifying_key();
        let message = format!("{header}.{payload}");
        verifying
            .verify_strict(message.as_bytes(), &Signature::from_bytes(&signature))
            .map_err(|_| AuthError::InvalidCredentials)?;
        let claims: AuthClaims = serde_json::from_slice(&unb64url(payload)?)
            .map_err(|_| AuthError::InvalidCredentials)?;
        if claims.iss != ISSUER {
            return Err(AuthError::InvalidCredentials);
        }
        let now = now();
        if claims.exp <= now {
            return Err(AuthError::Expired);
        }
        let connection = self.connection()?;
        let epoch: Option<i64> = connection
            .query_row(
                "SELECT token_epoch FROM users WHERE id = ?1",
                params![claims.sub],
                |row| row.get(0),
            )
            .optional()
            .map_err(state)?;
        let Some(epoch) = epoch else {
            return Err(AuthError::InvalidCredentials);
        };
        if claims.ver != epoch {
            return Err(AuthError::Revoked);
        }
        let revoked: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM revoked_jti WHERE jti = ?1 AND expires_at > ?2",
                params![claims.jti, now],
                |row| row.get(0),
            )
            .map_err(state)?;
        if revoked > 0 {
            return Err(AuthError::Revoked);
        }
        Ok(claims)
    }

    /// The account linked to an OIDC identity, when one is.
    ///
    /// # Errors
    ///
    /// The account store could not be read.
    pub fn user_for_identity(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<User>, AuthError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT users.id, users.name FROM identities
                 JOIN users ON users.id = identities.user_id
                 WHERE identities.issuer = ?1 AND identities.subject = ?2",
                params![issuer, subject],
                |row| {
                    Ok(User {
                        id: row.get(0)?,
                        name: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(state)
    }

    /// Link an OIDC identity to the only local account.
    ///
    /// Linking is a local CLI act, not an automatic email match: an email can be
    /// reassigned at a provider, so it is stored for display and never used as the
    /// key.
    ///
    /// # Errors
    ///
    /// There is no account, the identity is already linked, or the write failed.
    pub fn link_identity(
        &self,
        issuer: &str,
        subject: &str,
        email: Option<&str>,
    ) -> Result<User, AuthError> {
        let mut connection = self.connection()?;
        let user: Option<User> = connection
            .query_row("SELECT id, name FROM users LIMIT 1", [], |row| {
                Ok(User {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })
            .optional()
            .map_err(state)?;
        let Some(user) = user else {
            return Err(AuthError::NoAccounts);
        };
        let transaction = connection.transaction().map_err(state)?;
        transaction
            .execute(
                "INSERT INTO identities (user_id, issuer, subject, email, linked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![user.id, issuer, subject, email, now()],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(code, _)
                    if code.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    AuthError::AlreadyLinked
                }
                other => state(other),
            })?;
        transaction
            .execute(
                "INSERT INTO auth_events (at, event, user_id, detail) VALUES (?1, 'linked', ?2, ?3)",
                params![now(), user.id, format!("{issuer}#{subject}")],
            )
            .map_err(state)?;
        transaction.commit().map_err(state)?;
        Ok(user)
    }

    /// Remember one OIDC authorization request until its callback arrives.
    ///
    /// # Errors
    ///
    /// The row could not be written.
    pub fn save_pending_oidc(
        &self,
        pending: &PendingOidc,
        state_value: &str,
    ) -> Result<(), AuthError> {
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO oidc_pending (state, provider, nonce, verifier, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    state_value,
                    pending.provider,
                    pending.nonce,
                    pending.verifier,
                    now(),
                    pending.expires_at
                ],
            )
            .map_err(state)?;
        connection
            .execute(
                "DELETE FROM oidc_pending WHERE expires_at <= ?1",
                params![now()],
            )
            .map_err(state)?;
        Ok(())
    }

    /// Consume the pending authorization named by `state`.
    ///
    /// The row is deleted whether it is valid or expired, so a replayed callback
    /// cannot use the same verifier twice.
    ///
    /// # Errors
    ///
    /// The store could not be read or written.
    pub fn take_pending_oidc(&self, state_value: &str) -> Result<Option<PendingOidc>, AuthError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(state)?;
        let pending: Option<PendingOidc> = transaction
            .query_row(
                "SELECT provider, nonce, verifier, expires_at FROM oidc_pending WHERE state = ?1",
                params![state_value],
                |row| {
                    Ok(PendingOidc {
                        provider: row.get(0)?,
                        nonce: row.get(1)?,
                        verifier: row.get(2)?,
                        expires_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(state)?;
        transaction
            .execute(
                "DELETE FROM oidc_pending WHERE state = ?1",
                params![state_value],
            )
            .map_err(state)?;
        transaction.commit().map_err(state)?;
        Ok(pending.filter(|pending| pending.expires_at > now()))
    }

    /// Revoke one access token by id, until the time it would have expired anyway.
    ///
    /// # Errors
    ///
    /// The database could not be written.
    pub fn revoke_access(&self, claims: &AuthClaims) -> Result<(), AuthError> {
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT OR REPLACE INTO revoked_jti (jti, expires_at) VALUES (?1, ?2)",
                params![claims.jti, claims.exp],
            )
            .map_err(state)?;
        Ok(())
    }
}

/// One refresh-token row, before it is judged.
struct RefreshRow {
    id: String,
    user_id: String,
    family: String,
    expires_at: i64,
    used_at: Option<i64>,
    revoked_at: Option<i64>,
}

/// The schema, kept in one string because it is one version.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    changed_at    INTEGER NOT NULL,
    token_epoch   INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS refresh_tokens (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    family      TEXT NOT NULL,
    hash        TEXT NOT NULL UNIQUE,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    used_at     INTEGER,
    revoked_at  INTEGER
);
CREATE INDEX IF NOT EXISTS refresh_tokens_family ON refresh_tokens(family);
CREATE TABLE IF NOT EXISTS revoked_jti (
    jti        TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS auth_events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at      INTEGER NOT NULL,
    event   TEXT NOT NULL,
    user_id TEXT,
    detail  TEXT
);
CREATE INDEX IF NOT EXISTS auth_events_at ON auth_events(at);
CREATE TABLE IF NOT EXISTS identities (
    user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    issuer    TEXT NOT NULL,
    subject   TEXT NOT NULL,
    email     TEXT,
    linked_at INTEGER NOT NULL,
    PRIMARY KEY (issuer, subject)
);
CREATE TABLE IF NOT EXISTS oidc_pending (
    state      TEXT PRIMARY KEY,
    provider   TEXT NOT NULL,
    nonce      TEXT NOT NULL,
    verifier   TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
";

/// Trim and validate an account name.
fn valid_name(name: &str) -> Result<String, AuthError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(AuthError::Invalid(
            "an account name is 1 to 64 characters".to_owned(),
        ));
    }
    if !name
        .chars()
        .all(|character| character.is_alphanumeric() || "-_.@".contains(character))
    {
        return Err(AuthError::Invalid(
            "an account name may contain letters, digits, `-`, `_`, `.` and `@`".to_owned(),
        ));
    }
    Ok(name.to_owned())
}

/// Validate a password before hashing it.
fn valid_password(password: &str) -> Result<(), AuthError> {
    if password.len() < MIN_PASSWORD {
        return Err(AuthError::Invalid(format!(
            "a password is at least {MIN_PASSWORD} characters"
        )));
    }
    if password.len() > MAX_PASSWORD {
        return Err(AuthError::Invalid(format!(
            "a password is at most {MAX_PASSWORD} bytes"
        )));
    }
    Ok(())
}

/// The Argon2id parameters for a password: OWASP's 19 MiB, two passes.
fn hasher() -> Result<Argon2<'static>, AuthError> {
    let params = Params::new(19 * 1024, 2, 1, None).map_err(state)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hash a password with a fresh salt.
fn hash_password(password: &str) -> Result<String, AuthError> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(state)?;
    hasher()?
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(state)
}

/// Read the signing seed, creating it on first use.
fn load_key(path: &Path) -> Result<SigningKey, AuthError> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory).map_err(state)?;
        acpilot_host::private_dir(directory).map_err(state)?;
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                AuthError::State(format!("{} is not a 32-byte key", path.display()))
            })?;
            Ok(SigningKey::from_bytes(&seed))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).map_err(state)?;
            std::fs::write(path, seed).map_err(state)?;
            acpilot_host::private_file(path).map_err(state)?;
            Ok(SigningKey::from_bytes(&seed))
        }
        Err(error) => Err(state(error)),
    }
}

/// Sign the claims as a compact JWT.
fn sign(key_path: &Path, claims: &AuthClaims) -> Result<String, AuthError> {
    let key = load_key(key_path)?;
    let payload = serde_json::to_vec(claims).map_err(state)?;
    let message = format!("{HEADER}.{}", b64url(&payload));
    let signature = key.sign(message.as_bytes());
    Ok(format!("{message}.{}", b64url(&signature.to_bytes())))
}

/// The base64url alphabet, no padding, as JWT spells it.
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let packed = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(char::from(ALPHABET[(packed >> 18) as usize & 63]));
        out.push(char::from(ALPHABET[(packed >> 12) as usize & 63]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[(packed >> 6) as usize & 63]));
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[packed as usize & 63]));
        }
    }
    out
}

/// Decode base64url without padding.
fn unb64url(text: &str) -> Result<Vec<u8>, AuthError> {
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return Err(AuthError::InvalidCredentials),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).expect("a byte"));
        }
    }
    Ok(out)
}

/// Unix seconds.
fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// 32 random bytes as hex, for a refresh token.
fn random_hex() -> Result<String, AuthError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(state)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The SHA-256 of a refresh token, as hex.
///
/// Only the hash is stored: a database copied off the machine must not be a
/// database of usable sessions. The token has 256 bits of entropy, so a plain hash
/// is enough — there is nothing to brute-force.
fn digest(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Turn an IO or SQLite failure into the one error a client sees.
fn state(error: impl std::fmt::Display) -> AuthError {
    AuthError::State(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts() -> (tempfile::TempDir, Accounts) {
        let directory = tempfile::tempdir().expect("temp dir");
        let accounts = Accounts::in_directory(directory.path());
        (directory, accounts)
    }

    #[test]
    fn the_first_account_is_the_only_one() {
        let (_directory, accounts) = accounts();
        assert!(!accounts.has_users().expect("empty"));
        let user = accounts
            .create_owner("sam", "correct horse battery")
            .expect("created");
        assert_eq!(user.name, "sam");
        assert!(accounts.has_users().expect("one"));
        assert!(matches!(
            accounts.create_owner("other", "another password"),
            Err(AuthError::AlreadyInitialized)
        ));
    }

    #[test]
    fn a_password_is_checked_and_never_stored_in_the_clear() {
        let (_directory, accounts) = accounts();
        accounts
            .create_owner("sam", "correct horse battery")
            .expect("created");
        assert_eq!(
            accounts
                .authenticate("SAM", "correct horse battery")
                .expect("case-insensitive name")
                .name,
            "sam"
        );
        assert!(matches!(
            accounts.authenticate("sam", "wrong password"),
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            accounts.authenticate("nobody", "correct horse battery"),
            Err(AuthError::InvalidCredentials)
        ));
    }

    #[test]
    fn a_short_password_is_refused() {
        let (_directory, accounts) = accounts();
        assert!(matches!(
            accounts.create_owner("sam", "short"),
            Err(AuthError::Invalid(_))
        ));
    }

    #[test]
    fn an_access_token_verifies_and_survives_a_second_handle() {
        let (_directory, accounts) = accounts();
        let user = accounts
            .create_owner("sam", "correct horse battery")
            .expect("created");
        let session = accounts.issue(&user).expect("issued");
        let claims = accounts
            .verify_access(&session.access_token)
            .expect("verified");
        assert_eq!(claims.sub, user.id);
        let reopened = Accounts::at(accounts.path());
        assert_eq!(
            reopened
                .verify_access(&session.access_token)
                .expect("same key")
                .jti,
            claims.jti
        );
    }

    #[test]
    fn a_tampered_token_is_refused() {
        let (_directory, accounts) = accounts();
        let user = accounts
            .create_owner("sam", "correct horse battery")
            .expect("created");
        let mut session = accounts.issue(&user).expect("issued");
        let middle = session.access_token.len() / 2;
        let byte = session.access_token.as_bytes()[middle];
        session
            .access_token
            .replace_range(middle..=middle, if byte == b'A' { "B" } else { "A" });
        assert!(accounts.verify_access(&session.access_token).is_err());
    }

    #[test]
    fn refresh_rotates_and_reuse_revokes_the_family() {
        let (_directory, accounts) = accounts();
        let user = accounts
            .create_owner("sam", "correct horse battery")
            .expect("created");
        let first = accounts.issue(&user).expect("issued");
        let second = accounts.refresh(&first.refresh_token).expect("rotated");
        assert_ne!(first.refresh_token, second.refresh_token);
        assert!(matches!(
            accounts.refresh(&first.refresh_token),
            Err(AuthError::Reused)
        ));
        assert!(matches!(
            accounts.refresh(&second.refresh_token),
            Err(AuthError::Revoked)
        ));
    }

    #[test]
    fn a_password_change_revokes_refresh_and_old_access_tokens() {
        let (_directory, accounts) = accounts();
        let user = accounts
            .create_owner("sam", "correct horse battery")
            .expect("created");
        let session = accounts.issue(&user).expect("issued");
        accounts
            .change_password("sam", "correct horse battery", "a new long password")
            .expect("changed");
        assert!(matches!(
            accounts.refresh(&session.refresh_token),
            Err(AuthError::Revoked)
        ));
        assert!(matches!(
            accounts.verify_access(&session.access_token),
            Err(AuthError::Revoked)
        ));
    }

    #[test]
    fn a_logout_revokes_only_its_refresh_token() {
        let (_directory, accounts) = accounts();
        let user = accounts
            .create_owner("sam", "correct horse battery")
            .expect("created");
        let first = accounts.issue(&user).expect("issued");
        let second = accounts.issue(&user).expect("issued");
        accounts.logout(&first.refresh_token).expect("logged out");
        assert!(matches!(
            accounts.refresh(&first.refresh_token),
            Err(AuthError::Revoked)
        ));
        assert!(accounts.refresh(&second.refresh_token).is_ok());
    }

    #[test]
    fn an_identity_links_to_the_owner_and_finds_the_same_user() {
        let (_directory, accounts) = accounts();
        let owner = accounts
            .create_owner("sam", "correct horse battery")
            .expect("created");
        let linked = accounts
            .link_identity("https://id.example", "subject-1", Some("sam@example"))
            .expect("linked");
        assert_eq!(linked, owner);
        assert_eq!(
            accounts
                .user_for_identity("https://id.example", "subject-1")
                .expect("read")
                .expect("found")
                .id,
            owner.id
        );
        assert!(
            accounts
                .user_for_identity("https://id.example", "somebody-else")
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn a_refresh_cookie_gets_an_access_token_without_being_used() {
        let (_directory, accounts) = accounts();
        let user = accounts
            .create_owner("sam", "correct horse battery")
            .expect("created");
        let session = accounts.issue(&user).expect("issued");
        let browser = accounts
            .access_for_refresh(&session.refresh_token)
            .expect("cookie session");
        assert_eq!(browser.user.id, user.id);
        assert!(accounts.verify_access(&browser.access_token).is_ok());
        // The cookie token is still usable for a rotation; reading it was not a use.
        assert!(accounts.refresh(&session.refresh_token).is_ok());
    }

    #[test]
    fn status_reports_bootstrap_until_an_account_exists() {
        let (_directory, accounts) = accounts();
        let status = accounts.status().expect("status");
        assert!(status.bootstrap);
        assert!(!status.accounts);
        accounts
            .create_owner("sam", "correct horse battery")
            .expect("created");
        let status = accounts.status().expect("status");
        assert!(!status.bootstrap);
        assert!(status.accounts);
    }
}
