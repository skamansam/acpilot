//! The model lab: the recipe, the corpus, and what a run actually sent.
//!
//! Three panes, because a local model has three questions. The *recipe* is what
//! will be sent: the master prompt, the sampling knobs, the budget, the tool
//! ceiling. The *corpus* is what the workspace contributes, and what it costs.
//! The *output* is what came back, what it cost, and what the last runs answered.
//!
//! The rule the whole module obeys: a figure the provider did not report is not
//! drawn as if it had. Estimated tokens carry a `~` and a sentence saying so, a
//! gauge with nothing behind it is absent rather than zero, and a parameter the
//! server refused is named beside the request instead of quietly disappearing.
//!
//! Editing is local until `Ctrl-S`: the draft is a [`ModelRecipe`] the daemon does
//! not know about, so a recipe can be changed and read back without a write, and a
//! failed save leaves the draft on screen with the daemon's reason.

use acpilot_proto::model::{
    ContextStrategy, CorpusSelector, DiscoveredModel, ModelProblem, ModelRecipe, RunRecordView,
    RunStatus, RunSummary,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::theme::Theme;

/// A pane of the lab that the keyboard can be in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Pane {
    /// The recipe's fields.
    #[default]
    Recipe,
    /// The corpus selectors and the files the last run read.
    Corpus,
    /// The answer, the prompt and the run history.
    Output,
}

impl Pane {
    /// What the status line and the pane title call it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Recipe => "recipe",
            Self::Corpus => "corpus",
            Self::Output => "output",
        }
    }

    /// The next pane, in the order the columns are drawn.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Recipe => Self::Corpus,
            Self::Corpus => Self::Output,
            Self::Output => Self::Recipe,
        }
    }

    /// The previous pane.
    #[must_use]
    pub fn previous(self) -> Self {
        match self {
            Self::Recipe => Self::Output,
            Self::Corpus => Self::Recipe,
            Self::Output => Self::Corpus,
        }
    }
}

/// One editable row of the recipe pane.
///
/// The list is data rather than a `match` at every call site: the pane draws it,
/// `j`/`k` walk it, and the patch a save sends is built from the same set, so a
/// field cannot be drawn and not saved (or saved and not drawn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The display label.
    Label,
    /// The master prompt.
    Prompt,
    /// Sampling temperature.
    Temperature,
    /// Nucleus cutoff.
    TopP,
    /// Output cap.
    MaxTokens,
    /// Top-k cutoff.
    TopK,
    /// Repetition penalty.
    RepeatPenalty,
    /// Fixed seed.
    Seed,
    /// Context window.
    Window,
    /// Tokens held back for the answer.
    ReserveOutput,
    /// Corpus byte ceiling.
    MaxCorpusBytes,
    /// What to do on overflow.
    Strategy,
    /// Tool-call steps per turn.
    MaxSteps,
    /// Offer file reads.
    ReadFiles,
    /// Offer file writes.
    WriteFiles,
    /// Offer command execution.
    RunCommands,
    /// Offer workspace search.
    Search,
}

/// Every field, in the order the pane draws them.
pub const FIELDS: [Field; 17] = [
    Field::Label,
    Field::Prompt,
    Field::Temperature,
    Field::TopP,
    Field::MaxTokens,
    Field::TopK,
    Field::RepeatPenalty,
    Field::Seed,
    Field::Window,
    Field::ReserveOutput,
    Field::MaxCorpusBytes,
    Field::Strategy,
    Field::MaxSteps,
    Field::ReadFiles,
    Field::WriteFiles,
    Field::RunCommands,
    Field::Search,
];

impl Field {
    /// The row's name.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Label => "label",
            Self::Prompt => "master prompt",
            Self::Temperature => "temperature",
            Self::TopP => "top-p",
            Self::MaxTokens => "max tokens",
            Self::TopK => "top-k",
            Self::RepeatPenalty => "repeat penalty",
            Self::Seed => "seed",
            Self::Window => "window",
            Self::ReserveOutput => "reserve output",
            Self::MaxCorpusBytes => "corpus ceiling",
            Self::Strategy => "overflow",
            Self::MaxSteps => "max steps",
            Self::ReadFiles => "read files",
            Self::WriteFiles => "write files",
            Self::RunCommands => "run commands",
            Self::Search => "search",
        }
    }

    /// The heading the row sits under.
    #[must_use]
    pub fn group(self) -> &'static str {
        match self {
            Self::Label | Self::Prompt => "identity",
            Self::Temperature
            | Self::TopP
            | Self::MaxTokens
            | Self::TopK
            | Self::RepeatPenalty
            | Self::Seed => "sampling",
            Self::Window | Self::ReserveOutput | Self::MaxCorpusBytes | Self::Strategy => "budget",
            Self::MaxSteps
            | Self::ReadFiles
            | Self::WriteFiles
            | Self::RunCommands
            | Self::Search => "tools",
        }
    }

    /// Whether `Enter` opens a text editor rather than `Space` cycling the value.
    #[must_use]
    pub fn editable(self) -> bool {
        !matches!(
            self,
            Self::Strategy | Self::ReadFiles | Self::WriteFiles | Self::RunCommands | Self::Search
        )
    }

    /// Whether the editor takes more than one line.
    #[must_use]
    pub fn multiline(self) -> bool {
        matches!(self, Self::Prompt)
    }

    /// The value as the pane draws it, and as the editor starts with it.
    ///
    /// An absent knob is the empty string in the editor, because a person who wants
    /// the server's default should be able to clear the field; the pane spells the
    /// absence out.
    #[must_use]
    pub fn value(self, recipe: &ModelRecipe) -> String {
        match self {
            Self::Label => recipe.label.clone(),
            Self::Prompt => recipe.system_prompt.clone(),
            Self::Temperature => format!("{:.2}", recipe.sampling.temperature),
            Self::TopP => format!("{:.2}", recipe.sampling.top_p),
            Self::MaxTokens => recipe
                .sampling
                .max_tokens
                .map_or_else(String::new, |value| value.to_string()),
            Self::TopK => recipe
                .sampling
                .top_k
                .map_or_else(String::new, |value| value.to_string()),
            Self::RepeatPenalty => recipe
                .sampling
                .repeat_penalty
                .map_or_else(String::new, |value| format!("{value:.2}")),
            Self::Seed => recipe
                .sampling
                .seed
                .map_or_else(String::new, |value| value.to_string()),
            Self::Window => recipe.context.window.to_string(),
            Self::ReserveOutput => recipe.context.reserve_output.to_string(),
            Self::MaxCorpusBytes => recipe.context.max_corpus_bytes.to_string(),
            Self::Strategy => match recipe.context.strategy {
                ContextStrategy::TruncateOldest => "truncate oldest",
                ContextStrategy::Summarize => "summarise",
                ContextStrategy::Refuse => "refuse",
            }
            .to_owned(),
            Self::MaxSteps => recipe.tools.max_steps.to_string(),
            Self::ReadFiles => on_off(recipe.tools.read_files),
            Self::WriteFiles => on_off(recipe.tools.write_files),
            Self::RunCommands => on_off(recipe.tools.run_commands),
            Self::Search => on_off(recipe.tools.search),
        }
    }

    /// What the pane draws for a value that is absent rather than zero.
    #[must_use]
    pub fn shown(self, recipe: &ModelRecipe) -> String {
        let value = self.value(recipe);
        if value.is_empty() && self.editable() {
            return "server default".to_owned();
        }
        value
    }

    /// Apply typed text to the recipe.
    pub fn set(self, recipe: &mut ModelRecipe, text: &str) -> Result<(), String> {
        match self {
            Self::Label => {
                let label = text.trim();
                if label.is_empty() {
                    return Err("a recipe needs a label".to_owned());
                }
                recipe.label = label.to_owned();
            }
            Self::Prompt => recipe.system_prompt = text.to_owned(),
            Self::Temperature => {
                let value = number::<f32>(text, "temperature")?;
                if !(0.0..=2.0).contains(&value) {
                    return Err("temperature is between 0 and 2".to_owned());
                }
                recipe.sampling.temperature = value;
            }
            Self::TopP => {
                let value = number::<f32>(text, "top-p")?;
                if !(0.0..=1.0).contains(&value) {
                    return Err("top-p is between 0 and 1".to_owned());
                }
                recipe.sampling.top_p = value;
            }
            Self::MaxTokens => recipe.sampling.max_tokens = optional::<u32>(text, "max tokens")?,
            Self::TopK => recipe.sampling.top_k = optional::<u32>(text, "top-k")?,
            Self::RepeatPenalty => {
                let value = number::<f32>(text, "repeat penalty")?;
                if !(0.0..=2.0).contains(&value) {
                    return Err("repeat penalty is between 0 and 2".to_owned());
                }
                recipe.sampling.repeat_penalty = Some(value);
            }
            Self::Seed => recipe.sampling.seed = optional::<u64>(text, "seed")?,
            Self::Window => {
                let value = number::<u32>(text, "window")?;
                if value == 0 {
                    return Err("a window is at least one token".to_owned());
                }
                recipe.context.window = value;
            }
            Self::ReserveOutput => {
                let value = number::<u32>(text, "reserve output")?;
                if value >= recipe.context.window {
                    return Err("reserved output must leave room in the window".to_owned());
                }
                recipe.context.reserve_output = value;
            }
            Self::MaxCorpusBytes => {
                recipe.context.max_corpus_bytes = number::<u32>(text, "corpus ceiling")?;
            }
            Self::MaxSteps => {
                let value = number::<u32>(text, "max steps")?;
                if !(1..=50).contains(&value) {
                    return Err("max steps is between 1 and 50".to_owned());
                }
                recipe.tools.max_steps = value;
            }
            Self::Strategy
            | Self::ReadFiles
            | Self::WriteFiles
            | Self::RunCommands
            | Self::Search => {
                return Err(format!("`{}` is changed with Space", self.label()));
            }
        }
        Ok(())
    }

    /// Flip a toggle, or advance the overflow strategy, naming the new value.
    #[must_use]
    pub fn toggle(self, recipe: &mut ModelRecipe) -> Option<String> {
        match self {
            Self::ReadFiles => Some(toggle(&mut recipe.tools.read_files)),
            Self::WriteFiles => Some(toggle(&mut recipe.tools.write_files)),
            Self::RunCommands => Some(toggle(&mut recipe.tools.run_commands)),
            Self::Search => Some(toggle(&mut recipe.tools.search)),
            Self::Strategy => {
                recipe.context.strategy = match recipe.context.strategy {
                    ContextStrategy::TruncateOldest => ContextStrategy::Summarize,
                    ContextStrategy::Summarize => ContextStrategy::Refuse,
                    ContextStrategy::Refuse => ContextStrategy::TruncateOldest,
                };
                Some(self.value(recipe))
            }
            _ => None,
        }
    }
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_owned()
}

fn toggle(value: &mut bool) -> String {
    *value = !*value;
    on_off(*value)
}

fn number<T: std::str::FromStr>(text: &str, name: &str) -> Result<T, String> {
    text.trim()
        .parse()
        .map_err(|_| format!("{name} is a number"))
}

fn optional<T: std::str::FromStr>(text: &str, name: &str) -> Result<Option<T>, String> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("server default") {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|_| format!("{name} is a number, or empty for the server's default"))
}

/// What a draft edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// One recipe field.
    Field(Field),
    /// One corpus selector, counted from the list's top.
    Selector(usize),
}

/// A text editor open over one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// What is being edited.
    pub target: Target,
    /// The text, without the recipe's value until it is committed.
    pub text: String,
    /// The caret, counted in characters from the start.
    pub cursor: usize,
}

impl Draft {
    /// Whether `Enter` inserts a line rather than commits.
    #[must_use]
    pub fn multiline(&self) -> bool {
        matches!(self.target, Target::Field(field) if field.multiline())
    }
}

/// The lab, as the app holds it.
#[derive(Debug, Default)]
pub struct Lab {
    /// Every recipe in the profile, in the daemon's order.
    pub recipes: Vec<ModelRecipe>,
    /// Which recipe is on screen.
    pub selected: usize,
    /// What every enabled provider reported, shown beside the model id.
    pub models: Vec<DiscoveredModel>,
    /// Providers that did not answer the model list.
    pub problems: Vec<ModelProblem>,
    /// The selected recipe's runs, newest first.
    pub runs: Vec<RunSummary>,
    /// Which run the gauges and the inspector are about.
    pub run_selected: usize,
    /// The run `c` marked as the comparison base.
    pub compare: Option<usize>,
    /// The selected run's record, once the daemon has answered.
    pub record: Option<RunRecordView>,
    /// The answer text, as it arrives.
    pub output: String,
    /// The prompt the next run sends.
    pub prompt: String,
    /// Which pane has the keyboard.
    pub pane: Pane,
    /// Which field the recipe pane is on.
    pub field: usize,
    /// Which selector the corpus pane is on.
    pub selector: usize,
    /// The open editor, when one is.
    pub draft: Option<Draft>,
    /// Selectors the next run resolves against the workspace.
    pub selectors: Vec<CorpusSelector>,
    /// Whether the inspector is over the panes.
    pub inspector: bool,
    /// Whether the prompt line is taking text.
    pub typing: bool,
    /// Whether a run is streaming.
    pub running: bool,
    /// Whether the first answer from the daemon has arrived.
    pub loading: bool,
    /// Why the last request failed, when one did.
    pub error: Option<String>,
    /// What the last key did, for the status line.
    pub status: Option<String>,
    /// Whether the recipe has been changed since it was read or saved.
    pub dirty: bool,
}

impl Lab {
    /// The recipe on screen.
    #[must_use]
    pub fn recipe(&self) -> Option<&ModelRecipe> {
        self.recipes.get(self.selected)
    }

    /// The recipe on screen, to change.
    pub fn recipe_mut(&mut self) -> Option<&mut ModelRecipe> {
        self.recipes.get_mut(self.selected)
    }

    /// The run the gauges describe.
    #[must_use]
    pub fn current_run(&self) -> Option<&RunSummary> {
        self.runs.get(self.run_selected)
    }

    /// Point the lab at another recipe, dropping what belonged to the old one.
    pub fn select_recipe(&mut self, index: usize) {
        if index == self.selected || index >= self.recipes.len() {
            return;
        }
        self.selected = index;
        self.runs.clear();
        self.run_selected = 0;
        self.compare = None;
        self.record = None;
        self.output.clear();
        self.selectors.clear();
        self.field = 0;
        self.selector = 0;
        self.draft = None;
        self.dirty = false;
        self.error = None;
    }

    /// Move the recipe selection by one, stopping at the ends.
    pub fn step_recipe(&mut self, forward: bool) {
        let next = if forward {
            (self.selected + 1).min(self.recipes.len().saturating_sub(1))
        } else {
            self.selected.saturating_sub(1)
        };
        self.select_recipe(next);
    }

    /// Open an editor over a field.
    pub fn begin_field(&mut self) {
        let Some(recipe) = self.recipe() else {
            return;
        };
        let Some(field) = FIELDS.get(self.field).copied() else {
            return;
        };
        if !field.editable() {
            return;
        }
        let text = field.value(recipe);
        self.draft = Some(Draft {
            target: Target::Field(field),
            cursor: text.chars().count(),
            text,
        });
    }

    /// Open an editor over a selector, one line of `glob, max_files, max_bytes`.
    pub fn begin_selector(&mut self) {
        let Some(selector) = self.selectors.get(self.selector) else {
            return;
        };
        let text = format!(
            "{}, {}, {}",
            selector.glob, selector.max_files, selector.max_bytes
        );
        self.draft = Some(Draft {
            target: Target::Selector(self.selector),
            cursor: text.chars().count(),
            text,
        });
    }

    /// Apply the open editor. A refused value leaves the editor open with the reason.
    pub fn commit(&mut self) -> Result<(), String> {
        let Some((target, text)) = self
            .draft
            .as_ref()
            .map(|draft| (draft.target, draft.text.clone()))
        else {
            return Ok(());
        };
        let outcome = match target {
            Target::Field(field) => self
                .recipe_mut()
                .ok_or_else(|| "no recipe is on screen".to_owned())
                .and_then(|recipe| field.set(recipe, &text)),
            Target::Selector(index) => parse_selector(&text).and_then(|selector| {
                let Some(slot) = self.selectors.get_mut(index) else {
                    return Err("that selector is gone".to_owned());
                };
                *slot = selector;
                Ok(())
            }),
        };
        match outcome {
            Ok(()) => {
                self.draft = None;
                self.dirty = true;
                Ok(())
            }
            Err(reason) => Err(reason),
        }
    }

    /// Drop the open editor without touching the recipe.
    pub fn cancel(&mut self) {
        self.draft = None;
    }

    /// Flip the selected field, when it is a toggle or the overflow strategy.
    pub fn toggle_field(&mut self) -> Option<String> {
        let field = FIELDS.get(self.field).copied()?;
        let recipe = self.recipe_mut()?;
        let value = field.toggle(recipe)?;
        self.dirty = true;
        Some(format!("{} · {value}", field.label()))
    }

    /// Add a selector that reads Markdown, the common case for a lab.
    pub fn add_selector(&mut self) {
        self.selectors.push(CorpusSelector {
            glob: "docs/**/*.md".to_owned(),
            max_files: 20,
            max_bytes: 65_536,
        });
        self.selector = self.selectors.len() - 1;
    }

    /// Remove the selected selector.
    pub fn remove_selector(&mut self) {
        if self.selector < self.selectors.len() {
            self.selectors.remove(self.selector);
            self.selector = self.selector.min(self.selectors.len().saturating_sub(1));
        }
    }

    /// What the next run sends, as the inspector shows it.
    ///
    /// A record answers for the run that happened, because that is the only place
    /// the resolved files exist. Without one the inspector shows the plan: the
    /// selectors, the props and the budget, with the files it will read unknown
    /// rather than guessed.
    #[must_use]
    pub fn inspector_json(&self) -> String {
        let Some(recipe) = self.recipe() else {
            return "{\n  \"error\": \"no recipe is on screen\"\n}".to_owned();
        };
        let (prompt, corpus, files, budget) = match &self.record {
            Some(record) => (
                record.request.prompt.clone(),
                record.request.corpus.clone(),
                record
                    .request
                    .files
                    .iter()
                    .map(|file| {
                        serde_json::json!({
                            "path": file.path,
                            "bytes": file.bytes,
                            "tokens": file.tokens,
                        })
                    })
                    .collect::<Vec<_>>(),
                record.request.budget.clone(),
            ),
            None => (
                self.prompt.clone(),
                self.selectors.clone(),
                Vec::new(),
                recipe.context.clone(),
            ),
        };
        let params = match (&self.record, self.current_run()) {
            (Some(record), _) => serde_json::to_value(&record.summary.params).ok(),
            (None, Some(run)) => serde_json::to_value(&run.params).ok(),
            (None, None) => None,
        }
        .unwrap_or(serde_json::Value::Null);
        let value = serde_json::json!({
            "model": recipe.model_id,
            "provider": recipe.provider_id,
            "system_prompt": if recipe.system_prompt.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::from(format!(
                    "sha256 taken by the daemon; {} characters",
                    recipe.system_prompt.chars().count()
                ))
            },
            "prompt": prompt,
            "corpus": corpus,
            "files": files,
            "budget": {
                "window": budget.window,
                "reserve_output": budget.reserve_output,
                "max_corpus_bytes": budget.max_corpus_bytes,
            },
            "params": params,
        });
        serde_json::to_string_pretty(&value).unwrap_or_else(|error| error.to_string())
    }

    /// The token and latency difference between the marked run and this one.
    #[must_use]
    pub fn comparison(&self) -> Option<Comparison> {
        let base = self.compare?;
        if base == self.run_selected {
            return None;
        }
        let before = self.runs.get(base)?;
        let now = self.runs.get(self.run_selected)?;
        let tokens =
            |run: &RunSummary| i64::from(run.tokens.prompt) + i64::from(run.tokens.completion);
        Some(Comparison {
            index: base,
            tokens: tokens(now) - tokens(before),
            latency_ms: match (now.total_ms, before.total_ms) {
                (Some(now), Some(before)) => {
                    i64::try_from(now).unwrap_or(i64::MAX)
                        - i64::try_from(before).unwrap_or(i64::MAX)
                }
                _ => 0,
            },
            latency_known: now.total_ms.is_some() && before.total_ms.is_some(),
        })
    }

    /// Bytes and tokens the recorded run read against the recipe's ceiling.
    #[must_use]
    pub fn budget_use(&self) -> BudgetUse {
        let Some(record) = &self.record else {
            return BudgetUse::default();
        };
        BudgetUse {
            bytes: record.request.files.iter().map(|file| file.bytes).sum(),
            tokens: record.request.files.iter().map(|file| file.tokens).sum(),
            files: record.request.files.len(),
            ceiling: record.request.budget.max_corpus_bytes,
            known: true,
        }
    }

    /// The model the selected recipe names, when a provider reported it.
    #[must_use]
    pub fn discovered(&self) -> Option<&DiscoveredModel> {
        let recipe = self.recipe()?;
        self.models
            .iter()
            .find(|model| model.id == recipe.model_id && model.provider_id == recipe.provider_id)
    }
}

/// A run's corpus use against its ceiling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetUse {
    /// Bytes the files came to.
    pub bytes: u64,
    /// Tokens they were estimated at.
    pub tokens: u32,
    /// How many files were read.
    pub files: usize,
    /// The recipe's ceiling, in bytes.
    pub ceiling: u32,
    /// Whether a record answered; absent is not zero.
    pub known: bool,
}

impl BudgetUse {
    /// The filled part of the meter, in percent, clamped to what can be drawn.
    #[must_use]
    pub fn percent(self) -> u16 {
        if self.ceiling == 0 {
            return 0;
        }
        let percent = (self.bytes.saturating_mul(100)) / u64::from(self.ceiling);
        u16::try_from(percent.min(100)).unwrap_or(100)
    }
}

/// Two runs, side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Comparison {
    /// The run compared against.
    pub index: usize,
    /// Tokens now minus tokens then.
    pub tokens: i64,
    /// Wall-clock now minus then, in milliseconds.
    pub latency_ms: i64,
    /// Whether both runs reported a duration.
    pub latency_known: bool,
}

fn parse_selector(text: &str) -> Result<CorpusSelector, String> {
    let mut parts = text.split(',').map(str::trim);
    let glob = parts.next().unwrap_or_default();
    if glob.is_empty() {
        return Err("a selector needs a glob".to_owned());
    }
    let max_files = match parts.next().filter(|part| !part.is_empty()) {
        Some(part) => part
            .parse()
            .map_err(|_| "max files is a number".to_owned())?,
        None => 20,
    };
    let max_bytes = match parts.next().filter(|part| !part.is_empty()) {
        Some(part) => part
            .parse()
            .map_err(|_| "max bytes is a number".to_owned())?,
        None => 65_536,
    };
    Ok(CorpusSelector {
        glob: glob.to_owned(),
        max_files,
        max_bytes,
    })
}

/// Wrap text at `width` cells, keeping every word that fits.
fn wrapped(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut out = Vec::new();
    for line in text.lines() {
        let mut current = String::new();
        for word in line.split(' ') {
            let word_len = word.chars().count();
            let current_len = current.chars().count();
            if !current.is_empty() && current_len + 1 + word_len > width {
                out.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            if word_len > width {
                // A path or a token can be longer than the pane; cut it rather than
                // letting it wrap into a second line nobody counted.
                let mut rest = word.chars();
                loop {
                    let chunk: String = rest.by_ref().take(width).collect();
                    if chunk.is_empty() {
                        break;
                    }
                    out.push(chunk);
                }
            } else {
                current.push_str(word);
            }
        }
        out.push(current);
    }
    out
}

fn heading(text: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {text}"),
        Style::default()
            .fg(theme.accent())
            .add_modifier(Modifier::BOLD),
    ))
}

fn bar(percent: u16, width: usize, theme: &Theme) -> Line<'static> {
    let filled = (usize::from(percent) * width) / 100;
    Line::from(vec![
        Span::styled(
            "[".to_owned() + &"#".repeat(filled),
            Style::default().fg(theme.accent_high()),
        ),
        Span::styled(
            "-".repeat(width - filled) + "]",
            Style::default().fg(theme.border()),
        ),
        Span::styled(
            format!(" {percent}%"),
            Style::default().fg(theme.text_muted()),
        ),
    ])
}

/// Paint the lab over the body.
pub fn draw(frame: &mut Frame, lab: &Lab, theme: &Theme, area: Rect) {
    frame.render_widget(Clear, area);
    let title = match lab.recipe() {
        Some(recipe) => format!(
            " Model lab · {} · {}/{} {} ",
            recipe.label,
            lab.selected + 1,
            lab.recipes.len(),
            if lab.dirty { "· edited" } else { "" },
        ),
        None => " Model lab · no recipes ".to_owned(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent()))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 40 || inner.height < 6 {
        return;
    }
    let [body, hint] = Layout::vertical([Constraint::Min(4), Constraint::Length(1)]).areas(inner);
    let [recipe, corpus, output] = Layout::horizontal([
        Constraint::Percentage(38),
        Constraint::Percentage(28),
        Constraint::Min(30),
    ])
    .areas(body);
    draw_recipe(frame, lab, theme, recipe);
    draw_corpus(frame, lab, theme, corpus);
    draw_output(frame, lab, theme, output);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Tab panes · j/k move · Enter edit · Space toggle · ^R run · ^S save · v request · c compare · r reload · Esc close",
            Style::default().fg(theme.text_muted()),
        ))),
        hint,
    );
    if lab.inspector {
        draw_inspector(frame, lab, theme, body);
    }
}

fn panel(title: &str, theme: &Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border()))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(theme.accent_high()),
        ))
}

fn draw_recipe(frame: &mut Frame, lab: &Lab, theme: &Theme, area: Rect) {
    let active = lab.pane == Pane::Recipe;
    let mut block = panel("Recipe", theme);
    if active {
        block = block.border_style(Style::default().fg(theme.accent()));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(recipe) = lab.recipe() else {
        frame.render_widget(
            Paragraph::new("No recipes. Promote a recipe with `acpilot recipes new`."),
            inner,
        );
        return;
    };
    let mut rows: Vec<Line> = Vec::with_capacity(FIELDS.len() + 8);
    let mut field_at = [0usize; FIELDS.len()];
    let mut group = "";
    for (index, field) in FIELDS.iter().enumerate() {
        if field.group() != group {
            group = field.group();
            rows.push(heading(group, theme));
        }
        field_at[index] = rows.len();
        let chosen = active && index == lab.field;
        let marker = if chosen { "▸" } else { " " };
        let style = if chosen {
            Style::default()
                .fg(theme.accent_high())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text())
        };
        let text = if let Some(draft) = &lab.draft
            && draft.target == Target::Field(*field)
        {
            format!("{}{}", draft.text.replace('\n', "⏎"), "▏")
        } else if *field == Field::Prompt && !recipe.system_prompt.is_empty() {
            format!(
                "{} lines · {} chars",
                recipe.system_prompt.lines().count(),
                recipe.system_prompt.chars().count()
            )
        } else {
            field.shown(recipe)
        };
        // A tool is drawn as a checkbox, not as the words on and off: a ceiling is
        // read by scanning which rows are ticked.
        let value = if matches!(
            field,
            Field::ReadFiles | Field::WriteFiles | Field::RunCommands | Field::Search
        ) {
            let on = field.value(recipe) == "on";
            if on { "▣ yes" } else { "▢ no" }.to_owned()
        } else {
            text
        };
        rows.push(Line::from(vec![
            Span::styled(format!(" {marker} {:<15}", field.label()), style),
            Span::styled(value, style),
        ]));
    }
    let height = usize::from(inner.height).max(1);
    let first = field_at[lab.field].saturating_sub(height.saturating_sub(1));
    let visible: Vec<Line> = rows.into_iter().skip(first).take(height).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn draw_corpus(frame: &mut Frame, lab: &Lab, theme: &Theme, area: Rect) {
    let active = lab.pane == Pane::Corpus;
    let mut block = panel("Corpus", theme);
    if active {
        block = block.border_style(Style::default().fg(theme.accent()));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut rows: Vec<Line> = Vec::new();
    if lab.selectors.is_empty() {
        rows.push(Line::from(Span::styled(
            " no selectors · the prompt alone",
            Style::default().fg(theme.text_muted()),
        )));
    }
    for (index, selector) in lab.selectors.iter().enumerate() {
        let chosen = active && index == lab.selector;
        let style = if chosen {
            Style::default()
                .fg(theme.accent_high())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text())
        };
        rows.push(Line::from(Span::styled(
            format!(
                " {} {:<24} {} files · {} KiB",
                if chosen { "▸" } else { " " },
                selector.glob,
                selector.max_files,
                selector.max_bytes / 1024
            ),
            style,
        )));
    }
    rows.push(Line::from(""));
    let use_ = lab.budget_use();
    if use_.known {
        rows.push(Line::from(vec![
            Span::styled(
                format!(" {} files · {} tok · ", use_.files, use_.tokens),
                Style::default().fg(theme.text()),
            ),
            Span::styled(
                format!("{} / {} KiB", use_.bytes / 1024, use_.ceiling / 1024),
                Style::default().fg(theme.text_muted()),
            ),
        ]));
        rows.push(bar(use_.percent(), 18, theme));
        rows.push(Line::from(Span::styled(
            " estimated tokens: bytes ÷ 4 until the server reports usage",
            Style::default().fg(theme.text_muted()),
        )));
    } else {
        rows.push(Line::from(Span::styled(
            " no run yet · the resolved files arrive with a record",
            Style::default().fg(theme.text_muted()),
        )));
    }
    if let Some(record) = &lab.record
        && !record.request.files.is_empty()
    {
        rows.push(heading("files the run read", theme));
        for file in &record.request.files {
            rows.push(Line::from(format!(
                " {:<28} {:>7} B · {} tok",
                file.path, file.bytes, file.tokens
            )));
        }
    }
    if let Some(problem) = lab.problems.first() {
        rows.push(Line::from(Span::styled(
            format!(" {}: {}", problem.provider_id, problem.message),
            Style::default().fg(theme.warning()),
        )));
    }
    let height = usize::from(inner.height).max(1);
    let first = rows.len().saturating_sub(height);
    let visible: Vec<Line> = rows.into_iter().skip(first).take(height).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn draw_output(frame: &mut Frame, lab: &Lab, theme: &Theme, area: Rect) {
    let active = lab.pane == Pane::Output;
    let mut block = panel("Output", theme);
    if active {
        block = block.border_style(Style::default().fg(theme.accent()));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [gauges, answer, history, prompt] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(3),
        Constraint::Length(7),
        Constraint::Length(1),
    ])
    .areas(inner);

    let mut rows: Vec<Line> = Vec::new();
    match lab.current_run() {
        Some(run) => {
            let status = match run.status {
                RunStatus::Running => "running",
                RunStatus::Succeeded => "succeeded",
                RunStatus::Failed => "failed",
                RunStatus::Cancelled => "cancelled",
            };
            let colour = match run.status {
                RunStatus::Succeeded => theme.success(),
                RunStatus::Failed => theme.danger(),
                RunStatus::Running => theme.accent(),
                RunStatus::Cancelled => theme.warning(),
            };
            rows.push(Line::from(vec![
                Span::styled(
                    format!(" {} · {}/{} ", status, lab.run_selected + 1, lab.runs.len()),
                    Style::default().fg(colour).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    match run.total_ms {
                        Some(ms) => format!("{ms} ms"),
                        None => "no duration".to_owned(),
                    },
                    Style::default().fg(theme.text_muted()),
                ),
            ]));
            let mut gauges = vec![
                format!(
                    " first token {}",
                    run.first_token_ms
                        .map_or_else(|| "not reported".to_owned(), |ms| format!("{ms} ms"))
                ),
                format!(
                    " · {} tok/s",
                    run.tokens_per_second
                        .map_or_else(|| "not reported".to_owned(), |value| format!("{value:.1}"))
                ),
                format!(
                    " · stop {}",
                    run.stop_reason
                        .clone()
                        .unwrap_or_else(|| "not reported".to_owned())
                ),
                format!(
                    " · {} tok in · {} tok out",
                    run.tokens.prompt, run.tokens.completion
                ),
            ];
            if run.tokens.estimated {
                gauges.push(" · estimated: the server reported no usage".to_owned());
            }
            rows.push(Line::from(Span::styled(
                gauges.concat(),
                Style::default().fg(if run.tokens.estimated {
                    theme.warning()
                } else {
                    theme.text_muted()
                }),
            )));
            if let Some(comparison) = lab.comparison() {
                rows.push(Line::from(Span::styled(
                    format!(
                        " vs #{}{}: {:+} tok · {}",
                        comparison.index + 1,
                        "",
                        comparison.tokens,
                        if comparison.latency_known {
                            format!("{:+} ms", comparison.latency_ms)
                        } else {
                            "latency not reported".to_owned()
                        }
                    ),
                    Style::default().fg(theme.accent_high()),
                )));
            }
            if let Some(warning) = run.warnings.first() {
                rows.push(Line::from(Span::styled(
                    format!(" {}", warning),
                    Style::default().fg(theme.warning()),
                )));
            }
        }
        None => {
            rows.push(Line::from(Span::styled(
                if lab.loading {
                    " reading the recipe…"
                } else {
                    " no runs yet · ^R runs the recipe"
                },
                Style::default().fg(theme.text_muted()),
            )));
        }
    }
    if let Some(error) = &lab.error {
        rows.push(Line::from(Span::styled(
            format!(" {error}"),
            Style::default().fg(theme.danger()),
        )));
    }
    frame.render_widget(Paragraph::new(rows), gauges);

    let width = answer.width.saturating_sub(1);
    let mut lines = wrapped(&lab.output, width);
    let height = usize::from(answer.height).max(1);
    if lines.len() > height {
        lines = lines.split_off(lines.len() - height);
    }
    frame.render_widget(
        Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()),
        answer,
    );

    let mut rows: Vec<Line> = Vec::new();
    rows.push(heading("runs", theme));
    let visible = usize::from(history.height).saturating_sub(1).max(1);
    let first = lab.run_selected.saturating_sub(visible.saturating_sub(1));
    for (index, run) in lab.runs.iter().enumerate().skip(first).take(visible) {
        let chosen = active && index == lab.run_selected;
        let style = if chosen {
            Style::default()
                .fg(theme.accent_high())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_muted())
        };
        let mark = if lab.compare == Some(index) { "*" } else { " " };
        rows.push(Line::from(Span::styled(
            format!(
                "{mark}{} {:<9} {:>6} tok {:>7} ms {}",
                index + 1,
                match run.status {
                    RunStatus::Running => "running",
                    RunStatus::Succeeded => "ok",
                    RunStatus::Failed => "failed",
                    RunStatus::Cancelled => "cancelled",
                },
                run.tokens.prompt + run.tokens.completion,
                run.total_ms.unwrap_or(0),
                &run.started_at[11..16.min(run.started_at.len())],
            ),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(rows), history);

    let prompt_text = if lab.prompt.is_empty() {
        Span::styled(
            " press i to write a prompt · ^R runs",
            Style::default().fg(theme.text_muted()),
        )
    } else {
        Span::raw(format!(" > {}", lab.prompt))
    };
    frame.render_widget(Paragraph::new(Line::from(prompt_text)), prompt);
}

fn draw_inspector(frame: &mut Frame, lab: &Lab, theme: &Theme, area: Rect) {
    let width = area.width.saturating_mul(4) / 5;
    let height = area.height.saturating_mul(4) / 5;
    let card = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent()))
        .title(" Effective request · v closes · what the run sent ");
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let json = lab.inspector_json();
    let lines: Vec<Line> = json
        .lines()
        .take(usize::from(inner.height))
        .map(|line| Line::from(Span::raw(line.to_owned())))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_proto::model::{ContextBudget, SamplingParams, TokenUsage, ToolExposure};

    fn recipe() -> ModelRecipe {
        ModelRecipe {
            schema_version: 1,
            id: "lab".to_owned(),
            label: "Lab".to_owned(),
            provider_id: "lmstudio".to_owned(),
            model_id: "qwen2.5-coder-7b".to_owned(),
            system_prompt: "You are a careful reviewer.".to_owned(),
            sampling: SamplingParams::default(),
            context: ContextBudget {
                window: 32_768,
                reserve_output: 1_024,
                strategy: ContextStrategy::TruncateOldest,
                max_corpus_bytes: 65_536,
            },
            tools: ToolExposure {
                read_files: true,
                write_files: false,
                run_commands: false,
                search: false,
                max_steps: 8,
            },
            notes: None,
            updated_at: "2026-11-12T10:00:00Z".to_owned(),
        }
    }

    fn run(id: &str, prompt: u32, completion: u32, total_ms: u64, estimated: bool) -> RunSummary {
        RunSummary {
            id: id.to_owned(),
            recipe_id: "lab".to_owned(),
            status: RunStatus::Succeeded,
            started_at: "2026-11-12T10:00:00Z".to_owned(),
            first_token_ms: Some(180),
            total_ms: Some(total_ms),
            tokens: TokenUsage {
                prompt,
                completion,
                estimated,
            },
            tokens_per_second: Some(41.0),
            stop_reason: Some("stop".to_owned()),
            warnings: Vec::new(),
            params: SamplingParams::default(),
            corpus_files: 2,
            message: None,
        }
    }

    #[test]
    fn a_temperature_outside_its_range_is_refused_by_name() {
        let mut recipe = recipe();
        let error = Field::Temperature.set(&mut recipe, "9").unwrap_err();
        assert!(error.contains("temperature"), "{error}");
        Field::Temperature.set(&mut recipe, "0.7").unwrap();
        assert!((recipe.sampling.temperature - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn an_empty_optional_knob_means_the_servers_default() {
        let mut recipe = recipe();
        Field::MaxTokens.set(&mut recipe, "512").unwrap();
        assert_eq!(recipe.sampling.max_tokens, Some(512));
        Field::MaxTokens.set(&mut recipe, "").unwrap();
        assert_eq!(recipe.sampling.max_tokens, None);
        assert_eq!(Field::MaxTokens.shown(&recipe), "server default");
    }

    #[test]
    fn space_cycles_the_overflow_strategy_through_three_names() {
        let mut recipe = recipe();
        assert_eq!(
            Field::Strategy.toggle(&mut recipe).as_deref(),
            Some("summarise")
        );
        assert_eq!(
            Field::Strategy.toggle(&mut recipe).as_deref(),
            Some("refuse")
        );
        assert_eq!(
            Field::Strategy.toggle(&mut recipe).as_deref(),
            Some("truncate oldest")
        );
    }

    #[test]
    fn a_tool_toggle_names_the_new_value_and_marks_the_draft() {
        let mut lab = Lab {
            recipes: vec![recipe()],
            ..Lab::default()
        };
        lab.field = FIELDS
            .iter()
            .position(|field| *field == Field::WriteFiles)
            .unwrap();
        assert_eq!(lab.toggle_field().as_deref(), Some("write files · on"));
        assert!(lab.recipe().unwrap().tools.write_files);
        assert!(lab.dirty);
        assert_eq!(lab.toggle_field().as_deref(), Some("write files · off"));
    }

    #[test]
    fn a_draft_that_is_cancelled_leaves_the_recipe_alone() {
        let mut lab = Lab {
            recipes: vec![recipe()],
            ..Lab::default()
        };
        lab.field = FIELDS
            .iter()
            .position(|field| *field == Field::Window)
            .unwrap();
        lab.begin_field();
        lab.draft.as_mut().unwrap().text = "999".to_owned();
        lab.cancel();
        assert_eq!(lab.recipe().unwrap().context.window, 32_768);
        assert!(!lab.dirty);
    }

    #[test]
    fn a_selector_is_three_fields_with_defaults_for_the_last_two() {
        let selector = parse_selector("docs/**/*.md").unwrap();
        assert_eq!(selector.max_files, 20);
        assert_eq!(selector.max_bytes, 65_536);
        assert!(parse_selector("").is_err());
        assert!(parse_selector("docs/**, many").is_err());
    }

    #[test]
    fn compare_reports_the_token_and_latency_difference() {
        let mut lab = Lab {
            recipes: vec![recipe()],
            runs: vec![run("b", 100, 20, 900, false), run("a", 90, 10, 700, false)],
            run_selected: 1,
            compare: Some(0),
            ..Lab::default()
        };
        let comparison = lab.comparison().unwrap();
        assert_eq!(comparison.tokens, -20);
        assert_eq!(comparison.latency_ms, -200);
        // The same run is not compared with itself.
        lab.run_selected = 0;
        assert!(lab.comparison().is_none());
    }

    #[test]
    fn the_budget_meter_is_the_records_files_against_the_recipes_ceiling() {
        let mut lab = Lab {
            recipes: vec![recipe()],
            record: Some(RunRecordView {
                schema_version: 1,
                summary: run("a", 10, 5, 100, false),
                request: acpilot_proto::model::RecordedRunRequest {
                    prompt: "review".to_owned(),
                    prompt_sha256: "sha256:0".to_owned(),
                    system_prompt_sha256: Some("sha256:1".to_owned()),
                    corpus: Vec::new(),
                    files: vec![acpilot_proto::model::ResolvedCorpusFile {
                        path: "docs/a.md".to_owned(),
                        bytes: 32_768,
                        tokens: 8_192,
                    }],
                    budget: recipe().context,
                },
            }),
            ..Lab::default()
        };
        let use_ = lab.budget_use();
        assert!(use_.known);
        assert_eq!(use_.bytes, 32_768);
        assert_eq!(use_.percent(), 50);
        // Without a record the meter says it does not know, rather than zero.
        lab.record = None;
        assert!(!lab.budget_use().known);
    }

    #[test]
    fn the_page_draws_its_three_panes_without_panicking() {
        let mut lab = Lab {
            recipes: vec![recipe()],
            runs: vec![run("a", 10, 5, 100, false)],
            selectors: vec![CorpusSelector {
                glob: "docs/**/*.md".to_owned(),
                max_files: 5,
                max_bytes: 1024,
            }],
            prompt: "review this".to_owned(),
            ..Lab::default()
        };
        lab.pane = Pane::Output;
        let theme = crate::theme::from_palette(&acpilot_extensions::theme::Palette::default());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 32)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &lab, &theme, frame.area()))
            .expect("a frame");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Model lab"), "the page has a title");
        assert!(text.contains("Recipe"), "the recipe pane is drawn");
        assert!(text.contains("review this"), "the prompt is on screen");
    }

    #[test]
    fn the_inspector_names_the_files_a_recorded_run_read() {
        let lab = Lab {
            recipes: vec![recipe()],
            record: Some(RunRecordView {
                schema_version: 1,
                summary: run("a", 10, 5, 100, false),
                request: acpilot_proto::model::RecordedRunRequest {
                    prompt: "review".to_owned(),
                    prompt_sha256: "sha256:0".to_owned(),
                    system_prompt_sha256: None,
                    corpus: Vec::new(),
                    files: vec![acpilot_proto::model::ResolvedCorpusFile {
                        path: "docs/a.md".to_owned(),
                        bytes: 100,
                        tokens: 25,
                    }],
                    budget: recipe().context,
                },
            }),
            ..Lab::default()
        };
        let json = lab.inspector_json();
        assert!(json.contains("docs/a.md"), "{json}");
        assert!(json.contains("\"model\": \"qwen2.5-coder-7b\""), "{json}");
    }
}
