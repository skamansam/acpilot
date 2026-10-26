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
pub fn run(daemon: &str, token: Option<&str>, theme: Theme) -> io::Result<()> {
    let (updates, inbox) = tokio::sync::mpsc::unbounded_channel();
    let worker = backend::spawn(daemon.to_owned(), token.map(str::to_owned), updates);
    let mut app = App::new(daemon);
    app.set_theme(theme);
    drive(app, worker, inbox)
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
