//! The tail of an agent's stderr, kept in a fixed ring.
//!
//! An agent owns stdout for JSON-RPC, so everything it wants to say to a human
//! arrives on stderr: a stack trace, a missing credential, the reason `npx`
//! failed. None of that is allowed to be unbounded, because a chatty agent would
//! otherwise grow the daemon's memory for the length of a session. The ring keeps
//! the *end*, which is where the reason usually is, and says when it dropped the
//! beginning rather than pretending the buffer is complete.
//!
//! The buffer is shared with the reader thread and may be read at any time, so a
//! status line does not have to wait for the process to exit.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Mutex;

/// How much stderr is kept per agent.
///
/// 64 KiB is a few hundred lines of a backtrace and small enough that a profile
/// with a dozen agents costs less than a megabyte. The engine phase's P2-5 terminal buffer is
/// larger because a terminal is the product surface; stderr is diagnostics.
pub const STDERR_CAPACITY: usize = 64 * 1024;

/// A bounded, thread-safe tail of a child's stderr.
#[derive(Debug)]
pub struct StderrRing {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    bytes: VecDeque<u8>,
    /// Whether bytes were discarded to stay inside `capacity`.
    truncated: bool,
    /// Whether the reader has seen EOF or given up. Set once.
    closed: bool,
}

impl StderrRing {
    /// A ring with the default capacity.
    pub fn new() -> Self {
        Self::with_capacity(STDERR_CAPACITY)
    }

    /// A ring with an explicit capacity, for tests and for a caller that knows
    /// what it is keeping.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Append a chunk, discarding the oldest bytes if it does not fit.
    pub fn push(&self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let mut inner = self.lock();
        if chunk.len() >= self.capacity {
            // The chunk alone is larger than the ring: only its tail can
            // survive, and everything already held is older than it.
            inner.bytes.clear();
            inner.bytes.extend(&chunk[chunk.len() - self.capacity..]);
            inner.truncated = true;
            return;
        }
        let overflow = (inner.bytes.len() + chunk.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            inner.bytes.drain(..overflow);
            inner.truncated = true;
        }
        inner.bytes.extend(chunk);
    }

    /// Mark the stream finished. Idempotent, and only the reader calls it.
    pub fn close(&self) {
        self.lock().closed = true;
    }

    /// The bytes currently held, lossily decoded.
    ///
    /// Lossy because stderr is not required to be UTF-8 and a diagnostic
    /// containing a replacement character is more useful than nothing.
    pub fn text(&self) -> String {
        let inner = self.lock();
        String::from_utf8_lossy(&inner.bytes.iter().copied().collect::<Vec<u8>>()).into_owned()
    }

    /// How many bytes are held.
    pub fn len(&self) -> usize {
        self.lock().bytes.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether anything was discarded from the beginning.
    pub fn truncated(&self) -> bool {
        self.lock().truncated
    }

    /// Whether the reader has finished.
    pub fn closed(&self) -> bool {
        self.lock().closed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Nothing in a push can panic while holding the lock: the only work is
        // deque manipulation. A poisoned lock is therefore still consistent.
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl Default for StderrRing {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for StderrRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_everything_that_fits() {
        let ring = StderrRing::with_capacity(16);
        ring.push(b"hello ");
        ring.push(b"world");
        assert_eq!(ring.text(), "hello world");
        assert_eq!(ring.len(), 11);
        assert!(!ring.truncated());
    }

    #[test]
    fn drops_the_beginning_and_says_so() {
        let ring = StderrRing::with_capacity(8);
        ring.push(b"0123456789");
        assert_eq!(ring.text(), "23456789");
        assert_eq!(ring.len(), 8);
        assert!(ring.truncated());
    }

    #[test]
    fn a_chunk_larger_than_the_ring_keeps_its_tail() {
        let ring = StderrRing::with_capacity(4);
        ring.push(b"old");
        ring.push(b"abcdef");
        assert_eq!(ring.text(), "cdef");
        assert!(ring.truncated());

        ring.push(b"0123456789");
        assert_eq!(ring.text(), "6789");
    }

    #[test]
    fn non_utf8_is_replaced_rather_than_dropped() {
        let ring = StderrRing::with_capacity(8);
        ring.push(&[b'o', b'k', b' ', 0xff]);
        assert!(ring.text().starts_with("ok "));
        assert!(ring.text().contains('\u{fffd}'));
    }

    #[test]
    fn close_is_idempotent_and_visible() {
        let ring = StderrRing::new();
        assert!(!ring.closed());
        ring.close();
        ring.close();
        assert!(ring.closed());
    }

    #[test]
    fn empty_chunks_do_not_count_as_truncation() {
        let ring = StderrRing::with_capacity(4);
        ring.push(b"");
        assert!(!ring.truncated());
        assert!(ring.is_empty());
    }
}
