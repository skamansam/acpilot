//! The signed static registry index.
//!
//! The marketplace is a file, not a service (ADR 0003): a reviewed list of
//! extension versions, each naming the archive a person would download and the
//! sha256 that archive must hash to. This module answers two questions about it:
//! which version a listing names, and whether the maintainer signed that list.
//!
//! The signature is detached and covers the index's exact bytes, so the pairing
//! is the same one `acpilot update` uses for a release: the signature says who
//! wrote the file, and the checksums inside say which archives that author meant.
//! Verifying either alone proves nothing — an attacker who can rewrite the index
//! can rewrite its checksums too.
//!
//! A key that verifies nothing is not a warning. An index nobody trusted signed
//! is refused by name, because "the download happened anyway" is the one outcome
//! a registry must not have.

use acpilot_proto::extension::ExtensionKind;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::error::ExtensionError;
use crate::manifest::{is_version, validate_namespaced_id, validate_segment, version_tuple};

/// The index schema this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// The filename an index is published under.
pub const INDEX_FILE: &str = "index.json";

/// The suffix of the detached signature beside an index.
pub const SIGNATURE_SUFFIX: &str = ".sig";

/// The id the maintainer's key is known by.
pub const MAINTAINER_KEY_ID: &str = "acpilot-release";

/// The maintainer's ed25519 public key, as 64 hex characters.
///
/// Compiled in rather than read from the machine: a key file an attacker could
/// write is a key an attacker could replace, and the point of the signature is
/// deciding who wrote the list. `registry.trusted_keys` adds a private key beside
/// this one; it cannot replace it.
pub const MAINTAINER_PUBLIC_KEY: &str =
    "3a9c60b5686df20f7985b632de1960ce5ffc22b6530fc57cd8a46bf3f2dd5813";

/// The index compiled into this build.
///
/// `acpilot ext search` and `acpilot ext add` use this unless `--index` names
/// another file or URL. Shipping it in the binary is what keeps the default from
/// being a network call: a build's listings are the listings its release shipped
/// with, and publishing a new one takes a release rather than a background fetch.
pub const DEFAULT_INDEX: &[u8] = include_bytes!("../../../marketplace/index.json");

/// The signature over [`DEFAULT_INDEX`].
pub const DEFAULT_SIGNATURE: &[u8] = include_bytes!("../../../marketplace/index.json.sig");

/// One version of one extension, as a listing names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEntry {
    /// Namespaced identifier, `publisher.id`.
    pub id: String,
    /// Semantic version; the archive's manifest must agree.
    pub version: String,
    /// The registry the listing is filed under. An extension may contribute
    /// more kinds than this; the manifest is the authority on those.
    pub kind: ExtensionKind,
    /// Who published it; the manifest must agree.
    pub publisher: String,
    /// `sha256:<64 hex characters>` over the archive's bytes.
    pub checksum: String,
    /// The archive's length in bytes.
    pub size: u64,
    /// Oldest acpilot that can load it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_acpilot: Option<String>,
    /// Where the archive is downloaded from.
    pub download: String,
}

impl IndexEntry {
    /// `id@version`, for reports.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }

    /// The digest part of [`IndexEntry::checksum`].
    #[must_use]
    pub fn digest(&self) -> &str {
        self.checksum
            .strip_prefix("sha256:")
            .unwrap_or(&self.checksum)
    }

    /// Whether `current` is new enough to load this extension.
    ///
    /// A missing `min_acpilot` means the extension never asked; a malformed one
    /// is refused when the index is parsed, so this cannot silently pass.
    #[must_use]
    pub fn requires(&self, current: &str) -> bool {
        let Some(required) = self.min_acpilot.as_deref() else {
            return true;
        };
        version_tuple(current) >= version_tuple(required)
    }
}

/// The whole marketplace: one schema version and every listed version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    /// `1` today.
    pub schema_version: u32,
    /// Every listed version of every extension.
    #[serde(default)]
    pub extensions: Vec<IndexEntry>,
}

impl Index {
    /// Parse and validate an index.
    ///
    /// `path` is what errors name: the URL it came from or the file it was read
    /// from. Unknown keys are refused, for the same reason a manifest refuses
    /// them: a listing this build does not understand must fail loudly rather
    /// than drop the field a human is relying on.
    pub fn parse(path: &str, bytes: &[u8]) -> Result<Self, ExtensionError> {
        let index: Self = serde_json::from_slice(bytes).map_err(|error| ExtensionError::Index {
            path: path.to_owned(),
            message: format!("not a registry index: {error}"),
        })?;
        index.validate(path)?;
        Ok(index)
    }

    /// Every rule a listing has to keep.
    fn validate(&self, path: &str) -> Result<(), ExtensionError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ExtensionError::Index {
                path: path.to_owned(),
                message: format!(
                    "schema_version {} is not supported (this build speaks {SCHEMA_VERSION})",
                    self.schema_version
                ),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for entry in &self.extensions {
            validate_namespaced_id("extension id", &entry.id)?;
            validate_segment("publisher", &entry.publisher)?;
            if !is_version(&entry.version) {
                return Err(ExtensionError::Index {
                    path: path.to_owned(),
                    message: format!(
                        "{}: version {:?} is not MAJOR.MINOR.PATCH",
                        entry.id, entry.version
                    ),
                });
            }
            if entry.checksum.len() != 71
                || !entry.checksum.starts_with("sha256:")
                || !entry.checksum[7..]
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
            {
                return Err(ExtensionError::Index {
                    path: path.to_owned(),
                    message: format!(
                        "{}: checksum {:?} is not `sha256:<64 hex characters>`",
                        entry.label(),
                        entry.checksum
                    ),
                });
            }
            if entry.size == 0 {
                return Err(ExtensionError::Index {
                    path: path.to_owned(),
                    message: format!("{}: size may not be zero", entry.label()),
                });
            }
            if !is_download(&entry.download) {
                return Err(ExtensionError::Index {
                    path: path.to_owned(),
                    message: format!(
                        "{}: download {:?} is not an http(s) or file URL",
                        entry.label(),
                        entry.download
                    ),
                });
            }
            if let Some(required) = &entry.min_acpilot
                && !is_version(required)
            {
                return Err(ExtensionError::Index {
                    path: path.to_owned(),
                    message: format!(
                        "{}: min_acpilot {required:?} is not MAJOR.MINOR.PATCH",
                        entry.label()
                    ),
                });
            }
            if !seen.insert((entry.id.clone(), entry.version.clone())) {
                return Err(ExtensionError::Index {
                    path: path.to_owned(),
                    message: format!("{} is listed twice", entry.label()),
                });
            }
        }
        Ok(())
    }

    /// The newest listed version of an id.
    #[must_use]
    pub fn newest(&self, id: &str) -> Option<&IndexEntry> {
        self.extensions
            .iter()
            .filter(|entry| entry.id == id)
            .max_by(|a, b| version_tuple(&a.version).cmp(&version_tuple(&b.version)))
    }

    /// One exact version, when the index lists it.
    #[must_use]
    pub fn entry(&self, id: &str, version: &str) -> Option<&IndexEntry> {
        self.extensions
            .iter()
            .find(|entry| entry.id == id && entry.version == version)
    }

    /// Every listed version of an id, oldest first.
    #[must_use]
    pub fn versions(&self, id: &str) -> Vec<&IndexEntry> {
        let mut entries: Vec<&IndexEntry> = self
            .extensions
            .iter()
            .filter(|entry| entry.id == id)
            .collect();
        entries.sort_by_key(|entry| version_tuple(&entry.version));
        entries
    }

    /// The newest version of every id, filtered by a case-insensitive substring
    /// of the id or publisher. An empty query lists everything.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<&IndexEntry> {
        let query = query.trim().to_ascii_lowercase();
        let mut ids: Vec<&str> = self
            .extensions
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids.into_iter()
            .filter_map(|id| self.newest(id))
            .filter(|entry| {
                query.is_empty()
                    || entry.id.to_ascii_lowercase().contains(&query)
                    || entry.publisher.to_ascii_lowercase().contains(&query)
            })
            .collect()
    }
}

/// A key an index may be signed by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedKey {
    /// Short name the signature file names, for example `acpilot-release`.
    pub id: String,
    /// 64 hex characters of an ed25519 public key.
    pub public_key: String,
}

impl TrustedKey {
    /// Parse the key's bytes.
    ///
    /// # Errors
    ///
    /// The id is not a lowercase segment, or the key is not 64 hex characters
    /// encoding a point ed25519 accepts.
    pub fn verify_key(&self) -> Result<VerifyingKey, ExtensionError> {
        validate_segment("key id", &self.id)?;
        parse_public_key(&self.public_key).map_err(|message| ExtensionError::Index {
            path: format!("key {:?}", self.id),
            message,
        })
    }
}

/// The maintainer's key, as a trust root.
///
/// # Errors
///
/// The compiled-in key does not parse, which is a build defect rather than
/// something a person can fix.
pub fn maintainer_key() -> Result<TrustedKey, ExtensionError> {
    let key = TrustedKey {
        id: MAINTAINER_KEY_ID.to_owned(),
        public_key: MAINTAINER_PUBLIC_KEY.to_owned(),
    };
    key.verify_key()?;
    Ok(key)
}

/// The detached signature published beside an index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetachedSignature {
    /// Which trusted key signed the index.
    pub key: String,
    /// 128 hex characters: the 64-byte ed25519 signature.
    pub signature: String,
}

/// Check that `detached` is a trusted key's signature over `bytes`.
///
/// Returns the id of the key that signed it, which is the fact a report wants to
/// print: "signed by acpilot-release" is actionable, "signature ok" is not.
///
/// # Errors
///
/// The signature file is unreadable or names a key this build does not trust, or
/// the bytes are not what that key signed. The key's id is named in every case.
pub fn verify(
    path: &str,
    bytes: &[u8],
    detached: &[u8],
    trusted: &[TrustedKey],
) -> Result<String, ExtensionError> {
    let signature: DetachedSignature =
        serde_json::from_slice(detached).map_err(|error| ExtensionError::Index {
            path: path.to_owned(),
            message: format!("the detached signature is not JSON: {error}"),
        })?;

    let maintainer = maintainer_key()?;
    let mut keys = vec![maintainer];
    for key in trusted {
        if key.id == MAINTAINER_KEY_ID {
            if key.public_key.eq_ignore_ascii_case(MAINTAINER_PUBLIC_KEY) {
                continue;
            }
            return Err(ExtensionError::Index {
                path: path.to_owned(),
                message: format!(
                    "registry.trusted_keys names {:?} with a different key than the one built into \
                     acpilot; the maintainer's key cannot be replaced, only added to",
                    key.id
                ),
            });
        }
        if keys.iter().any(|known| known.id == key.id) {
            return Err(ExtensionError::Index {
                path: path.to_owned(),
                message: format!(
                    "registry.trusted_keys names {:?} twice; one id, one key",
                    key.id
                ),
            });
        }
        keys.push(key.clone());
    }

    let Some(trusted_key) = keys.iter().find(|key| key.id == signature.key) else {
        let known: Vec<&str> = keys.iter().map(|key| key.id.as_str()).collect();
        return Err(ExtensionError::Index {
            path: path.to_owned(),
            message: format!(
                "the index is signed by {:?}, and this build trusts {}; add that key's public half \
                 to registry.trusted_keys",
                signature.key,
                known.join(", ")
            ),
        });
    };

    let key = trusted_key.verify_key()?;
    let bytes_of_signature =
        decode_hex(&signature.signature, 64).map_err(|message| ExtensionError::Index {
            path: path.to_owned(),
            message: format!(
                "the signature from {:?} is unusable: {message}",
                signature.key
            ),
        })?;
    let mut signature_bytes = [0u8; 64];
    signature_bytes.copy_from_slice(&bytes_of_signature);
    key.verify_strict(bytes, &Signature::from_bytes(&signature_bytes))
        .map_err(|_| ExtensionError::Index {
            path: path.to_owned(),
            message: format!(
                "the index is not signed by {:?}; it was edited after it was signed, or it came \
                 from somewhere else",
                signature.key
            ),
        })?;
    Ok(signature.key)
}

/// Sign bytes the way an index is published. Tests only: a program that could
/// sign an index at runtime would need the maintainer's private key beside it.
#[must_use]
pub fn sign(signing_key: &SigningKey, bytes: &[u8]) -> Signature {
    use ed25519_dalek::Signer;
    signing_key.sign(bytes)
}

/// A 32-byte ed25519 public key written as 64 hex characters.
fn parse_public_key(hex: &str) -> Result<VerifyingKey, String> {
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
        let byte = u8::from_str_radix(pair, 16).map_err(|error| format!("not hex: {error}"))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// Hex, lowercase, for a signature file.
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The listings' word for a kind, as `index.json` writes it.
#[must_use]
pub fn kind_name(kind: ExtensionKind) -> &'static str {
    match kind {
        ExtensionKind::Theme => "theme",
        ExtensionKind::LanguageServers => "language_servers",
        ExtensionKind::Agents => "agents",
        ExtensionKind::Skills => "skills",
        ExtensionKind::Grammars => "grammars",
        ExtensionKind::Snippets => "snippets",
        ExtensionKind::Keymap => "keymap",
        ExtensionKind::TranscriptParser => "transcript_parser",
    }
}

/// Whether a download address is one this command speaks.
fn is_download(download: &str) -> bool {
    download.starts_with("https://")
        || download.starts_with("http://")
        || download.starts_with("file://")
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;

    fn signing_key(seed: u8) -> (SigningKey, TrustedKey) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let key = TrustedKey {
            id: "test-key".to_owned(),
            public_key: encode_hex(&signing.verifying_key().to_bytes()),
        };
        (signing, key)
    }

    fn signed(index: &str, key_id: &str, signing: &SigningKey) -> Vec<u8> {
        let signature = sign(signing, index.as_bytes());
        serde_json::to_vec(&DetachedSignature {
            key: key_id.to_owned(),
            signature: encode_hex(&signature.to_bytes()),
        })
        .expect("a signature file")
    }

    const ONE: &str = r#"{
        "schema_version": 1,
        "extensions": [{
            "id": "acpilot.night-flight",
            "version": "1.0.0",
            "kind": "theme",
            "publisher": "acpilot",
            "checksum": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "size": 1024,
            "download": "https://example.invalid/night-flight.tar.gz"
        }]
    }"#;

    #[test]
    fn a_signed_index_verifies_and_names_its_key() {
        let (signing, key) = signing_key(1);
        let bytes = ONE.as_bytes();
        let signature = signed(ONE, "test-key", &signing);
        let id = verify("index.json", bytes, &signature, &[key]).expect("the signature verifies");
        assert_eq!(id, "test-key");
    }

    #[test]
    fn a_tampered_index_is_refused() {
        let (signing, key) = signing_key(2);
        let signature = signed(ONE, "test-key", &signing);
        let tampered = ONE.replace("1024", "1025");
        let error =
            verify("index.json", tampered.as_bytes(), &signature, &[key]).expect_err("refused");
        assert!(error.to_string().contains("is not signed by"), "{error}");
        assert!(error.to_string().contains("test-key"), "{error}");
    }

    #[test]
    fn an_unknown_key_is_refused_by_the_name_it_gives() {
        let (signing, key) = signing_key(3);
        let signature = signed(ONE, "someone-else", &signing);
        let error = verify("index.json", ONE.as_bytes(), &signature, &[key]).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("someone-else"), "{message}");
        assert!(message.contains("registry.trusted_keys"), "{message}");
    }

    #[test]
    fn the_maintainer_key_cannot_be_replaced_from_settings() {
        let (signing, key) = signing_key(4);
        let shadow = TrustedKey {
            id: MAINTAINER_KEY_ID.to_owned(),
            public_key: key.public_key,
        };
        let signature = signed(ONE, MAINTAINER_KEY_ID, &signing);
        let error = verify("index.json", ONE.as_bytes(), &signature, &[shadow])
            .expect_err("a shadowed maintainer key is refused");
        assert!(error.to_string().contains("cannot be replaced"), "{error}");
    }

    #[test]
    fn the_shipped_index_is_signed_by_the_maintainer() {
        let id = verify(INDEX_FILE, DEFAULT_INDEX, DEFAULT_SIGNATURE, &[])
            .expect("marketplace/index.json is signed by the built-in key");
        assert_eq!(id, MAINTAINER_KEY_ID);
        Index::parse(INDEX_FILE, DEFAULT_INDEX).expect("the shipped index parses");
    }

    #[test]
    fn a_listing_missing_a_field_is_not_silently_accepted() {
        let missing = ONE.replace(r#""size": 1024,"#, "");
        let error = Index::parse("index.json", missing.as_bytes()).expect_err("refused");
        assert!(error.to_string().contains("size"), "{error}");
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let extra = ONE.replace(r#""size": 1024,"#, r#""size": 1024, "signature": "x","#);
        let error = Index::parse("index.json", extra.as_bytes()).expect_err("refused");
        assert!(error.to_string().contains("signature"), "{error}");
    }

    #[test]
    fn a_bad_checksum_is_refused_before_a_download() {
        let bad = ONE.replace("sha256:0000", "sha256:zzzz");
        let error = Index::parse("index.json", bad.as_bytes()).expect_err("refused");
        assert!(error.to_string().contains("checksum"), "{error}");
    }

    #[test]
    fn the_newest_version_is_the_one_a_bare_id_means() {
        let two = ONE.replace(
            r#""extensions": [{"#,
            r#""extensions": [{
                "id": "acpilot.night-flight",
                "version": "0.9.0",
                "kind": "theme",
                "publisher": "acpilot",
                "checksum": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "size": 1024,
                "download": "https://example.invalid/old.tar.gz"
            }, {"#,
        );
        let index = Index::parse("index.json", two.as_bytes()).expect("parses");
        assert_eq!(
            index.newest("acpilot.night-flight").unwrap().version,
            "1.0.0"
        );
        assert_eq!(index.versions("acpilot.night-flight").len(), 2);
        assert_eq!(index.search("night").len(), 1);
        assert!(index.search("nothing").is_empty());
    }

    #[test]
    fn min_acpilot_is_compared_as_versions() {
        let entry = IndexEntry {
            id: "acpilot.night-flight".to_owned(),
            version: "1.0.0".to_owned(),
            kind: ExtensionKind::Theme,
            publisher: "acpilot".to_owned(),
            checksum: format!("sha256:{}", "0".repeat(64)),
            size: 1,
            min_acpilot: Some("0.2.0".to_owned()),
            download: "https://example.invalid/x".to_owned(),
        };
        assert!(entry.requires("0.2.0"));
        assert!(entry.requires("0.10.0"));
        assert!(!entry.requires("0.1.9"));
    }
}
