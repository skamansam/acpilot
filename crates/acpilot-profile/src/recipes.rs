//! The profile's recipes: `recipes/<id>.toml`.
//!
//! A recipe is a tuned configuration for one model of one provider, and it is a
//! file rather than a row because a prompt change is a code change: it deserves a
//! diff, a review and a `git revert` (ADR 0002 §3). The shape is the wire type
//! [`ModelRecipe`] with one difference — `updated_at` is not written by hand, it is
//! the file's modification time, so the timestamp is the one the filesystem kept
//! rather than the one an editor remembered to bump.
//!
//! Nothing here dials anything. A recipe is checked against what the profile
//! declares and what a probe has already reported; an unknown key, a window wider
//! than the model's own, or a tool the model cannot call is refused at load rather
//! than discovered halfway through a run. That is the same rule `config.toml` and
//! `agents.toml` follow, for the same reason: a knob that silently does nothing is
//! a bug report waiting to happen.
//!
//! Validation is only as good as what is known. With no probe on record, the model
//! id and its capabilities cannot be contradicted, so they are not: the loader
//! checks them when the provider's model list exists, and says nothing when nobody
//! has asked the server yet.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use acpilot_proto::model::{
    ContextBudget, DiscoveredModel, ModelRecipe, ModelType, ProviderId, RecipeId, SamplingParams,
    ToolExposure,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::providers::ProvidersFile;

/// The directory a profile keeps its recipes in.
pub const RECIPES_DIR: &str = "recipes";

/// The schema this build reads and writes.
pub const RECIPES_SCHEMA: u32 = 1;

/// The smallest `tools.max_steps` a recipe may ask for. Zero would be a run that
/// never calls the model at all, which is a recipe that cannot do what it says.
pub const MIN_STEPS: u32 = 1;

/// The largest `tools.max_steps`. A loop with no ceiling is a bill without one.
pub const MAX_STEPS: u32 = 50;

/// What the pre-flight estimate divides by.
///
/// Four bytes per token is the heuristic P4.5-4 documents an error band for, and it
/// is deliberately the same number here: a recipe the loader accepts must not be
/// refused by the run's own pre-flight over arithmetic that disagrees.
const BYTES_PER_TOKEN: u32 = 4;

/// Room assumed for the tool schemas a run sends beside the prompt.
///
/// A placeholder with a name, so P4.5-4 can replace it with the generated schema's
/// own count in one place. Leaving tool schemas out would let a recipe whose budget
/// only fits because the loader forgot them fail on its first run instead of here.
const TOOL_SCHEMA_TOKENS: u32 = 1_024;

/// Why a recipe could not be loaded, or a directory of them could not be read.
#[derive(Debug, Error)]
pub enum RecipesError {
    /// The directory or a file exists and could not be read.
    #[error("{path}: {source}")]
    Read {
        /// The path that failed.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },

    /// The file is not TOML, or not this document.
    #[error("{path}: {message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parser's own message, with the line it reached.
        message: String,
    },

    /// The file declares a schema version this build does not read.
    #[error("{path}: schema_version {found} is not this build's {expected}")]
    Schema {
        /// The file.
        path: PathBuf,
        /// What the file says.
        found: u32,
        /// What this build reads.
        expected: u32,
    },

    /// The `id` and the file name disagree, so a recipe would have two names.
    #[error("{path}: `{id}` is not the file's name `{file}`")]
    Name {
        /// The file.
        path: PathBuf,
        /// The id inside it.
        id: String,
        /// The name the directory gave it.
        file: String,
    },

    /// A value is outside the range the run could honour.
    #[error("{path}: {why}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// Which field, and what it said.
        why: String,
    },

    /// The recipe names a provider the profile does not declare, or declares disabled.
    #[error("{path}: provider `{provider_id}` {why}")]
    Provider {
        /// The file.
        path: PathBuf,
        /// The id the recipe names.
        provider_id: ProviderId,
        /// `is not declared in providers.toml`, or `is declared and disabled`.
        why: String,
    },

    /// The recipe names a model the provider does not report, or cannot use.
    #[error("{path}: model `{model_id}` {why}")]
    Model {
        /// The file.
        path: PathBuf,
        /// The id the recipe names.
        model_id: String,
        /// What the probe said.
        why: String,
    },
}

/// One `recipes/<id>.toml`, exactly as written.
///
/// [`ModelRecipe`] minus `updated_at`: a timestamp written by hand is a timestamp
/// that goes stale the first time somebody edits the file with `sed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeFile {
    /// Schema version. A newer file is refused rather than half-read.
    pub schema_version: u32,
    /// Identifier, also the file name under `recipes/`.
    pub id: String,
    /// Display label.
    pub label: String,
    /// Provider to call.
    pub provider_id: ProviderId,
    /// Model identifier to send.
    pub model_id: String,
    /// The master prompt.
    pub system_prompt: String,
    /// Sampling knobs. Absent means the agent-shaped defaults.
    #[serde(default)]
    pub sampling: SamplingParams,
    /// Context plan. Required: the window is a budget, and a budget nobody stated
    /// is one the run would have to invent.
    pub context: ContextBudget,
    /// Tool ceiling for the promoted agent.
    pub tools: ToolExposure,
    /// What this recipe is for, and what it is bad at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl RecipeFile {
    /// This recipe as the wire type, with the timestamp the caller found.
    #[must_use]
    pub fn into_recipe(self, updated_at: String) -> ModelRecipe {
        ModelRecipe {
            schema_version: self.schema_version,
            id: self.id,
            label: self.label,
            provider_id: self.provider_id,
            model_id: self.model_id,
            system_prompt: self.system_prompt,
            sampling: self.sampling,
            context: self.context,
            tools: self.tools,
            notes: self.notes,
            updated_at,
        }
    }

    /// Check the file against what is known before it becomes a recipe.
    ///
    /// `providers` is the profile's declaration, and `models` is whatever a probe
    /// reported. An empty `models` is "nobody has asked yet", not "nothing exists",
    /// so the model and its capabilities are checked only when the provider's list
    /// is present.
    fn validate(
        &self,
        path: &Path,
        file: &str,
        providers: &ProvidersFile,
        models: &[DiscoveredModel],
    ) -> Result<(), RecipesError> {
        if self.schema_version != RECIPES_SCHEMA {
            return Err(RecipesError::Schema {
                path: path.to_path_buf(),
                found: self.schema_version,
                expected: RECIPES_SCHEMA,
            });
        }
        if self.id != file {
            return Err(RecipesError::Name {
                path: path.to_path_buf(),
                id: self.id.clone(),
                file: file.to_owned(),
            });
        }
        if self.id.trim().is_empty() || self.id.contains(['/', '\\']) || self.id.starts_with('.') {
            return Err(RecipesError::Invalid {
                path: path.to_path_buf(),
                why: "`id` is not one file name".to_owned(),
            });
        }
        self.check_sampling(path)?;
        self.check_context(path)?;
        self.check_steps(path)?;

        let provider = providers
            .find(&self.provider_id)
            .ok_or_else(|| RecipesError::Provider {
                path: path.to_path_buf(),
                provider_id: self.provider_id.clone(),
                why: "is not declared in providers.toml".to_owned(),
            })?;
        if !provider.enabled {
            return Err(RecipesError::Provider {
                path: path.to_path_buf(),
                provider_id: self.provider_id.clone(),
                why: "is declared and disabled".to_owned(),
            });
        }

        let known: Vec<&DiscoveredModel> = models
            .iter()
            .filter(|model| model.provider_id == self.provider_id)
            .collect();
        if known.is_empty() {
            return Ok(());
        }
        let model = known
            .iter()
            .find(|model| model.id == self.model_id)
            .ok_or_else(|| RecipesError::Model {
                path: path.to_path_buf(),
                model_id: self.model_id.clone(),
                why: format!(
                    "is not reported by `{}` (it has {})",
                    self.provider_id,
                    known
                        .iter()
                        .map(|model| format!("`{}`", model.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })?;

        if model.model_type == ModelType::Embedding {
            return Err(RecipesError::Model {
                path: path.to_path_buf(),
                model_id: self.model_id.clone(),
                why: "produces embeddings and cannot answer a chat turn".to_owned(),
            });
        }
        if let Some(reported) = model.context_window
            && self.context.window > reported
        {
            return Err(RecipesError::Invalid {
                path: path.to_path_buf(),
                why: format!(
                    "`context.window` is {} but `{}` reports {reported}",
                    self.context.window, self.model_id
                ),
            });
        }
        self.check_tools(path, model)
    }

    fn check_sampling(&self, path: &Path) -> Result<(), RecipesError> {
        let sampling = &self.sampling;
        let invalid = |why: String| RecipesError::Invalid {
            path: path.to_path_buf(),
            why,
        };
        // `contains` is false for NaN, so an unparseable float is refused rather
        // than compared and quietly kept.
        if !(0.0..=2.0).contains(&sampling.temperature) {
            return Err(invalid(format!(
                "`sampling.temperature` is {} and must be 0.0–2.0",
                sampling.temperature
            )));
        }
        if !(0.0..=1.0).contains(&sampling.top_p) {
            return Err(invalid(format!(
                "`sampling.top_p` is {} and must be 0.0–1.0",
                sampling.top_p
            )));
        }
        if sampling.max_tokens.is_some_and(|tokens| tokens == 0) {
            return Err(invalid(
                "`sampling.max_tokens` is 0, so the model could never answer".to_owned(),
            ));
        }
        if sampling.top_k.is_some_and(|top_k| top_k == 0) {
            return Err(invalid("`sampling.top_k` is 0".to_owned()));
        }
        if sampling
            .repeat_penalty
            .is_some_and(|penalty| !penalty.is_finite() || penalty <= 0.0)
        {
            return Err(invalid(format!(
                "`sampling.repeat_penalty` is {} and must be above 0",
                sampling.repeat_penalty.unwrap_or_default()
            )));
        }
        Ok(())
    }

    fn check_context(&self, path: &Path) -> Result<(), RecipesError> {
        let context = &self.context;
        if context.window == 0 {
            return Err(RecipesError::Invalid {
                path: path.to_path_buf(),
                why: "`context.window` is 0, so no prompt fits".to_owned(),
            });
        }
        if context.reserve_output == 0 {
            return Err(RecipesError::Invalid {
                path: path.to_path_buf(),
                why: "`context.reserve_output` is 0, so the answer could be cut off by its own budget"
                    .to_owned(),
            });
        }
        if context.reserve_output >= context.window {
            return Err(RecipesError::Invalid {
                path: path.to_path_buf(),
                why: format!(
                    "`context.reserve_output` is {} of a {} token window, leaving no room for a prompt",
                    context.reserve_output, context.window
                ),
            });
        }

        // The estimate is bytes over four, and the sum is in `u64` so a recipe with
        // a billion-byte corpus reports a budget that does not fit rather than
        // wrapping into one that does.
        let prompt = tokens(self.system_prompt.len());
        let corpus = tokens(context.max_corpus_bytes as usize);
        let needed = u64::from(context.reserve_output)
            + u64::from(prompt)
            + u64::from(corpus)
            + u64::from(TOOL_SCHEMA_TOKENS);
        if needed >= u64::from(context.window) {
            return Err(RecipesError::Invalid {
                path: path.to_path_buf(),
                why: format!(
                    "the budget does not fit: {needed} tokens needed ({} reserved for the answer, \
                     ~{prompt} for the prompt, ~{corpus} for the corpus, {TOOL_SCHEMA_TOKENS} for \
                     tool schemas) in a {} token window",
                    context.reserve_output, context.window
                ),
            });
        }
        Ok(())
    }

    /// The step ceiling is checked whether or not a probe has named the model:
    /// the range is a property of the loop, not of the server.
    fn check_steps(&self, path: &Path) -> Result<(), RecipesError> {
        if !(MIN_STEPS..=MAX_STEPS).contains(&self.tools.max_steps) {
            return Err(RecipesError::Invalid {
                path: path.to_path_buf(),
                why: format!(
                    "`tools.max_steps` is {} and must be {MIN_STEPS}–{MAX_STEPS}",
                    self.tools.max_steps
                ),
            });
        }
        Ok(())
    }

    fn check_tools(&self, path: &Path, model: &DiscoveredModel) -> Result<(), RecipesError> {
        let asked = [
            ("read_files", self.tools.read_files),
            ("write_files", self.tools.write_files),
            ("run_commands", self.tools.run_commands),
            ("search", self.tools.search),
        ];
        if model.capabilities.tools {
            return Ok(());
        }
        let Some((name, _)) = asked.iter().find(|(_, exposed)| *exposed) else {
            return Ok(());
        };
        Err(RecipesError::Model {
            path: path.to_path_buf(),
            model_id: self.model_id.clone(),
            why: format!(
                "cannot call tools, so `tools.{name}` cannot be offered. A prompt that says \
                 otherwise teaches the model to invent calls"
            ),
        })
    }
}

/// Bytes as tokens, rounded up so an empty prompt is zero and one byte is one.
fn tokens(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(BYTES_PER_TOKEN as usize)).unwrap_or(u32::MAX)
}

/// Every recipe a profile has, by id.
///
/// Ordered by id rather than by directory entry, because a directory hands its
/// entries back in whatever order the filesystem keeps them and a list that
/// reshuffles between two runs is a list nobody can diff.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recipes {
    by_id: BTreeMap<RecipeId, ModelRecipe>,
}

impl Recipes {
    /// Where a profile directory keeps its recipes.
    #[must_use]
    pub fn dir_in(directory: &Path) -> PathBuf {
        directory.join(RECIPES_DIR)
    }

    /// The recipe with this id, when there is one.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ModelRecipe> {
        self.by_id.get(id)
    }

    /// Every recipe, in id order.
    pub fn iter(&self) -> impl Iterator<Item = &ModelRecipe> {
        self.by_id.values()
    }

    /// How many recipes the profile has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the profile has no recipes at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Read every `*.toml` in `directory`.
    ///
    /// A directory that is not there is no recipes, not an error: a profile that has
    /// never had one is the ordinary first-run state, the same way a missing
    /// `providers.toml` declares nothing. A file that *is* there and is wrong fails
    /// the whole load, because a recipe the daemon quietly skipped is a recipe whose
    /// author thinks it is in effect.
    ///
    /// # Errors
    ///
    /// The directory cannot be read, or any file in it fails [`RecipeFile`]'s rules
    /// or names a provider or model that cannot serve it.
    pub fn load_in(
        directory: &Path,
        providers: &ProvidersFile,
        models: &[DiscoveredModel],
    ) -> Result<Self, RecipesError> {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(RecipesError::Read {
                    path: directory.to_path_buf(),
                    source,
                });
            }
        };

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| RecipesError::Read {
                path: directory.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let is_recipe = path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
                && !entry.file_name().to_string_lossy().starts_with('.');
            if is_recipe && path.is_file() {
                paths.push(path);
            }
        }
        paths.sort();

        let mut by_id = BTreeMap::new();
        for path in paths {
            let recipe = load_one(&path, providers, models)?;
            by_id.insert(recipe.id.clone(), recipe);
        }
        Ok(Self { by_id })
    }
}

/// One file, from bytes to a validated recipe.
fn load_one(
    path: &Path,
    providers: &ProvidersFile,
    models: &[DiscoveredModel],
) -> Result<ModelRecipe, RecipesError> {
    let text = std::fs::read_to_string(path).map_err(|source| RecipesError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let file: RecipeFile = toml::from_str(&text).map_err(|error| RecipesError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let name = path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_owned();
    file.validate(path, &name, providers, models)?;
    Ok(file.into_recipe(modified_at(path)))
}

/// The file's modification time as RFC 3339, or the epoch when the filesystem will
/// not say. A timestamp is provenance, not a reason to refuse a recipe that is
/// otherwise correct.
fn modified_at(path: &Path) -> String {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| jiff::Timestamp::try_from(time).ok())
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
        .to_string()
}

/// Recipes as a long-running reader holds them: the last set that loaded, and a
/// signal when one of the files changed.
///
/// The last good set is the point. A save is a truncate and a write, so a reader
/// that replaced its recipes with whatever it could parse at that instant would
/// fail a turn over a half-written file. A parse error is returned to the caller,
/// which is what raises the notice, and [`Self::recipes`] still answers with what
/// last worked.
pub struct Live {
    directory: PathBuf,
    recipes: Recipes,
    changes: Option<std::sync::mpsc::Receiver<()>>,
}

impl Live {
    /// Load the recipes, then watch the files that were loaded.
    ///
    /// Files rather than the directory: [`crate::watch`] owns the debounce policy
    /// and matches events by path, so a directory passed here would report every
    /// editor scratch file beside the recipes. A recipe *created* later is not a
    /// target — the lab that writes it has the new set in hand — while an edit to a
    /// recipe that exists is.
    ///
    /// # Errors
    ///
    /// As [`Recipes::load_in`]: the first load has no last-good set to fall back to.
    pub fn open(
        directory: &Path,
        providers: &ProvidersFile,
        models: &[DiscoveredModel],
    ) -> Result<Self, RecipesError> {
        let recipes = Recipes::load_in(directory, providers, models)?;
        let paths: Vec<PathBuf> = recipes
            .iter()
            .map(|recipe| directory.join(format!("{}.toml", recipe.id)))
            .collect();
        // An empty profile is not watched: there is no file to watch, and the next
        // recipe arrives through the API that writes it, not through this reader.
        let changes = if paths.is_empty() {
            None
        } else {
            crate::watch::changes(paths)
        };
        Ok(Self {
            directory: directory.to_path_buf(),
            recipes,
            changes,
        })
    }

    /// The last set that loaded.
    #[must_use]
    pub fn recipes(&self) -> &Recipes {
        &self.recipes
    }

    /// The signal a caller waits on, when the directory was watchable.
    #[must_use]
    pub fn changed(&self) -> Option<&std::sync::mpsc::Receiver<()>> {
        self.changes.as_ref()
    }

    /// Re-read every recipe, replacing the set only when all of them load.
    ///
    /// # Errors
    ///
    /// Any file now fails a rule. The error is returned and the previous set is
    /// kept, so a caller can raise a notice without having lost the working recipes.
    pub fn reload(
        &mut self,
        providers: &ProvidersFile,
        models: &[DiscoveredModel],
    ) -> Result<(), RecipesError> {
        let recipes = Recipes::load_in(&self.directory, providers, models)?;
        self.recipes = recipes;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_proto::model::{
        ContextStrategy, MetadataSource, ModelCapabilities, ProviderKind, ProviderStatus,
        ProviderSummary,
    };

    use crate::providers::{DeclaredProvider, PROVIDERS_SCHEMA};

    const GOOD: &str = r#"
schema_version = 1
id = "corpus-scraper"
label = "Corpus scraper (Qwen 7B)"
provider_id = "lmstudio"
model_id = "qwen2-7b-instruct"
system_prompt = """
You extract structured facts from a documentation corpus.
"""

[sampling]
temperature = 0.2
top_p = 0.95
max_tokens = 1200
seed = 42
stop = []

[context]
window = 32768
reserve_output = 1500
strategy = "truncate_oldest"
max_corpus_bytes = 65536

[tools]
read_files = true
write_files = false
run_commands = false
search = true
max_steps = 8

notes = "Good at extraction, bad at arithmetic."
"#;

    fn providers() -> ProvidersFile {
        ProvidersFile {
            schema_version: PROVIDERS_SCHEMA,
            providers: vec![DeclaredProvider {
                id: "lmstudio".to_owned(),
                kind: ProviderKind::LmStudio,
                label: None,
                base_url: "http://127.0.0.1:1234".to_owned(),
                credential_ref: None,
                enabled: true,
            }],
        }
    }

    fn model(id: &str) -> DiscoveredModel {
        DiscoveredModel {
            id: id.to_owned(),
            provider_id: "lmstudio".to_owned(),
            model_type: ModelType::Chat,
            architecture: None,
            quantization: None,
            loaded: Some(true),
            context_window: Some(32_768),
            context_source: MetadataSource::Server,
            capabilities: ModelCapabilities {
                chat: true,
                streaming: true,
                tools: true,
                structured_output: false,
                vision: false,
            },
        }
    }

    fn write(directory: &Path, name: &str, text: &str) -> PathBuf {
        std::fs::create_dir_all(directory).expect("recipes directory");
        let path = directory.join(name);
        std::fs::write(&path, text).expect("write recipe");
        path
    }

    fn load(text: &str) -> Result<Recipes, RecipesError> {
        load_with(text, providers(), vec![model("qwen2-7b-instruct")])
    }

    fn load_with(
        text: &str,
        providers: ProvidersFile,
        models: Vec<DiscoveredModel>,
    ) -> Result<Recipes, RecipesError> {
        let directory = tempfile::tempdir().expect("temp dir");
        write(directory.path(), "corpus-scraper.toml", text);
        Recipes::load_in(directory.path(), &providers, &models)
    }

    #[test]
    fn a_profile_with_no_recipes_directory_has_no_recipes() {
        let directory = tempfile::tempdir().expect("temp dir");
        let recipes = Recipes::load_in(
            &directory.path().join(RECIPES_DIR),
            &providers(),
            &[model("qwen2-7b-instruct")],
        )
        .expect("a missing directory is not an error");
        assert!(recipes.is_empty());
    }

    #[test]
    fn a_recipe_loads_with_the_shape_the_api_serves() {
        let recipes = load(GOOD).expect("the documented example loads");
        let recipe = recipes.get("corpus-scraper").expect("by id");
        assert_eq!(recipe.label, "Corpus scraper (Qwen 7B)");
        assert_eq!(recipe.provider_id, "lmstudio");
        assert_eq!(recipe.context.window, 32_768);
        assert_eq!(recipe.context.strategy, ContextStrategy::TruncateOldest);
        assert!(recipe.tools.read_files);
        assert!(!recipe.tools.run_commands);
        assert_eq!(recipe.tools.max_steps, 8);
        assert_eq!(recipe.sampling.seed, Some(42));
        assert!(
            recipe.updated_at.parse::<jiff::Timestamp>().is_ok(),
            "the timestamp is RFC 3339: {}",
            recipe.updated_at
        );
    }

    #[test]
    fn recipes_are_listed_by_id_and_not_by_directory_order() {
        let directory = tempfile::tempdir().expect("temp dir");
        for name in ["zebra", "alpha"] {
            write(
                directory.path(),
                &format!("{name}.toml"),
                &GOOD.replace("corpus-scraper", name),
            );
        }
        let recipes = Recipes::load_in(
            directory.path(),
            &providers(),
            &[model("qwen2-7b-instruct")],
        )
        .expect("both load");
        let ids: Vec<&str> = recipes.iter().map(|recipe| recipe.id.as_str()).collect();
        assert_eq!(ids, ["alpha", "zebra"]);
    }

    #[test]
    fn a_file_that_is_not_toml_is_refused_with_the_parser_line() {
        let error = load("schema_version = 1\nid = \n").expect_err("a broken file");
        assert!(matches!(error, RecipesError::Parse { .. }), "{error}");
    }

    #[test]
    fn a_typo_is_an_error_and_not_a_knob_that_does_nothing() {
        let error = load(&GOOD.replace("max_steps = 8", "max_step = 8"))
            .expect_err("an unknown key is a refusal");
        assert!(matches!(error, RecipesError::Parse { .. }), "{error}");
        assert!(
            error.to_string().contains("max_step"),
            "the message names the key: {error}"
        );
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_half_read() {
        let error = load(&GOOD.replace("schema_version = 1", "schema_version = 2"))
            .expect_err("a newer file is refused");
        assert!(
            matches!(error, RecipesError::Schema { found: 2, .. }),
            "{error}"
        );
    }

    #[test]
    fn an_id_that_is_not_the_file_name_is_refused() {
        let error = load(&GOOD.replace("id = \"corpus-scraper\"", "id = \"other\""))
            .expect_err("the file name is the id");
        assert!(matches!(error, RecipesError::Name { .. }), "{error}");
    }

    #[test]
    fn a_window_wider_than_the_model_reports_is_refused_with_both_numbers() {
        let error = load(&GOOD.replace("window = 32768", "window = 65536"))
            .expect_err("the model has 32768");
        assert!(matches!(error, RecipesError::Invalid { .. }), "{error}");
        let text = error.to_string();
        assert!(text.contains("65536") && text.contains("32768"), "{text}");
    }

    #[test]
    fn an_unknown_model_is_refused_when_the_provider_has_been_probed() {
        let error = load(&GOOD.replace("qwen2-7b-instruct", "ghost-9000"))
            .expect_err("the provider reports one model");
        assert!(matches!(error, RecipesError::Model { .. }), "{error}");
        assert!(
            error.to_string().contains("qwen2-7b-instruct"),
            "the message says what the server has: {error}"
        );
    }

    #[test]
    fn a_model_nobody_probed_is_not_contradicted() {
        let recipes = load_with(GOOD, providers(), Vec::new())
            .expect("with no probe on record the model cannot be checked");
        assert_eq!(recipes.len(), 1);
    }

    #[test]
    fn tools_on_a_model_that_cannot_call_them_are_refused_with_a_reason() {
        let mut incapable = model("qwen2-7b-instruct");
        incapable.capabilities.tools = false;
        let error =
            load_with(GOOD, providers(), vec![incapable]).expect_err("the model cannot call tools");
        assert!(matches!(error, RecipesError::Model { .. }), "{error}");
        let text = error.to_string();
        assert!(
            text.contains("read_files") && text.contains("cannot call tools"),
            "the message names the tool and the reason: {text}"
        );
    }

    #[test]
    fn a_model_that_cannot_call_tools_may_still_have_none_exposed() {
        let mut incapable = model("qwen2-7b-instruct");
        incapable.capabilities.tools = false;
        let quiet = GOOD
            .replace("read_files = true", "read_files = false")
            .replace("search = true", "search = false");
        load_with(&quiet, providers(), vec![incapable]).expect("a prompt-only recipe is fine");
    }

    #[test]
    fn an_embedding_model_cannot_be_a_recipe() {
        let mut embedding = model("qwen2-7b-instruct");
        embedding.model_type = ModelType::Embedding;
        let error = load_with(GOOD, providers(), vec![embedding])
            .expect_err("embeddings do not answer chat turns");
        assert!(error.to_string().contains("embeddings"), "{error}");
    }

    #[test]
    fn a_budget_that_does_not_fit_is_refused_with_the_numbers() {
        let error = load(&GOOD.replace("window = 32768", "window = 2000"))
            .expect_err("the sum does not fit");
        let text = error.to_string();
        assert!(
            text.contains("does not fit") && text.contains("2000"),
            "the message shows the arithmetic: {text}"
        );
    }

    #[test]
    fn zero_reserved_output_is_refused() {
        let error = load(&GOOD.replace("reserve_output = 1500", "reserve_output = 0"))
            .expect_err("the answer needs room");
        assert!(error.to_string().contains("reserve_output"), "{error}");
    }

    #[test]
    fn a_step_ceiling_outside_the_range_is_refused() {
        for steps in ["0", "51"] {
            let error = load(&GOOD.replace("max_steps = 8", &format!("max_steps = {steps}")))
                .expect_err("the ceiling has a range");
            assert!(matches!(error, RecipesError::Invalid { .. }), "{error}");
        }
    }

    #[test]
    fn sampling_outside_its_range_is_refused() {
        for (from, to) in [
            ("temperature = 0.2", "temperature = 2.5"),
            ("top_p = 0.95", "top_p = 1.5"),
            ("max_tokens = 1200", "max_tokens = 0"),
        ] {
            let error = load(&GOOD.replace(from, to)).expect_err("out of range");
            assert!(matches!(error, RecipesError::Invalid { .. }), "{error}");
        }
    }

    #[test]
    fn a_provider_that_is_not_declared_is_refused_by_name() {
        let error = load(&GOOD.replace("provider_id = \"lmstudio\"", "provider_id = \"ollama\""))
            .expect_err("nothing declares ollama");
        assert!(
            matches!(error, RecipesError::Provider { .. }),
            "the recipe names the provider: {error}"
        );
    }

    #[test]
    fn a_disabled_provider_is_refused_rather_than_tried() {
        let mut providers = providers();
        providers.providers[0].enabled = false;
        let error = load_with(GOOD, providers, vec![model("qwen2-7b-instruct")])
            .expect_err("a disabled provider is not a target");
        assert!(error.to_string().contains("disabled"), "{error}");
    }

    #[test]
    fn a_provider_row_from_a_probe_is_not_needed_to_load() {
        // The declaration is what the loader checks; the probe's row only adds the
        // model list. A profile that has never probed still loads its recipes.
        let summary = ProviderSummary {
            id: "lmstudio".to_owned(),
            kind: ProviderKind::LmStudio,
            label: "LM Studio".to_owned(),
            base_url: "http://127.0.0.1:1234".to_owned(),
            status: ProviderStatus::NotProbed,
            model_count: 0,
            credential_ref: None,
            last_probed_at: None,
            detail: None,
        };
        assert_eq!(summary.model_count, 0);
        load_with(GOOD, providers(), Vec::new()).expect("declared, unprobed, loads");
    }

    #[test]
    fn a_broken_edit_keeps_the_last_good_recipes() {
        let directory = tempfile::tempdir().expect("temp dir");
        let recipes_dir = Recipes::dir_in(directory.path());
        write(&recipes_dir, "corpus-scraper.toml", GOOD);
        let mut live = Live::open(&recipes_dir, &providers(), &[model("qwen2-7b-instruct")])
            .expect("the first load has a good file");

        write(
            &recipes_dir,
            "corpus-scraper.toml",
            "schema_version = 1\nid = \n",
        );
        let error = live
            .reload(&providers(), &[model("qwen2-7b-instruct")])
            .expect_err("a half-written save is an error, not a empty profile");
        assert!(matches!(error, RecipesError::Parse { .. }), "{error}");
        assert_eq!(live.recipes().len(), 1, "the working recipe survived");
        assert_eq!(
            live.recipes()
                .get("corpus-scraper")
                .expect("still there")
                .context
                .window,
            32_768,
            "the last good set is what a turn would use"
        );
    }

    #[test]
    fn a_good_edit_applies_on_reload() {
        let directory = tempfile::tempdir().expect("temp dir");
        let recipes_dir = Recipes::dir_in(directory.path());
        write(&recipes_dir, "corpus-scraper.toml", GOOD);
        let mut live =
            Live::open(&recipes_dir, &providers(), &[model("qwen2-7b-instruct")]).expect("loads");

        write(
            &recipes_dir,
            "corpus-scraper.toml",
            &GOOD
                .replace("window = 32768", "window = 16384")
                .replace("max_corpus_bytes = 65536", "max_corpus_bytes = 8192"),
        );
        live.reload(&providers(), &[model("qwen2-7b-instruct")])
            .expect("the edit loads");
        assert_eq!(
            live.recipes()
                .get("corpus-scraper")
                .expect("the recipe")
                .context
                .window,
            16_384
        );
    }

    #[test]
    fn a_save_reaches_the_holder() {
        let directory = tempfile::tempdir().expect("temp dir");
        let recipes_dir = Recipes::dir_in(directory.path());
        let path = write(&recipes_dir, "corpus-scraper.toml", GOOD);
        let live =
            Live::open(&recipes_dir, &providers(), &[model("qwen2-7b-instruct")]).expect("loads");
        let changes = live.changed().expect("the file is watchable");

        std::fs::write(&path, GOOD).expect("write");
        assert!(
            changes
                .recv_timeout(std::time::Duration::from_secs(10))
                .is_ok(),
            "an edit to a loaded recipe is a signal"
        );
    }

    #[test]
    fn a_neighbouring_file_that_is_not_a_recipe_is_ignored() {
        let directory = tempfile::tempdir().expect("temp dir");
        write(directory.path(), "corpus-scraper.toml", GOOD);
        write(directory.path(), "notes.txt", "not a recipe");
        write(
            directory.path(),
            ".corpus-scraper.toml.swp",
            "editor scratch",
        );
        let recipes = Recipes::load_in(
            directory.path(),
            &providers(),
            &[model("qwen2-7b-instruct")],
        )
        .expect("only the toml is read");
        assert_eq!(recipes.len(), 1);
    }
}
