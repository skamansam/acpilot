//! A [`Session`] against a real server, when one is installed.
//!
//! Ignored by default: it needs `rust-analyzer` on `PATH`.
//!
//! ```sh
//! cargo test -p acpilot-lsp --test session -- --ignored
//! ```

use std::fs;

use acpilot_lsp::Position;
use acpilot_lsp::session::Session;

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
