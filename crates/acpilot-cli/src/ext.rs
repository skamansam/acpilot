//! `acpilot ext` — the verbs that work without a daemon.
//!
//! Installing, verifying, and approving are filesystem operations; routing them
//! through `acpilotd` would mean an editor cannot be repaired when its daemon is
//! the thing that is broken. Everything here calls the same library the daemon
//! will, so there is one implementation, not a CLI copy of it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use acpilot_extensions::Activation;
use acpilot_extensions::ThemeAppearance;
use acpilot_extensions::activation::{
    ACTIVATION_FILE, ProfileExtensions, config_root, profile_dir_in,
};
use acpilot_extensions::error::ExtensionError;
use acpilot_extensions::manifest::validate_segment;
use acpilot_extensions::store::{Store, StoreStatus};
use acpilot_extensions::theme_import::from_tmtheme;

/// Where the store and profiles live for one command.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Extension store root.
    pub store_root: PathBuf,
    /// Acpilot config root; profiles live under `<config>/profiles`.
    pub config_root: PathBuf,
}

impl Paths {
    /// Use explicit paths when given, the platform defaults otherwise.
    pub fn resolve(
        store: Option<PathBuf>,
        config: Option<PathBuf>,
    ) -> Result<Self, ExtensionError> {
        let store_root = match store {
            Some(path) => path,
            None => Store::default_root()?,
        };
        let config_root = match config {
            Some(path) => path,
            None => config_root()?,
        };
        Ok(Self {
            store_root,
            config_root,
        })
    }

    /// The store this command operates on.
    #[must_use]
    pub fn store(&self) -> Store {
        Store::new(self.store_root.clone())
    }

    /// One profile directory under this config root.
    pub fn profile(&self, name: &str) -> Result<PathBuf, ExtensionError> {
        profile_dir_in(&self.config_root, name)
    }
}

/// Install a directory, printing what landed.
pub fn add(paths: &Paths, from: &Path) -> Result<String, ExtensionError> {
    let installed = paths.store().install_from_dir(from)?;
    Ok(format!(
        "installed {} ({}), checksum {}",
        installed.label(),
        installed.path.display(),
        installed.recorded.unwrap_or_default()
    ))
}

/// Rehash every installed version. Returns whether all of them are ready.
pub fn verify_all(paths: &Paths) -> Result<(bool, Vec<String>), ExtensionError> {
    let scan = paths.store().scan()?;
    if scan.is_empty() {
        return Ok((true, vec!["no extensions installed".to_owned()]));
    }

    let mut lines = Vec::new();
    let mut all_ready = true;
    let mut blocked = 0;
    for stored in &scan {
        match &stored.status {
            StoreStatus::Ready => lines.push(format!("ok      {}", stored.label())),
            StoreStatus::Blocked(reason) => {
                all_ready = false;
                blocked += 1;
                lines.push(format!("blocked {}: {reason}", stored.label()));
            }
        }
    }
    if blocked > 0 {
        lines.push(format!(
            "{blocked} of {} version{} blocked",
            scan.len(),
            if scan.len() == 1 { "" } else { "s" }
        ));
    }
    Ok((all_ready, lines))
}

/// Remove versions no profile activates. With `dry_run`, only report them.
pub fn prune(paths: &Paths, dry_run: bool) -> Result<Vec<String>, ExtensionError> {
    let keep = keep_set(paths)?;
    let store = paths.store();
    if dry_run {
        let doomed = store.plan_prune(&keep)?;
        if doomed.is_empty() {
            return Ok(vec!["nothing to prune".to_owned()]);
        }
        return Ok(doomed
            .into_iter()
            .map(|(id, version)| format!("would remove {id}@{version}"))
            .collect());
    }
    let removed = store.prune(&keep)?;
    if removed.is_empty() {
        return Ok(vec!["nothing to prune".to_owned()]);
    }
    Ok(removed
        .into_iter()
        .map(|(id, version)| format!("removed {id}@{version}"))
        .collect())
}

/// Approve an extension for one profile.
pub fn activate(paths: &Paths, profile: &str, id: &str) -> Result<String, ExtensionError> {
    validate_segment("profile", profile)?;
    let mut extensions = ProfileExtensions::open(paths.store(), paths.profile(profile)?)?;
    let changed = extensions.activate(id)?;
    if changed {
        Ok(format!("activated {id} in profile {profile}"))
    } else {
        Ok(format!("{id} was already active in profile {profile}"))
    }
}

/// Withdraw approval from one profile.
pub fn deactivate(paths: &Paths, profile: &str, id: &str) -> Result<String, ExtensionError> {
    validate_segment("profile", profile)?;
    let mut extensions = ProfileExtensions::open(paths.store(), paths.profile(profile)?)?;
    let changed = extensions.deactivate(id)?;
    if changed {
        Ok(format!("deactivated {id} in profile {profile}"))
    } else {
        Ok(format!("{id} was not active in profile {profile}"))
    }
}

/// Explain how a preset resolves for one profile.
pub fn why(paths: &Paths, profile: &str, id: &str) -> Result<String, ExtensionError> {
    validate_segment("profile", profile)?;
    let extensions = ProfileExtensions::open(paths.store(), paths.profile(profile)?)?;
    let resolution = extensions.effective().map_err(first_error)?;
    let mut out = resolution
        .registry
        .explain(id)
        .ok_or_else(|| ExtensionError::NotFound {
            what: format!("a preset or agent named {id}"),
        })?;
    for stored in &resolution.blocked {
        out.push_str(&format!(
            "warning: {} is active but blocked; it is not contributing\n",
            stored.label()
        ));
    }
    for missing in &resolution.missing {
        out.push_str(&format!(
            "warning: {missing} is active but not installed; it is not contributing\n"
        ));
    }
    Ok(out)
}

/// Convert a `.tmTheme` file into a theme extension and install it.
///
/// The files are written to a temporary directory and installed through the
/// same path as any other extension, so an imported theme is checksummed and can
/// be blocked like anything else.
pub fn import_theme(
    paths: &Paths,
    from: &Path,
    id: &str,
    version: &str,
    activate: Option<&str>,
) -> Result<Vec<String>, ExtensionError> {
    let text = fs::read_to_string(from).map_err(|source| ExtensionError::Io {
        path: from.display().to_string(),
        source,
    })?;
    let imported = from_tmtheme(id, version, &text)?;

    let staging = StagingSource::create()?;
    staging.write("extension.toml", &imported.manifest_toml)?;
    staging.write("theme.toml", &imported.theme_toml)?;
    let installed = paths.store().install_from_dir(staging.path())?;
    drop(staging);

    let appearance = match imported.appearance {
        ThemeAppearance::Dark => "dark",
        ThemeAppearance::Light => "light",
    };
    let mut lines = vec![format!(
        "installed {} as a {appearance} theme ({})",
        installed.label(),
        installed.path.display()
    )];
    lines.extend(
        imported
            .warnings
            .iter()
            .map(|warning| format!("warning: {warning}")),
    );

    match activate {
        Some(profile) => {
            validate_segment("profile", profile)?;
            let mut extensions = ProfileExtensions::open(paths.store(), paths.profile(profile)?)?;
            extensions.activate(&imported.id)?;
            lines.push(format!("activated {} in profile {profile}", imported.id));
        }
        None => lines.push(format!(
            "approve it with `acpilot ext activate {} --profile <name>`",
            imported.id
        )),
    }
    Ok(lines)
}

/// A throwaway directory for files that are about to be installed.
struct StagingSource {
    path: PathBuf,
}

static STAGING: AtomicU32 = AtomicU32::new(0);

impl StagingSource {
    fn create() -> Result<Self, ExtensionError> {
        let n = STAGING.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("acpilot-import-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).map_err(|source| ExtensionError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, name: &str, contents: &str) -> Result<(), ExtensionError> {
        let path = self.path.join(name);
        fs::write(&path, contents).map_err(|source| ExtensionError::Io {
            path: path.display().to_string(),
            source,
        })
    }
}

impl Drop for StagingSource {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The newest ready version of every id any profile activates.
fn keep_set(paths: &Paths) -> Result<BTreeSet<(String, String)>, ExtensionError> {
    let store = paths.store();
    let scan = store.scan()?;
    let newest = store.newest_ready(&scan);

    let mut keep = BTreeSet::new();
    let profiles = paths.config_root.join("profiles");
    let entries = match fs::read_dir(&profiles) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(keep),
        Err(source) => {
            return Err(ExtensionError::Io {
                path: profiles.display().to_string(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| ExtensionError::Io {
            path: profiles.display().to_string(),
            source,
        })?;
        if !entry.path().is_dir() {
            continue;
        }
        let activation = Activation::load(&entry.path().join(ACTIVATION_FILE))?;
        for id in &activation.active {
            if let Some(stored) = newest.get(id) {
                keep.insert((stored.id.clone(), stored.version.clone()));
            }
        }
    }
    Ok(keep)
}

fn first_error(errors: Vec<ExtensionError>) -> ExtensionError {
    errors
        .into_iter()
        .next()
        .unwrap_or_else(|| ExtensionError::NotFound {
            what: "a loadable extension set".to_owned(),
        })
}
