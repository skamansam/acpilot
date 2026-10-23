//! Snapshot the OpenAPI document.
//!
//! The snapshot is not a formality: it makes every wire change (a renamed field,
//! a new enum variant, a dropped schema) show up as a diff in review instead of
//! as a bug report from a client. Update it deliberately with `cargo insta
//! review`, or `INSTA_UPDATE=always cargo test -p acpilot-proto` where insta is
//! not installed.

use acpilot_proto::openapi_json;
use serde_json::{Map, Value};

#[test]
fn openapi_document_is_unchanged() {
    let document: Value =
        serde_json::from_str(&openapi_json()).expect("the document is valid JSON");
    insta::assert_json_snapshot!("openapi", canonical(document));
}

/// Sort every object key so the snapshot records the document, not the map
/// implementation `serde_json` happened to be built with.
///
/// `preserve_order` is a workspace-wide feature (an ACP dependency enables it),
/// so `cargo test -p acpilot-proto` and `cargo test --workspace` otherwise
/// produce the same document in two different orders and one of them fails.
/// Ordering is not a wire property; the set of keys is.
fn canonical(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect::<Map<String, Value>>(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}
