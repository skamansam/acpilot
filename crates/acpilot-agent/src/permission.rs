//! The permission broker: who may do what, and who said so.
//!
//! Every question an agent raises — a file write outside the workspace, a tool
//! call, a shell command — is answered here, and every answer is written down.
//! The two halves matter equally: a broker that decides silently is a
//! convenience, and a log with no broker is a rumour.
//!
//! # Three answers, not two
//!
//! [`Decision::Allow`] and [`Decision::Deny`] are what they sound like.
//! [`Decision::Ask`] does not mean "refused" — it means the request is published
//! as a [`PermissionRequest`] and the caller waits for [`Broker::answer`]. That
//! wait has a deadline ([`ASK_TIMEOUT`]) and two rules that keep it from becoming
//! a hang:
//!
//! - with no subscriber, the request is refused at once, because an approval
//!   nobody can give is not worth waiting for;
//! - with a subscriber that never answers, the deadline refuses it.
//!
//! Both are audited with the reason, so "the agent was blocked for five minutes
//! and then denied" is a line in the log rather than a mystery.
//!
//! # Rules are ordered, and the first match wins
//!
//! A rule constrains any of agent, tool and path; an unset constraint matches
//! everything. They are evaluated in file order, so a narrow allow written above
//! a broad deny does what it says. That is the one precedence rule this engine
//! has: no scoring, no specificity contest. "Which rule fired" is recorded on the
//! audit line, which is the answer to every later argument about it.
//!
//! Globs are anchored and path-shaped: `*` never crosses a `/`, `**` does, `?` is
//! one character that is not a `/`. Patterns are matched against the *canonical*
//! path the caller resolved on the host, so `workspace/../etc` cannot arrive as
//! something a pattern thinks is inside.
//!
//! # Defaults
//!
//! The shipped policy is the one the product documents: files inside the
//! workspace are read and written, a shell or an unrecognised tool is asked
//! about, and anything outside the workspace is asked about too — which is the
//! shadow-write guard, since a request nobody is listening for is refused. A
//! rule is how a workspace says "this directory, this tool, this agent"; the
//! defaults are what happens when nobody has said anything.
//!
//! A write inside the workspace is allowed rather than asked about because P2
//! has no interface to ask with. A policy that asks by default arrives with the
//! first client that can answer, as `[policy] default = "ask"` in a profile.
//!
//! # Allow always
//!
//! [`PermissionOption::AllowAlways`] records the *shape* of the request that was
//! approved — kind, tool and path — for the rest of the session, so a client does
//! not ask the same question four times for four lines of one file. It is stored
//! per session, never per process: the next conversation starts with no memory of
//! this one's exceptions.
//!
//! # Audit
//!
//! [`AuditEntry`] is the row: what was asked, what was decided, who decided it
//! (`user`, `policy:<rule-id>`, `default`, `session`, `timeout` or `unattended`)
//! and when. The log lives in memory here because P2 has no database; P3-3's
//! store appends these entries to the `audit` table with the same columns, and
//! the transcript (P2-8) is what survives a restart until then.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use acpilot_proto::permission::{
    PermissionKind, PermissionOption, PermissionRequest, PermissionRequestId,
};
use acpilot_proto::session::SessionId;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, oneshot};

/// How long an asked request waits for a person before it is refused.
///
/// Minutes, not seconds: somebody may be reading a diff. The deadline exists so
/// that going home does not wedge an agent for the rest of the night.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(300);

/// How many announcements a subscriber may fall behind on.
///
/// The same reasoning as [`EVENT_CAPACITY`](crate::session::EVENT_CAPACITY): a
/// stalled client should miss announcements, not grow the queue. A client that
/// connects late reads [`Broker::pending`] instead.
pub const ANNOUNCE_CAPACITY: usize = 16;

/// What the policy says about a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Carry it out without asking.
    Allow,
    /// Publish it and wait for an answer.
    Ask,
    /// Refuse it.
    Deny,
}

impl Decision {
    /// The spelling a config file uses.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    /// Parse the spelling a config file uses.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "allow" => Some(Self::Allow),
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a [`Pattern`] folds case, which it must on Windows and must not
/// elsewhere.
///
/// A Windows path is case-insensitive and a POSIX one is not; folding both would
/// let a rule match `/Work/Secret` on Linux, and folding neither would make a rule
/// written on Windows useless on the machine it was written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    /// Compare exactly.
    Sensitive,
    /// Compare ASCII-case-insensitively.
    Insensitive,
}

/// A path-or-name glob.
///
/// Patterns are matched against a whole string: `/work/src/*.rs` matches
/// `/work/src/main.rs` and nothing else, not `/work/src/deep/main.rs` and not
/// `/work/src/main.rs.bak`. Separators are normalised to `/` on both sides, so
/// the same rule file works on Windows and on POSIX.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    text: String,
    case: Case,
}

impl Pattern {
    /// A pattern with this platform's case rule.
    ///
    /// The platform question is answered by the host layer, which is the only
    /// place in the workspace allowed to branch on the operating system; this
    /// module reads the answer rather than asking again.
    #[must_use]
    pub fn new(glob: &str) -> Self {
        let case = if acpilot_host::CASE_INSENSITIVE_PATHS {
            Case::Insensitive
        } else {
            Case::Sensitive
        };
        Self::with_case(glob, case)
    }

    /// A pattern with an explicit case rule. Tests use this; so does a caller
    /// that knows the host it is writing a rule for.
    #[must_use]
    pub fn with_case(glob: &str, case: Case) -> Self {
        Self {
            text: glob.replace('\\', "/"),
            case,
        }
    }

    /// The pattern as written, with separators normalised.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether `text` matches the whole pattern.
    #[must_use]
    pub fn matches(&self, text: &str) -> bool {
        let text = text.replace('\\', "/");
        match self.case {
            Case::Sensitive => walk(&chars(&self.text), 0, &chars(&text), 0),
            Case::Insensitive => walk(
                &chars(&self.text.to_lowercase()),
                0,
                &chars(&text.to_lowercase()),
                0,
            ),
        }
    }
}

fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

/// The matcher itself. `*` stays inside a segment, `**` crosses separators, `?`
/// is one non-separator character.
///
/// Recursive rather than compiled to an automaton: rules are written by people
/// and are tens of characters long, so the backtracking is cheaper than the code
/// that would avoid it.
fn walk(pattern: &[char], mut pi: usize, text: &[char], mut ti: usize) -> bool {
    while pi < pattern.len() {
        match pattern[pi] {
            '*' if pattern.get(pi + 1) == Some(&'*') => {
                // `***` is `**`: collapsing keeps a typo from meaning something
                // subtly different.
                while pattern.get(pi) == Some(&'*') {
                    pi += 1;
                }
                if walk(pattern, pi, text, ti) {
                    return true;
                }
                // Any run, separators included.
                return (ti..text.len()).any(|end| walk(pattern, pi, text, end + 1));
            }
            '*' => {
                if walk(pattern, pi + 1, text, ti) {
                    return true;
                }
                // Any run inside the current segment.
                while ti < text.len() {
                    if text[ti] == '/' {
                        return false;
                    }
                    ti += 1;
                    if walk(pattern, pi + 1, text, ti) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if ti >= text.len() || text[ti] == '/' {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            literal => {
                if ti >= text.len() || text[ti] != literal {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == text.len()
}

/// One line of policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Stable name, recorded on the audit line and shown in `acpilot doctor`.
    pub id: String,
    /// What a match means.
    pub decision: Decision,
    /// Agent id this applies to; `None` is every agent.
    pub agent: Option<Pattern>,
    /// Tool label this applies to; `None` is every tool.
    pub tool: Option<Pattern>,
    /// Canonical path this applies to; `None` is every path.
    pub path: Option<Pattern>,
}

impl Rule {
    /// A rule with no constraints yet: as written, it would match everything.
    #[must_use]
    pub fn new(id: impl Into<String>, decision: Decision) -> Self {
        Self {
            id: id.into(),
            decision,
            agent: None,
            tool: None,
            path: None,
        }
    }

    /// Restrict the rule to one agent id (glob).
    #[must_use]
    pub fn agent(mut self, glob: &str) -> Self {
        self.agent = Some(Pattern::new(glob));
        self
    }

    /// Restrict the rule to one tool label (glob).
    #[must_use]
    pub fn tool(mut self, glob: &str) -> Self {
        self.tool = Some(Pattern::new(glob));
        self
    }

    /// Restrict the rule to one canonical path (glob).
    #[must_use]
    pub fn path(mut self, glob: &str) -> Self {
        self.path = Some(Pattern::new(glob));
        self
    }

    /// Whether every constraint the rule sets matches this request.
    #[must_use]
    pub fn matches(&self, subject: &Subject<'_>) -> bool {
        let agent = match &self.agent {
            Some(pattern) => subject.agent.is_some_and(|agent| pattern.matches(agent)),
            None => true,
        };
        let tool = match &self.tool {
            Some(pattern) => subject.tool.is_some_and(|tool| pattern.matches(tool)),
            None => true,
        };
        let path = match &self.path {
            Some(pattern) => subject
                .path
                .is_some_and(|path| pattern.matches(&normalised(path))),
            None => true,
        };
        agent && tool && path
    }
}

/// A canonical path as a pattern sees it: separators are `/`, always.
fn normalised(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// One request, as the policy sees it.
#[derive(Debug, Clone)]
pub struct Subject<'a> {
    /// What kind of thing is being asked for.
    pub kind: PermissionKind,
    /// Agent id, when the caller knows it.
    pub agent: Option<&'a str>,
    /// Tool label a `tool` glob matches against.
    pub tool: Option<&'a str>,
    /// Canonical path, when the request concerns one.
    pub path: Option<&'a Path>,
    /// One line, safe to show in a notification.
    pub title: String,
    /// What a person needs to decide: a diff, a command line, a payload.
    pub detail: String,
}

impl<'a> Subject<'a> {
    /// A request of `kind` with a title.
    #[must_use]
    pub fn new(kind: PermissionKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            agent: None,
            tool: None,
            path: None,
            title: title.into(),
            detail: String::new(),
        }
    }

    /// Name the agent that asked.
    #[must_use]
    pub fn agent(mut self, agent: &'a str) -> Self {
        self.agent = Some(agent);
        self
    }

    /// Name the tool, as a `tool` glob would spell it.
    #[must_use]
    pub fn tool(mut self, tool: &'a str) -> Self {
        self.tool = Some(tool);
        self
    }

    /// The canonical path the request concerns.
    #[must_use]
    pub fn path(mut self, path: &'a Path) -> Self {
        self.path = Some(path);
        self
    }

    /// The detail a person needs: a diff, a command, a payload.
    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

/// What the policy decided before anyone was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruling {
    /// The decision.
    pub decision: Decision,
    /// The rule that made it, if a rule did.
    pub rule: Option<String>,
}

/// The rule set, and the defaults for requests no rule mentions.
///
/// Reads, writes and everything else are separate defaults on purpose: "ask about
/// every write" is a usable policy, and "ask about every read" is not.
#[derive(Debug, Clone)]
pub struct Policy {
    rules: Vec<Rule>,
    read: Decision,
    write: Decision,
    tool: Decision,
    outside: Decision,
}

impl Default for Policy {
    fn default() -> Self {
        Self::shipped()
    }
}

impl Policy {
    /// The policy the product documents.
    ///
    /// Read and write inside: allow. Shell or unknown tool inside: ask. Anything
    /// outside the workspace: ask, which refuses when nobody is listening. A rule
    /// overrides all of it.
    #[must_use]
    pub fn shipped() -> Self {
        Self {
            rules: Vec::new(),
            read: Decision::Allow,
            write: Decision::Allow,
            tool: Decision::Ask,
            outside: Decision::Ask,
        }
    }

    /// The shipped defaults with these rules, tried in order.
    #[must_use]
    pub fn with_rules(rules: Vec<Rule>) -> Self {
        Self {
            rules,
            ..Self::shipped()
        }
    }

    /// Replace the default for reads inside the workspace.
    #[must_use]
    pub fn with_read(mut self, decision: Decision) -> Self {
        self.read = decision;
        self
    }

    /// Replace the default for writes inside the workspace.
    #[must_use]
    pub fn with_write(mut self, decision: Decision) -> Self {
        self.write = decision;
        self
    }

    /// Replace the default for shell and tool requests inside the workspace.
    #[must_use]
    pub fn with_tool(mut self, decision: Decision) -> Self {
        self.tool = decision;
        self
    }

    /// Replace the default for anything outside the workspace.
    ///
    /// `Ask` is the shipped answer, so an outside path is written only after
    /// somebody approves it; `Deny` is what a workspace sets when it means
    /// "nothing out there, ever". A specific outside path is better said with a
    /// rule that names it.
    #[must_use]
    pub fn with_outside(mut self, decision: Decision) -> Self {
        self.outside = decision;
        self
    }

    /// The rules, in the order they are tried.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Read the `[policy]` table of a profile config.
    ///
    /// The whole file is parsed, not one section of it: a real `config.toml`
    /// carries `schema_version` and its `[lsp.*]` tables next to this one, and
    /// the keys this engine does not know are ignored rather than refused.
    ///
    /// The shape is the one `reference/config.md` documents:
    ///
    /// ```toml
    /// [policy]
    /// default = "ask"
    /// outside_workspace = "deny"
    ///
    /// [[policy.rules]]
    /// id = "allow-format"
    /// decision = "allow"
    /// tool = "execute"
    /// path = "/work/**"
    /// ```
    ///
    /// Unknown keys are an error, the same as everywhere else in the
    /// configuration: a misspelled `outside_workspace` would otherwise silently
    /// mean "deny", which reads exactly like a policy that works.
    pub fn from_toml(text: &str) -> Result<Self, PolicyError> {
        let file: PolicyFile = toml::from_str(text)?;
        let Some(section) = file.policy else {
            return Ok(Self::shipped());
        };
        let default = parse_decision("policy.default", section.default.as_deref(), Decision::Ask)?;
        let outside = parse_decision(
            "policy.outside_workspace",
            section.outside_workspace.as_deref(),
            Decision::Deny,
        )?;

        let mut rules = Vec::with_capacity(section.rules.len());
        for (index, rule) in section.rules.into_iter().enumerate() {
            let field = format!("policy.rules[{index}].decision");
            rules.push(Rule {
                id: rule.id,
                decision: parse_decision(&field, Some(&rule.decision), Decision::Ask)?,
                agent: rule.agent.as_deref().map(Pattern::new),
                tool: rule.tool.as_deref().map(Pattern::new),
                path: rule.path.as_deref().map(Pattern::new),
            });
        }

        Ok(Self {
            rules,
            read: Decision::Allow,
            write: default,
            tool: default,
            outside,
        })
    }

    /// The decision for a request, and the rule that made it.
    ///
    /// `inside_workspace` is the caller's answer for a canonical path; a request
    /// with no path counts as inside, because the alternative is that every tool
    /// call is "outside" and denied.
    #[must_use]
    pub fn decide(&self, subject: &Subject<'_>, inside_workspace: bool) -> Ruling {
        if let Some(rule) = self.rules.iter().find(|rule| rule.matches(subject)) {
            return Ruling {
                decision: rule.decision,
                rule: Some(rule.id.clone()),
            };
        }
        let decision = if inside_workspace {
            match subject.kind {
                PermissionKind::FileRead => self.read,
                PermissionKind::FileWrite => self.write,
                PermissionKind::Terminal | PermissionKind::ToolCall | PermissionKind::Other => {
                    self.tool
                }
            }
        } else {
            self.outside
        };
        Ruling {
            decision,
            rule: None,
        }
    }
}

fn parse_decision(
    field: &str,
    value: Option<&str>,
    default: Decision,
) -> Result<Decision, PolicyError> {
    match value {
        None => Ok(default),
        Some(text) => Decision::parse(text).ok_or_else(|| PolicyError::Decision {
            field: field.to_owned(),
            value: text.to_owned(),
        }),
    }
}

/// What went wrong reading a policy.
#[derive(Debug, Error)]
pub enum PolicyError {
    /// The TOML did not parse, or a key was not recognised.
    #[error("the policy is not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),

    /// A decision was spelled in a way the engine does not accept.
    #[error("`{field}` is `{value}`; expected allow, ask or deny")]
    Decision {
        /// Which key was wrong.
        field: String,
        /// What it said.
        value: String,
    },
}

/// The part of a profile `config.toml` this engine reads.
///
/// Unknown top-level keys are ignored on purpose: the whole profile config is
/// handed to this parser, so `schema_version` and the `[lsp.*]` tables beside
/// `[policy]` must not make it fail. A file with no `[policy]` table at all is
/// the shipped policy rather than an error — a profile that says nothing about
/// permissions has not asked for none.
#[derive(Debug, Deserialize)]
struct PolicyFile {
    #[serde(default)]
    policy: Option<PolicySection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicySection {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    outside_workspace: Option<String>,
    #[serde(default)]
    rules: Vec<RuleSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSection {
    id: String,
    decision: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// Who or what made a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecidedBy {
    /// A person answered the request.
    User,
    /// An earlier `allow always` for this session covered it.
    Session,
    /// A rule matched; the id is which one.
    Policy(String),
    /// No rule matched and the built-in default decided.
    Default,
    /// Nobody answered inside [`ASK_TIMEOUT`].
    Timeout,
    /// Nothing was listening, so the question could not be put to anyone.
    Unattended,
}

impl DecidedBy {
    /// The spelling the audit line and the API use.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::User => "user",
            Self::Session => "session",
            Self::Policy(_) => "policy",
            Self::Default => "default",
            Self::Timeout => "timeout",
            Self::Unattended => "unattended",
        }
    }
}

impl fmt::Display for DecidedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Policy(id) => write!(f, "policy:{id}"),
            other => f.write_str(other.as_str()),
        }
    }
}

/// The broker's answer to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The option that was applied.
    pub option: PermissionOption,
    /// Who decided it, for the notice and the audit line.
    pub decided_by: DecidedBy,
}

impl Answer {
    /// Whether the request may proceed.
    #[must_use]
    pub fn allowed(&self) -> bool {
        self.option != PermissionOption::Deny
    }
}

/// One row of the audit table.
///
/// The fields are the columns P3-3's `audit` table will have: a caller that
/// wants to know why an agent could write a file reads this, not a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Monotonic within a broker, starting at 1.
    pub seq: u64,
    /// RFC 3339 timestamp of the decision.
    pub at: String,
    /// Session that asked.
    pub session: SessionId,
    /// Agent that asked, when known.
    pub agent: Option<String>,
    /// Classification of the request.
    pub kind: PermissionKind,
    /// Tool label, when the request had one.
    pub tool: Option<String>,
    /// Canonical path, when the request concerned one.
    pub path: Option<PathBuf>,
    /// The request as it was shown.
    pub title: String,
    /// What was decided.
    pub option: PermissionOption,
    /// Who decided it.
    pub decided_by: DecidedBy,
}

/// The policy, the pending questions, and the log.
///
/// One per process, shared by every session: `GET /v1/permissions` has to see one
/// list, and two brokers would mean two answers to "is anybody waiting?".
#[derive(Debug)]
pub struct Broker {
    policy: Policy,
    workspace: PathBuf,
    timeout: Option<Duration>,
    next: AtomicU64,
    state: Mutex<State>,
    announce: broadcast::Sender<PermissionRequest>,
}

#[derive(Debug, Default)]
struct State {
    pending: Vec<(PermissionRequest, oneshot::Sender<PermissionOption>)>,
    granted: Vec<(SessionId, SessionGrant)>,
    audit: Vec<AuditEntry>,
}

/// The shape of an `allow always`, kept per session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionGrant {
    kind: PermissionKind,
    tool: Option<String>,
    path: Option<PathBuf>,
}

impl SessionGrant {
    fn covers(&self, subject: &Subject<'_>) -> bool {
        self.kind == subject.kind
            && self
                .tool
                .as_deref()
                .is_none_or(|tool| Some(tool) == subject.tool)
            && self
                .path
                .as_deref()
                .is_none_or(|path| Some(path) == subject.path)
    }
}

impl Broker {
    /// A broker for `workspace`, which must already be canonical.
    ///
    /// Canonical because the policy's "inside or outside" question is answered by
    /// a prefix comparison, and an unresolved root would answer it wrongly for
    /// every path under a symlinked workspace.
    #[must_use]
    pub fn new(policy: Policy, workspace: impl Into<PathBuf>) -> Self {
        Self::with_timeout(policy, workspace, Some(ASK_TIMEOUT))
    }

    /// A broker with an explicit wait. `None` waits until the session ends,
    /// which is what a test proving that a call *blocks* needs.
    #[must_use]
    pub fn with_timeout(
        policy: Policy,
        workspace: impl Into<PathBuf>,
        timeout: Option<Duration>,
    ) -> Self {
        let (announce, _) = broadcast::channel(ANNOUNCE_CAPACITY);
        Self {
            policy,
            workspace: workspace.into(),
            timeout,
            next: AtomicU64::new(1),
            state: Mutex::new(State::default()),
            announce,
        }
    }

    /// The policy in force.
    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The workspace root every path is compared with.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Receive every request that needs an answer.
    pub fn subscribe(&self) -> broadcast::Receiver<PermissionRequest> {
        self.announce.subscribe()
    }

    /// The requests waiting right now, oldest first.
    #[must_use]
    pub fn pending(&self) -> Vec<PermissionRequest> {
        let state = self.lock();
        state
            .pending
            .iter()
            .map(|(request, _)| request.clone())
            .collect()
    }

    /// Answer one request. `false` if it is no longer pending.
    ///
    /// A decision is delivered once: the entry is removed before the answer is
    /// sent, so answering the same id twice is a `false`, not a second effect.
    pub fn answer(&self, id: &str, option: PermissionOption) -> bool {
        let entry = {
            let mut state = self.lock();
            let index = state
                .pending
                .iter()
                .position(|(request, _)| request.id == id);
            index.map(|index| state.pending.remove(index))
        };
        match entry {
            Some((_, decision)) => decision.send(option).is_ok(),
            None => false,
        }
    }

    /// Every decision so far, oldest first.
    #[must_use]
    pub fn audit(&self) -> Vec<AuditEntry> {
        self.lock().audit.clone()
    }

    /// The audit rows for one session.
    #[must_use]
    pub fn audit_for(&self, session: &str) -> Vec<AuditEntry> {
        self.lock()
            .audit
            .iter()
            .filter(|entry| entry.session == session)
            .cloned()
            .collect()
    }

    /// Answer one request, asking a person if the policy says to.
    ///
    /// This never returns an error: a refusal is an answer. Everything that went
    /// into it — the rule, the missing subscriber, the timeout, the person's
    /// click — is on the audit line that comes out.
    pub async fn ask(&self, session: &str, subject: Subject<'_>) -> Answer {
        if self.covered_by_a_session_grant(session, &subject) {
            return self.record(
                session,
                &subject,
                PermissionOption::AllowOnce,
                DecidedBy::Session,
            );
        }

        let inside = subject
            .path
            .is_none_or(|path| path.starts_with(&self.workspace));
        let ruling = self.policy.decide(&subject, inside);
        let by = || match &ruling.rule {
            Some(id) => DecidedBy::Policy(id.clone()),
            None => DecidedBy::Default,
        };
        match ruling.decision {
            Decision::Allow => self.record(session, &subject, PermissionOption::AllowOnce, by()),
            Decision::Deny => self.record(session, &subject, PermissionOption::Deny, by()),
            Decision::Ask => self.ask_a_person(session, &subject).await,
        }
    }

    fn covered_by_a_session_grant(&self, session: &str, subject: &Subject<'_>) -> bool {
        let state = self.lock();
        state
            .granted
            .iter()
            .any(|(owner, grant)| owner == session && grant.covers(subject))
    }

    async fn ask_a_person(&self, session: &str, subject: &Subject<'_>) -> Answer {
        if self.announce.receiver_count() == 0 {
            tracing::warn!(
                title = %subject.title,
                session,
                "a request needs a person and nothing is listening; refusing"
            );
            return self.record(
                session,
                subject,
                PermissionOption::Deny,
                DecidedBy::Unattended,
            );
        }

        let request = PermissionRequest {
            id: self.next_id(),
            session_id: session.to_owned(),
            kind: subject.kind,
            title: subject.title.clone(),
            detail: subject.detail.clone(),
            options: vec![
                PermissionOption::AllowOnce,
                PermissionOption::AllowAlways,
                PermissionOption::Deny,
            ],
            created_at: jiff::Timestamp::now().to_string(),
        };
        let (decision, answer) = oneshot::channel();
        {
            let mut state = self.lock();
            state.pending.push((request.clone(), decision));
        }

        if self.announce.send(request.clone()).is_err() {
            self.forget(&request.id);
            return self.record(
                session,
                subject,
                PermissionOption::Deny,
                DecidedBy::Unattended,
            );
        }

        let answered = match self.timeout {
            Some(limit) => match tokio::time::timeout(limit, answer).await {
                Ok(Ok(option)) => Some(option),
                Ok(Err(_)) | Err(_) => None,
            },
            None => answer.await.ok(),
        };

        match answered {
            Some(PermissionOption::AllowAlways) => {
                self.remember(session, subject);
                self.record(
                    session,
                    subject,
                    PermissionOption::AllowAlways,
                    DecidedBy::User,
                )
            }
            Some(option) => self.record(session, subject, option, DecidedBy::User),
            None => {
                // Approve and deny remove the entry; a timeout leaves it
                // behind, and a stale question is a dialog that writes a file
                // nobody is waiting for.
                self.forget(&request.id);
                self.record(session, subject, PermissionOption::Deny, DecidedBy::Timeout)
            }
        }
    }

    /// Record an `allow always` for the shape of *this* request, not for the
    /// rule that asked: approving one file must not approve every file in its
    /// directory, and approving an execute must not approve a delete.
    fn remember(&self, session: &str, subject: &Subject<'_>) {
        let mut state = self.lock();
        state.granted.push((
            session.to_owned(),
            SessionGrant {
                kind: subject.kind,
                tool: subject.tool.map(str::to_owned),
                path: subject.path.map(Path::to_path_buf),
            },
        ));
    }

    fn record(
        &self,
        session: &str,
        subject: &Subject<'_>,
        option: PermissionOption,
        decided_by: DecidedBy,
    ) -> Answer {
        let mut state = self.lock();
        let seq = state.audit.len() as u64 + 1;
        state.audit.push(AuditEntry {
            seq,
            at: jiff::Timestamp::now().to_string(),
            session: session.to_owned(),
            agent: subject.agent.map(str::to_owned),
            kind: subject.kind,
            tool: subject.tool.map(str::to_owned),
            path: subject.path.map(Path::to_path_buf),
            title: subject.title.clone(),
            option,
            decided_by: decided_by.clone(),
        });
        Answer { option, decided_by }
    }

    fn next_id(&self) -> PermissionRequestId {
        format!("perm-{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn forget(&self, id: &str) {
        let mut state = self.lock();
        state.pending.retain(|(request, _)| request.id != id);
    }

    /// One lock for pending, grants and audit: the three are read together far
    /// more often than they are written, and a poisoned lock here means another
    /// thread panicked mid-decision, which must not turn into "allow".
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    const PATIENCE: Duration = Duration::from_secs(5);
    const WORKSPACE: &str = "/work/repo";

    fn glob(pattern: &str) -> Pattern {
        Pattern::with_case(pattern, Case::Sensitive)
    }

    fn subject<'a>(kind: PermissionKind, path: Option<&'a Path>) -> Subject<'a> {
        let mut subject = Subject::new(kind, "a request");
        if let Some(path) = path {
            subject = subject.path(path);
        }
        subject
    }

    fn inside() -> PathBuf {
        PathBuf::from("/work/repo/src/main.rs")
    }

    fn outside() -> PathBuf {
        PathBuf::from("/etc/hosts")
    }

    #[test]
    fn a_star_stays_inside_one_segment() {
        let pattern = glob("/work/repo/*.rs");
        assert!(pattern.matches("/work/repo/main.rs"));
        assert!(!pattern.matches("/work/repo/src/main.rs"));
        assert!(!pattern.matches("/work/repo/a.rs.bak"));
        assert!(glob("/work/*/main.rs").matches("/work/repo/main.rs"));
    }

    #[test]
    fn a_double_star_crosses_separators() {
        assert!(glob("/work/**").matches("/work/repo/src/main.rs"));
        assert!(glob("/work/**").matches("/work/repo"));
        assert!(glob("**/*.rs").matches("/work/main.rs"));
        assert!(glob("**/*.rs").matches("/work/a/b/main.rs"));
        assert!(!glob("**/*.rs").matches("/work/main.toml"));
    }

    #[test]
    fn a_question_mark_is_one_character_and_not_a_separator() {
        assert!(glob("/work/repo/?.rs").matches("/work/repo/a.rs"));
        assert!(!glob("/work/repo/?.rs").matches("/work/repo/ab.rs"));
        assert!(!glob("/work/repo?rs").matches("/work/repo/rs"));
    }

    #[test]
    fn patterns_are_anchored_and_match_whole_strings() {
        assert!(!glob("main.rs").matches("/work/repo/main.rs"));
        assert!(!glob("/work/repo/main.rs").matches("main.rs"));
        assert!(glob("/work/repo/main.rs").matches("/work/repo/main.rs"));
    }

    #[test]
    fn windows_case_folding_is_opt_in_and_explicit() {
        let folded = Pattern::with_case("/Work/Repo/*.RS", Case::Insensitive);
        assert!(folded.matches("/work/repo/main.rs"));
        let exact = Pattern::with_case("/Work/Repo/*.RS", Case::Sensitive);
        assert!(!exact.matches("/work/repo/main.rs"));
    }

    #[test]
    fn the_first_matching_rule_wins_and_its_id_is_reported() {
        let policy = Policy::with_rules(vec![
            Rule::new("allow-one-file", Decision::Allow).path("/work/repo/src/gen/*"),
            Rule::new("deny-the-rest", Decision::Deny),
        ]);
        let path = PathBuf::from("/work/repo/src/gen/tables.rs");
        let ruled = policy.decide(&subject(PermissionKind::FileWrite, Some(&path)), true);
        assert_eq!(ruled.decision, Decision::Allow);
        assert_eq!(ruled.rule.as_deref(), Some("allow-one-file"));

        let other = PathBuf::from("/work/repo/src/main.rs");
        let ruled = policy.decide(&subject(PermissionKind::FileWrite, Some(&other)), true);
        assert_eq!(ruled.decision, Decision::Deny);
        assert_eq!(ruled.rule.as_deref(), Some("deny-the-rest"));
    }

    #[test]
    fn a_rule_with_several_constraints_needs_all_of_them() {
        let policy = Policy::with_rules(vec![
            Rule::new("allow-claude-fmt", Decision::Allow)
                .agent("claude*")
                .tool("execute")
                .path("/work/repo/**"),
        ]);
        let path = PathBuf::from("/work/repo/src/main.rs");
        let mut request = subject(PermissionKind::Terminal, Some(&path));
        request = request.agent("claude-code").tool("execute");
        assert_eq!(
            policy.decide(&request, true).decision,
            Decision::Allow,
            "every constraint matches"
        );

        let mut wrong_agent = subject(PermissionKind::Terminal, Some(&path));
        wrong_agent = wrong_agent.agent("other").tool("execute");
        assert_eq!(
            policy.decide(&wrong_agent, true).decision,
            Decision::Ask,
            "the agent is part of the rule"
        );

        let mut wrong_tool = subject(PermissionKind::Terminal, Some(&path));
        wrong_tool = wrong_tool.agent("claude-code").tool("fetch");
        assert_eq!(policy.decide(&wrong_tool, true).decision, Decision::Ask);
    }

    #[test]
    fn the_shipped_defaults_are_the_documented_ones() {
        let policy = Policy::shipped();
        let inside = inside();
        let outside = outside();

        assert_eq!(
            policy
                .decide(&subject(PermissionKind::FileRead, Some(&inside)), true)
                .decision,
            Decision::Allow,
            "a read inside the workspace needs no signature"
        );
        assert_eq!(
            policy
                .decide(&subject(PermissionKind::FileWrite, Some(&inside)), true)
                .decision,
            Decision::Allow,
            "a write inside the workspace needs no signature either: P2 has no\n             interface to ask with, and an edit is what an agent is for"
        );
        for kind in [
            PermissionKind::Terminal,
            PermissionKind::ToolCall,
            PermissionKind::Other,
        ] {
            assert_eq!(
                policy.decide(&subject(kind, Some(&inside)), true).decision,
                Decision::Ask,
                "{kind:?} is a question"
            );
            assert_eq!(
                policy
                    .decide(&subject(kind, Some(&outside)), false)
                    .decision,
                Decision::Ask,
                "{kind:?} outside the workspace is put to a person"
            );
        }
        assert_eq!(
            policy
                .decide(&subject(PermissionKind::FileWrite, Some(&outside)), false)
                .decision,
            Decision::Ask,
            "an outside write waits for approval rather than happening"
        );
    }

    #[test]
    fn a_rule_can_deny_a_read_the_default_would_allow() {
        let policy = Policy::with_rules(vec![
            Rule::new("no-secrets", Decision::Deny).path("**/.env"),
        ]);
        let path = PathBuf::from("/work/repo/.env");
        let ruled = policy.decide(&subject(PermissionKind::FileRead, Some(&path)), true);
        assert_eq!(ruled.decision, Decision::Deny);
        assert_eq!(ruled.rule.as_deref(), Some("no-secrets"));
    }

    #[test]
    fn the_documented_config_shape_parses() {
        let policy = Policy::from_toml(
            r#"
[policy]
default = "allow"
outside_workspace = "ask"

[[policy.rules]]
id = "no-secrets"
decision = "deny"
tool = "fs/read_text_file"
path = "**/.env"
"#,
        )
        .expect("the documented shape parses");

        let path = PathBuf::from("/work/repo/.env");
        let mut request = subject(PermissionKind::FileRead, Some(&path));
        request = request.tool("fs/read_text_file");
        assert_eq!(policy.decide(&request, true).decision, Decision::Deny);

        let other = inside();
        assert_eq!(
            policy
                .decide(&subject(PermissionKind::FileWrite, Some(&other)), true)
                .decision,
            Decision::Allow,
            "`default` governs writes and tools"
        );
        assert_eq!(
            policy
                .decide(&subject(PermissionKind::FileWrite, Some(&outside())), false)
                .decision,
            Decision::Ask,
            "`outside_workspace` overrides the default for paths outside"
        );
    }

    #[test]
    fn a_config_without_a_policy_table_keeps_the_shipped_policy() {
        let policy = Policy::from_toml(
            "schema_version = 1\n\n[lsp.rust]\ncommand = \"rust-analyzer\"\nargs = []\n",
        )
        .expect("a profile that says nothing about permissions still parses");

        let inside = inside();
        assert_eq!(
            policy
                .decide(&subject(PermissionKind::FileWrite, Some(&inside)), true)
                .decision,
            Decision::Allow,
            "the file said nothing, so the shipped default applies"
        );
        assert!(policy.rules().is_empty());
    }

    #[test]
    fn a_misspelled_decision_is_an_error_rather_than_a_default() {
        let error = Policy::from_toml(
            r#"
[policy]
default = "askk"
"#,
        )
        .expect_err("a typo is refused");
        assert!(
            error.to_string().contains("askk"),
            "the message quotes what it found: {error}"
        );

        let error = Policy::from_toml(
            r#"
[policy]
outside = "deny"
"#,
        )
        .expect_err("an unknown key is refused");
        assert!(matches!(error, PolicyError::Toml(_)), "{error:?}");
    }

    fn broker(policy: Policy) -> Broker {
        Broker::new(policy, WORKSPACE)
    }

    #[tokio::test]
    async fn an_allowed_request_is_audited_with_the_rule_that_allowed_it() {
        let broker = broker(Policy::with_rules(vec![
            Rule::new("allow-fmt", Decision::Allow).tool("execute"),
        ]));
        let path = inside();
        let mut request = subject(PermissionKind::Terminal, Some(&path));
        request = request.tool("execute");

        let answer = broker.ask("session-1", request).await;
        assert!(answer.allowed());
        assert_eq!(answer.decided_by, DecidedBy::Policy("allow-fmt".to_owned()));
        let audit = broker.audit();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].option, PermissionOption::AllowOnce);
        assert_eq!(audit[0].session, "session-1");
        assert_eq!(
            audit[0].decided_by,
            DecidedBy::Policy("allow-fmt".to_owned())
        );
        assert_eq!(audit[0].seq, 1);
    }

    #[tokio::test]
    async fn a_denied_request_never_becomes_pending() {
        let broker = broker(Policy::with_rules(vec![
            Rule::new("no-outside-writes", Decision::Deny).path("/etc/**"),
        ]));
        let mut announced = broker.subscribe();

        let answer = broker
            .ask(
                "session-1",
                subject(PermissionKind::FileWrite, Some(&outside())),
            )
            .await;
        assert!(!answer.allowed());
        assert_eq!(
            answer.decided_by,
            DecidedBy::Policy("no-outside-writes".to_owned())
        );
        assert!(broker.pending().is_empty());
        assert!(
            timeout(Duration::from_millis(50), announced.recv())
                .await
                .is_err(),
            "a request the policy denies is not published"
        );
    }

    /// Wait for the announcement, then answer it, while the ask is in flight.
    async fn next_request(
        announced: &mut broadcast::Receiver<PermissionRequest>,
    ) -> PermissionRequest {
        timeout(PATIENCE, announced.recv())
            .await
            .expect("a question is announced")
            .expect("the stream is open")
    }

    fn asking(broker: &Arc<Broker>, path: PathBuf) -> tokio::task::JoinHandle<Answer> {
        let broker = Arc::clone(broker);
        tokio::spawn(async move {
            broker
                .ask("session-1", subject(PermissionKind::FileWrite, Some(&path)))
                .await
        })
    }

    /// The shipped policy allows a write inside the workspace, so the tests that
    /// exercise the ask flow install a policy that asks: the flow, not the
    /// default, is what they are about.
    fn asking_broker() -> Broker {
        Broker::new(Policy::shipped().with_write(Decision::Ask), WORKSPACE)
    }

    #[tokio::test]
    async fn a_write_inside_the_workspace_needs_no_person() {
        let broker = broker(Policy::shipped());
        let answer = broker
            .ask(
                "session-1",
                subject(PermissionKind::FileWrite, Some(&inside())),
            )
            .await;
        assert!(answer.allowed());
        assert_eq!(answer.decided_by, DecidedBy::Default);
        assert!(broker.pending().is_empty());
    }

    #[tokio::test]
    async fn an_asked_request_is_published_with_three_options_and_answers_once() {
        let broker = Arc::new(asking_broker());
        let mut announced = broker.subscribe();
        let waiting = asking(&broker, inside());

        let request = next_request(&mut announced).await;
        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.kind, PermissionKind::FileWrite);
        assert_eq!(
            request.options,
            vec![
                PermissionOption::AllowOnce,
                PermissionOption::AllowAlways,
                PermissionOption::Deny
            ]
        );
        assert_eq!(request.detail, "");
        assert_eq!(broker.pending(), vec![request.clone()]);

        assert!(broker.answer(&request.id, PermissionOption::AllowOnce));
        let answer = waiting.await.expect("the ask finishes");
        assert!(answer.allowed());
        assert_eq!(answer.decided_by, DecidedBy::User);
        assert!(
            !broker.answer(&request.id, PermissionOption::AllowOnce),
            "a decision is delivered once"
        );
        assert!(broker.pending().is_empty());
        assert_eq!(broker.audit()[0].option, PermissionOption::AllowOnce);
    }

    #[tokio::test]
    async fn allow_always_answers_the_next_identical_request_without_asking() {
        let broker = Arc::new(asking_broker());
        let mut announced = broker.subscribe();
        let path = inside();
        let waiting = asking(&broker, path.clone());

        let request = next_request(&mut announced).await;
        assert!(broker.answer(&request.id, PermissionOption::AllowAlways));
        let first = waiting.await.expect("the ask finishes");
        assert!(first.allowed());
        assert_eq!(first.decided_by, DecidedBy::User);

        let second = timeout(
            PATIENCE,
            broker.ask("session-1", subject(PermissionKind::FileWrite, Some(&path))),
        )
        .await
        .expect("the second request does not wait for a person");
        assert!(second.allowed());
        assert_eq!(second.decided_by, DecidedBy::Session);
        assert!(broker.pending().is_empty());

        // A different path is a different question, and a denial of it does not
        // un-approve the first.
        let other = PathBuf::from("/work/repo/other.txt");
        let waiting = asking(&broker, other);
        let request = next_request(&mut announced).await;
        assert!(broker.answer(&request.id, PermissionOption::Deny));
        assert!(!waiting.await.expect("the ask finishes").allowed());
    }

    #[tokio::test]
    async fn a_request_with_nobody_listening_is_refused_at_once_and_audited() {
        let broker = asking_broker();
        let path = inside();
        let answer = timeout(
            PATIENCE,
            broker.ask("session-1", subject(PermissionKind::FileWrite, Some(&path))),
        )
        .await
        .expect("the refusal is immediate");
        assert!(!answer.allowed());
        assert_eq!(answer.decided_by, DecidedBy::Unattended);
        assert_eq!(broker.audit()[0].decided_by, DecidedBy::Unattended);
        assert!(broker.pending().is_empty());
    }

    #[tokio::test]
    async fn an_unanswered_request_expires_and_the_answer_is_no_longer_taken() {
        let broker = Broker::with_timeout(
            Policy::shipped().with_write(Decision::Ask),
            WORKSPACE,
            Some(Duration::from_millis(50)),
        );
        let mut announced = broker.subscribe();
        let path = inside();

        let answer = timeout(
            PATIENCE,
            broker.ask("session-1", subject(PermissionKind::FileWrite, Some(&path))),
        )
        .await
        .expect("the deadline ends the wait");
        let request = announced.try_recv().expect("the question was announced");
        assert!(!answer.allowed());
        assert_eq!(answer.decided_by, DecidedBy::Timeout);
        assert!(
            !broker.answer(&request.id, PermissionOption::AllowOnce),
            "an expired question is no longer answerable"
        );
        assert!(broker.pending().is_empty());
    }

    #[tokio::test]
    async fn the_audit_is_per_session_and_in_order() {
        let broker = broker(Policy::shipped());
        let path = inside();
        let read = subject(PermissionKind::FileRead, Some(&path));
        let _ = broker.ask("session-1", read).await;
        let write = subject(PermissionKind::FileWrite, Some(&path));
        let _ = broker.ask("session-2", write).await;

        let first = broker.audit_for("session-1");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].kind, PermissionKind::FileRead);
        let second = broker.audit_for("session-2");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].kind, PermissionKind::FileWrite);
        let all = broker.audit();
        assert_eq!(
            all.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
}
