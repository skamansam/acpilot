//! Base64, where the daemon needs bytes in a channel that carries text.
//!
//! Two callers, two shapes: the install script puts a whole binary on a shell's
//! stdin, where a line long enough to be a problem is worth wrapping at 76
//! characters; the PTY channel puts one chunk of output inside a JSON string,
//! where a newline costs an escape in every frame. So the codec is the same and
//! the wrapping is the caller's.
//!
//! Hand-rolled for the same reason the clipboard's is: one encoding is not worth a
//! dependency, and the daemon already carried an encoder for the install script.

/// The standard alphabet, padded, on one line.
#[must_use]
pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    push(bytes, &mut out, usize::MAX);
    out
}

/// The same, wrapped every 76 characters — RFC 2045's line, which every `base64`
/// implementation reads back.
#[must_use]
pub(crate) fn encode_lines(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4 + bytes.len() / 57 + 1);
    push(bytes, &mut out, 19);
    out
}

/// Encode `bytes`, starting a new line after every `columns` groups.
///
/// A group is three bytes and becomes four characters, so 19 groups is the 76 the
/// standard names.
fn push(bytes: &[u8], out: &mut String, columns: usize) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for (index, chunk) in bytes.chunks(3).enumerate() {
        if index > 0 && index % columns == 0 {
            out.push('\n');
        }
        let packed = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(char::from(ALPHABET[(packed >> 18) as usize & 63]));
        out.push(char::from(ALPHABET[(packed >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[(packed >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[packed as usize & 63])
        } else {
            '='
        });
    }
}

/// The bytes an encoded chunk carries, whitespace and all.
///
/// # Errors
///
/// A character outside the alphabet. A refusal rather than a skipped byte: a wrong
/// keystroke in a terminal is worse than a reported one, and the caller's message
/// names the character.
pub(crate) fn decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'=' => break,
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            other => return Err(format!("`{}` is not base64", char::from(other))),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            // Masked before narrowing: `buffer` keeps every group it has ever held, so
            // the bits above this byte are the previous groups' leftovers.
            out.push(u8::try_from((buffer >> bits) & 0xff).expect("a byte"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_byte_survives_both_spellings() {
        let bytes: Vec<u8> = (0..=255u8).rev().chain(0..=255u8).collect();
        assert_eq!(decode(&encode(&bytes)).expect("one line"), bytes);
        let wrapped = encode_lines(&bytes);
        assert!(
            wrapped.contains('\n'),
            "the install script's line is wrapped"
        );
        assert_eq!(decode(&wrapped).expect("wrapped"), bytes);
    }

    #[test]
    fn the_wrapping_is_the_standard_76_columns() {
        let line = encode_lines(&[0u8; 300]);
        let widths: Vec<usize> = line.split('\n').map(str::len).collect();
        assert_eq!(widths[0], 76);
        assert_eq!(*widths.last().expect("a last line"), 400 - 76 * 5);
    }

    #[test]
    fn padding_ends_the_chunk() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(decode("Zm8=").expect("padded"), b"fo");
    }

    #[test]
    fn a_character_outside_the_alphabet_is_reported() {
        let error = decode("aG!k").expect_err("refused");
        assert!(error.contains('!'), "{error}");
    }
}
