//! Promotion: one recipe becomes one `[[agents]]` entry.
//!
//! A promoted agent is not a new kind of agent. It is an `acp-stdio` line whose
//! command is `acpilot-agent-lm`, so `agents.toml` stays the one place that says
//! what an agent is: a promoted entry is read by the same loader, listed by the
//! same registry, and guarded by the same permission broker as a hand-written one.
//!
//! That makes this module a text editor rather than a second manifest type. It
//! owns three rules:
//!
//! - **One entry per id.** Promoting twice replaces the line this code wrote
//!   instead of appending a second one, so a re-promotion is idempotent.
//! - **A hand-written entry is never overwritten.** A person who wrote an agent
//!   by hand may have chosen another command, another `restart` or an environment
//!   variable promotion knows nothing about. The id collision is refused with the
//!   file and the id, rather than silently trading their entry for ours.
//! - **The entry says where it came from.** A `# acpilot:promoted` comment
//!   carries the recipe file and its modification time, and that comment is also
//!   the mark that makes the entry ours to replace or remove. A comment above a
//!   block that has been edited by hand is a comment, not a claim; the block still
//!   has to parse and match the id.
//!
//! Removal is the inverse of writing: the block goes, and when it was the only
//! agent the file is removed instead of left as an empty manifest, because the
//! loader refuses a file with no agents and a profile with no file is the state
//! this started from.

use std::path::{Path, PathBuf};

use crate::manifest::{MANIFEST_FILE, SCHEMA_VERSION, parse_definitions};

/// The comment that marks an entry this module wrote.
///
/// The text after the prefix is provenance for a person — the recipe file and its
/// mtime — and is never parsed back. Whether an entry is ours is decided by this
/// prefix, so the format can gain fields without making old entries look foreign.
pub const MARKER: &str = "# acpilot:promoted";

/// The command a promoted agent runs.
pub const COMMAND: &str = "acpilot-agent-lm";

/// The table header every agent block starts with.
const HEADER: &str = "[[agents]]";

/// One entry to write, as the recipe and the promotion request describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promoted {
    /// Agent id. Also the value `--recipe` receives.
    pub id: String,
    /// Display name: the recipe's label unless the request named one.
    pub name: String,
    /// The recipe file, relative to the profile, for the provenance comment.
    pub recipe: String,
    /// The recipe's modification time, for the provenance comment.
    pub modified_at: String,
    /// Profile the agent reads the recipe from. Empty when the caller has no name.
    pub profile: String,
}

/// Why a promotion or a removal did not happen.
#[derive(Debug, thiserror::Error)]
pub enum PromoteError {
    /// The manifest could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// An entry with this id exists and this module did not write it.
    #[error("`{id}` in {path} was written by hand, so promotion leaves it alone")]
    HandWritten {
        /// The id that collided.
        id: String,
        /// The manifest.
        path: PathBuf,
    },
    /// There is nothing to remove.
    #[error("{path} has no agent `{id}`")]
    Missing {
        /// The id that was asked for.
        id: String,
        /// The manifest.
        path: PathBuf,
    },
    /// The edited file does not load, so it is not written.
    #[error("{path} would not load after the edit: {message}")]
    Invalid {
        /// The manifest.
        path: PathBuf,
        /// The loader's own message.
        message: String,
    },
}

/// Write `entry` into the manifest at `path`, creating the file when it is absent.
///
/// # Errors
///
/// [`PromoteError::HandWritten`] when the id belongs to an entry this module did
/// not write, [`PromoteError::Invalid`] when the edited document would not load,
/// and [`PromoteError::Io`] when the file cannot be read or written.
pub fn promote(path: &Path, entry: &Promoted) -> Result<(), PromoteError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(PromoteError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let text = insert(existing.as_deref(), path, entry)?;
    write_atomic(path, &text)
}

/// Remove the promoted entry `id` from the manifest at `path`.
///
/// When the entry was the last agent the file is removed: an empty manifest does
/// not load, and "no file" is the state promotion started from.
///
/// # Errors
///
/// [`PromoteError::Missing`] when there is no such entry,
/// [`PromoteError::HandWritten`] when the entry exists but is not ours,
/// [`PromoteError::Invalid`] when the remaining file would not load, and
/// [`PromoteError::Io`] when the file cannot be read or written.
pub fn demote(path: &Path, id: &str) -> Result<(), PromoteError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PromoteError::Missing {
                id: id.to_owned(),
                path: path.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(PromoteError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    match remove(&text, path, id)? {
        Some(text) => write_atomic(path, &text),
        None => std::fs::remove_file(path).map_err(|source| PromoteError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// The block this module writes for `entry`, without a trailing blank line.
///
/// `args` and `env` are written as inline values rather than tables: the block
/// stays short enough to read in a diff, and it is the shape the documented
/// example uses.
#[must_use]
pub fn entry_text(entry: &Promoted) -> String {
    let mut text = String::new();
    text.push_str(HEADER);
    text.push('\n');
    text.push_str(&format!(
        "{MARKER} recipe={} mtime={}\n",
        quote(&entry.recipe),
        quote(&entry.modified_at)
    ));
    text.push_str(&format!("id = {}\n", quote(&entry.id)));
    text.push_str(&format!("name = {}\n", quote(&entry.name)));
    text.push_str("transport = \"acp-stdio\"\n");
    text.push_str(&format!("command = {}\n", quote(COMMAND)));
    text.push_str(&format!("args = [\"--recipe\", {}]\n", quote(&entry.id)));
    // A promoted agent whose process dies mid-session should come back: the
    // supervisor already limits the attempts, and a recipe does not change.
    text.push_str("restart = \"on-crash\"\n");
    if !entry.profile.trim().is_empty() {
        text.push_str(&format!(
            "env = {{ ACPILOT_RECIPE_PROFILE = {} }}\n",
            quote(&entry.profile)
        ));
    }
    text
}

/// Insert or replace `entry` in `text`, or start a new manifest when `None`.
///
/// The result is validated with the same loader the daemon uses, so a promotion
/// that would write a manifest the loader refuses is an error here rather than a
/// daemon that fails its next start.
fn insert(text: Option<&str>, path: &Path, entry: &Promoted) -> Result<String, PromoteError> {
    let header = format!("schema_version = {SCHEMA_VERSION}\n");
    let lines: Vec<&str> = text.unwrap_or(&header).lines().collect();
    let block = entry_text(entry);
    let block_lines: Vec<&str> = block.lines().collect();

    let sections = blocks(&lines);
    let replaced = sections
        .iter()
        .find(|section| section.id.as_deref() == Some(&entry.id));

    let mut output: Vec<String> = Vec::new();
    match replaced {
        Some(section) => {
            if !section.promoted {
                return Err(PromoteError::HandWritten {
                    id: entry.id.clone(),
                    path: path.to_path_buf(),
                });
            }
            for line in &lines[..section.start] {
                output.push((*line).to_owned());
            }
            for line in &block_lines {
                output.push((*line).to_owned());
            }
            for line in &lines[section.end..] {
                output.push((*line).to_owned());
            }
        }
        None => {
            for line in &lines {
                output.push((*line).to_owned());
            }
            // One blank line between the last agent and this one, and none
            // between a header that already ends the file and this agent: the
            // rendered file is the one an editor would have produced.
            while output.last().is_some_and(|line| line.trim().is_empty()) {
                output.pop();
            }
            if !output.is_empty() {
                output.push(String::new());
            }
            for line in &block_lines {
                output.push((*line).to_owned());
            }
        }
    }

    let rendered = render(&output);
    validate(&rendered, path)?;
    Ok(rendered)
}

/// Remove the promoted entry `id`, or answer `None` when nothing would be left.
fn remove(text: &str, path: &Path, id: &str) -> Result<Option<String>, PromoteError> {
    let lines: Vec<&str> = text.lines().collect();
    let sections = blocks(&lines);
    let Some(section) = sections
        .iter()
        .find(|section| section.id.as_deref() == Some(id))
    else {
        return Err(PromoteError::Missing {
            id: id.to_owned(),
            path: path.to_path_buf(),
        });
    };
    if !section.promoted {
        return Err(PromoteError::HandWritten {
            id: id.to_owned(),
            path: path.to_path_buf(),
        });
    }

    // The blank line that separated this block from the previous one belongs to
    // the gap, not to either neighbour, so it leaves with the block.
    let mut start = section.start;
    while start > 0 && lines[start - 1].trim().is_empty() {
        start -= 1;
    }
    let mut output: Vec<String> = Vec::new();
    for line in &lines[..start] {
        output.push((*line).to_owned());
    }
    for line in &lines[section.end..] {
        output.push((*line).to_owned());
    }

    if !output.iter().any(|line| line.trim() == HEADER) {
        return Ok(None);
    }

    let rendered = render(&output);
    validate(&rendered, path)?;
    Ok(Some(rendered))
}

/// One `[[agents]]` block in a document.
struct Block {
    /// First line, inclusive.
    start: usize,
    /// One past the last line.
    end: usize,
    /// The `id` the block declares, when it declares one.
    id: Option<String>,
    /// Whether this module wrote it.
    promoted: bool,
}

fn blocks(lines: &[&str]) -> Vec<Block> {
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == HEADER)
        .map(|(index, _)| index)
        .collect();

    let mut blocks = Vec::with_capacity(starts.len());
    for (position, start) in starts.iter().copied().enumerate() {
        // A comment between two agents belongs to the one below it, so a block
        // runs to the next header and no further.
        let end = starts.get(position + 1).copied().unwrap_or(lines.len());
        let body = &lines[start..end];
        blocks.push(Block {
            start,
            end,
            id: body.iter().find_map(|line| id_of(line)),
            promoted: body
                .iter()
                .any(|line| line.trim_start().starts_with(MARKER)),
        });
    }
    blocks
}

/// The value of an `id = "…"` line, when the line is one.
fn id_of(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("id")?;
    let rest = rest.trim_start();
    let value = rest.strip_prefix('=')?.trim();
    unquote(value)
}

/// A TOML string value, basic or literal.
fn unquote(value: &str) -> Option<String> {
    if let Some(rest) = value.strip_prefix('\'') {
        return rest.split('\'').next().map(ToOwned::to_owned);
    }
    let rest = value.strip_prefix('"')?;
    let mut escaped = false;
    for (index, character) in rest.char_indices() {
        match character {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => {
                return serde_json::from_str::<String>(&value[..index + 2]).ok();
            }
            _ => escaped = false,
        }
    }
    None
}

/// A value written as a TOML basic string.
///
/// JSON string escaping and TOML basic-string escaping agree on everything a
/// path or a name can contain, and using one serializer means a quote in a label
/// cannot close the string early.
fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("a string is always serialisable")
}

/// Lines to a document with one trailing newline and no trailing blank lines.
fn render(lines: &[String]) -> String {
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let mut text = lines[..end].join("\n");
    text.push('\n');
    text
}

fn validate(text: &str, path: &Path) -> Result<(), PromoteError> {
    parse_definitions(&path.display().to_string(), text)
        .map(|_| ())
        .map_err(|error| PromoteError::Invalid {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
}

/// Write beside the target and rename, like every other file Acpilot owns.
fn write_atomic(path: &Path, text: &str) -> Result<(), PromoteError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| PromoteError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, text).map_err(|source| PromoteError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    std::fs::rename(&temporary, path).map_err(|source| {
        let _ = std::fs::remove_file(&temporary);
        PromoteError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// The manifest path inside a profile directory.
#[must_use]
pub fn path_in(profile: &Path) -> PathBuf {
    profile.join(MANIFEST_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> Promoted {
        Promoted {
            id: id.to_owned(),
            name: format!("{id} agent"),
            recipe: format!("recipes/{id}.toml"),
            modified_at: "2026-11-12T12:00:00Z".to_owned(),
            profile: "work".to_owned(),
        }
    }

    fn directory() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn a_promotion_creates_the_manifest_when_there_is_none() {
        let directory = directory();
        let path = path_in(directory.path());
        promote(&path, &entry("corpus-scraper")).expect("promote");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("schema_version = 1\n"), "{text}");
        assert!(text.contains("[[agents]]"), "{text}");
        assert!(
            text.contains("# acpilot:promoted recipe=\"recipes/corpus-scraper.toml\""),
            "the provenance comment names the recipe: {text}"
        );
        assert!(text.contains("command = \"acpilot-agent-lm\""), "{text}");
        assert!(
            parse_definitions(&path.display().to_string(), &text)
                .expect("the loader reads what promotion wrote")
                .first()
                .is_some_and(|agent| agent.id == "corpus-scraper"),
            "{text}"
        );
    }

    #[test]
    fn promoting_twice_replaces_the_entry_instead_of_appending_one() {
        let directory = directory();
        let path = path_in(directory.path());
        promote(&path, &entry("one")).expect("first");
        promote(&path, &entry("one")).expect("second");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            text.matches("[[agents]]").count(),
            1,
            "one id is one entry: {text}"
        );
        let parsed = parse_definitions("agents.toml", &text).expect("loads");
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn a_hand_written_entry_is_never_replaced() {
        let directory = directory();
        let path = path_in(directory.path());
        std::fs::write(
            &path,
            "schema_version = 1\n\n[[agents]]\nid = \"mine\"\ntransport = \"acp-stdio\"\ncommand = \"my-agent\"\n",
        )
        .expect("write");
        let error = promote(&path, &entry("mine")).expect_err("the entry is not ours");
        assert!(matches!(error, PromoteError::HandWritten { .. }), "{error}");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("command = \"my-agent\""),
            "the hand-written command survived: {text}"
        );
        assert!(!text.contains(MARKER), "{text}");
    }

    #[test]
    fn a_promotion_keeps_the_hand_written_agents_around_it() {
        let directory = directory();
        let path = path_in(directory.path());
        std::fs::write(
            &path,
            "schema_version = 1\n\n# Mine.\n[[agents]]\nid = \"mine\"\ntransport = \"acp-stdio\"\ncommand = \"my-agent\"\n",
        )
        .expect("write");
        promote(&path, &entry("promoted")).expect("promote");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# Mine."), "the comment survived: {text}");
        let parsed = parse_definitions("agents.toml", &text).expect("loads");
        let ids: Vec<&str> = parsed.iter().map(|agent| agent.id.as_str()).collect();
        assert_eq!(ids, ["mine", "promoted"]);
    }

    #[test]
    fn demoting_removes_only_the_block_promotion_wrote() {
        let directory = directory();
        let path = path_in(directory.path());
        std::fs::write(
            &path,
            "schema_version = 1\n\n[[agents]]\nid = \"mine\"\ntransport = \"acp-stdio\"\ncommand = \"my-agent\"\n",
        )
        .expect("write");
        promote(&path, &entry("promoted")).expect("promote");
        demote(&path, "promoted").expect("demote");

        let text = std::fs::read_to_string(&path).expect("the file remains");
        assert!(text.contains("my-agent"), "{text}");
        assert!(!text.contains("promoted"), "{text}");
        parse_definitions("agents.toml", &text).expect("the rest still loads");
    }

    #[test]
    fn demoting_the_last_agent_removes_the_file_instead_of_emptying_it() {
        let directory = directory();
        let path = path_in(directory.path());
        promote(&path, &entry("only")).expect("promote");
        demote(&path, "only").expect("demote");
        assert!(
            !path.exists(),
            "an empty manifest does not load, so there is no file"
        );
    }

    #[test]
    fn demoting_something_that_is_not_there_names_it() {
        let directory = directory();
        let path = path_in(directory.path());
        promote(&path, &entry("one")).expect("promote");
        let error = demote(&path, "two").expect_err("nothing to remove");
        assert!(matches!(error, PromoteError::Missing { .. }), "{error}");
    }

    #[test]
    fn demoting_a_hand_written_entry_is_refused() {
        let directory = directory();
        let path = path_in(directory.path());
        std::fs::write(
            &path,
            "schema_version = 1\n\n[[agents]]\nid = \"mine\"\ntransport = \"acp-stdio\"\ncommand = \"my-agent\"\n",
        )
        .expect("write");
        let error = demote(&path, "mine").expect_err("not ours to remove");
        assert!(matches!(error, PromoteError::HandWritten { .. }), "{error}");
        assert!(path.is_file(), "the file was left alone");
    }

    #[test]
    fn an_id_the_manifest_rules_refuse_is_refused_before_the_file_moves() {
        let directory = directory();
        let path = path_in(directory.path());
        let error = promote(&path, &entry("Not An Id")).expect_err("the loader refuses it");
        assert!(matches!(error, PromoteError::Invalid { .. }), "{error}");
        assert!(!path.exists(), "nothing was written");
    }

    #[test]
    fn the_marker_is_what_makes_an_entry_ours() {
        // A marker on a different id does not make a nearby block ours, and a
        // marker in the middle of another agent's body belongs to that agent.
        let text = "schema_version = 1\n\n[[agents]]\nid = \"a\"\ntransport = \"acp-stdio\"\ncommand = \"x\"\n\n[[agents]]\n# acpilot:promoted recipe=\"recipes/b.toml\"\nid = \"b\"\ntransport = \"acp-stdio\"\ncommand = \"acpilot-agent-lm\"\n";
        let lines: Vec<&str> = text.lines().collect();
        let sections = blocks(&lines);
        assert_eq!(sections.len(), 2);
        assert!(!sections[0].promoted);
        assert!(sections[1].promoted);
        assert_eq!(sections[1].id.as_deref(), Some("b"));
    }

    #[test]
    fn a_label_with_a_quote_cannot_break_the_document() {
        let directory = directory();
        let path = path_in(directory.path());
        let mut named = entry("quoted");
        named.name = "The \"best\" agent".to_owned();
        promote(&path, &named).expect("promote");
        let text = std::fs::read_to_string(&path).expect("read");
        let parsed = parse_definitions("agents.toml", &text).expect("loads");
        assert_eq!(parsed[0].name, "The \"best\" agent");
    }
}
