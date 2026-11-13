//! Running a recipe: one assembled request, one answer, one record.
//!
//! A run is not an agent turn. It has no tools, no permission prompts and no
//! session: the lab asks a model a question and writes down what happened. The
//! agent that *does* use tools is `acpilot-agent-lm` (P4.5-5); a promoted recipe is
//! that process, not this route.
//!
//! The response is an SSE stream rather than a `202` with a job id, because a run
//! is short and the caller is the pane that wants the text as it arrives. The
//! frames are `model.run.started`, `model.run.delta` and `model.run.finished`;
//! `acpilot-model` sends no streaming chunks yet, so a delta currently carries the
//! whole answer. The shape is the one the lab needs, and P4.5-11 is where a server
//! that streams is tested.
//!
//! The record is written before `finished` is sent. A pane that sees the frame has
//! a line in `runs.jsonl` to point at, which is what makes "why was it better an
//! hour ago" answerable.

use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Instant;

use acpilot_core::AcpilotError;
use acpilot_model::context::{Assembly, assemble};
use acpilot_model::{Backend, ChatMessage, ChatRequest, Provider, Role, Usage};
use acpilot_proto::ErrorBody;
use acpilot_proto::model::{RunPage, RunRequest, RunStatus, RunSummary, TokenUsage};
use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, Sse};
use axum::routing::post;
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::providers::Credential;
use crate::runs::{PageQuery, ResolvedFile, RunLog, RunRecord, StoredRequest};
use crate::{ApiError, AppState};

/// What a run needs from the query string.
#[derive(Debug, Deserialize)]
struct RunQuery {
    /// Workspace root the corpus selectors are resolved against.
    root: Option<PathBuf>,
}

/// What a page needs from the query string.
#[derive(Debug, Deserialize)]
struct PageParams {
    limit: Option<u32>,
    before: Option<String>,
}

/// The run routes.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/recipes/{id}/runs", post(start).get(page))
}

/// Everything a run needs, resolved before the first frame.
struct Plan {
    run_id: String,
    recipe: acpilot_proto::model::ModelRecipe,
    backend: Backend,
    request: RunRequest,
    root: PathBuf,
    started_at: String,
    log: RunLog,
}

/// Where a run's stream is.
enum Phase {
    /// Announce the run, then start it.
    Started,
    /// Assemble, call the model, record the answer.
    Running,
    /// Send the finished frame that the running step prepared.
    Finishing(Event),
    /// Nothing more to say.
    Ended,
}

struct Frames {
    plan: Plan,
    phase: Phase,
}

async fn start(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<RunQuery>,
    body: Result<Json<RunRequest>, JsonRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a run request: {rejection}"
        )))
    })?;
    let plan = prepare(&state, &id, request, query.root)?;
    let frames = Frames {
        plan,
        phase: Phase::Started,
    };
    Ok(Sse::new(futures::stream::unfold(
        Some(frames),
        |frames| async move {
            let mut frames = frames?;
            step(&mut frames)
                .await
                .map(|event| (Ok::<_, Infallible>(event), Some(frames)))
        },
    )))
}

/// Resolve the recipe, the provider and the log before any frame is sent.
fn prepare(
    state: &AppState,
    id: &str,
    request: RunRequest,
    root: Option<PathBuf>,
) -> Result<Plan, ApiError> {
    let Some(recipes) = state.recipes() else {
        return Err(ApiError::new(AcpilotError::invalid(
            "this daemon has no profile, so it has no recipes directory",
        )));
    };
    let Some(log) = state.run_log() else {
        return Err(ApiError::new(AcpilotError::invalid(
            "this daemon has no profile, so it has no run log",
        )));
    };
    let recipe = recipes
        .get(id)?
        .ok_or_else(|| ApiError::new(AcpilotError::not_found(format!("no recipe `{id}`"))))?;

    let root = match root {
        Some(root) => root,
        None if request.corpus.is_empty() => std::env::current_dir().unwrap_or_default(),
        None => {
            return Err(ApiError::new(AcpilotError::invalid(
                "a run with corpus selectors needs a `root` query parameter",
            ))
            .with_detail(
                "root",
                "an absolute workspace path, for example `/home/me/project`",
            ));
        }
    };

    let declaration = state
        .providers()
        .declaration(&recipe.provider_id)
        .ok_or_else(|| {
            ApiError::new(AcpilotError::ModelUnavailable {
                provider: recipe.provider_id.clone(),
                message: "the recipe names a provider this profile does not declare".to_owned(),
            })
        })?;
    if !declaration.enabled {
        return Err(ApiError::new(AcpilotError::ModelUnavailable {
            provider: recipe.provider_id.clone(),
            message: "the recipe names a disabled provider".to_owned(),
        }));
    }

    let add = declaration.request();
    let token = match Credential::resolve(state.providers().resolver(), &add) {
        Credential::None => None,
        Credential::Bearer(token) => Some(token),
        Credential::Unusable(reason) => {
            return Err(ApiError::new(AcpilotError::ModelUnavailable {
                provider: recipe.provider_id.clone(),
                message: format!("the credential could not be read: {reason}"),
            }));
        }
    };
    let provider = Provider::new(add.id, add.kind, add.base_url).with_token(token);
    let backend = Backend::new(provider).map_err(|error| {
        ApiError::new(AcpilotError::ModelUnavailable {
            provider: recipe.provider_id.clone(),
            message: error.to_string(),
        })
    })?;

    Ok(Plan {
        run_id: uuid::Uuid::now_v7().to_string(),
        recipe,
        backend,
        request,
        root,
        started_at: jiff::Timestamp::now().to_string(),
        log: log.clone(),
    })
}

/// Advance a run's stream by one frame.
async fn step(frames: &mut Frames) -> Option<Event> {
    match std::mem::replace(&mut frames.phase, Phase::Ended) {
        Phase::Started => {
            frames.phase = Phase::Running;
            Some(frame(
                "model.run.started",
                json!({
                    "run_id": frames.plan.run_id,
                    "recipe_id": frames.plan.recipe.id,
                    "provider_id": frames.plan.recipe.provider_id,
                    "model_id": frames.plan.recipe.model_id,
                    "started_at": frames.plan.started_at,
                }),
            ))
        }
        Phase::Running => {
            let (summary, content) = run_once(frames).await;
            let delta = (!content.is_empty()).then(|| {
                frame(
                    "model.run.delta",
                    json!({ "run_id": summary.id, "text": content }),
                )
            });
            let finished = frame("model.run.finished", json!({ "run": summary }));
            match delta {
                Some(delta) => {
                    frames.phase = Phase::Finishing(finished);
                    Some(delta)
                }
                None => Some(finished),
            }
        }
        Phase::Finishing(event) => Some(event),
        Phase::Ended => None,
    }
}

/// Assemble, call, and write the record. Never fails: a failed run is a record.
async fn run_once(frames: &mut Frames) -> (RunSummary, String) {
    let plan = &frames.plan;
    let clock = Instant::now();
    let assembly = assemble(
        &plan.root,
        &plan.recipe.system_prompt,
        &plan.request.corpus,
        &plan.recipe.context,
        &[],
        &[],
    );

    let (prompt, warnings, files) = match assembly {
        Ok(Assembly::Ready(prompt)) => {
            let files: Vec<ResolvedFile> = prompt
                .corpus
                .files
                .iter()
                .map(|file| ResolvedFile {
                    path: file.path.clone(),
                    bytes: file.bytes,
                    tokens: acpilot_model::context::estimate_tokens(
                        usize::try_from(file.bytes).unwrap_or(usize::MAX),
                    ),
                })
                .collect();
            let warnings: Vec<String> = prompt.notices.iter().map(ToString::to_string).collect();
            (Some(prompt), warnings, files)
        }
        Ok(Assembly::NeedsSummary { .. }) => (
            None,
            vec!["the recipe asked for a summary of an empty history".to_owned()],
            Vec::new(),
        ),
        Err(error) => (None, vec![error.to_string()], Vec::new()),
    };

    let stored = StoredRequest::capture(
        &plan.request.prompt,
        Some(&plan.recipe.system_prompt),
        &plan.request.corpus,
        files,
        plan.recipe.context.clone(),
    );

    let mut warnings = warnings;
    let mut stop_reason = None;
    let mut usage = Usage::default();
    let mut content = String::new();
    let mut message = None;

    if let Some(prompt) = prompt {
        let mut messages = prompt.history.clone();
        messages.push(ChatMessage {
            role: Role::User,
            content: format!("{}\n\n{}", prompt.corpus.render(), plan.request.prompt)
                .trim()
                .to_owned(),
        });
        let request = ChatRequest {
            model: plan.recipe.model_id.clone(),
            system_prompt: Some(prompt.system_prompt.clone()),
            messages,
            params: plan.recipe.sampling.clone(),
            tools: Vec::new(),
            json_schema: None,
        };
        match plan.backend.chat(&request).await {
            Ok(answer) => {
                content = answer.content;
                usage = answer.usage;
                stop_reason = answer.stop_reason;
                warnings.extend(
                    answer
                        .unsupported_params
                        .iter()
                        .map(|parameter| format!("the server refused `{parameter}`")),
                );
            }
            Err(error) => message = Some(error.to_string()),
        }
    }

    let total_ms = u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX);
    let summary = RunSummary {
        id: plan.run_id.clone(),
        recipe_id: plan.recipe.id.clone(),
        status: if message.is_none() {
            RunStatus::Succeeded
        } else {
            RunStatus::Failed
        },
        started_at: plan.started_at.clone(),
        first_token_ms: None,
        total_ms: Some(total_ms),
        tokens: TokenUsage {
            prompt: usage.prompt_tokens,
            completion: usage.completion_tokens,
            estimated: !usage.reported,
        },
        tokens_per_second: None,
        stop_reason,
        warnings: warnings.clone(),
        params: plan.recipe.sampling.clone(),
        corpus_files: u32::try_from(stored.files.len()).unwrap_or(u32::MAX),
        message,
    };

    let record = RunRecord::new(summary.clone(), stored);
    if let Err(error) = plan.log.append(&record) {
        warnings.push(format!("the run record could not be written: {error}"));
        let mut summary = summary;
        summary.warnings = warnings;
        return (summary, content);
    }
    (summary, content)
}

fn frame(event: &str, data: serde_json::Value) -> Event {
    Event::default().event(event).data(data.to_string())
}

async fn page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<PageParams>,
) -> Result<Json<RunPage>, ApiError> {
    let Some(log) = state.run_log() else {
        return Err(ApiError::new(AcpilotError::invalid(
            "this daemon has no profile, so it has no run log",
        )));
    };
    let page = log
        .page(
            &id,
            &PageQuery {
                limit: params.limit.unwrap_or(0),
                before: params.before,
            },
        )
        .map_err(|error| match error {
            crate::runs::RunsError::Cursor { .. } => {
                ApiError::new(AcpilotError::invalid(error.to_string()))
            }
            other => ApiError::new(AcpilotError::Io(std::io::Error::other(other.to_string()))),
        })?;
    Ok(Json(RunPage {
        runs: page.runs,
        next: page.next,
    }))
}

/// The run operations, for the OpenAPI document.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    vec![
        (
            "/v1/recipes/{id}/runs",
            HttpMethod::Post,
            <__path_start_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/recipes/{id}/runs",
            HttpMethod::Get,
            <__path_page_doc as utoipa::Path>::operation(),
        ),
    ]
}

#[allow(dead_code)]
#[utoipa::path(
    post,
    operation_id = "start_run",
    path = "/v1/recipes/{id}/runs",
    tag = "models",
    summary = "Run a recipe and stream the answer",
    description = "Assembles the prompt (master prompt, corpus selectors, budget), calls the configured provider once and appends a record to the profile's `runs.jsonl`. The response is an SSE stream: `model.run.started`, `model.run.delta`, `model.run.finished`. A run with corpus selectors needs a `root` query parameter. There is no job to poll and no second request: the stream is the run.",
    request_body = RunRequest,
    params(("id" = String, Path, description = "Recipe id"), ("root" = Option<String>, Query, description = "Workspace root for corpus selectors")),
    responses(
        (status = 200, description = "The run's frames", content_type = "text/event-stream"),
        (status = 400, description = "A body or corpus root this build cannot use", body = ErrorBody),
        (status = 404, description = "No such recipe", body = ErrorBody),
        (status = 502, description = "The provider is not declared, disabled, or unreachable", body = ErrorBody)
    )
)]
fn start_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    operation_id = "list_runs",
    path = "/v1/recipes/{id}/runs",
    tag = "models",
    summary = "One page of a recipe's runs, newest first",
    description = "The cursor is a run id, not an offset: the log only grows, and an offset would repeat or skip a line when something is appended between two pages. A cursor naming another recipe's run is `400`.",
    params(
        ("id" = String, Path, description = "Recipe id"),
        ("limit" = Option<u32>, Query, description = "Runs per page; 20 when absent"),
        ("before" = Option<String>, Query, description = "Return runs older than this run id")
    ),
    responses(
        (status = 200, description = "A page of summaries", body = RunPage),
        (status = 400, description = "The cursor does not belong to this recipe", body = ErrorBody)
    )
)]
fn page_doc() {}
