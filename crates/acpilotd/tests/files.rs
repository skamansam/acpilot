//! The file routes, driven through the router against a real directory.
//!
//! The traversal tests are the point of this file. A path that leaves the
//! workspace is the one failure in this API that a well-meaning client can cause
//! by accident, and every spelling of it — `..`, an absolute path, a symlink — is
//! tried here rather than left to the string comparison's good intentions.

use std::fs;

use acpilotd::{AppState, Workspace};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app(state: AppState) -> axum::Router {
    acpilotd::router_with(state)
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(Body::from(
            body.map_or_else(String::new, |value| value.to_string()),
        ))
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

/// A daemon serving `directory` as the workspace named `app`.
fn serving(directory: &std::path::Path) -> AppState {
    AppState::new().with_workspace(Workspace {
        id: "app".to_owned(),
        root: directory.canonicalize().expect("resolved"),
    })
}

/// A workspace with a couple of files in it.
fn workspace() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temp dir");
    fs::create_dir(directory.path().join("src")).expect("src");
    fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").expect("main");
    fs::write(directory.path().join("README.md"), "# app\n").expect("readme");
    directory
}

#[tokio::test]
async fn a_gitignore_keeps_its_paths_out_of_the_tree() {
    // A workspace with the three things a tree has to get right: an ignored
    // directory, an ignored file, and `.git` itself.
    let directory = tempfile::tempdir().expect("a workspace");
    fs::create_dir(directory.path().join("target")).expect("target");
    fs::write(directory.path().join("target/app.bin"), "built").expect("target file");
    fs::create_dir(directory.path().join("src")).expect("src");
    fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").expect("main");
    fs::write(directory.path().join("notes.log"), "noise\n").expect("log");
    fs::create_dir(directory.path().join(".git")).expect(".git");
    fs::write(directory.path().join(".gitignore"), "target/\n*.log\n").expect("ignore");

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Vec<&str> = body
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect();
    assert_eq!(
        listed,
        vec![".gitignore", "src"],
        "an ignored directory, an ignored file and .git are not rows: {body}"
    );
}

/// Run `git` in `directory`, with an identity in the environment so a machine with
/// no global one still commits.
fn git(directory: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git is installed");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The `(path, status)` pairs a listing returned, for asserting on the letters.
fn letters(body: &serde_json::Value) -> Vec<(String, Option<String>)> {
    body.as_array()
        .expect("a list")
        .iter()
        .map(|entry| {
            (
                entry["path"].as_str().expect("a path").to_owned(),
                entry["status"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_row_says_what_git_says_changed() {
    let directory = tempfile::tempdir().expect("a workspace");
    fs::create_dir(directory.path().join("src")).expect("src");
    fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").expect("main");
    fs::write(directory.path().join("README.md"), "# app\n").expect("readme");
    git(directory.path(), &["init", "-q"]);
    git(directory.path(), &["add", "-A"]);
    git(directory.path(), &["commit", "-q", "-m", "start"]);

    // One of each state a listing can show: an edit to a tracked file, a staged new
    // one, a file git has never seen, and a tracked file that is gone.
    fs::write(directory.path().join("src/main.rs"), "fn main() { }\n").expect("edit");
    fs::write(directory.path().join("src/new.rs"), "pub fn new() {}\n").expect("new");
    git(directory.path(), &["add", "src/new.rs"]);
    fs::write(directory.path().join("notes.txt"), "scratch\n").expect("notes");
    fs::remove_file(directory.path().join("README.md")).expect("delete");

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        letters(&body),
        vec![
            ("notes.txt".to_owned(), Some("untracked".to_owned())),
            // One row for the directory, saying the most urgent thing inside it.
            ("src".to_owned(), Some("added".to_owned())),
        ],
        "a deleted file has no row, because a listing is the filesystem: {body}"
    );

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list?path=src",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        letters(&body),
        vec![
            ("src/main.rs".to_owned(), Some("modified".to_owned())),
            ("src/new.rs".to_owned(), Some("added".to_owned())),
        ],
        "{body}"
    );
}

#[tokio::test]
async fn a_read_says_which_lines_changed() {
    let directory = tempfile::tempdir().expect("a workspace");
    fs::create_dir(directory.path().join("src")).expect("src");
    fs::write(
        directory.path().join("src/main.rs"),
        "fn main() {\n    println!(\"one\");\n}\n",
    )
    .expect("main");
    git(directory.path(), &["init", "-q"]);
    git(directory.path(), &["add", "-A"]);
    git(directory.path(), &["commit", "-q", "-m", "start"]);
    fs::write(
        directory.path().join("src/main.rs"),
        "fn main() {\n    println!(\"two\");\n}\n",
    )
    .expect("edit");

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=src/main.rs",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["marks"],
        serde_json::json!([{"line": 2, "change": "modified"}]),
        "the gutter is the daemon's git, in the text's own line numbers: {body}"
    );

    // A file nobody changed has nothing to mark, and the field is absent rather than
    // an empty list per read.
    git(directory.path(), &["checkout", "--", "src/main.rs"]);
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=src/main.rs",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("marks").is_none(), "an unchanged file: {body}");
}

#[tokio::test]
async fn a_workspace_that_is_not_a_repository_has_no_letters() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body.as_array().expect("a list");
    assert!(
        !rows.is_empty(),
        "the tree still lists what is there: {body}"
    );
    assert!(
        rows.iter().all(|entry| entry.get("status").is_none()),
        "no repository is no letters, not a listing that fails: {body}"
    );
}

#[tokio::test]
async fn asking_for_hidden_rows_marks_them_instead_of_dropping_them() {
    let directory = tempfile::tempdir().expect("a workspace");
    fs::create_dir(directory.path().join("target")).expect("target");
    fs::create_dir(directory.path().join("src")).expect("src");
    fs::write(directory.path().join("notes.log"), "noise\n").expect("log");
    fs::create_dir(directory.path().join(".git")).expect(".git");
    fs::write(directory.path().join(".gitignore"), "target/\n*.log\n").expect("ignore");

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list?include_ignored=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body.as_array().expect("a list");
    let paths: Vec<&str> = rows
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect();
    assert_eq!(
        paths,
        vec![".gitignore", "notes.log", "src", "target"],
        "the ignored rows are back: {body}"
    );
    assert_eq!(rows[1]["ignored"], true, "a hidden file says so: {body}");
    assert_eq!(rows[3]["ignored"], true, "and so does a hidden directory");
    assert!(
        rows[2]["ignored"].is_null(),
        "an ordinary row carries no flag at all: {body}"
    );
    assert!(
        !paths.contains(&".git"),
        "the repository's own bookkeeping is still not a row: {body}"
    );
}

#[tokio::test]
async fn a_search_walks_the_workspace_and_follows_the_same_ignore_rules() {
    let directory = tempfile::tempdir().expect("a workspace");
    fs::create_dir_all(directory.path().join("src/deep")).expect("src");
    fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").expect("main");
    fs::write(directory.path().join("src/deep/main_helper.rs"), "").expect("helper");
    fs::write(directory.path().join("notes.log"), "noise\n").expect("log");
    fs::write(directory.path().join(".env"), "TOKEN=1\n").expect("env");
    fs::create_dir(directory.path().join(".git")).expect(".git");
    fs::write(directory.path().join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
    fs::write(directory.path().join(".gitignore"), "*.log\n").expect("ignore");

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/search?q=main",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let paths: Vec<&str> = body
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["src/main.rs", "src/deep/main_helper.rs"],
        "a search walks the whole workspace, and the nearer path ranks first: {body}"
    );

    // An ignored path is not findable, and a hidden file that is *not* ignored is:
    // the rule is the preference, not the leading dot.
    let (_, ignored) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/search?q=log",
        None,
    )
    .await;
    assert_eq!(ignored.as_array().map(Vec::len), Some(0), "{ignored}");
    let (_, hidden) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/search?q=.env",
        None,
    )
    .await;
    assert_eq!(hidden[0]["path"], ".env", "{hidden}");

    // A repository's own directory is not walked: no `HEAD`, no `objects`.
    let (_, git) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/search?q=HEAD",
        None,
    )
    .await;
    assert_eq!(git.as_array().map(Vec::len), Some(0), "{git}");

    // Nothing typed matches nothing rather than walking the repository.
    let (status, empty) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/search?q=",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty.as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn a_path_search_ranks_the_better_match_first() {
    let directory = tempfile::tempdir().expect("a workspace");
    fs::create_dir_all(directory.path().join("src")).expect("src");
    fs::create_dir_all(directory.path().join("crates/acpilot-tui/src")).expect("crates");
    fs::create_dir_all(directory.path().join("examples/old/src")).expect("examples");
    for path in [
        "src/app.rs",
        "crates/acpilot-tui/src/app.rs",
        "examples/old/src/app.rs",
    ] {
        fs::write(directory.path().join(path), "fn main() {}\n").expect(path);
    }

    // Initials: `aptui` is the letters of `acpilot-tui`, in order, and no substring
    // of any path matches it.
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/search?q=aptui",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body[0]["path"], "crates/acpilot-tui/src/app.rs",
        "the initials match ranks first: {body}"
    );

    // The same letters in three files: the shorter path is the one meant.
    let (_, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/search?q=app.rs",
        None,
    )
    .await;
    assert_eq!(body[0]["path"], "src/app.rs", "{body}");

    // A letter that is not in the path is not a match: fuzzy is not an anagram.
    let (_, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/search?q=zapp",
        None,
    )
    .await;
    assert_eq!(body.as_array().map(Vec::len), Some(0), "{body}");
}

#[tokio::test]
async fn a_content_search_finds_lines_and_skips_binaries() {
    let directory = tempfile::tempdir().expect("a workspace");
    fs::create_dir(directory.path().join("src")).expect("src");
    fs::write(
        directory.path().join("src/main.rs"),
        "fn main() {\n    let greeting = \"hello\";\n    println!(\"{greeting}\");\n}\n",
    )
    .expect("main");
    fs::write(directory.path().join("notes.txt"), "hello there\n").expect("notes");
    fs::write(directory.path().join("blob.txt"), [0x00, b'h', b'i', 0x01]).expect("blob");

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/grep?q=greeting",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let matches = body.as_array().expect("a list");
    assert_eq!(matches.len(), 2, "{body}");
    assert_eq!(matches[0]["path"], "src/main.rs");
    assert_eq!(matches[0]["line"], 2, "a line is counted from one: {body}");
    assert_eq!(matches[0]["text"], "    let greeting = \"hello\";");
    assert_eq!(matches[1]["line"], 3);

    // Case is ignored, a binary file is skipped rather than decoded, and the
    // binary's bytes are not a line of text either.
    let (_, caseless) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/grep?q=HELLO",
        None,
    )
    .await;
    let paths: Vec<&str> = caseless
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|hit| hit["path"].as_str())
        .collect();
    assert_eq!(paths, vec!["notes.txt", "src/main.rs"], "{caseless}");

    // Nothing typed matches nothing rather than reading every file.
    let (status, empty) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/grep?q=",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty.as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn a_content_search_with_no_workspace_is_a_conflict() {
    let (status, body) = send(app(AppState::new()), "GET", "/v1/files/grep?q=main", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn a_search_with_no_workspace_is_a_conflict() {
    let (status, body) = send(app(AppState::new()), "GET", "/v1/files/search?q=main", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn a_read_returns_the_text_and_a_token() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?workspace=app&path=src/main.rs",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["path"], "src/main.rs");
    assert_eq!(body["text"], "fn main() {}\n");
    let etag = body["etag"].as_str().expect("an etag").to_owned();

    // The same bytes and the same file must produce the same token, or every
    // save would look like a conflict.
    let (_, again) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?workspace=app&path=src/main.rs",
        None,
    )
    .await;
    assert_eq!(again["etag"], etag);
}

#[tokio::test]
async fn a_directory_is_a_conflict_that_names_the_list_route() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=src",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("/v1/files/list")),
        "{body}"
    );
}

#[tokio::test]
async fn a_path_that_climbs_out_is_a_400_before_it_is_resolved() {
    let directory = workspace();
    for path in ["../secret.env", "src/../../secret.env", "src/./../../x"] {
        let (status, body) = send(
            app(serving(directory.path())),
            "GET",
            &format!("/v1/files?path={path}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["details"][0]["field"], "path", "{path}: {body}");
    }
}

#[tokio::test]
async fn an_absolute_path_is_refused() {
    let directory = workspace();
    // Absolute on every platform, and outside the workspace: `/etc/passwd` is not
    // an absolute path on Windows, where the route refuses a rooted-but-relative
    // path for a different reason and the assertion below would be about that.
    let absolute = std::env::temp_dir().join("acpilot-absolute-path-probe.txt");
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        &format!("/v1/files?path={}", absolute.display()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("absolute")),
        "{absolute:?} is refused for being absolute: {body}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_workspace_is_a_403() {
    // The case a string prefix check cannot catch: the path stays inside, the
    // file does not.
    let directory = workspace();
    let outside = tempfile::tempdir().expect("outside");
    fs::write(outside.path().join("secret.env"), "TOKEN=1\n").expect("secret");
    std::os::unix::fs::symlink(
        outside.path().join("secret.env"),
        directory.path().join("link"),
    )
    .expect("symlink");

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=link",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn binary_content_is_refused_rather_than_mangled() {
    let directory = workspace();
    fs::write(directory.path().join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).expect("blob");
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=blob.bin",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("UTF-8")),
        "{body}"
    );
}

#[tokio::test]
async fn a_file_over_the_cap_is_refused() {
    let directory = workspace();
    let big = vec![b'a'; usize::try_from(acpilotd::MAX_FILE_BYTES).expect("fits") + 1];
    fs::write(directory.path().join("big.txt"), &big).expect("big");
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?path=big.txt",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_listing_is_sorted_and_uses_forward_slashes() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list?path=",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body.as_array().expect("an array");
    let paths: Vec<&str> = entries
        .iter()
        .map(|entry| entry["path"].as_str().expect("a path"))
        .collect();
    assert_eq!(paths, ["README.md", "src"]);
    assert_eq!(entries[1]["is_dir"], true);
    assert_eq!(entries[1]["size"], 0);
    assert_eq!(entries[0]["is_dir"], false);

    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list?path=src",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["path"], "src/main.rs");
}

#[tokio::test]
async fn a_file_is_not_a_listing() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files/list?path=README.md",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn a_write_needs_no_token_to_create_and_one_to_replace() {
    let directory = workspace();
    let state = serving(directory.path());

    let (status, created) = send(
        app(state.clone()),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({ "path": "notes.txt", "text": "first\n" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(created["etag"].as_str().is_some());
    assert_eq!(
        fs::read_to_string(directory.path().join("notes.txt")).expect("written"),
        "first\n"
    );

    // The same body again: creating over a file that exists is a conflict, not a
    // silent replace.
    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({ "path": "notes.txt", "text": "second\n" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let etag = created["etag"].as_str().expect("etag").to_owned();
    let (status, replaced) = send(
        app(state.clone()),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({
            "path": "notes.txt",
            "text": "second\n",
            "if_match": etag
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_ne!(replaced["etag"], etag);
}

#[tokio::test]
async fn a_stale_token_is_a_conflict_and_a_missing_file_is_a_404() {
    let directory = workspace();
    let state = serving(directory.path());
    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({
            "path": "README.md",
            "text": "clobbered\n",
            "if_match": "0-0-nothing"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        fs::read_to_string(directory.path().join("README.md")).expect("read"),
        "# app\n",
        "the file must not have been touched"
    );

    let (status, _) = send(
        app(state),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({
            "path": "gone.txt",
            "text": "x",
            "if_match": "0-0-nothing"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_write_into_a_missing_directory_is_a_404_not_a_mkdir() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({ "path": "new/deep/file.txt", "text": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!directory.path().join("new").exists());
}

#[tokio::test]
async fn a_write_through_a_path_that_escapes_is_refused() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "PUT",
        "/v1/files",
        Some(serde_json::json!({ "path": "../escape.txt", "text": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(!directory.path().join("..").join("escape.txt").exists());
}

#[tokio::test]
async fn an_unknown_workspace_is_a_404_and_no_workspace_is_a_conflict() {
    let directory = workspace();
    let (status, body) = send(
        app(serving(directory.path())),
        "GET",
        "/v1/files?workspace=other&path=README.md",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, _) = send(
        app(AppState::new()),
        "GET",
        "/v1/files?path=README.md",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_contract_lists_the_file_routes() {
    let (status, body) = send(app(AppState::new()), "GET", "/v1/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["paths"]["/v1/files"]["get"].is_object());
    assert!(body["paths"]["/v1/files"]["put"].is_object());
    assert!(body["paths"]["/v1/files/list"]["get"].is_object());
    assert!(body["paths"]["/v1/files/search"]["get"].is_object());
}
