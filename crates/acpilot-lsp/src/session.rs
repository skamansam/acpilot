//! One language server, initialized and asked questions.
//!
//! [`crate::client::Server`] is the transport: spawn, frame, request, answer the
//! server's own questions. This is the protocol above it — the handshake that
//! negotiates a position encoding, the document sync that makes a file known to the
//! server, and `textDocument/references`, which is the request a smart references
//! list is.
//!
//! # Why the encoding is recorded rather than assumed
//!
//! LSP defaults to UTF-16 columns and the editor's buffer is UTF-8 bytes. Two of the
//! three servers measured in `docs/notes/lsp-servers.md` say nothing and mean UTF-16;
//! `rust-analyzer` picks UTF-8 when offered. So the session advertises both, records
//! what came back, and reports every location in that encoding — the caller converts,
//! because only the caller has the text.
//!
//! # Documents are opened from what the caller has
//!
//! A server answers about a document it has been told about, so [`Session::open`]
//! sends the text: `didOpen` the first time and a full `didChange` after that. Full
//! sync rather than incremental is the honest choice here — the caller already has
//! the whole file, and an incremental change would be a second diff to get wrong.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use url::Url;

use crate::client::{ClientError, Server};
use crate::position::PositionEncoding;

/// How long `initialize` may take. A server that has not answered in twenty seconds
/// is a server that is not going to, and a references request is not worth waiting
/// behind it for.
const INITIALIZE: Duration = Duration::from_secs(20);

/// How long a `textDocument/references` may take.
const REQUEST: Duration = Duration::from_secs(10);

/// How long to wait between looks for a server's diagnostics.
///
/// A publish is pushed rather than answered, so there is nothing to await: the loop
/// polls the transport the way `tick` polls a frame, and twenty milliseconds is short
/// enough that a panel does not appear to be late and long enough that a server which
/// says nothing costs nothing.
const POLL: Duration = Duration::from_millis(20);

/// How many problems one file may report before the rest are dropped.
///
/// The same cap the design notes put on a diagnostics store (5k per file): a server
/// that has decided every line is wrong has said enough, and a list that long is not
/// read but scrolled past.
const PROBLEMS_CAP: usize = 5_000;

/// Why a session could not be started, or a question not answered.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The transport or the server failed.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// A path could not be spelled as a `file://` URI, or a URI back into a path.
    #[error("{0} is not a file path")]
    Uri(String),
}

/// One place the server says a name is used, in the workspace's own spelling.
///
/// The line and character are the server's, in the encoding
/// [`Session::encoding`] reports: converting them to bytes is the caller's job
/// because the caller is the one holding the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Absolute path, as the server named it.
    pub path: PathBuf,
    /// Zero-based line.
    pub line: u32,
    /// Zero-based column in the negotiated encoding.
    pub character: u32,
}

/// How bad a problem is, in the four levels the protocol defines.
///
/// An absent severity is [`Severity::Warning`]: the protocol leaves the choice to the
/// client, and a problem with no level is a thing the server thought worth saying
/// rather than a compile error — calling it an error would paint a clean build red.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The code does not compile.
    Error,
    /// It compiles and probably should not.
    Warning,
    /// A remark.
    Information,
    /// A suggestion.
    Hint,
}

/// One problem a server reported about a document.
///
/// The lines are zero-based, as the protocol counts them, and the characters are in
/// the session's negotiated encoding — the same rule [`Location`] follows, because the
/// caller is the one holding the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Zero-based line the problem starts on.
    pub line: u32,
    /// Zero-based column it starts at, in the negotiated encoding.
    pub character: u32,
    /// Zero-based line it ends on.
    pub end_line: u32,
    /// Zero-based column it ends at, in the negotiated encoding.
    pub end_character: u32,
    /// How bad it is.
    pub severity: Severity,
    /// What the server said.
    pub message: String,
    /// Who said it, when the server names itself.
    pub source: Option<String>,
    /// The server's code for it, when there is one.
    pub code: Option<String>,
}

/// One edit a formatter asked for.
///
/// The positions are in the session's negotiated encoding, like every other
/// position this crate returns. Applying an edit is the caller's job because the
/// caller is the one holding the text, and an edit applied to different text is an
/// edit that never matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatEdit {
    /// Where the edit starts.
    pub start: crate::position::Position,
    /// Where it ends.
    pub end: crate::position::Position,
    /// What replaces that range.
    pub text: String,
}

/// What a server published about a document this session opened.
///
/// The path rather than the URI: a publish names a document the way the protocol
/// spells one, and the only caller that can turn that back into something a workspace
/// can compare is the session, which was handed the path when it opened it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// The document, as this session opened it.
    pub path: PathBuf,
    /// The version the problems are about, when the server names one.
    ///
    /// Most servers leave it out of the publish — `rust-analyzer` does — so this is a
    /// fact to pass along rather than one to require.
    pub version: Option<i64>,
    /// What is wrong with it.
    pub problems: Vec<Problem>,
}

/// One `publishDiagnostics` notification, as the wire spells it.
struct Announcement {
    uri: String,
    version: Option<i64>,
    problems: Vec<Problem>,
}

/// A running server, initialized and ready to answer.
pub struct Session {
    server: Server,
    encoding: PositionEncoding,
    program: String,
    versions: HashMap<String, i32>,
    warm: bool,
    /// Where each document this session opened lives, by document URI.
    ///
    /// A publish names a URI, and a URI is a spelling rather than a path a workspace
    /// root can be compared against. Filled by [`Session::open`], which is the only
    /// way a document comes to exist for a server.
    documents: HashMap<String, PathBuf>,
    /// The last problems the server published, by document URI.
    ///
    /// Kept because a publish is pushed and a client asks later: the message that
    /// carried the answer is consumed by whoever was draining the transport at the
    /// time, which may have been a different request entirely.
    published: HashMap<String, Vec<Problem>>,
}

impl Session {
    /// Spawn `program` in `root` and complete the handshake.
    ///
    /// # Errors
    ///
    /// A binary that cannot be started, or a server that does not answer
    /// `initialize` with a result.
    pub async fn start(program: &str, args: &[String], root: &Path) -> Result<Self, SessionError> {
        let server = Server::spawn(program, args, root)?;
        Self::handshake(server, program, root).await
    }

    /// A session over a server somebody else started and connected.
    ///
    /// The daemon uses this for a server on an SSH or WSL host: the bytes are the same
    /// bytes, the handshake is the same handshake, and only the pipe's far end moved.
    ///
    /// # Errors
    ///
    /// The server does not answer `initialize` with a result.
    pub async fn attach(server: Server, program: &str, root: &Path) -> Result<Self, SessionError> {
        Self::handshake(server, program, root).await
    }

    async fn handshake(
        mut server: Server,
        program: &str,
        root: &Path,
    ) -> Result<Self, SessionError> {
        let root_uri = Url::from_directory_path(root)
            .map_err(|()| SessionError::Uri(root.display().to_string()))?;
        let answer = server
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri.as_str(),
                    "workspaceFolders": [{ "uri": root_uri.as_str(), "name": "workspace" }],
                    "capabilities": {
                        "general": { "positionEncodings": ["utf-8", "utf-16"] },
                        "workspace": { "workspaceFolders": true },
                        "textDocument": {
                            "synchronization": { "didSave": false, "dynamicRegistration": false },
                            "references": { "dynamicRegistration": false }
                        }
                    },
                    "clientInfo": { "name": "acpilot", "version": env!("CARGO_PKG_VERSION") }
                }),
                INITIALIZE,
            )
            .await?;
        let encoding = PositionEncoding::negotiated(
            answer
                .get("capabilities")
                .and_then(|capabilities| capabilities.get("positionEncoding"))
                .and_then(Value::as_str),
        );
        server.notify("initialized", json!({})).await?;
        Ok(Self {
            server,
            encoding,
            program: program.to_owned(),
            versions: HashMap::new(),
            warm: false,
            documents: HashMap::new(),
            published: HashMap::new(),
        })
    }

    /// Whether this server has answered a question with something in it.
    ///
    /// A server that has just been told about a document answers an empty list until
    /// it has indexed it — measured against `rust-analyzer`, which answers `[]` to a
    /// `textDocument/references` sent right after `didOpen`. The flag is how a caller
    /// knows that the first empty answer is the index rather than the truth, and that
    /// later ones are answers.
    #[must_use]
    pub fn warm(&self) -> bool {
        self.warm
    }

    /// Remember that the server has answered with something.
    pub fn mark_warm(&mut self) {
        self.warm = true;
    }

    /// The encoding this server's positions are counted in.
    #[must_use]
    pub fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    /// The command this session runs, for a status line or a log.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Make `text` the server's idea of `path`, opening or replacing it.
    ///
    /// # Errors
    ///
    /// A path that is not on this filesystem, or a transport that has gone away.
    pub async fn open(
        &mut self,
        path: &Path,
        language_id: &str,
        text: &str,
    ) -> Result<(), SessionError> {
        let uri = file_uri(path)?;
        let key = uri.to_string();
        // The previous version's problems are not this version's: dropping them here
        // is what makes the wait in [`Session::diagnostics`] wait for a publish about
        // the text that was just sent rather than return the answer to the old one.
        self.published.remove(&key);
        // Remembered because the answers come back naming the URI, and turning one
        // into a path is this session's job: it is the only place the path was known.
        self.documents.insert(key.clone(), path.to_path_buf());
        let version = self.versions.entry(key.clone()).or_insert(0);
        *version += 1;
        let version = *version;
        if version == 1 {
            self.server
                .notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": {
                        "uri": key,
                        "languageId": language_id,
                        "version": version,
                        "text": text,
                    }}),
                )
                .await?;
        } else {
            self.server
                .notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": key, "version": version },
                        "contentChanges": [{ "text": text }],
                    }),
                )
                .await?;
        }
        Ok(())
    }

    /// Where the server says the name at `position` is used, itself included.
    ///
    /// # Errors
    ///
    /// The server refused the request, the transport failed, or the answer did not
    /// arrive in [`REQUEST`].
    pub async fn references(
        &mut self,
        path: &Path,
        position: crate::position::Position,
    ) -> Result<Vec<Location>, SessionError> {
        let uri = file_uri(path)?;
        let answer = self
            .server
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": position.line, "character": position.character },
                    // The definition is a place the name is written, and a list that
                    // hides it is a list with a row missing.
                    "context": { "includeDeclaration": true }
                }),
                REQUEST,
            )
            .await?;
        Ok(locations(&answer))
    }

    /// Ask the server to format `path`, which it must already know through
    /// [`Session::open`].
    ///
    /// The answer is edits rather than a whole document: that is what the protocol
    /// promises, and applying them here would make this layer own the text. A server
    /// that refuses the request is an error; a server that answers `null` or an empty
    /// array has no edits to make, which is a successful format.
    ///
    /// # Errors
    ///
    /// A path that is not on this filesystem, a server that refused the request, or
    /// one that did not answer in [`REQUEST`].
    pub async fn format(
        &mut self,
        path: &Path,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Vec<FormatEdit>, SessionError> {
        let uri = file_uri(path)?;
        let answer = self
            .server
            .request(
                "textDocument/formatting",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "options": { "tabSize": tab_size, "insertSpaces": insert_spaces },
                }),
                REQUEST,
            )
            .await?;
        Ok(format_edits(Some(&answer)))
    }

    /// Wait for what the server says is wrong with `path`.
    ///
    /// A publish is a notification, not an answer, so this polls until one arrives or
    /// `wait` runs out. An empty list is both a clean file and a server that said
    /// nothing in time, and the two are told apart by the caller: it knows how long it
    /// waited and whether a session exists at all.
    ///
    /// # Errors
    ///
    /// A path that is not on this filesystem. A server that has gone away publishes
    /// nothing, which is the deadline rather than an error.
    pub async fn diagnostics(
        &mut self,
        path: &Path,
        wait: Duration,
    ) -> Result<Vec<Problem>, SessionError> {
        let uri = file_uri(path)?;
        let key = uri.to_string();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            while let Some(message) = self.server.try_next_notification() {
                self.record(&message);
            }
            if let Some(problems) = self.published.get(&key) {
                return Ok(problems.clone());
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(Vec::new());
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Take every problem the server has pushed since this was last called.
    ///
    /// The transport reads whether or not anybody is asking — its pump fills a queue
    /// — so a server that finishes an index and publishes afterwards has said
    /// something no request was there to hear. Draining is how a daemon that is not
    /// asking still learns about it, and the same [`Session::diagnostics`] call later
    /// finds the answer already in [`Self::published`], because both read the queue
    /// through [`Session::record`].
    pub fn drain_published(&mut self) -> Vec<Published> {
        let mut found = Vec::new();
        while let Some(message) = self.server.try_next_notification() {
            let Some(announcement) = self.record(&message) else {
                continue;
            };
            // A publish about a document this session never opened is not something
            // a caller can place: there is no path to compare it against. Dropping it
            // is the honest answer to a question nobody asked.
            let Some(path) = self.documents.get(&announcement.uri) else {
                continue;
            };
            found.push(Published {
                path: path.clone(),
                version: announcement.version,
                problems: announcement.problems,
            });
        }
        found
    }

    /// Keep the problems a notification carries, if it carries any.
    ///
    /// Every notification the session drains passes through here, because the one
    /// that matters is whatever arrived while nobody was asking: throwing the rest
    /// away would make a diagnostics answer depend on which request happened to be
    /// in flight when the server spoke.
    fn record(&mut self, message: &serde_json::Value) -> Option<Announcement> {
        let announcement = announcement(message)?;
        self.published
            .insert(announcement.uri.clone(), announcement.problems.clone());
        Some(announcement)
    }

    /// Ask the server to stop, then insist.
    pub async fn shutdown(&mut self) {
        self.server.shutdown().await;
    }
}

/// One `publishDiagnostics` notification, read into the pieces a caller places.
///
/// Anything that is not one of those is nothing, which is what makes a drained queue
/// safe to pass through: the protocol lets a server talk about a lot of things, and
/// only one of them is a problem with a document.
fn announcement(message: &Value) -> Option<Announcement> {
    if message.get("method").and_then(Value::as_str) != Some("textDocument/publishDiagnostics") {
        return None;
    }
    let params = message.get("params")?;
    let uri = params.get("uri").and_then(Value::as_str)?.to_owned();
    Some(Announcement {
        uri,
        version: params.get("version").and_then(Value::as_i64),
        problems: problems(params.get("diagnostics")),
    })
}

fn file_uri(path: &Path) -> Result<Url, SessionError> {
    Url::from_file_path(path).map_err(|()| SessionError::Uri(path.display().to_string()))
}

/// The problems in a `publishDiagnostics` params' `diagnostics` array.
///
/// Anything without a range is dropped rather than placed at the start of the file: a
/// problem nobody can point at is a problem the gutter would draw in the wrong place,
/// and a document-level message is what `window/showMessage` is for.
#[must_use]
pub fn problems(answer: Option<&Value>) -> Vec<Problem> {
    let Some(items) = answer.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for item in items {
        let Some(range) = item.get("range") else {
            continue;
        };
        let Some(start) = at(range.get("start")) else {
            continue;
        };
        let (end_line, end_character) = at(range.get("end")).unwrap_or(start);
        let Some(message) = item.get("message").and_then(Value::as_str) else {
            continue;
        };
        found.push(Problem {
            line: start.0,
            character: start.1,
            end_line,
            end_character,
            severity: severity(item.get("severity").and_then(Value::as_u64)),
            message: message.to_owned(),
            source: item
                .get("source")
                .and_then(Value::as_str)
                .map(str::to_owned),
            code: item.get("code").and_then(spelled),
        });
        if found.len() == PROBLEMS_CAP {
            break;
        }
    }
    found
}

/// A `{ line, character }` pair, when both numbers are there.
fn at(position: Option<&Value>) -> Option<(u32, u32)> {
    let position = position?;
    let line = position.get("line")?.as_u64()?;
    let character = position.get("character")?.as_u64()?;
    Some((u32::try_from(line).ok()?, u32::try_from(character).ok()?))
}

/// The protocol's severity number, with the client's default for an absent one.
fn severity(level: Option<u64>) -> Severity {
    match level {
        Some(1) => Severity::Error,
        Some(3) => Severity::Information,
        Some(4) => Severity::Hint,
        _ => Severity::Warning,
    }
}

/// A diagnostic code, which the protocol allows to be a string or a number.
fn spelled(value: &Value) -> Option<String> {
    match value {
        Value::String(code) => Some(code.clone()),
        Value::Number(code) => Some(code.to_string()),
        _ => None,
    }
}

/// The edits in a `textDocument/formatting` answer.
///
/// `null` and an empty array both mean "nothing to change"; anything inside an
/// edit that is not a range is dropped rather than defaulted to the start of the
/// file, because a malformed edit placed at byte zero would rewrite the document.
#[must_use]
pub fn format_edits(answer: Option<&Value>) -> Vec<FormatEdit> {
    let Some(items) = answer.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let range = item.get("range")?;
            let start = range.get("start")?;
            let end = range.get("end")?;
            Some(FormatEdit {
                start: crate::position::Position::new(
                    u32::try_from(start.get("line")?.as_u64()?).ok()?,
                    u32::try_from(start.get("character")?.as_u64()?).ok()?,
                ),
                end: crate::position::Position::new(
                    u32::try_from(end.get("line")?.as_u64()?).ok()?,
                    u32::try_from(end.get("character")?.as_u64()?).ok()?,
                ),
                text: item.get("newText")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

/// The locations in a `textDocument/references` answer.
///
/// Two shapes are legal and servers pick either: `Location { uri, range }` and
/// `LocationLink { targetUri, targetSelectionRange }`. A row is the start of the
/// range, which is what a list can jump to; anything that is not a `file://` URI is
/// dropped rather than invented as a path.
#[must_use]
pub fn locations(answer: &Value) -> Vec<Location> {
    let Some(items) = answer.as_array() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for item in items {
        let (uri, range) = match (item.get("uri"), item.get("range")) {
            (Some(uri), Some(range)) => (uri, range),
            _ => {
                let uri = item
                    .get("targetUri")
                    .or_else(|| item.get("targetUri_template"));
                let range = item
                    .get("targetSelectionRange")
                    .or_else(|| item.get("targetRange"));
                match (uri, range) {
                    (Some(uri), Some(range)) => (uri, range),
                    _ => continue,
                }
            }
        };
        let Some(path) = uri
            .as_str()
            .and_then(|uri| Url::parse(uri).ok())
            .and_then(|url| url.to_file_path().ok())
        else {
            continue;
        };
        let start = range.get("start");
        let line = start
            .and_then(|start| start.get("line"))
            .and_then(Value::as_u64)
            .and_then(|line| u32::try_from(line).ok());
        let character = start
            .and_then(|start| start.get("character"))
            .and_then(Value::as_u64)
            .and_then(|character| u32::try_from(character).ok());
        if let (Some(line), Some(character)) = (line, character) {
            found.push(Location {
                path,
                line,
                character,
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_edits_carry_their_range_and_replacement() {
        let answer = json!([
            {
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 3 }
                },
                "newText": "four"
            },
            {
                "range": {
                    "start": { "line": 2, "character": 1 },
                    "end": { "line": 2, "character": 1 }
                },
                "newText": "\n"
            }
        ]);
        let edits = format_edits(Some(&answer));
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].start, crate::position::Position::new(0, 0));
        assert_eq!(edits[0].end, crate::position::Position::new(0, 3));
        assert_eq!(edits[0].text, "four");
        assert_eq!(edits[1].text, "\n");
    }

    #[test]
    fn a_null_or_malformed_format_answer_makes_no_edits() {
        assert!(format_edits(Some(&Value::Null)).is_empty());
        assert!(format_edits(Some(&json!([{ "newText": "x" }]))).is_empty());
        assert!(format_edits(None).is_empty());
    }

    /// The `file://` spelling of an absolute path, as a server sends one.
    ///
    /// Built rather than written out: `file:///work/...` is not a path Windows can turn
    /// back into one, and these tests are about the conversion rather than about a
    /// Unix spelling of it.
    fn uri_of(path: &std::path::Path) -> String {
        url::Url::from_file_path(path)
            .expect("an absolute path")
            .to_string()
    }

    #[test]
    fn both_documented_shapes_are_read() {
        let directory = std::env::temp_dir().join("acpilot-lsp-shapes").join("src");
        let lib = directory.join("lib.rs");
        let main = directory.join("main.rs");
        let answer = json!([
            {
                "uri": uri_of(&lib),
                "range": { "start": { "line": 4, "character": 8 }, "end": { "line": 4, "character": 12 } }
            },
            {
                "targetUri": uri_of(&main),
                "targetRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                "targetSelectionRange": { "start": { "line": 9, "character": 3 }, "end": { "line": 9, "character": 7 } }
            }
        ]);
        let found = locations(&answer);
        assert_eq!(
            found,
            vec![
                Location {
                    path: lib,
                    line: 4,
                    character: 8
                },
                Location {
                    path: main,
                    line: 9,
                    character: 3
                },
            ],
            "a link is read from its selection range, not its whole target"
        );
    }

    #[test]
    fn percent_encoding_and_spaces_survive_the_trip() {
        let file = std::env::temp_dir()
            .join("acpilot lsp")
            .join("ünïcode")
            .join("lib.rs");
        let answer = json!([{
            "uri": uri_of(&file),
            "range": { "start": { "line": 1, "character": 2 }, "end": { "line": 1, "character": 3 } }
        }]);
        assert!(
            !uri_of(&file).contains(' '),
            "the space really is percent-encoded, which is what the test is about: {}",
            uri_of(&file)
        );
        assert_eq!(
            locations(&answer)[0].path,
            file,
            "and the URI decodes back to the path it came from"
        );
    }

    #[test]
    fn a_publish_is_read_into_problems_with_both_range_ends() {
        let diagnostics = json!([
            {
                "range": { "start": { "line": 2, "character": 4 }, "end": { "line": 2, "character": 9 } },
                "severity": 1,
                "message": "cannot find value `nope`",
                "source": "rust-analyzer",
                "code": "E0425",
                "tags": []
            },
            {
                "range": { "start": { "line": 7, "character": 0 } },
                "message": "unused"
            },
            { "message": "nowhere to point at" }
        ]);
        let found = problems(Some(&diagnostics));
        assert_eq!(
            found.len(),
            2,
            "a problem with no range is not placed anywhere"
        );
        assert_eq!(found[0].severity, Severity::Error);
        assert_eq!((found[0].line, found[0].character), (2, 4));
        assert_eq!((found[0].end_line, found[0].end_character), (2, 9));
        assert_eq!(found[0].code.as_deref(), Some("E0425"));
        assert_eq!(found[0].source.as_deref(), Some("rust-analyzer"));
        assert_eq!(
            found[1].severity,
            Severity::Warning,
            "an absent severity is a remark, not a red line"
        );
        assert_eq!(
            (found[1].end_line, found[1].end_character),
            (7, 0),
            "a range with no end is a point"
        );
    }

    #[test]
    fn a_publish_is_read_as_an_announcement_with_its_version() {
        let message = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///work/app/src/lib.rs",
                "version": 7,
                "diagnostics": [{
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                    "severity": 1,
                    "message": "nope"
                }]
            }
        });
        let announced = announcement(&message).expect("a publish");
        assert_eq!(announced.uri, "file:///work/app/src/lib.rs");
        assert_eq!(announced.version, Some(7));
        assert_eq!(announced.problems.len(), 1);

        // A server that leaves the version out is still saying something, and most
        // of them do: the number is passed along rather than required.
        let unversioned = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": "file:///work/app/src/lib.rs", "diagnostics": [] }
        });
        let announced = announcement(&unversioned).expect("still a publish");
        assert_eq!(announced.version, None);
        assert!(announced.problems.is_empty());

        // Everything else a server may say is not this.
        for other in [
            json!({ "method": "window/logMessage", "params": { "type": 3 } }),
            json!({ "method": "textDocument/publishDiagnostics" }),
            json!({ "result": [] }),
            json!(null),
        ] {
            assert!(
                announcement(&other).is_none(),
                "{other} is not a publish about a document"
            );
        }
    }

    #[test]
    fn a_numeric_code_is_spelled_rather_than_dropped() {
        let diagnostics = json!([{
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
            "severity": 3,
            "message": "hint",
            "code": 12
        }]);
        let found = problems(Some(&diagnostics));
        assert_eq!(found[0].code.as_deref(), Some("12"));
        assert_eq!(found[0].severity, Severity::Information);
    }

    #[test]
    fn nothing_that_is_not_a_file_is_a_row() {
        let answer = json!([
            { "uri": "https://example.com/x.rs", "range": { "start": { "line": 0, "character": 0 } } },
            { "uri": "file:///work/app/src/lib.rs" },
            { "uri": 7, "range": { "start": { "line": 0, "character": 0 } } },
            { "range": { "start": { "line": 0, "character": 0 } } }
        ]);
        assert!(locations(&answer).is_empty());
        assert!(locations(&json!(null)).is_empty());
        assert!(locations(&json!({})).is_empty());
    }
}
