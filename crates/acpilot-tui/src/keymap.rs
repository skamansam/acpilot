//! The keys a person can change, as data.
//!
//! `?` has always listed the keys; this is the module that decides them. A
//! binding names an [`Action`] and the contexts it is live in, so "what does `j`
//! do" has one answer per context rather than one `match` arm per pane, and
//! `keymap.toml` can change that answer without a build.
//!
//! Two presets ship: **vim**, which is the table this crate grew (motions on
//! `hjkl`, insert mode behind `i`), and **vscode**, which moves with the arrows and
//! Home/End instead and puts the palette on `Ctrl-Shift-P` as well. The difference
//! is the table, not the editor: a preset changes which key means `move-down`, not
//! what `move-down` does. A modeless editor would be a different pane, not a
//! different row.
//!
//! The file is read from `<config>/keymap.toml` beside `config.toml`:
//!
//! ```toml
//! preset = "vim"
//!
//! [keys]
//! palette = ["ctrl-p", "f1"]
//! quit = ["ctrl-q"]
//! move-down = ["down", "j", "ctrl-n"]
//! ```
//!
//! Every action in the table can be rebound or, with an empty list, unbound.
//! Anything else is a mistake with a name rather than a silently ignored line — a
//! typo in a key file that does nothing is indistinguishable from a key that does
//! not work.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// Where a binding is live.
///
/// A context is a mode the editor already has, or the client itself: the app's
/// keys are the ones that work from every pane, which is why they are not repeated
/// per pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Context {
    /// The client's own keys: the palette, the panes, the session, the terminal.
    App,
    /// The editor while its keys are commands.
    Editor,
    /// The editor while its motions extend a selection.
    Visual,
}

impl Context {
    /// What the file calls it, if it ever needs to.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Editor => "editor",
            Self::Visual => "visual",
        }
    }
}

/// What a keystroke means.
///
/// One verb, several contexts: `move-down` extends a selection in [`Context::Visual`]
/// and moves a bare caret in [`Context::Editor`]. The effect belongs to the pane;
/// the name belongs here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// Open or close the command palette.
    Palette,
    /// List the focused pane's keys.
    KeyList,
    /// Move the keyboard to the next pane.
    NextFocus,
    /// Open or close the menu bar.
    Menu,
    /// Open or leave the terminal pane.
    Terminal,
    /// Open or leave the diff review.
    Review,
    /// Give the transcript the keyboard, or take it back.
    Transcript,
    /// Swap the stats bar's figures for the source behind each one.
    StatsSources,
    /// Search the workspace's file contents.
    Grep,
    /// Open or close the source-control pane.
    SourceControl,
    /// Ask the daemon again.
    Refresh,
    /// Leave the app.
    Quit,
    /// Narrow the focused pane's column.
    Narrower,
    /// Widen the focused pane's column.
    Wider,
    /// Save the buffer through the daemon.
    Save,
    /// Move the caret up.
    MoveUp,
    /// Move the caret down.
    MoveDown,
    /// Move the caret left.
    MoveLeft,
    /// Move the caret right.
    MoveRight,
    /// Put the caret at the start of the line.
    LineStart,
    /// Put the caret at the end of the line.
    LineEnd,
    /// Scroll a page back.
    PageUp,
    /// Scroll a page forward.
    PageDown,
    /// Open the find box.
    Find,
    /// Ask for a line and column.
    GotoLine,
    /// Walk to the next match.
    NextMatch,
    /// Walk to the previous match.
    PrevMatch,
    /// Copy the selection to the terminal's clipboard.
    Yank,
    /// Start a visual selection, or leave it.
    Visual,
    /// Open the `:` box.
    Command,
    /// Clear the selection without moving the caret.
    Cancel,
    /// Type at the caret.
    Insert,
    /// Type after the caret.
    Append,
    /// Type at the start of the line.
    InsertLineStart,
    /// Type at the end of the line.
    InsertLineEnd,
    /// Open a line below and type on it.
    OpenBelow,
    /// Open a line above and type on it.
    OpenAbove,
    /// Delete the character under the caret.
    DeleteChar,
    /// Delete what a visual selection covers.
    DeleteSelection,
    /// Undo the last edit.
    Undo,
    /// Redo the undone edit.
    Redo,
}

impl Action {
    /// Every verb, in the order a key list should read them.
    pub const ALL: &'static [Self] = &[
        Self::Palette,
        Self::KeyList,
        Self::NextFocus,
        Self::Menu,
        Self::Terminal,
        Self::Review,
        Self::Transcript,
        Self::StatsSources,
        Self::Grep,
        Self::SourceControl,
        Self::Refresh,
        Self::Quit,
        Self::Narrower,
        Self::Wider,
        Self::Save,
        Self::MoveUp,
        Self::MoveDown,
        Self::MoveLeft,
        Self::MoveRight,
        Self::LineStart,
        Self::LineEnd,
        Self::PageUp,
        Self::PageDown,
        Self::Find,
        Self::GotoLine,
        Self::NextMatch,
        Self::PrevMatch,
        Self::Yank,
        Self::Visual,
        Self::Command,
        Self::Cancel,
        Self::Insert,
        Self::Append,
        Self::InsertLineStart,
        Self::InsertLineEnd,
        Self::OpenBelow,
        Self::OpenAbove,
        Self::DeleteChar,
        Self::DeleteSelection,
        Self::Undo,
        Self::Redo,
    ];

    /// The contexts the verb is live in.
    #[must_use]
    pub const fn contexts(self) -> &'static [Context] {
        use Context::{App, Editor, Visual};
        match self {
            Self::Palette
            | Self::KeyList
            | Self::NextFocus
            | Self::Menu
            | Self::Terminal
            | Self::Review
            | Self::Transcript
            | Self::StatsSources
            | Self::Grep
            | Self::SourceControl
            | Self::Refresh
            | Self::Quit
            | Self::Narrower
            | Self::Wider
            | Self::Save => &[App],
            Self::MoveUp | Self::MoveDown | Self::MoveLeft | Self::MoveRight => &[Editor, Visual],
            Self::LineStart | Self::LineEnd | Self::PageUp | Self::PageDown => &[Editor, Visual],
            Self::Yank => &[Editor, Visual],
            Self::Visual => &[Editor, Visual],
            Self::Command => &[Editor, Visual],
            Self::Cancel => &[Editor, Visual],
            Self::DeleteSelection => &[Visual],
            Self::Find
            | Self::GotoLine
            | Self::NextMatch
            | Self::PrevMatch
            | Self::Insert
            | Self::Append
            | Self::InsertLineStart
            | Self::InsertLineEnd
            | Self::OpenBelow
            | Self::OpenAbove
            | Self::DeleteChar
            | Self::Undo
            | Self::Redo => &[Editor],
        }
    }

    /// What a person would call it, for the key list.
    #[must_use]
    pub const fn what(self) -> &'static str {
        match self {
            Self::Palette => "commands",
            Self::KeyList => "these keys",
            Self::NextFocus => "next pane",
            Self::Menu => "menu bar",
            Self::Terminal => "terminal pane",
            Self::Review => "review diff",
            Self::Transcript => "read the transcript",
            Self::StatsSources => "where the stats came from",
            Self::Grep => "search file contents",
            Self::SourceControl => "source control",
            Self::Refresh => "refresh",
            Self::Quit => "quit",
            Self::Narrower => "narrower pane",
            Self::Wider => "wider pane",
            Self::Save => "save",
            Self::MoveUp => "move up",
            Self::MoveDown => "move down",
            Self::MoveLeft => "move left",
            Self::MoveRight => "move right",
            Self::LineStart => "start of line",
            Self::LineEnd => "end of line",
            Self::PageUp => "a page back",
            Self::PageDown => "a page on",
            Self::Find => "find in file",
            Self::GotoLine => "go to line",
            Self::NextMatch => "next match",
            Self::PrevMatch => "previous match",
            Self::Yank => "copy the selection",
            Self::Visual => "visual selection",
            Self::Command => "write / quit",
            Self::Cancel => "clear the selection",
            Self::Insert => "insert here",
            Self::Append => "insert after",
            Self::InsertLineStart => "insert at the line's start",
            Self::InsertLineEnd => "insert at the line's end",
            Self::OpenBelow => "a line below",
            Self::OpenAbove => "a line above",
            Self::DeleteChar => "delete a character",
            Self::DeleteSelection => "delete the selection",
            Self::Undo => "undo",
            Self::Redo => "redo",
        }
    }

    /// The name the file uses.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Palette => "palette",
            Self::KeyList => "key-list",
            Self::NextFocus => "next-pane",
            Self::Menu => "menu",
            Self::Terminal => "terminal",
            Self::Review => "review",
            Self::Transcript => "transcript",
            Self::StatsSources => "stats-sources",
            Self::Grep => "grep",
            Self::SourceControl => "source-control",
            Self::Refresh => "refresh",
            Self::Quit => "quit",
            Self::Narrower => "narrower",
            Self::Wider => "wider",
            Self::Save => "save",
            Self::MoveUp => "move-up",
            Self::MoveDown => "move-down",
            Self::MoveLeft => "move-left",
            Self::MoveRight => "move-right",
            Self::LineStart => "line-start",
            Self::LineEnd => "line-end",
            Self::PageUp => "page-up",
            Self::PageDown => "page-down",
            Self::Find => "find",
            Self::GotoLine => "goto-line",
            Self::NextMatch => "next-match",
            Self::PrevMatch => "prev-match",
            Self::Yank => "yank",
            Self::Visual => "visual",
            Self::Command => "command",
            Self::Cancel => "cancel",
            Self::Insert => "insert",
            Self::Append => "append",
            Self::InsertLineStart => "insert-line-start",
            Self::InsertLineEnd => "insert-line-end",
            Self::OpenBelow => "open-below",
            Self::OpenAbove => "open-above",
            Self::DeleteChar => "delete-char",
            Self::DeleteSelection => "delete-selection",
            Self::Undo => "undo",
            Self::Redo => "redo",
        }
    }

    /// The verb a file's line names, if it names one.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|action| action.name() == name)
    }
}

/// One key press: a code and the modifiers held with it.
///
/// Held in a canonical form rather than as typed: a capital letter *is* `Shift`, a
/// letter under `Ctrl` is the same letter either way, and a table that compared the
/// raw event would need `shift-n` and `N` to be two rows that mean one thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Chord {
    /// Read one key as a person writes it: `ctrl-p`, `shift-f12`, `?`, `enter`.
    ///
    /// # Errors
    ///
    /// The text names a modifier or a key that does not exist. The message names
    /// the part that was wrong, because a key file is edited by hand.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("an empty key".to_owned());
        }
        let mut parts: Vec<&str> = text.split('-').collect();
        let key = parts.pop().unwrap_or_default();
        let mut modifiers = KeyModifiers::empty();
        for part in parts {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                "alt" | "meta" | "option" => modifiers |= KeyModifiers::ALT,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                other => return Err(format!("not a modifier: {other}")),
            }
        }
        let code = match key.to_ascii_lowercase().as_str() {
            "" => return Err(format!("no key after the modifiers: {text}")),
            "space" => KeyCode::Char(' '),
            "tab" => KeyCode::Tab,
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            name if name.len() == 1 => KeyCode::Char(key.chars().next().unwrap_or(' ')),
            name => match name
                .strip_prefix('f')
                .and_then(|digits| digits.parse().ok())
            {
                Some(number) => KeyCode::F(number),
                None => return Err(format!("not a key: {key}")),
            },
        };
        // A capital is a shift, and saying it twice is not a conflict.
        if let KeyCode::Char(character) = code
            && character.is_ascii_uppercase()
        {
            modifiers |= KeyModifiers::SHIFT;
        }
        Ok(Self::new(code, modifiers))
    }

    /// A chord from the event a terminal sent, in the canonical form.
    #[must_use]
    pub fn from_event(key: KeyEvent) -> Self {
        let (code, modifiers) = canonical(key.code, key.modifiers);
        Self { code, modifiers }
    }

    /// A chord from a code and modifiers, canonicalised.
    #[must_use]
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        let (code, modifiers) = canonical(code, modifiers);
        Self { code, modifiers }
    }

    /// Whether this is the key that was pressed.
    #[must_use]
    pub fn matches(self, key: KeyEvent) -> bool {
        self == Self::from_event(key)
    }
}

/// The modifiers that mean something, in the order they are written.
const HELD: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::SHIFT);

fn canonical(code: KeyCode, modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let held = modifiers & HELD;
    match code {
        KeyCode::Char(character) => {
            if held.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                // `Ctrl-Shift-F` arrives as `F` on some terminals and as `f` with
                // shift held on others; both are one chord, and the shift has to
                // survive as a modifier because `Ctrl-F` is the editor's find and
                // `Ctrl-Shift-F` is the workspace's search. The capital is read as
                // the shift it means, and the letter is folded so the two spellings
                // of one press meet.
                let shifted = character.is_ascii_uppercase();
                let modifiers = if shifted {
                    held | KeyModifiers::SHIFT
                } else {
                    held
                };
                (KeyCode::Char(character.to_ascii_lowercase()), modifiers)
            } else {
                // Outside a chord, a capital *is* the key: `N` and `Shift-N` are one
                // press, not two rows in the table.
                (KeyCode::Char(character), held & !KeyModifiers::SHIFT)
            }
        }
        other => (other, held),
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (flag, name) in [
            (KeyModifiers::CONTROL, "ctrl"),
            (KeyModifiers::ALT, "alt"),
            (KeyModifiers::SHIFT, "shift"),
        ] {
            if self.modifiers.contains(flag) {
                write!(formatter, "{name}-")?;
            }
        }
        formatter.write_str(&key_name(self.code))
    }
}

fn key_name(code: KeyCode) -> String {
    match code {
        KeyCode::Char(character) => character.to_string(),
        KeyCode::F(number) => format!("f{number}"),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::BackTab => "shift-tab".to_owned(),
        _ => "?".to_owned(),
    }
}

/// Which table of bindings a client starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// The table this crate grew: `hjkl`, insert mode behind `i`.
    #[default]
    Vim,
    /// Motions on the arrows and Home/End, the palette on `Ctrl-Shift-P` too.
    VsCode,
}

impl Preset {
    /// The name the file uses.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Vim => "vim",
            Self::VsCode => "vscode",
        }
    }

    /// The preset a file names.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "vim" => Some(Self::Vim),
            "vscode" | "code" => Some(Self::VsCode),
            _ => None,
        }
    }

    fn table(self) -> Vec<(Action, &'static [&'static str])> {
        use Action::{
            Append, Cancel, Command, DeleteChar, DeleteSelection, Find, GotoLine, Insert,
            InsertLineEnd, InsertLineStart, LineEnd, LineStart, MoveDown, MoveLeft, MoveRight,
            MoveUp, NextMatch, OpenAbove, OpenBelow, PageDown, PageUp, PrevMatch, Redo, Undo,
            Visual, Yank,
        };

        let mut table: Vec<(Action, &'static [&'static str])> = APP.to_vec();
        match self {
            Self::Vim => {
                table.extend_from_slice(&[
                    (MoveUp, &["up", "k"]),
                    (MoveDown, &["down", "j"]),
                    (MoveLeft, &["left", "h"]),
                    (MoveRight, &["right", "l"]),
                    (LineStart, &["home", "0"]),
                    (LineEnd, &["end", "$"]),
                    (PageUp, &["pageup"]),
                    (PageDown, &["pagedown"]),
                    (Find, &["/", "ctrl-f"]),
                    (GotoLine, &["ctrl-g"]),
                    (NextMatch, &["n"]),
                    (PrevMatch, &["N"]),
                    (Yank, &["y"]),
                    (Visual, &["v"]),
                    (Command, &[":"]),
                    (Cancel, &["esc"]),
                    (Insert, &["i"]),
                    (Append, &["a"]),
                    (InsertLineStart, &["I"]),
                    (InsertLineEnd, &["A"]),
                    (OpenBelow, &["o"]),
                    (OpenAbove, &["O"]),
                    (DeleteChar, &["x"]),
                    (Undo, &["u", "ctrl-z"]),
                    (Redo, &["ctrl-y", "ctrl-shift-z"]),
                ]);
            }
            Self::VsCode => {
                table.extend_from_slice(&[
                    (MoveUp, &["up"]),
                    (MoveDown, &["down"]),
                    (MoveLeft, &["left"]),
                    (MoveRight, &["right"]),
                    (LineStart, &["home"]),
                    (LineEnd, &["end"]),
                    (PageUp, &["pageup"]),
                    (PageDown, &["pagedown"]),
                    (Find, &["ctrl-f"]),
                    (GotoLine, &["ctrl-g"]),
                    (NextMatch, &["n"]),
                    (PrevMatch, &["N"]),
                    (Yank, &["y"]),
                    (Visual, &["v"]),
                    (Command, &[":"]),
                    (Cancel, &["esc"]),
                    (Insert, &["i"]),
                    (Append, &["a"]),
                    (InsertLineEnd, &["A"]),
                    (OpenBelow, &["o"]),
                    (DeleteChar, &["x"]),
                    (Undo, &["ctrl-z"]),
                    (Redo, &["ctrl-y"]),
                ]);
            }
        }
        // The motions a visual selection needs are the same table in both presets:
        // what a person rebinds once should not have to be rebound twice.
        table.extend_from_slice(&[
            (MoveUp, &["up"]),
            (MoveDown, &["down"]),
            (MoveLeft, &["left"]),
            (MoveRight, &["right"]),
            (LineStart, &["home"]),
            (LineEnd, &["end"]),
            (PageUp, &["pageup"]),
            (PageDown, &["pagedown"]),
            (Yank, &["y"]),
            (DeleteSelection, &["d", "x"]),
            (Visual, &["v"]),
            (Cancel, &["esc"]),
            (Command, &[":"]),
        ]);
        if self == Self::VsCode {
            table.extend_from_slice(&[
                (Action::Palette, &["ctrl-shift-p"]),
                (Action::KeyList, &["f1"]),
            ]);
        }
        table
    }
}

use Action::{Grep, KeyList, Narrower, Transcript, Wider};
use Action::{
    Menu, NextFocus, Palette, Quit, Refresh, Review, Save, SourceControl, StatsSources, Terminal,
};

/// The client's own keys, the same in both presets unless a preset adds one.
const APP: &[(Action, &[&str])] = &[
    (Palette, &["ctrl-p"]),
    (KeyList, &["?"]),
    (NextFocus, &["tab"]),
    (Menu, &["f9"]),
    (Terminal, &["ctrl-t"]),
    (Review, &["ctrl-d"]),
    (Transcript, &["f3"]),
    (StatsSources, &["f4"]),
    (Grep, &["ctrl-shift-f"]),
    (SourceControl, &["f6"]),
    (Refresh, &["ctrl-r"]),
    (Quit, &["ctrl-c", "ctrl-q"]),
    (Narrower, &["ctrl-left"]),
    (Wider, &["ctrl-right"]),
    (Save, &["ctrl-s"]),
];

/// One verb and every key that means it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// What the key does.
    pub action: Action,
    /// The keys that do it, in the order they should be listed.
    pub chords: Vec<Chord>,
}

/// The bindings in force, and where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    preset: Preset,
    bindings: Vec<Binding>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::preset(Preset::Vim)
    }
}

impl Keymap {
    /// One preset's table, with nothing overridden.
    #[must_use]
    pub fn preset(preset: Preset) -> Self {
        let mut bindings: Vec<Binding> = Vec::new();
        for (action, keys) in preset.table() {
            let chords: Vec<Chord> = keys
                .iter()
                .map(|text| Chord::parse(text).expect("a built-in chord is a key"))
                .collect();
            match bindings.iter_mut().find(|binding| binding.action == action) {
                Some(binding) => binding.chords.extend(chords),
                None => bindings.push(Binding { action, chords }),
            }
        }
        Self { preset, bindings }
    }

    /// Read `keymap.toml`, or the default table when there is no file.
    ///
    /// A missing file is a first launch rather than an error, which is why this
    /// answers with the preset instead of [`Option`]: the caller's only job is to
    /// start with something.
    ///
    /// # Errors
    ///
    /// The file does not parse, names a preset or an action that does not exist,
    /// holds a key that does not exist, or binds one key to two verbs in one
    /// context.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                Self::from_toml(&text).map_err(|error| format!("{}: {error}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// Read the file's text.
    ///
    /// # Errors
    ///
    /// See [`Keymap::load`].
    pub fn from_toml(text: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct File {
            preset: Option<String>,
            #[serde(default)]
            keys: BTreeMap<String, Vec<String>>,
        }

        let file: File = toml::from_str(text).map_err(|error| error.to_string())?;
        let preset = match file.preset.as_deref() {
            None => Preset::Vim,
            Some(name) => Preset::from_name(name)
                .ok_or_else(|| format!("not a preset: {name} (vim or vscode)"))?,
        };
        let mut keymap = Self::preset(preset);
        for (name, keys) in &file.keys {
            let action =
                Action::from_name(name).ok_or_else(|| format!("not a keymap action: {name}"))?;
            let mut chords = Vec::with_capacity(keys.len());
            for text in keys {
                chords.push(Chord::parse(text).map_err(|error| format!("{name}: {error}"))?);
            }
            match keymap
                .bindings
                .iter_mut()
                .find(|binding| binding.action == action)
            {
                // Replaced where it stands, so a file does not reshuffle the list.
                Some(binding) => binding.chords = chords,
                None => keymap.bindings.push(Binding { action, chords }),
            }
        }
        keymap.check()?;
        Ok(keymap)
    }

    /// The preset the table came from.
    #[must_use]
    pub fn preset_name(&self) -> &'static str {
        self.preset.name()
    }

    /// What a key means in a context.
    #[must_use]
    pub fn action(&self, context: Context, key: KeyEvent) -> Option<Action> {
        self.bindings
            .iter()
            .filter(|binding| binding.action.contexts().contains(&context))
            .find(|binding| binding.chords.iter().any(|chord| chord.matches(key)))
            .map(|binding| binding.action)
    }

    /// Every key bound to an action, or nothing when it is unbound.
    #[must_use]
    pub fn chords(&self, action: Action) -> &[Chord] {
        self.bindings
            .iter()
            .find(|binding| binding.action == action)
            .map_or(&[], |binding| binding.chords.as_slice())
    }

    /// The effective bindings for a context, ready for the key list.
    ///
    /// One row per verb, with every key that means it: a person who rebound
    /// `move-down` to three keys should see three, not the first one.
    #[must_use]
    pub fn rows(&self, context: Context) -> Vec<(String, &'static str)> {
        let mut rows = Vec::new();
        for action in Action::ALL {
            if !action.contexts().contains(&context) {
                continue;
            }
            let chords = self.chords(*action);
            if chords.is_empty() {
                continue;
            }
            let keys: Vec<String> = chords.iter().map(ToString::to_string).collect();
            rows.push((keys.join(" / "), action.what()));
        }
        rows
    }

    /// Refuse a table that binds one key to two verbs in one context.
    ///
    /// Checked after the file is applied rather than while it is read, because an
    /// override can *free* a key as well as take one: binding `palette` away from
    /// `Ctrl-P` and `quit` to it is not a conflict, it is a swap.
    fn check(&self) -> Result<(), String> {
        for context in [Context::App, Context::Editor, Context::Visual] {
            let mut seen: Vec<(Chord, Action)> = Vec::new();
            for binding in &self.bindings {
                if !binding.action.contexts().contains(&context) {
                    continue;
                }
                for chord in &binding.chords {
                    if let Some((_, other)) = seen
                        .iter()
                        .find(|(taken, action)| taken == chord && action != &binding.action)
                    {
                        return Err(format!(
                            "{chord} means both {} and {} in {}",
                            other.name(),
                            binding.action.name(),
                            context.name()
                        ));
                    }
                    seen.push((*chord, binding.action));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    #[test]
    fn a_chord_parses_the_way_a_person_writes_it() {
        assert_eq!(
            Chord::parse("ctrl-p").expect("a key"),
            Chord::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            Chord::parse("shift-f12").expect("a key"),
            Chord::new(KeyCode::F(12), KeyModifiers::SHIFT)
        );
        assert_eq!(
            Chord::parse("?").expect("a key"),
            Chord::new(KeyCode::Char('?'), KeyModifiers::empty())
        );
        assert_eq!(
            Chord::parse("N").expect("a key"),
            Chord::new(KeyCode::Char('N'), KeyModifiers::empty()),
            "a capital is a shift, not a second key"
        );
        assert_eq!(Chord::parse("pgdn").expect("a key").to_string(), "pagedown");
        assert_eq!(
            Chord::parse("ctrl-shift-f").expect("a key").to_string(),
            "ctrl-shift-f",
            "two chords that mean different things say so"
        );
        assert_ne!(
            Chord::parse("ctrl-shift-f").expect("a key"),
            Chord::parse("ctrl-f").expect("a key")
        );
    }

    #[test]
    fn a_key_that_does_not_exist_is_refused_by_name() {
        let error = Chord::parse("ctrl-").expect_err("no key");
        assert!(error.contains("no key"), "{error}");
        let error = Chord::parse("hyper-p").expect_err("no such modifier");
        assert!(error.contains("hyper"), "{error}");
        let error = Chord::parse("ctrl-fnord").expect_err("no such key");
        assert!(error.contains("fnord"), "{error}");
    }

    #[test]
    fn a_keyboard_event_and_a_written_chord_meet_in_the_middle() {
        let chord = Chord::parse("ctrl-shift-f").expect("a key");
        assert!(chord.matches(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(chord.matches(KeyEvent::new(
            KeyCode::Char('F'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!chord.matches(ctrl('f')), "the shift is the point");
        assert!(
            Chord::parse("N")
                .expect("a key")
                .matches(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT))
        );
    }

    #[test]
    fn no_chord_means_two_things_in_one_context() {
        for preset in [Preset::Vim, Preset::VsCode] {
            let keymap = Keymap::preset(preset);
            keymap
                .check()
                .unwrap_or_else(|error| panic!("{preset:?}: {error}"));
        }
    }

    #[test]
    fn every_verb_has_a_binding_in_the_preset_that_owns_it() {
        for preset in [Preset::Vim, Preset::VsCode] {
            let keymap = Keymap::preset(preset);
            for action in Action::ALL {
                // The VS Code table is modeless about the keys a Vim insert leaves
                // behind, so two verbs are deliberately absent there.
                let optional = matches!(preset, Preset::VsCode)
                    && matches!(action, Action::InsertLineStart | Action::OpenAbove);
                if optional {
                    continue;
                }
                assert!(
                    !keymap.chords(*action).is_empty(),
                    "{preset:?}: {:?} has no binding",
                    action
                );
            }
        }
    }

    #[test]
    fn a_file_rebinds_a_verb_and_the_key_it_left_stops_working() {
        let keymap = Keymap::from_toml("preset = \"vim\"\n\n[keys]\npalette = [\"f1\"]\n")
            .expect("a keymap");
        assert_eq!(
            keymap.action(Context::App, key(KeyCode::F(1))),
            Some(Action::Palette)
        );
        assert_eq!(keymap.action(Context::App, ctrl('p')), None);
    }

    #[test]
    fn an_empty_list_unbinds_a_verb() {
        let keymap = Keymap::from_toml("[keys]\nquit = []\n").expect("a keymap");
        assert_eq!(keymap.action(Context::App, ctrl('q')), None);
        assert_eq!(keymap.action(Context::App, ctrl('c')), None);
        assert!(
            keymap
                .rows(Context::App)
                .iter()
                .all(|(_, what)| *what != "quit"),
            "an unbound verb is not listed"
        );
    }

    #[test]
    fn a_file_may_swap_two_keys() {
        let keymap = Keymap::from_toml("[keys]\npalette = [\"ctrl-q\"]\nquit = [\"ctrl-p\"]\n")
            .expect("a swap is not a conflict");
        assert_eq!(
            keymap.action(Context::App, ctrl('q')),
            Some(Action::Palette)
        );
        assert_eq!(keymap.action(Context::App, ctrl('p')), Some(Action::Quit));
    }

    #[test]
    fn one_key_bound_to_two_verbs_is_refused_naming_both() {
        let error = Keymap::from_toml("[keys]\npalette = [\"ctrl-q\"]\n")
            .expect_err("Ctrl-Q is already quit");
        assert!(
            error.contains("quit") && error.contains("palette"),
            "{error}"
        );
    }

    #[test]
    fn a_name_that_is_not_an_action_is_refused_by_name() {
        let error = Keymap::from_toml("[keys]\nflow-north = [\"j\"]\n").expect_err("not a verb");
        assert!(error.contains("flow-north"), "{error}");
        let error = Keymap::from_toml("preset = \"emacs\"\n").expect_err("not a preset");
        assert!(error.contains("emacs"), "{error}");
    }

    #[test]
    fn the_two_presets_move_with_different_keys() {
        let vim = Keymap::preset(Preset::Vim);
        assert_eq!(
            vim.action(Context::Editor, key(KeyCode::Char('j'))),
            Some(Action::MoveDown)
        );
        let code = Keymap::preset(Preset::VsCode);
        assert_eq!(code.action(Context::Editor, key(KeyCode::Char('j'))), None);
        assert_eq!(
            code.action(Context::Editor, key(KeyCode::Down)),
            Some(Action::MoveDown)
        );
        assert_eq!(code.preset_name(), "vscode");
        assert_eq!(
            code.action(
                Context::App,
                KeyEvent::new(
                    KeyCode::Char('P'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                )
            ),
            Some(Action::Palette),
            "the palette is where a VS Code hand expects it, as well as Ctrl-P"
        );
    }

    #[test]
    fn a_file_can_choose_a_preset() {
        let keymap = Keymap::from_toml("preset = \"vscode\"\n").expect("a keymap");
        assert_eq!(keymap.preset_name(), "vscode");
        assert_eq!(
            keymap.action(Context::Editor, key(KeyCode::Char('h'))),
            None
        );
    }

    #[test]
    fn a_launch_with_no_file_is_the_vim_table() {
        let directory = tempfile::tempdir().expect("temp dir");
        let keymap = Keymap::load(&directory.path().join("absent.toml")).expect("no file is fine");
        assert_eq!(keymap, Keymap::default());
    }

    #[test]
    fn the_rows_are_the_bindings_in_force() {
        let keymap = Keymap::from_toml("[keys]\nmove-down = [\"down\", \"j\", \"ctrl-n\"]\n")
            .expect("a keymap");
        let rows = keymap.rows(Context::Editor);
        let row = rows
            .iter()
            .find(|(_, what)| *what == "move down")
            .expect("a row");
        assert_eq!(row.0, "down / j / ctrl-n");
    }
}
