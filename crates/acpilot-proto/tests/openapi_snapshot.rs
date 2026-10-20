//! Snapshot the OpenAPI document.
//!
//! The snapshot is not a formality: it makes every wire change (a renamed field,
//! a new enum variant, a dropped schema) show up as a diff in review instead of
//! as a bug report from a client. Update it deliberately with `cargo insta
//! review`, or `INSTA_UPDATE=always cargo test -p acpilot-proto` where insta is
//! not installed.

use acpilot_proto::openapi_json;

#[test]
fn openapi_document_is_unchanged() {
    let document: serde_json::Value =
        serde_json::from_str(&openapi_json()).expect("the document is valid JSON");
    insta::assert_json_snapshot!("openapi", document);
}
