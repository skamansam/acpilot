//! A [`Session`] against a real server, when one is installed.
//!
//! Ignored by default: it needs `rust-analyzer` on `PATH`.
//!
//! ```sh
//! cargo test -p acpilot-lsp --test session -- --ignored --test-threads=1
//! ```
//!
//! Serial because two servers indexing two fixtures at once is what makes the
//! references test flaky: it gives up after five seconds, and that is a budget a second
//! cold index can spend. The tests are independent — the fixtures are separate
//! directories — so the flag is about the machine, not about shared state.

use std::fs;

use acpilot_lsp::Position;
use acpilot_lsp::session::{Published, Session};

fn crate_with_two_modules() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("a directory");
    fs::create_dir(directory.path().join("src")).expect("src");
    fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(
        directory.path().join("src/lib.rs"),
        "pub mod a { pub fn dup() {} pub fn call() { dup(); } }\n\
         pub mod b { pub fn dup() {} pub fn call() { dup(); } }\n",
    )
    .expect("lib");
    directory
}

#[tokio::test]
#[ignore = "needs rust-analyzer on PATH"]
async fn the_session_opens_a_file_and_answers_about_its_name() {
    let directory = crate_with_two_modules();
    let root = directory.path().canonicalize().expect("resolved");
    let file = root.join("src/lib.rs");
    let text = fs::read_to_string(&file).expect("lib");

    let mut session = Session::start("rust-analyzer", &[], &root)
        .await
        .expect("the server starts");
    session
        .open(&file, "rust", &text)
        .await
        .expect("the document opens");

    // The call in module `a`, which is line 1 and character 44 in the fixture.
    let index = acpilot_lsp::LineIndex::new(&text);
    let offset = text.find("dup();").expect("a call");
    let position = index
        .position(&text, offset, session.encoding())
        .expect("a position");
    assert_eq!(position, Position::new(0, 44), "{position:?}");

    // A cold server answers empty until it has indexed the file.
    let mut found = Vec::new();
    for _ in 0..20 {
        found = session
            .references(&file, position)
            .await
            .expect("the server answers");
        if !found.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    session.shutdown().await;

    let lines: Vec<u32> = found.iter().map(|location| location.line).collect();
    assert!(
        found.len() >= 2,
        "a definition and a call, at least: {found:?}"
    );
    assert!(
        lines.iter().all(|line| *line == 0),
        "module `b` has its own `dup` and must not be listed: {found:?}"
    );
    assert!(
        found.iter().all(|location| location.path == file),
        "one file in the fixture: {found:?}"
    );
}

/// Wait for a publish about `file` that `done` accepts, polling the queue the way the
/// daemon's timer does.
///
/// Twenty seconds because a cold server pays for an index first and a warm one
/// debounces: the wait is the server's business, and what this asserts is that nobody
/// has to ask for the answer to exist.
async fn drain_until(
    session: &mut Session,
    file: &std::path::Path,
    mut done: impl FnMut(&Published) -> bool,
) -> Published {
    let mut seen = Vec::new();
    for _ in 0..80 {
        for published in session.drain_published() {
            if published.path != file {
                continue;
            }
            if done(&published) {
                return published;
            }
            seen.push(published.problems.len());
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("no publish this test would accept arrived; problem counts seen: {seen:?}");
}

#[tokio::test]
#[ignore = "needs rust-analyzer on PATH"]
async fn a_change_nobody_asks_about_still_reaches_the_queue() {
    let directory = crate_with_two_modules();
    let root = directory.path().canonicalize().expect("resolved");
    let file = root.join("src/lib.rs");

    let mut session = Session::start("rust-analyzer", &[], &root)
        .await
        .expect("the server starts");
    // A syntax error rather than a type error: rust-analyzer reports what its own parse
    // found, while a type mismatch comes from a `cargo check` it may not have run at all.
    // The publish this test is about is the parse's.
    let broken = "pub fn broken( {\n";
    session
        .open(&file, "rust", broken)
        .await
        .expect("the document opens");

    // Nothing asks: this is only the queue being drained, which is what the daemon's
    // timer does between requests. A file that does not compile must say so.
    let first = drain_until(&mut session, &file, |published| {
        !published.problems.is_empty()
    })
    .await;
    assert!(
        first
            .problems
            .iter()
            .any(|problem| problem.severity == acpilot_lsp::session::Severity::Error),
        "a type mistake is an error: {:?}",
        first.problems
    );

    // The fix arrives as a `didChange`, which is what an agent edit is: no request and
    // no save, and the server is expected to publish again on its own.
    let fixed = "pub fn fixed() {}\n";
    session
        .open(&file, "rust", fixed)
        .await
        .expect("the document changes");
    let second = drain_until(&mut session, &file, |published| {
        published.problems.is_empty()
    })
    .await;
    assert_eq!(
        second.path, file,
        "the publish names the document it is about"
    );
    session.shutdown().await;
}
