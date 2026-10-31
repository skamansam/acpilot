//! The screens the documentation site shows, captured from the UIs themselves.
//!
//! A screenshot of a terminal UI is a claim that somebody ran it once. This crate
//! makes the claim checkable in a different way: it builds the seeded world
//! (`acpilot-demo`), drives [`acpilot_tui::App`] with the keys a person would press,
//! and paints each state through the real [`acpilot_tui::ui::draw`] into an
//! off-screen screen. What the site draws is therefore the client's own output, and
//! a layout change shows up as a diff in `site/src/data/screens.json` rather than as
//! a page that has quietly stopped describing the product.
//!
//! # Tokens, not colours
//!
//! A frame captured in night flight's colours could not be recoloured by a reader
//! who wants to see the light theme, or the palette their own terminal would solve
//! to. So the capture paints with a *sentinel* palette — one distinct synthetic
//! colour per theme token — and records which token each cell resolved to. The page
//! maps tokens to colours at paint time, which is also how the product works: a
//! surface never owns a palette, it asks the theme.
//!
//! A cell whose colour is not a sentinel — a program's own colours in the terminal
//! pane, an ANSI index — keeps its literal value. Those are the program's, not the
//! theme's, and recolouring them would be a lie about what the pane shows.
//!
//! # What else is captured
//!
//! [`desktop`] is the shell's own tables: its keymap and the rows its command
//! palette builds, neither of which needs gpui. The window itself is drawn by gpui
//! and is not captured here; the site composes a representation out of these tables
//! and says so.

use std::cell::Cell as OnceCell;

use acpilot_extensions::theme::{Color, Palette, SyntaxToken, UiToken};
use acpilot_tui::theme::{Theme, from_palette};
use acpilot_tui::ui;
use acpilot_tui::{App, backend::Update};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color as Ratatui, Modifier, Style as RatatuiStyle};
use serde::Serialize;
use serde_json::{Value, json};

/// The document the site reads its screens from, relative to the repository root.
pub const SCREENS: &str = "site/src/data/screens.json";

/// The document the site reads the desktop shell's tables from.
pub const DESKTOP: &str = "site/src/data/desktop.json";

/// The screen the capture paints into.
///
/// 120 columns is the width the design pass was drawn at, and the file tree needs
/// it: below 100 the tree folds away, which is a different screen from the one being
/// shown off.
pub const COLUMNS: u16 = 120;

/// The rows, including the menu bar and the two footer lines.
pub const ROWS: u16 = 34;

/// Sentinel channels. A token's index goes in green; red and blue are the marker.
const MARKER_RED: u8 = 0xa5;
const MARKER_BLUE: u8 = 0x5a;

/// A theme in which every token is a unique colour nobody would draw by accident.
///
/// Returned with the token names in index order, so a captured colour can be read
/// back as the token it stood for.
fn sentinel() -> (Theme, Vec<&'static str>) {
    let mut names = Vec::new();
    let mut palette = Palette::default();
    for token in UiToken::ALL {
        names.push(token.as_str());
        palette.ui.insert(
            token,
            Color::from_rgb(MARKER_RED, marked(names.len()), MARKER_BLUE),
        );
    }
    for token in SyntaxToken::ALL {
        names.push(token.as_str());
        palette.syntax.insert(
            token,
            Color::from_rgb(MARKER_RED, marked(names.len()), MARKER_BLUE),
        );
    }
    (from_palette(&palette), names)
}

/// The green channel for the token at `count`, one-based.
///
/// Never zero: a sentinel of `(0xa5, 0, 0x5a)` would be indistinguishable from a
/// colour a program painted by hand, and "is this a token?" would answer yes for it.
fn marked(count: usize) -> u8 {
    u8::try_from(count).expect("fewer than 255 tokens")
}

/// A stretch of one row with one style.
#[derive(Debug, Serialize)]
struct Run {
    column: u16,
    text: String,
    style: usize,
}

/// One entry of the style table.
///
/// `fg` and `bg` are `token:<name>` for a theme token, `#rrggbb` for a colour a
/// program chose, `ansi:<n>` for an indexed one, and `default` for the terminal's
/// own — which a page paints as the theme's background or body text, the way a
/// terminal does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Style {
    fg: String,
    bg: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    modifiers: Vec<&'static str>,
}

/// The sentinel theme, the token names it encodes, and the style table built while
/// frames are captured.
struct Capture {
    theme: Theme,
    tokens: Vec<&'static str>,
    styles: Vec<Style>,
}

impl Capture {
    fn new() -> Self {
        let (theme, tokens) = sentinel();
        Self {
            theme,
            tokens,
            styles: Vec::new(),
        }
    }

    /// The token, literal colour or terminal default behind a `ratatui` colour.
    fn describe(&self, colour: Ratatui) -> String {
        match colour {
            Ratatui::Reset => "default".to_owned(),
            Ratatui::Rgb(MARKER_RED, green, MARKER_BLUE) => self
                .tokens
                .get(usize::from(green).wrapping_sub(1))
                .map_or_else(
                    || format!("#{MARKER_RED:02x}{green:02x}{MARKER_BLUE:02x}"),
                    |token| format!("token:{token}"),
                ),
            Ratatui::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
            Ratatui::Indexed(index) => format!("ansi:{index}"),
            named => {
                let (red, green, blue) = named_rgb(named);
                format!("#{red:02x}{green:02x}{blue:02x}")
            }
        }
    }

    /// The style's index in the table, adding it if it is new.
    fn style(&mut self, style: RatatuiStyle) -> usize {
        let described = Style {
            fg: self.describe(style.fg.unwrap_or(Ratatui::Reset)),
            bg: self.describe(style.bg.unwrap_or(Ratatui::Reset)),
            modifiers: modifiers(style.add_modifier),
        };
        match self.styles.iter().position(|known| *known == described) {
            Some(index) => index,
            None => {
                self.styles.push(described);
                self.styles.len() - 1
            }
        }
    }

    /// Every row of `buffer`, as runs of one style each.
    ///
    /// A run carries the column it starts at rather than being a bare string: a
    /// blank cell is a gap, and a wide glyph covers two columns while being one
    /// character, so a page that laid runs end to end would drift.
    fn rows(&mut self, buffer: &Buffer) -> Vec<Vec<Run>> {
        let (width, height) = (buffer.area.width, buffer.area.height);
        let mut rows = Vec::new();
        for y in 0..height {
            let mut runs: Vec<Run> = Vec::new();
            let mut previous: Option<u16> = None;
            for x in 0..width {
                let Some(cell) = buffer.cell((x, y)) else {
                    previous = None;
                    continue;
                };
                // A wide glyph's trailing column is empty: its text is already in
                // the run, and no run may span it, or the columns after it slide.
                if cell.symbol().is_empty() {
                    previous = None;
                    continue;
                }
                let style = self.style(cell.style());
                let joinable = previous == Some(x.saturating_sub(1))
                    && runs.last().is_some_and(|run| run.style == style);
                if joinable {
                    if let Some(run) = runs.last_mut() {
                        run.text.push_str(cell.symbol());
                    }
                } else {
                    runs.push(Run {
                        column: x,
                        text: cell.symbol().to_owned(),
                        style,
                    });
                }
                previous = Some(x);
            }
            rows.push(
                runs.into_iter()
                    .filter(|run| !is_blank(run, &self.styles))
                    .collect(),
            );
        }
        rows
    }
}

/// Whether a run is a gap that only carries the background.
///
/// A selection highlight is spaces with a panel behind them, and dropping it would
/// drop the highlight; spaces on the theme's own background are nothing.
fn is_blank(run: &Run, styles: &[Style]) -> bool {
    run.text.trim().is_empty()
        && styles
            .get(run.style)
            .is_some_and(|style| style.bg == "default")
}

fn modifiers(modifier: Modifier) -> Vec<&'static str> {
    [
        (Modifier::BOLD, "bold"),
        (Modifier::DIM, "dim"),
        (Modifier::ITALIC, "italic"),
        (Modifier::UNDERLINED, "underline"),
        (Modifier::REVERSED, "reverse"),
        (Modifier::CROSSED_OUT, "strike"),
    ]
    .into_iter()
    .filter(|(flag, _)| modifier.contains(*flag))
    .map(|(_, name)| name)
    .collect()
}

/// The named terminal colours, as the defaults a terminal would draw them in.
///
/// The painter asks for none of these today — the tokens are truecolour — but a
/// build tool that panicked on one would be a worse neighbour than one that draws it.
fn named_rgb(colour: Ratatui) -> (u8, u8, u8) {
    match colour {
        Ratatui::Black => (0x00, 0x00, 0x00),
        Ratatui::Red => (0xcd, 0x31, 0x31),
        Ratatui::Green => (0x0d, 0xbc, 0x79),
        Ratatui::Yellow => (0xe5, 0xe5, 0x10),
        Ratatui::Blue => (0x24, 0x72, 0xc8),
        Ratatui::Magenta => (0xbc, 0x3f, 0xbc),
        Ratatui::Cyan => (0x11, 0xa8, 0xcd),
        Ratatui::Gray => (0xe5, 0xe5, 0xe5),
        Ratatui::DarkGray => (0x66, 0x66, 0x66),
        Ratatui::LightRed => (0xf1, 0x4c, 0x4c),
        Ratatui::LightGreen => (0x23, 0xd1, 0x8b),
        Ratatui::LightYellow => (0xf5, 0xf5, 0x43),
        Ratatui::LightBlue => (0x3b, 0x8e, 0xea),
        Ratatui::LightMagenta => (0xd6, 0x70, 0xd6),
        Ratatui::LightCyan => (0x29, 0xb8, 0xdb),
        Ratatui::White => (0xff, 0xff, 0xff),
        _ => (0x00, 0x00, 0x00),
    }
}

fn press(app: &mut App, event: KeyEvent) {
    // The request a press answers with goes to a daemon or a worker; the design
    // pass has neither, and what a capture wants is the state the panes draw in.
    let _ = app.handle_key(event);
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(letter: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL)
}

/// Paint `app` and append the frame to `frames`.
fn capture_into(
    app: &mut App,
    capture: &mut Capture,
    frames: &mut Vec<Value>,
    id: &str,
    title: &str,
    caption: &str,
    keys: &str,
) {
    let mut terminal = Terminal::new(TestBackend::new(COLUMNS, ROWS))
        .expect("an off-screen terminal is always constructible");
    // The viewports are what the event loop feeds back into the app after a draw,
    // and the panes that scroll need them: a transcript captured before its size is
    // known would not be the screen a person sees.
    let viewports = OnceCell::new(None);
    terminal
        .draw(|frame| viewports.set(Some(ui::draw(frame, app))))
        .expect("the off-screen backend cannot fail");
    if let Some(viewports) = viewports.take() {
        app.set_viewports(viewports);
    }
    let rows = capture.rows(terminal.backend().buffer());
    frames.push(json!({
        "id": id,
        "title": title,
        "caption": caption,
        "keys": keys,
        "rows": rows,
    }));
}

/// The screens, in the order a visitor walks them.
///
/// One session, one filmstrip: each frame is the state after the keys above it were
/// pressed, so the last frame is a screen somebody could reach by hand.
pub fn screens() -> String {
    let demo = acpilot_demo::demo();
    let mut app = App::demo(&demo);
    let mut capture = Capture::new();
    app.set_theme(capture.theme.clone());
    let mut frames = Vec::new();

    capture_into(
        &mut app,
        &mut capture,
        &mut frames,
        "design-pass",
        "The design pass",
        "The client as it opens on the seeded world: the file tree, the buffer, the \
         session list with its transcript, the status line and the per-provider stats.",
        "the screen as it opens",
    );

    press(&mut app, ctrl('p'));
    capture_into(
        &mut app,
        &mut capture,
        &mut frames,
        "palette",
        "The command palette",
        "One list of everything the client can do, with the chord that would have run \
         it. A row tagged with a task is designed and not built: running it says so \
         instead of doing nothing.",
        "Ctrl-P",
    );

    press(&mut app, ctrl('p'));
    press(&mut app, key(KeyCode::Esc));
    press(&mut app, key(KeyCode::Char('?')));
    capture_into(
        &mut app,
        &mut capture,
        &mut frames,
        "keys",
        "The keys of the focused pane",
        "`?` lists the chords of whichever pane has the keyboard, so the answer is about \
         where you are rather than a wall of every key in the client.",
        "Esc, ?",
    );

    // `F9` draws the titles; a second key opens one. The key that dismisses the
    // list above is pressed first because a capture walks one session, and the
    // notices a person dismisses on the way are part of the walk.
    press(&mut app, key(KeyCode::Esc));
    press(&mut app, key(KeyCode::F(9)));
    press(&mut app, key(KeyCode::Down));
    capture_into(
        &mut app,
        &mut capture,
        &mut frames,
        "menus",
        "The menu bar",
        "The titles are always drawn, so the commands are discoverable without a key \
         list. A row prints the chord that would have run it from anywhere; a row that \
         prints nothing is a command the focused pane owns.",
        "F9, ↓",
    );

    press(&mut app, key(KeyCode::Esc));
    press(&mut app, key(KeyCode::Esc));
    // A request the client already held opens nothing when the list is refreshed
    // under it, so the capture clears the seeded list first: this is a request
    // arriving, which is the moment the card is for.
    app.apply(Update::Permissions(Vec::new()));
    app.apply(Update::Permissions(vec![demo.permission.clone()]));
    capture_into(
        &mut app,
        &mut capture,
        &mut frames,
        "permission",
        "A pending permission",
        "The card is the reason this client exists: what the agent wants, the diff it \
         would write, and the three answers. Nothing behind it takes a keystroke until \
         it is answered.",
        "a request arrives",
    );

    press(&mut app, key(KeyCode::Esc));
    press(&mut app, ctrl('t'));
    capture_into(
        &mut app,
        &mut capture,
        &mut frames,
        "terminal-picker",
        "Choosing a terminal",
        "The session's terminals, one of them ended. A terminal belongs to the session \
         that opened it, so the list is that session's, not the profile's.",
        "Ctrl-T",
    );

    app.apply(Update::TerminalOpened {
        terminal: demo.terminals[0].clone(),
        replay_bytes: demo.terminal_screen.len(),
    });
    app.apply(Update::TerminalData(
        demo.terminal_screen.as_bytes().to_vec(),
    ));
    capture_into(
        &mut app,
        &mut capture,
        &mut frames,
        "terminal",
        "A program's own screen",
        "The pane parses a program's bytes into cells, so the colours here are the \
         program's: the one place on this page a palette does not come from the theme.",
        "Ctrl-T, then the attach",
    );

    let document = json!({
        "generated_by": "cargo xtask showcase",
        "schema_version": 1,
        "columns": COLUMNS,
        "rows": ROWS,
        "scenes": frames,
        "styles": capture.styles,
    });

    let mut text =
        serde_json::to_string_pretty(&document).expect("the captured frames are a document");
    text.push('\n');
    text
}

/// The desktop shell's tables.
///
/// The window is drawn by gpui and needs a graphics stack; what a page can show
/// without one is the data the shell is built from — the keymap it installs and the
/// rows its command palette contains — so the representation is the shell's own
/// words rather than a drawing of them.
pub fn desktop() -> String {
    let keymap: Vec<Value> = acpilot_gui::keymap::DEFAULT
        .iter()
        .map(|binding| {
            json!({
                "keys": binding.keys,
                "action": action_name(binding.action),
                "description": binding.description,
                "contexts": binding
                    .contexts
                    .iter()
                    .map(|context| context.name())
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    let commands: Vec<Value> = acpilot_gui::palette::commands(&[])
        .iter()
        .map(|command| {
            json!({
                "label": command.label,
                "detail": command.detail,
            })
        })
        .collect();

    let document = json!({
        "generated_by": "cargo xtask showcase",
        "schema_version": 1,
        "version": acpilot_core::VERSION,
        "keymap": keymap,
        "commands": commands,
    });

    let mut text =
        serde_json::to_string_pretty(&document).expect("the shell's tables are a document");
    text.push('\n');
    text
}

/// A keymap verb's name, as the shell spells it.
fn action_name(action: acpilot_gui::keymap::Action) -> &'static str {
    use acpilot_gui::keymap::Action;

    match action {
        Action::TogglePalette => "TogglePalette",
        Action::PaletteUp => "PaletteUp",
        Action::PaletteDown => "PaletteDown",
        Action::PaletteConfirm => "PaletteConfirm",
        Action::PaletteDismiss => "PaletteDismiss",
        Action::NextProfile => "NextProfile",
        Action::PreviousProfile => "PreviousProfile",
        Action::ToggleStatsSources => "ToggleStatsSources",
        Action::Quit => "Quit",
    }
}

/// Every document the site reads, with the path it is written to.
#[must_use]
pub fn documents() -> Vec<(&'static str, String)> {
    vec![(SCREENS, screens()), (DESKTOP, desktop())]
}

/// A frame as plain text, one line per row.
///
/// The tests read this: a frame that has gone blank, or one that repeats the frame
/// before it, is a capture that has stopped working, and a diff of text says which.
#[cfg(test)]
fn text_of(frame: &Value) -> String {
    let mut lines = Vec::new();
    for row in frame["rows"].as_array().expect("a row list") {
        let mut line: Vec<char> = " ".repeat(COLUMNS as usize).chars().collect();
        for run in row.as_array().expect("a run list") {
            let column =
                usize::try_from(run["column"].as_u64().expect("a column")).expect("a column");
            let text = run["text"].as_str().expect("text");
            for (offset, ch) in text.chars().enumerate() {
                if let Some(cell) = line.get_mut(column + offset) {
                    *cell = ch;
                }
            }
        }
        lines.push(line.into_iter().collect::<String>().trim_end().to_owned());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenes() -> Value {
        serde_json::from_str(&screens()).expect("the document parses")
    }

    #[test]
    fn the_capture_describes_the_screens_it_promises() {
        let document = scenes();
        let ids: Vec<&str> = document["scenes"]
            .as_array()
            .expect("a scene list")
            .iter()
            .map(|scene| scene["id"].as_str().expect("an id"))
            .collect();
        assert_eq!(
            ids,
            vec![
                "design-pass",
                "palette",
                "keys",
                "menus",
                "permission",
                "terminal-picker",
                "terminal",
            ],
            "the filmstrip is the client's own states, in the order a visitor walks them"
        );
    }

    #[test]
    fn every_screen_has_content_and_no_two_screens_are_the_same() {
        let document = scenes();
        let mut seen: Vec<String> = Vec::new();
        for scene in document["scenes"].as_array().expect("a scene list") {
            let text = text_of(scene);
            let words = text.split_whitespace().count();
            assert!(words > 100, "{} is mostly blank:\n{text}", scene["id"]);
            assert!(
                !seen.contains(&text),
                "{} is the previous screen: the key that reaches it changed nothing",
                scene["id"]
            );
            seen.push(text);
        }
    }

    #[test]
    fn a_cell_names_a_token_rather_than_a_colour() {
        // The page recolours a frame by asking a theme what `token:accent` is now. A
        // capture that recorded `#8a5a10` would be stuck at the theme it was taken
        // in, and a reader could see neither the light half nor their own palette.
        let document = scenes();
        let styles = document["styles"].as_array().expect("a style table");
        assert!(
            styles.iter().any(|style| style["fg"]
                .as_str()
                .is_some_and(|fg| fg.starts_with("token:"))),
            "the capture is painted in tokens"
        );
        assert!(
            styles.iter().any(|style| style["bg"]
                .as_str()
                .is_some_and(|bg| bg.starts_with("token:"))),
            "backgrounds too: a panel is a token"
        );
    }

    #[test]
    fn the_desktop_tables_are_the_shells_own() {
        let document: Value = serde_json::from_str(&desktop()).expect("the document parses");
        let bindings = document["keymap"].as_array().expect("a keymap");
        assert_eq!(bindings.len(), acpilot_gui::keymap::DEFAULT.len());
        assert!(
            bindings
                .iter()
                .any(|binding| binding["description"] == "Command palette"),
            "the rows are the shell's descriptions:\n{document:#}"
        );
        assert!(
            document["commands"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
        );
    }

    #[test]
    fn the_committed_documents_are_the_ones_this_build_captures() {
        // The rule the published schemas follow: a change to a pane, a key or a
        // token has to reach the site in the same commit, or the page describes a
        // screen nobody has run.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for (relative, current) in documents() {
            let path = root.join(relative);
            let committed = std::fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!(
                    "{} is missing ({error}); run `cargo xtask showcase`",
                    path.display()
                )
            });
            assert_eq!(
                committed,
                current,
                "{} is stale; run `cargo xtask showcase`",
                path.display()
            );
        }
    }
}
