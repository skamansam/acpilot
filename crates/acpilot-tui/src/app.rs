//! The TUI's state: what is on screen, and what a key press means.
//!
//! There is no domain logic here. Sessions, permissions and events arrive from
//! the daemon; this module only decides how they read and which request a
//! keystroke turns into. Keeping that boundary sharp is what lets the same app
//! state be driven by a test with no daemon and no terminal.

use acpilot_demo::{Command, CommandAction, Diff, FileNode, Run};
use acpilot_proto::agent::AgentSummary;
use acpilot_proto::event::{SessionEvent, SessionEventKind};
use acpilot_proto::permission::{PermissionOption, PermissionRequest};
use acpilot_proto::session::{SessionStatus, SessionSummary};
use acpilot_proto::terminal::{TerminalSize, TerminalSummary};
use acpilot_stats::Row;
use acpilot_text::highlight::Grammars;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::backend::{Request, Update};
use crate::editor::{EditorPane, Mode};
use crate::keymap::{Action, Context, Keymap};
use crate::layout::{Layout, MAX_FILES, MAX_SIDEBAR, MIN_FILES, MIN_SIDEBAR};
use crate::theme::Theme;

/// How long the open buffer has to stop changing before its problems are asked about.
///
/// Four frames at the loop's 120 ms: long enough that a sentence of typing is one
/// request rather than eighty, short enough that somebody who stops to read finds the
/// gutter already caught up. Shorter than this is a request per word; longer is a
/// gutter that trails the person's eyes.
const PROBLEMS_QUIET: std::time::Duration = std::time::Duration::from_millis(480);

/// Which pane has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The workspace tree.
    Files,
    /// The editor pane.
    Editor,
    /// The session list.
    Sessions,
    /// The open session's transcript, when it is being read.
    ///
    /// A pane like the others: it takes the keyboard with `Tab` (or `F3` from
    /// anywhere), and while it has it the keys scroll and select instead of being
    /// typed.
    Transcript,
    /// The prompt composer.
    Prompt,
    /// A terminal the user is typing into.
    Terminal,
    /// The open buffer's definitions, when the outline panel is up.
    Outline,
    /// The uses of one name, when the references panel is up.
    References,
    /// The workspace map the agent wrote, when the project map is up.
    ProjectMap,
}

impl Focus {
    /// The label the status line shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Files => "file manager",
            Self::Editor => "editor",
            Self::Sessions => "assistant",
            Self::Transcript => "transcript",
            Self::Prompt => "prompt",
            Self::Terminal => "terminal",
            Self::Outline => "outline",
            Self::References => "references",
            Self::ProjectMap => "project map",
        }
    }
}

/// A pane the shell can put on either side of the editor.
///
/// The editor is not one: it is the middle, and a shell whose text could be moved
/// off the centre would be a shell that had lost the argument about what the window
/// is for. These are the two the person arranges — where the workspace is read and
/// where the agent is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// The workspace tree, which is also the file finder.
    FileManager,
    /// The open buffer's definitions, in the order they appear in the file.
    Outline,
    /// Where the name under the caret is used, across the workspace.
    References,
    /// What the workspace is for, as the agent wrote it under `.acpilot/`.
    ProjectMap,
    /// The sessions, the transcript and the composer: the agent's half.
    Assistant,
}

impl Panel {
    /// What the menu and the status line call it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::FileManager => "File manager",
            Self::Outline => "Outline",
            Self::References => "References",
            Self::ProjectMap => "Project map",
            Self::Assistant => "Assistant",
        }
    }
}

/// Which side of the editor a panel sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Left of the editor.
    Left,
    /// Right of the editor.
    Right,
}

impl Side {
    /// What the menu and the status line call it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// Where the two movable panels are.
///
/// Two sides rather than two panes: a panel that moved has to be *somewhere*, and
/// a shell that could only swap them would have three arrangements for two panes.
/// Both on one side is allowed and stacks them, which is what makes the answer a
/// place rather than a choice between two fixed columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panels {
    /// Where the file manager is.
    pub files: Side,
    /// Where the outline is, while it is shown.
    pub outline: Side,
    /// Where the references list is, while it is shown.
    pub references: Side,
    /// Where the project map is, while it is shown.
    pub map: Side,
    /// Where the assistant is.
    pub assistant: Side,
}

impl Default for Panels {
    fn default() -> Self {
        Self {
            files: Side::Left,
            // Beside the tree it describes, and already closed on a first launch:
            // `show_outline` is what puts it on screen.
            outline: Side::Left,
            references: Side::Left,
            map: Side::Left,
            assistant: Side::Right,
        }
    }
}

impl Panels {
    /// Which side a panel is on.
    #[must_use]
    pub fn side(&self, panel: Panel) -> Side {
        match panel {
            Panel::FileManager => self.files,
            Panel::Outline => self.outline,
            Panel::References => self.references,
            Panel::ProjectMap => self.map,
            Panel::Assistant => self.assistant,
        }
    }

    /// Whether a panel is on `side`.
    #[must_use]
    pub fn on(&self, panel: Panel, side: Side) -> bool {
        self.side(panel) == side
    }

    /// Put a panel on a side.
    pub fn move_to(&mut self, panel: Panel, side: Side) {
        match panel {
            Panel::FileManager => self.files = side,
            Panel::Outline => self.outline = side,
            Panel::References => self.references = side,
            Panel::ProjectMap => self.map = side,
            Panel::Assistant => self.assistant = side,
        }
    }

    /// The panels on one side, in the order they are stacked: the workspace above
    /// the agent, because a person opens a file to show it to an agent.
    #[must_use]
    pub fn stack(&self, side: Side) -> Vec<Panel> {
        [
            Panel::FileManager,
            Panel::Outline,
            Panel::References,
            Panel::ProjectMap,
            Panel::Assistant,
        ]
        .into_iter()
        .filter(|panel| self.on(*panel, side))
        .collect()
    }
}

/// The command palette, while it is up.
#[derive(Debug, Clone, Default)]
pub struct Palette {
    /// What has been typed to filter it.
    pub query: String,
    /// Which row is selected, in the filtered list.
    pub selected: usize,
}

/// What a menu row does.
///
/// Every variant is an existing path — a focus move, a pane toggle, a request — so
/// a menu row cannot become a second implementation of a command the palette and
/// the keys already have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Show the About card.
    About,
    /// Open the command palette.
    Palette,
    /// List the focused pane's keys.
    Keys,
    /// Re-read the daemon's state.
    Refresh,
    /// Put the keyboard in the tree.
    Files,
    /// Put the keyboard in the editor.
    Editor,
    /// Put the keyboard in the session list.
    Sessions,
    /// Read the transcript.
    Transcript,
    /// Attach a terminal, or leave the one that is open.
    Terminal,
    /// Open or close the source-control pane.
    SourceControl,
    /// Start a session with the first agent.
    NewSession,
    /// Cancel the turn in flight.
    Cancel,
    /// Show a permission request that was put aside with `Esc`.
    AnswerPermission,
    /// Put a panel on a side of the editor.
    ///
    /// One row per panel per side rather than a toggle, because the row is a place: a
    /// person reading a menu can see where the pane will land, and a row that said
    /// "move" would be a row that only made sense once it had been used.
    MovePanel(Panel, Side),
    /// Show the outline panel, or put it away.
    ToggleOutline,
    /// Show the references panel, or put it away.
    ToggleReferences,
    /// Show the project map, or put it away.
    ToggleProjectMap,
    /// List where the name under the caret is used.
    FindReferences,
    /// Ask to leave.
    Quit,
}

/// One row of a menu.
pub struct MenuRow {
    /// What the row says.
    pub label: &'static str,
    /// The chord that does the same thing, as a person would type it. Empty when
    /// the row is the only way to the action.
    pub keys: &'static str,
    /// What the row and the chord both run.
    pub action: MenuAction,
}

/// One menu on the bar: its title, the letter that opens it and its rows.
pub struct MenuDef {
    /// What the bar shows.
    pub title: &'static str,
    /// The letter `Alt` opens it with, underlined in the title the way a menu bar
    /// has named its own keys since before terminals had a mouse.
    pub mnemonic: char,
    /// Its rows, in order.
    pub items: &'static [MenuRow],
}

/// The menu bar's contents.
///
/// The same shell ideas the desktop's menus name, plus the panes only this frontend
/// has. A bar rather than a modal: the menus are visible without being asked for,
/// which is what makes them discoverable at all in a terminal.
pub const MENUS: &[MenuDef] = &[
    MenuDef {
        title: "File",
        mnemonic: 'f',
        items: &[
            MenuRow {
                label: "Command Palette…",
                keys: "ctrl-p",
                action: MenuAction::Palette,
            },
            MenuRow {
                label: "Refresh",
                keys: "ctrl-r",
                action: MenuAction::Refresh,
            },
            MenuRow {
                label: "Quit Acpilot",
                keys: "ctrl-c",
                action: MenuAction::Quit,
            },
        ],
    },
    MenuDef {
        title: "View",
        mnemonic: 'v',
        items: &[
            // The panes are reached by cycling, so the rows that move the keyboard
            // there name no chord a person could press from anywhere.
            MenuRow {
                label: "File manager",
                keys: "",
                action: MenuAction::Files,
            },
            MenuRow {
                label: "Editor",
                keys: "",
                action: MenuAction::Editor,
            },
            MenuRow {
                label: "Assistant",
                keys: "",
                action: MenuAction::Sessions,
            },
            MenuRow {
                label: "Transcript",
                keys: "f3",
                action: MenuAction::Transcript,
            },
            MenuRow {
                label: "Source Control",
                keys: "f6",
                action: MenuAction::SourceControl,
            },
            MenuRow {
                label: "Terminal Pane",
                keys: "ctrl-t",
                action: MenuAction::Terminal,
            },
            MenuRow {
                label: "Find References",
                keys: "shift-f12",
                action: MenuAction::FindReferences,
            },
        ],
    },
    MenuDef {
        title: "Panels",
        mnemonic: 'p',
        items: &[
            // Where each panel goes, not where it is: the same row moves it whether or
            // not it is already there, so a person who forgets does not have to look.
            MenuRow {
                label: "File manager on the left",
                keys: "",
                action: MenuAction::MovePanel(Panel::FileManager, Side::Left),
            },
            MenuRow {
                label: "File manager on the right",
                keys: "",
                action: MenuAction::MovePanel(Panel::FileManager, Side::Right),
            },
            MenuRow {
                label: "Assistant on the left",
                keys: "",
                action: MenuAction::MovePanel(Panel::Assistant, Side::Left),
            },
            MenuRow {
                label: "Assistant on the right",
                keys: "",
                action: MenuAction::MovePanel(Panel::Assistant, Side::Right),
            },
            MenuRow {
                label: "Outline on the left",
                keys: "",
                action: MenuAction::MovePanel(Panel::Outline, Side::Left),
            },
            MenuRow {
                label: "Outline on the right",
                keys: "",
                action: MenuAction::MovePanel(Panel::Outline, Side::Right),
            },
            MenuRow {
                label: "References on the left",
                keys: "",
                action: MenuAction::MovePanel(Panel::References, Side::Left),
            },
            MenuRow {
                label: "References on the right",
                keys: "",
                action: MenuAction::MovePanel(Panel::References, Side::Right),
            },
            MenuRow {
                label: "Show outline",
                keys: "",
                action: MenuAction::ToggleOutline,
            },
            MenuRow {
                label: "Show references",
                keys: "",
                action: MenuAction::ToggleReferences,
            },
            MenuRow {
                label: "Project map on the left",
                keys: "",
                action: MenuAction::MovePanel(Panel::ProjectMap, Side::Left),
            },
            MenuRow {
                label: "Project map on the right",
                keys: "",
                action: MenuAction::MovePanel(Panel::ProjectMap, Side::Right),
            },
            MenuRow {
                label: "Show project map",
                keys: "",
                action: MenuAction::ToggleProjectMap,
            },
        ],
    },
    MenuDef {
        title: "Session",
        mnemonic: 's',
        items: &[
            // `n` and `c` are the sessions pane's, and `n` is also the deny key on a
            // permission card. A chord in this bar promises it works from wherever the
            // keyboard is, so a pane's key is named nowhere.
            MenuRow {
                label: "New Session",
                keys: "",
                action: MenuAction::NewSession,
            },
            MenuRow {
                label: "Cancel Turn",
                keys: "",
                action: MenuAction::Cancel,
            },
            // `Esc` puts the card away, and a question put away has to be
            // reachable again: this row is where a person finds it.
            MenuRow {
                label: "Answer Permission…",
                keys: "",
                action: MenuAction::AnswerPermission,
            },
        ],
    },
    MenuDef {
        title: "Help",
        mnemonic: 'h',
        items: &[
            MenuRow {
                label: "About Acpilot",
                keys: "",
                action: MenuAction::About,
            },
            MenuRow {
                label: "Keys for this pane",
                keys: "?",
                action: MenuAction::Keys,
            },
        ],
    },
];

/// Everything the About card says, in one place a test can read.
///
/// The same four rows the desktop's About prints — the facts a bug report asks for —
/// from the same version constant the CLI and the daemon report.
#[must_use]
pub fn about_lines() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Version", acpilot_core::VERSION),
        ("License", "MIT OR Apache-2.0"),
        ("Field manual", "rudeboy.dev/acpilot"),
        ("Built with", "Freebuff — freebuff.com"),
    ]
}

/// The menu bar while it has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuState {
    /// Which title is highlighted.
    pub open: usize,
    /// Whether its list is showing.
    pub items: bool,
    /// Which row of that list is highlighted.
    pub at: usize,
}

/// A notice about something that is designed and not built.
///
/// The task id is the point: "not implemented" is a dead end, "P6-6" is a
/// sentence someone can act on. It lives in the app rather than as a `Request`
/// because both backends — daemon and design pass — answer the same way, and a
/// round trip to say "no" would be a round trip for nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// The task that would build it, e.g. `P6-6`.
    pub task: &'static str,
    /// What the person tried to do.
    pub feature: String,
}

/// How a transcript line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// What the person typed.
    User,
    /// What the agent said.
    Assistant,
    /// The agent's reasoning.
    Thought,
    /// A tool the agent called.
    Tool,
    /// A note from the daemon or the agent.
    Notice,
}

/// One paragraph of the transcript.
///
/// Chunks are appended to the entry before them when the kind matches, so a
/// streamed message is one paragraph rather than one line per token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// How to style it.
    pub kind: Kind,
    /// Its text, which may contain newlines.
    pub text: String,
    /// A tool call's identity and result, when the entry is one.
    ///
    /// Apart from `text` because a second event about the same call — `pending`
    /// then `completed` — is one row whose status moved, not two rows. The agent
    /// names the call, so the name is what the second event is matched on.
    pub tool: Option<ToolCall>,
    /// Whether a tool call's detail is hidden.
    ///
    /// Folded by default: the header says which tool ran, and a transcript that
    /// printed every result in full is a conversation nobody can find the reply
    /// in. `Enter` in the transcript opens the one under the cursor.
    pub folded: bool,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            kind: Kind::Assistant,
            text: String::new(),
            tool: None,
            folded: false,
        }
    }
}

/// What a tool call said, kept so its row can be updated and opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The agent's id for the call, which is what a status update names.
    pub id: String,
    /// Where it is: `pending`, `completed`, or whatever the agent called it.
    pub status: String,
    /// What it did, in the agent's words.
    pub title: String,
    /// Its result, when the agent sent one. Shown only when the row is unfolded.
    pub detail: String,
}

/// One drawn line of the transcript.
///
/// The pane, a yank and a find all read this list, so the cells a person
/// highlights and the text they copy cannot disagree — and the markdown is
/// rendered once, in the one place that owns the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRow {
    /// How the entry it came from is styled.
    pub kind: Kind,
    /// Its runs, already marked up.
    pub segments: Vec<crate::markdown::Segment>,
    /// Which entry it came from, so a key can fold what the cursor is on.
    pub entry: usize,
}

impl TranscriptRow {
    /// The row as plain text, which is what a yank and a find read.
    #[must_use]
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }
}

/// The `@path` completion the composer is showing.
///
/// The rows are the daemon's answer to a walk, not a local listing: the files
/// are where the workspace is, and an `@` that offered what this process could
/// see would be a prompt about a different machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// What was typed after the `@`.
    pub query: String,
    /// Paths that match, best first, already capped.
    pub rows: Vec<String>,
    /// Which row `Enter` or `Tab` takes.
    pub selected: usize,
}

/// How many lines of history a pane keeps behind its live screen.
///
/// A pane is where a build prints a thousand lines and the one that matters
/// scrolled past; the visible screen alone is what sends a person back to their
/// own terminal. Ten thousand lines is a few megabytes of grid and more than a
/// session's worth of output.
const SCROLLBACK: usize = 10_000;

/// What the status line says when a pane starts being read.
const COPY_HINT: &str = "copy mode · j/k scroll · / find · v/V select · y copy · Esc leaves";

/// What the status line says while a query is being typed.
const SEARCH_HINT: &str = "find · type a pattern · Enter searches back · Esc cancels";

/// Whether a pane is following the program or being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneMode {
    /// New output moves the screen and keys go to the program.
    Live,
    /// The view can be scrolled and selected, and the keys are the pane's.
    Copy,
}

/// A pane being read: where the cursor is, what is selected, how far back.
///
/// The terminal and the transcript are different media — cells parsed from a
/// program's bytes, lines folded from the agent's events — but reading them is one
/// gesture, so it is one type. Nothing here knows what it is reading: the pane that
/// owns it answers how big the view is and how much history there is, which is what
/// makes the same keys, the same corners and the same yank work in both.
///
/// A line here is absolute — counted from the oldest line the pane still holds, not
/// from the top of the screen. That is what lets a selection keep the text it was
/// put on while output arrives, the wheel scrolls, or a drag at an edge reads
/// further back than one screen: only [`CopyView::scroll`] says which lines are on
/// screen, and the yank can take lines the screen no longer shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyView {
    /// Where the cursor is: its line counted from the oldest line the pane keeps,
    /// and its column in cells.
    pub cursor: (usize, u16),
    /// Where a selection started, on the same absolute line.
    pub anchor: Option<(usize, u16)>,
    /// Whether the selection takes whole lines rather than the cells between its
    /// two ends.
    ///
    /// `V` rather than a second pane: a build's output and an agent's answer are
    /// read in lines, and marking one character at a time to reach the end of a
    /// path or a timestamp is work the pane can do.
    pub line: bool,
    /// How many lines behind the live view the pane sits. Zero is live.
    pub scroll: usize,
}

impl CopyView {
    /// A cursor standing still on the live view.
    fn new(cursor: (usize, u16)) -> Self {
        Self {
            cursor,
            anchor: None,
            line: false,
            scroll: 0,
        }
    }

    /// The selection, in reading order: from the earlier cell to the later one.
    fn selection(&self) -> Option<((usize, u16), (usize, u16))> {
        let anchor = self.anchor?;
        let cursor = self.cursor;
        Some(if (anchor.0, anchor.1) <= (cursor.0, cursor.1) {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        })
    }

    /// The rows a line-wise selection covers, in reading order.
    fn line_selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        Some(if anchor.0 <= self.cursor.0 {
            (anchor.0, self.cursor.0)
        } else {
            (self.cursor.0, anchor.0)
        })
    }

    /// Whether a cell is inside the selection.
    ///
    /// A line-wise selection covers every cell of its rows, including the blanks
    /// past the end of a line: a row that was marked is a row the yank takes.
    #[must_use]
    pub fn selected(&self, line: usize, col: u16) -> bool {
        if self.line {
            return self
                .line_selection()
                .is_some_and(|(start, end)| line >= start && line <= end);
        }
        self.selection()
            .is_some_and(|(start, end)| (line, col) >= start && (line, col) <= end)
    }

    /// Start a selection at the cursor, or drop the one in progress.
    ///
    /// `v` is the character-wise one, so it also leaves a line-wise selection: the
    /// keys are two answers to "what am I marking", not two modes to remember.
    fn toggle_anchor(&mut self) {
        self.anchor = match self.anchor {
            Some(_) => None,
            None => Some(self.cursor),
        };
        self.line = false;
    }

    /// Start a line-wise selection at the cursor, or drop the one in progress.
    fn toggle_line(&mut self) {
        if self.line {
            self.line = false;
            self.anchor = None;
        } else {
            self.line = true;
            self.anchor = Some(self.cursor);
        }
    }

    /// Move the cursor in a `grid` whose history is `max_scroll` lines deep.
    ///
    /// Scrolling at the edges is what lets one set of keys reach the whole of a
    /// pane's history: a cursor that could walk off the screen would be a cursor
    /// nobody can see, and a cursor kept in screen rows would forget which line it
    /// was on as soon as output scrolled past it.
    fn move_by(&mut self, rows: i32, cols: i32, grid: (u16, u16), max_scroll: usize) {
        let (height, width) = grid;
        let last_line = max_scroll
            .saturating_add(usize::from(height))
            .saturating_sub(1);
        let last_col = width.saturating_sub(1);
        let (line, mut col) = self.cursor;
        let step = |delta: i32| u16::try_from(delta.unsigned_abs()).unwrap_or(u16::MAX);

        if cols < 0 {
            col = col.saturating_sub(step(cols));
        } else {
            col = col.saturating_add(step(cols)).min(last_col);
        }

        let line = if rows < 0 {
            line.saturating_sub(rows.unsigned_abs() as usize)
        } else {
            line.saturating_add(rows as usize).min(last_line)
        };

        self.cursor = (line, col);
        self.reveal(grid, max_scroll);
        self.clamp(grid, max_scroll);
    }

    /// The first line on screen, counted from the oldest line the pane keeps.
    fn first(&self, max_scroll: usize) -> usize {
        max_scroll.saturating_sub(self.scroll.min(max_scroll))
    }

    /// Scroll the least that puts the cursor back on screen.
    fn reveal(&mut self, grid: (u16, u16), max_scroll: usize) {
        let height = usize::from(grid.0.max(1));
        let first = self.first(max_scroll);
        let line = self.cursor.0;
        if line < first {
            self.scroll = max_scroll.saturating_sub(line).min(max_scroll);
        } else if line >= first.saturating_add(height) {
            let first = line.saturating_add(1).saturating_sub(height);
            self.scroll = max_scroll.saturating_sub(first).min(max_scroll);
        }
    }

    /// Put the view on `line`, with the line at the top and the cursor at `col`.
    ///
    /// A match at the bottom of a full screen is a match with three lines of
    /// context and no more; the top of the view is where a person looks after
    /// asking for something.
    fn jump_to(&mut self, line: usize, col: u16, max_scroll: usize, grid: (u16, u16)) {
        let last_line = max_scroll
            .saturating_add(usize::from(grid.0))
            .saturating_sub(1);
        self.cursor = (line.min(last_line), col.min(grid.1.saturating_sub(1)));
        self.scroll = max_scroll.saturating_sub(line).min(max_scroll);
    }

    /// Keep the cursor, the selection and the view inside a grid that changed
    /// shape, or a history that turned out shorter than the view.
    fn clamp(&mut self, grid: (u16, u16), max_scroll: usize) {
        self.scroll = self.scroll.min(max_scroll);
        let last_line = max_scroll
            .saturating_add(usize::from(grid.0))
            .saturating_sub(1);
        let last_col = grid.1.saturating_sub(1);
        self.cursor = (self.cursor.0.min(last_line), self.cursor.1.min(last_col));
        if let Some(anchor) = self.anchor {
            self.anchor = Some((anchor.0.min(last_line), anchor.1.min(last_col)));
        }
    }
}

/// What a copy-mode keystroke means, in either pane.
///
/// The terminal and the transcript read the same way, so they read the same keys:
/// one parser, so a motion added to one cannot quietly not exist in the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyKey {
    /// Leave copy mode and give the keyboard back.
    Leave,
    /// Start a selection at the cursor, or drop the one in progress.
    Select,
    /// Start a whole-line selection at the cursor, or drop the one in progress.
    SelectLine,
    /// Move the cursor, scrolling when it reaches an edge.
    Move { rows: i32, cols: i32 },
    /// The oldest line in the view.
    Top,
    /// Back to the live view.
    Live,
    /// Put the selection on the clipboard.
    Yank,
    /// Open a line for a search.
    Find,
    /// The next match, older.
    Older,
    /// The next match, newer.
    Newer,
    /// A key copy mode has no use for.
    Nothing,
}

impl CopyKey {
    /// Read a key, with `page` being how many rows a screenful is in this pane.
    fn parse(key: KeyEvent, page: i32) -> Self {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Self::Leave,
            KeyCode::Char('j') | KeyCode::Down => Self::Move { rows: 1, cols: 0 },
            KeyCode::Char('k') | KeyCode::Up => Self::Move { rows: -1, cols: 0 },
            KeyCode::Char('h') | KeyCode::Left => Self::Move { rows: 0, cols: -1 },
            KeyCode::Char('l') | KeyCode::Right => Self::Move { rows: 0, cols: 1 },
            KeyCode::Char('d') if control => Self::Move {
                rows: page,
                cols: 0,
            },
            KeyCode::Char('u') if control => Self::Move {
                rows: -page,
                cols: 0,
            },
            KeyCode::PageDown => Self::Move {
                rows: page,
                cols: 0,
            },
            KeyCode::PageUp => Self::Move {
                rows: -page,
                cols: 0,
            },
            KeyCode::Char('g') => Self::Top,
            KeyCode::Char('G') => Self::Live,
            KeyCode::Char('v') => Self::Select,
            KeyCode::Char('V') => Self::SelectLine,
            KeyCode::Char('y') => Self::Yank,
            KeyCode::Char('/') => Self::Find,
            KeyCode::Char('n') => Self::Older,
            KeyCode::Char('N') => Self::Newer,
            _ => Self::Nothing,
        }
    }
}

/// One place a search found its query.
///
/// The line is absolute — counted from the oldest line the history still holds —
/// rather than a row on screen, because the view moves independently of the match
/// and a hit has to stay on the same line while it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// The line, counted from the oldest line the scrollback kept.
    pub line: usize,
    /// The column it starts at.
    pub col: u16,
    /// How many cells it covers.
    pub len: u16,
}

/// What copy mode is looking for, and what it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Find {
    /// What was typed after `/`.
    pub query: String,
    /// Every match, oldest first.
    pub hits: Vec<Hit>,
    /// Which of them the cursor is sitting on.
    pub at: usize,
}

/// What a key did to the line a query is being typed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryKey {
    /// No line is open: the key belongs to the pane.
    Ignored,
    /// The line is still being typed.
    Typed,
    /// `Enter`: the line is closed and what it holds should be run.
    Run,
    /// `Esc`: the line is closed and nothing should be run.
    Cancelled,
}

/// A pane being read: the cursor, the selection, and the find.
///
/// The terminal and the transcript are different media, but reading them is one
/// gesture, so the motions, the corners and the search live here once. A pane that
/// owns a reader only has to say how big its view is, how many lines of history
/// there are, and how it produces the text a search reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reader {
    /// Where the cursor and the selection are.
    pub view: CopyView,
    /// The query being typed after `/`, before Enter runs it.
    pub typing: Option<String>,
    /// The matches the last search found, and which one is on the cursor.
    pub find: Option<Find>,
}

impl Reader {
    /// A cursor standing still on the live view.
    fn new(cursor: (usize, u16)) -> Self {
        Self {
            view: CopyView::new(cursor),
            ..Self::default()
        }
    }

    /// Open a line for a query.
    fn begin_search(&mut self) {
        self.typing = Some(String::new());
    }

    /// Feed a key to the query line, if one is open.
    ///
    /// `Enter` leaves the query where it is for the caller to run, because only the
    /// pane knows what to search through; `Esc` gives the line up.
    fn type_key(&mut self, key: KeyEvent) -> QueryKey {
        if self.typing.is_none() {
            return QueryKey::Ignored;
        }
        match key.code {
            KeyCode::Enter => QueryKey::Run,
            KeyCode::Esc => {
                self.typing = None;
                QueryKey::Cancelled
            }
            KeyCode::Backspace => {
                if let Some(query) = self.typing.as_mut() {
                    query.pop();
                }
                QueryKey::Typed
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(query) = self.typing.as_mut() {
                    query.push(character);
                }
                QueryKey::Typed
            }
            // While a query is open every key is the query's: a search box that let
            // motions through would move the view out from under the pattern.
            _ => QueryKey::Typed,
        }
    }

    /// Find `query` through `lines`, oldest first, and put the cursor on the match a
    /// person most likely means.
    ///
    /// That is the newest match at or before where they were reading, and the
    /// newest of all when there is nothing before it: reading back through a build's
    /// output or an agent's answer is the question a search here answers, and the
    /// line under the cursor is the first place to look. Returns how many matches
    /// there were.
    ///
    /// Case follows the query: lowercase finds either case, one capital means the
    /// person typed it on purpose. See [`case_sensitive`].
    fn search(
        &mut self,
        query: &str,
        lines: &[String],
        max_scroll: usize,
        grid: (u16, u16),
    ) -> usize {
        let needle: Vec<char> = query.chars().collect();
        let sensitive = case_sensitive(query);
        let mut hits = Vec::new();
        if !needle.is_empty() {
            for (line, text) in lines.iter().enumerate() {
                let haystack: Vec<char> = text.chars().collect();
                if haystack.len() < needle.len() {
                    continue;
                }
                for start in 0..=haystack.len() - needle.len() {
                    // Character at a time rather than lowercased strings: a fold can
                    // change a string's length (`İ` is two characters lowercased), and
                    // the column a match is found at has to be the column it is drawn
                    // at.
                    let found = haystack[start..start + needle.len()]
                        .iter()
                        .zip(&needle)
                        .all(|(hay, wanted)| {
                            if sensitive {
                                hay == wanted
                            } else {
                                hay.to_lowercase().eq(wanted.to_lowercase())
                            }
                        });
                    if found {
                        hits.push(Hit {
                            line,
                            col: u16::try_from(start).unwrap_or(u16::MAX),
                            len: u16::try_from(needle.len()).unwrap_or(u16::MAX),
                        });
                    }
                }
            }
        }
        let total = hits.len();
        self.find = None;
        if total == 0 {
            return 0;
        }
        let cursor = self.view.cursor.0;
        let at = hits
            .iter()
            .rposition(|hit| hit.line <= cursor)
            .unwrap_or(total - 1);
        self.find = Some(Find {
            query: query.to_owned(),
            hits,
            at,
        });
        self.jump(max_scroll, grid);
        total
    }

    /// Put the view on the match the search is sitting on.
    fn jump(&mut self, max_scroll: usize, grid: (u16, u16)) {
        let Some(hit) = self
            .find
            .as_ref()
            .and_then(|find| find.hits.get(find.at))
            .copied()
        else {
            return;
        };
        self.view.jump_to(hit.line, hit.col, max_scroll, grid);
    }

    /// Move to the next match, older or newer, wrapping at the ends, and say which
    /// one the view is on.
    fn step_hit(
        &mut self,
        older: bool,
        max_scroll: usize,
        grid: (u16, u16),
    ) -> Option<(usize, usize)> {
        let total = self.find.as_ref()?.hits.len();
        if total == 0 {
            return Some((0, 0));
        }
        let at = self.find.as_ref()?.at;
        let next = if older {
            (at + total - 1) % total
        } else {
            (at + 1) % total
        };
        if let Some(find) = self.find.as_mut() {
            find.at = next;
        }
        self.jump(max_scroll, grid);
        Some((next + 1, total))
    }

    /// Whether a cell is part of the match the search is sitting on.
    fn matched(&self, line: usize, col: u16) -> bool {
        let Some(hit) = self.find.as_ref().and_then(|find| find.hits.get(find.at)) else {
            return false;
        };
        hit.line == line && col >= hit.col && col < hit.col.saturating_add(hit.len)
    }
}

/// A terminal a person is looking at.
///
/// The screen is parsed here rather than in the daemon's frames because a pane is
/// a view: the bytes are the truth, and the cells are what a person can read. The
/// parser is also where a resize takes effect, so the pane and the program agree
/// on how wide a line is.
pub struct TerminalPane {
    /// What the daemon says it is: id, command, size, and whether it is running.
    pub summary: TerminalSummary,
    /// The screen, as drawn by the bytes the daemon sent.
    pub parser: vt100::Parser,
    /// Whether the program is still running.
    pub alive: bool,
    /// How it ended, once it has. `Some(None)` is "ended, no code reported".
    pub exit: Option<Option<i32>>,
    /// Following the program, or reading behind it.
    pub mode: PaneMode,
    /// The cursor, the selection and the find, while the pane is read.
    pub reader: Reader,
}

impl std::fmt::Debug for TerminalPane {
    // The parser holds a grid of cells and prints as nothing useful; a debug dump
    // that showed it would bury the fields a failure is actually about.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalPane")
            .field("summary", &self.summary)
            .field("alive", &self.alive)
            .field("exit", &self.exit)
            .field("mode", &self.mode)
            .field("reader", &self.reader)
            .finish_non_exhaustive()
    }
}

impl TerminalPane {
    fn new(summary: TerminalSummary, cols: u16, rows: u16) -> Self {
        Self {
            summary,
            parser: vt100::Parser::new(screen_rows(rows), screen_cols(cols), SCROLLBACK),
            alive: true,
            exit: None,
            mode: PaneMode::Live,
            reader: Reader::new((0, 0)),
        }
    }

    /// The pane's cell grid: rows, then columns.
    fn grid_size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (rows.get(), cols.get())
    }

    /// The lines of history behind the live screen.
    #[must_use]
    pub fn history(&self) -> usize {
        self.parser.screen().scrollback_len()
    }

    /// Move the view `offset` lines behind the live screen, clamped to what there
    /// is.
    ///
    /// The parser's own offset is what makes `cell` and `contents_between` read
    /// the scrolled rows, so the view and the yank cannot disagree about what is
    /// on screen.
    fn set_scroll(&mut self, offset: usize) {
        self.reader.view.scroll = offset;
        self.apply_scroll();
    }

    /// Put the parser at the offset the view says it is drawn at.
    fn apply_scroll(&mut self) {
        self.reader.view.scroll = self.reader.view.scroll.min(self.history());
        self.parser
            .screen_mut()
            .set_scrollback(self.reader.view.scroll);
    }

    /// Read back through the program's output, starting where its cursor is.
    ///
    /// The bottom line would be the obvious place to start, but a pane is usually
    /// a program that printed its answer and a prompt that has not echoed anything
    /// yet: the thing worth copying is at the cursor, not under it.
    fn begin_copy(&mut self) {
        let (rows, cols) = self.grid_size();
        let (row, col) = self.parser.screen().cursor_position();
        self.mode = PaneMode::Copy;
        self.reader = Reader::new((
            self.history() + usize::from(row.min(rows.saturating_sub(1))),
            col.min(cols.saturating_sub(1)),
        ));
        self.apply_scroll();
    }

    /// The line a cell of the view is on, counted from the oldest line kept.
    fn absolute(&self, (row, col): (u16, u16)) -> (usize, u16) {
        let history = self.history();
        let first = history.saturating_sub(self.reader.view.scroll.min(history));
        (first + usize::from(row), col)
    }

    /// Start a selection at the cell under the pointer, entering copy mode.
    ///
    /// A pointer has no key to press first, so the press is also what enters copy
    /// mode: a person dragging across a shell's output means the output, not the
    /// program. The view stays where it is — a click on a scrolled pane selects
    /// the cells it landed on, not the live ones behind them — and the anchor and
    /// the cursor are the same cell until the pointer moves, which is what lets a
    /// plain click be told from a drag.
    pub fn select_at(&mut self, cell: (u16, u16)) {
        let cell = self.absolute(self.cell_between(cell));
        let scroll = self.reader.view.scroll;
        self.mode = PaneMode::Copy;
        self.reader = Reader::new(cell);
        self.reader.view.scroll = scroll;
        self.reader.view.anchor = Some(cell);
        self.apply_scroll();
    }

    /// Grow the selection to the cell under the pointer.
    ///
    /// A pointer that has left the pane still extends the selection, to the edge
    /// it left by: a drag that overshoots is a person marking everything to the
    /// end of the screen, not one giving up. The view does not scroll — a drag
    /// that scrolled at the edges would be a drag that changed what its own
    /// earlier cells meant, which is the one thing a selection must not do.
    pub fn extend_to(&mut self, cell: (u16, u16)) {
        if self.reader.view.anchor.is_none() {
            return;
        }
        self.reader.view.cursor = self.absolute(self.cell_between(cell));
    }

    /// Whether the pane is holding a selection, rather than a bare cursor.
    #[must_use]
    pub fn has_selection(&self) -> bool {
        if self.reader.view.line {
            return self.reader.view.anchor.is_some();
        }
        self.reader
            .view
            .selection()
            .is_some_and(|(start, end)| start != end)
    }

    /// How many lines the selection covers, counting both ends.
    #[must_use]
    pub fn selected_lines(&self) -> usize {
        if self.reader.view.line {
            return self
                .reader
                .view
                .line_selection()
                .map_or(0, |(start, end)| end.saturating_sub(start) + 1);
        }
        self.reader
            .view
            .selection()
            .map_or(0, |(start, end)| end.0.saturating_sub(start.0) + 1)
    }

    /// A cell `vt100` would answer, clamped to the grid it is drawing.
    fn cell_between(&self, (row, col): (u16, u16)) -> (u16, u16) {
        let (rows, cols) = self.grid_size();
        (
            row.min(rows.saturating_sub(1)),
            col.min(cols.saturating_sub(1)),
        )
    }

    /// Back to following the program.
    pub fn end_copy(&mut self) {
        self.mode = PaneMode::Live;
        self.reader.view = CopyView::new((self.history(), 0));
        self.reader.typing = None;
        self.apply_scroll();
    }

    /// The most lines the view can hold: the history, plus the screen in front of
    /// it.
    #[must_use]
    pub fn scrollable(&self) -> usize {
        self.history()
    }

    /// Move the copy cursor, scrolling when it reaches an edge.
    fn copy_move(&mut self, rows: i32, cols: i32) {
        let grid = self.grid_size();
        self.reader.view.move_by(rows, cols, grid, self.history());
        self.apply_scroll();
    }

    /// Put the oldest line in the view.
    fn scroll_to_top(&mut self) {
        self.reader.view.cursor.0 = 0;
        self.set_scroll(self.scrollable());
    }

    /// Back to the live screen, without leaving copy mode.
    fn scroll_to_live(&mut self) {
        let (rows, _) = self.grid_size();
        self.reader.view.cursor.0 = self
            .history()
            .saturating_add(usize::from(rows))
            .saturating_sub(1);
        self.set_scroll(0);
    }

    /// Scroll the view a wheel's worth, which is what a terminal emulator does.
    ///
    /// Positive is older output. The copy cursor stays where it is: a wheel is not
    /// a motion key, and a selection must not move under a hand that is only
    /// reading. The offset is the one the view, the yank and the find all read, so
    /// what is scrolled is what would be copied, and it clamps at the oldest line
    /// rather than scrolling into blank rows.
    pub fn wheel(&mut self, lines: i32) {
        let scroll = if lines >= 0 {
            self.reader
                .view
                .scroll
                .saturating_add(lines.unsigned_abs() as usize)
        } else {
            self.reader
                .view
                .scroll
                .saturating_sub(lines.unsigned_abs() as usize)
        };
        self.set_scroll(scroll);
    }

    /// Scroll the view under a drag that is holding a selection.
    ///
    /// Positive is older output, the same way the wheel reads. The selection is
    /// kept in lines of the history and columns of cells, so scrolling the view
    /// under it cannot move what it marks: the two ends travel with the text they
    /// were put on, and a drag at an edge can read back past the screen it began
    /// on. Returns whether the view actually moved, so a drag at the oldest line
    /// stops rather than redrawing for a scroll there is no more of.
    pub fn scroll_selection(&mut self, lines: i32) -> bool {
        if self.reader.view.anchor.is_none() || lines == 0 {
            return false;
        }
        let before = self.reader.view.scroll;
        self.wheel(lines);
        self.reader.view.scroll != before
    }

    /// Back to the newest line, which is where answering a program puts the view.
    ///
    /// Reading behind a program is not a mode here, so the first keystroke is also
    /// the one that ends it: a command typed into a scrolled pane would otherwise
    /// print its answer out of sight.
    pub fn follow(&mut self) {
        self.set_scroll(0);
    }

    /// The cells of one history line, at the offset that line is visible at.
    ///
    /// The parser only exposes the screen it is showing, so reading an arbitrary
    /// line is one temporary scroll. `line` is absolute, so the offset is how many
    /// lines the history holds above it; a line on the live screen is read at
    /// offset zero. The view's own offset is put back, because a yank must not
    /// move the screen it was taken from.
    fn line_cells(&mut self, line: usize, from: u16, to: u16) -> String {
        let history = self.history();
        let (rows, cols) = self.grid_size();
        let offset = history.saturating_sub(line);
        let first = history - offset;
        let Some(row) = line
            .checked_sub(first)
            .and_then(|row| u16::try_from(row).ok())
            .filter(|row| *row < rows)
        else {
            return String::new();
        };
        let was = self.reader.view.scroll;
        self.parser.screen_mut().set_scrollback(offset);
        let text = self
            .parser
            .screen()
            .contents_between(row, from.min(cols), row, to.min(cols));
        self.parser.screen_mut().set_scrollback(was);
        text
    }

    /// The text under the selection, or the cursor's line when nothing is
    /// selected.
    ///
    /// A selection is held in lines of the history, so it can cover lines the
    /// screen has scrolled past; each of those is read at the offset it is visible
    /// at, which is what lets a drag at an edge take more than one screen.
    #[must_use]
    pub fn yank(&mut self) -> String {
        let (_, width) = self.grid_size();
        // Whole lines take the row from the left edge to the last non-blank cell:
        // a line is what a person marked, and the padding after it is not.
        if self.reader.view.line
            && let Some((start, end)) = self.reader.view.line_selection()
        {
            let mut lines = Vec::new();
            for line in start..=end {
                lines.push(self.line_cells(line, 0, width).trim_end().to_owned());
            }
            return lines.join("\n");
        }
        let (start, end) = match self.reader.view.selection() {
            Some(selection) => selection,
            None => {
                let line = self.reader.view.cursor.0;
                ((line, 0), (line, width.saturating_sub(1)))
            }
        };
        // `contents_between` stops before `end_col`, and the cell under the cursor
        // is part of what a person selected, so it asks for one past it. Trailing
        // blanks are padding, not text: a line yanked from a terminal is pasted
        // into a prompt more often than into a file.
        let mut text = String::new();
        for line in start.0..=end.0 {
            if line > start.0 {
                text.push('\n');
            }
            let from = if line == start.0 { start.1 } else { 0 };
            let to = if line == end.0 {
                end.1.saturating_add(1).min(width)
            } else {
                width
            };
            text.push_str(&self.line_cells(line, from, to));
        }
        text.trim_end().to_owned()
    }

    /// The history as text, oldest line first.
    ///
    /// `vt100` exposes the visible screen, and the scrollback offset is the one
    /// handle on the lines behind it — each offset reveals one line that was not in
    /// view before, at the top. So the whole history is one pass of offsets rather
    /// than a screen read per line; offset zero is the live screen and contributes
    /// all of it. The parser is left where it was.
    fn lines(&mut self) -> Vec<String> {
        let (rows, cols) = self.grid_size();
        let history = self.history();
        let was = self.reader.view.scroll;
        let mut lines = Vec::with_capacity(history + usize::from(rows));
        for offset in (0..=history).rev() {
            self.parser.screen_mut().set_scrollback(offset);
            let screen = self.parser.screen();
            if offset == 0 {
                lines.extend(screen.rows(0, cols));
            } else if let Some(line) = screen.rows(0, cols).next() {
                lines.push(line);
            }
        }
        self.parser.screen_mut().set_scrollback(was);
        lines
    }

    /// Find `query` in the history, and put the view on the match a person most
    /// likely means.
    ///
    /// It is at most [`SCROLLBACK`] rows — about 50 ms unoptimised over ten thousand
    /// of them, and an order of magnitude less in a release build — and it happens
    /// when Enter is pressed, not on every frame. Returns how many matches there
    /// were.
    fn search(&mut self, query: &str) -> usize {
        let lines = self.lines();
        let grid = self.grid_size();
        let total = self.reader.search(query, &lines, self.history(), grid);
        self.apply_scroll();
        total
    }

    /// Move to the next match, older or newer, wrapping at the ends, and say which
    /// one the view is on.
    fn step_hit(&mut self, older: bool) -> Option<(usize, usize)> {
        let grid = self.grid_size();
        let stepped = self.reader.step_hit(older, self.history(), grid);
        self.apply_scroll();
        stepped
    }

    /// Whether a cell is part of the match the search is sitting on.
    #[must_use]
    pub fn matched(&self, line: usize, col: u16) -> bool {
        self.reader.matched(line, col)
    }
}

/// How many lines one notch of the wheel scrolls.
///
/// Three, which is what a terminal gives a page-down key in some of them and what
/// a person expects from a notch: one line at a time reads as a wheel that is
/// broken, a page at a time as one that jumped.
const WHEEL_LINES: i32 = 3;

/// The columns the two dividers are drawn at.
///
/// A drag has to become a width, and a width is measured from the body's edges — so
/// the drawing hands back where the edges and the dividers are instead of the app
/// repeating the layout arithmetic and getting it subtly wrong.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dividers {
    /// The tree's right border, when the tree is on screen.
    pub tree: Option<u16>,
    /// The sidebar's left border, when the sidebar is on screen.
    pub sidebar: Option<u16>,
    /// The body's left edge.
    pub left: u16,
    /// The body's right edge.
    pub right: u16,
}

/// A divider a mouse is holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divider {
    /// Between the tree and the editor.
    Tree,
    /// Between the editor and the sidebar.
    Sidebar,
}

/// What the left button is holding, between pressing it and letting go.
///
/// Two gestures, because a pointer is better at two things a key cannot do: aim at
/// a width, and mark a run of cells. Which one a press began is decided once, at
/// the press, so a drag that wanders off the thing it started on keeps doing what
/// it was doing rather than turning into the other gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drag {
    /// A divider, being moved.
    Divider(Divider),
    /// The terminal pane's selection, being grown.
    Terminal,
    /// The editor pane's selection, being grown.
    Editor,
}

/// Where a pane was drawn, in cells, for the mouse.
///
/// The box is the pane's inner cells, border excluded, in the window's coordinates.
/// The drawing code hands it back rather than the mouse repeating the layout
/// arithmetic: a pane's cell and the window's cell are one subtraction apart, and a
/// second copy of that subtraction is a bug waiting for the first split.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneArea {
    /// The first cell inside the border.
    pub x: u16,
    /// The first row inside the border.
    pub y: u16,
    /// How many cells wide.
    pub width: u16,
    /// How many lines fit.
    pub height: u16,
}

impl PaneArea {
    /// Whether a cell is inside the pane.
    #[must_use]
    pub fn holds(&self, column: u16, row: u16) -> bool {
        column >= self.x
            && column < self.x.saturating_add(self.width)
            && row >= self.y
            && row < self.y.saturating_add(self.height)
            && self.width > 0
            && self.height > 0
    }
}

/// Where the editor's text was drawn, in cells, for the mouse.
///
/// `pane` is the box and `gutter` is how many of its cells are numbers and markers:
/// a click in the text is a caret, and a click on the numbers is the pane's
/// furniture. A wheel anywhere in the box scrolls, because a wheel is pointed at a
/// pane rather than aimed at a character.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditorArea {
    /// The pane's inner cells.
    pub pane: PaneArea,
    /// The columns the numbers and markers take before the text.
    pub gutter: u16,
}

impl EditorArea {
    /// Whether a cell is inside the text — the part a click can put a caret in.
    #[must_use]
    pub fn is_text(&self, column: u16, row: u16) -> bool {
        self.pane.holds(column, row)
            && self.pane.width > self.gutter
            && column >= self.pane.x.saturating_add(self.gutter)
    }

    /// Whether a cell is anywhere in the pane, the gutter included.
    #[must_use]
    pub fn holds(&self, column: u16, row: u16) -> bool {
        self.pane.holds(column, row)
    }
}

/// Where the open menu's list was drawn, in cells, for the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuList {
    /// Its left column, which is the border.
    pub x: u16,
    /// Its top row, which is the border.
    pub y: u16,
    /// Its width, borders included.
    pub width: u16,
    /// How many rows it shows inside the border.
    pub rows: u16,
}

/// Where the menu bar and its list were drawn, so a click can be answered.
///
/// The same shape as [`Dividers`]: the drawing is the only thing that knows where
/// anything is, so it says, rather than the keys repeating the layout arithmetic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuAreas {
    /// The bar's row.
    pub row: u16,
    /// The open list, when one was drawn.
    pub list: Option<MenuList>,
}

/// The cells a menu's title takes on the bar: where it starts and how wide it is.
///
/// The bar is one string of fixed labels, so this is arithmetic rather than a second
/// layout pass — and it is here, next to [`MENUS`], because the drawing, the
/// dropdown and the mouse all have to agree about it.
#[must_use]
pub fn menu_title(index: usize) -> (u16, u16) {
    // The brand is nine cells, then each title is `" Name "` and one space after it.
    // The title's cell starts one before the name, and the name is what the list
    // lines up with.
    let brand = 10;
    let start = MENUS.iter().take(index).fold(brand, |x, menu| {
        x + u16::try_from(menu.title.chars().count()).unwrap_or(0) + 3
    });
    let title = MENUS.get(index).map_or("", |menu| menu.title);
    let width = u16::try_from(title.chars().count() + 2).unwrap_or(0);
    (start.saturating_sub(1), width)
}

/// The menu `Alt` plus a letter opens, by its mnemonic.
#[must_use]
fn menu_for_mnemonic(letter: char) -> Option<usize> {
    let letter = letter.to_ascii_lowercase();
    MENUS
        .iter()
        .position(|menu| menu.mnemonic.eq_ignore_ascii_case(&letter))
}

/// The menu whose title is at a column on the bar, if any.
///
/// The gaps between titles are not targets: a click on the chrome between two menus
/// is a click on the bar, not a guess at which one it was near.
#[must_use]
fn menu_index_at(column: u16) -> Option<usize> {
    (0..MENUS.len()).find(|index| {
        let (start, width) = menu_title(*index);
        (start..start + width).contains(&column)
    })
}

/// The row of a drawn list a cell is over, inside the border.
#[must_use]
fn row_at(list: MenuList, column: u16, row: u16) -> Option<usize> {
    let inside = column > list.x && column < list.x + list.width.saturating_sub(1);
    if !inside {
        return None;
    }
    let at = row.checked_sub(list.y.checked_add(1)?)?;
    (at < list.rows).then_some(usize::from(at))
}

/// The sizes the panes were drawn at this frame.
///
/// Two panes need to know: the terminal, which tells its program so the program and
/// the parser agree on the width, and the transcript, whose copy cursor walks lines
/// of a viewport it cannot measure itself. The dividers are here for the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewports {
    /// The terminal pane's cells, whether or not a terminal is open.
    pub terminal: TerminalSize,
    /// The transcript's cells, when that pane was on screen.
    pub transcript: Option<TerminalSize>,
    /// Where the dividers and the body's edges were drawn.
    pub dividers: Dividers,
    /// Where the menu bar and its open list were drawn.
    pub menu: MenuAreas,
    /// Where the editor's text was drawn, when the pane was on screen.
    pub editor: Option<EditorArea>,
    /// Where the files tree's rows were drawn, when the tree was on screen.
    ///
    /// The box inside the border: a click on a row is that row, and the border is
    /// not a row.
    pub files: Option<PaneArea>,
    /// Where the session list's rows were drawn, when it was on screen.
    pub sessions: Option<PaneArea>,
    /// Where the terminal pane was drawn, when one is up.
    ///
    /// Not to be confused with `terminal`, which is the grid the program was told
    /// it has: this is where that grid landed on the window.
    pub terminal_area: Option<PaneArea>,
}

/// A diff review: the agent's edit, hunk by hunk, before the write is answered.
///
/// P6-9 asks for a review that can disagree with part of an edit. What an answer
/// can say is coarser than that: the agent asked to write one file, and the
/// permission protocol offers allow or deny, not "these hunks but not those". A
/// file written in part would be a file nobody asked for, so the decision is per
/// hunk and the answer is read off the hunks — every hunk accepted allows the
/// write, one rejected declines it, and the status line names the hunk that did
/// it. The per-hunk part is still the review's real work: it is how the one wrong
/// hunk is found without re-reading the whole edit.
#[derive(Debug)]
pub struct Review {
    /// The diff under review.
    pub diff: Diff,
    /// Whether each hunk is accepted, in the diff's order.
    pub accepted: Vec<bool>,
    /// Which hunk the keyboard is on.
    pub cursor: usize,
    /// Whether the two sides are drawn side by side rather than as one column.
    pub split: bool,
}

impl Review {
    /// A review of `diff` with nothing objected to yet.
    ///
    /// Every hunk starts accepted, because the common answer is yes and a review
    /// that starts by refusing would make the person argue for the write rather
    /// than against it.
    fn new(diff: Diff) -> Self {
        let accepted = vec![true; diff.hunks.len()];
        Self {
            diff,
            accepted,
            cursor: 0,
            split: false,
        }
    }

    /// How many hunks are accepted.
    #[must_use]
    pub fn accepted_hunks(&self) -> usize {
        self.accepted.iter().filter(|accept| **accept).count()
    }

    /// The hunks that were rejected, counted from 1 the way a person counts.
    #[must_use]
    pub fn rejected_hunks(&self) -> Vec<usize> {
        self.accepted
            .iter()
            .enumerate()
            .filter(|(_, accept)| !**accept)
            .map(|(index, _)| index + 1)
            .collect()
    }

    /// Decide the hunk the keyboard is on, and move to the next.
    ///
    /// One keystroke is one decision, so a long diff is read in one pass instead
    /// of a decision followed by a navigation per hunk.
    fn decide(&mut self, accept: bool) {
        if let Some(slot) = self.accepted.get_mut(self.cursor) {
            *slot = accept;
        }
        self.step(1);
    }

    /// Decide every hunk the same way.
    fn decide_all(&mut self, accept: bool) {
        self.accepted.fill(accept);
    }

    /// Move the cursor, stopping at either end rather than wrapping.
    fn step(&mut self, delta: i32) {
        let last = self.accepted.len().saturating_sub(1);
        self.cursor = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (self.cursor + delta as usize).min(last)
        };
    }
}

/// Everything the terminal draws, plus the state the keys act on.
#[derive(Debug)]
pub struct App {
    /// Where the daemon is, for the status line.
    pub daemon: String,
    /// The profile being served, when it is known.
    pub profile: String,
    /// The workspace root, when it is known: the tree's title.
    pub workspace: String,
    /// Whether the daemon answered a health check.
    pub connected: bool,
    /// The workspace tree, flattened. Empty until the daemon's listing arrives; the
    /// design pass seeds it.
    pub files: Vec<FileNode>,
    /// Which row of the tree is selected.
    pub file_selected: usize,
    /// Whether the tree draws the paths an ignore rule hides.
    ///
    /// A toggle rather than a second setting: `tree_respect_gitignore` is what a
    /// tree normally is, and this is the exception for one look. The daemon is asked
    /// for the same directories with `include_ignored`, and the rows come back marked
    /// so the pane can draw them as the hidden ones they are.
    pub show_ignored: bool,
    /// Where the two movable panels are.
    ///
    /// The shell's arrangement rather than a preference: it is one row away in the
    /// Panels menu, and a window that opened with the panes somewhere other than
    /// where they were left would be a window that hid itself.
    pub panels: Panels,
    /// Whether the outline panel is on screen.
    ///
    /// Apart from the side it is on: a panel that is put away has not moved, and the
    /// Panels menu can put it back where it was.
    pub show_outline: bool,
    /// The open buffer's definitions, as the outline panel lists them.
    pub outline: Vec<acpilot_text::outline::Symbol>,
    /// Which of them `Enter` goes to.
    pub outline_selected: usize,
    /// The document revision the outline was parsed from.
    ///
    /// A number rather than a flag: the panel re-parses when the text has changed since
    /// the read, and a comparison is cheaper than remembering which keys edit.
    pub outline_revision: u64,
    /// Whether the references panel is on screen.
    pub show_references: bool,
    /// The uses of [`App::references_name`], as the daemon found them.
    pub references: Vec<acpilot_proto::file::Reference>,
    /// The name the list is about: the word under the caret when it was asked for.
    pub references_name: String,
    /// Which use `Enter` opens.
    pub references_selected: usize,
    /// The document revision the list was last asked about.
    ///
    /// A revision rather than a flag: an edit can add or remove a use of the name the
    /// caret is resting on without the name itself changing, and a panel that follows
    /// the text has to re-list for that too.
    pub references_revision: u64,
    /// What the caret said at the previous frame: the word it was on, and the revision
    /// it was on it at, whether or not that was asked about.
    ///
    /// A question has to be the same on two frames running before it is sent, which is
    /// what keeps typing from costing a workspace walk per keystroke: a word that
    /// changes every frame is a word still being typed, and a buffer that changes every
    /// frame is a buffer still being written.
    pub references_seen: Option<(String, u64)>,
    /// Whether the project map panel is on screen.
    ///
    /// Apart from the side it is on, like the other panels: a map that is put away has
    /// not moved, and the Panels menu can put it back where it was.
    pub show_map: bool,
    /// The map the agent wrote, once the daemon has read it.
    pub map: Option<acpilot_proto::project::ProjectMap>,
    /// Which area `Enter` reads out.
    pub map_selected: usize,
    /// Whether the daemon has been asked for the map since the panel came up.
    ///
    /// A flag rather than asking every frame: the map is a file on disk, and a panel
    /// that re-read it each frame would be a file read per 120 ms for a document that
    /// changes when somebody asks the agent to rewrite it. `r` in the panel clears it.
    pub map_asked: bool,
    /// The rows a workspace search found, when one is up.
    pub found: Vec<FileNode>,
    /// The query being typed in the files pane's search, when it is.
    pub search: Option<String>,
    /// Requests the last key press asked for beyond the one it returned.
    ///
    /// A toggle has to re-list the root and every open directory, and one return
    /// value cannot say that; the loop drains this after each event.
    pub pending: Vec<Request>,
    /// Tokens the conversations behind the sessions hold, summed.
    ///
    /// [`None`] when nobody has had a turn: zero would be a measured zero, which is
    /// a different claim, and the status line stays quiet instead.
    pub used_tokens: Option<u32>,
    /// Whether any of those figures is this daemon's estimate rather than a report.
    pub tokens_estimated: bool,
    /// What the daemon's language server said about the open buffer.
    ///
    /// [`None`] is "nobody has checked", which is not a clean file: the answer itself
    /// says whether a server answered, and the gutter draws nothing either way while
    /// the status line says which of the two it is.
    pub diagnostics: Option<acpilot_proto::file::FileDiagnostics>,
    /// The file that answer is about.
    pub diagnostics_path: String,
    /// Whether a question about the buffer has been asked and not yet answered.
    ///
    /// The status line says `checking` while this is true. Not derivable from the
    /// revision: a first question can wait on a server being started and an index
    /// being built, which is seconds during which the text is already the version
    /// that was sent.
    pub diagnostics_pending: bool,
    /// The buffer revision the last diagnostics request was about.
    ///
    /// The text that has an answer, or is owed one. A buffer whose revision differs
    /// from this has been edited since the last question and is a question again once
    /// it stops moving — that is the whole of [`App::refresh_problems`].
    diagnostics_revision: u64,
    /// The revision the last frame saw, which is how an edit is noticed.
    ///
    /// Separate from [`Self::diagnostics_revision`] because a frame that sees a new
    /// revision only restarts the pause: nothing is asked until a frame finds the
    /// buffer unchanged and the pause ended.
    diagnostics_seen: u64,
    /// When the pause after the last edit ends, while one is running.
    diagnostics_due: Option<std::time::Instant>,
    /// The editor pane's document, when a file is open.
    ///
    /// A real [`acpilot_text::Document`] rather than a mock: the rope, the caret
    /// set and the undo history the desktop app and the browser will edit
    /// through, with the pane's viewport arithmetic on top of it (P6-4).
    pub editor: Option<EditorPane>,
    /// The grammars this session has compiled, one per language.
    ///
    /// Held by the app rather than by a pane: compiling a tree-sitter query is
    /// the expensive half of highlighting, and a second editor tab on a `.rs`
    /// file should reuse the first one's.
    pub grammars: Grammars,
    /// The diff the permission card previews, and the review opens on.
    pub diff: Option<Diff>,
    /// The diff review, when one is up.
    ///
    /// Apart from [`App::diff`] because the diff is what the agent asked for and
    /// the review is what a person has said about it so far: answering a hunk must
    /// not rewrite the diff the gutter's marks were parsed from.
    pub review: Option<Review>,
    /// The workspace content search, when one is up.
    pub grep: Option<GrepSearch>,
    /// The source-control pane, when one is up.
    pub source: Option<SourceControl>,
    /// The commit composer, when it is over the source-control pane.
    pub commit: Option<CommitComposer>,
    /// The line a jump list's row named, waiting for the file to arrive.
    ///
    /// A list answers with a line before the text exists, so the line is held here
    /// rather than being forgotten between the answer and the read.
    pub pending_line: Option<u32>,
    /// The palette, when it is up.
    pub palette: Option<Palette>,
    /// The menu bar, when it has the keyboard.
    pub menu: Option<MenuState>,
    /// Where the bar and its list were drawn this frame, for the mouse.
    pub menu_areas: MenuAreas,
    /// The colours every pane is drawn in.
    pub theme: Theme,
    /// Whether the colours on screen are the daemon's answer.
    ///
    /// The daemon reads the same cascade this client does, so its `prefs` frame
    /// and a local settings edit usually say the same thing. Where they differ,
    /// the daemon is right: it may be reading files this client cannot see (a
    /// remote host, another user's config), and a stale local copy must not
    /// override it.
    theme_from_daemon: bool,
    /// Whether the About card is up.
    pub about: bool,
    /// Whether the focused pane's keys are listed.
    pub which_key: bool,
    /// The tree's column, in cells.
    pub files_width: u16,
    /// The sidebar's column — sessions, transcript, composer — in cells.
    pub sidebar_width: u16,
    /// The notice the last planned action raised.
    pub toast: Option<Toast>,
    /// The stats bar's rows, measured for drawing.
    ///
    /// `acpilot_stats::Row` rather than the wire type: the ratio and the words are
    /// computed once, for every surface, and a keypress must not be able to change
    /// what the bar says.
    pub integrations: Vec<Row>,
    /// Whether the bar is showing where each figure came from instead of the figure.
    pub stats_sources: bool,
    /// Every command the palette offers, with the task behind each unbuilt one.
    pub commands: Vec<Command>,
    /// Whether the panes hold seeded data rather than the daemon's.
    pub demo: bool,
    /// The profile's agents, in the daemon's order.
    pub agents: Vec<AgentSummary>,
    /// Every session, oldest first.
    pub sessions: Vec<SessionSummary>,
    /// Index into [`App::sessions`].
    pub selected: usize,
    /// Whether the selection was moved at least once.
    ///
    /// Kept apart from `selected`: index 0 is the default, not a choice, and
    /// opening a transcript should need a key press rather than happening
    /// because a daemon happened to be running.
    pub chose: bool,
    /// The open session's id, when one is open.
    pub open: Option<String>,
    /// The open session's transcript.
    pub entries: Vec<Entry>,
    /// Last sequence accepted, so a reconnect cannot duplicate a paragraph.
    pub last_seq: u64,
    /// Requests waiting for an answer.
    pub permissions: Vec<PermissionRequest>,
    /// Whether the permission card is up.
    pub permission_card: bool,
    /// Which pending request the card is showing.
    ///
    /// An index rather than always the first: an agent can have two questions
    /// waiting, and a card that only ever answered the oldest would be a queue
    /// nobody could see the end of.
    pub permission_at: usize,
    /// The prompt being composed.
    pub input: String,
    /// The caret in [`App::input`], counted in characters from the start.
    ///
    /// A character index rather than a byte offset: the composer is a text field
    /// that takes whatever a person can type, and an offset that split a `日`
    /// would be an edit nobody meant.
    pub cursor: usize,
    /// The `@path` completion, when one is open.
    pub completion: Option<Completion>,
    /// Terminals a person may attach to, oldest first: the open session's, and the
    /// panes no session owns.
    ///
    /// A sessionless pane is an agent's shell whose session the daemon could not
    /// map (P3-6), or one a client asked for without a session (P8-6). It is
    /// listed here because a pane nobody can find is a process nobody can stop.
    pub terminals: Vec<TerminalSummary>,
    /// The terminal being typed into, when one is open.
    ///
    /// One at a time: a terminal is a keyboard, and two would be one that silently
    /// swallowed what was typed.
    pub terminal: Option<TerminalPane>,
    /// The transcript's reader: the cursor, the selection and the find.
    ///
    /// The terminal's copy mode over the agent's words — the same motions, the same
    /// corners, the same yank — on lines that arrived as events instead of bytes.
    /// Which pane has the keyboard decides whether it is being read: see
    /// [`Focus::Transcript`].
    pub transcript: Reader,
    /// The size the transcript was last drawn at.
    ///
    /// A copy cursor walks lines of a viewport, so it needs the viewport rather
    /// than the window: the transcript is one pane among several, and the number of
    /// lines it can scroll back is the number that are above the box.
    pub transcript_size: Option<TerminalSize>,
    /// Whether the picker for a session's terminals is up.
    ///
    /// A session can have several — an agent may open one per command — and
    /// opening the first would be a pane that hides the others.
    pub terminal_picker: bool,
    /// Which terminal the picker points at.
    pub picker_selected: usize,
    /// Text waiting for the terminal's clipboard.
    ///
    /// The app cannot write it: OSC 52 is an escape for the terminal a person is
    /// looking at, and only the event loop owns that stream.
    pub clipboard: Option<String>,
    /// Where the editor's text was last drawn, for the mouse.
    pub editor_area: Option<EditorArea>,
    /// Where the files tree's rows were last drawn, for the mouse.
    pub files_area: Option<PaneArea>,
    /// Where the session list's rows were last drawn, for the mouse.
    pub sessions_area: Option<PaneArea>,
    /// Where the terminal pane was last drawn, for the mouse.
    pub terminal_area: Option<PaneArea>,
    /// Where the dividers were last drawn, for the mouse.
    pub dividers: Dividers,
    /// What the mouse is holding, if it is holding anything.
    pub drag: Option<Drag>,
    /// Where the pointer was at the last press or drag.
    ///
    /// A drag only arrives while the pointer moves, so a pointer resting on a
    /// pane's edge sends nothing at all — and resting on the edge is exactly what
    /// asks for the view to scroll. The last cell is kept so the loop can keep
    /// asking, rather than waiting for a hand to twitch.
    pub drag_at: Option<(u16, u16)>,
    /// Whether the last autoscroll moved the view.
    ///
    /// The loop reads it to decide how long to wait for input: a drag resting at
    /// an edge wants the next line now, and a drag that has run out of file does
    /// not want a faster clock for nothing.
    pub drag_scroll: bool,
    /// Where the divider widths are remembered, when this client has somewhere to
    /// write them.
    ///
    /// [`None`] for the design pass and for every test: a demo that wrote to a
    /// person's data directory would be a screenshot that edited the machine it was
    /// taken on.
    pub layout_path: Option<std::path::PathBuf>,
    /// The widths last read or written, so a launch that changed nothing writes
    /// nothing.
    pub layout_saved: Layout,
    /// The keys in force: a preset, with `keymap.toml`'s overrides on top.
    ///
    /// On the app rather than in a `static` because a test and the design pass must
    /// be able to run a table nobody's file chose, and because `?` lists what this
    /// app actually answers to rather than what the crate used to.
    pub keymap: Keymap,
    /// The size the terminal pane was last drawn at.
    ///
    /// The event loop tells the app every frame and the program only when this
    /// changes, so a window resize reaches the shell once rather than sixty times
    /// a second.
    pub pane_size: Option<TerminalSize>,
    /// Which pane has the keyboard.
    pub focus: Focus,
    /// The status line's message.
    pub status: String,
    /// Set when the user asks to leave.
    pub should_quit: bool,
}

/// The workspace content search: what was typed, the lines it found, and the row
/// the keyboard is on.
///
/// A jump list rather than a pane: the only thing a person does with a result is go
/// there, so `Enter` opens the file at that line and closes the list. It lives
/// beside the panes rather than in one of them because the answer spans the whole
/// workspace — the tree lists directories, and this lists lines.
#[derive(Debug, Default)]
pub struct GrepSearch {
    /// The query, as typed.
    pub query: String,
    /// The lines the daemon found, sorted by path and line.
    pub matches: Vec<acpilot_proto::file::GrepMatch>,
    /// Which row `Enter` opens.
    pub selected: usize,
}

/// The source-control pane: what changed, and the diff of the selected path.
///
/// Modal rather than a panel because it is about the repository rather than the
/// open buffer, and the write it can lead to is a decision. It reads status and a
/// diff; it never stages. `loading` is what separates "the index is empty" from
/// "the daemon has not answered yet", which is the difference between a clean tree
/// and a pane that has not looked.
#[derive(Debug)]
pub struct SourceControl {
    /// Branch state and every changed path.
    pub status: acpilot_proto::git::GitStatus,
    /// The whole uncommitted diff and its file list.
    pub diff: acpilot_proto::git::GitDiff,
    /// Which changed path the diff belongs to.
    pub selected: usize,
    /// The first diff line on screen.
    pub scroll: usize,
    /// Whether the daemon's answer has arrived.
    pub loading: bool,
    /// Why there is no answer, when the request failed.
    pub error: Option<String>,
}

impl SourceControl {
    /// A pane with nothing in it, waiting for the daemon.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            status: acpilot_proto::git::GitStatus {
                branch: None,
                upstream: None,
                ahead: 0,
                behind: 0,
                head: None,
                files: Vec::new(),
            },
            diff: acpilot_proto::git::GitDiff {
                patch: String::new(),
                files: Vec::new(),
            },
            selected: 0,
            scroll: 0,
            loading: true,
            error: None,
        }
    }

    /// The design pass's one changed file, so the pane can be reviewed without a
    /// repository. It says `modified` in the index because a commit composer needs
    /// something staged to talk about, and the fixture's diff is the change.
    #[must_use]
    pub fn fixture(diff: &Diff) -> Self {
        let path = diff.path.clone();
        Self {
            status: acpilot_proto::git::GitStatus {
                branch: Some("main".to_owned()),
                upstream: Some("origin/main".to_owned()),
                ahead: 1,
                behind: 0,
                head: None,
                files: vec![acpilot_proto::git::GitStatusEntry {
                    path: path.clone(),
                    old_path: None,
                    index: acpilot_proto::git::GitChangeKind::Modified,
                    worktree: acpilot_proto::git::GitChangeKind::Unmodified,
                }],
            },
            diff: acpilot_proto::git::GitDiff {
                patch: diff.patch(),
                files: vec![acpilot_proto::git::GitDiffFile {
                    path,
                    old_path: None,
                    change: acpilot_proto::git::GitChangeKind::Modified,
                }],
            },
            selected: 0,
            scroll: 0,
            loading: false,
            error: None,
        }
    }

    /// How many paths the next commit would contain.
    #[must_use]
    pub fn staged(&self) -> usize {
        self.status
            .files
            .iter()
            .filter(|entry| staged_kind(entry.index))
            .count()
    }

    /// The path whose diff is on the right.
    #[must_use]
    pub fn selected_path(&self) -> Option<&str> {
        self.status
            .files
            .get(self.selected)
            .map(|entry| entry.path.as_str())
    }

    /// The selected file's section of the patch, header included.
    ///
    /// Selecting a path is the whole navigation model: the right side is one file,
    /// not a scroll through all of them, because a commit is reviewed a file at a
    /// time. An untracked path has no section yet, and an empty answer is the truth
    /// rather than an error.
    #[must_use]
    pub fn section(&self) -> Vec<&str> {
        let Some(path) = self.selected_path() else {
            return Vec::new();
        };
        let suffix = format!(" b/{path}");
        let mut lines = Vec::new();
        let mut inside = false;
        for line in self.diff.patch.lines() {
            if line.starts_with("diff --git ") {
                inside = line.ends_with(&suffix);
                if inside {
                    lines.push(line);
                }
            } else if inside {
                lines.push(line);
            }
        }
        lines
    }
}

/// Whether the index has something to commit for this kind.
#[must_use]
fn staged_kind(kind: acpilot_proto::git::GitChangeKind) -> bool {
    !matches!(
        kind,
        acpilot_proto::git::GitChangeKind::Unmodified
            | acpilot_proto::git::GitChangeKind::Untracked
    )
}

/// A commit message being written or generated.
///
/// The message is one string, because that is what git takes and what the daemon
/// commits: splitting it into a subject field and a body field would let the two
/// disagree. `confirming` is the point of the whole composer — `Ctrl-S` asks, `y`
/// commits, and nothing writes before that.
#[derive(Debug, Default)]
pub struct CommitComposer {
    /// The whole message.
    pub text: String,
    /// The caret, counted in characters rather than bytes.
    pub cursor: usize,
    /// Whether the session is still drafting.
    pub generating: bool,
    /// Whether a commit request is in flight.
    pub committing: bool,
    /// Whether `Ctrl-S` is waiting for `y`.
    pub confirming: bool,
    /// The last refusal, shown in the composer rather than only in the status line.
    pub error: Option<String>,
}

impl CommitComposer {
    /// Where the caret is: line and column, both zero-based.
    #[must_use]
    pub fn line_col(&self) -> (usize, usize) {
        let mut line = 0;
        let mut column = 0;
        for (index, character) in self.text.chars().enumerate() {
            if index == self.cursor {
                break;
            }
            if character == '\n' {
                line += 1;
                column = 0;
            } else {
                column += 1;
            }
        }
        (line, column)
    }

    fn byte_at_cursor(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map_or(self.text.len(), |(byte, _)| byte)
    }

    /// Type one character at the caret.
    pub fn insert(&mut self, character: char) {
        let at = self.byte_at_cursor();
        self.text.insert(at, character);
        self.cursor += 1;
        self.error = None;
    }

    /// Delete the character before the caret.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self.byte_at_cursor();
        if let Some(previous) = self.text[..at].chars().next_back() {
            self.text.remove(at - previous.len_utf8());
            self.cursor -= 1;
            self.error = None;
        }
    }

    /// Delete the character under the caret.
    pub fn delete(&mut self) {
        if self.cursor >= self.text.chars().count() {
            return;
        }
        let at = self.byte_at_cursor();
        self.text.remove(at);
        self.error = None;
    }

    /// Move the caret one character left.
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the caret one character right.
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    /// Put the caret at the start of its line.
    pub fn home(&mut self) {
        let (line, _) = self.line_col();
        self.cursor = self
            .text
            .chars()
            .enumerate()
            .filter(|(_, character)| *character == '\n')
            .nth(line.saturating_sub(1))
            .map_or(0, |(index, _)| index + 1);
        if line == 0 {
            self.cursor = 0;
        }
    }

    /// Put the caret at the end of its line.
    pub fn end(&mut self) {
        self.cursor = self
            .text
            .chars()
            .enumerate()
            .skip(self.cursor)
            .find(|(_, character)| *character == '\n')
            .map_or_else(|| self.text.chars().count(), |(index, _)| index);
    }

    /// Move the caret to the same column on another line, clamped to its length.
    pub fn vertical(&mut self, delta: isize) {
        let (line, column) = self.line_col();
        let target = if delta < 0 {
            line.saturating_sub(delta.unsigned_abs())
        } else {
            line.saturating_add(delta as usize)
        };
        let mut starts = vec![0];
        for (index, character) in self.text.chars().enumerate() {
            if character == '\n' {
                starts.push(index + 1);
            }
        }
        let Some(&start) = starts.get(target) else {
            return;
        };
        let end = self
            .text
            .chars()
            .enumerate()
            .skip(start)
            .find(|(_, character)| *character == '\n')
            .map_or_else(|| self.text.chars().count(), |(index, _)| index);
        self.cursor = (start + column).min(end);
    }
}

/// The first row a pane of `height` rows shows when `selected` is chosen.
///
/// A pure function of the selection rather than a scroll offset the pane remembers:
/// the tree has no scroll of its own, and one that could disagree with the cursor
/// would be a second answer to “which row is where”. The mouse reads a click back
/// through the same function the drawing placed the rows with.
#[must_use]
pub fn first_visible(selected: usize, len: usize, height: usize) -> usize {
    let height = height.max(1);
    selected
        .saturating_add(1)
        .saturating_sub(height)
        .min(len.saturating_sub(height))
}

impl App {
    /// A TUI pointed at `daemon`, with nothing loaded yet.
    #[must_use]
    pub fn new(daemon: impl Into<String>) -> Self {
        Self {
            daemon: daemon.into(),
            profile: String::new(),
            workspace: String::new(),
            connected: false,
            files: Vec::new(),
            file_selected: 0,
            show_ignored: false,
            panels: Panels::default(),
            show_outline: false,
            outline: Vec::new(),
            outline_selected: 0,
            outline_revision: 0,
            show_references: false,
            references: Vec::new(),
            references_name: String::new(),
            references_selected: 0,
            references_revision: 0,
            references_seen: None,
            show_map: false,
            map: None,
            map_selected: 0,
            map_asked: false,
            found: Vec::new(),
            search: None,
            pending: Vec::new(),
            used_tokens: None,
            tokens_estimated: false,
            diagnostics: None,
            diagnostics_path: String::new(),
            diagnostics_pending: false,
            diagnostics_revision: 0,
            diagnostics_seen: 0,
            diagnostics_due: None,
            editor: None,
            grammars: Grammars::new(),
            diff: None,
            review: None,
            grep: None,
            source: None,
            commit: None,
            pending_line: None,
            palette: None,
            menu: None,
            menu_areas: MenuAreas::default(),
            theme: Theme::default(),
            theme_from_daemon: false,
            about: false,
            which_key: false,
            files_width: Layout::default().files,
            sidebar_width: Layout::default().sidebar,
            toast: None,
            integrations: Vec::new(),
            stats_sources: false,
            commands: acpilot_demo::palette::commands(),
            demo: false,
            agents: Vec::new(),
            sessions: Vec::new(),
            selected: 0,
            chose: false,
            open: None,
            entries: Vec::new(),
            last_seq: 0,
            permissions: Vec::new(),
            permission_card: false,
            permission_at: 0,
            input: String::new(),
            cursor: 0,
            completion: None,
            terminals: Vec::new(),
            terminal: None,
            transcript: Reader::default(),
            transcript_size: None,
            terminal_picker: false,
            picker_selected: 0,
            clipboard: None,
            editor_area: None,
            files_area: None,
            sessions_area: None,
            terminal_area: None,
            dividers: Dividers::default(),
            drag: None,
            drag_at: None,
            drag_scroll: false,
            layout_path: None,
            layout_saved: Layout::default(),
            keymap: Keymap::default(),
            pane_size: None,
            focus: Focus::Sessions,
            status: "connecting".to_owned(),
            should_quit: false,
        }
    }

    /// The design pass: every pane seeded, no daemon.
    ///
    /// The transcript is folded through the same [`App::apply_event`] the daemon's
    /// events go through, so what a reviewer sees is what real events would
    /// produce. A fixture that drew its own transcript would be a picture of the
    /// fixture, and the layout would be argued about against something the product
    /// can never show.
    pub fn demo(demo: &acpilot_demo::Demo) -> Self {
        let mut app = Self::new(demo.daemon.clone());
        app.demo = true;
        app.profile = demo.profile.clone();
        app.workspace = demo.workspace.clone();
        app.connected = true;
        app.agents = demo.agents.clone();
        app.sessions = demo
            .sessions
            .iter()
            .map(|fixture| fixture.summary.clone())
            .collect();
        app.files = demo.files.clone();
        // The gutter's markers are git's answer about the diff the permission
        // card is showing, not a list of line numbers the fixture also keeps:
        // the review and the marks are then the same edit, parsed once.
        let marks = acpilot_git::line_changes(&demo.diff.patch(), &demo.editor.path);
        app.editor = Some(EditorPane::seeded(&demo.editor, marks, &mut app.grammars));
        app.diff = Some(demo.diff.clone());
        // The seeded rows go through the same conversion the daemon's do: a design
        // pass that drew a different bar from the product's would be a screenshot
        // of something nobody will see.
        app.note_tokens(&demo.integrations);
        app.integrations = acpilot_stats::rows_now(&demo.integrations);
        app.terminals = demo.terminals.clone();
        app.permissions = vec![demo.permission.clone()];
        if let Some(first) = demo.sessions.first() {
            app.open = Some(first.summary.id.clone());
            for event in &first.events {
                app.apply_event(event.clone());
            }
        }
        app.set_focus(Focus::Prompt);
        app.status = "design pass · seeded · no daemon".to_owned();
        app
    }

    /// Learn how big the terminal pane is this frame.
    ///
    /// The parser is resized with it, not after it: a program told it has 100
    /// columns while the pane still parses 80 would have its long lines cut off
    /// at the edge of a screen nobody can see the rest of.
    pub fn set_pane_size(&mut self, size: TerminalSize) {
        self.pane_size = Some(size);
        if let Some(pane) = &mut self.terminal {
            let (rows, cols) = pane.parser.screen().size();
            if (cols.get(), rows.get()) != (size.cols.max(1), size.rows.max(1)) {
                pane.parser
                    .set_size(screen_rows(size.rows), screen_cols(size.cols));
                // A resize reflows the grid; the offset the view was read at is
                // reapplied so a person scrolled back does not land somewhere
                // else because the window changed shape.
                let scroll = pane.reader.view.scroll;
                pane.set_scroll(scroll);
            }
        }
    }

    /// Learn the sizes the panes were drawn at this frame.
    ///
    /// The terminal's size is what its program is told, and the transcript's is
    /// what its copy cursor walks: the number of lines a person can scroll back is
    /// the number above the box, which only the drawing knows.
    /// Draw with these colours from now on.
    ///
    /// A setter rather than a constructor argument because a theme is a paint
    /// choice, not state: the demo, the tests and a client that resolved no theme
    /// all start from the terminal's own colours and are told otherwise when
    /// someone knows.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Recolour, and say which theme it became.
    ///
    /// Said out loud because a screen that recoloured itself silently looks like a
    /// bug to anyone who did not just edit the settings.
    fn announce_theme(&mut self, theme: Theme) {
        self.status = if theme.name().is_empty() {
            "theme · terminal defaults".to_owned()
        } else {
            format!("theme · {}", theme.name())
        };
        self.set_theme(theme);
    }

    pub fn set_viewports(&mut self, viewports: Viewports) {
        self.set_pane_size(viewports.terminal);
        self.dividers = viewports.dividers;
        self.menu_areas = viewports.menu;
        self.editor_area = viewports.editor;
        self.files_area = viewports.files;
        self.sessions_area = viewports.sessions;
        self.terminal_area = viewports.terminal_area;
        if let Some(size) = viewports.transcript {
            self.transcript_size = Some(size);
            let grid = (size.rows.max(1), size.cols.max(1));
            let max_scroll = self.transcript_len().saturating_sub(usize::from(grid.0));
            self.transcript.view.clamp(grid, max_scroll);
        }
    }

    /// Take the text copy mode put on the clipboard, if any.
    ///
    /// The event loop writes it as OSC 52: the clipboard belongs to the terminal
    /// the person is looking at, which over SSH is a different computer.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    /// The transcript as the pane draws it: every screen line, with the kind that
    /// styles it.
    ///
    /// The view and a yank read the same list, so the cells a person highlights and
    /// the text they copy cannot disagree — the same reason the terminal yanks from
    /// the screen at the offset it is drawn at.
    #[must_use]
    pub fn transcript_rows(&self) -> Vec<TranscriptRow> {
        let mut rows = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.kind == Kind::Tool {
                rows.push(TranscriptRow {
                    kind: entry.kind,
                    segments: vec![crate::markdown::Segment::plain(entry.text.clone())],
                    entry: index,
                });
                if !entry.folded
                    && let Some(call) = &entry.tool
                {
                    for line in call.detail.lines() {
                        rows.push(TranscriptRow {
                            kind: entry.kind,
                            segments: vec![crate::markdown::Segment::plain(format!("  {line}"))],
                            entry: index,
                        });
                    }
                }
                continue;
            }
            if entry.kind == Kind::Assistant {
                for line in crate::markdown::render(&entry.text) {
                    rows.push(TranscriptRow {
                        kind: entry.kind,
                        segments: line.segments,
                        entry: index,
                    });
                }
                continue;
            }
            rows.extend(entry.text.split('\n').map(|line| TranscriptRow {
                kind: entry.kind,
                segments: vec![crate::markdown::Segment::plain(line)],
                entry: index,
            }));
        }
        rows
    }

    /// How many screen lines the transcript holds, without building them.
    ///
    /// A key press needs the length to clamp a scroll, not the lines; counting
    /// newlines is the same answer without the allocation. A folded tool call is
    /// one row whatever its result holds, which is what makes the fold a fold.
    fn transcript_len(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| {
                if entry.kind == Kind::Tool {
                    let detail = if entry.folded {
                        0
                    } else {
                        entry
                            .tool
                            .as_ref()
                            .map_or(0, |call| call.detail.lines().count())
                    };
                    1 + detail
                } else {
                    entry.text.matches('\n').count() + 1
                }
            })
            .sum()
    }

    /// Whether the open session is in the middle of a turn.
    ///
    /// Read off the session list rather than kept as a flag: the daemon already
    /// says `busy`, and a client that guessed from the last event it saw would
    /// lose the end of a turn to a dropped connection.
    #[must_use]
    pub fn streaming(&self) -> bool {
        let Some(id) = &self.open else {
            return false;
        };
        self.sessions
            .iter()
            .any(|session| &session.id == id && session.status == SessionStatus::Busy)
    }

    /// How many rows the composer needs for its text.
    ///
    /// Grows with the lines a person wrote and stops at six: a prompt is not the
    /// document, and a composer that took the window would hide the transcript it
    /// is a reply to.
    #[must_use]
    pub fn composer_height(&self) -> u16 {
        let lines = u16::try_from(self.input.matches('\n').count() + 1).unwrap_or(u16::MAX);
        lines.clamp(1, 6) + 2
    }

    /// Where the caret sits in the composer: its row and its column.
    #[must_use]
    pub fn composer_position(&self) -> (u16, u16) {
        let before: String = self.input.chars().take(self.cursor).collect();
        let row = before.matches('\n').count();
        let column = before
            .rsplit('\n')
            .next()
            .map_or(0, |line| line.chars().count());
        (
            u16::try_from(row).unwrap_or(u16::MAX),
            u16::try_from(column).unwrap_or(u16::MAX),
        )
    }

    /// The `@path` being typed, as where its `@` starts and what follows it.
    ///
    /// Only the word the caret is in counts, and only when it began at the start
    /// of a word: an email address is not a file, and a completion that opened
    /// on one would be a popup nobody asked for.
    fn mention(&self) -> Option<(usize, String)> {
        let before: Vec<char> = self.input.chars().take(self.cursor).collect();
        let start = before.iter().rposition(|character| *character == '@')?;
        if before[start + 1..]
            .iter()
            .any(|character| character.is_whitespace())
        {
            return None;
        }
        if start > 0 && !before[start - 1].is_whitespace() {
            return None;
        }
        Some((start, before[start + 1..].iter().collect()))
    }

    /// Ask the daemon about the `@path` being typed, when it has changed.
    ///
    /// The question is what the popup is showing, not what was typed most
    /// recently: a query already answered is not asked twice, and a word that
    /// stopped being a mention closes the popup.
    fn refresh_completion(&mut self) -> Option<Request> {
        let query = self
            .mention()
            .map(|(_, query)| query)
            .filter(|query| !query.is_empty());
        match query {
            Some(query) => {
                if self
                    .completion
                    .as_ref()
                    .is_some_and(|completion| completion.query == query)
                {
                    return None;
                }
                self.completion = Some(Completion {
                    query: query.clone(),
                    rows: Vec::new(),
                    selected: 0,
                });
                Some(Request::CompleteFiles(query))
            }
            None => {
                self.completion = None;
                None
            }
        }
    }

    /// The byte offset of a character index in the composer.
    fn composer_byte(&self, chars: usize) -> usize {
        self.input
            .char_indices()
            .nth(chars)
            .map_or(self.input.len(), |(index, _)| index)
    }

    /// Put a character where the caret is.
    fn insert_character(&mut self, character: char) {
        let at = self.composer_byte(self.cursor);
        self.input.insert(at, character);
        self.cursor += 1;
    }

    /// Remove the character before the caret.
    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let from = self.composer_byte(self.cursor - 1);
        let to = self.composer_byte(self.cursor);
        self.input.replace_range(from..to, "");
        self.cursor -= 1;
    }

    /// Remove the character under the caret.
    fn delete_character(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let from = self.composer_byte(self.cursor);
        let to = self.composer_byte(self.cursor + 1);
        self.input.replace_range(from..to, "");
    }

    /// Move the caret a row up or down, keeping its column where it fits.
    fn composer_move(&mut self, delta: i32) {
        let before: Vec<char> = self.input.chars().take(self.cursor).collect();
        let row = before
            .iter()
            .filter(|character| **character == '\n')
            .count();
        let column = before
            .iter()
            .rev()
            .take_while(|character| **character != '\n')
            .count();
        let lines: Vec<&str> = self.input.split('\n').collect();
        let target = if delta < 0 {
            if row == 0 {
                return;
            }
            row - 1
        } else {
            if row + 1 >= lines.len() {
                return;
            }
            row + 1
        };
        let mut index = 0usize;
        for line in &lines[..target] {
            index += line.chars().count() + 1;
        }
        index += column.min(lines[target].chars().count());
        self.cursor = index;
    }

    /// Take the completion the popup is on, replacing the `@word` it matched.
    fn accept_completion(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some(row) = completion.rows.get(completion.selected).cloned() else {
            return;
        };
        let Some((start, _)) = self.mention() else {
            return;
        };
        let from = self.composer_byte(start);
        let to = self.composer_byte(self.cursor);
        self.input.replace_range(from..to, &format!("@{row}"));
        self.cursor = start + 1 + row.chars().count();
    }

    /// Open or close the tool call under the transcript's cursor.
    ///
    /// A tool call is a row until it is asked about, and a result nobody can open
    /// is a result a person scrolls past without knowing it is there.
    fn toggle_tool(&mut self) {
        let (height, _) = self.transcript_grid();
        let max_scroll = self.transcript_len().saturating_sub(usize::from(height));
        self.transcript.view.clamp((height, u16::MAX), max_scroll);
        let rows = self.transcript_rows();
        let Some(row) = rows.get(self.transcript.view.cursor.0) else {
            return;
        };
        let entry = row.entry;
        if !self
            .entries
            .get(entry)
            .is_some_and(|entry| entry.tool.is_some())
        {
            self.status = "that row is not a tool call".to_owned();
            return;
        }
        let folded = self
            .entries
            .get_mut(entry)
            .map(|entry| {
                entry.folded = !entry.folded;
                entry.folded
            })
            .unwrap_or(false);
        // The length changed under the cursor; putting the view back at the tail
        // when it was live is what keeps a fold from scrolling the transcript.
        let max_scroll = self.transcript_len().saturating_sub(usize::from(height));
        self.transcript.view.scroll = self.transcript.view.scroll.min(max_scroll);
        self.status = if folded {
            "tool call folded · Enter opens it".to_owned()
        } else {
            "tool call opened · Enter folds it".to_owned()
        };
    }

    /// Whether the transcript is a pane that can be read right now.
    ///
    /// It needs a session to have something in it, and the terminal covers it when
    /// one is attached, so a keyboard aimed at a transcript behind a terminal would
    /// be aimed at something nobody can see.
    #[must_use]
    pub fn readable_transcript(&self) -> bool {
        self.open.is_some() && self.terminal.is_none()
    }

    /// The transcript's cells: what it was last drawn in, or a default before the
    /// first frame.
    fn transcript_grid(&self) -> (u16, u16) {
        let size = self
            .transcript_size
            .unwrap_or(TerminalSize { cols: 80, rows: 24 });
        (size.rows.max(1), size.cols.max(1))
    }

    /// The lines a search in the transcript reads, oldest first.
    fn transcript_text(&self) -> Vec<String> {
        self.transcript_rows()
            .into_iter()
            .map(|row| row.text())
            .collect()
    }

    /// Move the keyboard, and put the transcript back on the live tail when it is no
    /// longer the pane being read.
    ///
    /// Focus is the transcript's mode, so the two cannot disagree: a view left
    /// scrolled where nobody is reading would be live output that stopped
    /// following the conversation.
    fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        if focus != Focus::Transcript {
            self.transcript.view = CopyView::new((0, 0));
            self.transcript.typing = None;
        }
    }

    /// Put the keyboard in the transcript, with the cursor on its newest line.
    ///
    /// A transcript is written a paragraph at a time and read the same way, so the
    /// pane is scrolled and selected by line, and the cursor starts where a session
    /// was left — at the end of it.
    fn focus_transcript(&mut self) {
        if !self.readable_transcript() {
            if self.terminal.is_some() {
                self.notice("ctrl-t leaves the terminal before the transcript");
            } else {
                self.notice("open a session before reading it");
            }
            return;
        }
        let last = self.transcript_len().saturating_sub(1);
        self.transcript = Reader::new((last, 0));
        self.focus = Focus::Transcript;
        self.status = COPY_HINT.to_owned();
    }

    /// The keys the transcript answers to while it has the keyboard.
    ///
    /// The same keys as the terminal's copy mode, because it is the same gesture:
    /// the pane is being scrolled and selected, not typed into.
    fn transcript_key(&mut self, key: KeyEvent) -> Option<Request> {
        if self.transcript.typing.is_some() {
            return self.transcript_search_key(key);
        }
        // `Enter` opens or folds the tool call under the cursor: a read-only pane
        // has the key free, and a result a person can see is the difference between
        // a call they can judge and a line they scroll past.
        if key.code == KeyCode::Enter {
            self.toggle_tool();
            return None;
        }
        let grid = self.transcript_grid();
        let max_scroll = self.transcript_len().saturating_sub(usize::from(grid.0));
        let page = i32::from(grid.0).saturating_sub(1).max(1);
        let action = CopyKey::parse(key, page);
        match action {
            CopyKey::Leave => {
                self.set_focus(Focus::Sessions);
                return None;
            }
            CopyKey::Find => {
                self.transcript.begin_search();
                self.status = SEARCH_HINT.to_owned();
                return None;
            }
            CopyKey::Older => {
                self.transcript_step(true);
                return None;
            }
            CopyKey::Newer => {
                self.transcript_step(false);
                return None;
            }
            _ => {}
        }

        let mut view = self.transcript.view;
        match action {
            CopyKey::Select => view.toggle_anchor(),
            CopyKey::SelectLine => view.toggle_line(),
            CopyKey::Move { rows, cols } => view.move_by(rows, cols, grid, max_scroll),
            CopyKey::Top => {
                view.cursor.0 = 0;
                view.scroll = max_scroll;
            }
            CopyKey::Live => {
                view.cursor.0 = self.transcript_len().saturating_sub(1);
                view.scroll = 0;
            }
            CopyKey::Yank => {
                let text = self.transcript_yank(&view);
                let lines = text.lines().count().max(1);
                self.set_focus(Focus::Sessions);
                if text.is_empty() {
                    self.status = "nothing to copy".to_owned();
                } else {
                    self.status = format!("copied {lines} line(s) from the transcript");
                    self.clipboard = Some(text);
                }
                return None;
            }
            CopyKey::Leave | CopyKey::Find | CopyKey::Older | CopyKey::Newer | CopyKey::Nothing => {
            }
        }
        self.transcript.view = view;
        None
    }

    /// The keys a query answers to in the transcript, once `/` has opened a line.
    fn transcript_search_key(&mut self, key: KeyEvent) -> Option<Request> {
        match self.transcript.type_key(key) {
            QueryKey::Typed | QueryKey::Ignored => {}
            QueryKey::Cancelled => self.status = COPY_HINT.to_owned(),
            QueryKey::Run => {
                let query = self.transcript.typing.take().unwrap_or_default();
                if query.is_empty() {
                    self.status = "nothing to find".to_owned();
                    return None;
                }
                let lines = self.transcript_text();
                let grid = self.transcript_grid();
                let max_scroll = lines.len().saturating_sub(usize::from(grid.0));
                let total = self.transcript.search(&query, &lines, max_scroll, grid);
                self.status = find_status(&query, total);
            }
        }
        None
    }

    /// Whether a cell of the transcript is part of the match the search is on.
    #[must_use]
    pub fn matched_transcript(&self, line: usize, col: u16) -> bool {
        self.transcript.matched(line, col)
    }

    /// Move to the next match in the transcript's search, and say where it landed.
    fn transcript_step(&mut self, older: bool) {
        let grid = self.transcript_grid();
        let max_scroll = self.transcript_len().saturating_sub(usize::from(grid.0));
        match self.transcript.step_hit(older, max_scroll, grid) {
            None => self.status = "no search yet · / finds".to_owned(),
            Some((0, 0)) => self.status = "no match in the transcript".to_owned(),
            Some((at, total)) => {
                self.status = format!("match {at}/{total} · n older · N newer · Esc leaves");
            }
        }
    }

    /// The transcript text under the copy cursor.
    ///
    /// The sibling of the terminal's yank: the same corners, over lines of prose
    /// instead of cells of a grid. With nothing selected it takes the cursor's line,
    /// and trailing blanks are dropped because a yank is pasted into a prompt more
    /// often than into a file.
    fn transcript_yank(&self, view: &CopyView) -> String {
        let lines = self.transcript_rows();
        // A line-wise selection is the rows' own text, trimmed of the padding a
        // line does not own. A blank line in the middle stays: it is a line the
        // person marked, and a yank that swallowed it would join two paragraphs.
        if view.line
            && let Some((start, end)) = view.line_selection()
        {
            let mut text = String::new();
            for row in start..=end {
                let Some(line) = lines.get(row) else {
                    break;
                };
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(line.text().trim_end());
            }
            return text;
        }
        let (start, end) = match view.selection() {
            Some(selection) => selection,
            None => {
                let row = view.cursor.0;
                ((row, 0), (row, u16::MAX))
            }
        };
        let mut text = String::new();
        for row in start.0..=end.0 {
            let Some(line) = lines.get(row) else {
                break;
            };
            let chars: Vec<char> = line.text().chars().collect();
            let from = usize::from(if row == start.0 { start.1 } else { 0 }).min(chars.len());
            let to = usize::from(if row == end.0 { end.1 } else { u16::MAX })
                .saturating_add(1)
                .min(chars.len());
            if row > start.0 {
                text.push('\n');
            }
            text.extend(&chars[from..to]);
        }
        text.trim_end().to_owned()
    }

    /// The session the selection points at.
    #[must_use]
    pub fn selected_session(&self) -> Option<&SessionSummary> {
        self.sessions.get(self.selected)
    }

    /// Take one update from the worker.
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Health { version } => {
                self.connected = true;
                self.status = format!("connected · acpilotd {version}");
            }
            Update::Agents(agents) => self.agents = agents,
            Update::Stats(stats) => {
                // The status line's total comes from the same readings the bar draws,
                // so the two cannot disagree about what a conversation costs.
                self.note_tokens(&stats);
                self.integrations = acpilot_stats::rows_now(&stats);
            }
            Update::Sessions(sessions) => {
                // Keep pointing at the same session when the list is reordered by
                // a new one arriving; an index alone would jump to a stranger.
                let current = self.selected_session().map(|session| session.id.clone());
                self.sessions = sessions;
                if let Some(id) = current
                    && let Some(index) = self.sessions.iter().position(|session| session.id == id)
                {
                    self.selected = index;
                }
                if self.selected >= self.sessions.len() {
                    self.selected = self.sessions.len().saturating_sub(1);
                }
            }
            Update::Opened { id } => {
                self.open = Some(id);
                self.entries.clear();
                self.last_seq = 0;
                // A terminal belongs to the session that opened it, so a different
                // transcript has a different list — and no pane to type into.
                self.terminals.clear();
                self.terminal = None;
                // A cursor on lines that no longer exist is a cursor pointing at
                // another session's transcript.
                self.set_focus(Focus::Prompt);
                self.status = "session open".to_owned();
            }
            Update::Terminals(terminals) => {
                self.terminals = terminals;
                // The list can shrink under a picker that is already up — a program
                // ended and the daemon dropped it — and a selection past the end
                // would be an index nothing opens.
                if self.picker_selected >= self.terminals.len() {
                    self.picker_selected = self.terminals.len().saturating_sub(1);
                }
                if self.terminals.is_empty() {
                    self.terminal_picker = false;
                }
            }
            Update::TerminalOpened {
                terminal,
                replay_bytes,
            } => {
                let (cols, rows) = self
                    .pane_size
                    .map_or((80, 24), |size| (size.cols, size.rows));
                self.status = format!(
                    "{} attached · {replay_bytes} bytes of screen",
                    terminal.command
                );
                self.terminal = Some(TerminalPane::new(terminal, cols, rows));
                self.terminal_picker = false;
                // The terminal covers the transcript, so a cursor walking lines
                // behind it would be a cursor on something nobody can see.
                self.set_focus(Focus::Terminal);
            }
            Update::TerminalData(bytes) => {
                if let Some(pane) = &mut self.terminal {
                    let before = pane.history();
                    pane.parser.process(&bytes);
                    // A reader keeps the line they were on. Lines that scroll off
                    // the screen join the history behind them, so the offset grows
                    // by as many as left; otherwise the text under the cursor — and
                    // the cells a selection was put on — would slide by one line per
                    // line of output. Live output follows the program, or a command's
                    // answer would print above the fold.
                    let delta = pane.history().saturating_sub(before);
                    if delta > 0 && (pane.mode == PaneMode::Copy || pane.reader.view.scroll > 0) {
                        // Absolute lines are stable as output appends below them, so
                        // only the window moves: growing the offset by as many lines
                        // as left the screen keeps the same text under the cursor
                        // and the same cells in the selection.
                        pane.reader.view.scroll = pane.reader.view.scroll.saturating_add(delta);
                        pane.apply_scroll();
                    }
                }
            }
            Update::TerminalExited { code } => {
                if let Some(pane) = &mut self.terminal {
                    pane.alive = false;
                    pane.exit = Some(code);
                }
                self.status = match code {
                    Some(code) => format!("terminal exited with code {code}"),
                    None => "terminal exited".to_owned(),
                };
            }
            Update::TerminalClosed => {
                // The screen stays: a command's last output is the part worth
                // reading, and the exit frame has usually just drawn it. Only a
                // program that ended without saying so is announced here.
                if let Some(pane) = &mut self.terminal
                    && pane.exit.is_none()
                {
                    pane.alive = false;
                    self.status = "terminal closed".to_owned();
                }
            }
            Update::Completions { query, rows } => {
                // An answer to the word before this one is an answer about a file
                // the caret has left, the same rule the searches keep.
                if self.mention().map(|(_, mention)| mention).as_deref() == Some(query.as_str()) {
                    self.completion = Some(Completion {
                        query,
                        rows: rows.into_iter().take(8).collect(),
                        selected: 0,
                    });
                }
            }
            Update::Event(event) => self.apply_event(event),
            Update::Permissions(pending) => {
                let was_empty = self.permissions.is_empty();
                // Point at the same request when the list is re-polled under it: a
                // refresh must not move the card to a stranger while it is being
                // read. The one that was answered is gone, so the next takes its
                // place.
                let current = self
                    .permissions
                    .get(self.permission_at)
                    .map(|request| request.id.clone());
                self.permissions = pending;
                self.permission_at = current
                    .and_then(|id| self.permissions.iter().position(|request| request.id == id))
                    .unwrap_or(0);
                // A card appears when work arrives, not when the list is merely
                // refreshed under one that is already up.
                if was_empty && !self.permissions.is_empty() {
                    self.permission_card = true;
                    self.permission_at = 0;
                }
                if self.permissions.is_empty() {
                    self.permission_card = false;
                    self.permission_at = 0;
                }
            }
            Update::Prompted {
                first_seq, turn_id, ..
            } => {
                self.status = format!("turn {turn_id} accepted at {first_seq}");
            }
            Update::Workspace(workspace) => self.workspace = workspace,
            Update::Listing { path, entries } => {
                let count = entries.len();
                self.apply_listing(&path, entries);
                self.status = if path.is_empty() {
                    format!("workspace · {count} entries")
                } else {
                    format!("{path} · {count} entries")
                };
            }
            Update::Found { query, entries } => {
                // An answer to the query before this one is an answer to a question
                // nobody is asking: a person types faster than a walk finishes, and
                // the words on screen are the newest question.
                if self.search.as_deref() == Some(query.as_str()) {
                    let total = entries.len();
                    self.found = Self::rows_from(entries);
                    self.file_selected = 0;
                    self.status = match total {
                        0 => format!("no file matches {query}"),
                        1 => "1 file matches".to_owned(),
                        total => format!("{total} files match"),
                    };
                }
            }
            Update::Matches { query, matches } => {
                // The same rule the path find keeps: an answer to the query before
                // this one is an answer to a question nobody is asking.
                if self.grep.as_ref().is_some_and(|view| view.query == query) {
                    let total = matches.len();
                    if let Some(view) = &mut self.grep {
                        view.matches = matches;
                        view.selected = 0;
                    }
                    self.status = match total {
                        0 => format!("no line contains {query}"),
                        1 => "1 line matches".to_owned(),
                        total => format!("{total} lines match"),
                    };
                }
            }
            Update::Git { status, diff } => {
                let changed = status.files.len();
                let staged = status
                    .files
                    .iter()
                    .filter(|entry| staged_kind(entry.index))
                    .count();
                if let Some(source) = &mut self.source {
                    source.status = status;
                    source.diff = diff;
                    source.loading = false;
                    source.error = None;
                    source.selected = source
                        .selected
                        .min(source.status.files.len().saturating_sub(1));
                }
                self.status = format!("source control · {changed} changed · {staged} staged");
            }
            Update::GitError(message) => {
                if let Some(source) = &mut self.source {
                    source.loading = false;
                    source.error = Some(message.clone());
                }
                self.status = message;
            }
            Update::CommitDraft(draft) => {
                if let Some(commit) = &mut self.commit {
                    commit.text = draft.message;
                    commit.cursor = commit.text.chars().count();
                    commit.generating = false;
                    commit.error = None;
                }
                self.status = format!(
                    "commit draft · {} · edit it, then ctrl-s commits",
                    draft.kind
                );
            }
            Update::CommitError(message) => {
                if let Some(commit) = &mut self.commit {
                    commit.generating = false;
                    commit.committing = false;
                    commit.confirming = false;
                    commit.error = Some(message.clone());
                }
                self.status = message;
            }
            Update::Committed(commit) => {
                let short: String = commit.id.chars().take(7).collect();
                self.commit = None;
                self.status = format!("committed {short} · {}", commit.subject);
                // The index just changed under the pane that was showing it. Ask
                // again rather than leaving a staged file on screen that is already
                // in history.
                self.pending.push(Request::Git);
            }
            Update::References { name, hits } => {
                // The same rule as the searches: an answer about the name before this
                // one is an answer about a word the caret has left.
                if self.references_name == name {
                    let total = hits.len();
                    self.references = hits;
                    self.references_selected = 0;
                    self.status = match total {
                        0 => format!("no code uses {name}"),
                        1 => format!("1 use of {name}"),
                        total => format!("{total} uses of {name}"),
                    };
                }
            }
            Update::Diagnostics { path, problems } => {
                // Only the pane the answer is about: a check that landed while another
                // file was opened must not put this file's errors in that gutter.
                if self
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.path() == path)
                {
                    if let Some(editor) = &mut self.editor {
                        editor.set_problems(problems.items.clone());
                    }
                    self.diagnostics = Some(problems);
                    self.diagnostics_path = path;
                    self.diagnostics_pending = false;
                }
            }
            Update::DiagnosticsPushed(notice) => {
                // A push is not the answer to the question this pane asked, so
                // `checking` is left alone: the two are about different versions of the
                // text, and a frame that cleared it would claim a check that is still
                // running.
                if self
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.path() == notice.path)
                {
                    if let Some(editor) = &mut self.editor {
                        editor.set_problems(notice.problems.items.clone());
                    }
                    self.diagnostics_path = notice.path;
                    self.diagnostics = Some(notice.problems);
                }
            }
            Update::DiagnosticsUnanswered { path, message } => {
                // The question is over either way. What the gutter already holds stays:
                // an answer from before is still the newest thing anybody knows, and a
                // transport failure is not evidence the file is clean.
                if self
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.path() == path)
                {
                    self.diagnostics_pending = false;
                }
                self.status = message;
            }
            Update::ProjectMap(map) => {
                let areas = map.areas.len();
                self.status = if !map.written {
                    format!("no project map yet · the agent writes {}", map.path)
                } else {
                    match areas {
                        1 => "project map · 1 area".to_owned(),
                        areas => format!("project map · {areas} areas"),
                    }
                };
                self.map = Some(map);
                self.map_selected = 0;
            }
            Update::File {
                path,
                text,
                etag,
                marks,
            } => {
                let lines = text.lines().count();
                // The gutter is the daemon's git, answered with the text: the client is
                // not where the repository is, and a `git` run here would be a second
                // opinion about a workspace this process may not even be on.
                let marks: acpilot_git::LineChanges = marks
                    .iter()
                    .map(|mark| {
                        let line = usize::try_from(mark.line.saturating_sub(1)).unwrap_or(0);
                        let change = match mark.change {
                            acpilot_proto::file::LineChange::Added => {
                                acpilot_git::LineChange::Added
                            }
                            acpilot_proto::file::LineChange::Modified => {
                                acpilot_git::LineChange::Modified
                            }
                            acpilot_proto::file::LineChange::Deleted => {
                                acpilot_git::LineChange::Deleted
                            }
                        };
                        (line, change)
                    })
                    .collect();
                let mut pane = EditorPane::from_text(&path, &text, marks, &mut self.grammars);
                pane.set_etag(etag);
                self.editor = Some(pane);
                self.refresh_outline();
                // A different document has not been checked: the previous file's
                // problems are not this one's, and this one is asked about now.
                self.diagnostics = None;
                self.diagnostics_path.clear();
                self.ask_about_problems();
                if let Some(line) = self.pending_line.take()
                    && let Some(editor) = &mut self.editor
                {
                    // The line is a place in a file, and the file arrives after the
                    // row that named it: the jump is taken here, once there is text
                    // to take it into.
                    editor.goto_line(line);
                }
                self.set_focus(Focus::Editor);
                self.status = format!("opened {path} · {lines} lines");
            }
            Update::Saved { path, etag } => {
                // Only the pane the answer is about: a save that landed while another
                // file was opened must not clear the new buffer's unsaved changes.
                let mut saved_this = false;
                if let Some(editor) = &mut self.editor
                    && editor.path() == path
                {
                    editor.set_etag(etag);
                    editor.mark_saved();
                    self.status = format!("saved {path}");
                    saved_this = true;
                }
                // A save is a version even when the text did not change since the open:
                // the file on disk is what other files are compiled against, so the
                // server's opinion of it is worth asking for again.
                if saved_this {
                    self.ask_about_problems();
                }
            }
            Update::Status(message) => self.status = message,
            Update::Theme(theme) => {
                // A settings file this client could read itself changed. Once the
                // daemon has said what the preferences are, this is the weaker of
                // two answers — a local file the daemon does not read (a machine
                // it cannot see, a stale copy) must not overwrite it.
                if self.theme_from_daemon {
                    return;
                }
                self.announce_theme(theme);
            }
            Update::DaemonTheme(theme) => {
                self.theme_from_daemon = true;
                self.announce_theme(theme);
            }
        }
    }

    /// Fold one session event into the transcript.
    ///
    /// Duplicates and out-of-order events are dropped rather than appended: a
    /// reconnecting stream replays from the last sequence it saw, and an
    /// overlapping replay must not repeat a paragraph.
    pub fn apply_event(&mut self, event: SessionEvent) {
        if let Some(open) = &self.open
            && &event.session_id != open
        {
            return;
        }
        if event.seq <= self.last_seq {
            return;
        }
        self.last_seq = event.seq;

        let payload = &event.payload;
        match event.kind {
            SessionEventKind::MessageChunk => {
                let role = payload["role"].as_str().unwrap_or("assistant");
                let kind = if role == "user" {
                    Kind::User
                } else {
                    Kind::Assistant
                };
                if let Some(content) = text_of(&payload["content"]) {
                    self.push(kind, content);
                }
            }
            SessionEventKind::Thought => {
                if let Some(content) = text_of(&payload["content"]) {
                    self.push(Kind::Thought, content);
                }
            }
            SessionEventKind::ToolCall => {
                let status = payload["status"].as_str().unwrap_or("pending");
                let id = payload["tool_call_id"].as_str().unwrap_or_default();
                let title = payload["title"]
                    .as_str()
                    .or_else(|| payload["tool_call_id"].as_str())
                    .unwrap_or("tool call");
                self.push_tool(id, status, title, &tool_detail(payload));
            }
            SessionEventKind::Plan => {
                let steps = payload["entries"]
                    .as_array()
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(|entry| entry["content"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n  ")
                    })
                    .unwrap_or_default();
                self.push(Kind::Notice, &format!("plan\n  {steps}"));
            }
            SessionEventKind::ModeChanged => {
                let mode = payload["mode_id"].as_str().unwrap_or("mode");
                self.push(Kind::Notice, &format!("mode → {mode}"));
            }
            SessionEventKind::Compaction => {
                let status = payload["status"].as_str().unwrap_or("compaction");
                self.push(Kind::Notice, &format!("context {status}"));
            }
            SessionEventKind::CommandsChanged => {
                self.push(Kind::Notice, "the agent's commands changed");
            }
            SessionEventKind::Notice => {
                let title = payload["title"].as_str().unwrap_or("notice");
                let message = payload["message"].as_str().unwrap_or_default();
                let text = if message.is_empty() {
                    title.to_owned()
                } else {
                    format!("{title}\n{message}")
                };
                self.push(Kind::Notice, &text);
            }
        }
    }

    /// Append text, continuing the paragraph before it when the kind matches.
    ///
    /// A tool call never continues the entry before it: two calls are two rows,
    /// and a status update is matched by id in [`App::push_tool`] instead.
    pub fn push(&mut self, kind: Kind, text: &str) {
        match self.entries.last_mut() {
            Some(last) if last.kind == kind && last.tool.is_none() => last.text.push_str(text),
            _ => self.entries.push(Entry {
                kind,
                text: text.to_owned(),
                ..Entry::default()
            }),
        }
    }

    /// Fold one tool-call event into its row, or start a row for it.
    ///
    /// The agent names the call, so `pending` and `completed` for the same id are
    /// one row whose status moved rather than two rows saying the same thing —
    /// and the result is kept for the fold rather than printed under every call.
    fn push_tool(&mut self, id: &str, status: &str, title: &str, detail: &str) {
        let header = format!("[{status}] {title}");
        if !id.is_empty()
            && let Some(entry) = self
                .entries
                .iter_mut()
                .rev()
                .find(|entry| entry.tool.as_ref().is_some_and(|call| call.id == id))
        {
            entry.text = header;
            if let Some(call) = &mut entry.tool {
                call.status = status.to_owned();
                call.title = title.to_owned();
                if !detail.is_empty() {
                    if !call.detail.is_empty() {
                        call.detail.push('\n');
                    }
                    call.detail.push_str(detail);
                }
            }
            return;
        }
        self.entries.push(Entry {
            kind: Kind::Tool,
            text: header,
            tool: Some(ToolCall {
                id: id.to_owned(),
                status: status.to_owned(),
                title: title.to_owned(),
                detail: detail.to_owned(),
            }),
            folded: true,
        });
    }

    /// What a key press means.
    ///
    /// [`None`] means the key changed something on screen and nothing needs to
    /// leave the process.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Request> {
        // A notice is for the keystroke that raised it: the next one means it was
        // read and the person moved on.
        self.toast = None;

        // The About card is a page, not a question: any key turns it. It comes
        // before the palette and the menus because nothing behind it is visible.
        if self.about {
            self.about = false;
            return None;
        }

        // The palette is the outermost question. `Ctrl-P` asks it and dismisses it,
        // and while it is up it owns the keyboard: it is drawn over every pane, so a
        // key reaching one of them would be a key acting on something invisible.
        if self.action(Context::App, key) == Some(Action::Palette) {
            self.toggle_palette();
            return None;
        }
        if self.palette.is_some() {
            return self.palette_key(key);
        }

        // The content search is the next question out: it is drawn over every pane
        // and its keys are text, so letting a key through to a pane would be a key
        // acting on something nobody can see.
        if self.grep.is_some() {
            return self.grep_key(key);
        }

        // The menu bar. `F9` is the key terminals have kept for menus since before
        // windows, and while the bar is up it owns the keyboard: a row drawn over a
        // pane is still the menu's key.
        if self.action(Context::App, key) == Some(Action::Menu) {
            self.toggle_menu();
            return None;
        }
        if self.menu.is_some() {
            return self.menu_key(key);
        }

        // The key list is a reminder, not a mode: the key that dismisses it is also
        // handled, so `?` then `j` scrolls rather than merely closing a panel.
        if self.which_key {
            self.which_key = false;
            if self.action(Context::App, key) == Some(Action::KeyList) {
                return None;
            }
        }

        // The review owns the keyboard while it is up, and it is checked above
        // the terminal for the same reason the picker is: it is a question with a
        // decision in it, and a keystroke aimed at a pane nobody can see would be
        // lost. Above the terminal on purpose — a review that could not be left
        // while a shell was behind it would be a mode with no exit.
        if self.review.is_some() {
            return self.review_key(key);
        }

        // The composer is the innermost question: it covers the source-control
        // pane, so every key is the message's until it closes. The pane behind it
        // is checked next, then `F6`, so the same key opens and closes it.
        if self.commit.is_some() {
            return self.commit_key(key);
        }
        if self.action(Context::App, key) == Some(Action::SourceControl) {
            return self.toggle_source();
        }
        if self.source.is_some() {
            return self.source_key(key);
        }

        // Ctrl-T is the way in and out of the terminal pane, and it outranks the
        // permission card: a shell that could not be left because an agent was
        // waiting would be a pane that traps the session.
        if self.action(Context::App, key) == Some(Action::Terminal) {
            return self.toggle_terminal();
        }

        // The picker is modal in the same way the permission card is: it is a
        // question with a list of answers, and a keystroke aimed at a program that
        // is not on screen yet would be lost.
        if self.terminal_picker {
            return self.picker_key(key);
        }

        // `F4` is the bar's provenance: the same rows with where each figure came
        // from, and the same key back. A function key for the same reason `F2` and
        // `F3` are — a pane whose keys are text gives up nothing printable for it —
        // and handled before the terminal takes the keyboard, because a bar is the
        // client's and a shell has no use for a function key that prints nothing.
        if self.action(Context::App, key) == Some(Action::StatsSources) {
            self.stats_sources = !self.stats_sources;
            return None;
        }

        // A pane being read takes the keys: scrolling, selecting and yanking are
        // the pane's, not the program's. `?` is the exception — it is the key that
        // lists them, and in a byte-for-byte pane it would otherwise be swallowed.
        if self
            .terminal
            .as_ref()
            .is_some_and(|pane| pane.mode == PaneMode::Copy)
        {
            if self.action(Context::App, key) == Some(Action::KeyList) {
                self.which_key = true;
                return None;
            }
            return self.copy_key(key);
        }

        // A terminal takes the keyboard. Every key is a byte for the program,
        // Ctrl-C included: a person typing into a shell means the shell, not
        // "quit the client". The way out is Ctrl-T, which is why the pane says so.
        if self.terminal.is_some() {
            // F2 is the way into copy mode, and a function key on purpose: every
            // printable key and every Ctrl chord is a byte a program can want, and
            // an escape the shell never needed is the only kind the pane can keep.
            if key.code == KeyCode::F(2) {
                if let Some(pane) = &mut self.terminal {
                    pane.begin_copy();
                }
                self.status = COPY_HINT.to_owned();
                return None;
            }
            // F3 is the transcript's way in, and the transcript is behind the pane
            // that is up. Saying so is better than a function key that looks
            // broken.
            if key.code == KeyCode::F(3) {
                self.notice("ctrl-t leaves the terminal before the transcript");
                return None;
            }
            // Answering a program puts the view back at the newest line: a shell
            // whose prompt is about to print below the fold should not be typed
            // into while the screen shows yesterday's output.
            if let Some(pane) = &mut self.terminal {
                pane.follow();
            }
            return terminal_bytes(key).map(Request::TerminalWrite);
        }

        // `Ctrl-D` is the review. It comes after the terminal has had its chance
        // to swallow it, because `Ctrl-D` in a shell is end-of-file and a pane
        // being typed into must keep it; from behind a terminal the review is one
        // `Ctrl-P` away instead.
        if self.action(Context::App, key) == Some(Action::Review) {
            self.toggle_review();
            return None;
        }

        // F3 is the transcript's way in, the sibling of the terminal's F2, and the
        // way back out: a function key on purpose, so no pane whose keys are text
        // has to give one up. `Tab` reaches the same pane without it.
        if self.action(Context::App, key) == Some(Action::Transcript) {
            if self.focus == Focus::Transcript {
                self.set_focus(Focus::Sessions);
            } else {
                self.focus_transcript();
            }
            return None;
        }

        // The divider keys. A chord rather than a mode, because a layout is adjusted
        // once and then read; the pane with the keyboard is the one that changes.
        match self.action(Context::App, key) {
            Some(Action::Narrower) => {
                self.resize(false);
                return None;
            }
            Some(Action::Wider) => {
                self.resize(true);
                return None;
            }
            _ => {}
        }

        // The chords the client owns. Anything else with a Ctrl in it falls through
        // to the focused pane, because a pane has chords of its own — Ctrl-S in the
        // editor, Ctrl-F in a terminal's copy mode.
        match self.action(Context::App, key) {
            Some(Action::Quit) => {
                self.should_quit = true;
                return Some(Request::Quit);
            }
            Some(Action::Refresh) => {
                // The source-control pane is about the repository, not the daemon's
                // session list, so refreshing it is a second question. It is queued
                // rather than returned because one key returns one request.
                if self.source.is_some() {
                    self.pending.push(Request::Git);
                }
                return Some(Request::Refresh {
                    include_ignored: self.show_ignored,
                });
            }
            // `Ctrl-F` is the editor's find box, which is why the content search
            // takes the shift; a file can move either, and the table is where the
            // two are kept apart now.
            Some(Action::Grep) => {
                self.open_grep();
                return None;
            }
            _ => {}
        }

        if self.permission_card && !self.permissions.is_empty() {
            let Some(request) = self.permissions.get(self.permission_at).cloned() else {
                self.permission_card = false;
                return None;
            };
            match key.code {
                // Tab walks the requests that are waiting, because a question does
                // not stop the world: an agent can ask about two files at once.
                KeyCode::Tab => {
                    self.permission_at = (self.permission_at + 1) % self.permissions.len();
                    self.status = format!(
                        "request {} of {}",
                        self.permission_at + 1,
                        self.permissions.len()
                    );
                    return None;
                }
                KeyCode::BackTab => {
                    self.permission_at =
                        (self.permission_at + self.permissions.len() - 1) % self.permissions.len();
                    self.status = format!(
                        "request {} of {}",
                        self.permission_at + 1,
                        self.permissions.len()
                    );
                    return None;
                }
                KeyCode::Esc => {
                    self.permission_card = false;
                    self.status = format!(
                        "permission put aside · {} waiting · Session → Answer Permission",
                        self.permissions.len()
                    );
                    return None;
                }
                KeyCode::Char(character) => {
                    // Only the answers this request actually offers, with the key
                    // the card draws: a card that advertised an option the wire did
                    // not send would answer with something nobody could choose.
                    if let Some(option) = option_for_key(character, &request.options) {
                        let message =
                            format!("{} · {} · recorded", option_label(option), request.title);
                        return self.answer_permission(option, message);
                    }
                }
                _ => {}
            }
            // The card is modal: everything else waits until it is answered.
            return None;
        }

        // `Alt` plus the letter underlined in a title opens that menu with its list
        // showing: the bar's own accelerators, the way a menu bar has named its keys
        // since before a terminal could report a mouse. It sits below the terminal and
        // the permission card on purpose — `Alt-F` is a word to a shell, and the card
        // is a question nothing else may answer over.
        if key.modifiers.contains(KeyModifiers::ALT)
            && let KeyCode::Char(letter) = key.code
            && let Some(index) = menu_for_mnemonic(letter)
        {
            self.menu = Some(MenuState {
                open: index,
                items: true,
                at: 0,
            });
            return None;
        }

        // An editor prompt takes every key while it is up, `?` and `Tab` included:
        // a find box that opened the key list on a question mark would be a box
        // nothing could be typed into.
        if self.focus == Focus::Editor && self.editor.as_ref().is_some_and(EditorPane::is_prompting)
        {
            return self.editor_key(key);
        }

        // Insert mode is a text field, so the keys that would otherwise be the
        // app's — `?` and `Tab` — are the buffer's while it is up. The chords above
        // still work: a pane that could not be left with `Ctrl-T` would be a pane
        // with no way out.
        if self.focus == Focus::Editor
            && self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.mode() == Mode::Insert)
        {
            return self.editor_key(key);
        }

        // Global from here down: the pane cycle and the key list. Everything else
        // belongs to the pane that has the keyboard. `?` is a question mark in the
        // composer, though — it is the one pane whose keys are text.
        // The files pane's find is a text box too: `?` is a character in a path, and a
        // box that opened the key list on one would be a box nobody could type into.
        let editing_a_query = self.search.is_some() && self.focus == Focus::Files;
        if self.action(Context::App, key) == Some(Action::KeyList)
            && self.focus != Focus::Prompt
            && !editing_a_query
        {
            self.which_key = true;
            return None;
        }
        if self.action(Context::App, key) == Some(Action::NextFocus) {
            // Tab never lands in the terminal: a shell wants the byte, so the pane
            // is entered and left with Ctrl-T and the cycle skips it. The transcript
            // is skipped too when there is nothing in it or a terminal covers it,
            // rather than being a step that goes nowhere.
            let next = match self.focus {
                Focus::Files if self.show_outline => Focus::Outline,
                Focus::Outline if self.show_references => Focus::References,
                Focus::References if self.show_map => Focus::ProjectMap,
                Focus::Files => Focus::Editor,
                Focus::Outline => Focus::Editor,
                Focus::References => Focus::Editor,
                Focus::ProjectMap => Focus::Editor,
                Focus::Editor => Focus::Sessions,
                Focus::Sessions if self.readable_transcript() => Focus::Transcript,
                Focus::Sessions => Focus::Prompt,
                Focus::Transcript => Focus::Prompt,
                Focus::Prompt => Focus::Files,
                Focus::Terminal => Focus::Files,
            };
            if next == Focus::Transcript {
                self.focus_transcript();
            } else {
                self.set_focus(next);
            }
            return None;
        }

        match self.focus {
            Focus::Files => self.files_key(key),
            Focus::Outline => self.outline_key(key),
            Focus::References => self.references_key(key),
            Focus::ProjectMap => self.map_key(key),
            Focus::Editor => self.editor_key(key),
            Focus::Sessions => self.sessions_key(key),
            Focus::Transcript => self.transcript_key(key),
            Focus::Prompt => self.prompt_key(key),
            // Unreachable: a pane returns above, because every key is a byte it may
            // want. Named so the compiler keeps this match honest.
            Focus::Terminal => None,
        }
    }

    /// The file tree's keys.
    ///
    /// Movement is local; what the rows mean is the daemon's. `Enter` on a directory
    /// asks for its children and on a file reads it, `H` shows what an ignore rule
    /// hides, and `/` searches the whole workspace by path.
    fn files_key(&mut self, key: KeyEvent) -> Option<Request> {
        // A search is a text field while it is up: `j` is a letter in a path and `H`
        // is a capital, so every printable key belongs to the query.
        if self.search.is_some() {
            return self.search_key(key);
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.file_selected = self.file_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.file_selected + 1 < self.visible_files().len() {
                    self.file_selected += 1;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => return self.set_expanded(false),
            KeyCode::Right | KeyCode::Char('l') => return self.set_expanded(true),
            KeyCode::Enter => return self.activate_row(),
            // The workspace's paths are the daemon's to walk, for the same reason its
            // directories are: the ignore rules and the disk are on that side, and a
            // find here must not see what the tree cannot.
            KeyCode::Char('/') => self.find_file(),
            // The other answer to `tree_respect_gitignore`, for one look: every open
            // directory is asked again so the tree keeps its shape while the hidden
            // rows appear.
            KeyCode::Char('H') => {
                self.show_ignored = !self.show_ignored;
                self.relist();
            }
            _ => {}
        }
        None
    }

    /// The row the keyboard or the pointer is on, acted on: a directory opens or
    /// closes, a file is read.
    ///
    /// One method because `Enter` and a click are the same verb. Two copies would be
    /// two answers to what a row means, and the one that drifted would be the mouse's.
    fn activate_row(&mut self) -> Option<Request> {
        let Some(node) = self.visible_files().get(self.file_selected).cloned() else {
            self.notice("the tree has no rows to open");
            return None;
        };
        if node.dir {
            return self.set_expanded(!node.expanded);
        }
        self.status = format!("opening {}…", node.path);
        Some(Request::OpenFile(node.path))
    }

    /// Open or close the directory the tree's selection is on.
    ///
    /// Opening asks the daemon, because the children are not in the client: the
    /// client is not where the files are, and a tree that read a disk would be a
    /// tree that cannot show a workspace on another machine. Closing is local —
    /// a row nobody is looking at costs nothing to forget.
    fn set_expanded(&mut self, expanded: bool) -> Option<Request> {
        let node = self.files.get(self.file_selected).cloned()?;
        if !node.dir {
            return None;
        }
        if !expanded {
            self.collapse(&node.path);
            return None;
        }
        if node.expanded {
            return None;
        }
        self.status = format!("opening {}…", node.path);
        Some(Request::ListDir {
            path: node.path,
            include_ignored: self.show_ignored,
        })
    }

    /// Drop the rows under a directory, and mark it closed.
    fn collapse(&mut self, path: &str) {
        let Some(index) = self.files.iter().position(|node| node.path == path) else {
            return;
        };
        let depth = self.files[index].depth;
        let end = self.files[index + 1..]
            .iter()
            .position(|node| node.depth <= depth)
            .map_or(self.files.len(), |offset| index + 1 + offset);
        self.files.drain(index + 1..end);
        self.files[index].expanded = false;
        if self.file_selected >= self.files.len() {
            self.file_selected = self.files.len().saturating_sub(1);
        }
    }

    /// Put a listing into the tree.
    ///
    /// The root replaces every row; a subdirectory is spliced in under its own, with
    /// the parent's depth plus one, because the list is flat and `depth` is the only
    /// thing the renderer indents by. A directory that was already open is closed
    /// first, so a second listing of the same place does not double its rows.
    fn apply_listing(&mut self, path: &str, entries: Vec<acpilot_proto::file::FileEntry>) {
        let mut rows = Self::rows_from(entries);

        if path.is_empty() {
            self.files = rows;
            self.file_selected = 0;
            return;
        }
        let Some(index) = self.files.iter().position(|node| node.path == path) else {
            // The row went away between the question and the answer — a refresh
            // replaced the tree. The listing is still the truth about that path, so
            // it becomes the tree rather than being dropped.
            self.files = rows;
            self.file_selected = 0;
            return;
        };
        self.collapse(path);
        let depth = self.files[index].depth + 1;
        let opened = !rows.is_empty();
        for row in &mut rows {
            row.depth = depth;
        }
        let insert_at = index + 1;
        self.files.splice(insert_at..insert_at, rows);
        if opened {
            self.files[index].expanded = true;
            self.file_selected = insert_at;
        }
    }

    /// A listing's rows, before they are placed in the tree.
    fn rows_from(entries: Vec<acpilot_proto::file::FileEntry>) -> Vec<FileNode> {
        let mut rows: Vec<FileNode> = entries
            .into_iter()
            .map(|entry| FileNode {
                path: entry.path,
                depth: 0,
                dir: entry.is_dir,
                expanded: false,
                ignored: entry.ignored,
                // Git's own answer, asked in the daemon: the client is not where
                // the files are, and a second `git` here would be a second opinion
                // about a workspace this process may not even be on.
                status: entry.status,
            })
            .collect();
        // A directory before a file in the same directory, and then by name: a
        // tree that interleaved them would be a tree nobody could scan.
        rows.sort_by(|left, right| right.dir.cmp(&left.dir).then(left.path.cmp(&right.path)));
        rows
    }

    /// The rows the files pane draws: a workspace search's results while one is up,
    /// and the tree otherwise.
    ///
    /// One accessor because the pane, the keys and the mouse all have to agree on
    /// which list is on screen, and an index into the wrong one is a file opened by
    /// accident.
    #[must_use]
    pub fn visible_files(&self) -> &[FileNode] {
        if self.search.is_some() {
            &self.found
        } else {
            &self.files
        }
    }

    /// Note what the conversations behind the sessions cost.
    ///
    /// The readings are one per agent — the newest conversation that agent had —
    /// which is what `GET /v1/stats` reports, so the status line and the bar are the
    /// same figures rather than two counts that drift apart.
    fn note_tokens(&mut self, stats: &[acpilot_proto::stats::IntegrationStats]) {
        let readings: Vec<&acpilot_proto::stats::ContextStats> = stats
            .iter()
            .filter_map(|row| row.context.as_ref())
            .collect();
        self.used_tokens = (!readings.is_empty())
            .then(|| readings.iter().map(|context| context.used_tokens).sum());
        self.tokens_estimated = readings
            .iter()
            .any(|context| context.source == acpilot_proto::stats::StatSource::Estimated);
    }

    /// Ask for the tree again, with the rows the current toggle allows.
    ///
    /// The root first, then every open directory in tree order: the worker answers in
    /// the order it was asked, so each listing is spliced under a parent that is
    /// already there. That is what keeps the tree's shape across a toggle, and a
    /// toggle is exactly when a person wants to see what appeared.
    fn relist(&mut self) {
        let open: Vec<String> = self
            .files
            .iter()
            .filter(|node| node.dir && node.expanded)
            .map(|node| node.path.clone())
            .collect();
        self.pending.push(Request::ListDir {
            path: String::new(),
            include_ignored: self.show_ignored,
        });
        for path in open {
            self.pending.push(Request::ListDir {
                path,
                include_ignored: self.show_ignored,
            });
        }
        self.status = if self.show_ignored {
            "hidden files shown, greyed · H hides them".to_owned()
        } else {
            "hidden files hidden · H shows them".to_owned()
        };
    }

    /// The files pane's search, while it is being typed.
    ///
    /// Letters are text here, so movement is the arrows and nothing else — a query
    /// that could not contain a `j` would be a query nobody could type. `Esc` puts the
    /// tree back exactly as the toggle left it.
    fn search_key(&mut self, key: KeyEvent) -> Option<Request> {
        let query = self.search.clone().unwrap_or_default();
        match key.code {
            KeyCode::Esc => {
                self.search = None;
                self.found.clear();
                self.file_selected = 0;
                self.relist();
                self.status = "the tree is back".to_owned();
            }
            KeyCode::Up => {
                self.file_selected = self.file_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if self.file_selected + 1 < self.found.len() {
                    self.file_selected += 1;
                }
            }
            KeyCode::Backspace => {
                let mut shorter = query;
                shorter.pop();
                return self.ask_for_files(shorter);
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let mut longer = query;
                longer.push(character);
                return self.ask_for_files(longer);
            }
            KeyCode::Enter => {
                let Some(node) = self.found.get(self.file_selected).cloned() else {
                    self.notice("nothing matches to open");
                    return None;
                };
                self.status = format!("opening {}…", node.path);
                return Some(Request::OpenFile(node.path));
            }
            _ => {}
        }
        None
    }

    /// Record the query and ask the daemon to walk the workspace for it.
    fn ask_for_files(&mut self, query: String) -> Option<Request> {
        self.status = format!("/{query}");
        self.search = Some(query.clone());
        if query.is_empty() {
            // A walk for nothing is a walk of the repository: the list clears instead,
            // and the next character is the question.
            self.found.clear();
            self.file_selected = 0;
            return None;
        }
        Some(Request::SearchFiles(query))
    }

    /// Open the tree's find, the box `/` opens in the pane.
    ///
    /// The palette row cannot type into a pane, so it moves the keyboard there
    /// first: a text field that appeared in a pane nobody was looking at would take
    /// the next keystroke from the one they were in.
    fn find_file(&mut self) {
        self.set_focus(Focus::Files);
        self.search = Some(String::new());
        self.found.clear();
        self.file_selected = 0;
        self.status = "find · type a path · Enter opens · Esc puts the tree back".to_owned();
    }

    /// Open the workspace content search.
    fn open_grep(&mut self) {
        self.grep = Some(GrepSearch::default());
        self.status = "find in files · type · Enter opens · Esc closes".to_owned();
    }

    /// The content search's keys: text, a row to choose, and a jump.
    ///
    /// Every printable key is a character, the same rule the path find keeps: a
    /// query that could not contain a `j` would be a query nobody could type.
    fn grep_key(&mut self, key: KeyEvent) -> Option<Request> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let view = self.grep.as_ref()?;
        let selected = view.selected;
        let found = view.matches.len();
        match key.code {
            KeyCode::Esc => {
                self.grep = None;
                self.status = "the search is closed".to_owned();
            }
            KeyCode::Up => {
                if let Some(view) = &mut self.grep {
                    view.selected = selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if selected + 1 < found
                    && let Some(view) = &mut self.grep
                {
                    view.selected = selected + 1;
                }
            }
            KeyCode::Backspace => {
                if let Some(view) = &mut self.grep {
                    view.query.pop();
                    view.selected = 0;
                }
                return self.ask_for_contents();
            }
            KeyCode::Char(character) if !control => {
                if let Some(view) = &mut self.grep {
                    view.query.push(character);
                    view.selected = 0;
                }
                return self.ask_for_contents();
            }
            KeyCode::Enter => {
                let Some(hit) = self
                    .grep
                    .as_ref()
                    .and_then(|view| view.matches.get(selected))
                    .cloned()
                else {
                    self.notice("nothing matches to open");
                    return None;
                };
                self.grep = None;
                self.pending_line = Some(hit.line);
                self.status = format!("opening {}:{}\u{2026}", hit.path, hit.line);
                return Some(Request::OpenFile(hit.path));
            }
            _ => {}
        }
        None
    }

    /// Ask the daemon which lines hold what has been typed.
    fn ask_for_contents(&mut self) -> Option<Request> {
        let query = self.grep.as_ref()?.query.clone();
        self.status = format!("find in files · {query}");
        if query.trim().is_empty() {
            // A walk for nothing is a read of every file: the list clears instead,
            // and the next character is the question.
            if let Some(view) = &mut self.grep {
                view.matches.clear();
                view.selected = 0;
            }
            return None;
        }
        Some(Request::Grep(query))
    }

    /// Requests a key asked for beyond the one it returned.
    pub(crate) fn take_pending(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.pending)
    }

    /// Re-read the open buffer's definitions.
    ///
    /// The parse tree is the outline, so this runs when a buffer arrives and whenever
    /// the text has changed since the last read — not per keystroke: [`App::tick`] is
    /// the clock that debounces a burst of typing into one parse, and the frame is
    /// 120 ms, so the list is at most one frame behind the text.
    fn refresh_outline(&mut self) {
        let Some(editor) = &self.editor else {
            self.outline.clear();
            self.outline_selected = 0;
            self.outline_revision = 0;
            return;
        };
        let text = editor.document().text();
        self.outline = acpilot_text::outline::symbols(editor.language(), &text);
        self.outline_selected = self
            .outline_selected
            .min(self.outline.len().saturating_sub(1));
        self.outline_revision = editor.document().revision();
    }

    /// Catch up on work that is deferred rather than done on the key that caused it.
    ///
    /// The event loop calls this once a frame, after the daemon's answers have been
    /// folded in and before the frame is painted, so the screen shows the list the text
    /// it is drawn beside implies.
    pub fn tick(&mut self) {
        self.tick_at(std::time::Instant::now());
    }

    /// [`App::tick`], with the clock handed in.
    ///
    /// The pause [`App::refresh_problems`] keeps is measured in time rather than in
    /// frames, so a test has to be able to say what time it is; the loop says `now`.
    fn tick_at(&mut self, now: std::time::Instant) {
        self.refresh_outline_if_stale();
        self.follow_references();
        self.ask_for_map();
        self.refresh_problems(now);
    }

    /// Ask the daemon about the open buffer's language.
    ///
    /// Three moments are versions: the open, the save, and a pause in typing.
    /// [`App::refresh_problems`] owns the last of those, because it is the one with a
    /// clock; this is the ask itself, and it remembers which revision it was for so a
    /// frame does not repeat it.
    fn ask_about_problems(&mut self) {
        let Some(editor) = &self.editor else {
            return;
        };
        let path = editor.path().to_owned();
        let text = editor.document().text();
        let revision = editor.document().revision();
        self.diagnostics_revision = revision;
        self.diagnostics_seen = revision;
        self.diagnostics_due = None;
        self.diagnostics_pending = true;
        self.pending.push(Request::Diagnostics { path, text });
    }

    /// Ask about the buffer once it has stopped changing.
    ///
    /// Diagnostics are the one answer worth a request per version of a buffer, and
    /// typing makes a version per key; the daemon may have to start a server on the
    /// first question, and the language-intelligence phase's rule is that the editing
    /// path never waits on one. So the text is asked about only after
    /// [`PROBLEMS_QUIET`] with no edit, measured on the loop's clock rather than on the
    /// key that typed: a burst of typing costs one request when it ends, and the gutter
    /// is at most a frame behind the pause.
    ///
    /// The pause restarts on every edit, because a frame that sees a new revision
    /// pushes the deadline out instead of letting an old one come due. A buffer the
    /// last ask already covered is left alone: the revision is the question, and a
    /// tick with nothing new to say is silent.
    fn refresh_problems(&mut self, now: std::time::Instant) {
        let Some(editor) = &self.editor else {
            self.diagnostics_seen = 0;
            self.diagnostics_due = None;
            return;
        };
        let revision = editor.document().revision();
        if revision != self.diagnostics_seen {
            self.diagnostics_seen = revision;
            self.diagnostics_due = Some(now + PROBLEMS_QUIET);
            return;
        }
        if revision == self.diagnostics_revision {
            self.diagnostics_due = None;
            return;
        }
        if self.diagnostics_due.is_some_and(|due| now >= due) {
            self.ask_about_problems();
        }
    }

    /// Ask the daemon for the map once, when the panel has opened or been told to
    /// re-read.
    ///
    /// The map is a document rather than a view of state that moves, so it is read
    /// when somebody asks — the panel coming up, or `r` in it — and not once a frame
    /// the way the outline follows a buffer that is being typed into.
    fn ask_for_map(&mut self) {
        if !self.show_map || self.map_asked {
            return;
        }
        self.map_asked = true;
        self.pending.push(Request::ProjectMap);
    }

    /// Re-read the outline when the buffer has changed since the last read.
    ///
    /// A closed panel is not re-parsed: opening it re-reads it, and a list nobody can
    /// see is not worth a parse per frame of typing.
    fn refresh_outline_if_stale(&mut self) {
        if !self.show_outline {
            return;
        }
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.document().revision() == self.outline_revision {
            return;
        }
        self.refresh_outline();
    }

    /// Keep the list on the word under the caret, and on the text as it changes.
    ///
    /// The panel is about a word, and the caret is how a person points at one, so the
    /// list follows rather than waiting for `Shift-F12` again. It also follows the
    /// buffer: a use typed while the caret rests on the name is a use the list is
    /// missing, so a changed revision is a new question even when the word is the same.
    ///
    /// Four rules keep that from being one workspace walk per frame: the question is
    /// only asked while the buffer holds the keys; a word about the revision already
    /// listed is not asked about again; the question has to be the same on two frames
    /// running before it is sent; and between two words nothing is asked at all. The
    /// frame is the clock, so a word that changes every frame is a word still being
    /// typed and a buffer that changes every frame is a buffer still being written.
    ///
    /// Between words the list goes rather than lying about a word nobody is pointing
    /// at. Clearing is local and free, so it happens on the frame the caret leaves the
    /// word, while the old rows stay up until the next answer arrives rather than
    /// blinking empty in between.
    fn follow_references(&mut self) {
        if !self.show_references || self.focus != Focus::Editor {
            return;
        }
        let Some(editor) = &self.editor else {
            return;
        };
        let text = editor.document().text();
        let revision = editor.document().revision();
        let caret = editor.caret_offset();
        let path = editor.path().to_owned();
        let Some((name, _)) = acpilot_text::references::identifier_at(&text, caret) else {
            self.references_seen = None;
            if !self.references_name.is_empty() {
                self.references_name.clear();
                self.references.clear();
                self.references_selected = 0;
            }
            return;
        };
        if name == self.references_name && revision == self.references_revision {
            // The list already answers this word about this text.
            self.references_seen = None;
            return;
        }
        if self.references_seen.as_ref() != Some(&(name.clone(), revision)) {
            self.references_seen = Some((name, revision));
            return;
        }
        self.references_name.clone_from(&name);
        self.references_revision = revision;
        self.references.clear();
        self.references_selected = 0;
        self.status = format!("references · {name}\u{2026}");
        self.pending.push(Request::References {
            name,
            path,
            offset: caret,
            text,
        });
    }

    /// The outline's keys: choose a definition, and go to it.
    ///
    /// A row is a place, like every other list in this client: `Enter` puts the caret
    /// on the line the definition starts at and gives the keys back to the buffer,
    /// because the reason to pick a definition is to read or edit it.
    fn outline_key(&mut self, key: KeyEvent) -> Option<Request> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.outline_selected = self.outline_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.outline_selected + 1 < self.outline.len() {
                    self.outline_selected += 1;
                }
            }
            KeyCode::Enter => {
                let Some(symbol) = self.outline.get(self.outline_selected) else {
                    self.notice("the outline has no rows to go to");
                    return None;
                };
                let line = u32::try_from(symbol.line + 1).unwrap_or(u32::MAX);
                let name = symbol.name.clone();
                if let Some(editor) = &mut self.editor {
                    editor.goto_line(line);
                } else {
                    self.notice("no file is open · the outline lists one buffer");
                    return None;
                }
                self.set_focus(Focus::Editor);
                self.status = format!("{name} · line {line}");
            }
            KeyCode::Esc => self.set_focus(Focus::Editor),
            _ => {}
        }
        None
    }

    /// Ask where the name under the caret is used.
    ///
    /// The word is read here rather than in the daemon: the caret is this pane's, and a
    /// request naming a line and a column would make the daemon open the file to find
    /// out what the person meant. The answer arrives as [`Update::References`], and the
    /// panel is put up first so the list has somewhere to land.
    ///
    /// Asking is also the panel's opening move, and after it [`App::follow_references`]
    /// keeps the list on whatever word the caret is standing on.
    fn find_references(&mut self) -> Option<Request> {
        let Some(editor) = &self.editor else {
            self.notice("no file is open · the caret names the word");
            return None;
        };
        let text = editor.document().text();
        let caret = editor.caret_offset();
        let path = editor.path().to_owned();
        let Some((name, _)) = acpilot_text::references::identifier_at(&text, caret) else {
            self.notice("the caret is not on a word");
            return None;
        };
        self.show_references = true;
        self.references_name.clone_from(&name);
        self.references_revision = editor.document().revision();
        // The chord asked about this word already, so the frame behind it does not ask
        // a second time for the name it put up.
        self.references_seen = None;
        self.references.clear();
        self.references_selected = 0;
        self.status = format!("references · {name}\u{2026}");
        Some(Request::References {
            name,
            path,
            offset: caret,
            text,
        })
    }

    /// The project map's keys: choose an area, and read what it says.
    ///
    /// A map is a document, not a list of places: `Enter` puts the area's summary on
    /// the status line rather than opening anything, because the row is already the
    /// answer to "what is this for". `r` asks the daemon again, which is how a map the
    /// agent has just rewritten appears without hiding and showing the panel.
    fn map_key(&mut self, key: KeyEvent) -> Option<Request> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.map_selected = self.map_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let last = self.map.as_ref().map_or(0, |map| map.areas.len());
                if self.map_selected + 1 < last {
                    self.map_selected += 1;
                }
            }
            KeyCode::Enter => {
                let Some(area) = self
                    .map
                    .as_ref()
                    .and_then(|map| map.areas.get(self.map_selected))
                else {
                    self.notice("the map has no areas to read");
                    return None;
                };
                self.status = format!("{} · {}", area.name, area.summary);
            }
            KeyCode::Char('r') => {
                self.map_asked = false;
                self.status = "project map · reading it…".to_owned();
            }
            KeyCode::Esc => self.set_focus(Focus::Editor),
            _ => {}
        }
        None
    }

    /// The references panel's keys: choose a use, and go to it.
    ///
    /// A row is a place, like the outline's: `Enter` opens the file at the line the name
    /// was found on and gives the keys back to the buffer. The list stays up, because
    /// reading the other uses is what a person does next.
    fn references_key(&mut self, key: KeyEvent) -> Option<Request> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.references_selected = self.references_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.references_selected + 1 < self.references.len() {
                    self.references_selected += 1;
                }
            }
            KeyCode::Enter => {
                let Some(hit) = self.references.get(self.references_selected).cloned() else {
                    self.notice("the list has no rows to go to");
                    return None;
                };
                self.pending_line = Some(hit.line);
                self.status = format!("opening {}:{}\u{2026}", hit.path, hit.line);
                return Some(Request::OpenFile(hit.path));
            }
            KeyCode::Esc => self.set_focus(Focus::Editor),
            _ => {}
        }
        None
    }

    /// Write the open buffer back through the daemon.
    ///
    /// The buffer is not written here: the daemon serialises writes per file with the
    /// `if_match` token the read came with, and a client that wrote the disk itself
    /// would be the second writer that token exists to prevent. A buffer with nothing
    /// to save says so rather than sending a write that would replace a file with what
    /// it already contains.
    /// Run a `:` command the editor handed back.
    ///
    /// Deliberately short: `:w` and `:q` are the two a person reaches for while
    /// editing, `:N` is the go-to-line box without opening it, and `:e path` is the
    /// tree's open without leaving the keyboard. Anything else is refused by name
    /// rather than silently doing nothing, because a command line that swallows a
    /// typo is a command line that looks broken.
    fn ex_command(&mut self, command: &str) -> Option<Request> {
        let command = command.trim();
        match command {
            "w" | "write" => self.save(),
            "q" | "quit" => {
                if self.editor.as_ref().is_some_and(|editor| editor.dirty()) {
                    self.notice("unsaved changes · :q! discards them");
                    None
                } else {
                    self.should_quit = true;
                    Some(Request::Quit)
                }
            }
            "q!" | "quit!" => {
                self.should_quit = true;
                Some(Request::Quit)
            }
            _ if !command.is_empty() && command.chars().all(|c| c.is_ascii_digit()) => {
                if let Ok(line) = command.parse::<u32>() {
                    if let Some(editor) = self.editor.as_mut() {
                        editor.goto_line(line);
                    }
                    self.status = format!("line {line}");
                }
                None
            }
            _ if command.starts_with("e ") || command.starts_with("edit ") => {
                let path = command
                    .split_once(' ')
                    .map(|(_, rest)| rest.trim())
                    .unwrap_or_default();
                if path.is_empty() {
                    self.notice(":e needs a path");
                    None
                } else {
                    Some(Request::OpenFile(path.to_owned()))
                }
            }
            _ => {
                self.notice(&format!("not a command: :{command}"));
                None
            }
        }
    }

    fn save(&mut self) -> Option<Request> {
        let Some(editor) = &self.editor else {
            self.notice("no file is open · nothing to save");
            return None;
        };
        if !editor.dirty() {
            self.notice("the buffer matches the file · nothing to save");
            return None;
        }
        let Some(etag) = editor.etag().map(str::to_owned) else {
            self.notice("this buffer did not come from a file · nothing to save");
            return None;
        };
        let path = editor.path().to_owned();
        let text = editor.document().text();
        self.status = format!("saving {path}…");
        Some(Request::SaveFile { path, text, etag })
    }

    /// The editor's keys, over the document the pane holds.
    ///
    /// A prompt takes every key while it is up, because a find box is text and
    /// nothing else; `Enter` ends it and `Esc` takes back what it did. The rest is
    /// the pane's: movement with no goal column (that is a key sequence's
    /// business, P6-5), `/` and `Ctrl-F` for a search that runs as it is typed,
    /// `Ctrl-G` for a line, `n`/`N` to walk the matches.
    ///
    /// Insert mode is the other half: `i`, `a`, `I`, `A`, `o` and `O` enter it,
    /// every printable key is text once there, and `Esc` comes back. Undo is `u` or
    /// `Ctrl-Z` and redo is `Ctrl-Y` — not vim's `Ctrl-R`, which is the client's
    /// refresh from every pane, and a chord that means two things is a bug report.
    ///
    /// What is still not here is not here on purpose: a save goes through the
    /// daemon's files API rather than writing the file from a UI (P7-10 serialises
    /// writes per file), so an edited buffer says so and `Ctrl-S` names the task.
    fn editor_key(&mut self, key: KeyEvent) -> Option<Request> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        // A save is the pane's rather than the buffer's: the write goes through the
        // daemon, so the key is answered before the document is borrowed. The key is
        // the keymap's like every other verb, so moving it off `Ctrl-S` moves the
        // request with it rather than leaving a chord nobody can find.
        if self.action(Context::App, key) == Some(Action::Save) {
            return self.save();
        }
        // What the key means is decided before the document is borrowed: the table
        // is the app's and the pane is the editor's, and a lookup while the pane is
        // held is two borrows of one thing.
        let visual = self.action(Context::Visual, key);
        let editing = self.action(Context::Editor, key);
        let Some(editor) = self.editor.as_mut() else {
            self.notice("no file is open · pick one in the files pane");
            return None;
        };
        if editor.is_prompting() {
            match key.code {
                KeyCode::Esc => editor.prompt_cancel(),
                KeyCode::Enter => {
                    editor.prompt_apply();
                }
                KeyCode::Backspace => editor.prompt_backspace(),
                KeyCode::Char(character) if !control => editor.prompt_insert(character),
                _ => {}
            }
            // `:` is a mode of the box, not of the pane: the pane hands the parsed
            // line back and the app is the end that knows how to save or leave.
            let command = editor.take_command();
            let message = editor.message().to_owned();
            self.status = message;
            if let Some(command) = command {
                return self.ex_command(&command);
            }
            return None;
        }

        // Insert mode: the keys are text, and the ones that are not are the ones
        // that mean something to a text field — a new line, a character gone, an
        // indent, a caret moved. Everything else is a character, including the
        // keys that are commands in normal mode.
        if editor.mode() == Mode::Insert {
            match key.code {
                KeyCode::Esc => editor.set_mode(Mode::Normal),
                KeyCode::Enter => {
                    editor.type_text("\n", std::time::Instant::now());
                }
                KeyCode::Backspace => {
                    editor.backspace(std::time::Instant::now());
                }
                KeyCode::Delete => {
                    editor.delete_forward(std::time::Instant::now());
                }
                // One level is four spaces because nothing in this build reads a
                // tab width yet; the settings cascade's answer is P4-7's.
                KeyCode::Tab => {
                    editor.indent(
                        acpilot_text::edit::IndentMode::In,
                        acpilot_text::edit::IndentStyle::Spaces(4),
                        std::time::Instant::now(),
                    );
                }
                KeyCode::BackTab => {
                    editor.indent(
                        acpilot_text::edit::IndentMode::Out,
                        acpilot_text::edit::IndentStyle::Spaces(4),
                        std::time::Instant::now(),
                    );
                }
                KeyCode::Up => editor.move_caret(-1, 0),
                KeyCode::Down => editor.move_caret(1, 0),
                KeyCode::Left => editor.move_caret(0, -1),
                KeyCode::Right => editor.move_caret(0, 1),
                KeyCode::Home => editor.line_edge(false),
                KeyCode::End => editor.line_edge(true),
                KeyCode::Char('z') if control => {
                    editor.undo();
                }
                KeyCode::Char('y') if control => {
                    editor.redo();
                }
                KeyCode::Char(character)
                    if !control && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    editor.type_text(&character.to_string(), std::time::Instant::now());
                }
                _ => {}
            }
            let message = editor.message().to_owned();
            if !message.is_empty() {
                self.status = message;
            }
            return None;
        }

        // Visual mode: the motions extend the run instead of moving a bare caret,
        // and only the three commands that are about the run are offered. `y`
        // copies it, `d`/`x` delete it, and `v`, `Esc` or a click leave it.
        if editor.mode() == Mode::Visual {
            let mut copied = None;
            match visual {
                Some(Action::Visual | Action::Cancel) => editor.leave_visual(),
                Some(Action::MoveUp) => editor.extend_caret(-1, 0),
                Some(Action::MoveDown) => editor.extend_caret(1, 0),
                Some(Action::MoveLeft) => editor.extend_caret(0, -1),
                Some(Action::MoveRight) => editor.extend_caret(0, 1),
                Some(Action::LineStart) => editor.extend_line_edge(false),
                Some(Action::LineEnd) => editor.extend_line_edge(true),
                Some(Action::PageUp) => editor.extend_page(false),
                Some(Action::PageDown) => editor.extend_page(true),
                Some(Action::Yank) => copied = editor.yank(),
                Some(Action::DeleteSelection) => {
                    editor.delete_selection(std::time::Instant::now());
                }
                Some(Action::Command) => editor.open_command(),
                _ => {}
            }
            let message = editor.message().to_owned();
            if !message.is_empty() {
                self.status = message;
            }
            if let Some(text) = copied {
                self.clipboard = Some(text);
            }
            return None;
        }

        // A key that asks the daemon a question rather than moving the caret: the pane
        // is borrowed for the whole match, so the request is made after it ends.
        let mut asked = false;
        let mut planned = false;
        match editing {
            Some(Action::MoveUp) => editor.move_caret(-1, 0),
            Some(Action::MoveDown) => editor.move_caret(1, 0),
            Some(Action::MoveLeft) => editor.move_caret(0, -1),
            Some(Action::MoveRight) => editor.move_caret(0, 1),
            Some(Action::LineStart) => editor.line_edge(false),
            Some(Action::LineEnd) => editor.line_edge(true),
            Some(Action::PageUp) => editor.page(false),
            Some(Action::PageDown) => editor.page(true),
            Some(Action::Find) => editor.search(),
            Some(Action::GotoLine) => editor.go_to_line(),
            Some(Action::NextMatch) => editor.step_match(true),
            Some(Action::PrevMatch) => editor.step_match(false),
            // A selection is marked first and copied second, the same contract the
            // terminal's copy mode keeps: a gesture that only moved a pointer does not
            // put anything on a clipboard.
            Some(Action::Yank) => {
                if let Some(text) = editor.yank() {
                    self.clipboard = Some(text);
                }
            }
            Some(Action::Visual) => editor.toggle_visual(),
            Some(Action::Command) => editor.open_command(),
            Some(Action::Cancel) => editor.clear_selection(),
            // The way into insert mode, and where the caret lands when it does:
            // `i` here, `a` after it, `I` and `A` at the ends of the line, `o` and
            // `O` on a line of their own. Six keys for one mode because a caret is
            // a position and a person has an opinion about which one to start at.
            Some(Action::Insert) => editor.set_mode(Mode::Insert),
            Some(Action::Append) => {
                editor.move_caret(0, 1);
                editor.set_mode(Mode::Insert);
            }
            Some(Action::InsertLineStart) => {
                editor.line_edge(false);
                editor.set_mode(Mode::Insert);
            }
            Some(Action::InsertLineEnd) => {
                editor.line_edge(true);
                editor.set_mode(Mode::Insert);
            }
            Some(Action::OpenBelow) => editor.open_line_below(std::time::Instant::now()),
            Some(Action::OpenAbove) => editor.open_line_above(std::time::Instant::now()),
            Some(Action::DeleteChar) => {
                editor.delete_forward(std::time::Instant::now());
            }
            // `u` is vim's undo and `Ctrl-Z` is everyone else's; `Ctrl-Z` typed in
            // insert mode reaches the same place, so the hand that learned one
            // editor is not the hand that has to learn this one.
            Some(Action::Undo) => {
                editor.undo();
            }
            Some(Action::Redo) => {
                editor.redo();
            }
            _ => {}
        }
        // `Shift-F12` is where a references list lives in the editors people came
        // from, and it is free beside the planned go-to-definition. Not in the table:
        // one of the two raises a task marker, and a task marker is not a verb a
        // person should be able to move onto a key that works.
        if key.code == KeyCode::F(12) {
            if shift {
                asked = true;
            } else {
                planned = true;
            }
        }
        let message = editor.message().to_owned();
        if !message.is_empty() {
            self.status = message;
        }
        if asked {
            return self.find_references();
        }
        if planned {
            self.planned("P7-4", "go to definition");
        }
        None
    }

    /// The session list's keys.
    fn sessions_key(&mut self, key: KeyEvent) -> Option<Request> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                    self.chose = true;
                    return self.open_selected();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.sessions.len() {
                    self.selected += 1;
                    self.chose = true;
                    return self.open_selected();
                }
            }
            KeyCode::Enter => {
                self.chose = true;
                return self.open_selected();
            }
            KeyCode::Char('i') => {
                self.set_focus(Focus::Prompt);
            }
            KeyCode::Char('n') => {
                if let Some(agent) = self.agents.first() {
                    return Some(Request::New(agent.id.clone()));
                }
            }
            KeyCode::Char('d') => {
                if let Some(session) = self.selected_session() {
                    return Some(Request::Delete(session.id.clone()));
                }
            }
            KeyCode::Char('c') => return Some(Request::Cancel),
            _ => {}
        }
        None
    }

    /// Put the caret where a click in the editor's text landed, and anchor a drag.
    ///
    /// A click is a focus move as well as a caret: a pane you can click into is one
    /// you expect to be typing in. The numbers and the frame are not text, so a
    /// click on them is not a caret — it falls through to the divider drag, which
    /// is what a click on a pane's edge is for.
    fn press_editor(&mut self, column: u16, row: u16, shift: bool) -> bool {
        let Some(area) = self.editor_area else {
            return false;
        };
        if !area.is_text(column, row) {
            return false;
        }
        self.set_focus(Focus::Editor);
        if let Some(editor) = self.editor.as_mut() {
            let row = usize::from(row - area.pane.y);
            let cell = usize::from(column - area.pane.x - area.gutter);
            if shift {
                editor.extend_click(row, cell);
            } else {
                editor.click(row, cell);
            }
        }
        true
    }

    /// Grow the editor's selection to where the pointer is.
    ///
    /// Counted from the pane's corner and clamped to it, the same way the terminal's
    /// is: a drag that left the pane selects to the edge it left by rather than
    /// dropping the gesture. The gutter is not text, so the cell the pointer is on
    /// is the cell of the text, which starts after the numbers.
    fn drag_editor(&mut self, column: u16, row: u16) {
        let Some(area) = self.editor_area else {
            return;
        };
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        editor.extend_to(
            usize::from(
                row.saturating_sub(area.pane.y)
                    .min(area.pane.height.saturating_sub(1)),
            ),
            usize::from(
                column
                    .saturating_sub(area.pane.x.saturating_add(area.gutter))
                    .min(
                        area.pane
                            .width
                            .saturating_sub(area.gutter)
                            .saturating_sub(1),
                    ),
            ),
        );
    }

    /// What letting go in the editor does: keep a selection, or leave a bare caret.
    ///
    /// A click that selected nothing leaves the caret where it was put and says
    /// nothing, because that is all a click is. A real selection stays highlighted
    /// and the status line names the two keys that finish it — nothing goes to the
    /// clipboard until the person says `y`, the same contract the terminal's copy
    /// mode keeps.
    fn release_editor(&mut self) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if !editor.has_selection() {
            editor.clear_selection();
            return;
        }
        let lines = editor.selected_lines();
        self.status = format!("selected {lines} line(s) · y copies · Esc clears");
    }

    /// Focus the tree, on the row a click landed on.
    ///
    /// The tree is a list of one cell per row and a click is that row: the marker and
    /// the indentation are drawn inside the row, so the row the pointer is over is the
    /// row it means. A click past the last file is still the pane's — it takes the
    /// keyboard and moves nothing.
    fn click_files(&mut self, column: u16, row: u16) -> Option<Option<Request>> {
        let area = self.files_area?;
        if !area.holds(column, row) {
            return None;
        }
        self.set_focus(Focus::Files);
        // The row a cell is, through the same window the drawing used: the pane shows
        // the selection and the rows around it, so a click's own row is the drawn one
        // plus the row it was drawn at.
        let height = usize::from(area.height).max(1);
        let len = self.visible_files().len();
        let first = first_visible(self.file_selected, len, height);
        let index = first + usize::from(row.saturating_sub(area.y));
        if index >= len {
            return Some(None);
        }
        self.file_selected = index;
        // A click runs the row's own verb, the same one `Enter` runs: a pointer is a
        // second way to say what the keyboard says, and a click that only moved the
        // cursor would be half of a file manager's job.
        Some(self.activate_row())
    }

    /// Focus the session list, on the session a click landed on, and open it.
    ///
    /// [`None`] means the click was not the list's. A click that lands inside the
    /// pane but past the last session takes the keyboard and opens nothing: the list
    /// is a list of sessions, not a grid, and the empty rows under it are not one.
    fn click_sessions(&mut self, column: u16, row: u16) -> Option<Option<Request>> {
        let area = self.sessions_area?;
        if !area.holds(column, row) {
            return None;
        }
        self.set_focus(Focus::Sessions);
        let index = usize::from(row.saturating_sub(area.y));
        if index >= self.sessions.len() {
            return Some(None);
        }
        self.selected = index;
        self.chose = true;
        Some(self.open_selected())
    }

    /// Scroll the pane under the wheel: `up` is towards the start of what it shows.
    ///
    /// A wheel is pointed rather than aimed, so it moves no caret, no copy cursor
    /// and no focus — it scrolls whatever it is over, and it does not have to be
    /// over the pane holding the keyboard. The editor and the terminal are the two
    /// panes with something to scroll; over anything else it is a no-op rather than
    /// a guess.
    fn scroll_under_pointer(&mut self, column: u16, row: u16, up: i32) {
        if self.scroll_files(column, row, up) {
            return;
        }
        if self.scroll_editor(column, row, -up) {
            return;
        }
        self.scroll_terminal(column, row, up);
    }

    /// Walk the file manager's selection with the wheel.
    ///
    /// The pane is a list of rows rather than a document, so the wheel moves what is
    /// chosen and the window follows it. That keeps one answer to "where is the
    /// cursor": a remembered scroll offset could disagree with the selection it is
    /// drawn around, which is how a tree ends up with a highlighted row nobody can
    /// see.
    fn scroll_files(&mut self, column: u16, row: u16, up: i32) -> bool {
        let Some(area) = self.files_area else {
            return false;
        };
        if !area.holds(column, row) {
            return false;
        }
        let last = self.visible_files().len().saturating_sub(1);
        let step = usize::try_from(up.unsigned_abs()).unwrap_or(1).max(1);
        self.file_selected = if up > 0 {
            self.file_selected.saturating_sub(step)
        } else {
            self.file_selected.saturating_add(step).min(last)
        };
        true
    }

    /// Scroll the pane a drag is resting against, and grow the selection with it.
    ///
    /// A pointer cannot leave the screen, so a drag alone can only ever mark what
    /// one screen holds. Resting on the top or bottom row of the pane the drag
    /// began in is the ask for the rest: the view moves a line and the selection
    /// keeps growing, which is how a selection reaches past the edge it is being
    /// dragged against. Like the wheel, the gesture is the pointer's, and it is
    /// also a key: `Ctrl-U`/`Ctrl-D` and Page Up/Down reach the same lines.
    ///
    /// Returns whether the view actually moved, so the loop knows whether to keep
    /// a faster clock: a drag resting at the top of a file has run out of file and
    /// should not redraw thirty times a second to prove it.
    ///
    /// What stays put is the part already marked. The editor's selection is a
    /// buffer offset, so scrolling the view under it cannot move it; the terminal's
    /// is a cell of the view, so the pane shifts it with its text — a drag across a
    /// shell's output must not quietly reassign the cells it covered.
    pub fn autoscroll(&mut self) -> bool {
        let step = self.edge_step();
        let Some(step) = step else {
            self.drag_scroll = false;
            return false;
        };
        let Some((column, row)) = self.drag_at else {
            self.drag_scroll = false;
            return false;
        };
        let moved = match self.drag {
            Some(Drag::Editor) => {
                let moved = self
                    .editor
                    .as_mut()
                    .is_some_and(|editor| editor.scroll(step));
                if moved {
                    self.drag_editor(column, row);
                }
                moved
            }
            Some(Drag::Terminal) => {
                // The editor's step is a direction in the file; the terminal's is
                // a scroll, and older output is positive.
                let moved = self
                    .terminal
                    .as_mut()
                    .is_some_and(|pane| pane.scroll_selection(-step));
                if moved {
                    self.drag_terminal(column, row);
                }
                moved
            }
            _ => false,
        };
        self.drag_scroll = moved;
        moved
    }

    /// Which way a resting drag is pulling the view: `-1` past the top, `1` past
    /// the bottom, nothing for a pointer that is over the pane.
    ///
    /// Rows are what an edge is here. A drag that has wandered sideways is still
    /// at the top of the pane if it is on the pane's top row or above it, and a
    /// drag that left the window upward is asking for more of the same.
    fn edge_step(&self) -> Option<i32> {
        let area = match self.drag {
            Some(Drag::Editor) => self.editor_area.map(|area| area.pane),
            Some(Drag::Terminal) => self.terminal_area,
            _ => return None,
        }?;
        if area.height == 0 {
            return None;
        }
        let (_, row) = self.drag_at?;
        let last = area.y.saturating_add(area.height.saturating_sub(1));
        if row <= area.y {
            return Some(-1);
        }
        if row >= last {
            return Some(1);
        }
        None
    }

    /// Whether the loop should keep the fast clock for a drag at an edge.
    ///
    /// Asked after [`App::autoscroll`], so it is the last move's answer rather than
    /// a guess: true only while there is more view in the direction the pointer is
    /// pulling.
    #[must_use]
    pub fn autoscrolling(&self) -> bool {
        self.drag_scroll
    }

    /// Scroll the terminal under the wheel, without taking the keyboard.
    fn scroll_terminal(&mut self, column: u16, row: u16, up: i32) -> bool {
        let Some(area) = self.terminal_area else {
            return false;
        };
        if !area.holds(column, row) {
            return false;
        }
        if let Some(pane) = self.terminal.as_mut() {
            pane.wheel(up);
        }
        true
    }

    /// Scroll the editor under the wheel, without moving the caret.
    fn scroll_editor(&mut self, column: u16, row: u16, lines: i32) -> bool {
        let Some(area) = self.editor_area else {
            return false;
        };
        if !area.holds(column, row) {
            return false;
        }
        if let Some(editor) = self.editor.as_mut() {
            editor.scroll(lines);
        }
        true
    }

    /// The composer's keys.
    ///
    /// A text field with more than one line: `Enter` sends and `Alt-Enter`, or
    /// `Shift-Enter` where the terminal reports it, writes a newline. The popup's
    /// keys come first because they are answers to a question that is on screen.
    fn prompt_key(&mut self, key: KeyEvent) -> Option<Request> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if self.completion.is_some() {
            match key.code {
                KeyCode::Up => {
                    if let Some(completion) = &mut self.completion {
                        completion.selected = completion.selected.saturating_sub(1);
                    }
                    return None;
                }
                KeyCode::Down => {
                    if let Some(completion) = &mut self.completion {
                        let last = completion.rows.len().saturating_sub(1);
                        completion.selected = (completion.selected + 1).min(last);
                    }
                    return None;
                }
                KeyCode::Esc => {
                    self.completion = None;
                    return None;
                }
                KeyCode::Tab | KeyCode::Enter if !control && !alt && !shift => {
                    self.accept_completion();
                    return None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter if alt || shift || control => {
                self.insert_character('\n');
                self.refresh_completion()
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_owned();
                if text.is_empty() {
                    return None;
                }
                self.input.clear();
                self.cursor = 0;
                self.completion = None;
                // A local echo: the daemon sends the user chunk too, and the
                // de-duplication by sequence keeps this from doubling it.
                self.push(Kind::User, &format!("{text}\n"));
                Some(Request::Prompt(text))
            }
            KeyCode::Tab => self.refresh_completion(),
            KeyCode::Backspace => {
                self.backspace();
                self.refresh_completion()
            }
            KeyCode::Delete => {
                self.delete_character();
                self.refresh_completion()
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                self.completion = None;
                None
            }
            KeyCode::Right => {
                if self.cursor < self.input.chars().count() {
                    self.cursor += 1;
                }
                self.completion = None;
                None
            }
            KeyCode::Up => {
                self.composer_move(-1);
                None
            }
            KeyCode::Down => {
                self.composer_move(1);
                None
            }
            KeyCode::Home => {
                let before: String = self.input.chars().take(self.cursor).collect();
                let row = before.matches('\n').count();
                self.cursor = self
                    .input
                    .split('\n')
                    .take(row)
                    .map(|line| line.chars().count() + 1)
                    .sum();
                self.completion = None;
                None
            }
            KeyCode::End => {
                let before: String = self.input.chars().take(self.cursor).collect();
                let row = before.matches('\n').count();
                let start: usize = self
                    .input
                    .split('\n')
                    .take(row)
                    .map(|line| line.chars().count() + 1)
                    .sum();
                self.cursor = start
                    + self
                        .input
                        .split('\n')
                        .nth(row)
                        .map_or(0, |line| line.chars().count());
                self.completion = None;
                None
            }
            KeyCode::Esc => {
                self.set_focus(Focus::Sessions);
                None
            }
            KeyCode::Char(character) if !control => {
                self.insert_character(character);
                self.refresh_completion()
            }
            _ => None,
        }
    }

    /// Open the palette, or close it.
    fn toggle_palette(&mut self) {
        if self.palette.is_some() {
            self.palette = None;
            return;
        }
        self.palette = Some(Palette::default());
        self.status = format!("{} commands · type to filter", self.commands.len());
    }

    /// Open the menu bar, or shut it.
    fn toggle_menu(&mut self) {
        self.menu = match self.menu {
            Some(_) => None,
            None => Some(MenuState {
                open: 0,
                items: false,
                at: 0,
            }),
        };
    }

    /// The menu bar's keys: walk the titles, open one, run a row.
    ///
    /// `Esc` steps out one level at a time — the list first, then the bar — so a
    /// person who opened a menu by mistake is one key from where they were.
    fn menu_key(&mut self, key: KeyEvent) -> Option<Request> {
        let mut menu = self.menu?;
        match key.code {
            KeyCode::Esc => {
                if menu.items {
                    menu.items = false;
                } else {
                    self.menu = None;
                    return None;
                }
            }
            // Another `Alt` letter switches menus without closing the one that is
            // open, which is what the bar's accelerators are for.
            KeyCode::Char(letter) if key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(index) = menu_for_mnemonic(letter) {
                    menu.open = index;
                    menu.items = true;
                    menu.at = 0;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                menu.open = (menu.open + MENUS.len() - 1) % MENUS.len();
                menu.items = false;
                menu.at = 0;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                menu.open = (menu.open + 1) % MENUS.len();
                menu.items = false;
                menu.at = 0;
            }
            KeyCode::Down | KeyCode::Enter if !menu.items => {
                menu.items = true;
                menu.at = 0;
            }
            KeyCode::Up if menu.items => {
                menu.items = false;
            }
            KeyCode::Down | KeyCode::Char('j') if menu.items => {
                let last = MENUS[menu.open].items.len().saturating_sub(1);
                menu.at = (menu.at + 1).min(last);
            }
            KeyCode::Up | KeyCode::Char('k') if menu.items => {
                menu.at = menu.at.saturating_sub(1);
            }
            KeyCode::Enter if menu.items => {
                let action = MENUS[menu.open].items.get(menu.at).map(|row| row.action);
                self.menu = None;
                return action.and_then(|action| self.run_menu(action));
            }
            _ => {}
        }
        self.menu = Some(menu);
        None
    }

    /// Do what a menu row says.
    fn run_menu(&mut self, action: MenuAction) -> Option<Request> {
        match action {
            MenuAction::About => self.about = true,
            MenuAction::Palette => self.toggle_palette(),
            MenuAction::Keys => self.which_key = true,
            MenuAction::Refresh => {
                return Some(Request::Refresh {
                    include_ignored: self.show_ignored,
                });
            }
            MenuAction::Files => self.set_focus(Focus::Files),
            MenuAction::Editor => self.set_focus(Focus::Editor),
            MenuAction::Sessions => self.set_focus(Focus::Sessions),
            MenuAction::Transcript => self.focus_transcript(),
            MenuAction::SourceControl => return self.toggle_source(),
            MenuAction::Terminal => return self.toggle_terminal(),
            MenuAction::MovePanel(panel, side) => {
                self.panels.move_to(panel, side);
                self.status = format!("{} · {}", panel.label(), side.label());
            }
            MenuAction::ToggleOutline => {
                self.show_outline = !self.show_outline;
                if self.show_outline {
                    self.refresh_outline();
                }
                self.status = if self.show_outline {
                    format!("outline · {} symbols", self.outline.len())
                } else {
                    "outline closed".to_owned()
                };
            }
            MenuAction::ToggleReferences => {
                self.show_references = !self.show_references;
                self.status = if self.show_references {
                    format!("references · {} uses", self.references.len())
                } else {
                    "references closed".to_owned()
                };
            }
            MenuAction::ToggleProjectMap => {
                self.show_map = !self.show_map;
                // Either way the next frame may ask: opened, it has no map yet;
                // closed and opened again, the file on disk may have been rewritten.
                self.map_asked = false;
                self.status = if self.show_map {
                    "project map · reading it…".to_owned()
                } else {
                    "project map closed".to_owned()
                };
            }
            MenuAction::FindReferences => return self.find_references(),
            MenuAction::NewSession => {
                return self
                    .agents
                    .first()
                    .map(|agent| Request::New(agent.id.clone()));
            }
            MenuAction::Cancel => return Some(Request::Cancel),
            MenuAction::AnswerPermission => self.reopen_permission(),
            MenuAction::Quit => {
                self.should_quit = true;
                return Some(Request::Quit);
            }
        }
        None
    }

    /// Make the focused pane's column wider or narrower.
    ///
    /// There are two dividers: the tree and the editor trade columns against each
    /// other, and the sidebar trades against both. The arrow follows the pane that
    /// has the keyboard rather than the divider, because the pane is what a person
    /// is looking at — so growing the editor is shrinking the tree.
    fn resize(&mut self, wider: bool) {
        const STEP: u16 = 2;
        let grow = if self.focus == Focus::Editor {
            !wider
        } else {
            wider
        };
        match self.focus {
            Focus::Files | Focus::Editor => {
                self.files_width = if grow {
                    (self.files_width + STEP).min(MAX_FILES)
                } else {
                    self.files_width.saturating_sub(STEP).max(MIN_FILES)
                };
            }
            Focus::Sessions | Focus::Transcript | Focus::Prompt | Focus::Terminal => {
                self.sidebar_width = if grow {
                    (self.sidebar_width + STEP).min(MAX_SIDEBAR)
                } else {
                    self.sidebar_width.saturating_sub(STEP).max(MIN_SIDEBAR)
                };
            }
            // The outline's column is a fixed width, so an arrow here has nothing to
            // widen. Falling through to the assistant's width — which the `_` arm did
            // — would have been an arrow in one pane resizing another.
            Focus::Outline | Focus::References | Focus::ProjectMap => {
                self.status = "a panel's column is fixed · Panels moves it".to_owned();
                return;
            }
        }
        self.status = format!(
            "panes · tree {} · sidebar {} columns",
            self.files_width, self.sidebar_width
        );
    }

    /// Open with the widths a previous launch left, and remember where they live.
    ///
    /// Only the clients that have somewhere to keep them call this: a width is
    /// remembered per machine and not per project, because the window is the same
    /// window whatever workspace is in it.
    /// Read the keymap a previous launch left, or the default table.
    ///
    /// A file that does not parse is reported and the preset is used: refusing to
    /// start over a key file would be worse than starting on the keys it describes
    /// everywhere else. The caller says where the problem is, because the screen is
    /// not up yet when this runs.
    pub fn remember_keymap(&mut self, path: std::path::PathBuf) -> Option<String> {
        match Keymap::load(&path) {
            Ok(keymap) => {
                self.keymap = keymap;
                None
            }
            Err(error) => Some(error),
        }
    }

    /// What a key means in one context, according to the table in force.
    #[must_use]
    pub fn action(&self, context: Context, key: KeyEvent) -> Option<Action> {
        self.keymap.action(context, key)
    }

    pub fn remember_layout(&mut self, path: std::path::PathBuf) {
        let layout = Layout::load(&path);
        self.files_width = layout.files;
        self.sidebar_width = layout.sidebar;
        self.layout_saved = layout;
        self.layout_path = Some(path);
    }

    /// Write the widths on the way out, when they are not the ones that were read.
    ///
    /// Once, at the end, rather than on every frame of a drag: a remembered column
    /// is not worth a file write per mouse move, and nothing is worth keeping until
    /// the dragging has stopped.
    ///
    /// The error is handed back rather than swallowed or printed here: the screen
    /// has to be the person's again before anything can be said to them, and only
    /// the caller is in a position to know that it is.
    pub fn save_layout(&mut self) -> Option<std::io::Error> {
        let path = self.layout_path.clone()?;
        let layout = self.layout();
        if layout == self.layout_saved {
            return None;
        }
        match layout.save(&path) {
            Ok(()) => {
                self.layout_saved = layout;
                None
            }
            Err(error) => Some(error),
        }
    }

    /// The widths as they are now, held to what a pane can be drawn at.
    #[must_use]
    fn layout(&self) -> Layout {
        Layout::new(self.files_width, self.sidebar_width)
    }

    /// What a mouse event means: a menu, a divider, or a selection.
    ///
    /// Those are the three things a pointer is better at than a key: a menu bar with
    /// a list is a target a person can see and hit, a width has no key that can aim,
    /// and a run of cells is one drag rather than eighty motions. Every one of them
    /// is also a key: the mouse is a second way to do what the keyboard already
    /// does, and never the only way.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> Option<Request> {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(request) = self.click_menu(event.column, event.row) {
                    return request;
                }
                if self.press_editor(
                    event.column,
                    event.row,
                    event.modifiers.contains(KeyModifiers::SHIFT),
                ) {
                    self.drag = Some(Drag::Editor);
                    self.drag_at = Some((event.column, event.row));
                    return None;
                }
                if self.click_terminal(event.column, event.row) {
                    self.drag = Some(Drag::Terminal);
                    self.drag_at = Some((event.column, event.row));
                    return None;
                }
                // A divider is a cell and a half wide, so it is looked for before
                // the panes beside it: a click on the tree's inner edge is still a
                // grab, which is what makes them easy to hit.
                if let Some(divider) = self.divider_at(event.column) {
                    self.drag = Some(Drag::Divider(divider));
                    self.drag_at = None;
                    return None;
                }
                if let Some(request) = self.click_files(event.column, event.row) {
                    return request;
                }
                if let Some(request) = self.click_sessions(event.column, event.row) {
                    return request;
                }
                self.drag = None;
                self.drag_at = None;
                None
            }
            // The wheel scrolls the pane under the pointer and nothing else: it is
            // a thing you point at, so it does not have to be focused first.
            MouseEventKind::ScrollUp => {
                self.scroll_under_pointer(event.column, event.row, WHEEL_LINES);
                None
            }
            MouseEventKind::ScrollDown => {
                self.scroll_under_pointer(event.column, event.row, -WHEEL_LINES);
                None
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.drag_at = Some((event.column, event.row));
                match self.drag {
                    Some(Drag::Divider(divider)) => self.drag_to(divider, event.column),
                    Some(Drag::Terminal) => self.drag_terminal(event.column, event.row),
                    Some(Drag::Editor) => self.drag_editor(event.column, event.row),
                    None => {}
                }
                None
            }
            // Motion is only for the menu: a bar whose titles light up under the
            // pointer, and a list whose row follows it. Nothing else follows a mouse
            // around, so nothing else is even looked at.
            MouseEventKind::Moved => {
                self.hover_menu(event.column, event.row);
                None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                match self.drag {
                    Some(Drag::Terminal) => self.release_terminal(),
                    Some(Drag::Editor) => self.release_editor(),
                    _ => {}
                }
                self.drag = None;
                self.drag_at = None;
                self.drag_scroll = false;
                None
            }
            _ => None,
        }
    }

    /// Begin the terminal's selection where a click landed.
    ///
    /// A click in a shell's output means the output, so the pane enters copy mode
    /// with a selection anchored on the cell under the pointer and lets a drag grow
    /// it. Nothing is taken that was not already taken — a terminal that is up has
    /// the keyboard — and nothing is copied yet: a selection is marked first and
    /// yanked when the person says so, which is the same contract `v` has.
    ///
    /// A query being typed owns the keyboard, so a click does not move the
    /// selection out from under it.
    fn click_terminal(&mut self, column: u16, row: u16) -> bool {
        let Some(area) = self.terminal_area else {
            return false;
        };
        if !area.holds(column, row) {
            return false;
        }
        let Some(pane) = self.terminal.as_mut() else {
            return false;
        };
        if pane.reader.typing.is_some() {
            return false;
        }
        pane.select_at((row - area.y, column - area.x));
        true
    }

    /// Grow the terminal's selection to where the pointer is.
    ///
    /// Counted from the pane's own corner, and clamped to it: a drag that left the
    /// pane selects to the edge it left by rather than dropping the gesture.
    fn drag_terminal(&mut self, column: u16, row: u16) {
        let Some(area) = self.terminal_area else {
            return;
        };
        let Some(pane) = self.terminal.as_mut() else {
            return;
        };
        pane.extend_to((
            row.saturating_sub(area.y)
                .min(area.height.saturating_sub(1)),
            column
                .saturating_sub(area.x)
                .min(area.width.saturating_sub(1)),
        ));
    }

    /// What letting go of the button does: keep a selection, or give the keys back.
    ///
    /// A click that selected nothing must not leave the pane in copy mode. Copy mode
    /// is the state in which a keystroke goes nowhere near the program, and a person
    /// who clicked a shell and went back to typing would be typing at a pane that had
    /// quietly stopped listening — so a click that marked one cell or none ends what
    /// it began. A real selection stays, and the status line says how to take it.
    fn release_terminal(&mut self) {
        let Some(pane) = self.terminal.as_mut() else {
            return;
        };
        if !pane.has_selection() {
            pane.end_copy();
            return;
        }
        let lines = pane.selected_lines();
        self.status = format!("selected {lines} line(s) · y copies · Esc leaves");
    }

    /// A click on the bar or the open list, answered.
    ///
    /// [`None`] means the click was not the menu's and belongs to whatever is under
    /// it. A click on a title opens that menu, closes it if it is the one already
    /// open, and switches to it otherwise; a click on a row runs it; a click anywhere
    /// else dismisses what is open, because a list drawn over the panes would
    /// otherwise let a click land on something the person cannot see.
    fn click_menu(&mut self, column: u16, row: u16) -> Option<Option<Request>> {
        let areas = self.menu_areas;
        if row == areas.row
            && let Some(index) = menu_index_at(column)
        {
            self.menu = match self.menu {
                Some(state) if state.open == index && state.items => None,
                Some(state) if state.open == index => Some(MenuState {
                    items: true,
                    ..state
                }),
                _ => Some(MenuState {
                    open: index,
                    items: true,
                    at: 0,
                }),
            };
            return Some(None);
        }
        let state = self.menu?;
        let action = areas
            .list
            .and_then(|list| row_at(list, column, row))
            .and_then(|at| MENUS[state.open].items.get(at))
            .map(|row| row.action);
        self.menu = None;
        // A row that only changes the screen returns nothing, and a click that ran it
        // is still the menu's click: the bar must not also start a drag underneath.
        Some(action.and_then(|action| self.run_menu(action)))
    }

    /// The title, or the row, a pointer is over.
    ///
    /// A menu that is open follows a pointer onto another title — the bar is a row of
    /// targets and sliding along it is how every menu bar since windows has been
    /// used. A menu that is not open is not: nothing lights up under a pointer that
    /// is only passing over the chrome.
    fn hover_menu(&mut self, column: u16, row: u16) {
        let Some(mut state) = self.menu else {
            return;
        };
        if row == self.menu_areas.row {
            if let Some(index) = menu_index_at(column)
                && index != state.open
            {
                state.open = index;
                state.at = 0;
                self.menu = Some(state);
            }
            return;
        }
        if let Some(list) = self.menu_areas.list
            && let Some(at) = row_at(list, column, row)
            && at < MENUS[state.open].items.len()
            && at != state.at
        {
            state.at = at;
            self.menu = Some(state);
        }
    }

    /// The divider under a column, give or take a cell: a one-cell target is a
    /// target people miss, and there are only two of them.
    fn divider_at(&self, column: u16) -> Option<Divider> {
        let near = |edge: Option<u16>| edge.is_some_and(|edge| column.abs_diff(edge) <= 1);
        if near(self.dividers.tree) {
            Some(Divider::Tree)
        } else if near(self.dividers.sidebar) {
            Some(Divider::Sidebar)
        } else {
            None
        }
    }

    /// Move a divider to a column, inside the widths the panes can live in.
    fn drag_to(&mut self, divider: Divider, column: u16) {
        match divider {
            Divider::Tree => {
                // The tree's right border is the divider, so its width is the drag
                // column less the body's left edge — plus the border cell itself.
                self.files_width = column
                    .saturating_sub(self.dividers.left)
                    .saturating_add(1)
                    .clamp(MIN_FILES, MAX_FILES);
            }
            Divider::Sidebar => {
                // The sidebar's width is everything from the drag column to the right
                // edge of the body.
                self.sidebar_width = self
                    .dividers
                    .right
                    .saturating_sub(column)
                    .clamp(MIN_SIDEBAR, MAX_SIDEBAR);
            }
        }
        self.status = format!(
            "panes · tree {} · sidebar {} columns",
            self.files_width, self.sidebar_width
        );
    }

    /// The palette's keys: filter, choose, run.
    ///
    /// The filtered list is read before the palette is borrowed mutably, because
    /// filtering borrows the commands and the selection borrows the palette.
    fn palette_key(&mut self, key: KeyEvent) -> Option<Request> {
        let selected = self.palette.as_ref().map(|palette| palette.selected)?;
        let count = self.matches().len();
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.palette = None;
            }
            KeyCode::Up => {
                if let Some(palette) = self.palette.as_mut() {
                    palette.selected = palette.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if selected + 1 < count
                    && let Some(palette) = self.palette.as_mut()
                {
                    palette.selected += 1;
                }
            }
            KeyCode::Backspace => {
                if let Some(palette) = self.palette.as_mut() {
                    palette.query.pop();
                    palette.selected = 0;
                }
            }
            KeyCode::Enter => {
                let chosen = self.matches().get(selected).copied();
                self.palette = None;
                if let Some(command) = chosen {
                    return self.run_command(command);
                }
            }
            KeyCode::Char(character) if !control => {
                if let Some(palette) = self.palette.as_mut() {
                    palette.query.push(character);
                    palette.selected = 0;
                }
            }
            _ => {}
        }
        None
    }

    /// The commands the palette lists, filtered by what has been typed.
    #[must_use]
    pub fn matches(&self) -> Vec<Command> {
        let query = self
            .palette
            .as_ref()
            .map(|palette| palette.query.to_lowercase())
            .unwrap_or_default();
        self.commands
            .iter()
            .copied()
            .filter(|command| command.title.to_lowercase().contains(&query))
            .collect()
    }

    /// Do what a palette row says, or say which task would make it do something.
    fn run_command(&mut self, command: Command) -> Option<Request> {
        let action = match command.action {
            CommandAction::Planned(task) => {
                self.planned(task, command.title);
                return None;
            }
            CommandAction::Run(action) => action,
        };
        match action {
            Run::NewSession => self
                .agents
                .first()
                .map(|agent| Request::New(agent.id.clone())),
            Run::Terminal => self.toggle_terminal(),
            Run::CopyMode => {
                if let Some(pane) = &mut self.terminal {
                    pane.begin_copy();
                    self.status = COPY_HINT.to_owned();
                } else {
                    self.notice("no terminal pane is open");
                }
                None
            }
            Run::Permission => {
                if self.permissions.is_empty() {
                    self.notice("nothing is waiting for an answer");
                } else {
                    self.permission_card = true;
                }
                None
            }
            Run::Find => {
                self.set_focus(Focus::Editor);
                match &mut self.editor {
                    Some(editor) => {
                        editor.search();
                        self.status = editor.message().to_owned();
                    }
                    None => self.notice("no file is open · pick one in the files pane"),
                }
                None
            }
            Run::GoToLine => {
                self.set_focus(Focus::Editor);
                match &mut self.editor {
                    Some(editor) => {
                        editor.go_to_line();
                        self.status = editor.message().to_owned();
                    }
                    None => self.notice("no file is open · pick one in the files pane"),
                }
                None
            }
            Run::FindFile => {
                self.find_file();
                None
            }
            Run::SearchContents => {
                self.open_grep();
                None
            }
            Run::ReviewDiff => {
                self.toggle_review();
                None
            }
            // The two answers are the review's, reached without opening it: the
            // row is for the person who has already read the card.
            Run::AcceptDiff => self.accept_review(),
            Run::RejectDiff => self.reject_review(),
            Run::Refresh => Some(Request::Refresh {
                include_ignored: self.show_ignored,
            }),
            Run::WhichKey => {
                self.which_key = true;
                None
            }
            Run::Palette => {
                self.palette = Some(Palette::default());
                None
            }
            // The servers are the daemon's processes, so the report is a request like
            // the references list is: the status line says what it answered rather
            // than this window guessing from a list it does not own.
            Run::LspStatus => Some(Request::LspStatus),
            Run::Quit => {
                self.should_quit = true;
                Some(Request::Quit)
            }
        }
    }

    /// Say that a command is designed and not built, naming the task.
    fn planned(&mut self, task: &'static str, feature: impl Into<String>) {
        let feature = feature.into();
        self.status = format!("planned · {task} · {feature}");
        self.toast = Some(Toast { task, feature });
    }

    /// Say something that needs no task id.
    fn notice(&mut self, message: &str) {
        self.status = message.to_owned();
    }

    /// Open the source-control pane, or put it away.
    ///
    /// The design pass has its one changed file already, so it opens without a
    /// request; a real client asks the daemon and draws the empty state until the
    /// answer arrives, which is a different claim from a clean tree.
    fn toggle_source(&mut self) -> Option<Request> {
        if self.source.take().is_some() {
            self.commit = None;
            self.notice("source control closed");
            return None;
        }
        self.commit = None;
        let source = if self.demo {
            self.diff
                .as_ref()
                .map_or_else(SourceControl::empty, SourceControl::fixture)
        } else {
            SourceControl::empty()
        };
        self.source = Some(source);
        if self.demo {
            self.status = "design pass · source control · the fixture's change".to_owned();
            return None;
        }
        self.status = "source control · reading the index…".to_owned();
        Some(Request::Git)
    }

    /// The source-control pane's keys: choose a file, scroll its diff, or open the
    /// composer. `r` asks again because staging happens in another window and the
    /// index this pane drew is already out of date.
    fn source_key(&mut self, key: KeyEvent) -> Option<Request> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.source = None;
                self.commit = None;
                self.notice("source control closed");
                None
            }
            KeyCode::Char('r') => Some(Request::Git),
            KeyCode::Char('c') => self.open_commit(false),
            KeyCode::Char('g') => self.open_commit(true),
            KeyCode::Down | KeyCode::Char('j') => {
                self.step_source(1);
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.step_source(-1);
                None
            }
            KeyCode::PageDown => {
                if let Some(source) = &mut self.source {
                    source.scroll = source.scroll.saturating_add(10);
                }
                None
            }
            KeyCode::PageUp => {
                if let Some(source) = &mut self.source {
                    source.scroll = source.scroll.saturating_sub(10);
                }
                None
            }
            _ => None,
        }
    }

    fn step_source(&mut self, delta: isize) {
        let Some(source) = &mut self.source else {
            return;
        };
        let last = source.status.files.len().saturating_sub(1);
        source.selected = if delta < 0 {
            source.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (source.selected + delta as usize).min(last)
        };
        source.scroll = 0;
    }

    /// Open the composer, and optionally ask the session for a first draft.
    ///
    /// A staged file is required before either: the message describes the next
    /// commit, and there is nothing for a model to read while the index is empty.
    fn open_commit(&mut self, generate: bool) -> Option<Request> {
        let staged = self.source.as_ref().map_or(0, SourceControl::staged);
        if staged == 0 {
            self.notice("nothing is staged · git add the files first");
            return None;
        }
        self.commit = Some(CommitComposer {
            generating: generate,
            ..CommitComposer::default()
        });
        if !generate {
            self.status =
                format!("commit · {staged} staged · write the message · ctrl-s reviews it");
            return None;
        }
        self.generate_commit()
    }

    /// Ask the open session to draft a message from what is staged.
    fn generate_commit(&mut self) -> Option<Request> {
        let session = self.open.clone();
        let Some(composer) = &mut self.commit else {
            return None;
        };
        composer.generating = true;
        composer.confirming = false;
        composer.error = None;
        let Some(session_id) = session else {
            let message = "no session is open · open one to generate a message".to_owned();
            composer.generating = false;
            composer.error = Some(message.clone());
            self.status = message;
            return None;
        };
        self.status = "commit · the session is drafting from the staged diff…".to_owned();
        Some(Request::GenerateCommit { session_id })
    }

    /// The composer's keys. Text edits locally; only `Ctrl-S` then `y` writes, and
    /// the commit request carries the message as it reads at that moment.
    fn commit_key(&mut self, key: KeyEvent) -> Option<Request> {
        if key.code == KeyCode::Esc {
            if self.commit.as_ref().is_some_and(|commit| commit.committing) {
                self.notice("the commit is in flight; wait for git");
                return None;
            }
            self.commit = None;
            self.notice("commit closed · nothing written");
            return None;
        }
        if self.commit.as_ref().is_some_and(|commit| commit.committing) {
            return None;
        }
        if self.commit.as_ref().is_some_and(|commit| commit.confirming) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let message = self.commit.as_ref().map(|commit| commit.text.clone())?;
                    if message.trim().is_empty() {
                        if let Some(commit) = &mut self.commit {
                            commit.confirming = false;
                            commit.error = Some("a commit needs a message".to_owned());
                        }
                        self.notice("a commit needs a message");
                        return None;
                    }
                    if let Some(commit) = &mut self.commit {
                        commit.confirming = false;
                        commit.committing = true;
                        commit.error = None;
                    }
                    self.status = "committing the staged index…".to_owned();
                    return Some(Request::Commit { message });
                }
                KeyCode::Char('n') => {
                    if let Some(commit) = &mut self.commit {
                        commit.confirming = false;
                    }
                    self.notice("commit cancelled · nothing written");
                    return None;
                }
                _ => return None,
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('g') => return self.generate_commit(),
                KeyCode::Char('s') => {
                    let text = self.commit.as_ref().map(|commit| commit.text.clone())?;
                    if text.trim().is_empty() {
                        if let Some(commit) = &mut self.commit {
                            commit.error = Some("a commit needs a message".to_owned());
                        }
                        self.notice("write a message, or ctrl-g to draft one");
                        return None;
                    }
                    let staged = self.source.as_ref().map_or(0, SourceControl::staged);
                    if let Some(commit) = &mut self.commit {
                        commit.confirming = true;
                    }
                    self.status = format!("commit {staged} staged? y confirms · n cancels");
                    return None;
                }
                _ => return None,
            }
        }
        let Some(commit) = &mut self.commit else {
            return None;
        };
        match key.code {
            KeyCode::Enter => commit.insert('\n'),
            KeyCode::Backspace => commit.backspace(),
            KeyCode::Delete => commit.delete(),
            KeyCode::Left => commit.left(),
            KeyCode::Right => commit.right(),
            KeyCode::Home => commit.home(),
            KeyCode::End => commit.end(),
            KeyCode::Up => commit.vertical(-1),
            KeyCode::Down => commit.vertical(1),
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                commit.insert(character);
            }
            _ => {}
        }
        None
    }

    /// Open the review of the diff that is up, or close the one that is open.
    fn toggle_review(&mut self) {
        if self.review.take().is_some() {
            self.close_review();
            return;
        }
        self.open_review();
    }

    /// Put the diff on the screen a hunk at a time, every hunk accepted.
    ///
    /// [`false`] when there is nothing to review, which is a state worth saying
    /// out loud: a review with no diff would be an empty card over a question it
    /// did not answer.
    fn open_review(&mut self) -> bool {
        let Some(diff) = self.diff.clone() else {
            self.notice("there is no diff to review");
            return false;
        };
        if diff.hunks.is_empty() {
            self.notice("this diff has no hunks to review");
            return false;
        }
        let count = diff.hunks.len();
        self.review = Some(Review::new(diff));
        self.status = format!("review · {count} hunks · a accepts · x rejects · y answers");
        true
    }

    /// Leave the review without answering anything.
    ///
    /// The permission stays pending: a review that closed the card would be a way
    /// to make an agent's question disappear by looking at it.
    fn close_review(&mut self) {
        self.review = None;
        self.notice("review closed · nothing answered");
    }

    /// The review's keys.
    ///
    /// `a` and `x` decide the hunk under the keyboard and move on; `y` answers the
    /// write from what the hunks say and `n` declines it whatever they say, so the
    /// two decisions a person actually makes are one keystroke each.
    fn review_key(&mut self, key: KeyEvent) -> Option<Request> {
        use KeyCode::{Char, Down, Enter, Esc, Up};
        if key.code == Esc || key.code == Char('q') {
            self.close_review();
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == Char('d') {
            self.close_review();
            return None;
        }

        // The decisions are the review's and the answer is the app's, so the
        // borrow ends before the request is built.
        let mut answer: Option<bool> = None;
        {
            let review = self.review.as_mut()?;
            match key.code {
                Up | Char('k') => review.step(-1),
                Down | Char('j') => review.step(1),
                Char('a') => review.decide(true),
                Char('x') => review.decide(false),
                Char('A') => review.decide_all(true),
                Char('X') => review.decide_all(false),
                Char('s') => review.split = !review.split,
                Char('y') | Enter => answer = Some(true),
                Char('n') => answer = Some(false),
                _ => return None,
            }
        }
        if let Some(accept) = answer {
            return if accept {
                self.accept_review()
            } else {
                self.reject_review()
            };
        }

        // The card stays a picture of the diff; where the keyboard is and what it
        // has decided is the status line's job.
        if let Some(review) = &self.review {
            let total = review.accepted.len();
            self.status = format!(
                "review · hunk {}/{total} · {} accepted · a/x decide · y answers",
                review.cursor + 1,
                review.accepted_hunks()
            );
        }
        None
    }

    /// Answer the write from the hunk decisions.
    ///
    /// With no review open this takes the diff as it stands and accepts all of it,
    /// which is what the palette's "Accept this diff" means — the row is the fast
    /// path for the person who has already read the card.
    fn accept_review(&mut self) -> Option<Request> {
        let review = self
            .review
            .take()
            .or_else(|| self.diff.clone().map(Review::new));
        let Some(review) = review else {
            self.notice("there is no diff to accept");
            return None;
        };
        let path = review.diff.path.clone();
        let total = review.accepted.len();
        let rejected = review.rejected_hunks();
        match rejected.first() {
            Some(first) => {
                let others = rejected.len().saturating_sub(1);
                let more = if others > 0 {
                    format!(" and {others} more")
                } else {
                    String::new()
                };
                let message = format!(
                    "review · hunk {first}{more} rejected · the write was declined · {path}"
                );
                self.resolve_review(PermissionOption::Deny, message)
            }
            None => {
                let message =
                    format!("review · {total} hunks accepted · the write was allowed · {path}");
                self.resolve_review(PermissionOption::AllowOnce, message)
            }
        }
    }

    /// Decline the write, whatever the hunks say.
    fn reject_review(&mut self) -> Option<Request> {
        let path = self
            .review
            .take()
            .map(|review| review.diff.path)
            .or_else(|| self.diff.as_ref().map(|diff| diff.path.clone()));
        let message = match path {
            Some(path) => format!("review · declined · {path} was not written"),
            None => "review · declined".to_owned(),
        };
        self.resolve_review(PermissionOption::Deny, message)
    }

    /// Answer the request that asked for the write.
    ///
    /// Nothing pending is not a failure: a diff can be reviewed when the request
    /// was answered on another client, or before the daemon has polled, and saying
    /// so is better than an answer nobody asked for.
    fn resolve_review(&mut self, option: PermissionOption, message: String) -> Option<Request> {
        self.answer_permission(option, message)
    }

    /// Bring back a permission request put aside with `Esc`.
    ///
    /// The card is dismissible because a question can arrive while a person is
    /// reading something else, and a modal that could not be put down would be an
    /// interruption with no way out. Put down is not answered, though: the queue is
    /// still there, and this is the way back to it.
    fn reopen_permission(&mut self) {
        if self.permissions.is_empty() {
            self.notice("no permission is waiting for an answer");
            return;
        }
        self.permission_card = true;
        self.status = format!("{} permission question(s) waiting", self.permissions.len());
    }

    /// Answer the request the card is showing, and move the card to the next one.
    ///
    /// The answer leaves the list here rather than waiting for the next poll: a
    /// question that was just answered should not be drawn again while the daemon
    /// is being told, and the card belongs to the next one if there is one.
    fn answer_permission(&mut self, option: PermissionOption, message: String) -> Option<Request> {
        let Some(request) = self.permissions.get(self.permission_at).cloned() else {
            self.notice(&format!("{message} · nothing was waiting for an answer"));
            return None;
        };
        self.permissions.remove(self.permission_at);
        if self.permission_at >= self.permissions.len() {
            self.permission_at = self.permissions.len().saturating_sub(1);
        }
        self.permission_card = !self.permissions.is_empty();
        self.status = message;
        Some(Request::Resolve {
            id: request.id,
            option,
        })
    }

    /// Open a terminal for the open session, or leave the one that is open.
    ///
    /// One terminal is the common case and attaches straight away — including a
    /// pane no session owns, which is still the shell the person asked to see.
    /// Several ask which: an agent may open a shell per command, and silently
    /// opening the first would be a pane that hides the rest. None starts a shell,
    /// in the session's own workspace: a person who pressed `Ctrl-T` asked for a
    /// terminal, and "an agent opens one" is a refusal that only makes sense to
    /// the agent.
    fn toggle_terminal(&mut self) -> Option<Request> {
        if self.terminal_picker {
            self.terminal_picker = false;
            self.status = "never mind".to_owned();
            return None;
        }
        if self.terminal.is_some() {
            self.terminal = None;
            self.set_focus(Focus::Prompt);
            self.status = "terminal closed".to_owned();
            return Some(Request::CloseTerminal);
        }
        if self.open.is_none() {
            self.status = "open a session before a terminal".to_owned();
            return None;
        }
        match self.terminals.len() {
            0 => {
                self.status = "starting a shell…".to_owned();
                Some(self.new_terminal_request())
            }
            1 => {
                let id = self.terminals[0].id.clone();
                self.status = "attaching to a terminal…".to_owned();
                Some(Request::OpenTerminal { id })
            }
            count => {
                self.terminal_picker = true;
                self.picker_selected = self.picker_selected.min(count - 1);
                self.status =
                    format!("{count} terminals · Enter opens · n starts one · Esc closes");
                None
            }
        }
    }

    /// A shell in the open session's workspace, at the size the pane has.
    ///
    /// The workspace is the id, not a host name: the daemon knows which
    /// workspaces are remote, and a window that guessed would be a second opinion.
    /// An empty id is the daemon's own workspace, which is what this client's own
    /// sessions are created with.
    fn new_terminal_request(&self) -> Request {
        let workspace = self
            .sessions
            .iter()
            .find(|session| Some(&session.id) == self.open.as_ref())
            .map(|session| session.workspace_id.clone())
            .filter(|id| !id.is_empty());
        let size = self
            .pane_size
            .unwrap_or(TerminalSize { cols: 80, rows: 24 });
        Request::NewTerminal { workspace, size }
    }

    /// The keys the terminal picker answers to.
    ///
    /// `j`/`k` as well as the arrows, because the picker is a list and the TUI's
    /// other lists take both.
    fn picker_key(&mut self, key: KeyEvent) -> Option<Request> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.terminal_picker = false;
                self.status = "never mind".to_owned();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.picker_selected = self.picker_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.picker_selected + 1 < self.terminals.len() {
                    self.picker_selected += 1;
                }
            }
            // A picker whose only rows are panes nobody owns would otherwise be a
            // list that traps the person who wanted a shell of their own.
            KeyCode::Char('n') => {
                self.terminal_picker = false;
                self.status = "starting a shell…".to_owned();
                return Some(self.new_terminal_request());
            }
            KeyCode::Enter => {
                if let Some(summary) = self.terminals.get(self.picker_selected) {
                    let id = summary.id.clone();
                    let command = summary.command.clone();
                    self.terminal_picker = false;
                    self.status = format!("attaching to {command}…");
                    return Some(Request::OpenTerminal { id });
                }
            }
            _ => {}
        }
        None
    }

    /// The keys copy mode answers to: vi's motions, a `v` to select with, `y` to
    /// take the text, and `/` to find in what has already scrolled past.
    ///
    /// Nothing here reaches the program — a program that could be typed into while
    /// a person scrolls it would be a program being typed at by accident.
    fn copy_key(&mut self, key: KeyEvent) -> Option<Request> {
        // A query being typed is text: `/` opened a line for it, and every key
        // belongs to that line until Enter runs it or Escape drops it.
        if self
            .terminal
            .as_ref()
            .is_some_and(|pane| pane.reader.typing.is_some())
        {
            return self.search_typing(key);
        }

        // The key that opened copy mode closes it, and the pane's own F2 would
        // otherwise be a dead key in a mode entered by pressing it.
        if key.code == KeyCode::F(2) {
            if let Some(pane) = self.terminal.as_mut() {
                pane.end_copy();
            }
            return None;
        }

        let page = {
            let pane = self.terminal.as_ref()?;
            i32::from(pane.grid_size().0).saturating_sub(1).max(1)
        };
        let action = CopyKey::parse(key, page);
        match action {
            CopyKey::Find => {
                if let Some(pane) = self.terminal.as_mut() {
                    pane.reader.begin_search();
                }
                self.status = SEARCH_HINT.to_owned();
                return None;
            }
            CopyKey::Older => {
                self.step_hit(true);
                return None;
            }
            CopyKey::Newer => {
                self.step_hit(false);
                return None;
            }
            _ => {}
        }

        let pane = self.terminal.as_mut()?;
        match action {
            CopyKey::Leave => pane.end_copy(),
            CopyKey::Select => pane.reader.view.toggle_anchor(),
            CopyKey::SelectLine => pane.reader.view.toggle_line(),
            CopyKey::Move { rows, cols } => pane.copy_move(rows, cols),
            CopyKey::Top => pane.scroll_to_top(),
            CopyKey::Live => pane.scroll_to_live(),
            CopyKey::Yank => {
                let text = pane.yank();
                let lines = text.lines().count().max(1);
                pane.end_copy();
                if text.is_empty() {
                    self.status = "nothing to copy".to_owned();
                    return None;
                }
                self.status = format!("copied {lines} line(s) to the clipboard");
                self.clipboard = Some(text);
            }
            CopyKey::Find | CopyKey::Older | CopyKey::Newer | CopyKey::Nothing => {}
        }
        None
    }

    /// The keys a query answers to, once `/` has opened a line for it.
    ///
    /// The query is the pane's, not a global search box: leaving copy mode drops
    /// it, and the matches it found stay with the screen they were found on.
    fn search_typing(&mut self, key: KeyEvent) -> Option<Request> {
        let outcome = {
            let pane = self.terminal.as_mut()?;
            pane.reader.type_key(key)
        };
        match outcome {
            QueryKey::Typed | QueryKey::Ignored => {}
            QueryKey::Cancelled => self.status = COPY_HINT.to_owned(),
            QueryKey::Run => {
                let searched = self.terminal.as_mut().map(|pane| {
                    let query = pane.reader.typing.take().unwrap_or_default();
                    if query.is_empty() {
                        return (query, None);
                    }
                    let total = pane.search(&query);
                    (query, Some(total))
                });
                match searched {
                    Some((query, Some(total))) => self.status = find_status(&query, total),
                    Some((_, None)) => self.status = "nothing to find".to_owned(),
                    None => {}
                }
            }
        }
        None
    }

    /// Move to the next match in the terminal's search, and say where it landed.
    fn step_hit(&mut self, older: bool) {
        let Some(pane) = self.terminal.as_mut() else {
            return;
        };
        match pane.step_hit(older) {
            None => self.status = "no search yet · / finds".to_owned(),
            Some((0, 0)) => self.status = "no match in the scrollback".to_owned(),
            Some((at, total)) => {
                self.status = format!("match {at}/{total} · n older · N newer · Esc leaves");
            }
        }
    }

    fn open_selected(&mut self) -> Option<Request> {
        let id = self.selected_session()?.id.clone();
        if let Some(open) = &self.open
            && open == &id
        {
            self.set_focus(Focus::Prompt);
            return None;
        }
        Some(Request::Select(id))
    }
}

/// Whether a query is case-sensitive: a capital in it means the person typed it on
/// purpose, which is `vim`'s smart-case rule.
///
/// A terminal and a transcript are full of lowercase log lines with one `Error:` in
/// them; typing `error` should find all of it, and typing `Error` should find the one
/// that mattered. Nothing else told the search which was meant.
fn case_sensitive(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

/// The sentence a finished search leaves on the status line.
///
/// Singular and plural are spelled out because "1 matches" is the kind of sentence
/// that makes a person distrust the count, and the case rule is named because a
/// search that quietly ignores capitals is a search nobody can predict.
fn find_status(query: &str, total: usize) -> String {
    let case = if case_sensitive(query) {
        "exact case"
    } else {
        "any case"
    };
    match total {
        0 => format!("no match for {query} ({case}) · Esc leaves copy mode"),
        1 => format!("1 match ({case}) · n older · N newer"),
        total => format!("{total} matches ({case}) · n older · N newer"),
    }
}

/// A row count `vt100` will take.
///
/// Zero is not a terminal: `vt100` refuses it with `NonZeroU16`, and a pane that
/// has not been drawn yet reports zero. One row is the smallest screen that can
/// answer, and the next frame resizes it to the truth.
fn screen_rows(rows: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(rows).unwrap_or(std::num::NonZeroU16::MIN)
}

fn screen_cols(cols: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(cols).unwrap_or(std::num::NonZeroU16::MIN)
}

/// The bytes a key press sends to a terminal.
///
/// A terminal is not a text field: `\r` and `\n` are different keys, arrows are
/// escape sequences, and Ctrl-A is a byte rather than a chord. `None` for a key
/// that has no byte, so a pane does not invent one.
fn terminal_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return control_byte(key.code);
    }
    // `Alt` and a letter are two bytes on the wire: the escape a terminal sends,
    // then the letter. Readline's `Alt-F` (forward a word) is that escape, and a
    // pane that dropped it would turn the chord into typing a letter.
    if key.modifiers.contains(KeyModifiers::ALT)
        && let KeyCode::Char(character) = key.code
    {
        let mut bytes = vec![0x1b];
        bytes.extend(character.to_string().into_bytes());
        return Some(bytes);
    }
    Some(match key.code {
        KeyCode::Char(character) => character.to_string().into_bytes(),
        // `\r`: what a terminal sends for Enter. A program that reads a line
        // expects the carriage return, and `\n` is a different byte on the wire.
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    })
}

/// The control byte for a chord, as a terminal sends it.
///
/// Ctrl-A through Ctrl-Z are 0x01–0x1a; the six punctuation keys that have one
/// are the neighbouring codes. Anything else is a chord the terminal does not
/// send, so it is not sent here either.
fn control_byte(code: KeyCode) -> Option<Vec<u8>> {
    let byte = match code {
        KeyCode::Char(character) => match character {
            'a'..='z' => character as u8 - b'a' + 1,
            'A'..='Z' => character as u8 - b'A' + 1,
            '@' => 0x00,
            '[' => 0x1b,
            '\\' => 0x1c,
            ']' => 0x1d,
            '^' => 0x1e,
            '_' => 0x1f,
            '?' => 0x7f,
            _ => return None,
        },
        _ => return None,
    };
    Some(vec![byte])
}

/// The text of a content block.
///
/// The daemon sends ACP's `ContentBlock`, which is an object with a `type` and a
/// `text` field, not a bare string. Reading only the string form is how a
/// transcript renders empty while the wire looks correct.
/// The key the permission card offers for an answer.
///
/// `y` and `n` are the yes and no the card has always used; `a` is the one that
/// lasts, because "allow once" and "allow always" cannot share a letter.
#[must_use]
pub const fn option_key(option: PermissionOption) -> char {
    match option {
        PermissionOption::AllowOnce => 'y',
        PermissionOption::AllowAlways => 'a',
        PermissionOption::Deny => 'n',
    }
}

/// What the card calls an answer.
///
/// `AllowAlways` says "this session" because that is what the wire's variant
/// means: the broker applies it to matching requests for the rest of the session,
/// not to every future one.
#[must_use]
pub const fn option_label(option: PermissionOption) -> &'static str {
    match option {
        PermissionOption::AllowOnce => "allow once",
        PermissionOption::AllowAlways => "allow always · this session",
        PermissionOption::Deny => "deny",
    }
}

/// The answer a key names, when the request offers it.
///
/// The request's own list is the gate: a request with no always option has no `a`,
/// and pressing one is not an answer the daemon could apply.
#[must_use]
pub fn option_for_key(key: char, options: &[PermissionOption]) -> Option<PermissionOption> {
    options
        .iter()
        .copied()
        .find(|option| option_key(*option) == key)
}

fn text_of(content: &serde_json::Value) -> Option<&str> {
    content
        .as_str()
        .or_else(|| content.get("text").and_then(serde_json::Value::as_str))
}

/// What a tool call's payload says its result was, in the agent's own words.
///
/// The adapters differ: ACP's tool call carries a content block, and a local
/// agent may send raw output. The first non-empty one is the answer — a tool
/// call with no result keeps its detail empty rather than inventing one.
fn tool_detail(payload: &serde_json::Value) -> String {
    ["content", "raw_output", "output"]
        .iter()
        .filter_map(|key| text_of(&payload[*key]))
        .find(|text| !text.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_proto::session::SessionStatus;

    /// A real theme, by id, so two themes differ in more than their names.
    fn theme(id: &str) -> Theme {
        let extensions = acpilot_extensions::builtin::load_all().expect("the built-ins load");
        let resolved = acpilot_extensions::ThemeRegistry::new(&extensions)
            .complete(id)
            .expect("the theme resolves");
        Theme::from_resolved(&resolved)
    }

    #[test]
    fn the_daemons_theme_outranks_a_settings_file_this_client_can_read() {
        // Two sources, one screen: the daemon reads the cascade for the machine it
        // serves, which may not be this one. A local file that disagrees is the
        // stale copy.
        let mut app = App::new("daemon");
        app.apply(Update::DaemonTheme(theme("acpilot.paper")));
        assert_eq!(app.theme.name(), "Paper");
        assert_eq!(app.status, "theme · Paper", "and the status line says so");

        app.apply(Update::Theme(theme("acpilot.night-flight")));
        assert_eq!(
            app.theme.name(),
            "Paper",
            "a local save does not override the daemon"
        );
    }

    #[test]
    fn before_the_daemon_speaks_a_local_save_still_recolours() {
        // The file watcher is what a client with no daemon — the design pass — has.
        let mut app = App::new("daemon");
        app.apply(Update::Theme(theme("acpilot.paper")));
        assert_eq!(app.theme.name(), "Paper");
    }

    fn session(id: &str, title: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_owned(),
            workspace_id: "work".to_owned(),
            agent_id: "mock".to_owned(),
            title: title.to_owned(),
            status: SessionStatus::Idle,
            created_at: "2026-10-24T00:00:00Z".to_owned(),
            updated_at: "2026-10-24T00:00:00Z".to_owned(),
        }
    }

    fn event(seq: u64, kind: SessionEventKind, payload: serde_json::Value) -> SessionEvent {
        SessionEvent {
            seq,
            session_id: "s1".to_owned(),
            kind,
            at: "2026-10-24T00:00:00Z".to_owned(),
            payload,
        }
    }

    fn open(app: &mut App) {
        app.apply(Update::Opened {
            id: "s1".to_owned(),
        });
    }

    #[test]
    fn chunks_of_one_message_are_one_paragraph() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.apply_event(event(
            1,
            SessionEventKind::MessageChunk,
            serde_json::json!({ "role": "assistant", "content": "Hello " }),
        ));
        app.apply_event(event(
            2,
            SessionEventKind::MessageChunk,
            serde_json::json!({ "role": "assistant", "content": "world" }),
        ));
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].text, "Hello world");
        assert_eq!(app.entries[0].kind, Kind::Assistant);
    }

    #[test]
    fn a_content_block_is_read_as_its_text() {
        // The shape the daemon actually sends: ACP's content block, not a string.
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.apply_event(event(
            1,
            SessionEventKind::MessageChunk,
            serde_json::json!({
                "role": "assistant",
                "content": { "type": "text", "text": "chunk 1" }
            }),
        ));
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].text, "chunk 1");
    }

    #[test]
    fn a_replayed_event_is_not_printed_twice() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        let chunk = event(
            3,
            SessionEventKind::MessageChunk,
            serde_json::json!({ "role": "assistant", "content": "once" }),
        );
        app.apply_event(chunk.clone());
        app.apply_event(chunk);
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].text, "once");
    }

    #[test]
    fn another_sessions_event_is_ignored() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        let mut other = event(
            1,
            SessionEventKind::MessageChunk,
            serde_json::json!({ "role": "assistant", "content": "not mine" }),
        );
        other.session_id = "s2".to_owned();
        app.apply_event(other);
        assert!(app.entries.is_empty());
    }

    #[test]
    fn a_notice_arrives_with_its_message() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.apply_event(event(
            1,
            SessionEventKind::Notice,
            serde_json::json!({ "title": "Permission granted", "message": "Write: allowed by user" }),
        ));
        assert_eq!(app.entries[0].kind, Kind::Notice);
        assert_eq!(
            app.entries[0].text,
            "Permission granted\nWrite: allowed by user"
        );
    }

    #[test]
    fn a_selection_survives_a_new_session_at_the_top() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::Sessions(vec![
            session("s1", "one"),
            session("s2", "two"),
        ]));
        app.selected = 1;
        app.apply(Update::Sessions(vec![
            session("s3", "three"),
            session("s1", "one"),
            session("s2", "two"),
        ]));
        assert_eq!(app.selected_session().map(|s| s.id.as_str()), Some("s2"));
    }

    #[test]
    fn a_new_permission_raises_the_card_only_once() {
        let mut app = App::new("http://127.0.0.1:7777");
        let request = PermissionRequest {
            id: "p1".to_owned(),
            session_id: "s1".to_owned(),
            kind: acpilot_proto::permission::PermissionKind::FileWrite,
            title: "Write src/main.rs".to_owned(),
            detail: "+fn main() {}".to_owned(),
            options: vec![PermissionOption::AllowOnce, PermissionOption::Deny],
            created_at: "2026-10-24T00:00:00Z".to_owned(),
        };
        app.apply(Update::Permissions(vec![request.clone()]));
        assert!(app.permission_card);
        app.permission_card = false;
        app.apply(Update::Permissions(vec![request]));
        assert!(
            !app.permission_card,
            "a refresh must not reopen a dismissed card"
        );
    }

    #[test]
    fn f4_turns_the_bar_into_its_sources_and_back() {
        let mut app = App::new("http://127.0.0.1:7777");
        let f4 = KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE);
        assert!(!app.stats_sources);
        assert_eq!(app.handle_key(f4), None);
        assert!(app.stats_sources, "the numbers become their provenance");
        assert_eq!(app.handle_key(f4), None);
        assert!(!app.stats_sources, "and the same key puts them back");
    }

    #[test]
    fn the_daemons_stats_are_measured_when_they_arrive() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::Stats(vec![
            acpilot_proto::stats::IntegrationStats {
                id: "lmstudio".to_owned(),
                kind: acpilot_proto::stats::IntegrationKind::Provider,
                label: "lmstudio".to_owned(),
                context: Some(acpilot_proto::stats::ContextStats {
                    used_tokens: 16_384,
                    window_tokens: Some(32_768),
                    source: acpilot_proto::stats::StatSource::Server,
                }),
                credits: None,
                updated_at: "2026-10-30T12:00:00Z".to_owned(),
            },
        ]));
        assert_eq!(app.integrations.len(), 1);
        let track = app.integrations[0]
            .context
            .as_ref()
            .expect("a context track");
        assert_eq!(track.fill, Some(0.5), "the ratio is computed once, here");
        assert_eq!(track.text, "16.4k / 32.8k");
    }

    #[test]
    fn escape_puts_the_card_away_and_the_menu_brings_it_back() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::Permissions(vec![PermissionRequest {
            id: "p1".to_owned(),
            session_id: "s1".to_owned(),
            kind: acpilot_proto::permission::PermissionKind::Terminal,
            title: "Run make".to_owned(),
            detail: "make check".to_owned(),
            options: vec![PermissionOption::AllowOnce, PermissionOption::Deny],
            created_at: "2026-10-24T00:00:00Z".to_owned(),
        }]));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.permission_card, "the card is down");
        assert_eq!(app.permissions.len(), 1, "and the question is not answered");
        assert!(app.status.contains("Answer Permission"), "{}", app.status);

        app.run_menu(MenuAction::AnswerPermission);
        assert!(app.permission_card, "the way back");
    }

    #[test]
    fn the_card_offers_only_the_answers_the_request_sent() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::Permissions(vec![PermissionRequest {
            id: "p1".to_owned(),
            session_id: "s1".to_owned(),
            kind: acpilot_proto::permission::PermissionKind::FileWrite,
            title: "Write a file".to_owned(),
            detail: String::new(),
            options: vec![PermissionOption::AllowOnce, PermissionOption::Deny],
            created_at: "2026-10-24T00:00:00Z".to_owned(),
        }]));
        app.focus = Focus::Prompt;

        // The request has no always option, so `a` is not an answer either.
        assert_eq!(app.handle_key(key(KeyCode::Char('a'))), None);
        assert_eq!(app.permissions.len(), 1, "nothing was answered");
        assert_eq!(
            app.handle_key(key(KeyCode::Char('n'))),
            Some(Request::Resolve {
                id: "p1".to_owned(),
                option: PermissionOption::Deny,
            })
        );
    }

    #[test]
    fn tab_walks_the_questions_and_answers_the_one_it_shows() {
        let mut app = App::new("http://127.0.0.1:7777");
        let request = |id: &str| PermissionRequest {
            id: id.to_owned(),
            session_id: "s1".to_owned(),
            kind: acpilot_proto::permission::PermissionKind::FileWrite,
            title: format!("write {id}"),
            detail: String::new(),
            options: vec![PermissionOption::AllowOnce, PermissionOption::Deny],
            created_at: "2026-10-24T00:00:00Z".to_owned(),
        };
        app.apply(Update::Permissions(vec![request("p1"), request("p2")]));

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.permission_at, 1, "the second question is on show");
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Request::Resolve {
                id: "p2".to_owned(),
                option: PermissionOption::AllowOnce,
            }),
            "the answer is about the request the card is showing"
        );
        assert_eq!(app.permissions.len(), 1);
        assert!(app.permission_card, "one question is still waiting");
        assert_eq!(app.permission_at, 0);
    }

    #[test]
    fn a_streamed_chunk_does_not_take_the_question_away() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.apply(Update::Permissions(vec![PermissionRequest {
            id: "p1".to_owned(),
            session_id: "s1".to_owned(),
            kind: acpilot_proto::permission::PermissionKind::Terminal,
            title: "Run make".to_owned(),
            detail: "make check".to_owned(),
            options: vec![PermissionOption::AllowOnce, PermissionOption::Deny],
            created_at: "2026-10-24T00:00:00Z".to_owned(),
        }]));
        app.apply_event(event(
            1,
            SessionEventKind::MessageChunk,
            serde_json::json!({ "role": "assistant", "content": "still thinking" }),
        ));
        assert!(app.permission_card, "a turn in flight does not dismiss it");
        assert_eq!(app.permissions.len(), 1);
        assert_eq!(app.entries[0].text, "still thinking");
    }

    #[test]
    fn the_permission_card_answers_with_the_request_it_shows() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::Permissions(vec![PermissionRequest {
            id: "p7".to_owned(),
            session_id: "s1".to_owned(),
            kind: acpilot_proto::permission::PermissionKind::FileWrite,
            title: "Write".to_owned(),
            detail: String::new(),
            options: vec![PermissionOption::AllowOnce],
            created_at: "2026-10-24T00:00:00Z".to_owned(),
        }]));
        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(
            app.handle_key(key),
            Some(Request::Resolve {
                id: "p7".to_owned(),
                option: PermissionOption::AllowOnce,
            })
        );
    }

    #[test]
    fn typing_while_the_card_is_up_does_not_reach_the_prompt() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.focus = Focus::Prompt;
        app.apply(Update::Permissions(vec![PermissionRequest {
            id: "p1".to_owned(),
            session_id: "s1".to_owned(),
            kind: acpilot_proto::permission::PermissionKind::FileRead,
            title: "Read".to_owned(),
            detail: String::new(),
            options: vec![PermissionOption::AllowOnce],
            created_at: "2026-10-24T00:00:00Z".to_owned(),
        }]));
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(app.handle_key(key), None);
        assert!(app.input.is_empty());
    }

    #[test]
    fn enter_in_the_prompt_sends_it_and_clears_the_box() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.input = "run the tests".to_owned();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            app.handle_key(key),
            Some(Request::Prompt("run the tests".to_owned()))
        );
        assert!(app.input.is_empty());
        assert_eq!(app.entries.last().map(|entry| entry.kind), Some(Kind::User));
    }

    #[test]
    fn moving_the_selection_opens_that_session() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::Sessions(vec![
            session("s1", "one"),
            session("s2", "two"),
        ]));
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.handle_key(down), Some(Request::Select("s2".to_owned())));
    }

    #[test]
    fn the_palette_answers_with_the_task_a_planned_command_is_waiting_on() {
        let mut app = App::new("http://127.0.0.1:7777");
        let palette = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(palette), None);
        assert!(app.palette.is_some(), "Ctrl-P opens it");

        for character in "rename".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.matches().len(), 1, "the query filters the list");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.palette.is_none(), "running a command closes it");
        let toast = app.toast.as_ref().expect("a planned command says so");
        assert_eq!(toast.task, "P7-5");
        assert!(app.status.contains("planned"), "{}", app.status);
    }

    #[test]
    fn a_query_that_matches_nothing_runs_nothing() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        for character in "zzzz".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert!(app.matches().is_empty());
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(app.toast.is_none());
        assert!(app.palette.is_none());
    }

    #[test]
    fn a_question_mark_in_the_composer_is_typed_not_a_key_list() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.focus = Focus::Prompt;
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.input, "?");
        assert!(!app.which_key, "the composer is a text box, not a view");
    }

    #[test]
    fn the_next_key_dismisses_a_notice() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        for character in "model".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.toast.is_some());
        app.handle_key(key(KeyCode::Esc));
        assert!(
            app.toast.is_none(),
            "a notice is for the key that raised it"
        );
    }

    #[test]
    fn the_tree_moves_toggles_a_directory_and_asks_the_daemon_for_a_file() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.files = acpilot_demo::tree::files();
        app.focus = Focus::Files;

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.file_selected, 1);

        // The second row is `crates`, a directory and open: Enter closes it, and
        // closing is local — the rows nobody is looking at cost nothing to forget.
        assert!(app.files[1].expanded);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(!app.files[1].expanded);
        assert!(
            app.files
                .iter()
                .all(|node| !node.path.starts_with("crates/")),
            "its children went with it"
        );

        // Opening it again is the daemon's question: the children are not in the
        // client, because the client is not where the files are.
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Request::ListDir {
                path: "crates".to_owned(),
                include_ignored: false,
            })
        );

        // A file is a read, and the editor is where the answer lands.
        app.file_selected = 0;
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Request::OpenFile("Cargo.toml".to_owned()))
        );
        assert_eq!(app.status, "opening Cargo.toml…");
    }

    #[test]
    fn the_editor_moves_its_caret_and_a_fixture_has_no_file_to_save() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.files = acpilot_demo::tree::files();
        // The gutter's markers are git's, so a test buffer has the marks a patch
        // gives it: nothing here, which is a file that did not change.
        let seed = acpilot_demo::editor::buffer();
        app.editor = Some(EditorPane::seeded(
            &seed,
            acpilot_git::line_changes(&acpilot_demo::editor::diff().patch(), &seed.path),
            &mut app.grammars,
        ));
        app.focus = Focus::Editor;

        let start = app.editor.as_ref().expect("a buffer").position();
        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(
            app.editor.as_ref().expect("a buffer").position(),
            (start.0, start.1 + 1)
        );
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.editor.as_ref().expect("a buffer").position(),
            (start.0 + 1, start.1 + 1)
        );

        // The seed is the design pass's own text rather than a file's, so an edited
        // buffer's save says which world it came from instead of writing a version it
        // does not have.
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.editor.as_ref().expect("a buffer").dirty());
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(save), None);
        assert!(
            app.status.contains("did not come from a file"),
            "{}",
            app.status
        );
    }

    #[test]
    fn a_saved_buffer_goes_through_the_daemon_with_its_token() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn main() {}\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        app.set_focus(Focus::Editor);

        // A buffer nobody touched has nothing to write.
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(save), None);
        assert!(app.status.contains("matches the file"), "{}", app.status);

        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Esc));
        let request = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let Some(Request::SaveFile { path, text, etag }) = request else {
            panic!("a save is a request: {request:?}");
        };
        assert_eq!(path, "src/main.rs");
        assert_eq!(etag, "\"v1\"", "the version the read returned");
        assert!(text.starts_with('x'), "the whole buffer, as edited: {text}");
        assert!(app.editor.as_ref().expect("a pane").dirty());

        // The answer is the new baseline: the next save has nothing to send.
        app.apply(Update::Saved {
            path: "src/main.rs".to_owned(),
            etag: "\"v2\"".to_owned(),
        });
        let pane = app.editor.as_ref().expect("a pane");
        assert!(!pane.dirty(), "the buffer and the file agree now");
        assert_eq!(pane.etag(), Some("\"v2\""));
        assert_eq!(app.status, "saved src/main.rs");
    }

    #[test]
    fn a_find_box_takes_every_key_and_the_status_line_says_what_it_found() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.set_focus(Focus::Editor);
        app.handle_key(key(KeyCode::Char('/')));
        assert!(
            app.editor.as_ref().expect("a buffer").is_prompting(),
            "/ opens the box"
        );

        // `?` is the key list everywhere else, and a character in a find box.
        app.handle_key(key(KeyCode::Char('?')));
        assert!(!app.which_key, "the key list must not open over the box");
        app.which_key = false;
        app.handle_key(key(KeyCode::Char('p')));
        let query = app
            .editor
            .as_ref()
            .and_then(|editor| editor.prompt())
            .map(|prompt| prompt.text.clone());
        assert_eq!(query.as_deref(), Some("?p"));

        app.handle_key(key(KeyCode::Enter));
        assert!(!app.editor.as_ref().expect("a buffer").is_prompting());
        assert_eq!(app.status, "no matches", "{:?}", app.status);
    }

    #[test]
    fn go_to_line_moves_the_caret_and_scrolls_it_into_view() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.set_focus(Focus::Editor);
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        for character in "18:5".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        let editor = app.editor.as_ref().expect("a buffer");
        assert_eq!(editor.position(), (18, 5));
        assert_eq!(app.status, "line 18");
    }

    #[test]
    fn the_review_opens_over_a_pending_question_and_leaves_it_pending() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        let review = app.review.as_ref().expect("the review is up");
        assert_eq!(review.accepted, vec![true, true], "nothing objected to yet");
        assert_eq!(review.cursor, 0);
        assert_eq!(app.permissions.len(), 1, "the question is still waiting");

        // Escape leaves the review and answers nothing: a question that vanished
        // because it had been looked at would be a way to lose an agent's request.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.review.is_none(), "esc closes the review");
        assert_eq!(app.permissions.len(), 1, "and leaves the question open");
        assert!(
            !app.permission_card,
            "reading a diff does not put the card up either"
        );
    }

    #[test]
    fn a_rejected_hunk_declines_the_write_and_names_itself() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.permission_card = true;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            app.review.as_ref().expect("a review").accepted,
            vec![false, true],
            "x decides the hunk it is on, and moves on"
        );
        assert_eq!(app.review.as_ref().expect("a review").cursor, 1);

        let id = app.permissions[0].id.clone();
        let request = app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            request,
            Some(Request::Resolve {
                id,
                option: PermissionOption::Deny,
            }),
            "a file written in part is a file nobody asked for"
        );
        assert!(app.review.is_none(), "answering closes the review");
        assert!(!app.permission_card, "and the question is answered");
        assert!(
            app.status.contains("hunk 1 rejected"),
            "the status names the hunk: {}",
            app.status
        );
    }

    #[test]
    fn accepting_every_hunk_allows_the_write_once() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        // Reject one, go back, and change the answer: a review whose decision
        // cannot be undone would be a review that punishes reading it twice.
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('a')));
        assert!(
            app.review
                .as_ref()
                .expect("a review")
                .rejected_hunks()
                .is_empty(),
            "a is the decision the keyboard is on, and it undoes the x"
        );

        let id = app.permissions[0].id.clone();
        let request = app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            request,
            Some(Request::Resolve {
                id,
                option: PermissionOption::AllowOnce,
            })
        );
        assert!(app.status.contains("2 hunks accepted"), "{}", app.status);
    }

    #[test]
    fn a_review_with_nothing_to_answer_says_so_instead_of_asking() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.permissions.clear();
        app.permission_card = false;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('A')));
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), None);
        assert!(app.review.is_none());
        assert!(
            app.status.contains("nothing was waiting"),
            "an answer nobody asked for is not sent: {}",
            app.status
        );
    }

    #[test]
    fn a_split_review_is_the_same_decisions_drawn_differently() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('s')));
        let review = app.review.as_ref().expect("a review");
        assert!(review.split, "s is the two-column view");
        assert_eq!(
            review.accepted,
            vec![true, true],
            "how a hunk is drawn is not what was decided about it"
        );
    }

    #[test]
    fn the_palette_answers_the_write_without_opening_the_review() {
        let mut app = App::demo(&acpilot_demo::demo());
        let command = *app
            .commands
            .iter()
            .find(|command| command.title == "Accept this diff")
            .expect("the palette has the row");
        assert!(
            command.planned().is_none(),
            "P6-9 is built, so the row must not still say it is not"
        );
        let id = app.permissions[0].id.clone();
        assert_eq!(
            app.run_command(command),
            Some(Request::Resolve {
                id,
                option: PermissionOption::AllowOnce,
            }),
            "the fast path for a person who already read the card"
        );
        assert!(app.review.is_none(), "and it need not open the review");
    }

    fn entry(path: &str, dir: bool) -> acpilot_proto::file::FileEntry {
        acpilot_proto::file::FileEntry {
            path: path.to_owned(),
            is_dir: dir,
            size: 0,
            modified: None,
            ignored: false,
            status: None,
        }
    }

    fn integration(
        id: &str,
        context: Option<(u32, acpilot_proto::stats::StatSource)>,
    ) -> acpilot_proto::stats::IntegrationStats {
        acpilot_proto::stats::IntegrationStats {
            id: id.to_owned(),
            kind: acpilot_proto::stats::IntegrationKind::Agent,
            label: id.to_owned(),
            context: context.map(|(used_tokens, source)| acpilot_proto::stats::ContextStats {
                used_tokens,
                window_tokens: None,
                source,
            }),
            credits: None,
            updated_at: "2026-10-30T12:00:00Z".to_owned(),
        }
    }

    #[test]
    fn the_content_search_asks_the_daemon_and_opens_the_line_it_chose() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert!(app.grep.is_some(), "the chord opens the search");

        // Nothing is assumed before the daemon answers: the list is empty while the
        // query is only what has been typed.
        for character in "seed".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            app.grep.as_ref().map(|view| view.query.as_str()),
            Some("seed")
        );
        assert_eq!(app.grep.as_ref().map(|view| view.matches.len()), Some(0));

        app.apply(Update::Matches {
            query: "seed".to_owned(),
            matches: vec![acpilot_proto::file::GrepMatch {
                path: "src/app.rs".to_owned(),
                line: 9,
                text: "seeded".to_owned(),
            }],
        });
        assert_eq!(app.grep.as_ref().map(|view| view.matches.len()), Some(1));

        // The row is a jump: the file it names, at the line it named.
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Request::OpenFile("src/app.rs".to_owned()))
        );
        assert!(app.grep.is_none(), "a jump closes the list");

        app.apply(Update::File {
            path: "src/app.rs".to_owned(),
            text: "one\ntwo\nthree".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        let editor = app.editor.as_ref().expect("a buffer");
        assert_eq!(
            editor.caret().line,
            2,
            "line 9 of a 3-line file lands on the last line"
        );
    }

    #[test]
    fn the_hidden_toggle_asks_for_the_root_and_every_open_directory() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.focus = Focus::Files;
        app.apply(Update::Listing {
            path: String::new(),
            entries: vec![entry("src", true), entry("docs", true)],
        });
        // `docs` sorts before `src`, so the second row is the one to open.
        app.file_selected = 1;
        app.handle_key(key(KeyCode::Enter));
        app.apply(Update::Listing {
            path: "src".to_owned(),
            entries: vec![entry("src/main.rs", false)],
        });
        assert!(app.files[1].expanded);

        assert_eq!(
            app.handle_key(key(KeyCode::Char('H'))),
            None,
            "the queue answers, not the return value"
        );
        assert!(app.show_ignored);
        assert_eq!(
            app.pending,
            vec![
                Request::ListDir {
                    path: String::new(),
                    include_ignored: true
                },
                Request::ListDir {
                    path: "src".to_owned(),
                    include_ignored: true
                },
            ],
            "the root first, then what was open, so each child lands under its parent"
        );
        assert!(app.status.contains("greyed"), "{}", app.status);
    }

    #[test]
    fn an_ignored_row_is_kept_and_marked() {
        let mut app = App::new("http://127.0.0.1:7777");
        let mut target = entry("target", true);
        target.ignored = true;
        app.apply(Update::Listing {
            path: String::new(),
            entries: vec![target, entry("src", true)],
        });
        // Directories first, then by name: `src` is the row above `target`.
        assert_eq!(app.files[1].path, "target");
        assert!(app.files[1].ignored, "the pane draws it as hidden");
        assert!(!app.files[0].ignored);
    }

    #[test]
    fn slash_searches_the_workspace_and_esc_puts_the_tree_back() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.focus = Focus::Files;
        app.apply(Update::Listing {
            path: String::new(),
            entries: vec![entry("src", true)],
        });
        assert_eq!(app.handle_key(key(KeyCode::Char('/'))), None);
        assert_eq!(app.search.as_deref(), Some(""));

        // A letter is a letter here: no `j` motion, no `H` toggle.
        assert_eq!(
            app.handle_key(key(KeyCode::Char('m'))),
            Some(Request::SearchFiles("m".to_owned()))
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            Some(Request::SearchFiles("ma".to_owned()))
        );
        app.apply(Update::Found {
            query: "ma".to_owned(),
            entries: vec![entry("src/main.rs", false)],
        });
        assert_eq!(app.visible_files().len(), 1);

        // An answer to the query before this one is dropped.
        app.apply(Update::Found {
            query: "m".to_owned(),
            entries: vec![entry("src/lib.rs", false)],
        });
        assert_eq!(app.visible_files()[0].path, "src/main.rs");

        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Request::OpenFile("src/main.rs".to_owned()))
        );

        app.handle_key(key(KeyCode::Esc));
        assert!(app.search.is_none());
        assert_eq!(app.files.len(), 1, "the tree is what the toggle left");
        assert!(
            !app.pending.is_empty(),
            "and it is re-listed, not remembered"
        );
    }

    #[test]
    fn the_status_line_adds_up_what_the_sessions_hold() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::Stats(vec![
            integration(
                "mock",
                Some((1_200, acpilot_proto::stats::StatSource::Estimated)),
            ),
            integration(
                "claude",
                Some((800, acpilot_proto::stats::StatSource::Server)),
            ),
            integration("quiet", None),
        ]));
        assert_eq!(app.used_tokens, Some(2_000));
        assert!(app.tokens_estimated, "one of the two is an estimate");

        // Nobody has had a turn: the line says nothing rather than "0".
        app.apply(Update::Stats(vec![integration("quiet", None)]));
        assert_eq!(app.used_tokens, None);
    }

    #[test]
    fn a_listing_becomes_a_tree_with_directories_first() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::Listing {
            path: String::new(),
            entries: vec![
                entry("src/main.rs", false),
                entry("docs", true),
                entry("Cargo.toml", false),
            ],
        });
        let rows: Vec<(&str, u8, bool)> = app
            .files
            .iter()
            .map(|node| (node.path.as_str(), node.depth, node.dir))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("docs", 0, true),
                ("Cargo.toml", 0, false),
                ("src/main.rs", 0, false)
            ],
            "a directory sorts before the files beside it"
        );
        assert_eq!(app.files[0].label(), "docs");
        assert_eq!(app.status, "workspace · 3 entries");
    }

    #[test]
    fn the_outline_lists_the_open_files_definitions_and_goes_to_one() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn first() {}\n\nstruct Second {\n    field: u8,\n}\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        app.run_menu(MenuAction::ToggleOutline);
        assert!(app.show_outline);
        let names: Vec<&str> = app
            .outline
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert_eq!(names, vec!["first", "Second"], "in source order");

        app.set_focus(Focus::Outline);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.outline_selected, 1);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Editor, "the keys go back to the buffer");
        assert_eq!(
            app.editor.as_ref().expect("a pane").position().0,
            3,
            "the struct's own line, counted from one"
        );
    }

    #[test]
    fn a_panel_moves_to_the_side_the_menu_names() {
        let mut app = App::new("http://127.0.0.1:7777");
        // The outline is in the arrangement from the first launch and drawn only while
        // it is shown: where a panel belongs and whether it is up are two answers.
        assert_eq!(
            app.panels.stack(Side::Left),
            vec![
                Panel::FileManager,
                Panel::Outline,
                Panel::References,
                Panel::ProjectMap
            ]
        );
        assert_eq!(app.panels.stack(Side::Right), vec![Panel::Assistant]);
        assert!(!app.show_outline);
        assert!(!app.show_references, "a panel's side is not its being up");

        assert_eq!(
            app.run_menu(MenuAction::MovePanel(Panel::Assistant, Side::Left)),
            None
        );
        assert_eq!(
            app.panels.stack(Side::Left),
            vec![
                Panel::FileManager,
                Panel::Outline,
                Panel::References,
                Panel::ProjectMap,
                Panel::Assistant
            ],
            "the workspace above the agent"
        );
        assert!(app.panels.stack(Side::Right).is_empty());
        assert_eq!(app.status, "Assistant · left");

        // The editor is not a panel and cannot be moved: the middle is the window's
        // argument about what it is for.
        // The assistant stayed where the earlier row put it, so the right column is the
        // tree alone: a row moves one panel and leaves the rest where they were.
        app.run_menu(MenuAction::MovePanel(Panel::FileManager, Side::Right));
        assert_eq!(app.panels.stack(Side::Right), vec![Panel::FileManager]);
        assert_eq!(
            app.panels.stack(Side::Left),
            vec![
                Panel::Outline,
                Panel::References,
                Panel::ProjectMap,
                Panel::Assistant
            ]
        );
    }

    #[test]
    fn the_outline_follows_the_buffer_as_it_is_edited() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn first() {}\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        app.run_menu(MenuAction::ToggleOutline);
        assert_eq!(app.outline.len(), 1);

        // Type a second definition. The list is not re-parsed per character — `tick`
        // is the frame's clock — so before the next frame it is still the old list.
        app.set_focus(Focus::Editor);
        app.handle_key(key(KeyCode::Char('o')));
        for character in "fn second() {}".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.tick();
        let names: Vec<&str> = app
            .outline
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["first", "second"],
            "the new definition is listed"
        );

        // Undo takes the line back out, and the frame after it says so.
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('u')));
        app.tick();
        assert_eq!(app.outline.len(), 1, "the list follows the text back");
    }

    #[test]
    fn the_gutter_asks_about_the_buffer_that_stopped_changing() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn main() {}\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        // The open is a version, so it is asked about at once.
        let opened = app.take_pending();
        assert!(
            matches!(opened.last(), Some(Request::Diagnostics { .. })),
            "the open is asked about: {opened:?}"
        );
        app.set_focus(Focus::Editor);

        // Typing is not. The frame that sees the edit starts the pause and asks
        // nothing, and a frame inside the pause asks nothing either.
        let start = std::time::Instant::now();
        app.handle_key(key(KeyCode::Char('o')));
        app.handle_key(key(KeyCode::Char('x')));
        app.tick_at(start);
        app.tick_at(start + std::time::Duration::from_millis(200));
        assert_eq!(app.take_pending(), Vec::new(), "still being written");

        // Another key restarts the pause, so the question is about the text the person
        // stopped on rather than the one they had typed a moment earlier.
        app.handle_key(key(KeyCode::Char('y')));
        app.tick_at(start + std::time::Duration::from_millis(300));
        app.tick_at(start + std::time::Duration::from_millis(600));
        assert_eq!(app.take_pending(), Vec::new(), "the pause restarted");
        app.tick_at(start + std::time::Duration::from_millis(900));
        let asked = app.take_pending();
        let Some(Request::Diagnostics { text, .. }) = asked.first() else {
            panic!("a quiet buffer is asked about: {asked:?}");
        };
        assert_eq!(asked.len(), 1, "one question for one pause");
        // `o` opened a line under the function, and the two keys are in it: the ask is
        // about the buffer as it reads now, not as it read when the pause began.
        assert!(text.ends_with("xy\n"), "the text as edited: {text:?}");

        // And nothing further until the text moves again.
        app.tick_at(start + std::time::Duration::from_millis(5_000));
        assert_eq!(app.take_pending(), Vec::new());
    }

    #[test]
    fn a_question_that_never_answers_stops_saying_it_is_checking() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn main() {}\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        assert!(app.diagnostics_pending, "the open asks");

        // An answer ends the question.
        app.apply(Update::Diagnostics {
            path: "src/main.rs".to_owned(),
            problems: acpilot_proto::file::FileDiagnostics {
                server: Some("rust-analyzer".to_owned()),
                encoding: None,
                items: Vec::new(),
            },
        });
        assert!(!app.diagnostics_pending, "an answer is the question ending");

        // A pause in typing asks again. A question the daemon never gets an answer to
        // still ends: `checking` for ever would be a claim about work nobody is doing.
        app.set_focus(Focus::Editor);
        app.handle_key(key(KeyCode::Char('o')));
        let start = std::time::Instant::now();
        app.tick_at(start);
        app.tick_at(start + std::time::Duration::from_millis(600));
        assert!(app.diagnostics_pending, "the pause asked");
        app.apply(Update::DiagnosticsUnanswered {
            path: "src/main.rs".to_owned(),
            message: "daemon: no route to host".to_owned(),
        });
        assert!(!app.diagnostics_pending);
        assert_eq!(app.status, "daemon: no route to host");
        assert!(
            app.diagnostics.is_some(),
            "the older answer stays: a failure is not evidence the file is clean"
        );
    }

    #[test]
    fn a_problem_the_server_finds_later_lands_in_the_gutter_without_asking() {
        fn notice(path: &str) -> acpilot_proto::file::DiagnosticsNotice {
            acpilot_proto::file::DiagnosticsNotice {
                path: path.to_owned(),
                version: Some(4),
                problems: acpilot_proto::file::FileDiagnostics {
                    server: Some("rust-analyzer".to_owned()),
                    encoding: None,
                    items: vec![acpilot_proto::file::Diagnostic {
                        line: 3,
                        column: 5,
                        end_line: 3,
                        end_column: 9,
                        severity: acpilot_proto::file::DiagnosticSeverity::Error,
                        message: "cannot find value `nope`".to_owned(),
                        source: Some("rust-analyzer".to_owned()),
                        code: Some("E0425".to_owned()),
                    }],
                },
            }
        }

        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn main() {}\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        app.apply(Update::Diagnostics {
            path: "src/main.rs".to_owned(),
            problems: acpilot_proto::file::FileDiagnostics {
                server: Some("rust-analyzer".to_owned()),
                encoding: None,
                items: Vec::new(),
            },
        });
        assert!(
            app.editor
                .as_ref()
                .expect("an editor")
                .problems()
                .is_empty(),
            "the server said the file was clean"
        );

        // A publish nobody asked for, arriving while the pane is still open: this is
        // the background index finishing, and its problems are the current ones.
        app.apply(Update::DiagnosticsPushed(notice("src/main.rs")));
        assert_eq!(
            app.editor.as_ref().expect("an editor").problem(2),
            Some('E'),
            "line 3 is red, from a frame the pane never asked for"
        );
        assert_eq!(app.diagnostics_path, "src/main.rs");
        assert!(
            !app.diagnostics_pending,
            "a push is not a question, so it neither asks nor unasks one"
        );

        // A publish about a file that is not on screen changes nothing here.
        app.apply(Update::DiagnosticsPushed(notice("src/other.rs")));
        assert_eq!(
            app.editor.as_ref().expect("an editor").problems().len(),
            1,
            "the open buffer keeps the problems that are its own"
        );
        assert_eq!(app.diagnostics_path, "src/main.rs");
    }

    #[test]
    fn the_references_panel_asks_for_the_word_under_the_caret_and_opens_a_use() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn first() {}\nfn second() { first(); }\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        // `fn f…`: three columns right of the start of the line is the name itself.
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Char('l')));
        }
        let mut chord = key(KeyCode::F(12));
        chord.modifiers = KeyModifiers::SHIFT;
        let asked = app.handle_key(chord).expect("the chord asks the daemon");
        match asked {
            Request::References {
                name,
                path,
                offset,
                text,
            } => {
                assert_eq!(name, "first", "the pane names the word");
                assert_eq!(path, "src/main.rs", "and the file the caret is in");
                assert_eq!(offset, 3, "the caret's offset, which a server resolves");
                assert!(
                    text.contains("fn second() { first(); }"),
                    "and the buffer as it reads, which is what an edit changes"
                );
            }
            other => panic!("the chord asked {other:?}"),
        }
        assert!(app.show_references, "the panel is up for the answer");

        app.apply(Update::References {
            name: "first".to_owned(),
            hits: vec![acpilot_proto::file::Reference {
                path: "src/main.rs".to_owned(),
                line: 2,
                column: 15,
                text: "fn second() { first(); }".to_owned(),
            }],
        });
        assert_eq!(app.status, "1 use of first");

        app.set_focus(Focus::References);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Request::OpenFile("src/main.rs".to_owned())),
            "a row is a place"
        );
        assert_eq!(app.pending_line, Some(2), "the line travels with the read");
    }

    #[test]
    fn the_references_panel_follows_the_caret_from_word_to_word() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn first() {}\nfn second() { first(); }\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        app.set_focus(Focus::Editor);
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Char('l')));
        }
        let mut chord = key(KeyCode::F(12));
        chord.modifiers = KeyModifiers::SHIFT;
        let asked = app.handle_key(chord).expect("the chord asks");
        assert!(
            matches!(asked, Request::References { ref name, .. } if name == "first"),
            "{asked:?}"
        );
        app.apply(Update::References {
            name: "first".to_owned(),
            hits: vec![acpilot_proto::file::Reference {
                path: "src/main.rs".to_owned(),
                line: 2,
                column: 15,
                text: "fn second() { first(); }".to_owned(),
            }],
        });
        assert_eq!(app.status, "1 use of first");

        // Onto the `second` on the line below. The first frame that sees it is a
        // keystroke in flight, not a word the person has stopped on.
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Char('l')));
        }
        app.handle_key(key(KeyCode::Char('j')));
        app.take_pending();
        app.tick();
        assert_eq!(
            app.take_pending(),
            Vec::new(),
            "a name one frame old is still being typed"
        );
        assert_eq!(
            app.references_name, "first",
            "the old rows stay up in between"
        );

        // The second frame is the person having stopped on it.
        app.tick();
        let asked = app.take_pending();
        assert!(
            matches!(&asked[..], [Request::References { name, .. }] if name == "second"),
            "the word the caret moved to is asked about without the chord: {asked:?}"
        );
        assert_eq!(app.references_name, "second");
        assert_eq!(app.status, "references · second\u{2026}");

        app.apply(Update::References {
            name: "second".to_owned(),
            hits: Vec::new(),
        });
        assert_eq!(app.status, "no code uses second");

        // The text can change without the caret leaving the word: deleting the `(`
        // after the name keeps `second` under the caret and changes the revision, and
        // the uses of a word in a buffer that changed are a different question.
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Char('l')));
        }
        app.handle_key(key(KeyCode::Char('x')));
        app.tick();
        app.take_pending();
        assert_eq!(app.references_name, "second", "the same word, still listed");
        app.tick();
        let asked = app.take_pending();
        assert!(
            matches!(&asked[..], [Request::References { name, .. }] if name == "second"),
            "a use typed while the caret rests on the name is not in the list yet: {asked:?}"
        );

        // Between two words there is no name, so the list goes rather than naming a
        // word the caret is beside. A caret after a word is still on it, so the space
        // left by the deleted bracket is the only place that is on neither.
        app.handle_key(key(KeyCode::Char('l')));
        app.tick();
        assert!(app.references_name.is_empty());
        assert!(app.references.is_empty());
    }

    #[test]
    fn the_references_panel_does_not_follow_a_caret_the_keys_are_not_in() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn first() {}\nfn second() { first(); }\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        app.set_focus(Focus::Editor);
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Char('l')));
        }
        let mut chord = key(KeyCode::F(12));
        chord.modifiers = KeyModifiers::SHIFT;
        app.handle_key(chord);
        app.take_pending();

        // A row opens a file at its use, and the keys are on the list while that read
        // is in flight: the caret must not re-ask about whatever word it lands on and
        // replace the list the person is reading.
        app.set_focus(Focus::References);
        app.tick();
        app.tick();
        assert_eq!(app.take_pending(), Vec::new());
        assert_eq!(app.references_name, "first");
    }

    #[test]
    fn the_project_map_panel_reads_the_map_once_and_marks_the_files_own_area() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::File {
            path: "crates/acpilot-tui/src/app.rs".to_owned(),
            text: "fn main() {}\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        assert_eq!(app.run_menu(MenuAction::ToggleProjectMap), None);
        assert!(app.show_map, "the row puts the panel up");

        // Opening the file asked what is wrong with it, which is a different question
        // from this one: draining that here keeps the assertion about the panel.
        app.take_pending();

        // A map is a document rather than a view of state that moves: the panel asks
        // when it opens, and the frame after that has nothing new to ask.
        app.tick();
        assert_eq!(app.take_pending(), vec![Request::ProjectMap]);
        app.tick();
        assert_eq!(app.take_pending(), Vec::new());

        app.apply(Update::ProjectMap(acpilot_proto::project::ProjectMap {
            path: ".acpilot/project-map.toml".to_owned(),
            written: true,
            updated: Some("2026-11-01".to_owned()),
            areas: vec![
                acpilot_proto::project::MapArea {
                    name: "Workspace".to_owned(),
                    path: String::new(),
                    summary: "everything".to_owned(),
                    depends_on: Vec::new(),
                },
                acpilot_proto::project::MapArea {
                    name: "TUI shell".to_owned(),
                    path: "crates/acpilot-tui".to_owned(),
                    summary: "the terminal frontend".to_owned(),
                    depends_on: vec!["acpilot-proto".to_owned()],
                },
            ],
        }));
        assert_eq!(app.status, "project map · 2 areas");
        let map = app.map.as_ref().expect("a map");
        assert_eq!(
            map.area_for("crates/acpilot-tui/src/app.rs"),
            Some(1),
            "the deepest area owns the open file"
        );

        app.set_focus(Focus::ProjectMap);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.map_selected, 1);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.status, "TUI shell · the terminal frontend");

        // `r` is how a map the agent has just rewritten appears without hiding and
        // showing the panel.
        app.handle_key(key(KeyCode::Char('r')));
        app.tick();
        assert_eq!(app.take_pending(), vec![Request::ProjectMap]);

        // And a workspace nobody has mapped answers with a sentence rather than an
        // empty list that reads as a workspace with nothing in it.
        app.apply(Update::ProjectMap(acpilot_proto::project::ProjectMap {
            path: ".acpilot/project-map.toml".to_owned(),
            written: false,
            updated: None,
            areas: Vec::new(),
        }));
        assert!(app.status.contains("no project map yet"), "{}", app.status);
    }

    #[test]
    fn the_tree_window_follows_the_row_it_is_on() {
        assert_eq!(first_visible(0, 10, 5), 0);
        assert_eq!(first_visible(4, 10, 5), 0, "the last row of a full pane");
        assert_eq!(first_visible(5, 10, 5), 1, "one past it scrolls one line");
        assert_eq!(first_visible(9, 10, 5), 5);
        assert_eq!(
            first_visible(2, 3, 5),
            0,
            "a list shorter than the pane cannot scroll"
        );
        assert_eq!(first_visible(0, 0, 0), 0, "an empty tree is the first row");
    }

    #[test]
    fn a_listing_brings_gits_letters_into_the_rows() {
        let mut app = App::new("http://127.0.0.1:7777");
        let mut main = entry("src/main.rs", false);
        main.status = Some(acpilot_proto::file::FileChange::Modified);
        let mut notes = entry("notes.txt", false);
        notes.status = Some(acpilot_proto::file::FileChange::Untracked);
        app.apply(Update::Listing {
            path: String::new(),
            entries: vec![main, notes, entry("Cargo.toml", false)],
        });
        // The letters are the daemon's answer and the client adds nothing: a row the
        // listing did not mark draws no letter, which is a different claim from a row
        // that is clean.
        assert_eq!(
            app.files
                .iter()
                .map(|node| (node.path.as_str(), node.status))
                .collect::<Vec<_>>(),
            vec![
                ("Cargo.toml", None),
                (
                    "notes.txt",
                    Some(acpilot_proto::file::FileChange::Untracked)
                ),
                (
                    "src/main.rs",
                    Some(acpilot_proto::file::FileChange::Modified)
                ),
            ]
        );
    }

    #[test]
    fn opening_a_directory_splices_its_children_under_it_and_closing_takes_them_away() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.focus = Focus::Files;
        app.apply(Update::Listing {
            path: String::new(),
            entries: vec![entry("src", true), entry("README.md", false)],
        });
        app.file_selected = 0;
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Request::ListDir {
                path: "src".to_owned(),
                include_ignored: false,
            }),
            "the children are the daemon's to send"
        );

        app.apply(Update::Listing {
            path: "src".to_owned(),
            entries: vec![entry("src/main.rs", false), entry("src/lib.rs", false)],
        });
        let under: Vec<(&str, u8)> = app
            .files
            .iter()
            .map(|node| (node.path.as_str(), node.depth))
            .collect();
        assert_eq!(
            under,
            vec![
                ("src", 0),
                ("src/lib.rs", 1),
                ("src/main.rs", 1),
                ("README.md", 0)
            ]
        );
        assert!(app.files[0].expanded);
        assert_eq!(app.file_selected, 1, "the first child is selected");

        app.file_selected = 0;
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.files.len(), 2, "closing drops the rows under it");
        assert!(!app.files[0].expanded);
    }

    #[test]
    fn enter_on_a_file_asks_the_daemon_and_the_answer_opens_a_buffer() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.focus = Focus::Files;
        app.apply(Update::Listing {
            path: String::new(),
            entries: vec![entry("src/main.rs", false)],
        });
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Request::OpenFile("src/main.rs".to_owned()))
        );

        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn main() {}\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        let editor = app.editor.as_ref().expect("the file is open");
        assert_eq!(editor.path(), "src/main.rs");
        assert_eq!(editor.tab(), "main.rs");
        assert_eq!(editor.language(), "rust", "the path chose the grammar");
        assert_eq!(editor.document().text(), "fn main() {}\n");
        assert_eq!(app.focus, Focus::Editor, "the keyboard follows the file");
        assert!(
            !editor.dirty(),
            "a file that was just opened has nothing to undo"
        );
    }

    #[test]
    fn insert_mode_types_into_the_buffer_and_esc_gives_the_motions_back() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.set_focus(Focus::Editor);
        let before = app.editor.as_ref().expect("a buffer").document().text();

        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.editor.as_ref().expect("a buffer").mode(), Mode::Insert);
        app.handle_key(key(KeyCode::Char('!')));
        // `?` is the key list everywhere else, and a character here: insert mode
        // is a text field, and a text field that opened a panel would be a pane
        // arguing with itself.
        app.handle_key(key(KeyCode::Char('?')));
        assert!(!app.which_key, "insert mode is a text field");

        let text = app.editor.as_ref().expect("a buffer").document().text();
        assert_ne!(text, before, "the keystrokes reached the buffer");
        assert!(text.contains("!?"), "{text}");

        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.editor.as_ref().expect("a buffer").mode(), Mode::Normal);
    }

    #[test]
    fn tab_indents_in_insert_mode_and_cycles_the_panes_otherwise() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.set_focus(Focus::Editor);
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Editor, "tab is an indent here");
        let text = app.editor.as_ref().expect("a buffer").document().text();
        assert!(
            text.starts_with("    ") || text.contains("\n    "),
            "{text:?}"
        );

        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Sessions, "and a pane away in normal mode");
    }

    #[test]
    fn a_rebound_key_is_the_one_the_app_answers_to() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.set_focus(Focus::Files);
        app.keymap = Keymap::from_toml("[keys]\npalette = [\"f1\"]\n").expect("a keymap");

        app.handle_key(key(KeyCode::F(1)));
        assert!(app.palette.is_some(), "F1 is the palette now");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(
            app.palette.is_none(),
            "Ctrl-P was bound to it and is not any more"
        );
    }

    #[test]
    fn a_rebound_editor_key_leaves_the_old_one_unbound() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.set_focus(Focus::Editor);
        app.keymap = Keymap::from_toml("[keys]\nvisual = [\"ctrl-v\"]\n").expect("a keymap");

        let mode = |app: &App| app.editor.as_ref().expect("a buffer").mode();
        app.handle_key(key(KeyCode::Char('v')));
        assert_eq!(mode(&app), Mode::Normal, "`v` is not a verb any more");
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert_eq!(mode(&app), Mode::Visual);
    }

    #[test]
    fn a_keymap_file_that_does_not_parse_is_reported_and_the_preset_is_kept() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("keymap.toml");
        std::fs::write(&path, "[keys]\nflow-north = [\"j\"]\n").expect("a file");

        let mut app = App::new("daemon");
        let error = app
            .remember_keymap(path)
            .expect("a sentence about the file");
        assert!(error.contains("flow-north"), "{error}");
        assert_eq!(
            app.action(
                Context::App,
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
            ),
            Some(Action::Palette),
            "the default table is still in force"
        );
    }

    #[test]
    fn undo_and_redo_walk_the_edits_and_the_dirty_mark_with_them() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.set_focus(Focus::Editor);
        let opened = app.editor.as_ref().expect("a buffer").document().text();
        assert!(!app.editor.as_ref().expect("a buffer").dirty());

        app.handle_key(key(KeyCode::Char('i')));
        for character in "abc".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert!(app.editor.as_ref().expect("a buffer").dirty());

        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('u')));
        assert_eq!(
            app.editor.as_ref().expect("a buffer").document().text(),
            opened,
            "a word typed without a pause is one undo"
        );
        assert!(
            !app.editor.as_ref().expect("a buffer").dirty(),
            "back at the text that was opened"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(
            app.editor.as_ref().expect("a buffer").dirty(),
            "redo put the word back"
        );
    }

    #[test]
    fn visual_mode_copies_the_run_the_motions_marked() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.set_focus(Focus::Editor);
        if let Some(editor) = app.editor.as_mut() {
            editor.goto_line(1);
        }
        let first = app
            .editor
            .as_ref()
            .expect("a buffer")
            .document()
            .text()
            .chars()
            .next()
            .expect("seeded text")
            .to_string();

        app.handle_key(key(KeyCode::Char('v')));
        assert_eq!(app.editor.as_ref().expect("a buffer").mode(), Mode::Visual);
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.clipboard.as_deref(), Some(first.as_str()));
        assert_eq!(
            app.editor.as_ref().expect("a buffer").mode(),
            Mode::Normal,
            "y leaves the run"
        );
    }

    #[test]
    fn a_colon_write_asks_for_the_same_save_as_ctrl_s() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.focus = Focus::Files;
        app.apply(Update::Listing {
            path: String::new(),
            entries: vec![entry("src/main.rs", false)],
        });
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn main() {}\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        app.set_focus(Focus::Editor);
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Esc));

        app.handle_key(key(KeyCode::Char(':')));
        app.handle_key(key(KeyCode::Char('w')));
        let request = app.handle_key(key(KeyCode::Enter));
        let Some(Request::SaveFile { path, etag, .. }) = request else {
            panic!("expected a save, got {request:?}");
        };
        assert_eq!(path, "src/main.rs");
        assert_eq!(etag, "\"v1\"", "the write names the version it replaces");
    }

    #[test]
    fn a_colon_quit_refuses_a_dirty_buffer_and_a_bang_discards_it() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.focus = Focus::Files;
        app.apply(Update::Listing {
            path: String::new(),
            entries: vec![entry("src/main.rs", false)],
        });
        app.apply(Update::File {
            path: "src/main.rs".to_owned(),
            text: "fn main() {}\n".to_owned(),
            etag: "\"v1\"".to_owned(),
            marks: Vec::new(),
        });
        app.set_focus(Focus::Editor);
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Esc));

        app.handle_key(key(KeyCode::Char(':')));
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(!app.should_quit, "a dirty buffer keeps the window open");
        assert!(app.status.contains("unsaved"), "{}", app.status);

        app.handle_key(key(KeyCode::Char(':')));
        app.handle_key(key(KeyCode::Char('q')));
        app.handle_key(key(KeyCode::Char('!')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Request::Quit),
            ":q! is the way out"
        );
        assert!(app.should_quit);
    }

    #[test]
    fn the_design_pass_is_seeded_and_its_transcript_went_through_the_renderer() {
        let demo = acpilot_demo::demo();
        let app = App::demo(&demo);
        assert!(app.demo);
        assert_eq!(app.sessions.len(), demo.sessions.len());
        assert_eq!(app.open.as_deref(), Some("s1"));
        assert!(
            app.entries.len() >= 4,
            "the transcript was folded, not dumped"
        );
        assert!(
            app.entries.iter().any(|entry| entry.kind == Kind::Tool),
            "a tool call is one of the shapes the renderer must survive"
        );
        assert!(app.editor.is_some(), "the editor pane has a buffer");
        assert!(!app.files.is_empty(), "the tree has rows");
        assert!(!app.integrations.is_empty(), "the stats bar has rows");
        assert_eq!(app.permissions.len(), 1, "a question is waiting");
        assert!(!app.permission_card, "and it is not in the way on launch");
    }

    #[test]
    fn f9_opens_the_menu_and_enter_runs_the_row_under_it() {
        let mut app = App::new("http://127.0.0.1:7777");
        assert_eq!(app.handle_key(key(KeyCode::F(9))), None);
        assert_eq!(app.menu.map(|menu| menu.open), Some(0));

        app.handle_key(key(KeyCode::Down));
        assert_eq!(
            app.menu.map(|menu| menu.items),
            Some(true),
            "the list opens"
        );
        app.handle_key(key(KeyCode::Enter));
        assert!(app.palette.is_some(), "the first row is the palette");
        assert!(app.menu.is_none(), "running a row closes the bar");
    }

    #[test]
    fn the_help_menu_opens_the_about_card() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.handle_key(key(KeyCode::F(9)));
        for _ in 0..MENUS.len() - 1 {
            app.handle_key(key(KeyCode::Right));
        }
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.about);
    }

    #[test]
    fn escape_steps_out_of_the_menu_one_level_at_a_time() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.handle_key(key(KeyCode::F(9)));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.menu.map(|menu| menu.items), Some(false));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.menu.is_none(), "the second press leaves the bar");
    }

    #[test]
    fn the_menu_bar_walks_sideways_and_a_row_moves_the_keyboard() {
        let mut app = transcript_app();
        app.handle_key(key(KeyCode::F(9)));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.menu.map(|menu| menu.open), Some(1), "View");
        app.handle_key(key(KeyCode::Down));
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Transcript, "the row is the pane it names");
    }

    #[test]
    fn any_key_turns_the_about_card() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.about = true;
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), None);
        assert!(!app.about);
        assert!(
            app.input.is_empty(),
            "the key that closed the card does not fall through to a pane"
        );
    }

    #[test]
    fn about_names_the_build_a_report_is_about() {
        // The four facts a bug report asks for, from the version constant the CLI
        // and the daemon report: a card that disagrees with its tarball is a report
        // about a build nobody has.
        let lines = about_lines();
        assert_eq!(lines.len(), 4);
        let value = |label: &str| {
            lines
                .iter()
                .find(|(name, _)| *name == label)
                .map(|(_, value)| *value)
        };
        assert_eq!(value("Version"), Some(acpilot_core::VERSION));
        assert_eq!(value("License"), Some("MIT OR Apache-2.0"));
        assert_eq!(value("Field manual"), Some("rudeboy.dev/acpilot"));
        assert_eq!(value("Built with"), Some("Freebuff — freebuff.com"));
    }

    #[test]
    fn ctrl_arrows_resize_the_focused_column() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.focus = Focus::Sessions;
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.sidebar_width, 46);
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(app.sidebar_width, 42);

        // The tree and the editor share a divider, so the arrow follows the pane
        // with the keyboard: a wider editor is a narrower tree.
        app.focus = Focus::Files;
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.files_width, 28);
        app.focus = Focus::Editor;
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.files_width, 26);
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(app.files_width, 28, "and the other arrow gives it back");
    }

    fn mouse(kind: MouseEventKind, column: u16) -> MouseEvent {
        mouse_at(kind, column, 4)
    }

    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// The bar is the top row, and a list of `rows` hangs under the first title.
    fn with_menu_areas(app: &mut App, list: Option<MenuList>) {
        app.set_viewports(Viewports {
            files: None,
            sessions: None,
            terminal: TerminalSize {
                cols: 120,
                rows: 34,
            },
            transcript: None,
            dividers: Dividers {
                tree: Some(25),
                sidebar: Some(75),
                left: 0,
                right: 120,
            },
            menu: MenuAreas { row: 0, list },
            editor: None,
            terminal_area: None,
        });
    }

    #[test]
    fn alt_and_the_underlined_letter_opens_a_menu_with_its_list() {
        let mut app = App::new("http://127.0.0.1:7777");
        let alt = |letter| KeyEvent::new(KeyCode::Char(letter), KeyModifiers::ALT);

        app.handle_key(alt('f'));
        assert_eq!(
            app.menu,
            Some(MenuState {
                open: 0,
                items: true,
                at: 0
            }),
            "Alt-F is File, with its list showing"
        );

        // The bar is up, so another `Alt` letter moves to that title rather than
        // closing what is open. Help is the last of the five.
        app.handle_key(alt('h'));
        assert_eq!(
            app.menu,
            Some(MenuState {
                open: MENUS.len() - 1,
                items: true,
                at: 0
            })
        );

        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.menu, None, "two escapes leave the bar");
        app.handle_key(alt('z'));
        assert_eq!(app.menu, None, "a letter no title underlines is not a menu");
    }

    #[test]
    fn a_shell_gets_alt_and_a_letter_as_a_byte() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.apply(crate::backend::Update::TerminalOpened {
            terminal: terminal(),
            replay_bytes: 0,
        });

        // `Alt-F` is a word to readline and the File menu to the client: the shell
        // wins while it has the keyboard, which is why the mnemonic is answered
        // below the terminal in `handle_key`.
        let request = app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(
            request,
            Some(Request::TerminalWrite(vec![0x1b, b'f'])),
            "the escape a terminal sends for Alt and a letter"
        );
        assert_eq!(app.menu, None);
    }

    #[test]
    fn a_click_on_a_title_opens_switches_and_closes() {
        let mut app = App::new("http://127.0.0.1:7777");
        with_menu_areas(&mut app, None);
        let click = |column| mouse_at(MouseEventKind::Down(MouseButton::Left), column, 0);

        // File's title starts at the tenth cell; View is six cells further on.
        assert_eq!(app.handle_mouse(click(11)), None);
        assert_eq!(app.menu.map(|state| state.open), Some(0));
        assert!(app.menu.is_some_and(|state| state.items));

        app.handle_mouse(click(18));
        assert_eq!(app.menu.map(|state| state.open), Some(1), "View");

        // A click on the title already open closes it, the way a menu bar toggles.
        app.handle_mouse(click(18));
        assert_eq!(app.menu, None);

        // The bar is chrome, not a pane: a click on the brand is not a menu and
        // does not start a drag either.
        assert_eq!(app.handle_mouse(click(2)), None);
        assert_eq!(app.menu, None);
        assert_eq!(app.drag, None);
    }

    #[test]
    fn a_click_on_a_row_runs_it_and_a_click_beside_the_list_closes_it() {
        let mut app = App::new("http://127.0.0.1:7777");
        // A File list drawn at column 9, one row down, four rows tall: Palette,
        // Refresh, Quit and a spare.
        with_menu_areas(
            &mut app,
            Some(MenuList {
                x: 9,
                y: 1,
                width: 30,
                rows: 3,
            }),
        );
        app.menu = Some(MenuState {
            open: 0,
            items: true,
            at: 0,
        });

        // The list's first row is one below its border: the third is Quit, and
        // clicking it is the same request `Ctrl-C` makes.
        let quit = app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 20, 4));
        assert_eq!(quit, Some(Request::Quit));
        assert!(app.should_quit);
        assert_eq!(app.menu, None, "running a row closes the list");

        app.menu = Some(MenuState {
            open: 0,
            items: true,
            at: 0,
        });
        // A click on the pane below the list dismisses it and lands on nothing else.
        assert_eq!(
            app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 60, 20)),
            None
        );
        assert_eq!(app.menu, None);
        assert_eq!(app.drag, None, "and a dismissed list starts no drag");
    }

    #[test]
    fn a_pointer_over_the_bar_follows_the_open_menu() {
        let mut app = App::new("http://127.0.0.1:7777");
        with_menu_areas(
            &mut app,
            Some(MenuList {
                x: 9,
                y: 1,
                width: 30,
                rows: 3,
            }),
        );
        app.menu = Some(MenuState {
            open: 0,
            items: true,
            at: 0,
        });

        app.handle_mouse(mouse_at(MouseEventKind::Moved, 18, 0));
        assert_eq!(
            app.menu.map(|state| state.open),
            Some(1),
            "View, under the pointer"
        );

        app.handle_mouse(mouse_at(MouseEventKind::Moved, 20, 3));
        assert_eq!(app.menu.map(|state| state.at), Some(1), "the row under it");

        // Nothing is highlighted under a pointer with no menu open.
        app.menu = None;
        app.handle_mouse(mouse_at(MouseEventKind::Moved, 18, 0));
        assert_eq!(app.menu, None);
    }

    #[test]
    fn a_dragged_divider_is_a_width() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.set_viewports(Viewports {
            files: None,
            sessions: None,
            terminal: TerminalSize {
                cols: 140,
                rows: 30,
            },
            transcript: None,
            dividers: Dividers {
                tree: Some(25),
                sidebar: Some(95),
                left: 0,
                right: 140,
            },
            menu: MenuAreas::default(),
            editor: None,
            terminal_area: None,
        });

        // The divider is one cell, so the grab is a cell either side of it.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 26));
        assert_eq!(app.drag, Some(Drag::Divider(Divider::Tree)));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 39));
        assert_eq!(app.files_width, 40, "40 cells, so the border is at 39");
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 39));
        assert!(app.drag.is_none(), "letting go lets go");

        // The sidebar is measured from the body's right edge.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 95));
        assert_eq!(app.drag, Some(Drag::Divider(Divider::Sidebar)));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 60));
        assert_eq!(app.sidebar_width, 80, "140 - 60, clamped to the widest");

        // A click on a pane is not a drag and not a focus change.
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 60));
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 70));
        assert!(app.drag.is_none());
        assert_eq!(app.focus, Focus::Sessions);
    }

    #[test]
    fn the_widths_a_drag_chose_are_the_widths_the_next_launch_opens() {
        let directory = tempfile::tempdir().expect("temp dir");
        // Nested, because the first launch on a machine is exactly when the
        // directory does not exist yet.
        let path = directory.path().join("acpilot").join("tui-layout.toml");

        let mut first = App::new("http://127.0.0.1:7777");
        first.remember_layout(path.clone());
        assert_eq!(
            first.files_width,
            crate::layout::DEFAULT_FILES,
            "a first launch"
        );
        with_dividers(&mut first);
        first.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 26));
        first.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 39));
        first.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 39));
        first.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 95));
        first.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 60));
        first.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 60));
        assert_eq!((first.files_width, first.sidebar_width), (40, 80));
        assert!(first.save_layout().is_none(), "a width it could write");

        let mut second = App::new("http://127.0.0.1:7777");
        second.remember_layout(path.clone());
        assert_eq!(
            (second.files_width, second.sidebar_width),
            (40, 80),
            "the drag was the launch's starting point"
        );

        // A launch that changed nothing writes nothing: opening a window is not a
        // reason to touch the disk.
        std::fs::remove_file(&path).expect("the file it read");
        assert!(second.save_layout().is_none());
        assert!(!path.exists(), "nothing to remember, nothing written");

        // The design pass has nowhere to write and says so by having no path.
        let demo = App::new("http://127.0.0.1:7777");
        assert!(demo.layout_path.is_none());
    }

    /// The two dividers where a 140-column body would draw them: the tree's border
    /// at 25 and the sidebar's at 95.
    fn with_dividers(app: &mut App) {
        app.set_viewports(Viewports {
            files: None,
            sessions: None,
            terminal: TerminalSize {
                cols: 140,
                rows: 30,
            },
            transcript: None,
            dividers: Dividers {
                tree: Some(25),
                sidebar: Some(95),
                left: 0,
                right: 140,
            },
            menu: MenuAreas::default(),
            editor: None,
            terminal_area: None,
        });
    }

    #[test]
    fn a_drag_cannot_make_a_pane_unusable() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.set_viewports(Viewports {
            files: None,
            sessions: None,
            terminal: TerminalSize {
                cols: 140,
                rows: 30,
            },
            transcript: None,
            dividers: Dividers {
                tree: Some(25),
                sidebar: Some(95),
                left: 0,
                right: 140,
            },
            menu: MenuAreas::default(),
            editor: None,
            terminal_area: None,
        });
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 25));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 0));
        assert_eq!(app.files_width, 16, "the tree keeps a column worth reading");
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 120));
        assert_eq!(app.files_width, 60);

        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 120));
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 95));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 139));
        assert_eq!(app.sidebar_width, 24, "and the sidebar keeps its own");
    }

    #[test]
    fn the_dividers_stop_where_a_pane_would_be_unusable() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.focus = Focus::Files;
        for _ in 0..40 {
            app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        }
        assert_eq!(app.files_width, 60);
        for _ in 0..40 {
            app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        }
        assert_eq!(app.files_width, 16);
    }

    /// An app with the editor on screen where the layout would draw it: a border,
    /// a five-column gutter, and twenty rows of text after both.
    fn with_editor() -> App {
        let mut app = App::new("http://127.0.0.1:7777");
        let seed = acpilot_demo::editor::buffer();
        let mut editor = EditorPane::seeded(
            &seed,
            acpilot_git::line_changes(&acpilot_demo::editor::diff().patch(), &seed.path),
            &mut app.grammars,
        );
        editor.resize(20, 55);
        app.editor = Some(editor);
        app.set_viewports(Viewports {
            files: None,
            sessions: None,
            terminal: TerminalSize {
                cols: 140,
                rows: 40,
            },
            transcript: None,
            dividers: Dividers {
                tree: Some(25),
                sidebar: Some(95),
                left: 0,
                right: 140,
            },
            menu: MenuAreas::default(),
            editor: Some(EditorArea {
                pane: PaneArea {
                    x: 25,
                    y: 2,
                    width: 60,
                    height: 20,
                },
                gutter: 5,
            }),
            terminal_area: None,
        });
        app
    }

    #[test]
    fn a_click_in_the_text_puts_the_caret_there_and_takes_the_keyboard() {
        let mut app = with_editor();
        app.focus = Focus::Sessions;

        // The text starts at the pane's first cell plus its gutter, so column 32
        // is the third cell of the fourth line on screen.
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 32, 5));
        assert_eq!(
            app.editor.as_ref().expect("a buffer").position(),
            (4, 3),
            "the cell under the pointer, counted from one"
        );
        assert_eq!(app.focus, Focus::Editor, "a click into a pane focuses it");
        assert_eq!(
            app.drag,
            Some(Drag::Editor),
            "the press is the anchor a drag grows from"
        );
    }

    #[test]
    fn a_drag_in_the_editor_selects_and_y_copies_it() {
        let mut app = with_editor();
        app.focus = Focus::Sessions;

        // The text starts at column 30, so a drag from the first cell of the fifth
        // screen row to the fourth cell of the row under it covers two lines — the
        // blank line above them is why the drag does not start one row higher.
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 30, 4));
        app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 34, 5));
        app.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), 34, 5));
        assert!(app.drag.is_none(), "letting go lets go");

        let editor = app.editor.as_ref().expect("a buffer");
        let selection = editor.selection().expect("a run of characters");
        let expected = editor.document().buffer().slice(selection);
        assert!(!expected.is_empty());
        assert!(app.status.contains("selected 2 line"), "{}", app.status);
        assert!(
            app.clipboard.is_none(),
            "a selection is marked first; `y` is what takes it"
        );

        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.clipboard.as_deref(), Some(expected.as_str()));
        assert_eq!(app.status, "copied 2 line(s)");
        assert!(
            !app.editor.as_ref().expect("a buffer").has_selection(),
            "the copy ends the selection"
        );
    }

    #[test]
    fn a_click_in_the_editor_that_selected_nothing_leaves_a_bare_caret() {
        let mut app = with_editor();
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 32, 5));
        app.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), 32, 5));
        assert!(
            !app.editor.as_ref().expect("a buffer").has_selection(),
            "one cell is a click, not a run"
        );
        assert!(app.clipboard.is_none());
        assert_eq!(app.focus, Focus::Editor, "and it is still a focus move");
    }

    #[test]
    fn a_drag_leaving_the_editor_selects_to_the_edge_it_left_by() {
        let mut app = with_editor();
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 30, 2));
        app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 200, 200));
        let editor = app.editor.as_ref().expect("a buffer");
        assert!(editor.has_selection(), "the gesture is not dropped");
        assert_eq!(
            Some(editor.caret().line),
            editor.rows().last().map(|row| row.line),
            "the head stopped at the last row the pane shows"
        );
    }

    #[test]
    fn a_drag_resting_at_the_editors_edge_scrolls_and_keeps_growing() {
        let mut app = with_editor();
        let top = app.editor.as_ref().expect("a buffer").viewport().top();
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 30, 4));
        app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 34, 21));

        // A pointer that rests sends nothing at all, so the scroll is the loop's
        // clock rather than an event's.
        assert!(app.autoscroll(), "there is more file under the edge");
        let editor = app.editor.as_ref().expect("a buffer");
        assert_eq!(editor.viewport().top(), top + 1, "the view moved a line");
        let first = editor.caret().line;

        app.autoscroll();
        let editor = app.editor.as_ref().expect("a buffer");
        assert_eq!(editor.viewport().top(), top + 2);
        assert!(editor.caret().line > first, "the selection grew with it");
        assert_eq!(
            Some(editor.caret().line),
            editor.rows().last().map(|row| row.line),
            "and its head is still the row the pointer rests on"
        );

        // The end of the file is the end of the scroll, and it says so rather than
        // redrawing thirty times a second to prove it.
        for _ in 0..80 {
            app.autoscroll();
        }
        assert!(!app.autoscroll());
        assert!(
            !app.autoscrolling(),
            "the loop can go back to its own clock"
        );
        assert!(
            app.editor.as_ref().expect("a buffer").has_selection(),
            "and the run it marked is still marked"
        );
    }

    /// An app with the tree and the session list where the layout would draw them.
    ///
    /// The boxes are the cells inside their borders, which is what the mouse counts
    /// against: the drawing is the only thing that knows them, so a test that clicks
    /// a pane has to say where it was drawn.
    fn with_panes() -> App {
        let demo = acpilot_demo::demo();
        let mut app = App::demo(&demo);
        app.set_viewports(Viewports {
            files: Some(PaneArea {
                x: 1,
                y: 2,
                width: 24,
                height: 20,
            }),
            sessions: Some(PaneArea {
                x: 96,
                y: 2,
                width: 42,
                height: 5,
            }),
            terminal: TerminalSize {
                cols: 100,
                rows: 30,
            },
            transcript: None,
            dividers: Dividers {
                tree: Some(25),
                sidebar: Some(95),
                left: 0,
                right: 140,
            },
            menu: MenuAreas::default(),
            editor: None,
            terminal_area: None,
        });
        app
    }

    #[test]
    fn a_click_in_the_tree_selects_that_row_and_takes_the_keyboard() {
        let mut app = with_panes();
        app.focus = Focus::Sessions;
        assert!(app.files.len() > 2, "the demo seeds a tree");

        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 5, 4));
        assert_eq!(app.focus, Focus::Files);
        assert_eq!(app.file_selected, 2, "the third row of the box");
        assert!(app.drag.is_none(), "a click in a pane is not a drag");

        // Inside the pane but past the last file: the keyboard moves, nothing else.
        app.file_selected = 1;
        let below = u16::try_from(app.files.len() + 2).expect("a small tree");
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 5, below));
        assert_eq!(app.focus, Focus::Files);
        assert_eq!(app.file_selected, 1, "the empty rows are not files");
    }

    #[test]
    fn a_click_in_the_session_list_opens_that_session() {
        let mut app = with_panes();
        assert!(app.sessions.len() > 1, "the demo seeds sessions");

        let request = app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 100, 3));
        assert_eq!(app.selected, 1, "the second row of the box");
        assert_eq!(app.focus, Focus::Sessions);
        assert!(
            matches!(&request, Some(Request::Select(id)) if id == "s2"),
            "a click on a session asks the worker for that session: {request:?}"
        );

        // A click on the empty rows under the last session moves the keyboard and
        // no session: the list is a list, not a grid.
        app.selected = 1;
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 100, 5));
        assert_eq!(app.focus, Focus::Sessions);
        assert_eq!(app.selected, 1, "the empty rows are not sessions");
    }

    #[test]
    fn a_click_on_the_numbers_is_not_a_caret() {
        let mut app = with_editor();
        let before = app.editor.as_ref().expect("a buffer").position();
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 27, 5));
        assert_eq!(app.editor.as_ref().expect("a buffer").position(), before);
        assert_eq!(app.focus, Focus::Sessions, "the gutter is furniture");
    }

    #[test]
    fn the_wheel_scrolls_the_editor_without_moving_the_caret() {
        let mut app = with_editor();
        let caret = app.editor.as_ref().expect("a buffer").position();

        app.handle_mouse(mouse_at(MouseEventKind::ScrollDown, 40, 10));
        assert_eq!(
            app.editor.as_ref().expect("a buffer").viewport().top(),
            usize::try_from(WHEEL_LINES).expect("three lines"),
            "a notch is a few lines, not one and not a page"
        );

        app.handle_mouse(mouse_at(MouseEventKind::ScrollUp, 40, 10));
        let editor = app.editor.as_ref().expect("a buffer");
        assert_eq!(editor.viewport().top(), 0);
        assert_eq!(editor.position(), caret, "the caret stayed where it was");
        assert_eq!(
            app.focus,
            Focus::Sessions,
            "a wheel is pointed, not aimed: it does not take the keyboard"
        );

        // A wheel somewhere else is somebody else's.
        app.handle_mouse(mouse_at(MouseEventKind::ScrollDown, 10, 10));
        assert_eq!(app.editor.as_ref().expect("a buffer").viewport().top(), 0);
    }

    #[test]
    fn the_wheel_reads_back_through_the_terminal_and_a_key_returns_to_live() {
        let mut app = with_terminal();
        let printed: String = (0..60).map(|line| format!("line {line}\r\n")).collect();
        app.apply(Update::TerminalData(printed.into_bytes()));
        app.set_viewports(Viewports {
            files: None,
            sessions: None,
            terminal: TerminalSize {
                cols: 100,
                rows: 30,
            },
            transcript: None,
            dividers: Dividers::default(),
            menu: MenuAreas::default(),
            editor: None,
            terminal_area: Some(PaneArea {
                x: 1,
                y: 2,
                width: 98,
                height: 30,
            }),
        });
        assert!(
            app.terminal.as_ref().expect("a pane").history() > 0,
            "the program printed more than a screen"
        );

        app.handle_mouse(mouse_at(MouseEventKind::ScrollUp, 40, 10));
        let scrolled = app.terminal.as_ref().expect("a pane").reader.view.scroll;
        assert!(scrolled > 0, "the wheel went back");
        assert_eq!(app.focus, Focus::Terminal, "and kept the keyboard it had");

        app.handle_mouse(mouse_at(MouseEventKind::ScrollDown, 40, 10));
        assert_eq!(
            app.terminal.as_ref().expect("a pane").reader.view.scroll,
            0,
            "a notch down comes back to the live screen"
        );

        // Read back, then answer the program: the prompt is about to print below
        // the fold, so the view returns to the newest line.
        app.handle_mouse(mouse_at(MouseEventKind::ScrollUp, 40, 10));
        app.handle_mouse(mouse_at(MouseEventKind::ScrollUp, 40, 10));
        assert!(
            app.terminal.as_ref().expect("a pane").reader.view.scroll > 0,
            "two notches of history"
        );
        let write = app.handle_key(key(KeyCode::Char('x')));
        assert!(
            matches!(write, Some(Request::TerminalWrite(_))),
            "the key still goes to the program"
        );
        assert_eq!(
            app.terminal.as_ref().expect("a pane").reader.view.scroll,
            0,
            "typing is answering the program, not reading behind it"
        );

        // A wheel above the pane is not the terminal's.
        app.handle_mouse(mouse_at(MouseEventKind::ScrollUp, 40, 0));
        assert_eq!(app.terminal.as_ref().expect("a pane").reader.view.scroll, 0);
    }

    /// A terminal drawn inside a 140×30 window, from column 1 and row 2.
    ///
    /// The area is what the mouse counts against, and the drawing is the only thing
    /// that knows it, so a test that clicks a pane has to say where it was drawn.
    fn drawn_terminal() -> App {
        let mut app = with_terminal();
        app.set_viewports(Viewports {
            files: None,
            sessions: None,
            terminal: TerminalSize {
                cols: 100,
                rows: 30,
            },
            transcript: None,
            dividers: Dividers::default(),
            menu: MenuAreas::default(),
            editor: None,
            terminal_area: Some(PaneArea {
                x: 1,
                y: 2,
                width: 98,
                height: 30,
            }),
        });
        app
    }

    #[test]
    fn a_drag_across_the_terminal_selects_the_run_and_y_copies_it() {
        let mut app = drawn_terminal();
        app.apply(Update::TerminalData(
            b"hello world\r\nsecond line\r\n".to_vec(),
        ));

        // The press is also what enters copy mode, because a pointer has no key to
        // press first, and the anchor is the cell it landed on.
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 1, 2));
        assert_eq!(app.drag, Some(Drag::Terminal));
        assert_eq!(app.terminal.as_ref().expect("a pane").mode, PaneMode::Copy);

        app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 6, 2));
        app.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), 6, 2));
        assert!(app.drag.is_none(), "letting go lets go");
        assert!(app.terminal.as_ref().expect("a pane").has_selection());
        assert!(app.status.contains("selected 1 line"), "{}", app.status);
        assert!(
            app.clipboard.is_none(),
            "a selection is marked first; `y` is what takes it"
        );

        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), None);
        assert_eq!(app.clipboard.as_deref(), Some("hello"));
        assert_eq!(
            app.terminal.as_ref().expect("a pane").mode,
            PaneMode::Live,
            "a yank leaves the pane following the program"
        );
    }

    #[test]
    fn a_drag_that_leaves_the_pane_selects_to_the_edge_it_left_by() {
        let mut app = drawn_terminal();
        app.apply(Update::TerminalData(b"hello world\r\n".to_vec()));

        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 1, 2));
        app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 200, 2));
        app.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), 200, 2));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            app.clipboard.as_deref(),
            Some("hello world"),
            "a pointer past the pane marks everything to its edge"
        );
    }

    #[test]
    fn a_drag_resting_at_the_terminals_edge_reads_further_back() {
        let mut app = drawn_terminal();
        let printed: String = (0..60).map(|line| format!("line {line}\r\n")).collect();
        app.apply(Update::TerminalData(printed.into_bytes()));

        // A press in the middle of the screen, dragged to the pane's first row.
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 1, 12));
        app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 2));
        let before = app.terminal.as_mut().expect("a pane").yank();
        let began_on = before.lines().last().unwrap_or("").to_owned();

        assert!(app.autoscroll(), "there is output behind the screen");
        let pane = app.terminal.as_mut().expect("a pane");
        assert_eq!(pane.reader.view.scroll, 1, "one line older, drawn lower");
        let after = pane.yank();
        assert_eq!(
            after.lines().last(),
            Some(began_on.as_str()),
            "the line the drag began on is still the line it marks"
        );
        assert!(
            after.lines().count() > before.lines().count(),
            "and the line above it joined the selection"
        );

        // Dragged the other way it comes forward again, and the same cells keep
        // meaning the same text.
        app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 31));
        assert!(app.autoscroll());
        assert_eq!(
            app.terminal.as_ref().expect("a pane").reader.view.scroll,
            0,
            "a drag at the bottom edge is the way back to live"
        );
        assert!(
            !app.autoscroll(),
            "and at the live screen there is nothing further forward"
        );
    }

    #[test]
    fn a_terminal_drag_reads_past_the_screen_it_began_on() {
        let mut app = drawn_terminal();
        let printed: String = (0..120).map(|line| format!("line {line}\r\n")).collect();
        app.apply(Update::TerminalData(printed.into_bytes()));

        // A press in the middle, dragged to the pane's first row and held there.
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 1, 12));
        app.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), 1, 2));
        let mut steps = 0;
        while app.autoscroll() {
            steps += 1;
            assert!(steps < 1_000, "the history is finite");
        }

        let pane = app.terminal.as_mut().expect("a pane");
        let rows = usize::from(pane.grid_size().0);
        assert_eq!(pane.reader.view.scroll, pane.history(), "the oldest line");
        let yanked = pane.yank();
        assert!(
            yanked.lines().count() > rows * 2,
            "a drag at the edge is not held to one screen: {} lines",
            yanked.lines().count()
        );
        assert_eq!(
            yanked.lines().next().map(str::trim_end),
            Some("line 0"),
            "the anchor kept its text while the head walked to the oldest line"
        );
    }

    #[test]
    fn a_click_that_selects_nothing_gives_the_keys_back_to_the_program() {
        let mut app = drawn_terminal();
        app.apply(Update::TerminalData(b"hello\r\n".to_vec()));
        let before = app.status.clone();

        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 20, 4));
        app.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), 20, 4));
        assert_eq!(
            app.terminal.as_ref().expect("a pane").mode,
            PaneMode::Live,
            "copy mode is where bytes go nowhere, so a click must not leave it here"
        );
        assert!(!app.terminal.as_ref().expect("a pane").has_selection());
        assert_eq!(app.status, before, "a click that did nothing says nothing");
        assert!(
            matches!(
                app.handle_key(key(KeyCode::Char('q'))),
                Some(Request::TerminalWrite(_))
            ),
            "and the next key is the program's again"
        );

        // A query being typed owns the keyboard: a click does not move the
        // selection out from under the line being typed.
        app.handle_key(key(KeyCode::F(2)));
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 1, 2));
        assert!(app.drag.is_none(), "the click was not the pane's");
        assert!(
            app.terminal
                .as_ref()
                .expect("a pane")
                .reader
                .typing
                .is_some(),
            "the query is still open"
        );
    }

    #[test]
    fn ctrl_c_asks_to_quit() {
        let mut app = App::new("http://127.0.0.1:7777");
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(key), Some(Request::Quit));
        assert!(app.should_quit);
    }

    fn terminal() -> TerminalSummary {
        TerminalSummary {
            id: "agent-terminal-1".to_owned(),
            session_id: Some("s1".to_owned()),
            workspace: None,
            host: None,
            size: TerminalSize { cols: 80, rows: 24 },
            alive: true,
            command: "/bin/sh -c make".to_owned(),
        }
    }

    fn with_terminal() -> App {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.set_pane_size(TerminalSize {
            cols: 100,
            rows: 30,
        });
        app.apply(Update::TerminalOpened {
            terminal: terminal(),
            replay_bytes: 0,
        });
        app
    }

    fn second_terminal() -> TerminalSummary {
        TerminalSummary {
            id: "agent-terminal-2".to_owned(),
            session_id: Some("s1".to_owned()),
            workspace: None,
            host: None,
            size: TerminalSize { cols: 80, rows: 24 },
            alive: false,
            command: "/bin/sh -c cargo test".to_owned(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn ctrl_t_asks_for_the_open_sessions_terminal() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        // One terminal is the common case and needs no question.
        app.apply(Update::Terminals(vec![terminal()]));
        let toggle = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(
            app.handle_key(toggle),
            Some(Request::OpenTerminal {
                id: "agent-terminal-1".to_owned()
            })
        );

        // A second press leaves the pane that is now up.
        app.apply(Update::TerminalOpened {
            terminal: terminal(),
            replay_bytes: 0,
        });
        assert_eq!(app.handle_key(toggle), Some(Request::CloseTerminal));
        assert!(app.terminal.is_none());
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn ctrl_t_starts_a_shell_in_the_sessions_workspace_when_none_is_open() {
        let mut app = App::new("http://127.0.0.1:7777");
        let mut remote = session("s1", "one");
        remote.workspace_id = "ssh:box:/srv/acpilot".to_owned();
        app.apply(Update::Sessions(vec![remote]));
        app.set_pane_size(TerminalSize {
            cols: 100,
            rows: 30,
        });
        open(&mut app);

        let toggle = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(
            app.handle_key(toggle),
            Some(Request::NewTerminal {
                // The id, not a host name: the daemon knows which workspaces are
                // remote, and a window that guessed would be a second opinion.
                workspace: Some("ssh:box:/srv/acpilot".to_owned()),
                // The pane's size, so the shell does not start 80 columns wide and
                // repaint on the first draw.
                size: TerminalSize {
                    cols: 100,
                    rows: 30
                },
            })
        );
        assert!(app.terminal.is_none(), "the daemon answers with the shell");
        assert!(app.status.contains("starting a shell"), "{}", app.status);
    }

    #[test]
    fn a_session_without_a_workspace_asks_for_the_daemons_own() {
        let mut app = App::new("http://127.0.0.1:7777");
        let mut local = session("s1", "one");
        // What this client's own sessions are created with: the daemon serves one
        // workspace, and an empty id is how a request says "that one".
        local.workspace_id = String::new();
        app.apply(Update::Sessions(vec![local]));
        open(&mut app);

        let toggle = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(
            app.handle_key(toggle),
            Some(Request::NewTerminal {
                workspace: None,
                size: TerminalSize { cols: 80, rows: 24 },
            })
        );
    }

    #[test]
    fn several_terminals_ask_which_one_and_enter_opens_it() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.apply(Update::Terminals(vec![terminal(), second_terminal()]));

        let toggle = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(toggle), None, "a question, not an attach");
        assert!(app.terminal_picker);
        assert!(app.terminal.is_none());

        // The picker is the only thing listening: a letter goes to it, not to a
        // shell that is not on screen yet.
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.picker_selected, 1);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Request::OpenTerminal {
                id: "agent-terminal-2".to_owned()
            })
        );
        assert!(!app.terminal_picker, "the question is answered");
    }

    #[test]
    fn a_sessionless_pane_is_offered_too() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        let mut orphan = terminal();
        orphan.session_id = None;
        app.apply(Update::Terminals(vec![orphan]));

        // One pane still needs no question, and a shell nobody owns is still the
        // shell the person pressed `Ctrl-T` for.
        let toggle = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(
            app.handle_key(toggle),
            Some(Request::OpenTerminal {
                id: "agent-terminal-1".to_owned()
            })
        );
    }

    #[test]
    fn the_picker_can_start_a_shell_beside_the_panes_it_lists() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.apply(Update::Sessions(vec![session("s1", "one")]));
        app.set_pane_size(TerminalSize {
            cols: 100,
            rows: 30,
        });
        open(&mut app);
        let mut orphan = terminal();
        orphan.session_id = None;
        app.apply(Update::Terminals(vec![terminal(), orphan]));

        let toggle = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(toggle), None, "a question, not an attach");
        assert!(app.terminal_picker);

        // The picker is the only way to a shell when every row belongs to somebody
        // else, so it answers to `n` as well as to a row.
        assert_eq!(
            app.handle_key(key(KeyCode::Char('n'))),
            Some(Request::NewTerminal {
                // The session's workspace, the same one `Ctrl-T` would have used.
                workspace: Some("work".to_owned()),
                size: TerminalSize {
                    cols: 100,
                    rows: 30
                },
            })
        );
        assert!(!app.terminal_picker);
        assert!(app.status.contains("starting a shell"), "{}", app.status);
    }

    #[test]
    fn escape_leaves_the_picker_without_attaching() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.apply(Update::Terminals(vec![terminal(), second_terminal()]));
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(app.terminal_picker);

        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert!(!app.terminal_picker);
        assert!(app.terminal.is_none());

        // Ctrl-T while the picker is up closes it too, rather than stacking a
        // second question on top of the first.
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(app.terminal_picker);
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(!app.terminal_picker);
    }

    #[test]
    fn the_picker_follows_a_list_that_shrinks_under_it() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.apply(Update::Terminals(vec![terminal(), second_terminal()]));
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.picker_selected, 1);

        // The second terminal ends and the daemon drops it: an index past the end
        // would be a row nothing opens.
        app.apply(Update::Terminals(vec![terminal()]));
        assert_eq!(app.picker_selected, 0);

        // With none left the question has no answers.
        app.apply(Update::Terminals(Vec::new()));
        assert!(!app.terminal_picker);
    }

    #[test]
    fn a_terminal_takes_the_keyboard_and_ctrl_c_is_an_interrupt() {
        let mut app = with_terminal();
        assert_eq!(app.focus, Focus::Terminal);

        let typing = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(
            app.handle_key(typing),
            Some(Request::TerminalWrite(b"l".to_vec()))
        );

        // Ctrl-C in a shell is SIGINT, not "quit the client": leaving is Ctrl-T.
        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            app.handle_key(interrupt),
            Some(Request::TerminalWrite(vec![0x03]))
        );
        assert!(!app.should_quit);
    }

    #[test]
    fn the_keys_that_are_not_text_are_their_escape_sequences() {
        let key = |code| terminal_bytes(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Enter), Some(b"\r".to_vec()));
        assert_eq!(key(KeyCode::Tab), Some(b"\t".to_vec()));
        assert_eq!(key(KeyCode::Backspace), Some(vec![0x7f]));
        assert_eq!(key(KeyCode::Esc), Some(vec![0x1b]));
        assert_eq!(key(KeyCode::Up), Some(b"\x1b[A".to_vec()));
        assert_eq!(key(KeyCode::Left), Some(b"\x1b[D".to_vec()));
        assert_eq!(key(KeyCode::Delete), Some(b"\x1b[3~".to_vec()));
        assert_eq!(key(KeyCode::F(5)), None, "a key with no byte invents none");
    }

    #[test]
    fn control_chords_are_the_bytes_a_terminal_sends() {
        let chord = |character| {
            terminal_bytes(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::CONTROL,
            ))
        };
        assert_eq!(chord('a'), Some(vec![0x01]));
        assert_eq!(chord('d'), Some(vec![0x04]), "end of input");
        assert_eq!(chord('z'), Some(vec![0x1a]));
        assert_eq!(chord('['), Some(vec![0x1b]), "Ctrl-[ is Escape");
        assert_eq!(chord('1'), None, "no such chord");
    }

    #[test]
    fn the_screen_is_parsed_from_the_bytes_the_daemon_sends() {
        let mut app = with_terminal();
        app.apply(Update::TerminalData(b"make: *** no rule\r\n".to_vec()));
        let screen = app
            .terminal
            .as_ref()
            .expect("the pane is open")
            .parser
            .screen()
            .contents();
        assert!(screen.contains("make: *** no rule"), "{screen:?}");

        app.apply(Update::TerminalExited { code: Some(2) });
        let pane = app
            .terminal
            .as_ref()
            .expect("the pane stays for the last screen");
        assert!(!pane.alive);
        assert_eq!(pane.exit, Some(Some(2)));
        assert!(app.status.contains('2'), "the status says how it ended");
    }

    fn scrolled_terminal() -> App {
        let mut app = with_terminal();
        for line in 0..60 {
            app.apply(Update::TerminalData(
                format!("line {line}\r\n").into_bytes(),
            ));
        }
        app
    }

    #[test]
    fn f2_reads_back_through_the_program_and_escape_returns() {
        let mut app = scrolled_terminal();
        assert_eq!(app.handle_key(key(KeyCode::F(2))), None);
        let pane = app.terminal.as_ref().expect("the pane is open");
        assert_eq!(pane.mode, PaneMode::Copy);
        assert!(
            pane.history() >= 25,
            "the output that scrolled away is still there: {}",
            pane.history()
        );

        // `g` is the top of the history, where the first line printed still is.
        app.handle_key(key(KeyCode::Char('g')));
        let pane = app.terminal.as_ref().expect("the pane is open");
        assert_eq!(pane.reader.view.scroll, pane.history());
        let screen = pane.parser.screen().contents();
        assert!(screen.contains("line 0"), "{screen:?}");

        // `G` is back to the program, still in copy mode.
        app.handle_key(key(KeyCode::Char('G')));
        let pane = app.terminal.as_ref().expect("the pane is open");
        assert_eq!(pane.reader.view.scroll, 0);
        assert_eq!(pane.mode, PaneMode::Copy);

        // Escape gives the keyboard back, and what it types is a byte again.
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(
            app.terminal.as_ref().expect("the pane is open").mode,
            PaneMode::Live
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('l'))),
            Some(Request::TerminalWrite(b"l".to_vec()))
        );
    }

    #[test]
    fn a_selection_is_the_text_between_its_corners() {
        let mut app = with_terminal();
        app.apply(Update::TerminalData(b"abcdef\r\n".to_vec()));
        app.handle_key(key(KeyCode::F(2)));
        // Copy mode starts at the program's cursor, which is on the blank line the
        // prompt waits on; `k` is the line that was printed.
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('v')));
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.take_clipboard().as_deref(), Some("abc"));
    }

    #[test]
    fn yanking_a_line_puts_it_on_the_clipboard() {
        let mut app = with_terminal();
        app.apply(Update::TerminalData(b"echo hi\r\nhi\r\n".to_vec()));
        app.handle_key(key(KeyCode::F(2)));
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.take_clipboard().as_deref(), Some("hi"));
        assert_eq!(
            app.terminal.as_ref().expect("the pane is open").mode,
            PaneMode::Live,
            "a copy ends copy mode, so the shell has the keyboard again"
        );
        assert!(app.status.contains("copied"), "{}", app.status);
    }

    #[test]
    fn output_while_reading_keeps_the_line_the_reader_was_on() {
        let mut app = with_terminal();
        let printed: String = (0..40).map(|index| format!("line {index}\r\n")).collect();
        app.apply(Update::TerminalData(printed.into_bytes()));
        app.handle_key(key(KeyCode::F(2)));
        app.handle_key(key(KeyCode::Char('k')));
        let before = app
            .terminal
            .as_ref()
            .expect("the pane is open")
            .reader
            .view
            .scroll;
        let text = app.terminal.as_mut().expect("the pane is open").yank();

        app.apply(Update::TerminalData(b"more\r\nmore\r\n".to_vec()));

        let pane = app.terminal.as_mut().expect("the pane is open");
        assert_eq!(
            pane.reader.view.scroll,
            before + 2,
            "the two lines that scrolled off joined the history behind the reader"
        );
        assert_eq!(
            pane.yank(),
            text,
            "the line under the cursor is still the line it was"
        );
    }

    #[test]
    fn a_line_selection_yanks_the_whole_lines() {
        let mut app = with_terminal();
        app.apply(Update::TerminalData(b"one\r\ntwo\r\nthree\r\n".to_vec()));
        app.handle_key(key(KeyCode::F(2)));
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Char('k')));
        }
        app.handle_key(key(KeyCode::Char('V')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            app.take_clipboard().as_deref(),
            Some("one\ntwo\nthree"),
            "V marks lines, not the cells between two columns"
        );
    }

    #[test]
    fn a_line_selection_in_the_transcript_takes_rows_not_cells() {
        let mut app = transcript_app();
        app.handle_key(key(KeyCode::F(3)));
        app.handle_key(key(KeyCode::Char('V')));
        assert!(app.transcript.view.line, "V starts a line-wise selection");
        app.handle_key(key(KeyCode::Char('v')));
        assert!(
            !app.transcript.view.line && app.transcript.view.anchor.is_none(),
            "v is character-wise, and drops the line-wise selection"
        );
        app.handle_key(key(KeyCode::Char('V')));
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            app.take_clipboard().as_deref(),
            Some("line 48\nline 49"),
            "the row above the cursor comes with the row it started on"
        );
    }

    #[test]
    fn a_pane_resizes_its_screen_with_the_pane_it_is_drawn_in() {
        let mut app = with_terminal();
        app.set_pane_size(TerminalSize { cols: 40, rows: 10 });
        let (rows, cols) = app
            .terminal
            .as_ref()
            .expect("the pane is open")
            .parser
            .screen()
            .size();
        assert_eq!((cols.get(), rows.get()), (40, 10));
    }

    #[test]
    fn a_slash_finds_what_scrolled_past_and_n_walks_back_through_it() {
        let mut app = scrolled_terminal();
        app.handle_key(key(KeyCode::F(2)));
        assert_eq!(app.handle_key(key(KeyCode::Char('/'))), None);
        assert!(
            app.terminal
                .as_ref()
                .expect("the pane is open")
                .reader
                .typing
                .is_some(),
            "a slash opens a line for the query"
        );
        for character in "line 3".chars() {
            assert_eq!(
                app.handle_key(key(KeyCode::Char(character))),
                None,
                "a query is text for the pane, not a byte for the program"
            );
        }
        app.handle_key(key(KeyCode::Enter));

        let pane = app.terminal.as_ref().expect("the pane is open");
        let find = pane.reader.find.as_ref().expect("the query found matches");
        assert_eq!(
            find.hits.len(),
            11,
            "\"line 3\" and \"line 30\"..\"line 39\""
        );
        assert!(app.status.contains("11 matches"), "{}", app.status);
        // The newest match before where the cursor was reading. It is on the live
        // screen, so the view does not move; the cursor goes to it.
        let line = pane.history() + 8;
        assert_eq!(
            pane.reader.view.cursor.0, line,
            "line 39 is row 8 of the live screen"
        );
        assert!(pane.matched(line, 0), "and the match is under the cursor");

        // `n` is the next one older, and the status says where in the list it is.
        app.handle_key(key(KeyCode::Char('n')));
        let pane = app.terminal.as_ref().expect("the pane is open");
        let line = pane.history() + 7;
        assert_eq!(pane.reader.view.cursor.0, line);
        assert!(pane.matched(line, 0));
        assert!(app.status.contains("match 10/11"), "{}", app.status);

        // `N` goes back the other way, to the newest of the eleven.
        app.handle_key(key(KeyCode::Char('N')));
        let pane = app.terminal.as_ref().expect("the pane is open");
        assert_eq!(pane.reader.find.as_ref().map(|find| find.at), Some(10));

        // A match that is only in the scrollback scrolls the view to it, with the
        // line at the top where the reading starts.
        app.handle_key(key(KeyCode::Char('/')));
        for character in "line 0".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        let pane = app.terminal.as_ref().expect("the pane is open");
        assert_eq!(first_row(pane), "line 0");
        assert_eq!(pane.reader.view.scroll, pane.history());
    }

    /// The top row of the screen the pane is currently parsed at.
    fn first_row(pane: &TerminalPane) -> String {
        let (_, cols) = pane.grid_size();
        pane.parser
            .screen()
            .rows(0, cols)
            .next()
            .unwrap_or_default()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn escape_drops_the_query_and_leaves_copy_mode_on_the_second_press() {
        let mut app = scrolled_terminal();
        app.handle_key(key(KeyCode::F(2)));
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('x')));

        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        let pane = app.terminal.as_ref().expect("the pane is open");
        assert!(pane.reader.typing.is_none(), "the query is dropped");
        assert_eq!(pane.mode, PaneMode::Copy, "the pane is still being read");

        app.handle_key(key(KeyCode::Esc));
        assert_eq!(
            app.terminal.as_ref().expect("the pane is open").mode,
            PaneMode::Live
        );
    }

    #[test]
    fn a_lowercase_query_ignores_case_and_a_capital_does_not() {
        let mut app = with_terminal();
        app.apply(Update::TerminalData(
            b"error: one\r\nError: two\r\nerror: three\r\n".to_vec(),
        ));
        app.handle_key(key(KeyCode::F(2)));

        // Lowercase is a search for the word, whatever case each line used — which
        // is how a log full of `Error:` is actually searched.
        app.handle_key(key(KeyCode::Char('/')));
        for character in "error".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        let pane = app.terminal.as_ref().expect("the pane is open");
        assert_eq!(
            pane.reader.find.as_ref().map(|find| find.hits.len()),
            Some(3)
        );
        assert!(app.status.contains("any case"), "{}", app.status);

        // A capital was typed on purpose, so it means the one line that has it.
        app.handle_key(key(KeyCode::Char('/')));
        for character in "Error".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        let pane = app.terminal.as_ref().expect("the pane is open");
        let find = pane.reader.find.as_ref().expect("a match");
        assert_eq!(find.hits.len(), 1);
        assert_eq!(find.hits[0].line, 1);
        assert!(app.status.contains("exact case"), "{}", app.status);
    }

    #[test]
    fn a_query_that_matches_nothing_leaves_the_view_where_it_was() {
        let mut app = scrolled_terminal();
        app.handle_key(key(KeyCode::F(2)));
        app.handle_key(key(KeyCode::Char('/')));
        for character in "nowhere".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        let pane = app.terminal.as_ref().expect("the pane is open");
        assert!(pane.reader.find.is_none());
        assert_eq!(pane.reader.view.scroll, 0, "no match is no movement");
        assert!(app.status.contains("no match"), "{}", app.status);
    }

    /// A session with a transcript long enough to scroll and a viewport to walk.
    fn transcript_app() -> App {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.entries = (0..50)
            .map(|index| Entry {
                kind: Kind::Assistant,
                text: format!("line {index}"),
                ..Entry::default()
            })
            .collect();
        app.set_viewports(Viewports {
            files: None,
            sessions: None,
            terminal: TerminalSize {
                cols: 100,
                rows: 30,
            },
            transcript: Some(TerminalSize { cols: 40, rows: 10 }),
            dividers: Dividers::default(),
            menu: MenuAreas::default(),
            editor: None,
            terminal_area: None,
        });
        app
    }

    #[test]
    fn f3_reads_the_transcript_with_the_terminals_copy_keys() {
        let mut app = transcript_app();
        assert_eq!(app.handle_key(key(KeyCode::F(3))), None);
        assert_eq!(app.focus, Focus::Transcript, "focus is the mode");
        assert_eq!(
            app.transcript.view.cursor,
            (49, 0),
            "the cursor starts on the newest line"
        );

        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.transcript.view.cursor.0, 48);

        // Twelve lines up is past the top of a ten-line view: the view scrolls so
        // the cursor stays on screen, which is how one set of keys reaches 50.
        for _ in 0..12 {
            app.handle_key(key(KeyCode::Char('k')));
        }
        assert_eq!(app.transcript.view.cursor.0, 36);
        assert_eq!(
            app.transcript.view.scroll, 4,
            "nine rows to the top, four lines further"
        );

        // The key that opened it closes it, back to the session list.
        app.handle_key(key(KeyCode::F(3)));
        assert_eq!(app.focus, Focus::Sessions);
    }

    #[test]
    fn tab_reaches_the_transcript_without_a_shortcut_key() {
        let mut app = transcript_app();
        app.focus = Focus::Sessions;
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Transcript);

        // The pane is read without pressing anything else: `g` moves the view.
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.transcript.view.scroll, 40);

        // Tab moves on, and the view goes back to following the newest output.
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Prompt);
        assert_eq!(app.transcript.view.scroll, 0);
    }

    #[test]
    fn tab_skips_the_transcript_when_there_is_nothing_in_it() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.focus = Focus::Sessions;
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(
            app.focus,
            Focus::Prompt,
            "a pane with no session would be a step that goes nowhere"
        );
    }

    #[test]
    fn escape_from_the_transcript_goes_back_to_the_session_list() {
        let mut app = transcript_app();
        app.handle_key(key(KeyCode::F(3)));
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(app.focus, Focus::Sessions);
        assert_eq!(app.transcript.view.scroll, 0, "and the view is live again");
    }

    #[test]
    fn a_line_yanked_from_the_transcript_goes_to_the_clipboard() {
        let mut app = transcript_app();
        app.handle_key(key(KeyCode::F(3)));
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), None);
        assert_eq!(app.take_clipboard().as_deref(), Some("line 48"));
        assert_eq!(app.focus, Focus::Sessions, "a copy gives the keyboard back");
        assert!(app.status.contains("transcript"), "{}", app.status);
    }

    #[test]
    fn a_tool_call_is_one_row_whose_status_moves() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.apply_event(event(
            1,
            SessionEventKind::ToolCall,
            serde_json::json!({
                "tool_call_id": "call-1",
                "status": "pending",
                "title": "Edit app.rs"
            }),
        ));
        app.apply_event(event(
            2,
            SessionEventKind::ToolCall,
            serde_json::json!({
                "tool_call_id": "call-1",
                "status": "completed",
                "title": "Edit app.rs",
                "content": "wrote 12 lines"
            }),
        ));
        assert_eq!(app.entries.len(), 1, "one call, not one row per update");
        assert_eq!(app.entries[0].text, "[completed] Edit app.rs");
        assert_eq!(
            app.entries[0]
                .tool
                .as_ref()
                .map(|call| call.detail.as_str()),
            Some("wrote 12 lines")
        );
        assert_eq!(app.transcript_rows().len(), 1, "folded by default");
        assert_eq!(
            app.transcript_len(),
            app.transcript_rows().len(),
            "the count is the rows, without building them"
        );
    }

    #[test]
    fn enter_opens_the_tool_call_under_the_transcript_cursor() {
        let mut app = transcript_app();
        app.entries = vec![Entry {
            kind: Kind::Tool,
            text: "[completed] Edit app.rs".to_owned(),
            tool: Some(ToolCall {
                id: "call-1".to_owned(),
                status: "completed".to_owned(),
                title: "Edit app.rs".to_owned(),
                detail: "line one\nline two".to_owned(),
            }),
            folded: true,
        }];
        app.handle_key(key(KeyCode::F(3)));
        assert_eq!(app.transcript.view.cursor, (0, 0));

        app.handle_key(key(KeyCode::Enter));
        assert!(!app.entries[0].folded, "the fold opened");
        assert_eq!(app.transcript_len(), 3, "the header and two result lines");
        assert!(app.status.contains("opened"), "{}", app.status);

        app.handle_key(key(KeyCode::Enter));
        assert!(app.entries[0].folded, "and it closes again");
        assert_eq!(app.transcript_len(), 1);
    }

    #[test]
    fn assistant_text_is_markdown_in_the_rows_a_person_reads() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.push(Kind::Assistant, "## What changed\n**bold** and `code`");
        let rows = app.transcript_rows();
        assert_eq!(rows.len(), 2, "the markdown keeps its rows");
        assert_eq!(rows[0].text(), "What changed");
        assert_eq!(rows[1].text(), "bold and code");
        assert!(
            rows[1]
                .segments
                .iter()
                .any(|segment| segment.emphasis == crate::markdown::Emphasis::Bold),
            "the emphasis is in the row, not added when it is painted"
        );
    }

    #[test]
    fn the_composer_takes_more_than_one_line_and_enter_sends_them_together() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.focus = Focus::Prompt;
        for character in "one".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        for character in "two".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.input, "one\ntwo");
        assert_eq!(app.cursor, 7);
        assert!(app.composer_height() > 3, "the box grew with the text");
        assert_eq!(app.composer_position(), (1, 3));

        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Request::Prompt("one\ntwo".to_owned()))
        );
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn the_caret_edits_where_it_is_rather_than_at_the_end() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.focus = Focus::Prompt;
        for character in "abc".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Char('X')));
        assert_eq!(app.input, "aXbc");
        assert_eq!(app.cursor, 2);
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "abc");
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(
            app.input, "ac",
            "delete takes the character under the caret"
        );
    }

    #[test]
    fn an_at_sign_asks_the_daemon_and_enter_takes_the_file() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.focus = Focus::Prompt;
        app.handle_key(key(KeyCode::Char('@')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            Some(Request::CompleteFiles("a".to_owned())),
            "the walk is the daemon's, like the tree's"
        );
        app.apply(Update::Completions {
            query: "a".to_owned(),
            rows: vec!["src/app.rs".to_owned()],
        });
        assert!(app.completion.is_some());

        // Enter takes the file rather than sending a one-word prompt.
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.input, "@src/app.rs");
        assert_eq!(app.cursor, "@src/app.rs".chars().count());
        assert!(app.completion.is_none());
        assert!(app.entries.is_empty(), "nothing was sent");
    }

    #[test]
    fn a_completion_for_a_word_the_caret_has_left_is_dropped() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        app.focus = Focus::Prompt;
        for character in "@ap".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.apply(Update::Completions {
            query: "a".to_owned(),
            rows: vec!["stale.rs".to_owned()],
        });
        assert!(
            app.completion.as_ref().is_some_and(|c| c.rows.is_empty()),
            "the answer was about `a`, and the word is `ap`"
        );
    }

    #[test]
    fn streaming_is_read_off_the_session_the_daemon_reports() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.sessions = vec![session("s1", "one")];
        open(&mut app);
        assert!(!app.streaming(), "an idle session has no cursor");
        app.sessions[0].status = SessionStatus::Busy;
        assert!(app.streaming());
    }

    #[test]
    fn a_slash_finds_what_the_agent_said_and_n_walks_back() {
        let mut app = transcript_app();
        app.handle_key(key(KeyCode::F(3)));
        assert_eq!(app.handle_key(key(KeyCode::Char('/'))), None);
        assert!(app.transcript.typing.is_some(), "the query has a line");
        for character in "line 4".chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(character))), None);
        }
        app.handle_key(key(KeyCode::Enter));

        let find = app
            .transcript
            .find
            .as_ref()
            .expect("the query found matches");
        assert_eq!(
            find.hits.len(),
            11,
            "\"line 4\" and \"line 40\"..\"line 49\""
        );
        assert!(app.status.contains("11 matches"), "{}", app.status);
        // The newest match before the cursor, which was on line 49 — so the newest
        // match of all, on the last line of the transcript.
        assert_eq!(app.transcript.view.cursor, (49, 0));
        assert!(
            app.matched_transcript(49, 0),
            "the match is under the cursor"
        );

        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.transcript.view.cursor, (48, 0), "line 48, one older");
        assert!(app.status.contains("match 10/11"), "{}", app.status);

        // A match far up the transcript scrolls the view to it: "line 7" is only on
        // line 7, which comes to the top of the view.
        app.handle_key(key(KeyCode::Char('/')));
        for character in "line 7".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("1 match"), "{}", app.status);
        assert_eq!(
            app.transcript.view.scroll, 33,
            "40 lines of history, and line 7 on top"
        );
        assert_eq!(app.transcript.view.cursor, (7, 0));
    }

    #[test]
    fn the_transcripts_find_is_smart_case_too() {
        let mut app = transcript_app();
        app.entries = vec![
            Entry {
                kind: Kind::Assistant,
                text: "error: one".to_owned(),
                ..Entry::default()
            },
            Entry {
                kind: Kind::Assistant,
                text: "Error: two".to_owned(),
                ..Entry::default()
            },
        ];
        app.handle_key(key(KeyCode::F(3)));
        app.handle_key(key(KeyCode::Char('/')));
        for character in "error".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.transcript.find.as_ref().map(|find| find.hits.len()),
            Some(2),
            "both lines, whatever case each of them wrote"
        );

        app.handle_key(key(KeyCode::Char('/')));
        for character in "Error".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.transcript.find.as_ref().map(|find| find.hits.len()),
            Some(1)
        );
        assert!(app.status.contains("exact case"), "{}", app.status);
    }

    #[test]
    fn escape_in_the_transcript_drops_the_query_before_leaving_the_pane() {
        let mut app = transcript_app();
        app.handle_key(key(KeyCode::F(3)));
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('z')));
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(app.focus, Focus::Transcript, "still reading");
        assert!(app.transcript.typing.is_none());

        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Sessions);
    }

    #[test]
    fn a_selection_across_two_transcript_lines_is_both_of_them() {
        let mut app = transcript_app();
        app.handle_key(key(KeyCode::F(3)));
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('v')));
        for _ in 0..20 {
            app.handle_key(key(KeyCode::Char('l')));
        }
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.take_clipboard().as_deref(), Some("line 48\nline 49"));
    }

    #[test]
    fn the_oldest_line_of_the_transcript_is_reachable() {
        let mut app = transcript_app();
        app.handle_key(key(KeyCode::F(3)));
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(
            app.transcript.view.scroll, 40,
            "50 lines in a 10-line view leave 40 behind it"
        );
        // `g` puts the cursor on the oldest line, so there is nothing further up
        // for these keys to find.
        for _ in 0..9 {
            app.handle_key(key(KeyCode::Char('k')));
        }
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.take_clipboard().as_deref(), Some("line 0"));
    }

    #[test]
    fn a_viewport_that_shrinks_keeps_the_copy_cursor_inside_it() {
        let mut app = transcript_app();
        app.handle_key(key(KeyCode::F(3)));
        app.set_viewports(Viewports {
            files: None,
            sessions: None,
            terminal: TerminalSize {
                cols: 100,
                rows: 30,
            },
            transcript: Some(TerminalSize { cols: 40, rows: 5 }),
            dividers: Dividers::default(),
            menu: MenuAreas::default(),
            editor: None,
            terminal_area: None,
        });
        assert_eq!(app.transcript.view.cursor, (49, 0));
        assert_eq!(
            app.transcript.view.scroll, 0,
            "the newest line is still at the bottom of a shorter view"
        );
    }

    #[test]
    fn f6_opens_the_source_pane_and_g_drafts_from_the_open_session() {
        let mut app = App::demo(&acpilot_demo::demo());

        assert_eq!(app.handle_key(key(KeyCode::F(6))), None);
        assert!(app.source.is_some(), "f6 opens source control");
        assert_eq!(
            app.source.as_ref().map(SourceControl::staged),
            Some(1),
            "the fixture's change is staged"
        );

        assert_eq!(
            app.handle_key(key(KeyCode::Char('g'))),
            Some(Request::GenerateCommit {
                session_id: "s1".to_owned()
            }),
            "g opens the composer and asks the session that is open"
        );
        assert!(app.commit.is_some_and(|commit| commit.generating));
    }

    #[test]
    fn ctrl_s_asks_before_a_commit_and_y_sends_the_edited_message() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(key(KeyCode::F(6)));
        app.handle_key(key(KeyCode::Char('c')));
        for character in "fix: keep the commit confirmed".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }

        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(ctrl_s), None);
        assert!(
            app.commit.as_ref().is_some_and(|commit| commit.confirming),
            "ctrl-s asks, it does not write"
        );

        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Request::Commit {
                message: "fix: keep the commit confirmed".to_owned()
            }),
            "the second key is the one that writes"
        );
        assert!(app.commit.as_ref().is_some_and(|commit| commit.committing));

        app.apply(Update::Committed(acpilot_proto::git::GitCommit {
            id: "0123456789abcdef".to_owned(),
            parents: vec!["parent".to_owned()],
            author: "Test User".to_owned(),
            email: "test@example.com".to_owned(),
            authored_at: "2026-11-07T12:00:00Z".to_owned(),
            subject: "fix: keep the commit confirmed".to_owned(),
        }));
        assert!(app.commit.is_none(), "the composer closes on the answer");
        assert!(app.status.contains("0123456"), "{}", app.status);
        assert_eq!(
            app.pending,
            vec![Request::Git],
            "the pane is asked for the new index"
        );
    }

    #[test]
    fn a_commit_draft_lands_in_the_composer_for_editing() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(key(KeyCode::F(6)));
        app.handle_key(key(KeyCode::Char('g')));

        app.apply(Update::CommitDraft(acpilot_proto::git::CommitDraft {
            message: "feat: draft\n\nbody".to_owned(),
            subject: "feat: draft".to_owned(),
            body: "body".to_owned(),
            kind: "feat".to_owned(),
            scope: None,
            breaking: false,
            emoji: None,
            generator: "session:s1".to_owned(),
        }));

        let commit = app.commit.as_ref().expect("the composer is up");
        assert_eq!(commit.text, "feat: draft\n\nbody");
        assert_eq!(commit.cursor, commit.text.chars().count());
        assert!(!commit.generating);
        assert!(app.status.contains("commit draft"), "{}", app.status);
    }

    #[test]
    fn f3_says_where_to_go_when_a_terminal_is_open() {
        let mut app = with_terminal();
        assert_eq!(app.handle_key(key(KeyCode::F(3))), None);
        assert_eq!(
            app.focus,
            Focus::Terminal,
            "the terminal covers the transcript"
        );
        assert!(app.status.contains("ctrl-t"), "{}", app.status);
    }
}
