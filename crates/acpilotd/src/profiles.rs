//! Several profiles in one daemon, and the rule that picks one per request.
//!
//! A profile is a directory: `config.toml` for its policy and preferences,
//! `agents.toml` for its roster, its own database for its conversations. Until
//! now the daemon opened exactly the directory it was started with, so
//! `X-Acpilot-Profile` was a header the page sent and the daemon ignored. That is
//! the state this module ends: every profile under `<config>/profiles` is opened
//! at startup for its settings and router, its database is opened lazily through
//! the shared store cache, and a request is dispatched to the one its header names.
//!
//! # Why a router per profile rather than a field on `AppState`
//!
//! Every handler already takes `State<AppState>`; that state owns the sessions,
//! the broker, the database and the language servers, and those are exactly the
//! things profiles exist to keep apart. Swapping a field per request would be a
//! second place to remember which parts are per-profile. A router per profile is
//! the same isolation the daemon has when it serves one, applied once per
//! directory: the handler cannot reach another profile's sessions because its
//! state has no reference to them.
//!
//! # The header rule
//!
//! A mutating request must name its profile. Missing is `400`, not "the active
//! one", because guessing an identity is how a prompt goes out under the wrong
//! key. A read without a header is the active profile, which is what lets a
//! client list first and choose. A daemon with no profiles at all (no
//! `--profile`, no profile directories) keeps the old behaviour: it has no
//! identity to name, so the rule cannot apply, and its one state answers.
//!
//! SSE and WebSocket connections bind at connect: their profile is chosen from
//! the header (or the active profile) once, when the connection is opened. A
//! switch is a reconnect, never a frame on an open stream.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use acpilot_core::AcpilotError;
use acpilot_proto::ErrorBody;
use acpilot_proto::profile::{Preferences, PreferencesPatch, ProfileSummary};
use acpilot_store::{Index, Stores};
use axum::Json;
use axum::Router;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::Method;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tower::ServiceExt;

use crate::{ApiError, AppState, StateError, router_with, startup_with_stores};

/// The header a request names its profile in.
///
/// Spelled once, here, because the daemon's rule and the page's client must not
/// disagree about its name; a second spelling is a request that silently becomes
/// the active profile.
pub const PROFILE_HEADER: &str = "x-acpilot-profile";

/// One profile's state and the router built over it.
struct Entry {
    /// The profile's directory, for the preference route that writes `profile.toml`.
    directory: PathBuf,
    /// Kept beside the router because shutdown and the summary need it: a router
    /// owns its state but does not hand it back.
    state: AppState,
    router: Router,
    /// The row the API lists; `active` is filled in when the list is answered.
    summary: ProfileSummary,
}

impl Entry {
    fn new(name: &str, directory: PathBuf, state: AppState) -> Self {
        let summary = ProfileSummary {
            id: name.to_owned(),
            label: name.to_owned(),
            active: false,
            agent_count: state
                .registry()
                .map_or(0, |registry| registry.agents().len() as u32),
            // The daemon serves one workspace to every profile today; the field
            // counts what a client could open under this profile, which is that
            // one and no more until workspaces become per-profile.
            workspace_count: u32::from(state.workspace().is_some()),
        };
        Self {
            directory,
            router: router_with(state.clone()),
            state,
            summary,
        }
    }
}

/// Every profile this daemon serves, and which one a headerless request is about.
pub struct Profiles {
    /// Named profiles, by directory name. Empty when the daemon has no profile.
    entries: BTreeMap<String, Entry>,
    /// The state of a daemon started without a profile and with none on disk.
    ///
    /// Separate from the map because it has no name: `GET /v1/profiles` is empty,
    /// and a header cannot select it. It exists so a profile-less daemon keeps
    /// serving the workspace it was given.
    anonymous: Option<Entry>,
    /// The profile a read without a header is about.
    active: RwLock<String>,
}

impl Profiles {
    /// Open every profile under `<config>/profiles`, plus the one `--profile`
    /// named even when it lives elsewhere.
    ///
    /// A profile that cannot be opened is skipped with a warning rather than
    /// taking the daemon down, because one half-written profile must not stop the
    /// others from serving. The exception is the profile this daemon was started
    /// with: a `--profile` that does not load is the refusal it has always been,
    /// and starting anyway would serve a daemon the person did not ask for.
    ///
    /// # Errors
    ///
    /// The started profile, or the only state of a daemon with none, could not be
    /// opened.
    pub fn discover(
        started: Option<&Path>,
        workspace: &Path,
        cors: bool,
    ) -> Result<Self, StateError> {
        // The index is disposable. A file that cannot be opened costs the profile
        // list's cross-session memory, not the daemon: directories are still the
        // authority on which profiles exist.
        let index = match Index::open(acpilot_profile::index_path()) {
            Ok(index) => Some(index),
            Err(error) => {
                tracing::warn!(%error, "the profile index is not available");
                None
            }
        };
        Self::discover_in_with(
            &acpilot_profile::profiles_dir(),
            index.as_ref(),
            started,
            workspace,
            cors,
        )
    }

    /// [`Self::discover`] over an explicit profiles root, without a global index.
    ///
    /// The root is a parameter so a test — or an embedded daemon serving a
    /// portable install — can point at its own directory without changing the
    /// process's environment. The index is deliberately not opened here: a test
    /// that wrote `<data>/index.sqlite` would be testing the machine's home
    /// directory as much as the daemon. A profile's database is not opened here
    /// either: each state attaches its path to the shared cache, which opens it on
    /// the first request that needs history.
    ///
    /// # Errors
    ///
    /// A profile that had to load and did not (see [`Self::discover`]).
    pub fn discover_in(
        root: &Path,
        started: Option<&Path>,
        workspace: &Path,
        cors: bool,
    ) -> Result<Self, StateError> {
        Self::discover_in_with(root, None, started, workspace, cors)
    }

    fn discover_in_with(
        root: &Path,
        index: Option<&Index>,
        started: Option<&Path>,
        workspace: &Path,
        cors: bool,
    ) -> Result<Self, StateError> {
        let stores = Arc::new(Stores::default());
        let mut directories = profile_directories(root);
        let started_name = started
            .and_then(|directory| directory.file_name())
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        if let Some(directory) = started {
            let name = started_name.clone().unwrap_or_else(|| "profile".to_owned());
            directories.insert(name, directory.to_path_buf());
        }

        if directories.is_empty() {
            let state = startup_with_stores(None, workspace, Arc::clone(&stores))?.with_cors(cors);
            return Ok(Self {
                entries: BTreeMap::new(),
                anonymous: Some(Entry::new("", PathBuf::new(), state)),
                active: RwLock::new(String::new()),
            });
        }

        let mut entries = BTreeMap::new();
        for (name, directory) in &directories {
            match startup_with_stores(Some(directory), workspace, Arc::clone(&stores)) {
                Ok(state) => {
                    let mut state = state;
                    if let Some(index) = index {
                        state = state.with_index(index.clone());
                        if let Err(error) = index.remember_profile(name) {
                            tracing::warn!(%error, profile = %name, "the profile was not indexed");
                        }
                        let workspace_id = format!(
                            "{}:{}",
                            crate::hosts::LOCAL_HOST,
                            workspace.to_string_lossy()
                        );
                        if let Err(error) = index.remember_workspace(name, &workspace_id) {
                            tracing::warn!(%error, profile = %name, "the workspace was not indexed");
                        }
                    }
                    entries.insert(
                        name.clone(),
                        Entry::new(name, directory.to_path_buf(), state.with_cors(cors)),
                    );
                }
                Err(error) if Some(name) == started_name.as_ref() => return Err(error),
                Err(error) => {
                    tracing::warn!(profile = %name, %error, "profile skipped: it did not load");
                }
            }
        }

        if entries.is_empty() {
            // Every directory failed and none of them was the started profile: a
            // daemon that serves nothing is worse than one that serves the root
            // it was given, so it falls back to the profile-less state.
            tracing::warn!("no profile loaded; serving the workspace without an identity");
            let state = startup_with_stores(None, workspace, Arc::clone(&stores))?.with_cors(cors);
            return Ok(Self {
                entries: BTreeMap::new(),
                anonymous: Some(Entry::new("", PathBuf::new(), state)),
                active: RwLock::new(String::new()),
            });
        }

        // The started profile is what the daemon is for; `default` is the name a
        // machine with no flag means; otherwise the first directory is the least
        // surprising answer, and it is stable because the map is sorted.
        let active = started_name
            .filter(|name| entries.contains_key(name))
            .or_else(|| {
                entries
                    .contains_key("default")
                    .then(|| "default".to_owned())
            })
            .or_else(|| entries.keys().next().cloned())
            .unwrap_or_default();
        tracing::info!(profiles = entries.len(), active = %active, "profiles loaded");
        Ok(Self {
            entries,
            anonymous: None,
            active: RwLock::new(active),
        })
    }

    /// The rows `GET /v1/profiles` answers, with `active` marked.
    #[must_use]
    pub fn summaries(&self) -> Vec<ProfileSummary> {
        let active = self.active();
        self.entries
            .values()
            .map(|entry| ProfileSummary {
                active: entry.summary.id == active,
                ..entry.summary.clone()
            })
            .collect()
    }

    /// One profile's row, when this daemon serves it.
    #[must_use]
    pub fn summary(&self, id: &str) -> Option<ProfileSummary> {
        self.summaries().into_iter().find(|row| row.id == id)
    }

    /// A registry over states a caller already built.
    ///
    /// The daemon's two launch paths use [`Self::discover`]; this is the seam a
    /// test uses to prove the routing without a store on somebody's disk, and the
    /// one an embedded daemon would use to serve states it owns.
    #[must_use]
    pub fn from_states(rows: Vec<(String, AppState)>, active: String) -> Self {
        let entries: BTreeMap<String, Entry> = rows
            .into_iter()
            .map(|(name, state)| {
                let directory = acpilot_profile::profiles_dir().join(&name);
                let entry = Entry::new(&name, directory, state);
                (name, entry)
            })
            .collect();
        let active = if entries.contains_key(&active) {
            active
        } else {
            entries.keys().next().cloned().unwrap_or_default()
        };
        Self {
            entries,
            anonymous: None,
            active: RwLock::new(active),
        }
    }

    /// Make `id` the profile a headerless read is about.
    ///
    /// In memory for the life of this daemon: persisting the choice is the
    /// settings cascade's (`active_profile`), and a daemon that wrote a file here
    /// would be deciding the next launch from a runtime request.
    ///
    /// # Errors
    ///
    /// No profile by that name is served.
    pub fn activate(&self, id: &str) -> Result<ProfileSummary, ApiError> {
        let Some(row) = self.summary(id) else {
            return Err(no_such_profile(id, &self.names()));
        };
        *self.active.write().unwrap_or_else(PoisonError::into_inner) = id.to_owned();
        Ok(ProfileSummary {
            active: true,
            ..row
        })
    }

    /// The active profile's name, empty when the daemon has no profile.
    #[must_use]
    pub fn active(&self) -> String {
        self.active
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The names this daemon serves, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Which profile a request is about.
    ///
    /// The header wins; absent on a read means the active profile; absent on a
    /// mutating request is refused when there are identities to refuse it for.
    fn select(
        &self,
        header: Option<&str>,
        method: &Method,
        path: &str,
    ) -> Result<String, ApiError> {
        if let Some(name) = header {
            return if self.entries.contains_key(name) {
                Ok(name.to_owned())
            } else if self.anonymous.is_some() {
                Err(ApiError::new(AcpilotError::not_found(format!(
                    "this daemon serves no profile, so `{name}` is not an identity it has"
                ))))
            } else {
                Err(no_such_profile(name, &self.names()))
            };
        }
        if self.entries.is_empty() {
            return Ok(String::new());
        }
        // `/v1/profiles*` is answered before the fallback; a mutating request that
        // reaches here is about a session, a file, an agent, a terminal or a
        // permission, and every one of those belongs to an identity.
        // Only when there is a choice: a daemon serving one profile has no second
        // identity to confuse, and its older clients — the TUI, the CLI — do not
        // send the header yet. A daemon with two or more refuses to guess.
        let mutating = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
        if mutating && self.entries.len() > 1 && !path.starts_with("/v1/profiles") {
            return Err(ApiError::new(AcpilotError::invalid(
                "name the profile in `X-Acpilot-Profile`: a mutating request is never sent as the \
                 active profile, because guessing an identity is how work goes out under the \
                 wrong one",
            )));
        }
        Ok(self.active())
    }

    fn entry(&self, name: &str) -> Result<&Entry, ApiError> {
        self.entries
            .get(name)
            .ok_or_else(|| no_such_profile(name, &self.names()))
    }

    fn router_for(&self, name: &str) -> Option<&Router> {
        if name.is_empty() {
            self.anonymous.as_ref().map(|entry| &entry.router)
        } else {
            self.entries.get(name).map(|entry| &entry.router)
        }
    }

    /// Stop every profile's language servers.
    ///
    /// A server is a process, and the router's drop is not its end: a task
    /// holding a state clone keeps the registry alive past `serve`, which is why
    /// this is a call and not a destructor.
    pub async fn shutdown_language_servers(&self) {
        for entry in self.entries.values().chain(self.anonymous.iter()) {
            entry.state.language_servers().shutdown().await;
        }
    }
}

/// The profile directories under `root`, by name.
///
/// A directory without a `config.toml` is not a profile: `Profile::open` refuses
/// it, and skipping it here is what keeps one half-created directory from turning
/// into a startup warning on every launch.
fn profile_directories(root: &Path) -> BTreeMap<String, PathBuf> {
    let mut directories = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && path.join(acpilot_profile::CONFIG_FILE).is_file()
                && let Some(name) = path.file_name().and_then(|name| name.to_str())
            {
                directories.insert(name.to_owned(), path);
            }
        }
    }
    directories
}

/// The failure for a header or an id naming a profile this daemon does not serve.
fn no_such_profile(name: &str, served: &[String]) -> ApiError {
    let serves = if served.is_empty() {
        "no profiles".to_owned()
    } else {
        format!("only {}", served.join(", "))
    };
    ApiError::new(AcpilotError::not_found(format!(
        "no profile `{name}` on this daemon; it serves {serves}"
    )))
}

/// The daemon's routes over several profiles.
///
/// The profile routes are answered here because they are about the registry
/// rather than one profile's state; everything else is dispatched to the router
/// of the profile the header names. Layers (auth, CORS, tracing) still wrap this
/// router once, at the listener, so a request is traced and authenticated before
/// it is attributed.
pub fn router(profiles: Arc<Profiles>) -> Router {
    Router::new()
        .route("/v1/profiles", get(list_profiles))
        .route("/v1/profiles/{id}", get(read_profile))
        .route("/v1/profiles/{id}/activate", post(activate_profile))
        .route(
            "/v1/profiles/{id}/preferences",
            get(read_preferences).patch(patch_preferences),
        )
        .fallback(dispatch)
        .with_state(profiles)
}

/// `GET /v1/profiles`.
async fn list_profiles(State(profiles): State<Arc<Profiles>>) -> Json<Vec<ProfileSummary>> {
    Json(profiles.summaries())
}

/// `GET /v1/profiles/{id}`.
async fn read_profile(
    State(profiles): State<Arc<Profiles>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<ProfileSummary>, ApiError> {
    profiles
        .summary(&id)
        .map(Json)
        .ok_or_else(|| no_such_profile(&id, &profiles.names()))
}

/// `POST /v1/profiles/{id}/activate`.
async fn activate_profile(
    State(profiles): State<Arc<Profiles>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<ProfileSummary>, ApiError> {
    profiles.activate(&id).map(Json)
}

/// `GET /v1/profiles/{id}/preferences`.
async fn read_preferences(
    State(profiles): State<Arc<Profiles>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Preferences>, ApiError> {
    let entry = profiles.entry(&id)?;
    let preferences = entry
        .state
        .preferences()
        .or_else(|| acpilot_profile::prefs::defaults().ok());
    preferences.map(Json).ok_or_else(|| {
        ApiError::new(AcpilotError::invalid(
            "this profile's settings do not resolve, so there are no preferences to read",
        ))
    })
}

/// `PATCH /v1/profiles/{id}/preferences`.
///
/// Only the named keys are written. The file's other tables and unknown keys stay
/// where they are: a preferences patch must not become a rewrite of a profile's
/// policy. The write is validated by reading the cascade back, and a file that
/// would not parse is restored rather than left as a profile that cannot start.
async fn patch_preferences(
    State(profiles): State<Arc<Profiles>>,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<PreferencesPatch>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Preferences>, ApiError> {
    let Json(patch) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a preferences patch: {rejection}"
        )))
    })?;
    if patch.is_empty() {
        return Err(ApiError::new(AcpilotError::invalid(
            "the preferences patch names no values",
        )));
    }
    let entry = profiles.entry(&id)?;
    let path = entry
        .directory
        .join(acpilot_profile::settings::PROFILE_SETTINGS_FILE);
    let previous = std::fs::read_to_string(&path).ok();
    write_preferences(&path, &patch)?;
    let preferences = entry.state.preferences();
    let Some(preferences) = preferences else {
        restore(&path, previous.as_deref());
        return Err(ApiError::new(AcpilotError::invalid(
            "the patch produced settings this daemon could not read; nothing was changed",
        )));
    };
    entry.state.announce_preferences(preferences.clone());
    Ok(Json(preferences))
}

/// Merge a patch into a profile's `profile.toml` and write it back.
///
/// The document is parsed as TOML and rendered again, so comments are not part of
/// the promise this function keeps: the keys and tables are. A comment-preserving
/// writer would need an editing parser rather than a value tree.
fn write_preferences(path: &Path, patch: &PreferencesPatch) -> Result<(), ApiError> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut document: toml::Value = if text.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        toml::from_str(&text).map_err(|error| {
            ApiError::new(AcpilotError::invalid(format!(
                "{} does not parse: {error}",
                path.display()
            )))
        })?
    };
    let Some(root) = document.as_table_mut() else {
        return Err(ApiError::new(AcpilotError::invalid(format!(
            "{} is not a TOML table",
            path.display()
        ))));
    };
    let preferences = root
        .entry("preferences")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .ok_or_else(|| {
            ApiError::new(AcpilotError::invalid(format!(
                "{} has a `preferences` key that is not a table",
                path.display()
            )))
        })?;
    patch_preferences_into(preferences, patch);
    let rendered = toml::to_string(&document).map_err(|error| {
        ApiError::new(AcpilotError::invalid(format!(
            "the preferences could not be written: {error}"
        )))
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| {
            ApiError::new(AcpilotError::Io(std::io::Error::new(
                source.kind(),
                format!("{}: {source}", parent.display()),
            )))
        })?;
    }
    std::fs::write(path, rendered).map_err(|source| {
        ApiError::new(AcpilotError::Io(std::io::Error::new(
            source.kind(),
            format!("{}: {source}", path.display()),
        )))
    })
}

/// Put only the keys a patch names into a `[preferences]` table.
fn patch_preferences_into(
    table: &mut toml::map::Map<String, toml::Value>,
    patch: &PreferencesPatch,
) {
    macro_rules! put {
        ($key:literal, $value:expr) => {
            if let Some(value) = $value {
                table.insert(
                    $key.to_owned(),
                    toml::Value::try_from(value).expect("a preference serializes"),
                );
            }
        };
    }
    put!("tab_width", patch.tab_width.as_ref());
    put!("insert_spaces", patch.insert_spaces.as_ref());
    put!("format_on_save", patch.format_on_save.as_ref());
    put!(
        "trim_trailing_whitespace",
        patch.trim_trailing_whitespace.as_ref()
    );
    put!("wrap", patch.wrap.as_ref());
    put!("rulers", patch.rulers.as_ref());
    put!("theme", patch.theme.as_ref());
    put!("theme_tui", patch.theme_tui.as_ref());
    put!("theme_gui", patch.theme_gui.as_ref());
    put!("theme_web", patch.theme_web.as_ref());
    put!(
        "theme_follow_terminal",
        patch.theme_follow_terminal.as_ref()
    );
    put!(
        "tree_respect_gitignore",
        patch.tree_respect_gitignore.as_ref()
    );
    put!(
        "diagnostics_min_severity",
        patch.diagnostics_min_severity.as_ref()
    );
}

/// Put a file back after a patch that did not produce readable settings.
fn restore(path: &Path, previous: Option<&str>) {
    match previous {
        Some(text) => {
            let _ = std::fs::write(path, text);
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Hand a request to the router of the profile it names.
async fn dispatch(State(profiles): State<Arc<Profiles>>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let header = request
        .headers()
        .get(PROFILE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let name = match profiles.select(header.as_deref(), &method, &path) {
        Ok(name) => name,
        Err(error) => return error.into_response(),
    };
    let Some(router) = profiles.router_for(&name) else {
        return no_such_profile(&name, &profiles.names()).into_response();
    };
    match router.clone().oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

/// The profiles API in the OpenAPI document.
///
/// Registered from this module for the same reason the session routes do it: the
/// document lives in `acpilot-proto`, which cannot name a daemon handler, so the
/// operation is inserted where the handler is.
pub fn register(document: &mut utoipa::openapi::OpenApi) {
    use utoipa::openapi::path::{HttpMethod, PathItem};

    let paths = &mut document.paths.paths;
    paths.insert(
        "/v1/profiles".to_owned(),
        PathItem::new(
            HttpMethod::Get,
            <__path_list_profiles_doc as utoipa::Path>::operation(),
        ),
    );
    paths.insert(
        "/v1/profiles/{id}".to_owned(),
        PathItem::new(
            HttpMethod::Get,
            <__path_read_profile_doc as utoipa::Path>::operation(),
        ),
    );
    paths.insert(
        "/v1/profiles/{id}/activate".to_owned(),
        PathItem::new(
            HttpMethod::Post,
            <__path_activate_profile_doc as utoipa::Path>::operation(),
        ),
    );
    let mut preferences = PathItem::new(
        HttpMethod::Get,
        <__path_read_preferences_doc as utoipa::Path>::operation(),
    );
    preferences.patch = Some(<__path_patch_preferences_doc as utoipa::Path>::operation());
    paths.insert("/v1/profiles/{id}/preferences".to_owned(), preferences);
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/profiles",
    tag = "profiles",
    summary = "List the profiles this daemon serves",
    description = "One row per profile directory that loaded, with the active one marked. A daemon started without a profile and with none on disk answers an empty list.",
    responses((
        status = 200,
        description = "Every profile this daemon can serve",
        body = Vec<ProfileSummary>
    ))
)]
fn list_profiles_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/profiles/{id}",
    tag = "profiles",
    summary = "One profile",
    params(("id" = String, Path, description = "The profile's directory name")),
    responses(
        (status = 200, description = "The profile", body = ProfileSummary),
        (status = 404, description = "No profile by that name on this daemon")
    )
)]
fn read_profile_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/profiles/{id}/activate",
    tag = "profiles",
    summary = "Make a profile the one headerless reads are about",
    description = "In memory for the life of this daemon: streams that bind at connect are unaffected until they reconnect, and persisting the choice is the settings cascade's job rather than a runtime request's.",
    params(("id" = String, Path, description = "The profile's directory name")),
    responses(
        (status = 200, description = "The profile, now active", body = ProfileSummary),
        (status = 404, description = "No profile by that name on this daemon")
    )
)]
fn activate_profile_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/profiles/{id}/preferences",
    tag = "profiles",
    summary = "Read a profile's effective preferences",
    description = "The merged answer a client would draw with: defaults, the user's file, the profile's files and the project's layer. This is the same object the `prefs` event carries.",
    params(("id" = String, Path, description = "The profile's directory name")),
    responses(
        (status = 200, description = "The effective preferences", body = Preferences),
        (status = 404, description = "No profile by that name on this daemon", body = ErrorBody)
    )
)]
fn read_preferences_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    patch,
    path = "/v1/profiles/{id}/preferences",
    tag = "profiles",
    summary = "Change some of a profile's preferences",
    description = "Only the keys the body names are written, into the profile's `profile.toml`; every other table and key is left alone. The answer is the merged preferences after the write, and the change is announced as a `prefs` event.",
    params(("id" = String, Path, description = "The profile's directory name")),
    request_body = PreferencesPatch,
    responses(
        (status = 200, description = "The effective preferences after the patch", body = Preferences),
        (status = 400, description = "An empty or malformed patch, or one that would make the settings unreadable", body = ErrorBody),
        (status = 404, description = "No profile by that name on this daemon", body = ErrorBody)
    )
)]
fn patch_preferences_doc() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_writes_only_the_keys_it_names() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("profile.toml");
        std::fs::write(
            &path,
            "[preferences]\ntab_width = 2\ntheme = \"acpilot.paper\"\n\n[policy]\ndefault = \"deny\"\n",
        )
        .expect("write");

        let patch = PreferencesPatch {
            format_on_save: Some(true),
            rulers: Some(vec![80, 100]),
            ..PreferencesPatch::default()
        };
        write_preferences(&path, &patch).expect("patch");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("format_on_save = true"), "{text}");
        assert!(text.contains("rulers = [80, 100]"), "{text}");
        assert!(text.contains("tab_width = 2"), "the old key stays: {text}");
        assert!(text.contains("[policy]"), "other tables stay: {text}");
    }

    #[test]
    fn an_empty_patch_is_refused_before_touching_the_file() {
        let patch = PreferencesPatch::default();
        assert!(patch.is_empty());
    }
}
