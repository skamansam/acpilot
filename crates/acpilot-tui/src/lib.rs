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
pub mod keymap;
pub mod lab;
pub mod layout;
pub mod markdown;
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
use crate::keymap::Keymap;

/// How long the loop waits for a key before redrawing.
///
/// Redrawing on a timer rather than only on input is what makes daemon updates
/// appear while nobody is typing. 120 ms is under the threshold where a counter
/// looks frozen and far above a busy loop.
const FRAME: Duration = Duration::from_millis(120);

/// How often a drag resting at a pane's edge scrolls.
///
/// A drag only arrives while the pointer moves, so a pointer held still against
/// an edge sends nothing at all. The scroll has to be the loop's own clock. 30 ms
/// is a line per tick at about 33 lines a second, which reads as one continuous
/// scroll without being a busy loop. The clock is only this fast while there is
/// more view in the direction the pointer is pulling.
const AUTOSCROLL: Duration = Duration::from_millis(30);

/// How long the terminal is given to answer the colour query.
///
/// One escape round-trip before the screen is taken over. It is long enough for
/// a terminal at the end of an SSH connection. It is short enough that a
/// terminal which will not answer costs a quarter of a second, not a second.
const QUERY_TIMEOUT: Duration = Duration::from_millis(250);

/// The file the divider widths are remembered in, under the data directory.
const LAYOUT_FILE: &str = "tui-layout.toml";

/// The file the keys are read from, under the config directory beside
/// `config.toml`.
///
/// A setting rather than a memory of the client's own: a person writes it, and it
/// belongs where the other settings are.
const KEYMAP_FILE: &str = "keymap.toml";

/// Run the TUI against the daemon at `daemon` until the user leaves.
///
/// `token` is passed through to the client and is what a TCP daemon requires. The
/// caller resolves it, because the caller is the one with arguments and a home
/// directory. A socket endpoint ignores it.
///
/// `theme` is resolved by the caller too: the settings it comes from are the
/// project's. This crate is given the answer rather than a path. It is the
/// fallback rather than the last word: `theme_follow_terminal` solves a palette
/// from the terminal itself. A terminal that will not answer leaves the
/// caller's theme on screen.
///
/// `mouse` says whether the terminal's mouse reporting is turned on. Off is a
/// real setting rather than a debug switch. Mouse reporting costs the terminal's
/// own click-drag selection. Some people would rather keep that than have a menu
/// bar answer a pointer.
///
/// `initial` is the file a launch named, relative to the workspace root. It is read
/// through the same request the tree's `Enter` sends. A command that named a file
/// opens it as an editor rather than as a session list. The daemon reads it, because
/// the client may not be on the machine the file is.
///
/// The terminal is restored on the way out, including when a draw fails. A TUI
/// that leaves the cursor hidden and the screen in raw mode is worse than the
/// crash that caused it.
pub fn run(
    daemon: &str,
    token: Option<&str>,
    theme: Theme,
    files: ThemeFiles,
    initial: Option<&str>,
    mouse: bool,
) -> io::Result<()> {
    // Asked before the screen is taken over and before the loop owns the terminal's
    // input. From then on, nothing in this process can read a reply but the event
    // reader. A second query is not a thing that can be added later.
    let files = files.with_terminal_colours();
    let theme = match reload_theme(&files) {
        Some(theme) => theme,
        None => {
            if files.terminal.is_none() && follows_terminal(&files) {
                // Silence here would be a setting that appears to do nothing: the
                // person asked for the terminal's own colours and got the named
                // theme. The reason is the terminal, not the file.
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
    let keymap_updates = updates.clone();
    let grammar_updates = updates.clone();
    // A profile's `keymap.toml` is the one the launch should use. The global file
    // is the fallback for a profile that has not written one. The daemon's
    // preferences live in `profile.toml`. A key table can be large and has a
    // reader of its own, so it stays a file rather than a `prefs` field.
    let keymap_path = files
        .profile
        .as_ref()
        .map(|directory| directory.join(KEYMAP_FILE))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| acpilot_profile::config_dir().join(KEYMAP_FILE));
    // A pack's keys sit above the preset and below the person's own file. Read
    // once at launch and again on every save of that file or of the profile's
    // activation list. Activating or dropping a pack is visible without a
    // restart just like a theme.
    let keymap_layers = match extension_keymaps(files.profile.as_deref()) {
        Ok(layers) => layers,
        Err(error) => {
            eprintln!("acpilot tui: extension keys not applied · {error}");
            Vec::new()
        }
    };
    let grammars = match extension_grammars(files.profile.as_deref()) {
        Ok(grammars) => grammars,
        Err(error) => {
            eprintln!("acpilot tui: extension grammars not applied · {error}");
            Vec::new()
        }
    };
    let snippets = match extension_snippets(files.profile.as_deref()) {
        Ok(snippets) => snippets,
        Err(error) => {
            eprintln!("acpilot tui: extension snippets not applied · {error}");
            Vec::new()
        }
    };
    let profile_dir = files.profile.clone();
    let worker = backend::spawn(
        daemon.to_owned(),
        token.map(str::to_owned),
        files.terminal,
        updates,
    );
    watch_theme(files, changes);
    // A launch is an editor: the assistant's panes are the answer to choosing an
    // agent, not the surface the command opens on.
    let mut app = App::editor_first(daemon);
    app.set_snippets(snippets);
    for grammar in grammars {
        if !app.grammars.register_wasm(
            &grammar.name,
            &grammar.extensions,
            grammar.wasm,
            grammar.highlights,
            grammar.indents,
        ) {
            eprintln!(
                "acpilot tui: grammar {} not applied · name or extension already in use",
                grammar.name
            );
        }
    }
    // The width a divider was dragged to is remembered per machine, in the data
    // directory rather than in `config.toml`. It is the client's memory of itself,
    // not a setting a person writes.
    app.remember_layout(acpilot_profile::data_dir().join(LAYOUT_FILE));
    // Before the screen is taken over: a key file that does not parse is a sentence
    // for the shell. The client opens on the preset it describes.
    if let Some(error) = app.remember_keymap(keymap_path.clone(), &keymap_layers) {
        eprintln!("acpilot tui: {error} · starting on the default keys");
    }
    watch_keymap(keymap_path, profile_dir.clone(), keymap_updates);
    watch_grammars(profile_dir, grammar_updates);
    app.set_theme(theme);
    // The answer arrives as `Update::File`, which puts the keyboard in the editor
    // and says which file was opened. Until it does, the window is already an
    // editor: a launch must not wait for a pane it has decided not to show.
    if let Some(path) = initial {
        worker.send(backend::Request::OpenFile(path.to_owned()));
    }
    drive(app, worker, inbox, mouse)
}

/// The settings files a running client re-reads when they change.
///
/// Paths rather than a value: a theme edited while the client is open should be
/// the theme on screen. That means knowing what to watch. `None` is a surface
/// with no such file. The design pass reads nothing from disk, which is why the
/// default watches nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeFiles {
    /// The user's `config.toml`.
    pub user: Option<std::path::PathBuf>,
    /// The profile directory whose `config.toml` and `profile.toml` are layers.
    pub profile: Option<std::path::PathBuf>,
    /// The project's `.acpilot/config.toml`, when the launch is in a project.
    pub project: Option<std::path::PathBuf>,
    /// What the terminal said its own colours are, when it answered.
    pub terminal: Option<TerminalColours>,
}

/// A pair of colours the terminal reported about itself.
///
/// The pair, not a theme: [`acpilot_extensions::follow`] is what turns it into a
/// complete token set. It is the same solver whether the answer came from a
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
    /// Asked once, before the drawing loop runs: the reply arrives on the same
    /// stream the loop will read keys from. Asking later would mean two readers
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
/// [`None`] when the settings do not load or name a theme that is not installed.
/// A client that is already drawing keeps the colours it has. It does not go
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
/// twice would be two answers to one question. The daemon's answer reaches a
/// terminal that cannot see the daemon's files.
#[must_use]
pub fn theme_for(
    preferences: &acpilot_proto::profile::Preferences,
    terminal: Option<TerminalColours>,
) -> Option<Theme> {
    use acpilot_proto::profile::ThemeSurface;

    if preferences.theme_follow_terminal {
        // No theme is named: the pair the terminal reported is solved into one.
        // A terminal that never answered leaves the theme already on screen. A
        // settings edit is not a reason to lose the colours a person is reading.
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
    let profile = files
        .profile
        .as_deref()
        .and_then(|directory| acpilot_profile::Profile::open(directory).ok());
    acpilot_profile::settings::Settings::for_launch(
        files.user.as_deref(),
        files.project.as_deref(),
        profile.as_ref(),
    )
    .ok()
}

/// Whether the settings ask for the terminal's own palette.
fn follows_terminal(files: &ThemeFiles) -> bool {
    settings(files).is_some_and(|settings| settings.preferences.theme_follow_terminal)
}

/// What the terminal says about its own colours, or [`None`] when it will not say.
///
/// One OSC round-trip (`11` for the background, `10` for the foreground) with a
/// timeout. `terminal-colorsaurus` asks, so raw mode is restored on the way out.
/// A terminal that does not answer is recognised rather than waited on. The cost
/// is paid once per launch and only in a terminal. No window, no daemon and no CI
/// job runs this.
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
/// announces the same saves on its own stream. Two watchers would be two
/// debounce policies and two answers to an editor that saves by rename. A client
/// with no settings to watch, such as the design pass or the demo, has no thread.
fn watch_theme(files: ThemeFiles, updates: tokio::sync::mpsc::UnboundedSender<Update>) {
    let profile = files
        .profile
        .as_deref()
        .and_then(|directory| acpilot_profile::Profile::open(directory).ok());
    let paths = acpilot_profile::settings::watch_paths(
        files.user.as_deref(),
        files.project.as_deref(),
        profile.as_ref(),
    );
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
                // The client is gone. So is the reason to watch.
                return;
            }
        }
    });
}

/// The keymap layers this profile's activated extensions contribute.
///
/// Sorted by extension id, so two packs that move one key resolve the same way on
/// every machine rather than in activation-file order. A profile with nothing
/// activated reads no store and answers with no layers.
///
/// # Errors
///
/// The activation file does not parse, or the store cannot resolve it. A caller
/// that wants the preset regardless logs this and carries on.
pub fn extension_keymaps(
    profile: Option<&std::path::Path>,
) -> Result<Vec<acpilot_extensions::KeymapLayer>, String> {
    let Some(directory) = profile else {
        return Ok(Vec::new());
    };
    let activation = acpilot_extensions::Activation::load(
        &directory.join(acpilot_extensions::activation::ACTIVATION_FILE),
    )
    .map_err(|error| error.to_string())?;
    if activation.active.is_empty() {
        return Ok(Vec::new());
    }
    let store = acpilot_extensions::Store::open_default().map_err(|error| error.to_string())?;
    let active = store.resolve_active(&activation.active).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    let mut layers: Vec<acpilot_extensions::KeymapLayer> = active
        .extensions
        .iter()
        .flat_map(|extension| extension.contributions.iter())
        .filter_map(|contribution| match contribution {
            acpilot_extensions::Contribution::Keymap(layer) => Some(layer.clone()),
            _ => None,
        })
        .collect();
    layers.sort_by(|a, b| a.extension.cmp(&b.extension));
    Ok(layers)
}

/// The grammars this profile's activated extensions contribute.
///
/// Sorted by extension id, so two packs that claim one language resolve the same
/// way on every machine. A profile with nothing activated reads no store.
///
/// # Errors
///
/// The activation file does not parse, or the store cannot resolve it.
pub fn extension_grammars(
    profile: Option<&std::path::Path>,
) -> Result<Vec<acpilot_extensions::GrammarAsset>, String> {
    let Some(directory) = profile else {
        return Ok(Vec::new());
    };
    let activation = acpilot_extensions::Activation::load(
        &directory.join(acpilot_extensions::activation::ACTIVATION_FILE),
    )
    .map_err(|error| error.to_string())?;
    if activation.active.is_empty() {
        return Ok(Vec::new());
    }
    let store = acpilot_extensions::Store::open_default().map_err(|error| error.to_string())?;
    let active = store.resolve_active(&activation.active).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    let mut grammars: Vec<acpilot_extensions::GrammarAsset> = active
        .extensions
        .iter()
        .flat_map(|extension| extension.contributions.iter())
        .filter_map(|contribution| match contribution {
            acpilot_extensions::Contribution::Grammar(grammar) => Some(grammar.clone()),
            _ => None,
        })
        .collect();
    grammars.sort_by(|a, b| a.extension.cmp(&b.extension));
    Ok(grammars)
}

/// The snippets this profile's activated extensions contribute.
///
/// Sorted by extension id, so two packs that claim one language resolve the same
/// way on every machine. A profile with nothing activated reads no store.
///
/// # Errors
///
/// The activation file does not parse, or the store cannot resolve it.
pub fn extension_snippets(
    profile: Option<&std::path::Path>,
) -> Result<Vec<acpilot_extensions::SnippetSet>, String> {
    let Some(directory) = profile else {
        return Ok(Vec::new());
    };
    let activation = acpilot_extensions::Activation::load(
        &directory.join(acpilot_extensions::activation::ACTIVATION_FILE),
    )
    .map_err(|error| error.to_string())?;
    if activation.active.is_empty() {
        return Ok(Vec::new());
    }
    let store = acpilot_extensions::Store::open_default().map_err(|error| error.to_string())?;
    let active = store.resolve_active(&activation.active).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    let mut snippets: Vec<acpilot_extensions::SnippetSet> = active
        .extensions
        .iter()
        .flat_map(|extension| extension.contributions.iter())
        .filter_map(|contribution| match contribution {
            acpilot_extensions::Contribution::Snippets(snippets) => Some(snippets.clone()),
            _ => None,
        })
        .collect();
    snippets.sort_by(|a, b| a.extension.cmp(&b.extension));
    Ok(snippets)
}

/// Watch the activation list and send the grammars it names.
///
/// The key watcher also reads this file because a pack's keys change with it.
/// This watcher sends the grammar set, so a deactivated pack stops claiming files.
fn watch_grammars(
    profile: Option<std::path::PathBuf>,
    updates: tokio::sync::mpsc::UnboundedSender<Update>,
) {
    let Some(directory) = profile.as_ref() else {
        return;
    };
    let path = directory.join(acpilot_extensions::activation::ACTIVATION_FILE);
    let Some(changes) = acpilot_profile::watch::changes([path]) else {
        return;
    };
    std::thread::spawn(move || {
        while changes.recv().is_ok() {
            match extension_grammars(profile.as_deref()) {
                Ok(grammars) => {
                    if updates.send(Update::Grammars(grammars)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    if updates
                        .send(Update::Status(format!(
                            "extension grammars not applied · {error}"
                        )))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
}

/// Watch the key table and send a new one when it parses.
///
/// A keymap is a domain file rather than a preference, so it is not in the
/// daemon's `prefs` frame. The client can read it wherever it is running, and a
/// save should take effect without a restart just like a theme does. A file that
/// does not parse leaves the table in force and says so.
fn watch_keymap(
    path: std::path::PathBuf,
    profile: Option<std::path::PathBuf>,
    updates: tokio::sync::mpsc::UnboundedSender<Update>,
) {
    let paths = keymap_watch_paths(path.clone(), profile.as_deref());
    let Some(changes) = acpilot_profile::watch::changes(paths) else {
        return;
    };
    std::thread::spawn(move || {
        while changes.recv().is_ok() {
            // Re-read the activation list too. A pack activated or deactivated
            // while the client is open changes the keys without a restart. The
            // save that says so is a save of a file this thread watches.
            let layers = match extension_keymaps(profile.as_deref()) {
                Ok(layers) => layers,
                Err(error) => {
                    if updates
                        .send(Update::Status(format!(
                            "extension keys not applied · {error}"
                        )))
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
            };
            match Keymap::load_with(&path, &layers) {
                Ok(keymap) => {
                    if updates.send(Update::Keymap(keymap)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    if updates
                        .send(Update::Status(format!("keymap not applied · {error}")))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
}

/// The files whose saves change the key table.
///
/// The person's own `keymap.toml` is one. The profile's activation list is the
/// other, because a pack's layer is part of the answer. Activating or dropping a
/// pack changes it. Both are named even when they do not exist yet. The watcher
/// arms at the nearest directory that does and re-arms when the file appears, so
/// the first activation is still a signal.
fn keymap_watch_paths(
    path: std::path::PathBuf,
    profile: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let mut paths = vec![path];
    if let Some(directory) = profile {
        paths.push(directory.join(acpilot_extensions::activation::ACTIVATION_FILE));
    }
    paths
}

/// Run the design pass: the seeded world from `acpilot-demo`, drawn by this UI.
///
/// No daemon is contacted and nothing is read from disk, so the screen is the same
/// on any machine. That is what makes it something to point at in a review. The
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
/// Mouse reporting is turned on for the same span when it is wanted. The menu
/// bar, the panes and the two dividers take the pointer. That costs the
/// terminal's own click-drag selection. Most terminals keep it on Shift-drag, and
/// copy mode is the built-in answer. `--no-mouse` is there for someone who would
/// rather have the terminal's.
fn drive(
    app: App,
    worker: Worker,
    mut inbox: UnboundedReceiver<Update>,
    mouse: bool,
) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    // A terminal that will not report the mouse is still a terminal: the keys and
    // the divider chords work. A refused escape is not a reason to fail a launch.
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
        // the pointer moving a cell. It is asked for here rather than waited
        // for as an event.
        app.autoscroll();
        // Work that is deferred rather than done on the key that caused it: the outline
        // re-parses a changed buffer once a frame. This loop's clock is the
        // debounce. The same clock decides how long a buffer must be still before
        // its problems are asked about.
        app.tick();

        // `draw` reads the layout and hands back the panes' inner sizes. A
        // program and the screen agree on the width, and a copy cursor and the
        // transcript agree on how many lines it can scroll. Without the loop
        // repeating the layout arithmetic. It takes the app mutably because the
        // editor pane measures itself against the cells it was given. It paints
        // from the parse that measurement asked for. Every other pane still only
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

        // A key can ask for more than one thing. The hidden-files toggle re-lists
        // the root and every open directory. The queue is how it says so without
        // a second return value for one rare key.
        for request in app.take_pending() {
            worker.send(request);
        }

        // A copy leaves as OSC 52 here rather than from the app: the escape goes
        // to the terminal a person is looking at. The drawing loop is the only
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
            profile: None,
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
            profile: None,
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
                profile: None,
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
            profile: None,
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
                profile: None,
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
            profile: None,
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
    fn saving_the_activation_list_changes_the_key_table() {
        // Activating or dropping a pack changes which layers apply. Watching only
        // `keymap.toml` made that wait for a save of a file nobody had touched.
        let directory = tempfile::tempdir().expect("temp dir");
        let keymap = write(directory.path(), KEYMAP_FILE, "[keys]\n");
        let changes =
            acpilot_profile::watch::changes(keymap_watch_paths(keymap, Some(directory.path())))
                .expect("the profile directory is watchable");

        write(
            directory.path(),
            acpilot_extensions::activation::ACTIVATION_FILE,
            "schema_version = 1\nactive = [\"acpilot.test-pack\"]\n",
        );
        assert!(
            changes.recv_timeout(Duration::from_secs(10)).is_ok(),
            "a saved activation list is a key-table change"
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
                profile: None,
                project: None,
                terminal: None,
            })
            .is_none()
        );
    }
}
