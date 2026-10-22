//! The platform boundary is enforced, not remembered.
//!
//! Unix and macOS are built first and Windows last, which means a Windows branch
//! written in a hurry has nowhere to be caught except a CI runner. So the rule is
//! checked here, in the workspace's own test run and on every OS in CI's matrix:
//!
//! **A `cfg` that names an operating system may appear only under `src/platform/`.**
//!
//! One file per platform, same signature, and the compiler then has to agree that
//! both sides exist. A `cfg(windows)` in the middle of the host layer would
//! compile on Windows and silently never run anywhere else — exactly the kind of
//! code that is discovered months later.
//!
//! Comments are exempt: prose is allowed to say "on Windows", and this crate's
//! own documentation explains the rule using the very words it forbids.

use std::fs;
use std::path::{Path, PathBuf};

/// The tokens that make a `cfg` a platform branch. `target_os` and friends are
/// included because `cfg(target_os = "windows")` is the same decision spelled
/// differently, and a rule that only catches one spelling is decoration.
const PLATFORMS: [&str; 5] = [
    "windows",
    "unix",
    "target_os",
    "target_family",
    "target_env",
];

fn rust_sources(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

/// `Some(token)` when this line is a platform branch in code.
fn branch_token(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") {
        return None;
    }
    // `cfg(` covers attributes and macros alike (`cfg!` is a separate spelling).
    let start = line.find("cfg(").or_else(|| line.find("cfg!("))?;
    let condition = &line[start..];
    PLATFORMS
        .into_iter()
        .find(|platform| condition.contains(platform))
}

#[test]
fn platform_branches_live_only_under_src_platform() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let crates = root.join("crates");
    let mut violations = Vec::new();

    for entry in fs::read_dir(&crates)
        .expect("crates/ is readable")
        .flatten()
    {
        let source = entry.path().join("src");
        let mut files = Vec::new();
        rust_sources(&source, &mut files);
        for file in files {
            let relative = file.strip_prefix(&source).expect("walked from src");
            if relative
                .components()
                .next()
                .is_some_and(|first| first.as_os_str() == "platform")
            {
                continue;
            }
            let text = fs::read_to_string(&file).expect("source is UTF-8");
            for (index, line) in text.lines().enumerate() {
                if let Some(token) = branch_token(line) {
                    violations.push(format!(
                        "{}:{}: {token} — {}",
                        file.display(),
                        index + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "platform branches belong in src/platform/, one file per platform:\n{}",
        violations.join("\n")
    );
}

#[test]
fn every_platform_has_a_module_and_the_seam_names_both() {
    let platform = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/platform");
    let seam = fs::read_to_string(platform.join("mod.rs")).expect("src/platform/mod.rs exists");

    for module in ["unix", "windows"] {
        assert!(
            platform.join(format!("{module}.rs")).is_file(),
            "src/platform/{module}.rs is missing: a platform without a file is a \
             platform whose seams compile by accident"
        );
        assert!(
            seam.contains(&format!("mod {module};")),
            "src/platform/mod.rs does not declare `mod {module};`"
        );
    }

    // The shared handle must not branch either: if it did, the branch would be
    // outside the one directory the rule allows.
    let handle = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pty.rs"))
        .expect("src/pty.rs exists");
    assert!(
        handle
            .lines()
            .all(|line| branch_token(line).is_none() || line.trim_start().starts_with("//")),
        "src/pty.rs contains a platform branch; it belongs in src/platform/"
    );
}
