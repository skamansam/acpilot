//! Base64 for the terminal channel.
//!
//! A PTY produces bytes, and the wire is JSON lines: a chunk can hold a `"`, a
//! newline, a lone `0x1b` or half of a UTF-8 character, so it cannot be a JSON
//! string. Base64 is the one encoding both ends already agree on, and it costs
//! a third of the channel's width rather than a parser that has to be right about
//! every escape a program can print.
//!
//! Hand-rolled for the same reason the clipboard's and the install script's are:
//! one encoding is not worth a dependency, and this end is uploaded where nothing
//! can be installed.

/// The standard alphabet, padded, on one line.
///
/// No wrapping: one notification is one line, and a newline in the middle of the
/// payload would be a second line the daemon has to know to join.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
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
    out
}

/// The bytes an encoded chunk carries.
///
/// Padding ends the chunk rather than being counted: what follows a `=` is not data,
/// and a decoder that kept reading would turn a stray character into bytes a program
/// never wrote. Whitespace is dropped, because a chunk that travelled through a shell
/// or a test's `base64` arrives wrapped.
///
/// # Errors
///
/// A character outside the alphabet. Reported rather than skipped: a wrong byte in a
/// terminal is a wrong keystroke, and a decoder that guessed would hide it.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
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
    fn every_byte_survives_a_round_trip() {
        // Every value, in an order where no two neighbours repeat: a codec that is
        // wrong about one six-bit group shows up here and nowhere in a sentence.
        let bytes: Vec<u8> = (0..=255u8).rev().chain(0..=255u8).collect();
        let encoded = encode(&bytes);
        assert!(!encoded.contains('\n'), "one notification is one line");
        assert_eq!(decode(&encoded).expect("our own base64"), bytes);
    }

    #[test]
    fn padding_ends_the_chunk() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(decode("Zg==").expect("padded"), b"f");
    }

    #[test]
    fn a_character_outside_the_alphabet_is_reported() {
        let error = decode("aG!k").expect_err("refused");
        assert!(error.contains('!'), "{error}");
    }
}
