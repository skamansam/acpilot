//! A terminal that answers, on the other end of a real pty.
//!
//! [`acpilot_tui::query_terminal_colours`] is one escape round-trip: `OSC 10` for
//! the foreground, `OSC 11` for the background, then `DA1` so a terminal that
//! ignores the first two is recognised instead of waited on. That is exactly the
//! shape of thing a test can accidentally mock away — an in-process test can only
//! assert what the solver does with a pair somebody handed it, which leaves the
//! query, the reply parsing and the `theme_follow_terminal` path unexercised.
//!
//! So this one is a real round-trip. The child is [`PROBE`], spawned on the slave
//! side of a pty; this process is the terminal on the master side and answers in
//! the order it was asked. What comes back is the theme the child resolved, so
//! query → parse → solve → theme is covered in one run, in the same order a launch
//! goes through it.
//!
//! Unix only: the probe relies on the pty's slave being its stdout, which is what
//! `terminal-trx` borrows rather than opening `/dev/tty`. Windows would need a
//! ConPTY harness of its own, and the query is not a thing CI's Windows runner has
//! a terminal for either.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use acpilot_extensions::{Color, ThemeAppearance, UiToken};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// The TUI's own launch path, as a program: `tests/fixtures/colour_probe.rs`.
const PROBE: &str = env!("CARGO_BIN_EXE_acpilot-terminal-colours");

/// `DA1` — the last of the three queries, so seeing it means the whole ask is out.
const QUERY_SENT: &[u8] = b"\x1b[c";

/// Long enough for a loaded runner, short enough that a disagreement fails instead
/// of hanging the suite.
const WATCHDOG: Duration = Duration::from_secs(20);

#[test]
fn a_terminal_that_answers_is_the_palette_the_client_paints_with() {
    // Two ends of the ramp, because the polarity is read from the pair: the same
    // code has to give a dark theme on a black terminal and a light one on a cream
    // terminal, and it has to get that from what the terminal said, not from a
    // setting.
    for (label, background, foreground, appearance) in [
        (
            "a black terminal",
            (0x00, 0x00, 0x00),
            (0xff, 0xff, 0xff),
            ThemeAppearance::Dark,
        ),
        (
            "a cream terminal",
            (0xfd, 0xf6, 0xe3),
            (0x59, 0x64, 0x6a),
            ThemeAppearance::Light,
        ),
    ] {
        let output = launch(background, foreground);

        // What the query came back with, before anything is solved: a channel lost
        // in parsing would show up here as the wrong colour rather than as a theme
        // that merely looks a shade off.
        let reported = format!("terminal {} {}", hex(background), hex(foreground));
        assert!(
            output.contains(&reported),
            "{label}: expected `{reported}` in what the probe printed:\n{output}"
        );

        // And what it then painted with is the solver's answer for that same pair,
        // token for token. Comparing against the function the TUI calls is the
        // point: the test is about the wiring around the solver, not a second copy
        // of its arithmetic.
        let solved = acpilot_extensions::follow::solve(rgb(background), rgb(foreground))
            .expect("the pair solves a palette");
        assert_eq!(
            solved.appearance, appearance,
            "{label}: the polarity read from the terminal's own pair"
        );
        assert_eq!(solved.name, acpilot_extensions::follow::NAME);

        let painted = format!(
            "theme {} {} {} {}",
            solved.name,
            token(&solved, UiToken::Bg),
            token(&solved, UiToken::BgPanel),
            token(&solved, UiToken::Text),
        );
        assert!(
            output.contains(&painted),
            "{label}: expected `{painted}` in what the probe printed:\n{output}"
        );
        assert!(
            !output.contains("Reset"),
            "{label}: a followed palette draws with real colours, not the terminal's \
             own:\n{output}"
        );
    }
}

/// Run the probe on a pty, answer its queries, and return everything it printed.
fn launch(background: (u8, u8, u8), foreground: (u8, u8, u8)) -> String {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("a pty");

    let directory = tempfile::tempdir().expect("a settings directory");
    let settings = directory.path().join("config.toml");
    fs::write(&settings, "[preferences]\ntheme_follow_terminal = true\n")
        .expect("the settings file the probe reads");

    let mut command = CommandBuilder::new(PROBE);
    command.arg(&settings);
    // `TERM` is how the query decides a terminal can be asked at all: unset or
    // `dumb` means "send no escapes", which would make this test pass by not
    // testing anything.
    command.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(command).expect("the probe starts");
    // The parent's copy of the slave, or the master never sees the child exit and
    // the read below waits on a terminal that is still half open.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("a reader on the master side");
    let mut writer = pair
        .master
        .take_writer()
        .expect("a writer on the master side");

    // Read on a thread of its own: a pty whose output nobody drains blocks the
    // child mid-write, which turns a wrong assertion into a hung test.
    let (sender, inbox) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 1024];
        while let Ok(read) = reader.read(&mut chunk) {
            if read == 0 || sender.send(chunk[..read].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut seen = Vec::new();
    let mut answered = false;
    let deadline = Instant::now() + WATCHDOG;
    // The probe ends its output with `theme …`, or says on stderr why it could
    // not. Either is a reason to stop reading and look at what it said.
    while !contains(&seen, b"theme ") && !contains(&seen, b"did not report") {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "the probe went quiet; it printed:\n{}",
            quoted(&seen)
        );
        let chunk = inbox
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("the probe went quiet; it printed:\n{}", quoted(&seen)));
        seen.extend_from_slice(&chunk);
        if !answered && contains(&seen, QUERY_SENT) {
            answer(&mut writer, background, foreground);
            answered = true;
        }
    }
    assert!(
        answered,
        "the probe never asked for the colours; it printed:\n{}",
        quoted(&seen)
    );

    let status = child.wait().expect("the probe exits");
    assert!(
        status.success(),
        "the probe failed ({}):\n{}",
        status.exit_code(),
        quoted(&seen)
    );
    String::from_utf8_lossy(&seen).into_owned()
}

/// Answer the three queries a launch sends, in the order it sends them.
///
/// `terminal-colorsaurus` writes `OSC 10`, `OSC 11`, then `DA1`, and reads the
/// answers positionally. Replying out of order is what a terminal that does not
/// understand the colour queries looks like, and the crate refuses those — so the
/// order here is part of what makes this a terminal rather than a stream of bytes.
fn answer(writer: &mut dyn Write, background: (u8, u8, u8), foreground: (u8, u8, u8)) {
    let reply = format!(
        "\x1b]10;rgb:{}\x1b\\\x1b]11;rgb:{}\x1b\\\x1b[?62;22c",
        xterm_rgb(foreground),
        xterm_rgb(background),
    );
    writer
        .write_all(reply.as_bytes())
        .expect("the fake terminal answers");
    writer.flush().expect("and flushes its answer");
}

/// A colour as `rgb:` takes it: four hex digits per channel.
///
/// Four and not two, because the crate scales sixteen bits down to eight. `ff`
/// would round-trip through a different denominator and land a shade off the byte
/// the fake terminal meant; repeating the byte makes `scale_to_8bit` exact.
fn xterm_rgb((red, green, blue): (u8, u8, u8)) -> String {
    format!("{red:02x}{red:02x}/{green:02x}{green:02x}/{blue:02x}{blue:02x}")
}

/// The pair as the theme file and the probe's first line spell it.
fn hex((red, green, blue): (u8, u8, u8)) -> String {
    rgb((red, green, blue)).to_string()
}

fn rgb((red, green, blue): (u8, u8, u8)) -> Color {
    Color::from_rgb(red, green, blue)
}

/// One token of a resolved palette, as the probe prints it.
fn token(theme: &acpilot_extensions::ResolvedTheme, which: UiToken) -> String {
    theme
        .palette
        .ui(which)
        .unwrap_or_else(|| panic!("{which} is unset"))
        .to_string()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The pty's bytes with their escapes visible, so a failing assertion shows what
/// the terminal and the probe actually said to each other.
fn quoted(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).escape_debug().to_string()
}
