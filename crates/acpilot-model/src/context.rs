//! Prompt assembly: what fills a model's window, and what has to leave.
//!
//! Two numbers decide a run. The window is what the server says it has, and
//! [`ContextBudget`] says how much of it the answer may keep. Everything here
//! divides the rest: the system prompt, the corpus, the conversation so far, and
//! the tool schemas. The split is reported rather than applied quietly, because a
//! prompt that was trimmed without saying so is a run whose answer cannot be
//! explained.
//!
//! Token counts are estimates until the server answers. This build assumes four
//! bytes per token, which is close for English prose and wrong for code, CJK text
//! and long identifiers. [`ESTIMATE_BAND`] is the honesty about that: a caller
//! that plans to the window's edge should expect to be off by a quarter in either
//! direction, and [`reconcile`] is where the guess gives way to the reported
//! `usage`.
//!
//! Nothing here sends a request, so `Summarize` cannot do its work: it returns the
//! messages that need replacing, and the caller asks the model and assembles again.

use std::fmt;
use std::path::{Path, PathBuf};

use acpilot_proto::model::{ContextBudget, ContextStrategy, CorpusSelector};
use ignore::WalkBuilder;
use ignore::gitignore::Gitignore;

use crate::{ChatMessage, Role, ToolSpec, Usage};

/// Bytes this build counts as one token.
pub const BYTES_PER_TOKEN: usize = 4;

/// The planning allowance for [`estimate_tokens`], as a fraction either way.
///
/// This is a bound to plan against, not a measurement: four bytes per token is
/// close for prose and wrong for code, CJK text and long paths. A budget that
/// fills the window exactly is therefore not a budget. `docs/notes/` gets the real
/// error once P4.5-6 records what servers report beside what was estimated.
pub const ESTIMATE_BAND: f32 = 0.25;

/// Tokens a chunk of text is estimated to occupy.
#[must_use]
pub fn estimate_tokens(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(BYTES_PER_TOKEN)).unwrap_or(u32::MAX)
}

/// The split of one window, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sections {
    /// The recipe's master prompt.
    pub system: u32,
    /// Workspace text pulled in by the selectors.
    pub corpus: u32,
    /// The conversation so far.
    pub history: u32,
    /// The JSON schemas the model is offered.
    pub tools: u32,
    /// Held back for the answer.
    pub reserve: u32,
}

impl Sections {
    /// Everything sent, without the reserve.
    #[must_use]
    pub fn prompt(self) -> u32 {
        self.system
            .saturating_add(self.corpus)
            .saturating_add(self.history)
            .saturating_add(self.tools)
    }

    /// Everything sent, plus the reserve.
    #[must_use]
    pub fn total(self) -> u32 {
        self.prompt().saturating_add(self.reserve)
    }
}

/// A window and the split of it a run planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// The window planned against.
    pub window: u32,
    /// The split.
    pub sections: Sections,
}

impl Plan {
    /// Tokens that remain for the conversation once everything else is placed.
    ///
    /// Saturating rather than signed: a plan that has already overspent reports
    /// zero room, and [`Plan::fits`] and the overflow error carry the shortfall.
    #[must_use]
    pub fn room_for_history(self) -> u32 {
        self.window
            .saturating_sub(self.sections.reserve)
            .saturating_sub(self.sections.system)
            .saturating_sub(self.sections.corpus)
            .saturating_sub(self.sections.tools)
    }

    /// Whether everything, including the reserve, fits the window.
    #[must_use]
    pub fn fits(self) -> bool {
        self.sections.total() <= self.window
    }

    /// How far over the window the plan is, or zero.
    #[must_use]
    pub fn over_by(self) -> u32 {
        self.sections.total().saturating_sub(self.window)
    }
}

/// Something the assembly did that the caller should say out loud.
///
/// Every variant is a trim. The point of the type is that a run record and an
/// event stream can name what was cut without re-deriving it, and that no trim
/// happens without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A selector matched a file larger than its `max_bytes`.
    SelectorTruncated {
        /// Path relative to the workspace root.
        path: String,
        /// Bytes kept.
        kept: u32,
        /// Bytes the file has.
        bytes: u64,
    },
    /// A selector stopped at its `max_files`.
    SelectorCapped {
        /// The glob that was capped.
        glob: String,
        /// Files it would have added.
        skipped: u32,
    },
    /// A selector's glob is not a glob.
    BadGlob {
        /// What the recipe said.
        glob: String,
        /// Why it was refused.
        why: String,
    },
    /// A matched file is not UTF-8, so it is not text a model can read.
    NotText {
        /// Path relative to the workspace root.
        path: String,
    },
    /// The corpus ceiling left some selected files out.
    CorpusCapped {
        /// The ceiling, in bytes.
        limit: u32,
        /// Files that did not fit.
        dropped: usize,
    },
    /// The strategy dropped turns from the front of the conversation.
    TurnsDropped {
        /// How many messages went.
        messages: u32,
    },
}

impl fmt::Display for Notice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelectorTruncated { path, kept, bytes } => {
                write!(formatter, "{path} kept {kept} of {bytes} bytes")
            }
            Self::SelectorCapped { glob, skipped } => {
                write!(formatter, "`{glob}` left out {skipped} files at max_files")
            }
            Self::BadGlob { glob, why } => write!(formatter, "`{glob}` is not a glob: {why}"),
            Self::NotText { path } => write!(formatter, "{path} is not UTF-8, so it was skipped"),
            Self::CorpusCapped { limit, dropped } => write!(
                formatter,
                "corpus stopped at {limit} bytes and left out {dropped} files"
            ),
            Self::TurnsDropped { messages } => {
                write!(formatter, "dropped the {messages} oldest messages")
            }
        }
    }
}

/// One file the corpus selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusFile {
    /// Path relative to the workspace root, with `/` separators.
    pub path: String,
    /// Size on disk.
    pub bytes: u64,
    /// The text that went into the prompt.
    pub text: String,
}

impl CorpusFile {
    /// Whether the selector's `max_bytes` cut this file.
    #[must_use]
    pub fn truncated(&self) -> bool {
        u64::try_from(self.text.len()).unwrap_or(u64::MAX) < self.bytes
    }
}

/// Selected workspace text, in the order it will appear in the prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Corpus {
    /// Files, in a stable order: by path, each one once.
    pub files: Vec<CorpusFile>,
    /// What the selection did, in the order it happened.
    pub notices: Vec<Notice>,
}

impl Corpus {
    /// The text the model sees.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::new();
        for file in &self.files {
            text.push_str("--- ");
            text.push_str(&file.path);
            text.push('\n');
            text.push_str(&file.text);
            if !file.text.ends_with('\n') {
                text.push('\n');
            }
        }
        text
    }

    /// Bytes of [`Corpus::render`].
    #[must_use]
    pub fn rendered_bytes(&self) -> usize {
        self.render().len()
    }

    /// Trim the rendered text to a byte ceiling, keeping whole files while they fit.
    ///
    /// The last file that straddles the ceiling is cut on a character boundary
    /// rather than dropped, because half a document usually answers more than none
    /// of it. Every file left out is counted in one notice.
    fn cap(&mut self, limit: u32) {
        let limit = limit as usize;
        if self.rendered_bytes() <= limit {
            return;
        }

        let total = self.files.len();
        let mut budget = limit;
        let mut kept = Vec::new();
        for file in std::mem::take(&mut self.files) {
            // The header and the newline are part of what the model reads, so the
            // ceiling counts them too. Counting only the file text would overshoot
            // the number the recipe asked for.
            let header = 4 + file.path.len() + 1;
            let cost = header + file.text.len() + usize::from(!file.text.ends_with('\n'));
            if cost <= budget {
                budget -= cost;
                kept.push(file);
                continue;
            }
            let room = budget.saturating_sub(header + 1);
            if room > 0 {
                let (text, _) = cut(&file.text, room);
                kept.push(CorpusFile { text, ..file });
            }
            break;
        }

        let dropped = total.saturating_sub(kept.len());
        self.files = kept;
        self.notices.push(Notice::CorpusCapped {
            limit: limit as u32,
            dropped,
        });
    }
}

/// Select the workspace text a run's selectors ask for.
///
/// One walk per selector, with the workspace's `.gitignore` applied: a corpus that
/// could read what the file tree hides would be two workspaces in one window. The
/// same rewrites the file tree make — `.ignore`, the global excludes, `.git` — are
/// not read, for the same reason they are not read there: they are per-machine, and
/// the tree a person sees is the tree everybody should see.
///
/// The result is sorted by path and de-duplicated, so two selectors that overlap
/// cost one copy and the same workspace always produces the same prompt.
#[must_use]
pub fn select_corpus(root: &Path, selectors: &[CorpusSelector]) -> Corpus {
    let mut corpus = Corpus::default();
    let mut seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();

    for selector in selectors {
        let matcher = match compile(root, &selector.glob) {
            Ok(matcher) => matcher,
            Err(why) => {
                corpus.notices.push(Notice::BadGlob {
                    glob: selector.glob.clone(),
                    why,
                });
                continue;
            }
        };

        let mut builder = WalkBuilder::new(root);
        builder
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false)
            .hidden(true)
            .follow_links(false)
            .require_git(false)
            .filter_entry(|entry| {
                !(entry.file_type().is_some_and(|kind| kind.is_dir())
                    && entry.file_name() == std::ffi::OsStr::new(".git"))
            });

        let mut paths: Vec<PathBuf> = builder
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            // A `-g`-style override would report a whitelisted file even when the
            // workspace's `.gitignore` hides it, which is the one thing this walk
            // must not do. So the walk keeps its own ignore rules and the glob is
            // asked afterwards, in the same matcher, so both agree on the syntax.
            .filter(|entry| {
                matcher
                    .matched_path_or_any_parents(entry.path(), false)
                    .is_ignore()
            })
            .map(ignore::DirEntry::into_path)
            .collect();
        paths.sort();

        let matched = u32::try_from(paths.len()).unwrap_or(u32::MAX);
        let mut taken = 0u32;
        let mut duplicates = 0u32;
        let mut unusable = 0u32;
        for path in paths {
            if taken >= selector.max_files {
                break;
            }
            if !seen.insert(path.clone()) {
                duplicates += 1;
                continue;
            }

            let Ok(bytes) = std::fs::read(&path) else {
                unusable += 1;
                continue;
            };
            let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            let Ok(text) = String::from_utf8(bytes) else {
                corpus.notices.push(Notice::NotText {
                    path: relative(root, &path),
                });
                unusable += 1;
                continue;
            };
            let (text, cut_short) = cut(&text, selector.max_bytes as usize);
            let name = relative(root, &path);
            if cut_short {
                corpus.notices.push(Notice::SelectorTruncated {
                    path: name.clone(),
                    kept: u32::try_from(text.len()).unwrap_or(u32::MAX),
                    bytes: size,
                });
            }
            corpus.files.push(CorpusFile {
                path: name,
                bytes: size,
                text,
            });
            taken += 1;
        }
        // A duplicate, an unreadable file and a file that is not text were all
        // considered. Counting them as "skipped at max_files" would report a
        // ceiling that did not bite.
        let skipped = matched
            .saturating_sub(taken)
            .saturating_sub(duplicates)
            .saturating_sub(unusable);
        if skipped > 0 {
            corpus.notices.push(Notice::SelectorCapped {
                glob: selector.glob.clone(),
                skipped,
            });
        }
    }

    corpus
}

/// A glob compiled with the matcher the walk itself uses, so `docs/**/*.md`
/// means to both what it means to `git`.
fn compile(root: &Path, glob: &str) -> Result<Gitignore, String> {
    if glob.trim().is_empty() {
        return Err("it is empty, so it selects nothing or everything".to_owned());
    }
    // The override builder is the syntax check the rest of this workspace uses.
    ignore::overrides::OverrideBuilder::new(root)
        .add(glob)
        .map_err(|error| error.to_string())?;
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    builder
        .add_line(None, glob)
        .map_err(|error| error.to_string())?;
    builder.build().map_err(|error| error.to_string())
}

/// `path` relative to `root`, with `/` separators so a prompt reads the same
/// everywhere.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Cut `text` to at most `limit` bytes without splitting a character.
fn cut(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// A prompt the assembly produced, with the split that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    /// Window and split.
    pub plan: Plan,
    /// The recipe's master prompt.
    pub system_prompt: String,
    /// Selected workspace text.
    pub corpus: Corpus,
    /// The conversation this run will send.
    pub history: Vec<ChatMessage>,
    /// Tools offered, unchanged.
    pub tools: Vec<ToolSpec>,
    /// Every trim, in the order it happened.
    pub notices: Vec<Notice>,
}

/// What the assembly decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Assembly {
    /// Send it.
    Ready(Prompt),
    /// The strategy is `summarize` and the history does not fit. The caller asks the
    /// model for a summary of `summarize`, puts that answer where those messages
    /// were, and assembles again against the same budget.
    NeedsSummary {
        /// The prompt with the tail that already fits.
        prompt: Prompt,
        /// The oldest messages that have to become a summary.
        summarize: Vec<ChatMessage>,
    },
}

/// Why a prompt could not be assembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextError {
    /// The parts that are not the conversation already exceed the window, so no
    /// strategy can help: the corpus or the system prompt has to shrink.
    Overflow {
        /// Tokens over the window.
        over_by: u32,
        /// The split that was attempted.
        plan: Plan,
    },
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow { over_by, plan } => write!(
                formatter,
                "prompt is {over_by} tokens over the {}-token window. system {}, corpus {}, history {}, tools {}, reserved output {}",
                plan.window,
                plan.sections.system,
                plan.sections.corpus,
                plan.sections.history,
                plan.sections.tools,
                plan.sections.reserve
            ),
        }
    }
}

impl std::error::Error for ContextError {}

/// Assemble a prompt for one run.
///
/// `history` is oldest first. The function never mutates the caller's slice and
/// never sends anything.
///
/// # Errors
///
/// [`ContextError::Overflow`] when the window cannot hold the system prompt, the
/// corpus, the tool schemas and the reserve. That is a recipe to edit, not a run to
/// retry, so no strategy is applied to it.
pub fn assemble(
    root: &Path,
    system_prompt: &str,
    selectors: &[CorpusSelector],
    budget: &ContextBudget,
    history: &[ChatMessage],
    tools: &[ToolSpec],
) -> Result<Assembly, ContextError> {
    let mut corpus = select_corpus(root, selectors);
    corpus.cap(budget.max_corpus_bytes);

    let mut sections = Sections {
        system: estimate_tokens(system_prompt.len()),
        corpus: estimate_tokens(corpus.rendered_bytes()),
        history: estimate_tokens(history_bytes(history)),
        tools: estimate_tokens(tools_bytes(tools)),
        reserve: budget.reserve_output,
    };
    let plan = Plan {
        window: budget.window,
        sections,
    };

    let placed = sections
        .system
        .saturating_add(sections.corpus)
        .saturating_add(sections.tools)
        .saturating_add(sections.reserve);
    if placed > budget.window {
        return Err(ContextError::Overflow {
            over_by: placed - budget.window,
            plan,
        });
    }
    let room = plan.room_for_history();
    if sections.history <= room {
        return Ok(Assembly::Ready(Prompt {
            plan,
            system_prompt: system_prompt.to_owned(),
            corpus,
            history: history.to_vec(),
            tools: tools.to_vec(),
            notices: Vec::new(),
        }));
    }

    match budget.strategy {
        ContextStrategy::Refuse => Err(ContextError::Overflow {
            over_by: sections.history - room,
            plan,
        }),
        ContextStrategy::TruncateOldest => {
            let (kept, messages) = drop_oldest(history, room);
            if estimate_tokens(history_bytes(&kept)) > room {
                return Err(ContextError::Overflow {
                    over_by: estimate_tokens(history_bytes(&kept)) - room,
                    plan,
                });
            }
            sections.history = estimate_tokens(history_bytes(&kept));
            let mut notices = Vec::new();
            if messages > 0 {
                notices.push(Notice::TurnsDropped { messages });
            }
            Ok(Assembly::Ready(Prompt {
                plan: Plan {
                    window: budget.window,
                    sections,
                },
                system_prompt: system_prompt.to_owned(),
                corpus,
                history: kept,
                tools: tools.to_vec(),
                notices,
            }))
        }
        ContextStrategy::Summarize => {
            let start = split_for_summary(history, room);
            let tail = &history[start..];
            if estimate_tokens(history_bytes(tail)) > room {
                return Err(ContextError::Overflow {
                    over_by: estimate_tokens(history_bytes(tail)) - room,
                    plan,
                });
            }
            sections.history = estimate_tokens(history_bytes(tail));
            let summarize = history[..start].to_vec();
            Ok(Assembly::NeedsSummary {
                prompt: Prompt {
                    plan: Plan {
                        window: budget.window,
                        sections,
                    },
                    system_prompt: system_prompt.to_owned(),
                    corpus,
                    history: tail.to_vec(),
                    tools: tools.to_vec(),
                    notices: Vec::new(),
                },
                summarize,
            })
        }
    }
}

/// Drop messages from the front until the rest fits `room`, and never leave a tool
/// result without the call it answers.
///
/// Returns the kept tail and how many messages went.
fn drop_oldest(history: &[ChatMessage], room: u32) -> (Vec<ChatMessage>, u32) {
    let mut start = 0usize;
    while start < history.len() && estimate_tokens(history_bytes(&history[start..])) > room {
        start += 1;
    }
    if history
        .get(start)
        .is_some_and(|message| message.role == Role::Tool)
    {
        start += 1;
    }
    (history[start..].to_vec(), start as u32)
}

/// Where the tail that fits begins, for `Summarize`.
///
/// A leading tool result is moved into the summary rather than kept, for the same
/// reason [`drop_oldest`] drops it.
fn split_for_summary(history: &[ChatMessage], room: u32) -> usize {
    let mut start = 0usize;
    while start < history.len() && estimate_tokens(history_bytes(&history[start..])) > room {
        start += 1;
    }
    if history
        .get(start)
        .is_some_and(|message| message.role == Role::Tool)
    {
        start += 1;
    }
    start
}

/// Bytes a conversation costs, with a small per-message allowance for the framing
/// both adapters add.
fn history_bytes(history: &[ChatMessage]) -> usize {
    const FRAMING_BYTES: usize = 4;
    history
        .iter()
        .map(|message| message.content.len().saturating_add(FRAMING_BYTES))
        .sum()
}

/// Bytes the tool schemas cost, as JSON, which is the form the model sees.
fn tools_bytes(tools: &[ToolSpec]) -> usize {
    tools
        .iter()
        .map(|tool| {
            let schema = serde_json::to_string(&tool.schema).unwrap_or_default();
            tool.name.len() + tool.description.len() + schema.len() + 8
        })
        .sum()
}

/// The counts to record for a run: the server's when it said so, this build's
/// estimate otherwise.
///
/// `usage.estimated` is the flag a transcript and the lab both read, so it is only
/// cleared when the server reported the number. A completion count the server told
/// us survives a prompt count it did not.
#[must_use]
pub fn reconcile(estimate: Sections, answered: Option<&Usage>) -> Usage {
    match answered {
        Some(usage) if usage.reported => *usage,
        Some(usage) => Usage {
            prompt_tokens: estimate.prompt(),
            completion_tokens: usage.completion_tokens,
            reported: false,
        },
        None => Usage {
            prompt_tokens: estimate.prompt(),
            completion_tokens: 0,
            reported: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(window: u32, strategy: ContextStrategy) -> ContextBudget {
        ContextBudget {
            window,
            reserve_output: 100,
            strategy,
            max_corpus_bytes: 4096,
        }
    }

    fn selector(glob: &str, max_files: u32, max_bytes: u32) -> CorpusSelector {
        CorpusSelector {
            glob: glob.to_owned(),
            max_files,
            max_bytes,
        }
    }

    fn write(root: &Path, name: &str, text: &str) {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(path, text).expect("write");
    }

    fn say(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_owned(),
        }
    }

    fn tool() -> ToolSpec {
        ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file.".to_owned(),
            schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn four_bytes_is_one_token_and_a_part_is_a_token() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2);
    }

    #[test]
    fn the_corpus_is_sorted_by_path_whatever_the_disk_order() {
        let directory = tempfile::tempdir().expect("temp dir");
        write(directory.path(), "src/zeta.rs", "zeta");
        write(directory.path(), "src/alpha.rs", "alpha");
        write(directory.path(), "README.md", "readme");

        let corpus = select_corpus(directory.path(), &[selector("**/*.rs", 10, 4096)]);
        let paths: Vec<&str> = corpus.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["src/alpha.rs", "src/zeta.rs"]);
    }

    #[test]
    fn a_gitignored_file_is_not_in_the_corpus() {
        let directory = tempfile::tempdir().expect("temp dir");
        write(directory.path(), ".gitignore", "secret.rs\n");
        write(directory.path(), "keep.rs", "keep");
        write(directory.path(), "secret.rs", "secret");

        let corpus = select_corpus(directory.path(), &[selector("**/*.rs", 10, 4096)]);
        assert_eq!(corpus.files.len(), 1, "{:?}", corpus.notices);
        assert_eq!(corpus.files[0].path, "keep.rs");
    }

    #[test]
    fn a_file_is_cut_to_its_selector_and_says_so() {
        let directory = tempfile::tempdir().expect("temp dir");
        write(directory.path(), "notes.md", "0123456789");

        let corpus = select_corpus(directory.path(), &[selector("*.md", 10, 4)]);
        assert_eq!(corpus.files[0].text, "0123");
        assert!(corpus.files[0].truncated());
        assert!(matches!(
            corpus.notices.as_slice(),
            [Notice::SelectorTruncated {
                kept: 4,
                bytes: 10,
                ..
            }]
        ));
    }

    #[test]
    fn a_selector_stops_at_its_file_ceiling() {
        let directory = tempfile::tempdir().expect("temp dir");
        write(directory.path(), "a.md", "a");
        write(directory.path(), "b.md", "b");
        write(directory.path(), "c.md", "c");

        let corpus = select_corpus(directory.path(), &[selector("*.md", 2, 4096)]);
        assert_eq!(corpus.files.len(), 2);
        assert_eq!(corpus.files[1].path, "b.md", "the first two in path order");
        assert!(
            corpus
                .notices
                .iter()
                .any(|notice| matches!(notice, Notice::SelectorCapped { .. }))
        );
    }

    #[test]
    fn two_selectors_that_overlap_cost_one_copy() {
        let directory = tempfile::tempdir().expect("temp dir");
        write(directory.path(), "docs/a.md", "a");

        let corpus = select_corpus(
            directory.path(),
            &[
                selector("docs/*.md", 10, 4096),
                selector("**/*.md", 10, 4096),
            ],
        );
        assert_eq!(corpus.files.len(), 1);
    }

    #[test]
    fn a_file_that_is_not_text_is_skipped_with_a_notice() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(directory.path().join("image.md"), [0xff, 0xfe, 0x00]).expect("write");

        let corpus = select_corpus(directory.path(), &[selector("*.md", 10, 4096)]);
        assert!(corpus.files.is_empty());
        assert!(matches!(
            corpus.notices.as_slice(),
            [Notice::NotText { .. }]
        ));
    }

    #[test]
    fn an_empty_glob_is_a_notice_not_a_panic() {
        let directory = tempfile::tempdir().expect("temp dir");
        write(directory.path(), "a.md", "a");

        let corpus = select_corpus(directory.path(), &[selector("", 10, 4096)]);
        assert!(corpus.files.is_empty());
        assert!(matches!(
            corpus.notices.as_slice(),
            [Notice::BadGlob { .. }]
        ));
    }

    #[test]
    fn the_ceiling_cuts_the_corpus_and_counts_what_it_left() {
        let directory = tempfile::tempdir().expect("temp dir");
        write(directory.path(), "a.md", "aaaa");
        write(directory.path(), "b.md", "bbbb");

        let mut corpus = select_corpus(directory.path(), &[selector("*.md", 10, 4096)]); // Each file costs a nine-byte header plus its text and a newline.
        corpus.cap(20);
        assert_eq!(corpus.files.len(), 1, "{:?}", corpus.notices);
        assert!(matches!(
            corpus.notices.as_slice(),
            [Notice::CorpusCapped {
                limit: 20,
                dropped: 1
            }]
        ));
    }

    #[test]
    fn the_plan_names_every_section() {
        let directory = tempfile::tempdir().expect("temp dir");
        write(directory.path(), "notes.md", &"n".repeat(400));

        let assembly = assemble(
            directory.path(),
            &"s".repeat(40),
            &[selector("*.md", 1, 4096)],
            &budget(1000, ContextStrategy::Refuse),
            &[say(Role::User, &"h".repeat(40))],
            &[tool()],
        )
        .expect("fits");

        let Assembly::Ready(prompt) = assembly else {
            panic!("nothing to summarise");
        };
        assert_eq!(prompt.plan.sections.system, 10);
        assert!(prompt.plan.sections.corpus >= 100);
        assert_eq!(prompt.plan.sections.history, 11);
        assert!(prompt.plan.sections.tools > 0);
        assert_eq!(prompt.plan.sections.reserve, 100);
        assert!(prompt.plan.fits());
    }

    #[test]
    fn refuse_says_how_far_over_it_is() {
        let directory = tempfile::tempdir().expect("temp dir");
        let history = vec![say(Role::User, &"h".repeat(4000))];

        let error = assemble(
            directory.path(),
            "system",
            &[],
            &budget(500, ContextStrategy::Refuse),
            &history,
            &[],
        )
        .expect_err("1000 tokens of history do not fit 400");

        let ContextError::Overflow { over_by, plan } = error;
        assert_eq!(over_by, plan.sections.history - plan.room_for_history());
        assert_eq!(plan.sections.history, 1001);
    }

    #[test]
    fn truncate_oldest_drops_the_front_and_keeps_the_newest() {
        let directory = tempfile::tempdir().expect("temp dir");
        let history = vec![
            say(Role::User, &"a".repeat(1600)),
            say(Role::Assistant, &"b".repeat(1600)),
            say(Role::User, "the question"),
        ];

        let assembly = assemble(
            directory.path(),
            "",
            &[],
            &budget(500, ContextStrategy::TruncateOldest),
            &history,
            &[],
        )
        .expect("the last turn fits");

        let Assembly::Ready(prompt) = assembly else {
            panic!("truncation never summarises");
        };
        assert_eq!(prompt.history.len(), 1);
        assert_eq!(prompt.history[0].content, "the question");
        assert_eq!(prompt.notices, [Notice::TurnsDropped { messages: 2 }]);
    }

    #[test]
    fn truncation_never_leaves_a_tool_result_without_its_call() {
        let directory = tempfile::tempdir().expect("temp dir");
        let history = vec![
            say(Role::Assistant, &"c".repeat(1600)),
            say(Role::Tool, &"t".repeat(1600)),
            say(Role::User, "next"),
        ];

        let assembly = assemble(
            directory.path(),
            "",
            &[],
            &budget(500, ContextStrategy::TruncateOldest),
            &history,
            &[],
        )
        .expect("fits");

        let Assembly::Ready(prompt) = assembly else {
            panic!("truncation never summarises");
        };
        assert_eq!(prompt.history.len(), 1);
        assert_eq!(prompt.history[0].role, Role::User);
    }

    #[test]
    fn summarize_hands_back_the_oldest_turns() {
        let directory = tempfile::tempdir().expect("temp dir");
        let history = vec![
            say(Role::User, &"a".repeat(1600)),
            say(Role::Assistant, &"b".repeat(1600)),
            say(Role::User, "the question"),
        ];

        let assembly = assemble(
            directory.path(),
            "",
            &[],
            &budget(500, ContextStrategy::Summarize),
            &history,
            &[],
        )
        .expect("the tail fits");

        let Assembly::NeedsSummary { prompt, summarize } = assembly else {
            panic!("the strategy asked for a summary");
        };
        assert_eq!(summarize.len(), 2);
        assert_eq!(prompt.history.len(), 1);
        assert_eq!(prompt.history[0].content, "the question");
    }

    #[test]
    fn a_window_that_the_system_prompt_alone_breaks_refuses_every_strategy() {
        let directory = tempfile::tempdir().expect("temp dir");

        for strategy in [
            ContextStrategy::Refuse,
            ContextStrategy::TruncateOldest,
            ContextStrategy::Summarize,
        ] {
            let error = assemble(
                directory.path(),
                &"s".repeat(4000),
                &[],
                &budget(500, strategy),
                &[],
                &[],
            )
            .expect_err("no strategy shrinks the system prompt");
            assert!(
                matches!(error, ContextError::Overflow { .. }),
                "{strategy:?}"
            );
        }
    }

    #[test]
    fn a_reported_usage_replaces_the_estimate() {
        let estimate = Sections {
            system: 10,
            corpus: 20,
            history: 30,
            tools: 40,
            reserve: 50,
        };
        let reported = Usage {
            prompt_tokens: 111,
            completion_tokens: 7,
            reported: true,
        };
        assert_eq!(reconcile(estimate, Some(&reported)), reported);
    }

    #[test]
    fn an_unreported_usage_keeps_the_estimate_and_the_flag() {
        let estimate = Sections {
            system: 10,
            corpus: 20,
            history: 30,
            tools: 40,
            reserve: 50,
        };
        let guessed = Usage {
            prompt_tokens: 999,
            completion_tokens: 7,
            reported: false,
        };
        let usage = reconcile(estimate, Some(&guessed));
        assert_eq!(
            usage.prompt_tokens, 100,
            "10 + 20 + 30 + 40, not the reserve"
        );
        assert_eq!(usage.completion_tokens, 7);
        assert!(!usage.reported);
    }
}
