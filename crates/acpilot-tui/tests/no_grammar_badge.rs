use acpilot_git::LineChanges;
use acpilot_tui::app::Focus;
use acpilot_tui::{App, EditorPane, ui};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[test]
fn a_file_with_no_grammar_says_so_in_the_badge() {
    let mut app = App::editor_first("http://127.0.0.1:7777");
    app.focus = Focus::Editor;
    app.editor = Some(EditorPane::from_text(
        "Makefile",
        "all:\n\techo hi",
        LineChanges::default(),
        &mut app.grammars,
    ));

    let mut terminal = Terminal::new(TestBackend::new(120, 34)).expect("a test terminal");
    terminal
        .draw(|frame| {
            ui::draw(frame, &mut app);
        })
        .expect("draw");

    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    assert!(text.contains("no grammar"), "{text}");
}
