//! `agents.toml`: the file that says what an agent is and how to start it.
//!
//! The manifest is data. Adding an agent is a line in a file, never a rebuild,
//! and every rule here is a refusal rather than a guess:
//!
//! - an unknown key is an error, the same rule the rest of the configuration
//!   follows, because a misspelled `sentinel` would otherwise mean "no sentinel"
//!   and the agent would look healthy while timing out on every turn;
//! - a field that has no meaning for the chosen transport is an error too — a
//!   `sentinel` on an ACP agent is a misunderstanding worth reporting;
//! - the shape of each transport lives in [`Transport`], so "an `acp-stdio`
//!   agent has a command" is a fact about the type rather than three checks
//!   repeated in the supervisor.
//!
//! `transport` accepts the wire spelling (`acp_stdio`) as well as the one the
//! documents use (`acp-stdio`), because the value shown by `GET /v1/agents` is
//! what people copy. The default it does *not* excuse is a typo: anything else
//! is rejected with the list of valid values.
//!
//! [`json_schema`] publishes the same rules as JSON Schema for editors, and it is
//! generated from these types rather than written by hand: a second copy of the
//! field list would be a second thing to forget. The one rule the schema cannot
//! carry is the loader's alone to state — a duplicate `id` (JSON Schema has no
//! keyword for uniqueness by a key) and a `keyring:` reference that does not
//! resolve.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use acpilot_proto::agent::AgentTransport;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use url::Url;

use crate::error::AgentError;
use crate::secret::{Secret, SecretRef, SecretResolver};

/// The file the registry reads, inside a profile directory.
pub const MANIFEST_FILE: &str = "agents.toml";

/// Schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Where the published JSON Schema for this file lives.
///
/// `site/public/schema/agents-v1.json` is served from the docs site under its
/// base path, so publishing the docs publishes the schema: one commit, no second
/// service and no redirect to go stale. The `-v1` is the major version, and that
/// file is frozen once shipped — a change that would make a `schema_version = 1`
/// manifest mean something else is `agents-v2.json` and [`SCHEMA_VERSION`] `2`.
/// A moving URL is what silently puts an editor's cached copy out of step with
/// the loader.
pub const SCHEMA_URL: &str = "https://rudeboy.dev/acpilot/schema/agents-v1.json";

/// The published schema, relative to the repository root.
///
/// Kept here so the generator, the drift test and the docs cannot disagree about
/// where the artifact is committed.
pub const SCHEMA_PATH: &str = "site/public/schema/agents-v1.json";

/// The presets this build ships, embedded so a new profile can be written with a
/// usable manifest before any extension store exists.
///
/// The same bytes are the `acpilot.agents` built-in, which is why a fresh profile
/// and `ext why` agree until the user edits the copy. The path is relative to
/// this file, and `include_str!` makes a moved preset a build error.
const SHIPPED_PRESETS: &str = include_str!("../../../extensions/acpilot.agents/agents.toml");

/// The manifest a new profile starts with: the `$schema` line, then the presets.
///
/// The schema key comes first because TOML would file a bare key under the last
/// `[[agents]]` table, and it points at the published document so an editor can
/// complete the file a person is about to edit. Writers use this rather than an
/// empty template: `agents = []` is refused, and "delete the file instead of
/// emptying it" is not a useful first instruction.
#[must_use]
pub fn initial_manifest() -> String {
    format!("\"$schema\" = \"{SCHEMA_URL}\"\n\n{SHIPPED_PRESETS}")
}

/// Gap between typing a prompt and pressing Enter in a PTY agent.
///
/// Design phase's task P0-2: sending `text` and Enter in one write silently fails to submit in
/// Freebuff and looks exactly like a completed turn. The value is a starting
/// point, not a measurement — the engine phase's task P2-7 owns tuning it per parser.
pub const DEFAULT_SUBMIT_DELAY: Duration = Duration::from_millis(50);

/// Backstop when a PTY agent's sentinel never fires.
///
/// Design phase's task P0-2 measured this as the smallest timeout that truncated no turn: an idle
/// timeout below 5 s truncated 4 of 5 real turns, because the model can think
/// for 3.4 s between two chunks of the same answer.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_millis(5_000);

/// One agent, validated and with its secrets already resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    /// Stable identifier, used by `--agent` and in the API.
    pub id: String,
    /// Display name. Defaults to the id.
    pub name: String,
    /// One sentence on what the agent is for, when the manifest says.
    pub description: Option<String>,
    /// How the daemon drives it, with that transport's settings.
    pub transport: Transport,
    /// What to do when the process dies.
    pub restart: Restart,
    /// Environment for the child, with references already resolved.
    pub env: BTreeMap<String, Secret>,
    /// Whether this agent may name another profile's keyring entries.
    pub allow_cross_profile: bool,
}

impl AgentSpec {
    /// The transport as the API spells it.
    pub fn transport_kind(&self) -> AgentTransport {
        self.transport.kind()
    }

    /// The command that will be spawned, when this transport spawns one.
    pub fn command(&self) -> Option<&CommandSpec> {
        self.transport.command()
    }
}

/// A program and its arguments, passed verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Executable, resolved on the host the workspace lives on.
    pub program: String,
    /// Arguments, in order.
    pub args: Vec<String>,
}

impl CommandSpec {
    /// The command as one line, for a status row or an error message.
    ///
    /// Display only: this is not a shell string and must not be handed to one.
    /// Arguments that would be ambiguous in a line are quoted so two different
    /// commands cannot render identically.
    pub fn command_line(&self) -> String {
        let mut line = quoted(&self.program);
        for arg in &self.args {
            line.push(' ');
            line.push_str(&quoted(arg));
        }
        line
    }
}

fn quoted(part: &str) -> String {
    let plain = !part.is_empty()
        && part.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '@')
        });
    if plain {
        part.to_owned()
    } else {
        format!("{part:?}")
    }
}

/// How an agent is driven.
///
/// The variant is the transport, so a field that only one transport uses cannot
/// be read for another: there is no `Option<Parser>` to unwrap in the
/// supervisor, and no way to forget that an HTTP agent has no command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// Native ACP over stdio.
    AcpStdio {
        /// The program that speaks ACP.
        command: CommandSpec,
    },
    /// A program driven through a PTY and a transcript parser.
    Pty {
        /// The program to drive.
        command: CommandSpec,
        /// How its screen becomes a transcript.
        profile: PtyProfile,
    },
    /// ACP over HTTP/JSON-RPC.
    HttpJsonRpc {
        /// Where the agent answers.
        url: Url,
        /// The credential, when the endpoint needs one.
        auth: Option<Secret>,
    },
}

impl Transport {
    /// The transport as the API spells it.
    pub fn kind(&self) -> AgentTransport {
        match self {
            Self::AcpStdio { .. } => AgentTransport::AcpStdio,
            Self::Pty { .. } => AgentTransport::Pty,
            Self::HttpJsonRpc { .. } => AgentTransport::HttpJsonRpc,
        }
    }

    /// The program this transport spawns, if any.
    pub fn command(&self) -> Option<&CommandSpec> {
        match self {
            Self::AcpStdio { command } | Self::Pty { command, .. } => Some(command),
            Self::HttpJsonRpc { .. } => None,
        }
    }

    /// The transport as the manifest spells it, which is also what diagnostics
    /// should say: a user edits this word, so an error should not translate it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::AcpStdio { .. } => "acp-stdio",
            Self::Pty { .. } => "pty",
            Self::HttpJsonRpc { .. } => "http-jsonrpc",
        }
    }
}

/// What a PTY agent needs beyond its command.
///
/// Every field is one of the design phase's P0-2 findings: a sentinel is the primary
/// turn-complete signal and an idle timeout is only the backstop, so neither can
/// be left to a default that guesses per program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyProfile {
    /// How the screen becomes a transcript.
    pub parser: Parser,
    /// Text that identifies the input row, so chrome above it can be cut away.
    pub prompt_row: Option<String>,
    /// The input row back at rest: the primary turn-complete signal.
    pub sentinel: String,
    /// What to type to leave cleanly. `Ctrl-D` is not universal.
    pub exit_command: Option<String>,
    /// Gap between the text and the Enter key.
    pub submit_delay: Duration,
    /// Backstop when the sentinel does not fire.
    pub idle_timeout: Duration,
}

/// A transcript parser named by a PTY agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parser {
    /// The screen as text, no escape sequences.
    PlainTranscript,
    /// The screen with attributes, so colours survive into the transcript.
    AnsiTranscript,
    /// An escape hatch: a user-supplied parser at this path.
    ///
    /// Parsing a *manifest* only records the path. Whether a script may run at
    /// all is a policy question for the engine phase's task P2-7, which owns the loader.
    Script(PathBuf),
}

impl Parser {
    /// Parse the manifest's `parser` value.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "plain-transcript" => Ok(Self::PlainTranscript),
            "ansi-transcript" => Ok(Self::AnsiTranscript),
            other => match other.strip_prefix("script:") {
                Some(path) if !path.trim().is_empty() => Ok(Self::Script(PathBuf::from(path))),
                Some(_) => Err("`script:` needs a path after it".to_owned()),
                None => Err(format!(
                    "{other:?} is not `plain-transcript`, `ansi-transcript`, or `script:<path>`"
                )),
            },
        }
    }
}

/// What the supervisor does when an agent's process dies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    /// Leave it dead and report it. The default: the examples write
    /// `on-crash` where they mean it, so restarting silently would be a
    /// surprise nobody asked for.
    #[default]
    Never,
    /// Restart it, up to the attempt limit the supervisor owns (three tries).
    OnCrash,
}

/// One agent as the file defines it, with credentials still as references.
///
/// Validation stops at [`Self::resolve`]: the shape of an agent — its id, its
/// transport's required fields, a command that is not empty — does not depend on
/// whether a keyring can be opened. That split is what lets a store list presets
/// without a profile, and what lets an extension's `agents` contribution be read
/// by the same code as the profile's own `agents.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    /// Stable identifier, used by `--agent` and in the API.
    pub id: String,
    /// Display name. Defaults to the id.
    pub name: String,
    /// One sentence on what the agent is for, when the manifest says.
    pub description: Option<String>,
    /// How the daemon drives it, with that transport's settings.
    pub transport: TransportDefinition,
    /// What to do when the process dies.
    pub restart: Restart,
    /// Environment for the child, values as written rather than resolved.
    pub env: BTreeMap<String, EnvValue>,
    /// Whether this agent may name another profile's keyring entries.
    pub allow_cross_profile: bool,
}

impl AgentDefinition {
    /// The transport as the API spells it.
    pub fn transport_kind(&self) -> AgentTransport {
        self.transport.kind()
    }

    /// The command that will be spawned, when this transport spawns one.
    pub fn command(&self) -> Option<&CommandSpec> {
        self.transport.command()
    }

    /// Fetch every credential this definition names.
    ///
    /// A missing secret is a load failure, not a turn failure: the point of
    /// resolving here is that the agent either starts with everything it asked
    /// for or does not start at all.
    pub fn resolve(&self, resolver: &dyn SecretResolver) -> Result<AgentSpec, AgentError> {
        let transport = match &self.transport {
            TransportDefinition::AcpStdio { command } => Transport::AcpStdio {
                command: command.clone(),
            },
            TransportDefinition::Pty { command, profile } => Transport::Pty {
                command: command.clone(),
                profile: profile.clone(),
            },
            TransportDefinition::HttpJsonRpc { url, auth } => Transport::HttpJsonRpc {
                url: url.clone(),
                auth: match auth {
                    Some(reference) => Some(secret(self, "auth", reference, resolver)?),
                    None => None,
                },
            },
        };

        let mut env = BTreeMap::new();
        for (variable, value) in &self.env {
            let value = match value {
                EnvValue::Literal(text) => Secret::new(text),
                EnvValue::Reference(reference) => secret(self, variable, reference, resolver)?,
            };
            env.insert(variable.clone(), value);
        }

        Ok(AgentSpec {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            transport,
            restart: self.restart,
            env,
            allow_cross_profile: self.allow_cross_profile,
        })
    }
}

/// One environment value as written.
///
/// The two cases are deliberately not a `String`: whether a value is a literal or
/// a reference decides whether it is fetched from the keyring or passed through,
/// and a type that keeps them apart cannot hand a reference to a child process as
/// if it were the secret.
#[derive(Clone, PartialEq, Eq)]
pub enum EnvValue {
    /// Passed to the child verbatim.
    Literal(String),
    /// Fetched from the keyring when the profile loads.
    Reference(SecretRef),
}

impl std::fmt::Debug for EnvValue {
    /// Never prints a literal: a manifest may write a key inline, and a debug
    /// line is the last place it should turn up.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Literal(_) => f.write_str("Literal(<redacted>)"),
            Self::Reference(reference) => write!(f, "Reference({})", reference.reference()),
        }
    }
}

/// A transport as defined, before its credential is fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportDefinition {
    /// Native ACP over stdio.
    AcpStdio {
        /// The program that speaks ACP.
        command: CommandSpec,
    },
    /// A program driven through a PTY and a transcript parser.
    Pty {
        /// The program to drive.
        command: CommandSpec,
        /// How its screen becomes a transcript.
        profile: PtyProfile,
    },
    /// ACP over HTTP/JSON-RPC.
    HttpJsonRpc {
        /// Where the agent answers.
        url: Url,
        /// The credential, when the endpoint needs one.
        auth: Option<SecretRef>,
    },
}

impl TransportDefinition {
    /// The transport as the API spells it.
    pub fn kind(&self) -> AgentTransport {
        match self {
            Self::AcpStdio { .. } => AgentTransport::AcpStdio,
            Self::Pty { .. } => AgentTransport::Pty,
            Self::HttpJsonRpc { .. } => AgentTransport::HttpJsonRpc,
        }
    }

    /// The program this transport spawns, if any.
    pub fn command(&self) -> Option<&CommandSpec> {
        match self {
            Self::AcpStdio { command } | Self::Pty { command, .. } => Some(command),
            Self::HttpJsonRpc { .. } => None,
        }
    }

    /// The transport as the manifest spells it, for diagnostics.
    pub fn name(&self) -> &'static str {
        match self {
            Self::AcpStdio { .. } => "acp-stdio",
            Self::Pty { .. } => "pty",
            Self::HttpJsonRpc { .. } => "http-jsonrpc",
        }
    }
}

/// Parse and validate a manifest, resolving every `keyring:` reference.
///
/// A missing credential fails the load instead of a turn. Nothing is cached: the
/// caller decides what to do with the result, and the registry decides when to
/// call this again.
pub fn parse(
    path: &Path,
    text: &str,
    resolver: &dyn SecretResolver,
) -> Result<Vec<AgentSpec>, AgentError> {
    parse_definitions(&path.display().to_string(), text)?
        .iter()
        .map(|definition| definition.resolve(resolver))
        .collect()
}

/// Read and validate a manifest, resolving nothing.
///
/// This is the reader. The registry and an extension's `agents` contribution both
/// come through here, so a file cannot be one thing to the daemon and another to
/// `acpilot ext why`; the only difference left is who owns the presets and whether
/// a credential may be named at all.
pub fn parse_definitions(path: &str, text: &str) -> Result<Vec<AgentDefinition>, AgentError> {
    let file: AgentsFile = toml::from_str(text).map_err(|error| AgentError::Toml {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(AgentError::SchemaVersion {
            path: path.to_owned(),
            found: u64::from(file.schema_version),
            expected: SCHEMA_VERSION,
        });
    }
    if file.agents.is_empty() {
        return Err(AgentError::NoAgents {
            path: path.to_owned(),
        });
    }

    let mut definitions: Vec<AgentDefinition> = Vec::with_capacity(file.agents.len());
    for entry in file.agents {
        let definition = definition(entry)?;
        if definitions
            .iter()
            .any(|existing| existing.id == definition.id)
        {
            return Err(AgentError::Duplicate { id: definition.id });
        }
        definitions.push(definition);
    }
    Ok(definitions)
}

/// The file as it is written, before any of it is trusted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentsFile {
    // Self-identification, not configuration: TOML has no comment-preserving
    // pointer to a schema other than this key or a `#:schema` first line, and
    // `deny_unknown_fields` would otherwise refuse the one line that makes the
    // file checkable before it is loaded.
    #[serde(default, rename = "$schema")]
    _schema: Option<String>,
    schema_version: u32,
    // Defaulted so that a version problem is reported as a version problem
    // rather than as "missing field `agents`" in a two-line file.
    #[serde(default)]
    agents: Vec<AgentEntry>,
}

/// One `[[agents]]` block as it is written.
///
/// This is the type [`json_schema`] is generated from, which is why every field
/// carries a doc comment: the comment is the `description` an editor shows, and a
/// field described in two places is a field described two different ways.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentEntry {
    /// Stable identifier: lowercase letters, digits, `-`, and `_`, starting with
    /// a letter or digit, at most 64 characters, unique within the profile.
    id: String,
    /// Display name. Defaults to `id`.
    #[serde(default)]
    name: Option<String>,
    /// One sentence on what the agent is for.
    #[serde(default)]
    description: Option<String>,
    /// How the daemon drives it: `acp-stdio`, `pty`, or `http-jsonrpc`.
    transport: TransportName,
    /// `acp-stdio` and `pty`: the executable, resolved on the host the workspace
    /// lives on.
    #[serde(default)]
    command: Option<String>,
    /// `acp-stdio` and `pty`: arguments, passed verbatim.
    #[serde(default)]
    args: Vec<String>,
    /// `acp-stdio` and `pty`: environment for the child. A value is a literal
    /// (`NO_COLOR = "1"`) or a `keyring:<name>` reference, resolved when the
    /// profile loads.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// What the supervisor does when the process dies. Defaults to `never`.
    #[serde(default)]
    restart: Restart,
    /// Required before a `keyring:<profile>/<name>` reference is accepted.
    #[serde(default)]
    allow_cross_profile: bool,
    /// `http-jsonrpc`: where the agent answers; `http` or `https`.
    #[serde(default)]
    url: Option<String>,
    /// `http-jsonrpc`: a `keyring:` reference. A plaintext token is refused.
    #[serde(default)]
    auth: Option<String>,
    /// `pty`: `plain-transcript`, `ansi-transcript`, or `script:<path>`.
    #[serde(default)]
    parser: Option<String>,
    /// `pty`: text that identifies the input row, so chrome above it can be cut
    /// away.
    #[serde(default)]
    prompt_row: Option<String>,
    /// `pty`: the input row back at rest — the primary turn-complete signal.
    #[serde(default)]
    sentinel: Option<String>,
    /// `pty`: what to type to leave cleanly (for example `/exit`); `Ctrl-D` is
    /// not universal.
    #[serde(default)]
    exit_command: Option<String>,
    /// `pty`: gap between the text and the Enter key, in milliseconds. 50 by
    /// default, because one write of `text\r` is a paste rather than a submit in
    /// at least one TUI.
    #[serde(default)]
    submit_delay_ms: Option<u64>,
    /// `pty`: backstop when the sentinel does not fire, in milliseconds. 5000 by
    /// default; `0` is refused.
    #[serde(default)]
    idle_timeout_ms: Option<u64>,
}

/// The `transport` value.
///
/// Kebab-case is the spelling the documents use; the snake_case alias is the one
/// the API returns, and accepting it means a value copied out of `GET
/// /v1/agents` can be pasted into the file. The wire enum itself is untouched:
/// the API has exactly one spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
enum TransportName {
    // Named individually rather than with `rename_all`: serde's kebab-case
    // would spell the third variant `http-json-rpc`, and the documents say
    // `http-jsonrpc`.
    #[serde(rename = "acp-stdio", alias = "acp_stdio")]
    AcpStdio,
    #[serde(rename = "pty")]
    Pty,
    #[serde(rename = "http-jsonrpc", alias = "http_json_rpc")]
    HttpJsonRpc,
}

impl TransportName {
    /// Every transport, in the order the documents list them.
    const ALL: [Self; 3] = [Self::AcpStdio, Self::Pty, Self::HttpJsonRpc];

    /// The spelling used in diagnostics, which is the one in the documents.
    fn as_str(self) -> &'static str {
        match self {
            Self::AcpStdio => "acp-stdio",
            Self::Pty => "pty",
            Self::HttpJsonRpc => "http-jsonrpc",
        }
    }

    /// The spelling `GET /v1/agents` returns, which this file accepts too.
    ///
    /// `None` where the two spellings are the same word, so the schema cannot
    /// list a duplicate.
    fn wire_spelling(self) -> Option<&'static str> {
        match self {
            Self::AcpStdio => Some("acp_stdio"),
            Self::Pty => None,
            Self::HttpJsonRpc => Some("http_json_rpc"),
        }
    }

    /// Fields this transport cannot load without.
    ///
    /// The loader states this with its own error messages; the published schema
    /// states it as `required`. A test holds the two together.
    fn required_fields(self) -> &'static [&'static str] {
        match self {
            Self::AcpStdio => &["command"],
            Self::Pty => &["command", "parser", "sentinel"],
            Self::HttpJsonRpc => &["url"],
        }
    }

    /// Fields each transport may carry. Anything outside this list is refused:
    /// a key that silently does nothing is how a manifest stops matching the
    /// behaviour somebody believes it has.
    fn allowed_fields(self) -> &'static [&'static str] {
        match self {
            Self::AcpStdio => &["command", "args", "env"],
            Self::Pty => &[
                "command",
                "args",
                "env",
                "parser",
                "prompt_row",
                "sentinel",
                "exit_command",
                "submit_delay_ms",
                "idle_timeout_ms",
            ],
            Self::HttpJsonRpc => &["url", "auth"],
        }
    }
}

/// The shape of an `id`, as a pattern.
///
/// [`validate_id`] is the authority — it produces the error a person reads — and
/// this is the same rule in the one form an editor can check against. The schema
/// tests feed the same ids to both.
const ID_PATTERN: &str = "^[a-z0-9][a-z0-9_-]{0,63}$";

/// A `script:` parser, as far as a pattern can see it.
///
/// Deliberately permissive: it refuses an empty path but accepts `script: `,
/// which [`Parser::parse`] then refuses for the whitespace. A schema that
/// rejected a file the loader accepts would be worse than one that misses it.
const SCRIPT_PARSER_PATTERN: &str = "^script:.+$";

/// Fields every agent may carry, whatever its transport.
const SHARED_FIELDS: [&str; 6] = [
    "id",
    "name",
    "description",
    "transport",
    "restart",
    "allow_cross_profile",
];

/// [`json_schema`] as the committed file has it: pretty JSON, one trailing
/// newline.
///
/// One renderer for the generator and the drift test, so a difference between
/// them can only ever be a difference in what the schema says.
#[must_use]
pub fn schema_json_text() -> String {
    let mut text = serde_json::to_string_pretty(&json_schema())
        .expect("the generated schema is always serialisable");
    text.push('\n');
    text
}

/// The published JSON Schema for `agents.toml`.
///
/// `transport` is an internally tagged enum, so the schema is a `oneOf` of three
/// objects: each fixes `transport` to one spelling and names exactly the fields
/// that transport may carry. That is the rule [`TransportName::allowed_fields`]
/// enforces at load, taken from the same table, so an editor cannot offer
/// `sentinel` on an ACP agent.
///
/// Generated rather than stored: the drift test regenerates this and compares it
/// with the committed `site/public/schema/agents-v1.json`, so a renamed field
/// shows up as a diff in review instead of a bug report from an editor.
///
/// Two rules stay the loader's alone, because JSON Schema cannot say them: a
/// duplicate `id` (there is no keyword for uniqueness by a property) and a
/// `keyring:` reference that does not resolve.
#[must_use]
pub fn json_schema() -> Value {
    let generated = serde_json::to_value(schemars::schema_for!(AgentEntry))
        .expect("a generated schema is always serialisable");
    let mut defs = generated
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // `schema_for!` inlines the type it was asked about and nests only what that
    // type refers to, so the entry is normally the document itself.
    let entry = defs.remove("AgentEntry").unwrap_or(generated);
    let mut properties = entry
        .get("properties")
        .and_then(Value::as_object)
        .expect("the entry is an object with properties")
        .clone();
    // The `transport` property is the only reference to this definition, and it
    // is replaced below with the spelling that branch accepts.
    defs.remove("TransportName");

    // Three rules neither a doc comment nor a derive can carry: the shape of an
    // `id`, the spellings of `parser`, and that `url` is a URL. Each is written
    // beside the code that enforces it, and the drift test checks the loader and
    // the schema on the same values.
    if let Some(Value::Object(id)) = properties.get_mut("id") {
        id.insert("pattern".to_owned(), json!(ID_PATTERN));
    }
    if properties.contains_key("parser") {
        properties.insert(
            "parser".to_owned(),
            json!({
                "description": "`pty`: `plain-transcript`, `ansi-transcript`, or `script:<path>`.",
                "oneOf": [
                    { "type": "string", "const": "plain-transcript" },
                    { "type": "string", "const": "ansi-transcript" },
                    { "type": "string", "pattern": SCRIPT_PARSER_PATTERN },
                ],
            }),
        );
    }
    if let Some(Value::Object(url)) = properties.get_mut("url") {
        url.insert("format".to_owned(), json!("uri"));
    }

    let agents: Vec<Value> = TransportName::ALL
        .into_iter()
        .map(|transport| {
            let mut branch = Map::new();
            for (name, schema) in &properties {
                if SHARED_FIELDS.contains(&name.as_str())
                    || transport.allowed_fields().contains(&name.as_str())
                {
                    branch.insert(name.clone(), schema.clone());
                }
            }

            let mut spellings = vec![transport.as_str()];
            spellings.extend(transport.wire_spelling());
            let mut tag = properties
                .get("transport")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "string" }));
            if let Value::Object(tag) = &mut tag {
                // The doc comment describes all three; this branch describes one.
                tag.remove("allOf");
                tag.remove("$ref");
                tag.insert("type".to_owned(), json!("string"));
                tag.insert("enum".to_owned(), json!(spellings));
            }
            branch.insert("transport".to_owned(), tag);

            let mut required: Vec<&str> = vec!["id", "transport"];
            required.extend_from_slice(transport.required_fields());

            json!({
                "title": format!("{} agent", transport.as_str()),
                "type": "object",
                "additionalProperties": false,
                "required": required,
                "properties": branch,
            })
        })
        .collect();

    let mut root = Map::new();
    root.insert(
        "$schema".to_owned(),
        json!("https://json-schema.org/draft/2020-12/schema"),
    );
    root.insert("$id".to_owned(), json!(SCHEMA_URL));
    root.insert("title".to_owned(), json!("Acpilot agent manifest"));
    root.insert(
        "description".to_owned(),
        json!(
            "`agents.toml`, the file that tells Acpilot what an agent is and how to start \
             it. One `[[agents]]` block per agent; `transport` decides which fields that \
             block may carry. The file is TOML — this schema describes it, it does not \
             replace it — and an editor reads it from the `$schema` key or a `#:schema` \
             first line."
        ),
    );
    root.insert(
        "$comment".to_owned(),
        json!(format!(
            "Apache-2.0. Generated from crates/acpilot-agent/src/manifest.rs by \
             `cargo xtask schema`; edit the Rust, not this file. This is the v1 document \
             and is frozen: a change a `schema_version = {SCHEMA_VERSION}` loader would \
             read differently ships as agents-v2.json."
        )),
    );
    root.insert("type".to_owned(), json!("object"));
    root.insert("additionalProperties".to_owned(), json!(false));
    root.insert("required".to_owned(), json!(["schema_version", "agents"]));
    root.insert(
        "properties".to_owned(),
        json!({
            "$schema": {
                "description": "This document, so an editor can complete and check the file. Ignored by the loader.",
                "type": "string",
                "format": "uri",
            },
            "schema_version": {
                "description": format!(
                    "Manifest schema major version; `{SCHEMA_VERSION}` is the only one this build speaks."
                ),
                "type": "integer",
                "const": SCHEMA_VERSION,
            },
            "agents": {
                "description": "Every agent in the profile. May not be empty: delete the file instead of emptying it.",
                "type": "array",
                "minItems": 1,
                "items": { "oneOf": agents },
            },
        }),
    );
    root.insert("$defs".to_owned(), Value::Object(defs));

    let mut root = Value::Object(root);
    for_toml(&mut root);
    root
}

/// Make the generated types say only what a TOML file can say.
///
/// - `Option<T>` generates `"type": ["string", "null"]`. TOML has no null, so
///   publishing it would describe a file nobody can write and offer an editor a
///   completion that does not parse. "Absent" is the only way to say "not set".
/// - `u64` generates `"format": "uint64"`, which is a Rust type name rather
///   than a keyword from the standard. `minimum: 0` already carries the fact
///   that matters; the annotation would only be noise in an editor's tooltip.
fn for_toml(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|kind| kind != "null");
            }
            if let Some(Value::Array(members)) = map.get_mut("anyOf") {
                members.retain(|member| member.get("type") != Some(&Value::from("null")));
            }
            if map.get("default") == Some(&Value::Null) {
                map.remove("default");
            }
            let rust_integer = map
                .get("format")
                .and_then(Value::as_str)
                .is_some_and(|format| format.starts_with("uint") || format.starts_with("int"));
            if rust_integer {
                map.remove("format");
            }
            // A one-item `type` array is noise; JSON Schema accepts the string.
            let single = match map.get("type") {
                Some(Value::Array(types)) if types.len() == 1 => types.first().cloned(),
                _ => None,
            };
            if let Some(single) = single {
                map.insert("type".to_owned(), single);
            }
            for child in map.values_mut() {
                for_toml(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                for_toml(item);
            }
        }
        _ => {}
    }
}

/// Turn one entry into a definition, or say which field is wrong.
fn definition(entry: AgentEntry) -> Result<AgentDefinition, AgentError> {
    validate_id(&entry.id)?;

    // Required fields first: a manifest that cannot run at all is the more
    // urgent fact than a field that would be ignored.
    let transport = match entry.transport {
        TransportName::AcpStdio => TransportDefinition::AcpStdio {
            command: require_command(&entry)?,
        },
        TransportName::Pty => TransportDefinition::Pty {
            command: require_command(&entry)?,
            profile: require_pty_profile(&entry)?,
        },
        TransportName::HttpJsonRpc => TransportDefinition::HttpJsonRpc {
            url: require_url(&entry)?,
            auth: require_auth(&entry)?,
        },
    };
    refuse_unexpected_fields(&entry)?;

    let env = env_values(&entry)?;

    let name = match entry.name.as_deref() {
        Some(name) if name.trim().is_empty() => {
            return Err(AgentError::EmptyField {
                id: entry.id,
                field: "name",
            });
        }
        Some(name) => name.to_owned(),
        None => entry.id.clone(),
    };

    Ok(AgentDefinition {
        id: entry.id,
        name,
        description: entry.description,
        transport,
        restart: entry.restart,
        env,
        allow_cross_profile: entry.allow_cross_profile,
    })
}

fn require_command(entry: &AgentEntry) -> Result<CommandSpec, AgentError> {
    let program = entry.command.as_deref().ok_or(AgentError::MissingField {
        id: entry.id.clone(),
        transport: entry.transport.as_str(),
        field: "command",
    })?;
    if program.trim().is_empty() {
        return Err(AgentError::EmptyField {
            id: entry.id.clone(),
            field: "command",
        });
    }
    if program.contains('\0') {
        return Err(AgentError::InvalidField {
            id: entry.id.clone(),
            field: "command",
            why: "may not contain a NUL byte".to_owned(),
        });
    }
    if entry.args.iter().any(|arg| arg.trim().is_empty()) {
        return Err(AgentError::EmptyField {
            id: entry.id.clone(),
            field: "args",
        });
    }
    if entry.args.iter().any(|arg| arg.contains('\0')) {
        return Err(AgentError::InvalidField {
            id: entry.id.clone(),
            field: "args",
            why: "may not contain a NUL byte".to_owned(),
        });
    }
    Ok(CommandSpec {
        program: program.to_owned(),
        args: entry.args.clone(),
    })
}

fn require_pty_profile(entry: &AgentEntry) -> Result<PtyProfile, AgentError> {
    let raw_parser = entry.parser.as_deref().ok_or(AgentError::MissingField {
        id: entry.id.clone(),
        transport: entry.transport.as_str(),
        field: "parser",
    })?;
    let parser = Parser::parse(raw_parser).map_err(|why| AgentError::InvalidField {
        id: entry.id.clone(),
        field: "parser",
        why,
    })?;

    let sentinel = entry
        .sentinel
        .as_deref()
        .ok_or_else(|| AgentError::MissingField {
            id: entry.id.clone(),
            transport: entry.transport.as_str(),
            field: "sentinel",
        })?;
    if sentinel.trim().is_empty() {
        return Err(AgentError::EmptyField {
            id: entry.id.clone(),
            field: "sentinel",
        });
    }
    let sentinel = sentinel.to_owned();

    let prompt_row = optional_text(entry, "prompt_row", entry.prompt_row.as_deref())?;
    let exit_command = optional_text(entry, "exit_command", entry.exit_command.as_deref())?;

    let submit_delay = entry
        .submit_delay_ms
        .map_or(DEFAULT_SUBMIT_DELAY, Duration::from_millis);
    let idle_timeout = match entry.idle_timeout_ms {
        // Zero would mean "never give up", which is the one setting the design phase's task P0-2 showed
        // truncates turns. Leaving the key out is how you ask for the default.
        Some(0) => {
            return Err(AgentError::InvalidField {
                id: entry.id.clone(),
                field: "idle_timeout_ms",
                why: "must be greater than 0; leave it out for the 5 s default".to_owned(),
            });
        }
        Some(ms) => Duration::from_millis(ms),
        None => DEFAULT_IDLE_TIMEOUT,
    };

    Ok(PtyProfile {
        parser,
        prompt_row,
        sentinel,
        exit_command,
        submit_delay,
        idle_timeout,
    })
}

fn optional_text(
    entry: &AgentEntry,
    field: &'static str,
    value: Option<&str>,
) -> Result<Option<String>, AgentError> {
    match value {
        Some(text) if text.trim().is_empty() => Err(AgentError::EmptyField {
            id: entry.id.clone(),
            field,
        }),
        Some(text) => Ok(Some(text.to_owned())),
        None => Ok(None),
    }
}

fn require_url(entry: &AgentEntry) -> Result<Url, AgentError> {
    let raw = entry.url.as_deref().ok_or(AgentError::MissingField {
        id: entry.id.clone(),
        transport: entry.transport.as_str(),
        field: "url",
    })?;
    let url = Url::parse(raw).map_err(|error| AgentError::InvalidField {
        id: entry.id.clone(),
        field: "url",
        why: error.to_string(),
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AgentError::InvalidField {
            id: entry.id.clone(),
            field: "url",
            why: format!("scheme {:?} is not http or https", url.scheme()),
        });
    }
    Ok(url)
}

/// Check `auth` as a reference the agent is allowed to name, without fetching it.
fn require_auth(entry: &AgentEntry) -> Result<Option<SecretRef>, AgentError> {
    let Some(raw) = entry.auth.as_deref() else {
        return Ok(None);
    };
    if !SecretRef::is_reference(raw) {
        return Err(AgentError::InvalidField {
            id: entry.id.clone(),
            field: "auth",
            why: "must be a `keyring:` reference; a plaintext secret is not accepted".to_owned(),
        });
    }
    let reference = SecretRef::parse(raw).map_err(|error| AgentError::Secret {
        id: entry.id.clone(),
        variable: "auth".to_owned(),
        message: error.to_string(),
    })?;
    check_cross_profile(entry, &reference, "auth")?;
    Ok(Some(reference))
}

/// Check every environment entry and keep it as written.
///
/// The name and the reference are checked here, at load, even though the value is
/// not fetched until [`AgentDefinition::resolve`]: a manifest that names a
/// variable a child process cannot receive is wrong whether or not the keyring is
/// open.
fn env_values(entry: &AgentEntry) -> Result<BTreeMap<String, EnvValue>, AgentError> {
    let mut values = BTreeMap::new();
    for (variable, raw) in &entry.env {
        if variable.is_empty()
            || variable.contains('=')
            || variable.contains('\0')
            || variable.chars().any(char::is_whitespace)
        {
            return Err(AgentError::InvalidField {
                id: entry.id.clone(),
                field: "env",
                why: format!("{variable:?} is not a valid environment variable name"),
            });
        }

        // A value may be a literal (`NO_COLOR=1`) or a reference. The prefix
        // decides, so a secret cannot arrive as a literal by accident.
        let value = if SecretRef::is_reference(raw) {
            let reference = SecretRef::parse(raw).map_err(|error| AgentError::Secret {
                id: entry.id.clone(),
                variable: variable.clone(),
                message: error.to_string(),
            })?;
            check_cross_profile(entry, &reference, variable)?;
            EnvValue::Reference(reference)
        } else {
            EnvValue::Literal(raw.clone())
        };
        values.insert(variable.clone(), value);
    }
    Ok(values)
}

/// Fetch one reference, naming the agent and the variable it was for.
fn secret(
    definition: &AgentDefinition,
    variable: &str,
    reference: &SecretRef,
    resolver: &dyn SecretResolver,
) -> Result<Secret, AgentError> {
    resolver
        .resolve(reference)
        .map_err(|error| AgentError::Secret {
            id: definition.id.clone(),
            variable: variable.to_owned(),
            message: error.to_string(),
        })
}

fn check_cross_profile(
    entry: &AgentEntry,
    reference: &SecretRef,
    variable: &str,
) -> Result<(), AgentError> {
    if reference.cross_profile() && !entry.allow_cross_profile {
        return Err(AgentError::CrossProfileSecret {
            id: entry.id.clone(),
            variable: variable.to_owned(),
            reference: reference.reference().to_owned(),
        });
    }
    Ok(())
}

fn refuse_unexpected_fields(entry: &AgentEntry) -> Result<(), AgentError> {
    let present = [
        ("command", entry.command.is_some()),
        ("args", !entry.args.is_empty()),
        ("env", !entry.env.is_empty()),
        ("url", entry.url.is_some()),
        ("auth", entry.auth.is_some()),
        ("parser", entry.parser.is_some()),
        ("prompt_row", entry.prompt_row.is_some()),
        ("sentinel", entry.sentinel.is_some()),
        ("exit_command", entry.exit_command.is_some()),
        ("submit_delay_ms", entry.submit_delay_ms.is_some()),
        ("idle_timeout_ms", entry.idle_timeout_ms.is_some()),
    ];
    let allowed = entry.transport.allowed_fields();
    for (field, is_present) in present {
        if is_present && !allowed.contains(&field) {
            return Err(AgentError::UnexpectedField {
                id: entry.id.clone(),
                transport: entry.transport.as_str(),
                field,
            });
        }
    }
    Ok(())
}

/// An agent id: lowercase, starts with a letter or digit, short enough to type.
///
/// Stricter than a display name on purpose — the id is a key in the API, a
/// `--agent` argument, and half of a keyring account.
fn validate_id(id: &str) -> Result<(), AgentError> {
    let why = if id.is_empty() {
        Some("may not be empty")
    } else if id.len() > 64 {
        Some("must be at most 64 characters")
    } else if !id.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        Some("must start with a letter or digit")
    } else if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        Some("is lowercase letters, digits, `-`, and `_`")
    } else {
        None
    };

    match why {
        Some(why) => Err(AgentError::InvalidId {
            what: "agent id",
            id: id.to_owned(),
            why,
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> Result<Vec<AgentSpec>, AgentError> {
        let resolver = crate::secret::MapResolver::new().with("anthropic", "sk-ant-test");
        parse(Path::new("agents.toml"), text, &resolver)
    }

    fn load_one(text: &str) -> AgentSpec {
        load(text).expect("manifest should load").remove(0)
    }

    fn error(text: &str) -> String {
        load(text)
            .expect_err("manifest should be refused")
            .to_string()
    }

    const HEADER: &str = "schema_version = 1\n";

    #[test]
    fn a_new_manifest_points_at_the_published_schema_and_still_parses() {
        // The presets we ship, through the same parser a user's file goes
        // through: the first test of the schema, and the text a new profile is
        // written with. If `acpilot.agents` stops parsing, the build fails
        // rather than a user's first run.
        let text = initial_manifest();
        let first = text.lines().next().expect("at least one line");
        assert_eq!(first, format!("\"$schema\" = \"{SCHEMA_URL}\""));

        let agents = parse(
            Path::new("agents.toml"),
            &text,
            &crate::secret::MapResolver::new(),
        )
        .expect("a new profile's manifest loads");
        assert_eq!(agents.len(), 4, "the four shipped presets");
        assert_eq!(agents[0].id, "claude-code");
    }

    #[test]
    fn the_shipped_presets_parse() {
        let agents = load(SHIPPED_PRESETS).expect("shipped presets");
        assert_eq!(agents.len(), 4);
        assert!(
            agents
                .iter()
                .all(|agent| agent.transport_kind() == AgentTransport::AcpStdio)
        );
        assert_eq!(agents[0].id, "claude-code");
        assert_eq!(agents[0].command().expect("command").program, "npx");
    }

    #[test]
    fn an_acp_stdio_agent_needs_only_a_command() {
        let agent = load_one(&format!(
            "{HEADER}[[agents]]\nid = \"claude\"\ntransport = \"acp-stdio\"\ncommand = \"npx\"\n\
             args = [\"-y\", \"@zed-industries/claude-code-acp\"]\n"
        ));
        assert_eq!(agent.name, "claude", "name falls back to the id");
        assert_eq!(agent.restart, Restart::Never);
        assert_eq!(
            agent.command().expect("command").args,
            ["-y", "@zed-industries/claude-code-acp"]
        );
    }

    #[test]
    fn a_pty_agent_gets_the_measured_defaults() {
        let agent = load_one(&format!(
            "{HEADER}[[agents]]\nid = \"freebuff\"\ntransport = \"pty\"\ncommand = \"freebuff\"\n\
             parser = \"ansi-transcript\"\nsentinel = \"▍Enter a coding task\"\n"
        ));
        let Transport::Pty { profile, .. } = &agent.transport else {
            panic!("expected a pty agent");
        };
        assert_eq!(profile.parser, Parser::AnsiTranscript);
        assert_eq!(profile.submit_delay, Duration::from_millis(50));
        assert_eq!(profile.idle_timeout, Duration::from_millis(5_000));
        assert_eq!(profile.prompt_row, None);
    }

    #[test]
    fn the_api_spelling_is_accepted_too() {
        let agent = load_one(&format!(
            "{HEADER}[[agents]]\nid = \"x\"\ntransport = \"acp_stdio\"\ncommand = \"a\"\n"
        ));
        assert_eq!(agent.transport_kind(), AgentTransport::AcpStdio);
    }

    #[test]
    fn an_http_agent_needs_a_url_and_a_reference_for_auth() {
        let agent = load_one(&format!(
            "{HEADER}[[agents]]\nid = \"remote\"\ntransport = \"http-jsonrpc\"\n\
             url = \"https://devin.internal/acp\"\nauth = \"keyring:anthropic\"\n"
        ));
        let Transport::HttpJsonRpc { url, auth } = &agent.transport else {
            panic!("expected an http agent");
        };
        assert_eq!(url.as_str(), "https://devin.internal/acp");
        assert_eq!(auth.as_ref().expect("auth").expose(), "sk-ant-test");
    }

    #[test]
    fn a_plaintext_credential_is_refused() {
        let message = error(&format!(
            "{HEADER}[[agents]]\nid = \"remote\"\ntransport = \"http-jsonrpc\"\n\
             url = \"https://example.test/acp\"\nauth = \"sk-live-1234\"\n"
        ));
        assert!(message.contains("keyring:"), "{message}");
    }

    #[test]
    fn a_missing_secret_fails_the_load_not_the_turn() {
        let message = error(&format!(
            "{HEADER}[[agents]]\nid = \"claude\"\ntransport = \"acp-stdio\"\ncommand = \"npx\"\n\
             env = {{ ANTHROPIC_API_KEY = \"keyring:nobody\" }}\n"
        ));
        assert!(message.contains("agent \"claude\""), "{message}");
        assert!(message.contains("ANTHROPIC_API_KEY"), "{message}");
        assert!(message.contains("keyring:nobody"), "{message}");
        assert!(!message.contains("sk-"), "{message}");
    }

    #[test]
    fn env_literals_pass_through_and_never_hide_a_secret_in_debug() {
        let agent = load_one(&format!(
            "{HEADER}[[agents]]\nid = \"claude\"\ntransport = \"acp-stdio\"\ncommand = \"npx\"\n\
             env = {{ NO_COLOR = \"1\", ANTHROPIC_API_KEY = \"keyring:anthropic\" }}\n"
        ));
        assert_eq!(agent.env["NO_COLOR"].expose(), "1");
        assert_eq!(agent.env["ANTHROPIC_API_KEY"].expose(), "sk-ant-test");
        let debugged = format!("{agent:?}");
        assert!(!debugged.contains("sk-ant-test"), "{debugged}");
        assert!(debugged.contains("<redacted>"), "{debugged}");
    }

    #[test]
    fn a_cross_profile_reference_needs_the_flag() {
        let without = error(&format!(
            "{HEADER}[[agents]]\nid = \"claude\"\ntransport = \"acp-stdio\"\ncommand = \"npx\"\n\
             env = {{ ANTHROPIC_API_KEY = \"keyring:work/anthropic\" }}\n"
        ));
        assert!(without.contains("allow_cross_profile"), "{without}");

        let with = load(&format!(
            "{HEADER}[[agents]]\nid = \"claude\"\ntransport = \"acp-stdio\"\ncommand = \"npx\"\n\
             allow_cross_profile = true\n\
             env = {{ ANTHROPIC_API_KEY = \"keyring:work/anthropic\" }}\n"
        ));
        let resolver_note = with.expect_err("the test resolver has no cross-profile entry");
        assert!(resolver_note.to_string().contains("agent \"claude\""));
    }

    #[test]
    fn every_required_field_is_named() {
        let cases = [
            ("acp-stdio", "", "command"),
            ("pty", "parser = \"ansi-transcript\"\n", "command"),
            ("pty", "command = \"f\"\nsentinel = \"s\"\n", "parser"),
            (
                "pty",
                "command = \"f\"\nparser = \"ansi-transcript\"\n",
                "sentinel",
            ),
            ("http-jsonrpc", "", "url"),
        ];
        for (transport, extra, field) in cases {
            let message = error(&format!(
                "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"{transport}\"\n{extra}"
            ));
            assert!(
                message.contains(&format!("requires \"{field}\"")),
                "{transport} without {field}: {message}"
            );
        }
    }

    #[test]
    fn a_field_that_means_nothing_for_the_transport_is_refused() {
        let message = error(&format!(
            "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"acp-stdio\"\ncommand = \"f\"\n\
             sentinel = \"s\"\n"
        ));
        assert!(message.contains("\"sentinel\" has no meaning"), "{message}");
        assert!(message.contains("acp-stdio"), "{message}");
    }

    #[test]
    fn an_unknown_key_is_an_error() {
        let message = error(&format!(
            "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"acp-stdio\"\ncommand = \"f\"\n\
             sentinal = \"s\"\n"
        ));
        assert!(message.contains("unknown field"), "{message}");
        assert!(message.contains("sentinal"), "{message}");
    }

    #[test]
    fn a_duplicate_id_is_an_error() {
        let message = error(&format!(
            "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"acp-stdio\"\ncommand = \"f\"\n\
             [[agents]]\nid = \"a\"\ntransport = \"acp-stdio\"\ncommand = \"g\"\n"
        ));
        assert!(message.contains("duplicate agent id \"a\""), "{message}");
    }

    #[test]
    fn ids_are_keys_and_must_look_like_them() {
        for id in ["", "Claude", "-claude", "claude code", "claude/code"] {
            let message = error(&format!(
                "{HEADER}[[agents]]\nid = \"{id}\"\ntransport = \"acp-stdio\"\ncommand = \"f\"\n"
            ));
            assert!(message.contains("agent id"), "{id:?}: {message}");
        }
    }

    #[test]
    fn an_unknown_transport_lists_the_valid_ones() {
        let message = error(&format!(
            "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"stdio\"\ncommand = \"f\"\n"
        ));
        assert!(message.contains("acp-stdio"), "{message}");
        assert!(message.contains("http-jsonrpc"), "{message}");
    }

    #[test]
    fn a_wrong_schema_version_says_which_one_this_build_speaks() {
        let message = error("schema_version = 99\n");
        assert!(message.contains("schema_version 99"), "{message}");
        assert!(message.contains("speaks 1"), "{message}");
    }

    #[test]
    fn an_empty_file_is_refused_rather_than_silently_meaning_no_agents() {
        let message = error("schema_version = 1\nagents = []\n");
        assert!(message.contains("may not be empty"), "{message}");
    }

    #[test]
    fn a_url_must_be_http_and_parse() {
        let not_http = error(&format!(
            "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"http-jsonrpc\"\nurl = \"ftp://x.test\"\n"
        ));
        assert!(not_http.contains("not http or https"), "{not_http}");

        let garbage = error(&format!(
            "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"http-jsonrpc\"\nurl = \"devin\"\n"
        ));
        assert!(garbage.contains("\"url\""), "{garbage}");
    }

    #[test]
    fn a_zero_idle_timeout_is_refused() {
        let message = error(&format!(
            "{HEADER}[[agents]]\nid = \"f\"\ntransport = \"pty\"\ncommand = \"f\"\n\
             parser = \"plain-transcript\"\nsentinel = \"s\"\nidle_timeout_ms = 0\n"
        ));
        assert!(message.contains("greater than 0"), "{message}");
    }

    #[test]
    fn a_script_parser_records_its_path() {
        let agent = load_one(&format!(
            "{HEADER}[[agents]]\nid = \"f\"\ntransport = \"pty\"\ncommand = \"f\"\n\
             parser = \"script:./parsers/freebuff.lua\"\nsentinel = \"s\"\n"
        ));
        let Transport::Pty { profile, .. } = &agent.transport else {
            panic!("expected a pty agent");
        };
        assert_eq!(
            profile.parser,
            Parser::Script(PathBuf::from("./parsers/freebuff.lua"))
        );

        let message = error(&format!(
            "{HEADER}[[agents]]\nid = \"f\"\ntransport = \"pty\"\ncommand = \"f\"\n\
             parser = \"script:\"\nsentinel = \"s\"\n"
        ));
        assert!(message.contains("needs a path"), "{message}");
    }
}
