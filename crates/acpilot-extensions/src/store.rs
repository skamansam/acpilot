//! The content-addressed extension store.
//!
//! Layout: `<root>/<publisher.id>/<version>/`, with a sibling
//! `<publisher.id>/<version>.sha256` record written once the directory is in
//! place.
//!
//! Install is: copy into a staging directory, load the copy (so the bytes that
//! get hashed are the bytes that were validated), hash it, then `rename` into
//! place. A reader never sees a half-written extension, and an interrupted
//! install leaves only a dot-directory that every scan ignores.
//!
//! Verification is the part that matters. The checksum covers a canonical byte
//! stream — a version tag, then each file sorted by path with length-prefixed
//! names and contents — so it cannot be confused by a file whose contents look
//! like a path, and it does not depend on directory iteration order. A mismatch
//! is [`StoreStatus::Blocked`], never a warning, and [`Store::resolve_active`]
//! will not return a blocked extension to any loader.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::error::ExtensionError;
use crate::loader::{LoadedExtension, load_dir};
use crate::manifest::{is_version, validate_namespaced_id, version_tuple};

/// Suffix of the checksum record that sits beside a version directory.
pub const RECORD_SUFFIX: &str = ".sha256";

/// Domain separation for the hash. Changing this deliberately changes every
/// checksum, which is exactly what should happen if the format ever moves.
const HASH_DOMAIN: &[u8] = b"acpilot-extension-v1\n";

/// Directory under the store root used for in-progress installs.
const STAGING_DIR: &str = ".staging";

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Why the store will not hand an extension to a loader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockReason {
    /// The directory has no checksum record: it was not installed by the store.
    Unrecorded,
    /// The bytes on disk do not hash to what was recorded at install time.
    Mismatch {
        /// What the record said.
        recorded: String,
        /// What the files hash to now.
        computed: String,
    },
    /// The record exists but is not in the format the store writes.
    BadRecord {
        /// The parser or format complaint.
        message: String,
    },
    /// A file or directory could not be read.
    Unreadable {
        /// What went wrong.
        message: String,
    },
    /// The tree contains something a content-addressed store cannot represent.
    UnsafeEntry {
        /// The offending entry.
        path: String,
        /// Why it is refused.
        why: String,
    },
    /// The checksum matched but the manifest under it does not load.
    InvalidManifest {
        /// The loader's complaint.
        message: String,
    },
    /// A directory under the store root is not shaped like `<id>/<version>`.
    InvalidName {
        /// The path as found on disk.
        path: String,
        /// Why it is not a store entry.
        message: String,
    },
}

impl std::fmt::Display for BlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unrecorded => {
                f.write_str("no checksum record: this directory was not installed by the store")
            }
            Self::Mismatch { recorded, computed } => {
                write!(
                    f,
                    "checksum mismatch: recorded {recorded}, computed {computed}"
                )
            }
            Self::BadRecord { message } => write!(f, "checksum record is unusable: {message}"),
            Self::Unreadable { message } => write!(f, "could not read the tree: {message}"),
            Self::UnsafeEntry { path, why } => write!(f, "{path}: {why}"),
            Self::InvalidManifest { message } => write!(f, "manifest does not load: {message}"),
            Self::InvalidName { path, message } => write!(f, "{path}: {message}"),
        }
    }
}

/// A version directory's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreStatus {
    /// Recorded checksum matches and the manifest loads.
    Ready,
    /// Refused. A blocked version is invisible to every loader.
    Blocked(BlockReason),
}

/// One version in the store, with its verification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredExtension {
    /// Extension id, or the directory name when it is not a valid id.
    pub id: String,
    /// Version, or empty when the directory name is not a version.
    pub version: String,
    /// Where it lives on disk.
    pub path: PathBuf,
    /// Checksum from the record, when there is one.
    pub recorded: Option<String>,
    /// Checksum computed now, when hashing got far enough to produce one.
    pub computed: Option<String>,
    /// Whether the store will load it.
    pub status: StoreStatus,
}

impl StoredExtension {
    /// Whether the store will hand this version to a loader.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status == StoreStatus::Ready
    }

    /// Whether verification refused it.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        !self.is_ready()
    }

    /// `id@version`, for reports. Falls back to the path when a directory name
    /// is not a valid pair.
    #[must_use]
    pub fn label(&self) -> String {
        if self.version.is_empty() {
            self.path.display().to_string()
        } else {
            format!("{}@{}", self.id, self.version)
        }
    }
}

/// What activation got out of the store.
#[derive(Debug, Clone, Default)]
pub struct ActiveSet {
    /// Verified extensions, newest version per id.
    pub extensions: Vec<LoadedExtension>,
    /// Versions that exist but are blocked.
    pub blocked: Vec<StoredExtension>,
    /// Ids that are active but not installed at all.
    pub missing: Vec<String>,
}

/// A directory of installed extensions.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open a store at a path. The directory is created on first install.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The platform default: `<data dir>/acpilot/extensions`.
    pub fn default_root() -> Result<PathBuf, ExtensionError> {
        let base = directories::BaseDirs::new().ok_or_else(|| ExtensionError::NotFound {
            what: "a home directory to put the extension store in".to_owned(),
        })?;
        Ok(base.data_dir().join("acpilot").join("extensions"))
    }

    /// Open the platform default store.
    pub fn open_default() -> Result<Self, ExtensionError> {
        Ok(Self::new(Self::default_root()?))
    }

    /// The store root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where one version lives.
    #[must_use]
    pub fn version_path(&self, id: &str, version: &str) -> PathBuf {
        self.root.join(id).join(version)
    }

    /// Where one version's checksum record lives.
    #[must_use]
    pub fn record_path(&self, id: &str, version: &str) -> PathBuf {
        self.root.join(id).join(format!("{version}{RECORD_SUFFIX}"))
    }

    /// Install a directory: validate, copy atomically, hash, record.
    ///
    /// The source is read twice on purpose — once to learn the id and version,
    /// once from the staging copy — so a file changing during install cannot
    /// produce a record that describes bytes nobody validated.
    pub fn install_from_dir(
        &self,
        source: impl AsRef<Path>,
    ) -> Result<StoredExtension, ExtensionError> {
        let source = source.as_ref();
        let extension = load_dir(source)?;
        let id = extension.manifest.id.clone();
        let version = extension.manifest.version.clone();
        let destination = self.version_path(&id, &version);

        if destination.exists() {
            return Err(ExtensionError::AlreadyInstalled { id, version });
        }

        self.ensure_root()?;
        let mut staging = Staging::create(self.staging_path())?;
        copy_tree(source, staging.path())?;

        let copied = load_dir(staging.path())?;
        if copied.manifest.id != id || copied.manifest.version != version {
            return Err(ExtensionError::NotFound {
                what: format!(
                    "a stable extension at {} (id or version changed during install)",
                    source.display()
                ),
            });
        }
        let checksum = hash_dir(staging.path())?;

        let parent = destination
            .parent()
            .expect("a version path always has an id directory");
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        fs::rename(staging.path(), &destination)
            .map_err(|source| io_error(staging.path(), source))?;
        staging.disarm();

        // If this fails, the version now exists unrecorded, which every scan
        // reports as blocked. Leaving it is safer than deleting bytes that might
        // be the only copy; the error names the record path that could not be
        // written.
        write_record(&self.record_path(&id, &version), &checksum)?;

        Ok(StoredExtension {
            id,
            version,
            path: destination,
            recorded: Some(checksum.clone()),
            computed: Some(checksum),
            status: StoreStatus::Ready,
        })
    }

    /// Rehash one version and decide whether it is loadable.
    pub fn verify(&self, id: &str, version: &str) -> Result<StoredExtension, ExtensionError> {
        let path = self.version_path(id, version);
        if !path.is_dir() {
            return Err(ExtensionError::NotFound {
                what: format!("{id}@{version} in the store"),
            });
        }
        Ok(self.inspect(id, version, &path))
    }

    /// Verify every version in the store.
    ///
    /// A missing store is an empty store, not an error: a machine that has never
    /// installed an extension should not look broken.
    pub fn scan(&self) -> Result<Vec<StoredExtension>, ExtensionError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error(&self.root, error)),
        };

        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| io_error(&self.root, error))?;
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => {
                    found.push(StoredExtension {
                        id: entry.file_name().to_string_lossy().into_owned(),
                        version: String::new(),
                        path: entry.path(),
                        recorded: None,
                        computed: None,
                        status: StoreStatus::Blocked(BlockReason::InvalidName {
                            path: entry.path().display().to_string(),
                            message: "extension directory name is not valid UTF-8".to_owned(),
                        }),
                    });
                    continue;
                }
            };
            if name.starts_with('.') {
                continue;
            }
            let id_path = entry.path();
            if !id_path.is_dir() {
                continue;
            }
            if let Err(error) = validate_namespaced_id("extension id", &name) {
                found.push(StoredExtension {
                    id: name,
                    version: String::new(),
                    path: id_path.clone(),
                    recorded: None,
                    computed: None,
                    status: StoreStatus::Blocked(BlockReason::InvalidName {
                        path: id_path.display().to_string(),
                        message: error.to_string(),
                    }),
                });
                continue;
            }

            let versions = fs::read_dir(&id_path).map_err(|error| io_error(&id_path, error))?;
            for version in versions {
                let version = version.map_err(|error| io_error(&id_path, error))?;
                let version_name = version.file_name().to_string_lossy().into_owned();
                if version_name.ends_with(RECORD_SUFFIX) || version_name.starts_with('.') {
                    continue;
                }
                if !version.path().is_dir() {
                    continue;
                }
                if !is_version(&version_name) {
                    found.push(StoredExtension {
                        id: name.clone(),
                        version: String::new(),
                        path: version.path(),
                        recorded: None,
                        computed: None,
                        status: StoreStatus::Blocked(BlockReason::InvalidName {
                            path: version.path().display().to_string(),
                            message: "version directory is not MAJOR.MINOR.PATCH".to_owned(),
                        }),
                    });
                    continue;
                }
                found.push(self.inspect(&name, &version_name, &version.path()));
            }
        }

        found.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| b.version.cmp(&a.version)));
        Ok(found)
    }

    /// Remove one version and its record.
    pub fn remove(&self, id: &str, version: &str) -> Result<(), ExtensionError> {
        let path = self.version_path(id, version);
        if !path.is_dir() {
            return Err(ExtensionError::NotFound {
                what: format!("{id}@{version} in the store"),
            });
        }
        fs::remove_dir_all(&path).map_err(|source| io_error(&path, source))?;
        let _ = fs::remove_file(self.record_path(id, version));
        // Only succeeds when empty; a non-empty id directory still has versions.
        let _ = fs::remove_dir(self.root.join(id));
        Ok(())
    }

    /// Versions that would be removed, without touching anything.
    ///
    /// A version is removable when no profile activates it **and** it is not the
    /// newest version of its id. The floor is deliberate: pruning a store where
    /// nothing has been activated yet must not delete the only copy of an
    /// extension somebody just installed. `keep` exists for pinned older
    /// versions once activation can name one.
    pub fn plan_prune(
        &self,
        keep: &BTreeSet<(String, String)>,
    ) -> Result<Vec<(String, String)>, ExtensionError> {
        let versions = self.list_versions()?;
        let mut newest: BTreeMap<String, String> = BTreeMap::new();
        for (id, version) in &versions {
            let replace = match newest.get(id) {
                None => true,
                Some(current) => version_tuple(version) > version_tuple(current),
            };
            if replace {
                newest.insert(id.clone(), version.clone());
            }
        }
        Ok(versions
            .into_iter()
            .filter(|(id, version)| {
                !keep.contains(&(id.clone(), version.clone()))
                    && newest.get(id).map(String::as_str) != Some(version.as_str())
            })
            .collect())
    }

    /// Newest ready version of every id in a scan.
    ///
    /// Activation names an id, not a version; when several versions are
    /// installed the newest verified one wins. `ext prune` keeps exactly these.
    #[must_use]
    pub fn newest_ready<'a>(
        &self,
        scan: &'a [StoredExtension],
    ) -> BTreeMap<String, &'a StoredExtension> {
        newest_ready(scan)
    }

    /// Remove every version not in `keep`, returning what was removed.
    pub fn prune(
        &self,
        keep: &BTreeSet<(String, String)>,
    ) -> Result<Vec<(String, String)>, ExtensionError> {
        let doomed = self.plan_prune(keep)?;
        for (id, version) in &doomed {
            self.remove(id, version)?;
        }
        Ok(doomed)
    }

    /// Resolve an activation list into loadable extensions.
    ///
    /// Newest verified version per id; blocked versions are reported and never
    /// loaded; ids with nothing on disk are reported as missing. The caller
    /// decides whether a missing extension is an error for its purposes.
    pub fn resolve_active(&self, ids: &[String]) -> Result<ActiveSet, Vec<ExtensionError>> {
        let scan = self.scan().map_err(|error| vec![error])?;
        let newest = newest_ready(&scan);
        let mut set = ActiveSet::default();
        let mut errors = Vec::new();

        for id in ids {
            match newest.get(id) {
                Some(stored) => match load_dir(&stored.path) {
                    Ok(extension) => set.extensions.push(extension),
                    Err(error) => errors.push(error),
                },
                None => {
                    let blocked: Vec<StoredExtension> = scan
                        .iter()
                        .filter(|stored| &stored.id == id && stored.is_blocked())
                        .cloned()
                        .collect();
                    if blocked.is_empty() {
                        set.missing.push(id.clone());
                    } else {
                        set.blocked.extend(blocked);
                    }
                }
            }
        }

        set.extensions
            .sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        if errors.is_empty() {
            Ok(set)
        } else {
            Err(errors)
        }
    }

    /// Load the newest verified version of every id.
    pub fn load_latest(&self) -> Result<Vec<LoadedExtension>, Vec<ExtensionError>> {
        let scan = self.scan().map_err(|error| vec![error])?;
        let mut errors = Vec::new();
        let mut loaded = Vec::new();
        for stored in newest_ready(&scan).values() {
            match load_dir(&stored.path) {
                Ok(extension) => loaded.push(extension),
                Err(error) => errors.push(error),
            }
        }
        loaded.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        if errors.is_empty() {
            Ok(loaded)
        } else {
            Err(errors)
        }
    }

    fn inspect(&self, id: &str, version: &str, path: &Path) -> StoredExtension {
        let record_path = self.record_path(id, version);

        let recorded = match fs::read_to_string(&record_path) {
            Ok(text) => match parse_record(&text) {
                Ok(checksum) => checksum,
                Err(message) => {
                    return blocked(
                        id,
                        version,
                        path,
                        None,
                        None,
                        BlockReason::BadRecord { message },
                    );
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return blocked(id, version, path, None, None, BlockReason::Unrecorded);
            }
            Err(error) => {
                return blocked(
                    id,
                    version,
                    path,
                    None,
                    None,
                    BlockReason::Unreadable {
                        message: error.to_string(),
                    },
                );
            }
        };

        let computed = match hash_dir(path) {
            Ok(checksum) => checksum,
            Err(ExtensionError::UnsupportedEntry { path: entry, why }) => {
                return blocked(
                    id,
                    version,
                    path,
                    Some(recorded),
                    None,
                    BlockReason::UnsafeEntry {
                        path: entry,
                        why: why.to_owned(),
                    },
                );
            }
            Err(error) => {
                return blocked(
                    id,
                    version,
                    path,
                    Some(recorded),
                    None,
                    BlockReason::Unreadable {
                        message: error.to_string(),
                    },
                );
            }
        };

        if recorded != computed {
            let reason = BlockReason::Mismatch {
                recorded: recorded.clone(),
                computed: computed.clone(),
            };
            return blocked(id, version, path, Some(recorded), Some(computed), reason);
        }

        if let Err(error) = load_dir(path) {
            return blocked(
                id,
                version,
                path,
                Some(recorded),
                Some(computed),
                BlockReason::InvalidManifest {
                    message: error.to_string(),
                },
            );
        }

        StoredExtension {
            id: id.to_owned(),
            version: version.to_owned(),
            path: path.to_path_buf(),
            recorded: Some(recorded),
            computed: Some(computed),
            status: StoreStatus::Ready,
        }
    }

    fn list_versions(&self) -> Result<Vec<(String, String)>, ExtensionError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error(&self.root, error)),
        };
        let mut versions = Vec::new();
        for id in entries {
            let id = id.map_err(|error| io_error(&self.root, error))?;
            let id_name = id.file_name().to_string_lossy().into_owned();
            if id_name.starts_with('.') || !id.path().is_dir() {
                continue;
            }
            let entries = fs::read_dir(id.path()).map_err(|error| io_error(&id.path(), error))?;
            for version in entries {
                let version = version.map_err(|error| io_error(&id.path(), error))?;
                let name = version.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || !version.path().is_dir() || !is_version(&name) {
                    continue;
                }
                versions.push((id_name.clone(), name));
            }
        }
        versions.sort();
        Ok(versions)
    }

    fn ensure_root(&self) -> Result<(), ExtensionError> {
        fs::create_dir_all(&self.root).map_err(|source| io_error(&self.root, source))
    }

    fn staging_path(&self) -> PathBuf {
        let counter = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        self.root
            .join(STAGING_DIR)
            .join(format!("{}-{counter}-{nanos}", std::process::id()))
    }
}

fn blocked(
    id: &str,
    version: &str,
    path: &Path,
    recorded: Option<String>,
    computed: Option<String>,
    reason: BlockReason,
) -> StoredExtension {
    StoredExtension {
        id: id.to_owned(),
        version: version.to_owned(),
        path: path.to_path_buf(),
        recorded,
        computed,
        status: StoreStatus::Blocked(reason),
    }
}

fn newest_ready(scan: &[StoredExtension]) -> BTreeMap<String, &StoredExtension> {
    let mut newest: BTreeMap<String, &StoredExtension> = BTreeMap::new();
    for stored in scan.iter().filter(|stored| stored.is_ready()) {
        let replace = match newest.get(&stored.id) {
            None => true,
            Some(existing) => version_tuple(&stored.version) > version_tuple(&existing.version),
        };
        if replace {
            newest.insert(stored.id.clone(), stored);
        }
    }
    newest
}

/// SHA-256 over a canonical byte stream: the format tag, then every file sorted
/// by path, each as length-prefixed path bytes followed by length-prefixed
/// contents. Empty directories are not represented — they carry no data.
pub fn hash_dir(root: &Path) -> Result<String, ExtensionError> {
    let files = collect_files(root)?;
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN);
    for (relative, path) in files {
        let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
        let path_len =
            u32::try_from(relative.len()).map_err(|_| ExtensionError::UnsupportedEntry {
                path: relative.clone(),
                why: "path is longer than 4 GiB",
            })?;
        hasher.update(path_len.to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("sha256:{}", hex(&hasher.finalize())))
}

/// Every regular file under a root, as `(relative path with forward slashes, path)`.
fn collect_files(root: &Path) -> Result<Vec<(String, PathBuf)>, ExtensionError> {
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

fn walk(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), ExtensionError> {
    let entries = fs::read_dir(directory).map_err(|source| io_error(directory, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_error(directory, source))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
        if metadata.file_type().is_symlink() {
            return Err(ExtensionError::UnsupportedEntry {
                path: path.display().to_string(),
                why: "symlinks are not part of a content-addressed tree",
            });
        }
        if metadata.is_dir() {
            walk(root, &path, files)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(ExtensionError::UnsupportedEntry {
                path: path.display().to_string(),
                why: "only regular files can be hashed",
            });
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| ExtensionError::UnsupportedEntry {
                path: path.display().to_string(),
                why: "resolved outside the tree being hashed",
            })?;
        let mut parts = Vec::new();
        for component in relative.components() {
            let part = component.as_os_str().to_str().ok_or_else(|| {
                ExtensionError::UnsupportedEntry {
                    path: path.display().to_string(),
                    why: "file names must be valid UTF-8, so a checksum means the same thing on every host",
                }
            })?;
            parts.push(part);
        }
        files.push((parts.join("/"), path));
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), ExtensionError> {
    fs::create_dir_all(destination).map_err(|error| io_error(destination, error))?;
    let entries = fs::read_dir(source).map_err(|error| io_error(source, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_error(source, error))?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from).map_err(|error| io_error(&from, error))?;
        if metadata.file_type().is_symlink() {
            return Err(ExtensionError::UnsupportedEntry {
                path: from.display().to_string(),
                why: "symlinks are not part of a content-addressed tree",
            });
        }
        if metadata.is_dir() {
            copy_tree(&from, &to)?;
        } else if metadata.is_file() {
            fs::copy(&from, &to).map_err(|error| io_error(&from, error))?;
        } else {
            return Err(ExtensionError::UnsupportedEntry {
                path: from.display().to_string(),
                why: "only regular files can be installed",
            });
        }
    }
    Ok(())
}

fn write_record(path: &Path, checksum: &str) -> Result<(), ExtensionError> {
    let temporary = path.with_extension("sha256.tmp");
    fs::write(&temporary, format!("{checksum}\n"))
        .map_err(|source| io_error(&temporary, source))?;
    fs::rename(&temporary, path).map_err(|source| io_error(&temporary, source))
}

fn parse_record(text: &str) -> Result<String, String> {
    let text = text.trim();
    let invalid = || format!("expected `sha256:<64 hex characters>`, got {text:?}");
    let digest = text.strip_prefix("sha256:").ok_or_else(invalid)?;
    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    Ok(format!("sha256:{}", digest.to_ascii_lowercase()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn io_error(path: &Path, source: io::Error) -> ExtensionError {
    ExtensionError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// A staging directory that cleans itself up unless the install succeeds.
struct Staging {
    path: PathBuf,
    armed: bool,
}

impl Staging {
    fn create(path: PathBuf) -> Result<Self, ExtensionError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        }
        fs::create_dir_all(&path).map_err(|source| io_error(&path, source))?;
        Ok(Self { path, armed: true })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
