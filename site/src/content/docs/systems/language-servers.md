---
title: Language servers
description: Owned by the daemon, so every cockpit shows the same diagnosis.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P7

Language servers live in the daemon, not in the UI. That one decision is why a
`rust-analyzer` indexing your SSH box can feed the TUI, the desktop app, and a
browser tab at the same time — and why "go to definition" means the same thing
everywhere.

## Why not per-client servers

Four clients each spawning their own server means four indexes, four RAM bills,
and four subtly different answers. The daemon already owns the workspace and the
host; it is the only component that can run one server in the right place.

## Position encodings are negotiated, not assumed

LSP defaults to UTF-16 offsets. The editor buffer is UTF-8 bytes. Applying a
UTF-16 offset to a UTF-8 rope works perfectly until someone types an emoji or a
CJK character, and then it is off by one column forever.

So the daemon advertises `general.positionEncodings = ["utf-8", "utf-16"]`,
records what each server chooses, and converts at the boundary with proptests on
emoji, CJK, and combining marks.

<Aside type="note" title="This is a solved problem that keeps being unsolved">
Every editor has shipped this bug at least once. The only defence is a test file
with the characters that break it.
</Aside>

## Security posture

A language server is an arbitrary binary with access to your repository and your
network. Therefore:

- **Trust-pinned.** Hashed on first use, prompt once, re-prompt if the binary
  changes.
- **No auto-download in v0.1.** Detect what is installed; do not fetch vendored
  servers from the internet on the user's behalf.
- **Restricted to the workspace root**, like agents.
- **Timeouts everywhere**, plus `$/cancelRequest`, plus a crash-loop cap. A
  wedged server costs a status-line warning, not a frozen editor.

## Agents can read diagnostics

This is the part that is more than an editor feature: the daemon that brokers an
agent's edits also holds the compiler's opinion of them. An agent can ask for
diagnostics as a tool, which turns "did my change compile?" from a guess into a
lookup.

## Status

Not started. P0-7 proves the framing and the encoding conversion against
`rust-analyzer`, `typescript-language-server`, and `pyright` before P7 builds on
them.
