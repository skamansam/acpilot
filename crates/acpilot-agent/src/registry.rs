//! The registry: `agents.toml` read once, replaced whole when it changes.
//!
//! Two rules make this more than a `HashMap` around a parser.
//!
//! **A reload is atomic.** The new file is parsed and validated into a fresh
//! `Vec<Arc<AgentSpec>>`; only a complete success replaces the old one. A
//! half-broken save therefore leaves every agent exactly as it was, which
//! matters because the file is editable while sessions are running — a typo
//! should cost a red line in `acpilot agents ls`, not three dead sessions.
//!
//! **The last failure is remembered, not thrown away.** [`Registry::last_error`]
//! keeps the reason the file was refused, so a UI can show it after the fact
//! instead of only while the edit is happening.
//!
//! Watching is a directory, not the file: editors that save atomically write a
//! temporary file and `rename` it over the target, which replaces the inode. A
//! watch on the file itself survives exactly one such save.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::error::AgentError;
use crate::manifest::{self, AgentSpec, MANIFEST_FILE};
use crate::secret::SecretResolver;

/// How long to keep collecting events after the first one, so one save that
/// produces five events causes one parse.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(50);

/// The agents of one profile, and the file they came from.
pub struct Registry {
    path: PathBuf,
    resolver: Arc<dyn SecretResolver>,
    state: RwLock<State>,
}

#[derive(Debug, Default)]
struct State {
    agents: Vec<Arc<AgentSpec>>,
    /// Bumped on every change, so a UI can tell whether it has to refresh
    /// without comparing every spec.
    revision: u64,
    /// The last reason a load failed, kept after the snapshot survives it.
    last_error: Option<String>,
}

impl std::fmt::Debug for Registry {
    /// Deliberately not every spec: the interesting facts about a registry are
    /// which file it reads and how far along it is, and the resolver is not
    /// printable by design.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("path", &self.path)
            .field("revision", &self.revision())
            .field("agents", &self.agents().len())
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// Read and validate the manifest at `path`.
    ///
    /// Fails if the file cannot be read or does not validate: there is no
    /// "partially loaded" state to start from. A daemon that wants to start with
    /// a broken manifest has to decide that explicitly, and the manifest type
    /// gives it nothing to decide it with.
    pub fn load(
        path: impl Into<PathBuf>,
        resolver: Arc<dyn SecretResolver>,
    ) -> Result<Self, AgentError> {
        let path = path.into();
        let agents = read(&path, resolver.as_ref())?
            .into_iter()
            .map(Arc::new)
            .collect();
        Ok(Self {
            path,
            resolver,
            state: RwLock::new(State {
                agents,
                revision: 1,
                last_error: None,
            }),
        })
    }

    /// Re-read the manifest, replacing the snapshot only if it validates.
    ///
    /// Returns whether anything changed: `false` when the file is the same as
    /// what is loaded (a save that only touched whitespace, or a duplicate
    /// watcher event), which is the common case for an editor's write.
    pub fn reload(&self) -> Result<bool, AgentError> {
        match read(&self.path, self.resolver.as_ref()) {
            Ok(specs) => {
                let agents: Vec<Arc<AgentSpec>> = specs.into_iter().map(Arc::new).collect();
                let mut state = self.write();
                state.last_error = None;
                if state.agents == agents {
                    return Ok(false);
                }
                state.agents = agents;
                state.revision += 1;
                Ok(true)
            }
            Err(error) => {
                self.write().last_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Every agent, in the order the file declares them.
    pub fn agents(&self) -> Vec<Arc<AgentSpec>> {
        self.read().agents.clone()
    }

    /// One agent by id.
    pub fn get(&self, id: &str) -> Option<Arc<AgentSpec>> {
        self.read()
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .cloned()
    }

    /// How many times the snapshot has changed. Starts at 1.
    pub fn revision(&self) -> u64 {
        self.read().revision
    }

    /// Why the most recent reload failed, if it did.
    ///
    /// Outlives the failure on purpose: the snapshot is still serving the last
    /// good file, and the reason it is stale is what a status line should say.
    pub fn last_error(&self) -> Option<String> {
        self.read().last_error.clone()
    }

    /// The manifest this registry reads.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reload whenever the manifest changes on disk.
    ///
    /// The returned value keeps the watcher alive; dropping it stops watching.
    /// A failed reload is not fatal and does not stop the watcher — the next
    /// save gets another try.
    pub fn watch(self: &Arc<Self>) -> Result<AgentWatcher, AgentError> {
        let directory = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let file_name = self.path.file_name().map_or_else(
            || OsStr::new(MANIFEST_FILE).to_os_string(),
            OsStr::to_os_string,
        );

        let (sender, receiver) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            // A send error means the registry is gone; there is nothing to tell.
            let _ = sender.send(event);
        })
        .map_err(|error| self.watch_error(error))?;

        watcher
            .watch(&directory, RecursiveMode::NonRecursive)
            .map_err(|error| self.watch_error(error))?;

        let registry = Arc::clone(self);
        let thread = thread::Builder::new()
            .name("agents-toml".to_owned())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    match event {
                        Ok(event) if touches(&event, &file_name) => {}
                        // Another file in the profile directory changed.
                        Ok(_) => continue,
                        // The watcher itself failed; the next event may still
                        // arrive, and a reload that fails records why.
                        Err(_) => continue,
                    }
                    while receiver.recv_timeout(RELOAD_DEBOUNCE).is_ok() {}
                    let _ = registry.reload();
                }
            })
            .map_err(|error| self.watch_error(error))?;

        Ok(AgentWatcher {
            watcher: Some(watcher),
            thread: Some(thread),
        })
    }

    fn watch_error(&self, error: impl std::fmt::Display) -> AgentError {
        AgentError::Watch {
            path: self.path.display().to_string(),
            message: error.to_string(),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        // A panic while the lock was held left the snapshot consistent: the
        // only writes are whole-struct assignments.
        self.state
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// Stops watching when dropped.
#[derive(Debug)]
pub struct AgentWatcher {
    watcher: Option<RecommendedWatcher>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for AgentWatcher {
    fn drop(&mut self) {
        // Dropping the watcher drops the closure that owns the sender, which
        // ends the loop in the watcher thread.
        self.watcher = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn touches(event: &Event, file_name: &OsStr) -> bool {
    event
        .paths
        .iter()
        .any(|path| path.file_name() == Some(file_name))
}

fn read(path: &Path, resolver: &dyn SecretResolver) -> Result<Vec<AgentSpec>, AgentError> {
    let text = fs::read_to_string(path).map_err(|source| AgentError::Io {
        path: path.display().to_string(),
        source,
    })?;
    manifest::parse(path, &text, resolver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_can_be_shared_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Registry>();
    }
}
