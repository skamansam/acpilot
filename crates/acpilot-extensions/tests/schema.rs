//! The published extension schema: that it is current, and that it says what the
//! loader says.
//!
//! The schema is generated (`acpilot_extensions::json_schema`) and committed at
//! `site/public/schema/extension-v1.json` because it is published from there. This
//! file is the gate: a renamed field, a new contribution kind or a changed
//! requirement shows up as a failing diff rather than as an editor that completes a
//! key `ext add` refuses.

use std::fs;
use std::path::{Path, PathBuf};

use acpilot_extensions::{
    Manifest, SCHEMA_PATH, SCHEMA_URL, SCHEMA_VERSION, json_schema, schema_json_text,
};
use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn committed() -> String {
    let path = repo_root().join(SCHEMA_PATH);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn schema() -> Value {
    serde_json::from_str(&committed()).expect("the committed schema is JSON")
}

/// Validate one `extension.toml` against the published schema.
fn validate(text: &str) -> Result<(), String> {
    let parsed: toml::Value = toml::from_str(text).map_err(|error| format!("not TOML: {error}"))?;
    let instance = serde_json::to_value(parsed).expect("a TOML value serialises as JSON");
    let validator = jsonschema::validator_for(&schema()).expect("the schema compiles");
    validator
        .validate(&instance)
        .map_err(|error| error.to_string())
}

/// What the loader says about the same text.
fn load(text: &str) -> Result<(), String> {
    Manifest::parse("extension.toml", text)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// A minimal valid manifest, with `extra` appended and fields overridable.
fn manifest(extra: &str) -> String {
    format!(
        "schema_version = 1\nid = \"acpilot.test\"\nname = \"Test pack\"\nversion = \"1.0.0\"\n\
         publisher = \"acpilot\"\ndescription = \"A test extension.\"\n{extra}\
         \n[[contributes]]\nkind = \"theme\"\npath = \"theme.toml\"\n"
    )
}

#[test]
fn the_committed_schema_is_current() {
    let generated = schema_json_text();
    let committed = committed();
    assert_eq!(
        committed, generated,
        "{SCHEMA_PATH} is stale: run `cargo xtask schema`"
    );
}

#[test]
fn the_schema_identifies_itself() {
    let doc = schema();
    assert_eq!(doc["$id"], SCHEMA_URL);
    assert_eq!(
        doc["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(doc["properties"]["schema_version"]["const"], SCHEMA_VERSION);

    // The URL and the file are the same artifact: a stale `$id` would have two
    // editors reading two different files.
    let file = Path::new(SCHEMA_PATH)
        .file_name()
        .and_then(|name| name.to_str());
    assert_eq!(
        Path::new(SCHEMA_URL).file_name().and_then(|n| n.to_str()),
        file
    );

    // D6, applied to the second artifact: one licence, where a person copying the
    // file will see it, and the promise that v1 is frozen.
    let comment = doc["$comment"].as_str().expect("$comment is a string");
    assert!(comment.contains("Apache-2.0"), "{comment}");
    assert!(comment.contains("frozen"), "{comment}");
}

#[test]
fn every_reference_resolves() {
    let doc = schema();
    let defs = doc["$defs"].as_object().cloned().unwrap_or_default();
    let mut references = Vec::new();
    collect_refs(&doc, &mut references);
    assert!(!references.is_empty(), "the schema still uses a definition");
    for reference in references {
        let name = reference
            .strip_prefix("#/$defs/")
            .unwrap_or_else(|| panic!("unexpected reference {reference}"));
        assert!(
            defs.contains_key(name),
            "{reference} has no definition; $defs holds {:?}",
            defs.keys().collect::<Vec<_>>()
        );
    }
}

fn collect_refs(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                found.push(reference.to_owned());
            }
            for child in map.values() {
                collect_refs(child, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_refs(item, found)),
        _ => {}
    }
}

#[test]
fn the_types_are_types_a_toml_file_can_have() {
    let text = json_schema().to_string();
    // TOML has no null and no `uint32`; publishing either would offer an editor a
    // value the file cannot hold.
    assert!(!text.contains("\"null\""), "a type mentions null");
    assert!(!text.contains("\"uint"), "{text}");
}

#[test]
fn every_shipped_extension_manifest_passes() {
    let dir = repo_root().join("extensions");
    let mut checked = 0;
    for entry in fs::read_dir(&dir).expect("extensions/ is part of the repository") {
        let path = entry
            .expect("a readable entry")
            .path()
            .join("extension.toml");
        if !path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&path).expect("a readable manifest");
        assert_eq!(
            validate(&text),
            Ok(()),
            "{} does not satisfy {SCHEMA_URL}",
            path.display()
        );
        assert_eq!(load(&text), Ok(()), "{} does not load", path.display());
        checked += 1;
    }
    assert!(checked > 0, "no shipped manifest under {}", dir.display());
}

#[test]
fn the_schema_refuses_what_the_loader_refuses() {
    let cases: [(&str, String, &str); 6] = [
        (
            "an unknown key",
            manifest("home_page = \"https://example.test\"\n"),
            "unknown field",
        ),
        (
            "an id without a publisher segment",
            manifest("").replace("id = \"acpilot.test\"", "id = \"test\""),
            "must be namespaced",
        ),
        (
            "a version without a patch",
            manifest("").replace("version = \"1.0.0\"", "version = \"1.0\""),
            "must be MAJOR.MINOR.PATCH",
        ),
        (
            "an absolute contribution path",
            manifest("").replace("path = \"theme.toml\"", "path = \"/etc/theme.toml\""),
            "must be relative",
        ),
        (
            "a schema version this build does not speak",
            manifest("").replace("schema_version = 1", "schema_version = 2"),
            "schema_version 2",
        ),
        (
            "no contributions",
            "schema_version = 1\nid = \"acpilot.test\"\nname = \"Test pack\"\nversion = \"1.0.0\"\n\
             publisher = \"acpilot\"\ndescription = \"A test extension.\"\ncontributes = []\n"
                .to_owned(),
            "may not be empty",
        ),
    ];

    for (what, text, loader_says) in cases {
        assert!(
            validate(&text).is_err(),
            "{what}: the schema accepted a file the loader refuses:\n{text}"
        );
        let loader_verdict = load(&text).expect_err(what);
        assert!(
            loader_verdict.contains(loader_says),
            "{what}: expected {loader_says:?}, got {loader_verdict:?}"
        );
    }
}

#[test]
fn the_schema_accepts_what_the_loader_accepts() {
    assert_eq!(validate(&manifest("")), Ok(()), "the minimum manifest");
    assert_eq!(load(&manifest("")), Ok(()), "the minimum manifest");

    // The optional keys a real pack uses, and every kind the loader knows: a kind
    // the schema forgot would make a valid pack incomplete in an editor.
    let kinds = [
        "theme",
        "language_servers",
        "agents",
        "grammars",
        "snippets",
        "keymap",
        "transcript_parser",
    ];
    for kind in kinds {
        let text = manifest("").replace("kind = \"theme\"", &format!("kind = {kind:?}"));
        assert_eq!(validate(&text), Ok(()), "{kind} in the schema");
        assert_eq!(load(&text), Ok(()), "{kind} in the loader");
    }

    let with_optional =
        manifest("homepage = \"https://example.test/pack\"\nmin_acpilot = \"0.1.0\"\n");
    assert_eq!(validate(&with_optional), Ok(()), "optional keys");
    assert_eq!(load(&with_optional), Ok(()), "optional keys");

    // The schema key an editor follows is ignored by the loader, not refused.
    let with_schema_key = manifest("").replace(
        "schema_version = 1",
        &format!("schema_version = 1\n\"$schema\" = \"{SCHEMA_URL}\""),
    );
    assert_eq!(load(&with_schema_key), Ok(()), "the loader ignores $schema");
}

#[test]
fn a_kind_the_loader_does_not_know_is_refused_by_both() {
    let text = manifest("").replace("kind = \"theme\"", "kind = \"fonts\"");
    assert!(validate(&text).is_err(), "the schema accepted a new kind");
    // The loader's message comes from serde, which lists what it knows.
    let message = load(&text).expect_err("the loader refuses it");
    assert!(message.contains("unknown variant"), "{message}");
}

#[test]
fn the_schema_requires_what_the_loader_requires() {
    // Read the requirement out of the schema, then ask the loader the same
    // question both ways: the full file loads, and dropping each required key is
    // the error that names it.
    let required: Vec<String> = schema()["required"]
        .as_array()
        .expect("required is a list")
        .iter()
        .map(|field| field.as_str().expect("a field name").to_owned())
        .collect();
    assert_eq!(load(&manifest("")), Ok(()), "the full manifest loads");

    let full = manifest("");
    for field in &required {
        let text = if field == "contributes" {
            // `[[contributes]]` is a table section, not a key line.
            let head = full
                .split("\n[[contributes]]")
                .next()
                .expect("the section is there");
            format!("{head}\n")
        } else {
            let without: Vec<&str> = full
                .lines()
                .filter(|line| !line.starts_with(&format!("{field} =")))
                .collect();
            format!("{}\n", without.join("\n"))
        };
        let message = load(&text).expect_err(field);
        assert!(
            message.contains(field),
            "without {field}: expected the loader to name it, got {message:?}"
        );
        assert!(
            validate(&text).is_err(),
            "without {field}: the schema still accepts it"
        );
    }
}
