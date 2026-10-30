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
    PaneMode, TerminalPane, Toast, Viewports, about_lines, menu_title,
};
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
    // is an interruption, and the toast is a note about the last key. The menu's
    // list is drawn first because a bar with a list open is still the lowest thing
    // over the panes.
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
    if app.permission_card
        && let Some(request) = app.permissions.first()
    {
        draw_permission(frame, request, app.diff.as_ref(), body, &app.theme);
    }
    if let Some(toast) = &app.toast {
        draw_toast(frame, toast, body, &app.theme);
    }

    viewports
}

/// The panels, or the terminal in place of the editor and the sidebar.
///
/// Three panels is the product's mental model — files, text, agents — and it is
/// the same three in every frontend. The tree needs a column of its own, so below
/// a hundred columns it folds away rather than squeezing the editor to a slit;
/// the centre and the sidebar are what a session is actually read in.
///
/// Returns the size a program in the pane should be told, and the transcript's own
/// when it is the pane on screen: the cells inside each, because that is what the
/// next attach uses and what a copy cursor walks.
fn draw_body(frame: &mut Frame, app: &mut App, area: Rect) -> Viewports {
    let wide = area.width >= 100;
    // The two dividers are the two widths in `app`: the tree's column and the
    // sidebar's, both clamped to what the panes beside them can spare. A layout that
    // obeyed the number instead of the window would put the editor at zero columns.
    let tree = if wide {
        app.files_width.min(area.width.saturating_sub(40)).max(16)
    } else {
        app.files_width
    };
    let (files, rest) = if wide {
        let [files, rest] =
            Layout::horizontal([Constraint::Length(tree), Constraint::Min(40)]).areas(area);
        (Some(files), rest)
    } else {
        (None, area)
    };
    let files_area = files.map(|files| draw_files(frame, app, files));
    let sidebar = app
        .sidebar_width
        .min(if wide { 80 } else { 34 })
        .min(rest.width.saturating_sub(20))
        .max(20);
    let dividers = Dividers {
        tree: files.map(|files| files.x + files.width.saturating_sub(1)),
        // The sidebar is behind the terminal when one is attached, so there is no
        // divider to grab there either.
        sidebar: app
            .terminal
            .is_none()
            .then(|| rest.x + rest.width.saturating_sub(sidebar)),
        left: area.x,
        right: area.right(),
    };

    let mut editor_area = None;
    let mut terminal_area = None;
    let mut sessions_area = None;
    let transcript = match &app.terminal {
        Some(pane) => {
            terminal_area = Some(draw_terminal(frame, pane, rest, &app.theme));
            // The transcript is behind the terminal, so there is no viewport for a
            // copy cursor to walk.
            None
        }
        None => {
            let [editor, agents] =
                Layout::horizontal([Constraint::Min(20), Constraint::Length(sidebar)]).areas(rest);
            editor_area = draw_editor(frame, app, editor);
            let (transcript, sessions) = draw_agents(frame, app, agents);
            sessions_area = Some(sessions);
            Some(transcript)
        }
    };

    Viewports {
        terminal: inner_size(rest),
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
    let title = match app.workspace.rsplit('/').next() {
        Some(workspace) if !workspace.is_empty() => format!(" Files · {workspace} "),
        _ => " Files ".to_owned(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(app.focus == Focus::Files, &app.theme));
    // The rows start inside the border, and the mouse is told so: a click on the
    // frame is a click on the frame, not on the first file.
    let inner = block.inner(area);

    if app.files.is_empty() {
        frame.render_widget(
            Paragraph::new("the tree is P6-6\nseeded by --demo")
                .style(Style::default().fg(app.theme.text_muted()))
                .block(block),
            area,
        );
        return pane_area(inner);
    }

    let items: Vec<ListItem> = app
        .files
        .iter()
        .enumerate()
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
            } else {
                Style::default()
            };
            let status = node.status.letter().unwrap_or(" ");
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
    let demo = app.demo;
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
                    format!(" {} ", editor.tab()),
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
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " {} · {}{title} ",
            editor.path(),
            if demo { "seeded" } else { "read-only" }
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
        let marker = editor
            .mark(row.line)
            .map_or_else(|| "  ".to_owned(), |letter| format!("{letter} "));
        let mut spans = vec![
            Span::styled(
                format!("{:>width$} ", row.line + 1, width = gutter - 3),
                base.fg(theme.text_muted()),
            ),
            Span::styled(marker, base.fg(theme.warning())),
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

/// The agent sidebar: sessions over a transcript over a composer.
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
        Constraint::Length(3),
    ])
    .areas(area);
    let sessions = draw_sessions(frame, app, sessions);
    let size = draw_transcript(frame, app, transcript);
    draw_composer(frame, app, composer);
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
                "copy {}/{history} · / find · v select · y copy · Esc leaves",
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
    let lines: Vec<Line> = (0..inner.height)
        .map(|row| terminal_line(pane, row, inner.width, theme))
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
fn terminal_line(pane: &TerminalPane, row: u16, cols: u16, theme: &Theme) -> Line<'static> {
    let screen = pane.parser.screen();
    let copying = pane.mode == PaneMode::Copy;
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
        if pane.reader.view.selected(row, col) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if pane.matched(row, col) {
            // Yellow rather than reversed: a match can sit inside a selection or
            // under the cursor, and it has to be readable through both.
            style = style.add_modifier(Modifier::UNDERLINED).fg(theme.warning());
        }
        if copying && pane.reader.view.cursor == (row, col) {
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
    let title = format!(" Sessions · {} ", app.sessions.len());
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
    let lines = app.transcript_lines();
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
        let rows: Vec<Line> = lines
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(index, (text, kind))| {
                transcript_line(
                    app,
                    *kind,
                    text,
                    u16::try_from(index - start).unwrap_or(u16::MAX),
                )
            })
            .collect();
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
fn transcript_line(app: &App, kind: Kind, text: &str, row: u16) -> Line<'static> {
    let style = entry_style(kind, &app.theme);
    let Some(view) = (app.focus == Focus::Transcript).then_some(&app.transcript.view) else {
        return Line::from(Span::styled(text.to_owned(), style));
    };
    let mut spans: Vec<Span> = text
        .chars()
        .enumerate()
        .map(|(col, character)| {
            let col = u16::try_from(col).unwrap_or(u16::MAX);
            let mut style = style;
            if view.selected(row, col) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            if app.matched_transcript(row, col) {
                style = style
                    .add_modifier(Modifier::UNDERLINED)
                    .fg(app.theme.warning());
            }
            if view.cursor == (row, col) {
                style = style.add_modifier(Modifier::REVERSED | Modifier::UNDERLINED);
            }
            Span::styled(character.to_string(), style)
        })
        .collect();
    // The cursor sits after the last character as well as on one, and a cursor on
    // a cell that does not exist would be a pane with no visible cursor at all.
    if view.cursor == (row, u16::try_from(text.chars().count()).unwrap_or(u16::MAX)) {
        spans.push(Span::styled(
            " ",
            style.add_modifier(Modifier::REVERSED | Modifier::UNDERLINED),
        ));
    }
    Line::from(spans)
}

fn draw_composer(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Prompt · Enter sends ")
        .border_style(border_style(app.focus == Focus::Prompt, &app.theme));
    let text = if app.input.is_empty() {
        Paragraph::new("ask the agent…").style(Style::default().fg(app.theme.text_muted()))
    } else {
        Paragraph::new(app.input.clone())
    };
    frame.render_widget(text.block(block), area);

    if app.focus == Focus::Prompt && !app.input.contains('\n') {
        // One line, so the cursor is a column count rather than a layout pass.
        let width = u16::try_from(app.input.chars().count()).unwrap_or(u16::MAX);
        frame.set_cursor_position((area.x + 1 + width, area.y + 1));
    }
}

/// The status line: connection, profile, which pane has the keys, the cursor, and
/// what just happened.
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
    if let Some(editor) = &app.editor {
        let (line, column) = editor.position();
        spans.push(Span::styled(format!("· {line}:{column} "), dim));
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
const COPY_KEYS: [(&str, &str); 10] = [
    ("j / k", "line down / up"),
    ("h / l", "left / right"),
    ("ctrl-u / ctrl-d", "page back / forward"),
    ("g / G", "oldest line / live"),
    ("/", "find in what is above"),
    ("n / N", "next match, older / newer"),
    ("v", "select"),
    ("y", "copy and leave"),
    ("esc", "leave"),
    ("?", "these keys"),
];

/// The keys the pane with the keyboard answers to, and what to call it.
///
/// A pane being read answers to the copy keys rather than to its own: the motions
/// are the whole of what it does, and a person who asks for the list while
/// scrolling asked for the motions.
fn keys_for(app: &App) -> (&'static str, &'static [(&'static str, &'static str)]) {
    let reading = app.focus == Focus::Transcript
        || (app.focus == Focus::Terminal
            && app
                .terminal
                .as_ref()
                .is_some_and(|pane| pane.mode == PaneMode::Copy));
    if reading {
        return ("copy mode", &COPY_KEYS);
    }
    (app.focus.label(), pane_keys(app.focus))
}

/// The keys a pane answers to, as the which-key panel lists them.
fn pane_keys(focus: Focus) -> &'static [(&'static str, &'static str)] {
    match focus {
        Focus::Files => &[
            ("j / k", "move"),
            ("h / l", "collapse / expand"),
            ("enter", "open"),
            ("/", "fuzzy open"),
            ("tab", "next pane"),
            ("ctrl-←/→", "narrower / wider"),
            ("?", "these keys"),
        ],
        Focus::Editor => &[
            ("h j k l", "move"),
            ("0 / $", "start / end of line"),
            ("pgup / pgdn", "a page"),
            ("/ or ctrl-f", "find in file"),
            ("n / N", "next / previous match"),
            ("ctrl-g", "go to line"),
            ("ctrl-s", "save"),
            ("ctrl-←/→", "narrower / wider"),
            ("i", "insert mode"),
            ("f12", "go to definition"),
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
        Focus::Terminal => &[
            ("ctrl-t", "leave the pane"),
            ("f2", "copy mode"),
            ("f4", "where the stats came from"),
            ("ctrl-c", "interrupt the program"),
        ],
    }
}

/// A git status as the tree's gutter colour.
fn git_colour(status: acpilot_demo::GitStatus, theme: &Theme) -> Color {
    match status {
        acpilot_demo::GitStatus::Clean => theme.text_muted(),
        acpilot_demo::GitStatus::Modified => theme.warning(),
        acpilot_demo::GitStatus::Added => theme.success(),
        acpilot_demo::GitStatus::Untracked => theme.accent(),
        acpilot_demo::GitStatus::Deleted => theme.danger(),
        acpilot_demo::GitStatus::Conflicted => theme.accent_high(),
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
    diff: Option<&Diff>,
    area: Rect,
    theme: &Theme,
) {
    let card = if diff.is_some() {
        middle(area, 84, 76)
    } else {
        centred(area)
    };
    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Permission needed ")
        .border_style(Style::default().fg(theme.warning()));
    let room = usize::from(card.height.saturating_sub(2)).saturating_sub(3);
    let mut lines = vec![Line::from(Span::styled(
        format!("{} · {}", request.session_id, request.title),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
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
        "y allow once · a allow always · n deny · Esc later",
        Style::default().fg(theme.warning()),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        card,
    );
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
                Span::styled(format!("{marker}{}{state}", terminal.command), row),
                Span::styled(format!("  {}", terminal.id), id),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter opens · j/k choose · Esc closes",
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
        });
        app.status = "connected · acpilotd 0.1.0".to_owned();
        app
    }

    #[test]
    fn the_shell_shows_the_panes_it_promises() {
        let text = render(&mut app_with_session());
        assert!(text.contains("Sessions · 1"), "{text}");
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
                session_id: "s1".to_owned(),
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
                session_id: "s1".to_owned(),
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
                session_id: "s1".to_owned(),
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
                session_id: "s1".to_owned(),
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
            session_id: "s1".to_owned(),
            size: TerminalSize { cols: 60, rows: 20 },
            alive,
            command: command.to_owned(),
        }
    }

    #[test]
    fn the_terminal_picker_lists_every_terminal_the_session_has() {
        let mut app = app_with_session();
        app.terminals
            .push(summary("agent-terminal-1", "/bin/sh -c make", true));
        app.terminals
            .push(summary("agent-terminal-2", "/bin/sh -c cargo test", false));
        app.terminal_picker = true;
        app.picker_selected = 1;

        let text = render(&mut app);
        assert!(text.contains("Terminals · 2"), "{text}");
        assert!(text.contains("/bin/sh -c make"), "{text}");
        assert!(text.contains("/bin/sh -c cargo test"), "{text}");
        assert!(text.contains("exited"), "an ended program says so: {text}");
        assert!(text.contains("Enter opens"), "{text}");
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
        assert!(text.contains("v select"), "{text}");
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
        assert!(text.contains("v select"), "{text}");
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
        assert!(text.contains("Files · acpilot"), "{text}");
        assert!(text.contains("crates/acpilot-tui/src/app.rs"), "{text}");
        assert!(text.contains("Sessions · 3"), "{text}");
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
        assert!(text.contains("Sessions · 3"), "{text}");
        assert!(text.contains("Prompt · Enter sends"), "{text}");
    }

    #[test]
    fn the_palette_marks_every_row_that_is_not_built_with_its_task() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        for character in "diff".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let text = render(&mut app);
        assert!(text.contains("Commands · 3"), "{text}");
        assert!(text.contains("Review diff"), "{text}");
        assert!(text.contains("P6-9"), "{text}");
    }

    #[test]
    fn a_planned_action_names_the_task_in_a_toast() {
        let mut app = App::demo(&acpilot_demo::demo());
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        for character in "review".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let text = render(&mut app);
        assert!(text.contains("planned · P6-9"), "{text}");
        assert!(text.contains("not implemented"), "{text}");
    }

    #[test]
    fn a_tall_transcript_shows_its_end_not_its_start() {
        let mut app = app_with_session();
        app.entries.clear();
        for index in 0..200 {
            app.entries.push(Entry {
                kind: Kind::Assistant,
                text: format!("line {index}"),
            });
        }
        let text = render(&mut app);
        assert!(!text.contains("line 0\n"), "{text}");
        assert!(text.contains("line 199"), "{text}");
    }
}
