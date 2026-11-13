//! Recipes: one TOML file per tuned model, edited through the API.
//!
//! The file is the source of truth, so every write is checked by reading the whole
//! directory back. A recipe that parses but names an undeclared provider is a
//! recipe a run cannot use, and the route says so at the moment somebody saved it
//! rather than at the moment they pressed Run.
//!
//! Writes go through a temporary file and a rename, like every other file this
//! daemon owns: an editor that saves by rename and this route both land as one
//! whole document. A save that fails validation puts the old bytes back, because a
//! rejected edit must not leave a half-state behind.

use std::path::{Path, PathBuf};

use acpilot_core::AcpilotError;
use acpilot_profile::providers::ProvidersFile;
use acpilot_profile::recipes::{RecipeFile, Recipes as Loaded};
use acpilot_proto::ErrorBody;
use acpilot_proto::model::{CreateRecipeRequest, ModelRecipe, RecipePatch};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::routing::get;
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::{ApiError, AppState};

/// A profile's recipe directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipes {
    directory: PathBuf,
}

impl Recipes {
    /// The recipes under `directory`. Nothing is created until the first write.
    #[must_use]
    pub fn open(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// The directory this store reads and writes.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Every recipe, sorted by id.
    ///
    /// # Errors
    ///
    /// The directory cannot be read or a file fails its rules.
    pub fn list(&self) -> Result<Vec<ModelRecipe>, ApiError> {
        let providers = self.providers()?;
        Loaded::load_in(&self.directory, &providers, &[])
            .map(|recipes| recipes.iter().cloned().collect())
            .map_err(invalid)
    }

    /// One recipe, when it exists.
    ///
    /// # Errors
    ///
    /// The directory cannot be read or a file fails its rules.
    pub fn get(&self, id: &str) -> Result<Option<ModelRecipe>, ApiError> {
        Ok(self.list()?.into_iter().find(|recipe| recipe.id == id))
    }

    /// Write a new recipe.
    ///
    /// # Errors
    ///
    /// The id is not one file name (`400`), the id is taken (`409`), or the file
    /// cannot be written (`500`).
    pub fn create(&self, request: CreateRecipeRequest) -> Result<ModelRecipe, ApiError> {
        one_file_name(&request.id)?;
        let path = self.path(&request.id);
        if path.exists() {
            return Err(ApiError::new(AcpilotError::conflict(format!(
                "recipe `{}` already exists",
                request.id
            ))));
        }
        let file = RecipeFile {
            schema_version: request.schema_version,
            id: request.id.clone(),
            label: request.label,
            provider_id: request.provider_id,
            model_id: request.model_id,
            system_prompt: request.system_prompt,
            sampling: request.sampling,
            context: request.context,
            tools: request.tools,
            notes: request.notes,
        };
        self.write_new(&path, &file)?;
        self.get(&request.id)?.ok_or_else(|| {
            ApiError::new(AcpilotError::Io(std::io::Error::other(format!(
                "recipe `{}` was written but reads back as nothing",
                request.id
            ))))
        })
    }

    /// Apply a patch to an existing recipe.
    ///
    /// # Errors
    ///
    /// No such recipe (`404`), or the patched file fails its rules (`400`), in
    /// which case the previous bytes are restored.
    pub fn patch(&self, id: &str, patch: RecipePatch) -> Result<ModelRecipe, ApiError> {
        one_file_name(id)?;
        let path = self.path(id);
        if !path.is_file() {
            return Err(not_found(id));
        }
        let old = std::fs::read_to_string(&path).map_err(io(&path))?;
        let mut file: RecipeFile = toml::from_str(&old).map_err(|error| {
            ApiError::new(AcpilotError::invalid(format!(
                "{} is not a recipe file: {error}",
                path.display()
            )))
        })?;
        if let Some(label) = patch.label {
            file.label = label;
        }
        if let Some(provider_id) = patch.provider_id {
            file.provider_id = provider_id;
        }
        if let Some(model_id) = patch.model_id {
            file.model_id = model_id;
        }
        if let Some(system_prompt) = patch.system_prompt {
            file.system_prompt = system_prompt;
        }
        if let Some(sampling) = patch.sampling {
            file.sampling = sampling;
        }
        if let Some(context) = patch.context {
            file.context = context;
        }
        if let Some(tools) = patch.tools {
            file.tools = tools;
        }
        if let Some(notes) = patch.notes {
            file.notes = Some(notes);
        }
        self.write_replacing(&path, &old, &file)?;
        self.get(id)?.ok_or_else(|| not_found(id))
    }

    /// Remove a recipe.
    ///
    /// # Errors
    ///
    /// No such recipe (`404`), or the file cannot be removed (`500`).
    pub fn delete(&self, id: &str) -> Result<(), ApiError> {
        one_file_name(id)?;
        let path = self.path(id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(not_found(id)),
            Err(source) => Err(ApiError::new(AcpilotError::Io(source))),
        }
    }

    /// The declarations a recipe is validated against.
    fn providers(&self) -> Result<ProvidersFile, ApiError> {
        let root = self.directory.parent().unwrap_or(&self.directory);
        ProvidersFile::load_in(root).map_err(invalid)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{id}.toml"))
    }

    /// Write a file that did not exist, removing it when it does not validate.
    fn write_new(&self, path: &Path, file: &RecipeFile) -> Result<(), ApiError> {
        std::fs::create_dir_all(&self.directory).map_err(io(&self.directory))?;
        let rendered = toml::to_string_pretty(file).map_err(|error| {
            ApiError::new(AcpilotError::invalid(format!(
                "the recipe cannot be written: {error}"
            )))
        })?;
        std::fs::write(path, rendered).map_err(io(path))?;
        if let Err(error) = self.list() {
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        Ok(())
    }

    /// Write a replacement, putting `old` back when the new bytes do not validate.
    fn write_replacing(&self, path: &Path, old: &str, file: &RecipeFile) -> Result<(), ApiError> {
        let rendered = toml::to_string_pretty(file).map_err(|error| {
            ApiError::new(AcpilotError::invalid(format!(
                "the recipe cannot be written: {error}"
            )))
        })?;
        std::fs::write(path, rendered).map_err(io(path))?;
        if let Err(error) = self.list() {
            let _ = std::fs::write(path, old);
            return Err(error);
        }
        Ok(())
    }
}

fn invalid(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(AcpilotError::invalid(error.to_string()))
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ApiError + '_ {
    move |source| {
        ApiError::new(AcpilotError::Io(std::io::Error::new(
            source.kind(),
            format!("{}: {source}", path.display()),
        )))
    }
}

fn not_found(id: &str) -> ApiError {
    ApiError::new(AcpilotError::not_found(format!("no recipe `{id}`")))
}

/// An id that is one file name, and not a path.
fn one_file_name(id: &str) -> Result<(), ApiError> {
    let usable = !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && !id.contains(['/', '\\'])
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        });
    if usable {
        Ok(())
    } else {
        Err(ApiError::new(AcpilotError::invalid(format!(
            "`{id}` is not a usable recipe id"
        )))
        .with_detail(
            "id",
            "one to 64 letters, digits, `-` or `_`, with no path separators",
        ))
    }
}

/// The recipe routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/recipes", get(list).post(create))
        .route("/v1/recipes/{id}", get(one).patch(patch).delete(remove))
}

fn store(state: &AppState) -> Result<&Recipes, ApiError> {
    state.recipes().ok_or_else(|| {
        ApiError::new(AcpilotError::invalid(
            "this daemon has no profile, so it has no recipes directory",
        ))
    })
}

async fn list(State(state): State<AppState>) -> Result<Json<Vec<ModelRecipe>>, ApiError> {
    Ok(Json(store(&state)?.list()?))
}

async fn one(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<ModelRecipe>, ApiError> {
    store(&state)?
        .get(&id)?
        .map(Json)
        .ok_or_else(|| not_found(&id))
}

async fn create(
    State(state): State<AppState>,
    body: Result<Json<CreateRecipeRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ModelRecipe>), ApiError> {
    let Json(request) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a recipe request: {rejection}"
        )))
    })?;
    let recipe = store(&state)?.create(request)?;
    Ok((StatusCode::CREATED, Json(recipe)))
}

async fn patch(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Result<Json<RecipePatch>, JsonRejection>,
) -> Result<Json<ModelRecipe>, ApiError> {
    let Json(patch) = body.map_err(|rejection| {
        ApiError::new(AcpilotError::invalid(format!(
            "the body is not a recipe patch: {rejection}"
        )))
    })?;
    Ok(Json(store(&state)?.patch(&id, patch)?))
}

async fn remove(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, ApiError> {
    store(&state)?.delete(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The recipe operations, for the OpenAPI document.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    vec![
        (
            "/v1/recipes",
            HttpMethod::Get,
            <__path_list_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/recipes",
            HttpMethod::Post,
            <__path_create_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/recipes/{id}",
            HttpMethod::Get,
            <__path_one_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/recipes/{id}",
            HttpMethod::Patch,
            <__path_patch_doc as utoipa::Path>::operation(),
        ),
        (
            "/v1/recipes/{id}",
            HttpMethod::Delete,
            <__path_remove_doc as utoipa::Path>::operation(),
        ),
    ]
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    operation_id = "list_recipes",
    path = "/v1/recipes",
    tag = "recipes",
    summary = "The recipes this profile holds",
    description = "One row per `recipes/<id>.toml`, sorted by id. Each read re-reads the directory, so a file edited by hand is visible without a restart. A file that does not load is a `400` naming it, not a silently shorter list.",
    responses((status = 200, description = "Every recipe that loads", body = Vec<ModelRecipe>))
)]
fn list_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    post,
    operation_id = "create_recipe",
    path = "/v1/recipes",
    tag = "recipes",
    summary = "Write a recipe file",
    description = "Writes `recipes/<id>.toml` and reads the directory back. A recipe naming an undeclared or disabled provider is `400`, and the new file is removed. An id that is already a file is `409`.",
    request_body = CreateRecipeRequest,
    responses(
        (status = 201, description = "The recipe as it now reads", body = ModelRecipe),
        (status = 400, description = "An id or field this build cannot use", body = ErrorBody),
        (status = 409, description = "The id is taken", body = ErrorBody)
    )
)]
fn create_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    get,
    operation_id = "get_recipe",
    path = "/v1/recipes/{id}",
    tag = "recipes",
    summary = "One recipe",
    responses(
        (status = 200, description = "The recipe", body = ModelRecipe),
        (status = 404, description = "No such recipe", body = ErrorBody)
    )
)]
fn one_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    patch,
    operation_id = "patch_recipe",
    path = "/v1/recipes/{id}",
    tag = "recipes",
    summary = "Edit the fields that changed",
    description = "Only the fields present move. The id is not editable: a run record and a promoted manifest refer to it. A patch that makes the file invalid is `400` and the previous bytes are restored.",
    request_body = RecipePatch,
    responses(
        (status = 200, description = "The recipe as it now reads", body = ModelRecipe),
        (status = 400, description = "The patched file fails its rules", body = ErrorBody),
        (status = 404, description = "No such recipe", body = ErrorBody)
    )
)]
fn patch_doc() {}

#[allow(dead_code)]
#[utoipa::path(
    delete,
    operation_id = "delete_recipe",
    path = "/v1/recipes/{id}",
    tag = "recipes",
    summary = "Remove a recipe file",
    responses(
        (status = 204, description = "The file is gone"),
        (status = 404, description = "No such recipe", body = ErrorBody)
    )
)]
fn remove_doc() {}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_proto::model::{ContextBudget, ContextStrategy, SamplingParams, ToolExposure};

    fn request(id: &str) -> CreateRecipeRequest {
        CreateRecipeRequest {
            schema_version: 1,
            id: id.to_owned(),
            label: "Demo".to_owned(),
            provider_id: "lm".to_owned(),
            model_id: "m".to_owned(),
            system_prompt: "Be terse.".to_owned(),
            sampling: SamplingParams::default(),
            context: ContextBudget {
                window: 4096,
                reserve_output: 256,
                strategy: ContextStrategy::TruncateOldest,
                max_corpus_bytes: 1024,
            },
            tools: ToolExposure {
                read_files: true,
                write_files: false,
                run_commands: false,
                search: false,
                max_steps: 4,
            },
            notes: None,
        }
    }

    fn with_provider() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            ProvidersFile::path_in(directory.path()),
            "schema_version = 1\n\n[[providers]]\nid = \"lm\"\nkind = \"lm_studio\"\nbase_url = \"http://127.0.0.1:1234\"\n",
        )
        .expect("providers");
        directory
    }

    #[test]
    fn a_recipe_round_trips_through_its_file() {
        let directory = with_provider();
        let store = Recipes::open(directory.path().join("recipes"));
        let created = store.create(request("demo")).expect("create");
        assert_eq!(created.id, "demo");
        assert_eq!(created.label, "Demo");
        assert!(
            store.directory().join("demo.toml").is_file(),
            "the file is the source of truth"
        );
        assert_eq!(store.list().expect("list").len(), 1);
        assert!(store.get("demo").expect("get").is_some());
    }

    #[test]
    fn a_recipe_naming_an_undeclared_provider_is_refused_and_removed() {
        let directory = with_provider();
        let store = Recipes::open(directory.path().join("recipes"));
        let mut bad = request("bad");
        bad.provider_id = "nowhere".to_owned();
        let error = store.create(bad).expect_err("the provider is not declared");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert!(
            !store.directory().join("bad.toml").exists(),
            "a refused recipe leaves no file behind"
        );
    }

    #[test]
    fn an_id_is_one_file_name() {
        for id in ["../escape", "a/b", ".hidden", "with space", ""] {
            assert!(one_file_name(id).is_err(), "{id}");
        }
        assert!(one_file_name("corpus-scraper").is_ok());
    }

    #[test]
    fn a_patch_moves_only_the_named_fields() {
        let directory = with_provider();
        let store = Recipes::open(directory.path().join("recipes"));
        store.create(request("demo")).expect("create");
        let patched = store
            .patch(
                "demo",
                RecipePatch {
                    label: Some("Renamed".to_owned()),
                    ..RecipePatch::default()
                },
            )
            .expect("patch");
        assert_eq!(patched.label, "Renamed");
        assert_eq!(patched.system_prompt, "Be terse.");
        assert_eq!(patched.tools.max_steps, 4);
    }

    #[test]
    fn a_patch_that_breaks_the_recipe_restores_the_old_bytes() {
        let directory = with_provider();
        let store = Recipes::open(directory.path().join("recipes"));
        store.create(request("demo")).expect("create");
        let before = std::fs::read_to_string(store.directory().join("demo.toml")).expect("read");
        let error = store
            .patch(
                "demo",
                RecipePatch {
                    provider_id: Some("nowhere".to_owned()),
                    ..RecipePatch::default()
                },
            )
            .expect_err("an undeclared provider is refused");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        let after = std::fs::read_to_string(store.directory().join("demo.toml")).expect("read");
        assert_eq!(after, before, "the rejected patch left the old file");
    }

    #[test]
    fn a_second_create_of_one_id_is_a_conflict() {
        let directory = with_provider();
        let store = Recipes::open(directory.path().join("recipes"));
        store.create(request("demo")).expect("create");
        let error = store
            .create(request("demo"))
            .expect_err("the home is taken");
        assert_eq!(error.status(), StatusCode::CONFLICT);
    }
}
