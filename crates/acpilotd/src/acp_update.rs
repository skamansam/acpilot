//! One recorded session event, as the driver's ACP protocol spells it.
//!
//! Both transports forward the same record — the stdio bridge over the driver's pipes
//! and the remote bridge over its notification stream — so the translation lives here
//! once. Two `match`es would drift the way two copies of a path do, and the failure is
//! quiet: a driver that never receives a tool call reads a turn that looks like the
//! agent did nothing.
//!
//! The payloads are the ones `acpilot-agent`'s event log writes (see
//! `crates/acpilot-agent/src/session.rs`), which use snake_case keys rather than the
//! protocol's camelCase: this is where the spelling changes, in one place.

use acpilot_proto::event::{SessionEvent, SessionEventKind};
use agent_client_protocol::v1::schema::{
    ContentBlock, ContentChunk, CurrentModeUpdate, Notice, NoticeSeverity, Plan, PlanEntry,
    SessionUpdate, TextContent, ToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The update a driver should receive, or `None` when the record has no exact
/// protocol spelling.
///
/// `None` is deliberate: compaction and a changed command list have no ACP update in
/// this schema version. Inventing a notice for them would tell a driver something
/// the protocol does not say.
pub(crate) fn update(event: &SessionEvent) -> Option<SessionUpdate> {
    let payload = &event.payload;
    match event.kind {
        SessionEventKind::MessageChunk => {
            // A driver wrote the prompt. Echoing it back as an assistant chunk would
            // read as if the agent had said its own words.
            if payload.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            chunk(payload).map(|text| {
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(text),
                )))
            })
        }
        SessionEventKind::Thought => chunk(payload).map(|text| {
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            )))
        }),
        SessionEventKind::ToolCall => tool_call(payload),
        SessionEventKind::Plan => {
            let entries: Vec<PlanEntry> = parse(payload, "entries")?;
            Some(SessionUpdate::Plan(Plan::new(entries)))
        }
        SessionEventKind::ModeChanged => {
            // A mode id is a `Cow<'static, str>` in the protocol: the payload's
            // borrow of the event cannot outlive this function, so it is owned here.
            let mode = payload.get("mode_id").and_then(Value::as_str)?;
            Some(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                mode.to_owned(),
            )))
        }
        SessionEventKind::Notice => Some(SessionUpdate::Notice(notice(payload))),
        SessionEventKind::Compaction | SessionEventKind::CommandsChanged => None,
    }
}

/// The text of a content-chunk payload, if it is text at all.
fn chunk(payload: &Value) -> Option<String> {
    payload
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// A tool call's announcement or its patch.
///
/// The log marks a patch with `patch: true`. Without that flag the same keys would be
/// a full call, and a driver sent a patch-shaped `tool_call` would draw a row that
/// forgot its title.
fn tool_call(payload: &Value) -> Option<SessionUpdate> {
    let id = payload.get("tool_call_id").and_then(Value::as_str)?;

    if payload.get("patch").and_then(Value::as_bool) == Some(true) {
        let mut fields = ToolCallUpdateFields::new();
        if let Some(status) = parse::<ToolCallStatus>(payload, "status") {
            fields = fields.status(status);
        }
        if let Some(kind) = parse::<ToolKind>(payload, "kind") {
            fields = fields.kind(kind);
        }
        if let Some(title) = payload.get("title").and_then(Value::as_str) {
            fields = fields.title(title.to_owned());
        }
        if let Some(name) = payload.get("name").and_then(Value::as_str) {
            fields = fields.name(name.to_owned());
        }
        if let Some(content) = parse::<Vec<ToolCallContent>>(payload, "content") {
            fields = fields.content(content);
        }
        if let Some(locations) = parse::<Vec<ToolCallLocation>>(payload, "locations") {
            fields = fields.locations(locations);
        }
        if let Some(input) = payload.get("raw_input") {
            fields = fields.raw_input(input.clone());
        }
        if let Some(output) = payload.get("raw_output") {
            fields = fields.raw_output(output.clone());
        }
        return Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id.to_owned(),
            fields,
        )));
    }

    let title = payload.get("title").and_then(Value::as_str)?;
    let mut call = ToolCall::new(id.to_owned(), title.to_owned());
    if let Some(status) = parse::<ToolCallStatus>(payload, "status") {
        call = call.status(status);
    }
    if let Some(kind) = parse::<ToolKind>(payload, "kind") {
        call = call.kind(kind);
    }
    if let Some(name) = payload.get("name").and_then(Value::as_str) {
        call = call.name(name.to_owned());
    }
    if let Some(content) = parse::<Vec<ToolCallContent>>(payload, "content") {
        call = call.content(content);
    }
    if let Some(locations) = parse::<Vec<ToolCallLocation>>(payload, "locations") {
        call = call.locations(locations);
    }
    if let Some(input) = payload.get("raw_input") {
        call = call.raw_input(input.clone());
    }
    if let Some(output) = payload.get("raw_output") {
        call = call.raw_output(output.clone());
    }
    Some(SessionUpdate::ToolCall(call))
}

/// The notice, with the severity and description the log recorded.
///
/// The description is in the payload under `message`. An earlier forwarder read
/// `description`, so every notice reached a driver as a bare title.
fn notice(payload: &Value) -> Notice {
    let severity = payload
        .get("level")
        .and_then(|level| serde_json::from_value::<NoticeSeverity>(level.clone()).ok())
        .unwrap_or(NoticeSeverity::Info);
    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Acpilot");
    let description = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Notice::new(severity, title).description(description.to_owned())
}

/// One payload key, deserialized into the protocol's own type.
///
/// The log wrote these values from the ACP types, so the serde spelling is the same
/// one the protocol reads. A value this build cannot spell is dropped rather than
/// guessed at.
fn parse<T: DeserializeOwned>(payload: &Value, key: &str) -> Option<T> {
    serde_json::from_value(payload.get(key)?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(kind: SessionEventKind, payload: Value) -> SessionEvent {
        serde_json::from_value(json!({
            "seq": 1,
            "session_id": "s1",
            "kind": kind,
            "at": "2026-11-16T00:00:00Z",
            "payload": payload,
        }))
        .expect("a session event")
    }

    fn wire(update: SessionUpdate) -> Value {
        serde_json::to_value(update).expect("an update")
    }

    #[test]
    fn a_tool_call_starts_with_its_identity_and_a_patch_updates_it() {
        let started = update(&event(
            SessionEventKind::ToolCall,
            json!({
                "tool_call_id": "t1",
                "title": "Read src/lib.rs",
                "status": "in_progress",
                "kind": "read",
                "locations": [{ "path": "/work/src/lib.rs" }],
                "raw_input": { "path": "src/lib.rs" },
            }),
        ))
        .expect("a tool call");
        let started = wire(started);
        assert_eq!(started["sessionUpdate"], "tool_call");
        assert_eq!(started["toolCallId"], "t1");
        assert_eq!(started["title"], "Read src/lib.rs");
        assert_eq!(started["status"], "in_progress");
        assert_eq!(started["kind"], "read");
        assert_eq!(started["locations"][0]["path"], "/work/src/lib.rs");

        // The patch names only what changed: a driver merges it into the row it
        // already drew. The title it knows must not be blanked out.
        let patched = update(&event(
            SessionEventKind::ToolCall,
            json!({
                "tool_call_id": "t1",
                "patch": true,
                "status": "completed",
                "raw_output": "fn main() {}",
            }),
        ))
        .expect("a patch");
        let patched = wire(patched);
        assert_eq!(patched["sessionUpdate"], "tool_call_update");
        assert_eq!(patched["toolCallId"], "t1");
        assert_eq!(patched["status"], "completed");
        assert_eq!(patched["rawOutput"], "fn main() {}");
        assert!(
            patched.get("title").is_none(),
            "a patch that carries no title must not carry an empty one: {patched}"
        );
    }

    #[test]
    fn a_plan_and_a_mode_change_are_the_protocol_updates() {
        let plan = update(&event(
            SessionEventKind::Plan,
            json!({
                "entries": [{
                    "content": "Read the file",
                    "priority": "high",
                    "status": "completed",
                }],
            }),
        ))
        .expect("a plan");
        let plan = wire(plan);
        assert_eq!(plan["sessionUpdate"], "plan");
        assert_eq!(plan["entries"][0]["content"], "Read the file");
        assert_eq!(plan["entries"][0]["status"], "completed");

        let mode = update(&event(
            SessionEventKind::ModeChanged,
            json!({ "mode_id": "code" }),
        ))
        .expect("a mode change");
        let mode = wire(mode);
        assert_eq!(mode["sessionUpdate"], "current_mode_update");
        assert_eq!(mode["currentModeId"], "code");
    }

    #[test]
    fn a_thought_is_a_thought_and_a_prompt_is_not_echoed() {
        let thought = update(&event(
            SessionEventKind::Thought,
            json!({ "role": "assistant", "content": { "type": "text", "text": "weighing it" } }),
        ))
        .expect("a thought");
        let thought = wire(thought);
        assert_eq!(thought["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(thought["content"]["text"], "weighing it");

        // The prompt is already on the driver's screen: sending it back as an
        // assistant chunk would attribute it to the agent.
        assert!(
            update(&event(
                SessionEventKind::MessageChunk,
                json!({ "role": "user", "content": { "type": "text", "text": "hello" } }),
            ))
            .is_none()
        );
    }

    #[test]
    fn a_notice_keeps_its_severity_and_description() {
        let notice = update(&event(
            SessionEventKind::Notice,
            json!({
                "level": "warning",
                "title": "Permission request routed to the machine",
                "message": "This driver has no `answer_permissions` grant.",
            }),
        ))
        .expect("a notice");
        let notice = wire(notice);
        assert_eq!(notice["sessionUpdate"], "notice");
        assert_eq!(notice["severity"], "warning");
        assert_eq!(notice["title"], "Permission request routed to the machine");
        assert_eq!(
            notice["description"],
            "This driver has no `answer_permissions` grant."
        );
    }

    #[test]
    fn a_record_with_no_protocol_spelling_is_dropped_rather_than_approximated() {
        assert!(
            update(&event(
                SessionEventKind::CommandsChanged,
                json!({ "commands": [] }),
            ))
            .is_none()
        );
        assert!(update(&event(SessionEventKind::Compaction, json!({}))).is_none());
    }
}
