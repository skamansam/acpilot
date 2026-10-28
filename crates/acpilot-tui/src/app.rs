//! The TUI's state: what is on screen, and what a key press means.
//!
//! There is no domain logic here. Sessions, permissions and events arrive from
//! the daemon; this module only decides how they read and which request a
//! keystroke turns into. Keeping that boundary sharp is what lets the same app
//! state be driven by a test with no daemon and no terminal.

use acpilot_demo::{Command, CommandAction, Diff, FileNode, Integration, Run};
use acpilot_proto::agent::AgentSummary;
use acpilot_proto::event::{SessionEvent, SessionEventKind};
use acpilot_proto::permission::{PermissionOption, PermissionRequest};
use acpilot_proto::session::SessionSummary;
use acpilot_proto::terminal::{TerminalSize, TerminalSummary};
use acpilot_text::highlight::Grammars;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::backend::{Request, Update};
use crate::editor::EditorPane;
use crate::theme::Theme;

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
}

impl Focus {
    /// The label the status line shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Editor => "editor",
            Self::Sessions => "sessions",
            Self::Transcript => "transcript",
            Self::Prompt => "prompt",
            Self::Terminal => "terminal",
        }
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
    /// Start a session with the first agent.
    NewSession,
    /// Cancel the turn in flight.
    Cancel,
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
                label: "Files",
                keys: "",
                action: MenuAction::Files,
            },
            MenuRow {
                label: "Editor",
                keys: "",
                action: MenuAction::Editor,
            },
            MenuRow {
                label: "Sessions",
                keys: "",
                action: MenuAction::Sessions,
            },
            MenuRow {
                label: "Transcript",
                keys: "f3",
                action: MenuAction::Transcript,
            },
            MenuRow {
                label: "Terminal Pane",
                keys: "ctrl-t",
                action: MenuAction::Terminal,
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
}

/// The tree's column, in cells, when nobody has dragged the divider.
const FILES_WIDTH: u16 = 26;

/// The sidebar's column — sessions, transcript, composer — when nobody has.
const SIDEBAR_WIDTH: u16 = 44;

/// How many lines of history a pane keeps behind its live screen.
///
/// A pane is where a build prints a thousand lines and the one that matters
/// scrolled past; the visible screen alone is what sends a person back to their
/// own terminal. Ten thousand lines is a few megabytes of grid and more than a
/// session's worth of output.
const SCROLLBACK: usize = 10_000;

/// What the status line says when a pane starts being read.
const COPY_HINT: &str = "copy mode · j/k scroll · / find · v select · y copy · Esc leaves";

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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyView {
    /// Where the cursor is, in cells of the view.
    pub cursor: (u16, u16),
    /// Where a selection started, once `v` has been pressed.
    pub anchor: Option<(u16, u16)>,
    /// How many lines behind the live view the pane sits. Zero is live.
    pub scroll: usize,
}

impl CopyView {
    /// A cursor standing still on the live view.
    fn new(cursor: (u16, u16)) -> Self {
        Self {
            cursor,
            anchor: None,
            scroll: 0,
        }
    }

    /// The selection, in reading order: from the earlier cell to the later one.
    fn selection(&self) -> Option<((u16, u16), (u16, u16))> {
        let anchor = self.anchor?;
        let cursor = self.cursor;
        Some(if (anchor.0, anchor.1) <= (cursor.0, cursor.1) {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        })
    }

    /// Whether a cell is inside the selection.
    #[must_use]
    pub fn selected(&self, row: u16, col: u16) -> bool {
        self.selection()
            .is_some_and(|(start, end)| (row, col) >= start && (row, col) <= end)
    }

    /// Start a selection at the cursor, or drop the one in progress.
    fn toggle_anchor(&mut self) {
        self.anchor = match self.anchor {
            Some(_) => None,
            None => Some(self.cursor),
        };
    }

    /// Move the cursor in a `grid` whose history is `max_scroll` lines deep.
    ///
    /// Scrolling at the edges is what lets one set of keys reach the whole of a
    /// pane's history: a cursor that could walk off the screen would be a cursor
    /// nobody can see.
    fn move_by(&mut self, rows: i32, cols: i32, grid: (u16, u16), max_scroll: usize) {
        let (height, width) = grid;
        let last_row = height.saturating_sub(1);
        let last_col = width.saturating_sub(1);
        let (mut row, mut col) = self.cursor;
        let step = |delta: i32| u16::try_from(delta.unsigned_abs()).unwrap_or(u16::MAX);

        if cols < 0 {
            col = col.saturating_sub(step(cols));
        } else {
            col = col.saturating_add(step(cols)).min(last_col);
        }

        if rows < 0 {
            for _ in 0..rows.unsigned_abs() {
                if row > 0 {
                    row -= 1;
                } else if self.scroll < max_scroll {
                    self.scroll += 1;
                } else {
                    break;
                }
            }
        } else {
            for _ in 0..rows.unsigned_abs() {
                if row < last_row {
                    row += 1;
                } else if self.scroll > 0 {
                    self.scroll -= 1;
                } else {
                    break;
                }
            }
        }

        self.cursor = (row, col);
        self.clamp(grid, max_scroll);
    }

    /// The line the cursor is on, counted from the oldest line the pane keeps.
    ///
    /// The view is a window: the first row on screen is `max_scroll - scroll` lines
    /// into the history, and the cursor is `row` rows below that.
    fn line(&self, max_scroll: usize) -> usize {
        max_scroll.saturating_sub(self.scroll) + usize::from(self.cursor.0)
    }

    /// Put the view on `line`, with the line at the top and the cursor at `col`.
    ///
    /// A match at the bottom of a full screen is a match with three lines of
    /// context and no more; the top of the view is where a person looks after
    /// asking for something.
    fn jump_to(&mut self, line: usize, col: u16, max_scroll: usize, grid: (u16, u16)) {
        self.scroll = max_scroll.saturating_sub(line);
        let first = max_scroll - self.scroll;
        self.cursor = (
            u16::try_from(line.saturating_sub(first))
                .unwrap_or(u16::MAX)
                .min(grid.0.saturating_sub(1)),
            col.min(grid.1.saturating_sub(1)),
        );
    }

    /// Keep the cursor, the selection and the view inside a grid that changed
    /// shape, or a history that turned out shorter than the view.
    fn clamp(&mut self, grid: (u16, u16), max_scroll: usize) {
        self.scroll = self.scroll.min(max_scroll);
        let last_row = grid.0.saturating_sub(1);
        let last_col = grid.1.saturating_sub(1);
        self.cursor = (self.cursor.0.min(last_row), self.cursor.1.min(last_col));
        if let Some(anchor) = self.anchor {
            self.anchor = Some((anchor.0.min(last_row), anchor.1.min(last_col)));
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
    fn new(cursor: (u16, u16)) -> Self {
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
        let cursor = self.view.line(max_scroll);
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
    fn matched(&self, row: u16, col: u16, max_scroll: usize) -> bool {
        let Some(hit) = self.find.as_ref().and_then(|find| find.hits.get(find.at)) else {
            return false;
        };
        let first = max_scroll.saturating_sub(self.view.scroll.min(max_scroll));
        let hit_row = hit.line as i64 - first as i64;
        hit_row == i64::from(row) && col >= hit.col && col < hit.col.saturating_add(hit.len)
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
            reader: Reader::new((rows.saturating_sub(1), 0)),
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
            row.min(rows.saturating_sub(1)),
            col.min(cols.saturating_sub(1)),
        ));
        self.apply_scroll();
    }

    /// Back to following the program.
    pub fn end_copy(&mut self) {
        self.mode = PaneMode::Live;
        self.reader.view = CopyView::new(self.reader.view.cursor);
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
        self.set_scroll(self.scrollable());
    }

    /// Back to the live screen, without leaving copy mode.
    fn scroll_to_live(&mut self) {
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

    /// Back to the newest line, which is where answering a program puts the view.
    ///
    /// Reading behind a program is not a mode here, so the first keystroke is also
    /// the one that ends it: a command typed into a scrolled pane would otherwise
    /// print its answer out of sight.
    pub fn follow(&mut self) {
        self.set_scroll(0);
    }

    /// The text under the selection, or the cursor's line when nothing is
    /// selected.
    ///
    /// The screen's own reader does the work, at the scroll offset the view is
    /// drawn at, so a copy takes the cells that are highlighted rather than the
    /// live screen behind them.
    #[must_use]
    pub fn yank(&self) -> String {
        let (_, width) = self.grid_size();
        let screen = self.parser.screen();
        let (start, end) = match self.reader.view.selection() {
            Some(selection) => selection,
            None => {
                let (row, _) = self.reader.view.cursor;
                ((row, 0), (row, width.saturating_sub(1)))
            }
        };
        // `contents_between` stops before `end_col`, and the cell under the cursor
        // is part of what a person selected, so it asks for one past it. Trailing
        // blanks are padding, not text: a line yanked from a terminal is pasted
        // into a prompt more often than into a file.
        let end_col = end.1.saturating_add(1).min(width);
        screen
            .contents_between(start.0, start.1, end.0, end_col)
            .trim_end()
            .to_owned()
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
    pub fn matched(&self, row: u16, col: u16) -> bool {
        self.reader.matched(row, col, self.history())
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
    /// Where the terminal pane was drawn, when one is up.
    ///
    /// Not to be confused with `terminal`, which is the grid the program was told
    /// it has: this is where that grid landed on the window.
    pub terminal_area: Option<PaneArea>,
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
    /// The workspace tree, flattened. Empty until P6-6 reads the filesystem; the
    /// design pass seeds it.
    pub files: Vec<FileNode>,
    /// Which row of the tree is selected.
    pub file_selected: usize,
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
    /// The diff review, when one is up.
    pub diff: Option<Diff>,
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
    /// The stats bar's rows.
    pub integrations: Vec<Integration>,
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
    /// The prompt being composed.
    pub input: String,
    /// Terminals the daemon has for the open session, oldest first.
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
    /// Where the terminal pane was last drawn, for the mouse.
    pub terminal_area: Option<PaneArea>,
    /// Where the dividers were last drawn, for the mouse.
    pub dividers: Dividers,
    /// Which divider the mouse is holding, if any.
    pub drag: Option<Divider>,
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
            editor: None,
            grammars: Grammars::new(),
            diff: None,
            palette: None,
            menu: None,
            menu_areas: MenuAreas::default(),
            theme: Theme::default(),
            theme_from_daemon: false,
            about: false,
            which_key: false,
            files_width: FILES_WIDTH,
            sidebar_width: SIDEBAR_WIDTH,
            toast: None,
            integrations: Vec::new(),
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
            input: String::new(),
            terminals: Vec::new(),
            terminal: None,
            transcript: Reader::default(),
            transcript_size: None,
            terminal_picker: false,
            picker_selected: 0,
            clipboard: None,
            editor_area: None,
            terminal_area: None,
            dividers: Dividers::default(),
            drag: None,
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
        app.integrations = demo.integrations.clone();
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
    pub fn transcript_lines(&self) -> Vec<(&str, Kind)> {
        self.entries
            .iter()
            .flat_map(|entry| entry.text.split('\n').map(move |line| (line, entry.kind)))
            .collect()
    }

    /// How many screen lines the transcript holds, without building them.
    ///
    /// A key press needs the length to clamp a scroll, not the lines; counting
    /// newlines is the same answer without the allocation.
    fn transcript_len(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| entry.text.matches('\n').count() + 1)
            .sum()
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
        self.transcript_lines()
            .into_iter()
            .map(|(line, _)| line.to_owned())
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
        let grid = self.transcript_grid();
        let last = u16::try_from(self.transcript_len().saturating_sub(1)).unwrap_or(u16::MAX);
        self.transcript = Reader::new((last.min(grid.0.saturating_sub(1)), 0));
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
            CopyKey::Move { rows, cols } => view.move_by(rows, cols, grid, max_scroll),
            CopyKey::Top => view.scroll = max_scroll,
            CopyKey::Live => view.scroll = 0,
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
    pub fn matched_transcript(&self, row: u16, col: u16) -> bool {
        let grid = self.transcript_grid();
        let max_scroll = self.transcript_len().saturating_sub(usize::from(grid.0));
        self.transcript.matched(row, col, max_scroll)
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
        let lines = self.transcript_lines();
        let height = usize::from(self.transcript_size.map_or(24, |size| size.rows.max(1)));
        // The selection is in cells of the view, and the view is a window onto the
        // lines: the first row on screen is this far into them.
        let first = lines
            .len()
            .saturating_sub(height)
            .saturating_sub(view.scroll);
        let (start, end) = match view.selection() {
            Some(selection) => selection,
            None => {
                let row = view.cursor.0;
                ((row, 0), (row, u16::MAX))
            }
        };
        let mut text = String::new();
        for row in start.0..=end.0 {
            let Some((line, _)) = lines.get(first + usize::from(row)) else {
                break;
            };
            let chars: Vec<char> = line.chars().collect();
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
                    pane.parser.process(&bytes);
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
            Update::Event(event) => self.apply_event(event),
            Update::Permissions(pending) => {
                let was_empty = self.permissions.is_empty();
                self.permissions = pending;
                // A card appears when work arrives, not when the list is merely
                // refreshed under one that is already up.
                if was_empty && !self.permissions.is_empty() {
                    self.permission_card = true;
                }
                if self.permissions.is_empty() {
                    self.permission_card = false;
                }
            }
            Update::Prompted {
                first_seq, turn_id, ..
            } => {
                self.status = format!("turn {turn_id} accepted at {first_seq}");
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
                let title = payload["title"]
                    .as_str()
                    .or_else(|| payload["tool_call_id"].as_str())
                    .unwrap_or("tool call");
                self.push(Kind::Tool, &format!("[{status}] {title}"));
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
    pub fn push(&mut self, kind: Kind, text: &str) {
        match self.entries.last_mut() {
            Some(last) if last.kind == kind => last.text.push_str(text),
            _ => self.entries.push(Entry {
                kind,
                text: text.to_owned(),
            }),
        }
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.toggle_palette();
            return None;
        }
        if self.palette.is_some() {
            return self.palette_key(key);
        }

        // The menu bar. `F9` is the key terminals have kept for menus since before
        // windows, and while the bar is up it owns the keyboard: a row drawn over a
        // pane is still the menu's key.
        if key.code == KeyCode::F(9) {
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
            if key.code == KeyCode::Char('?') {
                return None;
            }
        }

        // Ctrl-T is the way in and out of the terminal pane, and it outranks the
        // permission card: a shell that could not be left because an agent was
        // waiting would be a pane that traps the session.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            return self.toggle_terminal();
        }

        // The picker is modal in the same way the permission card is: it is a
        // question with a list of answers, and a keystroke aimed at a program that
        // is not on screen yet would be lost.
        if self.terminal_picker {
            return self.picker_key(key);
        }

        // A pane being read takes the keys: scrolling, selecting and yanking are
        // the pane's, not the program's. `?` is the exception — it is the key that
        // lists them, and in a byte-for-byte pane it would otherwise be swallowed.
        if self
            .terminal
            .as_ref()
            .is_some_and(|pane| pane.mode == PaneMode::Copy)
        {
            if key.code == KeyCode::Char('?') {
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

        // F3 is the transcript's way in, the sibling of the terminal's F2, and the
        // way back out: a function key on purpose, so no pane whose keys are text
        // has to give one up. `Tab` reaches the same pane without it.
        if key.code == KeyCode::F(3) {
            if self.focus == Focus::Transcript {
                self.set_focus(Focus::Sessions);
            } else {
                self.focus_transcript();
            }
            return None;
        }

        // The divider keys. A chord rather than a mode, because a layout is adjusted
        // once and then read; the pane with the keyboard is the one that changes.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Left => {
                    self.resize(false);
                    return None;
                }
                KeyCode::Right => {
                    self.resize(true);
                    return None;
                }
                _ => {}
            }
        }

        // The chords the client owns. Anything else with a Ctrl in it falls through
        // to the focused pane, because a pane has chords of its own — Ctrl-S in the
        // editor, Ctrl-F in a terminal's copy mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('q') => {
                    self.should_quit = true;
                    return Some(Request::Quit);
                }
                KeyCode::Char('r') => return Some(Request::Refresh),
                _ => {}
            }
        }

        if self.permission_card && !self.permissions.is_empty() {
            let option = match key.code {
                KeyCode::Char('y') => Some(PermissionOption::AllowOnce),
                KeyCode::Char('a') => Some(PermissionOption::AllowAlways),
                KeyCode::Char('n') => Some(PermissionOption::Deny),
                KeyCode::Esc => {
                    self.permission_card = false;
                    return None;
                }
                _ => None,
            };
            if let Some(option) = option {
                let id = self.permissions[0].id.clone();
                return Some(Request::Resolve { id, option });
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

        // Global from here down: the pane cycle and the key list. Everything else
        // belongs to the pane that has the keyboard. `?` is a question mark in the
        // composer, though — it is the one pane whose keys are text.
        if key.code == KeyCode::Char('?') && self.focus != Focus::Prompt {
            self.which_key = true;
            return None;
        }
        if key.code == KeyCode::Tab {
            // Tab never lands in the terminal: a shell wants the byte, so the pane
            // is entered and left with Ctrl-T and the cycle skips it. The transcript
            // is skipped too when there is nothing in it or a terminal covers it,
            // rather than being a step that goes nowhere.
            let next = match self.focus {
                Focus::Files => Focus::Editor,
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
    /// Movement is local: a tree that can be walked is part of the design even
    /// before the filesystem is read, and the rows here are the seeded ones.
    fn files_key(&mut self, key: KeyEvent) -> Option<Request> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.file_selected = self.file_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.file_selected + 1 < self.files.len() {
                    self.file_selected += 1;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => self.set_expanded(false),
            KeyCode::Right | KeyCode::Char('l') => self.set_expanded(true),
            KeyCode::Enter => {
                let Some(node) = self.files.get(self.file_selected) else {
                    self.planned("P6-6", "the file tree");
                    return None;
                };
                if node.dir {
                    let expanded = !node.expanded;
                    self.set_expanded(expanded);
                } else {
                    let label = node.label().to_owned();
                    self.planned("P6-6", format!("open {label} in the editor"));
                }
            }
            KeyCode::Char('/') => self.planned("P6-6", "fuzzy file open"),
            _ => {}
        }
        None
    }

    /// Open or close the directory the tree's selection is on.
    fn set_expanded(&mut self, expanded: bool) {
        if let Some(node) = self.files.get_mut(self.file_selected)
            && node.dir
        {
            node.expanded = expanded;
        }
    }

    /// The editor's keys, over the document the pane holds.
    ///
    /// A prompt takes every key while it is up, because a find box is text and
    /// nothing else; `Enter` ends it and `Esc` takes back what it did. The rest is
    /// the pane's: movement with no goal column (that is a key sequence's
    /// business, P6-5), `/` and `Ctrl-F` for a search that runs as it is typed,
    /// `Ctrl-G` for a line, `n`/`N` to walk the matches.
    ///
    /// What is not here is not here on purpose: there is nothing to edit until
    /// insert mode (P6-5), and a save goes through the daemon's files API rather
    /// than writing the file from a UI (P7-10 serialises writes per file).
    fn editor_key(&mut self, key: KeyEvent) -> Option<Request> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(editor) = self.editor.as_mut() else {
            self.planned("P6-6", "opening a file from the tree");
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
            let message = editor.message().to_owned();
            self.status = message;
            return None;
        }

        let mut planned: Option<(&'static str, &'static str)> = None;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => editor.move_caret(-1, 0),
            KeyCode::Down | KeyCode::Char('j') => editor.move_caret(1, 0),
            KeyCode::Left | KeyCode::Char('h') => editor.move_caret(0, -1),
            KeyCode::Right | KeyCode::Char('l') => editor.move_caret(0, 1),
            KeyCode::Home | KeyCode::Char('0') => editor.line_edge(false),
            KeyCode::End | KeyCode::Char('$') => editor.line_edge(true),
            KeyCode::PageUp => editor.page(false),
            KeyCode::PageDown => editor.page(true),
            // `/` is the search a terminal editor has always had; `Ctrl-F` is the
            // same box for the people who learned it somewhere else.
            KeyCode::Char('/') => editor.search(),
            KeyCode::Char('f') if control => editor.search(),
            KeyCode::Char('g') if control => editor.go_to_line(),
            KeyCode::Char('n') => editor.step_match(true),
            KeyCode::Char('N') => editor.step_match(false),
            KeyCode::Char('s') if control => {
                planned = Some(("P7-10", "saving through the daemon"));
            }
            KeyCode::Char('i') => planned = Some(("P6-5", "insert mode")),
            KeyCode::F(12) => planned = Some(("P7-4", "go to definition")),
            _ => {}
        }
        let message = editor.message().to_owned();
        if !message.is_empty() {
            self.status = message;
        }
        if let Some((task, feature)) = planned {
            self.planned(task, feature);
        }
        None
    }

    /// The session list's keys.
    fn sessions_key(&mut self, key: KeyEvent) -> Option<Request> {
        match key.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                    self.chose = true;
                    return self.open_selected();
                }
            }
            KeyCode::Down => {
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

    /// Put the caret where a click in the editor's text landed.
    ///
    /// A click is a focus move as well as a caret: a pane you can click into is one
    /// you expect to be typing in. The numbers and the frame are not text, so a
    /// click on them is not a caret — it falls through to the divider drag, which
    /// is what a click on a pane's edge is for.
    fn click_editor(&mut self, column: u16, row: u16) -> bool {
        let Some(area) = self.editor_area else {
            return false;
        };
        if !area.is_text(column, row) {
            return false;
        }
        self.set_focus(Focus::Editor);
        if let Some(editor) = self.editor.as_mut() {
            editor.click(
                usize::from(row - area.pane.y),
                usize::from(column - area.pane.x - area.gutter),
            );
        }
        true
    }

    /// Scroll the pane under the wheel: `up` is towards the start of what it shows.
    ///
    /// A wheel is pointed rather than aimed, so it moves no caret, no copy cursor
    /// and no focus — it scrolls whatever it is over, and it does not have to be
    /// over the pane holding the keyboard. The editor and the terminal are the two
    /// panes with something to scroll; over anything else it is a no-op rather than
    /// a guess.
    fn scroll_under_pointer(&mut self, column: u16, row: u16, up: i32) {
        if self.scroll_editor(column, row, -up) {
            return;
        }
        self.scroll_terminal(column, row, up);
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
    fn prompt_key(&mut self, key: KeyEvent) -> Option<Request> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => {
                let text = self.input.trim().to_owned();
                if text.is_empty() {
                    return None;
                }
                self.input.clear();
                // A local echo: the daemon sends the user chunk too, and the
                // de-duplication by sequence keeps this from doubling it.
                self.push(Kind::User, &format!("{text}\n"));
                Some(Request::Prompt(text))
            }
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::Esc => {
                self.set_focus(Focus::Sessions);
                None
            }
            KeyCode::Char(character) if !control => {
                self.input.push(character);
                None
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
            MenuAction::Refresh => return Some(Request::Refresh),
            MenuAction::Files => self.set_focus(Focus::Files),
            MenuAction::Editor => self.set_focus(Focus::Editor),
            MenuAction::Sessions => self.set_focus(Focus::Sessions),
            MenuAction::Transcript => self.focus_transcript(),
            MenuAction::Terminal => return self.toggle_terminal(),
            MenuAction::NewSession => {
                return self
                    .agents
                    .first()
                    .map(|agent| Request::New(agent.id.clone()));
            }
            MenuAction::Cancel => return Some(Request::Cancel),
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
                    (self.files_width + STEP).min(60)
                } else {
                    self.files_width.saturating_sub(STEP).max(16)
                };
            }
            _ => {
                self.sidebar_width = if grow {
                    (self.sidebar_width + STEP).min(80)
                } else {
                    self.sidebar_width.saturating_sub(STEP).max(24)
                };
            }
        }
        self.status = format!(
            "panes · tree {} · sidebar {} columns",
            self.files_width, self.sidebar_width
        );
    }

    /// What a mouse event means: a menu, or a drag on a divider.
    ///
    /// Those are the two things a pointer is better at than a key: a menu bar with a
    /// list is a target a person can see and hit, and a width has no key that can aim.
    /// A click on a pane is still not a focus change — every command in this interface
    /// has a key, and a mouse that could move the keyboard would be a second way to
    /// get lost.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> Option<Request> {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(request) = self.click_menu(event.column, event.row) {
                    return request;
                }
                if self.click_editor(event.column, event.row) {
                    return None;
                }
                self.drag = self.divider_at(event.column);
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
                if let Some(divider) = self.drag {
                    self.drag_to(divider, event.column);
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
                self.drag = None;
                None
            }
            _ => None,
        }
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
                    .clamp(16, 60);
            }
            Divider::Sidebar => {
                // The sidebar's width is everything from the drag column to the right
                // edge of the body.
                self.sidebar_width = self.dividers.right.saturating_sub(column).clamp(24, 80);
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
                    None => self.planned("P6-6", "opening a file from the tree"),
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
                    None => self.planned("P6-6", "opening a file from the tree"),
                }
                None
            }
            Run::Refresh => Some(Request::Refresh),
            Run::WhichKey => {
                self.which_key = true;
                None
            }
            Run::Palette => {
                self.palette = Some(Palette::default());
                None
            }
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

    /// Open a terminal for the open session, or leave the one that is open.
    ///
    /// One terminal is the common case and attaches straight away. Several ask
    /// which: an agent may open a shell per command, and silently opening the
    /// first would be a pane that hides the rest. None has nothing to attach to,
    /// and the status line says so rather than opening an empty pane.
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
                self.status =
                    "this session has no terminal — an agent opens one with its terminal tool"
                        .to_owned();
                None
            }
            1 => {
                let id = self.terminals[0].id.clone();
                self.status = "attaching to a terminal…".to_owned();
                Some(Request::OpenTerminal { id })
            }
            count => {
                self.terminal_picker = true;
                self.picker_selected = self.picker_selected.min(count - 1);
                self.status = format!("{count} terminals · Enter opens · Esc closes");
                None
            }
        }
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
fn text_of(content: &serde_json::Value) -> Option<&str> {
    content
        .as_str()
        .or_else(|| content.get("text").and_then(serde_json::Value::as_str))
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

        for character in "review".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.matches().len(), 1, "the query filters the list");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.palette.is_none(), "running a command closes it");
        let toast = app.toast.as_ref().expect("a planned command says so");
        assert_eq!(toast.task, "P6-9");
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
        for character in "review".chars() {
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
    fn the_tree_moves_and_a_file_names_the_task_that_would_open_it() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.files = acpilot_demo::tree::files();
        app.focus = Focus::Files;

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.file_selected, 1);

        // The second row is `crates`, a directory: opening it is local, not
        // planned, because the rows are here and the toggle is the pane's.
        let before = app.files[1].expanded;
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.files[1].expanded, !before);

        // A file needs the tree to read the filesystem, which is P6-6.
        app.file_selected = 0;
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.toast.as_ref().map(|toast| toast.task), Some("P6-6"));
    }

    #[test]
    fn the_editor_moves_its_caret_and_saving_names_the_task_that_would_write() {
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

        // A save goes through the daemon's files API; a UI that wrote the file
        // itself would be the second writer the daemon cannot serialise.
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        app.handle_key(save);
        assert_eq!(app.toast.as_ref().map(|toast| toast.task), Some("P7-10"));
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
        // closing what is open.
        app.handle_key(alt('h'));
        assert_eq!(
            app.menu,
            Some(MenuState {
                open: 3,
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
        assert_eq!(app.drag, Some(Divider::Tree));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 39));
        assert_eq!(app.files_width, 40, "40 cells, so the border is at 39");
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 39));
        assert!(app.drag.is_none(), "letting go lets go");

        // The sidebar is measured from the body's right edge.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 95));
        assert_eq!(app.drag, Some(Divider::Sidebar));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 60));
        assert_eq!(app.sidebar_width, 80, "140 - 60, clamped to the widest");

        // A click on a pane is not a drag and not a focus change.
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 60));
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 70));
        assert!(app.drag.is_none());
        assert_eq!(app.focus, Focus::Sessions);
    }

    #[test]
    fn a_drag_cannot_make_a_pane_unusable() {
        let mut app = App::new("http://127.0.0.1:7777");
        app.set_viewports(Viewports {
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
        assert!(app.drag.is_none(), "the pane is not its own divider");
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
            session_id: "s1".to_owned(),
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
            session_id: "s1".to_owned(),
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
    fn a_session_without_a_terminal_says_where_terminals_come_from() {
        let mut app = App::new("http://127.0.0.1:7777");
        open(&mut app);
        let toggle = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(toggle), None);
        assert!(app.terminal.is_none());
        assert!(
            app.status.contains("agent opens one"),
            "the status says why there is nothing: {}",
            app.status
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
        assert_eq!(
            pane.reader.view.cursor.0, 8,
            "line 39 is row 8 of the live screen"
        );
        assert!(pane.matched(8, 0), "and the match is under the cursor");

        // `n` is the next one older, and the status says where in the list it is.
        app.handle_key(key(KeyCode::Char('n')));
        let pane = app.terminal.as_ref().expect("the pane is open");
        assert_eq!(pane.reader.view.cursor.0, 7);
        assert!(pane.matched(7, 0));
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
            })
            .collect();
        app.set_viewports(Viewports {
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
            (9, 0),
            "the cursor starts on the newest line"
        );

        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.transcript.view.cursor.0, 8);

        // Twelve lines up is past the top of a ten-line view: the view scrolls so
        // the cursor stays on screen, which is how one set of keys reaches 50.
        for _ in 0..12 {
            app.handle_key(key(KeyCode::Char('k')));
        }
        assert_eq!(app.transcript.view.cursor.0, 0);
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
        assert_eq!(app.transcript.view.cursor, (9, 0));
        assert!(
            app.matched_transcript(9, 0),
            "the match is under the cursor"
        );

        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.transcript.view.cursor, (8, 0), "line 48, one older");
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
        assert_eq!(app.transcript.view.cursor, (0, 0));
    }

    #[test]
    fn the_transcripts_find_is_smart_case_too() {
        let mut app = transcript_app();
        app.entries = vec![
            Entry {
                kind: Kind::Assistant,
                text: "error: one".to_owned(),
            },
            Entry {
                kind: Kind::Assistant,
                text: "Error: two".to_owned(),
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
        // `g` moves the view, not the cursor: the top row is the oldest line
        // once the cursor has walked up to it.
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
        assert_eq!(app.transcript.view.cursor, (4, 0));
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
