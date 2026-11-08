//! `acpilot doctor`: what this machine has, and what is missing.
//!
//! First run is allowed to be empty. The editor opens a directory with no
//! profile, no `agents.toml` and no language server, so this command's job is to
//! say what is there rather than to demand setup. It starts no daemon, writes no
//! file, and asks no keyring for a value: a report is a read.
//!
//! The one failure is a thing that is *declared* and cannot run — a profile that
//! exists without a `config.toml`, a manifest that does not parse, or an agent
//! whose command is not on `PATH`. A machine with none of those but nothing set up
//! is healthy, which is the difference between a doctor and a toll booth.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use acpilot_agent::{KeyringResolver, Registry};
use acpilot_profile::Profile;
use acpilot_profile::project::Root;
use acpilot_proto::API_PREFIX;

/// Programs a person is likely to have already installed as an agent.
///
/// Detection only: Acpilot never starts one of these because it was found. The
/// list is what `doctor` looks for so a first-time user can see that the agent
/// they already use is one `agents.toml` line away.
pub const AGENT_PROGRAMS: &[&str] = &["claude", "codex", "gemini", "freebuff"];

/// What `doctor` found on one machine, as data so a test can read it without a
/// terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Where settings and profiles live.
    pub config: PathBuf,
    /// The profile line, already resolved: a profile need not exist.
    pub profile: String,
    /// The directory the launch is about.
    pub workspace: PathBuf,
    /// `.acpilot/config.toml`, when the workspace is a project.
    pub project: Option<PathBuf>,
    /// Agents the profile declares, in file order.
    pub declared: Vec<Declared>,
    /// Candidate agent programs found on `PATH`.
    pub on_path: Vec<&'static str>,
    /// Candidate agent programs not found on `PATH`.
    pub missing: Vec<&'static str>,
    /// Language servers this build knows, and whether each is installed.
    pub servers: Vec<Server>,
    /// Anything that is configured but cannot work.
    pub problems: Vec<String>,
}

/// One agent a profile declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    /// Display name from the manifest.
    pub name: String,
    /// The command the agent would spawn, when its transport has one.
    pub command: Option<String>,
    /// Whether that command is on `PATH`.
    pub installed: bool,
}

/// One language server the daemon can start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    /// The LSP language id.
    pub language: &'static str,
    /// The program the daemon would spawn.
    pub program: &'static str,
    /// Whether it is on `PATH`.
    pub installed: bool,
}

impl Report {
    /// Whether everything that is configured can actually run.
    ///
    /// A machine with no profile and no agents is healthy: that is the first-run
    /// state, not a broken one. Only a declared thing that cannot work fails.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    /// The report as a person reads it.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "acpilot {} · api {API_PREFIX}\n",
            acpilot_core::VERSION
        ));
        out.push_str(&format!("config     {}\n", self.config.display()));
        out.push_str(&format!("profile    {}\n", self.profile));
        out.push_str(&format!("workspace  {}\n", self.workspace.display()));
        match &self.project {
            Some(path) => out.push_str(&format!("project    {}\n", path.display())),
            None => out.push_str("project    none · temporary root, nothing is written\n"),
        }

        out.push_str("\nAgents\n");
        if self.declared.is_empty() {
            out.push_str("  declared   none · the editor works without one\n");
        } else {
            for agent in &self.declared {
                match (&agent.command, agent.installed) {
                    (Some(command), true) => {
                        out.push_str(&format!("  declared   {} · {command}\n", agent.name));
                    }
                    (Some(command), false) => {
                        out.push_str(&format!(
                            "  declared   {} · {command} · missing from PATH\n",
                            agent.name
                        ));
                    }
                    (None, _) => {
                        out.push_str(&format!(
                            "  declared   {} · remote, no local command\n",
                            agent.name
                        ));
                    }
                }
            }
        }
        if !self.on_path.is_empty() {
            out.push_str(&format!("  on PATH    {}\n", self.on_path.join(", ")));
        }
        if !self.missing.is_empty() {
            out.push_str(&format!("  missing    {}\n", self.missing.join(", ")));
        }

        out.push_str("\nLanguage servers\n");
        if self.servers.is_empty() {
            out.push_str("  none · this build knows no server\n");
        } else {
            for server in &self.servers {
                let state = if server.installed {
                    "installed"
                } else {
                    "missing"
                };
                out.push_str(&format!(
                    "  {:<12} {:<30} {state}\n",
                    server.language, server.program
                ));
            }
        }

        if !self.problems.is_empty() {
            out.push_str("\nProblems\n");
            for problem in &self.problems {
                out.push_str(&format!("  ! {problem}\n"));
            }
        }
        out
    }
}

/// Collect the report for `path` (or this directory) under `profile_name`.
///
/// `installed` answers whether a program is on `PATH`; the binary passes the
/// daemon's own lookup, and a test passes a table. Nothing else is injected: the
/// filesystem answers the rest.
///
/// # Errors
///
/// The root cannot be resolved — the current directory is unreadable, or a named
/// path does not exist. Everything after that is a finding in the report rather
/// than an error, because a wrong profile or manifest is what a doctor is for.
pub fn collect(
    path: Option<&Path>,
    profile_name: &str,
    installed: &dyn Fn(&str) -> bool,
) -> Result<Report, String> {
    let root = Root::resolve(path).map_err(|error| error.to_string())?;
    let mut problems = Vec::new();
    // A settings file that does not parse is a problem reported, not a reason to
    // stop: the rest of the report is still what someone needs to fix it.
    if let Err(error) = root.settings() {
        problems.push(error.to_string());
    }

    let profile_dir = acpilot_profile::profiles_dir().join(profile_name);
    let mut declared = Vec::new();
    let profile = if !profile_dir.exists() {
        format!(
            "{profile_name} · {} · not created yet (not required)",
            profile_dir.display()
        )
    } else {
        match Profile::named(profile_name) {
            Ok(profile) => {
                let manifest = profile.agents_path();
                if manifest.is_file() {
                    load_declared(
                        profile_name,
                        &manifest,
                        installed,
                        &mut declared,
                        &mut problems,
                    );
                }
                format!("{profile_name} · {}", profile.root().display())
            }
            Err(error) => {
                problems.push(error.to_string());
                format!("{profile_name} · {} · not a profile", profile_dir.display())
            }
        }
    };

    let mut on_path = Vec::new();
    let mut missing = Vec::new();
    for program in AGENT_PROGRAMS {
        if installed(program) {
            on_path.push(*program);
        } else {
            missing.push(*program);
        }
    }

    let servers = acpilotd::SERVERS
        .iter()
        .map(|&(language, program, _)| Server {
            language,
            program,
            installed: installed(program),
        })
        .collect();

    Ok(Report {
        config: acpilot_profile::config_dir(),
        profile,
        workspace: root.root().to_path_buf(),
        project: root.project().map(|project| project.config_path()),
        declared,
        on_path,
        missing,
        servers,
        problems,
    })
}

/// Read the profile's manifest into the report.
///
/// The real resolver is used rather than an empty map: a manifest whose secret
/// is missing is exactly the kind of broken declaration this command exists to
/// name. No secret is read unless the manifest asks for one.
fn load_declared(
    profile_name: &str,
    manifest: &Path,
    installed: &dyn Fn(&str) -> bool,
    declared: &mut Vec<Declared>,
    problems: &mut Vec<String>,
) {
    let resolver = Arc::new(KeyringResolver::new(profile_name));
    match Registry::load(manifest, resolver) {
        Ok(registry) => {
            for spec in registry.agents() {
                let command = spec.command().map(|command| command.program.clone());
                let available = command.as_deref().is_some_and(installed);
                if !available && let Some(command) = &command {
                    problems.push(format!(
                        "agent `{}` runs `{command}`, which is not on PATH",
                        spec.name
                    ));
                }
                declared.push(Declared {
                    name: spec.name.clone(),
                    command,
                    installed: available,
                });
            }
        }
        Err(error) => problems.push(format!("{}: {error}", manifest.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        Report {
            config: PathBuf::from("/cfg"),
            profile: "default · /cfg/profiles/default · not created yet (not required)".into(),
            workspace: PathBuf::from("/work"),
            project: None,
            declared: Vec::new(),
            on_path: vec!["claude"],
            missing: vec!["codex"],
            servers: vec![Server {
                language: "rust",
                program: "rust-analyzer",
                installed: true,
            }],
            problems: Vec::new(),
        }
    }

    #[test]
    fn a_machine_with_nothing_set_up_is_healthy() {
        let report = report();
        assert!(report.ok(), "first run is not a failure");
        let text = report.render();
        assert!(
            text.contains("declared   none · the editor works without one"),
            "{text}"
        );
        assert!(text.contains("not created yet (not required)"), "{text}");
        assert!(
            text.contains("temporary root, nothing is written"),
            "{text}"
        );
    }

    #[test]
    fn a_declared_agent_that_cannot_run_is_the_only_failure() {
        let mut report = report();
        report.declared.push(Declared {
            name: "Claude".into(),
            command: Some("claude".into()),
            installed: false,
        });
        report
            .problems
            .push("agent `Claude` runs `claude`, which is not on PATH".into());

        assert!(!report.ok());
        let text = report.render();
        assert!(text.contains("missing from PATH"), "{text}");
        assert!(text.contains("Problems"), "{text}");
    }

    #[test]
    fn an_agent_with_no_local_command_is_not_missing() {
        let mut report = report();
        report.declared.push(Declared {
            name: "Remote".into(),
            command: None,
            installed: false,
        });

        assert!(
            report.ok(),
            "reachability is a handshake fact, not a PATH fact"
        );
        assert!(report.render().contains("remote, no local command"));
    }
}
