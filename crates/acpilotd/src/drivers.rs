//! Drivers: who may attach to this daemon's ACP bridge, and what they may do.
//!
//! The local bridge needs no ledger — the pipes belong to one process and the
//! filesystem already said who may open them. A port does not inherit that
//! (ADR 0004 §3), so a remote driver proves itself with a token this module
//! issued, and the token is stored as an Argon2id hash rather than as itself: a
//! file somebody can read must not be a file that lets them drive the machine.
//!
//! # Pairing is a human act
//!
//! A code is issued by the cockpit — an authenticated call on the daemon's own
//! listener — shown to a person, and typed into the driver. It is single-use and
//! expires in [`PAIRING_TTL`], so a code read off a screen later is not a key.
//! Claiming the code returns the token exactly once; the ledger keeps only the
//! hash.
//!
//! # Scopes are a request, not a grant
//!
//! [`DriverScope::Observe`], [`Prompt`](DriverScope::Prompt) and
//! [`Cancel`](DriverScope::Cancel) are what a pair gets by default.
//! [`AnswerPermissions`](DriverScope::AnswerPermissions) is granted only when the
//! person issuing the code asked for it, and even then a driver's request for it is
//! intersected with that grant. A driver that asks for more than it was given is
//! answered with what it has, not with a refusal that would leave it unable to do
//! anything — the refusal belongs at the call that needs the missing scope, where
//! the message can name it.
//!
//! # Revocation is a status, not a deletion
//!
//! A revoked driver keeps its row, because the audit question "who prompted this
//! session?" is answered after the connection ended. Its hash is cleared, so the
//! old token stops working on the next request rather than when a process
//! restarts: [`Drivers::authenticate`] reads the ledger, and the ledger is the
//! file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use acpilot_proto::bridge::{
    AcpBridgeSummary, AcpDriverSummary, BridgeTransport, DriverScope, DriverStatus,
    PairDriverRequest, PairedDriver, PairingCode,
};
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// How long a pairing code lives.
///
/// Two minutes is the ticket's number: long enough to read a code off the cockpit
/// and type it into a driver, short enough that a screenshot is not a credential
/// by the time it is seen.
pub const PAIRING_TTL: Duration = Duration::from_secs(120);

/// How many requests one driver may make in a minute.
///
/// A driver is an editor talking to one daemon, not a crawler; the limit exists so
/// a loop in a driver cannot starve the daemon's own clients. The ACP calls are a
/// handful per turn, so sixty is generous for a person and small for a bug.
pub const RATE_PER_MINUTE: u32 = 60;

/// The scopes a pair gets unless the person issuing the code says otherwise.
pub const DEFAULT_SCOPES: [DriverScope; 3] = [
    DriverScope::Observe,
    DriverScope::Prompt,
    DriverScope::Cancel,
];

/// One message for a driver's event stream.
///
/// The HTTP transport has no half-duplex pipe, so what ACP would send as a
/// notification travels on `GET /acp/events` instead. The driver id is on the
/// message rather than the channel so one broadcast serves every connected driver.
#[derive(Debug, Clone)]
pub struct DriverMessage {
    /// The driver the message is for.
    pub driver: String,
    /// The JSON-RPC notification, already shaped for the wire.
    pub message: serde_json::Value,
}

/// A driver as the bridge handlers know it: who, and what it may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Driver {
    /// Stable id, as `GET /v1/acp/drivers` spells it.
    pub id: String,
    /// The name the driver gave when it claimed the code.
    pub name: String,
    /// Scopes actually granted.
    pub scopes: Vec<DriverScope>,
}

impl Driver {
    /// Whether this driver holds `scope`.
    #[must_use]
    pub fn allows(&self, scope: DriverScope) -> bool {
        self.scopes.contains(&scope)
    }
}

/// Why pairing did not produce a token.
#[derive(Debug, thiserror::Error)]
pub enum PairError {
    /// The code was issued and is past its two minutes.
    #[error("the pairing code has expired; ask the cockpit for another")]
    Expired,

    /// No such code, or it was already claimed.
    #[error("that pairing code is not one this daemon issued, or it has been used")]
    Invalid,

    /// The random source, the hash, or the ledger file refused.
    #[error("the driver could not be paired: {0}")]
    State(String),
}

/// The ledger: drivers, the codes waiting to become them, and rate windows.
#[derive(Debug)]
pub struct Drivers {
    inner: Mutex<Ledger>,
    /// Where the hashes are kept. `None` is a memory-only ledger, for tests and for
    /// an embedded daemon whose listener ends with its process.
    path: Option<PathBuf>,
    events: broadcast::Sender<DriverMessage>,
}

impl Default for Drivers {
    fn default() -> Self {
        Self::memory()
    }
}

#[derive(Debug, Default)]
struct Ledger {
    drivers: Vec<StoredDriver>,
    pairings: Vec<Pending>,
    /// Per-driver request counts, never persisted: a rate limit is about the last
    /// minute, and a restart is allowed to forget it.
    windows: HashMap<String, (Instant, u32)>,
    /// Which driver opened a session, so one driver cannot prompt another's.
    /// Not persisted: a session does not survive the process either.
    owners: HashMap<String, String>,
    /// How long a code lives. The ticket's two minutes unless a caller says
    /// otherwise; a test that wants an expired code does not wait for one.
    pairing_ttl: Duration,
}

/// A driver as the file keeps it.
///
/// `status` is stored, so a revocation survives a restart; a `connected` driver
/// becomes `disconnected` on load, because the connection ended with the process
/// that served it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredDriver {
    id: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client: Option<String>,
    peer: String,
    /// The Argon2id PHC string. Empty after revocation.
    hash: String,
    scopes: Vec<DriverScope>,
    status: DriverStatus,
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connected_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen_at: Option<String>,
    #[serde(default)]
    sessions: u32,
    #[serde(default)]
    prompt_turns: u32,
}

impl StoredDriver {
    fn summary(&self) -> AcpDriverSummary {
        AcpDriverSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            client: self.client.clone(),
            peer: self.peer.clone(),
            status: self.status,
            scopes: self.scopes.clone(),
            connected_at: self.connected_at.clone(),
            last_seen_at: self.last_seen_at.clone(),
            sessions: self.sessions,
            prompt_turns: self.prompt_turns,
        }
    }
}

/// A code waiting to be claimed.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    code: String,
    /// Unix seconds, because a lexicographic comparison of RFC 3339 text is only
    /// right when the fractional digits have the same width.
    expires_second: i64,
    expires_at: String,
    /// Whether the person who issued this code allowed the driver to answer
    /// permission requests.
    answer_permissions: bool,
}

/// What the file holds.
#[derive(Debug, Serialize, Deserialize)]
struct StoredFile {
    schema_version: u32,
    #[serde(default)]
    drivers: Vec<StoredDriver>,
}

impl Drivers {
    /// A ledger that forgets everything when the process ends.
    #[must_use]
    pub fn memory() -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            inner: Mutex::new(Ledger {
                pairing_ttl: PAIRING_TTL,
                ..Ledger::default()
            }),
            path: None,
            events,
        }
    }

    /// Use `ttl` as the life of a code issued from now on.
    ///
    /// A knob rather than a constant because a deployment behind a slow tunnel may
    /// want more than two minutes to read a code out, and because a test that proves
    /// expiry should not sleep through the real one.
    #[must_use]
    pub fn with_pairing_ttl(self, ttl: Duration) -> Self {
        self.lock().pairing_ttl = ttl;
        self
    }

    /// A ledger kept in `path`, read now and rewritten on every change.
    ///
    /// A file that does not parse is logged and treated as empty: refusing to
    /// start would take the daemon down over a record of who *used* to be paired,
    /// and the tokens in it would be as lost as they are unusable.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut ledger = Ledger {
            pairing_ttl: PAIRING_TTL,
            ..Ledger::default()
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<StoredFile>(&text) {
                Ok(file) => {
                    ledger.drivers = file
                        .drivers
                        .into_iter()
                        .map(|mut driver| {
                            if driver.status == DriverStatus::Connected {
                                driver.status = DriverStatus::Disconnected;
                            }
                            driver
                        })
                        .collect();
                }
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "the driver ledger did not parse; starting empty")
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "the driver ledger could not be read")
            }
        }
        let (events, _) = broadcast::channel(64);
        Self {
            inner: Mutex::new(ledger),
            path: Some(path),
            events,
        }
    }

    /// Watch the notifications sent to drivers.
    pub fn subscribe(&self) -> broadcast::Receiver<DriverMessage> {
        self.events.subscribe()
    }

    /// Send `message` to `driver`'s stream. Returns how many streams received it.
    pub fn publish(&self, driver: &str, message: serde_json::Value) -> usize {
        self.events
            .send(DriverMessage {
                driver: driver.to_owned(),
                message,
            })
            .unwrap_or(0)
    }

    /// Issue a code, and do not keep it past [`PAIRING_TTL`].
    pub fn issue(&self, answer_permissions: bool, handshake_url: Option<String>) -> PairingCode {
        let mut ledger = self.lock();
        let now = jiff::Timestamp::now().as_second();
        ledger
            .pairings
            .retain(|pending| pending.expires_second > now);
        let code = loop {
            let candidate = pairing_code();
            if !ledger
                .pairings
                .iter()
                .any(|pending| pending.code == candidate)
            {
                break candidate;
            }
        };
        let ttl = ledger.pairing_ttl;
        let expires_second = now + i64::try_from(ttl.as_secs()).unwrap_or(120);
        let expires_at = jiff::Timestamp::from_second(expires_second)
            .map(|at| at.to_string())
            .unwrap_or_default();
        ledger.pairings.push(Pending {
            code: code.clone(),
            expires_second,
            expires_at: expires_at.clone(),
            answer_permissions,
        });
        PairingCode {
            code,
            expires_at,
            handshake_url,
        }
    }

    /// Claim a code: check it, intersect scopes with what was granted, and mint a
    /// token that is returned once and stored only as a hash.
    ///
    /// # Errors
    ///
    /// [`PairError::Expired`] when the code outlived its minute, [`PairError::Invalid`]
    /// when it is unknown or already used, and [`PairError::State`] when the token
    /// could not be hashed or the ledger could not be written.
    pub fn pair(&self, request: &PairDriverRequest) -> Result<PairedDriver, PairError> {
        let mut ledger = self.lock();
        let now = jiff::Timestamp::now().as_second();
        let index = ledger
            .pairings
            .iter()
            .position(|pending| pending.code.eq_ignore_ascii_case(request.code.trim()))
            .ok_or(PairError::Invalid)?;
        if ledger.pairings[index].expires_second <= now {
            ledger.pairings.remove(index);
            return Err(PairError::Expired);
        }
        let pending = ledger.pairings.remove(index);

        let granted = intersect(&request.requested_scopes, pending.answer_permissions);
        let token =
            acpilot_host::token::generate().map_err(|error| PairError::State(error.to_string()))?;
        let hash = hash_token(&token)?;
        let id = format!("drv-{}", uuid::Uuid::now_v7());
        let driver = StoredDriver {
            id: id.clone(),
            name: request.name.trim().to_owned(),
            client: None,
            peer: "http".to_owned(),
            hash,
            scopes: granted.clone(),
            status: DriverStatus::Connected,
            created_at: jiff::Timestamp::now().to_string(),
            connected_at: Some(jiff::Timestamp::now().to_string()),
            last_seen_at: Some(jiff::Timestamp::now().to_string()),
            sessions: 0,
            prompt_turns: 0,
        };
        ledger.drivers.push(driver.clone());
        if let Err(error) = self.persist(&ledger) {
            // A token whose hash is not on disk stops working at the next restart
            // without anything saying so, which is worse than a pairing that fails
            // now and says why.
            ledger.drivers.retain(|kept| kept.id != id);
            return Err(error);
        }
        Ok(PairedDriver {
            driver: driver.summary(),
            token,
        })
    }

    /// The driver holding `token`, when one does.
    ///
    /// Verification is the Argon2 comparison, not a string compare: the file holds
    /// hashes, so there is no token to compare against. A revoked driver's row is
    /// skipped before the hash is tried, so a revocation is effective on the next
    /// request.
    pub fn authenticate(&self, token: &str) -> Option<Driver> {
        let mut ledger = self.lock();
        let now = jiff::Timestamp::now().to_string();
        let index = ledger.drivers.iter().position(|driver| {
            driver.status != DriverStatus::Revoked
                && !driver.hash.is_empty()
                && verify_tokens(&driver.hash, token)
        })?;
        let driver = &mut ledger.drivers[index];
        driver.status = DriverStatus::Connected;
        driver.last_seen_at = Some(now.clone());
        if driver.connected_at.is_none() {
            driver.connected_at = Some(now);
        }
        Some(Driver {
            id: driver.id.clone(),
            name: driver.name.clone(),
            scopes: driver.scopes.clone(),
        })
    }

    /// Whether this driver may make another request now.
    ///
    /// A fixed window per driver: a minute is the unit the ticket asks for, and a
    /// fixed window is enough for its purpose — stopping a runaway driver from
    /// starving the daemon's own clients — without a second clock to keep.
    pub fn rate_allows(&self, id: &str) -> bool {
        let mut ledger = self.lock();
        let entry = ledger
            .windows
            .entry(id.to_owned())
            .or_insert_with(|| (Instant::now(), 0));
        if entry.0.elapsed() >= Duration::from_secs(60) {
            *entry = (Instant::now(), 0);
        }
        if entry.1 >= RATE_PER_MINUTE {
            return false;
        }
        entry.1 += 1;
        true
    }

    /// Record the name and version the driver reported in `initialize`.
    pub fn note_client(&self, id: &str, client: Option<String>) {
        let mut ledger = self.lock();
        if let Some(driver) = ledger.drivers.iter_mut().find(|driver| driver.id == id)
            && let Some(client) = client
        {
            driver.client = Some(client);
        }
    }

    /// Remember that `driver` opened `session`.
    pub fn claim_session(&self, session: &str, driver: &str) {
        let mut ledger = self.lock();
        ledger.owners.insert(session.to_owned(), driver.to_owned());
    }

    /// Whether `driver` opened `session`.
    ///
    /// A session this daemon did not hand to a driver has no owner and is not
    /// reachable over the bridge: a driver is not allowed to type into the
    /// conversation a person started at the machine.
    #[must_use]
    pub fn owns_session(&self, session: &str, driver: &str) -> bool {
        self.lock()
            .owners
            .get(session)
            .is_some_and(|owner| owner == driver)
    }

    /// Record that this driver opened a session.
    pub fn note_session(&self, id: &str) {
        let mut ledger = self.lock();
        if let Some(driver) = ledger.drivers.iter_mut().find(|driver| driver.id == id) {
            driver.sessions = driver.sessions.saturating_add(1);
        }
    }

    /// Record that this driver prompted a turn.
    pub fn note_prompt(&self, id: &str) {
        let mut ledger = self.lock();
        if let Some(driver) = ledger.drivers.iter_mut().find(|driver| driver.id == id) {
            driver.prompt_turns = driver.prompt_turns.saturating_add(1);
        }
        if let Err(error) = self.persist(&ledger) {
            tracing::warn!(%error, driver = %id, "the prompt count was not written");
        }
    }

    /// Revoke a driver: the next request with its token is refused.
    ///
    /// The row stays, so a transcript can still answer who prompted a turn; the
    /// hash is cleared, so the token has nothing left to verify against.
    ///
    /// # Errors
    ///
    /// No driver has that id, or the ledger could not be written.
    pub fn revoke(&self, id: &str) -> Result<AcpDriverSummary, String> {
        let mut ledger = self.lock();
        let index = ledger
            .drivers
            .iter()
            .position(|driver| driver.id == id)
            .ok_or_else(|| format!("no driver `{id}` is on record"))?;
        ledger.drivers[index].status = DriverStatus::Revoked;
        ledger.drivers[index].hash.clear();
        self.persist(&ledger).map_err(|error| error.to_string())?;
        Ok(ledger.drivers[index].summary())
    }

    /// Every driver on record, including revoked ones.
    #[must_use]
    pub fn summaries(&self) -> Vec<AcpDriverSummary> {
        let ledger = self.lock();
        let mut rows: Vec<_> = ledger.drivers.iter().map(StoredDriver::summary).collect();
        rows.sort_by(|left, right| left.id.cmp(&right.id));
        rows
    }

    /// The bridge as the cockpit draws it.
    #[must_use]
    pub fn bridge(
        &self,
        enabled: bool,
        bind: Option<String>,
        public_url: Option<String>,
    ) -> AcpBridgeSummary {
        let ledger = self.lock();
        AcpBridgeSummary {
            enabled,
            transports: if enabled {
                vec![BridgeTransport::HttpJsonRpc]
            } else {
                Vec::new()
            },
            bind: enabled.then_some(bind).flatten(),
            public_url,
            // The HTTP transport always asks for a driver token: the code proves a
            // person showed it to the driver, and the token is what every later
            // request carries.
            require_token: true,
            connected: u32::try_from(
                ledger
                    .drivers
                    .iter()
                    .filter(|driver| driver.status == DriverStatus::Connected)
                    .count(),
            )
            .unwrap_or(u32::MAX),
            known_drivers: u32::try_from(ledger.drivers.len()).unwrap_or(u32::MAX),
        }
    }

    fn persist(&self, ledger: &Ledger) -> Result<(), PairError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let file = StoredFile {
            schema_version: 1,
            drivers: ledger.drivers.clone(),
        };
        let text = toml::to_string(&file).map_err(|error| PairError::State(error.to_string()))?;
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)
                .map_err(|error| PairError::State(error.to_string()))?;
        }
        std::fs::write(path, text).map_err(|error| PairError::State(error.to_string()))?;
        acpilot_host::private_file(path).map_err(|error| PairError::State(error.to_string()))?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What a driver asked for, limited to what the person granted.
///
/// The order is fixed — observe, prompt, cancel, answer_permissions — so two
/// summaries of the same grant read the same way. A request that names none gets
/// the defaults: a driver that said nothing asked for the ordinary seat.
fn intersect(requested: &[DriverScope], answer_permissions: bool) -> Vec<DriverScope> {
    let mut allowed = DEFAULT_SCOPES.to_vec();
    if answer_permissions {
        allowed.push(DriverScope::AnswerPermissions);
    }
    let wanted = if requested.is_empty() {
        return allowed;
    } else {
        requested
    };
    [
        DriverScope::Observe,
        DriverScope::Prompt,
        DriverScope::Cancel,
        DriverScope::AnswerPermissions,
    ]
    .into_iter()
    .filter(|scope| allowed.contains(scope) && wanted.contains(scope))
    .collect()
}

/// A code a person can read aloud: eight characters from a 32-letter alphabet,
/// in two groups, with `I`, `O`, `0` and `1` left out so a code is not a guess
/// between two glyphs.
fn pairing_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 8];
    // A failing random source here is not recoverable and not something the
    // caller can do anything about; the daemon's token path treats it the same
    // way. The alternative is a predictable pairing code.
    if getrandom::fill(&mut bytes).is_err() {
        return "PAIR-CODE".to_owned();
    }
    let code: String = bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect();
    format!("{}-{}", &code[..4], &code[4..])
}

/// The Argon2id hash of `token`, with a fresh salt.
///
/// The salt is sixteen random bytes, the length the PHC format recommends; it is
/// passed to the hash function rather than generated inside it so the random source
/// is the same one that mints the token.
fn hash_token(token: &str) -> Result<String, PairError> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|error| PairError::State(error.to_string()))?;
    let argon = hasher()?;
    argon
        .hash_password_with_salt(token.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| PairError::State(error.to_string()))
}

/// The Argon2id parameters: 4 MiB and three passes.
///
/// A token is 256 random bits, so the hash is not defending a guessable password;
/// it is defending a leaked file from being replayed. The defaults (19 MiB) cost
/// more than the local daemon's request path wants to pay for that.
fn hasher() -> Result<Argon2<'static>, PairError> {
    let params =
        Params::new(4096, 3, 1, None).map_err(|error| PairError::State(error.to_string()))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Whether `token` is the one `hash` was made from.
fn verify_tokens(hash: &str, token: &str) -> bool {
    hasher().is_ok_and(|argon| argon.verify_password(token.as_bytes(), hash).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(
        drivers: &Drivers,
        requested: &[DriverScope],
        answer_permissions: bool,
    ) -> Result<PairedDriver, PairError> {
        let code = drivers.issue(answer_permissions, None);
        drivers.pair(&PairDriverRequest {
            code: code.code,
            name: "devin".to_owned(),
            requested_scopes: requested.to_vec(),
        })
    }

    #[test]
    fn a_code_is_single_use_and_mints_a_working_token() {
        let drivers = Drivers::memory();
        let code = drivers.issue(false, None);
        let request = PairDriverRequest {
            code: code.code.clone(),
            name: "devin".to_owned(),
            requested_scopes: vec![DriverScope::Observe, DriverScope::Prompt],
        };
        let paired = drivers.pair(&request).expect("the first claim works");
        assert_eq!(paired.driver.name, "devin");
        assert_eq!(
            paired.driver.scopes,
            vec![DriverScope::Observe, DriverScope::Prompt]
        );

        let token = paired.token;
        let driver = drivers.authenticate(&token).expect("the token works");
        assert_eq!(driver.name, "devin");
        assert!(driver.allows(DriverScope::Prompt));
        assert!(!driver.allows(DriverScope::AnswerPermissions));

        assert!(matches!(drivers.pair(&request), Err(PairError::Invalid)));
    }

    #[test]
    fn an_expired_code_is_refused_and_not_kept() {
        let drivers = Drivers::memory();
        {
            let mut ledger = drivers.lock();
            ledger.pairings.push(Pending {
                code: "OLD-CODE".to_owned(),
                expires_second: jiff::Timestamp::now().as_second() - 1,
                expires_at: String::new(),
                answer_permissions: false,
            });
        }
        let error = drivers
            .pair(&PairDriverRequest {
                code: "OLD-CODE".to_owned(),
                name: "late".to_owned(),
                requested_scopes: vec![],
            })
            .expect_err("expired");
        assert!(matches!(error, PairError::Expired));
        assert!(drivers.lock().pairings.is_empty());
    }

    #[test]
    fn answer_permissions_is_granted_only_when_the_code_was_issued_with_it() {
        let asked = vec![DriverScope::AnswerPermissions, DriverScope::Prompt];
        let drivers = Drivers::memory();
        let paired = claim(&drivers, &asked, false).expect("paired");
        assert_eq!(paired.driver.scopes, vec![DriverScope::Prompt]);

        let paired = claim(&drivers, &asked, true).expect("paired");
        assert!(
            paired
                .driver
                .scopes
                .contains(&DriverScope::AnswerPermissions)
        );
    }

    #[test]
    fn a_revoked_driver_is_refused_by_the_next_call_and_its_row_stays() {
        let drivers = Drivers::memory();
        let paired = claim(&drivers, &[], false).expect("paired");
        let id = paired.driver.id;
        drivers.revoke(&id).expect("revoked");
        assert!(drivers.authenticate(&paired.token).is_none());

        let rows = drivers.summaries();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, DriverStatus::Revoked);
    }

    #[test]
    fn the_rate_limit_is_a_minute_of_requests() {
        let drivers = Drivers::memory();
        let paired = claim(&drivers, &[], false).expect("paired");
        for _ in 0..RATE_PER_MINUTE {
            assert!(drivers.rate_allows(&paired.driver.id));
        }
        assert!(!drivers.rate_allows(&paired.driver.id));
    }

    #[test]
    fn a_ledger_file_keeps_hashes_and_returns_with_connections_closed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("drivers.toml");
        let drivers = Drivers::at(&path);
        let paired = drivers
            .pair(&PairDriverRequest {
                code: drivers.issue(false, None).code,
                name: "devin".to_owned(),
                requested_scopes: vec![DriverScope::Observe],
            })
            .expect("paired");
        let text = std::fs::read_to_string(&path).expect("the ledger was written");
        assert!(
            !text.contains(&paired.token),
            "the token is not in the file"
        );
        assert!(text.contains("hash"), "the hash is: {text}");

        let reopened = Drivers::at(&path);
        // Read before the first request: the connection ended with the process that
        // served it, so a restart shows the driver as disconnected until it calls.
        assert_eq!(reopened.summaries()[0].status, DriverStatus::Disconnected);
        assert_eq!(
            reopened
                .authenticate(&paired.token)
                .expect("still paired")
                .id,
            paired.driver.id
        );
        assert_eq!(reopened.summaries()[0].status, DriverStatus::Connected);
    }
}
