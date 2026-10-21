//! Language-server and agent presets, as data.
//!
//! A preset is a command and the metadata needed to launch it. It grants
//! nothing: the command is trust-pinned on first run exactly like a hand-written
//! entry, so an extension cannot widen what the broker allows.
//!
//! Preset ids are bare (`rust-analyzer`, not `acpilot.rust-analyzer`) because
//! they name programs people already know. Collisions are an error, never an
//! override — two extensions shipping `rust-analyzer` is a bug in one of them.

use acpilot_proto::agent::AgentTransport;
use serde::Deserialize;

use crate::error::ExtensionError;
use crate::manifest::{SCHEMA_VERSION, validate_segment};

/// A language-server preset, with the extension that introduced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageServerPreset {
    /// Registry id, for example `rust-analyzer`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Program to spawn.
    pub command: String,
    /// Arguments, in order.
    pub args: Vec<String>,
    /// Language ids this server handles.
    pub languages: Vec<String>,
    /// Files that mark a workspace root.
    pub root_markers: Vec<String>,
    /// Extension that provided it.
    pub extension_id: String,
}

/// An agent preset, with the extension that introduced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPreset {
    /// Registry id, for example `claude-code`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// How the daemon drives it.
    pub transport: AgentTransport,
    /// Program to spawn.
    pub command: String,
    /// Arguments, in order.
    pub args: Vec<String>,
    /// One sentence on what it is for.
    pub description: Option<String>,
    /// Extension that provided it.
    pub extension_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServersFile {
    schema_version: u32,
    servers: Vec<ServerEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerEntry {
    id: String,
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    root_markers: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentsFile {
    schema_version: u32,
    agents: Vec<AgentEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentEntry {
    id: String,
    name: String,
    transport: AgentTransport,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    description: Option<String>,
}

/// Parse an `lsp.toml` fragment.
pub fn parse_language_servers(
    path: &str,
    text: &str,
    extension_id: &str,
) -> Result<Vec<LanguageServerPreset>, ExtensionError> {
    let file: ServersFile = toml::from_str(text).map_err(|error| ExtensionError::Toml {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(ExtensionError::SchemaVersion {
            path: path.to_owned(),
            found: u64::from(file.schema_version),
            expected: SCHEMA_VERSION,
        });
    }
    if file.servers.is_empty() {
        return Err(ExtensionError::EmptyField {
            path: path.to_owned(),
            field: "servers",
        });
    }

    let mut presets: Vec<LanguageServerPreset> = Vec::with_capacity(file.servers.len());
    for server in file.servers {
        validate_preset("server id", &server.id)?;
        validate_command(path, &server.command)?;
        for (field, values) in [
            ("args", &server.args),
            ("languages", &server.languages),
            ("root_markers", &server.root_markers),
        ] {
            if values.iter().any(|value| value.trim().is_empty()) {
                return Err(ExtensionError::EmptyField {
                    path: path.to_owned(),
                    field: match field {
                        "args" => "args",
                        "languages" => "languages",
                        _ => "root_markers",
                    },
                });
            }
        }
        if presets.iter().any(|existing| existing.id == server.id) {
            return Err(ExtensionError::Duplicate {
                what: "language-server preset",
                id: server.id,
                owner: extension_id.to_owned(),
            });
        }
        presets.push(LanguageServerPreset {
            id: server.id,
            name: server.name,
            command: server.command,
            args: server.args,
            languages: server.languages,
            root_markers: server.root_markers,
            extension_id: extension_id.to_owned(),
        });
    }
    Ok(presets)
}

/// Parse an `agents.toml` fragment.
pub fn parse_agents(
    path: &str,
    text: &str,
    extension_id: &str,
) -> Result<Vec<AgentPreset>, ExtensionError> {
    let file: AgentsFile = toml::from_str(text).map_err(|error| ExtensionError::Toml {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(ExtensionError::SchemaVersion {
            path: path.to_owned(),
            found: u64::from(file.schema_version),
            expected: SCHEMA_VERSION,
        });
    }
    if file.agents.is_empty() {
        return Err(ExtensionError::EmptyField {
            path: path.to_owned(),
            field: "agents",
        });
    }

    let mut presets: Vec<AgentPreset> = Vec::with_capacity(file.agents.len());
    for agent in file.agents {
        validate_preset("agent id", &agent.id)?;
        validate_command(path, &agent.command)?;
        if agent.args.iter().any(|arg| arg.trim().is_empty()) {
            return Err(ExtensionError::EmptyField {
                path: path.to_owned(),
                field: "args",
            });
        }
        if presets.iter().any(|existing| existing.id == agent.id) {
            return Err(ExtensionError::Duplicate {
                what: "agent preset",
                id: agent.id,
                owner: extension_id.to_owned(),
            });
        }
        presets.push(AgentPreset {
            id: agent.id,
            name: agent.name,
            transport: agent.transport,
            command: agent.command,
            args: agent.args,
            description: agent.description,
            extension_id: extension_id.to_owned(),
        });
    }
    Ok(presets)
}

fn validate_preset(what: &'static str, id: &str) -> Result<(), ExtensionError> {
    validate_segment(what, id)?;
    if id.len() > 64 {
        return Err(ExtensionError::InvalidId {
            what,
            id: id.to_owned(),
            why: "must be at most 64 characters",
        });
    }
    Ok(())
}

fn validate_command(path: &str, command: &str) -> Result<(), ExtensionError> {
    if command.trim().is_empty() {
        return Err(ExtensionError::EmptyField {
            path: path.to_owned(),
            field: "command",
        });
    }
    if command.contains('\0') {
        return Err(ExtensionError::InvalidId {
            what: "command",
            id: command.to_owned(),
            why: "may not contain a NUL byte",
        });
    }
    Ok(())
}
