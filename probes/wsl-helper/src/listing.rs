//! What `wsl.exe -l` prints, decoded and parsed.
//!
//! This lives outside `wsl.rs` on purpose. `wsl.rs` spawns `wsl.exe`, so it is
//! `cfg(windows)` and its tests only run on Windows; a parsing bug that decided
//! whether a distro is WSL1 or WSL2 must not be able to hide behind that gate.
//! Everything here is bytes in, structs out, and it is tested on both
//! operating systems by the same `cargo test`.
//!
//! Both listings are UTF-16LE with CRLF: `-l -q` is one padded name per line,
//! `-l -v` is a header plus `NAME STATE VERSION` columns. A reader that treats
//! them as UTF-8 sees NULs between every character.

/// One row of `wsl.exe -l -v`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distro {
    pub name: String,
    pub state: String,
    /// 1 is the syscall-translation layer; 2 is a real Linux kernel in a VM.
    pub version: u32,
}

/// `wsl.exe -l -q` writes one name per line. Decode it rather than assume it,
/// and fall back to UTF-8 for a build that changed its mind.
pub fn decode_listing(bytes: &[u8]) -> Vec<String> {
    split_lines(&decode(bytes))
}

/// `wsl.exe -l -v`. The header row fails the version parse and drops out, which
/// is cheaper than matching the word `NAME` in a localised build.
pub fn parse_verbose(bytes: &[u8]) -> Vec<Distro> {
    split_lines(&decode(bytes))
        .into_iter()
        .filter_map(|line| {
            // The running distro is marked with a `*`, and names may contain
            // spaces, so split from the right: version, state, then the rest.
            let line = line.trim_start_matches(|c: char| c == '*' || c.is_whitespace());
            let mut tokens: Vec<&str> = line.split_whitespace().collect();
            let version = tokens.pop()?.parse::<u32>().ok()?;
            let state = tokens.pop()?.to_string();
            let name = tokens.join(" ");
            if name.is_empty() {
                return None;
            }
            Some(Distro {
                name,
                state,
                version,
            })
        })
        .collect()
}

/// The gate the bench runs behind: WSL1 and WSL2 are different products, and a
/// number measured on one does not describe the other.
///
/// WSL1 translates Linux syscalls onto NT ones; WSL2 runs a real kernel in a
/// Hyper-V VM. Startup, `/mnt` and PTY behaviour all differ, so a WSL1 run of
/// the bench would produce a second, unrelated table rather than corroborate
/// the WSL2 one. GitHub's standard hosted Windows runners have no nested
/// virtualisation, so they can only offer WSL1.
pub fn check_wsl2(distro: &str, version: u32) -> Result<(), String> {
    if version == 2 {
        return Ok(());
    }
    Err(format!(
        "distro {distro} is WSL{version}, not WSL2: WSL1 translates syscalls instead of \
         running a kernel, so its startup cost, /mnt and PTY behaviour are a different \
         product and its numbers do not describe the shipped path. Run this where a \
         kernel VM can start — a real Windows host or a larger/self-hosted runner — or \
         drop --require-wsl2 to record a WSL{version} result under its own name."
    ))
}

fn decode(bytes: &[u8]) -> String {
    if let Some(body) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        decode_utf16le(body)
    } else if looks_utf16le(bytes) {
        decode_utf16le(bytes)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn split_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|line| line.trim_end_matches('\r').trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// True when every high byte is NUL, which is what UTF-16LE ASCII looks like.
/// A non-ASCII distro name would fail this test and fall back to the lossy
/// UTF-8 path.
fn looks_utf16le(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && bytes.len().is_multiple_of(2)
        && bytes.iter().skip(1).step_by(2).all(|high| *high == 0x00)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same encoding `wsl.exe` uses: UTF-16LE, no BOM, CRLF line endings.
    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect()
    }

    #[test]
    fn quiet_listing_decodes_utf16le() {
        let bytes = utf16le("Ubuntu-24.04\r\nUbuntu-22.04\r\ndocker-desktop\r\n");
        assert_eq!(
            decode_listing(&bytes),
            ["Ubuntu-24.04", "Ubuntu-22.04", "docker-desktop"]
        );
    }

    #[test]
    fn quiet_listing_accepts_utf8_and_a_bom() {
        assert_eq!(
            decode_listing(b"Ubuntu-24.04\nDebian\n"),
            ["Ubuntu-24.04", "Debian"]
        );
        let mut bom = vec![0xFF, 0xFE];
        bom.extend(utf16le("Ubuntu-24.04\r\n"));
        assert_eq!(decode_listing(&bom), ["Ubuntu-24.04"]);
    }

    #[test]
    fn verbose_listing_drops_the_header_and_keeps_versions() {
        let bytes = utf16le(
            "  NAME            STATE           VERSION\r\n\
             * Ubuntu-24.04    Running         2\r\n\
               Debian          Stopped         1\r\n",
        );
        assert_eq!(
            parse_verbose(&bytes),
            [
                Distro {
                    name: "Ubuntu-24.04".into(),
                    state: "Running".into(),
                    version: 2,
                },
                Distro {
                    name: "Debian".into(),
                    state: "Stopped".into(),
                    version: 1,
                },
            ]
        );
    }

    #[test]
    fn verbose_listing_keeps_a_name_with_spaces() {
        let bytes = utf16le("  NAME STATE VERSION\r\n  My Distro  Running  2\r\n");
        let distros = parse_verbose(&bytes);
        assert_eq!(distros.len(), 1);
        assert_eq!(distros[0].name, "My Distro");
        assert_eq!(distros[0].version, 2);
    }

    #[test]
    fn verbose_listing_survives_rubbish() {
        let bytes = utf16le("  NAME STATE VERSION\r\n  broken row\r\n\r\n");
        assert!(parse_verbose(&bytes).is_empty());
    }

    #[test]
    fn only_wsl2_passes_the_gate() {
        assert!(check_wsl2("Ubuntu-24.04", 2).is_ok());
        let error = check_wsl2("Ubuntu-24.04", 1).expect_err("WSL1 must be refused");
        assert!(error.contains("WSL1"), "{error}");
        assert!(error.contains("--require-wsl2"), "{error}");
        assert!(
            check_wsl2("Ubuntu-24.04", 3).is_err(),
            "unknown versions fail closed"
        );
    }
}
