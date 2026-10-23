//! The published schema: that it is current, that it says what the loader says,
//! and that a file it accepts is a file the loader accepts.
//!
//! The schema is generated (`acpilot_agent::json_schema`) and committed at
//! `site/public/schema/agents-v1.json`, because it is published from there. That
//! makes this file the gate: a renamed field, a new transport, a changed
//! requirement shows up as a failing diff here rather than as an editor that
//! completes a key the loader refuses.

use std::fs;
use std::path::{Path, PathBuf};

use acpilot_agent::secret::MapResolver;
use acpilot_agent::{SCHEMA_PATH, SCHEMA_URL, SCHEMA_VERSION, json_schema, schema_json_text};
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

/// Validate one `agents.toml` against the published schema.
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
    let resolver = MapResolver::new().with("anthropic", "sk-ant-test");
    acpilot_agent::manifest::parse(Path::new("agents.toml"), text, &resolver)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn shipped_manifests() -> Vec<PathBuf> {
    let dir = repo_root().join("extensions");
    let mut found: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("extensions/ is part of the repository")
        .map(|entry| entry.expect("a readable entry").path().join("agents.toml"))
        .filter(|path| path.is_file())
        .collect();
    found.sort();
    assert!(
        !found.is_empty(),
        "no shipped agents.toml under {}",
        dir.display()
    );
    found
}

#[test]
fn the_committed_schema_is_current() {
    let generated = schema_json_text();
    let committed = committed();
    assert_eq!(
        committed, generated,
        "{} is stale: run `cargo xtask schema`",
        SCHEMA_PATH
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

    // The URL and the file are the same artifact, so the name has to agree: a
    // stale `$id` would have two editors reading two different files.
    let file = Path::new(SCHEMA_PATH)
        .file_name()
        .and_then(|name| name.to_str());
    assert_eq!(
        Path::new(SCHEMA_URL).file_name().and_then(|n| n.to_str()),
        file
    );

    // D6: one artifact, one licence, stated where someone copying the file will
    // see it.
    let comment = doc["$comment"].as_str().expect("$comment is a string");
    assert!(comment.contains("Apache-2.0"), "{comment}");
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
    let doc = schema();
    let mut problems: Vec<String> = Vec::new();
    // TOML has no null and no `uint64`; a published schema should not offer
    // either, or an editor completes a value the file cannot hold.
    if json_schema().to_string().contains("\"null\"") {
        problems.push("a type mentions null".to_owned());
    }
    for (name, property) in doc["properties"].as_object().expect("root properties") {
        if let Some(format) = property.get("format").and_then(Value::as_str)
            && (format.starts_with("int") || format.starts_with("uint"))
        {
            problems.push(format!("{name} has the Rust format {format}"));
        }
    }
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn the_shipped_presets_pass_the_schema() {
    for path in shipped_manifests() {
        let text = fs::read_to_string(&path).expect("a readable preset");
        assert_eq!(
            validate(&text),
            Ok(()),
            "{} does not satisfy {}",
            path.display(),
            SCHEMA_URL
        );
        assert_eq!(load(&text), Ok(()), "{} does not load", path.display());
    }
}

#[test]
fn the_schema_refuses_what_the_loader_refuses() {
    const HEADER: &str = "schema_version = 1\n";
    let cases: [(&str, String, &str); 8] = [
        (
            "a misspelled key",
            format!(
                "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"acp-stdio\"\ncommand = \"f\"\n\
                 sentinal = \"s\"\n"
            ),
            "unknown field",
        ),
        (
            "a key from another transport",
            format!(
                "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"acp-stdio\"\ncommand = \"f\"\n\
                 sentinel = \"s\"\n"
            ),
            "has no meaning",
        ),
        (
            "a missing required key",
            format!("{HEADER}[[agents]]\nid = \"a\"\ntransport = \"acp-stdio\"\n"),
            "requires \"command\"",
        ),
        (
            "an id that is not a key",
            format!(
                "{HEADER}[[agents]]\nid = \"Claude\"\ntransport = \"acp-stdio\"\ncommand = \"f\"\n"
            ),
            "agent id",
        ),
        (
            "an unknown transport",
            format!("{HEADER}[[agents]]\nid = \"a\"\ntransport = \"stdio\"\ncommand = \"f\"\n"),
            "unknown variant",
        ),
        (
            "a schema version this build does not speak",
            "schema_version = 2\n".to_owned(),
            "schema_version 2",
        ),
        (
            "an empty file",
            format!("{HEADER}agents = []\n"),
            "may not be empty",
        ),
        (
            "an unknown parser",
            format!(
                "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"pty\"\ncommand = \"f\"\n\
                 parser = \"ansi\"\nsentinel = \"s\"\n"
            ),
            "is not `plain-transcript`",
        ),
    ];

    for (what, text, loader_says) in cases {
        let schema_verdict = validate(&text);
        assert!(
            schema_verdict.is_err(),
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
    const HEADER: &str = "schema_version = 1\n";
    let cases = [
        (
            "the API spelling of a transport",
            format!("{HEADER}[[agents]]\nid = \"a\"\ntransport = \"acp_stdio\"\ncommand = \"f\"\n"),
        ),
        (
            "the schema key an editor follows",
            format!(
                "\"$schema\" = \"{SCHEMA_URL}\"\n{HEADER}[[agents]]\nid = \"a\"\n\
                 transport = \"acp-stdio\"\ncommand = \"f\"\n"
            ),
        ),
        (
            "a script parser",
            format!(
                "{HEADER}[[agents]]\nid = \"a\"\ntransport = \"pty\"\ncommand = \"f\"\n\
                 parser = \"script:./freebuff.lua\"\nsentinel = \"s\"\n"
            ),
        ),
        (
            "an HTTP agent with a credential reference",
            format!(
                "{HEADER}[[agents]]\nid = \"remote\"\ntransport = \"http-jsonrpc\"\n\
                 url = \"https://agent.test/acp\"\nauth = \"keyring:anthropic\"\n"
            ),
        ),
    ];

    for (what, text) in cases {
        assert_eq!(validate(&text), Ok(()), "{what}: the schema refused it");
        assert_eq!(load(&text), Ok(()), "{what}: the loader refused it");
    }
}

#[test]
fn the_schema_requires_what_the_loader_requires() {
    // Read the requirement out of the schema, then ask the loader the same
    // question in both directions: the minimum file loads, and dropping each
    // required key is the error that names it. A requirement added on one side
    // only cannot pass both.
    for branch in schema()["properties"]["agents"]["items"]["oneOf"]
        .as_array()
        .expect("one branch per transport")
        .clone()
    {
        let transport = branch["properties"]["transport"]["enum"][0]
            .as_str()
            .expect("a spelling")
            .to_owned();
        let required: Vec<String> = branch["required"]
            .as_array()
            .expect("required is a list")
            .iter()
            .map(|field| field.as_str().expect("a field name").to_owned())
            .collect();

        let text = manifest_with(&transport, &required);
        assert_eq!(
            load(&text),
            Ok(()),
            "{transport}: the schema's required list is not enough to load"
        );
        assert_eq!(
            validate(&text),
            Ok(()),
            "{transport}: the schema refuses its own minimum"
        );

        for field in &required {
            if field == "id" || field == "transport" {
                continue;
            }
            let without: Vec<String> = required
                .iter()
                .filter(|kept| *kept != field)
                .cloned()
                .collect();
            let text = manifest_with(&transport, &without);
            let message = load(&text).expect_err(field);
            assert!(
                message.contains(&format!("requires \"{field}\"")),
                "{transport} without {field}: {message}"
            );
            assert!(
                validate(&text).is_err(),
                "{transport} without {field}: the schema still accepts it"
            );
        }
    }
}

/// A minimal manifest holding exactly these keys, with values that are valid for
/// every transport (the loader ignores the ones the transport does not use...
/// which it does not: it refuses them, so the list has to be exactly right).
fn manifest_with(transport: &str, fields: &[String]) -> String {
    let mut text = String::from("schema_version = 1\n\n[[agents]]\n");
    for field in fields {
        let value = match field.as_str() {
            "id" => "\"a\"".to_owned(),
            "transport" => format!("{transport:?}"),
            "command" => "\"f\"".to_owned(),
            "parser" => "\"plain-transcript\"".to_owned(),
            "sentinel" => "\"s\"".to_owned(),
            "url" => "\"https://agent.test/acp\"".to_owned(),
            other => panic!("the schema requires {other:?}, which this test cannot write"),
        };
        text.push_str(&format!("{field} = {value}\n"));
    }
    text
}
