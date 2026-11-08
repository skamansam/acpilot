//! The git routes, driven through the router against a real repository.
//!
//! The fixture is temporary and lives only for the test: `git init`, two commits, an
//! unstaged edit, a staged edit and an untracked file. The assertions are about what
//! the routes pass through, because the parsing is `acpilot-git`'s and is tested
//! there against the same porcelain output.

use std::fs;
use std::path::Path;
use std::process::Command;

use acpilotd::{AppState, Workspace};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Run `git` in `root` with the variables a hook exports removed.
///
/// The same reason `acpilot-git` has one: a suite run from a `pre-commit` hook
/// inherits `GIT_DIR`, and `-C` would then be naming a repository nobody asked about.
fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    for key in [
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_QUARANTINE_PATH",
        "GIT_NAMESPACE",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
    ] {
        command.env_remove(key);
    }
    let output = command.output().expect("git is on PATH");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository with a commit, an unstaged edit, a staged edit and an untracked file.
///
/// `limit` in a log test means a second commit; this one is the base every test
/// starts from.
fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("a temp dir");
    let root = directory.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test User"]);
    fs::write(root.join("one.txt"), "one\n").expect("one");
    fs::write(root.join("two.txt"), "two\n").expect("two");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "first"]);

    // Unstaged: the working tree differs from the index.
    fs::write(root.join("one.txt"), "one changed\n").expect("one");
    // Staged: the index differs from HEAD.
    fs::write(root.join("two.txt"), "two changed\n").expect("two");
    git(root, &["add", "two.txt"]);
    // Untracked: in neither.
    fs::write(root.join("three.txt"), "three\n").expect("three");
    directory
}

fn app(state: AppState) -> axum::Router {
    acpilotd::router_with(state)
}

/// A daemon serving `directory` as the workspace named `app`.
fn serving(directory: &Path) -> AppState {
    AppState::new().with_workspace(Workspace {
        id: "app".to_owned(),
        root: directory.canonicalize().expect("resolved"),
    })
}

async fn send(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("router is infallible");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn post(
    app: axum::Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = app.oneshot(request).await.expect("router is infallible");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// The paths `git diff --cached --name-only` reports.
fn staged_paths(root: &Path) -> Vec<String> {
    git(root, &["diff", "--cached", "--name-only"])
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One status row by path.
fn row<'a>(body: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    body["files"]
        .as_array()
        .expect("a file list")
        .iter()
        .find(|row| row["path"] == path)
        .unwrap_or_else(|| panic!("{path} is in the answer: {body}"))
}

#[tokio::test]
async fn status_reports_branch_and_both_halves_of_a_change() {
    let directory = repository();
    let (status, body) = send(app(serving(directory.path())), "/v1/git/status").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["branch"], "main");
    assert!(
        body["head"].as_str().is_some_and(|head| head.len() == 40),
        "the commit status is about is named: {body}"
    );

    let one = row(&body, "one.txt");
    assert_eq!(one["index"], "unmodified");
    assert_eq!(
        one["worktree"], "modified",
        "an edit that is not staged is the working tree's"
    );

    let two = row(&body, "two.txt");
    assert_eq!(two["index"], "modified");
    assert_eq!(
        two["worktree"], "unmodified",
        "a staged edit is in the index and not in the working tree"
    );

    let three = row(&body, "three.txt");
    assert_eq!(three["worktree"], "untracked");
}

#[tokio::test]
async fn the_diff_answers_the_scope_it_was_asked_for() {
    let directory = repository();
    let state = serving(directory.path());

    let (status, staged) = send(app(state.clone()), "/v1/git/diff?scope=staged").await;
    assert_eq!(status, StatusCode::OK, "{staged}");
    let staged_files: Vec<&str> = staged["files"]
        .as_array()
        .expect("a file list")
        .iter()
        .filter_map(|file| file["path"].as_str())
        .collect();
    assert_eq!(
        staged_files,
        ["two.txt"],
        "the staged diff is the index against HEAD: {staged}"
    );
    assert!(
        staged["patch"]
            .as_str()
            .is_some_and(|patch| patch.contains("two changed")),
        "the patch is the text a pane draws: {staged}"
    );

    let (status, all) = send(app(state.clone()), "/v1/git/diff?scope=all").await;
    assert_eq!(status, StatusCode::OK, "{all}");
    let all_files: Vec<&str> = all["files"]
        .as_array()
        .expect("a file list")
        .iter()
        .filter_map(|file| file["path"].as_str())
        .collect();
    assert_eq!(
        all_files,
        ["one.txt", "two.txt"],
        "all is the working tree against HEAD, and an untracked file has no diff: {all}"
    );

    let (status, default) = send(app(state), "/v1/git/diff").await;
    assert_eq!(status, StatusCode::OK, "{default}");
    let default_files: Vec<&str> = default["files"]
        .as_array()
        .expect("a file list")
        .iter()
        .filter_map(|file| file["path"].as_str())
        .collect();
    assert_eq!(
        default_files,
        ["one.txt"],
        "no scope is the working tree against the index: {default}"
    );
}

#[tokio::test]
async fn the_three_diff_scopes_are_snapshotted() {
    let directory = repository();
    let state = serving(directory.path());
    let mut scopes = serde_json::Map::new();
    for scope in ["unstaged", "staged", "all"] {
        let (status, body) = send(app(state.clone()), &format!("/v1/git/diff?scope={scope}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        scopes.insert(scope.to_owned(), body);
    }

    // The patch is the exact text three clients draw. A changed line, a hunk
    // header or a file list that moves shows up here as a reviewable diff rather
    // than as a screenshot somebody has to compare by eye.
    insta::assert_json_snapshot!("diff_scopes", serde_json::Value::Object(scopes));
}

#[tokio::test]
async fn stage_puts_an_edit_and_an_untracked_file_in_the_index() {
    let directory = repository();
    let state = serving(directory.path());
    let root = directory.path();

    let (status, body) = post(
        app(state),
        "/v1/git/stage",
        serde_json::json!({ "paths": ["one.txt", "three.txt"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(row(&body, "one.txt")["index"], "modified");
    assert_eq!(row(&body, "one.txt")["worktree"], "unmodified");
    assert_eq!(row(&body, "three.txt")["index"], "added");
    assert_eq!(row(&body, "three.txt")["worktree"], "unmodified");
    let mut wrote = staged_paths(root);
    wrote.sort();
    assert_eq!(
        wrote,
        ["one.txt", "three.txt", "two.txt"],
        "the response is the index git actually wrote, including the change already staged"
    );
}

#[tokio::test]
async fn unstage_removes_a_path_from_the_index_and_keeps_its_edit() {
    let directory = repository();
    let state = serving(directory.path());
    let root = directory.path();

    let (status, body) = post(
        app(state),
        "/v1/git/unstage",
        serde_json::json!({ "paths": ["two.txt"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(row(&body, "two.txt")["index"], "unmodified");
    assert_eq!(row(&body, "two.txt")["worktree"], "modified");
    assert!(
        staged_paths(root).is_empty(),
        "nothing remains in the next commit"
    );
    assert_eq!(
        fs::read_to_string(root.join("two.txt")).expect("two"),
        "two changed\n",
        "unstaging is not a checkout"
    );
}

#[tokio::test]
async fn stage_refuses_a_path_outside_the_repository() {
    let directory = repository();
    let (status, body) = post(
        app(serving(directory.path())),
        "/v1/git/stage",
        serde_json::json!({ "paths": ["../outside.txt"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    assert_eq!(body["details"][0]["field"], "paths");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("../outside.txt")),
        "the refusal names the path: {body}"
    );
}

#[tokio::test]
async fn an_empty_path_list_is_refused_rather_than_staging_everything() {
    let directory = repository();
    let (status, body) = post(
        app(serving(directory.path())),
        "/v1/git/stage",
        serde_json::json!({ "paths": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["details"][0]["field"], "paths");
    assert!(
        staged_paths(directory.path()) == ["two.txt"],
        "the fixture's staged edit is untouched"
    );
}

#[tokio::test]
async fn an_unknown_scope_is_refused_with_the_ones_it_accepts() {
    let directory = repository();
    let (status, body) = send(
        app(serving(directory.path())),
        "/v1/git/diff?scope=everything",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    assert_eq!(body["details"][0]["field"], "scope");
}

#[tokio::test]
async fn the_tree_lists_tracked_and_untracked_paths() {
    let directory = repository();
    let (status, body) = send(app(serving(directory.path())), "/v1/git/tree").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body.as_array().expect("a list");
    let paths: Vec<&str> = rows.iter().filter_map(|row| row["path"].as_str()).collect();
    assert_eq!(
        paths,
        ["one.txt", "three.txt", "two.txt"],
        "the working tree, sorted, with no .git directory: {body}"
    );
    assert_eq!(rows[0]["tracked"], true);
    assert_eq!(
        rows[1]["tracked"], false,
        "an untracked file is listed and says it is not in the index"
    );
}

#[tokio::test]
async fn log_answers_the_limit_and_keeps_parents() {
    let directory = repository();
    let root = directory.path();
    fs::write(root.join("one.txt"), "one again\n").expect("one");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "second"]);

    let state = serving(root);
    let (status, body) = send(app(state.clone()), "/v1/git/log?limit=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let commits = body.as_array().expect("a list");
    assert_eq!(commits.len(), 1, "the limit is the limit: {body}");
    assert_eq!(commits[0]["subject"], "second");
    assert_eq!(
        commits[0]["parents"].as_array().map(Vec::len),
        Some(1),
        "a commit names the parent a graph draws its edge to: {body}"
    );
    assert_eq!(commits[0]["author"], "Test User");

    let (status, empty) = send(app(state), "/v1/git/log?limit=0").await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(
        empty,
        serde_json::json!([]),
        "zero is an empty history, not an error"
    );
}

#[tokio::test]
async fn a_directory_that_is_not_a_repository_is_not_found() {
    let directory = tempfile::tempdir().expect("a dir");
    fs::write(directory.path().join("README.md"), "# not a repo\n").expect("readme");
    let (status, body) = send(app(serving(directory.path())), "/v1/git/status").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "not_found");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("repository")),
        "the answer has to say what was missing: {body}"
    );
}

#[tokio::test]
async fn a_workspace_on_another_host_is_refused_rather_than_answered_locally() {
    let directory = repository();
    let state = serving(directory.path());
    let row = state
        .workspaces()
        .open("ssh:box", "/home/box/project", None);
    let (status, body) = send(app(state), &format!("/v1/git/status?workspace={}", row.id)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("ssh:box")),
        "a refusal names the host whose answer this would not be: {body}"
    );
}

#[tokio::test]
async fn a_daemon_with_no_workspace_has_no_repository_to_describe() {
    let (status, body) = send(app(AppState::new()), "/v1/git/status").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict");
}

#[tokio::test]
async fn the_openapi_document_documents_the_git_routes() {
    let request = Request::builder()
        .method("GET")
        .uri("/v1/openapi.json")
        .body(Body::empty())
        .expect("request");
    let response = app(AppState::new())
        .oneshot(request)
        .await
        .expect("router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");

    for path in [
        "/v1/git/status",
        "/v1/git/diff",
        "/v1/git/tree",
        "/v1/git/log",
    ] {
        assert!(
            document["paths"][path]["get"].is_object(),
            "{path} is in the document"
        );
    }
    for path in ["/v1/git/stage", "/v1/git/unstage"] {
        assert!(
            document["paths"][path]["post"].is_object(),
            "{path} is in the document"
        );
    }
    for schema in [
        "GitStatus",
        "GitDiff",
        "GitTreeEntry",
        "GitCommit",
        "GitIndexRequest",
    ] {
        assert!(
            document["components"]["schemas"][schema].is_object(),
            "{schema} is in the components"
        );
    }
}
