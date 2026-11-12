//! The language servers this daemon can ask, and whether one is installed.
//!
//! `GET /v1/files/references` has two sources. A server that is running knows scope —
//! two functions called `new` are two functions — while a parse-tree search knows
//! only spelling. So a references request that names a caret asks the server first,
//! and the grammar search is the fallback for a workspace whose language has no
//! server installed.
//!
//! # Which server
//!
//! Detected, not configured: the ones `docs/notes/lsp-servers.md` measured, found on
//! `PATH`. That is the same posture the language-servers page states for v0.1 — detect
//! what is installed and do not fetch anything — and it is why nothing here downloads
//! a server, installs a package, or reads a trust database.
//!
//! # One session per language
//!
//! A server indexes a project once and answers from that index. Spawning one per
//! request would pay the index on every keystroke, so a session is started on first
//! use and kept. A session that fails is remembered as absent: a server that will not
//! start must not become a spawn attempt per request, and installing it later asks
//! for a daemon restart, which is the same trade every supervisor in this product
//! makes.
//!
//! # And stopped by the daemon
//!
//! Kept means kept until the process ends, and nothing else ends it: [`Servers::shutdown`]
//! is what the daemon calls on its way out, because a dropped router is not a dead
//! child. A language server with an index is tens of megabytes and holds the source
//! files open; leaving one behind per launch is a leak a person notices in `ps`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use acpilot_lsp::position::PositionEncoding;
use acpilot_lsp::session::{Location, Session};
use tokio::sync::Mutex;

use crate::helper::{Connections, Transport};

/// The servers this build knows how to start, by language.
///
/// Three fields per entry: the language id LSP uses, the program, and its arguments.
/// The list is deliberately short and every entry was measured end to end in the
/// P0-7 notes; a longer list of guesses would be a longer list of hangs.
/// How many times a cold server is asked before an empty answer is believed.
///
/// Twenty quarter-seconds is five, which is longer than `rust-analyzer` takes to index
/// the fixtures in `docs/notes/lsp-servers.md` on a warm cache and short enough that a
/// panel waiting on it still feels like a panel. The size of the workspace is the real
/// variable here; a monorepo's first index is minutes, and this is not where that is
/// solved.
const COLD_ATTEMPTS: usize = 20;

/// The pause between those attempts.
const COLD_PAUSE: Duration = Duration::from_millis(250);

/// How long a server may sit without a question before the daemon stops it.
///
/// Kept sessions are what make the second references request one round trip, and an
/// idle one is a process holding its index resident — `rust-analyzer` on this
/// repository is hundreds of megabytes with the source files open. Five minutes is
/// longer than the pause between two passes over the same file and shorter than
/// getting a coffee.
///
/// A constant rather than a preference: a server that starts again on the next ask
/// costs a few seconds, which is a pause, and the number that decides how much RAM a
/// daemon holds should not need a config file to be safe.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a server may take to publish what is wrong with a document.
///
/// A publish is pushed, not answered, and the wait is the index: `rust-analyzer` on
/// this repository answers in well under a second once it is warm and takes tens of
/// seconds on a cold cache. Ten is the point where a panel waiting on it is still a
/// panel rather than a hang, and a file that is genuinely quiet answers empty either
/// way.
pub const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(10);

/// How often the daemon looks for one.
///
/// A tenth of [`IDLE_TIMEOUT`], so a server is stopped between five and five and a
/// half minutes after its last question rather than up to a whole timeout late, and
/// the sweep is one lock and one timestamp comparison per language.
pub const IDLE_SWEEP: Duration = Duration::from_secs(30);

/// How often the daemon looks for a publish nobody asked for.
///
/// A publish is pushed and costs nothing to look for: the transport's pump fills a
/// queue whether or not anybody reads it, so this is one lock and one `try_recv` per
/// running server. A quarter of a second is short enough that a gutter that was
/// waiting on a background index fills in as it finishes rather than at the next
/// keystroke, and long enough that an idle daemon is not busy.
pub const DIAGNOSTICS_POLL: Duration = Duration::from_millis(250);

pub const SERVERS: &[(&str, &str, &[&str])] = &[
    ("rust", "rust-analyzer", &[]),
    ("python", "pyright-langserver", &["--stdio"]),
    ("typescript", "typescript-language-server", &["--stdio"]),
    ("javascript", "typescript-language-server", &["--stdio"]),
];

/// What a server answered, and the encoding its columns are counted in.
#[derive(Debug)]
pub struct Answer {
    /// The command that answered, for a log line or a status line.
    pub program: &'static str,
    /// The encoding every `character` in [`Answer::locations`] is counted in.
    pub encoding: PositionEncoding,
    /// Where the name is used, in the server's own spelling.
    pub locations: Vec<Location>,
}

/// The style options a formatter is asked to format with.
///
/// A struct rather than two booleans in the call: the pair travels together, and a
/// call site that named only one of them would be a formatter asked with the
/// other's default.
#[derive(Debug, Clone, Copy)]
pub struct FormatOptions {
    /// Columns per tab stop.
    pub tab_width: u8,
    /// Insert spaces rather than tabs.
    pub insert_spaces: bool,
}

/// What a formatter did, and the encoding its edit positions are counted in.
#[derive(Debug)]
pub struct Formatted {
    /// The command that answered.
    pub program: &'static str,
    /// The encoding every position in [`Formatted::edits`] is counted in.
    pub encoding: PositionEncoding,
    /// The edits, in the server's order.
    pub edits: Vec<acpilot_lsp::session::FormatEdit>,
}

/// What a server said is wrong with a document, and the encoding it said it in.
#[derive(Debug)]
pub struct Problems {
    /// The command that answered, for a log line or a status line.
    pub program: &'static str,
    /// The encoding every `character` in [`Problems::items`] is counted in.
    pub encoding: PositionEncoding,
    /// The problems themselves, in the server's order.
    pub items: Vec<acpilot_lsp::session::Problem>,
}

/// What a server published about a document without being asked.
///
/// A path rather than a URI, because a workspace root is a path and the comparison
/// that matters is whether the document is inside it — the session was handed the path
/// when it opened the document, so it is the one place the two spellings meet.
#[derive(Debug, Clone)]
pub struct Pushed {
    /// The document, as the daemon opened it.
    pub path: PathBuf,
    /// The version the problems are about, when the server names one.
    pub version: Option<i64>,
    /// The command that published, for the frame's `server` field.
    pub program: &'static str,
    /// The encoding every `character` in [`Pushed::items`] is counted in.
    pub encoding: PositionEncoding,
    /// The problems themselves, in the server's order.
    pub items: Vec<acpilot_lsp::session::Problem>,
}

/// The sessions this daemon has started, one per language.
///
/// `Debug` is written out rather than derived: a [`Session`] owns pipes and a child
/// process, and the only fact worth printing is which languages have one running.
/// Where a session runs: this daemon's machine, or a workspace's host.
#[derive(Debug, Clone)]
pub enum Where {
    /// The daemon's own filesystem.
    Local,
    /// A host reached through its helper.
    Remote {
        /// The transport that reaches it.
        transport: Transport,
        /// The host id, for a key and a message.
        host: String,
    },
}

/// A workspace root and the machine it is on.
///
/// Two spellings of one directory, kept together because they must not be confused: the
/// path a server is started in is the host's, and a `Path` resolved on this daemon is
/// the wrong string to hand a server on another machine.
#[derive(Debug, Clone)]
pub struct Place {
    /// The root, in the spelling of the machine the server runs on.
    pub root: PathBuf,
    /// Which machine that is.
    pub at: Where,
}

impl Place {
    /// The id a session for `language` is kept under.
    ///
    /// A local key is the language alone, so a daemon with no remote workspaces keeps
    /// the one-session-per-language behaviour it had; a remote key names the host and the
    /// root, because two workspaces on one host are two indexes.
    fn key(&self, language: &'static str) -> String {
        match &self.at {
            Where::Local => language.to_owned(),
            Where::Remote { host, .. } => {
                format!("{host}\u{0}{}\u{0}{language}", self.root.display())
            }
        }
    }
}

#[derive(Default)]
pub struct Servers {
    sessions: Mutex<HashMap<String, Option<Running>>>,
}

/// A started server and what the daemon knows about its use.
///
/// The clock lives here rather than on [`Session`] because "when was this last
/// asked" is the registry's question: a session answers, and a registry decides that
/// a session nobody has asked is a process to stop.
struct Running {
    session: Session,
    /// The command that runs, for a status line or a log.
    program: &'static str,
    /// The helper process, when this session runs on a host.
    ///
    /// Dropped with the session, which is what closes the far side and ends the
    /// server there; a `Session` on its own has no process to close.
    _remote: Option<crate::lsp_remote::RemoteHandle>,
    last_used: Instant,
    /// The length of the last `textDocument/references` call that answered, without
    /// the retries a cold index needs. What a status line reports is the round trip,
    /// because the index is paid once and the round trip is paid per keystroke.
    round_trip: Option<Duration>,
    /// How many questions this session has answered.
    requests: u64,
}

impl std::fmt::Debug for Servers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.sessions.try_lock().ok().map(|sessions| {
            sessions
                .iter()
                .filter(|(_, session)| session.is_some())
                .map(|(language, _)| language.clone())
                .collect::<Vec<_>>()
        });
        formatter
            .debug_struct("Servers")
            .field("running", &live)
            .finish()
    }
}

impl Servers {
    /// A registry that has started nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the server for `language` where the name at `offset` in `text` is used.
    ///
    /// `None` means no server answered — none is installed, it would not start, or it
    /// failed the question — and the caller falls back to its own search. A failure is
    /// not an error to serve: references are a list, and an empty one with a reason is
    /// worse than the parse tree's answer.
    pub async fn references(
        &self,
        language: &str,
        place: &Place,
        connections: &Arc<Connections>,
        path: &Path,
        text: &str,
        offset: usize,
    ) -> Option<Answer> {
        let (language, program) = SERVERS
            .iter()
            .find(|(id, _, _)| *id == language)
            .map(|(id, program, _)| (*id, *program))?;
        let key = place.key(language);
        let mut sessions = self.sessions.lock().await;
        let running = running(&mut sessions, language, place, connections).await?;
        // Marked used before the question rather than after the answer: the first
        // question is the index, and a sweep that stopped a server in the middle of
        // one would make the next keystroke pay for it a second time.
        running.last_used = Instant::now();
        running.requests += 1;

        // The caret's line and column are the server's encoding, and only the server
        // knows which one it picked: the conversion happens here, once it has. The
        // offset arrives counted in characters, because that is the editor's unit for
        // a caret and the request is about the character under it.
        let encoding = running.session.encoding();
        let index = acpilot_lsp::LineIndex::new(text);
        let position = index.position(text, byte_offset(text, offset), encoding)?;

        if let Err(error) = running.session.open(path, language, text).await {
            tracing::debug!(server = program, %error, "language server lost the document");
            // A session whose transport broke is not reusable: drop it so the next
            // request starts a fresh one rather than failing forever.
            sessions.insert(key.clone(), None);
            return None;
        }

        // A cold server answers `[]` until it has indexed the document it was just
        // handed, so an empty answer is only believed once the session has answered
        // with something or the attempts run out. The first question pays for the
        // index; later ones are one round trip.
        let mut attempts = 0;
        loop {
            attempts += 1;
            let asked = Instant::now();
            match running.session.references(path, position).await {
                Ok(locations) if !locations.is_empty() => {
                    running.session.mark_warm();
                    running.round_trip = Some(asked.elapsed());
                    running.last_used = Instant::now();
                    return Some(Answer {
                        program,
                        encoding,
                        locations,
                    });
                }
                Ok(locations) => {
                    if running.session.warm() || attempts == COLD_ATTEMPTS {
                        running.round_trip = Some(asked.elapsed());
                        running.last_used = Instant::now();
                        return Some(Answer {
                            program,
                            encoding,
                            locations,
                        });
                    }
                    tokio::time::sleep(COLD_PAUSE).await;
                }
                Err(error) => {
                    tracing::debug!(server = program, %error, "references failed");
                    sessions.insert(key.clone(), None);
                    return None;
                }
            }
        }
    }

    /// What the server for `language` says is wrong with `path`.
    ///
    /// `None` means nothing answered — no server for the language, none installed, or
    /// one that failed — which is the same answer [`Servers::references`] gives, and
    /// for the same reason: a clean file and an unasked question both read as no
    /// problems, and a file nobody could check is not a file with none.
    ///
    /// The document is the text that was handed over, not the file on disk: an editor
    /// that has typed half a line wants the problems that half a line has.
    pub async fn diagnostics(
        &self,
        language: &str,
        place: &Place,
        connections: &Arc<Connections>,
        path: &Path,
        text: &str,
    ) -> Option<Problems> {
        let (language, program) = SERVERS
            .iter()
            .find(|(id, _, _)| *id == language)
            .map(|(id, program, _)| (*id, *program))?;
        let key = place.key(language);
        let mut sessions = self.sessions.lock().await;
        let running = running(&mut sessions, language, place, connections).await?;
        running.last_used = Instant::now();
        running.requests += 1;
        let encoding = running.session.encoding();

        if let Err(error) = running.session.open(path, language, text).await {
            tracing::debug!(server = program, %error, "language server lost the document");
            sessions.insert(key.clone(), None);
            return None;
        }
        match running.session.diagnostics(path, DIAGNOSTICS_WAIT).await {
            Ok(items) => {
                running.last_used = Instant::now();
                Some(Problems {
                    program,
                    encoding,
                    items,
                })
            }
            Err(error) => {
                tracing::debug!(server = program, %error, "diagnostics failed");
                sessions.insert(key.clone(), None);
                None
            }
        }
    }

    /// Ask the server for `language` to format a buffer.
    ///
    /// `None` means nothing answered — no server for the language, none installed,
    /// or one that failed — and the caller saves the buffer unchanged. A server that
    /// answered with no edits is `Some`, because "already formatted" is an answer
    /// and "nobody looked" is not.
    pub async fn format(
        &self,
        language: &str,
        place: &Place,
        connections: &Arc<Connections>,
        path: &Path,
        text: &str,
        options: FormatOptions,
    ) -> Option<Formatted> {
        let (language, program) = SERVERS
            .iter()
            .find(|(id, _, _)| *id == language)
            .map(|(id, program, _)| (*id, *program))?;
        let key = place.key(language);
        let mut sessions = self.sessions.lock().await;
        let running = running(&mut sessions, language, place, connections).await?;
        running.last_used = Instant::now();
        running.requests += 1;
        let encoding = running.session.encoding();
        if let Err(error) = running.session.open(path, language, text).await {
            tracing::debug!(server = program, %error, "language server lost the document");
            sessions.insert(key.clone(), None);
            return None;
        }
        match running
            .session
            .format(
                path,
                u32::from(options.tab_width.max(1)),
                options.insert_spaces,
            )
            .await
        {
            Ok(edits) => {
                running.last_used = Instant::now();
                Some(Formatted {
                    program,
                    encoding,
                    edits,
                })
            }
            Err(error) => {
                tracing::debug!(server = program, %error, "formatting failed");
                sessions.insert(key.clone(), None);
                None
            }
        }
    }

    /// Take what the servers published since this was last called.
    ///
    /// The daemon's language server answers a question when it is asked one, and a
    /// server with an index also speaks on its own: `rust-analyzer` publishes again
    /// when a background check finishes, and a pane that only heard about problems it
    /// asked for would keep a stale gutter until the next save. Nothing here asks
    /// anything — it empties the queue the transport has been filling and hands back
    /// what was in it.
    ///
    /// Concurrent calls are fine: they queue on the registry's lock, and a duplicate
    /// drain is a drain that finds nothing.
    pub async fn drain_published(&self) -> Vec<Pushed> {
        let mut sessions = self.sessions.lock().await;
        let mut pushed = Vec::new();
        for slot in sessions.values_mut() {
            let Some(running) = slot.as_mut() else {
                continue;
            };
            let program = running.program;
            let encoding = running.session.encoding();
            for published in running.session.drain_published() {
                pushed.push(Pushed {
                    path: published.path,
                    version: published.version,
                    program,
                    encoding,
                    items: published.problems,
                });
            }
        }
        pushed
    }

    /// Stop every server this daemon started.
    ///
    /// A session outlives its last question on purpose, and nothing else ends it:
    /// dropping the registry when the router goes away looks like it would, and it
    /// does not — the terminal mirror and the settings watch each hold an [`AppState`]
    /// clone in a task that outlives `serve`, so the `Arc<Servers>` never reaches zero
    /// and the child keeps its index resident. Measured on 2026-11-02: every daemon
    /// that had answered a references request left one `rust-analyzer` behind.
    ///
    /// Concurrently, because a stop is a request, a notification and a wait: one
    /// wedged server must not add its timeout to the one queued behind it. A server
    /// that will not stop is killed by [`Session::shutdown`], and a `Session` dropped
    /// without one is killed by the transport's `kill_on_drop`, so this is the polite
    /// path rather than the only one.
    ///
    /// [`AppState`]: crate::AppState
    pub async fn shutdown(&self) {
        let started: Vec<Running> = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .drain()
                .filter_map(|(_, running)| running)
                .collect()
        };
        if started.is_empty() {
            return;
        }
        futures::future::join_all(started.into_iter().map(|mut running| async move {
            let server = running.program;
            running.session.shutdown().await;
            tracing::debug!(server, "language server stopped");
            drop(running);
        }))
        .await;
    }

    /// Stop the servers nothing has asked in `idle`.
    ///
    /// A session is kept for the next question, and a person who walks away has no
    /// next question: an idle `rust-analyzer` is its index resident, which is the
    /// difference between a daemon and a leak with a socket. The entry is removed
    /// rather than emptied, so the next request starts a fresh server instead of
    /// finding a hole where one used to be.
    ///
    /// Returns what was stopped, in whatever order the map yielded it: a set, for a
    /// log line and for a test.
    pub async fn stop_idle(&self, idle: Duration) -> Vec<&'static str> {
        let now = Instant::now();
        let stopped: Vec<Running> = {
            let mut sessions = self.sessions.lock().await;
            let idle_now: Vec<String> = sessions
                .iter()
                .filter(|(_, slot)| {
                    slot.as_ref()
                        .is_some_and(|running| now.duration_since(running.last_used) >= idle)
                })
                .map(|(key, _)| key.clone())
                .collect();
            idle_now
                .into_iter()
                .filter_map(|key| sessions.remove(&key).flatten())
                .collect()
        };
        if stopped.is_empty() {
            return Vec::new();
        }
        let languages: Vec<&'static str> = stopped.iter().map(|running| running.program).collect();
        futures::future::join_all(stopped.into_iter().map(|mut running| async move {
            let server = running.program;
            running.session.shutdown().await;
            tracing::debug!(server, "idle language server stopped");
            drop(running);
        }))
        .await;
        languages
    }

    /// What this daemon knows about its servers, one row per language it can start.
    ///
    /// A language with no session is [`ServerState::running`] `None`, which is a fact
    /// rather than an error: a server that was never started, one an idle sweep
    /// stopped, and one that would not start all look the same from outside, and
    /// `installed` is the one thing that tells the last of those apart.
    pub async fn status(&self) -> Vec<ServerState> {
        let now = Instant::now();
        let sessions = self.sessions.lock().await;
        SERVERS
            .iter()
            .map(|(language, program, _)| {
                let running =
                    sessions
                        .get(*language)
                        .and_then(|slot| slot.as_ref())
                        .map(|running| RunningState {
                            encoding: running.session.encoding(),
                            idle: now.duration_since(running.last_used),
                            round_trip: running.round_trip,
                            requests: running.requests,
                        });
                ServerState {
                    language,
                    program,
                    installed: installed(program),
                    running,
                }
            })
            .collect()
    }
}

/// The server behind `id`, started if it is not running.
///
/// One place rather than one per question, because both questions pay the same index
/// and both have to make the same three decisions: whether a server exists for the
/// language, whether it is installed, and whether a failed one is retried (it is not —
/// see the module note).
async fn running<'a>(
    sessions: &'a mut HashMap<String, Option<Running>>,
    language: &'static str,
    place: &Place,
    connections: &Arc<Connections>,
) -> Option<&'a mut Running> {
    let (_, program, args) = SERVERS.iter().find(|(id, _, _)| *id == language)?;
    let key = place.key(language);
    if !sessions.contains_key(&key) {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        // A local server is only offered when it is installed; a remote one is the
        // host's to have, and the refusal there names the program the host could not run.
        let started = match &place.at {
            Where::Local => {
                if installed(program) {
                    Session::start(program, &args, &place.root)
                        .await
                        .ok()
                        .map(|session| (session, None))
                } else {
                    None
                }
            }
            Where::Remote { transport, .. } => {
                let root = place.root.to_string_lossy().into_owned();
                match crate::lsp_remote::start(
                    Arc::clone(connections),
                    transport.clone(),
                    program,
                    args.clone(),
                    root,
                )
                .await
                {
                    Ok((session, handle)) => Some((session, Some(handle))),
                    Err(error) => {
                        tracing::debug!(server = program, %error, "language server did not start on the host");
                        None
                    }
                }
            }
        };
        match &started {
            Some((session, _)) => {
                tracing::debug!(server = program, encoding = %session.encoding(), "language server ready");
            }
            None => tracing::debug!(server = program, "language server did not start"),
        }
        sessions.insert(
            key.clone(),
            started.map(|(session, remote)| Running {
                session,
                program,
                _remote: remote,
                last_used: Instant::now(),
                round_trip: None,
                requests: 0,
            }),
        );
    }
    sessions.get_mut(&key)?.as_mut()
}

/// One language this build can start, as `GET /v1/lsp/servers` reports it.
#[derive(Debug, Clone)]
pub struct ServerState {
    /// The LSP language id, which is also the key in [`SERVERS`].
    pub language: &'static str,
    /// The program that would be spawned.
    pub program: &'static str,
    /// Whether that program is on this machine's `PATH`.
    pub installed: bool,
    /// The session, when one is running.
    pub running: Option<RunningState>,
}

/// A running session, as reported.
#[derive(Debug, Clone)]
pub struct RunningState {
    /// The encoding the server chose, which its columns are counted in.
    pub encoding: PositionEncoding,
    /// How long since it was last asked.
    pub idle: Duration,
    /// The last `textDocument/references` round trip that answered.
    pub round_trip: Option<Duration>,
    /// How many questions it has been asked.
    pub requests: u64,
}

/// The byte offset of character `characters` in `text`, or the end of the text.
///
/// A caret is a character offset in the pane — `identifier_at` counts characters — and
/// a position is a byte offset, so the conversion happens where the text is, once.
fn byte_offset(text: &str, characters: usize) -> usize {
    text.char_indices()
        .nth(characters)
        .map_or(text.len(), |(byte, _)| byte)
}

/// The LSP language id for a path, when this build has a server for it.
///
/// The id and the key in [`SERVERS`] are the same string, which is what lets the
/// route name a language and the registry find a session with it.
#[must_use]
pub fn language_of(path: &str) -> Option<&'static str> {
    let extension = path.rsplit('.').next()?.to_ascii_lowercase();
    match extension.as_str() {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        _ => None,
    }
}

/// Whether `program` is on this machine's `PATH`.
///
/// A scan rather than a shell-out: `which` is a second process and a second failure
/// mode, and the answer wanted here is "is there a file with this name on the path",
/// not "what does the shell think". The names to look for are the platform's
/// ([`crate::platform`]), because an extension is a platform fact and the workspace's
/// boundary test says so.
///
/// A program that contains a directory is a path, not a name to search for, so it is
/// checked where it points and does not need `PATH` to exist at all. Joining such a
/// name onto every path entry happens to work on Windows but only when the path is
/// absolute and non-empty, and a declaration that names its own executable is a
/// first-class case, not a lucky one.
#[must_use]
pub fn installed(program: &str) -> bool {
    let names = crate::platform::executable_names(program);
    if Path::new(program).components().count() > 1 {
        return names.iter().any(|name| name.is_file());
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        names.iter().any(|name| {
            let candidate = directory.join(name);
            candidate.is_file()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_language_is_read_off_the_extension() {
        assert_eq!(language_of("src/main.rs"), Some("rust"));
        assert_eq!(language_of("crates/a/src/lib.rs"), Some("rust"));
        assert_eq!(language_of("tool.py"), Some("python"));
        assert_eq!(language_of("app.tsx"), Some("typescript"));
        assert_eq!(language_of("app.mjs"), Some("javascript"));
        assert_eq!(language_of("README.md"), None);
        assert_eq!(language_of("Makefile"), None);
        assert_eq!(language_of("a.rs.bak"), None, "the last extension decides");
    }

    #[test]
    fn a_character_offset_becomes_a_byte_offset() {
        let text = "a日b";
        assert_eq!(byte_offset(text, 0), 0);
        assert_eq!(byte_offset(text, 1), 1, "before 日");
        assert_eq!(byte_offset(text, 2), 4, "before b, three bytes later");
        assert_eq!(byte_offset(text, 3), 5, "the end");
        assert_eq!(byte_offset(text, 99), 5, "past the end is the end");
    }

    #[test]
    fn a_program_that_cannot_be_here_is_not_installed() {
        assert!(!installed("acpilot-no-such-language-server"));
    }

    #[test]
    fn a_program_given_as_a_path_is_installed_at_that_path() {
        let program = std::env::current_exe().expect("the test binary has a path");
        assert!(
            installed(program.to_str().expect("utf-8 path")),
            "the file exists; PATH is not part of that answer"
        );
    }

    // "A program that is on PATH is installed" is asserted in `crate::platform`,
    // per platform: the name a program has on `PATH` is the platform's fact, and the
    // workspace's boundary test says a `cfg` naming one may not stand here.
}
