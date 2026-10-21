# 0001 — Record architecture decisions

- **Status:** accepted
- **Date:** 2026-10-20
- **Deciders:** project maintainers

## Context

Acpilot has several decisions that are expensive to reverse and cheap to argue
about twice: how many processes there are, whether file contents cross the HTTP
API, how remote hosts are reached, which GUI framework carries the text surface,
how profiles isolate credentials. Some of these have already been made with
research behind them (`README.md` §5, `TODO.md` open decisions `D1`–`D13`), and
the reasoning lives in a 500-line README that will keep growing.

A README describes what the system is. It is a poor place to record *why* a
particular road was not taken, and it cannot say "this was true in October 2026
and here is what would have to change".

## Decision

Record every hard-to-reverse decision as a numbered ADR in `docs/adr/`, in the
Nygard format: context, decision, consequences, alternatives considered.

Rules:

1. One decision per file, numbered sequentially: `NNNN-short-title.md`.
2. Accepted ADRs are immutable. To change one, write a new ADR that supersedes it
   and update the old one's status line to `superseded by NNNN`.
3. An ADR that resolves an open question cites its `D<n>` identifier from
   `TODO.md`, and the `TODO.md` entry links back.
4. Status is one of `proposed`, `accepted`, `superseded by NNNN`, `rejected`.
5. An ADR without consequences is a wish. Say what gets harder.

## Consequences

- More writing before some changes. Accepted: the alternative is re-litigating
  the process model in a PR comment six months from now.
- The README stays descriptive; rationale moves here, and `README.md` links to the
  ADR instead of carrying the argument inline.

## Alternatives considered

- **Keep rationale in the README.** Rejected: it mixes normative description with
  history, and there is no way to mark a paragraph as settled or stale.
- **Rely on commit messages.** Rejected: decisions are not tied to one commit, and
  nobody reads `git log` to understand a design.
- **A wiki.** Rejected: drifts from the code, no review, no history.

## Open decisions to record

These are tracked as `D1`–`D13` in `TODO.md`; each gets an ADR when it is settled.

| ADR | Decision |
| --- | --- |
| 0002 | Local models and the model lab (P4.5) — **recorded** |
| 0003 | Extensions are data-first (D18) — **recorded** |
| 0004 | Daemon-centric process model: one daemon, thin clients (D4, D7) |
| 0005 | The API is a control plane, not a keystroke pipe (D2) |
| 0006 | `Host` trait for local/SSH/WSL, with an uploaded helper (P8, P9) |
| 0007 | Profiles: isolation model and storage layout (D10, D11, D13) |
| 0008 | LSP in the daemon, negotiated position encodings (D8, D9, D12) |
| 0009 | GUI framework: gpui, pinned, with an iced fallback (D1) |
| 0010 | Daemon auth for non-loopback exposure (D5) |
| 0011 | Agent manifest licensing so other editors can adopt it (D6) |
