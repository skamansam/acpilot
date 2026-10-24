//! The terminal UI.
//!
//! `acpilot tui` is a client of the daemon, not a second engine: it opens
//! sessions, sends prompts and answers permission requests over the same HTTP and
//! SSE API a browser or the GUI uses. The three modules are the three
//! responsibilities, in order: [`backend`] talks to the daemon, [`app`] holds
//! state and maps keys to requests, and [`ui`] paints.
//!
//! The split is not decoration. It is what lets the app be tested by feeding it
//! events with no daemon and no terminal, and it is the same shape the GUI takes
//! in the desktop phase (P10) — which is the point of one API.

pub mod app;
pub mod backend;
pub mod ui;

pub use app::App;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};

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
/// The terminal is restored on the way out, including when a draw fails: a TUI
/// that leaves the cursor hidden and the screen in raw mode is worse than the
/// crash that caused it.
pub fn run(daemon: &str, token: Option<&str>) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, daemon, token.map(str::to_owned));
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    daemon: &str,
    token: Option<String>,
) -> io::Result<()> {
    let (updates, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    let worker = backend::spawn(daemon.to_owned(), token, updates);
    let mut app = App::new(daemon);

    loop {
        while let Ok(update) = inbox.try_recv() {
            app.apply(update);
        }

        terminal.draw(|frame| ui::draw(frame, &app))?;

        if event::poll(FRAME)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && let Some(request) = app.handle_key(key)
        {
            worker.send(request);
        }

        if app.should_quit {
            return Ok(());
        }
    }
}
