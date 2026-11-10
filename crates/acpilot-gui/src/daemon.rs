//! The window's line to the daemon: the theme it draws with, and the integration
//! stats its footer bar shows.
//!
//! The desktop app is a client of the same API as the terminal and the browser,
//! and the preferences it draws with are the daemon's answer rather than the
//! window's own reading of `config.toml`. That matters most for the case this
//! module exists for: a window started somewhere that cannot see the daemon's
//! files — a remote machine, a container, a user whose config lives on a share.
//! It subscribes to `GET /v1/events`, takes the `prefs` frame that opens the
//! stream, and takes a new one after every save.
//!
//! It lives outside the `gui` feature on purpose. The subscription and the
//! preferences-to-palette mapping are the part that can be wrong, and they are
//! tested without a window; [`crate::app`] only has to put the answer on screen.
//!
//! No daemon is spawned. A window that starts a daemon has taken on the session
//! and workspace it serves, which is P10-4's decision; until then a window with no
//! daemon behind it simply keeps the theme it was launched with.

use std::time::Duration;

use acpilot_extensions::loader::LoadedExtension;
use acpilot_extensions::theme::ThemeError;
use acpilot_proto::git::{CommitDraft, GitCommit, GitDiff, GitStatus};
use acpilot_proto::profile::{Preferences, ThemeSurface};
use acpilot_proto::session::SessionSummary;
use acpilot_stats::Row;

use crate::theme::GuiTheme;

/// How often the window asks what its integrations cost.
///
/// The same clock the terminal's bar runs on: a context window and a balance move
/// over a turn, not over a keystroke — and every row carries the moment it was
/// observed, so a fast clock would buy nothing but requests.
pub const STATS_POLL: Duration = Duration::from_secs(15);

/// Where the daemon is, when the caller did not say.
///
/// What the source-control pane asks the daemon for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitCommand {
    /// Read status, the whole diff, and the sessions a draft may use.
    Load,
    /// Put one repository-relative path in the index.
    Stage(String),
    /// Put one unstaged hunk of a path in the index.
    StageHunk {
        /// The path selected in the pane.
        path: String,
        /// The zero-based hunk in that path's unstaged diff.
        hunk: usize,
    },
    /// Remove one repository-relative path from the index.
    Unstage(String),
    /// Restore one path's working-tree half from the index.
    Discard(String),
    /// Restore one path's index and working tree from HEAD.
    Checkout(String),
    /// Ask this session to draft a message from the staged index.
    Generate(String),
    /// Commit the index with a message the person confirmed.
    Commit(String),
}

/// What the git worker tells a window.
#[derive(Debug, Clone)]
pub enum GitUpdate {
    /// Status, both diff halves and sessions, read at the same moment.
    Loaded {
        /// Branch state and every changed path.
        status: GitStatus,
        /// Working tree against the index: the hunks that can still be staged.
        unstaged: GitDiff,
        /// Index against `HEAD`: the hunks the next commit already holds.
        staged: GitDiff,
        /// The sessions a draft may be generated from.
        sessions: Vec<SessionSummary>,
    },
    /// The read failed, with the daemon's message.
    LoadFailed(String),
    /// A session's draft, for the composer to edit.
    Draft(CommitDraft),
    /// A draft was refused.
    GenerateFailed(String),
    /// The index was committed.
    Committed(GitCommit),
    /// A commit was refused by git or the daemon.
    CommitFailed(String),
}

/// Work the source-control requests one at a time, off the window's executor.
///
/// A thread of its own, like [`follow_stats`] and [`follow`]. The request channel is
/// blocking on purpose: a refresh, a draft and a commit are commands from a person,
/// not a poll, and each is answered on the same thread so two commits cannot race.
pub fn follow_git(
    endpoint: String,
    token: Option<String>,
    requests: std::sync::mpsc::Receiver<GitCommand>,
    sink: impl Fn(GitUpdate) + Send + 'static,
) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let client = client(&endpoint, token.as_deref());
        while let Ok(command) = requests.recv() {
            runtime.block_on(async {
                match command {
                    GitCommand::Load => sink(load(&client).await),
                    GitCommand::Stage(path) => {
                        match client.stage(std::slice::from_ref(&path)).await {
                            // The route answers with the new status, but the pane also
                            // needs the patch; one read of both after the write is the
                            // same moment the refresh draws.
                            Ok(_) => sink(load(&client).await),
                            Err(error) => sink(GitUpdate::LoadFailed(error.to_string())),
                        }
                    }
                    GitCommand::StageHunk { path, hunk } => {
                        match client.stage_hunk(&path, hunk).await {
                            Ok(_) => sink(load(&client).await),
                            Err(error) => sink(GitUpdate::LoadFailed(error.to_string())),
                        }
                    }
                    GitCommand::Unstage(path) => {
                        match client.unstage(std::slice::from_ref(&path)).await {
                            Ok(_) => sink(load(&client).await),
                            Err(error) => sink(GitUpdate::LoadFailed(error.to_string())),
                        }
                    }
                    GitCommand::Discard(path) => {
                        match client.discard(std::slice::from_ref(&path)).await {
                            Ok(_) => sink(load(&client).await),
                            Err(error) => sink(GitUpdate::LoadFailed(error.to_string())),
                        }
                    }
                    GitCommand::Checkout(path) => {
                        match client.checkout(std::slice::from_ref(&path)).await {
                            Ok(_) => sink(load(&client).await),
                            Err(error) => sink(GitUpdate::LoadFailed(error.to_string())),
                        }
                    }
                    GitCommand::Generate(session) => match client.commit_message(&session).await {
                        Ok(draft) => sink(GitUpdate::Draft(draft)),
                        Err(error) => sink(GitUpdate::GenerateFailed(error.to_string())),
                    },
                    GitCommand::Commit(message) => match client.commit(&message).await {
                        Ok(commit) => sink(GitUpdate::Committed(commit)),
                        Err(error) => sink(GitUpdate::CommitFailed(error.to_string())),
                    },
                }
            });
        }
    });
}

/// Read the two answers the source pane draws together.
///
/// Status and diff are one request each, but the pane treats them as one moment:
/// asking for them back to back after a write is how the staged count and the patch
/// describe the same index rather than two moments either side of it.
async fn load(client: &acpilot_client::Client) -> GitUpdate {
    match client.git_status().await {
        Ok(status) => match client.git_diff("unstaged").await {
            Ok(unstaged) => match client.git_diff("staged").await {
                Ok(staged) => {
                    // A session list is an optional half: an old daemon with no
                    // sessions still has a repository to show.
                    let sessions = client.sessions().await.unwrap_or_default();
                    GitUpdate::Loaded {
                        status,
                        unstaged,
                        staged,
                        sessions,
                    }
                }
                Err(error) => GitUpdate::LoadFailed(error.to_string()),
            },
            Err(error) => GitUpdate::LoadFailed(error.to_string()),
        },
        Err(error) => GitUpdate::LoadFailed(error.to_string()),
    }
}

/// `ACPILOT_API_URL` is the same variable `acpilot --api-url` fills, so a window
/// and a terminal started from one shell talk to one daemon. Otherwise the
/// platform's default endpoint: the per-user socket, or loopback HTTP on a
/// platform without one.
#[must_use]
pub fn endpoint() -> String {
    std::env::var(acpilot_client::daemon::ENV_ENDPOINT)
        .ok()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .unwrap_or_else(acpilot_client::daemon::default_endpoint)
}

/// The token a local daemon wrote, when there is one.
///
/// `acpilot serve` and a TCP daemon save the bearer token in the runtime
/// directory. A window against a port on this machine reads that file rather than
/// making the person copy the startup panel; `--token` or `ACPILOT_TOKEN` still
/// wins, for a daemon whose runtime directory this machine cannot see.
#[must_use]
pub fn token() -> Option<String> {
    acpilot_host::token::local()
}

/// A client that carries the token when the launch has one.
///
/// One place for the choice, rather than three copies that could disagree: an
/// untokened client against a guarded port answers 401 and looks exactly like a
/// daemon with nothing in it.
fn client(endpoint: &str, token: Option<&str>) -> acpilot_client::Client {
    match token {
        Some(token) => acpilot_client::Client::with_token(endpoint, token),
        None => acpilot_client::Client::new(endpoint),
    }
}

/// The theme these preferences ask the window to draw with.
///
/// `theme_gui` over `theme`, which is the same cascade the launch resolves — the
/// difference is where the answer comes from, not what it means.
pub fn theme_for(
    preferences: &Preferences,
    extensions: &[LoadedExtension],
) -> Result<GuiTheme, ThemeError> {
    GuiTheme::from_extensions(extensions, preferences.theme_for(ThemeSurface::Gui))
}

/// Follow the daemon's integration stats, calling `sink` with every answer.
///
/// A thread of its own, for the same reason the settings subscription has one: a
/// request is a wait, and the window's executor has to stay free to draw. Unlike
/// the settings stream this keeps asking after a failure — a daemon restarted under
/// a running window is ordinary, and a bar that went blank for the rest of the
/// session would be a worse answer than a stale one. The first failure is printed
/// once per outage, because a message every [`STATS_POLL`] is not a status line.
///
/// A failed request never reaches `sink`: the rows already on screen stay there,
/// exactly as the terminal's bar keeps them.
pub fn follow_stats(
    endpoint: String,
    token: Option<String>,
    sink: impl Fn(Vec<Row>) + Send + 'static,
) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            let client = client(&endpoint, token.as_deref());
            let mut said_so = false;
            loop {
                match client.stats().await {
                    Ok(stats) => {
                        said_so = false;
                        sink(acpilot_stats::rows_now(&stats));
                    }
                    Err(error) => {
                        if !said_so {
                            eprintln!("acpilot-gui: not following the daemon's stats: {error}");
                            said_so = true;
                        }
                    }
                }
                tokio::time::sleep(STATS_POLL).await;
            }
        });
    });
}

/// Follow the daemon's settings, calling `sink` with every theme it names.
///
/// A thread of its own with a current-thread runtime: the stream is one long
/// request, and the window's executor should not be holding it. It never returns
/// while the daemon answers, and it gives up quietly when it does not — a window
/// without a daemon is a window with the theme it was launched with, not an error
/// to show someone who only wanted to look at a file.
pub fn follow(
    endpoint: String,
    token: Option<String>,
    extensions: Vec<LoadedExtension>,
    sink: impl Fn(GuiTheme) + Send + 'static,
) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            use futures::StreamExt as _;

            let client = client(&endpoint, token.as_deref());
            let mut frames = client.profile_events();
            while let Some(frame) = frames.next().await {
                match frame {
                    Ok(acpilot_client::ProfileStream::Preferences(preferences)) => {
                        match theme_for(&preferences, &extensions) {
                            Ok(theme) => sink(theme),
                            // A preferences frame naming a theme nobody provides is
                            // the daemon's business to report; the window keeps the
                            // colours it has rather than going blank.
                            Err(error) => eprintln!("acpilot-gui: preferences: {error}"),
                        }
                    }
                    Ok(acpilot_client::ProfileStream::PreferencesError(message)) => {
                        eprintln!("acpilot-gui: settings not applied: {message}");
                    }
                    // The stream is alive and has nothing new. Nothing to draw.
                    Ok(_) => {}
                    Err(error) => {
                        // Said once, then the window is on its own: a daemon that
                        // cannot be reached is not a reason to shout every frame.
                        eprintln!("acpilot-gui: not following the daemon's settings: {error}");
                        return;
                    }
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use acpilot_extensions::builtin;

    use super::*;

    fn extensions() -> Vec<LoadedExtension> {
        builtin::load_all().expect("the built-in extensions load")
    }

    /// Preferences as the daemon would send them: the shipped defaults, with the
    /// two keys this module is about named. The frame carries every field, which
    /// is why the defaults are the starting point rather than a hand-built object.
    fn preferences(theme: &str, theme_gui: Option<&str>) -> Preferences {
        let mut preferences = acpilot_profile::settings::Settings::load(None, None)
            .expect("the shipped defaults load")
            .preferences;
        preferences.theme = theme.to_owned();
        preferences.theme_gui = theme_gui.map(str::to_owned);
        preferences
    }

    #[test]
    fn the_window_draws_with_the_theme_the_preferences_name_for_it() {
        let theme = theme_for(&preferences("acpilot.night-flight", None), &extensions())
            .expect("night flight resolves");
        assert_eq!(theme.name(), "Night flight");

        let theme = theme_for(
            &preferences("acpilot.night-flight", Some("acpilot.paper")),
            &extensions(),
        )
        .expect("paper resolves");
        assert_eq!(
            theme.name(),
            "Paper",
            "`theme_gui` is the window's half of the pair"
        );
        assert!(!theme.is_dark());
    }

    #[test]
    fn a_preferences_frame_naming_a_theme_nobody_provides_is_an_error_not_a_blank_window() {
        let error = theme_for(&preferences("acpilot.no-such-theme", None), &extensions())
            .expect_err("an unknown theme is refused");
        assert!(
            error.to_string().contains("acpilot.no-such-theme"),
            "{error}"
        );
    }

    #[test]
    fn the_endpoint_is_the_one_the_terminal_would_use() {
        // No environment manipulation: the variable is read, and the fallback is
        // the platform's default endpoint, so this asserts the relationship
        // rather than a value that would depend on the shell running the test.
        assert_eq!(
            endpoint(),
            std::env::var(acpilot_client::daemon::ENV_ENDPOINT)
                .ok()
                .filter(|endpoint| !endpoint.trim().is_empty())
                .unwrap_or_else(acpilot_client::daemon::default_endpoint)
        );
    }
}
