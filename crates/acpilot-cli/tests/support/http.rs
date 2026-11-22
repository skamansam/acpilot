//! Reading the requests a fake receives.

use std::io::Read;
use std::net::TcpStream;

/// Reads one request: the head, then the body it declares. Answering before the
/// body is read makes the close that follows a reset on some platforms. A
/// reset can discard the answer that was just written. That is the shape of the
/// macOS 404 one CLI test produced. A daemon reads what the client sent anyway.
pub fn read_request(stream: &mut TcpStream) -> Option<String> {
    // macOS and Windows hand out accepted sockets that inherit the listener's
    // non-blocking flag. A read of a body that has not arrived yet would then
    // look like a broken request. So the fake would answer before the body, the
    // very race this reader exists to close.
    if stream.set_nonblocking(false).is_err() {
        return None;
    }
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    let head_end = loop {
        if let Some(at) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return None,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let declared = content_length(&head).unwrap_or(0);
    let mut read = request.len() - head_end;
    while read < declared {
        let want = (declared - read).min(buffer.len());
        match stream.read(&mut buffer[..want]) {
            Ok(0) | Err(_) => break,
            Ok(got) => read += got,
        }
    }
    Some(head)
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("content-length") {
            return None;
        }
        value.trim().parse().ok()
    })
}
