# Contributing

Acpilot is a Rust workspace with one daemon and several clients. The rules below
exist because this codebase has four UIs and two transports, and the fastest way
to make it unmaintainable is to let one of them grow its own copy of the logic.

## Before you start

```bash
cargo install cargo-nextest --locked   # once; CI installs the same tool
cargo xtask check        # fmt + clippy -D warnings + nextest: what CI runs
curl -s localhost:7777/v1/health   # after `cargo run -p acpilotd`
```

`cargo xtask check` runs `cargo nextest run --workspace --no-fail-fast`, the
same command as CI. `cargo test --workspace` still works for a single crate or a
`--test` filter, but it is not the gate: nextest isolates tests in separate
processes and skips doctests, and the local check should fail for the same
reasons CI does.

The toolchain is pinned in `rust-toolchain.toml`; the MSRV floor is 1.95 and is
chosen by our dependencies, not by taste. If an upgrade raises it, that is a
reviewed decision (README §5.9), so say so in the PR.

## Where code goes

| Change | Lives in |
| --- | --- |
| Wire shape, new field, enum variant | `acpilot-proto` (and the OpenAPI snapshot will fail until you update it) |
| Domain rule, error class | `acpilot-core` |
| Agent behaviour, ACP, PTY adapters | `acpilot-agent` |
| Filesystem/process access, SSH, WSL | `acpilot-host` |
| Routes, SSE, persistence | `acpilotd` |
| Anything a user sees | `acpilot-cli`, `acpilot-tui`, `acpilot-gui`, `web/` |

Two things that do not belong anywhere: domain logic in a UI crate, and UI
concerns in the daemon. If the TUI cannot do something, the GUI should not be able
to either.

## Tests

Unit tests sit in `#[cfg(test)]` modules next to the code; anything that crosses a
process, a file, or a wire boundary gets an integration suite in
`crates/<crate>/tests/`. `cargo nextest run --workspace` runs both, locally and in
CI; `cargo xtask check` invokes exactly that command.

Two suites are contracts rather than coverage, and should not be weakened:

- `acpilot-proto/tests/openapi_snapshot.rs` — regenerate deliberately with
  `INSTA_UPDATE=always cargo test -p acpilot-proto` and review the diff.
- `acpilot-proto/tests/wire_contract.rs` — every schema, parsed and re-serialised.
  A failure here means the wire moved, not that the test is stale.

## Commits

One idea per commit, imperative subject, scoped by area:

```
proto: add terminal resize event
acpilotd: reject stale ETags on file writes
tui: keep cursor visible on fold
docs: record the GUI framework decision
```

Body explains *why*, not *what* — the diff already says what. Reference the task
ID from `TODO.md` (`P3-5`, `XT-1`) when a commit closes one.

## Pull requests

- `cargo xtask check` green locally; CI runs the same commands on Linux, macOS
  and Windows, so a platform-specific shortcut will be caught.
- New dependency → a sentence on why the existing ones do not do the job, plus a
  `cargo deny` clean run. The version policy is "latest stable" (README §5.9).
- Wire change → the OpenAPI snapshot diff is part of the review, by design.
- Performance-sensitive change → numbers, not adjectives, against the budgets in
  `TODO.md` §Performance budgets.

## Decisions

Architecture decisions are recorded as ADRs in `docs/adr/`, one file per decision,
numbered, immutable once accepted (supersede rather than edit). The open questions
are listed as `D1`–`D13` in `TODO.md`; an ADR resolves one by number.

If a change is hard to reverse — a process model, a protocol shape, a storage
layout — write the ADR first. Code is cheap to change; a promise to users is not.

## Style

`rustfmt` is authoritative. Import grouping (std, then external, then crate) is a
review convention rather than a `rustfmt.toml` setting, because those options are
nightly-only and this project builds on stable (see the note in `rustfmt.toml`).

Comments explain intent. A comment that restates the next line is noise; a comment
that explains why the obvious approach was wrong is the most valuable thing in the
file.
