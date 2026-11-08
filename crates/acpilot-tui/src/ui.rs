//! Drawing. Everything here reads state and paints; nothing here calls anything.
//!
//! The layout is the smallest thing that can hold the product: sessions on the
//! left, the transcript on the right, the composer under it, one status line, and
//! a permission card that covers the middle when an agent is waiting. Themes are
//! data (the mod-shop phase's P4.6-6) and do not exist yet, so the few colours
//! here are fixed rather than a second, private palette that would have to be
//! replaced later.

use acpilot_demo::{Diff, DiffKind};
use acpilot_proto::permission::PermissionRequest;
use acpilot_proto::session::SessionStatus;
use acpilot_proto::terminal::TerminalSize;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::app::{
    App, Dividers, EditorArea, Focus, Kind, MENUS, MenuAreas, MenuList, MenuState, PaneArea,
    PaneMode, Panel, Side, TerminalPane, Toast, Viewports, about_lines, first_visible, menu_title,
};
use crate::keymap::Context;

/// How wide the outline's column is.
///
/// Narrower than the tree and fixed: a definition's name and its kind are short, and a
/// column that could be dragged would be a third divider for a panel that is mostly
/// read rather than browsed.
const OUTLINE_WIDTH: u16 = 30;

/// How wide the references list's column is.
///
/// Wider than the outline because a row is a path and a line of code rather than a
/// name: the same reason the content search's rows carry their text.
const REFERENCES_WIDTH: u16 = 44;

/// How wide the project map's column is.
///
/// The references list's width, for the same reason: a row is a name and a path, and
/// the summary under the area the caret's file is in needs room to be a sentence.
const PROJECT_MAP_WIDTH: u16 = 44;
use crate::editor::{PromptKind, State};
use crate::theme::Theme;

/// Paint one frame.
///
/// Returns the sizes the panes were drawn at, which is how the event loop learns
/// what to tell a program and what a copy cursor can walk: the pane's inner cells,
/// not the window's, because a shell that thinks it is 80 columns wide inside a
/// 78-column box wraps every long command.
pub fn draw(frame: &mut Frame, app: &mut App) -> Viewports {
    let area = frame.area();
    // A bar of chrome, the panes, then two footer rows: the status line says where
    // the keyboard is and what happened, and the stats bar says what the turn costs.
    // They are separate because they answer different questions at different rates.
    let [bar, body, status, stats] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_menubar(frame, app, bar);
    let mut viewports = draw_body(frame, app, body);
    draw_status(frame, app, status);
    draw_stats(frame, app, stats);

    // Overlays, back to front: the picker and the palette are questions, the card
    // is an interruption, the review covers the card it answers, and the toast is
    // a note about the last key. The menu's list is drawn first because a bar with
    // a list open is still the lowest thing over the panes.
    let list = app
        .menu
        .filter(|state| state.items)
        .and_then(|state| draw_menu_list(frame, state, bar, body, &app.theme));
    viewports.menu = MenuAreas { row: bar.y, list };
    if app.about {
        draw_about(frame, app, body);
    }
    if app.terminal_picker {
        draw_terminal_picker(frame, app, body);
    }
    if app.which_key {
        draw_key_list(frame, app, body);
    }
    if app.palette.is_some() {
        draw_palette(frame, app, body);
    }
    if app.grep.is_some() {
        draw_grep(frame, app, body);
    }
    if app.source.is_some() {
        draw_source_control(frame, app, body);
    }
    if app.permission_card
        && let Some(request) = app.permissions.get(app.permission_at)
    {
        draw_permission(
            frame,
            request,
            app.permission_at + 1,
            app.permissions.len(),
            app.diff.as_ref(),
            body,
            &app.theme,
        );
    }
    if app.review.is_some() {
        draw_review(frame, app, body);
    }
    if app.commit.is_some() {
        draw_commit_composer(frame, app, body);
    }
    if let Some(toast) = &app.toast {
        draw_toast(frame, toast, body, &app.theme);
    }

    viewports
}
/// The panels, or the terminal in place of the editor.
///
/// Three panels is the product's mental model — the workspace, the text, the agent —
/// and it is the same three in every frontend. Two of them can be on either side of
/// the editor (the Panels menu moves them), so the body is built out of two optional
/// columns with the editor in the middle, rather than a fixed tree-editor-sidebar
/// split. A panel that moved keeps the width it was dragged to, and two panels on one
/// side share the column, the workspace above the agent.
///
/// Below a hundred columns both columns fold away rather than squeezing the editor to
/// a slit: the editor is what a session is read in.
///
/// Returns the size a program in the pane should be told, and the transcript's own
/// when it is the pane on screen: the cells inside each, because that is what the
/// next attach uses and what a copy cursor walks.
fn draw_body(frame: &mut Frame, app: &mut App, area: Rect) -> Viewports {
    let wide = area.width >= 100;
    // Each side's column is as wide as the widest panel on it, both widths clamped to
    // what the editor beside them can spare. A layout that obeyed the number instead
    // of the window would put the editor at zero columns.
    let files_width = app.files_width.min(area.width.saturating_sub(40)).max(16);
    let assistant_width = app
        .sidebar_width
        .min(if wide { 80 } else { 34 })
        .min(area.width.saturating_sub(40))
        .max(20);
    // Below a hundred columns the file manager folds away and the assistant stays: a
    // tree would squeeze the editor to a slit, and the editor is what a session is
    // read in. The assistant is the pane a person is watching, so it is the one that
    // keeps its column.
    let column = |side: Side| -> u16 {
        let mut width = 0;
        if wide && app.panels.on(Panel::FileManager, side) {
            width = width.max(files_width);
        }
        if wide && app.show_outline && app.panels.on(Panel::Outline, side) {
            width = width.max(OUTLINE_WIDTH);
        }
        if wide && app.show_references && app.panels.on(Panel::References, side) {
            width = width.max(REFERENCES_WIDTH);
        }
        if wide && app.show_map && app.panels.on(Panel::ProjectMap, side) {
            width = width.max(PROJECT_MAP_WIDTH);
        }
        if app.panels.on(Panel::Assistant, side) {
            width = width.max(assistant_width);
        }
        width
    };
    let (left, rest) = if column(Side::Left) > 0 {
        let [left, rest] =
            Layout::horizontal([Constraint::Length(column(Side::Left)), Constraint::Min(40)])
                .areas(area);
        (Some(left), rest)
    } else {
        (None, area)
    };
    // A terminal is a keyboard, and a shell at forty columns is a shell nobody can
    // use: it takes the editor's place and the column beside it, so the right column
    // is not split off while one is up. The session panes are behind it until it
    // closes, which is the trade the pane has always made.
    let (centre, right) = if app.terminal.is_none() && column(Side::Right) > 0 {
        let [centre, right] =
            Layout::horizontal([Constraint::Min(20), Constraint::Length(column(Side::Right))])
                .areas(rest);
        (centre, Some(right))
    } else {
        (rest, None)
    };

    let mut files_area = None;
    let mut sessions_area = None;
    let mut transcript = None;
    for (column, side) in [(left, Side::Left), (right, Side::Right)] {
        // Nothing on the right while a terminal is up: `right` is already `None` for
        // the default arrangement, and an assistant moved to the left is drawn beside
        // the shell rather than hidden under it.
        let Some(column) = column else {
            continue;
        };
        for (slot, panel) in panels_on(app, column, side, wide) {
            match panel {
                Panel::FileManager => files_area = Some(draw_files(frame, app, slot)),
                Panel::Outline => {
                    draw_outline(frame, app, slot);
                }
                Panel::References => {
                    draw_references(frame, app, slot);
                }
                Panel::ProjectMap => {
                    draw_map(frame, app, slot);
                }
                Panel::Assistant => {
                    let (size, sessions) = draw_agents(frame, app, slot);
                    sessions_area = Some(sessions);
                    transcript = Some(size);
                }
            }
        }
    }

    let dividers = Dividers {
        // A column is grabbable only where the shell has always put it. A panel that
        // moved to the far side is resized with the arrow keys instead, because a
        // divider whose meaning depended on the day would be one nobody could learn.
        tree: left
            .filter(|_| app.panels.on(Panel::FileManager, Side::Left))
            .map(|left| left.x + left.width.saturating_sub(1)),
        // The assistant is behind the terminal when one is attached, so there is no
        // divider to grab there either.
        sidebar: right
            .filter(|_| app.panels.on(Panel::Assistant, Side::Right) && app.terminal.is_none())
            .map(|right| right.x),
        left: area.x,
        right: area.right(),
    };

    let mut editor_area = None;
    let mut terminal_area = None;
    match &app.terminal {
        Some(pane) => {
            terminal_area = Some(draw_terminal(frame, pane, centre, &app.theme));
            // The transcript is behind the terminal, so there is no viewport for a
            // copy cursor to walk.
            transcript = None;
        }
        None => editor_area = draw_editor(frame, app, centre),
    };

    Viewports {
        terminal: inner_size(centre),
        transcript,
        dividers,
        // The bar is drawn before the body, and the list after it: `draw` fills
        // this in once the list knows where it landed.
        menu: MenuAreas::default(),
        editor: editor_area,
        files: files_area,
        sessions: sessions_area,
        terminal_area,
    }
}

/// The menu bar: the product's name, then one title per menu.
///
/// It is chrome, not a modal: the bar is always drawn, so the menus are discoverable
/// without a key list, and `F9` is only how the keyboard gets into it.
fn draw_menubar(frame: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(app.theme.text_muted());
    let mut spans = vec![Span::styled(
        " Acpilot ",
        Style::default()
            .fg(app.theme.accent())
            .add_modifier(Modifier::BOLD),
    )];
    for (index, menu) in MENUS.iter().enumerate() {
        let chosen = app.menu.is_some_and(|state| state.open == index);
        let style = if chosen {
            Style::default()
                .bg(app.theme.bg_panel())
                .fg(app.theme.accent_high())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        // The letter `Alt` opens it with is underlined in the title, which is how a
        // menu bar has named its own accelerators since before a terminal could
        // report a mouse. The spans stay the widths `menu_title` reports, because a
        // click is answered with the same arithmetic.
        let letters: Vec<char> = menu.title.chars().collect();
        let at = letters
            .iter()
            .position(|character| character.eq_ignore_ascii_case(&menu.mnemonic))
            .unwrap_or(0);
        let (before, after): (String, String) = (
            letters[..at].iter().collect(),
            letters[at + 1..].iter().collect(),
        );
        spans.push(Span::styled(" ", style));
        spans.push(Span::styled(before, style));
        // The title's own character, not the mnemonic field: the bar keeps the label's
        // capital letter and only says which one `Alt` answers to.
        spans.push(Span::styled(
            letters[at].to_string(),
            style.add_modifier(Modifier::UNDERLINED),
        ));
        spans.push(Span::styled(format!("{after} "), style));
        spans.push(Span::styled(" ", dim));
    }
    // The right end of the bar is the one place the profile is always named: it is
    // what the daemon is serving, and a session is read in its terms.
    spans.push(Span::styled("  f9 menus", dim));
    if !app.profile.is_empty() {
        spans.push(Span::styled(format!(" · {} ", app.profile), dim));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The list under an open menu, over the panes.
///
/// Returns where it landed, because the mouse needs to know what it can hit and the
/// drawing is the only thing that knows.
fn draw_menu_list(
    frame: &mut Frame,
    state: MenuState,
    bar: Rect,
    body: Rect,
    theme: &Theme,
) -> Option<MenuList> {
    let menu = MENUS.get(state.open)?;
    // Each row is a label and its chord, so the list is wide enough for the longest
    // pair and the gap between them: a shortcut clipped at the edge is a shortcut
    // nobody can read, which is the one thing a menu is for.
    let width = menu
        .items
        .iter()
        .map(|row| {
            u16::try_from(row.label.chars().count() + row.keys.chars().count()).unwrap_or(0) + 6
        })
        .max()
        .unwrap_or(24)
        .max(28);
    let height = u16::try_from(menu.items.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(body.height);
    if height < 3 || bar.width < width {
        return None;
    }
    let card = Rect {
        x: bar.x
            + menu_title(state.open)
                .0
                .min(bar.width.saturating_sub(width)),
        y: bar.y + 1,
        width,
        height,
    };
    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", menu.title))
        .border_style(Style::default().fg(theme.accent()));
    let inner = usize::from(width.saturating_sub(2));
    let rows: Vec<Line> = menu
        .items
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let chosen = index == state.at;
            let style = if chosen {
                Style::default()
                    .bg(theme.bg_panel())
                    .fg(theme.accent_high())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // The label is padded rather than the keys aligned after it, because the
            // chord column is what a person reads down.
            let label_width = inner.saturating_sub(1 + row.keys.chars().count() + 1);
            Line::from(vec![
                Span::styled(format!(" {:<label_width$}", row.label), style),
                Span::styled(
                    format!("{} ", row.keys),
                    Style::default().fg(theme.text_muted()),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(rows).block(block), card);
    Some(MenuList {
        x: card.x,
        y: card.y,
        width: card.width,
        rows: height.saturating_sub(2),
    })
}

/// The About card: the four facts a bug report asks for, and where the daemon is.
///
/// The same rows the desktop's About prints, from the same version constant, so a
/// report from either cockpit names the same build.
fn draw_about(frame: &mut Frame, app: &App, area: Rect) {
    // The card is the size of what it says, not a fraction of the screen: a fact
    // sheet clipped at the bottom is the one thing an About card cannot be. Two
    // borders, a name, a tagline, the facts, the daemon, a gap and the way out.
    if area.width < 32 || area.height < 8 {
        return;
    }
    let facts = u16::try_from(about_lines().len()).unwrap_or(4);
    let card = Rect {
        width: area.width.saturating_sub(4).clamp(32, 64),
        height: facts.saturating_add(8).min(area.height),
        ..Rect::default()
    };
    let card = Rect {
        x: area.x + area.width.saturating_sub(card.width) / 2,
        y: area.y + area.height.saturating_sub(card.height) / 2,
        ..card
    };
    if card.width < 32 || card.height < 8 {
        return;
    }
    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" About Acpilot ")
        .border_style(Style::default().fg(app.theme.accent()));
    let mut lines = vec![
        Line::from(Span::styled(
            "Acpilot",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "sessions, transcript and permission cards, over the daemon",
            Style::default().fg(app.theme.text_muted()),
        )),
        Line::from(""),
    ];
    for (label, value) in about_lines() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label:<14}"),
                Style::default().fg(app.theme.accent()),
            ),
            Span::raw(value.to_owned()),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled(
            format!("{:<14}", "Daemon"),
            Style::default().fg(app.theme.accent()),
        ),
        Span::raw(app.daemon.clone()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "any key closes",
        Style::default().fg(app.theme.text_muted()),
    )));
    frame.render_widget(Paragraph::new(lines).block(block), card);
}

/// The workspace tree.
///
/// Rows are flat: `depth` is drawn as indentation, not walked to. What a real
/// tree has to deliver is exactly this list, which is why the pane can be designed
/// before P6-6 reads a disk.
fn draw_files(frame: &mut Frame, app: &App, area: Rect) -> PaneArea {
    // The tree's own name, not the path: the column is 26 cells and a home
    // directory with the workspace in it is longer than the pane that holds it.
    let title = match &app.search {
        // The query is the title while one is up: the pane is answering that
        // question, and the words a person typed belong on screen.
        Some(query) => format!(" Find · {query} "),
        None => match app.workspace.rsplit('/').next() {
            Some(workspace) if !workspace.is_empty() => format!(" File manager · {workspace} "),
            _ => " File manager ".to_owned(),
        },
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(app.focus == Focus::Files, &app.theme));
    // The rows start inside the border, and the mouse is told so: a click on the
    // frame is a click on the frame, not on the first file.
    let inner = block.inner(area);

    if app.visible_files().is_empty() {
        let message = if app.search.is_some() {
            "no path matches\ntype more of it · Esc puts the tree back"
        } else {
            "no rows yet\nthe daemon serves the workspace"
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(app.theme.text_muted()))
                .block(block),
            area,
        );
        return pane_area(inner);
    }

    // The window follows the selection, and only says what fits: a chosen row that
    // walked off the bottom of the pane would be a pane with no cursor, and one that
    // kept drawing past the border would draw over the footer.
    let visible = app.visible_files();
    let height = usize::from(inner.height).max(1);
    let first = first_visible(app.file_selected, visible.len(), height);
    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(index, node)| {
            let chosen = index == app.file_selected;
            let marker = if chosen { "▸" } else { " " };
            let indent = "  ".repeat(usize::from(node.depth));
            let icon = if node.dir {
                if node.expanded { "▾ " } else { "▸ " }
            } else {
                "  "
            };
            let row = if chosen {
                Style::default()
                    .bg(app.theme.bg_panel())
                    .fg(app.theme.accent_high())
                    .add_modifier(Modifier::BOLD)
            } else if node.ignored {
                // A row an ignore rule hides, shown because someone asked to see
                // it: greyed and italic, so it never reads as an ordinary file.
                Style::default()
                    .fg(app.theme.text_muted())
                    .add_modifier(Modifier::ITALIC)
            } else {
                Style::default()
            };
            let status = node
                .status
                .map_or(" ", acpilot_proto::file::FileChange::letter);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker}{indent}{icon}{}", node.label()), row),
                Span::styled(
                    format!(" {status}"),
                    row.fg(git_colour(node.status, &app.theme)),
                ),
            ]))
        })
        .collect();
    frame.render_widget(List::new(items).block(block), area);
    pane_area(inner)
}

/// A rectangle as the mouse measures it: the cells inside a pane's border.
fn pane_area(inner: Rect) -> PaneArea {
    PaneArea {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: inner.height,
    }
}

/// The tab strip, the buffer under it, and the box it is asking a question in.
///
/// The pane is a real document now (P6-4): every row here came from
/// `acpilot_text::pane`'s viewport, which built only the lines that fit and only
/// the cells the horizontal scroll left, and every colour came from the
/// highlighter's tokens resolved through the theme. What this function decides is
/// furniture — the numbers, the git markers, the current line, where the caret
/// is — and nothing that the desktop app would have to decide again.
fn draw_editor(frame: &mut Frame, app: &mut App, area: Rect) -> Option<EditorArea> {
    let [strip, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let theme = app.theme.clone();
    let focused = app.focus == Focus::Editor;
    let Some(editor) = app.editor.as_mut() else {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Editor ")
            .border_style(border_style(focused, &theme));
        frame.render_widget(
            Paragraph::new("no buffer is open\nthe tree opens one — P6-6")
                .style(Style::default().fg(theme.text_muted()))
                .block(block),
            area,
        );
        return None;
    };

    // A prompt replaces the tab strip rather than covering the buffer: the query
    // is a question about the text, and the text it is about has to stay visible
    // while it is typed.
    match editor.prompt() {
        Some(prompt) => {
            let (sigil, rest) = match prompt.kind {
                PromptKind::Find => ('/', ""),
                PromptKind::Line => (':', "line"),
                PromptKind::Command => (':', ""),
            };
            let hint = if rest.is_empty() {
                String::new()
            } else {
                format!(" ({rest})")
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!(" {sigil}"),
                        Style::default()
                            .bg(theme.bg_panel())
                            .fg(theme.accent_high())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{}{hint}", prompt.text),
                        Style::default().bg(theme.bg_panel()),
                    ),
                ])),
                strip,
            );
            if focused {
                let column = u16::try_from(2 + prompt.text.chars().count()).unwrap_or(u16::MAX);
                frame.set_cursor_position((strip.x + column, strip.y));
            }
        }
        None => {
            let tab = Line::from(vec![
                Span::styled(
                    // A `+` while the buffer holds edits that are not in the file,
                    // which is the history's answer rather than a flag's: undo back
                    // to what was opened and the mark goes away by itself.
                    format!(
                        " {}{} ",
                        editor.tab(),
                        if editor.dirty() { " +" } else { "" }
                    ),
                    Style::default()
                        .bg(theme.bg_panel())
                        .fg(theme.accent_high())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {} ", editor.language()),
                    Style::default().fg(theme.text_muted()),
                ),
            ]);
            frame.render_widget(Paragraph::new(tab), strip);
        }
    }

    let title = if editor.find().is_empty() {
        String::new()
    } else {
        format!(" · /{}", editor.find().query())
    };
    // The mode is the title's, not the status line's: it is the one fact about the
    // pane that changes what every key does, and a person reading the text should
    // not have to look somewhere else to find out whether they can type. The design
    // pass's `seeded` marker gave up its room here for it — the status line still
    // says `demo · seeded`, and a title that clipped the running search to say the
    // buffer is a fixture would keep the smaller fact and lose the larger one.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " {} · {}{title} ",
            editor.path(),
            editor.mode().label()
        ))
        .border_style(border_style(focused, &theme));
    let inner = block.inner(body);
    frame.render_widget(block, body);

    // The pane learns its size here and answers with the cells it built for it:
    // this is the one place a viewport is measured, so a resize cannot leave the
    // gutter and the text disagreeing about where the text begins.
    editor.resize(usize::from(inner.height), usize::from(inner.width));
    let gutter = editor.gutter();
    let current = editor.current_line();
    let cursor = editor.cursor();
    let painted = editor.paint();

    let mut lines: Vec<Line> = Vec::with_capacity(painted.len());
    for row in &painted {
        let base = if row.line == current {
            Style::default().bg(theme.bg_panel())
        } else {
            Style::default()
        };
        // One cell, so a problem outranks a change: a line that does not compile is
        // the fact worth seeing first, and the git mark is still there once it does.
        let marker = match editor.problem(row.line) {
            Some(problem) => Span::styled(format!("{problem} "), base.fg(theme.danger())),
            None => Span::styled(
                editor
                    .mark(row.line)
                    .map_or_else(|| "  ".to_owned(), |letter| format!("{letter} ")),
                base.fg(theme.warning()),
            ),
        };
        let mut spans = vec![
            Span::styled(
                format!("{:>width$} ", row.line + 1, width = gutter - 3),
                base.fg(theme.text_muted()),
            ),
            marker,
        ];
        for run in &row.runs {
            let mut style = base;
            if let Some(token) = run.token {
                style = style.fg(theme.syntax(token));
            }
            style = match run.state {
                State::Plain => style,
                State::Match => style.fg(theme.warning()).add_modifier(Modifier::UNDERLINED),
                State::CurrentMatch => style.bg(theme.accent()).fg(theme.bg()),
                State::Bracket => style.fg(theme.accent_high()).add_modifier(Modifier::BOLD),
                // A drag's selection is reversed rather than tinted, the same mark
                // the terminal's copy mode uses: it has to read as marked on every
                // theme, including one whose accent is close to its background.
                State::Selected => style.add_modifier(Modifier::REVERSED),
            };
            spans.push(Span::styled(run.text.clone(), style));
        }
        // The current line is a row of the pane, not a run of text: the highlight
        // has to reach the right edge or it is a box drawn around some words.
        let used = gutter + row.width;
        if let Some(pad) = usize::from(inner.width).checked_sub(used)
            && pad > 0
        {
            spans.push(Span::styled(" ".repeat(pad), base));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if focused
        && editor.prompt().is_none()
        && let Some((row, column)) = cursor
    {
        // The gutter is a constant width for the frame the pane was drawn at, so
        // the caret is arithmetic over what the pane reported — not a second
        // measurement of the row it is on.
        let x = inner.x + u16::try_from(gutter).unwrap_or(0) + u16::try_from(column).unwrap_or(0);
        let y = inner.y + u16::try_from(row).unwrap_or(0);
        frame.set_cursor_position((x.max(inner.x), y.min(inner.bottom().saturating_sub(1))));
    }

    // The pane is the only thing that knows where its text begins, so it says,
    // rather than the mouse repeating the border and the gutter arithmetic.
    Some(EditorArea {
        pane: PaneArea {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height,
        },
        gutter: u16::try_from(gutter).unwrap_or(u16::MAX),
    })
}

/// The panels drawn in one column, each with the cells it may fill.
///
/// A column with one panel gives it everything. A column with two halves it: the
/// workspace above the agent, because a person opens a file to show it to the agent,
/// and the two have different jobs — a tree is scanned a row at a time, a transcript
/// is read in paragraphs, and neither wants the other's room.
fn panels_on(app: &App, column: Rect, side: Side, wide: bool) -> Vec<(Rect, Panel)> {
    // A narrow window is the file manager's exception, not the assistant's: the tree
    // folds away and the agent keeps its column.
    let stack: Vec<Panel> = app
        .panels
        .stack(side)
        .into_iter()
        .filter(|panel| wide || *panel != Panel::FileManager)
        .filter(|panel| app.show_outline || *panel != Panel::Outline)
        .filter(|panel| app.show_references || *panel != Panel::References)
        .filter(|panel| app.show_map || *panel != Panel::ProjectMap)
        .collect();
    if stack.is_empty() {
        return Vec::new();
    }
    // Equal shares however many panels are on the side: a column is a place, and a
    // pane that is on screen gets a third of it rather than nothing.
    let count = u32::try_from(stack.len()).unwrap_or(1);
    let slots: Vec<Rect> = Layout::vertical(std::iter::repeat_n(
        Constraint::Ratio(1, count),
        stack.len(),
    ))
    .split(column)
    .to_vec();
    slots.into_iter().zip(stack).collect()
}

/// The outline panel: one row per definition in the open buffer.
///
/// The kind is beside the name rather than instead of it: `fn` and `struct` are what a
/// person is scanning for, and a list of bare names is a list nobody can skip down.
fn draw_outline(frame: &mut Frame, app: &App, area: Rect) -> PaneArea {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Outline · {} ", app.outline.len()))
        .border_style(border_style(app.focus == Focus::Outline, &app.theme));
    let inner = block.inner(area);
    if app.outline.is_empty() {
        let message = if app.editor.is_some() {
            "no symbols in this file\nthe grammar it parses with defines none"
        } else {
            "no file is open\nEnter in the file manager reads one"
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(app.theme.text_muted()))
                .block(block),
            area,
        );
        return pane_area(inner);
    }
    let height = usize::from(inner.height).max(1);
    let first = first_visible(app.outline_selected, app.outline.len(), height);
    let items: Vec<ListItem> = app
        .outline
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(index, symbol)| {
            let chosen = index == app.outline_selected;
            let row = if chosen {
                Style::default()
                    .bg(app.theme.bg_panel())
                    .fg(app.theme.accent_high())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let indent = "  ".repeat(usize::from(symbol.depth));
            ListItem::new(Line::from(vec![
                Span::styled(format!("{indent}{}", symbol.name), row),
                Span::styled(
                    format!("  {}", symbol.kind),
                    Style::default().fg(app.theme.text_muted()),
                ),
            ]))
        })
        .collect();
    frame.render_widget(List::new(items).block(block), area);
    pane_area(inner)
}

/// The references panel: one row per code use of the name under the caret.
///
/// The row is the path and the line, then the line's own text with the name marked:
/// the answer to "where is this used" has to be readable before it is chosen from,
/// which is the same reason the content search's rows carry text.
fn draw_references(frame: &mut Frame, app: &App, area: Rect) -> PaneArea {
    let title = if app.references_name.is_empty() {
        " References ".to_owned()
    } else {
        format!(
            " References · {} · {} ",
            app.references_name,
            app.references.len()
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(app.focus == Focus::References, &app.theme));
    let inner = block.inner(area);
    if app.references.is_empty() {
        // Three different empty panels, because they ask for three different things: a
        // buffer, a caret on a name, or nothing at all. A panel that said "no uses"
        // while the caret stood between two words would be an answer to a question
        // nobody asked.
        let message = if app.editor.is_none() {
            "no file is open\nEnter in the file manager reads one".to_owned()
        } else if app.references_name.is_empty() {
            "the caret is not on a word\nmove it onto a name".to_owned()
        } else {
            format!(
                "no code uses {}\nshift-f12 names the word under the caret",
                app.references_name
            )
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(app.theme.text_muted()))
                .block(block),
            area,
        );
        return pane_area(inner);
    }
    let height = usize::from(inner.height).max(1);
    let first = first_visible(app.references_selected, app.references.len(), height);
    let rows: Vec<Line> = app
        .references
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(index, hit)| {
            let chosen = index == app.references_selected;
            let row = if chosen {
                Style::default()
                    .bg(app.theme.bg_panel())
                    .fg(app.theme.accent_high())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let body = if chosen {
                row
            } else {
                Style::default().fg(app.theme.text_muted())
            };
            let mark = Style::default()
                .bg(app.theme.accent())
                .fg(app.theme.bg())
                .add_modifier(Modifier::BOLD);
            let mut spans = vec![
                Span::styled(format!("{}:{}", hit.path, hit.line), row),
                Span::styled("  ", body),
            ];
            spans.extend(highlight_line(
                hit.text.trim(),
                &app.references_name,
                body,
                mark,
            ));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(rows).block(block), area);
    pane_area(inner)
}

/// The project map: one row per area the agent named, with the caret's area opened.
///
/// The map is a document about the workspace rather than a listing of it, so a row is
/// a name and a path and nothing else until it is worth reading: the area the open
/// file belongs to is marked and opened, because that is the neighbourhood the person
/// is standing in, and the selected row opens too, because `j` is how they ask.
fn draw_map(frame: &mut Frame, app: &App, area: Rect) -> PaneArea {
    let title = match &app.map {
        Some(map) if map.written => format!(" Project map · {} areas ", map.areas.len()),
        _ => " Project map ".to_owned(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(app.focus == Focus::ProjectMap, &app.theme));
    let inner = block.inner(area);
    let muted = Style::default().fg(app.theme.text_muted());
    let Some(map) = &app.map else {
        frame.render_widget(
            Paragraph::new("reading the map…").style(muted).block(block),
            area,
        );
        return pane_area(inner);
    };
    if !map.written {
        frame.render_widget(
            Paragraph::new(format!(
                "no map yet\nthe agent writes {}\nfrom the tree and the README",
                map.path
            ))
            .style(muted)
            .block(block),
            area,
        );
        return pane_area(inner);
    }
    if map.areas.is_empty() {
        frame.render_widget(
            Paragraph::new(format!("{} names no areas", map.path))
                .style(muted)
                .block(block),
            area,
        );
        return pane_area(inner);
    }
    let here = app
        .editor
        .as_ref()
        .and_then(|editor| map.area_for(editor.path()));
    let height = usize::from(inner.height).max(1);
    let width = usize::from(inner.width).saturating_sub(2).max(8);
    let first = first_visible(app.map_selected, map.areas.len(), height);
    let mut lines: Vec<Line> = Vec::new();
    for (index, area) in map.areas.iter().enumerate().skip(first) {
        if lines.len() >= height {
            break;
        }
        let chosen = index == app.map_selected;
        let standing = here == Some(index);
        let row = if chosen {
            Style::default()
                .bg(app.theme.bg_panel())
                .fg(app.theme.accent_high())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let bullet = if standing {
            Span::styled("● ", Style::default().fg(app.theme.accent()))
        } else {
            Span::styled("  ", muted)
        };
        let path = if area.path.is_empty() || area.path == "." {
            String::new()
        } else {
            format!("  {}", area.path)
        };
        lines.push(Line::from(vec![
            bullet,
            Span::styled(area.name.clone(), row),
            Span::styled(path, if chosen { row } else { muted }),
        ]));
        // The summary belongs to the area being read, not to every row: a map where
        // all the prose is always on screen is a map nobody can scan.
        if chosen || standing {
            for line in wrap_text(&area.summary, width) {
                if lines.len() >= height {
                    break;
                }
                lines.push(Line::from(Span::styled(
                    format!("    {line}"),
                    Style::default().fg(app.theme.text()),
                )));
            }
            if !area.depends_on.is_empty() && lines.len() < height {
                lines.push(Line::from(Span::styled(
                    format!("    → {}", area.depends_on.join(", ")),
                    muted,
                )));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
    pane_area(inner)
}

/// `text` as lines no wider than `width` columns, breaking at spaces.
///
/// A paragraph widget would wrap the rows too, and a path wrapped mid-word reads as
/// two paths. A word longer than the column is cut rather than left to overflow.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if line.is_empty() {
                line = word.chars().take(width).collect();
            } else if line.chars().count() + 1 + word.chars().count() <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                lines.push(std::mem::take(&mut line));
                line = word.chars().take(width).collect();
            }
        }
        lines.push(line);
    }
    lines
}

/// The agent panel: sessions over a transcript over a composer.
///
/// Returns the transcript's cells, because that is the viewport a copy cursor
/// walks and nothing outside this function measures it.
fn draw_agents(frame: &mut Frame, app: &App, area: Rect) -> (TerminalSize, PaneArea) {
    let list = u16::try_from(app.sessions.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .clamp(4, area.height.saturating_sub(8).max(4));
    let [sessions, transcript, composer] = Layout::vertical([
        Constraint::Length(list),
        Constraint::Min(3),
        Constraint::Length(app.composer_height()),
    ])
    .areas(area);
    let sessions = draw_sessions(frame, app, sessions);
    let size = draw_transcript(frame, app, transcript);
    draw_composer(frame, app, composer);
    draw_completion(frame, app, area, composer);
    (size, sessions)
}

/// The cells inside a bordered pane.
fn inner_size(area: Rect) -> TerminalSize {
    TerminalSize {
        cols: area.width.saturating_sub(2).max(1),
        rows: area.height.saturating_sub(2).max(1),
    }
}

fn draw_terminal(frame: &mut Frame, pane: &TerminalPane, area: Rect, theme: &Theme) -> PaneArea {
    // The title is the pane's own legend: which program, and every key that gets
    // out of it. Copy mode shows how far back the view is, because a scrolled
    // terminal otherwise looks exactly like a program that stopped printing.
    // The title is the pane's own legend: which program, and every key that gets
    // out of it. Copy mode shows how far back the view is and what a find found,
    // because a scrolled terminal otherwise looks exactly like a program that
    // stopped printing, and a query being typed is invisible without its line.
    let hint = match pane.mode {
        PaneMode::Copy => match (&pane.reader.typing, &pane.reader.find) {
            (Some(query), _) => format!("find {query}▏ · Enter searches · Esc cancels"),
            (None, Some(find)) => format!(
                "copy {}/{history} · match {}/{total} · n/N · Esc leaves",
                pane.reader.view.scroll,
                find.at + 1,
                total = find.hits.len(),
                history = pane.history()
            ),
            (None, None) => format!(
                "copy {}/{history} · / find · v/V select · y copy · Esc leaves",
                pane.reader.view.scroll,
                history = pane.history()
            ),
        },
        // Scrolled back without entering copy mode: the pane has to say so, or it
        // looks exactly like a program that stopped printing.
        PaneMode::Live if pane.reader.view.scroll > 0 => format!(
            "behind {}/{history} · any key returns · ctrl-t leaves",
            pane.reader.view.scroll,
            history = pane.history()
        ),
        PaneMode::Live if pane.alive => "F2 copy · ctrl-t leaves".to_owned(),
        PaneMode::Live => "exited · F2 copy · ctrl-t leaves".to_owned(),
    };
    let border = match pane.mode {
        PaneMode::Copy => theme.accent_high(),
        PaneMode::Live if pane.alive => theme.accent(),
        PaneMode::Live => theme.warning(),
    };
    let title = format!(" {} · {} · {hint} ", pane.summary.command, pane.summary.id);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border));

    let inner = block.inner(area);
    let first = pane
        .history()
        .saturating_sub(pane.reader.view.scroll.min(pane.history()));
    let lines: Vec<Line> = (0..inner.height)
        .map(|row| terminal_line(pane, row, first, inner.width, theme))
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);

    // The pane says where its grid landed, so the wheel does not have to guess at
    // the border it was drawn with.
    PaneArea {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: inner.height,
    }
}

/// One screen row as styled spans.
///
/// Cells, not `contents()`: a shell colours its errors, and a pane that dropped
/// the attributes would be a terminal that lies about what a program drew. Runs of
/// one style become one span, so a 200-column line is not 200 of them; trailing
/// blanks are dropped because they would only paint the background.
fn terminal_line(
    pane: &TerminalPane,
    row: u16,
    first: usize,
    cols: u16,
    theme: &Theme,
) -> Line<'static> {
    let screen = pane.parser.screen();
    let copying = pane.mode == PaneMode::Copy;
    let line = first + usize::from(row);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run_style: Option<Style> = None;
    let mut run_text = String::new();
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            break;
        };
        // A wide character's second cell has no glyph of its own; the first one
        // already drew both columns.
        if cell.is_wide_continuation() {
            continue;
        }
        let mut style = cell_style(cell);
        if pane.reader.view.selected(line, col) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if pane.matched(line, col) {
            // Yellow rather than reversed: a match can sit inside a selection or
            // under the cursor, and it has to be readable through both.
            style = style.add_modifier(Modifier::UNDERLINED).fg(theme.warning());
        }
        if copying && pane.reader.view.cursor == (line, col) {
            // The cursor sits inside selections, so it reverses *and* underlines:
            // a bare inversion would be a cell a person cannot tell from the rest
            // of the selection.
            style = style.add_modifier(Modifier::REVERSED | Modifier::UNDERLINED);
        }
        if run_style == Some(style) {
            run_text.push_str(cell.contents());
            continue;
        }
        if let Some(style) = run_style.take() {
            spans.push(Span::styled(std::mem::take(&mut run_text), style));
        }
        run_style = Some(style);
        run_text.push_str(cell.contents());
    }
    if let Some(style) = run_style {
        // Trailing blanks are a background colour with nothing in them.
        let trimmed = run_text.trim_end().len();
        run_text.truncate(trimmed);
        if !run_text.is_empty() {
            spans.push(Span::styled(run_text, style));
        }
    }
    Line::from(spans)
}

/// A cell's colour and emphasis, in `ratatui`'s terms.
fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    if let Some(colour) = colour(cell.fgcolor()) {
        style = style.fg(colour);
    }
    if let Some(colour) = colour(cell.bgcolor()) {
        style = style.bg(colour);
    }
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn colour(colour: vt100::Color) -> Option<Color> {
    match colour {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(Color::Indexed(index)),
        vt100::Color::Rgb(red, green, blue) => Some(Color::Rgb(red, green, blue)),
    }
}

fn draw_sessions(frame: &mut Frame, app: &App, area: Rect) -> PaneArea {
    let title = format!(" Assistant · {} ", app.sessions.len());
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(index, session)| {
            let chosen = index == app.selected;
            let marker = if chosen { "▸ " } else { "  " };
            let status = status_label(session.status);
            let style = if chosen {
                Style::default()
                    .bg(app.theme.bg_panel())
                    .fg(app.theme.accent_high())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker}{}", session.title), style),
                Span::styled(
                    format!("  {status}"),
                    style.fg(status_colour(session.status, &app.theme)),
                ),
            ]))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(app.focus == Focus::Sessions, &app.theme));
    let inner = block.inner(area);
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("no sessions · n starts one")
                .style(Style::default().fg(app.theme.text_muted()))
                .block(block),
            area,
        );
    } else {
        frame.render_widget(List::new(items).block(block), area);
    }
    pane_area(inner)
}

/// The transcript, and the cells it was drawn in.
///
/// The size comes back because a copy cursor walks lines of a viewport: the pane
/// cannot measure itself, and this is the only place that knows how many lines fit
/// above the composer.
fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) -> TerminalSize {
    let lines = app.transcript_rows();
    let inner = inner_size(area);
    let height = usize::from(inner.rows);
    let max_scroll = lines.len().saturating_sub(height);

    let name = match (&app.open, app.selected_session()) {
        (Some(id), Some(session)) if &session.id == id => {
            format!("{} · {}", session.agent_id, session.title)
        }
        (Some(id), _) => id.clone(),
        (None, _) => "Transcript".to_owned(),
    };
    // The hint is short on purpose: the sidebar is 44 columns, and the title is
    // where the session is named. The keys belong on the status line, which has the
    // whole window to spend on them.
    let reading = app.focus == Focus::Transcript;
    let hint = match (reading, app.open.is_some()) {
        (true, true) if app.transcript.view.scroll == 0 => "copy".to_owned(),
        (true, true) => format!(
            "copy {}/{max_scroll}",
            app.transcript.view.scroll.min(max_scroll)
        ),
        // A turn in flight has a cursor at the end of the last line and says so:
        // a reply that is still arriving looks exactly like one that stopped.
        (false, true) if app.streaming() => "streaming · tab · f3".to_owned(),
        (false, true) => "tab · f3".to_owned(),
        // A transcript with the keyboard and no session is not a state the app can
        // reach, but it still has to say something.
        (_, _) => String::new(),
    };
    let title = if hint.is_empty() {
        format!(" {name} ")
    } else {
        format!(" {name} · {hint} ")
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(reading, &app.theme));

    let body = if lines.is_empty() {
        Paragraph::new("no transcript yet").style(Style::default().fg(app.theme.text_muted()))
    } else {
        // The last screen of a conversation is the part people read; a
        // transcript that always starts at the top is a transcript nobody
        // scrolls back to the bottom of.
        let scroll = if reading {
            app.transcript.view.scroll.min(max_scroll)
        } else {
            0
        };
        let start = max_scroll - scroll;
        let mut rows: Vec<Line> = lines
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(index, row)| transcript_line(app, row, index))
            .collect();
        // The cursor sits after the newest text and only while the tail is on
        // screen: a scrolled transcript with a cursor would point at a line the
        // turn has not reached yet.
        if app.streaming()
            && !reading
            && start + height >= lines.len()
            && let Some(last) = rows.last_mut()
        {
            last.spans
                .push(Span::styled("▏", Style::default().fg(app.theme.accent())));
        }
        let paragraph = Paragraph::new(rows);
        // Wrapped for reading, unwrapped while it is being read back: in copy mode
        // one line of text is one row of the view, so a cell means one character.
        // A wrapped line would put two different characters in the same place.
        if reading {
            paragraph
        } else {
            paragraph.wrap(Wrap { trim: false })
        }
    };

    frame.render_widget(body.block(block), area);
    inner
}

/// One row of the transcript, with the selection and the cursor when it is being
/// read.
///
/// A cell at a time only in copy mode: outside it a line is one span, because a
/// style run per character is a cost a view pays only while someone points at it.
fn transcript_line(app: &App, line: &crate::app::TranscriptRow, index: usize) -> Line<'static> {
    let base = entry_style(line.kind, &app.theme);
    let reading = app.focus == Focus::Transcript;
    let mut spans: Vec<Span> = Vec::new();
    let mut col = 0u16;
    for segment in &line.segments {
        let emphasis = emphasis_style(segment.emphasis, &app.theme);
        let mut run = String::new();
        let mut run_style: Option<Style> = None;
        for character in segment.text.chars() {
            let mut style = base.patch(emphasis);
            if reading {
                if app.transcript.view.selected(index, col) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                if app.matched_transcript(index, col) {
                    style = style
                        .add_modifier(Modifier::UNDERLINED)
                        .fg(app.theme.warning());
                }
                if app.transcript.view.cursor == (index, col) {
                    style = style.add_modifier(Modifier::REVERSED | Modifier::UNDERLINED);
                }
            }
            if run_style == Some(style) {
                run.push(character);
            } else {
                if let Some(style) = run_style.take() {
                    spans.push(Span::styled(std::mem::take(&mut run), style));
                }
                run_style = Some(style);
                run.push(character);
            }
            col = col.saturating_add(1);
        }
        if let Some(style) = run_style {
            spans.push(Span::styled(run, style));
        }
    }
    // The cursor sits after the last character as well as on one, and a cursor on
    // a cell that does not exist would be a pane with no visible cursor at all.
    if reading
        && app.transcript.view.cursor
            == (
                index,
                u16::try_from(line.text().chars().count()).unwrap_or(u16::MAX),
            )
    {
        spans.push(Span::styled(
            " ",
            base.add_modifier(Modifier::REVERSED | Modifier::UNDERLINED),
        ));
    }
    Line::from(spans)
}

/// How a markdown run is set, on top of whatever its kind already asked for.
fn emphasis_style(emphasis: crate::markdown::Emphasis, theme: &Theme) -> Style {
    use crate::markdown::Emphasis;
    match emphasis {
        Emphasis::Plain => Style::default(),
        Emphasis::Bold => Style::default().add_modifier(Modifier::BOLD),
        Emphasis::Italic => Style::default().add_modifier(Modifier::ITALIC),
        Emphasis::Code => Style::default().fg(theme.accent_high()),
        Emphasis::Link => Style::default()
            .fg(theme.accent())
            .add_modifier(Modifier::UNDERLINED),
        Emphasis::Heading => Style::default()
            .fg(theme.accent_high())
            .add_modifier(Modifier::BOLD),
        Emphasis::Quote => Style::default().fg(theme.text_muted()),
    }
}

fn draw_composer(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Prompt · Enter sends · Alt-Enter a line · @ a file ")
        .border_style(border_style(app.focus == Focus::Prompt, &app.theme));
    let text = if app.input.is_empty() {
        Paragraph::new("ask the agent…").style(Style::default().fg(app.theme.text_muted()))
    } else {
        Paragraph::new(app.input.clone())
    };
    frame.render_widget(text.block(block), area);

    if app.focus == Focus::Prompt {
        let (row, col) = app.composer_position();
        // The box has borders, and a caret past the last row of a box that stopped
        // growing would be drawn outside the pane that owns it.
        frame.set_cursor_position((
            (area.x + 1 + col).min(area.x + area.width.saturating_sub(2)),
            (area.y + 1 + row).min(area.y + area.height.saturating_sub(2)),
        ));
    }
}

/// The `@path` popup, drawn over the transcript just above the composer.
///
/// Above rather than inside: the composer is where the caret is, and a list that
/// covered the thing being typed would be a list a person could not type past.
fn draw_completion(frame: &mut Frame, app: &App, panel: Rect, composer: Rect) {
    let Some(completion) = &app.completion else {
        return;
    };
    if app.focus != Focus::Prompt {
        return;
    }
    let rows = u16::try_from(completion.rows.len().max(1)).unwrap_or(u16::MAX);
    let height = rows.min(6).saturating_add(2);
    if composer.y < height + panel.y {
        return;
    }
    let width = panel.width.saturating_sub(2).max(12);
    let area = Rect {
        x: panel.x + 1,
        y: composer.y - height,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" @{} ", completion.query))
        .border_style(Style::default().fg(app.theme.accent()))
        .style(Style::default().bg(app.theme.bg_panel()));
    let inner = block.inner(area);
    let visible = usize::from(inner.height).max(1);
    let first = completion
        .selected
        .saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = if completion.rows.is_empty() {
        vec![Line::from(Span::styled(
            "no file matches",
            Style::default().fg(app.theme.text_muted()),
        ))]
    } else {
        completion
            .rows
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .map(|(index, path)| {
                let style = if index == completion.selected {
                    Style::default()
                        .bg(app.theme.accent())
                        .fg(app.theme.bg())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(path.clone(), style))
            })
            .collect()
    };
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Where a pane runs, when it is not in this window's own workspace and host.
///
/// The picker and the status line answer the question with these words, so a person
/// who learned to read a row does not have to learn a second spelling once the pane
/// has the keyboard. A pane a session opened is the session's, and its workspace is
/// already the window's: the words are for the panes that are not.
fn pane_origin(terminal: &acpilot_proto::terminal::TerminalSummary) -> Option<String> {
    if terminal.session_id.is_some() {
        return None;
    }
    let workspace = terminal.workspace.as_deref().unwrap_or("this workspace");
    let host = terminal.host.as_deref().unwrap_or("this machine");
    Some(format!("{workspace} on {host}"))
}

/// The status line: connection, profile, which pane has the keys and where it runs,
/// the cursor, and what just happened.
///
/// The order is the order a person reads it — is it alive, what am I in, where am I,
/// what did it say — and the address is last because it is the thing that is almost
/// always the same.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(app.theme.text_muted());
    let narrow = area.width < 100;
    let mut spans = vec![Span::styled(
        if app.connected { " ● " } else { " ○ " },
        Style::default().fg(if app.connected {
            app.theme.success()
        } else {
            app.theme.danger()
        }),
    )];
    if !app.profile.is_empty() {
        spans.push(Span::styled(
            format!("{} ", app.profile),
            Style::default().fg(app.theme.accent_high()),
        ));
        spans.push(Span::styled("· ", dim));
    }
    spans.push(Span::styled(
        format!("{} ", app.focus.label()),
        Style::default().fg(app.theme.accent()),
    ));
    // The pane behind the keyboard may not be this window's: a sessionless shell can
    // run in another workspace on another host, and the picker that said so is gone
    // by the time commands are being typed into it.
    if let Some(origin) = app
        .terminal
        .as_ref()
        .and_then(|pane| pane_origin(&pane.summary))
    {
        spans.push(Span::styled(format!("· no session · {origin} "), dim));
    }
    if let Some(editor) = &app.editor {
        let (line, column) = editor.position();
        spans.push(Span::styled(format!("· {line}:{column} "), dim));
        // What the language server says about this file, counted the way the gutter
        // marks it. Silence when there is nothing wrong, and silence when nothing was
        // asked: this line has no room to say "unchecked" in a way that reads.
        let problems = editor.problems();
        let errors = problems
            .iter()
            .filter(|problem| problem.severity == acpilot_proto::file::DiagnosticSeverity::Error)
            .count();
        let warnings = problems
            .iter()
            .filter(|problem| problem.severity == acpilot_proto::file::DiagnosticSeverity::Warning)
            .count();
        if errors + warnings > 0 {
            spans.push(Span::styled(
                format!("· {errors}E {warnings}W "),
                Style::default().fg(if errors > 0 {
                    app.theme.danger()
                } else {
                    app.theme.warning()
                }),
            ));
        }
        // A question still in flight is worth a word: the first one waits on a server
        // being started and an index being built, which is seconds, and a gutter that
        // has not moved in seconds must not read as a clean file.
        if app.diagnostics_pending {
            spans.push(Span::styled("· checking ", dim));
        }
    }
    // What the sessions hold, from the same readings the bar draws. A `~` is the
    // daemon's own count rather than an agent's report, and an absent total is
    // silence instead of a zero nobody measured.
    if let Some(tokens) = app.used_tokens {
        let mark = if app.tokens_estimated { "~" } else { "" };
        spans.push(Span::styled(
            format!("· {mark}{} tokens ", acpilot_stats::tokens(tokens)),
            dim,
        ));
    }
    spans.push(Span::styled("· ", dim));
    spans.push(Span::raw(app.status.clone()));
    // The address is the one thing on the bar that rarely changes, so a narrow
    // terminal spends its width on the status instead.
    if !narrow {
        spans.push(Span::styled(format!("  {} ", app.daemon), dim));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The stats bar: one segment per integration, in the daemon's order.
///
/// The rows arrive already measured — `acpilot_stats::Row`, the same value the
/// desktop app and the page draw — so this function is cells and colours, not
/// arithmetic. A window nobody reported is a track with no fill rather than a bar
/// that happens to look comfortable, and a reading that went quiet is named as old
/// rather than drawn as current.
///
/// `F4` swaps every number for the source behind it: a bar has no room for
/// provenance, and provenance is the difference between a measurement and a guess.
fn draw_stats(frame: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(app.theme.text_muted());
    if app.integrations.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(" stats · no integrations ", dim)),
            area,
        );
        return;
    }
    // The bar shrinks before it drops a row: a row that vanished because a window
    // narrowed is a row a person cannot check, and listing quiet integrations is
    // only worth doing if the list is stable. Even the last level can run out of
    // room, so whatever still does not fit is counted rather than silently gone.
    let width = usize::from(area.width);
    let room = [Room::Full, Room::Figures, Room::Tracks]
        .into_iter()
        .find(|room| bar_width(&app.integrations, app, *room) <= width)
        .unwrap_or(Room::Tracks);
    let mut spans = Vec::new();
    let mut used = 0usize;
    let mut drawn = 0usize;
    for (index, row) in app.integrations.iter().enumerate() {
        let segment = stats_segment(row, app, room);
        let segment_width: usize = segment.iter().map(Span::width).sum();
        // Even the last level can overflow — twenty integrations on an eighty-column
        // terminal — so a segment is drawn whole or not at all: half a label reads as
        // a different integration. The count at the end says how many did not fit,
        // because a bar that quietly hides a provider is worse than a short one.
        if index > 0 && used + SEPARATOR_WIDTH + segment_width > width {
            break;
        }
        if index > 0 {
            spans.push(Span::styled(" │ ", dim));
            used += SEPARATOR_WIDTH;
        }
        used += segment_width;
        drawn += 1;
        spans.extend(segment);
    }
    let hidden = app.integrations.len() - drawn;
    if hidden > 0 {
        spans.push(Span::styled(format!(" │ +{hidden} "), dim));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// How much of a row the bar has room for.
///
/// Three levels rather than a fixed width per integration: a track and a label are
/// what identify a row, the figures are what make it useful, and the credit sentence
/// is the first thing to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Room {
    /// Names, both tracks, and both sentences.
    Full,
    /// Names, both tracks, context figures; credits as a track alone.
    Figures,
    /// Names and tracks.
    Tracks,
}

/// The width `" │ "` takes, so the bar's budget and what it draws agree.
const SEPARATOR_WIDTH: usize = 3;

/// How wide the whole bar would be at a level, so a level can be chosen before
/// anything is drawn.
fn bar_width(rows: &[acpilot_stats::Row], app: &App, room: Room) -> usize {
    rows.iter()
        .map(|row| {
            stats_segment(row, app, room)
                .iter()
                .map(Span::width)
                .sum::<usize>()
        })
        .sum::<usize>()
        + SEPARATOR_WIDTH * rows.len().saturating_sub(1)
}

/// One integration's cells: a name, a context track, and a credit track.
///
/// A row with nothing reported says so instead of drawing two empty tracks, which
/// would be read as two zeros.
fn stats_segment(row: &acpilot_stats::Row, app: &App, room: Room) -> Vec<Span<'static>> {
    use acpilot_proto::stats::IntegrationKind;

    let dim = Style::default().fg(app.theme.text_muted());
    let glyph = match row.kind {
        IntegrationKind::Provider => "◇",
        IntegrationKind::Agent => "◆",
    };
    // A stale reading is the label's colour, not a warning row: the number is still
    // the last one somebody reported, and saying when is enough to read it right.
    let label = if row.stale {
        Style::default()
            .fg(app.theme.text_muted())
            .add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(app.theme.text())
    };
    let mut spans = vec![
        Span::styled(format!(" {glyph} "), dim),
        Span::styled(row.label.clone(), label),
    ];
    if row.stale {
        spans.push(Span::styled(" (old)", dim));
    }

    if app.stats_sources {
        spans.push(Span::styled(format!(" · {} ", row.sources()), dim));
        return spans;
    }

    if row.is_quiet() {
        spans.push(Span::styled(" · nothing reported", dim));
        return spans;
    }
    if let Some(context) = &row.context {
        spans.push(Span::raw(" "));
        spans.extend(track_spans(context, app, room != Room::Tracks));
    }
    if let Some(credits) = &row.credits {
        spans.push(Span::styled("  cr ", dim));
        spans.extend(track_spans(credits, app, room == Room::Full));
    }
    spans
}

/// How many cells a track gets. Wide enough to see a half from a third, short
/// enough that two integrations and a credit bar fit side by side.
const TRACK_CELLS: usize = 8;

/// One measured quantity: the track, then the figures, then how they were got.
///
/// The colour is the source, not the level: green is "the server said so", amber is
/// "this client counted", and the theme's accent is "a plan stated it". A bar that
/// coloured by fullness instead would paint an estimate as a fact.
fn track_spans(track: &acpilot_stats::Track, app: &App, sentence: bool) -> Vec<Span<'static>> {
    use acpilot_proto::stats::StatSource;

    let colour = match track.source {
        StatSource::Server => app.theme.success(),
        StatSource::Configured => app.theme.accent(),
        StatSource::Estimated => app.theme.warning(),
        StatSource::Unknown => app.theme.text_muted(),
    };
    let dim = Style::default().fg(app.theme.text_muted());
    let mut spans = Vec::new();
    match track.fill {
        Some(fill) => {
            // `f32` to a cell count: a 0.02 fill is one cell rather than none, because
            // a bar that says "nothing" while a turn is running is a bar nobody
            // believes.
            let filled = (fill * TRACK_CELLS as f32).round() as usize;
            let filled = filled.clamp(usize::from(fill > 0.0), TRACK_CELLS);
            spans.push(Span::styled(
                "█".repeat(filled),
                Style::default().fg(colour),
            ));
            spans.push(Span::styled(
                "░".repeat(TRACK_CELLS - filled),
                Style::default().fg(app.theme.border()),
            ));
        }
        // An unknown denominator: a dashed track, never a full or an empty one. The
        // dash is the shape a person already reads as "no value".
        None => spans.push(Span::styled(
            "╌".repeat(TRACK_CELLS),
            Style::default().fg(app.theme.text_muted()),
        )),
    }
    if !sentence {
        return spans;
    }
    // `~` is the second half of "estimated": the colour says it too, for a terminal
    // that has no amber, and the mark survives a screenshot.
    let text = if track.source == StatSource::Estimated {
        format!(" ~{}", track.text)
    } else {
        format!(" {}", track.text)
    };
    spans.push(Span::styled(
        text,
        if track.source == StatSource::Unknown {
            dim
        } else {
            Style::default().fg(app.theme.text())
        },
    ));
    spans
}

/// The command palette: every command, and the task behind each one that is not
/// built.
fn draw_palette(frame: &mut Frame, app: &App, area: Rect) {
    let Some(palette) = &app.palette else {
        return;
    };
    let matches = app.matches();
    let card = middle(area, 70, 70);
    if card.width < 20 || card.height < 4 {
        return;
    }
    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Commands · {} ", matches.len()))
        .border_style(Style::default().fg(app.theme.accent()));
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let [input, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);

    let query = if palette.query.is_empty() {
        Span::styled(
            "type to filter…",
            Style::default().fg(app.theme.text_muted()),
        )
    } else {
        Span::raw(palette.query.clone())
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(app.theme.accent())),
            query,
        ])),
        input,
    );
    let typed = u16::try_from(palette.query.chars().count()).unwrap_or(u16::MAX);
    frame.set_cursor_position((input.x + 2 + typed, input.y));

    // The list follows the selection, so a filtered palette never selects a row
    // that is off the bottom of the box.
    let capacity = usize::from(list.height).max(1);
    let first = palette.selected.saturating_sub(capacity - 1);
    let rows: Vec<Line> = matches
        .iter()
        .enumerate()
        .skip(first)
        .take(capacity)
        .map(|(index, command)| {
            let chosen = index == palette.selected;
            let row = if chosen {
                Style::default()
                    .bg(app.theme.bg_panel())
                    .fg(app.theme.accent_high())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled(command.title.to_owned(), row)];
            if !command.keys.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", command.keys),
                    Style::default().fg(app.theme.text_muted()),
                ));
            }
            if let Some(task) = command.planned() {
                spans.push(Span::styled(
                    format!("  · {task}"),
                    Style::default().fg(app.theme.warning()),
                ));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), list);
}

/// The workspace content search: the query, and one row per matching line.
///
/// A jump list, so every row leads somewhere: the path and line on the left, the
/// line's text beside them, with the words that matched marked, so a row explains
/// itself without being re-read. The text is not truncated here — a row that runs off
/// the card is a row whose match was past the edge, and guessing at where to cut would
/// hide the reason it matched more often than not.
fn draw_grep(frame: &mut Frame, app: &App, area: Rect) {
    let Some(grep) = &app.grep else {
        return;
    };
    let card = middle(area, 90, 70);
    if card.width < 24 || card.height < 4 {
        return;
    }
    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Find in files · {} ", grep.matches.len()))
        .border_style(Style::default().fg(app.theme.accent()));
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let [input, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);

    let query = if grep.query.is_empty() {
        Span::styled(
            "type to search…",
            Style::default().fg(app.theme.text_muted()),
        )
    } else {
        Span::raw(grep.query.clone())
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(app.theme.accent())),
            query,
        ])),
        input,
    );
    let typed = u16::try_from(grep.query.chars().count()).unwrap_or(u16::MAX);
    frame.set_cursor_position((input.x + 2 + typed, input.y));

    // The list follows the selection, so a match far down the file list is never
    // chosen off the bottom of the card.
    let capacity = usize::from(list.height).max(1);
    let first = grep.selected.saturating_sub(capacity - 1);
    let rows: Vec<Line> = grep
        .matches
        .iter()
        .enumerate()
        .skip(first)
        .take(capacity)
        .map(|(index, hit)| {
            let chosen = index == grep.selected;
            let row = if chosen {
                Style::default()
                    .bg(app.theme.bg_panel())
                    .fg(app.theme.accent_high())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // The selection is the row's background, so the body inherits it rather
            // than starting from nothing and leaving a band of terminal colour in the
            // middle of the chosen row.
            let body = if chosen {
                row
            } else {
                Style::default().fg(app.theme.text_muted())
            };
            let mark = Style::default()
                .bg(app.theme.accent())
                .fg(app.theme.bg())
                .add_modifier(Modifier::BOLD);
            let mut spans = vec![
                Span::styled(format!("{}:{}", hit.path, hit.line), row),
                Span::styled("  ", body),
            ];
            spans.extend(highlight_line(hit.text.trim(), &grep.query, body, mark));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), list);
}

/// The line's text, with every occurrence of the query marked.
///
/// The daemon matches case-insensitively (`line.to_lowercase().contains(needle)`), and
/// this folds the same way: a row that matched with nothing marked is a row a person
/// has to read again to see why. The offsets are the original string's, never the
/// folded one's, because lowering a character can change how many bytes it takes.
///
/// A query that is empty after trimming marks nothing, which is what the daemon's
/// empty query returns too.
fn highlight_line<'a>(text: &'a str, query: &str, base: Style, mark: Style) -> Vec<Span<'a>> {
    let needle: Vec<char> = query.trim().to_lowercase().chars().collect();
    if needle.is_empty() {
        return vec![Span::styled(text, base)];
    }
    let folded: Vec<(usize, char)> = text
        .char_indices()
        .flat_map(|(index, character)| character.to_lowercase().map(move |folded| (index, folded)))
        .collect();
    let mut spans = Vec::new();
    let mut cursor = 0;
    let mut at = 0;
    while at + needle.len() <= folded.len() {
        let matched = folded[at..at + needle.len()]
            .iter()
            .zip(&needle)
            .all(|((_, folded), wanted)| folded == wanted);
        if !matched {
            at += 1;
            continue;
        }
        let start = folded[at].0;
        let last = folded[at + needle.len() - 1].0;
        let end = last + text[last..].chars().next().map_or(0, char::len_utf8);
        if start > cursor {
            spans.push(Span::styled(&text[cursor..start], base));
        }
        spans.push(Span::styled(&text[start..end], mark));
        cursor = end;
        at += needle.len();
    }
    if cursor < text.len() {
        spans.push(Span::styled(&text[cursor..], base));
    }
    spans
}

/// The keys the focused pane answers to, for `?`.
fn draw_key_list(frame: &mut Frame, app: &App, area: Rect) {
    let (pane, keys) = keys_for(app);
    let height = u16::try_from(keys.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height);
    if height < 3 || area.width < 30 {
        return;
    }
    let card = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height),
        width: area.width.min(46),
        height,
    };
    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {pane} keys · any key closes "))
        .border_style(Style::default().fg(app.theme.accent()));
    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(
                    format!("{key:<10}"),
                    Style::default().fg(app.theme.accent()),
                ),
                Span::raw((*what).to_owned()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), card);
}

/// A note about something that is designed and not built.
fn draw_toast(frame: &mut Frame, toast: &Toast, area: Rect, theme: &Theme) {
    let width = area.width.min(54);
    let height = 4.min(area.height);
    if width < 20 || height < 3 {
        return;
    }
    let card = Rect {
        x: area.x + area.width.saturating_sub(width),
        y: area.y + area.height.saturating_sub(height + 2),
        width,
        height,
    };
    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" planned · {} ", toast.task))
        .border_style(Style::default().fg(theme.warning()));
    let lines = vec![
        Line::from(Span::styled(
            toast.feature.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "not implemented in this build",
            Style::default().fg(theme.text_muted()),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), card);
}

/// The keys a pane being read answers to, in either pane.
///
/// One list for both, because the keys are one list: the terminal's copy mode and
/// the transcript are the same gesture over different media.
const COPY_KEYS: [(&str, &str); 11] = [
    ("j / k", "line down / up"),
    ("h / l", "left / right"),
    ("ctrl-u / ctrl-d", "page back / forward"),
    ("g / G", "oldest line / live"),
    ("/", "find in what is above"),
    ("n / N", "next match, older / newer"),
    ("v", "select"),
    ("V", "select whole lines"),
    ("y", "copy and leave"),
    ("esc", "leave"),
    ("?", "these keys"),
];

/// The keys the pane with the keyboard answers to, and what to call it.
///
/// The editor's list is the keymap's, because those keys are: rebinding
/// `move-down` or the palette changes what this card says, and the client's keys
/// are appended after the pane's because they are live there too. A pane whose own
/// keys are still the crate's match arms keeps its written list, which names the
/// client's keys in the pane alongside its own — the same card a person has always
/// read, and the place the next piece of the keymap work lands.
fn keys_for(app: &App) -> (&'static str, Vec<(String, &'static str)>) {
    let reading = app.focus == Focus::Transcript
        || (app.focus == Focus::Terminal
            && app
                .terminal
                .as_ref()
                .is_some_and(|pane| pane.mode == PaneMode::Copy));
    if reading {
        return (
            "copy mode",
            COPY_KEYS
                .iter()
                .map(|(key, what)| ((*key).to_owned(), *what))
                .collect(),
        );
    }
    // No insert-mode list here: `?` is a character in insert mode, so the panel
    // cannot open while those keys are the pane's. The way in is on the normal-mode
    // list and the insert keys are in `docs/notes/tui-shell.md`, which is the one
    // place a person reading the settings can find them.
    // The editor's verbs come from the table, then the client's — they are the same
    // in every pane and a person who rebound the palette expects the card to say so.
    // The other panes keep their written list, which is shorter than a table dump and
    // already names the client's keys among them.
    if app.focus == Focus::Editor {
        let mut rows = app.keymap.rows(Context::Editor);
        rows.extend(app.keymap.rows(Context::App));
        // Not in the table: one plans a task and the other asks the daemon, and the
        // one that plans is not a verb a person should be able to bind.
        rows.push(("f12".to_owned(), "go to definition"));
        rows.push(("shift-f12".to_owned(), "find references"));
        return (Focus::Editor.label(), rows);
    }
    let rows = pane_keys(app.focus)
        .iter()
        .map(|(key, what)| ((*key).to_owned(), *what))
        .collect();
    (app.focus.label(), rows)
}

/// The keys a pane answers to, as the which-key panel lists them.
fn pane_keys(focus: Focus) -> &'static [(&'static str, &'static str)] {
    match focus {
        Focus::Files => &[
            ("j / k", "move"),
            ("h / l", "collapse / expand"),
            ("enter", "open"),
            ("/", "find a file"),
            ("H", "hidden files"),
            ("tab", "next pane"),
            ("ctrl-←/→", "narrower / wider"),
            ("?", "these keys"),
        ],
        // The editor's keys are the keymap's; this arm exists because the match is on
        // every pane and the key list asks the table instead.
        Focus::Editor => &[],
        Focus::References => &[
            ("j / k", "move"),
            ("enter", "open the use"),
            ("shift-f12", "another word"),
            ("esc", "back to the buffer"),
            ("tab", "next pane"),
        ],
        Focus::Sessions => &[
            ("j / k", "choose"),
            ("enter", "open"),
            ("i", "compose"),
            ("tab", "read the transcript"),
            ("ctrl-←/→", "narrower / wider"),
            ("n", "new session"),
            ("d", "end session"),
            ("c", "cancel turn"),
        ],
        // Reached only by a pane that is not being read: once it is, this list is
        // replaced by the copy keys above.
        Focus::Transcript => &[
            ("j / k", "scroll"),
            ("v", "select"),
            ("y", "copy"),
            ("esc", "back to the list"),
            ("tab", "next pane"),
        ],
        Focus::Prompt => &[
            ("enter", "send"),
            ("esc", "back to the list"),
            ("ctrl-t", "terminal pane"),
            ("tab", "read the transcript"),
            ("ctrl-p", "commands"),
            ("f4", "where the stats came from"),
        ],
        Focus::Outline => &[
            ("j / k", "choose a definition"),
            ("enter", "go to its line"),
            ("esc", "back to the buffer"),
        ],
        Focus::ProjectMap => &[
            ("j / k", "choose an area"),
            ("enter", "read its summary"),
            ("r", "read the map again"),
            ("esc", "back to the buffer"),
        ],
        Focus::Terminal => &[
            ("ctrl-t", "leave the pane"),
            ("f2", "copy mode"),
            ("f4", "where the stats came from"),
            ("ctrl-c", "interrupt the program"),
        ],
    }
}

/// A git status as the tree's gutter colour.
///
/// A clean row's letter is a blank, and the blank is muted like the rest of the
/// furniture rather than unset: a column that changes colour per row is a column
/// that reads as five different things instead of one.
fn git_colour(status: Option<acpilot_proto::file::FileChange>, theme: &Theme) -> Color {
    match status {
        None => theme.text_muted(),
        Some(acpilot_proto::file::FileChange::Modified) => theme.warning(),
        Some(acpilot_proto::file::FileChange::Added) => theme.success(),
        Some(acpilot_proto::file::FileChange::Untracked) => theme.accent(),
        Some(acpilot_proto::file::FileChange::Deleted) => theme.danger(),
        Some(acpilot_proto::file::FileChange::Conflicted) => theme.accent_high(),
    }
}

/// The card an agent's question gets, with the diff when the question is a write.
///
/// P6-8 is the modal and P6-9 the diff review; the design draws both in one card
/// because that is where the decision is made — a review in a separate pane is a
/// review nobody reads before answering.
fn draw_permission(
    frame: &mut Frame,
    request: &PermissionRequest,
    at: usize,
    total: usize,
    diff: Option<&Diff>,
    area: Rect,
    theme: &Theme,
) {
    // The preview is the write's own: a read or a terminal has a detail, not a
    // diff, and showing the seeded write beside a shell's question would be a
    // preview of an edit nobody asked about.
    let diff =
        diff.filter(|_| request.kind == acpilot_proto::permission::PermissionKind::FileWrite);
    let card = if diff.is_some() {
        middle(area, 84, 76)
    } else {
        centred(area)
    };
    frame.render_widget(Clear, card);
    let title = if total > 1 {
        format!(" Permission · {at}/{total} ")
    } else {
        " Permission needed ".to_owned()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(theme.warning()));
    let room = usize::from(card.height.saturating_sub(2)).saturating_sub(3);
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "{} · {} · {}",
                permission_kind_label(request.kind),
                request.session_id,
                request.title
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        // Each question in a queue names its place, so `Tab` has something to
        // count and an answer says which of them it was.
        Line::from(Span::styled(
            format!("request {at} of {total}"),
            Style::default().fg(theme.text_muted()),
        )),
    ];
    // The detail is prose with newlines in it, not one long line: a request that
    // describes three changes describes them on three rows.
    lines.extend(
        request
            .detail
            .lines()
            .map(|line| Line::from(line.to_owned())),
    );
    if let Some(diff) = diff {
        lines.push(Line::from(""));
        for hunk in &diff.hunks {
            if lines.len() + 2 > room {
                break;
            }
            lines.push(Line::from(Span::styled(
                hunk.header.clone(),
                Style::default().fg(theme.accent()),
            )));
            for change in &hunk.lines {
                if lines.len() + 2 > room {
                    break;
                }
                let (sign, colour) = match change.kind {
                    DiffKind::Added => ("+", theme.success()),
                    DiffKind::Removed => ("-", theme.danger()),
                    DiffKind::Context => (" ", theme.text_muted()),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{sign} "), Style::default().fg(colour)),
                    Span::raw(change.text.clone()),
                ]));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        permission_hint(request, at, total),
        Style::default().fg(theme.warning()),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        card,
    );
}

/// What the card calls the tool the request is about.
fn permission_kind_label(kind: acpilot_proto::permission::PermissionKind) -> &'static str {
    use acpilot_proto::permission::PermissionKind;
    match kind {
        PermissionKind::FileWrite => "file write",
        PermissionKind::FileRead => "file read",
        PermissionKind::Terminal => "terminal",
        PermissionKind::ToolCall => "tool call",
        PermissionKind::Other => "request",
    }
}

/// The keys the card is actually offering, built from the request's own options.
///
/// A card that printed a fixed line would advertise `a allow always` on a request
/// the wire sent with no always option, and a key that is drawn but does nothing
/// is worse than one that is not drawn.
fn permission_hint(request: &PermissionRequest, at: usize, total: usize) -> String {
    if request.options.is_empty() {
        return "no answer this build can give · Esc later".to_owned();
    }
    let mut parts: Vec<String> = request
        .options
        .iter()
        .map(|option| {
            format!(
                "{} {}",
                crate::app::option_key(*option),
                crate::app::option_label(*option)
            )
        })
        .collect();
    if total > 1 {
        parts.push(format!("Tab next · {at}/{total}"));
    }
    parts.push("Esc later".to_owned());
    parts.push("every answer is audited".to_owned());
    parts.join(" · ")
}

/// The source-control pane: changed paths on the left, the selected file's diff
/// on the right.
///
/// Modal because it is about the repository rather than the buffer, and because its
/// writes — moving a path across the index, then committing — are decisions that
/// should not be one keystroke away from a pane that edits text. `s`/`u` stage and
/// unstage the selected path through the daemon; the composer still confirms the
/// commit.
fn draw_source_control(frame: &mut Frame, app: &App, area: Rect) {
    let Some(source) = &app.source else {
        return;
    };
    let card = middle(area, 110, 88);
    if card.width < 50 || card.height < 8 {
        return;
    }
    frame.render_widget(Clear, card);
    let branch = source.status.branch.as_deref().unwrap_or("(detached)");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " Source control · {branch} · {} changed · {} staged ",
            source.status.files.len(),
            source.staged()
        ))
        .border_style(Style::default().fg(app.theme.accent()));
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let files_width = (body.width / 3).clamp(18, 38);
    let [files, patch] =
        Layout::horizontal([Constraint::Length(files_width), Constraint::Min(10)]).areas(body);

    if source.loading {
        frame.render_widget(
            Paragraph::new("reading the index…").style(Style::default().fg(app.theme.text_muted())),
            files,
        );
    } else if source.status.files.is_empty() {
        frame.render_widget(
            Paragraph::new("no changed files").style(Style::default().fg(app.theme.text_muted())),
            files,
        );
    } else {
        let start = first_visible(
            source.selected,
            source.status.files.len(),
            usize::from(files.height),
        );
        let rows: Vec<Line<'static>> = source
            .status
            .files
            .iter()
            .enumerate()
            .skip(start)
            .take(usize::from(files.height))
            .map(|(index, entry)| {
                let chosen = index == source.selected;
                let style = if chosen {
                    Style::default()
                        .fg(app.theme.accent_high())
                        .bg(app.theme.bg_panel())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(app.theme.text())
                };
                let marks = format!(
                    "{}{} ",
                    change_letter(entry.index),
                    change_letter(entry.worktree)
                );
                let mark_style = Style::default()
                    .fg(change_colour(entry.worktree, &app.theme))
                    .bg(if chosen {
                        app.theme.bg_panel()
                    } else {
                        app.theme.bg()
                    });
                Line::from(vec![
                    Span::styled(marks, mark_style),
                    Span::styled(entry.path.clone(), style),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), files);
    }
    if let Some(error) = &source.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .style(Style::default().fg(app.theme.danger()))
                .wrap(Wrap { trim: false }),
            patch,
        );
    } else if source.loading {
        frame.render_widget(
            Paragraph::new("reading the diff…").style(Style::default().fg(app.theme.text_muted())),
            patch,
        );
    } else {
        let section = source.section();
        if section.is_empty() {
            frame.render_widget(
                Paragraph::new("no diff for this path yet · s stages it")
                    .style(Style::default().fg(app.theme.text_muted()))
                    .wrap(Wrap { trim: false }),
                patch,
            );
        } else {
            let height = usize::from(patch.height);
            let lines: Vec<Line<'static>> = section
                .iter()
                .skip(source.scroll)
                .take(height)
                .map(|line| diff_line(line, &app.theme))
                .collect();
            frame.render_widget(Paragraph::new(lines), patch);
        }
    }

    let total = source.section().len();
    let position = if total > 0 {
        format!(
            " · lines {}-{}",
            source.scroll + 1,
            (source.scroll + usize::from(patch.height)).min(total)
        )
    } else {
        String::new()
    };
    let footer_line = Line::from(Span::styled(
        format!(
            " j/k files · s stage · u unstage · pgup/pgdn diff{position} · c compose · g generate · r refresh · esc close "
        ),
        Style::default().fg(app.theme.warning()),
    ));
    frame.render_widget(Paragraph::new(footer_line), footer);
}

fn change_letter(kind: acpilot_proto::git::GitChangeKind) -> char {
    use acpilot_proto::git::GitChangeKind::{
        Added, Copied, Deleted, Ignored, Modified, Renamed, TypeChanged, Unmerged, Unmodified,
        Untracked,
    };
    match kind {
        Unmodified | Ignored => '·',
        Modified => 'M',
        Added => 'A',
        Deleted => 'D',
        Renamed => 'R',
        Copied => 'C',
        TypeChanged => 'T',
        Unmerged => 'U',
        Untracked => '?',
    }
}

fn change_colour(kind: acpilot_proto::git::GitChangeKind, theme: &Theme) -> Color {
    use acpilot_proto::git::GitChangeKind::{Added, Deleted, Renamed, Unmerged, Untracked};
    match kind {
        Added | Untracked => theme.success(),
        Deleted => theme.danger(),
        Renamed => theme.accent_high(),
        Unmerged => theme.warning(),
        _ => theme.text_muted(),
    }
}

/// One line of a unified diff, coloured by what its first character says.
fn diff_line(text: &str, theme: &Theme) -> Line<'static> {
    let colour = match text.chars().next() {
        Some('+') => theme.success(),
        Some('-') => theme.danger(),
        Some('@') => theme.accent(),
        Some('d') => theme.accent_high(),
        _ => theme.text_muted(),
    };
    Line::from(Span::styled(text.to_owned(), Style::default().fg(colour)))
}

/// The commit composer, over the pane it will commit from.
///
/// The message is shown with a caret rather than sent to a terminal's line editor:
/// the body is paragraphs, and editing it has to work before the one action that
/// writes. The footer changes with the state, so `Ctrl-S` is visibly a question and
/// `y` is visibly the answer.
fn draw_commit_composer(frame: &mut Frame, app: &App, area: Rect) {
    let Some(composer) = &app.commit else {
        return;
    };
    let card = middle(area, 84, 64);
    if card.width < 32 || card.height < 6 {
        return;
    }
    frame.render_widget(Clear, card);
    let staged = app.source.as_ref().map_or(0, |source| source.staged());
    let state = if composer.generating {
        "drafting"
    } else if composer.committing {
        "committing"
    } else if composer.confirming {
        "confirm"
    } else {
        "edit"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Commit message · {staged} staged · {state} "))
        .border_style(Style::default().fg(if composer.confirming {
            app.theme.warning()
        } else {
            app.theme.accent()
        }));
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let lines = composer_lines(composer, &app.theme);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);

    let (text, colour) = if let Some(error) = &composer.error {
        (format!(" {error} "), app.theme.danger())
    } else if composer.generating {
        (
            " the session is drafting from the staged diff… ".to_owned(),
            app.theme.warning(),
        )
    } else if composer.committing {
        (
            " committing the staged index… ".to_owned(),
            app.theme.warning(),
        )
    } else if composer.confirming {
        (
            format!(" commit {staged} staged? y confirms · n cancels "),
            app.theme.warning(),
        )
    } else {
        (
            " ctrl-g generate from session · ctrl-s reviews and commits · esc closes ".to_owned(),
            app.theme.text_muted(),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, Style::default().fg(colour)))),
        footer,
    );
}

/// The message as lines, with the caret drawn as a block or a bar.
fn composer_lines(composer: &crate::app::CommitComposer, theme: &Theme) -> Vec<Line<'static>> {
    let (cursor_line, cursor_column) = composer.line_col();
    let mut lines = Vec::new();
    for (line_index, text) in composer.text.split('\n').enumerate() {
        let mut spans = Vec::new();
        if line_index == cursor_line {
            let characters: Vec<char> = text.chars().collect();
            for (column, character) in characters.iter().enumerate() {
                if column == cursor_column {
                    spans.push(Span::styled(
                        character.to_string(),
                        Style::default().fg(theme.bg()).bg(theme.accent()),
                    ));
                } else {
                    spans.push(Span::styled(
                        character.to_string(),
                        Style::default().fg(theme.text()),
                    ));
                }
            }
            if cursor_column >= characters.len() {
                spans.push(Span::styled("▏", Style::default().fg(theme.accent())));
            }
        } else {
            spans.push(Span::styled(
                text.to_owned(),
                Style::default().fg(theme.text()),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// The diff review: the agent's edit, hunk by hunk, with a decision on each.
///
/// Drawn over the permission card rather than beside it, because the card is the
/// question and this is how it is answered: a review in a pane of its own would be
/// a review nobody reads before pressing `y`. The hunks are scrolled by the cursor
/// rather than by an offset — the hunk under the keyboard is the top of the box, so
/// moving the cursor is the only scrolling there is.
fn draw_review(frame: &mut Frame, app: &App, area: Rect) {
    let Some(review) = &app.review else {
        return;
    };
    let card = middle(area, 96, 84);
    if card.width < 34 || card.height < 8 || review.diff.hunks.is_empty() {
        return;
    }
    frame.render_widget(Clear, card);
    let total = review.diff.hunks.len();
    let accepted = review.accepted_hunks();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Review · {} ", review.diff.path))
        .border_style(Style::default().fg(app.theme.accent()));
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let [body, keys_row] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    let height = usize::from(body.height);
    // The hunks are drawn from the top whenever the ones up to the cursor fit, so
    // that deciding a hunk does not scroll the hunk just decided out of sight. Past
    // that the cursor's hunk is the top of the box, which is the only scrolling
    // there is.
    let up_to_cursor: usize = review
        .diff
        .hunks
        .iter()
        .take(review.cursor + 1)
        .map(|hunk| 1 + hunk.lines.len())
        .sum();
    let start = if up_to_cursor > height {
        review.cursor
    } else {
        0
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, hunk) in review.diff.hunks.iter().enumerate().skip(start) {
        if lines.len() >= height {
            break;
        }
        let chosen = index == review.cursor;
        let agreed = review.accepted.get(index).copied().unwrap_or(true);
        let (word, colour) = if agreed {
            ("accepted", app.theme.success())
        } else {
            ("rejected", app.theme.danger())
        };
        let marker = if chosen { "▸" } else { " " };
        let mut heading = Style::default().fg(app.theme.accent());
        if chosen {
            heading = heading.add_modifier(Modifier::BOLD);
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), heading),
            Span::styled(hunk.header.clone(), Style::default().fg(app.theme.accent())),
            Span::styled(format!("  · {word}"), Style::default().fg(colour)),
        ]));
        if review.split {
            split_hunk(&mut lines, hunk, body.width, height, &app.theme);
        } else {
            for change in &hunk.lines {
                if lines.len() >= height {
                    break;
                }
                lines.push(inline_change(change, &app.theme));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), body);

    let next = if review.split { "inline" } else { "split" };
    let footer = Line::from(Span::styled(
        format!(
            " hunk {}/{total} · {accepted} accepted · a accept · x reject · A all · X none · s {next} · y answer · n decline · esc close ",
            review.cursor + 1
        ),
        Style::default().fg(app.theme.warning()),
    ));
    frame.render_widget(Paragraph::new(footer), keys_row);
}

/// One line of a unified diff: the sigil is the diff's, the colour is the kind's.
fn inline_change(change: &acpilot_demo::DiffLine, theme: &Theme) -> Line<'static> {
    let (sign, colour) = match change.kind {
        DiffKind::Added => ("+", theme.success()),
        DiffKind::Removed => ("-", theme.danger()),
        DiffKind::Context => (" ", theme.text_muted()),
    };
    Line::from(vec![
        Span::styled(format!("  {sign} "), Style::default().fg(colour)),
        Span::raw(change.text.clone()),
    ])
}

/// A hunk as two columns, old on the left and new on the right.
///
/// Aligned the way a diff aligns them rather than as two independent lists: a
/// context line is on both sides, a removal only on the left and an addition only
/// on the right, so the same row of the card is the same place in the file.
fn split_hunk(
    lines: &mut Vec<Line<'static>>,
    hunk: &acpilot_demo::Hunk,
    width: u16,
    height: usize,
    theme: &Theme,
) {
    let column = usize::from(width.saturating_sub(1)) / 2;
    for change in &hunk.lines {
        if lines.len() >= height {
            return;
        }
        let (left, right) = match change.kind {
            DiffKind::Removed => (Some(change.text.as_str()), None),
            DiffKind::Added => (None, Some(change.text.as_str())),
            DiffKind::Context => (Some(change.text.as_str()), Some(change.text.as_str())),
        };
        let (left_colour, right_colour) = match change.kind {
            DiffKind::Removed => (theme.danger(), theme.text_muted()),
            DiffKind::Added => (theme.text_muted(), theme.success()),
            DiffKind::Context => (theme.text_muted(), theme.text_muted()),
        };
        lines.push(Line::from(vec![
            Span::styled(cell(left, column), Style::default().fg(left_colour)),
            Span::styled("│", Style::default().fg(theme.border())),
            Span::styled(cell(right, column), Style::default().fg(right_colour)),
        ]));
    }
}

/// Text padded or cut to `width` characters, so that two columns line up.
///
/// Characters rather than cells: a diff of code is ASCII in the overwhelming
/// case, and the alternative — measuring every glyph in a card — would make the
/// review's drawing answer a question the editor already answers properly.
fn cell(text: Option<&str>, width: usize) -> String {
    let mut out: String = text
        .unwrap_or_default()
        .chars()
        .take(width.saturating_sub(1))
        .collect();
    let len = out.chars().count();
    out.push_str(&" ".repeat(width.saturating_sub(len)));
    out
}

/// The list of a session's terminals, when there is more than one.
///
/// The command is the label and the id is the subtitle, in that order: two shells
/// running `make` are told apart by which one a person meant, and two ids are told
/// apart by nothing.
fn draw_terminal_picker(frame: &mut Frame, app: &App, area: Rect) {
    let card = centred(area);
    frame.render_widget(Clear, card);
    let mut lines: Vec<Line> = app
        .terminals
        .iter()
        .enumerate()
        .map(|(index, terminal)| {
            let chosen = index == app.picker_selected;
            let marker = if chosen { "▸ " } else { "  " };
            let state = if terminal.alive { "" } else { " · exited" };
            // A pane no session owns is not this session's, and the picker says so
            // rather than letting it pass for one the agent opened here. Where it runs
            // is part of that: a shell on another host is not the shell this window
            // implies, and a pane that will not take this session's commands is exactly
            // the one a person needs an address for.
            let owner = pane_origin(terminal)
                .map(|origin| format!(" · no session · {origin}"))
                .unwrap_or_default();
            let row = if chosen {
                Style::default()
                    .bg(app.theme.bg_panel())
                    .fg(app.theme.accent_high())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let id = if chosen {
                Style::default().fg(app.theme.text())
            } else {
                Style::default().fg(app.theme.text_muted())
            };
            Line::from(vec![
                Span::styled(format!("{marker}{}{state}{owner}", terminal.command), row),
                Span::styled(format!("  {}", terminal.id), id),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter opens · j/k choose · n starts a shell · Esc closes",
        Style::default().fg(app.theme.accent()),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Terminals · {} ", app.terminals.len()))
        .border_style(Style::default().fg(app.theme.accent()));
    frame.render_widget(Paragraph::new(lines).block(block), card);
}

/// The middle of the screen, two thirds across and half as tall.
fn centred(area: Rect) -> Rect {
    middle(area, 70, 50)
}

/// A centred box `width` per cent across and `height` per cent tall.
fn middle(area: Rect, width: u16, height: u16) -> Rect {
    // Percentages above 100 are clamped rather than subtracted from: a caller asking
    // for a card wider than the area means "as wide as it goes", and the arithmetic
    // below should not underflow into a panic on the way to saying so.
    let width = width.min(100);
    let height = height.min(100);
    let top = (100 - height) / 2;
    let side = (100 - width) / 2;
    let [_, middle, _] = Layout::vertical([
        Constraint::Percentage(top),
        Constraint::Percentage(height),
        Constraint::Percentage(top),
    ])
    .areas(area);
    let [_, inner, _] = Layout::horizontal([
        Constraint::Percentage(side),
        Constraint::Percentage(width),
        Constraint::Percentage(side),
    ])
    .areas(middle);
    inner
}

fn border_style(focused: bool, theme: &Theme) -> Style {
    if focused {
        Style::default().fg(theme.accent())
    } else {
        Style::default().fg(theme.text_muted())
    }
}

fn entry_style(kind: Kind, theme: &Theme) -> Style {
    match kind {
        Kind::User => Style::default().fg(theme.accent()),
        Kind::Assistant => Style::default(),
        Kind::Thought => Style::default()
            .fg(theme.text_muted())
            .add_modifier(Modifier::ITALIC),
        Kind::Tool => Style::default().fg(theme.warning()),
        Kind::Notice => Style::default().fg(theme.accent_high()),
    }
}

fn status_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Idle => "idle",
        SessionStatus::Busy => "busy",
        SessionStatus::AwaitingPermission => "waiting",
        SessionStatus::Closed => "closed",
        SessionStatus::Failed => "failed",
    }
}

fn status_colour(status: SessionStatus, theme: &Theme) -> Color {
    match status {
        SessionStatus::Idle => theme.success(),
        SessionStatus::Busy => theme.warning(),
        SessionStatus::AwaitingPermission => theme.accent_high(),
        SessionStatus::Closed => theme.text_muted(),
        SessionStatus::Failed => theme.danger(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Entry;
    use acpilot_proto::permission::{PermissionKind, PermissionOption};
    use acpilot_proto::session::SessionSummary;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &mut App) -> String {
        render_at(app, 120, 34)
    }

    fn render_at(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
        terminal
            .draw(|frame| {
                draw(frame, app);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn permission(
        id: &str,
        kind: PermissionKind,
        options: Vec<PermissionOption>,
        title: &str,
    ) -> PermissionRequest {
        PermissionRequest {
            id: id.to_owned(),
            session_id: "s1".to_owned(),
            kind,
            title: title.to_owned(),
            detail: "detail".to_owned(),
            options,
            created_at: "2026-10-24T00:00:00Z".to_owned(),
        }
    }

    fn app_with_session() -> App {
        let mut app = App::new("http://127.0.0.1:7777");
        app.connected = true;
        app.sessions.push(SessionSummary {
            id: "s1".to_owned(),
            workspace_id: "work".to_owned(),
            agent_id: "mock".to_owned(),
            title: "wire the tui".to_owned(),
            status: SessionStatus::Idle,
            created_at: "2026-10-24T00:00:00Z".to_owned(),
            updated_at: "2026-10-24T00:00:00Z".to_owned(),
        });
        app.open = Some("s1".to_owned());
        app.entries.push(Entry {
            kind: Kind::Assistant,
            text: "the daemon has the turn".to_owned(),
            ..Entry::default()
        });
        app.status = "connected · acpilotd 0.1.0".to_owned();
        app
    }

    #[test]
    fn the_shell_shows_the_panes_it_promises() {
        let text = render(&mut app_with_session());
        assert!(text.contains("Assistant · 1"), "{text}");
        assert!(text.contains("wire the tui"), "{text}");
        assert!(text.contains("the daemon has the turn"), "{text}");
        assert!(text.contains("Prompt"), "{text}");
        assert!(text.contains("127.0.0.1:7777"), "{text}");
    }

    #[test]
    fn an_empty_transcript_says_so_instead_of_being_blank() {
        let mut app = app_with_session();
        app.entries.clear();
        assert!(render(&mut app).contains("no transcript yet"));
    }

    #[test]
    fn the_permission_card_names_the_tool_and_the_queue() {
        let mut app = app_with_session();
        app.permissions.push(permission(
            "p1",
            PermissionKind::Terminal,
            vec![PermissionOption::AllowOnce, PermissionOption::Deny],
            "Run make",
        ));
        app.permissions.push(permission(
            "p2",
            PermissionKind::FileWrite,
            vec![PermissionOption::AllowAlways, PermissionOption::Deny],
            "Write src/main.rs",
        ));
        app.permission_card = true;
        app.permission_at = 1;
        let text = render(&mut app);
        assert!(text.contains("Permission · 2/2"), "{text}");
        assert!(text.contains("request 2 of 2"), "{text}");
        assert!(text.contains("file write"), "{text}");
        assert!(text.contains("a allow always · this session"), "{text}");
        assert!(
            !text.contains("y allow once"),
            "the request does not offer it: {text}"
        );
        // The card is narrow enough to wrap its footer, so the sentence is read
        // as its two words rather than as one line the buffer split.
        assert!(text.contains("every answer"), "{text}");
        assert!(text.contains("audited"), "{text}");
    }

    #[test]
    fn the_card_is_drawn_over_a_streaming_transcript() {
        let mut app = app_with_session();
        app.sessions[0].status = SessionStatus::Busy;
        app.permissions.push(permission(
            "p1",
            PermissionKind::FileWrite,
            vec![PermissionOption::AllowOnce, PermissionOption::Deny],
            "Write src/main.rs",
        ));
        app.permission_card = true;
        app.apply(crate::backend::Update::Event(
            acpilot_proto::event::SessionEvent {
                seq: 1,
                session_id: "s1".to_owned(),
                kind: acpilot_proto::event::SessionEventKind::MessageChunk,
                at: "2026-10-24T00:00:00Z".to_owned(),
                payload: serde_json::json!({ "role": "assistant", "content": "still arriving" }),
            },
        ));
        let text = render(&mut app);
        assert!(text.contains("Permission needed"), "{text}");
        assert!(text.contains("Write src/main.rs"), "{text}");
        assert!(text.contains("y allow once"), "{text}");
    }

    #[test]
    fn the_permission_card_covers_the_middle_of_the_screen() {
        let mut app = app_with_session();
        app.permissions.push(PermissionRequest {
            id: "p1".to_owned(),
            session_id: "s1".to_owned(),
            kind: PermissionKind::FileWrite,
            title: "Write src/main.rs".to_owned(),
            detail: "+fn main() {}".to_owned(),
            options: vec![PermissionOption::AllowOnce, PermissionOption::Deny],
            created_at: "2026-10-24T00:00:00Z".to_owned(),
        });
        app.permission_card = true;
        let text = render(&mut app);
        assert!(text.contains("Permission needed"), "{text}");
        assert!(text.contains("Write src/main.rs"), "{text}");
        assert!(text.contains("y allow once"), "{text}");
    }

    #[test]
    fn the_terminal_pane_draws_the_program_and_says_how_to_leave() {
        let mut app = app_with_session();
        app.set_pane_size(TerminalSize { cols: 60, rows: 20 });
        app.apply(crate::backend::Update::TerminalOpened {
            terminal: acpilot_proto::terminal::TerminalSummary {
                id: "agent-terminal-1".to_owned(),
                session_id: Some("s1".to_owned()),
                workspace: None,
                host: None,
                size: TerminalSize { cols: 60, rows: 20 },
                alive: true,
                command: "/bin/sh -c make".to_owned(),
            },
            replay_bytes: 0,
        });
        app.apply(crate::backend::Update::TerminalData(
            b"make: *** no rule\r\n".to_vec(),
        ));

        let text = render(&mut app);
        assert!(text.contains("make: *** no rule"), "{text}");
        assert!(text.contains("ctrl-t leaves"), "{text}");
        assert!(
            !text.contains("no transcript yet"),
            "the terminal replaces the transcript while it is open: {text}"
        );
    }

    #[test]
    fn a_terminal_scrolled_back_says_so_rather_than_looking_stuck() {
        let mut app = app_with_session();
        app.set_pane_size(TerminalSize { cols: 60, rows: 8 });
        app.apply(crate::backend::Update::TerminalOpened {
            terminal: acpilot_proto::terminal::TerminalSummary {
                id: "agent-terminal-1".to_owned(),
                session_id: Some("s1".to_owned()),
                workspace: None,
                host: None,
                size: TerminalSize { cols: 60, rows: 8 },
                alive: true,
                command: "/bin/sh -c make".to_owned(),
            },
            replay_bytes: 0,
        });
        let printed: String = (0..20).map(|line| format!("line {line}\r\n")).collect();
        app.apply(crate::backend::Update::TerminalData(printed.into_bytes()));
        app.terminal.as_mut().expect("a pane").wheel(3);

        // The title is the only thing that tells a scrolled pane from a program
        // that stopped printing, so it has to say both where you are and how to
        // come back.
        let text = render(&mut app);
        assert!(text.contains("behind 3/"), "{text}");
        assert!(text.contains("any key returns"), "{text}");
    }

    /// The drawn cells, for a test that is about a style rather than a character.
    ///
    /// The drawn viewports are handed back to the app the way the event loop hands
    /// them back: a click counts against the cells the frame actually got, and a
    /// test that skipped that step would be clicking a layout nobody was given.
    fn cells(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
        let mut viewports = None;
        terminal
            .draw(|frame| {
                viewports = Some(draw(frame, app));
            })
            .expect("draw");
        if let Some(viewports) = viewports {
            app.set_viewports(viewports);
        }
        terminal.backend().buffer().clone()
    }

    #[test]
    fn a_selection_dragged_over_the_terminal_is_drawn_reversed() {
        let mut app = app_with_session();
        app.set_pane_size(TerminalSize { cols: 60, rows: 8 });
        app.apply(crate::backend::Update::TerminalOpened {
            terminal: acpilot_proto::terminal::TerminalSummary {
                id: "agent-terminal-1".to_owned(),
                session_id: Some("s1".to_owned()),
                workspace: None,
                host: None,
                size: TerminalSize { cols: 60, rows: 8 },
                alive: true,
                command: "/bin/sh -c make".to_owned(),
            },
            replay_bytes: 0,
        });
        app.apply(crate::backend::Update::TerminalData(
            b"hello world\r\n".to_vec(),
        ));

        // The pane's own corner comes from the drawing, so the drag is aimed at the
        // cells a person can see rather than at arithmetic copied from the layout —
        // which means one frame has to be drawn before there is anything to click.
        let _ = cells(&mut app, 120, 34);
        let area = app.terminal_area.expect("the pane was drawn");
        let down = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(down(area.x, area.y));
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x + 4,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 4,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.terminal.as_ref().expect("a pane").has_selection());

        let drawn = cells(&mut app, 120, 34);
        for (offset, selected) in [(0, true), (3, true), (5, false)] {
            let cell = &drawn[(area.x + offset, area.y)];
            assert_eq!(
                cell.modifier.contains(Modifier::REVERSED),
                selected,
                "cell {offset} of the row ({:?})",
                cell.symbol()
            );
        }
    }

    #[test]
    fn a_selection_dragged_over_the_editor_is_drawn_reversed() {
        let mut app = demo_with_theme();
        app.focus = Focus::Editor;
        // The pane measures itself against the cells a frame gives it, so one frame
        // has to be drawn before there is a box to aim a drag at.
        let _ = cells(&mut app, 120, 34);
        let area = app.editor_area.expect("the pane was drawn");
        let text = area.pane.x + area.gutter;
        let row = area.pane.y + 3;
        let mouse = |kind, column| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), text + 2));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), text + 6));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), text + 6));
        assert!(app.editor.as_ref().expect("a buffer").has_selection());

        let drawn = cells(&mut app, 120, 34);
        for (offset, selected) in [(1, false), (2, true), (5, true), (6, false)] {
            let cell = &drawn[(text + offset, row)];
            assert_eq!(
                cell.modifier.contains(Modifier::REVERSED),
                selected,
                "cell {offset} of the row ({:?})",
                cell.symbol()
            );
        }
    }

    #[test]
    fn a_finished_terminal_keeps_its_last_screen_and_says_it_ended() {
        let mut app = app_with_session();
        app.apply(crate::backend::Update::TerminalOpened {
            terminal: acpilot_proto::terminal::TerminalSummary {
                id: "agent-terminal-1".to_owned(),
                session_id: Some("s1".to_owned()),
                workspace: None,
                host: None,
                size: TerminalSize { cols: 60, rows: 20 },
                alive: true,
                command: "/bin/sh -c make".to_owned(),
            },
            replay_bytes: 0,
        });
        app.apply(crate::backend::Update::TerminalData(b"done".to_vec()));
        app.apply(crate::backend::Update::TerminalExited { code: Some(0) });

        let text = render(&mut app);
        assert!(text.contains("done"), "{text}");
        assert!(text.contains("exited"), "{text}");
    }

    fn summary(id: &str, command: &str, alive: bool) -> acpilot_proto::terminal::TerminalSummary {
        acpilot_proto::terminal::TerminalSummary {
            id: id.to_owned(),
            session_id: Some("s1".to_owned()),
            workspace: None,
            host: None,
            size: TerminalSize { cols: 60, rows: 20 },
            alive,
            command: command.to_owned(),
        }
    }

    #[test]
    fn the_terminal_picker_lists_the_sessions_panes_and_the_ones_nobody_owns() {
        let mut app = app_with_session();
        app.terminals
            .push(summary("agent-terminal-1", "/bin/sh -c make", true));
        app.terminals
            .push(summary("agent-terminal-2", "/bin/sh -c cargo test", false));
        let mut orphan = summary("agent-terminal-3", "tail -f", true);
        orphan.session_id = None;
        orphan.workspace = Some("build".to_owned());
        orphan.host = Some("ssh:box".to_owned());
        app.terminals.push(orphan);
        app.terminal_picker = true;
        app.picker_selected = 1;

        let text = render(&mut app);
        assert!(text.contains("Terminals · 3"), "{text}");
        assert!(text.contains("/bin/sh -c make"), "{text}");
        assert!(text.contains("/bin/sh -c cargo test"), "{text}");
        assert!(text.contains("exited"), "an ended program says so: {text}");
        assert!(
            text.contains("no session"),
            "a pane with no owner says so: {text}"
        );
        assert!(
            text.contains("build on ssh:box"),
            "a pane on another host says where: {text}"
        );
        assert!(text.contains("n starts a shell"), "{text}");
    }

    #[test]
    fn the_status_line_says_where_an_attached_pane_runs() {
        let mut app = app_with_session();
        app.set_pane_size(TerminalSize { cols: 60, rows: 20 });
        let mut orphan = summary("agent-terminal-3", "tail -f", true);
        orphan.session_id = None;
        orphan.workspace = Some("build".to_owned());
        orphan.host = Some("ssh:box".to_owned());
        app.apply(crate::backend::Update::TerminalOpened {
            terminal: orphan,
            replay_bytes: 0,
        });

        let text = render(&mut app);
        assert!(
            text.contains("build on ssh:box"),
            "the status line says where the pane behind the keyboard runs: {text}"
        );

        // A pane the session owns already runs where the window does, and saying so on
        // every attach would be noise on the path a person takes most often.
        let mut app = app_with_session();
        app.set_pane_size(TerminalSize { cols: 60, rows: 20 });
        app.apply(crate::backend::Update::TerminalOpened {
            terminal: summary("agent-terminal-1", "/bin/sh -c make", true),
            replay_bytes: 0,
        });
        let text = render(&mut app);
        assert!(
            !text.contains("this workspace on this machine"),
            "a session's own pane says nothing about where it runs: {text}"
        );
    }

    #[test]
    fn copy_mode_announces_itself_in_the_pane() {
        let mut app = app_with_session();
        app.set_pane_size(TerminalSize { cols: 60, rows: 20 });
        app.apply(crate::backend::Update::TerminalOpened {
            terminal: summary("agent-terminal-1", "/bin/sh -c make", true),
            replay_bytes: 0,
        });
        app.apply(crate::backend::Update::TerminalData(b"hello\r\n".to_vec()));
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));

        let text = render(&mut app);
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("copy"), "{text}");
        assert!(text.contains("v/V select"), "{text}");
    }

    #[test]
    fn the_terminal_pane_shows_the_query_and_where_a_find_landed() {
        let mut app = app_with_session();
        app.set_pane_size(TerminalSize { cols: 60, rows: 20 });
        app.apply(crate::backend::Update::TerminalOpened {
            terminal: summary("agent-terminal-1", "/bin/sh -c make", true),
            replay_bytes: 0,
        });
        app.apply(crate::backend::Update::TerminalData(
            b"error: one\r\nerror: two\r\n".to_vec(),
        ));
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "error".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let text = render(&mut app);
        assert!(text.contains("find error"), "the query is a line: {text}");

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let text = render(&mut app);
        assert!(text.contains("match 2/2"), "the newest match: {text}");
    }

    #[test]
    fn the_transcript_says_when_it_is_being_read() {
        let mut app = app_with_session();
        app.handle_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
        let text = render(&mut app);
        assert!(text.contains("copy"), "{text}");
        assert!(text.contains("v/V select"), "{text}");
        assert!(text.contains("y copy"), "{text}");
    }

    #[test]
    fn the_key_list_becomes_the_copy_keys_while_the_transcript_is_read() {
        let mut app = app_with_session();
        app.handle_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));

        let text = render(&mut app);
        assert!(text.contains("copy mode keys"), "{text}");
        assert!(text.contains("find in what is above"), "{text}");
        assert!(text.contains("ctrl-u / ctrl-d"), "{text}");
    }

    #[test]
    fn the_key_list_in_a_terminal_being_read_is_the_copy_keys_too() {
        let mut app = app_with_session();
        app.set_pane_size(TerminalSize { cols: 60, rows: 20 });
        app.apply(crate::backend::Update::TerminalOpened {
            terminal: summary("agent-terminal-1", "/bin/sh -c make", true),
            replay_bytes: 0,
        });
        app.apply(crate::backend::Update::TerminalData(
            b"make: *** no rule\r\n".to_vec(),
        ));
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));

        let text = render(&mut app);
        assert!(text.contains("copy mode keys"), "{text}");
        assert!(text.contains("page back / forward"), "{text}");
        assert!(
            !text.contains("interrupt the program"),
            "the shell's keys are not the pane's while it is being read: {text}"
        );
    }

    #[test]
    fn the_key_list_shows_the_keys_in_force_rather_than_the_ones_it_grew_with() {
        let mut app = app_with_session();
        app.focus = Focus::Editor;
        app.keymap =
            crate::keymap::Keymap::from_toml("[keys]\npalette = [\"f1\"]\n").expect("a keymap");

        // Asked of the list rather than of a drawn screen: the card clips its tail
        // on a short terminal, and a test of which bindings are listed should not
        // depend on whether this one happens to fit.
        let (pane, rows) = keys_for(&app);
        assert_eq!(pane, "editor");
        assert!(
            rows.iter()
                .any(|(keys, what)| keys == "f1" && *what == "commands"),
            "{rows:?}"
        );
        assert!(
            rows.iter().all(|(keys, _)| !keys.contains("ctrl-p")),
            "the chord the file moved is not listed as if it still worked: {rows:?}"
        );
        assert!(
            rows.first().is_some_and(|(_, what)| *what == "move up"),
            "the pane's own keys come first: {rows:?}"
        );
    }

    #[test]
    fn the_menu_bar_is_drawn_and_its_list_opens_under_the_title() {
        let mut app = app_with_session();
        let text = render(&mut app);
        assert!(text.contains("Acpilot"), "{text}");
        assert!(text.contains("View"), "{text}");
        assert!(text.contains("Session"), "{text}");
        assert!(text.contains("f9 menus"), "{text}");

        app.handle_key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let text = render(&mut app);
        assert!(text.contains("Command Palette"), "{text}");
        assert!(text.contains("Quit Acpilot"), "{text}");
        // Every row that has a chord names it beside the label.
        assert!(text.contains("ctrl-p"), "{text}");
        assert!(text.contains("ctrl-c"), "{text}");

        // The About card is a row of the Help menu, one title further along.
        for _ in 0..MENUS.len() - 1 {
            app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let text = render(&mut app);
        assert!(text.contains("About Acpilot"), "{text}");
        assert!(text.contains("Keys for this pane"), "{text}");
    }

    #[test]
    fn the_about_card_names_the_build_it_is() {
        let mut app = app_with_session();
        app.about = true;
        let text = render(&mut app);
        assert!(text.contains("About Acpilot"), "{text}");
        assert!(text.contains(acpilot_core::VERSION), "{text}");
        assert!(text.contains("MIT OR Apache-2.0"), "{text}");
        assert!(text.contains("rudeboy.dev/acpilot"), "{text}");
        assert!(text.contains("any key closes"), "{text}");
    }

    #[test]
    fn the_drawing_reports_the_dividers_it_drew_and_a_drag_moves_them() {
        let mut app = App::demo(&acpilot_demo::demo());
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).expect("a test terminal");
        let mut drawn = None;
        terminal
            .draw(|frame| drawn = Some(draw(frame, &mut app)))
            .expect("draw");
        let viewports = drawn.expect("a frame");
        // The tree is 26 columns wide, so its border is the 26th cell and the
        // divider a drag has to hit.
        assert_eq!(viewports.dividers.tree, Some(25));
        assert_eq!(viewports.dividers.right, 120);
        let sidebar = viewports
            .dividers
            .sidebar
            .expect("the sidebar is on screen");

        app.set_viewports(viewports);
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 25,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 39,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.files_width, 40);

        // The next frame draws the divider where the drag left it — the loop from a
        // column to a width and back has no arithmetic of its own.
        terminal
            .draw(|frame| drawn = Some(draw(frame, &mut app)))
            .expect("draw");
        let viewports = drawn.expect("a frame");
        assert_eq!(viewports.dividers.tree, Some(39));
        assert_eq!(
            viewports.dividers.sidebar,
            Some(sidebar),
            "and the sidebar did not move"
        );
    }

    #[test]
    fn the_mouse_hits_the_menu_the_drawing_drew() {
        use crate::app::menu_title;
        use crate::backend::Request;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mut app = App::demo(&acpilot_demo::demo());
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).expect("a test terminal");
        let mut drawn = None;
        terminal
            .draw(|frame| drawn = Some(draw(frame, &mut app)))
            .expect("draw");
        app.set_viewports(drawn.expect("a frame"));

        // The letter `Alt` opens the menu with is underlined on the bar, and the
        // title's cell is where `menu_title` says it is.
        let (start, _) = menu_title(0);
        let bar = terminal.backend().buffer().clone();
        let cell = &bar[(start + 1, 0)];
        assert_eq!(cell.symbol(), "F", "the title keeps its capital");
        assert!(
            cell.modifier.contains(Modifier::UNDERLINED),
            "and the letter Alt answers to is marked"
        );

        // A click on the title opens the list, which hangs under the name rather than
        // under the padding before it.
        let click = |column: u16, row: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(app.handle_mouse(click(start + 1, 0)), None);
        assert!(app.menu.is_some_and(|state| state.items));
        terminal
            .draw(|frame| drawn = Some(draw(frame, &mut app)))
            .expect("draw");
        let viewports = drawn.expect("a frame");
        app.set_viewports(viewports);
        let list = viewports.menu.list.expect("the list was drawn");
        assert_eq!(list.x, start);

        // The third row of File is Quit, and a click on it is the request `Ctrl-C`
        // makes — the loop that turns a click into a command, end to end.
        assert_eq!(
            app.handle_mouse(click(list.x + 2, list.y + 3)),
            Some(Request::Quit)
        );
        assert_eq!(app.menu, None, "and running a row closes the list");
    }

    /// A demo app drawn in the real theme, for the tests that care about colour.
    fn demo_with_theme() -> App {
        let mut app = App::demo(&acpilot_demo::demo());
        let extensions = acpilot_extensions::builtin::load_all().expect("the built-in themes load");
        let resolved = acpilot_extensions::ThemeRegistry::new(&extensions)
            .complete("acpilot.night-flight")
            .expect("night flight resolves");
        app.set_theme(crate::theme::Theme::from_resolved(&resolved));
        app
    }

    /// Draw at a size and hand back the terminal, so a test can read cells.
    fn drawn(app: &mut App, width: u16, height: u16) -> ratatui::Terminal<TestBackend> {
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
        terminal
            .draw(|frame| {
                draw(frame, app);
            })
            .expect("draw");
        terminal
    }

    /// How many cells come before a byte offset in a rendered row.
    fn cells_to(row: &str, byte: usize) -> usize {
        row[..byte].chars().count()
    }

    /// The rows of a rendered frame, as strings.
    fn rows(terminal: &ratatui::Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_editor_draws_a_real_buffer_with_a_gutter_and_syntax_colour() {
        let mut app = demo_with_theme();
        app.focus = Focus::Editor;
        let terminal = drawn(&mut app, 120, 34);
        let rows = rows(&terminal);
        let text = rows.join("\n");

        // Line numbers, the marker column, and the seeded buffer's own text.
        assert!(
            text.contains("  4   const SCROLLBACK: usize = 10_000;"),
            "the gutter and the fourth line: {text}"
        );
        // The marker is git's answer about the diff, in the column beside the
        // number: the first hunk adds four lines at line 8 of the new file, so
        // the ninth line on screen is the first one marked.
        assert!(
            text.contains("  9 +     Live,"),
            "the diff's marker is in its column: {text}"
        );
        assert!(
            !text.contains(" 14 ~") && !text.contains(" 14 +"),
            "a line the diff does not touch has no marker: {text}"
        );

        // `const` is a keyword, and it is painted with the theme's keyword colour
        // at the cell the gutter puts it in.
        let row = rows
            .iter()
            .position(|row| row.contains("const SCROLLBACK"))
            .expect("the line is on screen");
        // A column is cells, not bytes: the frame holds border glyphs, so a
        // `find` offset would land two cells to the right of what it found.
        let column = cells_to(&rows[row], rows[row].find("const").expect("the keyword")) as u16;
        let cell = &terminal.backend().buffer()[(column, row as u16)];
        assert_eq!(cell.symbol(), "c");
        assert_eq!(
            cell.fg,
            app.theme
                .syntax(acpilot_extensions::theme::SyntaxToken::Keyword),
            "`const` is the theme's keyword colour"
        );

        // The current line — the caret's — carries the panel background across
        // the whole row, and the line under it does not.
        let current = rows
            .iter()
            .position(|row| row.contains("let (mut row, mut"))
            .expect("the caret's line");
        let at = cells_to(
            &rows[current],
            rows[current].find("let (mut row").expect("the text"),
        ) as u16;
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(at, current as u16)].bg,
            app.theme.bg_panel(),
            "the caret's line is a row, not a word: {}",
            rows[current]
        );
        assert_ne!(
            buffer[(at, current as u16 + 1)].bg,
            app.theme.bg_panel(),
            "and the row under it is not: {}",
            rows[current + 1]
        );
    }

    #[test]
    fn the_editor_scrolls_sideways_when_the_caret_leaves_the_right_edge() {
        let mut app = demo_with_theme();
        app.focus = Focus::Editor;
        let before = rows(&drawn(&mut app, 100, 30)).join("\n");
        assert!(
            before.contains("use acpilot_proto"),
            "the first line starts at its beginning: {before}"
        );

        // `End` on the first line puts the caret past the right edge of a narrow
        // pane, and the window has to follow it. The whole pane scrolls — a
        // horizontal scroll is one decision for all the rows, not one per line.
        app.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL,
        ));
        app.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('1'),
            KeyModifiers::NONE,
        ));
        app.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        app.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::End,
            KeyModifiers::NONE,
        ));
        let after = rows(&drawn(&mut app, 100, 30)).join("\n");
        assert!(
            !after.contains("use acpilot_proto"),
            "the line scrolled off the left: {after}"
        );
        assert!(
            after.contains("TerminalSummary};"),
            "and its end is on screen: {after}"
        );
        assert!(
            after.contains("  1   "),
            "the gutter did not scroll with it: {after}"
        );
    }

    #[test]
    fn a_find_prompt_is_drawn_in_the_tab_strip_above_the_buffer() {
        let mut app = demo_with_theme();
        app.focus = Focus::Editor;
        app.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        ));
        for character in "Pane".chars() {
            app.handle_key(crossterm::event::KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            ));
        }
        let text = rows(&drawn(&mut app, 120, 34)).join("\n");
        assert!(text.contains("/Pane"), "the query is drawn: {text}");
        assert!(
            text.contains("· /Pane"),
            "and the title says which search is running: {text}"
        );
    }

    #[test]
    fn the_tree_column_is_the_width_the_app_stores() {
        let mut app = App::demo(&acpilot_demo::demo());
        // The tree's top border runs from column 0 to its own width: in 26 columns
        // the right corner is the 26th cell, and a wider tree moves it.
        let corner = |app: &mut App| {
            let text = render(app);
            text.lines()
                .nth(1)
                .and_then(|row| row.chars().position(|cell| cell == '┐'))
        };
        assert_eq!(corner(&mut app), Some(25), "26 columns, corner at index 25");
        app.files_width = 40;
        assert_eq!(corner(&mut app), Some(39));
    }

    #[test]
    fn the_design_pass_draws_the_three_panels_and_the_two_footer_rows() {
        let mut app = App::demo(&acpilot_demo::demo());
        let text = render(&mut app);

        // Files, text, agents — the same three in every frontend.
        assert!(text.contains("File manager · acpilot"), "{text}");
        assert!(text.contains("crates/acpilot-tui/src/app.rs"), "{text}");
        assert!(text.contains("Assistant · 3"), "{text}");
        assert!(text.contains("wire the terminal pane"), "{text}");
        assert!(
            text.contains("seeded"),
            "the panes say they are fixtures: {text}"
        );

        // The footer: what the turn costs, and where the keyboard is.
        assert!(text.contains("lmstudio"), "{text}");
        assert!(text.contains("acpilot.night-flight"), "{text}");
        assert!(
            text.contains("17:9"),
            "the status line reports the cursor: {text}"
        );
    }

    #[test]
    fn the_stats_bar_draws_a_track_and_a_figure_per_integration() {
        let mut app = App::demo(&acpilot_demo::demo());
        let text = render_at(&mut app, 200, 34);

        assert!(text.contains("◇ lmstudio"), "{text}");
        assert!(text.contains("18.4k / 32.8k"), "{text}");
        assert!(text.contains("8.40 / 20.00 USD"), "{text}");
        assert!(
            text.contains("◆ Claude Code"),
            "an agent is a row beside the providers: {text}"
        );
        assert!(
            text.contains("(old)"),
            "a quiet reading says it is old rather than looking current: {text}"
        );
        assert!(
            text.contains("4.50 credits left"),
            "a balance with no ceiling is still a number: {text}"
        );
    }

    #[test]
    fn an_unknown_window_is_a_dashed_track_not_a_full_one() {
        let mut app = App::demo(&acpilot_demo::demo());
        let text = render_at(&mut app, 200, 34);
        assert!(
            text.contains("╌╌╌╌╌╌╌╌"),
            "the estimate has no denominator to fill against: {text}"
        );
        assert!(
            text.contains("~41.0k / ?"),
            "and the number it does have is marked as an estimate: {text}"
        );
    }

    #[test]
    fn a_narrow_bar_shrinks_its_segments_before_it_drops_a_row() {
        let mut app = App::demo(&acpilot_demo::demo());
        // 160 columns is room for the four seeded rows with their tracks and no
        // sentences: the shrink is the first answer, so no row disappears.
        let text = render_at(&mut app, 160, 34);
        for label in ["lmstudio", "anthropic", "Claude Code", "openrouter"] {
            assert!(
                text.contains(label),
                "{label} is still a row at 160 columns: {text}"
            );
        }
        assert!(
            !text.contains("18.4k / 32.8k"),
            "the figures are what the narrow bar gave up: {text}"
        );
    }

    #[test]
    fn a_bar_that_cannot_fit_every_row_says_how_many_it_hid() {
        let mut app = App::demo(&acpilot_demo::demo());
        let text = render_at(&mut app, 80, 34);
        assert!(
            text.contains("+2") || text.contains("+1") || text.contains("+3"),
            "a provider nobody can see is worse than a short bar: {text}"
        );
        assert!(
            !text.contains("openrouter"),
            "and the count is what stands in for it: {text}"
        );
    }

    #[test]
    fn a_quiet_integration_says_it_reported_nothing() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.integrations = vec![acpilot_stats::Row {
            id: "quiet".to_owned(),
            kind: acpilot_proto::stats::IntegrationKind::Provider,
            label: "quiet".to_owned(),
            context: None,
            credits: None,
            updated_at: "2026-10-30T12:00:00Z".to_owned(),
            stale: false,
        }];
        let text = render_at(&mut app, 200, 34);
        assert!(
            text.contains("quiet · nothing reported"),
            "an empty row is not two empty tracks, which read as two zeros: {text}"
        );
    }

    #[test]
    fn the_sources_are_one_key_away() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
        let text = render_at(&mut app, 200, 34);
        assert!(
            text.contains("context server · credits —"),
            "a bar's provenance, named: {text}"
        );
        assert!(
            text.contains("context estimated"),
            "an estimate says it is one: {text}"
        );
        assert!(
            !text.contains("18.4k / 32.8k"),
            "the sources replace the figures rather than crowding them: {text}"
        );
    }

    #[test]
    fn a_narrow_terminal_drops_the_tree_before_it_squeezes_the_editor() {
        let mut app = App::demo(&acpilot_demo::demo());
        let text = render_at(&mut app, 80, 24);
        assert!(!text.contains("Files ·"), "the tree folds away: {text}");
        assert!(text.contains("Assistant · 3"), "{text}");
        assert!(text.contains("Prompt · Enter sends"), "{text}");
    }

    #[test]
    fn the_palette_marks_every_row_that_is_not_built_with_its_task() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        for character in "symbol".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let text = render(&mut app);
        assert!(text.contains("Rename symbol"), "{text}");
        assert!(text.contains("· P7-5"), "{text}");
    }

    #[test]
    fn the_diff_rows_are_built_rather_than_promised() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        for character in "diff".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let text = render(&mut app);
        assert!(text.contains("Commands · 3"), "{text}");
        assert!(text.contains("Review diff"), "{text}");
        assert!(
            !text.contains("P6-9"),
            "a built row carries no task tag: {text}"
        );
    }

    #[test]
    fn the_editor_says_which_mode_it_is_in_and_marks_an_edited_buffer() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.focus = Focus::Editor;
        let normal = render(&mut app);
        assert!(normal.contains("· normal"), "{normal}");

        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        let insert = render(&mut app);
        assert!(insert.contains("· insert"), "{insert}");
        assert!(
            !insert.contains("app.rs +"),
            "nothing has been typed yet: {insert}"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let edited = render(&mut app);
        assert!(
            edited.contains("app.rs +"),
            "an edited buffer says it is not in the file: {edited}"
        );
    }

    #[test]
    fn a_check_in_flight_says_so_beside_the_position() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.focus = Focus::Editor;
        let quiet = render(&mut app);
        assert!(!quiet.contains("checking"), "{quiet}");

        // A question that has been asked and not answered is a fact about the screen: a
        // gutter that has not moved for the seconds a first index takes must not read as
        // a clean file.
        app.diagnostics_pending = true;
        let asking = render(&mut app);
        assert!(asking.contains("· checking"), "{asking}");
    }

    #[test]
    fn a_planned_action_names_the_task_in_a_toast() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        // A row that is still planned: a definition is the language server's (P7-4)
        // and nothing in this build asks one.
        for character in "definition".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let text = render(&mut app);
        assert!(text.contains("planned · P7-4"), "{text}");
        assert!(text.contains("not implemented"), "{text}");
    }

    #[test]
    fn the_content_search_draws_a_row_per_matching_line() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.grep = Some(crate::app::GrepSearch {
            query: "const".to_owned(),
            matches: vec![acpilot_proto::file::GrepMatch {
                path: "src/app.rs".to_owned(),
                line: 3,
                text: "const SCROLLBACK: usize = 10_000;".to_owned(),
            }],
            selected: 0,
        });
        let text = render(&mut app);
        assert!(text.contains("Find in files · 1"), "{text}");
        assert!(text.contains("src/app.rs:3"), "{text}");
        assert!(text.contains("const SCROLLBACK"), "{text}");
    }

    #[test]
    fn a_result_row_marks_the_words_that_matched() {
        let base = Style::default().fg(Color::Gray);
        let mark = Style::default().fg(Color::Black).bg(Color::Yellow);
        let marked = |text: &str, query: &str| -> Vec<String> {
            highlight_line(text, query, base, mark)
                .iter()
                .filter(|span| span.style == mark)
                .map(|span| span.content.to_string())
                .collect()
        };

        let text = "const SCROLLBACK: usize = 10_000;";
        let spans = highlight_line(text, "const", base, mark);
        // Marking splits the line, so the pieces have to add back up to it: a row that
        // dropped or repeated a character would not read as the file does.
        let joined: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(joined, text);
        assert_eq!(marked(text, "const"), ["const"]);
        assert_eq!(marked("const and const", "const"), ["const", "const"]);
        // The daemon folds case to decide a line matched, so the row folds it to mark.
        assert_eq!(marked("Const x", "CONST"), ["Const"]);
        // Nothing to mark: the whole line stays in the body's own style.
        assert!(marked(text, "  ").is_empty());
        assert_eq!(highlight_line(text, "", base, mark).len(), 1);
    }

    #[test]
    fn the_review_draws_each_hunk_with_its_decision() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        let text = render(&mut app);
        assert!(
            text.contains("Review · crates/acpilot-tui/src/app.rs"),
            "{text}"
        );
        assert!(text.contains("@@ -8,6 +8,12 @@"), "{text}");
        assert!(text.contains("accepted"), "{text}");
        assert!(text.contains("hunk 1/2"), "{text}");

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let text = render(&mut app);
        assert!(text.contains("rejected"), "{text}");
        assert!(text.contains("hunk 2/2"), "{text}");
    }

    #[test]
    fn a_split_review_draws_both_sides_of_a_hunk() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        let text = render(&mut app);
        assert_eq!(
            text.matches("pub exit: Option<Option<i32>>,").count(),
            2,
            "a context line is on both sides: {text}"
        );
        assert_eq!(
            text.matches("pub mode: PaneMode,").count(),
            1,
            "an addition is on one: {text}"
        );
    }

    #[test]
    fn a_streaming_turn_has_a_cursor_and_says_so() {
        let mut app = app_with_session();
        app.sessions[0].status = SessionStatus::Busy;
        let text = render(&mut app);
        assert!(text.contains("streaming"), "{text}");
        assert!(text.contains('▏'), "the cursor is at the end: {text}");
    }

    #[test]
    fn assistant_markdown_is_drawn_without_its_markers() {
        let mut app = app_with_session();
        app.entries = vec![Entry {
            kind: Kind::Assistant,
            text: "## What changed\n**bold** and `code`".to_owned(),
            ..Entry::default()
        }];
        let text = render(&mut app);
        assert!(text.contains("What changed"), "{text}");
        assert!(
            !text.contains("##"),
            "the hashes are a marker, not text: {text}"
        );
        assert!(text.contains("bold and code"), "{text}");
        assert!(!text.contains('`'), "{text}");
    }

    #[test]
    fn the_at_completion_is_drawn_over_the_transcript() {
        let mut app = app_with_session();
        app.focus = Focus::Prompt;
        app.input = "@app".to_owned();
        app.cursor = 4;
        app.completion = Some(crate::app::Completion {
            query: "app".to_owned(),
            rows: vec!["crates/acpilot-tui/src/app.rs".to_owned()],
            selected: 0,
        });
        let text = render(&mut app);
        assert!(text.contains("@app"), "{text}");
        assert!(text.contains("crates/acpilot-tui/src/app.rs"), "{text}");
    }

    #[test]
    fn the_source_pane_draws_the_changed_file_and_its_diff() {
        let mut app = app_with_session();
        let demo = acpilot_demo::demo();
        app.source = Some(crate::app::SourceControl::fixture(&demo.diff));

        let text = render(&mut app);
        assert!(text.contains("Source control"), "{text}");
        assert!(text.contains("crates/acpilot-tui/src/app.rs"), "{text}");
        assert!(text.contains("diff --git"), "{text}");
        assert!(
            text.contains("c compose"),
            "the footer names the composer: {text}"
        );
    }

    #[test]
    fn the_composer_draws_the_message_and_the_confirmation_question() {
        let mut app = app_with_session();
        let demo = acpilot_demo::demo();
        app.source = Some(crate::app::SourceControl::fixture(&demo.diff));
        app.commit = Some(crate::app::CommitComposer {
            text: "feat: write the message".to_owned(),
            cursor: 24,
            ..crate::app::CommitComposer::default()
        });

        let text = render(&mut app);
        assert!(text.contains("Commit message"), "{text}");
        assert!(text.contains("feat: write the message"), "{text}");
        assert!(text.contains("ctrl-s"), "{text}");
    }

    #[test]
    fn a_tall_transcript_shows_its_end_not_its_start() {
        let mut app = app_with_session();
        app.entries.clear();
        for index in 0..200 {
            app.entries.push(Entry {
                kind: Kind::Assistant,
                text: format!("line {index}"),
                ..Entry::default()
            });
        }
        let text = render(&mut app);
        assert!(!text.contains("line 0\n"), "{text}");
        assert!(text.contains("line 199"), "{text}");
    }
}
