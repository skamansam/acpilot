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
//! The bearer token is not read. `credential_ref` names a keyring entry, and the
//! half of this task that owns secrets resolves it; until then a server that wants
//! one answers `unauthorized`, which is the honest answer rather than a guessed
//! pass. Nothing removes a provider, and the last probe's answer is not cached, so
//! `acpilot models probe` is the way to ask again.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use acpilot_core::AcpilotError;
use acpilot_model::{Backend, Provider};
use acpilot_profile::providers::{DeclaredProvider, ProvidersError, ProvidersFile};
use acpilot_proto::ErrorBody;
use acpilot_proto::model::{AddProviderRequest, ProviderStatus, ProviderSummary};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::routing::get;
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
    /// The declaration could not be read back or written.
    File(ProvidersError),
}

impl std::fmt::Display for Declined {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Taken => formatter.write_str("the id is already declared"),
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
        let declarations = ProvidersFile::load(&path)?;
        Ok(Self {
            known: Mutex::new(declarations.providers.iter().map(row_from).collect()),
            file: Some(path),
        })
    }

    /// Every provider, in the order they were declared.
    #[must_use]
    pub fn list(&self) -> Vec<ProviderSummary> {
        self.lock().clone()
    }

    /// The provider with this id, when one was added.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<ProviderSummary> {
        self.lock().iter().find(|row| row.id == id).cloned()
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
            file.providers.push(declaration);
            file.save(path).map_err(Declined::File)?;
        }
        known.push(row);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<ProviderSummary>> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The provider routes.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/providers", get(list).post(create))
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

    let row = probe(&request).await;
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
        Err(Declined::Taken) => Err(duplicate(&request.id)),
        Err(Declined::File(error)) => Err(ApiError::new(AcpilotError::Io(std::io::Error::other(
            format!(
                "`{}` answered but the declaration was not written: {error}",
                request.id
            ),
        )))),
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

/// Ask the server what it has, and turn the answer into the row the API serves.
///
/// Public because `acpilot models probe` asks the same question with the same
/// words: a second mapper in the CLI would be a second opinion about what a
/// refused credential means.
pub async fn probe(request: &AddProviderRequest) -> ProviderSummary {
    if !request.enabled {
        return row(
            request,
            ProviderStatus::Disabled,
            0,
            None,
            Some("disabled, so nothing was sent".to_owned()),
        );
    }

    let provider = Provider::new(request.id.clone(), request.kind, request.base_url.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_proto::model::ProviderKind;

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
        let row = probe(&disabled).await;
        assert_eq!(row.status, ProviderStatus::Disabled);
        assert_eq!(row.model_count, 0);
        assert!(
            row.last_probed_at.is_none(),
            "nothing was asked, so nothing was probed at"
        );
    }

    #[tokio::test]
    async fn an_unreachable_provider_is_still_a_row() {
        let row = probe(&request("lm", "http://127.0.0.1:1")).await;
        assert_eq!(row.status, ProviderStatus::Unreachable);
        assert!(
            row.detail.is_some_and(|detail| !detail.is_empty()),
            "a refusal without a reason is a dialog with nothing to fix"
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
