//! The terminal UI.
//!
//! `acpilot tui` is a client of the daemon, not a second engine: it opens
//! sessions, sends prompts and answers permission requests over the same HTTP and
//! SSE API a browser or the GUI uses. The modules are the responsibilities, in
//! order: [`backend`] talks to the daemon, [`app`] holds state and maps keys to
//! requests, [`ui`] paints, and [`clipboard`] is the one escape a pane writes for
//! itself — OSC 52, because the clipboard belongs to the terminal, not to us.
//!
//! # Two sources, one interface
//!
//! [`run`] talks to a daemon and [`run_demo`] draws the seeded world in
//! `acpilot-demo`. They share the loop, the state and every widget: the design
//! pass is the real interface with a fixture behind it, which is what makes the
//! layout a thing that can be reviewed before the backend for it exists (P6-3).

pub mod app;
pub mod backend;
pub mod clipboard;
pub mod theme;
pub mod ui;

pub use app::App;
pub use theme::Theme;

use std::io;
use std::time::Duration;

use acpilot_proto::terminal::TerminalSize;
use crossterm::event::{self, Event, KeyEventKind};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::backend::{Update, Worker};

/// How long the loop waits for a key before redrawing.
///
/// Redrawing on a timer rather than only on input is what makes daemon updates
/// appear while nobody is typing. 120 ms is under the threshold where a counter
/// looks frozen and far above a busy loop.
const FRAME: Duration = Duration::from_millis(120);

/// Run the TUI against the daemon at `daemon` until the user leaves.
///
/// `token` is passed through to the client and is what a TCP daemon requires; the
/// caller resolves it, because the caller is the one with arguments and a home
/// directory. A socket endpoint ignores it.
///
/// `theme` is resolved by the caller too: the settings it comes from are the
/// project's, and this crate is given the answer rather than a path.
///
/// The terminal is restored on the way out, including when a draw fails: a TUI
/// that leaves the cursor hidden and the screen in raw mode is worse than the
/// crash that caused it.
pub fn run(daemon: &str, token: Option<&str>, theme: Theme, files: ThemeFiles) -> io::Result<()> {
    let (updates, inbox) = tokio::sync::mpsc::unbounded_channel();
    let changes = updates.clone();
    let worker = backend::spawn(daemon.to_owned(), token.map(str::to_owned), updates);
    watch_theme(files, changes);
    let mut app = App::new(daemon);
    app.set_theme(theme);
    drive(app, worker, inbox)
}

/// The settings files a running client re-reads when they change.
///
/// Paths rather than a value: a theme edited while the client is open should be
/// the theme on screen, and that means knowing what to watch. `None` is a surface
/// with no such file — the design pass reads nothing from disk — which is why the
/// default watches nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeFiles {
    /// The user's `config.toml`.
    pub user: Option<std::path::PathBuf>,
    /// The project's `.acpilot/config.toml`, when the launch is in a project.
    pub project: Option<std::path::PathBuf>,
}

/// Re-resolve the terminal's theme from the settings files.
///
/// [`None`] when the settings do not load or name a theme that is not installed:
/// a client that is already drawing keeps the colours it has rather than going
/// blank because a file was half-written when it was read.
#[must_use]
pub fn reload_theme(files: &ThemeFiles) -> Option<Theme> {
    use acpilot_proto::profile::ThemeSurface;

    let settings =
        acpilot_profile::settings::Settings::load(files.user.as_deref(), files.project.as_deref())
            .ok()?;
    let extensions = acpilot_extensions::builtin::load_all().ok()?;
    let resolved = acpilot_extensions::ThemeRegistry::new(&extensions)
        .complete(settings.preferences.theme_for(ThemeSurface::Tui))
        .ok()?;
    Some(Theme::from_resolved(&resolved))
}

/// Watch the settings files and send a new theme when one of them changes.
///
/// The directory is watched rather than the file because a settings file is often
/// created after the client starts, and because an editor that saves by rename
/// would otherwise stop being watched the first time it saved. Events are
/// debounced: one save is several writes, and a theme that flickered through three
/// palettes would be worse than one that changed once.
fn watch_theme(files: ThemeFiles, updates: tokio::sync::mpsc::UnboundedSender<Update>) {
    std::thread::spawn(move || {
        let (events, inbox) = std::sync::mpsc::channel();
        let Ok(mut watcher) = notify::recommended_watcher(move |event| {
            let _ = events.send(event);
        }) else {
            return;
        };
        let mut watched = Vec::new();
        watch_directories(&mut watcher, &files, &mut watched);
        if watched.is_empty() {
            // The design pass and the demo read no settings: nothing to re-read.
            return;
        }
        while let Ok(Ok(_)) = inbox.recv() {
            std::thread::sleep(std::time::Duration::from_millis(150));
            while inbox.try_recv().is_ok() {}
            // The file may have appeared in a directory the first event created, so
            // the watch is re-armed before the settings are read.
            watch_directories(&mut watcher, &files, &mut watched);
            let Some(theme) = reload_theme(&files) else {
                continue;
            };
            if updates.send(Update::Theme(theme)).is_err() {
                // The client is gone; so is the reason to watch.
                return;
            }
        }
    });
}

/// Watch every settings directory that exists, once each.
///
/// Called again on each event: a directory created since the last one may hold the
/// file now, which is how a project that had no `.acpilot` when the client started
/// still gets a theme when one is written.
fn watch_directories(
    watcher: &mut impl notify::Watcher,
    files: &ThemeFiles,
    watched: &mut Vec<std::path::PathBuf>,
) {
    for directory in [files.user.as_deref(), files.project.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(nearest_directory)
    {
        if watched.contains(&directory) {
            continue;
        }
        // A directory that cannot be watched is not a failed launch: the theme
        // simply stays the one that is on screen.
        if watcher
            .watch(&directory, notify::RecursiveMode::NonRecursive)
            .is_ok()
        {
            watched.push(directory);
        }
    }
}

/// The directory to watch for `path`: its own, or the nearest ancestor that exists.
///
/// A settings file is not necessarily there when the client starts — a project that
/// has never been configured has no `.acpilot` directory — and watching a path that
/// does not exist fails rather than waiting for it. The nearest existing ancestor
/// exists by construction, so the creation below it is the event that re-arms the
/// watch closer to the file.
fn nearest_directory(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut directory = path.parent()?;
    loop {
        if directory.is_dir() {
            return Some(directory.to_path_buf());
        }
        directory = directory.parent()?;
    }
}

/// Run the design pass: the seeded world from `acpilot-demo`, drawn by this UI.
///
/// No daemon is contacted and nothing is read from disk, so the screen is the same
/// on any machine — which is what makes it something to point at in a review. The
/// desktop app draws the same world when it gets its shell (P10).
pub fn run_demo(demo: acpilot_demo::Demo, theme: Theme) -> io::Result<()> {
    let (updates, inbox) = tokio::sync::mpsc::unbounded_channel();
    let worker = backend::spawn_demo(demo.clone(), updates);
    let mut app = App::demo(&demo);
    app.set_theme(theme);
    drive(app, worker, inbox)
}

/// Put the terminal in and out of raw mode around the loop.
///
/// Mouse reporting is turned on for the same span, because the menu bar and the two
/// dividers take the pointer. That costs the terminal's own click-drag selection: most terminals keep
/// it on Shift-drag, and copy mode is the built-in answer.
fn drive(app: App, worker: Worker, mut inbox: UnboundedReceiver<Update>) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    // A terminal that will not report the mouse is still a terminal: the keys and
    // the divider chords work, so a refused escape is not a reason to fail a launch.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let result = event_loop(&mut terminal, app, worker, &mut inbox);
    // Reportings off before the screen is restored, so a failed session does not
    // leave the shell receiving mouse events it never asked for.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    worker: Worker,
    inbox: &mut UnboundedReceiver<Update>,
) -> io::Result<()> {
    // The size the program was last told about. A resize is news once, not every
    // frame: a terminal that reflowed 8 times a second would flicker.
    let mut told: Option<TerminalSize> = None;

    loop {
        while let Ok(update) = inbox.try_recv() {
            app.apply(update);
        }

        // `draw` reads the layout and hands back the panes' inner sizes, so a
        // program and the screen agree on the width, and a copy cursor and the
        // transcript agree on how many lines it can scroll — without the loop
        // repeating the layout arithmetic.
        let viewports = std::cell::Cell::new(None);
        terminal.draw(|frame| viewports.set(Some(ui::draw(frame, &app))))?;
        if let Some(viewports) = viewports.get() {
            app.set_viewports(viewports);
            if app.terminal.is_some() {
                let size = viewports.terminal;
                if told != Some(size) {
                    worker.send(backend::Request::TerminalResize(size));
                    told = Some(size);
                }
            } else {
                told = None;
            }
        }

        if event::poll(FRAME)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(request) = app.handle_key(key) {
                        worker.send(request);
                    }
                }
                // A dragged divider is a width, not a keystroke: it changes the
                // layout and sends nothing to the daemon. A menu row clicked is a
                // command, so it answers with what a key would.
                Event::Mouse(mouse) => {
                    if let Some(request) = app.handle_mouse(mouse) {
                        worker.send(request);
                    }
                }
                _ => {}
            }
        }

        // A copy leaves as OSC 52 here rather than from the app: the escape goes
        // to the terminal a person is looking at, and the drawing loop is the only
        // thing in this process that owns that stream.
        if let Some(text) = app.take_clipboard() {
            crossterm::execute!(
                std::io::stdout(),
                crossterm::style::Print(clipboard::osc52(&text))
            )?;
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write(directory: &std::path::Path, name: &str, text: &str) -> std::path::PathBuf {
        let path = directory.join(name);
        fs::write(&path, text).expect("settings file");
        path
    }

    #[test]
    fn a_theme_edit_is_the_theme_after_it_is_saved() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "config.toml",
            "[preferences]\ntheme_tui = \"acpilot.night-flight\"\n",
        );
        let files = ThemeFiles {
            user: Some(user.clone()),
            project: None,
        };
        assert_eq!(
            reload_theme(&files).expect("a theme").name(),
            "Night flight"
        );

        // The same file, saved again with another id: no restart in between.
        write(
            directory.path(),
            "config.toml",
            "[preferences]\ntheme_tui = \"acpilot.paper\"\n",
        );
        assert_eq!(reload_theme(&files).expect("a theme").name(), "Paper");
    }

    #[test]
    fn the_projects_theme_wins_over_the_users_for_that_project() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "user.toml",
            "[preferences]\ntheme = \"acpilot.paper\"\n",
        );
        let project = write(
            directory.path(),
            "project.toml",
            "[preferences]\ntheme_tui = \"acpilot.night-flight\"\n",
        );
        let files = ThemeFiles {
            user: Some(user),
            project: Some(project),
        };
        assert_eq!(
            reload_theme(&files).expect("a theme").name(),
            "Night flight",
            "the project's terminal theme, over the user's shared one"
        );
    }

    #[test]
    fn a_settings_file_that_does_not_parse_keeps_the_current_theme() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(directory.path(), "config.toml", "[preferences\ntheme = 3\n");
        assert!(
            reload_theme(&ThemeFiles {
                user: Some(user),
                project: None,
            })
            .is_none(),
            "no theme, so the caller keeps what it is drawing"
        );
    }

    #[test]
    fn a_settings_file_that_is_not_there_yet_is_watched_at_the_nearest_directory() {
        let directory = tempfile::tempdir().expect("temp dir");
        let missing = directory.path().join(".acpilot").join("config.toml");
        assert_eq!(
            nearest_directory(&missing).as_deref(),
            Some(directory.path()),
            "the project root, because `.acpilot` does not exist yet"
        );
    }

    #[test]
    fn a_theme_that_is_not_installed_is_not_silently_the_default() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "config.toml",
            "[preferences]\ntheme_tui = \"acpilot.no-such-theme\"\n",
        );
        assert!(
            reload_theme(&ThemeFiles {
                user: Some(user),
                project: None,
            })
            .is_none()
        );
    }
}
