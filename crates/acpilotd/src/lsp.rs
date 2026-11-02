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

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use acpilot_lsp::position::PositionEncoding;
use acpilot_lsp::session::{Location, Session};
use tokio::sync::Mutex;

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

/// The sessions this daemon has started, one per language.
///
/// `Debug` is written out rather than derived: a [`Session`] owns pipes and a child
/// process, and the only fact worth printing is which languages have one running.
#[derive(Default)]
pub struct Servers {
    sessions: Mutex<HashMap<&'static str, Option<Session>>>,
}

impl std::fmt::Debug for Servers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.sessions.try_lock().ok().map(|sessions| {
            sessions
                .iter()
                .filter(|(_, session)| session.is_some())
                .map(|(language, _)| *language)
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
        root: &Path,
        path: &Path,
        text: &str,
        offset: usize,
    ) -> Option<Answer> {
        let (language, program, args) = SERVERS
            .iter()
            .find(|(id, _, _)| *id == language)
            .map(|(id, program, args)| (*id, *program, *args))?;
        if !installed(program) {
            return None;
        }

        let mut sessions = self.sessions.lock().await;
        if !sessions.contains_key(language) {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            let started = Session::start(program, &args, root).await;
            match &started {
                Ok(session) => {
                    tracing::debug!(server = program, encoding = %session.encoding(), "language server ready");
                }
                Err(error) => {
                    tracing::debug!(server = program, %error, "language server did not start");
                }
            }
            sessions.insert(language, started.ok());
        }
        let session = sessions.get_mut(language)?.as_mut()?;

        // The caret's line and column are the server's encoding, and only the server
        // knows which one it picked: the conversion happens here, once it has. The
        // offset arrives counted in characters, because that is the editor's unit for
        // a caret and the request is about the character under it.
        let encoding = session.encoding();
        let index = acpilot_lsp::LineIndex::new(text);
        let position = index.position(text, byte_offset(text, offset), encoding)?;

        if let Err(error) = session.open(path, language, text).await {
            tracing::debug!(server = program, %error, "language server lost the document");
            // A session whose transport broke is not reusable: drop it so the next
            // request starts a fresh one rather than failing forever.
            sessions.insert(language, None);
            return None;
        }

        // A cold server answers `[]` until it has indexed the document it was just
        // handed, so an empty answer is only believed once the session has answered
        // with something or the attempts run out. The first question pays for the
        // index; later ones are one round trip.
        let mut attempts = 0;
        loop {
            attempts += 1;
            match session.references(path, position).await {
                Ok(locations) if !locations.is_empty() => {
                    session.mark_warm();
                    return Some(Answer {
                        program,
                        encoding,
                        locations,
                    });
                }
                Ok(locations) => {
                    if session.warm() || attempts == COLD_ATTEMPTS {
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
                    sessions.insert(language, None);
                    return None;
                }
            }
        }
    }
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
#[must_use]
pub fn installed(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let names = crate::platform::executable_names(program);
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

    // "A program that is on PATH is installed" is asserted in `crate::platform`,
    // per platform: the name a program has on `PATH` is the platform's fact, and the
    // workspace's boundary test says a `cfg` naming one may not stand here.
}
