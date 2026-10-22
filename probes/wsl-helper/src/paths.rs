//! Windows ↔ one WSL distro path translation.
//!
//! `wslpath` turns out not to be one prefix mapping but **two independent
//! mappings**, and only one of them is about the distro:
//!
//! * `C:\Users\SamTyler` ↔ `/mnt/c/Users/SamTyler` — a *mounted drive*. WSL2
//!   mounts `C:` by default and nothing else unless `/etc/wsl.conf` adds it, so a
//!   drive that is not under `/mnt` has no Linux path at all: `wslpath -w
//!   /mnt/d/x` printed `\\wsl.localhost\Ubuntu-24.04\mnt\d\x`, i.e. it treated
//!   `/mnt/d/x` as an ordinary distro path because `/mnt/d` does not exist.
//! * `/home/sam` ↔ `\\wsl.localhost\Ubuntu-24.04\home\sam` — the *distro root*,
//!   named by the distro, not by a fixed prefix.
//!
//! Everything here is pure and tested off Windows; the only platform-dependent
//! part is discovering the distro name and the mounted drives, which lives in
//! `wsl.rs`. This module is written to be lifted into `acpilot-host` as is.
//!
//! Deliberately **not** a sanitizer: `.` and `..` segments pass through, because
//! translation and traversal policy are different jobs (P3-5 owns the latter).

use std::fmt;

/// The two prefixes of one distro: its UNC root and the drives mounted under
/// `/mnt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslPathMap {
    distro: String,
    /// Lowercase drive letters that really exist under `/mnt`.
    mounted_drives: Vec<char>,
}

/// Why a path could not be translated. A caller that gets one of these should
/// refuse the path rather than guess: a wrong translation is a wrong file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    /// `D:\x` when `/mnt/d` is not mounted — there is no Linux path to give.
    UnmountedDrive {
        drive: char,
        mounted: Vec<char>,
    },
    /// A `\\wsl.localhost\...` path for a different distro than the one running.
    OtherDistro {
        expected: String,
        found: String,
    },
    /// A UNC path that is not a WSL share (`\\server\share`).
    UnsupportedUnc {
        host: String,
    },
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty path"),
            Self::UnmountedDrive { drive, mounted } => {
                let mounted = if mounted.is_empty() {
                    "none".to_string()
                } else {
                    mounted
                        .iter()
                        .map(|d| format!("{d}:"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                write!(
                    f,
                    "drive {drive}: is not mounted in WSL (mounted: {mounted}); \
                     there is no Linux path for it until it is added to /etc/wsl.conf"
                )
            }
            Self::OtherDistro { expected, found } => {
                write!(f, "path belongs to distro {found}, not {expected}")
            }
            Self::UnsupportedUnc { host } => {
                write!(f, "\\{host} is not a WSL share")
            }
        }
    }
}

impl std::error::Error for PathError {}

/// The distro named by a `\\wsl.localhost\<distro>\...` (or the older
/// `\\wsl$\<distro>\...`) path.
///
/// This is the entry point for the case that matters most in practice: a
/// developer keeps the code in WSL and works in a Windows app, so the only path
/// the app is handed is a UNC one. The Windows path is a door, not an identity —
/// the workspace is the *pair* of that distro and the Linux path behind it, and
/// everything else (agent, LSP, git, PTY) happens inside the distro.
///
/// `None` for anything that is not a WSL share, so a network path cannot be
/// mistaken for a distro.
pub fn distro_from_unc(windows: &str) -> Option<String> {
    let unified = windows.trim().replace('\\', "/");
    let rest = unified.strip_prefix("//")?;
    let (host, rest) = rest.split_once('/')?;
    if !host.eq_ignore_ascii_case("wsl.localhost") && !host.eq_ignore_ascii_case("wsl$") {
        return None;
    }
    let distro = match rest.split_once('/') {
        Some((distro, _)) => distro,
        None => rest,
    };
    (!distro.is_empty()).then(|| distro.to_string())
}

impl WslPathMap {
    pub fn new(distro: impl Into<String>, mounted_drives: impl IntoIterator<Item = char>) -> Self {
        let mut mounted: Vec<char> = mounted_drives
            .into_iter()
            .map(|d| d.to_ascii_lowercase())
            .collect();
        mounted.sort_unstable();
        mounted.dedup();
        Self {
            distro: distro.into(),
            mounted_drives: mounted,
        }
    }

    pub fn distro(&self) -> &str {
        &self.distro
    }

    pub fn mounted_drives(&self) -> &[char] {
        &self.mounted_drives
    }

    /// `C:\Users\SamTyler` → `/mnt/c/Users/SamTyler`,
    /// `\\wsl.localhost\<distro>\home\sam` → `/home/sam`.
    ///
    /// A relative path is passed through with its separators normalised, which is
    /// what `wslpath -u` does; it is relative to the process working directory on
    /// the Linux side, not to the Windows one.
    pub fn to_linux(&self, windows: &str) -> Result<String, PathError> {
        let path = windows.trim();
        if path.is_empty() {
            return Err(PathError::Empty);
        }

        // Both separators are accepted: `wslpath -u 'C:/Users/SamTyler'` works,
        // and so does the backslash form.
        let unified = path.replace('\\', "/");

        if let Some(rest) = unified.strip_prefix("//") {
            return self.unc_to_linux(rest);
        }

        let bytes = unified.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            if !self.mounted_drives.contains(&drive) {
                return Err(PathError::UnmountedDrive {
                    drive,
                    mounted: self.mounted_drives.clone(),
                });
            }
            let tail = unified[2..].trim_start_matches('/');
            let tail = tail.trim_end_matches('/');
            return Ok(if tail.is_empty() {
                format!("/mnt/{drive}")
            } else {
                format!("/mnt/{drive}/{tail}")
            });
        }

        Ok(unified)
    }

    fn unc_to_linux(&self, rest: &str) -> Result<String, PathError> {
        let (host, rest) = match rest.split_once('/') {
            Some((host, rest)) => (host, rest),
            None => (rest, ""),
        };
        let host_lower = host.to_ascii_lowercase();
        if host_lower != "wsl.localhost" && host_lower != "wsl$" {
            return Err(PathError::UnsupportedUnc {
                host: host.to_string(),
            });
        }
        let (distro, inner) = match rest.split_once('/') {
            Some((distro, inner)) => (distro, inner),
            None => (rest, ""),
        };
        if !distro.eq_ignore_ascii_case(&self.distro) {
            return Err(PathError::OtherDistro {
                expected: self.distro.clone(),
                found: distro.to_string(),
            });
        }
        let inner = inner.trim_matches('/');
        Ok(if inner.is_empty() {
            "/".to_string()
        } else {
            format!("/{inner}")
        })
    }

    /// `/mnt/c/Users/SamTyler` → `C:\Users\SamTyler`,
    /// `/home/sam` → `\\wsl.localhost\<distro>\home\sam`.
    ///
    /// `/mnt/d/...` with `D:` unmounted is *not* an error here: like `wslpath -w`
    /// it falls through to the distro-root mapping, because in that case the path
    /// really is an ordinary directory inside the distro.
    pub fn to_windows(&self, linux: &str) -> Result<String, PathError> {
        let path = linux.trim();
        if path.is_empty() {
            return Err(PathError::Empty);
        }
        if !path.starts_with('/') {
            // `wslpath -w relative/x` → `relative\x`; still relative, still the
            // caller's working directory problem, not ours.
            return Ok(path.replace('/', "\\"));
        }

        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return Ok(format!("\\\\wsl.localhost\\{}\\", self.distro));
        }

        let rest = &trimmed[1..];
        if let Some(after) = rest.strip_prefix("mnt/") {
            let (head, tail) = match after.split_once('/') {
                Some((head, tail)) => (head, tail),
                None => (after, ""),
            };
            let mut chars = head.chars();
            if head.len() == 1
                && let Some(drive) = chars.next()
            {
                let drive = drive.to_ascii_lowercase();
                if drive.is_ascii_alphabetic() && self.mounted_drives.contains(&drive) {
                    let letter = drive.to_ascii_uppercase();
                    return Ok(if tail.is_empty() {
                        format!("{letter}:\\")
                    } else {
                        format!("{letter}:\\{}", tail.replace('/', "\\"))
                    });
                }
            }
        }

        Ok(format!(
            "\\\\wsl.localhost\\{}\\{}",
            self.distro,
            rest.replace('/', "\\")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> WslPathMap {
        // The dev box: C: mounted, D: present in Windows but not in WSL.
        WslPathMap::new("Ubuntu-24.04", ['c'])
    }

    #[test]
    fn mounted_drive_both_ways() {
        assert_eq!(
            map().to_linux(r"C:\Users\SamTyler").unwrap(),
            "/mnt/c/Users/SamTyler"
        );
        assert_eq!(
            map().to_linux("C:/Users/SamTyler").unwrap(),
            "/mnt/c/Users/SamTyler"
        );
        assert_eq!(
            map().to_linux(r"c:\users").unwrap(),
            "/mnt/c/users",
            "drive letters are case-insensitive on Windows"
        );
        assert_eq!(map().to_linux(r"C:\").unwrap(), "/mnt/c");
        assert_eq!(map().to_linux("C:").unwrap(), "/mnt/c");
        assert_eq!(
            map().to_windows("/mnt/c/Users/SamTyler").unwrap(),
            r"C:\Users\SamTyler"
        );
        assert_eq!(map().to_windows("/mnt/c").unwrap(), r"C:\");
        assert_eq!(map().to_windows("/mnt/c/").unwrap(), r"C:\");
        assert_eq!(
            map().to_windows("/mnt/c/Users/SamTyler/").unwrap(),
            r"C:\Users\SamTyler"
        );
    }

    #[test]
    fn distro_root_both_ways() {
        assert_eq!(
            map().to_windows("/home/sam").unwrap(),
            r"\\wsl.localhost\Ubuntu-24.04\home\sam"
        );
        assert_eq!(
            map()
                .to_linux(r"\\wsl.localhost\Ubuntu-24.04\home\sam")
                .unwrap(),
            "/home/sam"
        );
        assert_eq!(
            map().to_linux(r"\\wsl$\Ubuntu-24.04\home\sam").unwrap(),
            "/home/sam",
            "the legacy wsl$ share is the same mapping"
        );
        assert_eq!(
            map().to_windows("/").unwrap(),
            r"\\wsl.localhost\Ubuntu-24.04\"
        );
        assert_eq!(
            map().to_linux(r"\\wsl.localhost\Ubuntu-24.04").unwrap(),
            "/"
        );
        assert_eq!(
            map()
                .to_linux("//wsl.localhost/Ubuntu-24.04/home/sam")
                .unwrap(),
            "/home/sam",
            "forward-slash UNC is what a URL parser hands back"
        );
    }

    #[test]
    fn unmounted_drive_has_no_linux_path() {
        let error = map().to_linux(r"D:\data").unwrap_err();
        assert_eq!(
            error,
            PathError::UnmountedDrive {
                drive: 'd',
                mounted: vec!['c']
            }
        );
        // …but `/mnt/d` on the Linux side is just a directory that does not
        // exist, exactly as `wslpath -w` reported it.
        assert_eq!(
            map().to_windows("/mnt/d/a b/x").unwrap(),
            r"\\wsl.localhost\Ubuntu-24.04\mnt\d\a b\x"
        );
    }

    #[test]
    fn other_distro_is_refused() {
        let error = map()
            .to_linux(r"\\wsl.localhost\Debian\home\sam")
            .unwrap_err();
        assert_eq!(
            error,
            PathError::OtherDistro {
                expected: "Ubuntu-24.04".to_string(),
                found: "Debian".to_string()
            }
        );
        // Windows paths are case-insensitive, and so is the distro prefix here.
        assert_eq!(
            map()
                .to_linux(r"\\WSL.LOCALHOST\ubuntu-24.04\home")
                .unwrap(),
            "/home"
        );
    }

    #[test]
    fn non_wsl_unc_is_refused() {
        assert_eq!(
            map().to_linux(r"\\server\share\file").unwrap_err(),
            PathError::UnsupportedUnc {
                host: "server".to_string()
            }
        );
    }

    #[test]
    fn spaces_and_non_ascii_survive() {
        assert_eq!(
            map().to_linux(r"C:\a b\ü\文件").unwrap(),
            "/mnt/c/a b/ü/文件"
        );
        assert_eq!(
            map().to_windows("/tmp/a b/ü/文件").unwrap(),
            r"\\wsl.localhost\Ubuntu-24.04\tmp\a b\ü\文件"
        );
    }

    #[test]
    fn relative_paths_pass_through() {
        assert_eq!(map().to_linux(r"relative\x").unwrap(), "relative/x");
        assert_eq!(map().to_windows("relative/x").unwrap(), r"relative\x");
    }

    #[test]
    fn empty_is_an_error_not_the_cwd() {
        assert_eq!(map().to_linux("").unwrap_err(), PathError::Empty);
        assert_eq!(map().to_linux("   ").unwrap_err(), PathError::Empty);
        assert_eq!(map().to_windows("").unwrap_err(), PathError::Empty);
    }

    #[test]
    fn dot_segments_are_not_resolved() {
        // Translation is not sanitisation; `..` is the policy layer's problem.
        assert_eq!(
            map().to_linux(r"C:\Users\SamTyler\..\Public").unwrap(),
            "/mnt/c/Users/SamTyler/../Public"
        );
    }

    #[test]
    fn round_trip_is_stable() {
        for path in [
            r"C:\Users\SamTyler",
            r"C:\a b\ü",
            r"C:\",
            r"\\wsl.localhost\Ubuntu-24.04\home\sam",
        ] {
            let linux = map().to_linux(path).unwrap();
            let back = map().to_windows(&linux).unwrap();
            let again = map().to_linux(&back).unwrap();
            assert_eq!(linux, again, "round trip changed {path}");
            assert_eq!(back, map().to_windows(&linux).unwrap());
        }
    }

    #[test]
    fn distro_is_read_from_a_wsl_unc_path() {
        // Both spellings, with and without a path under the distro.
        assert_eq!(
            distro_from_unc(r"\\wsl.localhost\Ubuntu-24.04\home\sam").as_deref(),
            Some("Ubuntu-24.04")
        );
        assert_eq!(
            distro_from_unc(r"\\wsl$\Ubuntu-22.04").as_deref(),
            Some("Ubuntu-22.04")
        );
        assert_eq!(
            distro_from_unc(r"\\WSL.LOCALHOST\Ubuntu-24.04").as_deref(),
            Some("Ubuntu-24.04"),
            "the share name is case-insensitive"
        );
    }

    #[test]
    fn only_wsl_shares_name_a_distro() {
        assert_eq!(distro_from_unc(r"\\fileserver\share\proj"), None);
        assert_eq!(distro_from_unc(r"C:\Users\SamTyler"), None);
        assert_eq!(
            distro_from_unc(r"\\wsl.localhost"),
            None,
            "no distro segment"
        );
        assert_eq!(distro_from_unc(""), None);
    }
}
