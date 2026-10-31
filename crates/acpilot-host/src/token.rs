//! The token a TCP listener requires: where it lives, and how to replace it.
//!
//! Here rather than in the daemon because three things need to agree about it: the
//! daemon that requires it, the CLI that offers to rotate it, and the docs that
//! tell a person where to look. A path spelled in two crates is a rotation that
//! half-applies.
//!
//! The file is `0600` inside the `0700` runtime directory, generated on first use
//! and reused afterwards. It is not hashed: hashing protects a secret stored where
//! someone else can read it, and this one is readable only by its owner — a hash
//! would add a second secret that needs the same protection and give it none. What
//! the file buys is that the token survives a restart, so a client does not have to
//! be told a new one.
//!
//! Rotation is therefore just a rewrite: the daemon reads the file per request
//! rather than at startup, so a new token takes effect on the next call and nothing
//! restarts. That is the point — the thing a rotation is for is a token that may
//! have leaked, and a rotation that needs an outage is one nobody performs.

use std::io;
use std::path::{Path, PathBuf};

/// Where the token lives: beside the socket, in the runtime directory.
///
/// Runtime state, not configuration: the daemon creates it on first use, and a
/// reboot is allowed to take it. Written here so the daemon that writes the file,
/// the CLI that rotates it and the client that is offered its contents cannot
/// spell the path three ways.
#[must_use]
pub fn path() -> PathBuf {
    crate::runtime_dir().join("token")
}

/// The token in `path`, or `None` when there is none to read.
///
/// A missing file and an empty file are the same answer: nothing is accepted, and
/// the caller does not need to tell them apart to decide that.
///
/// # Errors
///
/// The read failure, unless it is "not found" — a file that exists and cannot be
/// read is a permissions problem the caller should hear about.
pub fn read(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let token = text.trim();
            Ok((!token.is_empty()).then(|| token.to_owned()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The token in `path`, creating it when this is the first TCP daemon here.
///
/// # Errors
///
/// The directory or file could not be created or made private, or the system
/// random source refused.
pub fn load_or_create(path: &Path) -> io::Result<String> {
    if let Some(token) = read(path)? {
        return Ok(token);
    }
    write(path)
}

/// Replace the token in `path`, and return the new one.
///
/// The caller is a person who thinks the old token is no longer a secret. Every
/// client holding the old one is refused from the next request on, which is the
/// whole intent.
///
/// # Errors
///
/// The directory or file could not be created or made private, or the system
/// random source refused.
pub fn rotate(path: &Path) -> io::Result<String> {
    write(path)
}

/// Put a fresh token in `path`.
fn write(path: &Path) -> io::Result<String> {
    if let Some(directory) = path.parent()
        && !directory.as_os_str().is_empty()
    {
        std::fs::create_dir_all(directory)?;
        crate::private_dir(directory)?;
    }
    let token = random()?;
    // Written first, narrowed second: the directory is already `0700`, so the
    // window between the two is not one another user can use.
    std::fs::write(path, format!("{token}\n"))?;
    crate::private_file(path)?;
    Ok(token)
}

/// A fresh token that is not written anywhere.
///
/// For a listener that lives no longer than one client: an embedded daemon
/// (P6-11) has no runtime directory to share a secret through, and a token on
/// disk would outlive the only process it guarded. Everything else should use
/// [`load_or_create`], which is what makes a rotation possible at all.
///
/// # Errors
///
/// The system's random source refused.
pub fn generate() -> io::Result<String> {
    random()
}

/// 32 bytes from the system's random source, as hex.
///
/// 256 bits because a token has no rate limit of its own: whatever can reach the
/// port can guess, and the only defence is that guessing is hopeless.
fn random() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Compare without stopping at the first differing byte.
///
/// The timing signal of a byte-by-byte comparison is small, and over a network
/// with noise it is very small — but "very small" is the answer that gets written
/// into an incident report rather than the one that prevents it.
#[must_use]
pub fn same(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    presented
        .iter()
        .zip(expected)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_token_is_256_bits_of_hex() {
        let token = generate().expect("random source");
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|character| character.is_ascii_hexdigit()));
        assert_ne!(token, generate().expect("random source"));
    }

    #[test]
    fn the_comparison_is_exact() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
        assert!(!same("", "abc"));
    }

    #[test]
    fn an_existing_token_is_reused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("token");
        std::fs::write(&path, "  from-before  \n").expect("write");
        assert_eq!(load_or_create(&path).expect("loaded"), "from-before");
    }

    #[test]
    fn a_rotation_replaces_what_a_reader_sees_next() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("token");
        let first = load_or_create(&path).expect("created");
        let second = rotate(&path).expect("rotated");
        assert_ne!(first, second);
        assert_eq!(read(&path).expect("read"), Some(second));
    }

    #[test]
    fn a_missing_file_is_no_token_rather_than_an_error() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("nothing-here");
        assert_eq!(read(&path).expect("missing is fine"), None);
    }
}
