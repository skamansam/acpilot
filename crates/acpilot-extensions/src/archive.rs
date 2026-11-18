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

/// Unpack an archive into a directory that must already exist.
///
/// # Errors
///
/// The bytes are neither a gzip tarball nor a zip, an entry is not a regular file
/// or directory, or an entry's path is absolute or climbs out of `destination`.
pub fn unpack(bytes: &[u8], destination: &Path) -> Result<(), ExtensionError> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        unpack_tar_gz(bytes, destination)
    } else if bytes.starts_with(b"PK\x03\x04") {
        unpack_zip(bytes, destination)
    } else {
        Err(ExtensionError::Archive {
            path: destination.display().to_string(),
            message: "the download is neither a gzip tarball nor a zip; nothing was installed"
                .to_owned(),
        })
    }
}

fn unpack_tar_gz(bytes: &[u8], destination: &Path) -> Result<(), ExtensionError> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| ArchiveError::from(error, destination))?;
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

fn unpack_zip(bytes: &[u8], destination: &Path) -> Result<(), ExtensionError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| ArchiveError::from(error, destination))?;
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
