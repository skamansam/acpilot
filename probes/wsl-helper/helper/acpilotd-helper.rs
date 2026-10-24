//! `acpilotd-helper` — the Linux end of the design phase's task P0-5, a single-file probe.
//!
//! The contract is deliberately boring, because the probe is measuring the
//! *transport*, not the protocol: one JSON line in, one JSON line out, stdout is
//! for responses only, stderr is for the human. It reads until stdin closes and
//! exits 0.
//!
//! No dependencies and no cargo project on the Linux side: `rustc -O` is enough,
//! so installing it needs no network and no toolchain beyond the one WSL already
//! has.
//!
//! ```text
//! $ printf '{"jsonrpc":"2.0","id":1,"method":"ping"}\n' | acpilotd-helper --stdio
//! {"jsonrpc":"2.0","id":1,"result":{"pid":4242,"cwd":"/home/sam",...}}
//! ```

use std::io::{self, BufRead, Write};

const NAME: &str = "acpilotd-helper 0.0.0 (P0-5 probe)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("{NAME}");
        return;
    }
    // `--stdio` is the only transport this probe ships. Refuse anything else
    // rather than silently ignoring it: a daemon that starts the helper with the
    // wrong flag should fail here, not hang waiting for a response.
    if let Some(other) = args.iter().find(|arg| arg.as_str() != "--stdio") {
        eprintln!("acpilotd-helper: unknown argument {other}");
        std::process::exit(2);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let id = extract_id(&line);
        let cwd = std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let user = std::env::var("USER").unwrap_or_default();
        let argv = args
            .iter()
            .map(|arg| format!("\"{}\"", escape(arg)))
            .collect::<Vec<_>>()
            .join(",");

        // One write per response, flushed immediately: a daemon waiting on the
        // pipe must never wait for a buffer to fill.
        let response = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"pid\":{pid},\"cwd\":\"{cwd}\",\
             \"user\":\"{user}\",\"argv\":[{argv}],\"request_bytes\":{bytes}}}}}",
            pid = std::process::id(),
            cwd = escape(&cwd),
            user = escape(&user),
            argv = argv,
            id = id,
            bytes = line.len(),
        );
        if writeln!(out, "{response}")
            .and_then(|()| out.flush())
            .is_err()
        {
            break;
        }
    }
}

/// The id is echoed, not interpreted: the probe has no dispatcher. `null` when
/// the request has none, which is what a JSON-RPC notification would get.
fn extract_id(line: &str) -> String {
    let Some(index) = line.find("\"id\"") else {
        return "null".to_string();
    };
    let rest = &line[index + 4..];
    let Some(colon) = rest.find(':') else {
        return "null".to_string();
    };
    let digits: String = rest[colon + 1..]
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        "null".to_string()
    } else {
        digits
    }
}

#[cfg(test)]
mod tests {
    use super::{escape, extract_id};

    #[test]
    fn id_is_echoed() {
        assert_eq!(
            extract_id(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#),
            "7"
        );
        assert_eq!(
            extract_id(r#"{"id": 42}"#),
            "42",
            "whitespace before the value"
        );
    }

    #[test]
    fn missing_or_non_numeric_id_is_null() {
        assert_eq!(extract_id(r#"{"method":"ping"}"#), "null");
        assert_eq!(extract_id(r#"{"id":"abc"}"#), "null");
        assert_eq!(extract_id(r#"{"id":null}"#), "null");
        assert_eq!(extract_id("not json"), "null");
    }

    #[test]
    fn escape_covers_json_metacharacters() {
        assert_eq!(escape("a\"b\\c"), r#"a\"b\\c"#);
        assert_eq!(escape("a\nb\tc\rd"), r#"a\nb\tc\rd"#);
        assert_eq!(escape("\u{1}"), r#"\u0001"#);
    }

    #[test]
    fn escape_leaves_utf8_alone() {
        assert_eq!(escape("/tmp/ü/文件"), "/tmp/ü/文件");
    }
}

/// JSON string escaping for the fields this helper emits. Enough for paths and
/// user names; a real helper would reuse the daemon's serde types.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}
