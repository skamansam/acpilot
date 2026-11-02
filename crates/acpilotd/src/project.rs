//! The project map: the document the agent writes about what the workspace is for.
//!
//! The tree answers "what files are here" and the outline answers "what is in this
//! file"; neither says which directory owns which idea or what that idea leans on.
//! That is the map, and it belongs to the agent: a walk of the tree and a read of the
//! README is its input, so the map is written by the thing that read those.
//!
//! # Why the daemon reads it and not the client
//!
//! The map is a file in the workspace, and the workspace may be on another machine
//! (P8). A panel that read `.acpilot/project-map.toml` itself would work locally and
//! silently stop working the day the workspace moved, so the file is read where it
//! lives and served as [`ProjectMap`] — the same rule the tree and the searches
//! follow.
//!
//! # What a missing map is
//!
//! Not an error. A repository nobody has asked the agent about has no map yet, which
//! is its first state rather than a failure, so absence is `written: false` with no
//! areas. A map that will not parse *is* an error: a document nobody can read is a
//! fact the person editing it needs, and a panel showing "no map" over a file that
//! exists would send them looking for the wrong problem.

use std::path::Path;

use acpilot_core::AcpilotError;
use acpilot_proto::ErrorBody;
use acpilot_proto::project::{MapArea, ProjectMap};
use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;
use serde::Deserialize;
use utoipa::openapi::path::{HttpMethod, Operation};

use crate::{ApiError, AppState};

/// Where the map lives, relative to the workspace root.
///
/// One constant rather than a setting: the agent is told to write here, the panel
/// reads from here, and a configurable path would be a third place to disagree.
pub const MAP_PATH: &str = ".acpilot/project-map.toml";

/// `GET /v1/project/map?workspace=`.
///
/// # Errors
///
/// No workspace to read in (`409`), a map that is not readable as text or not TOML
/// (`400`). A map that is not there is an empty answer, not a `404`: see the module
/// comment.
async fn map(
    State(state): State<AppState>,
    Query(query): Query<MapQuery>,
) -> Result<Json<ProjectMap>, ApiError> {
    let root = crate::files::workspace_root(&state, query.workspace.as_deref())?;
    read_map(&root).map(Json)
}

/// The map file's shape, as the agent writes it.
///
/// A separate type from [`ProjectMap`] because the wire type says `written` and
/// carries `path`, and neither is something the file should have to repeat: the file
/// is the map, and where it sits is the route's business.
#[derive(Debug, Deserialize)]
struct Written {
    #[serde(default)]
    updated: Option<String>,
    #[serde(default)]
    areas: Vec<WrittenArea>,
}

/// One `[[areas]]` block.
#[derive(Debug, Deserialize)]
struct WrittenArea {
    name: String,
    /// Optional, because a map of a repository that is not split up is one area for
    /// the whole workspace, and `.` is the spelling nobody should have to type.
    #[serde(default)]
    path: String,
    summary: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

/// The query string this route takes.
#[derive(Debug, Deserialize)]
struct MapQuery {
    /// Workspace id, when the client names one. The daemon serves exactly one.
    workspace: Option<String>,
}

/// Read and parse the map under `root`.
fn read_map(root: &Path) -> Result<ProjectMap, ApiError> {
    let file = root.join(MAP_PATH);
    let text = match std::fs::read(&file) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| {
            ApiError::new(AcpilotError::invalid(format!(
                "{MAP_PATH} is not UTF-8 text"
            )))
            .with_detail("path", MAP_PATH)
        })?,
        // No map yet, which is a state rather than a failure: the panel says so, and
        // the agent is one prompt away from writing one.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectMap {
                path: MAP_PATH.to_owned(),
                written: false,
                updated: None,
                areas: Vec::new(),
            });
        }
        Err(error) => {
            return Err(ApiError::new(AcpilotError::Io(std::io::Error::new(
                error.kind(),
                format!("{file}: {error}", file = MAP_PATH),
            ))));
        }
    };
    let written: Written = toml::from_str(&text).map_err(|error| {
        ApiError::new(AcpilotError::invalid(format!("{MAP_PATH}: {error}"))).with_detail(
            "path",
            "the map is TOML: `updated`, then one [[areas]] per area",
        )
    })?;
    Ok(ProjectMap {
        path: MAP_PATH.to_owned(),
        written: true,
        updated: written.updated,
        areas: written
            .areas
            .into_iter()
            .map(|area| MapArea {
                name: area.name,
                path: area.path,
                summary: area.summary,
                depends_on: area.depends_on,
            })
            .collect(),
    })
}

/// The project routes, ready to merge into the daemon's router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/project/map", get(map))
}

/// This module's operations, for the OpenAPI document.
pub fn operations() -> Vec<(&'static str, HttpMethod, Operation)> {
    vec![(
        "/v1/project/map",
        HttpMethod::Get,
        <__path_map_doc as utoipa::Path>::operation(),
    )]
}

#[allow(dead_code)]
#[utoipa::path(
    get,
    path = "/v1/project/map",
    tag = "project",
    summary = "Read the project map",
    params(
        ("workspace" = Option<String>, Query, description = "Workspace id; empty or absent means the one this daemon serves")
    ),
    responses(
        (status = 200, description = "The map, or an empty one with `written: false` when nobody has written it", body = ProjectMap),
        (status = 400, description = "A map that is not UTF-8 or not TOML", body = ErrorBody),
        (status = 409, description = "This daemon has no workspace to read", body = ErrorBody)
    )
)]
fn map_doc() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_map_that_is_not_there_is_an_empty_answer() {
        let root = tempfile::tempdir().expect("a root");
        let map = read_map(root.path()).expect("absence is not an error");
        assert!(!map.written);
        assert!(map.areas.is_empty());
        assert_eq!(map.path, MAP_PATH);
    }

    #[test]
    fn the_written_map_is_read_in_the_order_the_agent_wrote_it() {
        let root = tempfile::tempdir().expect("a root");
        fs::create_dir(root.path().join(".acpilot")).expect(".acpilot");
        fs::write(
            root.path().join(MAP_PATH),
            "updated = \"2026-11-01\"\n\
             [[areas]]\nname = \"TUI\"\npath = \"crates/acpilot-tui\"\n\
             summary = \"the terminal frontend\"\ndepends_on = [\"acpilot-proto\"]\n\
             [[areas]]\nname = \"Daemon\"\npath = \"crates/acpilotd\"\n\
             summary = \"sessions, files, agents\"\n",
        )
        .expect("map");
        let map = read_map(root.path()).expect("a map");
        assert!(map.written);
        assert_eq!(map.updated.as_deref(), Some("2026-11-01"));
        let names: Vec<&str> = map.areas.iter().map(|area| area.name.as_str()).collect();
        assert_eq!(names, vec!["TUI", "Daemon"], "written order, not sorted");
        assert_eq!(map.areas[0].depends_on, vec!["acpilot-proto"]);
        assert!(
            map.areas[1].depends_on.is_empty(),
            "an area with no edges is a leaf, not an error"
        );
    }

    #[test]
    fn a_map_that_does_not_parse_is_an_error_rather_than_an_empty_one() {
        let root = tempfile::tempdir().expect("a root");
        fs::create_dir(root.path().join(".acpilot")).expect(".acpilot");
        fs::write(root.path().join(MAP_PATH), "areas = 3\n").expect("map");
        let error = read_map(root.path()).expect_err("not a map");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}
