//! Tool schemas and the strict parsing of what the model called.
//!
//! The tool ceiling is a recipe field, and this module is the only place that
//! turns it into JSON Schema. Nothing here contacts a client: a parsed
//! [`ToolInvocation`] is a description of a call, and the ACP loop is what runs
//! it. Keeping the split here means an unsupported tool name is a test, not a
//! turn that discovers it.
//!
//! `search` is deliberately absent even when the recipe asks for it: ACP v1 has
//! no workspace-search callback. Faking one with a shell would need
//! `run_commands` as well, which a recipe may have turned off. The agent says so
//! in a notice rather than offering a tool that cannot work.

use acpilot_model::ToolSpec;
use acpilot_proto::model::ToolExposure;
use serde_json::{Value, json};

/// Said once per session when a recipe asks for a search tool.
pub const SEARCH_UNAVAILABLE: &str = "search is not offered: ACP has no workspace-search callback, and this agent does not run a shell to fake one";

/// What a valid tool call asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolInvocation {
    /// Read a file through `fs/read_text_file`.
    ReadFile {
        /// Absolute path, as the model wrote it.
        path: String,
        /// First line, 1-based, when the model named one.
        line: Option<u32>,
        /// Number of lines, when the model named one.
        limit: Option<u32>,
    },
    /// Write a file through `fs/write_text_file`.
    WriteFile {
        /// Absolute path.
        path: String,
        /// Whole new contents.
        content: String,
    },
    /// Run a command through `terminal/create`, `terminal/wait_for_exit` and
    /// `terminal/output`.
    RunCommand {
        /// Program to run.
        command: String,
        /// Arguments, in order.
        args: Vec<String>,
    },
}

impl ToolInvocation {
    /// The tool name the model used.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::ReadFile { .. } => "read_file",
            Self::WriteFile { .. } => "write_file",
            Self::RunCommand { .. } => "run_command",
        }
    }

    /// One line for the tool-call title.
    #[must_use]
    pub fn title(&self) -> String {
        match self {
            Self::ReadFile { path, .. } => format!("Read {path}"),
            Self::WriteFile { path, .. } => format!("Write {path}"),
            Self::RunCommand { command, args } => {
                if args.is_empty() {
                    format!("Run {command}")
                } else {
                    format!("Run {command} {}", args.join(" "))
                }
            }
        }
    }
}

/// Why a tool call could not be turned into an invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The model named a tool the recipe did not offer.
    UnknownTool(String),
    /// The arguments were not a JSON object.
    NotJson(String),
    /// The arguments parsed but a required field was missing or the wrong type.
    Schema(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTool(name) => {
                write!(formatter, "`{name}` is not one of the offered tools")
            }
            Self::NotJson(message) => write!(formatter, "arguments are not JSON: {message}"),
            Self::Schema(message) => {
                write!(formatter, "arguments do not match the schema: {message}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Build the tool list the model sees for one recipe.
///
/// Order is fixed — read, write, run — so two runs of the same recipe send the
/// same bytes and a diff between requests is about the prompt, not the order.
#[must_use]
pub fn schema(exposure: &ToolExposure) -> Vec<ToolSpec> {
    let mut tools = Vec::new();
    if exposure.read_files {
        tools.push(ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a text file through the editor. Give an absolute path; line and limit are optional."
                .to_owned(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path of the file to read" },
                    "line": { "type": "integer", "minimum": 1, "description": "First line, 1-based" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Maximum lines to read" }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        });
    }
    if exposure.write_files {
        tools.push(ToolSpec {
            name: "write_file".to_owned(),
            description: "Write a whole text file through the editor. The client asks the user before a write outside the workspace."
                .to_owned(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path of the file to write" },
                    "content": { "type": "string", "description": "The complete new contents" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        });
    }
    if exposure.run_commands {
        tools.push(ToolSpec {
            name: "run_command".to_owned(),
            description: "Run one program in a terminal. It is not a shell, so pipes and redirection are not available; put each argument in `args`."
                .to_owned(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Program to run, without arguments" },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Arguments, one string each"
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        });
    }
    tools
}

/// Parse one tool call, strictly.
///
/// A missing required field is an error rather than an empty default: a write
/// with no content is a file erased, and guessing is the worst possible repair.
///
/// # Errors
///
/// [`ParseError`] names whether the tool, the JSON, or a field was wrong.
pub fn parse_call(
    name: &str,
    raw: &str,
    exposure: &ToolExposure,
) -> Result<ToolInvocation, ParseError> {
    let offered = offered_name(name, exposure);
    if !offered {
        return Err(ParseError::UnknownTool(name.to_owned()));
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|error| ParseError::NotJson(error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| ParseError::NotJson("arguments are not an object".to_owned()))?;

    match name {
        "read_file" => {
            let path = string(object, "path")?;
            let line = optional_u32(object, "line")?;
            let limit = optional_u32(object, "limit")?;
            Ok(ToolInvocation::ReadFile { path, line, limit })
        }
        "write_file" => {
            let path = string(object, "path")?;
            let content = string(object, "content")?;
            Ok(ToolInvocation::WriteFile { path, content })
        }
        "run_command" => {
            let command = string(object, "command")?;
            let args = match object.get("args") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.as_str().map(str::to_owned).ok_or_else(|| {
                            ParseError::Schema("`args` must contain strings only".to_owned())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => return Err(ParseError::Schema("`args` must be an array".to_owned())),
            };
            Ok(ToolInvocation::RunCommand { command, args })
        }
        other => Err(ParseError::UnknownTool(other.to_owned())),
    }
}

/// Whether the recipe offered this tool.
fn offered_name(name: &str, exposure: &ToolExposure) -> bool {
    match name {
        "read_file" => exposure.read_files,
        "write_file" => exposure.write_files,
        "run_command" => exposure.run_commands,
        _ => false,
    }
}

fn string(object: &serde_json::Map<String, Value>, key: &str) -> Result<String, ParseError> {
    match object.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        Some(Value::String(_)) => Err(ParseError::Schema(format!("`{key}` is empty"))),
        Some(_) => Err(ParseError::Schema(format!("`{key}` must be a string"))),
        None => Err(ParseError::Schema(format!("`{key}` is required"))),
    }
}

fn optional_u32(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<u32>, ParseError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| ParseError::Schema(format!("`{key}` must be a positive integer"))),
        Some(_) => Err(ParseError::Schema(format!(
            "`{key}` must be a positive integer"
        ))),
    }
}

/// The message that asks the model to fix one malformed call.
///
/// It repeats the arguments it received and the error, so the model repairs what
/// it wrote rather than starting over with a different call.
#[must_use]
pub fn repair_message(name: &str, raw: &str, error: &ParseError) -> String {
    format!(
        "Your call to `{name}` was not accepted: {error}. The arguments were: {raw}. Reply with the same call again, as one JSON object that matches the tool schema. Do not add prose."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> ToolExposure {
        ToolExposure {
            read_files: true,
            write_files: true,
            run_commands: true,
            search: true,
            max_steps: 8,
        }
    }

    #[test]
    fn the_schema_follows_the_ceiling() {
        let exposure = ToolExposure {
            read_files: true,
            write_files: false,
            run_commands: false,
            search: false,
            max_steps: 4,
        };
        let tools = schema(&exposure);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(
            tools[0].schema["required"],
            json!(["path"]),
            "the path is the one field a read cannot guess"
        );
    }

    #[test]
    fn search_is_never_offered_because_acp_has_no_callback_for_it() {
        let tools = schema(&all());
        assert!(
            tools.iter().all(|tool| tool.name != "search"),
            "offering a tool with no transport is worse than omitting it"
        );
        assert_eq!(tools.len(), 3);
    }

    #[test]
    fn a_read_parses_with_and_without_a_range() {
        let exposure = all();
        let read = parse_call("read_file", r#"{"path":"/work/a.rs"}"#, &exposure).expect("valid");
        assert_eq!(
            read,
            ToolInvocation::ReadFile {
                path: "/work/a.rs".to_owned(),
                line: None,
                limit: None
            }
        );

        let ranged = parse_call(
            "read_file",
            r#"{"path":"/work/a.rs","line":10,"limit":20}"#,
            &exposure,
        )
        .expect("valid");
        assert_eq!(
            ranged,
            ToolInvocation::ReadFile {
                path: "/work/a.rs".to_owned(),
                line: Some(10),
                limit: Some(20)
            }
        );
    }

    #[test]
    fn a_write_without_content_is_refused_rather_than_erasing_the_file() {
        let error = parse_call("write_file", r#"{"path":"/work/a.rs"}"#, &all())
            .expect_err("content is required");
        assert!(matches!(error, ParseError::Schema(_)), "{error}");
        assert!(error.to_string().contains("content"), "{error}");
    }

    #[test]
    fn a_tool_the_recipe_does_not_expose_is_unknown() {
        let exposure = ToolExposure {
            read_files: true,
            write_files: false,
            run_commands: false,
            search: false,
            max_steps: 2,
        };
        let error = parse_call("write_file", r#"{"path":"/a","content":"x"}"#, &exposure)
            .expect_err("not offered");
        assert_eq!(error, ParseError::UnknownTool("write_file".to_owned()));
    }

    #[test]
    fn malformed_json_is_named_as_json_and_not_as_a_missing_field() {
        let error = parse_call("run_command", "{command: ", &all()).expect_err("not JSON");
        assert!(matches!(error, ParseError::NotJson(_)), "{error}");
    }

    #[test]
    fn a_command_keeps_its_arguments_in_order() {
        let call = parse_call(
            "run_command",
            r#"{"command":"cargo","args":["test","--workspace"]}"#,
            &all(),
        )
        .expect("valid");
        assert_eq!(
            call,
            ToolInvocation::RunCommand {
                command: "cargo".to_owned(),
                args: vec!["test".to_owned(), "--workspace".to_owned()]
            }
        );
        assert_eq!(call.title(), "Run cargo test --workspace");
    }

    #[test]
    fn an_empty_path_is_a_schema_error() {
        let error = parse_call("read_file", r#"{"path":"  "}"#, &all()).expect_err("empty");
        assert!(matches!(error, ParseError::Schema(_)), "{error}");
    }

    #[test]
    fn the_repair_message_repeats_the_arguments_it_wants_fixed() {
        let error = ParseError::NotJson("expected value".to_owned());
        let message = repair_message("write_file", "{bad", &error);
        assert!(
            message.contains("{bad"),
            "the model sees its own call: {message}"
        );
        assert!(message.contains("`write_file`"), "{message}");
        assert!(message.contains("one JSON object"), "{message}");
    }
}
