//! Single instance on Windows is P10-5's job, and until then every launch is its
//! own process.
//!
//! The same seam and the same [`Claim`] shape as Unix, so `main` reads the same
//! on both platforms, and `Claim::Unavailable` says out loud that this launch got
//! no channel instead of pretending it owns one. A named pipe
//! (`\\.\pipe\acpilot-gui-<user>`) is the obvious implementation; it needs a
//! second thread reading `ConnectNamedPipe` in a loop and a message shape, which
//! is more than this slice is for — the device path it would cut is the same
//! 320–411 ms recorded in `docs/notes/gui-bench.md`, so it is worth doing, not
//! worth guessing at.

use futures::channel::mpsc::{UnboundedReceiver, unbounded};

/// What this launch found.
pub enum Claim {
    /// This process owns the channel and will be asked for windows.
    Primary(Instance),
    /// Another instance was running and has been asked to open a window.
    Secondary,
    /// No channel exists on this platform yet, so this launch stands alone.
    Unavailable,
}

/// The listening half, on a platform that has one.
pub struct Instance;

impl Instance {
    /// Requests from later launches.
    ///
    /// Nothing ever sends: [`claim`] cannot return [`Claim::Primary`] here, so
    /// this exists to keep the seam's shape identical on both platforms.
    pub fn requests(self) -> UnboundedReceiver<()> {
        unbounded().1
    }
}

/// Claim the instance channel. There is none on Windows yet.
#[must_use]
pub fn claim() -> Claim {
    Claim::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_reports_that_it_has_no_instance_channel() {
        assert!(
            matches!(claim(), Claim::Unavailable),
            "a platform without a channel must say so rather than claim one"
        );
    }
}
