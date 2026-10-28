---
title: Flight status
description: What actually works today, and what is still in the hangar.
---

<span class="pill pill--boarding">boarding</span>

**Current altitude: 3,000 ft and climbing.** The workspace exists, the contract
tests pass, and nothing has flown a real agent yet. This page is the honest
instrument panel; if it says a thing works, it works in the repository today.

## What works right now

| Capability | State | Notes |
| --- | --- | --- |
| Workspace builds | <span class="pill pill--flying">flying</span> | `cargo xtask check` — fmt, clippy, tests, on Linux/macOS/Windows |
| Daemon binds and serves | <span class="pill pill--flying">flying</span> | `GET /v1/health`, `GET /v1/agents`, `/v1/permissions`, `GET /v1/events` (SSE), `/v1/sessions`, `/v1/terminals` + the attach WebSocket, `/v1/files` with `if_match`, `GET /v1/openapi.json`. Listens on a unix socket (`0700` directory, `0600` socket; TCP only with `--addr`), and `acpilot tui` starts it when nothing answers (`--no-spawn` to opt out) |
| API document | <span class="pill pill--flying">flying</span> | Snapshot-tested, so a wire change is a reviewable diff |
| Error contract | <span class="pill pill--flying">flying</span> | One error type → fixed HTTP statuses and exit codes |
| Swagger UI | <span class="pill pill--flying">flying</span> | `--features docs`, at `/docs` |
| CLI | <span class="pill pill--boarding">boarding</span> | `version`, `tui`, the filesystem `ext` verbs and `secret`; the rest is the command-line phase (P5) |
| TUI | <span class="pill pill--boarding">boarding</span> | A client of the daemon: session list, streaming transcript, permission card answered with `y`/`a`/`n`, and it starts the daemon when nothing answers. A terminal pane over the shells an agent opened, with scrollback read back by the wheel, find (smart-case) and copy over OSC 52; a menu bar with `Alt` mnemonics and the mouse; draggable dividers; `acpilot tui --demo` draws the whole shell on a seeded world. The editor pane draws a real document with syntax highlighting, a numbered gutter whose markers are parsed from the review's own patch, the current line, bracket matching, `/` find, go-to-line, and a click that lands the caret and a wheel that scrolls the view |
| Text model | <span class="pill pill--boarding">boarding</span> | `acpilot-text`, the crate every editor pane will edit through: a character-indexed rope that keeps a file's line endings and byte-order mark, selection sets with motions, edits as replacements, and an undo history that groups a typed run. `pane` adds the viewport arithmetic a renderer needs: which rows are on screen, what survives the horizontal scroll (tabs and double-width characters measured in cells), where the caret lands, and what `/` found. Tested with property tests (undo-then-redo is the identity, no caret leaves the text, no offset panics, no row wider than its viewport). The TUI's editor pane draws it |
| Syntax highlighting | <span class="pill pill--boarding">boarding</span> | `acpilot-text::highlight`: four grammars (Rust, Python, JSON, TOML) compiled in and loaded lazily, an incremental reparse per edit, and spans for the visible range only — resolved to the theme's syntax tokens, so a theme file needs no knowledge of tree-sitter. The TUI's editor pane paints them; the desktop and web panes are P10 and P11 |
| Themes | <span class="pill pill--boarding">boarding</span> | The token schema and the two built-in themes, applied by every surface: `theme` plus `theme_tui`/`theme_gui`/`theme_web` in the settings cascade, resolved by one `ThemeRegistry` — the TUI draws every pane from it, the desktop app takes its id from settings, and the daemon themes the page it serves. Saving a settings file recolours a running TUI, and the daemon reads `theme_web` per request, so the page follows on its next load; `prefs.changed` for clients that watch no file remains (P4-7) |
| Desktop, web | <span class="pill pill--hangar">hangar</span> | Crates exist so the layout is real: the desktop app opens a window with the shared theme tokens, a menu bar, the profile list and About; the daemon serves an embedded page carrying the theme's CSS variables. Panes and the editor are the desktop and web phases (P10, P11) |
| Agents, sessions, permissions | <span class="pill pill--boarding">boarding</span> | Engine phase (P2) — manifest registry, supervisor (stderr ring, restart, tree kill), sessions (initialize/new/load/prompt/cancel, numbered events), the agent-facing file callbacks (workspace-scoped reads, writes outside it held for approval), the permission broker (ordered rules, path globs, `allow always`, audit rows), and the API half: `GET /v1/agents` with `available`/`missing` from `PATH`, the pending list, the resolve route, and a stream that announces a new request. Terminals: create, list, and a WebSocket pane that replays the screen (P3-6), plus the `terminal/*` requests an agent makes, served by the daemon's own registry and ended with their session — and a pane over each of those, so a client can watch and type into the shell an agent opened. Every session appends its events and permission decisions to `session.jsonl` under the data directory, so a conversation outlives the process — but no route reads a stored one back yet |
| Language servers | <span class="pill pill--hangar">hangar</span> | Language intelligence phase (P7) |
| Profiles | <span class="pill pill--hangar">hangar</span> | Profiles phase (P4) |
| Local models (LM Studio) | <span class="pill pill--boarding">boarding</span> | Models phase (P4.5) — `acpilot-model` discovers models and probes capabilities against a mock server; no daemon route or CLI verb yet, so you cannot reach it from the app |
| The model lab | <span class="pill pill--hangar">hangar</span> | Models phase (P4.5) — recipes, runs, workbench |
| Integration stats | <span class="pill pill--hangar">hangar</span> | Contract only: `IntegrationStats` and the fill math are tested; no `/v1/stats` route and no UI yet |
| Git diff and tree | <span class="pill pill--hangar">hangar</span> | `acpilot-git` reads status, diffs, tree and log from the system `git`; no route or pane yet |
| Commit messages | <span class="pill pill--hangar">hangar</span> | Prompt and validator landed in `acpilot-core`; no generator route and no editor |
| Skills | <span class="pill pill--hangar">hangar</span> | `acpilot-skills` discovers `SKILL.md` from the workspace and extensions; no creator/editor yet |
| Remote drivers over ACP | <span class="pill pill--hangar">hangar</span> | Models phase (P4.5-10) — bridge, pairing, scopes. Off by default |
| Extensions (themes, presets, grammars) | <span class="pill pill--boarding">boarding</span> | Mod shop phase (P4.6) — loader, checksummed store, per-profile activation with precedence, TextMate theme import, a shared theme resolver all three surfaces use, and a curated marketplace list. Signed index and grammar loading remain |
| SSH / WSL hosts | <span class="pill pill--hangar">hangar</span> | Remote hosts phase (P8) / Windows phase (P9) |

## The flight plan

| Phase | What lands | State |
| --- | --- | --- |
| Design phase (P0) | Probes: ACP handshake, PTY agent, gpui text benchmark, SSH, WSL, LSP | in progress — ACP handshake, PTY agent, LSP round-trips and the gpui benchmark done |
| Foundation phase (P1) | Workspace skeleton, error model, CI, OpenAPI snapshot | **done** |
| Engine phase (P2) | ACP engine, sessions, permission broker (headless, mock agent) | in progress — registry, supervisor, sessions, the filesystem callbacks and the permission broker done |
| Service phase (P3) | Daemon API: SSE, files, terminals, SQLite | in progress — `GET /v1/agents`, the pending-permission list and its resolve route, `GET /v1/events` announcing new requests, `acpilotd --profile <dir>` putting a profile's `[policy]` and `agents.toml` behind the daemon, sessions with their SSE transcript, and terminals with the attach socket. Files, hosts and SQLite remain |
| Profiles phase (P4) | Profiles and preferences | in progress — the settings cascade (defaults, user, project) with per-key merge is done; profile files and `ACPILOT_*` layers remain |
| Models phase (P4.5) | Local models: LM Studio, the model lab, promotion, ACP bridge for remote drivers | contract only |
| Mod shop phase (P4.6) | Extensions: declarative packs (themes, presets, grammars), signed index, per-profile activation | store + activation done |
| Command line phase (P5) | CLI: full API coverage | |
| Editor phase (P6) | TUI: an editor you could use daily | in progress — the client half and the text model work; the panes that draw and edit a buffer are what remains |
| Language intelligence phase (P7) | Language servers | |
| Remote hosts phase (P8) | SSH hosts | |
| Windows phase (P9) | Windows + WSL | |
| Desktop phase (P10) | Desktop app (gpui) | framework decided and measured (the design phase's P0-3 benchmark, gpui 0.2.2); the shell itself starts here |
| Web phase (P11) | Web UI + `acpilot serve` | |
| Release phase (P12) | Packaging, docs, v0.1 | |

## How this page stays honest

The phases come from `TODO.md` in the repository, which is the source of truth.
When a phase lands, its row here changes in the same pull request — a status page
that drifts is just a rumour with better typography.

<Aside type="tip" title="Why publish this at all">
Because "coming soon" is what people write when they mean "not started". A
numbered phase with a visible state is the same information without the
marketing.
</Aside>
