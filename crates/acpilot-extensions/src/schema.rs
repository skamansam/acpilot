//! `extension.toml` as a published JSON Schema.
//!
//! The document is generated from [`Manifest`] — the type the loader parses — so
//! an editor's completion and the loader's refusal cannot describe different
//! files. [`SCHEMA_PATH`] is where the file is committed, under the docs site's
//! `public/`, and [`SCHEMA_URL`] is that address served: publishing the docs
//! publishes the schema, and `$id` *is* the address people paste.
//!
//! The same rules as the agent manifest's schema, for the same reason (D6): the
//! major version is in the filename and matches `schema_version`, v1 only ever
//! gains optional keys or enum members, and anything a `schema_version = 1`
//! loader would read differently ships as `extension-v2.json`. Regenerate with
//! `cargo xtask schema`; the drift test in `tests/schema.rs` is the gate.

use serde_json::{Map, Value, json};

use crate::manifest::{Manifest, SCHEMA_VERSION};

/// Where the published extension schema lives, relative to the repository root.
pub const SCHEMA_PATH: &str = "site/public/schema/extension-v1.json";

/// The address the committed file is served from, which is also its `$id`.
pub const SCHEMA_URL: &str = "https://rudeboy.dev/acpilot/schema/extension-v1.json";

/// An extension id: `publisher.name`, at least two lowercase segments of at most
/// forty characters each.
///
/// [`crate::manifest::validate_namespaced_id`] is the authority — it produces the
/// message a person reads — and the test feeds the same ids to both.
const ID_PATTERN: &str = "^[a-z0-9][a-z0-9_-]{0,39}(\\.[a-z0-9][a-z0-9_-]{0,39})+$";

/// One lowercase segment, the rule for `publisher`.
const SEGMENT_PATTERN: &str = "^[a-z0-9][a-z0-9_-]{0,39}$";

/// `MAJOR.MINOR.PATCH`, with an optional `-pre` or `+build` suffix — the shape
/// [`crate::manifest::is_version`] accepts as a store directory name.
const VERSION_PATTERN: &str =
    "^[0-9]+\\.[0-9]+\\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\\+[0-9A-Za-z.-]+)?$";

/// A contribution path: relative, and forward-slashed.
///
/// The other half of the rule — no `.` or `..` segment — stays the loader's alone,
/// because an editor's regex cannot enumerate path segments safely. The
/// description says so, so a reader is not told the schema is the whole check.
const RELATIVE_PATH_PATTERN: &str = "^[^/\\\\][^\\\\]*$";

/// [`json_schema`] as the committed file has it: pretty JSON, one trailing
/// newline.
///
/// One renderer for the generator and the drift test, so a difference between them
/// can only ever be a difference in what the schema says.
#[must_use]
pub fn schema_json_text() -> String {
    let mut text = serde_json::to_string_pretty(&json_schema())
        .expect("the generated schema is always serialisable");
    text.push('\n');
    text
}

/// The published JSON Schema for `extension.toml`.
///
/// Generated from [`Manifest`], then tightened where a derive cannot say the rule:
/// the shape of an id, of a version, and of a contribution path. `kind` is the
/// [`ExtensionKind`] enum itself, so the seven spellings come from the type the
/// loader matches on rather than from a list beside it.
///
/// What the schema keeps: `additionalProperties: false` (from `deny_unknown_fields`),
/// every field's `required`, `contributes` non-empty, and `schema_version` as a
/// `const`. What it cannot: two contributions with the same kind *and* path, which
/// JSON Schema has no keyword for, and the `.`/`..` half of the path rule.
#[must_use]
pub fn json_schema() -> Value {
    let generated = serde_json::to_value(schemars::schema_for!(Manifest))
        .expect("a generated schema is always serialisable");
    let mut generated = generated
        .as_object()
        .cloned()
        .expect("the manifest is an object with properties");
    // The struct's own docs describe the type; the document's describe the file.
    generated.remove("title");
    generated.remove("description");

    // Metadata first so the document reads the way a person opens it: identity,
    // then the shape.
    let mut root = Map::new();
    root.insert(
        "$schema".to_owned(),
        json!("https://json-schema.org/draft/2020-12/schema"),
    );
    root.insert("$id".to_owned(), json!(SCHEMA_URL));
    root.insert("title".to_owned(), json!("Acpilot extension manifest"));
    root.insert(
        "description".to_owned(),
        json!(
            "`extension.toml`, the entire interface between an extension and the daemon. It \
             names files and nothing else: there is no install script and no build step, every \
             contribution is parsed by the same reader as a user's own configuration, and a \
             contribution path may not leave the extension directory. The file is TOML — this \
             schema describes it, it does not replace it — and an editor reads it from the \
             `$schema` key or a `#:schema` first line."
        ),
    );
    root.insert(
        "$comment".to_owned(),
        json!(format!(
            "Apache-2.0. Generated from crates/acpilot-extensions/src/manifest.rs by \
             `cargo xtask schema`; edit the Rust, not this file. This is the v1 document and \
             is frozen: a change a `schema_version = {SCHEMA_VERSION}` loader would read \
             differently ships as extension-v2.json."
        )),
    );
    root.extend(generated);

    tighten_properties(&mut root);
    tighten_contribution(&mut root);

    let mut document = Value::Object(root);
    for_toml(&mut document);
    document
}

/// Add the patterns and bounds a derive cannot express.
fn tighten_properties(root: &mut Map<String, Value>) {
    let Some(Value::Object(properties)) = root.get_mut("properties") else {
        return;
    };

    pattern(properties.get_mut("id"), ID_PATTERN);
    if let Some(Value::Object(id)) = properties.get_mut("id") {
        // The pattern bounds each segment; this bounds the whole id, which is
        // what `validate_namespaced_id` checks before anything else.
        id.insert("maxLength".to_owned(), json!(128));
    }
    pattern(properties.get_mut("publisher"), SEGMENT_PATTERN);
    pattern(properties.get_mut("version"), VERSION_PATTERN);
    pattern(properties.get_mut("min_acpilot"), VERSION_PATTERN);

    if let Some(Value::Object(schema)) = properties.get_mut("$schema") {
        schema.insert("format".to_owned(), json!("uri"));
    }
    if let Some(Value::Object(version)) = properties.get_mut("schema_version") {
        version.insert("const".to_owned(), json!(SCHEMA_VERSION));
    }
    if let Some(Value::Object(contributes)) = properties.get_mut("contributes") {
        // "Delete the file instead of emptying it" is the loader's rule, and the
        // schema can say the same thing in one keyword.
        contributes.insert("minItems".to_owned(), json!(1));
    }
}

/// The `path` half of a contribution, in the definition the array refers to.
fn tighten_contribution(root: &mut Map<String, Value>) {
    let Some(Value::Object(definitions)) = root.get_mut("$defs") else {
        return;
    };
    let Some(Value::Object(contribution)) = definitions.get_mut("Contribution") else {
        return;
    };
    let Some(Value::Object(properties)) = contribution.get_mut("properties") else {
        return;
    };
    pattern(properties.get_mut("path"), RELATIVE_PATH_PATTERN);
    if let Some(Value::Object(path)) = properties.get_mut("path") {
        path.insert(
            "description".to_owned(),
            json!(
                "Path relative to the extension directory, with forward slashes. `.` and `..` \
                 segments are refused by the loader; this pattern cannot state that, so it \
                 states the other half — no leading slash, no backslash."
            ),
        );
    }
}

/// Put `pattern` on a property, if the property is there.
fn pattern(property: Option<&mut Value>, expression: &str) {
    if let Some(Value::Object(property)) = property {
        property.insert("pattern".to_owned(), json!(expression));
    }
}

/// Make the generated types say only what a TOML file can say.
///
/// `Option<T>` generates `"type": ["string", "null"]`, and TOML has no null:
/// publishing it would describe a file nobody can write and offer an editor a
/// completion that does not parse. "Absent" is the only way to say "not set".
/// The agent manifest's schema applies the same rule to its own document; a test
/// here fails if any null survives.
fn for_toml(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|kind| kind != "null");
            }
            if let Some(Value::Array(members)) = map.get_mut("anyOf") {
                members.retain(|member| member.get("type") != Some(&Value::from("null")));
            }
            if map.get("default") == Some(&Value::Null) {
                map.remove("default");
            }
            // `u32` generates `"format": "uint32"`, a Rust type name rather than
            // a keyword from the standard. `minimum` already says what a reader
            // needs to know.
            let rust_integer = map
                .get("format")
                .and_then(Value::as_str)
                .is_some_and(|format| format.starts_with("uint") || format.starts_with("int"));
            if rust_integer {
                map.remove("format");
            }
            // A one-item `type` array is noise; JSON Schema accepts the string.
            let single = match map.get("type") {
                Some(Value::Array(types)) if types.len() == 1 => types.first().cloned(),
                _ => None,
            };
            if let Some(single) = single {
                map.insert("type".to_owned(), single);
            }
            for child in map.values_mut() {
                for_toml(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                for_toml(item);
            }
        }
        _ => {}
    }
}
