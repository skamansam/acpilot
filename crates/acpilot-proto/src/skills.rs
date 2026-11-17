//! Agent Skills: the portable `SKILL.md` directory convention.
//!
//! A skill is data, not code. The daemon lists what a workspace, profile and its
//! activated packs provide, and the lab writes a new one through the same parser
//! that reads it.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Where a skill was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// The workspace's `.acpilot/skills/`.
    Workspace,
    /// The profile's `skills/`, shared by every workspace.
    Profile,
    /// An activated extension's `skills/`.
    Extension,
}

/// Where a saved skill is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillDestination {
    /// This workspace only.
    Workspace,
    /// Every workspace this profile opens.
    Profile,
}

/// One skill, including its instructions for preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SkillSummary {
    /// Lowercase hyphenated name, also the directory name.
    pub name: String,
    /// One-line description used by the picker.
    pub description: String,
    /// The instructions after the frontmatter.
    pub instructions: String,
    /// Which layer provided it.
    pub source: SkillSource,
    /// Absolute directory containing `SKILL.md`.
    pub path: String,
}

/// A create-or-replace request from the lab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillDraft {
    /// Lowercase hyphenated name, also the directory name.
    pub name: String,
    /// One-line description, at most 1024 characters.
    pub description: String,
    /// Instructions after the frontmatter.
    pub instructions: String,
    /// Workspace or profile.
    pub destination: SkillDestination,
}
