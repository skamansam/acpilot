//! `acpilot ext` against the signed index: search, add, update, remove, list, show.
//!
//! The index answers which version exists and which bytes it is. The store answers
//! whether those bytes arrived and whether they still hash to what was recorded.
//! This module is the seam between them, and it never writes until both answers
//! agree.
//!
//! The default index is compiled into the binary ([`index::DEFAULT_INDEX`]), so a
//! build's listings are the ones its release shipped with. `--index` names another
//! file or URL for a private registry. It is verified against the maintainer's key
//! plus `registry.trusted_keys`, which is read from the global config only — a
//! workspace that could add a key could sign its own extensions.
//!
//! Every verb returns a [`Report`]: the lines a terminal prints and the document
//! `--json` prints. One function builds both, so a script and a person cannot be
//! told different things about the same store.

use std::collections::BTreeMap;
use std::fs;

use acpilot_extensions::StoreStatus;
use acpilot_extensions::ThemeRegistry;
use acpilot_extensions::activation::{ACTIVATION_FILE, Activation};
use acpilot_extensions::index::{self, Index, IndexEntry, TrustedKey};
use acpilot_extensions::manifest::{validate_namespaced_id, version_tuple};
use acpilot_extensions::store::StoredExtension;
use acpilot_proto::extension::{ExtensionDetail, ExtensionListing, ExtensionSummary, ThemeSummary};

use crate::ext::Paths;

/// One verb's answer: what a terminal prints and what `--json` prints.
pub struct Report {
    /// Lines, in the order a person reads them.
    pub lines: Vec<String>,
    /// The same facts as one document.
    pub json: serde_json::Value,
}

impl Report {
    fn new(lines: Vec<String>, json: serde_json::Value) -> Self {
        Self { lines, json }
    }
}

/// The trust roots a command may verify an index against.
fn trusted_keys(paths: &Paths) -> Result<Vec<TrustedKey>, String> {
    let file = paths.config_root.join(acpilot_profile::CONFIG_FILE);
    if !file.is_file() {
        return Ok(Vec::new());
    }
    let settings = acpilot_profile::settings::Settings::load(Some(&file), None)
        .map_err(|error| format!("{}: {error}", file.display()))?;
    Ok(settings
        .registry
        .trusted_keys
        .into_iter()
        .map(|key| TrustedKey {
            id: key.id,
            public_key: key.public_key,
        })
        .collect())
}

/// Read, verify, and parse an index.
fn load_index(paths: &Paths, source: Option<&str>) -> Result<Index, String> {
    let trusted = trusted_keys(paths)?;
    let (label, bytes, signature) = match source {
        None => (
            index::INDEX_FILE.to_owned(),
            index::DEFAULT_INDEX.to_vec(),
            index::DEFAULT_SIGNATURE.to_vec(),
        ),
        Some(source) if is_url(source) => {
            let bytes = fetch(source)?;
            let signature_url = format!("{source}{}", index::SIGNATURE_SUFFIX);
            let signature = fetch(&signature_url)?;
            (source.to_owned(), bytes, signature)
        }
        Some(path) => {
            let bytes = fs::read(path).map_err(|error| format!("{path}: {error}"))?;
            let signature_path = format!("{path}{}", index::SIGNATURE_SUFFIX);
            let signature =
                fs::read(&signature_path).map_err(|error| format!("{signature_path}: {error}"))?;
            (path.to_owned(), bytes, signature)
        }
    };
    index::verify(&label, &bytes, &signature, &trusted).map_err(|error| error.to_string())?;
    Index::parse(&label, &bytes).map_err(|error| error.to_string())
}

fn is_url(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("http://")
}

/// One GET, for an index or an archive.
///
/// The user agent is required by GitHub, not politeness. Without it the answer is
/// a refusal that reads like a network fault. A redirect is followed, which is
/// what makes a `releases/download` URL answer with bytes.
fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("no runtime for the download: {error}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent(format!("acpilot/{}", acpilot_core::VERSION))
            .build()
            .map_err(|error| format!("no HTTP client: {error}"))?;
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| format!("{url}: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("{url}: HTTP {status}"));
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| format!("{url}: {error}"))
    })
}

/// Download one listing's archive and install it.
fn install_entry(paths: &Paths, entry: &IndexEntry) -> Result<StoredExtension, String> {
    let bytes = fetch(&entry.download)?;
    paths
        .store()
        .install_from_archive(&bytes, entry)
        .map_err(|error| error.to_string())
}

/// `acpilot ext search [QUERY]`.
pub fn search(paths: &Paths, source: Option<&str>, query: Option<&str>) -> Result<Report, String> {
    let index = load_index(paths, source)?;
    let entries = index.search(query.unwrap_or_default());
    let json = serde_json::json!({ "extensions": entries });
    if entries.is_empty() {
        let line = match query {
            Some(query) => format!("no extension in this index matches {query:?}"),
            None => "this index lists no extensions".to_owned(),
        };
        return Ok(Report::new(vec![line], json));
    }
    let mut lines = Vec::new();
    for entry in entries {
        lines.push(format!(
            "{} {} ({})",
            entry.id,
            entry.version,
            index::kind_name(entry.kind)
        ));
        lines.push(format!("  {}", entry.download));
    }
    Ok(Report::new(lines, json))
}

/// `acpilot ext add <ID> [--version]`, or `--from <DIR>` for a checkout.
///
/// `activate` names the profile to approve the id for once the archive is in the
/// store, so the install and the approval are one command. The activation is
/// written after the store accepted the bytes: a profile never names something the
/// store would refuse to load.
pub fn add(
    paths: &Paths,
    source: Option<&str>,
    id: &str,
    version: Option<&str>,
    activate: Option<&str>,
) -> Result<Report, String> {
    validate_namespaced_id("extension id", id).map_err(|error| error.to_string())?;
    let index = load_index(paths, source)?;
    let entry = match version {
        Some(version) => index.entry(id, version).ok_or_else(|| {
            format!("{id}@{version} is not in this index; `acpilot ext search {id}` lists what is")
        })?,
        None => index.newest(id).ok_or_else(|| {
            format!("{id} is not in this index; `acpilot ext search` lists what it offers")
        })?,
    };
    if !entry.requires(acpilot_core::VERSION) {
        return Err(format!(
            "{} requires acpilot {} or newer; this build is {}",
            entry.label(),
            entry.min_acpilot.as_deref().unwrap_or("?"),
            acpilot_core::VERSION
        ));
    }
    let installed = install_entry(paths, entry)?;
    if let Some(profile) = activate {
        crate::ext::activate(paths, profile, &entry.id).map_err(|error| error.to_string())?;
    }
    let profiles = activating(paths, &entry.id)?;
    let summary = installed.summary(&profiles);
    let lines = vec![
        format!(
            "installed {} from {} (checksum {})",
            installed.label(),
            entry.download,
            installed.recorded.clone().unwrap_or_default()
        ),
        match activate {
            Some(profile) => format!("activated {} in profile {profile}", entry.id),
            None => format!(
                "approve it for a profile with `acpilot ext activate {} --profile <name>`",
                entry.id
            ),
        },
    ];
    Ok(Report::new(
        lines,
        serde_json::json!({ "extension": summary, "activated_in": activate }),
    ))
}

/// `acpilot ext update [ID]`: install the newest listed version of what is here.
pub fn update(paths: &Paths, source: Option<&str>, only: Option<&str>) -> Result<Report, String> {
    let index = load_index(paths, source)?;
    let scan = paths.store().scan().map_err(|error| error.to_string())?;
    let mut installed: BTreeMap<String, String> = BTreeMap::new();
    for stored in &scan {
        if stored.version.is_empty() {
            continue;
        }
        let replace = installed
            .get(&stored.id)
            .is_none_or(|current| version_tuple(&stored.version) > version_tuple(current));
        if replace {
            installed.insert(stored.id.clone(), stored.version.clone());
        }
    }

    let ids = match only {
        Some(id) => {
            validate_namespaced_id("extension id", id).map_err(|error| error.to_string())?;
            if !installed.contains_key(id) {
                return Err(format!("{id} is not installed; use `acpilot ext add {id}`"));
            }
            vec![id.to_owned()]
        }
        None => installed.keys().cloned().collect(),
    };
    if ids.is_empty() {
        return Ok(Report::new(
            vec!["no extensions installed".to_owned()],
            serde_json::json!({ "extensions": [] }),
        ));
    }

    let mut lines = Vec::new();
    let mut changes = Vec::new();
    for id in ids {
        let current = installed
            .get(&id)
            .expect("the id came from the installed set")
            .clone();
        let Some(entry) = index.newest(&id) else {
            lines.push(format!(
                "{id} {current} is installed but this index does not list it; leaving it alone"
            ));
            changes.push(serde_json::json!({
                "id": id,
                "from": current,
                "to": null,
                "status": "not_listed",
            }));
            continue;
        };
        if version_tuple(&entry.version) <= version_tuple(&current) {
            lines.push(format!("{id} {current} is up to date"));
            changes.push(serde_json::json!({
                "id": id,
                "from": current,
                "to": current,
                "status": "up_to_date",
            }));
            continue;
        }
        let stored = install_entry(paths, entry)?;
        lines.push(format!(
            "updated {id} from {current} to {} (from {})",
            stored.version, entry.download
        ));
        changes.push(serde_json::json!({
            "id": id,
            "from": current,
            "to": stored.version,
            "status": "updated",
        }));
    }
    Ok(Report::new(
        lines,
        serde_json::json!({ "extensions": changes }),
    ))
}

/// `acpilot ext remove <ID>`: every installed version of one id.
pub fn remove(paths: &Paths, id: &str) -> Result<Report, String> {
    validate_namespaced_id("extension id", id).map_err(|error| error.to_string())?;
    let profiles = activating(paths, id)?;
    if !profiles.is_empty() {
        return Err(format!(
            "{id} is active in {}; deactivate it there first, so no profile is left asking for \
             something that is gone",
            profiles.join(", ")
        ));
    }
    let versions: Vec<String> = paths
        .store()
        .scan()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|stored| stored.id == id && !stored.version.is_empty())
        .map(|stored| stored.version)
        .collect();
    if versions.is_empty() {
        return Err(format!("{id} is not installed"));
    }
    let mut lines = Vec::new();
    for version in &versions {
        paths
            .store()
            .remove(id, version)
            .map_err(|error| error.to_string())?;
        lines.push(format!("removed {id}@{version}"));
    }
    Ok(Report::new(
        lines,
        serde_json::json!({ "id": id, "versions": versions }),
    ))
}

/// `acpilot ext list`: what is on disk, and whether the store would load it.
pub fn list(paths: &Paths) -> Result<Report, String> {
    let scan = paths.store().scan().map_err(|error| error.to_string())?;
    if scan.is_empty() {
        return Ok(Report::new(
            vec!["no extensions installed".to_owned()],
            serde_json::json!({ "extensions": [] }),
        ));
    }
    let active = active_profiles(paths)?;
    let mut lines = Vec::new();
    for stored in &scan {
        let mut line = match &stored.status {
            StoreStatus::Ready => format!("ok      {}", stored.label()),
            StoreStatus::Blocked(reason) => format!("blocked {}: {reason}", stored.label()),
        };
        if stored.is_linked() {
            line.push_str(" (linked)");
        }
        if let Some(profiles) = active.get(&stored.id) {
            line.push_str(&format!("  [active: {}]", profiles.join(", ")));
        }
        lines.push(line);
    }
    let mut rows: Vec<ExtensionSummary> = scan
        .iter()
        .map(|stored| {
            stored.summary(
                active
                    .get(&stored.id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )
        })
        .collect();
    rows.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then(version_tuple(&left.version).cmp(&version_tuple(&right.version)))
    });
    Ok(Report::new(
        lines,
        serde_json::json!({ "extensions": rows }),
    ))
}

/// `acpilot ext show <ID>`: what the index offers beside what is installed.
pub fn show(paths: &Paths, source: Option<&str>, id: &str) -> Result<Report, String> {
    validate_namespaced_id("extension id", id).map_err(|error| error.to_string())?;
    let index = load_index(paths, source)?;
    let mut lines = Vec::new();
    let mut listed = Vec::new();

    let offered = index.versions(id);
    match offered.last() {
        Some(newest) => {
            lines.push(format!(
                "{id} {}, published by {}",
                newest.version, newest.publisher
            ));
            lines.push(format!("  kind   {}", index::kind_name(newest.kind)));
            for entry in &offered {
                lines.push(format!(
                    "  {:9} {:>10} bytes  {}",
                    entry.version, entry.size, entry.download
                ));
            }
        }
        None => lines.push(format!("{id} is not in this index")),
    }
    for entry in &offered {
        listed.push(ExtensionListing {
            version: entry.version.clone(),
            kind: entry.kind,
            publisher: entry.publisher.clone(),
            checksum: entry.checksum.clone(),
            size: entry.size,
            download: entry.download.clone(),
            min_acpilot: entry.min_acpilot.clone(),
        });
    }

    let scan = paths.store().scan().map_err(|error| error.to_string())?;
    let mut installed: Vec<&StoredExtension> =
        scan.iter().filter(|stored| stored.id == id).collect();
    installed.sort_by_key(|stored| version_tuple(&stored.version));
    if installed.is_empty() {
        lines.push("not installed".to_owned());
    } else {
        lines.push("installed:".to_owned());
        for stored in &installed {
            match &stored.status {
                StoreStatus::Ready => lines.push(if stored.is_linked() {
                    format!("  {} ready (linked)", stored.version)
                } else {
                    format!("  {} ready", stored.version)
                }),
                StoreStatus::Blocked(reason) => {
                    lines.push(format!("  {} blocked: {reason}", stored.version));
                }
            }
        }
    }

    let profiles = activating(paths, id)?;
    if !profiles.is_empty() {
        lines.push(format!("active in: {}", profiles.join(", ")));
    }

    let json = match installed.last().copied() {
        Some(newest) => serde_json::json!({
            "extension": ExtensionDetail {
                summary: newest.summary(&profiles),
                contributions: newest.contributions(),
                manifest: newest.manifest_text().unwrap_or_default(),
                active_profiles: profiles.clone(),
                versions: installed
                    .iter()
                    .map(|stored| stored.summary(&profiles))
                    .collect(),
                listed: listed.clone(),
            }
        }),
        None => serde_json::json!({
            "id": id,
            "installed": [],
            "listed": listed,
            "active_profiles": profiles,
        }),
    };
    Ok(Report::new(lines, json))
}

/// `acpilot ext themes`: every theme a profile can draw with, resolved.
pub fn themes(paths: &Paths, profile: Option<&str>) -> Result<Report, String> {
    let name = profile.unwrap_or(acpilot_profile::launch::DEFAULT_PROFILE);
    let directory = paths.profile(name).map_err(|error| error.to_string())?;
    let activation =
        Activation::load(&directory.join(ACTIVATION_FILE)).map_err(|error| error.to_string())?;
    let active = paths
        .store()
        .resolve_active(&activation.active)
        .map_err(first_message)?;
    let mut extensions = acpilot_extensions::builtin::load_all().map_err(first_message)?;
    extensions.extend(active.extensions);

    let preferences = acpilot_profile::project::Root::resolve(None)
        .ok()
        .and_then(|root| {
            let profile = acpilot_profile::Profile::open(&directory).ok()?;
            root.settings_with_profile(Some(&profile))
                .ok()
                .map(|settings| settings.preferences)
        });

    let registry = ThemeRegistry::new(&extensions);
    let mut rows: Vec<ThemeSummary> = Vec::new();
    for id in registry.ids() {
        let Ok(resolved) = registry.resolve(id) else {
            continue;
        };
        let selected = preferences
            .as_ref()
            .map(|preferences| {
                [
                    acpilot_proto::profile::ThemeSurface::Tui,
                    acpilot_proto::profile::ThemeSurface::Gui,
                    acpilot_proto::profile::ThemeSurface::Web,
                ]
                .into_iter()
                .filter(|surface| preferences.theme_for(*surface) == resolved.id)
                .collect()
            })
            .unwrap_or_default();
        rows.push(ThemeSummary {
            id: resolved.id.clone(),
            name: resolved.name.clone(),
            appearance: resolved.appearance,
            built_in: resolved.built_in,
            extension_id: (!resolved.built_in).then(|| resolved.extension_id.clone()),
            selected,
            ui: resolved
                .palette
                .ui
                .iter()
                .map(|(token, color)| (token.as_str().to_owned(), color.to_string()))
                .collect(),
            syntax: resolved
                .palette
                .syntax
                .iter()
                .map(|(token, color)| (token.as_str().to_owned(), color.to_string()))
                .collect(),
        });
    }
    rows.sort_by(|left, right| left.id.cmp(&right.id));

    let mut lines = Vec::new();
    if rows.is_empty() {
        lines.push("this profile has no themes".to_owned());
    }
    for row in &rows {
        let origin = row
            .extension_id
            .as_deref()
            .map_or_else(|| "built-in".to_owned(), |id| format!("from {id}"));
        let selected = if row.selected.is_empty() {
            String::new()
        } else {
            format!(
                "  [selected: {}]",
                row.selected
                    .iter()
                    .map(|surface| format!("{surface:?}").to_lowercase())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        lines.push(format!("{}  {} ({origin}){selected}", row.id, row.name));
    }
    Ok(Report::new(lines, serde_json::json!({ "themes": rows })))
}

fn first_message(errors: Vec<acpilot_extensions::ExtensionError>) -> String {
    errors.into_iter().next().map_or_else(
        || "the extension set does not load".to_owned(),
        |error| error.to_string(),
    )
}

/// Profiles that activate one id.
fn activating(paths: &Paths, id: &str) -> Result<Vec<String>, String> {
    let mut active = active_profiles(paths)?;
    Ok(active.remove(id).unwrap_or_default())
}

/// Every profile's activation list, by extension id.
fn active_profiles(paths: &Paths) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut active: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let profiles = paths.config_root.join("profiles");
    let entries = match fs::read_dir(&profiles) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(active),
        Err(error) => return Err(format!("{}: {error}", profiles.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", profiles.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let activation =
            Activation::load(&path.join(ACTIVATION_FILE)).map_err(|error| error.to_string())?;
        for id in activation.active {
            active.entry(id).or_default().push(name.to_owned());
        }
    }
    for profiles in active.values_mut() {
        profiles.sort();
    }
    Ok(active)
}
