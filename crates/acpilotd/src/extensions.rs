//! Extensions over HTTP: what is installed, what a profile may use, and the
//! themes every surface draws with.
//!
//! Installing is global and activation is per profile (ADR 0003), so the two are
//! separate routes: `POST /v1/extensions` puts bytes in the store, and
//! `POST /v1/extensions/{id}/activate` writes one profile's approval. A list route
//! that mixed them would have to guess which profile a caller was asking about.
//!
//! The route answers are the CLI's own types (`ExtensionSummary`, `ThemeSummary`),
//! built from the same store scan the `acpilot ext` verbs use. A window and a
//! terminal reading two different ideas of "installed" is the bug this module is
//! shaped to prevent.
//!
//! Every mutation announces an `extension.*` frame on the profile-wide stream, so
//! a UI that is showing the list can refetch it without polling. The frame carries
//! the id, version and profile and nothing else: the list is state, and a client
//! that missed a frame asks for it again.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use acpilot_core::AcpilotError;
use acpilot_extensions::activation::{ACTIVATION_FILE, Activation, ProfileExtensions};
use acpilot_extensions::index::{self, Index};
use acpilot_extensions::manifest::{validate_namespaced_id, version_tuple};
use acpilot_extensions::store::{Store, StoredExtension};
use acpilot_extensions::{ThemeAppearance, ThemeRegistry};
use acpilot_proto::ErrorBody;
use acpilot_proto::extension::{
    ExtensionAction, ExtensionDetail, ExtensionListing, ExtensionRemoval, ExtensionSummary,
    ExtensionVerify, InstallExtensionRequest, ThemeSummary,
};
use acpilot_proto::profile::ThemeSurface;
use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::{ApiError, AppState};

/// The routes an extension-aware client uses.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/extensions", get(list).post(install))
        .route("/v1/extensions/{id}", get(detail).delete(remove))
        .route("/v1/extensions/{id}/activate", post(activate))
        .route("/v1/extensions/{id}/deactivate", post(deactivate))
        .route("/v1/extensions/{id}/verify", post(verify))
        .route("/v1/themes", get(themes))
}

/// The extension operations, for the OpenAPI document.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    vec![
        (
            "/v1/extensions",
            HttpMethod::Get,
            <__path_list_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/extensions",
            HttpMethod::Post,
            <__path_install_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/extensions/{id}",
            HttpMethod::Get,
            <__path_detail_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/extensions/{id}",
            HttpMethod::Delete,
            <__path_remove_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/extensions/{id}/activate",
            HttpMethod::Post,
            <__path_activate_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/extensions/{id}/deactivate",
            HttpMethod::Post,
            <__path_deactivate_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/extensions/{id}/verify",
            HttpMethod::Post,
            <__path_verify_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/themes",
            HttpMethod::Get,
            <__path_themes_doc as utoipa::Path>::operation(),
        ),
    ]
}

/// What changed, for the `extension.*` frames.
#[derive(Debug, Clone, Serialize)]
pub struct Notice {
    /// Which verb ran.
    pub action: Action,
    /// Extension it ran on.
    pub id: String,
    /// Version, when the verb named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Profile, for the two verbs that write a profile's approval list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// The four things that can happen to an extension here.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Bytes arrived in the store.
    Installed,
    /// Every version of an id left the store.
    Removed,
    /// A profile approved an id.
    Activated,
    /// A profile withdrew its approval.
    Deactivated,
}

impl Action {
    /// The SSE event name, `extension.installed` and friends.
    #[must_use]
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::Installed => "extension.installed",
            Self::Removed => "extension.removed",
            Self::Activated => "extension.activated",
            Self::Deactivated => "extension.deactivated",
        }
    }
}

/// `GET /v1/extensions`: every installed version of every id.
async fn list(State(state): State<AppState>) -> Result<Json<Vec<ExtensionSummary>>, ApiError> {
    let scan = scan(&state)?;
    let active = active_profiles(&state)?;
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
    Ok(Json(rows))
}

/// `GET /v1/extensions/{id}`: what is installed, what the index lists, and what
/// every version contributes.
async fn detail(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<ExtensionDetail>, ApiError> {
    validate_namespaced_id("extension id", &id).map_err(api)?;
    let scan = scan(&state)?;
    let active = active_profiles(&state)?;
    let profiles = active.get(&id).cloned().unwrap_or_default();

    let mut versions: Vec<&StoredExtension> =
        scan.iter().filter(|stored| stored.id == id).collect();
    versions.sort_by_key(|stored| version_tuple(&stored.version));
    let Some(newest) = versions.last().copied() else {
        return Err(ApiError::new(AcpilotError::not_found(format!(
            "extension {id} is not installed"
        ))));
    };

    Ok(Json(ExtensionDetail {
        summary: newest.summary(&profiles),
        contributions: newest.contributions(),
        manifest: newest.manifest_text().unwrap_or_default(),
        active_profiles: profiles.clone(),
        versions: versions
            .iter()
            .map(|stored| stored.summary(&profiles))
            .collect(),
        listed: listings(&id),
    }))
}

/// Versions the signed index compiled into this build offers for one id.
///
/// A broken embedded listing is reported as no listings rather than as a failed
/// detail request: the installed bytes are still a fact, and the signature is
/// checked at build time by the store's own tests.
fn listings(id: &str) -> Vec<ExtensionListing> {
    let Ok(index) = index::verify(
        index::INDEX_FILE,
        index::DEFAULT_INDEX,
        index::DEFAULT_SIGNATURE,
        &[],
    )
    .and_then(|_key| Index::parse(index::INDEX_FILE, index::DEFAULT_INDEX)) else {
        return Vec::new();
    };
    index
        .versions(id)
        .into_iter()
        .map(|entry| ExtensionListing {
            version: entry.version.clone(),
            kind: entry.kind,
            publisher: entry.publisher.clone(),
            checksum: entry.checksum.clone(),
            size: entry.size,
            download: entry.download.clone(),
            min_acpilot: entry.min_acpilot.clone(),
        })
        .collect()
}

/// `POST /v1/extensions`: install from the signed index, or copy a directory.
async fn install(
    State(state): State<AppState>,
    body: Result<Json<InstallExtensionRequest>, JsonRejection>,
) -> Result<Json<ExtensionSummary>, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not an install request: {rejection}"
        )))
    })?;
    let store = store(&state)?;

    let stored = if let Some(path) = &request.path {
        store.install_from_dir(path).map_err(api)?
    } else {
        validate_namespaced_id("extension id", &request.id).map_err(api)?;
        let (label, index) = load_index(&state, request.index.as_deref()).await?;
        let entry = match request.version.as_deref() {
            Some(version) => index.entry(&request.id, version).ok_or_else(|| {
                ApiError::new(AcpilotError::not_found(format!(
                    "{}@{version} in {label}",
                    request.id
                )))
            })?,
            None => index.newest(&request.id).ok_or_else(|| {
                ApiError::new(AcpilotError::not_found(format!(
                    "{} in {label}",
                    request.id
                )))
            })?,
        };
        if !entry.requires(acpilot_core::VERSION) {
            return Err(ApiError::new(AcpilotError::invalid(format!(
                "{} requires acpilot {} or newer; this build is {}",
                entry.label(),
                entry.min_acpilot.as_deref().unwrap_or("?"),
                acpilot_core::VERSION
            ))));
        }
        let bytes = fetch(&entry.download).await?;
        store.install_from_archive(&bytes, entry).map_err(api)?
    };

    if request.activate {
        let directory = profile_directory(&state, None)?;
        let mut extensions = ProfileExtensions::open(store.clone(), &directory).map_err(api)?;
        if extensions.activate(&stored.id).map_err(api)? {
            state.announce_extension(Notice {
                action: Action::Activated,
                id: stored.id.clone(),
                version: Some(stored.version.clone()),
                profile: Some(profile_label(&state, None)),
            });
        }
    }

    state.announce_extension(Notice {
        action: Action::Installed,
        id: stored.id.clone(),
        version: Some(stored.version.clone()),
        profile: None,
    });

    let active = active_profiles(&state)?;
    Ok(Json(
        stored.summary(
            active
                .get(&stored.id)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        ),
    ))
}

/// `POST /v1/extensions/{id}/activate`: approve one id for one profile.
async fn activate(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<ProfileBody>, JsonRejection>,
) -> Result<Json<ExtensionAction>, ApiError> {
    let Json(body) = profile_body(body)?;
    validate_namespaced_id("extension id", &id).map_err(api)?;
    let directory = profile_directory(&state, body.profile.as_deref())?;
    let profile = profile_label(&state, body.profile.as_deref());
    let mut extensions = ProfileExtensions::open(store(&state)?, &directory).map_err(api)?;
    let changed = extensions.activate(&id).map_err(api)?;
    if changed {
        state.announce_extension(Notice {
            action: Action::Activated,
            id: id.clone(),
            version: None,
            profile: Some(profile.clone()),
        });
    }
    Ok(Json(ExtensionAction {
        id,
        profile,
        changed,
    }))
}

/// `POST /v1/extensions/{id}/deactivate`: withdraw one profile's approval.
async fn deactivate(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<ProfileBody>, JsonRejection>,
) -> Result<Json<ExtensionAction>, ApiError> {
    let Json(body) = profile_body(body)?;
    validate_namespaced_id("extension id", &id).map_err(api)?;
    let directory = profile_directory(&state, body.profile.as_deref())?;
    let profile = profile_label(&state, body.profile.as_deref());
    let mut extensions = ProfileExtensions::open(store(&state)?, &directory).map_err(api)?;
    let changed = extensions.deactivate(&id).map_err(api)?;
    if changed {
        state.announce_extension(Notice {
            action: Action::Deactivated,
            id: id.clone(),
            version: None,
            profile: Some(profile.clone()),
        });
    }
    Ok(Json(ExtensionAction {
        id,
        profile,
        changed,
    }))
}

/// `POST /v1/extensions/{id}/verify`: rehash every installed version of one id.
async fn verify(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<ExtensionVerify>, ApiError> {
    validate_namespaced_id("extension id", &id).map_err(api)?;
    let scan = scan(&state)?;
    let profiles = active_profiles(&state)?
        .get(&id)
        .cloned()
        .unwrap_or_default();
    let mut rows: Vec<ExtensionSummary> = scan
        .iter()
        .filter(|stored| stored.id == id)
        .map(|stored| stored.summary(&profiles))
        .collect();
    if rows.is_empty() {
        return Err(ApiError::new(AcpilotError::not_found(format!(
            "extension {id} is not installed"
        ))));
    }
    rows.sort_by_key(|row| version_tuple(&row.version));
    let ready = rows.iter().all(|row| {
        !matches!(
            row.status,
            acpilot_proto::extension::ExtensionStatus::Blocked
        )
    });
    Ok(Json(ExtensionVerify {
        ready,
        extensions: rows,
    }))
}

/// `DELETE /v1/extensions/{id}`: remove every installed version of one id.
///
/// Refused while any profile activates it, which is the same rule the CLI
/// follows: removing the bytes under an approval leaves a profile asking for
/// something that is gone, and the error names the profiles to deactivate first.
async fn remove(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<ExtensionRemoval>, ApiError> {
    validate_namespaced_id("extension id", &id).map_err(api)?;
    let active = active_profiles(&state)?;
    if let Some(profiles) = active.get(&id).filter(|profiles| !profiles.is_empty()) {
        return Err(ApiError::new(AcpilotError::conflict(format!(
            "{id} is active in {}; deactivate it there first, so no profile is left asking \
             for something that is gone",
            profiles.join(", ")
        ))));
    }
    let store = store(&state)?;
    let versions: Vec<String> = scan(&state)?
        .into_iter()
        .filter(|stored| stored.id == id && !stored.version.is_empty())
        .map(|stored| stored.version)
        .collect();
    if versions.is_empty() {
        return Err(ApiError::new(AcpilotError::not_found(format!(
            "extension {id} is not installed"
        ))));
    }
    for version in &versions {
        store.remove(&id, version).map_err(api)?;
    }
    state.announce_extension(Notice {
        action: Action::Removed,
        id: id.clone(),
        version: None,
        profile: None,
    });
    Ok(Json(ExtensionRemoval { id, versions }))
}

/// Which profile a theme list is for.
#[derive(Debug, Deserialize)]
struct ThemeQuery {
    /// Profile id. The daemon's own profile when absent.
    profile: Option<String>,
}

/// `GET /v1/themes`: every theme a profile can draw with, resolved.
///
/// A theme is shown with its inheritance already applied, because that is what a
/// surface needs and resolving it in three frontends is how two of them end up
/// disagreeing about a token. `selected` names the surfaces whose settings choose
/// it right now, which is a fact about the daemon's own profile and therefore only
/// filled in when no other profile was named.
async fn themes(
    State(state): State<AppState>,
    Query(query): Query<ThemeQuery>,
) -> Result<Json<Vec<ThemeSummary>>, ApiError> {
    let own_profile = query.profile.is_none();
    let directory = profile_directory(&state, query.profile.as_deref())?;
    let activation = Activation::load(&directory.join(ACTIVATION_FILE)).map_err(api)?;
    let active = store(&state)?
        .resolve_active(&activation.active)
        .map_err(all)?;
    let mut extensions = acpilot_extensions::builtin::load_all().map_err(all)?;
    extensions.extend(active.extensions);

    let preferences = own_profile.then(|| state.preferences()).flatten();
    let registry = ThemeRegistry::new(&extensions);
    let mut rows = Vec::new();
    for id in registry.ids() {
        let Ok(resolved) = registry.resolve(id) else {
            // A theme whose base is missing or whose chain loops is not listed:
            // a surface cannot draw with it, and offering it would be an option
            // that fails after it is picked.
            continue;
        };
        let selected = preferences
            .as_ref()
            .map(|preferences| {
                [ThemeSurface::Tui, ThemeSurface::Gui, ThemeSurface::Web]
                    .into_iter()
                    .filter(|surface| preferences.theme_for(*surface) == resolved.id)
                    .collect()
            })
            .unwrap_or_default();
        rows.push(ThemeSummary {
            id: resolved.id.clone(),
            name: resolved.name.clone(),
            appearance: match resolved.appearance {
                ThemeAppearance::Dark => ThemeAppearance::Dark,
                ThemeAppearance::Light => ThemeAppearance::Light,
            },
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
    Ok(Json(rows))
}

/// The body of an activation request.
#[derive(Debug, Default, Deserialize)]
struct ProfileBody {
    /// Profile id. The daemon's own profile when absent.
    #[serde(default)]
    profile: Option<String>,
}

fn profile_body(
    body: Result<Json<ProfileBody>, JsonRejection>,
) -> Result<Json<ProfileBody>, ApiError> {
    body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not `{{ \"profile\": ... }}`: {rejection}"
        )))
    })
}

/// The store this daemon installs into.
fn store(state: &AppState) -> Result<Store, ApiError> {
    state.extension_store().map_err(api)
}

fn scan(state: &AppState) -> Result<Vec<StoredExtension>, ApiError> {
    store(state)?.scan().map_err(api)
}

/// The profiles root, derived from the daemon's own profile directory when that
/// sits in the usual `<config>/profiles/<name>` shape.
fn profiles_root(state: &AppState) -> Result<PathBuf, ApiError> {
    if let Some(parent) = state.profile_root().and_then(Path::parent)
        && parent.file_name().is_some_and(|name| name == "profiles")
    {
        return Ok(parent.to_path_buf());
    }
    acpilot_extensions::activation::profiles_root().map_err(api)
}

/// One profile's directory: the named one, or the daemon's own.
fn profile_directory(state: &AppState, name: Option<&str>) -> Result<PathBuf, ApiError> {
    match name {
        Some(name) => {
            acpilot_extensions::manifest::validate_segment("profile", name).map_err(api)?;
            Ok(profiles_root(state)?.join(name))
        }
        None => state.profile_root().map(Path::to_path_buf).ok_or_else(|| {
            ApiError::new(AcpilotError::conflict(
                "this daemon has no profile, so there is no activation list to write",
            ))
        }),
    }
}

/// The name a change is reported under.
fn profile_label(state: &AppState, requested: Option<&str>) -> String {
    if let Some(name) = requested {
        return name.to_owned();
    }
    state
        .profile_name()
        .map(str::to_owned)
        .or_else(|| {
            state
                .profile_root()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// Every profile's activation list, by extension id.
fn active_profiles(state: &AppState) -> Result<BTreeMap<String, Vec<String>>, ApiError> {
    let root = profiles_root(state)?;
    let mut active: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(active),
        Err(error) => {
            return Err(ApiError::new(AcpilotError::invalid(format!(
                "{}: {error}",
                root.display()
            ))));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            ApiError::new(AcpilotError::invalid(format!(
                "{}: {error}",
                root.display()
            )))
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let activation = Activation::load(&path.join(ACTIVATION_FILE)).map_err(api)?;
        for id in activation.active {
            active.entry(id).or_default().push(name.to_owned());
        }
    }
    for profiles in active.values_mut() {
        profiles.sort();
    }
    Ok(active)
}

/// Read, verify and parse an index: the compiled-in one, or the one named.
async fn load_index(state: &AppState, source: Option<&str>) -> Result<(String, Index), ApiError> {
    let (label, bytes, signature) = match source {
        None => (
            index::INDEX_FILE.to_owned(),
            index::DEFAULT_INDEX.to_vec(),
            index::DEFAULT_SIGNATURE.to_vec(),
        ),
        Some(source) if is_url(source) => {
            let bytes = fetch(source).await?;
            let signature_url = format!("{source}{}", index::SIGNATURE_SUFFIX);
            let signature = fetch(&signature_url).await?;
            (source.to_owned(), bytes, signature)
        }
        Some(path) => {
            let bytes = fs::read(path).map_err(|error| {
                ApiError::new(AcpilotError::invalid(format!("{path}: {error}")))
            })?;
            let signature_path = format!("{path}{}", index::SIGNATURE_SUFFIX);
            let signature = fs::read(&signature_path).map_err(|error| {
                ApiError::new(AcpilotError::invalid(format!("{signature_path}: {error}")))
            })?;
            (path.to_owned(), bytes, signature)
        }
    };
    index::verify(&label, &bytes, &signature, &state.trusted_keys()).map_err(api)?;
    let index = Index::parse(&label, &bytes).map_err(api)?;
    Ok((label, index))
}

fn is_url(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("http://")
}

/// One GET, for an index or an archive.
async fn fetch(url: &str) -> Result<Vec<u8>, ApiError> {
    let client = reqwest::Client::builder()
        .user_agent(format!("acpilot/{}", acpilot_core::VERSION))
        .build()
        .map_err(|error| {
            ApiError::new(AcpilotError::invalid(format!("no HTTP client: {error}")))
        })?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| ApiError::new(AcpilotError::invalid(format!("{url}: {error}"))))?;
    let status = response.status();
    if !status.is_success() {
        return Err(ApiError::new(AcpilotError::invalid(format!(
            "{url}: HTTP {status}"
        ))));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| ApiError::new(AcpilotError::invalid(format!("{url}: {error}"))))
}

/// The loader's failures, joined: one broken extension asset and one broken
/// manifest in the same activation list are one refusal with every reason, not
/// whichever happened to be first.
fn all(errors: Vec<acpilot_extensions::ExtensionError>) -> ApiError {
    ApiError::new(AcpilotError::invalid(
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

/// Map a store failure to the envelope: not-found and conflict are the two a
/// caller can act on, and everything else is the request being wrong.
fn api(error: acpilot_extensions::ExtensionError) -> ApiError {
    let message = error.to_string();
    match error {
        acpilot_extensions::ExtensionError::NotFound { .. } => {
            ApiError::new(AcpilotError::not_found(message))
        }
        acpilot_extensions::ExtensionError::AlreadyInstalled { .. }
        | acpilot_extensions::ExtensionError::Duplicate { .. } => {
            ApiError::new(AcpilotError::conflict(message))
        }
        _ => ApiError::new(AcpilotError::invalid(message)),
    }
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/extensions",
    tag = "extensions",
    summary = "List every installed extension version",
    description = "One row per installed version, with the profiles that activate it. A blocked version is listed with its reason rather than hidden: the refusal is a fact about the bytes.",
    responses(
        (status = 200, description = "Every installed version, ordered by id and then version", body = Vec<ExtensionSummary>),
        (status = 500, description = "The store could not be read", body = ErrorBody)
    )
)]
fn list_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/extensions",
    tag = "extensions",
    summary = "Install an extension from the signed index or a directory",
    description = "An index install verifies the listing's signature, checks the archive's size and sha256, and refuses an archive whose manifest names a different id, version or publisher. A `path` install copies that directory through the same store checks.",
    request_body = InstallExtensionRequest,
    responses(
        (status = 200, description = "The installed version", body = ExtensionSummary),
        (status = 400, description = "The request names nothing installable, or the archive failed its checks", body = ErrorBody),
        (status = 404, description = "The id or version is not in the index", body = ErrorBody),
        (status = 409, description = "That id and version are already installed", body = ErrorBody)
    )
)]
fn install_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/extensions/{id}",
    tag = "extensions",
    summary = "One extension: installed versions, contributions and index listings",
    params(("id" = String, Path, description = "Extension id, `publisher.name`")),
    responses(
        (status = 200, description = "The detail, newest version first in `versions`", body = ExtensionDetail),
        (status = 400, description = "Not a namespaced extension id", body = ErrorBody),
        (status = 404, description = "No version of that id is installed", body = ErrorBody)
    )
)]
fn detail_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    delete,
    path = "/v1/extensions/{id}",
    tag = "extensions",
    summary = "Remove every installed version of one extension",
    description = "Refused while a profile activates the id; the error names the profiles to deactivate first.",
    params(("id" = String, Path, description = "Extension id, `publisher.name`")),
    responses(
        (status = 200, description = "The versions removed", body = ExtensionRemoval),
        (status = 400, description = "Not a namespaced extension id", body = ErrorBody),
        (status = 404, description = "Nothing installed under that id", body = ErrorBody),
        (status = 409, description = "A profile still activates it", body = ErrorBody)
    )
)]
fn remove_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/extensions/{id}/activate",
    tag = "extensions",
    summary = "Approve an installed extension for one profile",
    params(("id" = String, Path, description = "Extension id, `publisher.name`")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "What the approval list now says", body = ExtensionAction),
        (status = 400, description = "Not a namespaced id, or a blocked/absent extension", body = ErrorBody),
        (status = 404, description = "No ready version of that id is installed", body = ErrorBody),
        (status = 409, description = "The daemon has no profile to write", body = ErrorBody)
    )
)]
fn activate_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/extensions/{id}/deactivate",
    tag = "extensions",
    summary = "Withdraw one profile's approval",
    params(("id" = String, Path, description = "Extension id, `publisher.name`")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "What the approval list now says; `changed` is false when it already did not list the id", body = ExtensionAction),
        (status = 400, description = "Not a namespaced extension id", body = ErrorBody),
        (status = 409, description = "The daemon has no profile to write", body = ErrorBody)
    )
)]
fn deactivate_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/extensions/{id}/verify",
    tag = "extensions",
    summary = "Rehash every installed version of one extension",
    description = "`ready` is false when any version's bytes no longer match the recorded checksum, or the manifest no longer loads.",
    params(("id" = String, Path, description = "Extension id, `publisher.name`")),
    responses(
        (status = 200, description = "Each version with its verdict", body = ExtensionVerify),
        (status = 400, description = "Not a namespaced extension id", body = ErrorBody),
        (status = 404, description = "Nothing installed under that id", body = ErrorBody)
    )
)]
fn verify_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/themes",
    tag = "extensions",
    summary = "Every theme a profile can draw with, resolved",
    description = "Built-in themes plus those an activated extension provides, with inheritance applied and every token as `#rrggbb`. `selected` names the surfaces whose settings choose the theme, for the daemon's own profile.",
    params(("profile" = Option<String>, Query, description = "Profile id. The daemon's own profile when absent")),
    responses(
        (status = 200, description = "Themes ordered by id", body = Vec<ThemeSummary>),
        (status = 409, description = "No profile was named and the daemon has none", body = ErrorBody)
    )
)]
fn themes_doc() {}
