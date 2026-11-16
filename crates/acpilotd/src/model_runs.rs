//! Running a recipe: one assembled request, one answer, one record.
//!
//! A run is not an agent turn. It has no tools, no permission prompts and no
//! session: the lab asks a model a question and writes down what happened. The
//! agent that *does* use tools is `acpilot-agent-lm` (P4.5-5); a promoted recipe is
//! that process, not this route.
//!
//! The response is an SSE stream rather than a `202` with a job id, because a run
//! is short and the caller is the pane that wants the text as it arrives. The
//! frames are `model.run.started`, `model.run.delta` and `model.run.finished`. A
//! delta carries text as the provider produces it; a provider that ignores
//! `stream: true` answers one body, and the run emits one delta for it.
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
use acpilot_proto::model::{
    RecordedRunRequest, ResolvedCorpusFile, RunPage, RunRecordView, RunRequest, RunStatus,
    RunSummary, TokenUsage,
};
use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, Sse};
use axum::routing::{get, post};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
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
    Router::new()
        .route("/v1/recipes/{id}/runs", post(start).get(page))
        .route("/v1/recipes/{id}/runs/{run}", get(record))
        .route("/v1/recipes/{id}/runs/{run}/replay", post(replay))
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
    /// The run is going; each poll takes the next piece of assistant text.
    Streaming {
        /// Text as the provider produces it. Closed when the run ends, which is
        /// also when its record has been written.
        deltas: mpsc::UnboundedReceiver<String>,
        /// The run itself, spawned so it keeps going while this stream waits for
        /// its next delta. It must not be merely stored: a future nobody polls
        /// never produces a delta and never closes the channel.
        run: tokio::task::JoinHandle<RunSummary>,
    },
    /// Nothing more to say.
    Ended,
}

struct Frames {
    /// The id the frames name. Kept outside the run future, which owns the rest
    /// of the plan, because every delta carries it.
    run_id: String,
    plan: Option<Plan>,
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
    Ok(frames(plan))
}

/// Run a recorded request again, unchanged.
///
/// The prompt and the selectors come from the record, not from the caller: a
/// replay that accepted a new prompt would be a new run wearing an old run's
/// name. The corpus itself is read from `root` again, because the record keeps
/// the files' paths and sizes rather than their bytes; a workspace that changed
/// since is a different experiment, and the record that comes back says what it
/// actually sent.
///
/// The replay appends its own record with its own id. It is not a retry of the
/// old one and must not overwrite it: the point of replaying is to compare the
/// two lines.
async fn replay(
    State(state): State<AppState>,
    Path((id, run_id)): Path<(String, String)>,
    Query(query): Query<RunQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let Some(log) = state.run_log() else {
        return Err(ApiError::new(AcpilotError::invalid(
            "this daemon has no profile, so it has no run log",
        )));
    };
    let record = log.get(&id, &run_id).map_err(|error| match error {
        crate::runs::RunsError::NotFound { .. } => {
            ApiError::new(AcpilotError::not_found(error.to_string()))
        }
        other => ApiError::new(AcpilotError::Io(std::io::Error::other(other.to_string()))),
    })?;
    let Some(record) = record else {
        return Err(ApiError::new(AcpilotError::not_found(format!(
            "no run `{run_id}` for recipe `{id}`"
        ))));
    };
    let plan = prepare(&state, &id, record.request.request(), query.root)?;
    Ok(frames(plan))
}

/// The SSE body a prepared run becomes.
fn frames(plan: Plan) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let frames = Frames {
        run_id: plan.run_id.clone(),
        plan: Some(plan),
        phase: Phase::Started,
    };
    Sse::new(futures::stream::unfold(Some(frames), |frames| async move {
        let mut frames = frames?;
        step(&mut frames)
            .await
            .map(|event| (Ok::<_, Infallible>(event), Some(frames)))
    }))
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
            let plan = frames.plan.take().expect("a run starts once");
            let started = frame(
                "model.run.started",
                json!({
                    "run_id": plan.run_id,
                    "recipe_id": plan.recipe.id,
                    "provider_id": plan.recipe.provider_id,
                    "model_id": plan.recipe.model_id,
                    "started_at": plan.started_at,
                }),
            );
            let (sender, deltas) = mpsc::unbounded_channel();
            frames.phase = Phase::Streaming {
                deltas,
                run: tokio::spawn(run_once(plan, sender)),
            };
            Some(started)
        }
        Phase::Streaming { mut deltas, run } => {
            match deltas.recv().await {
                Some(text) => {
                    let delta = frame(
                        "model.run.delta",
                        json!({ "run_id": frames.run_id, "text": text }),
                    );
                    frames.phase = Phase::Streaming { deltas, run };
                    Some(delta)
                }
                // The sender closes only when the run returns, and the record is
                // written before that, so this frame never points at nothing.
                None => {
                    // A run reports its own failures as records, so a join error is
                    // a bug rather than a failed run.
                    let summary = run.await.expect("the run task reports its own failures");
                    Some(frame("model.run.finished", json!({ "run": summary })))
                }
            }
        }
        Phase::Ended => None,
    }
}

/// Assemble, call, and write the record. Never fails: a failed run is a record.
///
/// It owns the plan so it can run while the stream polls the deltas it sends.
async fn run_once(plan: Plan, deltas: mpsc::UnboundedSender<String>) -> RunSummary {
    let plan = &plan;
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
        // Each delta goes straight down the stream: the lab draws the answer as
        // the model writes it, not when the model stops.
        let mut streamed = false;
        match plan
            .backend
            .chat_stream(&request, &mut |text| {
                streamed = true;
                let _ = deltas.send(text.to_owned());
            })
            .await
        {
            Ok(answer) => {
                // A provider that ignored `stream: true` answered one body, so
                // this is the only chance to send its text.
                if !streamed && !answer.content.is_empty() {
                    let _ = deltas.send(answer.content.clone());
                }
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
        return summary;
    }
    summary
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

/// One run's record: the summary and the request that produced it.
///
/// The page returns summaries because that is what a history draws; the lab also
/// asks what a single run read, which lives in the record's request half. A run
/// the log does not hold is `404`, including one that belongs to another recipe.
async fn record(
    State(state): State<AppState>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<Json<RunRecordView>, ApiError> {
    let Some(log) = state.run_log() else {
        return Err(ApiError::new(AcpilotError::invalid(
            "this daemon has no profile, so it has no run log",
        )));
    };
    match log.get(&id, &run_id) {
        Ok(Some(record)) => Ok(Json(record_view(&record))),
        Ok(None) => Err(ApiError::new(AcpilotError::not_found(format!(
            "no run `{run_id}` for recipe `{id}`"
        )))),
        Err(error) => Err(ApiError::new(AcpilotError::Io(std::io::Error::other(
            error.to_string(),
        )))),
    }
}

/// The wire shape of a stored record.
fn record_view(record: &RunRecord) -> RunRecordView {
    RunRecordView {
        schema_version: record.schema_version,
        summary: record.summary.clone(),
        request: RecordedRunRequest {
            prompt: record.request.prompt.clone(),
            prompt_sha256: record.request.prompt_sha256.clone(),
            system_prompt_sha256: record.request.system_prompt_sha256.clone(),
            corpus: record.request.corpus.clone(),
            files: record
                .request
                .files
                .iter()
                .map(|file| ResolvedCorpusFile {
                    path: file.path.clone(),
                    bytes: file.bytes,
                    tokens: file.tokens,
                })
                .collect(),
            budget: record.request.budget.clone(),
        },
    }
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
        (
            "/v1/recipes/{id}/runs/{run}",
            HttpMethod::Get,
            <__path_record_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/recipes/{id}/runs/{run}/replay",
            HttpMethod::Post,
            <__path_replay_doc as utoipa::Path>::operation(),
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

#[allow(dead_code)]
#[utoipa::path(
    get,
    operation_id = "get_run",
    path = "/v1/recipes/{id}/runs/{run}",
    tag = "models",
    summary = "One run's record: the answer and the request that produced it",
    description = "The page returns summaries because that is what a history draws. This returns the stored request as well: the prompt, the corpus selectors, the files they resolved to with sizes, and the budget the assembly planned against. A run the log does not hold is `404`, including a run id that belongs to another recipe.",
    params(
        ("id" = String, Path, description = "Recipe id"),
        ("run" = String, Path, description = "Run id")
    ),
    responses(
        (status = 200, description = "The run's record", body = RunRecordView),
        (status = 404, description = "No such run for this recipe", body = ErrorBody)
    )
)]
fn record_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    operation_id = "replay_run",
    path = "/v1/recipes/{id}/runs/{run}/replay",
    tag = "models",
    summary = "Run a recorded request again, unchanged",
    description = "Re-sends the prompt and corpus selectors the record holds, through the recipe as it is now, and appends a new record with its own id: a replay is an experiment to compare with the first line, not a retry that overwrites it. The corpus is read from `root` again because the record stores paths and sizes rather than file bytes, so a replay is byte-identical only while the workspace and recipe are unchanged.",
    params(
        ("id" = String, Path, description = "Recipe id"),
        ("run" = String, Path, description = "Run id to replay"),
        ("root" = Option<String>, Query, description = "Workspace root for corpus selectors")
    ),
    responses(
        (status = 200, description = "The new run's frames", content_type = "text/event-stream"),
        (status = 404, description = "No such run for this recipe", body = ErrorBody),
        (status = 502, description = "The provider is not declared, disabled, or unreachable", body = ErrorBody)
    )
)]
fn replay_doc() {}
