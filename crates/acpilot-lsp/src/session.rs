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

/// A running server, initialized and ready to answer.
pub struct Session {
    server: Server,
    encoding: PositionEncoding,
    program: String,
    versions: HashMap<String, i32>,
    warm: bool,
}

impl Session {
    /// Spawn `program` in `root` and complete the handshake.
    ///
    /// # Errors
    ///
    /// A binary that cannot be started, or a server that does not answer
    /// `initialize` with a result.
    pub async fn start(program: &str, args: &[String], root: &Path) -> Result<Self, SessionError> {
        let mut server = Server::spawn(program, args, root)?;
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

    /// Ask the server to stop, then insist.
    pub async fn shutdown(&mut self) {
        self.server.shutdown().await;
    }
}

fn file_uri(path: &Path) -> Result<Url, SessionError> {
    Url::from_file_path(path).map_err(|()| SessionError::Uri(path.display().to_string()))
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
    fn both_documented_shapes_are_read() {
        let answer = json!([
            {
                "uri": "file:///work/app/src/lib.rs",
                "range": { "start": { "line": 4, "character": 8 }, "end": { "line": 4, "character": 12 } }
            },
            {
                "targetUri": "file:///work/app/src/main.rs",
                "targetRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                "targetSelectionRange": { "start": { "line": 9, "character": 3 }, "end": { "line": 9, "character": 7 } }
            }
        ]);
        let found = locations(&answer);
        assert_eq!(
            found,
            vec![
                Location {
                    path: PathBuf::from("/work/app/src/lib.rs"),
                    line: 4,
                    character: 8
                },
                Location {
                    path: PathBuf::from("/work/app/src/main.rs"),
                    line: 9,
                    character: 3
                },
            ],
            "a link is read from its selection range, not its whole target"
        );
    }

    #[test]
    fn percent_encoding_and_spaces_survive_the_trip() {
        let answer = json!([{
            "uri": "file:///work/my%20app/%C3%BCn%C3%AFcode/lib.rs",
            "range": { "start": { "line": 1, "character": 2 }, "end": { "line": 1, "character": 3 } }
        }]);
        assert_eq!(
            locations(&answer)[0].path,
            PathBuf::from("/work/my app/ünïcode/lib.rs")
        );
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
