//! The clipboard a terminal can reach.
//!
//! Copy mode copies to the terminal a person is looking at, not to the machine
//! the TUI runs on: over SSH those are two computers and there is no system
//! clipboard in between. OSC 52 puts the text on the same stream the screen is
//! drawn on and lets the far end turn it into its own clipboard. tmux forwards it
//! when `set-clipboard` allows it; a terminal that ignores it ignores it
//! silently, which is why the status line also says the copy happened.

/// The escape that puts `text` on the terminal's clipboard.
///
/// `c` is the clipboard selection — the one `Ctrl-V` pastes — rather than `p`,
/// the primary selection a middle-click uses. BEL terminates it: ST is equally
/// correct and less widely understood.
#[must_use]
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Standard base64 with padding.
///
/// Written here rather than pulled in as a dependency: OSC 52 needs one alphabet
/// and nothing else, and a crate for twenty lines is a crate to keep current.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = chunk.iter().enumerate().fold(0u32, |bits, (index, byte)| {
            bits | (u32::from(*byte) << (16 - 8 * index))
        });
        encoded.push(char::from(ALPHABET[(bits >> 18) as usize & 0x3f]));
        encoded.push(char::from(ALPHABET[(bits >> 12) as usize & 0x3f]));
        if chunk.len() > 1 {
            encoded.push(char::from(ALPHABET[(bits >> 6) as usize & 0x3f]));
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(char::from(ALPHABET[bits as usize & 0x3f]));
        } else {
            encoded.push('=');
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads_the_last_group() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"hi"), "aGk=");
    }

    #[test]
    fn a_copy_is_one_osc_52_escape() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
        // Newlines survive: a selection is often several lines, and a shell
        // prompt is where most of them are pasted.
        assert_eq!(osc52("a\nb"), "\x1b]52;c;YQpi\x07");
    }
}
