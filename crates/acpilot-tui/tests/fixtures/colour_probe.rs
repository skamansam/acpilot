//! One launch's worth of the colour query, as a program.
//!
//! Nothing in the product runs this. It is the child half of
//! `tests/terminal_colours.rs`: it takes the path to a `config.toml` as its only
//! argument and does what `acpilot tui` does before it takes the screen over — ask
//! the terminal for its own background and foreground, solve a palette around the
//! pair, and resolve it into the theme the drawing loop would paint with.
//!
//! It is a separate process rather than a function because the query needs three
//! things an in-process test cannot fake: a file descriptor that is really a
//! terminal, read-write, with a program on the other end of the pty to answer. The
//! test supplies all three from the master side and reads the two lines printed
//! here.

use std::process::ExitCode;

use acpilot_tui::{ThemeFiles, reload_theme};
use ratatui::style::Color;

fn main() -> ExitCode {
    let Some(config) = std::env::args().nth(1) else {
        eprintln!("usage: acpilot-terminal-colours <config.toml>");
        return ExitCode::from(2);
    };

    let files = ThemeFiles {
        user: Some(config.into()),
        project: None,
        terminal: None,
    }
    .with_terminal_colours();

    let Some(colours) = files.terminal else {
        eprintln!("the terminal did not report its colours");
        return ExitCode::FAILURE;
    };
    // What the query came back with, before anything is solved from it: the test
    // asserts this against the pair it answered with, so a parsing bug shows up as
    // a wrong colour rather than as a plausible-looking theme.
    println!("terminal {} {}", colours.background, colours.foreground);

    let Some(theme) = reload_theme(&files) else {
        eprintln!("the settings resolved to no theme");
        return ExitCode::FAILURE;
    };
    println!(
        "theme {} {} {} {}",
        theme.name(),
        hex(theme.bg()),
        hex(theme.bg_panel()),
        hex(theme.text())
    );
    ExitCode::SUCCESS
}

/// A token as the `#rrggbb` the theme file would have written it as.
///
/// `Color::Reset` prints as itself rather than as a hex string: a followed palette
/// must not fall back to the terminal's own colours, and the test can see that in
/// one line instead of by matching every token.
fn hex(color: Color) -> String {
    match color {
        Color::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
        other => format!("{other:?}"),
    }
}
