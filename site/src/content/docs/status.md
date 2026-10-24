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
| Daemon binds and serves | <span class="pill pill--flying">flying</span> | `GET /v1/health`, `GET /v1/agents`, `/v1/permissions`, `GET /v1/events` (SSE), `GET /v1/openapi.json` |
| API document | <span class="pill pill--flying">flying</span> | Snapshot-tested, so a wire change is a reviewable diff |
| Error contract | <span class="pill pill--flying">flying</span> | One error type → fixed HTTP statuses and exit codes |
| Swagger UI | <span class="pill pill--flying">flying</span> | `--features docs`, at `/docs` |
| CLI | <span class="pill pill--boarding">boarding</span> | `acpilot version` only; the rest is the command-line phase (P5) |
| TUI, desktop, web | <span class="pill pill--hangar">hangar</span> | Crates exist so the layout is real; the UIs are the editor phase (P6)/Desktop phase (P10)/Web phase (P11) |
| Agents, sessions, permissions | <span class="pill pill--boarding">boarding</span> | Engine phase (P2) — manifest registry, supervisor (stderr ring, restart, tree kill), sessions (initialize/new/load/prompt/cancel, numbered events), the agent-facing file callbacks (workspace-scoped reads, writes outside it held for approval), the permission broker (ordered rules, path globs, `allow always`, audit rows), and the API half: `GET /v1/agents` with `available`/`missing` from `PATH`, the pending list, the resolve route, and a stream that announces a new request; terminals next |
| Language servers | <span class="pill pill--hangar">hangar</span> | Language intelligence phase (P7) |
| Profiles | <span class="pill pill--hangar">hangar</span> | Profiles phase (P4) |
| Local models (LM Studio) | <span class="pill pill--boarding">boarding</span> | Models phase (P4.5) — `acpilot-model` discovers models and probes capabilities against a mock server; no daemon route or CLI verb yet, so you cannot reach it from the app |
| The model lab | <span class="pill pill--hangar">hangar</span> | Models phase (P4.5) — recipes, runs, workbench |
| Integration stats | <span class="pill pill--hangar">hangar</span> | Contract only: `IntegrationStats` and the fill math are tested; no `/v1/stats` route and no UI yet |
| Git diff and tree | <span class="pill pill--hangar">hangar</span> | `acpilot-git` reads status, diffs, tree and log from the system `git`; no route or pane yet |
| Commit messages | <span class="pill pill--hangar">hangar</span> | Prompt and validator landed in `acpilot-core`; no generator route and no editor |
| Skills | <span class="pill pill--hangar">hangar</span> | `acpilot-skills` discovers `SKILL.md` from the workspace and extensions; no creator/editor yet |
| Remote drivers over ACP | <span class="pill pill--hangar">hangar</span> | Models phase's task P4.5-10 — bridge, pairing, scopes. Off by default |
| Extensions (themes, presets, grammars) | <span class="pill pill--boarding">boarding</span> | Mod shop phase (P4.6) — loader, checksummed store, per-profile activation with precedence, TextMate theme import, and a curated marketplace list. Signed index and grammar loading remain |
| SSH / WSL hosts | <span class="pill pill--hangar">hangar</span> | Remote hosts phase (P8) / Windows phase (P9) |

## The flight plan

| Phase | What lands | State |
| --- | --- | --- |
| Design phase (P0) | Probes: ACP handshake, PTY agent, gpui text benchmark, SSH, WSL, LSP | in progress — ACP handshake, PTY agent, LSP round-trips and the gpui benchmark done |
| Foundation phase (P1) | Workspace skeleton, error model, CI, OpenAPI snapshot | **done** |
| Engine phase (P2) | ACP engine, sessions, permission broker (headless, mock agent) | in progress — registry, supervisor, sessions, the filesystem callbacks and the permission broker done |
| Service phase (P3) | Daemon API: SSE, files, terminals, SQLite | in progress — `GET /v1/agents`, the pending-permission list and its resolve route, `GET /v1/events` announcing new requests, and `acpilotd --profile <dir>` putting a profile's `[policy]` and `agents.toml` behind the daemon |
| Profiles phase (P4) | Profiles and preferences | |
| Models phase (P4.5) | Local models: LM Studio, the model lab, promotion, ACP bridge for remote drivers | contract only |
| Mod shop phase (P4.6) | Extensions: declarative packs (themes, presets, grammars), signed index, per-profile activation | store + activation done |
| Command line phase (P5) | CLI: full API coverage | |
| Editor phase (P6) | TUI: an editor you could use daily | |
| Language intelligence phase (P7) | Language servers | |
| Remote hosts phase (P8) | SSH hosts | |
| Windows phase (P9) | Windows + WSL | |
| Desktop phase (P10) | Desktop app (gpui) | framework decided and measured (the design phase's task P0-3, gpui 0.2.2); the shell starts in the desktop phase (P10) |
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
