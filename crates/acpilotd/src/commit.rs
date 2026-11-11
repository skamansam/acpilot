//! Commit messages: the prompt a profile owns, and a draft made from the index.
//!
//! The work is split the way the rules are. A prompt template lives with the
//! profile, because it is the person's instructions and survives a daemon restart;
//! generation runs in a session the person already has open, because that is the
//! agent and the model they chose. The daemon's part is to gather what git says,
//! fill the template, and check the answer before a client offers it.
//!
//! A draft and a commit are separate requests on purpose. The response to
//! `commit-message` is a draft the editor can change, and `commit` is the explicit,
//! confirmed action that gives that message to git. A message that ignores the format
//! is a `400` naming the rule rather than a repaired string, so the person sees what
//! the agent did wrong instead of a commit subject nobody wrote.
//!
//! # Only one generator exists
//!
//! `TODO.md` names a model-lab recipe as the other generator. There are no
//! providers, recipes or model-lab routes in this build yet (P4.5), so the only
//! honest generator is an active session. The request names the session, and the
//! response says `session:<id>`: a client never has to guess which path ran.

use std::path::Path;

use acpilot_core::AcpilotError;
use acpilot_core::commit::{CommitContext, DEFAULT_PROMPT, render_prompt};
use acpilot_git::{DiffScope, GitError, Repo};
use acpilot_proto::ErrorBody;
use acpilot_proto::git::{
    CommitDraft, CommitMessageRequest, CommitPrompt, CommitPromptSource, GitCommit,
    GitCommitRequest,
};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::git::git_error;
use crate::{ApiError, AppState};

/// The file in a profile directory that replaces [`DEFAULT_PROMPT`].
///
/// A file, not a route: the prompt is instructions a person edits, like
/// `agents.toml` and `config.toml`, and a daemon that wrote it would need a save
/// action no ticket has asked for. The file is read per request, so an edit is
/// visible without restarting.
pub const PROMPT_FILE: &str = "commit-prompt.txt";

/// The prompt to fill, and where it came from.
fn prompt_for(state: &AppState) -> Result<(String, CommitPromptSource), ApiError> {
    let Some(root) = state.profile_root() else {
        return Ok((DEFAULT_PROMPT.to_owned(), CommitPromptSource::Default));
    };
    match std::fs::read_to_string(prompt_path(root)) {
        Ok(template) if template.trim().is_empty() => {
            Err(ApiError::new(AcpilotError::invalid(format!(
                "{} is empty; write a prompt or delete the file to use the default",
                prompt_path(root).display()
            )))
            .with_detail("path", prompt_path(root).display().to_string()))
        }
        Ok(template) => Ok((template, CommitPromptSource::Profile)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((DEFAULT_PROMPT.to_owned(), CommitPromptSource::Default))
        }
        Err(error) => Err(ApiError::new(AcpilotError::Io(error))),
    }
}

fn prompt_path(root: &Path) -> std::path::PathBuf {
    root.join(PROMPT_FILE)
}

/// The template for one request: the caller's, the profile's, or the default.
fn template_for(state: &AppState, prompt: Option<String>) -> Result<String, ApiError> {
    match prompt {
        Some(template) if template.trim().is_empty() => {
            Err(ApiError::new(AcpilotError::invalid("`prompt` is empty"))
                .with_detail("prompt", "a template, or leave the field out"))
        }
        Some(template) => Ok(template),
        None => prompt_for(state).map(|(template, _)| template),
    }
}

/// Check an answer and turn it into a draft no client has to assemble.
fn draft_of(answer: &str, generator: String) -> Result<CommitDraft, ApiError> {
    let suggestion = acpilot_core::commit::validate_suggestion(answer).map_err(|error| {
        let message = error.to_string();
        ApiError::new(AcpilotError::invalid(message.clone())).with_detail("message", message)
    })?;
    let message = if suggestion.body.is_empty() {
        suggestion.subject.clone()
    } else {
        format!("{}\n\n{}", suggestion.subject, suggestion.body)
    };
    Ok(CommitDraft {
        message,
        subject: suggestion.subject,
        body: suggestion.body,
        kind: suggestion.kind,
        scope: suggestion.scope,
        breaking: suggestion.breaking,
        emoji: suggestion.emoji,
        generator,
    })
}

/// `GET /v1/git/commit/prompt`.
///
/// The effective template before any placeholders are filled. `source` says whether
/// it is the profile's file or the shipped default, so an editor can say which one it
/// is showing without comparing strings.
///
/// # Errors
///
/// `400` when the profile's file exists and is empty, rather than silently falling
/// back: a file somebody wrote is a decision, and an empty one is a mistake to name.
async fn prompt(State(state): State<AppState>) -> Result<Json<CommitPrompt>, ApiError> {
    let (template, source) = prompt_for(&state)?;
    Ok(Json(CommitPrompt { template, source }))
}

/// `POST /v1/git/commit-message`.
///
/// Renders the staged patch and its files into the prompt, asks the named session,
/// and validates the answer. The generator is the session named in the body; a
/// per-request `prompt` replaces the profile's template for this attempt only.
///
/// # Errors
///
/// `400` for an empty session id or prompt, or an answer that is not a valid
/// Conventional Commit; `404` for an unknown session or no repository; `409` for no
/// workspace, a workspace on another host, a session that is busy, or an empty
/// index; `502` for a missing `git` or an agent that failed to answer.
async fn commit_message(
    State(state): State<AppState>,
    Json(request): Json<CommitMessageRequest>,
) -> Result<Json<CommitDraft>, ApiError> {
    if request.session_id.trim().is_empty() {
        return Err(
            ApiError::new(AcpilotError::invalid("`session_id` is empty"))
                .with_detail("session_id", "the id of an active session"),
        );
    }

    let root = crate::files::workspace_root(&state, request.workspace.as_deref())?;
    let repo = Repo::discover(&root).map_err(git_error)?;
    // One diff, asked once: the patch and the file list are two views of the same
    // index, and asking twice could describe two different moments.
    let staged = repo.diff(DiffScope::Staged).map_err(git_error)?;
    if staged.files.is_empty() && staged.patch.trim().is_empty() {
        return Err(ApiError::new(AcpilotError::conflict(
            "nothing is staged; the message describes what the next commit would contain",
        )));
    }
    let status = repo.status().map_err(git_error)?;
    let recent = repo
        .log(5)
        .map_err(git_error)?
        .into_iter()
        .map(|commit| commit.subject)
        .collect::<Vec<_>>();
    let files = staged
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();

    let template = template_for(&state, request.prompt)?;
    let branch = status.branch.unwrap_or_else(|| "(detached)".to_owned());
    let rendered = render_prompt(
        &template,
        &CommitContext {
            branch: &branch,
            recent_subjects: &recent,
            staged_files: &files,
            diff: &staged.patch,
        },
    );

    let answer = state.sessions().ask(&request.session_id, rendered).await?;
    Ok(Json(draft_of(
        &answer,
        format!("session:{}", request.session_id),
    )?))
}

/// `POST /v1/git/commit`.
///
/// Commits exactly what the index holds, with the message the person confirmed. The
/// route never stages: an unstaged edit is not in the commit, and a caller that wants
/// it there stages it with git first. The staged diff is checked before git runs, so
/// an empty index is a `409` that says why rather than git's own `nothing to commit`
/// on stderr.
///
/// # Errors
///
/// `400` for an empty message; `404` for no repository; `409` for nothing staged, a
/// merge conflict, or a git hook that refused; `502` for a missing `git`.
async fn commit(
    State(state): State<AppState>,
    Json(request): Json<GitCommitRequest>,
) -> Result<Json<GitCommit>, ApiError> {
    let message = request.message.trim_end();
    if message.trim().is_empty() {
        return Err(ApiError::new(AcpilotError::invalid("`message` is empty"))
            .with_detail("message", "a subject line, and a body when there is one"));
    }

    let root = crate::files::workspace_root(&state, request.workspace.as_deref())?;
    let repo = Repo::discover(&root).map_err(git_error)?;
    let staged = repo.diff(DiffScope::Staged).map_err(git_error)?;
    if staged.files.is_empty() && staged.patch.trim().is_empty() {
        return Err(ApiError::new(AcpilotError::conflict(
            "nothing is staged; stage the files the commit should contain",
        ))
        .with_detail(
            "message",
            "stage the files in the source pane first; this route never stages for you",
        ));
    }

    let commit = repo.commit(message).map_err(commit_error)?;
    Ok(Json(GitCommit {
        id: commit.id,
        parents: commit.parents,
        author: commit.author,
        email: commit.email,
        authored_at: commit.authored_at,
        subject: commit.subject,
    }))
}

/// A commit failure in this API's terms.
///
/// A refused commit is the request's conflict, not the host's `500`: a pre-commit
/// hook, an unresolved merge or a missing identity is something the person can fix,
/// and git's own words are more useful than a flattened "git failed".
fn commit_error(error: GitError) -> ApiError {
    match error {
        GitError::EmptyMessage => ApiError::new(AcpilotError::invalid("`message` is empty"))
            .with_detail("message", "a subject line, and a body when there is one"),
        GitError::Command {
            command,
            status,
            stderr,
        } => {
            let reason = stderr.trim();
            ApiError::new(AcpilotError::conflict(format!(
                "git {command} exited {status}: {reason}"
            )))
            .with_detail("message", reason.to_owned())
        }
        other => git_error(other),
    }
}

/// This module's routes, ready to merge into the daemon's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/git/commit/prompt", get(prompt))
        .route("/v1/git/commit-message", post(commit_message))
        .route("/v1/git/commit", post(commit))
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/git/commit/prompt",
    tag = "git",
    summary = "The commit-message prompt this profile would use",
    responses(
        (status = 200, description = "The template, unrendered, and whether it is the profile's or the default", body = CommitPrompt),
        (status = 400, description = "The profile's prompt file is empty", body = ErrorBody)
    )
)]
fn commit_prompt_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/git/commit-message",
    tag = "git",
    summary = "Draft a commit message from the staged diff",
    request_body = CommitMessageRequest,
    responses(
        (status = 200, description = "A validated draft; nothing has been committed", body = CommitDraft),
        (status = 400, description = "An empty session id or prompt, or an answer that is not a Conventional Commit", body = ErrorBody),
        (status = 404, description = "The session is unknown, or the workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "No workspace, a workspace on another host, a busy session, or nothing staged", body = ErrorBody),
        (status = 502, description = "`git` is missing, or the agent failed to answer", body = ErrorBody)
    )
)]
fn commit_message_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    path = "/v1/git/commit",
    tag = "git",
    summary = "Commit the staged index with a confirmed message",
    request_body = GitCommitRequest,
    responses(
        (status = 200, description = "The commit that was made", body = GitCommit),
        (status = 400, description = "The message is empty", body = ErrorBody),
        (status = 404, description = "The workspace is not inside a git repository", body = ErrorBody),
        (status = 409, description = "Nothing is staged, or git refused the commit", body = ErrorBody),
        (status = 502, description = "`git` is missing", body = ErrorBody)
    )
)]
fn commit_doc() {}

/// This module's operations, for the OpenAPI document.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    use utoipa::Path;
    vec![
        (
            "/v1/git/commit/prompt",
            HttpMethod::Get,
            <__path_commit_prompt_doc as Path>::operation(),
        ),
        (
            "/v1/git/commit-message",
            HttpMethod::Post,
            <__path_commit_message_doc as Path>::operation(),
        ),
        (
            "/v1/git/commit",
            HttpMethod::Post,
            <__path_commit_doc as Path>::operation(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draft_puts_a_body_after_one_blank_line() {
        let draft = draft_of(
            "feat(api): ✨ serve a draft\n\nThe session writes it, the person edits it.",
            "session:s1".to_owned(),
        )
        .expect("valid");
        assert_eq!(draft.subject, "feat(api): ✨ serve a draft");
        assert_eq!(
            draft.message,
            "feat(api): ✨ serve a draft\n\nThe session writes it, the person edits it."
        );
        assert_eq!(draft.scope.as_deref(), Some("api"));
        assert_eq!(draft.generator, "session:s1");
    }

    #[test]
    fn an_invalid_answer_is_a_400_naming_the_rule() {
        let error = draft_of("added a route", "session:s1".to_owned())
            .expect_err("there is no type to parse");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(error.details[0].field, "message");
        assert!(
            error.details[0].message.contains("<type>"),
            "the answer says what the format is: {:?}",
            error.details[0].message
        );
    }

    #[test]
    fn a_profile_without_a_prompt_file_uses_the_default() {
        let directory = tempfile::tempdir().expect("a profile directory");
        let state = AppState::new().with_profile_root(Some(directory.path().to_path_buf()));
        let (template, source) = prompt_for(&state).expect("the default");
        assert_eq!(template, DEFAULT_PROMPT);
        assert_eq!(source, CommitPromptSource::Default);
    }

    #[test]
    fn a_profile_s_prompt_file_replaces_the_default_and_is_read_per_request() {
        let directory = tempfile::tempdir().expect("a profile directory");
        let state = AppState::new().with_profile_root(Some(directory.path().to_path_buf()));
        std::fs::write(
            prompt_file(directory.path()),
            "Write one line about {{files}}.\n",
        )
        .expect("the prompt");

        let (template, source) = prompt_for(&state).expect("the profile's prompt");
        assert_eq!(template, "Write one line about {{files}}.\n");
        assert_eq!(source, CommitPromptSource::Profile);
    }

    #[test]
    fn an_empty_prompt_file_is_refused_rather_than_ignored() {
        let directory = tempfile::tempdir().expect("a profile directory");
        let state = AppState::new().with_profile_root(Some(directory.path().to_path_buf()));
        std::fs::write(prompt_file(directory.path()), "  \n").expect("the prompt");

        let error = prompt_for(&state).expect_err("an empty prompt asks for nothing");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(error.details[0].field, "path");
        assert!(
            error.details[0].message.contains(PROMPT_FILE),
            "the refusal names the file: {}",
            error.details[0].message
        );
    }

    #[test]
    fn an_empty_override_names_the_field_instead_of_using_the_default() {
        let error = template_for(&AppState::new(), Some(" \n".to_owned()))
            .expect_err("an empty override is a mistake, not a request for the default");
        assert_eq!(error.details[0].field, "prompt");
    }

    fn prompt_file(root: &Path) -> std::path::PathBuf {
        prompt_path(root)
    }
}
