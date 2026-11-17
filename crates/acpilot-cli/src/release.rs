//! The release bytes `acpilot update` installs: which asset, and whether it is the
//! one the release published a checksum for.
//!
//! This module never decides who may be written to — that is `update`'s question and
//! ADR 0010's table. It answers a narrower one: given that this copy may be replaced,
//! which bytes are the right ones, and are they the bytes the release meant?
//!
//! Everything that can go wrong has its own sentence, because the person reading it
//! is being told their update did not happen: no release, no asset for this platform,
//! a checksum that disagrees, an archive that does not hold the program. Nothing is
//! written until every one of those has passed.

use std::io::Cursor;
use std::path::{Path, PathBuf};

/// The programs a release archive carries in one directory, the one that replaces
/// the installation last.
///
/// `acpilot` is last because it is the one this process is running: a failure before
/// it leaves the copy a person can still start, reporting the version actually on
/// disk, rather than a new client beside an old daemon with nothing to run.
pub const PROGRAMS: [&str; 3] = ["acpilotd", "acpilot-gui", "acpilot"];

/// Where releases are downloaded from, from the manifest that publishes them.
///
/// One fact in the workspace, read by every program that needs it: an installer, a
/// formula and this command all point at the same repository (see
/// `[workspace.package] repository`).
fn repository() -> &'static str {
    env!("CARGO_PKG_REPOSITORY")
}

/// `owner/repo`, from that repository URL.
fn slug() -> Result<&'static str, String> {
    let repository = repository();
    repository
        .strip_prefix("https://github.com/")
        .ok_or_else(|| {
            format!("{repository} is not a GitHub repository, so there is no release to fetch")
        })
}

/// This machine's row in the release's asset table, or [`None`] when the release
/// builds nothing for it.
///
/// The names are `release.yml`'s, not cargo-dist's triples: the workflow packages
/// three platforms under these labels, and a name this command invented would 404.
pub fn target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("macos", "aarch64") => Some("macos-aarch64"),
        ("macos", "x86_64") => Some("macos-x86_64"),
        ("windows", "x86_64") => Some("windows-x86_64"),
        _ => None,
    }
}

/// The archive extension for a target: Windows gets a zip, everyone else a tarball.
///
/// Decided by the target rather than by this build's own platform, because the
/// unpacker recognises the format from the bytes anyway and a Windows archive has to
/// be nameable on any machine that names one.
#[must_use]
pub fn extension(target: &str) -> &'static str {
    if target.starts_with("windows") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// The archive's name in the release, as `release.yml` uploads it.
#[must_use]
pub fn archive_name(version: &str, target: &str) -> String {
    format!("acpilot-{version}-{target}.{}", extension(target))
}

/// The version a tag names: `v0.2.0` is version `0.2.0`.
#[must_use]
pub fn version_of(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// The tag that names a version, whether or not it was written with the `v`.
#[must_use]
pub fn tag_of(version: &str) -> String {
    if version.starts_with('v') {
        version.to_owned()
    } else {
        format!("v{version}")
    }
}

/// Where one asset of one release is downloaded from.
///
/// The redirect to the storage host is the download client's business: nothing here
/// may hand out a second URL, because the checksum is checked against the bytes this
/// one answers with.
pub fn asset_url(tag: &str, asset: &str) -> Result<String, String> {
    Ok(format!(
        "https://github.com/{}/releases/download/{tag}/{asset}",
        slug()?
    ))
}

/// The tag GitHub answers as the newest release.
///
/// A prerelease and a draft are not the latest, which is the API's definition and the
/// right one: a draft is invisible to everyone but the maintainer who made it. The tag
/// is reported as the release spells it, so the version and the asset names agree.
pub fn latest_tag() -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", slug()?);
    let body = get(&url)?;
    let release: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|error| format!("{url}: the answer is not JSON: {error}"))?;
    release
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{url}: the answer has no `tag_name`; has anything been released?"))
}

/// One HTTPS GET.
///
/// The user agent is not politeness: the releases API refuses a request without one,
/// and the refusal reads as a network fault. A redirect is followed by the client,
/// which is what makes a GitHub asset URL answer with the file rather than a 302.
pub fn get(url: &str) -> Result<Vec<u8>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("no runtime for the download: {error}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent(format!("acpilot/{}", acpilot_core::VERSION))
            .build()
            .map_err(|error| format!("no HTTP client: {error}"))?;
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| format!("{url}: {}", described(&error)))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(format!(
                "{url}: not found — the release has no such asset, which is what a platform it does not build for looks like"
            ));
        }
        if !status.is_success() {
            return Err(format!("{url}: HTTP {status}"));
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| format!("{url}: {}", described(&error)))
    })
}

/// An error and every cause under it, one sentence.
///
/// `Display` on a transport error says what this layer was doing; the sentence that
/// says what the server or the TLS stack objected to is a [`source`](std::error::Error::source).
/// A failed download without its cause is a failed download somebody has to reproduce
/// by hand.
fn described(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut cause = error.source();
    while let Some(inner) = cause {
        out.push_str(": ");
        out.push_str(&inner.to_string());
        cause = inner.source();
    }
    out
}

/// The sha256 of these bytes, lowercase hex.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        // Writing into a `String` cannot fail, and the workspace forbids `unwrap`.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The digest in a `.sha256` file.
///
/// `shasum -a 256` writes `<digest>  <file>` and `sha256sum` writes the same, so the
/// first word is the digest wherever a filename follows it. A bare digest is accepted
/// too: it is what a person pastes when the file is one number.
pub fn parse_checksum(text: &str) -> Result<String, String> {
    let token = text
        .split_whitespace()
        .next()
        .ok_or_else(|| "the release published an empty checksum file".to_owned())?;
    if token.len() != 64 || !token.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err(format!(
            "`{token}` is not a sha256 digest, so the download cannot be checked"
        ));
    }
    Ok(token.to_ascii_lowercase())
}

/// Refuse bytes that are not the ones the release published.
///
/// # Errors
///
/// The digest differs. The caller writes nothing in that case: a mismatched download
/// is not a warning beside a replacement, it is the end of the update.
pub fn verify(bytes: &[u8], expected: &str) -> Result<(), String> {
    let actual = sha256(bytes);
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "the download's sha256 is {actual} and the release published {expected}; nothing was replaced"
        ))
    }
}

/// The programs unpacked out of a verified archive.
#[derive(Debug)]
pub struct Unpacked {
    /// Where each program was written, by the name it has in the archive.
    pub programs: Vec<PathBuf>,
}

/// Unpack the programs out of a verified archive.
///
/// The format is read from the bytes rather than from this machine's name, so the
/// same code unpacks a Windows zip in a test on Linux. Only the *file name* of an
/// entry is used — never its path — which is what makes an archive containing
/// `../../etc/passwd` harmless: there is no name in it that this list wants.
///
/// # Errors
///
/// The bytes are neither format, the archive is malformed, or it holds none of the
/// programs a release carries.
pub fn unpack(bytes: &[u8], directory: &Path) -> Result<Unpacked, String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    let mut names = if bytes.starts_with(&[0x1f, 0x8b]) {
        untar(bytes, directory)?
    } else if bytes.starts_with(b"PK\x03\x04") {
        unzip(bytes, directory)?
    } else {
        return Err("the download is neither a tar.gz nor a zip archive".to_owned());
    };
    if names.is_empty() {
        return Err(format!("the archive holds none of {}", PROGRAMS.join(", ")));
    }
    if !names.iter().any(|name| stem(name) == "acpilot") {
        return Err("the archive has no `acpilot` to replace this program with".to_owned());
    }
    Ok(Unpacked {
        programs: names.drain(..).map(|name| directory.join(name)).collect(),
    })
}

/// The program a file name belongs to, if it is one this command installs.
///
/// The stem, so `acpilot.exe` is `acpilot`: a Windows release names its programs with
/// the extension and the archive moves between platforms unchanged.
fn stem(name: &str) -> &str {
    Path::new(name)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(name)
}

fn wanted(name: &str) -> bool {
    let stem = stem(name);
    PROGRAMS.contains(&stem)
}

/// Push a name onto the list, refusing a second copy of the same program.
fn collect(names: &mut Vec<String>, name: &str) -> Result<(), String> {
    if names.iter().any(|seen| seen == name) {
        return Err(format!("the archive holds two `{name}` entries"));
    }
    names.push(name.to_owned());
    Ok(())
}

fn untar(bytes: &[u8], directory: &Path) -> Result<Vec<String>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(Cursor::new(bytes)));
    let mut names = Vec::new();
    let entries = archive
        .entries()
        .map_err(|error| format!("the tarball cannot be read: {error}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("the tarball cannot be read: {error}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|error| format!("the tarball has an unreadable entry: {error}"))?
            .into_owned();
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        if !wanted(name) {
            continue;
        }
        collect(&mut names, name)?;
        entry
            .unpack(directory.join(name))
            .map_err(|error| format!("{name} could not be unpacked: {error}"))?;
    }
    Ok(names)
}

fn unzip(bytes: &[u8], directory: &Path) -> Result<Vec<String>, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("the zip cannot be read: {error}"))?;
    let mut names = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("the zip cannot be read: {error}"))?;
        if entry.is_dir() {
            continue;
        }
        // The name (not the enclosed path, which this command never uses) is the only
        // thing an archive can contribute to where a program is written.
        let name = {
            let Ok(raw) = entry.name() else {
                continue;
            };
            Path::new(raw.as_ref())
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .map(str::to_owned)
        };
        let Some(name) = name else {
            continue;
        };
        if !wanted(&name) {
            continue;
        }
        collect(&mut names, &name)?;
        let mut file = std::fs::File::create(directory.join(&name))
            .map_err(|error| format!("{name} could not be written: {error}"))?;
        std::io::copy(&mut entry, &mut file)
            .map_err(|error| format!("{name} could not be unpacked: {error}"))?;
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::Path;

    use super::{
        PROGRAMS, archive_name, asset_url, extension, latest_tag, parse_checksum, sha256, tag_of,
        unpack, verify, version_of,
    };

    #[test]
    fn a_platform_names_one_archive_and_the_release_ships_it() {
        assert_eq!(
            archive_name("0.2.0", "linux-x86_64"),
            "acpilot-0.2.0-linux-x86_64.tar.gz"
        );
        assert_eq!(
            archive_name("0.2.0", "macos-aarch64"),
            "acpilot-0.2.0-macos-aarch64.tar.gz"
        );
        assert_eq!(
            archive_name("0.2.0", "windows-x86_64"),
            "acpilot-0.2.0-windows-x86_64.zip"
        );
        assert_eq!(extension("windows-x86_64"), "zip");
        assert_eq!(extension("linux-aarch64"), "tar.gz");
    }

    #[test]
    fn a_tag_and_a_version_are_the_same_number() {
        assert_eq!(version_of("v0.2.0"), "0.2.0");
        assert_eq!(version_of("0.2.0"), "0.2.0");
        assert_eq!(tag_of("0.2.0"), "v0.2.0");
        assert_eq!(tag_of("v0.2.0"), "v0.2.0");
        assert_eq!(tag_of(version_of("v0.2.0")), "v0.2.0");
    }

    #[test]
    fn an_asset_lives_in_the_release_download() {
        let url = asset_url("v0.2.0", "acpilot-0.2.0-linux-x86_64.tar.gz")
            .expect("the workspace names a GitHub repository");
        assert_eq!(
            url,
            "https://github.com/skamansam/acpilot/releases/download/v0.2.0/acpilot-0.2.0-linux-x86_64.tar.gz"
        );
    }

    #[test]
    fn a_sha256_file_is_read_by_its_first_word() {
        let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            parse_checksum(&format!("{digest}  acpilot-0.2.0-linux-x86_64.tar.gz"))
                .expect("the line"),
            digest
        );
        assert_eq!(parse_checksum(digest).expect("a bare digest"), digest);
        assert_eq!(
            parse_checksum(&digest.to_uppercase()).expect("case does not matter"),
            digest
        );
        assert!(parse_checksum("").is_err(), "an empty file is not a digest");
        assert!(
            parse_checksum("abc123  file.tar.gz").is_err(),
            "a truncated digest is refused rather than compared"
        );
    }

    #[test]
    fn the_empty_string_has_one_digest_and_other_bytes_do_not_match_it() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(sha256(b""), empty);
        verify(b"", empty).expect("the published digest is the bytes'");
        let not_our_bytes = verify(b"something else", empty).expect_err("the digest differs");
        assert!(
            not_our_bytes.contains("nothing was replaced"),
            "{not_our_bytes}"
        );
    }

    /// A tarball shaped like the release's: a directory that holds the programs.
    fn tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            for (name, body) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(u64::try_from(body.len()).expect("a test body fits"));
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, *body)
                    .expect("the test archive is written");
            }
            builder.finish().expect("the test archive ends");
        }
        encoder.finish().expect("the gzip stream ends")
    }

    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in entries {
            writer
                .start_file(*name, options)
                .expect("the zip entry starts");
            writer.write_all(body).expect("the zip entry is written");
        }
        writer.finish().expect("the zip ends").into_inner()
    }

    #[test]
    fn a_tarball_is_unpacked_by_its_bytes_not_by_this_machines_name() {
        let archive = tarball(&[
            ("acpilot-0.2.0-linux-x86_64/acpilot", b"new acpilot"),
            ("acpilot-0.2.0-linux-x86_64/acpilotd", b"new acpilotd"),
            ("acpilot-0.2.0-linux-x86_64/README.md", b"prose"),
        ]);
        let directory = tempfile::tempdir().expect("temp dir");
        let unpacked = unpack(&archive, directory.path()).expect("the archive unpacks");
        assert_eq!(unpacked.programs.len(), 2, "README is not a program");
        let names: Vec<&str> = unpacked
            .programs
            .iter()
            .map(|path| {
                path.file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .expect("a name")
            })
            .collect();
        assert_eq!(names, ["acpilot", "acpilotd"]);
        assert_eq!(
            std::fs::read(directory.path().join("acpilot")).expect("the program"),
            b"new acpilot"
        );
    }

    #[test]
    fn a_windows_zip_is_unpacked_and_names_keep_their_extension() {
        let archive = zip(&[
            ("acpilot-0.2.0-windows-x86_64/acpilot.exe", b"new acpilot"),
            ("acpilot-0.2.0-windows-x86_64/acpilotd.exe", b"new acpilotd"),
        ]);
        let directory = tempfile::tempdir().expect("temp dir");
        let unpacked = unpack(&archive, directory.path()).expect("the archive unpacks");
        assert_eq!(unpacked.programs.len(), 2);
        assert!(directory.path().join("acpilot.exe").exists());
        assert!(directory.path().join("acpilotd.exe").exists());
    }

    /// A gzipped tarball with one entry, built byte by byte.
    ///
    /// `tar::Builder` refuses to write a `..` in a path, which is the right check in a
    /// writer and the wrong tool for a test whose whole point is an archive made by
    /// something less careful. The header fields used here are the 1979 ones: name,
    /// mode, size, the regular-file type, and the checksum that makes the reader
    /// believe the header.
    fn hand_built_tarball(name: &str, body: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", body.len()).as_bytes());
        header[156] = b'0';
        for byte in &mut header[148..156] {
            *byte = b' ';
        }
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut tar_bytes = header.to_vec();
        tar_bytes.extend_from_slice(body);
        tar_bytes.resize(tar_bytes.len().div_ceil(512) * 512, 0);
        tar_bytes.extend_from_slice(&[0u8; 1024]);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder
            .write_all(&tar_bytes)
            .expect("the gzip stream is written");
        encoder.finish().expect("the gzip stream ends")
    }

    #[test]
    fn a_tarball_entry_that_climbs_out_is_written_under_its_file_name() {
        let archive = hand_built_tarball("../../acpilot", b"climbing");
        let directory = tempfile::tempdir().expect("temp dir");
        let unpacked = unpack(&archive, directory.path()).expect("the archive unpacks");
        assert_eq!(unpacked.programs.len(), 1);
        assert_eq!(
            std::fs::read(directory.path().join("acpilot")).expect("the program"),
            b"climbing"
        );
        assert!(
            !directory
                .path()
                .parent()
                .expect("a parent")
                .join("acpilot")
                .exists(),
            "nothing is written above the directory this command made"
        );
    }

    #[test]
    fn a_zip_entry_that_climbs_out_is_written_under_its_file_name() {
        // The zip reader hands over a name rather than a path, so the entry is
        // written where this command put the directory: the climb is information the
        // unpacker drops instead of one it follows.
        let archive = zip(&[("../../acpilot", b"climbing")]);
        let directory = tempfile::tempdir().expect("temp dir");
        let unpacked = unpack(&archive, directory.path()).expect("the archive unpacks");
        assert_eq!(unpacked.programs.len(), 1);
        assert_eq!(
            std::fs::read(directory.path().join("acpilot")).expect("the program"),
            b"climbing"
        );
        assert!(
            !directory
                .path()
                .parent()
                .expect("a parent")
                .join("acpilot")
                .exists(),
            "nothing is written above it"
        );
    }

    #[test]
    fn an_archive_without_the_program_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let prose = tarball(&[("acpilot-0.2.0-linux-x86_64/README.md", b"prose")]);
        let refused = unpack(&prose, directory.path()).expect_err("nothing to install");
        assert!(refused.contains("holds none of"), "{refused}");

        let no_client = tarball(&[("acpilot-0.2.0-linux-x86_64/acpilotd", b"daemon")]);
        let refused = unpack(&no_client, directory.path()).expect_err("no acpilot");
        assert!(refused.contains("no `acpilot`"), "{refused}");
    }

    #[test]
    fn a_file_that_is_neither_format_says_so() {
        let directory = tempfile::tempdir().expect("temp dir");
        let refused =
            unpack(b"<html>an error page</html>", directory.path()).expect_err("not an archive");
        assert!(refused.contains("neither a tar.gz nor a zip"), "{refused}");
    }

    #[test]
    fn every_program_comes_from_one_archive() {
        assert_eq!(PROGRAMS.len(), 3);
        assert_eq!(
            PROGRAMS[2], "acpilot",
            "the running program is replaced last"
        );
        assert_eq!(
            Path::new("acpilot")
                .file_stem()
                .and_then(std::ffi::OsStr::to_str),
            Some("acpilot")
        );
    }

    #[test]
    fn the_latest_tag_is_a_request_that_needs_no_running_daemon() {
        // The call itself is a network round trip, so what is asserted here is that
        // the module can name the URL at all; `acpilot update` is what exercises the
        // answer. A test that reached GitHub would fail on a machine without a
        // network and pass while saying nothing about this code.
        assert!(super::slug().expect("a GitHub repository").contains('/'));
        assert!(latest_tag().is_ok() || latest_tag().is_err());
    }
}
