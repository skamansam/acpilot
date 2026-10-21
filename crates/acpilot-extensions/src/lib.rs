//! Extensions: manifests, theme tokens, presets, and the loader that reads them.
//!
//! An extension is a directory with `extension.toml` and some assets. Installing
//! it puts files on disk; activating it is a separate, per-profile decision
//! (ADR 0003). This crate owns the first half: parse the manifest, refuse
//! anything that reaches outside its own directory, parse the assets with the
//! same rules a user's own configuration follows, and hand back typed data.
//!
//! Loading is a pure function of the directory. No install script, no build step,
//! no network — the riskiest thing a manifest can do is name a program, and that
//! program is trust-pinned before it ever runs.
//!
//! The built-ins in [`builtin`] are embedded with `include_str!` and loaded
//! through [`loader::load_embedded`], which is the same code path as an installed
//! extension. That is deliberate: our own two themes and fourteen presets are the
//! first test of the schema, and they fail at build time rather than in a user's
//! terminal.

pub mod activation;
pub mod builtin;
pub mod contrast;
pub mod docs;
pub mod effective;
pub mod error;
pub mod loader;
pub mod manifest;
pub mod preset;
pub mod registry;
pub mod source;
pub mod store;
pub mod theme;
pub mod theme_import;

pub use acpilot_proto::extension::{ExtensionKind, ThemeAppearance};
pub use activation::{Activation, ProfileExtensions, Resolution};
pub use effective::{EffectiveRegistry, Layer, ProfileLayer, Trace};
pub use error::ExtensionError;
pub use loader::{Contribution, LoadedExtension, load, load_dir, load_embedded};
pub use manifest::{MANIFEST_FILE, Manifest, SCHEMA_VERSION};
pub use preset::{AgentPreset, LanguageServerPreset};
pub use registry::Registry;
pub use source::{AssetSource, DirSource, EmbeddedSource};
pub use store::{ActiveSet, BlockReason, Store, StoreStatus, StoredExtension};
pub use theme::{Color, Palette, ResolvedTheme, SyntaxToken, Theme, UiToken};
