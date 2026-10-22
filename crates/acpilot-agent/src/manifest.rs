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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use acpilot_proto::agent::AgentTransport;
use serde::Deserialize;
use url::Url;

use crate::error::AgentError;
use crate::secret::{Secret, SecretRef, SecretResolver};

/// The file the registry reads, inside a profile directory.
pub const MANIFEST_FILE: &str = "agents.toml";

/// Schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Gap between typing a prompt and pressing Enter in a PTY agent.
///
/// P0-2: sending `text` and Enter in one write silently fails to submit in
/// Freebuff and looks exactly like a completed turn. The value is a starting
/// point, not a measurement — P2-7 owns tuning it per parser.
pub const DEFAULT_SUBMIT_DELAY: Duration = Duration::from_millis(50);

/// Backstop when a PTY agent's sentinel never fires.
///
/// P0-2 measured this as the smallest timeout that truncated no turn: an idle
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
}

/// What a PTY agent needs beyond its command.
///
/// Every field is one of P0-2's findings: a sentinel is the primary
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
    /// all is a policy question for P2-7, which owns the loader.
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    /// Leave it dead and report it. The default: the examples write
    /// `on-crash` where they mean it, so restarting silently would be a
    /// surprise nobody asked for.
    #[default]
    Never,
    /// Restart it, up to the attempt limit the supervisor owns (P2-2).
    OnCrash,
}

/// Parse and validate a manifest.
///
/// Every `keyring:` reference is resolved here, so a missing credential fails
/// the load instead of a turn. Nothing is cached: the caller decides what to do
/// with the result, and the registry decides when to call this again.
pub fn parse(
    path: &Path,
    text: &str,
    resolver: &dyn SecretResolver,
) -> Result<Vec<AgentSpec>, AgentError> {
    let file: AgentsFile = toml::from_str(text).map_err(|error| AgentError::Toml {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(AgentError::SchemaVersion {
            path: path.display().to_string(),
            found: u64::from(file.schema_version),
            expected: SCHEMA_VERSION,
        });
    }
    if file.agents.is_empty() {
        return Err(AgentError::NoAgents {
            path: path.display().to_string(),
        });
    }

    let mut specs: Vec<AgentSpec> = Vec::with_capacity(file.agents.len());
    for entry in file.agents {
        let spec = build(entry, resolver)?;
        if specs.iter().any(|existing| existing.id == spec.id) {
            return Err(AgentError::Duplicate { id: spec.id });
        }
        specs.push(spec);
    }
    Ok(specs)
}

/// The file as it is written, before any of it is trusted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentsFile {
    schema_version: u32,
    // Defaulted so that a version problem is reported as a version problem
    // rather than as "missing field `agents`" in a two-line file.
    #[serde(default)]
    agents: Vec<AgentEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    transport: TransportName,
    // acp-stdio and pty
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    restart: Restart,
    #[serde(default)]
    allow_cross_profile: bool,
    // http-jsonrpc
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    auth: Option<String>,
    // pty
    #[serde(default)]
    parser: Option<String>,
    #[serde(default)]
    prompt_row: Option<String>,
    #[serde(default)]
    sentinel: Option<String>,
    #[serde(default)]
    exit_command: Option<String>,
    #[serde(default)]
    submit_delay_ms: Option<u64>,
    #[serde(default)]
    idle_timeout_ms: Option<u64>,
}

/// The `transport` value.
///
/// Kebab-case is the spelling the documents use; the snake_case alias is the one
/// the API returns, and accepting it means a value copied out of `GET
/// /v1/agents` can be pasted into the file. The wire enum itself is untouched:
/// the API has exactly one spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
    /// The spelling used in diagnostics, which is the one in the documents.
    fn as_str(self) -> &'static str {
        match self {
            Self::AcpStdio => "acp-stdio",
            Self::Pty => "pty",
            Self::HttpJsonRpc => "http-jsonrpc",
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

/// Turn one entry into a spec, or say which field is wrong.
fn build(entry: AgentEntry, resolver: &dyn SecretResolver) -> Result<AgentSpec, AgentError> {
    validate_id(&entry.id)?;

    // Required fields first: a manifest that cannot run at all is the more
    // urgent fact than a field that would be ignored.
    let transport = match entry.transport {
        TransportName::AcpStdio => Transport::AcpStdio {
            command: require_command(&entry)?,
        },
        TransportName::Pty => Transport::Pty {
            command: require_command(&entry)?,
            profile: require_pty_profile(&entry)?,
        },
        TransportName::HttpJsonRpc => {
            let url = require_url(&entry)?;
            let auth = resolve_auth(&entry, resolver)?;
            Transport::HttpJsonRpc { url, auth }
        }
    };
    refuse_unexpected_fields(&entry)?;

    let env = resolve_env(&entry, resolver)?;

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

    Ok(AgentSpec {
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
        // Zero would mean "never give up", which is the one setting P0-2 showed
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

fn resolve_auth(
    entry: &AgentEntry,
    resolver: &dyn SecretResolver,
) -> Result<Option<Secret>, AgentError> {
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
    resolver
        .resolve(&reference)
        .map(Some)
        .map_err(|error| AgentError::Secret {
            id: entry.id.clone(),
            variable: "auth".to_owned(),
            message: error.to_string(),
        })
}

fn resolve_env(
    entry: &AgentEntry,
    resolver: &dyn SecretResolver,
) -> Result<BTreeMap<String, Secret>, AgentError> {
    let mut resolved = BTreeMap::new();
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
            resolver
                .resolve(&reference)
                .map_err(|error| AgentError::Secret {
                    id: entry.id.clone(),
                    variable: variable.clone(),
                    message: error.to_string(),
                })?
        } else {
            Secret::new(raw)
        };
        resolved.insert(variable.clone(), value);
    }
    Ok(resolved)
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

    /// The presets we ship, through the same parser a user's file goes through.
    ///
    /// This is the first test of the schema: if `acpilot.agents` stops parsing,
    /// the build fails rather than a user's first run.
    const SHIPPED_PRESETS: &str = include_str!("../../../extensions/acpilot.agents/agents.toml");

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
