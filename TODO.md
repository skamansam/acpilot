# Acpilot — TODO

The plan to go from an empty directory to `v0.1.0`: **one agent, one machine, four
interfaces, zero rewrites.**

## How to read this

- Tasks are `P<phase>-<n>` so commits and issues can reference them (`feat(acp): P2-4`).
- A phase is done when every box is checked **and** the phase's acceptance test passes.
- "DoD" (definition of done) is stated per phase; it is deliberately testable.
- Anything marked **[decide]** needs a short ADR in `docs/adr/` before code lands.
- Estimates are ideal engineering days, not calendar promises.

Ordering rule: **no UI work before the daemon API works.** UIs are clients; if a UI needs
something, add it to the API first. That single rule is what stops the four frontends from
becoming four products.

---

## P0 — De-risking spikes (≈9 days)

Throwaway code in `spikes/`, no workspace integration, no polish. Each spike answers one
question that would be expensive to get wrong.

- [x] **P0-1 · ACP handshake against a real agent** (2d) — **done 2026-10-21**
  - `cargo new spikes/acp-handshake`; deps: `agent-client-protocol = { version = "3.4", features = ["process", "stdio"] }`, `tokio`.
  - Implement the `Client` trait: `initialize` (protocol v1) → `session/new` → `session/prompt` "list the files here" → print every `session/update`.
  - Handle `session/request_permission` with an auto-deny and log the payload.
  - Run against `npx -y @zed-industries/claude-code-acp` and `gemini --experimental-acp`.
  - **Answer:** exact capabilities advertised by each agent; which `session/update` kinds arrive; what `stopReason` values appear; does `session/load` work.
  - **Exit criteria:** transcript of two different agents driving the same 40 lines of client code, checked into `docs/notes/acp-transcripts.md`.
  - **Result:** the client is `spikes/acp-handshake` (613 lines, standalone crate, SDK 3.4.0) and
    the transcript is `docs/notes/acp-transcripts.md`. It ran Claude Agent 0.94.0 and OpenCode
    1.19.3 through one code path: both advertise `loadSession`, and both replay a **condensed**
    history (5 and 7 updates for turns that streamed 29 and 16), so `session/load` cannot replace
    our own transcript. Seven update kinds appeared, none of them `plan` or `thought`. A denied
    write is a `failed` tool call with `end_turn`, not a protocol error. OpenCode 1.15.13 answered
    `end_turn` with zero tokens and no text and no error — agent versions must be recorded. Usage
    on `PromptResponse` is behind `unstable_end_turn_token_usage` and was dropped by the typed view.
    Two plan details changed in the doing: the Claude package is now
    `@agentclientprotocol/claude-agent-acp` (the old name is deprecated), and Gemini CLI was not
    installed or signed in on this machine, so OpenCode 1.19.3 was the second agent.

- [x] **P0-2 · Program-as-agent over a PTY** (2d) — **done 2026-10-21**
  - Spawn `freebuff` (and one other TUI CLI) under `portable-pty`, render its screen with `vt100`, detect "turn complete" from a sentinel (prompt string reappearing / idle timeout).
  - **Answer:** is a transcript adapter viable, or do we need per-program adapter profiles? What breaks with alternate-screen TUIs, colours, and bracketed paste?
  - **Exit criteria:** a `PtyAgent` prototype that takes a prompt string and returns the final screen text + exit state for two programs.
  - **Result:** `spikes/pty-agent` (one driver: spawn, `vt100` screen, terminal-query answers,
    submit, snapshot, exit state) drove Freebuff 0.0.106-wrapper and Claude Code 2.1.306 through a
    real turn each, plus a deterministic `fake-tui.sh` double. The transport is generic; the
    **semantics are per-program** — submit key, answer region, completion sentinel, and startup
    dialogs all differ, so `transport = "pty"` must carry a profile rather than a parser guess.
    An idle timeout below 5 s truncated the answer in 4 of 5 real turns (largest mid-turn gap
    3.4 s, from the model thinking); 5 s never lied and added 5 s of latency, versus ~200 ms for a
    per-program sentinel. Both programs run on the alternate screen, so the screen must be captured
    before exit — after `quit` it is empty. Sending `text` and Enter in one write silently fails to
    submit in Freebuff and looks exactly like a completed turn. Both probe the terminal (`CSI ? u`,
    `CSI > 0 q`, plus `6 n`/`c`) and both gate the first prompt behind a dialog. Numbers, screens and
    commands: `docs/notes/pty-agents.md`. All runs were on WSL2 (kernel
    6.18.33.2-microsoft-standard-WSL2): a real Linux PTY, so these numbers say nothing about Windows
    ConPTY, which P0-5 owns.

- [ ] **P0-3 · gpui text benchmark** (2d)
  - Render a 100k-line Rust file in gpui, scroll, select, type, undo; build the screen against the `text_system` shaped-line API and a `uniform_list` row window rather than one giant element.
  - Measure: first paint, keystroke→paint, scroll frame time, and the cost of highlighting a 3k-line visible window with tree-sitter.
  - Also measure a minimal `iced` build of the same screen, so the fallback is priced rather than assumed.
  - Verify the pin actually builds clean from crates.io alone (no `zed/` checkout, no network beyond the lockfile): this is where the `zed-font-kit` git dependency and any missing feature will bite.
  - Write down the upstream gaps we hit in 0.2.2 (accessibility is known) as a list we and the GUI-builder project can push on together — that list, not a wish, is what justifies escalating to a git `rev`.
  - **Exit criteria:** numbers in `docs/notes/gui-bench.md` vs budgets (§Perf). If gpui misses by >2×, needs a nightly toolchain, or cannot be built from a clean vendored tree, fall back to `iced` 0.14 → Tauri **now** and say so in the ADR.

- [ ] **P0-4 · SSH PTY + agent spawn** (1d)
  - `russh` connect to a local `sshd` container, open a session with a PTY, run `bash -lc 'echo hi'`; then run an ACP agent over the channel.
  - **Answer:** does ACP over an SSH `exec` channel behave (line buffering, stderr, exit codes, resize)?
  - **Exit criteria:** `docker compose up sshd && cargo run --bin ssh-spike` prints a real ACP session from inside the container.

- [x] **P0-5 · WSL helper round-trip** (1d) — **done 2026-10-21**
  - On a Windows box (CI runner or VM): enumerate distros, `wsl.exe -d <d> --cd ~ -- ~/.local/bin/acpilotd-helper --stdio`, send one JSON line, get one back.
  - **Starting points**, measured 2026-10-21 from *inside* WSL2 on the dev box: `wsl.exe -l -q` works
    through interop (135 ms) but prints **UTF-16LE** — NUL-interleaved output that a naive line reader
    turns into garbage; the box has `Ubuntu-24.04` and `docker-desktop`. `wslpath` maps **two** things,
    not one prefix: `/home/sam` → `\\wsl.localhost\Ubuntu-24.04\home\sam`, while
    `C:\Users\SamTyler` → `/mnt/c/Users/SamTyler`. So the translation function is a pair of mappings
    (mounted drives ↔ `/mnt/<letter>`, and the distro root ↔ its UNC path), and the harness must decode UTF-16.
    The interop path does not cover ConPTY or `wsl.exe` invoked from Windows, which is what the helper
    actually ships against — that half still needs a Windows runner.
  - **Answer:** startup latency per call, path translation (`C:\x` ↔ `/mnt/c/x`), interop quirks.
  - **Exit criteria:** latency table + a path-translation function with unit tests that we can lift into `acpilot-host`.
  - **Result:** `spikes/wsl-helper` (Windows driver, `wsl.exe` 2.7.10.0) plus
    `helper/acpilotd-helper.rs` (dependency-free Linux end, installed with `rustc -O` through stdin).
    Measured on Windows 11 26300 / Ubuntu-24.04: one helper **spawn costs ~140 ms** to the first
    response and ~197 ms through exit, while a message on the warm session costs **0.1 ms** (p50, 30
    samples); a 1 MiB request line arrived intact in 1.3 ms. So the helper is a session, not a call:
    P2-2/P3-1 must spawn it once per host and multiplex ids. `wsl.exe -- <cmd>` goes through the user's
    shell and eats backslashes (missing binary → 127); `-e` execs directly and keeps them (missing
    binary → relay error, exit 1). `-e` does **not** expand `~` in the program path, so the helper path
    is resolved from `$HOME` and `--cd ~` is passed explicitly. `-l -q` is UTF-16LE+CRLF (as the
    starting points said); enumeration also returns `docker-desktop`, which is not a usable distro.
    Translation is two rules and only one is unconditional: `C:\x` ↔ `/mnt/c/x` for **mounted** drives
    only (`ls /mnt` gave `c` and `e` here; `D:` has no Linux path and `wslpath` refuses it too), and
    `/home/sam` ↔ `\\wsl.localhost\Ubuntu-24.04\home\sam`. `acpilot-host` gets `WslPathMap` from
    `spikes/wsl-helper/src/paths.rs` (12 tests, no Windows APIs), populated from `ls /mnt`, with an
    unmounted drive or a foreign distro as a typed refusal; `..` is left to the policy layer. Under
    ConPTY `wsl.exe` sends `CSI 6 n` and waits, so the helper never starts until DSR is answered — with
    an answer it works, echoing the request and ending lines with CRLF. Pipes are the transport for the
    JSON channel; ConPTY stays for terminals, where a query responder is mandatory anyway. Numbers,
    the `wslpath` oracle table and every quirk: `docs/notes/wsl-helper.md`. The suite runs on both
    sides: fmt/clippy/`cargo test` (23) under Windows and inside the distro, the helper's 4 unit tests
    with `rustc --test` on either OS, and `helper/check_protocol.py` against a native helper build in
    both. CI (`.github/workflows/ci.yml`, job `p0-5 spike`) runs that suite on both OSes on every push.
    The round trip is a second, opt-in job (`wsl-bench`) because WSL2 needs nested virtualisation, which
    standard hosted Windows runners do not have (they can offer WSL1 only); the bench reads the
    `VERSION` column from `wsl -l -v`, records `wsl_version` in its JSON, and `--require-wsl2` makes a
    WSL1 distro a hard failure instead of a quietly different table. Enable it with `WSL_BENCH=true`
    and `WSL_RUNNER` pointing at a self-hosted Windows runner with the `wsl2` label; it uploads
    `wsl-helper-bench` for the note's tables. The user story is measured end to end too
    (`wsl-helper workspace`): a folder opened as `\\wsl.localhost\Ubuntu-24.04\...` becomes
    `wsl://Ubuntu-24.04` + `/home/sam/workspace-other/acpilot`, the helper starts in that
    directory (cwd asserted) at ~124 ms with 0.1 ms per message, and the same 37 KB file
    costs **13.0 ms/read over UNC vs 0.096 ms in-distro (136×)** — so the daemon reads files
    in-distro and UNC stays a read-only fallback, as README §5.6 claims. Cold start after a
    reboot is **not** measured: terminating the distro would kill this checkout's own processes.

- [x] **P0-6 · API shape review (paper)** (0.5d) — **done 2026-10-21**
  - Write `docs/api.md` with the v1 endpoint list from the README, the SSE event taxonomy, and the error envelope. Review it against: CLI needs, TUI needs, permission answering from a script, web reconnect.
  - **Exit criteria:** endpoint list frozen for P3; anything missing from the README added there.
  - **Result:** `docs/api.md` freezes the v1 route list, the SSE taxonomy (`session`, `permission`,
    `heartbeat` and `resync` frames; `Last-Event-ID` replay from the transcript; a 15 s heartbeat;
    `resync` instead of silence when an id cannot be replayed), the prompt contract (`202` with
    `turn_id`, `idempotency_key`, `409` for a turn already in flight), the permission rules
    (idempotent for the same answer, `409` for a different one, no timeout in v1) and the error
    envelope with its HTTP mapping. The review found six gaps and fixed each rather than noting it:
    no way out of `AgentStatus::untrusted` (`POST /v1/agents/{id}/trust`), no host connection test,
    no profile activate/export, no directory listing (`GET /v1/files/list`), no profile-wide
    `GET /v1/events`, and no error envelope in the README at all. It also settled three names where
    two documents disagreed: `permissions/{id}/resolve` (not `/decide`), `/v1/lsp/servers`
    (not `/v1/lsp`), and the flat `/v1/terminals?session=` (not nested). Providers, models, recipes
    and the ACP bridge stay explicitly post-v1. The proto doc comments and the committed OpenAPI
    snapshot were corrected to the frozen names; those snapshot edits are description-only, so no
    wire type moved.

- [x] **P0-7 · LSP client round-trip + position encodings** (1d) — **done 2026-10-21**
  - Hand-roll `Content-Length` framing over stdio (it is ~150 lines), `initialize` advertising
    `general.positionEncodings = ["utf-8", "utf-16"]`, then `didOpen` a file containing emoji,
    CJK, and combining marks; assert a `definition`/`diagnostics` range lands on the right column.
  - Run `rust-analyzer`, `typescript-language-server`, and `pyright`: record which encoding each
    picks, whether pull diagnostics (`textDocument/diagnostic`) are supported, and what
    `$/progress` traffic arrives during the first index.
  - Try `async-lsp` 0.2 as the client loop (it supports `LanguageClient` + `ServerSocket`) and
    report what it costs to run it over a non-stdio transport.
  - **Answer:** UTF-16 vs UTF-8 conversion cost per keystroke; does any server send malformed
    URIs (Windows drive letters, percent-encoding, `file://` with a host)?
  - **Exit criteria:** `docs/notes/lsp-servers.md` capability table + a position-conversion
    function with proptests, liftable into `acpilot-lsp`.
  - **Result:** the conversion lives in `acpilot-lsp::position` (proptested over emoji, CJK,
    combining marks, CRLF) and says **negotiate both, honour the server, default to UTF-16**.
    rust-analyzer chose `utf-8`; `typescript-language-server` and `pyright` sent no
    `positionEncoding` and used UTF-16. Definitions landed on the expected byte in all three,
    including a path with a space and `ü`. Conversion is 18–28 ns; the whole-file index build
    (2.26 ms per 100k lines) is what must become incremental in P7. `rust-analyzer` answers
    pull diagnostics only after one `-32802` cancellation, and is the only server that sends
    substantial `$/progress` traffic (133 notifications for a one-file index). Measurements,
    commands and the `async-lsp` 0.2 assessment: `docs/notes/lsp-servers.md`.

---

## P1 — Workspace skeleton (≈3 days) — **done**

- [x] **P1-1** Workspace root layout per README §6; `Cargo.toml` with `resolver = "3"`, `edition = "2024"`, `rust-version = "1.95"` (raised from 1.94: `rusqlite_migration` 2.6 requires 1.95), and `[workspace.dependencies]` for every crate (single place to bump versions).
- [x] **P1-2** `rust-toolchain.toml` pinning **1.97.0**; `rustfmt.toml` with stable-only options (`edition`, `max_width`); `clippy.toml` with `msrv = "1.95"`. `group_imports = "StdExternalCrate"` was dropped: it is nightly-only, so on a stable toolchain it prints a warning and does nothing. Import grouping is a review convention (`CONTRIBUTING.md`) until rustfmt stabilises it.
- [x] **P1-3** `crates/acpilot-proto`: typed modules `host`, `workspace`, `agent`, `session`, `event`, `permission`, `file`, `terminal`, `profile`, `lsp`, `error`; every type `serde` + `utoipa::ToSchema`; `ErrorBody { code, message, details }`; `ApiDoc` registers them all.
- [x] **P1-4** `AcpilotError` in `acpilot-core::error` with the seven variants; `ApiError` newtype in `acpilotd` maps to `404/409/403/502/502/500/500`; `exit_code()` maps to `3/4/5/1`, with `USAGE = 2` owned by the parser. Both mappings are unit-tested.
- [x] **P1-5** `.github/workflows/ci.yml`: fmt, clippy `-D warnings`, nextest, docs-feature tests, `cargo-deny`, and an OpenAPI-snapshot freshness job, on linux+macos+windows. `.github/workflows/release.yml` is the stub until P12 wires `cargo-dist`. A separate `p0-5 spike` job checks `spikes/wsl-helper` on ubuntu+windows: it is deliberately not a workspace member, so the workspace commands cannot reach it.
- [x] **P1-6** `xtask`: `cargo xtask check` runs what CI runs; `dev`, `web-build` and `dist` exit 2 with the phase that owns them rather than pretending to work.
- [x] **P1-7** `.gitignore`, `LICENSE-MIT`, `LICENSE-APACHE`, `CONTRIBUTING.md`, `docs/adr/0001-record-architecture-decisions.md` (with the index of ADRs 0002–0009 to come).
- [x] **P1-8** `crates/acpilot-proto/tests/openapi_snapshot.rs` with `insta`; the 811-line snapshot is committed and CI fails if regenerating changes it.
  - **DoD:** `cargo xtask check` green; every crate builds; `GET /v1/openapi.json` served by the real daemon; Swagger UI at `/docs` behind the `docs` feature (assets from a CDN, so no build-time download and no third-party script in a default build).
  - **Verified locally (linux):** cross-OS runs are CI's job, not this machine's.

---

## P2 — ACP engine, sessions, permissions (≈12 days)

The heart. Headless: no daemon, no UI, tested against a **mock agent** so tests are fast
and deterministic.

- [ ] **P2-1** `acpilot-agent::registry`: load the active profile's `agents.toml` (P4-2), validate manifests (`transport ∈ {acp-stdio, pty, http-jsonrpc}`, required fields per transport), resolve `keyring:` secrets, expose `AgentSpec`. Hot-reload on file change.
- [ ] **P2-2** `acpilot-agent::supervisor`: spawn an `acp-stdio` agent via `agent-client-protocol` `process` feature; capture stderr into a ring buffer for the session log; restart policy (`never|on-crash`, max 3); clean process-tree kill (job object on Windows).
- [ ] **P2-3** `acpilot-agent::session`: wrap `initialize`/`session/new`/`session/load`/`session/prompt`/`session/cancel`; translate ACP `session/update` into our `SessionEvent` enum (`MessageChunk{role}`, `Thought`, `ToolCall{status}`, `Plan`, `ModeChanged`, `Notice`, `Compaction`, `CommandsChanged`) with stable sequence numbers.
- [ ] **P2-4** `acpilot-agent::fs`: implement ACP client-side `fs/read_text_file` and `fs/write_text_file` against the `Host` trait, with a **shadow-write guard**: writes go to disk only if the workspace policy allows, otherwise a pending grant is created and the call blocks.
- [ ] **P2-5** `acpilot-agent::terminal`: implement `terminal/create|output|wait_for_exit|kill|release` on top of `Host::spawn_pty`; enforce output ring buffer (1 MiB) and idle reaping.
- [ ] **P2-6** `acpilot-agent::permission`: the broker. `PermissionRequest { id, session, kind, title, detail, options }`; policy engine with rules (`allow`, `ask`, `deny` × tool glob × path glob × agent), default-deny for shell writes outside workspace; every decision appended to `audit` table.
- [ ] **P2-7** `acpilot-agent::pty_agent`: the program-as-agent adapter from P0-2, behind `transport = "pty"`; ship two built-in parsers (`plain-transcript`, `ansi-transcript`), a shipped `freebuff` profile as the reference case, and an escape hatch `parser = "script:<path>"` later.
- [ ] **P2-8** `acpilot-agent::transcript`: append-only `session.jsonl` (`events`, `grants`, `meta`), reader for replay, compaction into sqlite later.
- [ ] **P2-9** Test fixture: `crates/acpilot-agent/tests/fixtures/mock_agent.rs` — a tiny ACP agent (using the SDK's `Agent` trait) that can be told to stream N chunks, request a permission, request a file write, open a terminal, and hang. All P2 tests run against it in-process.
  - **DoD:** a `session_smoke` test that prompts the mock agent, receives streamed chunks in order, answers a permission, and asserts the grant appears in the audit log — under 200 ms.

---

## P3 — Daemon + API (≈10 days)

- [ ] **P3-1** `acpilotd` bootstrap: config (`figment`, layered per P4-2), `tracing` (JSON to file + pretty to stderr), `--foreground`, socket binding (`$XDG_RUNTIME_DIR/acpilot/acpilot.sock`, `\\.\pipe\acpilot-$USER` on Windows, TCP `127.0.0.1:7777` opt-in), pidfile + single-instance lock.
- [ ] **P3-2** axum router with the endpoints from `docs/api.md`; `tower-http` `TraceLayer`, `CompressionLayer`, `CorsLayer` (web only); `--token` bearer auth for non-loopback binds (`argon2` stored hash).
- [ ] **P3-3** Event bus: per-session `tokio::broadcast` + monotonic seq; `GET /v1/sessions/{id}/events` as SSE with `Last-Event-ID` replay from the transcript (reconnect-safe). Include a heartbeat every 15 s, and answer an un-replayable id with `event: resync` carrying the latest seq instead of pretending to catch up (`docs/api.md`). Pending permissions are state, not history: the client fetches `GET /v1/permissions?status=pending` on connect and the stream only announces new requests.
- [ ] **P3-4** `POST /v1/sessions/{id}/prompt`: idempotency key, returns `202` + `turn_id`; stream deltas on SSE. `POST .../cancel` maps to ACP `session/cancel`.
- [ ] **P3-5** Files API: `GET/PUT /v1/files` with workspace-relative paths, ETag (`mtime+size+hash`), `If-Match` for optimistic concurrency, 10 MiB cap, binary detection. Path traversal tests.
- [ ] **P3-6** Terminals API: create/attach (`WS /v1/terminals/{id}/attach`), resize (cols/rows), detach-without-kill, replay last screen on attach (from `vt100` state).
- [ ] **P3-7** SQLite: `migrations/0001_init.sql` (`workspaces`, `hosts`, `agents`, `sessions`, `events`, `grants`, `terminals`); `rusqlite` with `bundled`; WAL; `PRAGMA foreign_keys=ON`; migration runner on boot; retention job (events older than 30 days compacted). One database
  per profile (P4-4), so migrations run once per open profile — keep them idempotent.
- [ ] **P3-8** Hosts API: `POST /v1/hosts` (`local|ssh|wsl`), connection test endpoint, capability report (can spawn? can read fs? latency).
  - WSL specifics, from P0-5's `workspace` measurement (`docs/notes/wsl-helper.md`): discovery is one `wsl.exe -l -q`, and **absence must be silent** — no distro, a failing `wsl.exe`, or an empty list means no `wsl://` host, no prompt, no error; drop `docker-desktop`. That rule is already written and tested in the spike (`spikes/wsl-helper/src/discovery.rs`, five real-process tests on both OSes), so P3-8 should reuse the code rather than restate the rule. A folder opened as `\\wsl.localhost\<distro>\...` becomes `wsl://<distro>` plus a Linux cwd (infer the distro from the path, don't ask). Spawn one helper per host and multiplex ids (0.1 ms per message vs ~125 ms per spawn), always with `--cd <linux path>` and an absolute helper path. Read files in-distro: the same 37 KB file costs 13.0 ms over UNC and 0.096 ms inside (136×), so UNC is for read-only browsing only.
- [ ] **P3-9** `acpilotd-helper`: a 300-line stdio JSON-RPC shim that the daemon drops into a remote/WSL host and reuses for fs+PTY+spawn+LSP, so remote behaviour is identical to local. Static musl build, embedded in the daemon binary (`include_bytes!`), uploaded on connect with a version handshake.
- [ ] **P3-10** Tests: `axum` route tests with `tower::ServiceExt::oneshot`; a live-server test that opens SSE and asserts ordered delivery; `insta` snapshots of all error bodies.
  - **DoD:** `acpilotd` runs headless, a scripted `curl` session (create → prompt → stream → permission → resolve) works, and SSE replay after disconnect loses no events.

---

## P4 — Profiles & preferences (≈10 days)

A **profile** is a named identity bundle — `work`, `personal`, `client-acme` — with its own
agents, credentials, language servers, policy, and preferences. Profiles exist because those
things are *context*-scoped, not machine-scoped: the key you use for a client is not the one
you use on a weekend, and silently mixing them is the worst failure mode this product has.

Three rules:

1. **A profile is a directory.** Every mutable path resolves through one function; nothing
   else in the workspace calls `directories`.
2. **Resolution is explicit and loud.** A name that does not resolve is a hard error, never a
   fallback.
3. **Isolation by construction.** Separate secrets namespace, separate SQLite, separate
   sessions. Cross-profile access needs an opt-in and leaves an audit line.

- [ ] **P4-1 · `acpilot-profile` crate** (1.5d)
  - `ProfileId` newtype (slug `[a-z0-9][a-z0-9-]{1,31}`, case-insensitive input, reserved
    `default|global|system`); `Profile { id, label, extends, created_at }`.
  - `ProfileRoots { config, data, cache, state, runtime }` via `directories`: Linux
    `~/.config/acpilot/profiles/<id>/` + `~/.local/share/acpilot/profiles/<id>/`; macOS
    `Application Support/Acpilot/profiles/<id>/`; Windows `%APPDATA%\\acpilot\\profiles\\<id>\\`.
  - `ACPILOT_HOME` override for tests, portable installs, and CI; every test uses a temp root.
  - Global registry `config.toml`: `schema_version`, `active_profile`, `[daemon]` settings, one
    table per profile. No secrets in it, ever.

- [ ] **P4-2 · Config cascade** (1.5d)
  - Per-profile files: `profile.toml` (preferences), `agents.toml`, `lsp.toml`, `policy.toml`,
    `keymap.toml`, `theme.toml`.
  - Layer order (`figment`): built-in defaults < global `config.toml` < profile files <
    workspace `.acpilot/config.toml` < `ACPILOT_*` env < CLI flags.
  - Merge is **per key**, not whole-file: arrays replace, except `disabled`/`deny` which union;
    `extends` merges beneath the child (max depth 4, cycles rejected).
  - Unknown keys warn once (so a newer config on an older binary still runs); invalid values
    fail with file, line, and key. `acpilot config explain <key>` prints the winner and its origin.
  - [decide] D13: full `extends` inheritance vs copy-on-create. Recommend `extends` for
    `agents`/`policy`/`lsp`, copy for preferences (easier to reason about, harder to surprise).

- [ ] **P4-3 · Resolution order + workspace marker** (0.5d)
  - `--profile` > `ACPILOT_PROFILE` > `.acpilot/profile` (committed, one line, no secrets) >
    `config.toml:active_profile` > `default` (auto-created on first run).
  - A marker naming a missing profile exits `3` with the exact `acpilot profile new <name>`
    hint. Never fall back: falling back means sending work prompts with personal keys.
  - Table-driven precedence matrix, including "flag and marker disagree" and "marker inside a
    profile-less checkout".

- [ ] **P4-4 · Profile-scoped state** (1d)
  - One SQLite per profile (`<data>/profile.sqlite`) with the P3-7 schema, plus a small global
    `index.sqlite` (`profiles`, `workspaces(profile, path, first_seen)`).
  - Transcripts under `<data>/sessions/`. `profile rm` moves the directory to the OS trash and
    prints the path; `--purge` is required to delete outright.
  - Daemon opens stores lazily and keeps at most 4 open (LRU) with per-store migration state.
  - [decide] D11: DB-per-profile (recommended) vs one DB with a `profile` column.
    `acpilot sessions ls --all` iterates stores sequentially in the first case; budget ≤50 ms
    for 10 profiles, otherwise build the index.

- [ ] **P4-5 · Per-profile secrets** (1d)
  - Keyring service `acpilot`, account `<profile>/<name>`; `keyring:anthropic` resolves in the
    active profile only.
  - Cross-profile refs (`keyring:work/anthropic`) need `allow_cross_profile = true` on that
    agent and log a warning on every resolution.
  - First run adopts legacy unscoped entries into `default`. `profile export` redacts secrets;
    `--with-secrets` additionally requires `--i-understand-the-risk` and a `0600` output file.
  - Test: the same env var name in two profiles resolves to two values, and neither appears in
    the other profile's child process environment.

- [ ] **P4-6 · Profiles in the domain** (1.5d)
  - `Workspace.profile` and `Session.profile`; every lookup takes a profile as an argument.
    `acpilot-core` has no ambient "current profile" — it is resolved at the edge and passed down.
  - Workspace owned by another profile → `409 profile_mismatch` with `details.owner`.
    `acpilot workspace open --profile other <path>` offers `--steal` (explicit, audited).
  - Agent children get exactly their profile's environment, verified by a test that runs two
    profiles' mock agents concurrently and diffs their `env`.
  - Hosts are global; a profile may override per-host identity (`user`, `identity_file`,
    `ProxyJump`) but not the hostname — one machine, several identities.

- [ ] **P4-7 · Preferences schema + live reload** (1d)
  - `Preferences { schema_version, editor, theme, keymap, ui, lsp, agents }`, typed in
    `acpilot-profile::prefs` and exported to `acpilot-proto` for the API.
  - Defaults are data (a checked-in `defaults.toml`), so adding a preference is a one-line change
    in one file rather than four UIs.
  - Daemon watches profile files (`notify`, 200 ms debounce) and emits `profile.changed` /
    `prefs.changed`; UIs apply theme, keymap, and format-on-save without restart. A syntax error
    keeps the last good config and raises a notice.
  - Preferences that must not fork per UI: `tab_width`, `insert_spaces`, `format_on_save`,
    `trim_trailing_whitespace`, `wrap`, `rulers`, `diagnostics.min_severity`.

- [ ] **P4-8 · API** (1d)
  - `GET/POST /v1/profiles`, `GET/PATCH/DELETE /v1/profiles/{id}`,
    `POST /v1/profiles/{id}/activate`, `GET/PATCH /v1/profiles/{id}/preferences`,
    `POST /v1/profiles/{id}/export`.
  - Every other route accepts `X-Acpilot-Profile`. Missing header on a **mutating** request is
    `400` — no guessing; missing on a read means the active profile. SSE subscriptions bind to a
    profile at connect; a global `GET /v1/events` carries only `profile.*` and `daemon.*`.
  - Generated TS/Rust clients take `profile` in their constructor, so forgetting it is a compile
    error, not a runtime surprise.

- [ ] **P4-9 · CLI** (0.5d)
  - `acpilot profile new|ls|show|use|rm|export|import|diff`; global `--profile`, accepted before
    or after the subcommand.
  - `acpilot profile diff a b` prints the effective merged config with per-key origin — the tool
    that answers "why is it doing that?".
  - `acpilot doctor` reports the resolved profile, its roots, and every unknown key.

- [ ] **P4-10 · Tests** (0.5d)
  - Precedence matrix; missing-profile marker; cross-profile denial; keyring namespacing;
    `profile diff` snapshot; concurrent profiles without env/cwd bleed; export redaction.
  - **DoD:** `acpilot --profile work agents list` and `--profile personal agents list` differ in
    agents *and* in resolved keys; a `work` session is invisible to `personal` (404, not an empty
    list); an explicit cross-profile request returns `409` with the owner named; `profile diff`
    explains every effective value; deleting one profile leaves the others byte-identical
    (directory hash before/after).

---

## P4.5 — Local models and the model lab (≈14 days)

Numbered `P4.5` so every existing task id stays valid: this lands after profiles (recipes
are per-profile data, and credentials are keyring entries) and before the CLI grows the
commands that drive it. Decisions and consequences: `docs/adr/0002-local-models-and-the-model-lab.md`.

The shape in one paragraph: a **provider** is an HTTP chat endpoint; a **recipe** is a tuned
configuration for one model of one provider; the **lab** is a UI over the API that edits
recipes and records runs; **promotion** writes a recipe into `agents.toml` as an ordinary
`acp-stdio` agent that runs `acpilot-agent-lm`. The model therefore never touches the disk:
the shim reaches files, terminals, and permissions through ACP callbacks, exactly like
`claude-code-acp` does. No new trust boundary, no second permission implementation.

LM Studio is the day-one provider, because it reports what a lab needs: `GET /api/v0/models`
gives `type`, `arch`, `quantization`, `state`, and `max_context_length`, and
`POST /api/v0/chat/completions` returns `stats` (tok/s, time to first token) and `model_info`.
Ollama, llama.cpp, vLLM, and OpenRouter ride the same `open_ai_compatible` adapter; Anthropic
and Gemini are not OpenAI-shaped and get their own variant when the adapter exists.

- [ ] **P4.5-1 · `acpilot-model` crate** (2d) — `Backend`, both adapters, and the refusal retry
  landed with 17 tests in this crate; provider registration and the daemon route are still open.
  - `ChatBackend` trait: `models()`, `probe()`, `chat(request) -> stream/response`, all
    cancel-safe and timeout-bounded (2 s connect, 10 s discovery, no timeout on generation
    beyond the HTTP client's idle read timeout).
  - `LmStudioBackend` on the native `/api/v0` surface; `OpenAiCompatibleBackend` on `/v1`.
    The difference is deliberate: fewer facts means fewer gauges, not invented ones.
  - Errors map to `AcpilotError::ModelUnavailable` (`502 model_unavailable`, exit `1`): the
    daemon is fine, the inference server behind it is not, which is what lets a UI offer "retry"
    instead of "give up". A model id the server does not have is `404 not_found`; a rejected
    token is `403 permission_denied`; no retries on a 4xx. The variant is added in this phase
    (it is the one failure the P1 enum could not describe without lying).

- [ ] **P4.5-2 · Discovery and capability probing** (1.5d) — discovery mapping and
  `Backend::probe` landed (`crates/acpilot-model/src/lib.rs`); the CLI verb and the probe cache
  are still open.
  - Map `/api/v0/models` fields into `DiscoveredModel` (`llm`→`chat`, `vlm`→`vision`,
    `embeddings`→`embedding`, `state`→`loaded`, `max_context_length`→`context_window` with
    `context_source = server`). Where a server says nothing, `context_source = unknown` and the
    field stays null — the daemon never assumes 4096 or 8192.
  - Probe on explicit action only (`acpilot models probe`, a button in the lab); no polling, no
    startup scan, and an unreachable provider is reported, not retried in the background.
  - Capability probe sends one tool-shaped request and one JSON-schema request against the
    loaded model and records what came back in `ModelCapabilities`. A model that claims tools
    and then emits prose gets `tools: false` with the evidence in `detail`.

- [ ] **P4.5-3 · Recipes** (1d)
  - `profiles/<name>/recipes/<id>.toml`, `schema_version = 1`, unknown keys are errors (same
    rule as config and manifests). Hot-reload on change, 200 ms debounce, last-good kept on a
    parse failure with a notice.
  - Validation at load, not at run: `window` must not exceed the model's reported window; a tool
    the model cannot call is refused with an explanation rather than silently omitted;
    `max_steps` in `1..=50`; `reserve_output + max_corpus_bytes_estimate` must fit.

- [ ] **P4.5-4 · Context assembly** (2d)
  - Budget split — system prompt, corpus, history, tool schemas, reserved output — with a
    per-section estimate; corpus from `CorpusSelector` globs honouring `max_files`/`max_bytes`,
    `ignore` rules, and a stable order so a run is reproducible.
  - `ContextStrategy::{TruncateOldest, Summarize, Refuse}`; overflow is an event with the numbers
    attached, never a silent trim. Summarisation uses the same model and is labelled as such in
    the prompt, because a summarised history is a different history.
  - Token counting is an estimate until the server answers: pre-flight uses a bytes/4 heuristic
    with a documented error band, and every `RunSummary` reconciles against `usage` and sets
    `TokenUsage.estimated = false` only then.

- [ ] **P4.5-5 · `acpilot-agent-lm`** (3d)
  - A small ACP agent binary: reads a recipe, runs the model loop, and reaches the world only
    through ACP client callbacks (`fs/read_text_file`, `fs/write_text_file`, `terminal/*`,
    `session/request_permission`). It holds no filesystem handle of its own.
  - Tool schema generated from `ToolExposure`; strict parsing of tool calls with one repair
    attempt, then a `Notice` event; `max_steps` enforced with a clear stop reason; `session/cancel`
    aborts the HTTP stream immediately rather than after the next chunk.
  - Streams `SessionEvent`-shaped updates so the transcript, the TUI, and the web UI need no
    model-specific code. Usage and stop reason are reported on the final update.

- [ ] **P4.5-6 · Run records** (1d)
  - Every run appended to `runs.jsonl` next to the session transcript: exact request (prompt hash,
    sampling, corpus selectors and resolved file list, budget), then the result (latency, first
    token, usage, stop reason, warnings).
  - `GET /v1/recipes/{id}/runs` paginates; `POST /v1/recipes/{id}/runs/{run}/replay` re-sends the
    stored request unchanged. Compare is a diff of two records, so "it was better an hour ago"
    has an answer.

- [ ] **P4.5-7 · API** (1.5d)
  - `GET/POST /v1/providers`, `PATCH/DELETE /v1/providers/{id}`, `POST /v1/providers/{id}/probe`,
    `GET /v1/models`, `GET/POST /v1/recipes`, `GET/PATCH/DELETE /v1/recipes/{id}`,
    `POST /v1/recipes/{id}/runs` (202 + SSE stream), `GET /v1/recipes/{id}/runs`,
    `POST /v1/recipes/{id}/promote`.
  - SSE gains `model.run.started`, `model.run.delta`, `model.run.finished`, `model.provider.status`.
    OpenAPI snapshot updated in the same PR.

- [ ] **P4.5-8 · The lab UI** (2.5d, TUI first; web and desktop reuse the API)
  - Three panes: recipe (master prompt, sampling sliders, context budget, tool ceiling), corpus
    (globs, resolved file list with sizes, budget meter), output (streaming text, run history).
  - Gauges show only what the provider reported: first token, tok/s, stop reason, usage. Estimated
    tokens are drawn differently from reported ones, and the difference is stated in words.
  - "Effective request" inspector: the exact JSON that was sent, with any parameter the server
    refused listed beside it. Debugging a local model is mostly finding out which knob the
    server dropped — and Acpilot can only report refusals, not silent disregard, so it says so.
  - Compare mode: two runs side by side with a token and latency delta; promote is one keystroke
    from here, and it warns first.

- [ ] **P4.5-9 · Promotion** (1d)
  - Write the `agents.toml` entry (`transport = "acp-stdio"`, `command = "acpilot-agent-lm"`,
    `args = ["--recipe", "<id>"]`, `env = { ACPILOT_RECIPE_PROFILE = "work" }`), with a comment
    naming the recipe file and its mtime, and refuse to overwrite a hand-written entry.
  - Warn — not block — when the recipe's model never passed the tools probe, when tool-call
    validity is below 90% across its last ten runs, or when the window exceeds host memory.
  - `acpilot agents rm <id>` reverses it exactly; a promoted agent is a manifest line, not a
    migration.

- [ ] **P4.5-10 · ACP bridge for remote drivers** (2d)
  - `acpilotd` serves ACP over stdio to a local client and over HTTP/JSON-RPC (bearer token,
    `argon2`-hashed, plus a single-use pairing code, two-minute expiry) to a remote one.
    Off unless `[acp.bridge] enabled = true` with an explicit bind address; loopback may skip
    the token, anything else may not.
  - `initialize` returns our capabilities and records the client name/version. Sessions opened
    by a driver record the driver id, so a transcript can answer "who asked for this?".
  - Scopes are intersected with the grant: `observe`, `prompt`, `cancel` by default;
    `answer_permissions` requires an explicit per-driver grant. Without it, a permission request
    from a driver-started turn is routed to the human's attached clients and the driver's call
    blocks — with a `Notice` explaining why, so it is never a mysterious hang.
  - Rate limits per driver, 0600 token file, TLS assumed to be terminated by a tunnel or reverse
    proxy in v0.1 (documented, not hand-waved), and `GET /v1/acp/{bridge,drivers}` for the
    cockpit's connection panel.

- [ ] **P4.5-11 · Tests** (1.5d)
  - Mock OpenAI-compatible server: model list, streaming chat, tool calls, a server that ignores
    `top_k`, a server that 500s mid-stream, a server that never finishes. Snapshot the exact
    request bodies so a param regression is a diff.
  - Bridge tests: expired code, wrong token, scope escalation refused, driver cannot answer a
    permission without the grant, revoked driver's next call fails with `401`.
  - **DoD:** `acpilot recipes run <id> "…"` streams from a real LM Studio on port 1234, the run
    record reproduces byte-identically on replay, `promote` produces an agent that passes the P2
    session smoke test, and a scripted ACP client paired as `devin` can prompt that agent while
    the permission prompt appears in the TUI and not in the script.

- [ ] **P4.5-12 · Integration stats** (1d) — the shared contract landed:
  `acpilot_proto::stats::IntegrationStats` covers providers and agents, every figure carries
  `StatSource` (`server`, `configured`, `estimated`, `unknown`), and
  `acpilot_core::stats::{context_fill, credit_fill}` is the tested fill math both UIs use.
  Still open: `GET /v1/stats`, the collector that fills it, and provider balance reads.
  - Context: the window and usage come from the provider when it reports them (LM Studio
    `max_context_length`, `usage`) and from the session transcript (marked `estimated`)
    otherwise; no default window is ever substituted.
  - Credits: read from a provider that exposes a balance endpoint (for example OpenRouter's
    `/api/v1/credits`) or from a configured plan; a provider with no such endpoint shows no
    credits rather than zero. Local servers show a context bar and no credit bar.
  - One row per integration, including quiet ones, so the bar does not reflow when a provider
    stops answering; `updated_at` makes a stale reading visible.

- [ ] **P4.5-13 · Skills** (2d) — `acpilot-skills` parses Agent Skills (`SKILL.md`: YAML
  frontmatter plus instructions) and resolves workspace `.acpilot/skills/<name>/` over an
  extension's `skills/<name>/`, with the name/directory/description limits enforced and 12
  tests over temporary trees. Still open:
  - a `skills` contribution kind in extension manifests and activation, so a pack can ship them
    and a profile decides whether they exist;
  - the creator/editor in the model lab: write `SKILL.md`, validate before saving, preview the
    instructions, and choose workspace or profile as the destination;
  - selection in a recipe/session, and how the instructions reach the agent — system prompt for
    model-backed agents, an ACP attachment for external ones — without pretending every agent
    has a native skill mechanism.

---

## P4.6 — Extensions: the mod shop (≈11 days)

Numbered after the model lab for the same reason: no existing task id moves. Decisions and the
argument against WASM-everything: `docs/adr/0003-extensions-are-data-first.md`.

The shape: a **declarative** extension is a directory with `extension.toml` and assets — a theme,
language-server presets, agent presets, grammars, snippets, a keymap layer. It is installed once
into a content-addressed store and **activated per profile**; activation never rewrites your
config, it changes what the loader sees, so `profile diff` can still explain every value. The
registry is a signed static index reviewed by a human, with mandatory checksums and no
auto-update. Code extensions (transcript parsers, tokenizers) are Tier 2 and deliberately not in
v0.1 (D18).

- [x] **P4.6-1 · Manifest and loader** (1.5d) — done: `crates/acpilot-extensions`, 24 tests
  - `extension.toml`: `schema_version`, `id` (`publisher.id`), `name`, `version`, `description`,
    `publisher`, `homepage`, `min_acpilot`, and `[[contributes]]` entries with `kind` + path.
  - Unknown keys are errors; ids are namespaced so an extension cannot shadow a built-in; a
    contribution whose path escapes the extension directory is rejected before anything is read.
  - Loading is a pure function of the directory: no install-time script, no network.

- [x] **P4.6-2 · The built-ins become extensions** (2d) — done: `extensions/`, embedded with `include_str!`
  - Extract the two themes (`night-flight`, `paper`), the ten LSP presets, and the four agent
    presets out of Rust and into `extensions/` shipped with the binary, loaded through the same
    loader as anything installed. If the schema is wrong, the first build says so.
  - Built-ins are marked `built_in` and cannot be uninstalled, only shadowed by id namespacing.

- [x] **P4.6-3 · Store and verification** (1.5d) — done: `store.rs`, `ext add|verify|prune`
  - `~/.local/share/acpilot/extensions/<id>/<version>/` extracted atomically to a temp path,
    hashed (SHA-256 over a sorted file list), then renamed. A mismatch is `Blocked`, never a
    warning, and a blocked extension is invisible to every loader.
  - `acpilot ext verify --all` rehashes the store; `acpilot ext prune` removes versions no profile
    activates.

- [ ] **P4.6-4 · Registry: signed static index** (1.5d)
  - The curated list itself exists: `marketplace/index.toml`, reviewed by pull request; `acpilot ext add --from`
    installs. Downloading from a listing, checksums from the index, and the signature remain.
  - `index.json` (`{id, version, kind, checksum, size, publisher, min_acpilot, download}`) plus a
    detached signature. The maintainer key ships in the binary; `registry.trusted_keys` adds a
    private one. Signature failure is a hard error with the key id named.
  - `acpilot ext search|add|update|remove|list|show`; `--from <dir>` installs a path link for
    development; `--index <url>` for a private registry. No fetch on startup, ever.

- [x] **P4.6-5 · Activation and precedence** (1d) — done: `activation.rs`, `effective.rs`, `ext why`; the daemon's `extension.changed` event waits for P4.6-10
  - Per-profile `extensions.toml` activation list; precedence built-ins < activated extensions <
    profile's own config. `acpilot ext why rust-analyzer` prints the chain that produced the
    effective preset, reusing the `profile diff` origin machinery.
  - Deactivating restores the previous answer without a restart; the daemon emits
    `extension.changed` and every attached UI re-reads the registries.

- [ ] **P4.6-6 · Themes as data** (1.5d)
  - Token schema (`bg`, `bg_panel`, `text`, `text_muted`, `accent`, `accent_high`, `border`,
    `success`, `warning`, `danger`, plus syntax classes), shared by TUI, GUI, and web; hot switch
    via `prefs.changed`.
  - CI runs a contrast check over every theme token pair and fails below AA, because "it looked
    fine on my monitor" is how a theme ships unreadable. **Done with P4.6-1**: 34 pairs per
    theme (17 foreground tokens x 2 surfaces), `built_in_themes_meet_wcag_aa`. Hot switch and the
    TUI/GUI/web consumers remain.

- [ ] **P4.6-7 · Presets that point at programs** (1d)
  - A `language_servers` or `agents` contribution merges into the typed registries; a command it
    introduces is trust-pinned on first run (P7-12) exactly like a hand-written one, and the pin
    UI names the extension so the question is "trust rust-analyzer from acpilot.rust-pack?" rather
    than a bare binary path.

- [ ] **P4.6-8 · Grammars** (2d)
  - Prebuilt tree-sitter `.wasm` + `highlights.scm`/`indents.scm`, loaded lazily on first file of
    that language, with a parse-time budget and a visible "grammar missing" fallback rather than
    plain text and no explanation.

- [ ] **P4.6-9 · Snippets and keymap layers** (0.5d)
  - Snippets in the editor's existing JSON format; keymap layers applied above the built-in one
    and below the user's own `keymap.toml`, with conflicts reported by `acpilot keys check`.

- [ ] **P4.6-10 · API and CLI** (1d)
  - `GET /v1/extensions`, `GET /v1/extensions/{id}`, `POST /v1/extensions`,
    `POST /v1/extensions/{id}/activate|deactivate|verify`, `DELETE /v1/extensions/{id}`,
    `GET /v1/themes`; SSE `extension.*`. CLI mirrors it, `--json` everywhere.

- [ ] **P4.6-11 · Tests** (1d)
  - Manifest matrix (missing fields, unknown keys, path traversal, id collision); checksum mismatch
    → `Blocked` and invisible; signature failure; install→activate→use→deactivate→use cycle;
    precedence snapshots; theme contrast; a grammar that fails to parse degrades visibly.
  - **DoD:** `acpilot ext add acpilot.night-flight-pack`, activate it in `work`, and both themes
    plus the Rust preset appear in the TUI with no config edit; `acpilot ext why rust-analyzer`
    names the extension and the manifest version; a tampered extension is refused by name.
  - Status: manifest matrix, checksum/store, activation cycle, precedence, contrast, and the CLI
    verbs are covered; signature failure arrives with P4.6-4, grammar degradation with P4.6-8.

- [ ] **P4.6-12 · Import from other editors** (future, not v0.1)
  - TextMate `.tmTheme` is done (`acpilot ext import-theme`). VS Code (JSON themes, node
    extensions) and Zed's marketplace (server + WASM code extensions) are deliberately future
    work: importing a code-bearing extension means either translating its declarative parts or
    growing a runtime, which is the decision in ADR 0003. Start with VS Code themes, which are
    data, and keep the curated list reviewed by pull request.

---

## P5 — CLI (≈6 days)

- [ ] **P5-1** `acpilot-cli` skeleton: `clap` derive, global `--json`, `--api-url`, `--token`, `--config`, `--profile`; client discovery of a running daemon (socket then TCP), auto-spawn daemon (`--no-spawn` to disable) with a 2 s readiness wait.
- [ ] **P5-2** Commands: `host add|ls|test|rm`, `workspace open|ls|rm`, `agents list|refresh|show`,
  `session new|ls|show|rm|fork`, `prompt <text> [--session <id|@alias>] [--file <path>] [--follow]`,
  `watch <session>` (follow SSE, render markdown to the terminal),
  `permissions ls|allow|deny|pending`, `files ls|cat|write`, `term attach`,
  `profile new|ls|show|use|rm|export|import|diff`, `lsp ls|restart|logs|status`,
  `diagnostics [--workspace] [--severity]`, `format <path...> [--check]`,
  `api <METHOD> <PATH> [--body]`, `tui`, `gui`, `serve`, `doctor`.
- [ ] **P5-3** Output layer: human tables/colour (`anstyle`), `--json` (stable, documented as an API), `--no-color`, `NO_COLOR`, `TERM=dumb` fallbacks.
- [ ] **P5-4** Completions + man pages generated at build time (`clap_complete`, `clap_mangen`) and shipped in release archives.
- [ ] **P5-5** `acpilot doctor`: toolchain checks, agent commands resolvable, keyring access, daemon reachable, host latency, WSL/ssh config parse results.
- [ ] **P5-6** Exit-code contract documented in `--help` and tested: `0` ok, `1` general, `2` usage, `3` not found, `4` permission denied, `5` host/agent failure.
- [ ] **P5-7** Integration tests via `assert_cmd` + a temp daemon; snapshot help output.
  - **DoD:** every API endpoint reachable from the CLI; `acpilot prompt` streams a real agent's answer to stdout and exits non-zero on a failed turn.

---

## P6 — TUI editor (≈25 days)

The largest single chunk. Editor first, agent pane second, but both in one binary.

- [ ] **P6-1** `acpilot-text`: rope buffer (`ropey`), `Selection`/`Cursor` sets, `Edit` ops (insert/delete/replace/indent), undo/redo grouping by 500 ms or word boundary, line endings + BOM preservation, `proptest` invariants (apply→undo == identity; no panics on random UTF-8).
- [ ] **P6-2** `acpilot-text::highlight`: tree-sitter parse per buffer, incremental reparse on edit, `HighlightSpan` extraction for a visible line range only; grammar registry with lazy load; theme model (shared with GUI) loaded from TOML.
- [ ] **P6-3** TUI shell: ratatui `Layout` with editor pane, tab strip, status line (profile, mode, position), agent sidebar, transcript pane, command palette (`Ctrl-P`), which-key hints.
- [ ] **P6-4** Editor pane: virtualised line rendering (only visible rows), horizontal scroll, gutter with line numbers + diff markers, current-line highlight, bracket matching, search-in-file (`/`), go-to-line.
- [ ] **P6-5** Keymap layer: Vim (normal/insert/visual, `hjkl`, motions, `:` commands) and a VS Code-ish default; `keymap.toml` overrides; `?` shows effective bindings.
- [ ] **P6-6** File tree + fuzzy open (`ignore` + `nucleo` matcher), git status decorations, workspace-level grep (`grep-searcher`), results in a jump list.
- [ ] **P6-7** Agent panel: session picker, live transcript (markdown → styled lines, collapsible tool calls), streaming cursor, token/cost line, cancel keybinding, prompt composer with multiline editing and `@file` completion.
- [ ] **P6-8** Permission modal: title, tool, diff preview for writes, `once|session|always|deny` choices, keyboard-only, audited. Must render even while the transcript is streaming.
- [ ] **P6-9** Diff review: split/inline diff of agent edits (`imara-diff`), accept/reject hunks → `fs/write_text_file` decision or revert.
- [ ] **P6-10** Terminal pane: `tui-term` attachment to session PTYs; `Ctrl-T` to open; scrollback; copy mode with OSC 52 clipboard fallback for SSH.
- [ ] **P6-11** Embedded mode: `acpilot tui` works with **no daemon** (in-process core) and attaches to a daemon when one exists — same code path, different `Host`/client impl. Guard with a trait so behaviour cannot fork.
- [ ] **P6-12** Mouse support (optional), focus cycling, resize handling, `--no-mouse`, and a 60 fps budget on a 200×60 terminal.
- [ ] **P6-13** Integration stats bar, shared by TUI, GUI and web: one row per provider and agent,
  a context bar (`used/window`) and a credit bar (`used/limit`, or `remaining`), drawn from
  `IntegrationStats` with `acpilot_core::stats` for the fill. Unknown denominators render as an
  indeterminate track, never full or empty; estimated figures are styled differently and the
  source is one keystroke away. TUI snapshots via `ratatui::TestBackend`, GUI in P10, web in P11.
  - **DoD:** open a 100k-line file, edit with undo, open a session with Claude, stream a turn, answer a permission, accept a hunk, run `cargo test` in the terminal pane — all without leaving the TUI.

---

## P6.5 — Git and the commit writer (≈6 days)

Numbered after P6 so no existing task id moves. The backend and the prompt logic land first;
what needs an editor shell waits for the editor shell.

- [x] **P6.5-1 · `acpilot-git`** — `status` over `--porcelain=v2 -z` (branch, upstream,
  ahead/behind, both sides of each change, renames), scoped diffs with a patch and a file list,
  a working-tree listing, and `log` with parents for a graph. It shells out to the user's `git`,
  so config, filters, worktrees, submodules and signing stay consistent with the command line.
  Nine tests run against real temporary repositories, including a staged rename, an unborn
  repository, and a subdirectory discovery.
- [ ] **P6.5-2 · API surface** — `GET /v1/git/status`, `/diff?scope=`, `/tree`, `/log?limit=`;
  proto types mirroring `acpilot-git`; `404 not_found` for "not a repository" and a clear
  `host_unavailable` when `git` is missing on the host.
- [ ] **P6.5-3 · Commit-message generation** — `acpilot_core::commit` has the editable default
  prompt (Conventional Commits with the gitmoji inside the description), the gitmoji table, and
  strict validation of the model's answer. Still open: per-profile prompt storage, a route that
  runs either a model-lab recipe or an active agent session, and a draft the editor can change
  before committing.
  - The generator is selectable per request; the staged diff is what it sees, and nothing is
    committed until the user confirms.
  - Reject rather than repair: an unknown type, a missing description, a subject over 72
    characters, or a body without a blank line is a validation error, not something the daemon
    silently rewrites.
- [ ] **P6.5-4 · TUI** — source-control pane with the changed-file tree and diff, plus a commit
  composer whose message can be generated then edited. `git` writes stay a separate, confirmed
  action; the pane never stages or commits on its own.
- [ ] **P6.5-5 · Desktop and web** — the same two panes over the same API and the same diff text;
  no second diff implementation.
- [ ] **P6.5-6 · Tests** — route tests over a fixture repository, snapshot diffs, and commit
  generation exercised without a network call.

---

## P7 — Language servers (≈19 days)

LSP lives **in the daemon**, next to the files and the agents — not in the UIs. Three reasons:
servers have to run where the code lives (SSH, WSL); one server per root must be shared by four
clients; and the TUI, GUI, and browser must show the *same* diagnostics. The UIs are clients of
`/v1/lsp/*`, exactly like they are clients of sessions.

Minimum for v0.1: P7-1…P7-10, P7-14 and P7-15 (local servers only). P7-11 rides on P8/P9.
P7-13 is the first thing to slip to v0.2 if the phase runs long; P7-12 does not slip — an
untrusted binary with your repo and your network is exactly the hole worth closing early.

- [ ] **P7-1 · Wire types + URI mapping** (1d)
  - `acpilot-lsp::wire` re-exports **`gen-lsp-types` 0.11** with the `url` feature (URIs as
    `url::Url`, which is what servers actually send). `lsp-types` 0.97 is unmaintained and moved
    to `fluent-uri`, which trades interoperability for spec purity — see D8.
  - One module boundary: no `gen_lsp_types` path appears outside `acpilot-lsp::wire`, so the type
    crate can be swapped in a day if it stalls.
  - `uri_for(host, path)` / `path_for(host, uri)` handling drive letters, UNC, percent-encoding,
    and non-UTF-8 paths, with round-trip proptests.

- [ ] **P7-2 · Framing + request loop** (1.5d)
  - `Content-Length` codec over `Host` stdio (or the helper's channel remotely): request table,
    `$/cancelRequest`, per-request timeouts, out-of-order responses, `$/progress`,
    `window/workDoneProgress/create`, and server→client requests with their own handlers.
  - **Keep it separate from ACP's codec:** ACP is newline-delimited JSON-RPC, LSP is header-framed.
    Sharing the transport abstraction is right; sharing the framing is not.
  - `async-lsp` 0.2 stays an option until P0-7 reports; the default is our own loop because it has
    to run over SSH channels, WSL pipes, and a WS bridge to the browser.

- [ ] **P7-3 · Server registry (`lsp.toml`)** (1d)
  - Declarative entries `{ id, command, args, filetypes, root_markers, initialization_options,
    settings, env }`, per profile (P4-2) and per workspace.
  - ~10 built-in presets: rust-analyzer, typescript-language-server, gopls, pyright, clangd,
    lua-language-server, bash-language-server, taplo, yaml-language-server, marksman. Presets are
    data files, not code, so the community can fix them without a release.
  - **No auto-download in v0.1.** `acpilot lsp status` shows what was found on `PATH`, what is
    missing, and the exact install command; [decide] D12 whether v0.2 bundles installers.

- [ ] **P7-4 · Supervisor** (1d)
  - Spawn via `Host`; `initialize` advertising only capabilities we implement; `initialized`;
    `shutdown`/`exit` on teardown (then kill after 2 s). Idle shutdown after 10 min, restart with
    backoff and a crash-loop cap, stderr ring buffer per server (`acpilot lsp logs`).
  - One server per `(profile, host, root, id)`, shared by every buffer under that root, including
    buffers opened in a different UI. Reference-counted against open documents.

- [ ] **P7-5 · Document sync** (2d)
  - `didOpen`/`didChange`/`didClose`/`didSave` driven by daemon file state; full vs incremental
    per server capability; monotonic versions per URI.
  - `ropey` ↔ LSP `Position` with the **negotiated encoding** (D9): propose `utf-8`, fall back to
    `utf-16` (the default every server supports). Byte offsets that assume UTF-8 will silently
    drift on any emoji, CJK character, or combining mark — proptest this with surrogate pairs.
  - Agent writes arrive as `didChange`, never `didClose`+`didOpen`, so the server keeps its
    analysis (and its cache) across an agent edit.
  - `workspace/didChangeWatchedFiles` fed from `notify` for external edits, `git checkout`, and
    generated files; debounce 300 ms and coalesce per root.

- [ ] **P7-6 · Diagnostics store** (1.5d)
  - `publishDiagnostics` plus the pull model (`textDocument/diagnostic`, `workspace/diagnostic`)
    where advertised; keyed by `(uri, version)`, stale-marked rather than deleted on version skew.
  - Severity, tags (unnecessary/deprecated), code + source, related information, and a stable sort
    so the list does not jump while typing.
  - Caps: 5k per file, 50k per workspace, with a truncation notice. In-memory with a latest
    snapshot for late-joining UIs — diagnostics are recomputed, so persisting them is waste.

- [ ] **P7-7 · Requests and features for v0.1** (2d)
  - hover, completion (+`completionItem/resolve`), signature help, definition/declaration/
    typeDefinition/implementation, references, document symbols, workspace symbols, rename +
    `prepareRename`, code actions (+resolve), formatting/range formatting, code lens, inlay hints,
    folding ranges, semantic tokens (full + delta), selection ranges.
  - Every one is a typed endpoint, cancellable, and debounced independently: completion 150 ms,
    hover 250 ms. Nothing LSP-related may run on the keystroke path — a slow server must never
    make typing feel slow.

- [ ] **P7-8 · Server→client side effects** (1d)
  - `workspace/applyEdit` and `workspace/executeCommand` (organize imports, "add missing import",
    quick fixes) go through **the same write path and permission broker as agent edits**: preview
    as a diff, allow/deny by policy, audit the decision. A code action is a file edit; treating it
    as a trusted side channel would be a hole in the model.
  - `window/showMessage(Request)` → daemon notice + UI toast; `workspace/configuration` answered
    from the profile's `lsp.toml`; dynamic capability registration honoured.

- [ ] **P7-9 · API + events** (1d)
  - `GET /v1/lsp/servers`, `POST /v1/lsp/servers/{id}/restart|stop`,
    `GET /v1/lsp/servers/{id}/logs`, `GET /v1/lsp/diagnostics?workspace=&severity=`,
    `POST /v1/lsp/request { server, method, params }` (escape hatch for servers ahead of us),
    plus typed routes for P7-7.
  - SSE: `lsp.diagnostics`, `lsp.status`, `lsp.progress`, `lsp.message`. Snapshot-tested OpenAPI,
    and a documented WS bridge shape so the web UI can attach `@codemirror/lsp-client` directly.

- [ ] **P7-10 · Save pipeline** (1d)
  - Format-on-save (off by default, per language), format selection, organize-imports-on-save.
  - Writes are serialised per file: the daemon formats *before* acknowledging a save and returns
    the formatted text plus a diff so the UI can rebase its buffer instead of clobbering it.
  - `acpilot format <paths>` and `--check` for CI. Document the interaction with agent writes:
    last writer wins, but both are diffed and both are audited.

- [ ] **P7-11 · Remote language servers** (1d, lands with P8/P9)
  - Over SSH and WSL the server runs **on the host**, spawned through `acpilotd-helper` alongside
    fs/PTY/spawn. Pointing a local server at remote files is unsupported, not merely slow: wrong
    toolchain, wrong paths, wrong import roots.
  - `rootUri` comes from the helper; path↔URI mapping is host-aware; the capability report shows
    `lsp: remote` and per-server round-trip latency.

- [ ] **P7-12 · Trust gate** (0.5d)
  - A language server is an arbitrary executable with your source and your network. First use of a
    new binary in a workspace prompts once and hash-pins it; `policy.toml` can pre-approve;
    `acpilot lsp status` shows the resolved binary path and hash.
  - Untrusted workspaces spawn no servers until approved. Generated code and `build.rs`-triggered
    execution are the exact threat this blocks.

- [ ] **P7-13 · Agents can read diagnostics** (1d)
  - `acpilotd mcp` exposes `lsp_diagnostics`, `lsp_hover`, `lsp_definition` as MCP tools (via
    `agent-client-protocol-rmcp`), so an agent can ask "what does the compiler say" instead of
    re-running `cargo check` blind. Opt-in per session, redacted like any other context.
  - This is the part no other editor can offer: the same daemon that brokers the agent's edits also
    holds the language server's opinion of them.

- [ ] **P7-14 · TUI client** (2d, depends P6)
  - Diagnostics: gutter signs, underlines at the right byte ranges, problems panel, `]d`/`[d`
    navigation. Hover popup, async completion popup, definition jump with a back-jump stack,
    references panel, rename prompt, code-action menu, format-on-save indicator, and
    server status/progress in the statusline. `:lsp restart` and `:lsp logs`.
  - All of it through the API: the TUI binary contains zero LSP code.

- [ ] **P7-15 · Tests + benchmarks** (1.5d)
  - In-process mock LSP server fixture: progress, cancellation, server→client requests, malformed
    frames, crash mid-request, and a server that never answers.
  - Nightly against real rust-analyzer and typescript-language-server; snapshot a diagnostics set
    per fixture file so encoding regressions surface as diffs.
  - Budgets: diagnostics ≤200 ms p95 after an edit on a 100k-line repo, completion ≤300 ms p95,
    hover ≤200 ms p95, and a hard assertion that no LSP call happens inside a keystroke handler.
  - **DoD:** open a Rust workspace, see rust-analyzer diagnostics inline, hover/definition/
    completion/rename/format-on-save all work from the TUI, a quick fix that rewrites a file is
    previewed and goes through the permission broker, and `acpilot lsp status` reports a healthy
    server with its log one flag away.

---

## P8 — SSH hosts (≈10 days)

- [ ] **P8-1** `acpilot-host::ssh`: `SshHost` on `russh` — connect (agent auth, key files, password via keyring, `known_hosts` verification with TOFU prompt), keepalive, reconnect with backoff.
- [ ] **P8-2** `ssh2-config` parsing: `Host`/`HostName`/`User`/`Port`/`IdentityFile`/`ProxyJump`/`Include`/`ForwardAgent`; surface unknown directives as warnings in `acpilot host test`.
- [ ] **P8-3** Capabilities over SSH: upload `acpilotd-helper` (musl, `scp`-less — write via `exec` + base64 or SFTP), version handshake, then fs/PTY/spawn/LSP through the helper.
- [ ] **P8-4** Remote workspace: root path on remote, file read/write with ETag, `notify`-equivalent via helper polling interval (500 ms) until inotify-over-SSH exists.
- [ ] **P8-5** Remote agent spawn: ACP over an SSH `exec` channel (stdio bridged), stderr captured separately, resize forwarded, clean kill on cancel.
- [ ] **P8-6** Remote terminal: PTY request (`xterm-256color`, current size), resize messages, reconnect that replays the last screen.
- [ ] **P8-7** Fallbacks: `ssh` binary mode via `openssh` crate for configs russh cannot express (FIDO, `Match`, ControlMaster); auto-fallback with a single log line.
- [ ] **P8-8** Tests: docker-compose `sshd` fixture; tests for agent auth, bad host key, dropped connection mid-turn, and a 200 MB file read.
  - **DoD:** edit a file and run an agent on a remote box from the TUI, with the daemon local; kill the network mid-turn and recover.

---

## P9 — Windows + WSL (≈8 days)

- [ ] **P9-1** Windows build green in CI (MSVC), ConPTY verified via `portable-pty` for both local and WSL terminals.
- [ ] **P9-2** `acpilot-host::wsl`: distro enumeration (`wsl.exe -l -q`, registry via `winreg` as fallback), default distro detection, `wsl --cd` support, path translation both ways (`C:\a` ↔ `/mnt/c/a`, `\\wsl.localhost\D\a` ↔ `/a`).
- [ ] **P9-3** In-distro helper: install `acpilotd-helper` into `~/.local/bin` inside the distro on first use, handshake, then all fs/PTY/spawn/LSP happens in-distro (see README §5.6 design note).
- [ ] **P9-4** WSL terminal pane + WSL agent over the helper's stdio; verify interactive TUIs (colours, resize, Ctrl-C) work through ConPTY.
- [ ] **P9-5** UNC fallback for read-only browsing; explicit warning in the UI when editing over 9P.
- [ ] **P9-6** Windows process hygiene: job objects for agent trees, named-pipe single instance, `\\?\` long-path handling, CRLF preservation.
  - **DoD:** on a clean Windows VM, `acpilot workspace open \\wsl.localhost\Ubuntu\home\me\app`, run an agent inside the distro, edit a file, and see the change from `wsl` — no manual setup beyond `wsl --install`.

---

## P10 — GUI (≈25 days)

- [ ] **P10-1** Spike outcome review from P0-3; write `docs/adr/0007-gui-framework.md` with the decision, the pinned `gpui` rev/version, the numbers that justify it, and the recorded trigger that would force the iced fallback.
- [ ] **P10-2** `acpilot-gui` shell on gpui 0.2.2: `Application` + `Entity`/`View` tree, window and menu bar, theme tokens (shared `theme.toml` with TUI), profile switcher, command palette, panes, and keyboard-first navigation via gpui `KeyBinding` contexts (same keymap data as the TUI, not a second hand-written table).
- [ ] **P10-3** Text surface on `acpilot-text`: shaped `LineLayout` wrapping, display map for folds, virtualised rows (`uniform_list`), cursor blink, IME, selection, soft-wrap toggle, minimap (optional/later).
- [ ] **P10-4** Panels: file tree, tabs, editor, agent session list + transcript + composer, permission dialogs, diff review, LSP surfaces (diagnostics, hover, completion, code actions — from the daemon, same as the TUI), terminal (`vt100` grid rendered as a gpui element, reusing the TUI's grid model).
- [ ] **P10-5** System integration: menu bar, open-with, recent workspaces, drag-and-drop files, notifications on turn completion/needs-input, dock/taskbar badge, single instance + deep links (`acpilot://session/<id>`).
- [ ] **P10-6** Performance pass against the budgets in §Performance budgets; `tracing` spans + a frame-timing overlay (gpui has no off-the-shelf integration, so measure in-crate and keep the harness alongside the editor).
- [ ] **P10-7** Accessibility — **not inherited**: gpui 0.2.2 ships no `accesskit` bridge, so this is an implementation task, not an audit. Wire focus order, labels, and role/live-region events for the transcript and permission dialog; 200% scaling; high-contrast theme. Timebox a spike early in P10: if the bridge is not viable by then, ship TUI-parity keyboard paths, document the gap in the release notes, and treat it as a v0.1 limitation rather than silently claiming accessibility.
- [ ] **P10-8** Packaging: `.app` bundle + notarisation (macOS), MSI/MSIX (Windows), AppImage + `.deb` (Linux) via `cargo-dist` + `cargo-bundle`.
  - **DoD:** a user who never opens a terminal can install Acpilot, open a workspace, run an agent, and approve a write.

---

## P11 — Web UI (≈12 days)

- [ ] **P11-1** `web/`: Vite + Svelte 5 + TS; layout mirrors TUI panels; profile switcher bound to `X-Acpilot-Profile`; router for `/p/:profile/w/:workspace`, `/p/:profile/s/:session`.
- [ ] **P11-2** API client generated by `openapi-typescript` into `web/src/api/schema.d.ts`; `openapi-fetch` wrapper that injects the token; CI check that regeneration is a no-op.
- [ ] **P11-3** CodeMirror 6 editor: rope-equivalent view over the files API (`If-Match` saves), tree-sitter-wasm highlighting (fall back to `@codemirror/lang-*` if wasm load is slow), diff via `@codemirror/merge`. Attach **`@codemirror/lsp-client` 6.x** through the daemon's WS bridge: the browser never spawns a server, and diagnostics match the TUI byte for byte.
- [ ] **P11-4** Session view: SSE-driven transcript, streaming markdown, tool-call cards, permission prompts (optimistic resolve + reconcile), cancel.
- [ ] **P11-5** Terminal: xterm.js attach over WS, fit + webgl addons, resize, copy/paste, reconnect replay.
- [ ] **P11-6** `acpilot-web` crate: embed built assets (`rust-embed`), `GET /` serves the SPA, `acpilot serve --http --bind 0.0.0.0 --token` with a first-run token printed once; CSRF-safe (token in header only), strict CSP.
- [ ] **P11-7** E2E with Playwright: open workspace → edit → prompt mock agent → approve permission → see diff. Runs in CI against the daemon.
  - **DoD:** from a laptop browser, drive a session running on the dev box over the LAN with a token, with reconnect surviving a daemon restart.

---

## P12 — Release v0.1 (≈6 days)

- [ ] **P12-1** `cargo-dist` config: tarballs (linux x86_64/aarch64 musl+gnu, macos arm64/x86_64, windows x86_64), shell + PowerShell installers, Homebrew tap, checksums + SBOM (`cargo-cyclonedx`).
- [ ] **P12-2** Versioning (`0.1.0`), `CHANGELOG.md` via `git-cliff`, release workflow with signed tags, reproducible builds documented.
- [ ] **P12-3** Docs site (`docs/`): quickstart per interface, agent manifest reference, profile + config reference (`docs/config.md`, cascade and origin), host setup (ssh/wsl), policy reference, LSP server setup (`docs/lsp.md`), API reference from OpenAPI, troubleshooting (`acpilot doctor` output explained).
- [ ] **P12-4** First-run experience: `acpilot setup` creates the `default` profile, detects agents on PATH (`claude`, `codex`, `gemini`, `freebuff`) and language servers, writes that profile's `agents.toml` + `lsp.toml`, tests one handshake, prints next steps.
- [ ] **P12-5** Telemetry: **none** by default; optional crash reports via `sentry` behind an explicit flag and a documented payload.
- [ ] **P12-6** Security review: token handling, path traversal, permission bypass attempts, command injection in manifests, LSP binary trust pins (P7-12), cross-profile secret leakage (P4-5), helper upload integrity (signature or hash pin), `cargo audit` clean.
- [ ] **P12-7** Migration story: config/state paths stable from 0.1 (`~/.config/acpilot`, `~/.local/share/acpilot`, one subtree per profile), documented, with a tested upgrade path from a pre-profile layout.

---

## Cross-cutting work (runs alongside phases)

### Testing

- [x] Wire contract: `crates/acpilot-proto/tests/wire_contract.rs` parses and re-serialises
every schema and enum spelling in the crate, so a renamed field or a changed enum value
fails with the type's name. The OpenAPI snapshot covers the document; this covers the
serialiser.
- [ ] Unit tests per crate. Every crate with behaviour has them: `acpilot-core` (error
  contract, stats math), `acpilot-cli` (unit plus two process-level suites),
  `acpilot-extensions` (unit plus five integration suites), `acpilot-lsp` (position, framing,
  client, probe), `acpilot-model` (unit plus a mock provider), `acpilot-proto` (wire
  contract plus OpenAPI snapshot) and `acpilotd` (unit plus HTTP). `acpilot-agent`,
  `acpilot-gui`, `acpilot-host`, `acpilot-profile`, `acpilot-text`, `acpilot-tui`,
  `acpilot-web` and `acpilotd-helper` are empty stubs by design; their tests land with the
  phase that gives them code.
- [ ] `insta` snapshots for transcripts and CLI help.
- [ ] Mock ACP agent (P2-9) is the default fixture; one nightly job against real agents (network, secrets-gated).
- [ ] Mock LSP server (P7-15) as the default fixture; rust-analyzer + typescript-language-server nightly; position-encoding proptests on every CI run.
- [ ] Profile precedence matrix and keyring isolation run on all three OSes (P4-10).
- [ ] Official ACP **test compatibility kit** in CI once published as a binary; assert we pass as a client.
- [ ] `proptest` for buffer invariants and path translation (WSL/SSH).
- [x] CLI: process-level scripting tests in `crates/acpilot-cli/tests/` (`ext.rs`, `cli.rs`):
  exit codes `0/1/2/3/4/5`, stderr for usage errors, `ACPILOT_PROFILE`, help snapshots.
- [x] API: route tests through the router (`tower::ServiceExt::oneshot`) plus a live-socket
  test that binds an ephemeral port and uses a real HTTP client (`acpilotd/tests/http.rs`).
- [x] Commit-time hooks: `.pre-commit-config.yaml` for prek runs fmt, clippy `-D warnings`,
  the nextest workspace, the `docs`-feature test, and `cargo deny` — the CI commands, plus the
  jobs `xtask check` does not cover yet. Install with `prek install`.
- [x] Git: `acpilot-git` is tested against real temporary repositories (status, scoped diffs,
  renames, tree, log, unborn HEAD, discovery), and the commit prompt/validation in
  `acpilot-core` has unit tests, including fences and every rejection path.
- [x] Skills: `acpilot-skills` is tested over temporary workspace/extension trees: parsing,
  instructions, name/directory agreement, limits, duplicate handling, and workspace precedence.
- [ ] Web: Playwright in **P11**, against `acpilot serve` with a temp profile — no mocked routes.
- [ ] TUI: headless snapshots via `ratatui`'s `TestBackend` in **P6**, including the stats bar
  at known dimensions and a 200×60 frame budget.
- [ ] GUI: a gpui test harness in **P10** (frame + text layout assertions), added with the crate.
- [ ] SSE: a live-server suite that opens the stream, drops it, reconnects with `Last-Event-ID`,
  and asserts ordered replay with no gaps (**P3**).

### Dependencies & upgrades

- [ ] **XT-1** Continuous updates: automation (Dependabot or Renovate) opens a PR the day a release lands — patch/minor auto-merge on green CI, majors get a human. `cargo update` weekly even when no manifest changes, because the lockfile is what ships. Every upgrade runs the P0-3 harness, so a performance regression fails the PR instead of surfacing months later. gpui is the one exact pin (D1) and still moves within a week of a release; when the GUI-builder project starts, both repos move to one `rev` and share one patch series.
- [ ] **XT-2** Reproducible GUI builds: `cargo vendor` (or a registry mirror) exercised in CI from a clean checkout with no `~/.cargo/git` cache — the `zed-font-kit` git dependency means a naive build silently depends on GitHub.
- [ ] **XT-3** Dependency budget: one PR per new direct dependency, with licence + maintenance check; a `cargo deny` config in CI so a surprise transitive licence is a build failure, not a discovery.

### Performance budgets (measured in CI on a reference runner)

| Metric | Budget |
| --- | --- |
| Keystroke → paint (GUI/TUI) | ≤ 16 ms p95 |
| Open 100k-line file | ≤ 150 ms to first paint |
| TUI cold start | ≤ 80 ms |
| Daemon idle RSS | ≤ 60 MB (no sessions) |
| Session event latency (agent → UI) | ≤ 50 ms p95 local, ≤ 150 ms SSH |
| Memory per idle session | ≤ 15 MB |
| 1 M-line grep in 50k files | ≤ 3 s |
| LSP diagnostics after an edit (100k-line repo) | ≤ 200 ms p95 |
| LSP completion round-trip | ≤ 300 ms p95 |
| LSP hover | ≤ 200 ms p95 |
| Profile switch on a warm daemon | ≤ 100 ms |
| Lab overhead per run, excluding model time | ≤ 25 ms p95 |
| Recipe edit → next run accepted (window open) | ≤ 50 ms |
| Context assembly, 50 files / 2 MB corpus | ≤ 120 ms |

### Security

- [ ] Every filesystem/terminal/permission path goes through the broker; no direct agent access to the OS outside policy.
- [ ] Language servers are programs, not libraries: trust-pinned on first use, no auto-download (P7-12), restricted to the workspace root.
- [ ] Profiles are isolation boundaries: separate keyring namespace, separate stores, `409` on cross-profile access, audited `--steal`.
- [ ] Default policy: read anywhere in workspace, ask on write, deny outside workspace, deny network-spawning tools (configurable).
- [ ] Audit log is append-only and included in `session.jsonl`.
- [ ] Secrets: keyring only; redact secret-looking strings from logs and transcripts (`sk-`, `ghp_`, bearer tokens).
- [ ] Sandbox spike (later): `landlock` (Linux), `sandbox-exec` (macOS), AppContainer (Windows) per-session; [decide] whether v0.2.

### Accessibility & i18n

- [ ] Keyboard-only operation for every feature in every UI; no drag-only affordances.
- [ ] Screen-reader labels for transcript, permission dialogs, and status. GUI uses accesskit; TUI writes plain, ordered text.
- [ ] All user-facing strings in `crates/*/i18n/en.ftl` (Fluent) from day one — cheap now, expensive later.

### Documentation

- [x] Docs site (`site/`): Astro + Starlight, custom theme, status pills per page, link checker in the Docs workflow. Deploy is gated on `DOCS_DEPLOY` until the repository is public (Pages is not available for private repos on this plan).
- [ ] ADRs: protocol v1-only vs v2 draft, GUI framework, CRDT vs LWW, wasm vs TS web, daemon auth model, LSP wire types + position encoding (D8/D9), profile isolation model (D10).
- [ ] `docs/api.md` kept in sync with `utoipa` output (CI diff).

---

## Dependency manifest (starting point)

```toml
[workspace.dependencies]
# protocol
agent-client-protocol      = { version = "3.4", features = ["process", "stdio"] }
agent-client-protocol-schema = "1.12"  # versioned separately from the 3.x runtime — do not assume the majors match
# runtime
tokio                      = { version = "1.54", features = ["full"] }
futures                    = "0.3"
# api
axum                       = "0.8"
axum-extra                 = "0.12"
tower                      = "0.5"
tower-http                 = { version = "0.7", features = ["trace", "compression-full", "cors"] }
utoipa                     = { version = "6", features = ["axum_extras"] }
# cli
clap                       = { version = "4.6", features = ["derive", "env", "wrap_help"] }
clap_complete              = "4.6"
clap_mangen                = "0.3"
reqwest                    = { version = "0.13", default-features = false, features = ["rustls", "http2", "json", "stream"] }  # 0.13 renamed rustls-tls -> rustls
eventsource-stream         = "0.2"
# tui
ratatui                    = "0.30"
crossterm                  = "0.29"
tui-term                   = "0.3"
vt100                      = "0.18"
# gui
gpui                       = "0.2.2"           # exact pin; see D1 + XT-1. Escalate to { git = "…/zed", rev = "<sha>" } only with an ADR note.
# gui fallback (P0-3 veto): iced = "0.14"
# editor
ropey                      = "1.6"
tree-sitter                = "0.27"
tree-sitter-highlight      = "0.27"
imara-diff                 = "0.2"
similar                    = "3"
ignore                     = "0.4"
grep-searcher              = "0.1"
grep-regex                 = "0.1"
lsp-types                  = "0.97"          # fallback only; see D8
gen-lsp-types              = { version = "0.11", features = ["url"] }
async-lsp                  = "0.2"           # spike candidate only; see P7-2
url                        = "2"
# hosts
russh                      = "0.64"
russh-sftp                 = "3"
ssh2-config                = "0.9"
openssh                    = "0.11"          # optional fallback
portable-pty               = "0.9"
# storage & config
rusqlite                   = { version = "0.40", features = ["bundled"] }
rusqlite_migration         = "2"
figment                    = "0.10"
keyring                    = "4.2"
directories                = "6"
# misc
serde                      = { version = "1", features = ["derive"] }
serde_json                 = "1"
schemars                   = "1"
toml                       = "1.1"
uuid                       = { version = "1", features = ["v7", "serde"] }
jiff                       = "0.2"
notify                     = "8"
arboard                    = "3.7"
pulldown-cmark             = "0.13"
nucleo                     = "0.5"
thiserror                  = "2"
anyhow                     = "1"
miette                     = "7"
tracing                    = "0.1"
tracing-subscriber         = { version = "0.3", features = ["json", "env-filter"] }
tracing-appender           = "0.2"
argon2                     = "0.6"
[workspace.dependencies.windows]
version = "0.62"
features = ["Win32_Foundation", "Win32_System_JobObjects", "Win32_System_Threading"]
```

Every direct dependency above resolves from crates.io as written — verified on 2026-10-20 by
extracting this block and running `cargo generate-lockfile`. That check is what caught three
stale entries (`agent-client-protocol-schema` is **1.12**, not 3.x; `windows` is **0.62**, not
0.6; `argon2` is **0.6**) and reqwest's renamed TLS feature. Re-run it after any edit here;
the block is a build input, not prose.

`agent-client-protocol` v3 enables features explicitly (none are on by default in v3).
Web-only deps live in `web/package.json` (`@codemirror/lsp-client` 6.x, `@codemirror/merge`,
`@xterm/xterm`). Version policy and the current exceptions list: README §5.9.

---

## Risks and mitigations

| Risk | Impact | Mitigation |
| --- | --- | --- |
| GUI framework churn (gpui/iced) | Losing months | P0-3 spike with hard numbers, UI-agnostic `acpilot-text`, bounded fallback path |
| gpui has no semver contract | A routine upgrade eats a week; a moved rev breaks the build | Exact pin, vendored builds in CI (XT-2), release-watch automation (XT-1), ADR records pin + escape hatch |
| gpui ships no accessibility bridge | Screen-reader users locked out; retrofit arrives too late | P10-7 is an implementation task with an early viability spike, keyboard-complete TUI parity, gap documented if unresolved |
| ACP v2 draft lands with breaking changes | Rework in the engine | Isolate all protocol types in `acpilot-agent`; never leak ACP types into `acpilot-proto` |
| Agents are non-deterministic; tests flake | Slow CI, mistrust | Mock agent for tests, real agents nightly only, transcript snapshots |
| Remote file watching over SSH is polling | Stale UI, battery drain | 500 ms coalesced poll, ETag revalidation, explicit "remote" indicator, inotify-over-ssh later |
| Program-as-agent parsers are brittle | "Run anything" promise breaks | Ship 2 generic parsers, make profiles data-driven and user-editable, keep ACP-native path primary |
| Permission broker bugs = data loss | Trust destroyed | Default-deny writes, shadow-write guard, audit log, property tests for path canonicalisation |
| Scope creep across four UIs | Never ships | API-first rule; v0.1 is CLI+TUI only; GUI/web gated on daemon stability |
| LSP servers hang, crash, or index forever | Editor feels broken; RAM spikes | Timeouts + `$/cancelRequest` everywhere, per-server stderr ring, idle shutdown, crash-loop cap, nothing on the keystroke path |
| Profile mix-up sends code to the wrong account | Broken trust, possibly a client confidentiality incident | Keyring namespacing, hard error on unresolved profile, no fallback, cross-profile refs opt-in + audited, `profile diff` in support docs |
| Local models are bad at tool calls | A promoted agent loops, or calls nothing and explains why | Capability probe (P4.5-2), validity metric across runs, promotion warns below 90%, `max_steps` cap, repair-once parsing with an honest notice |
| A server lies about its context window | Silent truncation, or a 400 mid-run | `MetadataSource` on every window, recipe validation against the reported value, overflow is an event with numbers, `estimated` flag on pre-flight token counts |
| The model lab becomes a second product | Four UIs, one lab, endless drift | The lab is an API client like everything else (`POST /v1/recipes/{id}/runs` + SSE); the recipe is a file; the agent is a manifest line |
| A remote driver approves its own writes | The one promise the product makes, broken by a cloud agent | `answer_permissions` off by default and granted per driver; unanswered requests route to human clients; every driver action audited with its identity |
| Bridge exposure on a LAN | Remote code execution with a nice UI | Off unless configured, bearer token + hashed storage, single-use pairing code with a 2-minute expiry, scopes intersected, TLS documented as the operator's tunnel in v0.1 |
| A grammar in an extension is still executable wasm | Code execution inside the editor's parser runtime | Reviewed index, pinned checksum, lazy load, parse budget; the docs say "declarative" is narrower, not inert |
| The extension index becomes a bottleneck | Good extensions wait on a maintainer | Small, reviewable submissions; private registry supported via `--index` + trusted key; the schema is public so packs can be self-hosted |
| Themes drift across four UIs | An extension that looks wrong in one cockpit | One token schema, hot-switched through `prefs.changed`, contrast checked in CI |
| Extension id collision shadows a built-in | Silently different behaviour than the docs describe | Namespaced ids (`publisher.id`), collisions are errors, built-ins cannot be overridden by name |

## Open decisions ([decide])

- [x] D1 · GUI framework: **gpui 0.2.2** (decided 2026-10-20 — a sibling GUI-builder project will use gpui, so framework knowledge and pins are shared; the text stack and the reference ACP client come for free). P0-3 can still veto: fallback `iced` 0.14 → Tauri.
- [ ] D2 · Multi-client concurrency model: last-write-wins + ETag conflict UI (v0.1) vs CRDT (`loro`/`automerge`) later.
- [ ] D3 · Web frontend: Svelte 5 + CodeMirror (recommended) vs Leptos/WASM.
- [ ] D4 · Local IPC: HTTP over unix socket (recommended, one protocol everywhere) vs a second JSON-RPC transport.
- [ ] D5 · Daemon auth for LAN exposure: static token (v0.1) vs OIDC device flow.
- [ ] D6 · Licensing of agent manifest schema: Apache-2.0 so other editors can adopt it.
- [ ] D7 · Whether `acpilot` ships as one binary with subcommands (recommended) or separate `acpilot`/`acpilotd` binaries everywhere.
- [ ] D8 · LSP wire types: `gen-lsp-types` 0.11 with the `url` feature (recommended) vs `ls-types` 0.0.6 (3.18 proposals) vs `lsp-types` 0.97 (unmaintained, `fluent-uri`). Isolated behind `acpilot-lsp::wire` either way.
- [x] D9 · Position encoding: **advertise `utf-8` + `utf-16`, use the server's answer, default to UTF-16 when it says nothing** (decided 2026-10-21 on P0-7's numbers). UTF-8 saves 10 ns per conversion and is what rust-analyzer picks; always-UTF-16 would be wrong for it, and assuming UTF-8 breaks the two servers that advertise nothing. Index rebuild, not conversion, is the cost that matters. See `docs/notes/lsp-servers.md`.
- [ ] D10 · Profile isolation: one daemon with N stores (recommended — profiles must run concurrently) vs one daemon per profile (stronger blast-radius, worse UX).
- [ ] D11 · Profile storage: database per profile (recommended) vs single database with a `profile` column.
- [ ] D12 · Language-server installation: detect-only in v0.1 (recommended) vs managed downloads with checksums.
- [ ] D13 · Profile inheritance: `extends` merge vs copy-on-create only.
- [x] D14 · Model adapter seam: **one `ChatBackend` trait, `LmStudio` native plus `OpenAiCompatible`, new variants only with their adapter** (decided 2026-10-20, ADR 0002). Anthropic/Gemini are not OpenAI-shaped and are explicitly out of v0.1.
- [x] D15 · Remote driver trust: **ACP bridge with scopes, `answer_permissions` off by default, permission prompts route to a human** (decided 2026-10-20, ADR 0002). TLS termination is the operator's tunnel in v0.1.
- [ ] D16 · Token counting: bytes/4 heuristic with a documented error band (recommended for v0.1) vs bundling a tokenizer per model family. The lab's `estimated` flag exists so this can change without rewriting the UI.
- [x] D17 · Extension registry trust: **signed static index, mandatory SHA-256, reviewed pull requests, no auto-update** (decided 2026-10-20, ADR 0003). A private registry is `--index` plus a trusted key.
- [ ] D18 · Extension code execution: **Tier 2 (wasmtime, ABI 1, declared capabilities) proposed, not v0.1** (ADR 0003). The open question is the capability set and the resource budget, not whether to have one.
- [x] D19 · Platform order: **unix and macOS first, Windows last, and every OS branch under `src/platform/`** (decided 2026-10-21). The Windows half is the one that cannot be developed on the machines the project is written on, so writing it first means writing it blind. `acpilot-host` now has the seam: `Host::spawn` is portable (`std::process`) with one implementation, `spawn_pty` branches once into `platform/unix.rs` (forkpty via `portable-pty`) and `platform/windows.rs` (a typed refusal naming P2-5 until ConPTY lands). The rule is enforced, not remembered: a boundary test fails if `cfg(windows)`/`cfg(unix)`/`cfg(target_os…)` appears outside `src/platform/` in any crate, and `cargo xtask check` type-checks the crate for the windows-msvc and both apple targets from any host.

## Definition of done for v0.1.0

- [ ] `acpilot setup` → `acpilot tui` on macOS, Linux, Windows; open a workspace, edit, save.
- [ ] One native ACP agent runs a full turn with streaming, tool calls, and an approved write.
- [ ] One non-ACP program runs as an agent through the PTY adapter.
- [ ] Same session reachable from a second terminal via `acpilot watch`.
- [ ] Remote SSH workspace and WSL workspace both work from the TUI.
- [ ] Permission policy denies an out-of-workspace write by default and records the decision.
- [ ] Language servers attach to a workspace: diagnostics, completion, hover, rename, format-on-save; a code action that edits a file is previewed and permissioned.
- [ ] Two profiles (`work`, `personal`) run agents side by side with different agents, keys, and policies; neither sees the other's sessions or history.
- [ ] All performance budgets met in CI; `acpilot doctor` clean on all three OSes.
- [ ] One local model runs a full turn through a promoted recipe on LM Studio: window, tools, and permissions all visible in the lab.
- [ ] A theme and a language-server preset arrive through an extension, activated per profile, with the built-ins themselves shipped as extensions.
- [ ] A remote driver pairs over the ACP bridge, prompts that agent, and **cannot** answer the permission prompt it raises — the human does.
- [ ] Docs cover install, agents, profiles, hosts, policy, LSP, local models, remote drivers, API; OpenAPI published.
