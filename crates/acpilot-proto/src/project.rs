//! The project map: what the workspace is for, as the agent wrote it down.
//!
//! A map is not a listing. The tree already answers "what files are here", and the
//! outline answers "what is in this file"; the map answers the question neither can:
//! which directory owns which idea, what that idea is, and what it leans on. The
//! agent writes it — a walk of the tree and a read of the README is the input — and
//! it lives in the repository under `.acpilot/` as TOML a person can read, correct
//! and review, because a wrong statement about the architecture should be a diff
//! somebody argues with rather than a fact only a model remembers.
//!
//! An area is a directory with a claim on it, and the areas are ordered as the agent
//! wrote them: a map that sorted itself would be a map whose order nobody chose.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// One part of the workspace, with what it is for and what it depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MapArea {
    /// What the area is called in a sentence: `TUI shell`, `Daemon`.
    pub name: String,
    /// The directory this area owns, workspace-relative. Empty or `.` is the whole
    /// workspace, which is how a map describes a repository that is not split up.
    pub path: String,
    /// What the area is for, in the agent's words. One paragraph rather than a list
    /// of files: the tree already lists files.
    pub summary: String,
    /// The areas and packages this one leans on, as the agent wrote them.
    ///
    /// Names rather than resolved edges: a map is a document, and a dependency the
    /// map gets wrong is a sentence to argue with rather than a build to break.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// The workspace map, as `GET /v1/project/map` serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProjectMap {
    /// Where the map was read from, workspace-relative.
    ///
    /// Always the same path today, and on the wire so a client can say where to look
    /// rather than have the location of the file compiled into two places.
    pub path: String,
    /// Whether the file is there at all.
    ///
    /// A map nobody has written yet is the normal first state, not an error, so the
    /// answer is an empty map with this false: a panel can say "the agent has not
    /// written one" without a 404 standing in for a sentence.
    pub written: bool,
    /// When the agent says it wrote the map, as it wrote it.
    ///
    /// A string rather than a timestamp for the reason every timestamp on this wire
    /// is: the format should not track a library choice, and the map is the agent's
    /// document rather than the daemon's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    /// The areas, in the order the agent wrote them.
    ///
    /// Always on the wire, empty included: this is the list the answer is about, and
    /// `written: false` is the field that says nobody has written one — a client
    /// should not have to read absence as emptiness as well.
    #[serde(default)]
    pub areas: Vec<MapArea>,
}

impl ProjectMap {
    /// The area that owns `file`: the deepest area path that is a prefix of it.
    ///
    /// Compared by path segments rather than by string prefix, because `crates/tui`
    /// is not a parent of `crates/tui-tools`. The deepest match wins so a map can
    /// describe the workspace and then refine it, and a tie is the earlier area:
    /// the order the agent wrote is the only tiebreak a reader can see.
    #[must_use]
    pub fn area_for(&self, file: &str) -> Option<usize> {
        let segments: Vec<&str> = file.split('/').filter(|part| !part.is_empty()).collect();
        let mut best: Option<(usize, usize)> = None;
        for (index, area) in self.areas.iter().enumerate() {
            let owned: Vec<&str> = area
                .path
                .split('/')
                .filter(|part| !part.is_empty() && *part != ".")
                .collect();
            if owned.len() > segments.len() {
                continue;
            }
            if !owned
                .iter()
                .zip(&segments)
                .all(|(owned, seen)| owned == seen)
            {
                continue;
            }
            if best.is_none_or(|(depth, _)| owned.len() > depth) {
                best = Some((owned.len(), index));
            }
        }
        best.map(|(_, index)| index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(path: &str) -> MapArea {
        MapArea {
            name: path.to_owned(),
            path: path.to_owned(),
            summary: String::new(),
            depends_on: Vec::new(),
        }
    }

    fn map(areas: &[&str]) -> ProjectMap {
        ProjectMap {
            path: ".acpilot/project-map.toml".to_owned(),
            written: true,
            updated: None,
            areas: areas.iter().map(|path| area(path)).collect(),
        }
    }

    #[test]
    fn the_deepest_area_owns_a_file() {
        let map = map(&["", "crates", "crates/acpilot-tui", "docs"]);
        assert_eq!(map.area_for("crates/acpilot-tui/src/app.rs"), Some(2));
        assert_eq!(map.area_for("crates/acpilotd/src/lib.rs"), Some(1));
        assert_eq!(map.area_for("docs/api.md"), Some(3));
        assert_eq!(
            map.area_for("README.md"),
            Some(0),
            "the workspace's own area"
        );
    }

    #[test]
    fn a_prefix_of_a_name_is_not_a_parent() {
        let map = map(&["crates/tui"]);
        assert_eq!(map.area_for("crates/tui-tools/src/lib.rs"), None);
        assert_eq!(map.area_for("crates/tui/src/app.rs"), Some(0));
    }

    #[test]
    fn an_area_that_owns_nothing_matches_nothing() {
        assert_eq!(map(&[]).area_for("src/main.rs"), None);
        let map = ProjectMap {
            areas: Vec::new(),
            ..map(&[])
        };
        assert_eq!(
            map.area_for("src/main.rs"),
            None,
            "an unwritten map is empty"
        );
    }
}
