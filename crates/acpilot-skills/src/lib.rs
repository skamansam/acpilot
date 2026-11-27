//! Agent Skills, discovered from the workspace and from extensions.
//!
//! A skill is a directory with a `SKILL.md`: YAML frontmatter (`name`,
//! `description`) followed by instructions. That is the portable convention
//! agents already understand, so a skill written for another tool works here
//! without a converter.
//!
//! Two sources exist, and the precedence is deliberate:
//!
//! 1. extensions install skills for every workspace that activates them;
//! 2. the workspace's own `.acpilot/skills/` directory overrides a skill of the
//!    same name, because a project's instructions should win over a pack's.
//!
//! Skills are data. Nothing here executes, and nothing is loaded until a caller
//! asks for the list; the trust boundary is the same as the extension store's.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Longest accepted skill name, in characters.
pub const NAME_LIMIT: usize = 64;
/// Longest accepted description, in characters.
pub const DESCRIPTION_LIMIT: usize = 1024;

/// Where a skill came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// A project's `.acpilot/skills/` directory.
    Workspace,
    /// A profile's `skills/` directory, available to every workspace it opens.
    Profile,
    /// An activated extension's `skills/` directory.
    Extension,
}

impl SkillSource {
    /// Precedence: a project beats a profile, which beats a pack.
    fn rank(self) -> u8 {
        match self {
            Self::Workspace => 3,
            Self::Profile => 2,
            Self::Extension => 1,
        }
    }
}

/// One parsed skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Identifier: the frontmatter name, which must match the directory name.
    pub name: String,
    /// One-line description used for discovery and the skill picker.
    pub description: String,
    /// The instructions after the frontmatter.
    pub instructions: String,
    /// Directory containing `SKILL.md`.
    pub directory: PathBuf,
    /// Which source it was found in.
    pub source: SkillSource,
}

/// Why a skill could not be loaded.
#[derive(Debug, Error)]
pub enum SkillError {
    /// The file does not start with `---`.
    #[error("{}: missing YAML frontmatter", path.display())]
    MissingFrontmatter {
        /// The file that was read.
        path: PathBuf,
    },
    /// The opening `---` has no closing delimiter.
    #[error("{}: frontmatter is not closed with `---`", path.display())]
    UnclosedFrontmatter {
        /// The file that was read.
        path: PathBuf,
    },
    /// The frontmatter is not valid YAML for the expected fields.
    #[error("{}: invalid frontmatter: {source}", path.display())]
    Yaml {
        /// The file that was read.
        path: PathBuf,
        /// The parser's diagnosis.
        #[source]
        source: Box<serde_saphyr::Error>,
    },
    /// `name` is absent.
    #[error("{}: frontmatter has no `name`", path.display())]
    MissingName {
        /// The file that was read.
        path: PathBuf,
    },
    /// `name` is not a valid skill identifier.
    #[error("{}: `{name}` is not a lowercase hyphenated name of at most {NAME_LIMIT} characters", path.display())]
    InvalidName {
        /// The file that was read.
        path: PathBuf,
        /// The rejected name.
        name: String,
    },
    /// The directory disagrees with the frontmatter.
    #[error("{}: skill name `{name}` does not match directory `{directory}`", path.display())]
    NameMismatch {
        /// The file that was read.
        path: PathBuf,
        /// Declared name.
        name: String,
        /// Directory name.
        directory: String,
    },
    /// `description` is absent or empty.
    #[error("{}: frontmatter has no `description`", path.display())]
    MissingDescription {
        /// The file that was read.
        path: PathBuf,
    },
    /// The description is longer than the convention allows.
    #[error("{}: description is {len} characters; the limit is {DESCRIPTION_LIMIT}", path.display())]
    DescriptionTooLong {
        /// The file that was read.
        path: PathBuf,
        /// Measured length in characters.
        len: usize,
    },
    /// The instructions are empty.
    #[error("{}: the skill has no instructions", path.display())]
    EmptyInstructions {
        /// The file that was read.
        path: PathBuf,
    },
    /// Two skills in the same source share a name.
    #[error("duplicate skill `{name}`: {} and {}", first.display(), second.display())]
    Duplicate {
        /// The shared name.
        name: String,
        /// First definition.
        first: PathBuf,
        /// Second definition.
        second: PathBuf,
    },
    /// Reading a file or directory failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Deserialize)]
struct FrontMatter {
    name: Option<String>,
    description: Option<String>,
}

/// Discover skills from a workspace and a set of extension roots.
///
/// Workspace skills live in `<workspace>/.acpilot/skills/<name>/SKILL.md`;
/// extension skills live in `<extension>/skills/<name>/SKILL.md`. Missing
/// directories are not an error: most workspaces have no skills, and most
/// extensions do not ship any.
pub fn discover(
    workspace_root: &Path,
    extension_roots: &[PathBuf],
) -> Result<Vec<Skill>, SkillError> {
    discover_scoped(workspace_root, None, extension_roots)
}

/// Discover skills from a workspace, an optional profile, and extension roots.
///
/// Precedence is project over profile over pack: a project's instructions are
/// about this checkout, a profile's are about every checkout, and a pack's are
/// somebody else's defaults. A lower layer with the same name is replaced; two
/// skills in one layer are still an error.
pub fn discover_scoped(
    workspace_root: &Path,
    profile_root: Option<&Path>,
    extension_roots: &[PathBuf],
) -> Result<Vec<Skill>, SkillError> {
    let discovered = discover_scoped_recovering(workspace_root, profile_root, extension_roots);
    match discovered.problems.into_iter().next() {
        Some(error) => Err(error),
        None => Ok(discovered.skills),
    }
}

/// Everything discovery found, with the skills that would not parse kept aside.
///
/// A daemon that lists skills or opens a session needs the valid rows even when one
/// `SKILL.md` is broken. Failing the whole scope for one bad file denies a person
/// every skill they have. The problem is still reported, so the file is not lost.
#[derive(Debug, Default)]
pub struct Discovered {
    /// The skills that parsed, in name order.
    pub skills: Vec<Skill>,
    /// One entry per skill that could not be loaded.
    pub problems: Vec<SkillError>,
}

/// Discover skills, keeping the ones that parse and naming the ones that do not.
///
/// Precedence is the same as [`discover_scoped`]: project, then profile, then pack.
/// A name collision at equal precedence is a problem rather than an error, so the
/// first definition still lists while the second is reported.
pub fn discover_scoped_recovering(
    workspace_root: &Path,
    profile_root: Option<&Path>,
    extension_roots: &[PathBuf],
) -> Discovered {
    let mut found: BTreeMap<String, Skill> = BTreeMap::new();
    let mut problems = Vec::new();

    for root in extension_roots {
        load_tree(
            &root.join("skills"),
            SkillSource::Extension,
            &mut found,
            &mut problems,
        );
    }
    if let Some(profile_root) = profile_root {
        load_tree(
            &profile_root.join("skills"),
            SkillSource::Profile,
            &mut found,
            &mut problems,
        );
    }
    load_tree(
        &workspace_root.join(".acpilot/skills"),
        SkillSource::Workspace,
        &mut found,
        &mut problems,
    );

    Discovered {
        skills: found.into_values().collect(),
        problems,
    }
}

/// Write one skill under `root`, validated by the same parser that reads it.
///
/// `root` is the workspace, profile or extension directory; the directory layout
/// follows from `source`. The write is one rename, so a half-written `SKILL.md`
/// is never discoverable.
pub fn save_skill(
    root: &Path,
    source: SkillSource,
    name: &str,
    description: &str,
    instructions: &str,
) -> Result<Skill, SkillError> {
    let directory = match source {
        SkillSource::Workspace => root.join(".acpilot/skills").join(name),
        SkillSource::Profile | SkillSource::Extension => root.join("skills").join(name),
    };
    let path = directory.join("SKILL.md");
    let description = description.trim();
    let instructions = instructions.trim();
    let contents = render(name, description, instructions);
    // Parse before writing: a name that is not a directory, a missing
    // description or an empty body is refused while the old file is intact.
    let skill = parse_skill(&contents, &path, source)?;
    fs::create_dir_all(&directory).map_err(|source| SkillError::Io {
        path: directory.clone(),
        source,
    })?;
    let temporary = directory.join("SKILL.md.tmp");
    fs::write(&temporary, contents).map_err(|source| SkillError::Io {
        path: temporary.clone(),
        source,
    })?;
    fs::rename(&temporary, &path).map_err(|source| SkillError::Io { path, source })?;
    Ok(skill)
}

/// The system-prompt section for `skills`, in the order given.
///
/// Every consumer appends the same text: the model agent, a lab run, and an
/// external ACP agent. A skill then reads the same in each place. The leading
/// blank line separates the section from the prompt before it.
#[must_use]
pub fn instructions(skills: &[Skill]) -> String {
    let mut text = String::new();
    for skill in skills {
        text.push_str("\n\n## Skill: ");
        text.push_str(&skill.name);
        text.push_str("\n\n");
        text.push_str(&skill.instructions);
    }
    text
}

/// The file text for one skill, with the YAML escaped rather than interpolated.
#[must_use]
pub fn render(name: &str, description: &str, instructions: &str) -> String {
    // A JSON string is a valid YAML double-quoted scalar, and escaping by hand
    // keeps this crate free of a serialization dependency it does not otherwise
    // need. A description is one line, but a pasted one may not be.
    let description = description
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!(
        "---\nname: {name}\ndescription: \"{description}\"\n---\n\n{}\n",
        instructions.trim()
    )
}

fn load_tree(
    directory: &Path,
    source: SkillSource,
    found: &mut BTreeMap<String, Skill>,
    problems: &mut Vec<SkillError>,
) {
    if !directory.is_dir() {
        return;
    }

    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            problems.push(SkillError::Io {
                path: directory.to_path_buf(),
                source: error,
            });
            return;
        }
    };
    let mut entries: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();

    for entry in entries {
        let manifest = entry.join("SKILL.md");
        if !manifest.is_file() {
            // A directory without SKILL.md is assets, not a skill.
            continue;
        }
        let skill = match load_skill(&manifest, source) {
            Ok(skill) => skill,
            Err(error) => {
                problems.push(error);
                continue;
            }
        };
        if let Some(existing) = found.get(&skill.name) {
            if skill.source.rank() > existing.source.rank() {
                found.insert(skill.name.clone(), skill);
                continue;
            }
            // Equal precedence is a real collision: two packs, or two skills in
            // one directory, claiming one name means neither is predictable.
            problems.push(SkillError::Duplicate {
                name: skill.name,
                first: existing.directory.clone(),
                second: skill.directory.clone(),
            });
            continue;
        }
        found.insert(skill.name.clone(), skill);
    }
}

/// Load one `SKILL.md`.
pub fn load_skill(path: &Path, source: SkillSource) -> Result<Skill, SkillError> {
    let contents = fs::read_to_string(path).map_err(|error| SkillError::Io {
        path: path.to_path_buf(),
        source: error,
    })?;
    parse_skill(&contents, path, source)
}

/// Parse a `SKILL.md` whose path is used for errors and the directory check.
pub fn parse_skill(contents: &str, path: &Path, source: SkillSource) -> Result<Skill, SkillError> {
    let text = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let rest = text
        .strip_prefix("---")
        .and_then(|rest| {
            rest.strip_prefix("\r\n")
                .or_else(|| rest.strip_prefix('\n'))
        })
        .ok_or_else(|| SkillError::MissingFrontmatter {
            path: path.to_path_buf(),
        })?;

    let mut frontmatter = String::new();
    let mut body_start = None;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            body_start = Some(offset);
            break;
        }
        frontmatter.push_str(line);
    }
    let body_start = body_start.ok_or_else(|| SkillError::UnclosedFrontmatter {
        path: path.to_path_buf(),
    })?;

    let meta: FrontMatter =
        serde_saphyr::from_str(&frontmatter).map_err(|source| SkillError::Yaml {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;

    let name = meta.name.ok_or_else(|| SkillError::MissingName {
        path: path.to_path_buf(),
    })?;
    if !valid_name(&name) {
        return Err(SkillError::InvalidName {
            path: path.to_path_buf(),
            name,
        });
    }

    let directory = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if directory != name {
        return Err(SkillError::NameMismatch {
            path: path.to_path_buf(),
            name,
            directory: directory.to_owned(),
        });
    }

    let description = meta
        .description
        .map(|description| description.trim().to_owned())
        .filter(|description| !description.is_empty())
        .ok_or_else(|| SkillError::MissingDescription {
            path: path.to_path_buf(),
        })?;
    let description_len = description.chars().count();
    if description_len > DESCRIPTION_LIMIT {
        return Err(SkillError::DescriptionTooLong {
            path: path.to_path_buf(),
            len: description_len,
        });
    }

    let instructions = rest[body_start..].trim().to_owned();
    if instructions.is_empty() {
        return Err(SkillError::EmptyInstructions {
            path: path.to_path_buf(),
        });
    }

    Ok(Skill {
        name,
        description,
        instructions,
        directory: path.parent().unwrap_or(path).to_path_buf(),
        source,
    })
}

fn valid_name(name: &str) -> bool {
    let len = name.chars().count();
    len > 0
        && len <= NAME_LIMIT
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("acpilot-skills-{}-{id}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("temp dir");
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().expect("parent")).expect("parent");
            fs::write(&path, contents).expect("write");
            path
        }

        fn skill(&self, relative: &str) -> PathBuf {
            self.write(
                relative,
                "---\nname: pdf\ndescription: Read PDFs.\n---\n\nUse the tool.\n",
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_workspace_skill_loads_with_frontmatter_and_instructions() {
        let fixture = Fixture::new("workspace");
        fixture.write(
            ".acpilot/skills/pdf/SKILL.md",
            "---\nname: pdf\ndescription: Read PDFs.\nlicense: MIT\nmetadata:\n  owner: docs\n---\n\nUse the tool.\n",
        );

        let skills = discover(&fixture.root, &[]).expect("discover");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "pdf");
        assert_eq!(skills[0].description, "Read PDFs.");
        assert_eq!(skills[0].instructions, "Use the tool.");
        assert_eq!(skills[0].source, SkillSource::Workspace);
    }

    #[test]
    fn an_extension_skill_is_found() {
        let workspace = Fixture::new("ext-workspace");
        let extension = Fixture::new("ext-pack");
        fixture_skill(&extension, "skills/pdf/SKILL.md", "pdf", "Extension PDFs.");

        let skills =
            discover(&workspace.root, std::slice::from_ref(&extension.root)).expect("discover");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].source, SkillSource::Extension);
        assert_eq!(skills[0].description, "Extension PDFs.");
    }

    #[test]
    fn a_workspace_skill_overrides_an_extension_with_the_same_name() {
        let workspace = Fixture::new("override-workspace");
        let extension = Fixture::new("override-pack");
        fixture_skill(&extension, "skills/pdf/SKILL.md", "pdf", "Extension PDFs.");
        fixture_skill(
            &workspace,
            ".acpilot/skills/pdf/SKILL.md",
            "pdf",
            "Workspace PDFs.",
        );

        let skills =
            discover(&workspace.root, std::slice::from_ref(&extension.root)).expect("discover");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].source, SkillSource::Workspace);
        assert_eq!(skills[0].description, "Workspace PDFs.");
    }

    #[test]
    fn two_extensions_with_one_skill_name_are_an_error() {
        let workspace = Fixture::new("duplicate-workspace");
        let first = Fixture::new("duplicate-a");
        let second = Fixture::new("duplicate-b");
        fixture_skill(&first, "skills/pdf/SKILL.md", "pdf", "First.");
        fixture_skill(&second, "skills/pdf/SKILL.md", "pdf", "Second.");

        // Directory names must match the skill name, so two separate extension
        // roots are the way one name can appear twice in the same tier.
        let error = discover(&workspace.root, &[first.root.clone(), second.root.clone()])
            .expect_err("duplicate");
        assert!(matches!(error, SkillError::Duplicate { .. }), "{error}");
    }

    #[test]
    fn the_directory_must_match_the_name() {
        let fixture = Fixture::new("mismatch");
        fixture.skill(".acpilot/skills/pdf/SKILL.md");
        let error = parse_skill(
            "---\nname: other\ndescription: X.\n---\nDo it.\n",
            &fixture.root.join(".acpilot/skills/pdf/SKILL.md"),
            SkillSource::Workspace,
        )
        .expect_err("mismatch");
        assert!(matches!(error, SkillError::NameMismatch { .. }), "{error}");
    }

    #[test]
    fn invalid_names_are_rejected() {
        let path = PathBuf::from("/tmp/Pdf/SKILL.md");
        let error = parse_skill(
            "---\nname: Pdf\ndescription: X.\n---\nDo it.\n",
            &path,
            SkillSource::Workspace,
        )
        .expect_err("uppercase");
        assert!(matches!(error, SkillError::InvalidName { .. }), "{error}");
    }

    #[test]
    fn a_missing_description_is_rejected() {
        let path = PathBuf::from("/tmp/pdf/SKILL.md");
        let error = parse_skill(
            "---\nname: pdf\n---\nDo it.\n",
            &path,
            SkillSource::Workspace,
        )
        .expect_err("no description");
        assert!(
            matches!(error, SkillError::MissingDescription { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_long_description_is_rejected() {
        let path = PathBuf::from("/tmp/pdf/SKILL.md");
        let contents = format!(
            "---\nname: pdf\ndescription: {}\n---\nDo it.\n",
            "x".repeat(DESCRIPTION_LIMIT + 1)
        );
        let error = parse_skill(&contents, &path, SkillSource::Workspace).expect_err("long");
        assert!(
            matches!(error, SkillError::DescriptionTooLong { .. }),
            "{error}"
        );
    }

    #[test]
    fn empty_instructions_are_rejected() {
        let path = PathBuf::from("/tmp/pdf/SKILL.md");
        let error = parse_skill(
            "---\nname: pdf\ndescription: X.\n---\n\n",
            &path,
            SkillSource::Workspace,
        )
        .expect_err("empty body");
        assert!(
            matches!(error, SkillError::EmptyInstructions { .. }),
            "{error}"
        );
    }

    #[test]
    fn unclosed_frontmatter_is_rejected() {
        let path = PathBuf::from("/tmp/pdf/SKILL.md");
        let error = parse_skill(
            "---\nname: pdf\ndescription: X.\n",
            &path,
            SkillSource::Workspace,
        )
        .expect_err("unclosed");
        assert!(
            matches!(error, SkillError::UnclosedFrontmatter { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_file_without_frontmatter_is_rejected() {
        let path = PathBuf::from("/tmp/pdf/SKILL.md");
        let error = parse_skill("Just instructions.\n", &path, SkillSource::Workspace)
            .expect_err("no frontmatter");
        assert!(
            matches!(error, SkillError::MissingFrontmatter { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_broken_skill_leaves_the_valid_ones_in_the_list() {
        let fixture = Fixture::new("broken");
        fixture_skill(
            &fixture,
            ".acpilot/skills/review/SKILL.md",
            "review",
            "Read.",
        );
        fixture.write(
            ".acpilot/skills/release/SKILL.md",
            "---\nname: release\n---\n\nWrite it.\n",
        );

        let discovered = discover_scoped_recovering(&fixture.root, None, &[]);
        assert_eq!(
            discovered
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["review"],
            "the file that parses still lists"
        );
        assert_eq!(discovered.problems.len(), 1, "the broken file is reported");
        assert!(
            matches!(
                discovered.problems[0],
                SkillError::MissingDescription { .. }
            ),
            "{:?}",
            discovered.problems[0]
        );
        assert!(
            discover(&fixture.root, &[]).is_err(),
            "strict discovery still refuses a scope with a broken file"
        );
    }

    #[test]
    fn missing_skill_directories_are_empty_not_an_error() {
        let workspace = Fixture::new("empty");
        let extension = Fixture::new("empty-pack");
        let skills =
            discover(&workspace.root, std::slice::from_ref(&extension.root)).expect("discover");
        assert!(skills.is_empty());
    }

    fn fixture_skill(fixture: &Fixture, relative: &str, name: &str, description: &str) {
        fixture.write(
            relative,
            &format!("---\nname: {name}\ndescription: {description}\n---\n\nDo it.\n"),
        );
    }
}
