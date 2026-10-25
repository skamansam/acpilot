//! The TUI's state: what is on screen, and what a key press means.
//!
//! There is no domain logic here. Sessions, permissions and events arrive from
//! the daemon; this module only decides how they read and which request a
//! keystroke turns into. Keeping that boundary sharp is what lets the same app
//! state be driven by a test with no daemon and no terminal.

use acpilot_proto::agent::AgentSummary;
use acpilot_proto::event::{SessionEvent, SessionEventKind};
use acpilot_proto::permission::{PermissionOption, PermissionRequest};
use acpilot_proto::session::SessionSummary;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::backend::{Request, Update};

/// Which pane has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The session list.
    Sessions,
    /// The prompt composer.
    Prompt,
}

impl Focus {
    /// The label the status line shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Sessions => "sessions",
            Self::Prompt => "prompt",
        }
    }
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

/// Everything the terminal draws, plus the state the keys act on.
#[derive(Debug, Clone)]
pub struct App {
    /// Where the daemon is, for the status line.
    pub daemon: String,
    /// Whether the daemon answered a health check.
    pub connected: bool,
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
            connected: false,
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
            focus: Focus::Sessions,
            status: "connecting".to_owned(),
            should_quit: false,
        }
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
                self.focus = Focus::Prompt;
                self.status = "session open".to_owned();
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
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') | KeyCode::Char('q') => {
                    self.should_quit = true;
                    Some(Request::Quit)
                }
                KeyCode::Char('r') => Some(Request::Refresh),
                _ => None,
            };
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

        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Sessions => Focus::Prompt,
                    Focus::Prompt => Focus::Sessions,
                };
                None
            }
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                    self.chose = true;
                    return self.open_selected();
                }
                None
            }
            KeyCode::Down => {
                if self.selected + 1 < self.sessions.len() {
                    self.selected += 1;
                    self.chose = true;
                    return self.open_selected();
                }
                None
            }
            KeyCode::Enter => match self.focus {
                Focus::Sessions => {
                    self.chose = true;
                    self.open_selected()
                }
                Focus::Prompt => {
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
            },
            KeyCode::Char('n') if self.focus == Focus::Sessions => {
                let agent = self.agents.first()?.id.clone();
                Some(Request::New(agent))
            }
            KeyCode::Char('d') if self.focus == Focus::Sessions => {
                let id = self.selected_session()?.id.clone();
                Some(Request::Delete(id))
            }
            KeyCode::Char('c') if self.focus == Focus::Sessions => Some(Request::Cancel),
            KeyCode::Char(character) if self.focus == Focus::Prompt => {
                self.input.push(character);
                None
            }
            KeyCode::Backspace if self.focus == Focus::Prompt => {
                self.input.pop();
                None
            }
            _ => None,
        }
    }

    fn open_selected(&mut self) -> Option<Request> {
        let id = self.selected_session()?.id.clone();
        if let Some(open) = &self.open
            && open == &id
        {
            self.focus = Focus::Prompt;
            return None;
        }
        Some(Request::Select(id))
    }
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
    fn ctrl_c_asks_to_quit() {
        let mut app = App::new("http://127.0.0.1:7777");
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(key), Some(Request::Quit));
        assert!(app.should_quit);
    }
}
