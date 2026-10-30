//! What a session's conversation costs, counted from the events it produced.
//!
//! The stats bar wants one number per agent — how much of the context is in use —
//! and nothing in the engine produces one: ACP lets an agent report usage, and most
//! do not, so the daemon counts what the conversation carries and labels the answer
//! as the estimate it is (`StatSource::Estimated`). A reading the agent *did* report
//! is kept and preferred, because a measurement beats an estimate.
//!
//! # What is counted
//!
//! Every character the session says and hears: the prompt when it is sent, and the
//! strings carried by each event the agent streams — message chunks, thoughts, tool
//! calls (their arguments and their output), and plans. Four characters to a token,
//! which is the usual rule of thumb and wrong in both directions: CJK is nearer one
//! character a token and long identifiers tokenise better. That is why the figure is
//! labelled, and why it is never given a denominator this client invented — the bar
//! draws `18.4k / ?` with a track that says "no window", not a comfortable-looking
//! full one.
//!
//! Three things are deliberately *not* counted:
//!
//! * the agent's echo of the user's own prompt (`role: "user"`), because the prompt
//!   was counted when the daemon sent it;
//! * the daemon's own notices and the session's mode or command list, which are the
//!   bar's business rather than the model's;
//! * framing: JSON object keys, and the fields that label a payload rather than fill
//!   it — `role`, `type`, `status`, `level`, `subject` and the ids. Counting the word
//!   `assistant` on every chunk would be a per-chunk tax on the estimate.
//!
//! A tool call that is *patched* (`ToolCallUpdate`) repeats the call's title and
//! content, and the count adds that up. An agent that patches in small steps is
//! therefore counted a little high; reconciling the patches would want the tool
//! call's own state, and the figure is labelled and drawn against no window, so the
//! alternative on offer is not worth a second copy of that state here.
//!
//! # Compaction
//!
//! A compaction replaces the conversation with a summary, so the count starts again
//! from the summary's own text: it is the new conversation, not an addition to the
//! old one. Until the summary arrives the count is zero and the reading is absent —
//! "nobody has reported anything since the context was rewritten" is the honest
//! answer, where a carried-over number would describe a conversation that no longer
//! exists.
//!
//! # What this is not
//!
//! A tokeniser. One would be a dependency of megabytes to sharpen a figure the
//! window is unknown for; the estimate is already the best available *local* answer,
//! and a client that needs an exact count gets it from the agent's usage update.

use acpilot_proto::event::{SessionEvent, SessionEventKind};
use acpilot_proto::stats::{ContextStats, StatSource};
use serde_json::Value;

/// Roughly how many characters make one token.
const CHARS_PER_TOKEN: u64 = 4;

/// One session's running estimate, and the last reading the agent reported.
#[derive(Debug, Default)]
pub struct Usage {
    /// Characters of conversation counted since the last compaction.
    counted: u64,
    /// The compaction being summarised, so that streamed chunks accumulate into one
    /// new conversation rather than each replacing the last.
    compacting: Option<(String, u64)>,
    /// What the agent itself last reported, when it reports at all.
    reported: Option<Reading>,
}

/// A usage update from the agent: tokens used, and the window when it names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    used: u32,
    window: Option<u32>,
}

impl Usage {
    /// Count a prompt on its way to the agent.
    pub fn prompt(&mut self, text: &str) {
        self.counted += chars(text);
    }

    /// Count what one event carried, or take the reading it reports.
    ///
    /// A usage update wins over the running count and is not added to it: the agent
    /// is describing the context it holds, which includes everything counted so far
    /// and the framing this client cannot see.
    pub fn observe(&mut self, event: &SessionEvent) {
        if let Some(reading) = reported(&event.payload) {
            self.reported = Some(reading);
            return;
        }
        match event.kind {
            SessionEventKind::MessageChunk if is_user_echo(&event.payload) => {}
            SessionEventKind::MessageChunk
            | SessionEventKind::Thought
            | SessionEventKind::ToolCall
            | SessionEventKind::Plan => self.counted += carried(&event.payload),
            SessionEventKind::Compaction => self.compacted(&event.payload),
            // A notice is the daemon's word to the user, and a mode or command list
            // is what the session can do rather than what it holds.
            SessionEventKind::Notice
            | SessionEventKind::ModeChanged
            | SessionEventKind::CommandsChanged => {}
        }
    }

    /// What the bar draws, when there is anything to draw.
    ///
    /// `None` is "nothing has happened yet", which the route reports as an absent
    /// field rather than a measured zero. A reported reading of zero *is* a reading
    /// and comes back as one.
    #[must_use]
    pub fn context(&self) -> Option<ContextStats> {
        if let Some(reading) = self.reported {
            return Some(ContextStats {
                used_tokens: reading.used,
                // A size of zero is a window nobody stated, not a window of no room:
                // the shared fill rules would refuse to divide by it anyway, and the
                // wire says absent.
                window_tokens: reading.window.filter(|size| *size > 0),
                source: StatSource::Server,
            });
        }
        let used = tokens(self.counted);
        (used > 0).then_some(ContextStats {
            used_tokens: used,
            window_tokens: None,
            source: StatSource::Estimated,
        })
    }

    /// Start again from a compaction summary.
    fn compacted(&mut self, payload: &Value) {
        let id = payload
            .get("compaction_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let fresh = self
            .compacting
            .as_ref()
            .is_none_or(|(current, _)| current != id);
        if fresh {
            self.compacting = Some((id.to_owned(), 0));
        }
        let carried = carried(payload);
        if let Some((_, counted)) = self.compacting.as_mut() {
            *counted += carried;
            self.counted = *counted;
        }
        // The reading described the conversation that was just replaced.
        self.reported = None;
    }
}

/// Whether a message chunk is the agent echoing a prompt this daemon already counted.
fn is_user_echo(payload: &Value) -> bool {
    payload
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|role| role == "user")
}

/// The usage update in a payload, when it is one.
///
/// The engine carries ACP's `UsageUpdate` as a notice with `subject: "usage"`, which
/// is what a client draws; this is the same payload read for its numbers.
fn reported(payload: &Value) -> Option<Reading> {
    if payload.get("subject").and_then(Value::as_str)? != "usage" {
        return None;
    }
    let used = u32::try_from(payload.get("used")?.as_u64()?).ok()?;
    let window = payload
        .get("size")
        .and_then(Value::as_u64)
        .and_then(|size| u32::try_from(size).ok());
    Some(Reading { used, window })
}

/// Fields that label a payload rather than fill it.
///
/// A conversation is not longer because a chunk says it came from the assistant, or
/// because a tool call has an id; the model sees the labels as a handful of framing
/// tokens, and a per-chunk tax on the estimate would add up to more than that.
const FRAMING: [&str; 8] = [
    "role",
    "type",
    "status",
    "level",
    "subject",
    "tool_call_id",
    "message_id",
    "compaction_id",
];

/// Every character of text a payload carries, framing and numbers excluded.
fn carried(value: &Value) -> u64 {
    match value {
        Value::String(text) => chars(text),
        Value::Array(items) => items.iter().map(carried).sum(),
        Value::Object(map) => map
            .iter()
            .filter(|(key, _)| !FRAMING.contains(&key.as_str()))
            .map(|(_, value)| carried(value))
            .sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

/// Characters, not bytes: a multi-byte glyph is one character to a tokeniser.
fn chars(text: &str) -> u64 {
    text.chars().count() as u64
}

/// A character count as tokens, rounded up so that a non-empty conversation is
/// never zero tokens, and clamped where the wire's `u32` ends.
fn tokens(counted: u64) -> u32 {
    u32::try_from(counted.div_ceil(CHARS_PER_TOKEN)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(kind: SessionEventKind, payload: Value) -> SessionEvent {
        SessionEvent {
            seq: 1,
            session_id: "s1".to_owned(),
            kind,
            at: "2026-10-30T12:00:00Z".to_owned(),
            payload,
        }
    }

    fn chunk(role: &str, text: &str) -> SessionEvent {
        event(
            SessionEventKind::MessageChunk,
            json!({ "role": role, "content": { "type": "text", "text": text } }),
        )
    }

    #[test]
    fn a_prompt_is_an_estimate_against_a_window_nobody_reported() {
        let mut usage = Usage::default();
        usage.prompt("explain this repository");

        let context = usage.context().expect("a reading");
        assert_eq!(context.used_tokens, 6, "24 characters, four to a token");
        assert_eq!(context.window_tokens, None, "no window was stated");
        assert_eq!(context.source, StatSource::Estimated);
    }

    #[test]
    fn nothing_that_happened_is_nothing_reported() {
        let usage = Usage::default();
        assert!(
            usage.context().is_none(),
            "an untouched session says nothing"
        );
    }

    #[test]
    fn one_character_is_one_token_rather_than_zero() {
        let mut usage = Usage::default();
        usage.prompt("a");
        assert_eq!(usage.context().expect("a reading").used_tokens, 1);
    }

    #[test]
    fn the_agents_words_are_counted() {
        let mut usage = Usage::default();
        usage.observe(&chunk("assistant", "twelve chars"));
        usage.observe(&event(
            SessionEventKind::Thought,
            json!({ "role": "assistant", "content": { "text": "thinking" } }),
        ));
        // 12 + 8 = 20 characters, four to a token.
        assert_eq!(usage.context().expect("a reading").used_tokens, 5);
    }

    #[test]
    fn the_agents_echo_of_the_prompt_is_not_counted_twice() {
        let mut usage = Usage::default();
        usage.prompt("hello");
        usage.observe(&chunk("user", "hello"));
        assert_eq!(
            usage.context().expect("a reading").used_tokens,
            2,
            "the prompt was counted when it was sent"
        );
    }

    #[test]
    fn a_tool_calls_arguments_and_output_are_part_of_the_context() {
        let mut usage = Usage::default();
        usage.observe(&event(
            SessionEventKind::ToolCall,
            json!({
                "tool_call_id": "t1",
                "title": "read",
                "raw_input": { "path": "src/main.rs" },
                "raw_output": "fn main() {}\n",
            }),
        ));
        // The keys and the id are framing; the title, the argument and the file body
        // are what the model is sent: 28 characters.
        assert_eq!(usage.context().expect("a reading").used_tokens, 7);
    }

    #[test]
    fn a_reported_reading_replaces_the_estimate_and_carries_its_window() {
        let mut usage = Usage::default();
        usage.prompt("a long question that this client will count for a while");
        usage.observe(&event(
            SessionEventKind::Notice,
            json!({
                "level": "info",
                "title": "Context usage",
                "subject": "usage",
                "used": 1200,
                "size": 200_000,
            }),
        ));

        let context = usage.context().expect("a reading");
        assert_eq!(context.used_tokens, 1200);
        assert_eq!(context.window_tokens, Some(200_000));
        assert_eq!(
            context.source,
            StatSource::Server,
            "a measurement, not this client's guess"
        );

        // More text does not change a figure the agent is reporting; the next usage
        // update will.
        usage.observe(&chunk("assistant", "and more words after the report"));
        assert_eq!(usage.context().expect("a reading").used_tokens, 1200);
    }

    #[test]
    fn a_reading_without_a_size_is_a_reading_with_no_window() {
        let mut usage = Usage::default();
        for payload in [
            json!({ "subject": "usage", "used": 300, "size": 0 }),
            json!({ "subject": "usage", "used": 300 }),
        ] {
            usage.observe(&event(SessionEventKind::Notice, payload));
            let context = usage.context().expect("a reading");
            assert_eq!(context.used_tokens, 300);
            assert_eq!(context.window_tokens, None);
        }
    }

    #[test]
    fn a_notice_that_is_not_a_reading_leaves_the_count_alone() {
        let mut usage = Usage::default();
        usage.prompt("hello");
        usage.observe(&event(
            SessionEventKind::Notice,
            json!({ "level": "warning", "title": "Turn failed", "message": "the agent died" }),
        ));
        assert_eq!(
            usage.context().expect("a reading").used_tokens,
            2,
            "the daemon's own words are not the conversation"
        );
    }

    #[test]
    fn a_compaction_starts_the_count_again_from_its_summary() {
        let mut usage = Usage::default();
        usage.prompt("a conversation long enough to be worth compacting, twice over");
        assert!(
            usage
                .context()
                .is_some_and(|context| context.used_tokens > 0)
        );

        // The status event names the compaction and carries no text of its own.
        usage.observe(&event(
            SessionEventKind::Compaction,
            json!({ "compaction_id": "c1", "status": "started" }),
        ));
        assert!(
            usage.context().is_none(),
            "until the summary arrives the new context is not known"
        );

        // Its summary streams in chunks, which accumulate rather than each replacing
        // the last.
        usage.observe(&event(
            SessionEventKind::Compaction,
            json!({ "compaction_id": "c1", "content": "twelve chars" }),
        ));
        usage.observe(&event(
            SessionEventKind::Compaction,
            json!({ "compaction_id": "c1", "content": "eight!!!" }),
        ));
        let context = usage.context().expect("a reading");
        assert_eq!(context.used_tokens, 5, "20 characters of summary");
        assert_eq!(context.source, StatSource::Estimated);
    }

    #[test]
    fn a_second_compaction_does_not_inherit_the_first_ones_summary() {
        let mut usage = Usage::default();
        usage.observe(&event(
            SessionEventKind::Compaction,
            json!({ "compaction_id": "c1", "content": "eight!!!" }),
        ));
        usage.observe(&event(
            SessionEventKind::Compaction,
            json!({ "compaction_id": "c2", "content": "four" }),
        ));
        assert_eq!(
            usage.context().expect("a reading").used_tokens,
            1,
            "the second summary is the whole of the new conversation"
        );
    }

    #[test]
    fn a_compaction_drops_the_reading_that_described_the_old_context() {
        let mut usage = Usage::default();
        usage.observe(&event(
            SessionEventKind::Notice,
            json!({ "subject": "usage", "used": 90_000, "size": 200_000 }),
        ));
        assert_eq!(
            usage.context().expect("a reading").source,
            StatSource::Server
        );

        usage.observe(&event(
            SessionEventKind::Compaction,
            json!({ "compaction_id": "c1", "status": "started" }),
        ));
        assert!(
            usage.context().is_none(),
            "90 000 tokens describe a conversation that no longer exists"
        );
    }

    #[test]
    fn a_usage_update_with_a_broken_shape_is_not_a_reading() {
        let mut usage = Usage::default();
        for payload in [
            json!({ "subject": "usage" }),
            json!({ "subject": "usage", "used": "lots" }),
            json!({ "subject": "usage", "used": -1 }),
        ] {
            usage.observe(&event(SessionEventKind::Notice, payload));
            assert!(usage.context().is_none(), "not a figure: {usage:?}");
        }
    }
}
