//! Model providers: the servers a profile can point a chat at.
//!
//! A provider is declared, never discovered. Nothing here scans the loopback
//! interface: a network call is something a person asked for, and a startup that
//! dials every port is a startup that hangs behind a firewall.
//!
//! Adding one *is* the explicit action the models phase probes on, so the answer to
//! `POST /v1/providers` is the server's own reply. A row reads `reachable` because
//! this daemon asked and was answered, and one that did not answer carries the
//! reason in `detail`. A disabled provider is not contacted at all and reads
//! `disabled`; nothing is ever "probably fine".
//!
//! A failed probe still adds the row. `502` would leave the person with a provider
//! they can neither see nor correct, while a row whose `detail` says the connection
//! was refused is a dialog with something to fix. The status is the report, so the
//! add itself is `201` either way.
//!
//! The declaration is written to the profile's `providers.toml` before the row is
//! kept, so a restart reads back what somebody declared rather than an empty list.
//! A row that came back that way reads `not_probed`: nothing was tried since the
//! daemon started, and `unreachable` would be a claim about a call nobody made.
//!
//! # What is not here
//!
//! A token is read from the keyring, under this profile's namespace, at the moment
//! of the probe — never at startup and never into an HTTP answer. A reference that
//! cannot be read keeps its row and says which entry to fix, because sending the
//! request anonymously would let the server take the blame for a local problem.
//!
//! The last answer is remembered in `providers.state.toml`, beside the declaration
//! and separate from it: a restart can then say "unreachable at 12:04" instead of
//! forgetting the one fact somebody needed. That file is derived data, so a copy
//! that will not parse is forgotten rather than failing a startup.
//!
//! A provider can be edited, probed again and removed. Removal is refused while a
//! recipe names it: deleting the server would leave a recipe that cannot run and
//! no clue why.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use acpilot_agent::{KeyringResolver, SecretRef, SecretResolver};
use acpilot_core::AcpilotError;
use acpilot_model::{Backend, Provider};
use acpilot_profile::providers::{DeclaredProvider, ProvidersError, ProvidersFile, ProvidersState};
use acpilot_proto::ErrorBody;
use acpilot_proto::model::{
    AddProviderRequest, ModelCatalogue, ModelProblem, ProviderStatus, ProviderSummary,
    UpdateProviderRequest,
};
use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::{ApiError, AppState};

/// How long a provider id may be, so a row cannot arrive as a paragraph.
const MAX_ID: usize = 64;

/// Why a request cannot become a provider.
///
/// Kept apart from the HTTP error so the CLI verb refuses with the same words the
/// route does: two spellings of "this URL has no scheme" would be two answers to
/// one question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The field to fix.
    pub field: &'static str,
    /// One sentence naming what is wrong.
    pub message: String,
    /// What a usable value looks like.
    pub detail: String,
}

/// Why a declaration was not kept.
#[derive(Debug)]
pub enum Declined {
    /// The id is already declared.
    Taken,
    /// No declaration has that id.
    Missing,
    /// A recipe names the id, so removing it would leave a recipe that cannot run.
    Referenced(Vec<String>),
    /// The declaration could not be read back or written.
    File(ProvidersError),
}

impl std::fmt::Display for Declined {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Taken => formatter.write_str("the id is already declared"),
            Self::Missing => formatter.write_str("no provider has that id"),
            Self::Referenced(recipes) => {
                write!(formatter, "recipes still name it: {}", recipes.join(", "))
            }
            Self::File(error) => write!(formatter, "{error}"),
        }
    }
}

/// The providers this daemon has been told about.
///
/// A registry rather than a file read per request: `GET /v1/providers` is what a
/// picker draws, and a list that changed between two reads would be a dialog that
/// flickers. Behind a mutex because an add is rare and a read is a clone.
#[derive(Debug, Default)]
pub struct Providers {
    known: Mutex<Vec<ProviderSummary>>,
    /// Where a declaration is written, when this daemon has a profile.
    ///
    /// The file is re-read on each add rather than trusted from the first load:
    /// somebody editing `providers.toml` between two adds should keep their edit,
    /// not have it replaced by a stale copy.
    file: Option<PathBuf>,
    /// Where the last answers are remembered, beside the declaration.
    state: Option<PathBuf>,
    /// The keyring a `credential_ref` resolves through, when this daemon has a
    /// profile to name in it.
    resolver: Option<Arc<KeyringResolver>>,
}

impl Providers {
    /// The providers a profile declares, with no answer yet.
    ///
    /// A missing file is an empty registry: a profile with no provider is the
    /// ordinary first-run state. A file that does not load is an error, because a
    /// daemon that answered "no providers" over a typo would look healthy and be
    /// wrong.
    ///
    /// # Errors
    ///
    /// The file cannot be read, is not this document, or declares one id twice.
    pub fn load(path: PathBuf) -> Result<Self, ProvidersError> {
        Self::load_with(path, None)
    }

    /// [`Self::load`], with the keyring that resolves a declaration's
    /// `credential_ref`.
    ///
    /// # Errors
    ///
    /// As [`Self::load`].
    pub fn load_with(
        path: PathBuf,
        resolver: Option<Arc<KeyringResolver>>,
    ) -> Result<Self, ProvidersError> {
        let declarations = ProvidersFile::load(&path)?;
        let state = path.parent().map(ProvidersState::path_in);
        let remembered = state
            .as_deref()
            .map(ProvidersState::load)
            .unwrap_or_default();
        // An answer describes one declaration, and the record keeps the fields it
        // was given for. A declaration that changed is `not_probed` again: the old
        // server's model count is not the new server's answer.
        let known = declarations
            .providers
            .iter()
            .map(|declaration| {
                remembered
                    .row(declaration)
                    .cloned()
                    .unwrap_or_else(|| row_from(declaration))
            })
            .collect();
        Ok(Self {
            known: Mutex::new(known),
            file: Some(path),
            state,
            resolver,
        })
    }

    /// The keyring this daemon resolves credentials through, when it has one.
    #[must_use]
    pub fn resolver(&self) -> Option<&KeyringResolver> {
        self.resolver.as_deref()
    }

    /// Every provider, in the order they were declared.
    #[must_use]
    pub fn list(&self) -> Vec<ProviderSummary> {
        self.remember();
        self.lock().clone()
    }

    /// The provider with this id, when one was added.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<ProviderSummary> {
        self.remember();
        self.lock().iter().find(|row| row.id == id).cloned()
    }

    /// Take the answers another process wrote since this one last looked.
    ///
    /// `acpilot models probe` runs without a daemon and writes the same file. Two
    /// surfaces that answered differently about one server would be a bug people
    /// blame on the server, and the file is a few hundred bytes.
    fn remember(&self) {
        let Some(state) = &self.state else {
            return;
        };
        let remembered = ProvidersState::load(state);
        if remembered.probes.is_empty() {
            return;
        }
        let mut known = self.lock();
        for row in known.iter_mut() {
            if row.status == ProviderStatus::Disabled {
                continue;
            }
            let answer = remembered.probes.iter().find(|record| {
                record.id == row.id
                    && record.kind == row.kind
                    && record.base_url == row.base_url
                    && record.credential_ref == row.credential_ref
            });
            if let Some(answer) = answer {
                *row = answer.summary.clone();
            }
        }
    }

    /// Record a declaration and its probe result, and write it down.
    ///
    /// The file is written before the row is kept: a row that a restart forgets is
    /// a declaration somebody made and lost, while a write that failed is an error
    /// they can see. `Taken` means the id is in use, which is a conflict rather
    /// than an update — replacing the row would move every recipe that names it to
    /// another server without saying so.
    ///
    /// # Errors
    ///
    /// The id is taken, or the declaration could not be read back or written.
    pub fn insert(
        &self,
        row: ProviderSummary,
        declaration: DeclaredProvider,
    ) -> Result<(), Declined> {
        let mut known = self.lock();
        if known.iter().any(|existing| existing.id == row.id) {
            return Err(Declined::Taken);
        }
        if let Some(path) = &self.file {
            let mut file = ProvidersFile::load(path).map_err(Declined::File)?;
            file.providers.push(declaration.clone());
            file.save(path).map_err(Declined::File)?;
        }
        known.push(row.clone());
        if let Some(state) = &self.state {
            let mut remembered = ProvidersState::load(state);
            remembered.record(&declaration, &row);
            // A memory that cannot be written is one question somebody repeats, not
            // a declaration lost: the add itself already succeeded.
            if let Err(error) = remembered.save(state) {
                tracing::warn!(%error, "the probe answer was not remembered");
            }
        }
        Ok(())
    }

    /// Every declaration, in file order.
    #[must_use]
    pub fn declarations(&self) -> Vec<DeclaredProvider> {
        let Some(path) = &self.file else {
            return Vec::new();
        };
        ProvidersFile::load(path)
            .map(|file| file.providers)
            .unwrap_or_default()
    }

    /// One declaration, in file order.
    #[must_use]
    pub fn declaration(&self, id: &str) -> Option<DeclaredProvider> {
        self.declarations().into_iter().find(|row| row.id == id)
    }

    /// Replace a declaration and its row.
    ///
    /// # Errors
    ///
    /// No such declaration, or the file cannot be written.
    pub fn replace(
        &self,
        declaration: DeclaredProvider,
        row: ProviderSummary,
    ) -> Result<(), Declined> {
        let Some(path) = &self.file else {
            return Err(Declined::Missing);
        };
        let mut file = ProvidersFile::load(path).map_err(Declined::File)?;
        let Some(slot) = file
            .providers
            .iter_mut()
            .find(|row| row.id == declaration.id)
        else {
            return Err(Declined::Missing);
        };
        *slot = declaration.clone();
        file.save(path).map_err(Declined::File)?;
        let mut known = self.lock();
        if let Some(slot) = known.iter_mut().find(|existing| existing.id == row.id) {
            *slot = row.clone();
        } else {
            known.push(row.clone());
        }
        drop(known);
        self.remember_answer(&declaration, &row);
        Ok(())
    }

    /// Remove a declaration, its row and its remembered answer.
    ///
    /// # Errors
    ///
    /// No such declaration, a recipe names it, or the file cannot be written.
    pub fn remove(&self, id: &str) -> Result<(), Declined> {
        let Some(path) = &self.file else {
            return Err(Declined::Missing);
        };
        if let Some(directory) = path.parent() {
            let recipes = acpilot_profile::recipes::Recipes::dir_in(directory);
            let named: Vec<String> = std::fs::read_dir(&recipes)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|entry| {
                            let text = std::fs::read_to_string(entry.path()).ok()?;
                            let recipe: acpilot_profile::recipes::RecipeFile =
                                toml::from_str(&text).ok()?;
                            (recipe.provider_id == id).then_some(recipe.id)
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !named.is_empty() {
                return Err(Declined::Referenced(named));
            }
        }

        let mut file = ProvidersFile::load(path).map_err(Declined::File)?;
        let before = file.providers.len();
        file.providers.retain(|row| row.id != id);
        if file.providers.len() == before {
            return Err(Declined::Missing);
        }
        file.save(path).map_err(Declined::File)?;
        self.lock().retain(|row| row.id != id);
        if let Some(state) = &self.state {
            let mut remembered = ProvidersState::load(state);
            remembered.probes.retain(|record| record.id != id);
            if let Err(error) = remembered.save(state) {
                tracing::warn!(%error, "the removed provider's answer was not forgotten");
            }
        }
        Ok(())
    }

    fn remember_answer(&self, declaration: &DeclaredProvider, row: &ProviderSummary) {
        if let Some(state) = &self.state {
            let mut remembered = ProvidersState::load(state);
            if row.status == ProviderStatus::Disabled {
                remembered.probes.retain(|record| record.id != row.id);
            } else {
                remembered.record(declaration, row);
            }
            if let Err(error) = remembered.save(state) {
                tracing::warn!(%error, "the probe answer was not remembered");
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<ProviderSummary>> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The provider routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/providers", get(list).post(create))
        .route("/v1/providers/{id}", patch(update).delete(remove))
        .route("/v1/providers/{id}/probe", post(probe_one))
        .route("/v1/models", get(models))
}

async fn list(State(state): State<AppState>) -> Json<Vec<ProviderSummary>> {
    Json(state.providers().list())
}

/// `POST /v1/providers` — declare a server, and ask it what it has.
async fn create(
    State(state): State<AppState>,
    body: Result<Json<AddProviderRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ProviderSummary>), ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a provider request: {rejection}"
        )))
    })?;
    if let Some(refusal) = refuse(&request) {
        return Err(ApiError::new(AcpilotError::invalid(refusal.message))
            .with_detail(refusal.field, refusal.detail));
    }
    if state.providers().get(&request.id).is_some() {
        return Err(duplicate(&request.id));
    }

    let credential = Credential::resolve(state.providers().resolver(), &request);
    let row = probe(&request, credential).await;
    let declaration = DeclaredProvider {
        id: request.id.clone(),
        kind: request.kind,
        // An empty label is stored as absent so the file says what was meant; the
        // row builder is what turns "no label" into the id.
        label: Some(request.label.clone()).filter(|label| !label.trim().is_empty()),
        base_url: request.base_url.clone(),
        credential_ref: request.credential_ref.clone(),
        enabled: request.enabled,
    };
    match state.providers().insert(row.clone(), declaration) {
        Ok(()) => Ok((StatusCode::CREATED, Json(row))),
        // Two adds of one id raced; the first one recorded is the answer.
        Err(Declined::Taken | Declined::Missing | Declined::Referenced(_)) => {
            Err(duplicate(&request.id))
        }
        Err(Declined::File(error)) => Err(ApiError::new(AcpilotError::Io(std::io::Error::other(
            format!(
                "`{}` answered but the declaration was not written: {error}",
                request.id
            ),
        )))),
    }
}

/// `PATCH /v1/providers/{id}` — change a declaration and ask it again.
///
/// Every change that can move a request is followed by one probe: a row that
/// described the old URL would be a claim about a server nobody asked.
async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateProviderRequest>, JsonRejection>,
) -> Result<Json<ProviderSummary>, ApiError> {
    let Json(patch) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a provider patch: {rejection}"
        )))
    })?;
    let Some(mut declaration) = state.providers().declaration(&id) else {
        return Err(missing(&id));
    };
    if let Some(label) = patch.label {
        declaration.label = Some(label);
    }
    if let Some(base_url) = patch.base_url {
        declaration.base_url = base_url;
    }
    if let Some(credential_ref) = patch.credential_ref {
        declaration.credential_ref = Some(credential_ref);
    }
    if let Some(enabled) = patch.enabled {
        declaration.enabled = enabled;
    }
    let request = declaration.request();
    if let Some(refusal) = refuse(&request) {
        return Err(ApiError::new(AcpilotError::invalid(refusal.message))
            .with_detail(refusal.field, refusal.detail));
    }
    let credential = Credential::resolve(state.providers().resolver(), &request);
    let row = probe(&request, credential).await;
    state
        .providers()
        .replace(declaration, row.clone())
        .map_err(declined)?;
    Ok(Json(row))
}

/// `DELETE /v1/providers/{id}`.
async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.providers().remove(&id).map_err(declined)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/providers/{id}/probe` — ask it again, and keep the answer.
async fn probe_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ProviderSummary>, ApiError> {
    let Some(declaration) = state.providers().declaration(&id) else {
        return Err(missing(&id));
    };
    let request = declaration.request();
    let credential = Credential::resolve(state.providers().resolver(), &request);
    let row = probe(&request, credential).await;
    state
        .providers()
        .replace(declaration, row.clone())
        .map_err(declined)?;
    Ok(Json(row))
}

#[derive(Debug, serde::Deserialize)]
struct ModelsQuery {
    provider: Option<String>,
}

/// `GET /v1/models` — what the declared providers report right now.
///
/// One server that cannot answer does not hide the others: its reason is beside
/// the list. A named provider that is disabled is reported rather than contacted,
/// because the declaration is the decision.
async fn models(
    State(state): State<AppState>,
    Query(query): Query<ModelsQuery>,
) -> Result<Json<ModelCatalogue>, ApiError> {
    let declarations = match query.provider {
        Some(id) => vec![
            state
                .providers()
                .declaration(&id)
                .ok_or_else(|| missing(&id))?,
        ],
        None => state.providers().declarations(),
    };
    let mut catalogue = ModelCatalogue {
        models: Vec::new(),
        problems: Vec::new(),
    };
    for declaration in declarations {
        if !declaration.enabled {
            catalogue.problems.push(ModelProblem {
                provider_id: declaration.id.clone(),
                message: "disabled, so nothing was sent".to_owned(),
            });
            continue;
        }
        let request = declaration.request();
        let token = match Credential::resolve(state.providers().resolver(), &request) {
            Credential::None => None,
            Credential::Bearer(token) => Some(token),
            Credential::Unusable(reason) => {
                catalogue.problems.push(ModelProblem {
                    provider_id: declaration.id.clone(),
                    message: format!("nothing was sent: {reason}"),
                });
                continue;
            }
        };
        let provider = Provider::new(request.id, request.kind, request.base_url).with_token(token);
        let answer = match Backend::new(provider) {
            Ok(backend) => backend.models().await,
            Err(error) => Err(error),
        };
        match answer {
            Ok(mut rows) => catalogue.models.append(&mut rows),
            Err(error) => catalogue.problems.push(ModelProblem {
                provider_id: declaration.id.clone(),
                message: error.to_string(),
            }),
        }
    }
    Ok(Json(catalogue))
}

fn missing(id: &str) -> ApiError {
    ApiError::new(AcpilotError::not_found(format!("no provider `{id}`")))
}

fn declined(error: Declined) -> ApiError {
    match error {
        Declined::Missing => missing("that id"),
        Declined::Taken | Declined::Referenced(_) => {
            ApiError::new(AcpilotError::conflict(error.to_string()))
        }
        Declined::File(error) => {
            ApiError::new(AcpilotError::Io(std::io::Error::other(error.to_string())))
        }
    }
}

fn duplicate(id: &str) -> ApiError {
    ApiError::new(AcpilotError::conflict(format!(
        "`{id}` is already declared; remove it before declaring another with that id"
    )))
}

/// Refuse what cannot be a provider before anything is sent.
///
/// The id is checked because it is a key in a file a person edits by hand, and the
/// URL because a scheme decides whether the request is TLS or plaintext — a typo
/// there should be a refusal, not a connection to the wrong port.
#[must_use]
pub fn refuse(request: &AddProviderRequest) -> Option<Refusal> {
    let id = request.id.as_str();
    let shape = !id.is_empty()
        && id.len() <= MAX_ID
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        });
    if !shape {
        return Some(Refusal {
            field: "id",
            message: format!("`{}` is not a usable provider id", request.id),
            detail: "one to 64 letters, digits, `-` or `_`, with no spaces around it".to_owned(),
        });
    }
    if request.base_url.trim() != request.base_url
        || !(request.base_url.starts_with("http://") || request.base_url.starts_with("https://"))
    {
        return Some(Refusal {
            field: "base_url",
            message: format!("`{}` is not a usable base URL", request.base_url),
            detail: "a scheme and a host with no trailing space, for example http://127.0.0.1:1234"
                .to_owned(),
        });
    }
    None
}

/// What a declaration's `credential_ref` produced.
///
/// Three outcomes rather than an `Option`, because "no token was declared" and
/// "a token was declared and could not be read" lead to different rows: the first
/// server is asked anonymously, the second is not asked at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// No reference was declared; the server is asked without a token.
    None,
    /// The reference resolved to this value.
    Bearer(String),
    /// The reference could not be read. Nothing is sent, and the row says why.
    Unusable(String),
}

impl Credential {
    /// Resolve a request's reference against a profile's keyring.
    ///
    /// A process with no keyring — a daemon started without a profile — cannot
    /// resolve one, and that is `Unusable` rather than a silent anonymous request:
    /// the server would answer `401` and the row would blame it for a local setup
    /// mistake.
    #[must_use]
    pub fn resolve(resolver: Option<&KeyringResolver>, request: &AddProviderRequest) -> Self {
        let Some(reference) = request.credential_ref.as_deref() else {
            return Self::None;
        };
        let Some(resolver) = resolver else {
            return Self::Unusable(format!(
                "`{reference}` names a keyring entry, and this process has no keyring to read it from"
            ));
        };
        match SecretRef::parse(reference).and_then(|parsed| resolver.resolve(&parsed)) {
            Ok(secret) => Self::Bearer(secret.expose().to_owned()),
            Err(error) => Self::Unusable(error.to_string()),
        }
    }
}

/// Ask the server what it has, and turn the answer into the row the API serves.
///
/// Public because `acpilot models probe` asks the same question with the same
/// words: a second mapper in the CLI would be a second opinion about what a
/// refused credential means.
pub async fn probe(request: &AddProviderRequest, credential: Credential) -> ProviderSummary {
    if !request.enabled {
        return row(
            request,
            ProviderStatus::Disabled,
            0,
            None,
            Some("disabled, so nothing was sent".to_owned()),
        );
    }

    let token = match credential {
        Credential::None => None,
        Credential::Bearer(token) => Some(token),
        // `last_probed_at` stays empty because nothing was asked: a timestamp here
        // would say the server was given a chance and refused, which is a different
        // problem with a different fix.
        Credential::Unusable(reason) => {
            return row(
                request,
                ProviderStatus::Unauthorized,
                0,
                None,
                Some(format!("nothing was sent: {reason}")),
            );
        }
    };

    let provider =
        Provider::new(request.id.clone(), request.kind, request.base_url.clone()).with_token(token);
    let answer = match Backend::new(provider) {
        Ok(backend) => backend.models().await,
        Err(error) => Err(error),
    };
    let probed_at = Some(jiff::Timestamp::now().to_string());

    match answer {
        Ok(models) => {
            let count = u32::try_from(models.len()).unwrap_or(u32::MAX);
            row(
                request,
                ProviderStatus::Reachable,
                count,
                probed_at,
                Some(format!(
                    "answered with {count} model{}",
                    if count == 1 { "" } else { "s" }
                )),
            )
        }
        // A refused credential is not a broken server: the row says which of the
        // two happened, because the fix is different.
        Err(AcpilotError::PermissionDenied { .. }) => row(
            request,
            ProviderStatus::Unauthorized,
            0,
            probed_at,
            Some("the server rejected the credential".to_owned()),
        ),
        Err(error) => row(
            request,
            ProviderStatus::Unreachable,
            0,
            probed_at,
            Some(error.to_string()),
        ),
    }
}

fn row(
    request: &AddProviderRequest,
    status: ProviderStatus,
    model_count: u32,
    last_probed_at: Option<String>,
    detail: Option<String>,
) -> ProviderSummary {
    ProviderSummary {
        id: request.id.clone(),
        kind: request.kind,
        // A label is for a person; the id is the name a recipe uses, so an empty
        // label reads as the id rather than as a blank row in a picker.
        label: if request.label.trim().is_empty() {
            request.id.clone()
        } else {
            request.label.clone()
        },
        base_url: request.base_url.clone(),
        status,
        model_count,
        credential_ref: request.credential_ref.clone(),
        last_probed_at,
        detail,
    }
}

/// A declaration read back from disk, before anything has asked it a question.
fn row_from(declaration: &DeclaredProvider) -> ProviderSummary {
    ProviderSummary {
        id: declaration.id.clone(),
        kind: declaration.kind,
        label: declaration
            .label
            .clone()
            .filter(|label| !label.trim().is_empty())
            .unwrap_or_else(|| declaration.id.clone()),
        base_url: declaration.base_url.clone(),
        status: if declaration.enabled {
            ProviderStatus::NotProbed
        } else {
            ProviderStatus::Disabled
        },
        model_count: 0,
        credential_ref: declaration.credential_ref.clone(),
        last_probed_at: None,
        detail: Some(if declaration.enabled {
            format!(
                "declared and not probed; `acpilot models probe {}` asks it",
                declaration.id
            )
        } else {
            "disabled, so nothing was sent".to_owned()
        }),
    }
}

/// The provider operations, for the OpenAPI document.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    vec![
        (
            "/v1/providers",
            HttpMethod::Get,
            <__path_list_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/providers",
            HttpMethod::Post,
            <__path_create_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/providers/{id}",
            HttpMethod::Patch,
            <__path_update_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/providers/{id}",
            HttpMethod::Delete,
            <__path_remove_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/providers/{id}/probe",
            HttpMethod::Post,
            <__path_probe_one_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/models",
            HttpMethod::Get,
            <__path_models_doc as utoipa::Path>::operation(),
        ),
    ]
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    operation_id = "list_providers",
    path = "/v1/providers",
    tag = "providers",
    summary = "The model servers this profile declares",
    description = "One row per declaration in `providers.toml`, in the order they appear. A row that no probe has answered since the daemon started reads `not_probed`; nothing is contacted to build this list.",
    responses((status = 200, description = "Every declared provider, with the last answer this daemon got", body = Vec<ProviderSummary>))
)]
fn list_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    operation_id = "add_provider",
    path = "/v1/providers",
    tag = "providers",
    summary = "Declare a model server, and ask it what it has",
    description = "Writes the declaration to the profile's `providers.toml` and probes it once. A refused or failed probe is still `201` with the reason in `detail`, because the row is what the person needs in order to correct it. One id is one provider: a second declaration of the same id is `409`.",
    request_body = AddProviderRequest,
    responses(
        (status = 201, description = "The provider, with the answer it gave or the reason it did not", body = ProviderSummary),
        (status = 400, description = "An id or base URL this build cannot use", body = ErrorBody),
        (status = 409, description = "The id is already declared", body = ErrorBody)
    )
)]
fn create_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    patch,
    operation_id = "update_provider",
    path = "/v1/providers/{id}",
    tag = "providers",
    summary = "Change a provider declaration, then ask it again",
    description = "Only the fields present move. The declaration is written to `providers.toml` and probed once, so the row describes the server that is declared now rather than the one it replaced.",
    request_body = UpdateProviderRequest,
    responses(
        (status = 200, description = "The provider, with the answer it gave or the reason it did not", body = ProviderSummary),
        (status = 400, description = "A URL or label this build cannot use", body = ErrorBody),
        (status = 404, description = "No such provider", body = ErrorBody)
    )
)]
fn update_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    delete,
    operation_id = "delete_provider",
    path = "/v1/providers/{id}",
    tag = "providers",
    summary = "Remove a provider declaration",
    description = "Removes the row from `providers.toml` and forgets its last answer. A recipe that still names it makes this `409`: deleting the server would leave a recipe with no clue why it cannot run.",
    responses(
        (status = 204, description = "The declaration is gone"),
        (status = 404, description = "No such provider", body = ErrorBody),
        (status = 409, description = "A recipe still names it", body = ErrorBody)
    )
)]
fn remove_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    operation_id = "probe_provider",
    path = "/v1/providers/{id}/probe",
    tag = "providers",
    summary = "Ask one provider what it has, and keep the answer",
    description = "The only route that contacts a server without a declaration changing. A disabled provider is reported as disabled and is not contacted. A refused credential is `unauthorized`, not `unreachable`.",
    responses(
        (status = 200, description = "The provider, with the answer it gave", body = ProviderSummary),
        (status = 404, description = "No such provider", body = ErrorBody)
    )
)]
fn probe_one_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    operation_id = "list_models",
    path = "/v1/models",
    tag = "providers",
    summary = "What the declared providers report right now",
    description = "Asks every enabled provider, or the one named by `?provider=`. A provider that cannot answer is listed in `problems` beside the models the others returned, because a list that quietly omitted a server would look like a server with no models.",
    params(("provider" = Option<String>, Query, description = "Only this provider id")),
    responses((status = 200, description = "Models and the providers that could not answer", body = ModelCatalogue))
)]
fn models_doc() {}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_agent::{CredentialStore, KEYRING_SERVICE};
    use acpilot_proto::model::ProviderKind;

    /// A keyring holding one account, so the resolution path is tested without a
    /// desktop session — the same reason the agent crate's backend is a trait.
    #[derive(Debug)]
    struct OneEntry(&'static str, &'static str);

    impl CredentialStore for OneEntry {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
            Ok((service == KEYRING_SERVICE && account == self.0).then(|| self.1.to_owned()))
        }

        fn set(&self, _service: &str, _account: &str, _value: &str) -> Result<(), String> {
            Err("this test store is read only".to_owned())
        }

        fn delete(&self, _service: &str, _account: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    fn request(id: &str, base_url: &str) -> AddProviderRequest {
        AddProviderRequest {
            id: id.to_owned(),
            kind: ProviderKind::OpenAiCompatible,
            label: String::new(),
            base_url: base_url.to_owned(),
            credential_ref: None,
            enabled: true,
        }
    }

    fn declaration(id: &str) -> DeclaredProvider {
        DeclaredProvider {
            id: id.to_owned(),
            kind: ProviderKind::LmStudio,
            label: None,
            base_url: "http://127.0.0.1:1234".to_owned(),
            credential_ref: None,
            enabled: true,
        }
    }

    #[test]
    fn an_id_that_is_not_a_key_is_refused() {
        for id in ["", "with space", "a/b", "ünïcode"] {
            assert!(
                refuse(&request(id, "http://127.0.0.1:1234")).is_some(),
                "{id}"
            );
        }
        assert!(refuse(&request(&"a".repeat(MAX_ID + 1), "http://127.0.0.1:1234")).is_some());
        assert!(refuse(&request("work-lm", "http://127.0.0.1:1234")).is_none());
    }

    #[test]
    fn a_url_without_a_scheme_is_refused_before_a_connection_is_tried() {
        for url in [
            "127.0.0.1:1234",
            "ftp://host",
            " http://host",
            "http://host ",
        ] {
            assert!(refuse(&request("lm", url)).is_some(), "{url}");
        }
        assert!(refuse(&request("lm", "https://api.example.com")).is_none());
    }

    #[test]
    fn an_occupancy_check_is_a_conflict_and_not_a_replacement() {
        let providers = Providers::default();
        let first = row(
            &request("lm", "http://127.0.0.1:1234"),
            ProviderStatus::Disabled,
            0,
            None,
            None,
        );
        assert!(providers.insert(first, declaration("lm")).is_ok());
        assert!(
            matches!(
                providers.insert(
                    row(
                        &request("lm", "http://127.0.0.1:9999"),
                        ProviderStatus::Disabled,
                        0,
                        None,
                        None
                    ),
                    declaration("lm")
                ),
                Err(Declined::Taken)
            ),
            "a second row with one id would move every recipe naming it"
        );
        assert_eq!(providers.list().len(), 1);
        assert_eq!(
            providers.get("lm").map(|row| row.base_url).as_deref(),
            Some("http://127.0.0.1:1234")
        );
    }

    #[test]
    fn a_declaration_is_written_before_the_row_is_kept() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = ProvidersFile::path_in(directory.path());
        let providers = Providers::load(path.clone()).expect("an empty file");
        let row = row(
            &request("lm", "http://127.0.0.1:1234"),
            ProviderStatus::Disabled,
            0,
            None,
            None,
        );
        providers
            .insert(row, declaration("lm"))
            .expect("the declaration is written");
        let text = std::fs::read_to_string(&path).expect("the file exists");
        assert!(text.contains("[[providers]]"), "{text}");
        assert!(text.contains("id = \"lm\""), "{text}");
        assert!(text.contains("schema_version = 1"), "{text}");
    }

    #[test]
    fn a_row_read_back_claims_nothing_it_did_not_measure() {
        let enabled = row_from(&declaration("lm"));
        assert_eq!(enabled.status, ProviderStatus::NotProbed);
        assert_eq!(enabled.model_count, 0);
        assert!(enabled.last_probed_at.is_none());
        assert!(
            enabled
                .detail
                .is_some_and(|detail| detail.contains("acpilot models probe lm")),
            "the row says how to ask"
        );

        let mut disabled = declaration("later");
        disabled.enabled = false;
        let disabled = row_from(&disabled);
        assert_eq!(disabled.status, ProviderStatus::Disabled);
        assert_eq!(disabled.label, "later", "an absent label reads as the id");
    }

    #[tokio::test]
    async fn a_disabled_provider_is_never_contacted() {
        // Port 1 refuses every connection, so a probe that ran would be
        // `unreachable`. The status is the proof that no request left.
        let mut disabled = request("lm", "http://127.0.0.1:1");
        disabled.enabled = false;
        let row = probe(&disabled, Credential::None).await;
        assert_eq!(row.status, ProviderStatus::Disabled);
        assert_eq!(row.model_count, 0);
        assert!(
            row.last_probed_at.is_none(),
            "nothing was asked, so nothing was probed at"
        );
    }

    #[tokio::test]
    async fn an_unreachable_provider_is_still_a_row() {
        let row = probe(&request("lm", "http://127.0.0.1:1"), Credential::None).await;
        assert_eq!(row.status, ProviderStatus::Unreachable);
        assert!(
            row.detail.is_some_and(|detail| !detail.is_empty()),
            "a refusal without a reason is a dialog with nothing to fix"
        );
    }

    #[test]
    fn a_reference_resolves_through_the_profiles_keyring() {
        let resolver = KeyringResolver::with_store("work", OneEntry("work/lm", "sk-test"));
        let mut with_token = request("lm", "http://127.0.0.1:1234");
        with_token.credential_ref = Some("keyring:lm".to_owned());
        assert_eq!(
            Credential::resolve(Some(&resolver), &with_token),
            Credential::Bearer("sk-test".to_owned()),
            "the account is <profile>/<name>, which is where the agent crate files a secret"
        );

        let bare = request("bare", "http://127.0.0.1:1234");
        assert_eq!(
            Credential::resolve(Some(&resolver), &bare),
            Credential::None
        );
        assert!(
            matches!(
                Credential::resolve(None, &with_token),
                Credential::Unusable(_)
            ),
            "a daemon with no keyring cannot honestly send the request anonymously"
        );
    }

    #[test]
    fn a_reference_with_no_entry_is_unusable_and_names_the_account() {
        let resolver = KeyringResolver::with_store("work", OneEntry("work/other", "sk-test"));
        let mut with_token = request("lm", "http://127.0.0.1:1234");
        with_token.credential_ref = Some("keyring:lm".to_owned());
        let Credential::Unusable(reason) = Credential::resolve(Some(&resolver), &with_token) else {
            panic!("a missing entry is not a token");
        };
        assert!(reason.contains("work/lm"), "the account to add: {reason}");
    }

    #[tokio::test]
    async fn a_credential_that_cannot_be_read_is_not_sent() {
        let mut with_token = request("lm", "http://127.0.0.1:1");
        with_token.credential_ref = Some("keyring:absent".to_owned());
        let row = probe(
            &with_token,
            Credential::Unusable("no entry for account \"work/absent\"".to_owned()),
        )
        .await;
        assert_eq!(row.status, ProviderStatus::Unauthorized);
        assert!(
            row.last_probed_at.is_none(),
            "nothing was asked, so a timestamp would blame the server"
        );
        let detail = row.detail.clone().unwrap_or_default();
        assert!(
            detail.contains("work/absent"),
            "the row names the entry to add: {detail}"
        );
    }

    #[test]
    fn an_empty_label_reads_as_the_id() {
        let row = row(
            &request("studio", "http://127.0.0.1:1234"),
            ProviderStatus::Disabled,
            0,
            None,
            None,
        );
        assert_eq!(row.label, "studio");
    }
}
