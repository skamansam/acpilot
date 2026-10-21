//! Where an extension's files come from.
//!
//! The loader never opens a path itself: it asks an [`AssetSource`]. That is what
//! lets the four built-ins — compiled into the binary — go through exactly the
//! same validation as a directory somebody installed, and it is the seam a
//! remote host uses later.
//!
//! A directory source resolves every path through `canonicalize` and re-checks
//! the prefix afterwards, so a symlink inside the tree cannot point out of it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::ExtensionError;
use crate::manifest::validate_contribution_path;

/// A read-only view of the files inside one extension directory.
pub trait AssetSource {
    /// A label for errors, usually the directory or the extension id.
    fn label(&self) -> String;

    /// Read a path relative to the extension root.
    fn read(&self, relative: &str) -> Result<Vec<u8>, ExtensionError>;

    /// Whether the path exists and is readable.
    fn exists(&self, relative: &str) -> Result<bool, ExtensionError>;

    /// Read a path as UTF-8.
    fn read_text(&self, relative: &str) -> Result<String, ExtensionError> {
        let bytes = self.read(relative)?;
        String::from_utf8(bytes).map_err(|error| ExtensionError::Io {
            path: format!("{}:{}", self.label(), relative),
            source: io::Error::new(io::ErrorKind::InvalidData, error),
        })
    }
}

/// An extension directory on this machine.
#[derive(Debug, Clone)]
pub struct DirSource {
    root: PathBuf,
}

impl DirSource {
    /// Open a directory as an extension root.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, ExtensionError> {
        let given = root.as_ref();
        let root = fs::canonicalize(given).map_err(|source| ExtensionError::Io {
            path: given.display().to_string(),
            source,
        })?;
        if !root.is_dir() {
            return Err(ExtensionError::Io {
                path: root.display().to_string(),
                source: io::Error::new(io::ErrorKind::NotADirectory, "not a directory"),
            });
        }
        Ok(Self { root })
    }

    /// The canonical root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, relative: &str) -> Result<PathBuf, ExtensionError> {
        validate_contribution_path(relative)?;
        let joined = self.root.join(relative);
        // Resolve symlinks before the prefix test: a link planted inside the
        // tree is exactly how a path check gets bypassed.
        let canonical = fs::canonicalize(&joined).map_err(|source| ExtensionError::Io {
            path: format!("{}/{}", self.root.display(), relative),
            source,
        })?;
        if !canonical.starts_with(&self.root) {
            return Err(ExtensionError::UnsafePath {
                path: relative.to_owned(),
                why: "resolves outside the extension directory",
            });
        }
        Ok(canonical)
    }
}

impl AssetSource for DirSource {
    fn label(&self) -> String {
        self.root.display().to_string()
    }

    fn read(&self, relative: &str) -> Result<Vec<u8>, ExtensionError> {
        let path = self.resolve(relative)?;
        fs::read(&path).map_err(|source| ExtensionError::Io {
            path: path.display().to_string(),
            source,
        })
    }

    fn exists(&self, relative: &str) -> Result<bool, ExtensionError> {
        match self.resolve(relative) {
            // Not `is_file`: a grammar contribution points at a directory, and
            // the asset is the whole tree.
            Ok(path) => Ok(path.exists()),
            Err(ExtensionError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Ok(false)
            }
            Err(other) => Err(other),
        }
    }
}

/// Files compiled into the binary.
///
/// Only whole files: the embedded source has no directories, so a built-in that
/// points at a missing asset fails at build time via `include_str!` and at load
/// time via the same `exists` check as anything else.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedSource {
    files: &'static [(&'static str, &'static str)],
}

impl EmbeddedSource {
    /// Wrap a table of `(path, contents)` pairs.
    #[must_use]
    pub const fn new(files: &'static [(&'static str, &'static str)]) -> Self {
        Self { files }
    }
}

impl AssetSource for EmbeddedSource {
    fn label(&self) -> String {
        match self.files.first() {
            Some((path, _)) => format!("built-in:{path}"),
            None => "built-in:empty".to_owned(),
        }
    }

    fn read(&self, relative: &str) -> Result<Vec<u8>, ExtensionError> {
        self.files
            .iter()
            .find(|(path, _)| *path == relative)
            .map(|(_, contents)| contents.as_bytes().to_vec())
            .ok_or_else(|| ExtensionError::Io {
                path: self.label(),
                source: io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{relative} is not part of this embedded extension"),
                ),
            })
    }

    fn exists(&self, relative: &str) -> Result<bool, ExtensionError> {
        Ok(self.files.iter().any(|(path, _)| *path == relative))
    }
}
