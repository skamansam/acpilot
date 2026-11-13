//! `POST /v1/recipes/{id}/promote`: one recipe becomes one `agents.toml` entry.
//!
//! Promotion is deliberately small. It does not start anything, install anything
//! or teach the daemon about prompts: it writes the line that makes
//! `acpilot-agent-lm` an ordinary `acp-stdio` agent, so the registry, the session
//! routes and the permission broker apply unchanged. The text edit and its rules
//! live in `acpilot-agent::promote`, which is also what `acpilot agents rm` uses;
//! this module only supplies the facts and the warnings.
//!
//! The warnings are the ticket's honesty clause. A workbench that refuses to let
//! somebody try a weak model is not a workbench, so nothing here blocks: a model
//! that does not answer the tools probe is promoted with a warning that says so,
//! and an unreachable server is promoted with a warning that says the probe never
//! ran. Both are in the response and nowhere else, because a warning nobody reads
//! is the same as no warning at all.
//!
//! The tools probe is asked once, here, on the explicit action. It is two chat
//! requests to the provider the recipe already names — the same call
//! `acpilot models probe` makes — so a promotion is not free, and it is not
//! something a startup does on somebody's behalf.

use acpilot_agent::promote::{self as manifest, PromoteError, Promoted};
use acpilot_core::AcpilotError;
use acpilot_model::{Backend, Provider};
use acpilot_proto::ErrorBody;
use acpilot_proto::model::{ModelRecipe, PromoteRecipeRequest, PromotedAgent};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::routing::post;
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::providers::Credential;
use crate::{ApiError, AppState};

/// The promotion route.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/recipes/{id}/promote", post(promote))
}

async fn promote(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Result<Json<PromoteRecipeRequest>, JsonRejection>,
) -> Result<Json<PromotedAgent>, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a promotion request: {rejection}"
        )))
    })?;
    let Some(recipes) = state.recipes() else {
        return Err(ApiError::new(AcpilotError::invalid(
            "this daemon has no profile, so it has no recipes directory",
        )));
    };
    let Some(root) = state.profile_root() else {
        return Err(ApiError::new(AcpilotError::invalid(
            "this daemon has no profile directory, so it has no `agents.toml` to write",
        )));
    };
    let recipe = recipes
        .get(&id)?
        .ok_or_else(|| ApiError::new(AcpilotError::not_found(format!("no recipe `{id}`"))))?;

    let warnings = warnings(&state, &recipe).await;
    let entry = Promoted {
        id: request.agent_id.clone(),
        name: request.name.clone().unwrap_or_else(|| recipe.label.clone()),
        // Relative to the profile, which is the directory `agents.toml` lives in:
        // an absolute path in a comment would name somebody's home directory in a
        // file they commit.
        recipe: format!("recipes/{}.toml", recipe.id),
        modified_at: recipe.updated_at.clone(),
        profile: state.profile_name().unwrap_or_default().to_owned(),
    };

    let manifest_path = manifest::path_in(root);
    manifest::promote(&manifest_path, &entry).map_err(|error| refusal(&error))?;

    let Some(registry) = state.registry() else {
        return Err(ApiError::new(AcpilotError::invalid(
            "this daemon has no agents registry, so the entry has nowhere to appear",
        )));
    };
    // The watcher would reload on its own, but the person who pressed the button
    // is looking at `GET /v1/agents` now, not after the debounce.
    registry.reload().map_err(|error| {
        ApiError::new(AcpilotError::invalid(format!(
            "the entry was written but the manifest no longer loads: {error}"
        )))
    })?;
    let agent = registry
        .get(&entry.id)
        .map(|spec| crate::trust::summary(&spec, None))
        .ok_or_else(|| {
            ApiError::new(AcpilotError::invalid(format!(
                "the entry was written but the registry does not list `{}`",
                entry.id
            )))
        })?;

    Ok(Json(PromotedAgent {
        agent,
        manifest_path: manifest_path.display().to_string(),
        warnings,
    }))
}

/// The caveats that travel with a promotion, not in a log file.
async fn warnings(state: &AppState, recipe: &ModelRecipe) -> Vec<String> {
    let mut warnings = Vec::new();
    if recipe.tools.search {
        warnings.push(
            "`tools.search` is exposed in the recipe, and the promoted agent does not offer it: \
             ACP has no workspace-search callback, so the agent says so in a notice instead of \
             running a shell to fake it"
                .to_owned(),
        );
    }
    let asks_for_tools = recipe.tools.read_files
        || recipe.tools.write_files
        || recipe.tools.run_commands
        || recipe.tools.search;
    if !asks_for_tools {
        // Nothing offered, nothing to confirm: a prompt-only agent cannot have a
        // tool-call failure, so probing it would be two requests for a fact the
        // recipe already answers.
        return warnings;
    }
    match tools_probe(state, recipe).await {
        Ok(true) => {}
        Ok(false) => warnings.push(format!(
            "`{}` did not call the tools probe, so nothing confirms it understands tool \
             schemas. The agent will repair a tool call once and then say it failed rather \
             than guess an argument",
            recipe.model_id
        )),
        Err(reason) => warnings.push(format!(
            "the tools probe could not run, so tool support is unconfirmed: {reason}"
        )),
    }
    warnings
}

/// Ask the model one tool-shaped question, the way `acpilot models probe` does.
async fn tools_probe(state: &AppState, recipe: &ModelRecipe) -> Result<bool, String> {
    let declaration = state
        .providers()
        .declaration(&recipe.provider_id)
        .ok_or_else(|| format!("the profile does not declare `{}`", recipe.provider_id))?;
    if !declaration.enabled {
        return Err(format!("`{}` is disabled", declaration.id));
    }
    let add = declaration.request();
    let token = match Credential::resolve(state.providers().resolver(), &add) {
        Credential::None => None,
        Credential::Bearer(token) => Some(token),
        Credential::Unusable(reason) => return Err(reason),
    };
    let provider = Provider::new(add.id, add.kind, add.base_url).with_token(token);
    let backend = Backend::new(provider).map_err(|error| error.to_string())?;
    let probe = backend
        .probe(&recipe.model_id)
        .await
        .map_err(|error| error.to_string())?;
    Ok(probe.tools)
}

/// A `PromoteError` as the API's own envelope.
///
/// A collision is `409` because the fix is a decision — keep the hand-written
/// entry, or remove it — not a corrected request. A manifest that would not load
/// is `400`: the body named an id the loader refuses.
fn refusal(error: &PromoteError) -> ApiError {
    match error {
        PromoteError::HandWritten { .. } => {
            ApiError::new(AcpilotError::conflict(error.to_string()))
        }
        PromoteError::Missing { .. } => ApiError::new(AcpilotError::not_found(error.to_string())),
        PromoteError::Invalid { .. } => ApiError::new(AcpilotError::invalid(error.to_string())),
        PromoteError::Io { .. } => {
            ApiError::new(AcpilotError::Io(std::io::Error::other(error.to_string())))
        }
    }
}

/// The promotion operation, for the OpenAPI document.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    vec![(
        "/v1/recipes/{id}/promote",
        HttpMethod::Post,
        <__path_promote_doc as utoipa::Path>::operation(),
    )]
}

#[allow(dead_code)]
#[utoipa::path(
    post,
    operation_id = "promote_recipe",
    path = "/v1/recipes/{id}/promote",
    tag = "recipes",
    summary = "Write a recipe into `agents.toml` as an agent",
    description = "Writes one `acp-stdio` entry whose command is `acpilot-agent-lm`, with a comment naming the recipe file and its mtime. An entry with the same id that promotion did not write is `409` and is left untouched. The tools probe is asked once and its answer, when it is bad or could not be given, comes back in `warnings`; warnings never block the write. Removing the entry is `acpilot agents rm`.",
    request_body = PromoteRecipeRequest,
    responses(
        (status = 200, description = "The agent as `GET /v1/agents` will now report it", body = PromotedAgent),
        (status = 400, description = "No profile directory, or an id the manifest loader refuses", body = ErrorBody),
        (status = 404, description = "No such recipe", body = ErrorBody),
        (status = 409, description = "A hand-written entry already owns the id", body = ErrorBody)
    )
)]
fn promote_doc() {}
