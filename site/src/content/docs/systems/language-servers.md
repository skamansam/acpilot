---
title: Language servers
description: Owned by the daemon, so every cockpit shows the same diagnosis.
---

<span class="pill pill--hangar">hangar</span> &nbsp; Language intelligence phase (P7)

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

The first thing a server is used for is **references**. `POST /v1/files/references`
takes the file, the caret's offset and the buffer's own text alongside the name,
starts the server installed for that language — `rust-analyzer`,
`pyright-langserver` or `typescript-language-server`, detected on `PATH` — and asks
`textDocument/references`, having opened the buffer as the document the caret is in.
That is what makes an edit nobody has saved yet the text the answer is about. The list
is then **scoped** rather than spelled: two functions called `new` are two functions.
A language with no server falls back to a parse-tree search, which is a feature of the
daemon (`parse-references`, on by default) rather than a second client: a build
without it links no grammar at all. The `GET` form, a name and no buffer, is the
spelling search.

The rest of the engine arrives in the language intelligence phase (P7); the design
phase's P0-7 probe is done. Framing, negotiation, and the UTF-8/UTF-16 conversion are
in `acpilot-lsp`, tested over emoji, CJK, combining marks, and CRLF, and were proven
end-to-end against `rust-analyzer`, `typescript-language-server`, and `pyright`. The
numbers and the capability table live in the design phase's P0-7 notes; the short
version is that two of the three servers say nothing about encoding and mean UTF-16,
so the daemon negotiates and honours whatever the server answers.
