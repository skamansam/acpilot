//! `Content-Length` framing, which is all a language server's transport is.
//!
//! LSP over stdio is `Content-Length: <bytes>\r\n\r\n<body>` repeated until EOF,
//! with the body being one UTF-8 JSON-RPC message. The only real decisions are
//! what to do with a header we do not understand (ignore it), a message larger
//! than memory (refuse before allocating), and EOF in the middle of a body
//! (report it, do not return a truncated message for the parser to choke on).
//!
//! Framing is generic over `AsyncRead`/`AsyncWrite` rather than tied to
//! `ChildStdin`/`ChildStdout` so the same code runs over a socket, an SSH
//! channel, or an in-memory duplex in a test. P0-7's other question — what a
//! client loop over a non-stdio transport costs — starts here.

use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Refuse a message larger than this rather than growing a buffer until the
/// process dies. Language servers do send large payloads (a full-project
/// diagnostic pull on a big workspace); 64 MiB is far above anything observed,
/// and still bounded.
pub const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

/// Framing failures, distinct from protocol errors: this is a broken transport.
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    /// The underlying stream failed.
    #[error("transport: {0}")]
    Io(#[from] io::Error),
    /// The header block ended without a `Content-Length` header.
    #[error("message header has no Content-Length")]
    MissingContentLength,
    /// `Content-Length` was not a decimal byte count.
    #[error("invalid Content-Length value {0:?}")]
    InvalidContentLength(String),
    /// The advertised body exceeds [`MAX_CONTENT_LENGTH`].
    #[error("message of {0} bytes exceeds the {MAX_CONTENT_LENGTH}-byte limit")]
    TooLarge(usize),
    /// The stream ended after fewer than `Content-Length` bytes.
    #[error("stream ended {0} bytes into a {1}-byte message")]
    Truncated(usize, usize),
    /// Header bytes were not UTF-8, so they cannot be parsed.
    #[error("message header is not UTF-8")]
    InvalidHeader,
}

/// Read one message body, headers consumed.
///
/// Returns `Ok(None)` at a clean EOF: the peer closed between messages, which is
/// how a language server exits. Any other incomplete state is an error.
pub async fn read_message<R>(reader: &mut R) -> Result<Option<Vec<u8>>, FramingError>
where
    R: AsyncBufRead + Unpin,
{
    let mut content_length = None;
    let mut saw_header = false;

    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line).await?;
        if read == 0 {
            return if saw_header {
                Err(FramingError::Truncated(0, content_length.unwrap_or(0)))
            } else {
                Ok(None)
            };
        }
        let line = std::str::from_utf8(&line).map_err(|_| FramingError::InvalidHeader)?;
        saw_header = true;

        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            break;
        }

        let Some((name, value)) = line.split_once(':') else {
            continue; // a header we do not understand is not a reason to die
        };
        if name.eq_ignore_ascii_case("content-length") {
            let value = value.trim();
            let length: usize = value
                .parse()
                .map_err(|_| FramingError::InvalidContentLength(value.to_owned()))?;
            content_length = Some(length);
        }
    }

    let length = content_length.ok_or(FramingError::MissingContentLength)?;
    if length > MAX_CONTENT_LENGTH {
        return Err(FramingError::TooLarge(length));
    }

    let mut body = vec![0; length];
    if let Err(error) = reader.read_exact(&mut body).await {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            // `read_exact` does not say how much it read; the caller gets a
            // framing error either way, and the next read would be garbage.
            return Err(FramingError::Truncated(0, length));
        }
        return Err(FramingError::Io(error));
    }
    Ok(Some(body))
}

/// Write one message body with its header, then flush.
///
/// Flushing matters: a server that receives half a request waits, the client
/// waits for a response, and the bug shows up as a hang rather than an error.
pub async fn write_message<W>(writer: &mut W, body: &[u8]) -> Result<(), FramingError>
where
    W: AsyncWrite + Unpin,
{
    if body.len() > MAX_CONTENT_LENGTH {
        return Err(FramingError::TooLarge(body.len()));
    }
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader, duplex};

    #[tokio::test]
    async fn round_trips_a_unicode_body() {
        let body = r#"{"jsonrpc":"2.0","method":"x","params":{"text":"a😀日e\u0301"}}"#;
        let (mut client, server) = duplex(4096);
        write_message(&mut client, body.as_bytes()).await.unwrap();
        drop(client);

        let mut server = BufReader::new(server);
        let read = read_message(&mut server).await.unwrap().unwrap();
        assert_eq!(String::from_utf8(read).unwrap(), body);
        assert!(read_message(&mut server).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn length_is_bytes_not_characters() {
        let body = "😀"; // four bytes, one char
        let (mut client, server) = duplex(128);
        write_message(&mut client, body.as_bytes()).await.unwrap();
        drop(client);
        let mut server = BufReader::new(server);
        assert_eq!(
            read_message(&mut server).await.unwrap().unwrap(),
            body.as_bytes()
        );
    }

    #[tokio::test]
    async fn ignores_unknown_headers_and_header_case() {
        let raw = b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}";
        let mut reader = BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap(), b"{}");
    }

    #[tokio::test]
    async fn tolerates_bare_lf_headers() {
        let raw = b"Content-Length: 2\n\n{}";
        let mut reader = BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap(), b"{}");
    }

    #[tokio::test]
    async fn survives_a_message_arriving_in_pieces() {
        let (mut client, server) = duplex(64);
        let writer = tokio::spawn(async move {
            // Header and body split across writes, including mid-number.
            client.write_all(b"Content-Len").await.unwrap();
            client.write_all(b"gth: 7\r\n\r\n{\"a\":1").await.unwrap();
            client.write_all(b"}").await.unwrap();
            client.flush().await.unwrap();
        });
        let mut reader = BufReader::new(server);
        writer.await.unwrap();
        assert_eq!(
            read_message(&mut reader).await.unwrap().unwrap(),
            b"{\"a\":1}"
        );
    }

    #[test]
    fn missing_content_length_is_an_error() {
        let mut reader = BufReader::new(&b"Content-Type: x\r\n\r\n{}"[..]);
        let error = read_message_blocking(&mut reader);
        assert!(matches!(error, Err(FramingError::MissingContentLength)));
    }

    #[test]
    fn invalid_content_length_is_an_error() {
        let mut reader = BufReader::new(&b"Content-Length: seven\r\n\r\n{}"[..]);
        let error = read_message_blocking(&mut reader);
        assert!(matches!(error, Err(FramingError::InvalidContentLength(_))));
    }

    #[test]
    fn oversized_message_is_refused_before_allocating() {
        let raw = format!("Content-Length: {}\r\n\r\n", MAX_CONTENT_LENGTH + 1);
        let mut reader = BufReader::new(raw.as_bytes());
        let error = read_message_blocking(&mut reader);
        assert!(matches!(error, Err(FramingError::TooLarge(_))));
    }

    #[test]
    fn eof_inside_a_body_is_not_a_message() {
        let mut reader = BufReader::new(&b"Content-Length: 10\r\n\r\n{}"[..]);
        let error = read_message_blocking(&mut reader);
        assert!(matches!(error, Err(FramingError::Truncated(..))));
    }

    #[test]
    fn clean_eof_is_the_end_of_the_stream() {
        let mut reader = BufReader::new(&b""[..]);
        assert!(read_message_blocking(&mut reader).unwrap().is_none());
    }

    // The async API needs a runtime; these cases only need the parser, so a
    // current-thread runtime keeps them in the normal test binary.
    fn read_message_blocking(
        reader: &mut BufReader<&[u8]>,
    ) -> Result<Option<Vec<u8>>, FramingError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(read_message(reader))
    }
}
