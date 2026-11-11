//! The precedence matrix and the isolation the profile model promises, driven
//! through the process boundary scripts use (P4-10).
//!
//! One table, one command. Every row builds a fresh home, so a row cannot pass on
//! a layer another row wrote. The assertions are about the layer that won and the
//! text that names it, not about how the cascade is built internally: a test that
//! read `Cascade` directly would agree with itself after a bug in the CLI.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use tempfile::TempDir;

/// One preference that exists in every build.
const KEY: &str = "preferences.tab_width";

struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: TempDir::new().expect("temp dir"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write a file under the home, creating its parents.
    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parents");
        }
        fs::write(&path, text).expect("write");
        path
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_acpilot"));
        command
            .args(args)
            .env("ACPILOT_HOME", self.path())
            .env_remove("ACPILOT_PROFILE")
            .env_remove("ACPILOT_TAB_WIDTH")
            .env_remove("ACPILOT_API_URL")
            .env_remove("ACPILOT_TOKEN")
            .current_dir(self.path());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run acpilot")
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn acpilot")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The `from …` line `config explain` prints, without its indent and prefix.
fn origin(text: &str) -> String {
    text.lines()
        .last()
        .unwrap_or_default()
        .trim()
        .trim_start_matches("from ")
        .to_owned()
}

#[test]
fn the_precedence_matrix_is_a_table() {
    struct Case {
        name: &'static str,
        global: bool,
        profile: bool,
        project: bool,
        environment: Option<&'static str>,
        expected: &'static str,
        value: &'static str,
    }

    // Values differ per layer, so a row that silently read the wrong file would
    // fail on the number as well as on the origin.
    let cases = [
        Case {
            name: "the built-in defaults",
            global: false,
            profile: false,
            project: false,
            environment: None,
            expected: "built-in defaults",
            value: "",
        },
        Case {
            name: "the global config file",
            global: true,
            profile: false,
            project: false,
            environment: None,
            expected: "global",
            value: "2",
        },
        Case {
            name: "the profile file beats the global file",
            global: true,
            profile: true,
            project: false,
            environment: None,
            expected: "profile",
            value: "4",
        },
        Case {
            name: "the project file beats the profile file",
            global: true,
            profile: true,
            project: true,
            environment: None,
            expected: "project",
            value: "6",
        },
        Case {
            name: "the environment beats the project file",
            global: true,
            profile: true,
            project: true,
            environment: Some("8"),
            // An environment value is a string, so it renders quoted.
            expected: "environment",
            value: "\"8\"",
        },
    ];

    for case in cases {
        let home = Home::new();
        if case.global {
            home.write(
                "config.toml",
                "schema_version = 1\n[preferences]\ntab_width = 2\n",
            );
        }
        if case.profile {
            home.write(
                "profiles/work/config.toml",
                "schema_version = 1\n[preferences]\ntab_width = 4\n",
            );
        }
        let project = home.path().join("proj");
        if case.project {
            home.write(
                "proj/.acpilot/config.toml",
                "schema_version = 1\n[preferences]\ntab_width = 6\n",
            );
        } else {
            fs::create_dir_all(&project).expect("project dir");
        }

        let mut args = vec!["config", "explain", KEY];
        let project_text = project.display().to_string();
        args.push(&project_text);
        if case.profile {
            args.push("--profile");
            args.push("work");
        }
        let mut command = home.command(&args);
        if let Some(value) = case.environment {
            command.env("ACPILOT_TAB_WIDTH", value);
        }
        let output = command.output().expect("run acpilot");
        assert!(
            output.status.success(),
            "{}: {}",
            case.name,
            stderr(&output)
        );
        let text = stdout(&output);
        // The origin is a canonical path, and a canonical path differs by
        // platform: macOS resolves /var to /private/var, and Windows may write a
        // verbatim prefix. Compare the tail below the temporary directory, which
        // is what says which layer won.
        let got = origin(&text).replace('\\', "/");
        match case.expected {
            "global" => assert!(
                got.ends_with("/config.toml") && !got.contains("/profiles/"),
                "{}: {text}",
                case.name
            ),
            "profile" => assert!(
                got.ends_with("/profiles/work/config.toml"),
                "{}: {text}",
                case.name
            ),
            "project" => assert!(
                got.ends_with("/proj/.acpilot/config.toml"),
                "{}: {text}",
                case.name
            ),
            "environment" => {
                assert_eq!(got, "ACPILOT_* environment", "{}", case.name);
            }
            other => assert_eq!(got, other, "{}: {text}", case.name),
        }
        if !case.value.is_empty() {
            assert!(
                text.contains(&format!("= {}", case.value)),
                "{}: the winning value is {}: {text}",
                case.name,
                case.value
            );
        }
    }
}

#[test]
fn the_global_profile_is_accepted_before_and_after_the_subcommand() {
    let home = Home::new();
    home.write("config.toml", "schema_version = 1\n");
    home.write(
        "profiles/work/config.toml",
        "schema_version = 1\n[preferences]\ntab_width = 3\n",
    );

    let before = home.run(&["--profile", "work", "config", "explain", KEY]);
    let after = home.run(&["config", "explain", KEY, "--profile", "work"]);
    assert!(before.status.success(), "{}", stderr(&before));
    assert!(after.status.success(), "{}", stderr(&after));
    assert_eq!(
        stdout(&before),
        stdout(&after),
        "one flag, two positions, one answer"
    );
    assert!(stdout(&after).contains("= 3"), "{}", stdout(&after));

    // A name that does not resolve is still exit 3 with the command that fixes
    // it, whichever side of the subcommand it was typed on.
    let missing = home.run(&["--profile", "ghost", "config", "explain", KEY]);
    assert_eq!(missing.status.code(), Some(3), "{}", stderr(&missing));
    assert!(
        stderr(&missing).contains("acpilot profile new ghost"),
        "{}",
        stderr(&missing)
    );
}

#[test]
fn doctor_reports_the_five_roots_and_every_unknown_key() {
    let home = Home::new();
    let created = home.run(&["profile", "new", "work"]);
    assert!(created.status.success(), "{}", stderr(&created));
    home.write(
        "profiles/work/config.toml",
        "schema_version = 1\n[preferences]\ntab_wide = 3\n",
    );

    let path = home.path().display().to_string();
    let output = home.run(&["--profile", "work", "doctor", &path]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("profile    work"), "{text}");
    for root in ["config", "data", "cache", "state", "runtime"] {
        assert!(
            text.lines().any(|line| line.trim_start().starts_with(root)),
            "the {root} root is reported: {text}"
        );
    }
    assert!(text.contains("present"), "{text}");
    assert!(
        text.contains("preferences.tab_wide"),
        "the typed key is named: {text}"
    );
    assert!(
        text.replace('\\', "/")
            .contains("profiles/work/config.toml"),
        "with the file that wrote it: {text}"
    );
}

#[test]
fn two_profiles_resolve_independently_and_at_the_same_time() {
    let home = Home::new();
    home.write("config.toml", "schema_version = 1\n");
    home.write(
        "profiles/work/config.toml",
        "schema_version = 1\n[preferences]\ntab_width = 3\n",
    );
    home.write(
        "profiles/personal/config.toml",
        "schema_version = 1\n[preferences]\ntab_width = 5\n",
    );

    // Spawned before either is waited on, so the two processes run together and a
    // shared environment or working directory would show up as a crossed answer.
    let work = home.spawn(&["--profile", "work", "config", "explain", KEY]);
    let personal = home.spawn(&["--profile", "personal", "config", "explain", KEY]);
    let work = work.wait_with_output().expect("work output");
    let personal = personal.wait_with_output().expect("personal output");
    assert!(work.status.success(), "{}", stderr(&work));
    assert!(personal.status.success(), "{}", stderr(&personal));
    assert!(stdout(&work).contains("= 3"), "{}", stdout(&work));
    assert!(stdout(&personal).contains("= 5"), "{}", stdout(&personal));
    for (output, profile) in [(&work, "work"), (&personal, "personal")] {
        let text = stdout(output).replace('\\', "/");
        assert!(
            text.contains(&format!("profiles/{profile}/config.toml")),
            "{text}"
        );
    }
}

#[test]
fn diff_names_every_differing_key_with_both_origins() {
    let home = Home::new();
    home.write("config.toml", "schema_version = 1\n");
    home.write(
        "profiles/work/config.toml",
        "schema_version = 1\n[preferences]\ntab_width = 3\n",
    );
    home.write(
        "profiles/personal/config.toml",
        "schema_version = 1\n[preferences]\ntab_width = 5\n",
    );

    let output = home.run(&["profile", "diff", "work", "personal"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("work -> personal"), "{text}");
    assert!(text.contains("  preferences.tab_width"), "{text}");
    assert!(text.contains("    work: 3"), "{text}");
    assert!(text.contains("    personal: 5"), "{text}");
    for profile in ["work", "personal"] {
        let path = home
            .path()
            .join("profiles")
            .join(profile)
            .join("config.toml");
        assert!(
            text.contains(&path.display().to_string()),
            "the origin of {profile} is named: {text}"
        );
    }
}
