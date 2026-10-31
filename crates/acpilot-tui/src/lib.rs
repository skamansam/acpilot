//! The terminal UI.
//!
//! `acpilot tui` is a client of the daemon, not a second engine: it opens
//! sessions, sends prompts and answers permission requests over the same HTTP and
//! SSE API a browser or the GUI uses. The modules are the responsibilities, in
//! order: [`backend`] talks to the daemon, [`app`] holds state and maps keys to
//! requests, [`editor`] owns the document the editor pane draws, [`ui`] paints,
//! and [`clipboard`] is the one escape a pane writes for itself — OSC 52, because
//! the clipboard belongs to the terminal, not to us.
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
pub mod editor;
pub mod layout;
pub mod theme;
pub mod ui;

pub use app::App;
pub use editor::EditorPane;
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

/// How often a drag resting at a pane's edge scrolls.
///
/// A drag only arrives while the pointer moves, so a pointer held still against
/// an edge sends nothing at all — the scroll has to be the loop's own clock. 30 ms
/// is a line per tick at about 33 lines a second, which reads as one continuous
/// scroll without being a busy loop; the clock is only this fast while there is
/// more view in the direction the pointer is pulling.
const AUTOSCROLL: Duration = Duration::from_millis(30);

/// How long the terminal is given to answer the colour query.
///
/// One escape round-trip before the screen is taken over. Long enough for a
/// terminal at the end of an SSH connection, short enough that a terminal which
/// will not answer costs a quarter of a second at launch rather than a second.
const QUERY_TIMEOUT: Duration = Duration::from_millis(250);

/// The file the divider widths are remembered in, under the data directory.
const LAYOUT_FILE: &str = "tui-layout.toml";

/// Run the TUI against the daemon at `daemon` until the user leaves.
///
/// `token` is passed through to the client and is what a TCP daemon requires; the
/// caller resolves it, because the caller is the one with arguments and a home
/// directory. A socket endpoint ignores it.
///
/// `theme` is resolved by the caller too: the settings it comes from are the
/// project's, and this crate is given the answer rather than a path. It is the
/// fallback rather than the last word: `theme_follow_terminal` solves a palette
/// from the terminal itself, and a terminal that will not answer leaves the
/// caller's theme on screen.
///
/// `mouse` says whether the terminal's mouse reporting is turned on. Off is a real
/// setting rather than a debug switch: mouse reporting costs the terminal's own
/// click-drag selection, which some people would rather keep than have a menu bar
/// answer a pointer.
///
/// The terminal is restored on the way out, including when a draw fails: a TUI
/// that leaves the cursor hidden and the screen in raw mode is worse than the
/// crash that caused it.
pub fn run(
    daemon: &str,
    token: Option<&str>,
    theme: Theme,
    files: ThemeFiles,
    mouse: bool,
) -> io::Result<()> {
    // Asked before the screen is taken over and before the loop owns the terminal's
    // input: from then on, nothing in this process can read a reply but the event
    // reader, so a second query is not a thing that can be added later.
    let files = files.with_terminal_colours();
    let theme = match reload_theme(&files) {
        Some(theme) => theme,
        None => {
            if files.terminal.is_none() && follows_terminal(&files) {
                // Silence here would be a setting that appears to do nothing: the
                // person asked for the terminal's own colours and got the named
                // theme, and the reason is the terminal, not the file.
                eprintln!(
                    "acpilot tui: theme_follow_terminal is on and this terminal did not report \
                     its colours; drawing the theme the settings name instead"
                );
            }
            theme
        }
    };
    let (updates, inbox) = tokio::sync::mpsc::unbounded_channel();
    let changes = updates.clone();
    let worker = backend::spawn(
        daemon.to_owned(),
        token.map(str::to_owned),
        files.terminal,
        updates,
    );
    watch_theme(files, changes);
    let mut app = App::new(daemon);
    // A width the dividers were dragged to is remembered per machine, in the data
    // directory rather than in `config.toml`: it is the client's memory of itself,
    // not a setting a person writes.
    app.remember_layout(acpilot_profile::data_dir().join(LAYOUT_FILE));
    app.set_theme(theme);
    drive(app, worker, inbox, mouse)
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
    /// What the terminal said its own colours are, when it answered.
    pub terminal: Option<TerminalColours>,
}

/// A pair of colours the terminal reported about itself.
///
/// The pair, not a theme: [`acpilot_extensions::follow`] is what turns it into a
/// complete token set, and it is the same solver whether the answer came from a
/// terminal or from a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalColours {
    /// The terminal's background.
    pub background: acpilot_extensions::Color,
    /// The terminal's foreground.
    pub foreground: acpilot_extensions::Color,
}

impl ThemeFiles {
    /// The same files, plus the colours the terminal reports about itself.
    ///
    /// Asked once, before the drawing loop starts: the reply arrives on the same
    /// stream the loop will read keys from, so asking later would mean two readers
    /// of one terminal. A terminal that does not answer leaves [`None`] here, which
    /// is the setting's fallback path rather than an error.
    #[must_use]
    pub fn with_terminal_colours(mut self) -> Self {
        self.terminal = query_terminal_colours();
        self
    }
}

/// Re-resolve the terminal's theme from the settings files.
///
/// [`None`] when the settings do not load or name a theme that is not installed:
/// a client that is already drawing keeps the colours it has rather than going
/// blank because a file was half-written when it was read.
#[must_use]
pub fn reload_theme(files: &ThemeFiles) -> Option<Theme> {
    let settings = settings(files)?;
    theme_for(&settings.preferences, files.terminal)
}

/// The theme these preferences ask this client to draw with.
///
/// One function for both sources: the settings a launch read off the disk, and the
/// `prefs` frame a daemon sends. They are the same preferences, so resolving them
/// twice would be two answers to one question — and the daemon's is the one that
/// reaches a terminal that cannot see the daemon's files.
#[must_use]
pub fn theme_for(
    preferences: &acpilot_proto::profile::Preferences,
    terminal: Option<TerminalColours>,
) -> Option<Theme> {
    use acpilot_proto::profile::ThemeSurface;

    if preferences.theme_follow_terminal {
        // No theme is named: the pair the terminal reported is solved into one. A
        // terminal that never answered leaves the theme already on screen, because
        // a settings edit is not a reason to lose the colours a person is reading.
        let colours = terminal?;
        let resolved = acpilot_extensions::follow::solve(colours.background, colours.foreground)?;
        return Some(Theme::from_resolved(&resolved));
    }
    let extensions = acpilot_extensions::builtin::load_all().ok()?;
    let resolved = acpilot_extensions::ThemeRegistry::new(&extensions)
        .complete(preferences.theme_for(ThemeSurface::Tui))
        .ok()?;
    Some(Theme::from_resolved(&resolved))
}

/// The settings a launch resolved, when they load.
fn settings(files: &ThemeFiles) -> Option<acpilot_profile::settings::Settings> {
    acpilot_profile::settings::Settings::load(files.user.as_deref(), files.project.as_deref()).ok()
}

/// Whether the settings ask for the terminal's own palette.
fn follows_terminal(files: &ThemeFiles) -> bool {
    settings(files).is_some_and(|settings| settings.preferences.theme_follow_terminal)
}

/// What the terminal says about its own colours, or [`None`] when it will not say.
///
/// One OSC round-trip (`11` for the background, `10` for the foreground) with a
/// timeout, asked by `terminal-colorsaurus` so that raw mode is restored on the way
/// out and a terminal that does not answer is recognised rather than waited on. The
/// cost is paid once per launch and only in a terminal: no window, no daemon and no
/// CI job runs this.
#[must_use]
pub fn query_terminal_colours() -> Option<TerminalColours> {
    let mut options = terminal_colorsaurus::QueryOptions::default();
    options.timeout = QUERY_TIMEOUT;
    let palette = terminal_colorsaurus::color_palette(options).ok()?;
    let (red, green, blue) = palette.background.scale_to_8bit();
    let background = acpilot_extensions::Color::from_rgb(red, green, blue);
    let (red, green, blue) = palette.foreground.scale_to_8bit();
    Some(TerminalColours {
        background,
        foreground: acpilot_extensions::Color::from_rgb(red, green, blue),
    })
}

/// Watch the settings files and send a new theme when one of them changes.
///
/// The watching is [`acpilot_profile::watch`]'s, not this crate's: the daemon
/// announces the same saves on its own stream, and two watchers would be two
/// debounce policies and two answers to an editor that saves by rename. A client
/// with no settings to watch — the design pass, the demo — simply has no thread.
fn watch_theme(files: ThemeFiles, updates: tokio::sync::mpsc::UnboundedSender<Update>) {
    let paths: Vec<std::path::PathBuf> = [files.user.clone(), files.project.clone()]
        .into_iter()
        .flatten()
        .collect();
    let Some(changes) = acpilot_profile::watch::changes(paths) else {
        return;
    };
    std::thread::spawn(move || {
        while changes.recv().is_ok() {
            let Some(theme) = reload_theme(&files) else {
                // A file that is half-written, or an id no extension provides: the
                // colours on screen stay, and the next save is read normally.
                continue;
            };
            if updates.send(Update::Theme(theme)).is_err() {
                // The client is gone; so is the reason to watch.
                return;
            }
        }
    });
}

/// Run the design pass: the seeded world from `acpilot-demo`, drawn by this UI.
///
/// No daemon is contacted and nothing is read from disk, so the screen is the same
/// on any machine — which is what makes it something to point at in a review. The
/// desktop app draws the same world when it gets its shell (P10).
pub fn run_demo(demo: acpilot_demo::Demo, theme: Theme, mouse: bool) -> io::Result<()> {
    let (updates, inbox) = tokio::sync::mpsc::unbounded_channel();
    let worker = backend::spawn_demo(demo.clone(), updates);
    let mut app = App::demo(&demo);
    app.set_theme(theme);
    drive(app, worker, inbox, mouse)
}

/// Put the terminal in and out of raw mode around the loop.
///
/// Mouse reporting is turned on for the same span, when it is wanted, because the
/// menu bar, the panes and the two dividers take the pointer. That costs the
/// terminal's own click-drag selection: most terminals keep it on Shift-drag, and
/// copy mode is the built-in answer — and `--no-mouse` is there for someone who
/// would rather have the terminal's.
fn drive(
    app: App,
    worker: Worker,
    mut inbox: UnboundedReceiver<Update>,
    mouse: bool,
) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    // A terminal that will not report the mouse is still a terminal: the keys and
    // the divider chords work, so a refused escape is not a reason to fail a launch.
    if mouse {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    }
    let mut app = app;
    let result = event_loop(&mut terminal, &mut app, worker, &mut inbox);
    // Reportings off before the screen is restored, so a failed session does not
    // leave the shell receiving mouse events it never asked for.
    if mouse {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    }
    ratatui::restore();
    // After the screen is the shell's again, so the sentence lands in a terminal
    // where the person can read it.
    if let Some(error) = app.save_layout() {
        eprintln!("acpilot tui: cannot remember the pane widths: {error}");
    }
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
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

        // A drag held against a pane's edge is a scroll that keeps going without
        // the pointer moving a cell, so it is asked for here rather than waited
        // for as an event.
        app.autoscroll();

        // `draw` reads the layout and hands back the panes' inner sizes, so a
        // program and the screen agree on the width, and a copy cursor and the
        // transcript agree on how many lines it can scroll — without the loop
        // repeating the layout arithmetic. It takes the app mutably because the
        // editor pane measures itself against the cells it was given and paints
        // from the parse the measurement asked for; every other pane still only
        // reads.
        let viewports = std::cell::Cell::new(None);
        terminal.draw(|frame| viewports.set(Some(ui::draw(frame, app))))?;
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

        let wait = if app.autoscrolling() {
            AUTOSCROLL
        } else {
            FRAME
        };
        if event::poll(wait)? {
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
            terminal: None,
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
            terminal: None,
        };
        assert_eq!(
            reload_theme(&files).expect("a theme").name(),
            "Night flight",
            "the project's terminal theme, over the user's shared one"
        );
    }

    #[test]
    fn the_daemons_preferences_resolve_the_theme_the_files_would() {
        // One resolver for both sources: a `prefs` frame and a `config.toml` say the
        // same preferences, so they must name the same palette.
        let mut preferences = acpilot_profile::settings::Settings::load(None, None)
            .expect("the shipped defaults")
            .preferences;
        preferences.theme_tui = Some("acpilot.paper".to_owned());
        assert_eq!(
            theme_for(&preferences, None)
                .expect("paper resolves")
                .name(),
            "Paper"
        );
    }

    #[test]
    fn a_daemon_that_follows_the_terminal_is_solved_from_the_pair_it_reported() {
        let mut preferences = acpilot_profile::settings::Settings::load(None, None)
            .expect("the shipped defaults")
            .preferences;
        preferences.theme_follow_terminal = true;
        let colours = TerminalColours {
            background: acpilot_extensions::Color::from_rgb(0x00, 0x00, 0x00),
            foreground: acpilot_extensions::Color::from_rgb(0xff, 0xff, 0xff),
        };
        let theme = theme_for(&preferences, Some(colours)).expect("solvable");
        assert_eq!(theme.name(), acpilot_extensions::follow::NAME);
        assert!(theme.is_dark());
        assert!(
            theme_for(&preferences, None).is_none(),
            "with no pair there is no theme, so the caller keeps what it has"
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
                terminal: None,
            })
            .is_none(),
            "no theme, so the caller keeps what it is drawing"
        );
    }

    #[test]
    fn a_followed_palette_is_the_one_the_terminal_reports() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "config.toml",
            "[preferences]\ntheme_follow_terminal = true\n",
        );
        let files = ThemeFiles {
            user: Some(user),
            project: None,
            terminal: Some(TerminalColours {
                background: acpilot_extensions::Color::from_rgb(0x1e, 0x1e, 0x1e),
                foreground: acpilot_extensions::Color::from_rgb(0xd8, 0xd8, 0xd8),
            }),
        };
        let theme = reload_theme(&files).expect("a solved palette");
        assert_eq!(theme.name(), acpilot_extensions::follow::NAME);
        assert!(theme.is_dark());
        // Real colours, not `Color::Reset`: the solver's answer is what ratatui
        // paints with, which is the whole point of following the terminal.
        assert!(
            matches!(theme.bg(), ratatui::style::Color::Rgb(..)),
            "a followed background is an rgb value, not the terminal's own"
        );
        assert!(
            matches!(theme.bg_panel(), ratatui::style::Color::Rgb(..)),
            "and so is the panel the window ladder is measured on"
        );
    }

    #[test]
    fn a_terminal_that_did_not_answer_keeps_the_theme_already_drawn() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "config.toml",
            "[preferences]\ntheme_follow_terminal = true\ntheme_tui = \"acpilot.paper\"\n",
        );
        assert!(
            reload_theme(&ThemeFiles {
                user: Some(user),
                project: None,
                terminal: None,
            })
            .is_none(),
            "no colours to solve from, so the caller keeps what it is drawing"
        );
    }

    #[test]
    fn following_the_terminal_is_what_turns_the_setting_off_and_on() {
        let directory = tempfile::tempdir().expect("temp dir");
        let following = write(
            directory.path(),
            "config.toml",
            "[preferences]\ntheme_follow_terminal = true\n",
        );
        let files = ThemeFiles {
            user: Some(following),
            project: None,
            terminal: Some(TerminalColours {
                background: acpilot_extensions::Color::from_rgb(0xf6, 0xf2, 0xe8),
                foreground: acpilot_extensions::Color::from_rgb(0x22, 0x22, 0x22),
            }),
        };
        let followed = reload_theme(&files).expect("solved");
        assert_eq!(followed.name(), acpilot_extensions::follow::NAME);
        assert!(!followed.is_dark(), "a light terminal is a light theme");

        // The same file with the key switched off: the named theme is back, in the
        // same session, without a restart.
        write(
            directory.path(),
            "config.toml",
            "[preferences]\ntheme_tui = \"acpilot.paper\"\n",
        );
        assert_eq!(
            reload_theme(&files).expect("a theme").name(),
            "Paper",
            "the named theme, not the terminal's"
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
                terminal: None,
            })
            .is_none()
        );
    }
}
