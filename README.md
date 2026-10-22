# Acpilot

**[Read the field manual](https://rudeboy.dev/acpilot/)** — the docs site under [`site/`](site/),
written in the tone of a flight crew that has seen too much and honest about which phase each
page belongs to. Preview locally with `cd site && npm run dev`.

**One editor. Every agent. You fly.**

Acpilot is a code editor for the agent era. It is a Rust-native editor whose first-class
citizens are coding agents — Claude, Codex, Devin, Gemini, Freebuff, or any program you
care to wrap — all driven over the [Agent Client Protocol][acp] (ACP), from any interface
you prefer: terminal, TUI, native GUI, or browser.

`acp` + `pilot`. It pilots ACP agents. Say it out loud and it lands next to
"autopilot", which is roughly how it feels when a dozen agents are working and you are
only stepping in to approve the dangerous bits.

- Status: **pre-alpha / design**. No code yet — see [TODO.md](TODO.md).
- License: dual MIT OR Apache-2.0.
- Binaries: `acpilot` (CLI + TUI + GUI launcher), `acpilotd` (daemon).
- Directory: `acpilot/`, planned Rust workspace (edition 2024), layout in §6.

### Why "Acpilot" (and the runners-up)

- **Acpilot** — `acp` + `pilot`. It pilots ACP agents; it is one word, pronounceable,
  and it does not need a subtitle to explain itself. No existing product uses the name.
- **Acpex** — sharper, but the name is already taken by an Elixir ACP library (`acpex`).
- **Acprentice** — agents as apprentices is a lovely idea; the word is not. Too long to
  type, harder to spell than to say.
- **ACP Aviary** — the best metaphor on the list (agents are birds you keep and fly), but
  `acp` disappears into the second word.
- **Acolyte** — evokes ACP phonetically and literally means "attendant", but every
  explanation starts with an apology about the acronym.

---

## 1. The problem

An agent is now a program that edits your repo, runs your tests, and asks for permission
to `rm -rf`. The ecosystem converged on two facts:

1. **ACP standardizes the editor↔agent boundary.** A JSON-RPC 2.0 protocol, published by
   Zed Industries in August 2025, with a stable v1: `initialize` → `session/new` →
   `session/prompt`, streamed `session/update` notifications, and agent→client calls for
   filesystem, terminals and permission requests. Write one client, run every agent.
2. **Nobody has built the neutral workbench.** Today each agent ships its own CLI, its own
   transcript format, its own idea of where your repo lives. Running three agents on the
   same machine means three terminals, three sets of credentials, and no shared view of
   what they are doing. Running them against a remote box or a WSL distro is worse.

Acpilot is the neutral workbench: an editor that treats *agents* the way VS Code treated
*extensions* — a managed, permissioned, observable resource, addressable from anywhere.

## 2. What Acpilot does

- **Edits code.** A real editor: rope buffers, tree-sitter highlighting, search,
  multi-cursor, diffs — shared by the TUI and the GUI, so behaviour does not fork.
- **Speaks LSP properly.** Language servers are owned by the daemon, not the UIs: one
  `rust-analyzer` per workspace root, shared by every interface, running where the code
  lives — local, SSH, or inside WSL. Diagnostics, completion, hover, rename, code actions
  and format-on-save therefore look the same in a terminal as in a browser tab, and an
  agent can read the same diagnostics the human sees.
- **Keeps contexts apart.** Profiles — `work`, `personal`, `client-acme` — scope agents,
  credentials, language servers, permission policy, preferences and history. One daemon,
  several identities, no accidental key reuse.
- **Manages agents as first-class sessions.** Start, pause, cancel, resume, fork, and
  inspect sessions. Every session is an ACP conversation with a live transcript, a plan,
  tool-call timeline, and token/cost accounting.
- **Brokers permissions.** `session/request_permission` becomes a durable, addressable
  object. Answer it from the TUI, the GUI, a browser tab, or `acpilot permissions allow`
  in a script. Policy rules per workspace, per agent, per tool.
- **Runs programs as agents.** Anything can be an agent: a native ACP server
  (`claude-code-acp`, `gemini --experimental-acp`), a plain CLI wrapped in a PTY
  (`freebuff`, `aider`, `codex`), or an HTTP/JSON-RPC endpoint. One `agents.toml` entry.
- **Reaches the machine the work lives on.** Local subprocess, SSH host, or WSL distro —
  same session model, same UI. Agents run *where the code is*, not where the editor is.
- **Speaks one API.** The CLI is a thin client of the daemon's HTTP API. So are the TUI,
  the GUI, and the web UI. There is exactly one implementation of every behaviour.

## 3. Architecture

Acpilot is a **daemon plus thin clients**. The daemon (`acpilotd`) owns the control
plane: workspaces, hosts, sessions, agent processes, permissions, terminals, language
servers, profiles, and history. Clients never talk to an agent directly.

```
   acpilot (CLI)  ─┐
   acpilot tui    ─┤   HTTP/JSON + SSE (events) + WS (terminals, streams)
   Acpilot (GUI)  ─┼──────────────▶ ┌─────────────────────────────────────────┐
   browser / web  ─┘                │  acpilotd                               │
   (third parties) ───────────────▶ │   axum API  ·  permission broker       │
                                    │   session manager  ·  agent supervisor  │
                                    │   SQLite: sessions, events, transcripts │
                                    └───────────┬─────────────────────────────┘
                                                │  transport (one per host)
                    ┌───────────────────────────┼───────────────────────────┐
                    │                           │                           │
              local subprocess            SSH (russh)                 WSL (wsl.exe)
                    │                           │                           │
             claude-code-acp              remote agent              distro-side agent
             freebuff (PTY)               remote fs/pty             UNC or in-distro fs
```

Three rules make this hold together:

**Rule 1 — the API is a control plane, not a keystroke pipe.** Typing latency must never
depend on HTTP. Each UI owns its text buffer locally (`acpilot-text`), applies edits
optimistically, and syncs on save / on agent-edit. The daemon owns truth for *sessions,
agents, permissions, terminals, and file contents on disk*. Agent file writes arrive as
`session/update`-driven patch events over SSE, which the UI applies to its buffer.

**Rule 2 — the transport is a trait, not a branch.** Everything a session needs from a
machine is a small set of capabilities: spawn process, read/write file, run PTY, stat,
watch. `LocalHost`, `SshHost`, `WslHost` implement the same `Host` trait, so the ACP
client, the permission broker, and the UIs are unaware of where the work happens.

**Rule 3 — language servers and profiles are daemon state.** An LSP server is spawned through
the same `Host` trait as an agent, so `rust-analyzer` runs on the box that owns the files and
its diagnostics are broadcast like session events. Profiles partition everything mutable —
agents, keys, servers, policy, history — and the profile is a *required argument* through the
API, never ambient state, so a client cannot accidentally act as the wrong identity.

### Core concepts

| Term | Meaning |
| --- | --- |
| **Workspace** | A root directory (possibly on a remote host) plus its config, agents, and open buffers. |
| **Host** | A place code and agents live: `local`, `ssh://user@box`, `wsl://Ubuntu-24.04`. |
| **Profile** | A named identity: agents, secrets, LSP servers, policy, preferences, and its own history. |
| **Language server** | An LSP server owned by the daemon and bound to a workspace root on a host. |
| **Agent** | An ACP-speaking program, described by a manifest (`agents.toml`). |
| **Session** | One ACP conversation: `session/new`, prompt turns, updates, tool calls. |
| **Turn** | One `session/prompt` → `session/update`* → stop reason. |
| **Grant** | A resolved permission request, with policy, scope, and audit trail. |
| **Terminal** | A PTY owned by a session (agent-requested or user-opened). |

### Agent manifests

```toml
# ~/.config/acpilot/profiles/work/agents.toml
schema_version = 1

[[agents]]
id        = "claude"
name      = "Claude Code"
transport = "acp-stdio"
command   = "npx"
args      = ["-y", "@zed-industries/claude-code-acp"]
env       = { ANTHROPIC_API_KEY = "keyring:anthropic" }

[[agents]]
id        = "freebuff"
transport = "pty"                 # program-as-agent: no ACP, we drive it in a PTY
command   = "freebuff"
parser    = "ansi-transcript"     # built-in transcript adapters, see TODO P2-7
sentinel  = "▍Enter a coding task" # the input row back at rest ends the turn

[[agents]]
id        = "devin-remote"
transport = "http-jsonrpc"
url       = "https://devin.internal/acp"
auth      = "keyring:devin"
```

`transport = "acp-stdio"` is the native path and the one implemented first. `pty` is the
"run any program as an agent" path: launch it in a PTY, stream the screen, detect turns,
and expose it through the same session/turn API. `http-jsonrpc` covers hosted agents.
`keyring:<name>` resolves in the profile the file belongs to; reaching into another one
needs `keyring:<profile>/<name>` *and* `allow_cross_profile = true` on that agent.

### Profiles

A profile is a directory, a keyring namespace, and a store — not a theme setting. Agents,
secrets, language servers, policy, preferences, and session history are scoped to one,
because the key you use for a client is not the one you use on a weekend, and mixing them
silently is the failure mode with the worst consequences.

```
~/.config/acpilot/
├── config.toml              # global: active_profile, [daemon], profile registry
└── profiles/
    ├── work/
    │   ├── profile.toml     # preferences: editor, theme, keymap, ui
    │   ├── agents.toml      # per-profile agents and keyring references
    │   ├── lsp.toml         # per-profile language servers and their settings
    │   ├── policy.toml      # permission rules, workspace trust
    │   ├── theme.toml
    │   └── keymap.toml
    └── personal/…
```

| Scoped to a profile | Global |
| --- | --- |
| Agents and their credentials (`keyring:<name>`, resolved in this profile) | Daemon settings (bind, token, log level) |
| Language servers and their settings | Profile registry and `active_profile` |
| Permission policy, workspace trust, LSP trust pins | Host definitions (one machine, many identities) |
| Editor, theme, keymap and UI preferences | |
| Sessions, transcripts, grants, workspaces, diagnostics cache | |

Resolution is `--profile` → `ACPILOT_PROFILE` → `.acpilot/profile` (a committed, secret-free
marker) → `active_profile` → `default`. A name that does not resolve is an error, never a
silent fallback: guessing here means running the wrong agent with the wrong key. Settings
cascade per key (defaults < global < profile < workspace < env < flags); `acpilot config
explain <key>` and `acpilot profile diff a b` name the file that won.

Profiles are local identities, not accounts: two developers on one box means two profiles.
Nothing is synced, hosted, or sent anywhere.

## 4. Interfaces

### CLI — `acpilot` (the API, typed)

```console
$ acpilot host add wsl --distro Ubuntu-24.04 --name dev
$ acpilot workspace open ~/code/app --host dev
$ acpilot --profile work agents list
$ acpilot session new --agent claude --workspace app
$ acpilot prompt --session @s3 "add retries to the http client and run the tests" --follow
$ acpilot sessions ls --json | jq '.[] | select(.status=="needs-input")'
$ acpilot permissions pending
$ acpilot   permissions allow 8f3c --once
$ acpilot lsp status
$ acpilot diagnostics --workspace app --severity error
$ acpilot tui
```

Every command maps 1:1 to an endpoint, and `acpilot api <METHOD> <PATH>` is the escape
hatch for anything not yet wrapped.

### TUI — `acpilot tui`

Terminal-native editor + agent panes. Works over SSH into a box with no display server.
Split editor / transcript / tool timeline / permission prompt; inline diffs; PTY
attachments for program-agents. Diagnostics gutter, hover and completion popups come from
the daemon's language servers, so the terminal and the GUI agree about what is broken. The
status line carries the active profile — switching it is one keystroke, never a restart.

### GUI — `acpilot gui` / Acpilot.app

Native desktop app: same panels, GPU-rendered text, LSP, image previews, real IME and
accessibility. Talks to the daemon exactly as the TUI does.

### Web — `acpilot serve --http`

`acpilotd` serves a compiled SPA (CodeMirror 6 + xterm.js) plus the API. Same daemon,
same sessions; a browser tab can watch an agent run on the office workstation. Token
auth, optional TLS termination, no cloud dependency.

### API — HTTP + SSE + WS

```
GET    /v1/health                         GET  /v1/openapi.json
GET    /v1/hosts                          POST /v1/hosts
                                          POST /v1/hosts/{id}/test
GET    /v1/workspaces                     POST /v1/workspaces
GET    /v1/agents                         POST /v1/agents/{id}/refresh
                                          POST /v1/agents/{id}/trust
GET    /v1/sessions                       POST /v1/sessions
GET    /v1/sessions/{id}                  DELETE /v1/sessions/{id}
POST   /v1/sessions/{id}/prompt           POST /v1/sessions/{id}/cancel
POST   /v1/sessions/{id}/mode             GET  /v1/sessions/{id}/events   (SSE)
WS     /v1/sessions/{id}/stream           GET  /v1/events                 (SSE)
GET    /v1/permissions?status=pending     POST /v1/permissions/{id}/resolve
GET    /v1/files?workspace=&path=         PUT  /v1/files
GET    /v1/files/list
GET    /v1/terminals?session=             POST /v1/terminals
WS     /v1/terminals/{id}/attach
GET    /v1/profiles                       POST /v1/profiles
GET    /v1/profiles/{id}                  PATCH/DELETE /v1/profiles/{id}
POST   /v1/profiles/{id}/activate         POST /v1/profiles/{id}/export
GET    /v1/profiles/{id}/preferences      PATCH .../preferences
GET    /v1/lsp/servers                    POST /v1/lsp/servers/{id}/restart
GET    /v1/lsp/servers/{id}/logs          GET  /v1/lsp/diagnostics
POST   /v1/lsp/request
```

The list is **frozen** for v1, with the reasoning and the rejected alternatives in
[`docs/api.md`](docs/api.md). Providers, models, recipes and the ACP bridge are deliberately
later; none of them changes a route above.

Every route takes `X-Acpilot-Profile`; on a mutating request the header is *required* — a
client cannot silently act as the wrong identity. On a read or a stream, omitting it means
the active profile, and a stream binds whichever profile it was opened with.

Agent output streams over SSE with a per-session monotonic sequence number: `Last-Event-ID`
replays from the transcript, a heartbeat every 15 s proves the stream is alive, and an id
the daemon cannot replay is answered with `resync` rather than silence. A prompt returns
`202` with a `turn_id` instead of waiting for the agent, and an `idempotency_key` makes a
retried prompt the same turn. Pending permissions are *state*
(`GET /v1/permissions?status=pending`), while resolving one is idempotent for a repeated
answer and `409` for a different one — which is what makes a script able to answer a prompt.

Every failure is the same envelope, `{code, message, details?}`, and `code` is stable
enough to switch on: `invalid_request` (400), `permission_denied` (403), `not_found`
(404), `conflict` (409), `timeout` (504), `host_unavailable`, `model_unavailable` and
`agent_failed` (502), `protocol` and `io` (500). The 502/500 split is the "retry" versus
"file a bug" signal.

OpenAPI 3.1 is generated from the Rust types (`utoipa`); TS and Rust clients are
generated from it so a third-party UI is a weekend project, not a fork.

## 5. Library research: what to build each UI on

Versions are what `cargo search` reports as of **2026-10-20**. The point of the table is
not novelty: it is minimising the number of places where we will be the first serious
user of a library.

### 5.1 Protocol core (everything depends on this)

| Need | Choice | Notes |
| --- | --- | --- |
| ACP client & agent runtime | **`agent-client-protocol` 3.4** | Official Rust SDK. Implements both `Client` and `Agent` traits, JSON-RPC envelopes, transports (`Lines`, `ByteStreams`, `Channel`), typed v1 sessions. Enable features `process` + `stdio`. Protocol v1 is stable; v2 is behind a draft feature — target v1, design not to block v2. |
| Raw protocol types / schema | `agent-client-protocol-schema` | Same org; use when you need wire types without the runtime (e.g. a wasm web client). |
| MCP bridge | `agent-client-protocol-rmcp` | For attaching MCP servers to sessions the way ACP `mcpServers` expects. |
| Async runtime | `tokio` 1.54 | Multi-thread runtime for the daemon; current-thread for the CLI. |
| Errors | `thiserror` 2 (libs), `anyhow` (bins), `miette` 7 (user-facing) | Standard split. |
| Tracing | `tracing` 0.1 + `tracing-subscriber` + `tracing-appender` | JSON logs, rotation, per-session spans. |

**Why not roll our own JSON-RPC:** ACP has typing quirks (`camelCase` keys, `_meta`
extensibility, 1-based lines, absolute paths) that the SDK already encodes, and its test
compatibility kit is what we will be graded against in CI.

### 5.2 CLI

| Need | Choice | Alternatives considered |
| --- | --- | --- |
| Argument parsing | **`clap` 4.6** (derive) | `bpaf` (smaller, nice combinator style), `argh` (fast, no completions), `xflags` |
| Shell completions / man pages | `clap_complete` 4.6, `clap_mangen` | — |
| HTTP client | **`reqwest` 0.13** (rustls, no OpenSSL — the 0.13 feature is `rustls`, not 0.12's `rustls-tls`) | `ureq` (blocking, tiny — good for a pure CLI), `hyper` raw |
| Event streaming | `eventsource-stream` (SSE) + `tokio-tungstenite` 0.30 | poll the events endpoint (worse: latency + load) |
| Output | `owo-colors`/`anstyle`, `tabled` or `comfy-table`, `indicatif` | `nu-ansi-term`, `cli-table` |
| Config layering | **`figment` 0.10** (TOML + env + flags + `~/.config`) | `confique`, `config` |

`clap` wins because completions and man pages are generated from the same derive, and
because the CLI is a public API surface: help text quality is a product feature. The CLI
holds no domain logic — it deserialises into `acpilot-proto` types and calls the API.

### 5.3 TUI

| Need | Choice | Alternatives considered |
| --- | --- | --- |
| Widgets / layout / diffing | **`ratatui` 0.30** (+ `crossterm` backend) | `cursive` (retained, callback-based — real widgets, harder to compose with async), `iocraft` (declarative, young), `tuirealm` |
| Text buffer | **`ropey`** (1.6.x; 2.0 is in beta — pin stable behind `acpilot-text`) | `xi-rope`, plain `Vec<String>` (no) |
| Syntax highlighting | **`tree-sitter` 0.27** + `tree-sitter-highlight` | `syntect` + `two-face` (regex-based, no incremental parse, but zero grammar build cost) |
| Terminal panes | `tui-term` 0.3 (wraps `vt100` 0.18) | render raw escape codes (no) |
| Transcript rendering | `pulldown-cmark` (markdown → styled lines, our own small renderer) | `tui-markdown` (quick start, less control) |
| Clipboard | `arboard` 3.7 | OSC 52 only (works over SSH) |

`ratatui` is the only mature choice that is still an immediate-mode library: the editor
loop owns state, which is exactly what a text editor wants. We deliberately do **not** use
`tui-textarea` for the editor pane — it is a textarea, not an editor — but it is a fine
reference implementation for editing keymaps.

### 5.4 GUI

This is the one genuinely contested decision, so it gets a benchmark probe with hard exit
criteria (TODO `P0-3`) and a decision ADR (TODO `P10-1`) rather than a fait accompli.

| Candidate | Case for | Case against |
| --- | --- | --- |
| **`gpui` 0.2.2** (chosen) | Built for exactly this: shaped line layout and line wrapping in `gpui::text_system`, `uniform_list` for virtualised rows, drag-and-drop and IME in the framework itself, and in Zed's `crates/editor` the display-map design (folds, blocks, inlays) we can copy into `acpilot-text` — plus the *reference ACP client* (agent panel, diff review, terminal view) as readable source. Apache-2.0, stable Rust (Zed pins 1.91.1), Linux/macOS/Windows backends. The decisive reason is not technical: a sibling project here will be a **GUI builder written in gpui**, so Acpilot becomes the second real user — framework knowledge, upgrade work, and any upstream fixes are paid once across both instead of sunk into a one-off. The `zed/` checkout on disk is reference reading for those patterns, not a dependency. | No semver contract: `0.2.2` was published **2025-10-22** and main has moved on since, so we adopt a year-old surface deliberately and wait for features rather than tracking main. Pulls `zed-font-kit` as a **git dependency pinned by rev**, so a crates.io-only build is not self-contained. No `accesskit` at this version, so screen-reader support is work we do, not a checkbox we inherit. Upstream's release cadence becomes ours, and the gaps we hit are the same ones the GUI builder waits on. |
| `iced` 0.14 (fallback, held in reserve) | Stable public API since 0.10, real semver, MIT, MSRV 1.88, Elm-style update loop, `wgpu` + tiny-skia fallback, `accesskit` already wired, used by System76's COSMIC | Wrong shape for an editor: `text_editor` is a textarea-grade widget, so the shaped, virtualised, fold-aware surface is entirely ours to build — the thing gpui gives away |
| `egui` 0.37 / `eframe` | Fastest to a working panel; instant dev loop; `accesskit` support | Immediate mode rebuilds text layout every frame; weak native selection/IME story; feels like a debug tool, not an editor |
| Tauri 2 + the web UI | Reuses the CodeMirror frontend wholesale; small binaries; great text editing for free | WebView variance, an IPC hop for every edit, and the "native" app is a browser — contradicts the Rust-native premise |

Decision: build the GUI shell in **gpui**, pinned deliberately. The trade is explicit — we
accept a dependency without a stability contract, and in exchange we stop writing a text
surface from scratch and start from the same primitives Zed's editor uses. It is only a good
trade if the pin is disciplined, so it is a task, not a footnote:

- **Pin, don't float.** `gpui = "0.2.2"` in the manifest. When we need a fix that only exists
  on main, bump to an exact git `rev` and record why in the ADR — never `branch = "main"`.
- **Vendor for reproducible builds.** `cargo vendor` (or a registry mirror) in CI, because one
  of gpui's own dependencies is already a git rev. A build that needs GitHub at compile time
  is a build that breaks on an air-gapped release day.
- **Upgrades track releases, not a calendar.** A new gpui release is adopted within a week
  (`XT-1` in TODO), with the P0-3 harness as the gate; the pin is exact so breakage is
  attributable, not a nightly gamble.
- **Share the pin with the GUI builder.** When that project starts, both repos pin the same
  gpui `rev` and share one `patches/gpui/` series and one upgrade cadence. Two divergent pins
  would double the upgrade cost and split our upstream pressure for no benefit.
- **Patch narrowly, upstream first.** If we must carry a change, it lives as a small commit
  series in `patches/gpui/` with the upstream PR linked; a drifting fork is the failure mode
  this bullet exists to prevent.

The P0-3 benchmark still has veto power: if gpui misses the 16 ms p95 keystroke→paint or
150 ms first-paint budgets by more than 2×, or needs a nightly toolchain or an unsound patch
to get there, the fallback order is **iced 0.14** (semver, `accesskit`) → Tauri (reuse the web
UI). The cost of that switch stays bounded because nothing UI-specific lives outside
`acpilot-gui`.

### 5.5 Web UI

| Need | Choice | Alternatives considered |
| --- | --- | --- |
| Editor | **CodeMirror 6** | Monaco (2–5 MB, VS Code engine, better IntelliSense by default, poor for embedding); Ace (legacy) |
| Diff view | `@codemirror/merge` | Monaco's diff editor |
| Terminal | **xterm.js** (`@xterm/xterm` + fit + webgl addons) | WASM terminal emulators (promising, young; revisit after v0.1) |
| App shell | **Svelte 5 + Vite + TypeScript** | React 19 (biggest hiring pool), Solid, Leptos/WASM (keeps it all-Rust; CodeMirror still needs JS glue, so the all-Rust win is smaller than it looks) |
| API client | `openapi-typescript` + `openapi-fetch` (generated from the daemon's OpenAPI) | hand-written client (drifts) |
| State | Svelte runes / stores | TanStack Query (fine, but SSE makes it partial) |

CodeMirror 6 over Monaco is the same call Sourcegraph made when they migrated: smaller,
tree-shakeable, and it lets us style the editor to match the TUI instead of shipping a
second design language. The web UI holds no domain logic either — it is a view over SSE
events plus optimistic local edits.

### 5.6 Hosts: SSH, WSL, terminals

| Need | Choice | Notes |
| --- | --- | --- |
| SSH client | **`russh` 0.64** | Pure Rust, async, client *and* server, no C toolchain, no libssh2. Track advisories (it had a DoS, CVE-2026-73429, patched — pin and audit). |
| SSH config parsing | `ssh2-config` 0.9 | Parses `~/.ssh/config` including `ProxyJump`/`Include`; feed results into russh. |
| SFTP | `russh-sftp` 3.0 | Remote file read/write and workspace browsing. |
| Escalation path | `openssh` 0.11 | Shell out to the system `ssh` when a config uses FIDO keys / `Match` / ControlMaster and russh cannot express it. Feature-flagged "system ssh" mode. |
| PTY | **`portable-pty` 0.9** | WezTerm's cross-platform PTY: ConPTY on Windows, `forkpty` elsewhere. |
| Terminal emulation | `vt100` 0.18 | Parse PTY output into a screen; `tui-term` (TUI) and `xterm.js` (web) render it. |
| WSL | `wsl.exe` + `windows` crate + `winreg` | Enumerate distros from the registry / `wsl -l -q`; launch `wsl.exe -d <distro> --cd <dir> -- acpilotd-helper`; ConPTY for the PTY case. |
| Windows services | `windows` 0.62 | Job objects for process-tree cleanup, named pipes for the local daemon socket. |

**Why not `ssh2`/libssh2:** it needs a C toolchain on every target, is client-only, and
2026 brought a critical libssh2 memory-corruption advisory (CVE-2026-55200). Pure Rust
removes both the build and the blast radius. The system-`ssh` fallback exists because
users' `~/.ssh/config` is a decade of accumulated exotica, and it is cheaper to respect it
than to reimplement it.

**WSL design note:** do not edit WSL files over `\\wsl.localhost` UNC paths in the hot
path — 9P is slow and silently changes case/permission semantics. Run the daemon helper
*inside* the distro instead (`wsl.exe -d Ubuntu -- acpilotd-helper --stdio`), so file and
agent operations are in-distro and only the control plane crosses the boundary. UNC is the
fallback for read-only browsing. Measured on the dev box, same file and same cache: 13.0 ms
per 37 KB read over UNC against 0.096 ms in-distro, 136× — `docs/notes/wsl-helper.md`.

The path a user opens is only a door. A folder that arrives as
`\\wsl.localhost\Ubuntu-24.04\home\sam\proj` becomes the host `wsl://Ubuntu-24.04`
plus the Linux cwd `/home/sam/proj`; the Windows path never becomes the identity of the
workspace. On a machine without WSL there is no `wsl://` host at all — detection is one
`wsl.exe -l -q`, and absent, failing, or empty is not an error and never a prompt.

### 5.7 Language servers (LSP)

LSP lives in the daemon (`acpilot-lsp`), because a language server is another process that
must run where the files are and be shared by every client. The UIs render what the API
hands them; none of them learns the protocol.

| Need | Choice | Notes |
| --- | --- | --- |
| Wire types | **`gen-lsp-types` 0.11** (`url` feature) | Generated from the official LSP metamodel, so it tracks the spec and distinguishes `null` from absent. It deliberately leaves URI encoding to the consumer, so we pick `url::Url` — what servers actually send. `lsp-types` 0.97 is unmaintained and moved to `fluent-uri`, trading interoperability for purity (D8). Isolated behind `acpilot-lsp::wire`. |
| Client loop | **our own** (~400 lines) | `Content-Length` framing, request table, `$/cancelRequest`, `$/progress`, server→client requests. ACP gives us the transport abstraction; LSP framing differs (headers, not newline-delimited), so only the transport is shared. `async-lsp` 0.2 is the fallback: real client support, but pinned to `lsp-types` and built around local stdio. |
| Document store | `ropey` + our own position mapping | No crate fits: incremental sync, per-URI versions, and UTF-16↔UTF-8 conversion must run against the same buffer the UI edits. |
| Servers | ~10 presets in `lsp.toml`, declared as data | rust-analyzer, typescript-language-server, gopls, pyright, clangd, lua-language-server, bash-language-server, taplo, yaml-language-server, marksman. Detect-only in v0.1 — no downloads (D12). |
| Web transport | `@codemirror/lsp-client` 6.x over a WS bridge | First-party CodeMirror client (6.0, July 2025); its `Transport` interface maps onto a daemon WebSocket, so the browser sees the same diagnostics as the TUI. |
| MCP bridge | `agent-client-protocol-rmcp` | Exposes `lsp_diagnostics`/`lsp_hover`/`lsp_definition` to agents, so "what does the compiler say" is a tool call instead of a blind re-run of `cargo check`. |

Three non-obvious calls:

- **Position encoding is negotiated, not assumed.** LSP defaults to UTF-16 code units; `ropey`
  is UTF-8 bytes. We advertise `general.positionEncodings = ["utf-8", "utf-16"]` and convert
  when a server insists on UTF-16. Byte offsets that ignore this drift on emoji, CJK, and
  combining marks — the bug that makes a new editor feel subtly broken.
- **Code actions are edits.** `workspace/applyEdit` and `executeCommand` (a quick fix,
  "organize imports") go through the same diff preview and permission broker as an agent's
  write. A language server does not get exempted from the model by being local.
- **LSP never sits in the keystroke path.** Completions are debounced and cancellable;
  diagnostics arrive out of band. A wedged `rust-analyzer` should cost a status-line warning,
  not a laggy editor.

### 5.8 Editor internals

| Need | Choice | Notes |
| --- | --- | --- |
| Rope / buffer | `ropey` | Fallback: `xi-rope` if ropey stalls or the 2.0 beta slips. |
| Incremental parse | `tree-sitter` 0.27 + grammar crates | Ship ~15 grammars in v1 (rust, ts, tsx, js, py, go, json, toml, md, yaml, sh, c, cpp, java, html, css). |
| LSP | see §5.7 | Wire types, client loop and transport are decided there; `acpilot-text` supplies the position mapping and buffer versions. |
| Diff | `imara-diff` 0.2 (fast) / `similar` 3 (ergonomic) | Needed for agent edit previews and permission review. |
| Search | `ignore` + `grep-searcher`/`grep-regex` | The ripgrep crates; do not write a grep. |
| File watching | `notify` (pin 8.x; 9 is RC) | Debounced, coalesced into one event stream per workspace. |
| Markdown | `pulldown-cmark` | Rendered per-UI. |
| Persistence | `rusqlite` 0.40 (bundled SQLite) + `rusqlite_migration` | Sessions, events, grants, transcript index. Append-only JSONL per session as the portable format. |
| Config / secrets | `figment` + `directories` + `keyring` 4.2 | Secrets never written to config files; `keyring:` URI scheme in manifests. |
| IDs / time | `uuid` (v7), `jiff` 0.2 | Time-ordered IDs make the event log cheap to paginate. |

### 5.9 Tooling

`cargo nextest` (tests), `insta` (snapshots of transcripts/API JSON), `proptest`
(rope/undo invariants), `criterion` (benchmarks), `cargo-deny` (licences/advisories),
`cargo-dist` 0.32 (installers: tarballs, Homebrew, PowerShell, MSI), `just` or `cargo
xtask` for dev tasks, GitHub Actions matrix for linux/macos/windows.

**Version policy: latest stable, no lag.** Every dependency sits at the newest stable release
and is updated as soon as a new one exists — automated PRs (Dependabot or Renovate) on release,
a weekly `cargo update` for the lockfile, majors reviewed by a human, and the P0-3 performance
harness in CI so an upgrade that costs milliseconds fails loudly. No dependency is allowed to
drift without a reason recorded in this table:

| Dependency | Held at | Why | Adopt when |
| --- | --- | --- | --- |
| `gpui` | `0.2.2` | No semver; one exact pin shared with the GUI-builder project (D1) | A release lands → upgrade within a week, P0-3 gates it |
| `ropey` | `1.6` | `2.0.0-beta.1` is a pre-release; the rope underpins every edit path | `2.0.0` ships |
| `notify` | `8.2` | `9.0.0-rc.6` is an RC and file watching runs in the daemon's hot loop | `9.0.0` ships |
| `lsp-types` | `0.97` | Only a fallback behind `acpilot-lsp::wire`; unmaintained upstream (D8) | Never, unless D8 flips |

Pre-releases are otherwise not used: "latest" means latest stable. `agent-client-protocol`
3.4 already targets protocol v1 stable, with the v2 draft tracked rather than adopted. The same
policy covers `web/package.json`.

**Toolchain.** We track current stable Rust, pinned in `rust-toolchain.toml` and bumped the same
way. This is not free: `rusqlite_migration` 2.6 already requires **1.95**, which sets the floor
for the workspace, so an MSRV bump is a dependency decision reviewed with the PR — not a silent
side effect of `cargo update`.

## 6. Repository layout

```
acpilot/
├── Cargo.toml                 # workspace, edition 2024, resolver 3, every version
├── rust-toolchain.toml        # pinned stable toolchain
├── deny.toml                  # licence + advisory policy (cargo-deny)
├── .github/workflows/         # CI: fmt, clippy, tests, deny, snapshot freshness
├── crates/
│   ├── acpilot-proto/         # API DTOs + OpenAPI (utoipa) — shared by daemon & clients
│   ├── acpilot-core/          # domain: workspaces, sessions, agents, permissions
│   ├── acpilot-text/          # rope, selection, edits, tree-sitter, diff — UI-agnostic
│   ├── acpilot-agent/         # ACP client engine, supervision, transcript adapters
│   ├── acpilot-model/         # local model backends, capability probes, recipes
│   ├── acpilot-host/          # Host trait: local, ssh, wsl
│   ├── acpilot-lsp/           # LSP engine: supervision, document sync, diagnostics
│   ├── acpilot-profile/       # profiles, config cascade, preferences, path roots
│   ├── acpilot-extensions/    # extension manifests, theme tokens, presets, loader
│   ├── acpilotd/              # daemon: axum API, sqlite, event bus
│   ├── acpilotd-helper/       # tiny in-host shim for SSH/WSL (stdio JSON-RPC)
│   ├── acpilot-cli/           # `acpilot` binary: CLI + subcommand dispatch
│   ├── acpilot-tui/           # ratatui UI
│   ├── acpilot-gui/           # gpui app (pinned; see §5.4)
│   └── acpilot-web/           # embed the built SPA + static serving
├── extensions/                # built-in extensions (themes, presets), embedded in the binary
├── marketplace/               # curated list of extension repositories, reviewed by PR
├── web/                       # Svelte 5 + Vite + CodeMirror 6 + xterm.js (P11)
├── site/                      # docs site: Astro + Starlight, deployed to Pages
├── patches/                   # narrow, upstreamed patches if a pinned dep needs one
├── probes/                    # time-boxed P0 measurements, one question each (CONTRIBUTING §Probes)
├── migrations/                # sqlite migrations
├── docs/                      # ADRs, ACP + LSP notes, protocol map
├── xtask/                     # build/dev/packaging tasks
├── README.md
└── TODO.md
```

## 7. Roadmap (details in [TODO.md](TODO.md))

| Phase | Outcome |
| --- | --- |
| P0 | Probes: ACP handshake, PTY program-agent, gpui text benchmark, SSH PTY, WSL helper, LSP round-trip |
| P1 | Workspace skeleton, CI, `acpilot-proto`, error model |
| P2 | ACP engine + session manager + permission broker (headless, tested against a mock agent) |
| P3 | `acpilotd` HTTP/SSE API + SQLite history |
| P4 | Profiles: config cascade, per-profile secrets, state, and preferences |
| P4.5 | Local models: LM Studio provider, the model lab (tune a prompt/params/context, record runs), recipe→agent promotion, ACP bridge for remote drivers |
| P4.6 | Extensions: declarative packs for themes, language-server and agent presets, grammars, snippets; signed index; per-profile activation |
| P5 | CLI (complete API coverage, `--profile` everywhere) |
| P6 | TUI editor (usable daily driver for editing + one agent) |
| P7 | Language servers: diagnostics, completion, hover, rename, format-on-save, remote LSP |
| P8 | SSH hosts: remote workspace, remote agents, remote PTY, remote LSP |
| P9 | Windows + WSL hosts |
| P10 | GUI (gpui) |
| P11 | Web UI + `acpilot serve` |
| P12 | Packaging, docs, v0.1 release |

**v0.1 "one agent, one machine"**: native ACP agent, local + WSL + SSH, TUI and CLI,
permissions, profiles, LSP, session resume. GUI and web follow; they share the same daemon,
so they are additive, not rewrites.

## 8. Non-goals for v0.1

- Collaborative multi-user editing (design for it; do not ship CRDT in v0.1).
- Being a language server, or shipping and auto-installing other people's servers. We
  configure the ones you already have; v0.1 downloads no servers (D12). Extensions can
  carry a preset for a server you installed yourself, never the binary.
- Synced or hosted profiles. A profile is a local identity, not an account.
- Being an agent itself. Acpilot ships no hosted model, no default personality, and no
  opinion about which agent is best. The model lab (P4.5) turns a model *you* already run
  into an ordinary agent manifest — the cockpit still belongs to the pilot.
- Replacing the user's shell, git client, or package manager.
- Hosted/cloud service. The daemon binds localhost by default; remote exposure is
  explicitly opt-in with a token.

## 9. Design principles

1. **One behaviour, one implementation.** If the TUI can do it, it goes through the API.
2. **The agent is a process, not a plugin.** Crash it, restart it, run it over there.
3. **Permissions are durable objects.** Every "allow" has an ID, a scope, and a log line.
4. **Local-first, remote-capable.** No network requirement to edit a file.
5. **Boring dependencies, pinned.** Prefer the crate everyone uses over the crate with the
   prettiest README — with the documented exceptions above, and an ADR for each.
6. **Profiles are isolation boundaries, not themes.** If two things must not share a
   credential or a history, they must not share a profile — and the code must not be able to
   guess which one it is in.

[acp]: https://agentclientprotocol.com
