//! Drawing. Everything here reads state and paints; nothing here calls anything.
//!
//! The layout is the smallest thing that can hold the product: sessions on the
//! left, the transcript on the right, the composer under it, one status line, and
//! a permission card that covers the middle when an agent is waiting. Themes are
//! data (the mod-shop phase's P4.6-6) and do not exist yet, so the few colours
//! here are fixed rather than a second, private palette that would have to be
//! replaced later.

use acpilot_proto::permission::PermissionRequest;
use acpilot_proto::session::SessionStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::app::{App, Focus, Kind};

/// Paint one frame.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let [main, composer, status] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);
    let [sessions, transcript] =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Min(24)]).areas(main);

    draw_sessions(frame, app, sessions);
    draw_transcript(frame, app, transcript);
    draw_composer(frame, app, composer);
    draw_status(frame, app, status);

    if app.permission_card
        && let Some(request) = app.permissions.first()
    {
        draw_permission(frame, request, area);
    }
}

fn draw_sessions(frame: &mut Frame, app: &App, area: Rect) {
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
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker}{}", session.title), style),
                Span::styled(
                    format!("  {status}"),
                    style.fg(status_colour(session.status)),
                ),
            ]))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style(app.focus == Focus::Sessions));
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("no sessions · n starts one")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
    } else {
        frame.render_widget(List::new(items).block(block), area);
    }
}

fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let title = match (&app.open, app.selected_session()) {
        (Some(id), Some(session)) if &session.id == id => {
            format!(" {} · {} ", session.agent_id, session.title)
        }
        (Some(id), _) => format!(" {id} "),
        (None, _) => " Transcript ".to_owned(),
    };

    let lines: Vec<Line> = app
        .entries
        .iter()
        .flat_map(|entry| {
            let style = entry_style(entry.kind);
            entry
                .text
                .split('\n')
                .map(|line| Line::from(Span::styled(line.to_owned(), style)))
                .collect::<Vec<_>>()
        })
        .collect();

    let body = if lines.is_empty() {
        Paragraph::new("no transcript yet").style(Style::default().fg(Color::DarkGray))
    } else {
        // The last screen of a conversation is the part people read; a
        // transcript that always starts at the top is a transcript nobody
        // scrolls back to the bottom of.
        let height = area.height.saturating_sub(2) as usize;
        let start = lines.len().saturating_sub(height);
        Paragraph::new(lines[start..].to_vec())
    };

    frame.render_widget(
        body.block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_composer(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Prompt · Enter sends · Tab switches panes ")
        .border_style(border_style(app.focus == Focus::Prompt));
    let text = if app.input.is_empty() {
        Paragraph::new("ask the agent…").style(Style::default().fg(Color::DarkGray))
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

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let connection = if app.connected { "●" } else { "○" };
    let line = Line::from(vec![
        Span::styled(
            format!(" {connection} {} ", app.daemon),
            Style::default().fg(if app.connected {
                Color::Green
            } else {
                Color::Red
            }),
        ),
        Span::styled(
            format!("· {} ", app.focus.label()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(app.status.clone()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_permission(frame: &mut Frame, request: &PermissionRequest, area: Rect) {
    let card = centred(area);
    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Permission needed ")
        .border_style(Style::default().fg(Color::Yellow));
    let lines = vec![
        Line::from(Span::styled(
            format!("{} · {}", request.session_id, request.title),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(request.detail.clone()),
        Line::from(""),
        Line::from(Span::styled(
            "y allow once · a allow always · n deny · Esc later",
            Style::default().fg(Color::Yellow),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        card,
    );
}

/// The middle of the screen, two thirds across and half as tall.
fn centred(area: Rect) -> Rect {
    let [_, middle, _] = Layout::vertical([
        Constraint::Percentage(25),
        Constraint::Percentage(50),
        Constraint::Percentage(25),
    ])
    .areas(area);
    let [_, inner, _] = Layout::horizontal([
        Constraint::Percentage(15),
        Constraint::Percentage(70),
        Constraint::Percentage(15),
    ])
    .areas(middle);
    inner
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn entry_style(kind: Kind) -> Style {
    match kind {
        Kind::User => Style::default().fg(Color::Cyan),
        Kind::Assistant => Style::default(),
        Kind::Thought => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        Kind::Tool => Style::default().fg(Color::Yellow),
        Kind::Notice => Style::default().fg(Color::Blue),
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

fn status_colour(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Idle => Color::Green,
        SessionStatus::Busy => Color::Yellow,
        SessionStatus::AwaitingPermission => Color::Magenta,
        SessionStatus::Closed => Color::DarkGray,
        SessionStatus::Failed => Color::Red,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Entry;
    use acpilot_proto::permission::{PermissionKind, PermissionOption};
    use acpilot_proto::session::SessionSummary;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).expect("a test terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
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
        let text = render(&app_with_session());
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
        assert!(render(&app).contains("no transcript yet"));
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
        let text = render(&app);
        assert!(text.contains("Permission needed"), "{text}");
        assert!(text.contains("Write src/main.rs"), "{text}");
        assert!(text.contains("y allow once"), "{text}");
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
        let text = render(&app);
        assert!(!text.contains("line 0\n"), "{text}");
        assert!(text.contains("line 199"), "{text}");
    }
}
