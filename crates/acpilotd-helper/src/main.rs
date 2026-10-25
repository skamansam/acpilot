//! Remote/WSL helper: the daemon's hands on a host it cannot reach directly.
//!
//! Empty in the foundation phase (P1). The service phase's task P3-9 turns this into a small stdio JSON-RPC shim that exposes
//! filesystem, PTY, spawn and (later) language-server operations, so a remote
//! workspace behaves identically to a local one. It is built static for musl and
//! embedded in the daemon, which uploads it on connect with a version handshake.

fn main() {
    eprintln!("acpilotd-helper: not implemented yet (P3-9)");
    std::process::exit(2);
}
