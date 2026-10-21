//! The extensions that ship inside the binary.
//!
//! These are ordinary extension directories under `extensions/` at the repository
//! root, embedded with `include_str!`. They are loaded through the same loader as
//! anything installed, so a schema mistake fails on our own files first — which
//! is the cheapest time to find out.
//!
//! `include_str!` also means a manifest that points at a missing asset is a build
//! error, not a runtime one.

use crate::error::ExtensionError;
use crate::loader::{LoadedExtension, load_embedded};

/// `night-flight`: the default dark theme.
pub const NIGHT_FLIGHT: &[(&str, &str)] = &[
    (
        "extension.toml",
        include_str!("../../../extensions/acpilot.night-flight/extension.toml"),
    ),
    (
        "night-flight.toml",
        include_str!("../../../extensions/acpilot.night-flight/night-flight.toml"),
    ),
];

/// `paper`: the light theme.
pub const PAPER: &[(&str, &str)] = &[
    (
        "extension.toml",
        include_str!("../../../extensions/acpilot.paper/extension.toml"),
    ),
    (
        "paper.toml",
        include_str!("../../../extensions/acpilot.paper/paper.toml"),
    ),
];

/// The ten language-server presets.
pub const LANGUAGE_SERVERS: &[(&str, &str)] = &[
    (
        "extension.toml",
        include_str!("../../../extensions/acpilot.language-servers/extension.toml"),
    ),
    (
        "lsp.toml",
        include_str!("../../../extensions/acpilot.language-servers/lsp.toml"),
    ),
];

/// The four agent presets.
pub const AGENTS: &[(&str, &str)] = &[
    (
        "extension.toml",
        include_str!("../../../extensions/acpilot.agents/extension.toml"),
    ),
    (
        "agents.toml",
        include_str!("../../../extensions/acpilot.agents/agents.toml"),
    ),
];

/// Every built-in, in load order.
pub const ALL: [&[(&str, &str)]; 4] = [NIGHT_FLIGHT, PAPER, LANGUAGE_SERVERS, AGENTS];

/// Load every built-in through the public loader.
///
/// Errors are collected so a schema mistake in one built-in names all of them
/// rather than only the first.
pub fn load_all() -> Result<Vec<LoadedExtension>, Vec<ExtensionError>> {
    let mut loaded = Vec::with_capacity(ALL.len());
    let mut errors = Vec::new();
    for files in ALL {
        match load_embedded(files) {
            Ok(extension) => loaded.push(extension),
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Ok(loaded)
    } else {
        Err(errors)
    }
}
