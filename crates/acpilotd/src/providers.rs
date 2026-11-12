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
//! # What is not here
//!
//! The bearer token is not read. `credential_ref` names a keyring entry, and the
//! half of this task that owns secrets resolves it; until then a server that wants
//! one answers `unauthorized`, which is the honest answer rather than a guessed
//! pass. The profile's `[providers]` table is where a restart reads these rows back,
//! which is the recipe work's file, so this registry is the routes' shared state.

use std::sync::{Mutex, PoisonError};

use acpilot_core::AcpilotError;
use acpilot_model::{Backend, Provider};
use acpilot_proto::model::{AddProviderRequest, ProviderStatus, ProviderSummary};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::routing::get;

use crate::{ApiError, AppState};

/// How long a provider id may be, so a row cannot arrive as a paragraph.
const MAX_ID: usize = 64;

/// The providers this daemon has been told about.
///
/// A registry rather than a file read per request: `GET /v1/providers` is what a
/// picker draws, and a list that changed between two reads would be a dialog that
/// flickers. Behind a mutex because an add is rare and a read is a clone.
#[derive(Debug, Default)]
pub struct Providers {
    known: Mutex<Vec<ProviderSummary>>,
}

impl Providers {
    /// Every provider, in the order they were added.
    #[must_use]
    pub fn list(&self) -> Vec<ProviderSummary> {
        self.lock().clone()
    }

    /// The provider with this id, when one was added.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<ProviderSummary> {
        self.lock().iter().find(|row| row.id == id).cloned()
    }

    /// Record a provider. `false` means the id is taken, which is a conflict rather
    /// than an update: replacing the row would move every recipe that names it to
    /// another server without saying so.
    pub fn insert(&self, row: ProviderSummary) -> bool {
        let mut known = self.lock();
        if known.iter().any(|existing| existing.id == row.id) {
            return false;
        }
        known.push(row);
        true
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
    validate(&request)?;
    if state.providers().get(&request.id).is_some() {
        return Err(duplicate(&request.id));
    }

    let row = probe(&request).await;
    if !state.providers().insert(row.clone()) {
        // Two adds of one id raced; the first one recorded is the answer.
        return Err(duplicate(&request.id));
    }
    Ok((StatusCode::CREATED, Json(row)))
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
fn validate(request: &AddProviderRequest) -> Result<(), ApiError> {
    let id = request.id.as_str();
    let shape = !id.is_empty()
        && id.len() <= MAX_ID
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        });
    if !shape {
        return Err(ApiError::new(AcpilotError::invalid(format!(
            "`{}` is not a usable provider id",
            request.id
        )))
        .with_detail(
            "id",
            "one to 64 letters, digits, `-` or `_`, with no spaces around it",
        ));
    }
    if request.base_url.trim() != request.base_url
        || !(request.base_url.starts_with("http://") || request.base_url.starts_with("https://"))
    {
        return Err(ApiError::new(AcpilotError::invalid(format!(
            "`{}` is not a usable base URL",
            request.base_url
        )))
        .with_detail(
            "base_url",
            "a scheme and a host with no trailing space, for example http://127.0.0.1:1234",
        ));
    }
    Ok(())
}

/// Ask the server what it has, and turn the answer into the row the API serves.
async fn probe(request: &AddProviderRequest) -> ProviderSummary {
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

    #[test]
    fn an_id_that_is_not_a_key_is_refused() {
        for id in ["", "with space", "a/b", "ünïcode"] {
            assert!(
                validate(&request(id, "http://127.0.0.1:1234")).is_err(),
                "{id}"
            );
        }
        assert!(validate(&request(&"a".repeat(MAX_ID + 1), "http://127.0.0.1:1234")).is_err());
        assert!(validate(&request("work-lm", "http://127.0.0.1:1234")).is_ok());
    }

    #[test]
    fn a_url_without_a_scheme_is_refused_before_a_connection_is_tried() {
        for url in [
            "127.0.0.1:1234",
            "ftp://host",
            " http://host",
            "http://host ",
        ] {
            assert!(validate(&request("lm", url)).is_err(), "{url}");
        }
        assert!(validate(&request("lm", "https://api.example.com")).is_ok());
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
        assert!(providers.insert(first));
        assert!(
            !providers.insert(row(
                &request("lm", "http://127.0.0.1:9999"),
                ProviderStatus::Disabled,
                0,
                None,
                None
            )),
            "a second row with one id would move every recipe naming it"
        );
        assert_eq!(providers.list().len(), 1);
        assert_eq!(
            providers.get("lm").map(|row| row.base_url).as_deref(),
            Some("http://127.0.0.1:1234")
        );
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
