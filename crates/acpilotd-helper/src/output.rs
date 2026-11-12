//! The helper's one stdout.
//!
//! Responses and notifications come from different threads — the request loop
//! answers, while a terminal's or language server's reader publishes — and they
//! share one pipe. Two writers with two locks can interleave a response with a
//! notification mid-line, which the daemon sees as two malformed JSON lines and
//! the caller sees as a reply that never arrived. One lock around the only stdout
//! is what makes "one message, one line" true.

use std::io::{self, Write};
use std::sync::{Mutex, PoisonError};

use serde_json::Value;

/// The process's stdout, serialised by message rather than by byte.
pub struct Output {
    out: Mutex<io::Stdout>,
}

impl Output {
    /// The helper's stdout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            out: Mutex::new(io::stdout()),
        }
    }

    /// Write one JSON line.
    ///
    /// A failed write is not reported: the daemon is the only reader, so a closed
    /// pipe means it is gone and the next read of stdin ends the process anyway.
    pub fn send(&self, value: &Value) {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(out, "{value}").and_then(|()| out.flush());
    }
}
