//! Language-server and agent presets, as data.
//!
//! A preset is a command and the metadata needed to launch it. It grants
//! nothing: the command is trust-pinned on first run exactly like a hand-written
//! entry, so an extension cannot widen what the broker allows.
//!
//! Preset ids are bare (`rust-analyzer`, not `acpilot.rust-analyzer`) because
//! they name programs people already know. Collisions are an error, never an
//! override — two extensions shipping `rust-analyzer` is a bug in one of them.
//!
//! There is one `agents.toml` reader, and it lives in `acpilot-agent`; this module
//! adds provenance and one rule of its own. A *profile's* file may name a
//! credential, because the person who wrote it owns the keyring it reads. An
//! extension's file may not: a pack that could name `keyring:work/token` would be
//! able to have another profile's secret handed to a process it also chose.

use acpilot_agent::CommandSpec;
use acpilot_agent::manifest::{AgentDefinition, TransportDefinition, parse_definitions};
use acpilot_proto::agent::AgentTransport;
use serde::Deserialize;

use crate::error::ExtensionError;
use crate::manifest::{SCHEMA_VERSION, validate_segment};

/// Owner recorded on presets the profile itself provides.
///
/// The one check in [`parse_agents`] that depends on the owner: everything else
/// about a preset file is the same whether an extension or a person wrote it.
pub const PROFILE_OWNER: &str = "profile";

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
///
/// The definition is what the one manifest reader produced, not a narrower copy
/// of the same fields: a profile's `agents.toml` may carry a PTY parser or an
/// HTTP endpoint, and `ext why` must be able to say so rather than refusing a file
/// the daemon loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPreset {
    /// The entry, validated by the reader that reads every `agents.toml`.
    pub definition: AgentDefinition,
    /// Extension that provided it, or [`PROFILE_OWNER`].
    pub extension_id: String,
}

impl AgentPreset {
    /// Registry id, for example `claude-code`.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.definition.id
    }

    /// Display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.definition.name
    }

    /// How the daemon drives it.
    #[must_use]
    pub fn transport(&self) -> AgentTransport {
        self.definition.transport_kind()
    }

    /// The program to spawn, when this transport spawns one.
    #[must_use]
    pub fn command(&self) -> Option<&CommandSpec> {
        self.definition.command()
    }
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
///
/// The file is the agent manifest, read by `acpilot-agent`, so the two readers
/// cannot drift: a field added there is a field accepted here. `extension_id` is
/// [`PROFILE_OWNER`] for a profile's own file, which is the only case that may name
/// a credential.
///
/// Duplicate ids are refused by the reader, before provenance is attached; the
/// owner in that message is the file, which is what somebody editing a pack needs
/// to see.
pub fn parse_agents(
    path: &str,
    text: &str,
    extension_id: &str,
) -> Result<Vec<AgentPreset>, ExtensionError> {
    let definitions =
        parse_definitions(path, text).map_err(|error| ExtensionError::AgentManifest {
            message: error.to_string(),
        })?;

    let from_profile = extension_id == PROFILE_OWNER;
    let mut presets: Vec<AgentPreset> = Vec::with_capacity(definitions.len());
    for definition in definitions {
        if !from_profile {
            let field = if !definition.env.is_empty() {
                Some("env")
            } else if matches!(
                definition.transport,
                TransportDefinition::HttpJsonRpc { auth: Some(_), .. }
            ) {
                Some("auth")
            } else if definition.allow_cross_profile {
                Some("allow_cross_profile")
            } else {
                None
            };
            if let Some(field) = field {
                return Err(ExtensionError::AgentCredential {
                    path: path.to_owned(),
                    agent: definition.id,
                    field,
                });
            }
        }
        presets.push(AgentPreset {
            definition,
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
