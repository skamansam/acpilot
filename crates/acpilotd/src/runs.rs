//! Run records: `runs.jsonl`, one line per run, appended and never rewritten.
//!
//! A recipe edit is a hypothesis and a run is the experiment, so a run has to
//! survive the thing that produced it. The record keeps the exact request that was
//! sent — the prompt, a hash of it, the selectors, the files they resolved to and
//! the budget — beside what came back: timings, usage, stop reason and warnings.
//! "It was better an hour ago" is then a diff between two lines rather than a
//! memory.
//!
//! The format is JSON Lines for the reason the transcript is: one record per line,
//! readable with `jq`, appendable without rewriting what is already there. A line
//! is written whole in one `write_all` on a file opened `O_APPEND`, so a partial
//! record can only ever be the last one, and [`RunLog::read`] stops there instead
//! of refusing every record written before a crash.
//!
//! The prompt is stored as well as hashed. A hash is what identifies a request,
//! but a replay is "re-send this unchanged", and a replay that had to ask somebody
//! to supply the prompt again would not be a replay. The hash is what a caller
//! compares when it wants identity without the text.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use acpilot_proto::model::{ContextBudget, CorpusSelector, RunRequest, RunStatus, RunSummary};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The file a profile's runs are appended to, inside whatever directory a caller
/// gave the log.
pub const RUNS_FILE: &str = "runs.jsonl";

/// The schema this build writes and reads.
///
/// A newer line is refused rather than half-read: the writer is a later build, and
/// the fields this one does not know are the fields a replay would silently drop.
pub const RUNS_SCHEMA: u32 = 1;

/// The default page size, and the one a client that names no limit accepts.
pub const DEFAULT_PAGE: u32 = 20;

/// What went wrong reading or writing a run log.
#[derive(Debug, Error)]
pub enum RunsError {
    /// The log could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The log could not be written.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The record could not be encoded, which nothing here can cause.
    #[error("a run record could not be encoded: {source}")]
    Encode {
        /// Why.
        source: serde_json::Error,
    },
    /// A line that is not the last one is not a record.
    #[error("{}: line {line} is not a run record: {message}", path.display())]
    Record {
        /// The file.
        path: PathBuf,
        /// One-based line number.
        line: usize,
        /// The parser's message.
        message: String,
    },
    /// A line declares a schema this build does not read.
    #[error(
        "{}: line {line} has schema_version {found}, not {expected}",
        path.display()
    )]
    Schema {
        /// The file.
        path: PathBuf,
        /// One-based line number.
        line: usize,
        /// What the line says.
        found: u32,
        /// What this build reads.
        expected: u32,
    },
    /// No such run for that recipe.
    #[error("recipe `{recipe}` has no run `{run}`")]
    NotFound {
        /// The recipe.
        recipe: String,
        /// The run.
        run: String,
    },
    /// The page cursor names a run this log does not have for that recipe, so
    /// continuing from it would skip records without saying so.
    #[error("`{run}` is not a run \u{60}{recipe}\u{60} can continue from")]
    Cursor {
        /// The recipe.
        recipe: String,
        /// The cursor that was given.
        run: String,
    },
}

/// One file a selector resolved to, as it went into the prompt.
///
/// Recorded rather than re-derived: a replay must not depend on the workspace still
/// looking the way it did, and a person asking why a run was expensive needs the
/// file list with sizes, not the glob that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedFile {
    /// Path relative to the workspace root, with `/` separators.
    pub path: String,
    /// Bytes read from disk.
    pub bytes: u64,
    /// Tokens the file was estimated at.
    pub tokens: u32,
}

/// The request half of a record: everything needed to send it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRequest {
    /// The user message, unchanged.
    pub prompt: String,
    /// `sha256:` and the hex digest of `prompt`, so identity is comparable without
    /// the text.
    pub prompt_sha256: String,
    /// The master prompt's digest, when the recipe had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_sha256: Option<String>,
    /// Selectors in the order the run used them.
    pub corpus: Vec<CorpusSelector>,
    /// Files they resolved to, in prompt order.
    pub files: Vec<ResolvedFile>,
    /// The budget the assembly planned against.
    pub budget: ContextBudget,
}

impl StoredRequest {
    /// Capture a request at the moment it is sent.
    #[must_use]
    pub fn capture(
        prompt: &str,
        system_prompt: Option<&str>,
        corpus: &[CorpusSelector],
        files: Vec<ResolvedFile>,
        budget: ContextBudget,
    ) -> Self {
        Self {
            prompt: prompt.to_owned(),
            prompt_sha256: prompt_hash(prompt),
            system_prompt_sha256: system_prompt.map(prompt_hash),
            corpus: corpus.to_vec(),
            files,
            budget,
        }
    }

    /// The request to send, unchanged.
    ///
    /// No idempotency key: a replay is a new run with its own record, and carrying
    /// the old key would make a retry look like a duplicate of the run it replays.
    #[must_use]
    pub fn request(&self) -> RunRequest {
        RunRequest {
            prompt: self.prompt.clone(),
            corpus: self.corpus.clone(),
            idempotency_key: None,
        }
    }
}

/// One line of `runs.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    /// Schema version of this line.
    pub schema_version: u32,
    /// What the API returns for this run.
    pub summary: RunSummary,
    /// What was sent.
    pub request: StoredRequest,
}

impl RunRecord {
    /// A record at the current schema.
    #[must_use]
    pub fn new(summary: RunSummary, request: StoredRequest) -> Self {
        Self {
            schema_version: RUNS_SCHEMA,
            summary,
            request,
        }
    }
}

/// What page to ask for.
///
/// `before` is a run id, not an offset: a run log only grows, and an offset into a
/// list that grew between two requests would repeat or skip a line without anyone
/// noticing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageQuery {
    /// Most runs to return. Zero reads as [`DEFAULT_PAGE`].
    pub limit: u32,
    /// Return runs older than this one, exclusive.
    pub before: Option<String>,
}

impl Default for PageQuery {
    fn default() -> Self {
        Self {
            limit: DEFAULT_PAGE,
            before: None,
        }
    }
}

/// One page of runs, newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// The runs.
    pub runs: Vec<RunSummary>,
    /// Pass back as `before` for the next page. Absent at the end.
    pub next: Option<String>,
}

/// A profile's run log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLog {
    path: PathBuf,
}

impl RunLog {
    /// The log in `directory`. Nothing is created until the first [`Self::append`].
    #[must_use]
    pub fn at(directory: &Path) -> Self {
        Self {
            path: directory.join(RUNS_FILE),
        }
    }

    /// The file this log appends to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record.
    ///
    /// # Errors
    ///
    /// [`RunsError::Encode`] when the record is not serializable (nothing here is)
    /// and [`RunsError::Write`] when the file or its directory refuses.
    pub fn append(&self, record: &RunRecord) -> Result<(), RunsError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| RunsError::Write {
                path: self.path.clone(),
                source,
            })?;
        }
        let mut line = serde_json::to_vec(record).map_err(|source| RunsError::Encode { source })?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| RunsError::Write {
                path: self.path.clone(),
                source,
            })?;
        file.write_all(&line).map_err(|source| RunsError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// One run, when the log has it.
    ///
    /// # Errors
    ///
    /// As [`Self::read`].
    pub fn get(&self, recipe: &str, run: &str) -> Result<Option<RunRecord>, RunsError> {
        Ok(self
            .read()?
            .into_iter()
            .find(|record| record.summary.recipe_id == recipe && record.summary.id == run))
    }

    /// One page of a recipe's runs, newest first.
    ///
    /// # Errors
    ///
    /// As [`Self::read`], and [`RunsError::Cursor`] when `before` names a run this
    /// recipe does not have.
    pub fn page(&self, recipe: &str, query: &PageQuery) -> Result<Page, RunsError> {
        let mut matching: Vec<RunRecord> = self
            .read()?
            .into_iter()
            .filter(|record| record.summary.recipe_id == recipe)
            .collect();
        matching.reverse();

        let start = match &query.before {
            Some(cursor) => {
                let Some(index) = matching
                    .iter()
                    .position(|record| &record.summary.id == cursor)
                else {
                    return Err(RunsError::Cursor {
                        recipe: recipe.to_owned(),
                        run: cursor.clone(),
                    });
                };
                index + 1
            }
            None => 0,
        };

        let limit = if query.limit == 0 {
            DEFAULT_PAGE
        } else {
            query.limit
        } as usize;
        let remaining = matching.get(start..).unwrap_or_default();
        let runs: Vec<RunSummary> = remaining
            .iter()
            .take(limit)
            .map(|record| record.summary.clone())
            .collect();
        let next = if remaining.len() > runs.len() {
            runs.last().map(|run| run.id.clone())
        } else {
            None
        };
        Ok(Page { runs, next })
    }

    /// The request a recorded run sent, ready to send again.
    ///
    /// # Errors
    ///
    /// As [`Self::read`], and [`RunsError::NotFound`] when the recipe has no such
    /// run.
    pub fn replay(&self, recipe: &str, run: &str) -> Result<RunRequest, RunsError> {
        let record = self.get(recipe, run)?.ok_or_else(|| RunsError::NotFound {
            recipe: recipe.to_owned(),
            run: run.to_owned(),
        })?;
        Ok(record.request.request())
    }

    /// Every record, oldest first.
    ///
    /// A missing file is an empty log: the first append creates it, and a profile
    /// that has never run anything is not an error.
    ///
    /// # Errors
    ///
    /// [`RunsError::Read`] when the file cannot be read, [`RunsError::Record`] when
    /// a line that is not the last one is not a record, and [`RunsError::Schema`]
    /// when a line is newer than this build.
    fn read(&self) -> Result<Vec<RunRecord>, RunsError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(RunsError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };

        let partial_tail = !text.ends_with('\n');
        let lines: Vec<&str> = text.lines().collect();
        let mut records = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record = match serde_json::from_str::<RunRecord>(line) {
                Ok(record) => record,
                Err(_) if partial_tail && index + 1 == lines.len() => break,
                Err(source) => {
                    return Err(RunsError::Record {
                        path: self.path.clone(),
                        line: index + 1,
                        message: source.to_string(),
                    });
                }
            };
            if record.schema_version != RUNS_SCHEMA {
                return Err(RunsError::Schema {
                    path: self.path.clone(),
                    line: index + 1,
                    found: record.schema_version,
                    expected: RUNS_SCHEMA,
                });
            }
            records.push(record);
        }
        Ok(records)
    }
}

/// `sha256:` and the lower-case hex digest of `text`.
#[must_use]
pub fn prompt_hash(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(text.as_bytes());
    let mut hash = String::with_capacity(7 + 64);
    hash.push_str("sha256:");
    for byte in digest {
        hash.push(HEX[usize::from(byte >> 4)] as char);
        hash.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    hash
}

/// A run's status as the log stores it, unchanged; kept here so the log does not
/// need a second spelling of "still running".
#[allow(dead_code)]
fn status_is_terminal(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_proto::model::{ContextStrategy, SamplingParams, TokenUsage};

    fn budget() -> ContextBudget {
        ContextBudget {
            window: 32768,
            reserve_output: 1500,
            strategy: ContextStrategy::TruncateOldest,
            max_corpus_bytes: 65536,
        }
    }

    fn selector() -> CorpusSelector {
        CorpusSelector {
            glob: "docs/**/*.md".to_owned(),
            max_files: 8,
            max_bytes: 4096,
        }
    }

    fn summary(id: &str, recipe: &str) -> RunSummary {
        RunSummary {
            id: id.to_owned(),
            recipe_id: recipe.to_owned(),
            status: RunStatus::Succeeded,
            started_at: "2026-11-12T12:00:00Z".to_owned(),
            first_token_ms: Some(120),
            total_ms: Some(2400),
            tokens: TokenUsage {
                prompt: 900,
                completion: 80,
                estimated: false,
            },
            tokens_per_second: Some(31.5),
            stop_reason: Some("stop".to_owned()),
            warnings: Vec::new(),
            params: SamplingParams::default(),
            corpus_files: 2,
            message: None,
        }
    }

    fn record(id: &str, recipe: &str) -> RunRecord {
        RunRecord::new(
            summary(id, recipe),
            StoredRequest::capture(
                "why is the sky blue?",
                Some("You are terse."),
                &[selector()],
                vec![ResolvedFile {
                    path: "docs/a.md".to_owned(),
                    bytes: 40,
                    tokens: 10,
                }],
                budget(),
            ),
        )
    }

    fn log() -> (tempfile::TempDir, RunLog) {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = RunLog::at(directory.path());
        (directory, log)
    }

    #[test]
    fn a_run_is_appended_once_and_read_back_unchanged() {
        let (_directory, log) = log();
        let written = record("r1", "corpus-scraper");
        log.append(&written).expect("append");

        let found = log
            .get("corpus-scraper", "r1")
            .expect("reads")
            .expect("found");
        assert_eq!(found, written);
        assert_eq!(found.request.prompt, "why is the sky blue?");
        assert_eq!(found.request.files.len(), 1);
    }

    #[test]
    fn a_log_that_was_never_written_is_empty() {
        let (_directory, log) = log();
        assert_eq!(log.get("any", "any").expect("reads"), None);
        let page = log.page("any", &PageQuery::default()).expect("reads");
        assert!(page.runs.is_empty());
        assert_eq!(page.next, None);
    }

    #[test]
    fn a_page_is_newest_first_and_the_cursor_walks_back() {
        let (_directory, log) = log();
        for id in ["r1", "r2", "r3"] {
            log.append(&record(id, "recipe")).expect("append");
        }

        let first = log
            .page(
                "recipe",
                &PageQuery {
                    limit: 2,
                    before: None,
                },
            )
            .expect("reads");
        let ids: Vec<&str> = first.runs.iter().map(|run| run.id.as_str()).collect();
        assert_eq!(ids, ["r3", "r2"]);
        assert_eq!(first.next.as_deref(), Some("r2"));

        let second = log
            .page(
                "recipe",
                &PageQuery {
                    limit: 2,
                    before: first.next,
                },
            )
            .expect("reads");
        let ids: Vec<&str> = second.runs.iter().map(|run| run.id.as_str()).collect();
        assert_eq!(ids, ["r1"]);
        assert_eq!(second.next, None);
    }

    #[test]
    fn a_page_holds_one_recipe_at_a_time() {
        let (_directory, log) = log();
        log.append(&record("r1", "one")).expect("append");
        log.append(&record("r2", "two")).expect("append");

        let page = log.page("two", &PageQuery::default()).expect("reads");
        let ids: Vec<&str> = page.runs.iter().map(|run| run.id.as_str()).collect();
        assert_eq!(ids, ["r2"]);
    }

    #[test]
    fn a_cursor_from_another_recipe_is_refused_rather_than_skipping() {
        let (_directory, log) = log();
        log.append(&record("r1", "one")).expect("append");

        let error = log
            .page(
                "one",
                &PageQuery {
                    limit: 10,
                    before: Some("nope".to_owned()),
                },
            )
            .expect_err("there is no such run");
        assert!(matches!(error, RunsError::Cursor { .. }), "{error}");
    }

    #[test]
    fn a_replay_is_the_request_that_was_sent() {
        let (_directory, log) = log();
        let original = record("r1", "recipe");
        log.append(&original).expect("append");

        let request = log.replay("recipe", "r1").expect("replays");
        assert_eq!(request.prompt, original.request.prompt);
        assert_eq!(request.corpus, original.request.corpus);
        assert_eq!(request.idempotency_key, None);
    }

    #[test]
    fn replaying_a_run_that_is_not_there_names_it() {
        let (_directory, log) = log();
        let error = log.replay("recipe", "ghost").expect_err("no such run");
        assert!(matches!(error, RunsError::NotFound { .. }), "{error}");
    }

    #[test]
    fn a_partial_last_line_ends_the_read_instead_of_failing_it() {
        let (directory, log) = log();
        log.append(&record("r1", "recipe")).expect("append");
        let path = directory.path().join(RUNS_FILE);
        let mut text = fs::read_to_string(&path).expect("read");
        text.push_str("{\"schema_version\":1,\"summary\":{\"id\":\"r2\"");
        fs::write(&path, text).expect("write");

        let page = log.page("recipe", &PageQuery::default()).expect("reads");
        assert_eq!(page.runs.len(), 1);
        assert_eq!(page.runs[0].id, "r1");
    }

    #[test]
    fn a_broken_line_that_is_not_the_last_one_names_its_number() {
        let (directory, log) = log();
        log.append(&record("r1", "recipe")).expect("append");
        log.append(&record("r2", "recipe")).expect("append");
        let path = directory.path().join(RUNS_FILE);
        let text = fs::read_to_string(&path).expect("read");
        let mut lines: Vec<String> = text.lines().map(ToOwned::to_owned).collect();
        lines[0] = "not json".to_owned();
        fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write");

        let error = log
            .page("recipe", &PageQuery::default())
            .expect_err("broken");
        assert!(
            matches!(error, RunsError::Record { line: 1, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_half_read() {
        let (directory, log) = log();
        log.append(&record("r1", "recipe")).expect("append");
        let path = directory.path().join(RUNS_FILE);
        let mut line = fs::read_to_string(&path).expect("read");
        line = line.replace("\"schema_version\":1", "\"schema_version\":2");
        fs::write(&path, line).expect("write");

        let error = log.get("recipe", "r1").expect_err("newer schema");
        assert!(
            matches!(error, RunsError::Schema { found: 2, .. }),
            "{error}"
        );
    }

    #[test]
    fn the_prompt_hash_is_stable_and_moves_with_one_character() {
        let hash = prompt_hash("why is the sky blue?");
        assert!(hash.starts_with("sha256:"), "{hash}");
        assert_eq!(hash.len(), 7 + 64);
        assert_eq!(hash, prompt_hash("why is the sky blue?"));
        assert_ne!(hash, prompt_hash("why is the sky green?"));
    }

    #[test]
    fn a_terminal_status_is_the_three_that_are_over() {
        assert!(status_is_terminal(RunStatus::Succeeded));
        assert!(status_is_terminal(RunStatus::Failed));
        assert!(status_is_terminal(RunStatus::Cancelled));
        assert!(!status_is_terminal(RunStatus::Running));
    }
}
