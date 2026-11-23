//! Unpacking a published extension archive.
//!
//! A registry download is bytes from the network, so the unpacker is the one
//! place a path in an archive becomes a path on disk. Every entry is checked
//! before it is written: only relative paths made of plain components, only
//! regular files and directories, and no links of either kind. An archive that
//! carries something else is refused whole rather than skipped, because
//! "installed with one file missing" is a broken extension with a checksum that
//! says it is fine.
//!
//! The format is read from the bytes — gzip magic for a tarball, `PK` for a zip —
//! so a Windows archive unpacks on Linux and the same code runs in a test
//! everywhere.

use std::io::Cursor;
use std::path::{Component, Path, PathBuf};

use crate::error::ExtensionError;

/// The most one archive may unpack to.
///
/// An extension is data: TOML presets, a theme, and later a few grammars. The
/// index's `size` bounds the *download*, which is the compressed bytes and says
/// nothing about what they expand to — a few hundred kilobytes of gzip can fill a
/// disk. This is the bound the listing cannot give, checked as entries are written
/// so a bomb stops at the limit rather than at whatever the filesystem does first.
/// It is deliberately generous: several prebuilt grammars fit, and nothing a pack
/// is allowed to carry needs more.
pub const MAX_UNPACKED_BYTES: u64 = 64 * 1024 * 1024;

/// Unpack an archive into a directory that must already exist.
///
/// The tree is capped at [`MAX_UNPACKED_BYTES`].
///
/// # Errors
///
/// The bytes are neither a gzip tarball nor a zip, an entry is not a regular file
/// or directory, an entry's path is absolute or climbs out of `destination`, or the
/// entries add up to more than the cap.
pub fn unpack(bytes: &[u8], destination: &Path) -> Result<(), ExtensionError> {
    unpack_within(bytes, destination, MAX_UNPACKED_BYTES)
}

/// [`unpack`] with the cap named, so a test can make it small enough to cross.
fn unpack_within(bytes: &[u8], destination: &Path, limit: u64) -> Result<(), ExtensionError> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        unpack_tar_gz(bytes, destination, limit)
    } else if bytes.starts_with(b"PK\x03\x04") {
        unpack_zip(bytes, destination, limit)
    } else {
        Err(ExtensionError::Archive {
            path: destination.display().to_string(),
            message: "the download is neither a gzip tarball nor a zip; nothing was installed"
                .to_owned(),
        })
    }
}

fn unpack_tar_gz(bytes: &[u8], destination: &Path, limit: u64) -> Result<(), ExtensionError> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| ArchiveError::from(error, destination))?;
    let mut written = 0_u64;
    for entry in entries {
        let mut entry = entry.map_err(|error| ArchiveError::from(error, destination))?;
        let kind = entry.header().entry_type();
        // Metadata entries the tar crate already folds into the next path. Seeing
        // one is normal; writing one would not be.
        if kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
            || kind.is_gnu_longname()
            || kind.is_gnu_longlink()
        {
            continue;
        }
        let path = entry
            .path()
            .map_err(|error| ArchiveError::from(error, destination))?
            .into_owned();
        let relative = safe_relative(&path, destination)?;
        let target = destination.join(&relative);
        if kind.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|error| ArchiveError::from(error, destination))?;
            continue;
        }
        if !kind.is_file() {
            return Err(ExtensionError::Archive {
                path: relative.display().to_string(),
                message: "only regular files and directories can be unpacked; symlinks, hard \
                          links and device nodes are refused"
                    .to_owned(),
            });
        }
        // The header's size is exactly how much `unpack` writes for a regular
        // file, so the budget is spent here rather than measured afterwards.
        written = written
            .checked_add(entry.size())
            .ok_or_else(|| too_large(&relative, limit))?;
        if written > limit {
            return Err(too_large(&relative, limit));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ArchiveError::from(error, destination))?;
        }
        entry
            .unpack(&target)
            .map_err(|error| ArchiveError::from(error, destination))?;
    }
    Ok(())
}

fn unpack_zip(bytes: &[u8], destination: &Path, limit: u64) -> Result<(), ExtensionError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| ArchiveError::from(error, destination))?;
    let mut written = 0_u64;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|error| ArchiveError::from(error, destination))?;
        let Some(path) = file.enclosed_name() else {
            let name = file
                .name()
                .map(|name| name.into_owned())
                .unwrap_or_default();
            return Err(ExtensionError::Archive {
                path: name,
                message: "the entry's path is absolute or climbs out of the archive".to_owned(),
            });
        };
        let relative = safe_relative(&path, destination)?;
        if file.is_dir() {
            std::fs::create_dir_all(destination.join(&relative))
                .map_err(|error| ArchiveError::from(error, destination))?;
            continue;
        }
        if file
            .unix_mode()
            .is_some_and(|mode| mode & 0o170_000 == 0o120_000)
        {
            return Err(ExtensionError::Archive {
                path: relative.display().to_string(),
                message: "only regular files and directories can be unpacked; symlinks are \
                          refused"
                    .to_owned(),
            });
        }
        // The central directory's uncompressed size is what the reader yields, so
        // it is the number the budget is charged, before any byte is written.
        written = written
            .checked_add(file.size())
            .ok_or_else(|| too_large(&relative, limit))?;
        if written > limit {
            return Err(too_large(&relative, limit));
        }
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ArchiveError::from(error, destination))?;
        }
        let mut writer = std::fs::File::create(&target)
            .map_err(|error| ArchiveError::from(error, destination))?;
        std::io::copy(&mut file, &mut writer)
            .map_err(|error| ArchiveError::from(error, destination))?;
    }
    Ok(())
}

/// The refusal a download gets when its entries would exceed the cap.
fn too_large(entry: &Path, limit: u64) -> ExtensionError {
    ExtensionError::Archive {
        path: entry.display().to_string(),
        message: format!(
            "the archive unpacks to more than {limit} bytes at this entry, and an extension is \
             data; nothing was installed"
        ),
    }
}

/// A path relative to the unpack root, or the reason it is not.
fn safe_relative(path: &Path, destination: &Path) -> Result<PathBuf, ExtensionError> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => {}
            _ => {
                return Err(ExtensionError::Archive {
                    path: format!("{}/{}", destination.display(), path.display()),
                    message: "the entry's path is absolute or climbs out of the archive".to_owned(),
                });
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(ExtensionError::Archive {
            path: path.display().to_string(),
            message: "the archive has an entry with an empty path".to_owned(),
        });
    }
    Ok(relative)
}

/// One error type for the three readers, so each branch says the same thing.
struct ArchiveError;

impl ArchiveError {
    fn from(error: impl std::fmt::Display, destination: &Path) -> ExtensionError {
        ExtensionError::Archive {
            path: destination.display().to_string(),
            message: error.to_string(),
        }
    }
}

/// Read a whole file into memory, for a test fixture that is about to be packed.
#[cfg(test)]
fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(name).expect("the fixture is readable")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// Pack one file at `path` into a gzip tarball.
    fn tar_gz(path: &str, contents: &[u8]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, Cursor::new(contents))
            .expect("append");
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip")
    }

    #[test]
    fn a_tarball_unpacks_preserving_its_paths() {
        let directory = tempfile::tempdir().expect("temp dir");
        let bytes = tar_gz("pack/theme.toml", b"bg = \"#000000\"\n");
        unpack(&bytes, directory.path()).expect("unpacks");
        assert_eq!(
            std::fs::read(directory.path().join("pack/theme.toml")).expect("the file"),
            b"bg = \"#000000\"\n"
        );
    }

    #[test]
    fn an_entry_that_climbs_out_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        // The tar crate refuses `..` in `set_path`, so the name field is written
        // directly: an attacker is not using the tar crate.
        let mut header = tar::Header::new_gnu();
        header.set_path("escaped").expect("a valid name first");
        header.set_size(3);
        header.set_mode(0o644);
        let name = b"../escaped";
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_cksum();
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder
            .append(&header, Cursor::new(b"bad"))
            .expect("append");
        let encoder = builder.into_inner().expect("finish tar");
        let bytes = encoder.finish().expect("finish gzip");
        let error = unpack(&bytes, directory.path()).expect_err("refused");
        assert!(error.to_string().contains("climbs out"), "{error}");
        assert!(
            !directory.path().parent().unwrap().join("escaped").exists(),
            "nothing is written outside the destination"
        );
    }

    #[test]
    fn a_symlink_is_refused_rather_than_skipped() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_path("pack/link").expect("path");
        header.set_link_name("/etc/passwd").expect("link");
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder.append(&header, std::io::empty()).expect("append");
        let encoder = builder.into_inner().expect("finish tar");
        let bytes = encoder.finish().expect("finish gzip");
        let error = unpack(&bytes, directory.path()).expect_err("refused");
        assert!(error.to_string().contains("symlinks"), "{error}");
    }

    #[test]
    fn a_zip_unpacks_too() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            writer
                .start_file(
                    "pack/extension.toml",
                    zip::write::SimpleFileOptions::default(),
                )
                .expect("start");
            writer.write_all(b"schema_version = 1\n").expect("write");
            writer.finish().expect("finish");
        }
        let bytes = buffer.into_inner();
        unpack(&bytes, directory.path()).expect("unpacks");
        assert_eq!(
            std::fs::read(directory.path().join("pack/extension.toml")).expect("the file"),
            b"schema_version = 1\n"
        );
    }

    /// Pack several files into a gzip tarball, in order.
    fn tar_gz_files(files: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, Cursor::new(*contents))
                .expect("append");
        }
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip")
    }

    #[test]
    fn a_tarball_that_unpacks_past_the_limit_is_refused_whole() {
        let directory = tempfile::tempdir().expect("temp dir");
        let bytes = tar_gz_files(&[
            ("pack/extension.toml", b"schema_version = 1\n"),
            ("pack/payload.bin", &[0_u8; 4096]),
        ]);
        let error = unpack_within(&bytes, directory.path(), 1024).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("more than 1024 bytes"), "{message}");
        // The file name, not the joined path: a refusal prints the path the platform
        // joins, and this assertion is about the entry being named at all.
        assert!(
            message.contains("payload.bin"),
            "the entry that crossed the cap is named: {message}"
        );
    }

    #[test]
    fn a_zip_that_unpacks_past_the_limit_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            writer
                .start_file("pack/big.bin", zip::write::SimpleFileOptions::default())
                .expect("start");
            writer.write_all(&[0_u8; 4096]).expect("write");
            writer.finish().expect("finish");
        }
        let bytes = buffer.into_inner();
        let error = unpack_within(&bytes, directory.path(), 1024).expect_err("refused");
        assert!(
            error.to_string().contains("more than 1024 bytes"),
            "{error}"
        );
    }

    #[test]
    fn the_cap_leaves_room_for_what_a_pack_carries() {
        // A whole built-in extension is far below the cap, so the shipped packs are
        // never the thing a limit refuses.
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions/acpilot.language-servers");
        let mut total = 0_u64;
        let mut stack = vec![fixture];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory).expect("a readable fixture") {
                let path = entry.expect("an entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    total += std::fs::metadata(&path).expect("metadata").len();
                }
            }
        }
        assert!(
            total < MAX_UNPACKED_BYTES / 16,
            "the language-server pack is {total} bytes, and the cap is {MAX_UNPACKED_BYTES}"
        );
    }

    #[test]
    fn bytes_that_are_neither_format_are_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let error = unpack(b"not an archive", directory.path()).expect_err("refused");
        assert!(error.to_string().contains("neither"), "{error}");
    }

    #[allow(dead_code)]
    fn unused_fixture() {
        let _ = fixture(".");
    }
}
