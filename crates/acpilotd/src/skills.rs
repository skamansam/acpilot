//! Agent Skills: list what this workspace and profile provide, and write one.
//!
//! A skill is a directory with a `SKILL.md`. The parser in `acpilot-skills` is the
//! only authority on whether one is valid. This module maps that parser to the
//! wire shape and chooses where a saved skill goes — the workspace's
//! `.acpilot/skills/` or the profile's `skills/` — so the lab never has to know
//! the directory layout.
//!
//! Writes are atomic and validated before the old file is replaced: a rejected
//! edit leaves the last good `SKILL.md` in place.

use std::path::PathBuf;

use acpilot_core::AcpilotError;
use acpilot_proto::skills::{SkillDestination, SkillDraft, SkillSource, SkillSummary};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::routing::get;
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::{ApiError, AppState};

/// The skills routes.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/skills", get(list).post(save))
}

/// The skills operations, for the OpenAPI document.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    vec![
        (
            "/v1/skills",
            HttpMethod::Get,
            <__path_list_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/skills",
            HttpMethod::Post,
            <__path_save_doc as utoipa::Path>::operation(),
        ),
    ]
}

/// `GET /v1/skills`.
async fn list(State(state): State<AppState>) -> Result<Json<Vec<SkillSummary>>, ApiError> {
    let workspace = workspace(&state)?;
    let profile = state.profile_root();
    // A malformed `SKILL.md` costs the picker that one row. Refusing the whole
    // list would hide every skill a person has because one file is broken.
    let discovered = acpilot_skills::discover_scoped_recovering(&workspace, profile, &[]);
    for problem in &discovered.problems {
        tracing::warn!(%problem, "a skill is missing from the list");
    }
    Ok(Json(discovered.skills.into_iter().map(summary).collect()))
}

/// `POST /v1/skills`: create or replace one skill.
async fn save(
    State(state): State<AppState>,
    body: Result<Json<SkillDraft>, JsonRejection>,
) -> Result<Json<SkillSummary>, ApiError> {
    let Json(draft) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a skill draft: {rejection}"
        )))
    })?;
    let workspace = workspace(&state)?;
    let (root, source) = match draft.destination {
        SkillDestination::Workspace => (workspace, acpilot_skills::SkillSource::Workspace),
        SkillDestination::Profile => (
            state.profile_root().map(PathBuf::from).ok_or_else(|| {
                ApiError::new(AcpilotError::conflict(
                    "this daemon has no profile, so a profile skill has nowhere to live",
                ))
            })?,
            acpilot_skills::SkillSource::Profile,
        ),
    };
    let skill = acpilot_skills::save_skill(
        &root,
        source,
        draft.name.trim(),
        &draft.description,
        &draft.instructions,
    )
    .map_err(|error| ApiError::new(AcpilotError::invalid(error.to_string())))?;
    Ok(Json(summary(skill)))
}

fn workspace(state: &AppState) -> Result<PathBuf, ApiError> {
    state
        .workspace()
        .map(|workspace| workspace.root.clone())
        .ok_or_else(|| {
            ApiError::new(AcpilotError::conflict(
                "this daemon has no workspace, so there are no skills to list",
            ))
        })
}

fn summary(skill: acpilot_skills::Skill) -> SkillSummary {
    SkillSummary {
        name: skill.name,
        description: skill.description,
        instructions: skill.instructions,
        source: match skill.source {
            acpilot_skills::SkillSource::Workspace => SkillSource::Workspace,
            acpilot_skills::SkillSource::Profile => SkillSource::Profile,
            acpilot_skills::SkillSource::Extension => SkillSource::Extension,
        },
        path: skill.directory.display().to_string(),
    }
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    operation_id = "list_skills",
    path = "/v1/skills",
    tag = "skills",
    summary = "List the skills this workspace can see",
    description = "Workspace skills override profile skills, which override those an activated extension ships. Instructions are included for preview.",
    responses((status = 200, description = "One row per skill", body = Vec<acpilot_proto::skills::SkillSummary>))
)]
fn list_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    operation_id = "save_skill",
    path = "/v1/skills",
    tag = "skills",
    summary = "Create or replace a skill",
    description = "The draft is validated by the same parser that reads `SKILL.md`; a rejected draft leaves the previous file intact. `destination` chooses the workspace or the profile.",
    request_body = acpilot_proto::skills::SkillDraft,
    responses(
        (status = 200, description = "The written skill", body = acpilot_proto::skills::SkillSummary),
        (status = 400, description = "The draft is not a valid skill", body = acpilot_proto::ErrorBody),
        (status = 409, description = "No workspace or profile to write into", body = acpilot_proto::ErrorBody)
    )
)]
fn save_doc() {}
