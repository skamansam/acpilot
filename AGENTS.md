# Notes for coding agents

This is the short version, written for an agent that is about to change this
repository. `CONTRIBUTING.md` is the authority on the gate and the conventions,
and `README.md` explains what the product is. When this file and those disagree,
they win.

## Read these first

| File | Why |
| --- | --- |
| `TODO.md` | The plan. Every task has a code (`P2-7`, `XT-1`) that is also a commit reference and an issue title. Check the box state before you start: a task can be half-done with its `Open:` list at the bottom. |
| `CONTRIBUTING.md` | The gate, the test rules, the platform rule, commits and releases. |
| `README.md` §3 and §6 | The process model and the crate list. Read §3 before touching anything that crosses a process boundary. |
| `docs/adr/` | Decisions that bind this codebase. An ADR is superseded, never edited. |
| `docs/notes/` | Measurements with numbers. A change that contradicts one needs a new number, not an adjective. |

## The gate

```bash
cargo xtask check          # what CI runs: fmt, clippy -D warnings, nextest, the probe, the web UI
cargo xtask check --no-web # the Rust-only loop, when Node is not the point
```

- `cargo nextest run --workspace --no-fail-fast` is the test command, not
  `cargo test --workspace`. Focused runs are `cargo test -p <crate> --test <name>`
  or `cargo test -p <crate> <filter>`.
- `prek install` writes the commit hook, which runs the same commands. Do not use
  `git commit --no-verify`; the same failure is waiting in CI.
- A wire change also moves `crates/acpilot-proto/tests/snapshots/openapi_snapshot__openapi.snap`:
  regenerate with `INSTA_UPDATE=always cargo test -p acpilot-proto` and review the
  diff. `crates/acpilot-proto/tests/wire_contract.rs` is a contract, not coverage —
  do not weaken it.
- Anything under `web/` needs `cargo xtask web-build`, and the regenerated
  `crates/acpilot-web/assets/index.html` goes in the same commit.
- Anything that changes a pane, a key or a theme token needs `cargo xtask showcase`,
  and the regenerated `crates/acpilot-showcase` JSON goes in the same commit.
- `cargo xtask check` also cross-checks `acpilot-host` for Windows and macOS, and
  runs the P0-5 probe, which is not a workspace member.

## Where code goes

| Change | Crate |
| --- | --- |
| Wire shape, a field, an enum variant | `acpilot-proto` |
| A domain rule, an error class | `acpilot-core` |
| Agent behaviour, ACP session, PTY adapter, permissions | `acpilot-agent` |
| Filesystem, processes, PTYs, SSH, WSL | `acpilot-host` |
| Routes, SSE, sessions, persistence, language servers | `acpilotd` |
| Anything a person sees | `acpilot-cli`, `acpilot-tui`, `acpilot-gui`, `web/` |
| The contract a client reads | `crates/acpilot-client` |

Domain logic does not go in a UI crate, and UI concerns do not go in the daemon.
If the TUI cannot do something, the GUI must not be able to either.

## Rules that will bite you

- **`cfg` naming an operating system only under a crate's `src/platform/`.** One
  file per platform, same signature. `crates/acpilot-host/tests/platform_boundary.rs`
  fails the build otherwise.
- **No `unsafe`.** It is forbidden at the workspace level; a job object on Windows
  is not a reason to add it.
- **A new dependency needs a sentence** on why the existing ones do not do the job,
  plus a clean `cargo deny`.
- **`agents.toml` is schemad.** The published schema is
  `site/public/schema/agents-v1.json`, generated from the loader's own types and
  frozen once shipped. A new *field* is an `agents-v2.json` decision, not a patch.
- **The UI copy is not the contract.** `docs/api.md`, `site/src/content/docs/` and
  `docs/notes/` describe behaviour; if you change the behaviour and leave them
  saying the old thing, the change is not finished.
- **A probe answers one question and is deletable.** Its numbers go in
  `docs/notes/<topic>.md`; see CONTRIBUTING §Probes.
- **Tests are not the place to make a claim true.** A failing test is the finding.

## Working a ticket

1. Read the task in `TODO.md` and every document it cites. A task's code and its
   `Open:` list are usually more current than the prose above it.
2. Write the test that says the behaviour exists, then make it pass. Unit tests
   sit in `#[cfg(test)]` beside the code; anything crossing a process, file or wire
   boundary gets an integration suite in `crates/<crate>/tests/`.
3. Update `TODO.md`: tick the box, add a `Result:` paragraph saying what shipped
   and — in an `Open:` line — what deliberately did not. A tick with no result is a
   claim nobody can audit.
4. Update the documents the change invalidates. That includes the TUI and desktop
   pages under `site/src/content/docs/` when a key or a pane changes.
5. Run the gate, commit, push, and watch CI to green:
   ```bash
   cargo xtask check
   git commit -m "area: what changed, in one line

   Why it changed, and the failure it prevents."
   git push origin <branch>
   gh run watch --exit-status
   ```
   The user's standing rule is **commit and push after every finished ticket**, and
   do not report a ticket done while its run is red.

## Commands worth keeping

```bash
cargo xtask run tui -- .              # daemon + TUI, building both
cargo run -p acpilot-cli -- tui --demo # the whole shell on seeded data, no daemon
cargo xtask run daemon                # the backend alone, for a debugger
cargo xtask dev                       # daemon + Vite over the web UI
cargo xtask gui-check                 # the desktop shell's lint, link and tests
curl -s --unix-socket "$XDG_RUNTIME_DIR/acpilot/acpilot.sock" http://localhost/v1/health
```

LSP tests and the PTY-agent tests that need a real tool are `#[ignore]`d on
purpose: `cargo test -p acpilot-lsp --test session -- --ignored` runs them on a
machine with `rust-analyzer`. Do not un-ignore them for a CI runner that does not
promise the tool.

## Writing the code

- `rustfmt` is authoritative. Import order is std, then external, then crate.
- Comments explain intent and the failure they prevent. A comment that restates the
  next line is noise; the one that explains why the obvious approach was wrong is
  the most valuable thing in the file.
- Match the surrounding module's tone and its habit of justifying a refusal.
- Prefer editing an existing module to adding a parallel one.
