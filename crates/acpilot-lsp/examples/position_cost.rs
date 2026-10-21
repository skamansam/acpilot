//! What does a position conversion cost on a large file?
//!
//! D9 asks whether to always speak UTF-16 or to negotiate UTF-8. The answer
//! depends on the cost of converting a cursor offset on every keystroke, and on
//! what rebuilding the line index costs for a 100k-line file. This prints both,
//! in nanoseconds per conversion, with enough iterations that the number is not
//! a scheduler artefact.
//!
//! ```text
//! cargo run --release -p acpilot-lsp --example position_cost
//! ```

use std::fmt::Write as _;
use std::time::Instant;

use acpilot_lsp::position::{LineIndex, PositionEncoding};

const LINES: usize = 100_000;
const ROUNDS: usize = 20;

fn main() {
    let text = document();
    let started = Instant::now();
    let index = LineIndex::new(&text);
    let build = started.elapsed();

    // One cursor position per line, roughly where a human leaves it: a quarter
    // of the way in, moved back to a character boundary.
    let mut offsets = Vec::with_capacity(LINES);
    let mut start = 0;
    for line in text.split('\n') {
        let mut offset = start + line.len() / 4;
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        offsets.push(offset);
        start += line.len() + 1;
    }

    let utf8 = measure(&text, &index, &offsets, PositionEncoding::Utf8);
    let utf16 = measure(&text, &index, &offsets, PositionEncoding::Utf16);

    let report = serde_json::json!({
        "lines": LINES,
        "bytes": text.len(),
        "rounds": ROUNDS,
        "conversions_per_encoding": ROUNDS * offsets.len(),
        "index_build_ms": build.as_secs_f64() * 1000.0,
        "utf8": utf8,
        "utf16": utf16,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("a Value is serialisable")
    );
}

fn document() -> String {
    let mut text = String::with_capacity(LINES * 64);
    for line in 0..LINES {
        match line % 10 {
            0 => {
                let _ = writeln!(
                    text,
                    "    let label_{line} = \"😀 日本語 e\u{301}\"; // コメント {line}"
                );
            }
            _ => {
                let _ = writeln!(
                    text,
                    "    let value_{line}: usize = compute({line}, \"ascii only\") + {line};"
                );
            }
        }
    }
    text
}

fn measure(
    text: &str,
    index: &LineIndex,
    offsets: &[usize],
    encoding: PositionEncoding,
) -> serde_json::Value {
    let mut checksum = 0_u64;

    let started = Instant::now();
    for _ in 0..ROUNDS {
        for &offset in offsets {
            let position = index
                .position(text, offset, encoding)
                .expect("a char boundary is a position");
            checksum += u64::from(position.character);
        }
    }
    let position_ns = started.elapsed().as_nanos() as f64 / (ROUNDS * offsets.len()) as f64;

    let positions: Vec<_> = offsets
        .iter()
        .map(|&offset| index.position(text, offset, encoding).unwrap())
        .collect();

    let started = Instant::now();
    for _ in 0..ROUNDS {
        for &position in &positions {
            checksum += index
                .offset(text, position, encoding)
                .expect("a position inside a line maps back") as u64;
        }
    }
    let offset_ns = started.elapsed().as_nanos() as f64 / (ROUNDS * offsets.len()) as f64;

    serde_json::json!({
        "position_ns": position_ns,
        "offset_ns": offset_ns,
        "checksum": checksum,
    })
}
