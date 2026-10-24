//! One instance per user, and a socket that carries a second launch's request.
//!
//! The socket lives in `$XDG_RUNTIME_DIR`, which the session already guarantees is
//! private to the user (`0700`) and cleared on logout — so the path cannot be
//! squatted by another account, and a reboot cannot leave a socket that outlives
//! the process it names. macOS has no XDG runtime directory and `$TMPDIR` is the
//! per-user equivalent there. When neither is set there is no safe place to put
//! one, and the honest answer is to stand alone rather than guess `/tmp`, which
//! is world-writable and could be pre-created by another account.
//!
//! The protocol is the connection. A launch connects and writes a short line; the
//! primary accepts, drops the stream and opens a window. Nothing is parsed,
//! which means nothing in the protocol can be wrong, and a stale socket from a
//! crashed instance fails at `connect` and is replaced at `bind`.
//!
//! Known race, and its bound: two launches that both fail `connect` before either
//! `bind`s can each conclude they are the primary, and the second removes the
//! first's socket. The window is the few hundred microseconds between `connect`
//! and `bind`; the result is two processes, which is exactly what every launch
//! did before this existed — single instance is an optimisation here, not a
//! safety property. A lock file taken with `try_lock` before the bind is the fix
//! when P10-5 does deep links, because then a lost request stops being cosmetic.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};

/// The socket's file name inside the runtime directory.
const SOCKET: &str = "acpilot-gui.sock";

/// What this launch found.
pub enum Claim {
    /// This process owns the socket and will be asked for windows.
    Primary(Instance),
    /// Another instance was running and has been asked to open a window.
    Secondary,
    /// There is nowhere safe for a socket, so this launch stands alone.
    Unavailable,
}

/// The listening half, owned by the first process.
pub struct Instance {
    listener: UnixListener,
}

impl Instance {
    /// Turn accepted connections into requests, one per launch.
    ///
    /// The accept loop runs on its own thread and sends into a channel the gpui
    /// executor awaits, rather than polling a non-blocking listener from a timer:
    /// a 250 ms poll would make every activation slower than the window it opens,
    /// and a 40 ms poll would wake a laptop 25 times a second forever.
    pub fn requests(self) -> UnboundedReceiver<()> {
        let (requests, incoming) = unbounded();
        std::thread::spawn(move || {
            for connection in self.listener.incoming() {
                if connection.is_err() {
                    // The listener itself failed; there is nothing left to serve.
                    break;
                }
                if requests.unbounded_send(()).is_err() {
                    // The app is gone, so the listener may as well be.
                    break;
                }
            }
        });
        incoming
    }
}

/// Claim the instance socket, or hand this launch to the instance that owns it.
#[must_use]
pub fn claim() -> Claim {
    let Some(path) = socket_path(runtime_dir().as_deref()) else {
        return Claim::Unavailable;
    };
    match claim_at(&path) {
        Ok(claim) => claim,
        // A runtime directory we cannot write is not a reason to refuse to start.
        // Standing alone costs one process, which is what every launch did before
        // this existed.
        Err(_) => Claim::Unavailable,
    }
}

/// The per-user runtime directory this platform offers, if any.
///
/// `XDG_RUNTIME_DIR` first, because that is the one with a defined lifetime; then,
/// on macOS only, `TMPDIR`, which the system creates per user. No other fallback:
/// a shared `/tmp` path is squattable, and losing single-instance is cheaper than
/// connecting to someone else's socket.
fn runtime_dir() -> Option<std::ffi::OsString> {
    let named = |name: &str| std::env::var_os(name).filter(|dir| !dir.is_empty());
    if let Some(dir) = named("XDG_RUNTIME_DIR") {
        return Some(dir);
    }
    #[cfg(target_os = "macos")]
    return named("TMPDIR");
    #[cfg(not(target_os = "macos"))]
    None
}

/// The socket path for a runtime directory, or `None` when there is not one.
#[must_use]
pub fn socket_path(runtime_dir: Option<&OsStr>) -> Option<PathBuf> {
    let directory = runtime_dir.filter(|directory| !directory.is_empty())?;
    Some(PathBuf::from(directory).join(SOCKET))
}

/// Claim a specific socket. Split out so the tests can use a temporary directory.
pub fn claim_at(path: &Path) -> io::Result<Claim> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if let Ok(mut stream) = UnixStream::connect(path) {
        // The connection is the request; the line makes it explicit for anyone
        // reading the socket, and costs one syscall.
        let _ = stream.write_all(b"open\n");
        return Ok(Claim::Secondary);
    }

    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        // A crashed instance leaves its socket file behind, and `bind` refuses to
        // reuse one. Removing it is safe here: nothing is listening, which is
        // exactly what the failed `connect` above proved.
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            std::fs::remove_file(path)?;
            UnixListener::bind(path)?
        }
        Err(error) => return Err(error),
    };

    Ok(Claim::Primary(Instance { listener }))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::{Duration, Instant};

    use futures::channel::mpsc::TryRecvError;

    use super::*;

    /// Wait up to two seconds for the accept thread to deliver one request.
    fn accepted(requests: &mut UnboundedReceiver<()>) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match requests.try_recv() {
                Ok(()) => return true,
                Err(TryRecvError::Closed) => return false,
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        false
    }

    #[test]
    fn the_first_claim_owns_the_socket_and_the_second_hands_over() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join(SOCKET);

        let primary = claim_at(&path).expect("the first claim succeeds");
        assert!(
            matches!(primary, Claim::Primary(_)),
            "the first launch owns the socket"
        );

        let second = claim_at(&path).expect("the second claim succeeds");
        assert!(
            matches!(second, Claim::Secondary),
            "the second launch hands over instead of starting a process"
        );
    }

    #[test]
    fn a_secondary_claim_reaches_the_primary_as_one_request() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join(SOCKET);

        let Claim::Primary(instance) = claim_at(&path).expect("primary") else {
            panic!("the first claim owns the socket");
        };
        let mut requests = instance.requests();

        // No runtime needed to observe the accept thread: polling the channel is
        // the same thing the executor does, and the wait is bounded so a broken
        // hand-over fails instead of hanging the suite.
        claim_at(&path).expect("secondary");
        assert!(accepted(&mut requests), "the launch reached the primary");
    }

    #[test]
    fn a_socket_left_by_a_crashed_instance_is_replaced() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join(SOCKET);

        // A file that is not a listening socket is what a killed process leaves:
        // `connect` refuses it and `bind` reports the name as taken.
        std::fs::write(&path, b"").expect("write the stale path");
        let claim = claim_at(&path).expect("the stale socket is replaced");
        assert!(
            matches!(claim, Claim::Primary(_)),
            "a stale file must not stop the app from starting"
        );
    }

    #[test]
    fn there_is_no_socket_path_without_a_runtime_directory() {
        assert_eq!(socket_path(None), None);
        assert_eq!(socket_path(Some(OsStr::new(""))), None);
        assert_eq!(
            socket_path(Some(OsStr::new("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000").join(SOCKET))
        );
    }

    #[test]
    fn the_socket_is_named_for_the_app_and_sits_where_it_was_put() {
        let path = socket_path(Some(&OsString::from("/run/user/1000"))).expect("a path");
        assert!(path.ends_with(SOCKET));
        assert!(path.starts_with("/run/user/1000"));
    }
}
