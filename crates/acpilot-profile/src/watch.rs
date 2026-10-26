//! Watching the files the settings cascade reads.
//!
//! More than one surface live-reloads: the terminal client recolours itself, and
//! the daemon announces a `prefs` frame to clients that cannot see its disk. They
//! have to agree about what "the settings changed" means — the debounce, an editor
//! that saves by rename, a config directory created after the program started — so
//! the watching lives here, beside the paths it watches, and not once per client.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// How long a save is given to stop being several writes.
///
/// One save is a truncate and a write, and often a rename. Debouncing here rather
/// than in a caller is what keeps a recolour from flickering through three
/// palettes on the way to the one that was chosen.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// Watch `paths` and yield once per settled change, forever.
///
/// The directories are watched rather than the files, because a settings file is
/// often created after the program starts and an editor that saves by rename would
/// otherwise stop being watched the first time it saved. Each event re-arms the
/// watch against the nearest directory that exists at that moment, which is how a
/// project that had no `.acpilot` at launch still gets a signal when one appears.
///
/// [`None`] when no directory could be watched — a settings file whose parents do
/// not exist, or a platform without a usable backend. A caller that gets [`None`]
/// has no live reload, which is the same outcome as a surface that never asked for
/// one, so this is not an error.
///
/// Only the files named are changes. A directory holds other things — a log, an
/// editor's scratch file, a second settings file — and a watcher that reported
/// every one of them would re-read the cascade, and recolour, for something that
/// is not the settings.
#[must_use]
pub fn changes(paths: impl IntoIterator<Item = PathBuf>) -> Option<Receiver<()>> {
    // Resolved before anything is compared: an event reports the path a
    // filesystem reports, and on a platform where `/tmp` is a link to `private/tmp`
    // that is not the path a caller asked about.
    let targets: Vec<PathBuf> = paths
        .into_iter()
        .filter_map(|path| resolved(&path))
        .collect();
    let (marks, watch_events) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        // The receiver is gone only when the watcher itself is, and the watcher is
        // moved into the thread below.
        let _ = marks.send(event);
    })
    .ok()?;

    let mut watched = Vec::new();
    arm(&mut watcher, &targets, &mut watched);
    if watched.is_empty() {
        return None;
    }

    let (changes, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(Ok(event)) = watch_events.recv() {
            if !touches(&targets, &event) {
                continue;
            }
            std::thread::sleep(DEBOUNCE);
            while watch_events.try_recv().is_ok() {}
            // Re-armed before the caller reads anything: the event that just
            // arrived may be the creation of the directory the file will be in.
            arm(&mut watcher, &targets, &mut watched);
            if changes.send(()).is_err() {
                // Nobody is listening any more; so is the reason to watch.
                return;
            }
        }
    });
    Some(receiver)
}

/// Whether an event is about one of the watched files rather than its neighbours.
///
/// Two shapes count: the file itself, and any directory that is an ancestor of it —
/// the second is how the creation of `.acpilot` is a change to the `config.toml`
/// that is about to be written inside it. Everything else in the same directory is
/// somebody else's business.
fn touches(targets: &[PathBuf], event: &notify::Event) -> bool {
    is_a_change(event)
        && event.paths.iter().any(|changed| {
            targets
                .iter()
                .any(|target| target == changed || target.starts_with(changed))
        })
}

/// Whether the event says the file was written, rather than looked at.
///
/// Reading is an event on Linux and macOS, and a caller reacts to a change by
/// reading the file — so a watcher that counted reads would answer every read with
/// another `Read`, forever. A close-for-write is still a change: it is how an
/// editor's save arrives on some platforms.
fn is_a_change(event: &notify::Event) -> bool {
    use notify::event::{AccessKind, AccessMode};

    !matches!(
        event.kind,
        notify::EventKind::Access(
            AccessKind::Read | AccessKind::Open(_) | AccessKind::Close(AccessMode::Read)
        )
    )
}

/// `path` with the nearest existing ancestor it has made absolute.
///
/// The file may not exist yet, so it cannot be canonicalised itself; the directory
/// it would live in can, and the rest of the path is appended unchanged.
fn resolved(path: &Path) -> Option<PathBuf> {
    let directory = nearest_directory(path)?;
    let base = std::fs::canonicalize(&directory).ok()?;
    Some(base.join(path.strip_prefix(&directory).ok()?))
}

/// Watch every directory `paths` live in that is not watched yet.
fn arm(watcher: &mut impl notify::Watcher, paths: &[PathBuf], watched: &mut Vec<PathBuf>) {
    for directory in paths.iter().filter_map(|path| nearest_directory(path)) {
        // Watched by name, matched by file: a directory is the only thing a
        // filesystem will watch, and `touches` is what narrows it back down.
        if watched.contains(&directory) {
            continue;
        }
        // A directory that cannot be watched is not a failure: the other one may
        // be, and a caller with one of two paths still has live reload.
        if watcher
            .watch(&directory, notify::RecursiveMode::NonRecursive)
            .is_ok()
        {
            watched.push(directory);
        }
    }
}

/// The directory to watch for `path`: its own, or the nearest ancestor that exists.
///
/// A settings file is not necessarily there when the program starts — a project
/// that has never been configured has no `.acpilot` directory — and watching a
/// path that does not exist fails rather than waiting for it. The nearest existing
/// ancestor exists by construction, so a creation below it is the event that
/// re-arms the watch closer to the file.
fn nearest_directory(path: &Path) -> Option<PathBuf> {
    let mut directory = path.parent()?;
    loop {
        if directory.is_dir() {
            return Some(directory.to_path_buf());
        }
        directory = directory.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settings_file_that_is_not_there_yet_is_watched_at_the_nearest_directory() {
        let directory = tempfile::tempdir().expect("temp dir");
        let missing = directory.path().join(".acpilot").join("config.toml");
        assert_eq!(
            nearest_directory(&missing).as_deref(),
            Some(directory.path()),
            "the project root, because `.acpilot` does not exist yet"
        );
    }

    #[test]
    fn a_save_is_one_signal() {
        let directory = tempfile::tempdir().expect("temp dir");
        let settings = directory.path().join("config.toml");
        let changes = changes([settings.clone()]).expect("the directory is watchable");

        // Written after the watch is armed, and twice, the way an editor saves:
        // the point is that the caller is told once, not twice.
        std::fs::write(&settings, "[preferences]\ntheme = \"acpilot.paper\"\n").expect("write");
        assert!(
            changes.recv_timeout(Duration::from_secs(10)).is_ok(),
            "a save reaches the caller"
        );
        assert_eq!(
            changes.recv_timeout(DEBOUNCE / 2),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "and the second write inside the window was folded into it"
        );
    }

    #[test]
    fn reading_the_settings_is_not_a_change() {
        // The caller reads the file when it is told it changed. If a read were a
        // change, every announcement would produce the next one — which is what
        // watching a daemon serve `prefs` frames looked like: a stream of them,
        // two hundred milliseconds apart, for one save that never happened again.
        let directory = tempfile::tempdir().expect("temp dir");
        let settings = directory.path().join("config.toml");
        std::fs::write(&settings, "[preferences]\ntheme = \"acpilot.paper\"\n").expect("write");
        let changes = changes([settings.clone()]).expect("the directory is watchable");

        std::fs::write(&settings, "[preferences]\ntheme = \"acpilot.paper\"\n").expect("write");
        assert!(
            changes.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the save is a change"
        );

        // What every caller does with the signal, then does not hear back from.
        std::fs::read_to_string(&settings).expect("read");
        assert_eq!(
            changes.recv_timeout(DEBOUNCE * 3),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "reading the settings is not a change to them"
        );
    }

    #[test]
    fn a_neighbouring_file_is_not_a_change_to_the_settings() {
        // A watched directory holds other files. If a log written next to the
        // settings counts as a change, a daemon logging into its own config
        // directory announces preferences on every line — which is how this was
        // found, watching the daemon's log grow.
        let directory = tempfile::tempdir().expect("temp dir");
        let settings = directory.path().join("config.toml");
        let changes = changes([settings.clone()]).expect("the directory is watchable");

        std::fs::write(directory.path().join("daemon.log"), "not the settings\n").expect("write");
        assert_eq!(
            changes.recv_timeout(DEBOUNCE * 3),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "a log beside the settings is not a settings change"
        );

        std::fs::write(&settings, "[preferences]\ntheme = \"acpilot.paper\"\n").expect("write");
        assert!(
            changes.recv_timeout(Duration::from_secs(10)).is_ok(),
            "and the settings file itself still is"
        );
    }

    #[test]
    fn a_file_in_a_directory_that_does_not_exist_yet_is_still_a_change() {
        let directory = tempfile::tempdir().expect("temp dir");
        let settings = directory.path().join(".acpilot").join("config.toml");
        let changes = changes([settings.clone()]).expect("the parent is watchable");

        std::fs::create_dir_all(settings.parent().expect("a parent")).expect("mkdir");
        std::fs::write(&settings, "[preferences]\ntheme = \"acpilot.paper\"\n").expect("write");
        assert!(
            changes.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the directory appearing is the event, and the file written below it is the change"
        );
    }
}
