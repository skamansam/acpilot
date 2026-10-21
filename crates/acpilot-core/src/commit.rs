//! Commit-message prompts and validation.
//!
//! The prompt is data, not a string buried in a UI: the same default is shown in
//! the editor, used by the daemon when a recipe writes a message, and asserted by
//! tests. A user can replace it, and the output is parsed rather than trusted, so
//! a model that ignores the format is caught before `git commit` sees it.
//!
//! The default asks for Conventional Commits with the gitmoji inside the
//! description (`feat(proto): ✨ add stats`). Putting the emoji after the colon
//! keeps the type first, which is what commitlint and changelog tools parse.

use thiserror::Error;

/// Conventional Commit types accepted in a generated message.
pub const TYPES: &[&str] = &[
    "build", "chore", "ci", "docs", "feat", "fix", "perf", "refactor", "revert", "style", "test",
];

/// One gitmoji and the Conventional Commit type it belongs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gitmoji {
    /// Commit type this emoji matches.
    pub kind: &'static str,
    /// The character(s) to place at the start of the description.
    pub emoji: &'static str,
    /// Shortcode used by gitmoji tooling.
    pub shortcode: &'static str,
    /// What it means, for the prompt table.
    pub description: &'static str,
}

/// The default gitmoji set, one per Conventional Commit type.
pub const GITMOJIS: &[Gitmoji] = &[
    Gitmoji {
        kind: "feat",
        emoji: "✨",
        shortcode: ":sparkles:",
        description: "a new feature",
    },
    Gitmoji {
        kind: "fix",
        emoji: "🐛",
        shortcode: ":bug:",
        description: "a bug fix",
    },
    Gitmoji {
        kind: "docs",
        emoji: "📝",
        shortcode: ":memo:",
        description: "documentation",
    },
    Gitmoji {
        kind: "style",
        emoji: "💄",
        shortcode: ":lipstick:",
        description: "formatting or presentation",
    },
    Gitmoji {
        kind: "refactor",
        emoji: "♻️",
        shortcode: ":recycle:",
        description: "a refactor with no behaviour change",
    },
    Gitmoji {
        kind: "perf",
        emoji: "⚡️",
        shortcode: ":zap:",
        description: "a performance improvement",
    },
    Gitmoji {
        kind: "test",
        emoji: "✅",
        shortcode: ":white_check_mark:",
        description: "tests",
    },
    Gitmoji {
        kind: "build",
        emoji: "👷",
        shortcode: ":construction_worker:",
        description: "the build system or dependencies",
    },
    Gitmoji {
        kind: "ci",
        emoji: "💚",
        shortcode: ":green_heart:",
        description: "continuous integration",
    },
    Gitmoji {
        kind: "chore",
        emoji: "🔧",
        shortcode: ":wrench:",
        description: "tooling or housekeeping",
    },
    Gitmoji {
        kind: "revert",
        emoji: "⏪",
        shortcode: ":rewind:",
        description: "reverting a previous commit",
    },
];

/// The editable default prompt.
///
/// Placeholders are replaced by [`render_prompt`]. The gitmoji table is generated
/// from [`GITMOJIS`] rather than written twice, so adding one updates the prompt.
pub const DEFAULT_PROMPT: &str = r#"You write exactly one git commit message for the staged diff below.

Format: Conventional Commits 1.0.0, `type(scope): description`.
- `type` is one of: build, chore, ci, docs, feat, fix, perf, refactor, revert, style, test.
- `scope` is optional, lowercase, and names the area (for example `proto`, `cli`, `tui`).
- Start the description with the matching gitmoji, a space, then a lowercase imperative verb.
- Subject at most 72 characters. Add a body after one blank line only when it helps; wrap at 72.
- Put `!` before the colon for a breaking change, with a `BREAKING CHANGE:` footer.

Gitmoji to use:
{{gitmojis}}

Rules:
- Describe only changes the diff supports. Do not invent files, issues, or tests.
- No code fences, no preamble, no attribution, no `Co-Authored-By`.
- Output the message and nothing else.

Branch: {{branch}}

Recent subjects, for tone (do not repeat them):
{{recent}}

Staged files:
{{files}}

Diff:
{{diff}}
"#;

/// Everything the prompt needs, already gathered by the caller.
#[derive(Debug, Clone, Copy)]
pub struct CommitContext<'a> {
    /// Current branch, or `(detached)`.
    pub branch: &'a str,
    /// Recent subject lines, newest first.
    pub recent_subjects: &'a [String],
    /// Paths staged for this commit.
    pub staged_files: &'a [String],
    /// The staged patch.
    pub diff: &'a str,
}

/// Fill the prompt template.
///
/// Unknown placeholders are left alone rather than replaced with an empty string:
/// a typo in a user template should be visible, not silently swallow context.
#[must_use]
pub fn render_prompt(template: &str, context: &CommitContext<'_>) -> String {
    let gitmojis = GITMOJIS
        .iter()
        .map(|gitmoji| {
            format!(
                "- {} {} `{}` — {}",
                gitmoji.emoji, gitmoji.kind, gitmoji.shortcode, gitmoji.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let recent = if context.recent_subjects.is_empty() {
        "(none)".to_owned()
    } else {
        context
            .recent_subjects
            .iter()
            .map(|subject| format!("- {subject}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let files = if context.staged_files.is_empty() {
        "(none staged)".to_owned()
    } else {
        context
            .staged_files
            .iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    template
        .replace("{{gitmojis}}", &gitmojis)
        .replace("{{branch}}", context.branch)
        .replace("{{recent}}", &recent)
        .replace("{{files}}", &files)
        .replace("{{diff}}", context.diff)
}

/// A generated message that passed the format check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSuggestion {
    /// The first line, with type, optional scope, and emoji.
    pub subject: String,
    /// Everything after the blank line, if any.
    pub body: String,
    /// Conventional type.
    pub kind: String,
    /// Optional scope.
    pub scope: Option<String>,
    /// Whether the subject marked a breaking change.
    pub breaking: bool,
    /// The leading gitmoji, when the description starts with one.
    pub emoji: Option<String>,
}

/// Why a generated message was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CommitError {
    /// The model returned nothing but whitespace.
    #[error("the generated commit message is empty")]
    Empty,
    /// The header has no `type:` prefix.
    #[error("`{header}` is not `<type>(<scope>): <description>`")]
    MalformedHeader {
        /// The offending first line.
        header: String,
    },
    /// The type is not in the Conventional Commit set.
    #[error("`{kind}` is not a Conventional Commit type")]
    UnknownType {
        /// The type that was found.
        kind: String,
    },
    /// The description is empty.
    #[error("the commit subject has no description")]
    MissingDescription,
    /// The subject is longer than the accepted limit.
    #[error("the commit subject is {len} characters; the limit is {limit}")]
    SubjectTooLong {
        /// Measured length in characters.
        len: usize,
        /// Limit that was exceeded.
        limit: usize,
    },
    /// Body text appeared without the required blank separator.
    #[error("the commit body must be separated from the subject by a blank line")]
    BodyNeedsBlankLine,
}

/// Maximum subject length, in characters.
pub const SUBJECT_LIMIT: usize = 72;

/// Check and normalise a model's answer before it reaches `git commit`.
///
/// A surrounding code fence is stripped because models add one despite being
/// asked not to; everything else is a real failure. Being strict here is the
/// point: a malformed subject is easier to fix in the editor than in `git log`.
pub fn validate_suggestion(raw: &str) -> Result<CommitSuggestion, CommitError> {
    let text = strip_fence(raw).trim();
    if text.is_empty() {
        return Err(CommitError::Empty);
    }

    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default().trim();
    let rest: Vec<&str> = lines.collect();

    if let Some(first) = rest.first()
        && !first.trim().is_empty()
    {
        return Err(CommitError::BodyNeedsBlankLine);
    }

    let body = rest
        .iter()
        .skip_while(|line| line.trim().is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned();

    let (kind, scope, breaking, description) = parse_header(header)?;
    if description.is_empty() {
        return Err(CommitError::MissingDescription);
    }
    let len = header.chars().count();
    if len > SUBJECT_LIMIT {
        return Err(CommitError::SubjectTooLong {
            len,
            limit: SUBJECT_LIMIT,
        });
    }

    let emoji = description
        .split_whitespace()
        .next()
        .filter(|token| {
            token
                .chars()
                .next()
                .is_some_and(|first| !first.is_ascii_alphanumeric())
        })
        .map(str::to_owned);

    Ok(CommitSuggestion {
        subject: header.to_owned(),
        body,
        kind,
        scope,
        breaking,
        emoji,
    })
}

fn parse_header(header: &str) -> Result<(String, Option<String>, bool, String), CommitError> {
    let (prefix, description) =
        header
            .split_once(':')
            .ok_or_else(|| CommitError::MalformedHeader {
                header: header.to_owned(),
            })?;

    let prefix = prefix.trim();
    let breaking = prefix.ends_with('!');
    let prefix = prefix.strip_suffix('!').unwrap_or(prefix).trim();

    let (kind, scope) = if let Some(open) = prefix.find('(') {
        let close = prefix
            .strip_suffix(')')
            .and_then(|_| prefix.rfind(')'))
            .ok_or_else(|| CommitError::MalformedHeader {
                header: header.to_owned(),
            })?;
        let kind = prefix[..open].trim().to_owned();
        let scope = prefix[open + 1..close].trim().to_owned();
        if scope.is_empty() || scope.contains(char::is_whitespace) {
            return Err(CommitError::MalformedHeader {
                header: header.to_owned(),
            });
        }
        (kind, Some(scope))
    } else {
        (prefix.to_owned(), None)
    };

    if !TYPES.contains(&kind.as_str()) {
        return Err(CommitError::UnknownType { kind });
    }

    Ok((kind, scope, breaking, description.trim().to_owned()))
}

fn strip_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some(newline) = rest.find('\n') else {
        return trimmed;
    };
    let body = &rest[newline + 1..];
    body.strip_suffix("```").unwrap_or(body).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_prompt_asks_for_both_formats() {
        let context = CommitContext {
            branch: "main",
            recent_subjects: &["feat: ✨ add stats".to_owned()],
            staged_files: &["crates/acpilot-core/src/commit.rs".to_owned()],
            diff: "+hello\n",
        };
        let prompt = render_prompt(DEFAULT_PROMPT, &context);

        assert!(prompt.contains("Conventional Commits 1.0.0"));
        assert!(prompt.contains("gitmoji"));
        assert!(prompt.contains("✨ feat"));
        assert!(prompt.contains(":sparkles:"));
        assert!(prompt.contains("Branch: main"));
        assert!(prompt.contains("- feat: ✨ add stats"));
        assert!(prompt.contains("- crates/acpilot-core/src/commit.rs"));
        assert!(prompt.contains("+hello"));
        assert!(!prompt.contains("{{"));
    }

    #[test]
    fn an_empty_context_says_so_instead_of_leaving_placeholders() {
        let context = CommitContext {
            branch: "(detached)",
            recent_subjects: &[],
            staged_files: &[],
            diff: "",
        };
        let prompt = render_prompt(DEFAULT_PROMPT, &context);
        assert!(prompt.contains("(none)"));
        assert!(prompt.contains("(none staged)"));
    }

    #[test]
    fn a_well_formed_suggestion_parses() {
        let suggestion =
            validate_suggestion("feat(proto): ✨ add integration stats\n").expect("valid");
        assert_eq!(suggestion.kind, "feat");
        assert_eq!(suggestion.scope.as_deref(), Some("proto"));
        assert!(!suggestion.breaking);
        assert_eq!(suggestion.emoji.as_deref(), Some("✨"));
        assert_eq!(suggestion.body, "");
    }

    #[test]
    fn a_body_and_breaking_marker_are_preserved() {
        let suggestion = validate_suggestion(
            "feat(api)!: ✨ require a token\n\nThe bridge rejects unauthenticated drivers.\n\nBREAKING CHANGE: pairings now need a bearer token.\n",
        )
        .expect("valid");
        assert!(suggestion.breaking);
        assert!(suggestion.body.contains("bearer token"));
    }

    #[test]
    fn a_code_fence_is_stripped_because_models_add_one() {
        let suggestion =
            validate_suggestion("```text\nfix(cli): 🐛 keep exit code 3\n```\n").expect("valid");
        assert_eq!(suggestion.subject, "fix(cli): 🐛 keep exit code 3");
    }

    #[test]
    fn a_missing_type_is_rejected() {
        let error = validate_suggestion("add integration stats").expect_err("no type");
        assert!(matches!(error, CommitError::MalformedHeader { .. }));
    }

    #[test]
    fn an_unknown_type_is_rejected_with_its_name() {
        let error = validate_suggestion("feature: ✨ add stats").expect_err("bad type");
        assert_eq!(
            error,
            CommitError::UnknownType {
                kind: "feature".to_owned()
            }
        );
    }

    #[test]
    fn an_empty_description_is_rejected() {
        let error = validate_suggestion("feat(proto):  ").expect_err("empty");
        assert_eq!(error, CommitError::MissingDescription);
    }

    #[test]
    fn a_subject_over_the_limit_is_rejected() {
        let error = validate_suggestion(&format!("feat: ✨ {}", "x".repeat(80))).expect_err("long");
        assert!(matches!(error, CommitError::SubjectTooLong { .. }));
    }

    #[test]
    fn a_body_without_a_blank_line_is_rejected() {
        let error =
            validate_suggestion("feat: ✨ add stats\nbecause it is useful").expect_err("body");
        assert_eq!(error, CommitError::BodyNeedsBlankLine);
    }

    #[test]
    fn empty_output_is_rejected() {
        assert_eq!(
            validate_suggestion("   \n").expect_err("empty"),
            CommitError::Empty
        );
    }

    #[test]
    fn a_scope_cannot_contain_spaces() {
        let error = validate_suggestion("feat(two words): ✨ add").expect_err("scope");
        assert!(matches!(error, CommitError::MalformedHeader { .. }));
    }

    #[test]
    fn a_scope_is_optional() {
        let suggestion = validate_suggestion("chore: 🔧 refresh the lockfile").expect("valid");
        assert_eq!(suggestion.kind, "chore");
        assert_eq!(suggestion.scope, None);
    }
}
