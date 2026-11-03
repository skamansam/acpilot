For agents you can't configure much, the reliable lever is a short block in their AGENTS.md that:

1. names gstack's router file,
2. tells the agent which task opens which skill,
3. gives it your Rust commands, so gstack's test and release steps don't have to guess them.

gstack's own routing block (gstack-harness register generic --print) covers step 1 in one sentence. Here it is extended for Rust-first work with some web:

<!-- gstack:begin harness-routing -->
## gstack skills

gstack's engineering workflows are installed at /home/sam/workspace-other/gstack.
For a coding, review, test, plan or release task, read
/home/sam/workspace-other/gstack/SKILL.md, pick the one skill whose line matches,
then read that skill's SKILL.md and follow it step by step, starting with its
Preamble command. Load nothing else up front.
<!-- gstack:end harness-routing -->

### When to use which skill

- Bug, failing test, panic, or wrong output: `investigate` (root cause before any fix).
- Done with a change, before calling it finished: `verify`, then `review` on the diff.
- New feature or vague idea: `spec`; for a non-trivial design, `plan-eng-review` before writing code.
- Ready to open a PR: `ship`.
- Code touching auth, input parsing, `unsafe`, FFI, or network boundaries: `cso`.
- Web UI work: `qa` to exercise it in a real browser, `design-review` for visual polish,
  `benchmark` for page performance. Use the `browse` skill for all web browsing.
- Don't use skills for one-line edits or questions about the code.

### Rust checks (what `verify` and `ship` must run)

- Format: `cargo fmt --all -- --check`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Test: `cargo nextest run --workspace` (fall back to `cargo test --workspace`)
- Gate at workspace level; `-p <crate>` alone can pass where the workspace fails.

### Web checks

- In the web package: `npm ci`, then its `lint`, `typecheck` and `test` scripts.
  Never `npm install` unless the task is changing dependencies.

Why it's written this way:

- Absolute paths. Some agents don't expand ~.
- "Starting with its Preamble command." gstack's own measurements found that agents outside Claude usually skip each skill's setup step (one run in 29 for OpenClaw and Hermes). Without that step, the skill loses its session setup, which handles questions, consent and upgrade checks. Saying it explicitly is the cheapest fix.
- The Rust commands. I checked the skill files: only ship (and qa, briefly) mention cargo. verify, health and test-audit would have to infer your checks, so the block spells them out. Edit them to fit each repo, e.g. a cargo ci-* alias or just check.
- The "don't use skills" line. gstack skills are long. Without it, a loosely controlled agent may run a whole workflow for a typo fix.
- The gstack:begin/end markers. They keep the block easy to find and swap later, and gstack's tooling uses the same markers for the block it can write itself.

Where to put it:

- Every project: a global rules file, if the agent reads one. Freebuff and Devin both read AGENTS.md; I haven't confirmed whether either has a user-wide AGENTS.md.
- A specific repo: the repo's AGENTS.md. gstack can write its 3-line routing block there for you with ~/workspace-other/gstack/bin/gstack-harness register generic --context <repo>/AGENTS.md, and track it for clean removal. Paste the routing-hints and checks sections under it yourself.

If an agent ignores skills entirely, a smaller fallback is gstack's 1.8 KB digest, ~/workspace-other/gstack/agents-digest/gstack-AGENTS.md. It holds only gstack's working principles (reuse before building, fix root causes, and so on), with no workflows. Paste it into AGENTS.md, and re-copy it after each gstack upgrade.
