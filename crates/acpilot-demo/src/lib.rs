//! The seeded world both UIs are designed against.
//!
//! The interface is agreed before the backend for it exists: the file tree, the
//! editor pane, the diff review and the stats bar are P6 tasks, and a screenshot
//! of a wireframe is a harder thing to argue about than a screen that answers the
//! keyboard. [`demo`] is that screen's data.
//!
//! # Why a crate and not a test fixture
//!
//! The desktop app mirrors the terminal one (P10), so the same sessions, the same
//! transcript and the same tree have to arrive in both. A fixture inside the TUI
//! would be copied into the GUI and the two designs would drift in the first week;
//! this crate is the one copy, and the protocol types are what make it portable —
//! a session here is the session the daemon serves, not a lookalike.
//!
//! # What is *not* here
//!
//! Behaviour. Which key opens the palette, what a toast looks like and how a pane
//! is laid out are the frontends' business; this crate only says what is on the
//! screen. That boundary is what lets the GUI render the same world in gpui
//! without importing a line of the TUI.

use acpilot_proto::agent::{AgentStatus, AgentSummary, AgentTransport};
use acpilot_proto::event::{SessionEvent, SessionEventKind};
use acpilot_proto::permission::{PermissionKind, PermissionOption, PermissionRequest};
use acpilot_proto::session::{SessionStatus, SessionSummary};
use acpilot_proto::terminal::{TerminalSize, TerminalSummary};
use serde_json::json;

pub mod editor;
pub mod palette;
pub mod stats;
pub mod tree;

pub use editor::{Diff, DiffKind, DiffLine, EditorBuffer, EditorLine, Hunk};
pub use palette::{Command, CommandAction, Run};
pub use stats::Integration;
pub use tree::{FileNode, GitStatus};

/// One session and the transcript it replays.
///
/// The pair is the unit because a session list is not worth designing without the
/// transcript behind it: the sidebar is a list of things that open onto something.
#[derive(Debug, Clone)]
pub struct SessionFixture {
    /// What `GET /v1/sessions` would say about it.
    pub summary: SessionSummary,
    /// The events it replays, in `seq` order.
    pub events: Vec<SessionEvent>,
}

/// A profile's worth of seeded state.
#[derive(Debug, Clone)]
pub struct Demo {
    /// The profile being designed against, as the status line names it.
    pub profile: String,
    /// The workspace root, as the file tree is titled.
    pub workspace: String,
    /// What the status line says where a daemon address goes.
    pub daemon: String,
    /// The profile's agents, with one missing so the registry's missing case is on
    /// screen rather than described.
    pub agents: Vec<AgentSummary>,
    /// Sessions, oldest first — the order the real list comes back in.
    pub sessions: Vec<SessionFixture>,
    /// The tree, already flattened: `depth` is the indent and `expanded` says
    /// whether a directory's children follow it, so a renderer draws indentation
    /// and never a recursion.
    pub files: Vec<FileNode>,
    /// The buffer the editor pane shows.
    pub editor: EditorBuffer,
    /// The diff the review pane shows, for the same file the permission asks about.
    pub diff: Diff,
    /// A pending write, so the permission card is part of the design rather than a
    /// pane nobody has seen.
    pub permission: PermissionRequest,
    /// The session's terminals, one of them ended.
    pub terminals: Vec<TerminalSummary>,
    /// What the stats bar counts.
    pub integrations: Vec<Integration>,
    /// Every command the palette offers, tagged with the task that would build it.
    pub commands: Vec<Command>,
    /// The bytes the seeded terminal replays: a real screen, with colour, because a
    /// pane that only draws plain text is a pane nobody can judge.
    pub terminal_screen: String,
}

/// The seeded world.
#[must_use]
pub fn demo() -> Demo {
    Demo {
        profile: "acpilot.night-flight".to_owned(),
        workspace: "~/workspace/acpilot".to_owned(),
        daemon: "demo · seeded".to_owned(),
        agents: agents(),
        sessions: sessions(),
        files: tree::files(),
        editor: editor::buffer(),
        diff: editor::diff(),
        permission: permission(),
        terminals: terminals(),
        integrations: stats::integrations(),
        commands: palette::commands(),
        terminal_screen: terminal_screen(),
    }
}

/// The reply the design pass gives a prompt.
///
/// Three events rather than one paragraph: the transcript has to show a thought, a
/// tool call and streamed text *while they arrive*, because a pane that only ever
/// draws a finished message is a pane nobody has seen under load.
#[must_use]
pub fn scripted_reply(first: u64, session: &str) -> Vec<SessionEvent> {
    vec![
        event(
            first,
            session,
            SessionEventKind::Thought,
            json!({ "content": "A real turn goes to the daemon; this one is a script, so the panes can be reviewed without one." }),
        ),
        event(
            first + 1,
            session,
            SessionEventKind::ToolCall,
            json!({ "tool_call_id": "call-script", "status": "completed", "title": "Answer from the design fixture" }),
        ),
        event(
            first + 2,
            session,
            SessionEventKind::MessageChunk,
            json!({
                "role": "assistant",
                "content": "Everything above this line is the seeded transcript; this reply is scripted. The panes, the keys and the layout are the real ones — the daemon behind them is not."
            }),
        ),
    ]
}

fn agents() -> Vec<AgentSummary> {
    vec![
        AgentSummary {
            id: "mock".to_owned(),
            name: "Mock Agent".to_owned(),
            transport: AgentTransport::AcpStdio,
            status: AgentStatus::Available,
            command: "target/debug/mock_agent".to_owned(),
            protocol_version: Some("1".to_owned()),
        },
        AgentSummary {
            id: "claude".to_owned(),
            name: "Claude Code".to_owned(),
            transport: AgentTransport::AcpStdio,
            status: AgentStatus::Available,
            command: "claude-code-acp".to_owned(),
            protocol_version: Some("1".to_owned()),
        },
        AgentSummary {
            id: "codex".to_owned(),
            name: "Codex".to_owned(),
            transport: AgentTransport::AcpStdio,
            status: AgentStatus::Missing,
            command: "codex-acp".to_owned(),
            protocol_version: None,
        },
    ]
}

fn summary(
    id: &str,
    title: &str,
    agent: &str,
    status: SessionStatus,
    created: &str,
) -> SessionSummary {
    SessionSummary {
        id: id.to_owned(),
        workspace_id: "acpilot".to_owned(),
        agent_id: agent.to_owned(),
        title: title.to_owned(),
        status,
        created_at: created.to_owned(),
        updated_at: created.to_owned(),
    }
}

fn event(
    seq: u64,
    session: &str,
    kind: SessionEventKind,
    payload: serde_json::Value,
) -> SessionEvent {
    SessionEvent {
        seq,
        session_id: session.to_owned(),
        kind,
        at: format!("2026-10-25T09:{:02}:00Z", seq.min(59)),
        payload,
    }
}

fn sessions() -> Vec<SessionFixture> {
    vec![
        SessionFixture {
            summary: summary(
                "s1",
                "wire the terminal pane",
                "mock",
                SessionStatus::Busy,
                "2026-10-25T08:58:00Z",
            ),
            events: transcript(),
        },
        SessionFixture {
            summary: summary(
                "s2",
                "flaky retry test",
                "claude",
                SessionStatus::Idle,
                "2026-10-24T16:20:00Z",
            ),
            events: vec![
                event(
                    1,
                    "s2",
                    SessionEventKind::MessageChunk,
                    json!({ "role": "user", "content": "the retry test fails one run in five" }),
                ),
                event(
                    2,
                    "s2",
                    SessionEventKind::MessageChunk,
                    json!({
                        "role": "assistant",
                        "content": "It waits for a fixed 50 ms; the daemon's poll is a second. I would wait for the event instead."
                    }),
                ),
            ],
        },
        SessionFixture {
            summary: summary(
                "s3",
                "review the pane API",
                "mock",
                SessionStatus::Closed,
                "2026-10-23T11:02:00Z",
            ),
            events: vec![event(
                1,
                "s3",
                SessionEventKind::Notice,
                json!({ "title": "Session closed", "message": "ended by the user" }),
            )],
        },
    ]
}

/// The open session's transcript.
///
/// Written as the events a real turn produces, not as final text: the transcript
/// has to show a tool call, a thought, a plan and a notice, because those are the
/// four shapes the renderer has to survive in one paragraph.
fn transcript() -> Vec<SessionEvent> {
    let mut events = vec![
        event(
            1,
            "s1",
            SessionEventKind::MessageChunk,
            json!({ "role": "user", "content": "Attach the TUI to a terminal an agent opened." }),
        ),
        event(
            2,
            "s1",
            SessionEventKind::Thought,
            json!({ "content": "The client already speaks the frames; the pane needs a screen and a keyboard, and neither belongs in the worker." }),
        ),
        event(
            3,
            "s1",
            SessionEventKind::ToolCall,
            json!({ "tool_call_id": "call-1", "status": "pending", "title": "Edit crates/acpilot-tui/src/app.rs" }),
        ),
        event(
            4,
            "s1",
            SessionEventKind::ToolCall,
            json!({ "tool_call_id": "call-1", "status": "completed", "title": "Edit crates/acpilot-tui/src/app.rs" }),
        ),
        event(
            5,
            "s1",
            SessionEventKind::Plan,
            json!({
                "entries": [
                    { "content": "parse the screen in the app, not in the worker" },
                    { "content": "map keys to bytes, Ctrl-C included" },
                    { "content": "tell the program the pane's size" }
                ]
            }),
        ),
        event(
            6,
            "s1",
            SessionEventKind::MessageChunk,
            json!({
                "role": "assistant",
                "content": "The pane attaches over the same socket the daemon writes to, so what a person sees and what the agent polls cannot disagree. Keys are bytes: `Ctrl-C` in a shell is an interrupt, and leaving the pane is `Ctrl-T`, which the title says."
            }),
        ),
        event(
            7,
            "s1",
            SessionEventKind::Notice,
            json!({ "title": "Turn complete", "message": "3 files changed, 41 tests passing" }),
        ),
        event(
            8,
            "s1",
            SessionEventKind::MessageChunk,
            json!({ "role": "user", "content": "Does copy mode need OSC 52?" }),
        ),
    ];
    events.push(event(
        9,
        "s1",
        SessionEventKind::MessageChunk,
        json!({
            "role": "assistant",
            "content": "Only because the copy has to leave the machine. Over SSH the clipboard that matters is on the terminal in front of the person, so the text goes out as an escape rather than through a local clipboard."
        }),
    ));
    events
}

fn permission() -> PermissionRequest {
    PermissionRequest {
        id: "p-1".to_owned(),
        session_id: "s1".to_owned(),
        kind: PermissionKind::FileWrite,
        title: "Write crates/acpilot-tui/src/app.rs".to_owned(),
        detail: "+ 1 000 lines of scrollback\n+ F2 copy mode\n+ a picker when a session has several terminals".to_owned(),
        options: vec![
            PermissionOption::AllowOnce,
            PermissionOption::AllowAlways,
            PermissionOption::Deny,
        ],
        created_at: "2026-10-25T09:12:00Z".to_owned(),
    }
}

fn terminals() -> Vec<TerminalSummary> {
    vec![
        TerminalSummary {
            id: "agent-terminal-1".to_owned(),
            session_id: "s1".to_owned(),
            size: TerminalSize {
                cols: 100,
                rows: 24,
            },
            alive: true,
            command: "/bin/sh -c make check".to_owned(),
        },
        TerminalSummary {
            id: "agent-terminal-2".to_owned(),
            session_id: "s1".to_owned(),
            size: TerminalSize { cols: 80, rows: 24 },
            alive: false,
            command: "/bin/sh -c cargo run -- tui".to_owned(),
        },
    ]
}

/// A screen with colour on it, so the pane is judged on what it can draw.
fn terminal_screen() -> String {
    let mut screen = String::new();
    screen.push_str("$ make check\r\n");
    screen.push_str("  cargo fmt --all -- --check\r\n");
    screen.push_str("  cargo clippy --workspace --all-targets -- -D warnings\r\n");
    screen.push_str("  cargo nextest run --workspace --no-fail-fast\r\n");
    screen.push_str("\r\n");
    screen.push_str("        \x1b[32mPASS\x1b[0m [   0.31s] acpilot-tui::app::yanking_a_line_puts_it_on_the_clipboard\r\n");
    screen.push_str("        \x1b[32mPASS\x1b[0m [   0.28s] acpilot-tui::ui::copy_mode_announces_itself_in_the_pane\r\n");
    screen.push_str("        \x1b[32mPASS\x1b[0m [   0.44s] acpilotd::agent_terminals::typing_into_the_pane_reaches_the_agents_process\r\n");
    screen
        .push_str("     \x1b[1mSummary\x1b[0m [   4.82s] 158 tests run: 158 passed, 0 skipped\r\n");
    screen.push_str("\r\n");
    screen.push_str("$ \x1b[7m \x1b[0m");
    screen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_session_has_a_transcript_and_the_list_has_more_than_one_session() {
        let demo = demo();
        assert!(demo.sessions.len() >= 3, "a list of one is not a list");
        let open = &demo.sessions[0];
        assert_eq!(open.summary.id, "s1");
        assert!(open.events.len() >= 8, "the transcript shows every kind");
        // Sequences are what the renderer de-duplicates by, so they have to be
        // strictly increasing.
        assert!(open.events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    }

    #[test]
    fn the_permission_asks_about_the_file_the_diff_is_for() {
        let demo = demo();
        assert!(demo.permission.title.contains(&demo.diff.path));
        assert!(
            !demo.diff.hunks.is_empty(),
            "a diff with no hunks proves nothing"
        );
    }

    #[test]
    fn the_tree_is_flattened_and_children_follow_their_directory() {
        let demo = demo();
        assert!(demo.files.iter().any(|node| node.dir));
        // A child is deeper than the file before it and at most one level deeper,
        // which is what makes indentation a rendering job rather than a walk.
        for pair in demo.files.windows(2) {
            assert!(
                pair[1].depth <= pair[0].depth + 1,
                "{} is indented past its parent",
                pair[1].path
            );
        }
    }

    #[test]
    fn every_palette_command_is_either_runnable_or_named_after_the_task_that_would_build_it() {
        let demo = demo();
        assert!(
            demo.commands
                .iter()
                .any(|command| command.planned().is_some())
        );
        assert!(
            demo.commands
                .iter()
                .any(|command| command.planned().is_none())
        );
        for command in &demo.commands {
            if let Some(task) = command.planned() {
                assert!(
                    task.starts_with('P') && task.chars().any(|c| c.is_ascii_digit()),
                    "a planned command names its task: {task}"
                );
            }
        }
    }

    #[test]
    fn the_seeded_screen_has_colour_and_a_prompt() {
        let demo = demo();
        assert!(
            demo.terminal_screen.contains("\x1b[32m"),
            "the pane draws styled cells"
        );
        assert!(demo.terminal_screen.ends_with("$ \x1b[7m \x1b[0m"));
    }
}
