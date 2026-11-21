//! `acpilot lsp` — the signed catalog and the explicit installer (ADR 0014).
//!
//! v0.1 detects what is already on `PATH`. This module is the v0.2 path: a catalog
//! generated from reviewed server descriptions, signed with a key that is not the
//! release key, and an install that a person asks for by name. Nothing here runs on
//! a timer, on daemon startup, or because a file mentioned a language; the install
//! is the approval, and the executable's digest is recorded with it.
//!
//! # The unit of trust
//!
//! The catalog's detached signature says which bytes the maintainers published. A
//! checksum in that signed document says which archive those bytes name. The
//! archive is verified before it is unpacked, and the unpacker refuses a path that
//! leaves its directory. Verifying any one of those alone proves nothing: a
//! substituted archive comes with a matching checksum, and an unsigned checksum is
//! a number in a text file.
//!
//! # What is deliberately absent
//!
//! No update check, no background prefetch, and no silent selection. Installing a
//! newer version is another command. A version this build did not install is not
//! started on the strength of being on `PATH` when a managed one exists, which is
//! the resolution rule ADR 0014 records and the daemon implements.

use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::LspCommand;
use crate::release;

/// The largest archive an install will unpack.
///
/// A language server is tens of megabytes; half a gigabyte is a generous ceiling
/// that still turns a decompression bomb into a refusal before it fills a disk.
pub const MAX_ARCHIVE: u64 = 512 * 1024 * 1024;

/// Where the reviewed catalog is published.
///
/// Overridable for a mirror or a test, the same way the extension index is. A
/// catalog fetched from another address is still verified with the same key: the
/// address is a transport choice, not a trust decision.
pub const DEFAULT_INDEX: &str = "https://rudeboy.dev/acpilot/lsp/index-v1.json";

/// The environment variable that names an index.
const INDEX_ENV: &str = "ACPILOT_LSP_INDEX";

/// The environment variable that carries the catalog's public key.
const KEY_ENV: &str = "ACPILOT_LSP_INDEX_PUBKEY";

/// The published catalog's ed25519 public key, as 64 hex characters.
///
/// Separate from the extension index's key and the release key (ADR 0014): a
/// catalog review is a different duty from cutting a release, and rotating one
/// must not invalidate the other. A mirror can add a key through the environment;
/// it cannot replace this one.
pub const MAINTAINER_PUBLIC_KEY: &str =
    "19d3f6a87b2ea858b2e3024f1dd4fc3648c74f69d5cf052efb1743656a02bddf";

/// One published catalog.
#[derive(Debug, Deserialize)]
pub struct Catalog {
    /// Schema version. Only 1 is understood.
    pub schema: u32,
    /// The servers the catalog knows, sorted by id as published.
    pub servers: Vec<Server>,
}

/// One server in the catalog.
#[derive(Debug, Deserialize)]
pub struct Server {
    /// Stable id, the argument to `lsp install`.
    pub id: String,
    /// Human name.
    pub name: String,
    /// LSP language ids this server answers.
    pub languages: Vec<String>,
    /// SPDX expression or a short licence name.
    pub license: String,
    /// Whether the project's licence permits redistributing a build.
    ///
    /// False entries are still listed, with no download URL: knowing a server
    /// exists is useful, and pretending it can be fetched is not.
    pub redistributable: bool,
    /// Published versions, newest last.
    pub versions: Vec<Version>,
}

/// One version of one server.
#[derive(Debug, Deserialize)]
pub struct Version {
    /// The version string, as the project spells it.
    pub version: String,
    /// One entry per target triple.
    pub targets: BTreeMap<String, Artifact>,
}

/// One downloadable build.
#[derive(Debug, Deserialize)]
pub struct Artifact {
    /// Where the archive is downloaded from.
    pub url: String,
    /// `sha256:<64 hex>` over the archive.
    pub sha256: String,
    /// The archive's exact length, so a truncated download is caught before hashing.
    pub size: u64,
    /// The executable to find inside the archive.
    pub executable: String,
    /// `tar.gz`, `zip` or `gz`. Absent is `tar.gz`, the shape v1 first published.
    #[serde(default)]
    pub archive: String,
    /// Arguments the server needs, in order.
    #[serde(default)]
    pub args: Vec<String>,
}

/// What an install wrote beside the executable.
///
/// The daemon reads this to resolve a language to a managed server. It records the
/// digest of the executable, not only of the archive, so a file replaced after the
/// install is visible as a different program.
#[derive(Debug, Serialize, Deserialize)]
pub struct Installed {
    /// Schema version.
    pub schema: u32,
    /// Catalog id.
    pub id: String,
    /// Human name.
    pub name: String,
    /// Installed version.
    pub version: String,
    /// Target triple.
    pub target: String,
    /// Absolute path to the executable.
    pub executable: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Languages this server answers.
    pub languages: Vec<String>,
    /// `sha256:<64 hex>` of the executable at install time.
    pub sha256: String,
}

/// Run one `acpilot lsp` verb.
pub fn run(command: LspCommand) -> u8 {
    match command {
        LspCommand::Search { query } => search(query.as_deref()),
        LspCommand::Install { id, version } => install(&id, version.as_deref()),
        LspCommand::List => list(),
        LspCommand::Remove { id } => remove(&id),
    }
}

/// Fetch and verify the catalog.
fn catalog() -> Result<Catalog, String> {
    let url = std::env::var(INDEX_ENV).unwrap_or_else(|_| DEFAULT_INDEX.to_owned());
    let bytes = release::get(&url)?;
    let signature = release::get(&format!("{url}.sig"))?;
    // A mirror's key is added beside the maintainer's, never instead of it: a
    // settings variable that could replace the built-in key would be a way to
    // publish a catalog the binary did not choose to trust.
    let extra = std::env::var(KEY_ENV).ok();
    let mut keys = vec![MAINTAINER_PUBLIC_KEY];
    if let Some(extra) = extra.as_deref()
        && extra != MAINTAINER_PUBLIC_KEY
    {
        keys.push(extra);
    }
    let mut last_error = None;
    let mut trusted = false;
    for key in keys {
        match verify(&bytes, &signature, key) {
            Ok(()) => {
                trusted = true;
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    if !trusted {
        return Err(last_error.unwrap_or_else(|| "the catalog has no trusted signature".to_owned()));
    }
    let catalog: Catalog = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{url} is not a language-server catalog: {error}"))?;
    validate(&catalog)?;
    Ok(catalog)
}

/// Verify a detached signature over the exact catalog bytes.
///
/// The signature file is the extension index's shape — `{"key": "...", "signature":
/// "<128 hex>"}` — so one publishing pipeline produces both documents.
///
/// # Errors
///
/// The signature is malformed, the key is not 32 bytes of hex, or the signature is
/// not this key's over these bytes.
pub fn verify(bytes: &[u8], signature: &[u8], key_hex: &str) -> Result<(), String> {
    let key = parse_key(key_hex)?;
    let file: SignatureFile = serde_json::from_slice(signature)
        .map_err(|error| format!("the catalog signature is not JSON: {error}"))?;
    let raw = decode_hex(&file.signature, 64).map_err(|error| {
        format!("the catalog signature is not a 64-byte ed25519 signature: {error}")
    })?;
    let mut signature = [0u8; 64];
    signature.copy_from_slice(&raw);
    key.verify_strict(bytes, &Signature::from_bytes(&signature))
        .map_err(|_| "the catalog is not signed by the key this command was given".to_owned())
}

/// The published signature envelope.
#[derive(Debug, Deserialize)]
struct SignatureFile {
    #[allow(dead_code)]
    key: String,
    signature: String,
}

/// Parse a 64-hex public key.
fn parse_key(hex: &str) -> Result<VerifyingKey, String> {
    let bytes = decode_hex(hex.trim(), 32)?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&key).map_err(|error| format!("not an ed25519 public key: {error}"))
}

/// Hex into exactly `length` bytes.
fn decode_hex(hex: &str, length: usize) -> Result<Vec<u8>, String> {
    if hex.len() != length * 2 {
        return Err(format!(
            "expected {} hex characters and got {}",
            length * 2,
            hex.len()
        ));
    }
    let mut bytes = Vec::with_capacity(length);
    for pair in hex.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(pair).map_err(|error| format!("not hex: {error}"))?;
        bytes.push(u8::from_str_radix(pair, 16).map_err(|error| format!("not hex: {error}"))?);
    }
    Ok(bytes)
}

/// Check the parts of a catalog that would make an install unsafe.
fn validate(catalog: &Catalog) -> Result<(), String> {
    if catalog.schema != 1 {
        return Err(format!(
            "the catalog schema is {} and this build understands 1",
            catalog.schema
        ));
    }
    for server in &catalog.servers {
        if !valid_id(&server.id) {
            return Err(format!("`{}` is not a catalog id", server.id));
        }
        for version in &server.versions {
            if version.version.is_empty() || version.version.contains('/') {
                return Err(format!("{} has an invalid version", server.id));
            }
            for (target, artifact) in &version.targets {
                if !valid_id(target) {
                    return Err(format!("{} names an invalid target `{target}`", server.id));
                }
                if artifact.size > MAX_ARCHIVE {
                    return Err(format!(
                        "{} {} is {} bytes, past the {} byte ceiling",
                        server.id, version.version, artifact.size, MAX_ARCHIVE
                    ));
                }
                if !matches!(artifact.archive.as_str(), "" | "tar.gz" | "zip" | "gz") {
                    return Err(format!(
                        "{} {} names an archive `{}`; expected tar.gz, zip or gz",
                        server.id, version.version, artifact.archive
                    ));
                }
                if artifact.archive == "gz" && artifact.executable.contains(['/', '\\']) {
                    return Err(format!(
                        "{} {} is a single gzip stream but names a path as its executable",
                        server.id, version.version
                    ));
                }
                if !artifact.sha256.starts_with("sha256:")
                    || artifact.sha256.len() != "sha256:".len() + 64
                    || !artifact.sha256["sha256:".len()..]
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
                {
                    return Err(format!(
                        "{} {} has no `sha256:<64 hex>` checksum",
                        server.id, version.version
                    ));
                }
                if server.redistributable && !artifact.url.starts_with("https://") {
                    return Err(format!(
                        "{} {} is downloadable but its URL is not https",
                        server.id, version.version
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Whether `id` is one path segment of lower-case letters, digits, `-` or `_`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || "-_".contains(character)
        })
}

/// Print the servers that match `query`.
fn search(query: Option<&str>) -> u8 {
    let catalog = match catalog() {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("acpilot lsp search: {error}");
            return acpilot_core::error::exit::FAILURE;
        }
    };
    let needle = query.unwrap_or("").to_ascii_lowercase();
    let mut found = 0;
    for server in &catalog.servers {
        let matches = needle.is_empty()
            || server.id.to_ascii_lowercase().contains(&needle)
            || server.name.to_ascii_lowercase().contains(&needle)
            || server
                .languages
                .iter()
                .any(|language| language.to_ascii_lowercase().contains(&needle));
        if !matches {
            continue;
        }
        found += 1;
        let version = server
            .versions
            .last()
            .map_or("no build", |version| version.version.as_str());
        let fetch = if server.redistributable {
            "installable"
        } else {
            "no redistribution; bring your own"
        };
        println!(
            "{}  {} — {} [{}] {} ({version})",
            server.id,
            server.name,
            server.languages.join(", "),
            server.license,
            fetch
        );
    }
    if found == 0 {
        println!("no server matches {needle:?}");
    }
    acpilot_core::error::exit::SUCCESS
}

/// Install one server version for this machine.
fn install(id: &str, version: Option<&str>) -> u8 {
    let catalog = match catalog() {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("acpilot lsp install: {error}");
            return acpilot_core::error::exit::FAILURE;
        }
    };
    let server = match catalog.servers.iter().find(|server| server.id == id) {
        Some(server) => server,
        None => {
            eprintln!(
                "acpilot lsp install: no `{id}` in the catalog; `acpilot lsp search` lists ids"
            );
            return acpilot_core::error::exit::NOT_FOUND;
        }
    };
    if !server.redistributable {
        eprintln!(
            "acpilot lsp install: {} is {} and its licence does not permit redistribution; \
             install it with the project's own package manager",
            server.name, server.license
        );
        return acpilot_core::error::exit::FAILURE;
    }
    let triple = match release::triple() {
        Some(triple) => triple,
        None => {
            eprintln!(
                "acpilot lsp install: this build has no target triple for {} {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            return acpilot_core::error::exit::FAILURE;
        }
    };
    let selected = version.map_or_else(
        || server.versions.last(),
        |wanted| {
            server
                .versions
                .iter()
                .find(|version| version.version == wanted)
        },
    );
    let Some(version) = selected else {
        eprintln!(
            "acpilot lsp install: {} has no {} build",
            server.id,
            version.unwrap_or("published")
        );
        return acpilot_core::error::exit::NOT_FOUND;
    };
    let Some(artifact) = version.targets.get(triple) else {
        eprintln!(
            "acpilot lsp install: {} {} does not publish a {triple} build",
            server.id, version.version
        );
        return acpilot_core::error::exit::FAILURE;
    };
    if artifact.size > MAX_ARCHIVE {
        eprintln!("acpilot lsp install: the archive is past the size ceiling");
        return acpilot_core::error::exit::FAILURE;
    }
    println!(
        "installing {} {} for {triple} ({}, {:.1} MiB)",
        server.name,
        version.version,
        server.license,
        artifact.size as f64 / (1024.0 * 1024.0)
    );
    let bytes = match release::get(&artifact.url) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("acpilot lsp install: {error}");
            return acpilot_core::error::exit::FAILURE;
        }
    };
    if bytes.len() as u64 > MAX_ARCHIVE {
        eprintln!(
            "acpilot lsp install: the download is {} bytes, past the {} byte ceiling; nothing was written",
            bytes.len(),
            MAX_ARCHIVE
        );
        return acpilot_core::error::exit::FAILURE;
    }
    if bytes.len() as u64 != artifact.size {
        eprintln!(
            "acpilot lsp install: the catalog says {} bytes and the download is {}; nothing was written",
            artifact.size,
            bytes.len()
        );
        return acpilot_core::error::exit::FAILURE;
    }
    if let Err(error) = release::verify(&bytes, artifact.sha256.trim_start_matches("sha256:")) {
        eprintln!("acpilot lsp install: {error}");
        return acpilot_core::error::exit::FAILURE;
    }
    let root = match install_root() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("acpilot lsp install: {error}");
            return acpilot_core::error::exit::FAILURE;
        }
    };
    let destination = root.join(&server.id).join(&version.version).join(triple);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let staging = root.join(format!(".staging-{}-{stamp}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&staging) {
        eprintln!(
            "acpilot lsp install: cannot create {}: {error}",
            staging.display()
        );
        return acpilot_core::error::exit::FAILURE;
    }
    let installed = (|| {
        let executable = extract(&bytes, &staging, &artifact.executable, &artifact.archive)?;
        acpilot_host::make_executable(&executable)
            .map_err(|error| format!("cannot make {} executable: {error}", executable.display()))?;
        let digest = Sha256::digest(
            std::fs::read(&executable)
                .map_err(|error| format!("cannot read {}: {error}", executable.display()))?,
        );
        let digest = format!(
            "sha256:{}",
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        if destination.exists() {
            std::fs::remove_dir_all(&destination)
                .map_err(|error| format!("cannot replace {}: {error}", destination.display()))?;
        }
        std::fs::rename(&staging, &destination).map_err(|error| {
            format!(
                "cannot move {} to {}: {error}",
                staging.display(),
                destination.display()
            )
        })?;
        let installed = Installed {
            schema: 1,
            id: server.id.clone(),
            name: server.name.clone(),
            version: version.version.clone(),
            target: triple.to_owned(),
            executable: destination
                .join(executable.strip_prefix(&staging).unwrap_or(&executable))
                .to_string_lossy()
                .into_owned(),
            args: artifact.args.clone(),
            languages: server.languages.clone(),
            sha256: digest,
        };
        let manifest = serde_json::to_vec_pretty(&installed)
            .map_err(|error| format!("cannot encode the install record: {error}"))?;
        std::fs::write(destination.join("install.json"), manifest)
            .map_err(|error| format!("cannot write the install record: {error}"))?;
        Ok::<Installed, String>(installed)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    match installed {
        Ok(installed) => {
            println!("installed {} {}", installed.id, installed.version);
            println!("{}", installed.executable);
            println!(
                "the daemon prefers this over `PATH`; remove it with `acpilot lsp remove {}`",
                installed.id
            );
            acpilot_core::error::exit::SUCCESS
        }
        Err(error) => {
            eprintln!("acpilot lsp install: {error}; nothing was installed");
            acpilot_core::error::exit::FAILURE
        }
    }
}

/// Unpack `bytes` and return the executable's path inside `directory`.
fn extract(
    bytes: &[u8],
    directory: &Path,
    executable: &str,
    archive: &str,
) -> Result<PathBuf, String> {
    match archive {
        "gz" => gunzip(bytes, directory, executable)?,
        "zip" => unzip(bytes, directory)?,
        "" | "tar.gz" => untar(bytes, directory)?,
        other => return Err(format!("the catalog names an unknown archive `{other}`")),
    }
    find_executable(directory, executable)
        .ok_or_else(|| format!("the archive holds no `{executable}`"))
}

/// Decompress one gzip stream into the executable the catalog names.
fn gunzip(bytes: &[u8], directory: &Path, executable: &str) -> Result<(), String> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Err("the download is not a gzip stream".to_owned());
    }
    let path = directory.join(executable);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let mut file = std::fs::File::create(&path)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    std::io::copy(
        &mut flate2::read::GzDecoder::new(Cursor::new(bytes)),
        &mut file,
    )
    .map_err(|error| format!("cannot decompress the executable: {error}"))?;
    file.flush()
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(())
}

/// Unpack a gzip tarball, refusing any entry that is not a regular file inside.
fn untar(bytes: &[u8], directory: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(Cursor::new(bytes)));
    for entry in archive
        .entries()
        .map_err(|error| format!("the tarball cannot be read: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("the tarball cannot be read: {error}"))?;
        if !entry.header().entry_type().is_file() {
            return Err("the tarball holds something that is not a regular file".to_owned());
        }
        let path = entry
            .path()
            .map_err(|error| format!("the tarball has an unreadable path: {error}"))?;
        if !safe(&path) {
            return Err(format!(
                "the tarball holds an unsafe path: {}",
                path.display()
            ));
        }
        entry
            .unpack_in(directory)
            .map_err(|error| format!("the tarball cannot be unpacked: {error}"))?;
    }
    Ok(())
}

/// Unpack a zip, refusing a name that leaves the directory.
fn unzip(bytes: &[u8], directory: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("the zip cannot be read: {error}"))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("the zip cannot be read: {error}"))?;
        let Some(name) = entry.enclosed_name() else {
            return Err("the zip holds an unsafe or unreadable path".to_owned());
        };
        if !safe(&name) {
            return Err(format!("the zip holds an unsafe path: {}", name.display()));
        }
        let path = directory.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(&path)
                .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let mut file = std::fs::File::create(&path)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        std::io::copy(&mut entry, &mut file)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        file.flush()
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    Ok(())
}

/// Whether an archive path is relative and has no `..` component.
fn safe(path: &Path) -> bool {
    path.components().all(|component| {
        matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
}

/// Find a file named `executable` under `directory`.
fn find_executable(directory: &Path, executable: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(directory).ok()?;
    let mut directories = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name == executable)
        {
            return Some(path);
        }
        if path.is_dir() {
            directories.push(path);
        }
    }
    directories
        .into_iter()
        .find_map(|directory| find_executable(&directory, executable))
}

/// The directory managed installs live under: `<data>/lsp`.
fn install_root() -> Result<PathBuf, String> {
    let root = acpilot_profile::data_dir().join("lsp");
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("cannot create {}: {error}", root.display()))?;
    Ok(root)
}

/// Print every managed install.
fn list() -> u8 {
    let root = match install_root() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("acpilot lsp list: {error}");
            return acpilot_core::error::exit::FAILURE;
        }
    };
    let mut rows = Vec::new();
    if let Ok(servers) = std::fs::read_dir(&root) {
        for server in servers.flatten() {
            if !server.path().is_dir() {
                continue;
            }
            if let Ok(versions) = std::fs::read_dir(server.path()) {
                for version in versions.flatten() {
                    if let Ok(targets) = std::fs::read_dir(version.path()) {
                        for target in targets.flatten() {
                            let manifest = target.path().join("install.json");
                            if let Ok(text) = std::fs::read_to_string(&manifest)
                                && let Ok(installed) = serde_json::from_str::<Installed>(&text)
                            {
                                rows.push((installed, target.path()));
                            }
                        }
                    }
                }
            }
        }
    }
    rows.sort_by(|left, right| {
        left.0
            .id
            .cmp(&right.0.id)
            .then_with(|| left.0.version.cmp(&right.0.version))
    });
    if rows.is_empty() {
        println!("no managed language servers; `acpilot lsp search` lists the catalog");
        return acpilot_core::error::exit::SUCCESS;
    }
    for (installed, directory) in rows {
        println!(
            "{} {} ({}, {})",
            installed.id,
            installed.version,
            installed.target,
            directory.display()
        );
        println!("  {}", installed.executable);
        println!("  languages: {}", installed.languages.join(", "));
    }
    acpilot_core::error::exit::SUCCESS
}

/// Remove every version of one managed server.
fn remove(id: &str) -> u8 {
    if !valid_id(id) {
        eprintln!("acpilot lsp remove: `{id}` is not a catalog id");
        return acpilot_core::error::exit::USAGE;
    }
    let root = match install_root() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("acpilot lsp remove: {error}");
            return acpilot_core::error::exit::FAILURE;
        }
    };
    let directory = root.join(id);
    if !directory.exists() {
        eprintln!("acpilot lsp remove: {id} is not installed");
        return acpilot_core::error::exit::NOT_FOUND;
    }
    match std::fs::remove_dir_all(&directory) {
        Ok(()) => {
            println!("removed {id}");
            acpilot_core::error::exit::SUCCESS
        }
        Err(error) => {
            eprintln!(
                "acpilot lsp remove: cannot remove {}: {error}",
                directory.display()
            );
            acpilot_core::error::exit::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    fn key(seed: u8) -> (ed25519_dalek::SigningKey, String) {
        let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let hex = key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        (key, hex)
    }

    fn sign(key: &ed25519_dalek::SigningKey, bytes: &[u8]) -> Vec<u8> {
        let signature = key.sign(bytes);
        serde_json::to_vec(&serde_json::json!({
            "key": "test",
            "signature": signature.to_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        }))
        .expect("json")
    }

    const CATALOG: &str = r#"{
        "schema": 1,
        "servers": [{
            "id": "rust-analyzer",
            "name": "rust-analyzer",
            "languages": ["rust"],
            "license": "MIT OR Apache-2.0",
            "redistributable": true,
            "versions": [{
                "version": "2025-11-01",
                "targets": {
                    "x86_64-unknown-linux-gnu": {
                        "url": "https://example.com/rust-analyzer.gz",
                        "sha256": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "size": 10,
                        "executable": "rust-analyzer",
                        "args": []
                    }
                }
            }]
        }]
    }"#;

    #[test]
    fn the_published_catalog_is_signed_by_the_built_in_key() {
        let catalog = include_bytes!("../../../marketplace/lsp/index-v1.json");
        let signature = include_bytes!("../../../marketplace/lsp/index-v1.json.sig");
        verify(catalog, signature, MAINTAINER_PUBLIC_KEY).expect("the published signature");
        let parsed: Catalog = serde_json::from_slice(catalog).expect("the published catalog");
        validate(&parsed).expect("the published catalog validates");
    }

    #[test]
    fn a_single_gzip_stream_becomes_the_named_executable() {
        use std::io::Write as _;
        let directory = tempfile::tempdir().expect("temp dir");
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"the server").expect("compress");
        let bytes = encoder.finish().expect("gzip bytes");
        let executable = extract(&bytes, directory.path(), "rust-analyzer", "gz")
            .expect("the gzip stream is the executable");
        assert_eq!(std::fs::read(executable).expect("read"), b"the server");
    }

    #[test]
    fn a_signed_catalog_verifies_and_a_changed_one_does_not() {
        let (key, hex) = key(7);
        let signature = sign(&key, CATALOG.as_bytes());
        verify(CATALOG.as_bytes(), &signature, &hex).expect("signed");
        let mut changed = CATALOG.as_bytes().to_vec();
        changed[0] = b' ';
        assert!(verify(&changed, &signature, &hex).is_err());
    }

    #[test]
    fn a_catalog_id_is_one_safe_segment() {
        assert!(valid_id("rust-analyzer"));
        assert!(!valid_id("../rust"));
        assert!(!valid_id("Rust"));
        assert!(!valid_id(""));
    }

    #[test]
    fn an_archive_path_may_not_leave_its_directory() {
        assert!(safe(Path::new("bin/server")));
        assert!(!safe(Path::new("../server")));
        assert!(!safe(Path::new("/etc/passwd")));
    }

    #[test]
    fn a_catalog_without_a_checksum_is_refused() {
        let catalog = Catalog {
            schema: 1,
            servers: vec![Server {
                id: "x".to_owned(),
                name: "x".to_owned(),
                languages: vec!["rust".to_owned()],
                license: "MIT".to_owned(),
                redistributable: true,
                versions: vec![Version {
                    version: "1".to_owned(),
                    targets: BTreeMap::from([(
                        "x86_64-unknown-linux-gnu".to_owned(),
                        Artifact {
                            url: "https://example.com/x.gz".to_owned(),
                            sha256: "nope".to_owned(),
                            size: 1,
                            executable: "x".to_owned(),
                            archive: "gz".to_owned(),
                            args: Vec::new(),
                        },
                    )]),
                }],
            }],
        };
        assert!(validate(&catalog).is_err());
    }

    #[test]
    fn an_executable_is_found_under_a_leading_directory() {
        let directory = tempfile::tempdir().expect("temp dir");
        let bin = directory.path().join("rust-analyzer-2025/bin");
        std::fs::create_dir_all(&bin).expect("dirs");
        let program = bin.join("rust-analyzer");
        std::fs::write(&program, b"binary").expect("write");
        assert_eq!(
            find_executable(directory.path(), "rust-analyzer"),
            Some(program)
        );
    }
}
