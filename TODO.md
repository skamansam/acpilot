# Acpilot — TODO

The plan to go from an empty directory to `v0.1.0`: **one agent, one machine, four
interfaces, zero rewrites.**

## How to read this

- Phases have a name and a code. Prose says "the desktop phase (P10)"; the code is
the stable identifier commits and issues cite, so a name can be reworded without
breaking a reference.
- Tasks are `P<phase>-<n>`, so `feat(acp): P2-4` still resolves: `P0` design, `P1`
foundation, `P2` engine, `P3` service, `P4` profiles, `P4.5` models, `P4.6` mod
shop, `P5` command line, `P6` editor, `P6.5` version control, `P7` language
intelligence, `P8` remote hosts, `P9` Windows, `P10` desktop, `P11` web, `P12`
release, `P13` chat, `P14` studio.
- A phase is done when every box is checked **and** the phase's acceptance test passes.
- "DoD" (definition of done) is stated per phase; it is deliberately testable.
- Anything marked **[decide]** needs a short ADR in `docs/adr/` before code lands.
- Estimates are ideal engineering days, not calendar promises.

Ordering rule: **no UI work before the daemon API works.** UIs are clients; if a UI needs
something, add it to the API first. That single rule is what stops the four frontends from
becoming four products.

---

## Design phase (P0) — De-risking probes (≈9 days)

Throwaway code in `probes/`, no workspace integration, no polish. Each probe answers one
question that would be expensive to get wrong.

- [x] **P0-1 · ACP handshake against a real agent** (2d) — **done 2026-10-21**
  - `cargo new probes/acp-handshake`; deps: `agent-client-protocol = { version = "3.4", features = ["process", "stdio"] }`, `tokio`.
  - Implement the `Client` trait: `initialize` (protocol v1) → `session/new` → `session/prompt` "list the files here" → print every `session/update`.
  - Handle `session/request_permission` with an auto-deny and log the payload.
  - Run against `npx -y @zed-industries/claude-code-acp` and `gemini --experimental-acp`.
  - **Answer:** exact capabilities advertised by each agent; which `session/update` kinds arrive; what `stopReason` values appear; does `session/load` work.
  - **Exit criteria:** transcript of two different agents driving the same 40 lines of client code, checked into `docs/notes/acp-transcripts.md`.
  - **Result:** the client is `probes/acp-handshake` (613 lines, standalone crate, SDK 3.4.0) and
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
  - **Result:** `probes/pty-agent` (one driver: spawn, `vt100` screen, terminal-query answers,
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
    ConPTY, which the design phase's task P0-5 owns.

- [x] **P0-3 · gpui text benchmark** (2d) — **closed 2026-10-22; D1 stands**
  - Render a 100k-line Rust file in gpui, scroll, select, type, undo; build the screen against the `text_system` shaped-line API and a `uniform_list` row window rather than one giant element.
  - Measure: first paint, keystroke→paint, scroll frame time, and the cost of highlighting a 3k-line visible window with tree-sitter.
  - Also measure a minimal `iced` build of the same screen, so the fallback is priced rather than assumed.
  - Verify the pin actually builds clean from crates.io alone (no `zed/` checkout, no network beyond the lockfile): this is where the `zed-font-kit` git dependency and any missing feature will bite.
  - Write down the upstream gaps we hit in 0.2.2 (accessibility is known) as a list we and the GUI-builder project can push on together — that list, not a wish, is what justifies escalating to a git `rev`.
  - **Exit criteria:** numbers in `docs/notes/gui-bench.md` vs budgets (§Perf). If gpui misses by >2×, needs a nightly toolchain, or cannot be built from a clean vendored tree, fall back to `iced` 0.14 → Tauri **now** and say so in the ADR.
  - **Result (2026-10-22): both frameworks meet the frame budget, both miss first paint, and the fallback is the
    slower of the two — so the veto does not fire and D1 stands.** The GPU was there all along: WSL2 has the
    RTX 5060 (`/dev/dxg`, driver 616.56) but no Vulkan ICD for it (`dzn` is not in Ubuntu's
    `mesa-vulkan-drivers`), and gpui's Linux renderer is Vulkan-only, so both benchmarks ran **natively on
    Windows** through the WSL interop — the same hardware, the platform the budget is about.
    `probes/gpui-bench` and `probes/iced-bench` draw the same screen (100k rows, 50 visible at 18 px, 13 px
    Consolas, generated rows, three phases after warm-up, one cold 3k-line shaping probe) and were run
    interleaved so neither got a colder system. **Frame pacing: p95 4.3–4.6 ms at 240 Hz for both**, 0–1 dropped
    intervals per run — the 16 ms budget met with room, though vsync-bound so headroom above 240 Hz is
    unmeasured. **First paint: gpui 277–326 ms, iced 851–896 ms, against 150 ms.** The fallback is not a fix:
    iced boots 4–5× faster (50 ms vs 231–273 ms) and spends it several times over in window/device creation
    (853–898 ms vs 272–313 ms), i.e. ~3× worse in total at the same pacing. Shaping 3 000 lines cold is ~50 ms
    in both (~17 µs/line), which is why shaped lines are cached rather than re-shaped.
    **The >2× clause needs care, and this is the finding that changes:** on a warm machine gpui is 1.85–2.17×
    over (median 1.90×), so it does not fire — but the same binary measured 580–968 ms (3.9–6.5×) earlier in the
    session before the DLL and driver caches were warm, and iced misses by 5.7–6.0×. A first-paint budget that
    does not state the machine state measures the OS file cache, and the clause read as an absolute number
    rejects the fallback harder than the framework it protects. So it is restated as a comparison: a framework
    is vetoed when the alternative is >2× better, which no measurement here supports. The lever left is
    window/device creation (iced: 51 ms boot → 750–800 ms to a window), not the view, which costs ~0 ms per
    frame in both; a profile of that phase is the next useful measurement, and §Perf should say that a 150 ms
    first paint is a property of how the app starts, not of the framework.
    The pin still needs **`libc` held at ≤ 0.2.189** on Linux (`xattr` 0.2.3 uses `libc::ENOATTR`) and
    `libxkbcommon-x11`'s dev symlink even with X11 off. Numbers, method and limitations:
    `docs/notes/gui-bench.md`. Still unmeasured: tree-sitter highlight cost for a 3k-line window, a profile of
    the window/device phase, and a macOS run.

- [x] **P0-4 · SSH PTY + agent spawn** (1d) — **done 2026-10-21**
  - `russh` connect to a local `sshd` container, open a session with a PTY, run `bash -lc 'echo hi'`; then run an ACP agent over the channel.
  - **Answer:** does ACP over an SSH `exec` channel behave (line buffering, stderr, exit codes, resize)?
  - **Exit criteria:** `docker compose up sshd && cargo run --bin ssh-probe` prints a real ACP session from inside the container.
  - **Result:** `probes/ssh` (+ `sshd.sh`) against OpenSSH 9.6p1 on this box, `russh` 0.64.1.
    No usable container runtime here (Docker Desktop's WSL integration is off, rootless podman's socket
    refuses), so `sshd.sh` runs the distro's own `sshd` unprivileged on `127.0.0.1:2222` with a throwaway
    host key and key-only auth — same server, same channels, minus the process boundary, which the design phase's P0-4 was not
    about. **The exec channel is a byte stream, not a message channel:** 103 JSON lines written one at a
    time arrived in 20–33 `DATA` events, up to 37 frames in one event (largest 6,007 B), so the client
    must buffer and split on `\n`; zero frames straddled an event on loopback, which is the conservative
    case and not a guarantee. stderr arrives as `EXTENDED_DATA ext=1` and the exit code survives (probe
    exits 3), so a frame stream is never polluted by an agent's warnings — the probe fails the run if it is.
    A PTY over the hop takes a `window_change` from `40 100` to `50 120` while a program watches, and `tty`
    answers a real `/dev/pts/N`, so resize is a channel message rather than a reconnect. Cost is the
    connection, not the messages: connect+auth 70.7–113.9 ms, then an exec round trip 44.7–49.8 ms and the
    whole 103-frame ACP exchange 43.1–51.4 ms (5 runs) — connect once per host and multiplex, the same
    verdict as the design phase's task P0-5 from a different transport. Numbers, the harness and what the remote-hosts phase's task P8-1 inherits:
    `docs/notes/ssh-agent.md`. The agent is a mock speaking real ACP methods; the design phase's task P0-1 already ran two real
    agents over stdio, and the question here was whether SSH preserves their framing.

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
  - **Result:** `probes/wsl-helper` (Windows driver, `wsl.exe` 2.7.10.0) plus
    `helper/acpilotd-helper.rs` (dependency-free Linux end, installed with `rustc -O` through stdin).
    Measured on Windows 11 26300 / Ubuntu-24.04: one helper **spawn costs ~140 ms** to the first
    response and ~197 ms through exit, while a message on the warm session costs **0.1 ms** (p50, 30
    samples); a 1 MiB request line arrived intact in 1.3 ms. So the helper is a session, not a call:
    The engine phase's task P2-2 and the service phase's task P3-1 must spawn it once per host and multiplex ids. `wsl.exe -- <cmd>` goes through the user's
    shell and eats backslashes (missing binary → 127); `-e` execs directly and keeps them (missing
    binary → relay error, exit 1). `-e` does **not** expand `~` in the program path, so the helper path
    is resolved from `$HOME` and `--cd ~` is passed explicitly. `-l -q` is UTF-16LE+CRLF (as the
    starting points said); enumeration also returns `docker-desktop`, which is not a usable distro.
    Translation is two rules and only one is unconditional: `C:\x` ↔ `/mnt/c/x` for **mounted** drives
    only (`ls /mnt` gave `c` and `e` here; `D:` has no Linux path and `wslpath` refuses it too), and
    `/home/sam` ↔ `\\wsl.localhost\Ubuntu-24.04\home\sam`. `acpilot-host` gets `WslPathMap` from
    `probes/wsl-helper/src/paths.rs` (12 tests, no Windows APIs), populated from `ls /mnt`, with an
    unmounted drive or a foreign distro as a typed refusal; `..` is left to the policy layer. Under
    ConPTY `wsl.exe` sends `CSI 6 n` and waits, so the helper never starts until DSR is answered — with
    an answer it works, echoing the request and ending lines with CRLF. Pipes are the transport for the
    JSON channel; ConPTY stays for terminals, where a query responder is mandatory anyway. Numbers,
    the `wslpath` oracle table and every quirk: `docs/notes/wsl-helper.md`. The suite runs on both
    sides: fmt/clippy/`cargo test` (23) under Windows and inside the distro, the helper's 4 unit tests
    with `rustc --test` on either OS, and `helper/check_protocol.py` against a native helper build in
    both. CI (`.github/workflows/ci.yml`, job `p0-5 probe`) runs that suite on both OSes on every push.
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
  - **Exit criteria:** endpoint list frozen for the service phase (P3); anything missing from the README added there.
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
    (2.26 ms per 100k lines) is what must become incremental in the language-intelligence phase (P7). `rust-analyzer` answers
    pull diagnostics only after one `-32802` cancellation, and is the only server that sends
    substantial `$/progress` traffic (133 notifications for a one-file index). Measurements,
    commands and the `async-lsp` 0.2 assessment: `docs/notes/lsp-servers.md`.

---

## Foundation phase (P1) — Workspace skeleton (≈3 days) — **done**

- [x] **P1-1** Workspace root layout per README §6; `Cargo.toml` with `resolver = "3"`, `edition = "2024"`, `rust-version = "1.95"` (raised from 1.94: `rusqlite_migration` 2.6 requires 1.95), and `[workspace.dependencies]` for every crate (single place to bump versions).
- [x] **P1-2** `rust-toolchain.toml` pinning **1.97.0**; `rustfmt.toml` with stable-only options (`edition`, `max_width`); `clippy.toml` with `msrv = "1.95"`. `group_imports = "StdExternalCrate"` was dropped: it is nightly-only, so on a stable toolchain it prints a warning and does nothing. Import grouping is a review convention (`CONTRIBUTING.md`) until rustfmt stabilises it.
- [x] **P1-3** `crates/acpilot-proto`: typed modules `host`, `workspace`, `agent`, `session`, `event`, `permission`, `file`, `terminal`, `profile`, `lsp`, `error`; every type `serde` + `utoipa::ToSchema`; `ErrorBody { code, message, details }`; `ApiDoc` registers them all.
- [x] **P1-4** `AcpilotError` in `acpilot-core::error` with the seven variants; `ApiError` newtype in `acpilotd` maps to `404/409/403/502/502/500/500`; `exit_code()` maps to `3/4/5/1`, with `USAGE = 2` owned by the parser. Both mappings are unit-tested.
- [x] **P1-5** `.github/workflows/ci.yml`: fmt, clippy `-D warnings`, nextest, docs-feature tests, `cargo-deny`, and an OpenAPI-snapshot freshness job, on linux+macos+windows. `.github/workflows/release.yml` is the stub until the release phase (P12) wires `cargo-dist`. A separate `p0-5 probe` job checks `probes/wsl-helper` on ubuntu+windows: it is deliberately not a workspace member, so the workspace commands cannot reach it.
- [x] **P1-6** `xtask`: `cargo xtask check` runs what CI runs; `dev`, `web-build` and `dist` exit 2 with the phase that owns them rather than pretending to work.
- [x] **P1-7** `.gitignore`, `LICENSE-MIT`, `LICENSE-APACHE`, `CONTRIBUTING.md`, `docs/adr/0001-record-architecture-decisions.md` (with the index of ADRs 0002–0009 to come).
- [x] **P1-8** `crates/acpilot-proto/tests/openapi_snapshot.rs` with `insta`; the 811-line snapshot is committed and CI fails if regenerating changes it.
  - **DoD:** `cargo xtask check` green; every crate builds; `GET /v1/openapi.json` served by the real daemon; Swagger UI at `/docs` behind the `docs` feature (assets from a CDN, so no build-time download and no third-party script in a default build).
  - **Verified locally (linux):** cross-OS runs are CI's job, not this machine's.

---

## Engine phase (P2) — ACP engine, sessions, permissions (≈12 days)

The heart. Headless: no daemon, no UI, tested against a **mock agent** so tests are fast
and deterministic.

- [x] **P2-1** `acpilot-agent::registry`: load the active profile's `agents.toml` (P4-2), validate manifests (`transport ∈ {acp-stdio, pty, http-jsonrpc}`, required fields per transport), resolve `keyring:` secrets, expose `AgentSpec`. Hot-reload on file change. — **done 2026-10-22**
  - **Result:** four modules in `acpilot-agent` — `manifest` (schema → `AgentSpec`), `secret`
    (references and the resolver seam), `registry` (snapshot + watcher), `error` — with 38 tests.
    The shape of each transport is in the type: `Transport::AcpStdio { command }`,
    `Pty { command, profile }`, `HttpJsonRpc { url, auth }`, so "an `acp-stdio` agent has a
    command" cannot be forgotten by the supervisor that consumes it. Validation refuses rather
    than guesses: an unknown key, a field the chosen transport has no meaning for (`sentinel`
    on an ACP agent), a missing per-transport field (acp-stdio `command`; pty `command`,
    `parser`, `sentinel`; http `url`), a duplicate or malformed `id`, a non-http `url`, and
    `idle_timeout_ms = 0` — the one setting the design phase's task P0-2 showed truncates turns. Defaults are the
    measured ones: pty idle backstop 5 s, submit delay 50 ms (the engine phase's task P2-7 owns tuning it), restart
    `never` (the docs write `on-crash` explicitly, so it is not the default).
    **Secrets are resolved at load, not mid-turn.** `Secret` prints `<redacted>` in `Debug` and
    `Display`, so an `AgentSpec` can be logged; `expose()` is the single path to a child's
    environment. `env` values may be literals (`NO_COLOR = "1"`) or `keyring:<name>`
    references — the prefix decides, so a secret cannot arrive as a literal by accident —
    while `auth` must be a reference, because a plaintext token there has no non-secret
    reading. The grammar is the profiles phase's P4-5: `keyring:<name>` is this profile, and
    `keyring:<profile>/<name>` additionally requires `allow_cross_profile = true`, refused
    here rather than left to the resolver. The backend is injected (`SecretResolver`), so this
    crate has no keyring dependency yet; the profiles phase's task P4-5 supplies the OS keyring, tests supply
    `MapResolver`, and the namespace (`acpilot` service, `<profile>/<name>` account) is decided
    in one place when that lands.
    **Reload is atomic and remembers why it failed.** A new file is parsed into a fresh
    `Vec<Arc<AgentSpec>>` and only a complete success replaces the old one, so a typo while
    sessions run costs a status line, not three dead sessions; `revision()` is unchanged and
    `last_error()` carries the reason. A byte-identical save returns `false` rather than
    bumping the revision, so UIs are not asked to refresh for whitespace. Watching holds the
    **directory**, not the file: the tests cover a plain save, an editor's
    write-temp-then-rename save (which replaces the inode and would kill a file watch after
    one save), an unrelated `lsp.toml` edit in the same directory, a broken save followed by a
    good one, and that dropping the watcher stops it. A burst of events from one save is
    coalesced over 50 ms.
    Three documents disagreed with each other and with the plan, and the code settled it:
    README's snippet was a third schema (`[agents.<id>]` with `label`, `prompt`, and an
    `oauth:` scheme that does not exist), `reference/manifest.md` had the keyring order
    reversed (`keyring:<agent>/<profile>`) and no `url`/`auth` rows at all, and `secrets.md`
    used `api_key`, which is not a field. All three now show the array form with `env`. On
    spelling: the manifest accepts the documents' `acp-stdio` *and* the API's `acp_stdio`, so a
    value copied out of `GET /v1/agents` can be pasted in, while the wire enum and its
    snapshot are untouched. Not in this task: the keyring backend and namespace (P4-5), the
    spawn (P2-2), running a `script:<path>` parser (P2-7), and availability — whether `command`
    is on `PATH` is a host question, not a parse.
    **The schema is published, not described.** `manifest::json_schema` generates
    `site/public/schema/agents-v1.json` (`cargo xtask schema`) from the same `AgentEntry` the
    loader parses, so the artifact cannot name a key the loader refuses. `transport` becomes
    an internally tagged `oneOf`: one branch per transport, each fixing the tag to the two
    spellings it accepts and listing exactly `TransportName::allowed_fields` plus the shared
    keys, with the branch's `required` taken from the same table the loader's refusals come
    from. Three rules are written by hand because neither a derive nor a doc comment can say
    them — the `id` pattern, the `parser` spellings, `url` as a URI — and `Option`/`u64`
    output is pruned of `null` and `"format": "uint64"`, since a TOML file cannot hold
    either. `"$schema"` is accepted as a root key so the file can point at the schema it is
    checked against. Tests: the committed copy is current, every `$ref` resolves, the shipped
    presets validate, and a list of eight refusals is checked twice — the schema rejects it
    and the loader rejects it with the message that names the field.
- [x] **P2-2** `acpilot-agent::supervisor`: spawn an `acp-stdio` agent; capture stderr into a ring buffer for the session log; restart policy (`never|on-crash`, max 3); clean process-tree kill. — **done 2026-10-22**
  - **Result:** `supervisor` starts an `acp-stdio` spec and nothing else: `pty` (P2-7) and
    `http-jsonrpc` (no local process) are refused by name, in the manifest's own spelling, so a program
    is never started with the wrong transport and diagnosed later. The pipes are `std::process`'s rather
    than `agent-client-protocol`'s `spawn_process`: the engine phase's task P2-3 owns an async ACP connection, and a supervisor
    that both spawned and spoke the protocol could not hand the child to it. `stdin` and `stdout` are
    handed out once and never read here — draining stdout would eat JSON-RPC frames. stderr is a 64 KiB
    ring filled by one reader thread: readable while the agent runs (so a status line does not wait for
    an exit), lossily decoded, and marked truncated when the beginning was dropped. `AgentProcess`'s
    `Debug` shows only the id, pid and buffer size; a resolved secret reaches the child through
    `Secret::expose` and nowhere else.
    **Kill is a tree kill.** unix starts the child as its own process-group leader and signals the group,
    so `npx → node` dies with the wrapper and the stderr pipe reaches EOF; Windows uses `taskkill /T /F`
    because a job object needs `unsafe`, which the workspace forbids (the engine phase's task P2-5 owns the stronger guarantee).
    `Drop` kills, then waits up to 500 ms for the reader and detaches it rather than blocking on a
    grandchild that inherited the pipe. **Restart is a policy, not a loop**: `on-crash` restarts after a
    non-zero exit up to three times on the same buffer, so the first crash is still readable after the
    last attempt, and a clean exit is never restarted. `availability()` answers `available`/`missing`
    from a local `PATH` lookup without starting anything. Eight tests run real `/bin/sh` processes;
    the Windows path is type-checked by the cross-target build but has no Windows runner in this suite
    yet, which is recorded here rather than papered over.
- [x] **P2-3** `acpilot-agent::session`: wrap `initialize`/`session/new`/`session/load`/`session/prompt`/`session/cancel`; translate ACP `session/update` into our `SessionEvent` enum (`MessageChunk{role}`, `Thought`, `ToolCall{status}`, `Plan`, `ModeChanged`, `Notice`, `Compaction`, `CommandsChanged`) with stable sequence numbers. — **done 2026-10-22**
  - **Result:** `session` owns an ACP connection in a background task and hands out
    `Session` as a handle. `spawn(AgentProcess)` takes the supervisor's two pipes and runs them
    through `blocking::Unblock`; `over(transport)` is the same code for an in-memory channel, an SSH
    host (P3-9), or a test. `initialize` runs before the constructor returns, so a caller never holds a
    session whose capabilities it has not seen. `session/new` and `session/load` store the agent's
    id (the response is returned, modes and config options included), `session/prompt` is refused
    with `NoSession` before either ran, and a second concurrent turn is refused with `Busy` here
    rather than delegated to whatever an agent does with two prompts. The busy flag is a guard, so a
    dropped prompt future releases it. `session/cancel` is a notification, sent through the same
    connection, and works while a prompt is pending.
    **Events are sequenced once, before broadcast.** Every `session/update` becomes one
    `SessionEvent` with a sequence number starting at 1; a slow subscriber drops events but cannot
    renumber them, and the service phase's task P3-3 adds replay on top of the same numbers. The closed kind enum is kept as
    is; updates that have no kind of their own (`config_option_update`, `session_info_update`,
    `usage_update`) become `Notice` with a `subject`, and a future ACP variant becomes a `Notice` with
    its raw payload instead of disappearing. Payloads are explicit snake_case objects, documented
    as a table in the module, rather than a passthrough of the ACP object: a consumer should not have
    to know the wire schema to render a chunk. A tool-call update carries `patch: true` and the
    *current* status tracked from earlier updates, so a patch that only changes a title still says
    `in_progress`.
    **Permissions are denied by default.** The session answers `session/request_permission` with the
    agent's own `reject_once` (or `reject_always`, else `cancelled`) and records a `notice`, because
    there is no policy engine until the engine phase's task P2-6 and an implied grant is the one failure this product cannot
    have. The client advertises session notices and compaction so agents may send them, and does not
    advertise filesystem or terminal support until the engine phase's tasks P2-4 and P2-5 exist. Twelve tests: nine on the
    translation and the refusal choice, three against an in-process mock agent over a channel
    (initialize/new/prompt with ordered chunks, cancel reaching the agent, and the busy guard).
    **Open:** the Windows side has no runner yet, as with the engine phase's task P2-2. (The pipe wiring this task could
    only type-check is process-tested now that the P2-9 fixture is a real child; see task P2-9 of the
    engine phase.)
- [x] **P2-4** `acpilot-agent::fs`: implement ACP client-side `fs/read_text_file` and `fs/write_text_file` against the `Host` trait, with a **shadow-write guard**: writes go to disk only if the workspace policy allows, otherwise a pending grant is created and the call blocks. — **done 2026-10-23**
  - **Result:** `acpilot_agent::fs` serves both methods from a workspace, and the `Host` trait grew the three methods that make that possible: `canonicalize`, `read_file`, `write_file`. `canonicalize` is the one that matters. A containment check on the text an agent sent is wrong — `workspace/link` is a path that looks contained and points at `/etc` — so the path is resolved *on the host*, symlinks followed, and only then compared with the root. That also keeps the guard honest for a remote workspace: `std::fs` in this module would have been a silent local answer to a remote question. `LocalHost` implements all three; `SshHost` refuses them by name, because serving a remote path from this machine's disk is a wrong answer that looks like a right one, and the service phase's task P3-9 adds the SFTP channel (with `canonicalize` first, since a containment check cannot be sound without it).
    **The shadow write is a wait, not a staging copy.** A write inside the workspace goes to disk. A write outside it does not touch anything: a `PendingWrite` is published on `Grants`, the call waits, and the file is written only after the approval. While the grant is pending the old contents are still the contents, which is what lets a UI show a diff against a file that has not moved. Nothing is written speculatively and rolled back — a file restored by a rollback is not the same inode the editor had open. `Grants::approve`/`deny` take the id, a decision is delivered once (the entry is removed before the answer is sent), and `Grants::pending()` is the list the service phase's P3-3 `GET /v1/permissions?status=pending` is supposed to serve: a pending request is state, not history. (the engine phase's task P2-6 replaced these three types with `Broker`; the behaviour is the same, and the list is `Broker::pending`.)
    **A blocked write must not become a hang**, which is the failure the engine phase's task P2-9 found one layer down. Two rules: with no subscriber the write is refused immediately, because an approval nobody can give is not worth waiting for; and a grant nobody answers expires after `GRANT_TIMEOUT` (five minutes — long enough to read a diff, short enough that going home does not wedge an agent). Both end the turn with a denial carrying the path. Reads outside the workspace are refused rather than queued: there is no approval flow for reads, and inventing one before the engine phase's task P2-6 owns the policy engine would be two answers to one question. A relative path is refused instead of joined onto the workspace, a file that is not UTF-8 is refused instead of converted, and a missing parent directory is an error rather than a tree to build.
    **The session advertises the capability iff it can serve it.** `SessionOptions::fs` is the same switch that registers the handlers, so a session with no workspace says it has no filesystem and refuses a request that arrives anyway; every refusal is recorded as a `notice` before it is returned, so the transcript shows what the agent was told. A pending grant carries the daemon's session id, not the agent's ACP id, because the person approving it needs the conversation, not the agent's name for it. Disk I/O runs on the blocking pool (`spawn_blocking`), so a large file on a slow disk cannot stall the session's runtime thread.
    **Tests:** eleven in `fs.rs` (a read in the workspace, a 1-based line range that keeps its terminators, a read outside, a symlink out of the workspace, a relative path, binary content, a missing file, a write inside with no grant, an outside write that blocks and then writes after approval, a denial that writes nothing, a refusal with nobody listening, and an expiring grant), two in `acpilot-host` (bytes round-trip, a missing file naming itself, a symlink resolved to its target), and three against the engine phase's P2-9 fixture process: the fixture asks for a read inside, a read of `/etc/passwd` and a write inside in one turn; it asks for a write outside and the turn is still blocked while the test checks the file is absent; and a denied write ends the turn with a refusal the agent can read.
  - **Open:** rules, path globs and the audit table are the engine phase's task P2-6 — this layer has exactly one rule. Files over SSH/WSL are the service phase's task P3-9. The 10 MiB cap and binary *detection* (as opposed to refusing non-UTF-8) are the service phase's P3-5 files API.
- [ ] **P2-5** `acpilot-agent::terminal`: implement `terminal/create|output|wait_for_exit|kill|release` on top of `Host::spawn_pty`; enforce output ring buffer (1 MiB) and idle reaping.
- [x] **P2-6** `acpilot-agent::permission`: the broker. `PermissionRequest { id, session, kind, title, detail, options }`; policy engine with rules (`allow`, `ask`, `deny` × tool glob × path glob × agent), default-deny for shell writes outside workspace; every decision appended to `audit` table. — **done 2026-10-23**
  - **Result:** `acpilot_agent::permission` is a `Policy` and a `Broker`. A `Rule` is a decision plus any of agent, tool and path; they are evaluated in file order and the first match wins, and the id of the one that fired goes on the audit line. No scoring, no specificity contest: the answer to "why was this allowed" is a rule id, not a plausibility argument. Paths are `Pattern` globs, anchored and path-shaped (`*` stops at a separator, `**` does not, `?` is one character), matched against the path the caller already canonicalised on the host, so `workspace/../etc` cannot arrive as something a pattern believes is inside. Case is folded on Windows and not elsewhere, because a Windows path is case-insensitive and a POSIX one is not.
    **The ask is a wait with two ways out.** `Decision::Ask` publishes a `PermissionRequest` on a broadcast channel, keeps the `oneshot` in `pending()`, and blocks the caller until `answer` is called — which is what makes a multi-file edit coherent instead of half-applied. With no subscriber the request is refused at once (`Unattended`), because an approval nobody can give is not worth waiting for; with a subscriber that never answers it is refused after `ASK_TIMEOUT` (five minutes: long enough to read a diff, short enough that going home does not wedge an agent). Both are audited with the reason. A decision is delivered once — approving and denying remove the entry before the answer is sent — so answering the same id twice is a `false`, not a second effect.
    **`allow always` remembers a shape, not a rule.** It is stored per session as kind + tool + path, so approving one file does not approve its directory and approving an execute does not approve a delete. It never outlives the session: the next conversation starts with no memory of this one's exceptions.
    **The shipped defaults are the documented ones, with one correction.** Read and write inside the workspace are allowed, a shell or an unrecognised tool is asked about, and anything outside is asked about too. The engine phase's task P2-4 shipped a grant queue for outside writes, so "outside is denied" in the permissions page was never true of the code; asking is the same behaviour with a name, and a policy that really means *never* sets `outside_workspace = "deny"`. A write inside is **allowed** rather than asked about because the engine phase (P2) has no interface to ask with: a broker that asked would leave every headless agent unable to edit, which is the one thing it is for. `[policy] default = "ask"` is the shipped answer once a client can show the dialog.
    **The `Grants`/`PendingWrite`/`GRANT_TIMEOUT` from the engine phase's P2-4 are gone.** The filesystem now asks the broker, so a read is audited and a rule can refuse it — before that, it could only compare a path with the root. The session classifies a `session/request_permission` by the agent's own `ToolKind` plus its first location, answers with the agent's own option id for the decision (an agent that called its allow option `yolo` receives `yolo`; `AllowOnce`/`AllowAlways`/`Reject*` are matched, not renamed), and cancels the request when the agent offered no option of the kind the decision needs, because approving something nobody offered is not ours to do. Every answer is recorded as a `notice`: `Permission granted` at info, `Permission request denied` at warning, with who decided (`user`, `policy:<id>`, `default`, `session`, `timeout`, `unattended`) in the description. A session with no broker still refuses everything with the agent's reject option, so an unconfigured session cannot imply a grant; `SessionOptions::fs` supplies the filesystem's broker for callers that do not set one. A rule may be scoped to an agent, so the subject carries one: `SessionOptions::agent` is filled from the spec the supervisor started, and a rule naming the manifest's `id` therefore matches the agent that is actually running.
  - **Tests:** nineteen in `permission.rs` (the two glob forms and case folding, first-match-wins with its rule id, a rule needing every constraint it names, the shipped defaults, the documented `[config]` shape, a misspelled decision, an allow and its audit row, a denial that never becomes pending, an inside write needing no person, an ask published with three options and answered once, `allow always` covering the next identical request but not a different path, nobody listening, an expiry that is no longer answerable, and the per-session audit order), fourteen in `fs.rs` (the engine phase's task P2-4 set, with the write tests split by policy: inside allowed and audited, an asking policy refused at once with nobody there, an outside write waiting for approval, a denying policy writing nothing, an expiry), three new in `session.rs` (classification from kind and location, an allow selecting the agent's option with the notice naming the decider, an allow the agent never offered being cancelled), and three in `tests/process.rs` against the engine phase's P2-9 fixture: a write outside the workspace holds the turn while the test checks the file is absent and then writes it after the answer, a denial ends the turn with a refusal the agent can read, and a rule naming the running agent turns that agent's permission request into an allow (the audit says `policy:allow-mock` and the fixture reports the agent's own `allow` option).
  - **Open:** the loader exists — `acpilot-profile::Profile` reads a profile's `config.toml`, and `acpilotd --profile <dir>` puts that policy behind the permissions routes (P3-2) — but a profile *name* resolving to a directory is the profiles phase's P4-2, so a caller still spells the path. The audit lives in memory: the service phase's P3-3 `audit` table is the first thing that keeps it. `GET /v1/permissions` is served (P3-2) but lists pending requests; the audit is not on the wire yet. Rules cannot be reloaded without rebuilding the broker, and a running session holds its `Arc`. Terminal requests reach `Ask` with no terminal to open until the engine phase's task P2-5. An `fs/*` callback's audit row has no agent on it: one `Fs` serves every session on its workspace, so the id would be a guess — the session's permission requests do carry theirs.
- [ ] **P2-7** `acpilot-agent::pty_agent`: the program-as-agent adapter from the design phase's task P0-2, behind `transport = "pty"`; ship two built-in parsers (`plain-transcript`, `ansi-transcript`), a shipped `freebuff` profile as the reference case, and an escape hatch `parser = "script:<path>"` later.
- [ ] **P2-8** `acpilot-agent::transcript`: append-only `session.jsonl` (`events`, `grants`, `meta`), reader for replay, compaction into sqlite later.
- [x] **P2-9** Test fixture: `crates/acpilot-agent/tests/fixtures/mock_agent.rs` — a tiny ACP agent (using the SDK's `Agent` trait) that can be told to stream N chunks, request a permission, request a file write, open a terminal, and hang. — **done 2026-10-23**
  - **Result:** the fixture is a **process**, not an in-process helper, because the gap it was
    written to close was the wiring only a child has: a supervisor's two pipes, taken once, and a
    process that dies when the session does. It is built as `acpilot-mock-agent` (a `[[bin]]` in
    `acpilot-agent` with `test = false`, so `cargo test` builds it and the tests name it through
    `CARGO_BIN_EXE_` rather than guessing a target directory) and speaks ACP over real stdin and
    stdout. What it does comes from the prompt text — `stream 3`, `permission`, `fs`, `terminal`,
    `hang`, anything else echoes — so a test needs no argument and no environment variable. What it
    *heard* comes back as a `Notice`: the client is the only side a test can observe, and a fixture
    that decided the outcome and kept it to itself would be asserting nothing. Requests to the
    client are sent from a task spawned on the connection, because awaiting one inside an incoming
    handler deadlocks the dispatch loop (the SDK says so on `block_task`, and the first version of
    this file proved it). The in-process mock in `tests/session.rs` stays where it is: a channel can
    be observed from both ends, which is how the cancel test reads what the agent saw.
    `tests/process.rs` has the five tests this buys: three chunks in order with seq 1..=3 and the
    elapsed time asserted under 200 ms (the DoD's budget, reported with the measurement on failure),
    a permission request answered with the agent's own `reject_once` option with both the refusal and
    the agent's report as events, `fs/*` and `terminal/create` refused, a hung turn that stays
    pending and still leaves the session usable for the next one, and a dropped session taking its
    child with it (Linux, by pid).
    **It found a real defect on the first run.** A request for a method this client does not
    implement was left *unanswered*, so the agent blocked on it forever: `fs/read_text_file`,
    `fs/write_text_file` and `terminal/create` now get a `MethodNotFound` naming the method, which
    the engine phase's tasks P2-4 and P2-5 replace with the real answer. Not advertising a capability does not stop an agent
    asking for it, and a request has a response or it is a hang.
  - **DoD:** a `session_smoke` test that prompts the mock agent, receives streamed chunks in order,
    answers a permission, and asserts the grant appears in the audit log — under 200 ms.
    **Met except the audit line:** there is no audit log to assert against until task P2-6 of the engine phase owns the
    broker — the "grant" this DoD expects recorded is that task's decision. What exists now is the
    refusal path, and it is asserted end to end.

---

## Service phase (P3) — Daemon + API (≈10 days)

- [ ] **P3-1** `acpilotd` bootstrap: config (`figment`, layered per the profiles phase's task P4-2), `tracing` (JSON to file + pretty to stderr), `--foreground`, socket binding (`$XDG_RUNTIME_DIR/acpilot/acpilot.sock`, `\\.\pipe\acpilot-$USER` on Windows, TCP `127.0.0.1:7777` opt-in), pidfile + single-instance lock.
  - **Started 2026-10-24:** the socket transport is ADR 0004 §3's step 2, and it landed with the client so the product was never half-moved. `acpilotd` binds `$XDG_RUNTIME_DIR/acpilot/acpilot.sock` (`$TMPDIR` on macOS) by default, creating the directory `0700` and the socket `0600` — the filesystem is the access control, which is the only reason "a local request needs no token" is true. A path another daemon still answers on is refused (`AddrInUse`, naming the flag to move it); one left by a killed daemon is removed and the log says `replaced_stale_socket=true`, which is what makes a crash recoverable without instructions. `--socket <path>` moves it, `--addr <host:port>` is the opt-in TCP listener, both at once is an error rather than a precedence rule, and `SIGTERM` as well as Ctrl-C removes the socket on the way out — a service manager that leaves a socket file behind makes the next start look like a crash. The client needed no new HTTP stack: `reqwest` 0.13 has `ClientBuilder::unix_socket`, so `Client::new` takes either a URL or a path and `--api-url`/`ACPILOT_API_URL` accept both. Defaults live in one place (`acpilot_host::default_endpoint`), so a client's probe and a daemon's bind are the same string by construction. Still open from this item: `figment` configuration, `--foreground`, the pidfile, and the discovery metadata under `daemons/<name>.json`. The bearer token on `--addr` was the last of them and is done — see P3-2 below. The Windows named pipe is P10-5's, so that platform defaults to loopback HTTP for now.
- [ ] **P3-2** axum router with the endpoints from `docs/api.md`; `tower-http` `TraceLayer`, `CompressionLayer`, `CorsLayer` (web only); `--token` bearer auth for non-loopback binds (`argon2` stored hash).
  - **Started 2026-10-24:** the token half is served, for a TCP listener rather than for a non-loopback one: a port does not inherit the socket's filesystem boundary, so it does not inherit the exemption either (ADR 0004 §3). 32 bytes from the system source, hex, written to `$XDG_RUNTIME_DIR/acpilot/token` `0600` and reused across restarts so a client is not re-taught a secret every launch; `ACPILOT_TOKEN` overrides it for a client. **Not hashed with `argon2`** as this item first proposed, and the reason is worth recording: hashing protects a secret stored where someone can read it, and this file is already readable only by its owner — a hash would add a second secret that also has to be protected, and no protection. That changes if the token ever moves into `config.toml` (P4-2), where a hash is the difference between a leak and a leak of a verifier. The middleware compares in constant time, accepts `bearer` in any case (the scheme is case-insensitive; the token is not), and answers `401 unauthorized` in the standard envelope. A client that gets a `401` from its probe now reports "refused the token, it is in …" instead of "no daemon" — the difference between editing a file and starting a second daemon. The socket listener deliberately requires nothing. Still open here: the `tower-http` layers, `CorsLayer` for the web phase, and the `argon2` path for a token that lives in configuration.
  - **Started 2026-10-22:** `GET /v1/agents` serves an injected `acpilot_agent::Registry` with
    `AgentStatus` from `acpilot_agent::supervisor::availability` (a `PATH` lookup on the daemon's host,
    never a spawn); the wire type is unchanged and the path is merged into the served OpenAPI document
    from the handler's own metadata. With no profile loaded the list is empty — the daemon has no
    profile resolution yet, and an empty list is the honest answer rather than a `404` for a profile
    that does not exist. A second test binds a real ephemeral port and fetches `/v1/agents` with
    `reqwest` against a `router_with(AppState::with_registry(...))`, so the socket, the JSON and the
    `PATH` lookup are exercised the way the CLI will use them, not only through `oneshot`. Still open
    here: every other route, the tower layers, and token auth.
  - **Started 2026-10-23:** `GET /v1/permissions?status=pending[&session=]` and `POST
    /v1/permissions/{id}/resolve` serve the broker to a client that did not raise the request. The
    broker is `AppState::with_broker`, one per daemon, so the list, the answer and the audit are one
    of each; a request that no broker exists for lists as empty and resolves as `404`, the same
    honest-empty answer `/v1/agents` gives without a profile. `ApiError` grew field-level `details`,
    and `AcpilotError::InvalidRequest` is the `400 invalid_request` the error taxonomy had documented
    but the enum could not spell. Idempotency is the daemon's to keep, not the broker's: the broker
    answers once and forgets, and the API promises the same body for a retried `resolve` and a `409`
    for a *different* option, so `AppState` remembers each decision by id. The daemon's broker has no
    deadline, because the contract is that a request stays pending until it is resolved — `ASK_TIMEOUT`
    is for a session with no client at all, which a daemon with this route is not. `acpilotd --profile
    <dir> --workspace <dir>` turns a profile's `[policy]` into that broker (`acpilot-profile` reads the
    file, `state_for_profile` resolves the root and builds it), and both flags are logged at startup
    with the rule count. Fourteen tests in `tests/permissions.rs`: the list's shape (options, kind,
    session, timestamp), a resolve that reaches the agent's side of the wait and lands on the audit
    log, an idempotent retry with an identical body, a different option as `409`, an unknown id as
    `404`, an unknown option as `400` with the allowed set in `details`, the session filter, a status
    v1 cannot list as `400`, a daemon with no broker, a profile's rule deciding and naming itself,
    a missing profile failing the startup, the OpenAPI paths, and the whole thing over a real socket.
    Checked by hand too: `acpilotd --profile <dir>` logs `rules=1` and answers both routes with the
    documented envelopes. Still open: `X-Acpilot-Profile` and session close denying pending work —
    `acpilot_agent::DecidedBy` has no `close`, because there is no session lifecycle in the daemon to
    close one. The decision ledger grows for the life of the process; the service phase's P3-7 audit table is where a
    bounded record belongs, and the ledger should read from it rather than grow beside it.
  - **Started 2026-10-23, later:** `GET /v1/events` is the announcement half of the service phase's task P3-3. The broker
    already broadcast every new request (`Broker::subscribe`); the route turns that channel and a 15 s
    clock into SSE frames — `permission` with `{"type":"requested","request"}` and a per-connection
    `id`, `heartbeat` with the latest id, and `resync` when the client fell further behind than the
    announcement channel keeps (the recovery is to refetch the pending list, which is why nothing is
    replayed). A daemon with no broker keeps the stream open and quiet rather than closing it: a
    closing stream sends every client into a reconnect loop against a daemon that is healthy. The
    stream is profile-wide rather than per-session because there is no session lifecycle yet for a
    client to name — `docs/api.md` now says so, since the pending list it pairs with is already
    profile-wide. Four tests in `tests/events.rs`: a request raised after the client connected arrives
    as a frame with the documented payload over a real socket (the client never polls), the no-broker
    stream answers `200 text/event-stream`, the route is in the served OpenAPI document, and the
    broker announces one request once. `--profile` also loads the profile's `agents.toml` now, so
    `GET /v1/agents` answers beside the policy: no file means no registry (the daemon does not write
    one into the profile), a file that does not load fails the startup like a broken `config.toml`,
    and the keyring namespace is the directory's last segment (`Profile::name`). Still open: the
    registry is loaded, not watched — an edit to `agents.toml` reaches a running daemon only after a
    restart.
- [ ] **P3-3** Event bus: per-session `tokio::broadcast` + monotonic seq; `GET /v1/sessions/{id}/events` as SSE with `Last-Event-ID` replay from the transcript (reconnect-safe). Include a heartbeat every 15 s, and answer an un-replayable id with `event: resync` carrying the latest seq instead of pretending to catch up (`docs/api.md`). Pending permissions are state, not history: the client fetches `GET /v1/permissions?status=pending` on connect and the stream only announces new requests.
  - **Started 2026-10-23:** the state half is served (see the service phase's task P3-2): the pending list and the resolve route are the two calls a reconnecting client needs, so the part of this item that does not depend on SSE exists. `GET /v1/events` now announces `permission.requested` with a heartbeat and `resync` (also the service phase's task P3-2), so the polling loop `docs/api.md` rules out is gone for permissions. Still open: the *session* stream (`GET /v1/sessions/{id}/events`), replay from the transcript with `Last-Event-ID`, the per-session monotonic `seq`, and `permission.resolved` — the broker does not broadcast decisions, so a client that watches another client resolve still refetches the list.
- [ ] **P3-4** `POST /v1/sessions/{id}/prompt`: idempotency key, returns `202` + `turn_id`; stream deltas on SSE. `POST .../cancel` maps to ACP `session/cancel`.
- [ ] **P3-5** Files API: `GET/PUT /v1/files` with workspace-relative paths, ETag (`mtime+size+hash`), `If-Match` for optimistic concurrency, 10 MiB cap, binary detection. Path traversal tests.
  - **Started 2026-10-24:** `GET /v1/files?path=`, `GET /v1/files/list?path=` and `PUT /v1/files` are served. The etag is `mtime-size-hash16`: the first two make a change visible without reading the file, the hash catches an edit that preserved both. `if_match` absent means create (and an existing file is `409`), present means replace only if it is still that version; neither is retried or resolved by the daemon. Traversal is refused in two independent ways rather than one: components are checked before joining (`..` is a `400`, not something to resolve, so the answer does not depend on whether the directory it climbs out of exists), and the resolved path is compared against the canonical root, which is the only check that catches a symlink inside the workspace pointing outside it (`403`). A directory is `409` naming the list route, and a file is `409` naming the read route, because "you asked the wrong question" is more useful than an empty answer. Binary is `400` on invalid UTF-8 *or* a NUL byte (a UTF-16 file is valid in the bytes it shares), the 10 MiB cap is checked before and after the read, and a missing parent directory is `404` — the route saves files, it does not build trees. Fifteen tests: the read/etag round trip, a directory, three spellings of `..`, an absolute path, a symlink escape, binary content, the cap, listing shape and sort order, listing a file, create/replace/conflict/stale-token/missing-file writes, a write into a missing directory, a write that escapes, unknown and absent workspaces, and the OpenAPI paths. Still open: no delete, no create-directory, no watch (the editor phase, P6, will say what a save needs), and no `acpilot` verb until the command-line phase (P5).
- [ ] **P3-6** Terminals API: create/attach (`WS /v1/terminals/{id}/attach`), resize (cols/rows), detach-without-kill, replay last screen on attach (from `vt100` state).
  - **Started 2026-10-24:** `POST /v1/terminals` starts a PTY and returns the summary once the process is running, so the id it hands back is attachable immediately; `GET /v1/terminals?session=` lists them. The terminal is a [`vt100::Parser`] fed by the thread that pumps the PTY, and a pane's replay and its subscription are taken under one lock — screen first, then subscription would drop whatever arrived in between, subscription first would repeat it. Replay is `contents_formatted()`, i.e. the escape sequences that reproduce the current screen, sent as the binary frame `attached.replay_bytes` announces, so a pane opening on a command that finished two minutes ago shows what it printed. Binary frames are bytes in both directions and text frames are control (`resize`, `detach` from a client; `attached`, `exit` from the daemon) because a shell can print `{"type":…}` and the two must not be confused. A `cwd` is resolved against the workspace and refused outside it (`403`); a zero dimension is `400` before anything is spawned; a command that will not start is `500 io`, naming the command. Closing the socket never ends the process — that is what "detach without kill" means — and a pane that stops reading is closed with an error code rather than queued without bound, which costs it one screen to reattach. The default shell lives in `acpilot-host`'s platform files (`$SHELL`, else `sh`; `%COMSPEC%`, else `cmd.exe`), so the daemon has no branch of its own. Nine tests: six route tests (empty list, no workspace, zero size, args without a command, unknown session, missing size) and, against real PTYs, a late pane's replay, typed bytes, a resize that `stty size` reports, a pane leaving without ending the process, a refused zero dimension, and the list's shape. Still open: no HTTP resize route (the WS control frame is it, per this item's "create/attach"), no kill route, no closing of a session's terminals when the session is deleted, and no client method or TUI pane yet (editor phase P6).
- [ ] **P3-7** SQLite: `migrations/0001_init.sql` (`workspaces`, `hosts`, `agents`, `sessions`, `events`, `grants`, `terminals`); `rusqlite` with `bundled`; WAL; `PRAGMA foreign_keys=ON`; migration runner on boot; retention job (events older than 30 days compacted). One database
  per profile (P4-4), so migrations run once per open profile — keep them idempotent.
- [ ] **P3-8** Hosts API: `POST /v1/hosts` (`local|ssh|wsl`), connection test endpoint, capability report (can spawn? can read fs? latency).
  - WSL specifics, from the design phase's P0-5 `workspace` measurement (`docs/notes/wsl-helper.md`): discovery is one `wsl.exe -l -q`, and **absence must be silent** — no distro, a failing `wsl.exe`, or an empty list means no `wsl://` host, no prompt, no error; drop `docker-desktop`. That rule is already written and tested in the probe (`probes/wsl-helper/src/discovery.rs`, five real-process tests on both OSes), so the service phase's task P3-8 should reuse the code rather than restate the rule. A folder opened as `\\wsl.localhost\<distro>\...` becomes `wsl://<distro>` plus a Linux cwd (infer the distro from the path, don't ask). Spawn one helper per host and multiplex ids (0.1 ms per message vs ~125 ms per spawn), always with `--cd <linux path>` and an absolute helper path. Read files in-distro: the same 37 KB file costs 13.0 ms over UNC and 0.096 ms inside (136×), so UNC is for read-only browsing only.
- [ ] **P3-9** `acpilotd-helper`: a 300-line stdio JSON-RPC shim that the daemon drops into a remote/WSL host and reuses for fs+PTY+spawn+LSP, so remote behaviour is identical to local. Static musl build, embedded in the daemon binary (`include_bytes!`), uploaded on connect with a version handshake.
- [ ] **P3-10** Tests: `axum` route tests with `tower::ServiceExt::oneshot`; a live-server test that opens SSE and asserts ordered delivery; `insta` snapshots of all error bodies.
  - **DoD:** `acpilotd` runs headless, a scripted `curl` session (create → prompt → stream → permission → resolve) works, and SSE replay after disconnect loses no events.

---

## Profiles phase (P4) — Profiles & preferences (≈10 days)

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
  - `agents.toml` has **one reader**: `acpilot_agent::manifest::parse_definitions`, which
    `acpilot-extensions` calls for an extension's `agents` contribution too. Do not add a second
    parser anywhere; the only rule that differs by owner is that an extension may not name a
    credential (`env`, `auth`, `allow_cross_profile` are refused in a pack).
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
  - One SQLite per profile (`<data>/profile.sqlite`) with the service phase's task P3-7 schema, plus a small global
    `index.sqlite` (`profiles`, `workspaces(profile, path, first_seen)`).
  - Transcripts under `<data>/sessions/`. `profile rm` moves the directory to the OS trash and
    prints the path; `--purge` is required to delete outright.
  - Daemon opens stores lazily and keeps at most 4 open (LRU) with per-store migration state.
  - [decide] D11: DB-per-profile (recommended) vs one DB with a `profile` column.
    `acpilot sessions ls --all` iterates stores sequentially in the first case; budget ≤50 ms
    for 10 profiles, otherwise build the index.

- [ ] **P4-5 · Per-profile secrets** (1d)
  - **Started 2026-10-22:** `acpilot-agent::keyring` implements the namespace. `KeyringResolver`
    reads the OS store through `keyring` 4.2's `v1` API (Keychain, Credential Manager, Secret Service),
    service `acpilot`, account `<profile>/<name>`, for both `keyring:<name>` and
    `keyring:<profile>/<name>`. The store sits behind a `CredentialStore` trait, so two profiles
    resolving the same entry name to different values, a missing account, and a locked keyring are all
    tested without a desktop session. `CredentialStore` gained write and delete so the CLI can use the
    same namespace rather than a second copy of the rule: `acpilot secret set` reads the value from
    stdin (no `--value`, which would land in shell history and `ps`), `rm` of an absent entry exits
    `3`, and `list` parses the profile's `agents.toml` with a recording resolver and prints only
    `present`/`missing` per account — the keyring API cannot enumerate, so the manifest is the honest
    list of what should exist. Still open: profile resolution (P4-2) supplies the name, legacy
    unscoped entries are adopted into `default`, cross-profile resolutions warn on every use, and
    `profile export` redacts.
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

## Models phase (P4.5) — Local models and the model lab (≈14 days)

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
    (it is the one failure the enum the foundation phase (P1) shipped could not describe without lying).

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
    record reproduces byte-identically on replay, `promote` produces an agent that passes the engine phase (P2)
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

## Mod shop phase (P4.6) — Extensions: the mod shop (≈11 days)

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

- [x] **P4.6-5 · Activation and precedence** (1d) — done: `activation.rs`, `effective.rs`, `ext why`; the daemon's `extension.changed` event waits for the mod-shop phase's task P4.6-10
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
    fine on my monitor" is how a theme ships unreadable. **Done with the mod-shop phase's task P4.6-1**: 34 pairs per
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
    verbs are covered; signature failure arrives with task P4.6-4, grammar degradation with task P4.6-8.

- [ ] **P4.6-12 · Import from other editors** (future, not v0.1)
  - TextMate `.tmTheme` is done (`acpilot ext import-theme`). VS Code (JSON themes, node
    extensions) and Zed's marketplace (server + WASM code extensions) are deliberately future
    work: importing a code-bearing extension means either translating its declarative parts or
    growing a runtime, which is the decision in ADR 0003. Start with VS Code themes, which are
    data, and keep the curated list reviewed by pull request.

- **Planned shelf (2026-10-24)** — the component-system packs (`acpilot.web-components`, `.svelte`, `.vue`, `.lit`, `.solid`, `.react`, `.angular`) and `acpilot.blog`, written up on the docs site's *planned extensions* page
  (`site/src/content/docs/mod-shop/planned.md`). Each pack's first two layers — a grammar and a
  language-server preset — are shippable with the kinds this phase already defines; the third layer,
  rendering the component, needs a `preview` contribution kind plus a sandboxed panel that belongs
  to the desktop and web surfaces. Nothing is on the marketplace until it installs.

---

## Command line phase (P5) — CLI (≈6 days)

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

## Editor phase (P6) — TUI editor (≈25 days)

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
  source is one keystroke away. TUI snapshots via `ratatui::TestBackend`, GUI in the desktop phase (P10), the browser in the web phase (P11).
  - **DoD:** open a 100k-line file, edit with undo, open a session with Claude, stream a turn, answer a permission, accept a hunk, run `cargo test` in the terminal pane — all without leaving the TUI.

---

## Version control phase (P6.5) — Git and the commit writer (≈6 days)

Numbered after the editor phase (P6) so no existing task id moves. The backend and the prompt logic land first;
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

## Language intelligence phase (P7) — Language servers (≈19 days)

LSP lives **in the daemon**, next to the files and the agents — not in the UIs. Three reasons:
servers have to run where the code lives (SSH, WSL); one server per root must be shared by four
clients; and the TUI, GUI, and browser must show the *same* diagnostics. The UIs are clients of
`/v1/lsp/*`, exactly like they are clients of sessions.

Minimum for v0.1: the language-intelligence phase's tasks P7-1…P7-10 and P7-14–P7-15 (local servers only). Task P7-11 rides on the remote-hosts phase (P8) and the Windows phase (P9).
Task P7-13 is the first thing to slip to v0.2 if the phase runs long; task P7-12 does not slip — an
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
  - `async-lsp` 0.2 stays an option until the design phase's task P0-7 reports; the default is our own loop because it has
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
    plus typed routes for the language-intelligence phase's task P7-7.
  - SSE: `lsp.diagnostics`, `lsp.status`, `lsp.progress`, `lsp.message`. Snapshot-tested OpenAPI,
    and a documented WS bridge shape so the web UI can attach `@codemirror/lsp-client` directly.

- [ ] **P7-10 · Save pipeline** (1d)
  - Format-on-save (off by default, per language), format selection, organize-imports-on-save.
  - Writes are serialised per file: the daemon formats *before* acknowledging a save and returns
    the formatted text plus a diff so the UI can rebase its buffer instead of clobbering it.
  - `acpilot format <paths>` and `--check` for CI. Document the interaction with agent writes:
    last writer wins, but both are diffed and both are audited.

- [ ] **P7-11 · Remote language servers** (1d, lands with the remote hosts phase (P8) and the Windows phase (P9))
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

- [ ] **P7-14 · TUI client** (2d, depends on the editor phase (P6))
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

## Remote hosts phase (P8) — SSH hosts (≈10 days)

- [ ] **P8-1** `acpilot-host::ssh`: `SshHost` on `russh` — connect (agent auth, key files, password via keyring, `known_hosts` verification with TOFU prompt), keepalive, reconnect with backoff.
  **Started 2026-10-22:** `acpilot-host::ssh` implements `Host` over one connection — `connect` with a key file and a `known_hosts` policy (strict, TOFU, or explicit accept-any), exec channels that keep stderr out of the frame stream and preserve exit codes, PTY channels with resize, and one session thread multiplexing channels (byte-stream readers on the caller's side, per `docs/notes/ssh-agent.md`). `tests/ssh_host.rs` exercises all of it against the design phase's P0-4 real `sshd`. Still open here: ssh-agent and password/keyring auth, keepalives, reconnect/backoff, `signal`/kill semantics verified against a server, a connect timeout policy beyond the 30 s dial guard, and RSA keys — disabled because the `rsa` crate carries RUSTSEC-2023-0071 with no fix, so `ssh-rsa`-only servers are refused rather than the advisory suppressed.
- [ ] **P8-2** `ssh2-config` parsing: `Host`/`HostName`/`User`/`Port`/`IdentityFile`/`ProxyJump`/`Include`/`ForwardAgent`; surface unknown directives as warnings in `acpilot host test`.
- [ ] **P8-3** Capabilities over SSH: upload `acpilotd-helper` (musl, `scp`-less — write via `exec` + base64 or SFTP), version handshake, then fs/PTY/spawn/LSP through the helper.
- [ ] **P8-4** Remote workspace: root path on remote, file read/write with ETag, `notify`-equivalent via helper polling interval (500 ms) until inotify-over-SSH exists.
- [ ] **P8-5** Remote agent spawn: ACP over an SSH `exec` channel (stdio bridged), stderr captured separately, resize forwarded, clean kill on cancel.
- [ ] **P8-6** Remote terminal: PTY request (`xterm-256color`, current size), resize messages, reconnect that replays the last screen.
- [ ] **P8-7** Fallbacks: `ssh` binary mode via `openssh` crate for configs russh cannot express (FIDO, `Match`, ControlMaster); auto-fallback with a single log line.
- [ ] **P8-8** Tests: docker-compose `sshd` fixture; tests for agent auth, bad host key, dropped connection mid-turn, and a 200 MB file read.
  - **DoD:** edit a file and run an agent on a remote box from the TUI, with the daemon local; kill the network mid-turn and recover.

---

## Windows phase (P9) — Windows + WSL (≈8 days)

- [ ] **P9-1** Windows build green in CI (MSVC), ConPTY verified via `portable-pty` for both local and WSL terminals.
- [ ] **P9-2** `acpilot-host::wsl`: distro enumeration (`wsl.exe -l -q`, registry via `winreg` as fallback), default distro detection, `wsl --cd` support, path translation both ways (`C:\a` ↔ `/mnt/c/a`, `\\wsl.localhost\D\a` ↔ `/a`).
- [ ] **P9-3** In-distro helper: install `acpilotd-helper` into `~/.local/bin` inside the distro on first use, handshake, then all fs/PTY/spawn/LSP happens in-distro (see README §5.6 design note).
- [ ] **P9-4** WSL terminal pane + WSL agent over the helper's stdio; verify interactive TUIs (colours, resize, Ctrl-C) work through ConPTY.
- [ ] **P9-5** UNC fallback for read-only browsing; explicit warning in the UI when editing over 9P.
- [ ] **P9-6** Windows process hygiene: job objects for agent trees, named-pipe single instance, `\\?\` long-path handling, CRLF preservation.
  - **DoD:** on a clean Windows VM, `acpilot workspace open \\wsl.localhost\Ubuntu\home\me\app`, run an agent inside the distro, edit a file, and see the change from `wsl` — no manual setup beyond `wsl --install`.

---

## Desktop phase (P10) — GUI (≈25 days)

- [x] **P10-1 · recorded 2026-10-24** Probe outcome review from the design phase's task P0-3, written up as `docs/adr/0009-gui-framework.md` (ADR 0001 reserved 0009 for the GUI framework; this task said 0007, which is profiles). Records the exact pin, the numbers that justify it, the veto restated as a comparison rather than an absolute 2×, and five recorded triggers that would supersede it. Two claims were corrected on the way: the `zed-font-kit` git dependency does not exist at this pin (zero git sources in the probe lockfile), and the manifest's `"0.2.2"` was a caret that floats across 0.2.x, now `=0.2.2`. Also noted: the probe's default-feature iced build contains no `accesskit` either, so the accessibility bridge is implementation work under either framework.
- [ ] **P10-2** `acpilot-gui` shell on gpui 0.2.2: `Application` + `Entity`/`View` tree, window and menu bar, theme tokens (shared `theme.toml` with TUI), profile switcher, command palette, panes, and keyboard-first navigation via gpui `KeyBinding` contexts (same keymap data as the TUI, not a second hand-written table).
  - **Started 2026-10-24:** gpui is wired in behind a `gui` feature, off by default so `cargo test --workspace` still compiles no renderer. `theme.rs` follows a theme's `extends` chain into `0xRRGGBB` words and refuses a palette with a hole by naming the tokens; `profiles.rs` reads what `--profile` hands it (name, rule count, whether it declares agents); `keymap.rs` is the keyboard table; `palette.rs` is the command list and its matching. The shell draws a menu bar (`Acpilot`, `Profile`), a command palette on `secondary-shift-p`, and the profile list, all reachable by keyboard or click. 28 tests, the two data layers and the palette's matching among them; the view is type-checked and runs.
    The keymap is data with *contexts per binding*: `down` is "next profile" in the shell and "next row" in the palette, and one table row says both, so the TUI can read the same table when the editor phase (P6) gives it a palette. Text entry is read from `on_key_down` because gpui 0.2.2 has no text field — the one place this slice works around the framework rather than with it.
    Platform notes: native menus are macOS-only at this pin, which is why the palette is the path that works everywhere. On this box gpui 0.2.2 **panics on WSLg's Wayland** (`xdg_wm_base` → `UnsupportedVersion`); with `WAYLAND_DISPLAY` unset it opens on Xwayland and runs. Next: panes, then the text surface (the desktop phase's tasks P10-3 and P10-4).
    Two build facts that cost an hour and are now written down: the lockfile must hold `libc` at **0.2.189** for gpui 0.2.2's `xattr` on Linux (ADR 0009 §5), and the Linux link needs `libxkbcommon-x11`'s dev symlink.
    **First paint moved off the critical path (2026-10-24).** Since the trace from the design phase's P0-3 put 320–411 ms of a 616–652 ms start inside `CreateDevice`, and gpui at the pin offers no async or pre-warmed device, the lever is the device's lifetime. `crates/acpilot-gui/src/platform/` now claims a per-user socket (`$XDG_RUNTIME_DIR/acpilot-gui.sock`, `$TMPDIR` on macOS) before gpui starts; a second launch connects, writes a line, and exits 0 — the instance that owns the socket opens the window and activates. Measured on this box (Linux/X11 through Xwayland on Weston, debug build): a cold launch maps its window in 69–70 ms, a repeat launch in 7–8 ms, with the forwarding process gone in 4 ms. On Linux this saves the boot (font system, `Application::new`), not the device — gpui 0.2.2 creates the blade renderer per window, so that 88–90 ms stays in the warm process. On Windows the trace puts `CreateDevice` inside `Application::new`, so a forwarded launch skips it; that ~41–62 ms remainder is inferred from the phase table, not yet measured. Numbers, method and limits: `docs/notes/gui-bench.md` §"What the app does about it". Windows has no channel yet (`Claim::Unavailable`, every launch is a process) — the desktop phase's P10-5 named pipe is the remaining half.
- [ ] **P10-3** Text surface on `acpilot-text`: shaped `LineLayout` wrapping, display map for folds, virtualised rows (`uniform_list`), cursor blink, IME, selection, soft-wrap toggle, minimap (optional/later).
- [ ] **P10-4** Panels: file tree, tabs, editor, agent session list + transcript + composer, permission dialogs, diff review, LSP surfaces (diagnostics, hover, completion, code actions — from the daemon, same as the TUI), terminal (`vt100` grid rendered as a gpui element, reusing the TUI's grid model), and a sandboxed component-preview panel — the surface the component-system extension packs wait on (see the mod shop's planned extensions).
- [ ] **P10-5** System integration: menu bar, open-with, recent workspaces, drag-and-drop files, notifications on turn completion/needs-input, dock/taskbar badge, single instance + deep links (`acpilot://session/<id>`).
  - **Single instance, Unix done (2026-10-24):** landed under the desktop phase's task P10-2 because it is first paint rather than chrome. The seam is `crates/acpilot-gui/src/platform/{mod,unix,windows}.rs`; Windows returns `Claim::Unavailable` until a named pipe exists there, so every Windows launch is still its own process. Deep links will ride the same channel — the request is currently one unparsed line, and `acpilot://` parsing belongs at that accept loop.
- [ ] **P10-6** Performance pass against the budgets in §Performance budgets; `tracing` spans + a frame-timing overlay (gpui has no off-the-shelf integration, so measure in-crate and keep the harness alongside the editor).
  - the design phase's P0-3 phase trace (2026-10-24) puts **320–411 ms of a 616–652 ms start inside `CreateDevice` on Direct3D 11.1**, before any window exists — window creation is 34–46 ms and the view ~0. So the lever is the device's lifetime (pre-warm, reuse, or a splash that precedes it), not a faster view. Numbers: `docs/notes/gui-bench.md`.
  - **Reuse is the lever taken (2026-10-24):** a second launch forwards to the running process instead of starting one (the desktop phase's P10-2 note). Still open on this budget: a Linux first-paint number (the probe's instrument is Windows-only so far), and the Windows figure for a forwarded launch, which the phase table predicts at ~41–62 ms but does not measure.
- [ ] **P10-7** Accessibility — **not inherited**: gpui 0.2.2 ships no `accesskit` bridge, so this is an implementation task, not an audit. Wire focus order, labels, and role/live-region events for the transcript and permission dialog; 200% scaling; high-contrast theme. Timebox a probe early in the desktop phase (P10): if the bridge is not viable by then, ship TUI-parity keyboard paths, document the gap in the release notes, and treat it as a v0.1 limitation rather than silently claiming accessibility.
- [ ] **P10-8** Packaging: `.app` bundle + notarisation (macOS), MSI/MSIX (Windows), AppImage + `.deb` (Linux) via `cargo-dist` + `cargo-bundle`.
  - **DoD:** a user who never opens a terminal can install Acpilot, open a workspace, run an agent, and approve a write.

---

## Web phase (P11) — Web UI (≈12 days)

- [ ] **P11-1** `web/`: Vite + Svelte 5 + TS; layout mirrors TUI panels; profile switcher bound to `X-Acpilot-Profile`; router for `/p/:profile/w/:workspace`, `/p/:profile/s/:session`.
- [ ] **P11-2** API client generated by `openapi-typescript` into `web/src/api/schema.d.ts`; `openapi-fetch` wrapper that injects the token; CI check that regeneration is a no-op.
- [ ] **P11-3** CodeMirror 6 editor: rope-equivalent view over the files API (`If-Match` saves), tree-sitter-wasm highlighting (fall back to `@codemirror/lang-*` if wasm load is slow), diff via `@codemirror/merge`. Attach **`@codemirror/lsp-client` 6.x** through the daemon's WS bridge: the browser never spawns a server, and diagnostics match the TUI byte for byte.
- [ ] **P11-4** Session view: SSE-driven transcript, streaming markdown, tool-call cards, permission prompts (optimistic resolve + reconcile), cancel.
- [ ] **P11-5** Terminal: xterm.js attach over WS, fit + webgl addons, resize, copy/paste, reconnect replay.
- [ ] **P11-6** `acpilot-web` crate: embed built assets (`rust-embed`), `GET /` serves the SPA, `acpilot serve --http --bind 0.0.0.0 --token` with a first-run token printed once; CSRF-safe (token in header only), strict CSP.
- [ ] **P11-7** E2E with Playwright: open workspace → edit → prompt mock agent → approve permission → see diff. Runs in CI against the daemon.
  - **DoD:** from a laptop browser, drive a session running on the dev box over the LAN with a token, with reconnect surviving a daemon restart.

---

## Release phase (P12) — Release v0.1 (≈6 days)

- [ ] **P12-1** `cargo-dist` config: tarballs (linux x86_64/aarch64 musl+gnu, macos arm64/x86_64, windows x86_64), shell + PowerShell installers, Homebrew tap, checksums + SBOM (`cargo-cyclonedx`).
- [ ] **P12-2** Versioning (`0.1.0`), `CHANGELOG.md` via `git-cliff`, release workflow with signed tags, reproducible builds documented.
- [ ] **P12-3** Docs site (`docs/`): quickstart per interface, agent manifest reference, profile + config reference (`docs/config.md`, cascade and origin), host setup (ssh/wsl), policy reference, LSP server setup (`docs/lsp.md`), API reference from OpenAPI, troubleshooting (`acpilot doctor` output explained).
- [ ] **P12-4** First-run experience, with no setup command to run first. The first use of a profile materialises it: `Registry::open_or_create` creates `agents.toml` with the `$schema` line and the shipped presets, once, and never over a file that exists. `acpilot doctor` then detects agents on PATH (`claude`, `codex`, `gemini`, `freebuff`) and language servers, tests one handshake, and the first launch prints next steps. A program that needs no configuration launches with sane defaults and says nothing; a file appears only when there is a profile to save. Decided 2026-10-22: a required `setup` step is a toll booth in front of a product whose whole claim is that the manifest is a file you can edit.
- [ ] **P12-5** Telemetry: **none** by default; optional crash reports via `sentry` behind an explicit flag and a documented payload.
- [ ] **P12-6** Security review: token handling, path traversal, permission bypass attempts, command injection in manifests, LSP binary trust pins (P7-12), cross-profile secret leakage (P4-5), helper upload integrity (signature or hash pin), `cargo audit` clean.
- [ ] **P12-7** Migration story: config/state paths stable from 0.1 (`~/.config/acpilot`, `~/.local/share/acpilot`, one subtree per profile), documented, with a tested upgrade path from a pre-profile layout.
- [ ] **P12-8** *(post-v0.1, noted 2026-10-24)* Update notification: say a newer release exists **once per version**, not on every launch — in `acpilot doctor`, the TUI status line, and the desktop menu, with the upgrade command and no auto-download. Deliberately not v0.1: it means a network call at startup, which P12-5's "none by default" outranks. Noted while wiring ADR 0004's launch path, because the client is already the one place that knows which daemon version it is talking to.

---

## Chat phase (P13) — Chats, calls, and screen sharing (post-v1, ≈40 days)

A chat is where a team decides what the agent should do next, so it belongs where the work
does: one **global** chat per profile, and one **project** chat per workspace. A named agent
joins a chat as a member and speaks only when `@`-mentioned. Calls are opt-in,
human-to-human, and the agent never receives call media.

The chat phase (P13) is deliberately after v0.1. The product is designed around one person and one machine;
a chat adds a second person, which means identity, invites, a server to run, and a data
boundary that did not exist before. Shipping a half-answer here would turn a local-first
tool into an accidental social network.

What is scoped out on purpose:

- **“Global” is per profile, not per machine.** `work` and `personal` cannot enumerate each
  other's chats, for the same reason their secrets cannot cross. A machine-wide chat would
  be the first feature to leak an identity between profiles.
- **Identity is required.** Every member has an account on the chat server, and every
  message and call has a named author. Self-hosted first; a hosted service is a business
  decision with a data-processing agreement attached, not a default.
- **Text is the durable record; calls are ephemeral.** No recording by default. The daemon
  may signal a call but never relays media, so a packet capture of the chat server is not
  a recording of the room.
- **The agent is a member, not a listener.** It is added by name, answers a mention with a
  turn on its own daemon, streams the reply back as a message, and gets the chat history up
  to that mention bounded by a token budget. It is not in the call, and a chat message is
  not a permission grant.

- [ ] **P13-1 · Chat domain and store** (6d)
  - `Chat { id, scope: Profile | Workspace(WorkspaceId), name, topic, archived_at }`,
    `Member { chat, principal, role }`, `Message { id, chat, author, at, body, edited_at,
    reply_to, mentions }`. A separate chat database per profile
    (`<data>/profiles/<name>/chat.sqlite`), so deleting a session does not delete the
    conversation that produced it and `profile export` can redact chats independently.
  - The global chat is created with the profile; a project chat is created from a
    workspace and carries its id. Moving or renaming a workspace does not reparent a chat;
    deleting a workspace archives its chats and leaves the history readable.
  - Message bodies are stored as sent (markdown, code fences, file references), not
    rendered HTML. A file shared into chat is a workspace-relative reference that the
    receiver's daemon resolves locally; a secret never leaves its machine as an attachment.
  - [decide] D21: one chat database per profile vs. chat tables in the profile database.
    Recommendation: a separate file, because message retention and write volume differ
    from session history and a corrupt session store must not take the chat with it.
- [ ] **P13-2 · Chat server and sync** (9d)
  - `acpilot chat serve`: invite-only accounts, per-chat membership, and a resumable
    WebSocket. Every chat has a monotonic server sequence; a client reconnects with its
    cursor and receives exactly the gap, or a snapshot when the gap is older than
    retention — the same rule as SSE, with the same refusal to pretend to catch up.
  - Message ids are server-assigned; retries de-duplicate on `(client, client_message_id)`
    so a flaky phone cannot post twice. Edits and deletions are new records with a
    reference, never in-place rewrites, so a reader that missed the edit can still see the
    original.
  - Presence, typing and read cursors are **not** stored history: they expire, and a
    reconnecting client gets the current state rather than a replay of every keystroke.
  - [decide] D22: self-hosted process + SQLite (recommended) vs. a hosted service. The
    self-hosted form keeps the trust story the same as the daemon's; a hosted form needs
    an operator, a retention policy, and a story for who can read the database.
- [ ] **P13-3 · Agent participants** (7d)
  - An agent member has a display name (`@rust-helper`), an agent id, and a permission
    policy. Only the chat owner adds or removes one. A message without a mention is never
    sent to it; a mention of a name it does not own is not a silent trigger.
  - A mention opens a normal session (the engine phase's task P2-3, the service phase's task P3-4) whose context is the thread up to the
    mention, streams the reply into the chat as it arrives, and links the transcript to
    the message. Cancelling edits the message to say so rather than leaving a half-reply.
  - Its tool calls go through the permission broker (P2-6) exactly as in a session; the
    chat cannot pre-approve a write, and an approval UI shows the chat and the agent that
    asked. `/mute @name` silences an agent for a thread without removing it.
  - The agent is never a call participant and receives no audio, video, or shared-screen
    frames. A summary of a call is something a human writes, not something the agent
    overhears.
- [ ] **P13-4 · Calls: audio and video** (8d)
  - WebRTC for the web UI and the desktop app. Mesh for calls up to four participants; an
    SFU behind the same interface when a fifth joins, so the UI does not change when the
    topology does. A TURN relay is required for symmetric NATs and is configuration, not
    a cloud account; the chat server issues short-lived credentials.
  - Join/leave, mute, camera on/off, device pickers, bandwidth caps via `setParameters`,
    and a transmitting indicator that the UI cannot hide. Joining is muted and
    camera-off; nothing is transmitted until the person turns it on.
  - No recording, no transcription, no server-side mixing in the first cut. “We can add
    recording later” is only true if the architecture does not require a mixer now, so it
    does not.
- [ ] **P13-5 · Screen sharing** (6d)
  - Web: `getDisplayMedia` with a picker and a persistent indicator; a text profile
    (1080p/5 fps) and a motion profile (720p/30), chosen by what is being shared and
    switchable mid-call.
  - Native: a platform seam for capture — ScreenCaptureKit on macOS, Windows Graphics
    Capture, PipeWire portal on Linux — so the desktop app can share a window without a
    browser. The seam lives under `src/platform/`, per D19; the picker and the indicator
    are UI, not platform code.
  - The sharer can stop from Acpilot's own chrome even when the shared window is covered;
    ending the call or closing the window ends the track. Annotating a shared screen is
    not in this phase.
- [ ] **P13-6 · Clients** (6d)
  - Chat panes in the GUI and the web UI: global chat beside the workspace list, project
    chats in the workspace switcher, unread counts measured from your cursor rather than
    from install. The TUI gets chat as a tab with the session keybindings.
  - Mentions autocomplete members and agents; mentioning an agent shows the policy it will
    run under before the message is sent. A message from an agent is visibly an agent's,
    with a link to the session and the tool calls it made.
- [ ] **P13-7 · Tests and privacy review** (4d)
  - Two clients exchange messages across a disconnect and lose nothing; a retry duplicates
    nothing; an agent replies to a mention and to nothing else; a three-way call connects
    through a TURN relay; closing a shared window ends the track.
  - Profile isolation: a member of `work`'s chat cannot enumerate or join `personal`'s,
    even with a valid token for the same server. No message body reaches logs, crash
    reports, or the OpenAPI document; the audit log records mentions and approvals, not
    text.
  - **DoD:** two teammates and one named agent in a project chat, a three-way call with one
    screen share, a reconnect that neither duplicates nor drops a message, and a call whose
    media never touches the daemon's API.

---

## Studio phase (P14) — Studio extensions: API, regex, database, and frontend delivery (post-v1, ≈26 days)

The studio phase's tasks P14-1 and P14-2 are extensions in the mod-shop phase (P4.6) sense, and both are the first real customers of Tier 2
(D18): a panel that speaks HTTP and a panel that runs a regex engine are *code*, not tokens.
Writing them down now is how D18's capability set stops being hypothetical — what they need is
`net.http` (outbound requests, host allowlist, audited with the extension's id) and `compute` (a
pure wasm module with no capability at all). The studio phase's task P14-3 needs `net.socket` (a database connection, host
allowlist, statement timeout) and the same audit trail. The studio phase's task P14-4 needs no runtime at all: it hands out
the frontends the daemon already builds, so it depends on the web phase's task P11-6 and the release phase's task P12-1 instead.

Rules the studio shares:

- **The extension holds the logic; the daemon holds the power.** A send or a grep goes through the
daemon, so it is policy-checked, audited, cancellable, and visible in the session transcript like
any other tool call. An extension that opened its own socket would be a hole in the one promise
the product makes.
- **What the user owns is a file.** Requests and patterns are TOML in the profile (`api/`,
  `regex/`), shareable, diffable, and shippable as an extension pack — not rows in a database the
  user cannot read.
- **One data path, three surfaces.** The TUI gets list + result; the GUI and web get the rich view;
  both read the same route, so the second implementation is a renderer and not a second product.
- **Every action is a tool an agent can call.** A panel a person can drive and an agent cannot is a
  feature that only works when someone is watching. Each action is exposed on the broker as
  `<extension>.<action>` with its target in the permission subject (P14-5), which is what lets one
  prompt cross all four: import the spec, call the endpoint, assert the row.

- [ ] **P14-1 · API workbench** (~9d)
  - Import OpenAPI 3.0/3.1 and Swagger 2.0 from a file or a URL (fetched through the daemon), with
    `$ref` resolution, several specs per profile, and a stable operation id so a saved request
    survives a spec that renames its path.
  - **DoD accepts no substitute: the first spec it must import is the daemon's own
    `/v1/openapi.json`.** That makes Acpilot a client of its own API — two calls from the panel
    (`/v1/agents`, `/v1/permissions`) with a profile token, both in the audit log with the
    extension's id — and every wire change becomes a regression test for the panel as a side effect.
  - Request builder from an operation: path/query/header/cookie parameters, JSON/form/multipart
    bodies generated from the schema, examples, and required-field validation *before* the send
    rather than a 400 explaining it afterwards.
  - Environments as TOML with `keyring:` refs for secrets and per-environment overrides; the value
    never round-trips to the UI (only `credential_ref`), which is the same rule the model providers
    already follow. `.env` import, but never `.env` auto-read.
  - Auth: none / bearer / basic / API-key / OAuth2 client-credentials with a token cache the daemon
    holds. A refresh token in a panel's memory is a secret in a screenshot.
  - Sending: policy per host — the spec's servers are the allowlist, everything else is denied
    until a rule says so — with time and size caps, redirects off unless configured, and the
    response streamed so a 40 MB body does not arrive as one allocation.
  - Response: status, timing, size, JSON tree with copy-json-path, headers, raw; save as example;
    diff two responses on the same surface as agent edits, because "what changed between these
    two deploys" is the question people actually have.
  - Collections: request chaining (a value from response *n* into request *n+1*), and an
    agent-visible tool so an agent can *ask* for a request to be run instead of being handed the
    token — the same broker decision as a write, with the same audit row.
  - Authoring aids: validate and lint a spec (dangling `$ref`, undocumented 4xx, unused schemas),
    and a breaking-change diff between two versions of the same document.

- [ ] **P14-2 · Regex lab** (~5d)
  - Live pattern editing over a test corpus: match highlighting, capture groups (named and indexed)
    with their spans, and a replacement preview with `$1`/`${name}` expansion.
  - Flavors behind one `Engine` trait, each a wasm build: Rust `regex` (linear time, no
    backreferences or lookaround — the panel says so instead of failing mysteriously), PCRE2,
    JavaScript, Go RE2. The flavor is chosen per pattern, not per profile, because comparing them
    is the point.
  - Explanation: the parsed AST as an indented tree with the sub-pattern that produced each node.
    The payoff is teaching, and it is what regex101 is actually used for.
  - Test corpus with per-match expectations, saved beside the pattern so a pattern is a small
    regression suite; run over workspace files through the daemon's grep path (the one the editor phase (P6) and the language intelligence phase (P7) use),
    capped and cancellable.
  - Benchmark: matches/µs over the corpus, and a catastrophic-backtracking warning **only** for the
    flavors where it exists — warning on Rust's linear-time engine would be a lie.
  - Share: a pattern is a TOML file; a pack of curated patterns is an extension, which is also how
    the agent prompt library and the snippet set get reusable, tested regexes.
  - **DoD:** a pattern with named groups, three cases and a replacement round-trips through a file,
    renders in the TUI and the GUI from the same data, and names the failing case by line.

- [ ] **P14-3 · Database workbench** (~8d)
  - Drivers: SQLite (already `rusqlite`, bundled) and PostgreSQL first, behind one narrow trait
    (`connect`, `describe`, `query`, `execute`, `explain`), so MySQL and MSSQL arrive as a driver
    rather than as a fork of the panel.
  - Read: a schema browser — tables, columns with types and nullability, indexes, foreign keys, row
    counts marked as estimates — a query editor completed from that schema, `EXPLAIN` output, and a
    result grid that pages server-side instead of materialising two million rows into the window.
  - Write: an edit is a generated `UPDATE … WHERE <primary key>` shown **before** it runs and applied
    in a transaction the user commits. A table with no primary key is read-only and says why.
    `UPDATE`/`DELETE` without a `WHERE`, `DROP` and `TRUNCATE` are denied by default and need an
    explicit approval, because "the agent tidied the database" is not a recoverable sentence.
  - Secrets: a connection is `dsn = "keyring:acpilot/work/staging-db"` in `db.toml`, never a password
    in a file the window reads. Read-only is a per-connection flag, not a promise.
  - AI support that keeps the rule above: the agent sees the *schema* — tables, columns, keys, a row
    count estimate — and never the DSN. It proposes SQL, the panel shows the statement and the rows
    it would touch, and running it is `db.execute` through the broker with the same audit row a
    terminal command gets.
  - Agent tools: `db.schema` (a bounded description), `db.query` (read, row- and time-capped),
    `db.execute` (permissioned), and `db.assert` (a query plus the rows expected) — the "is it
    responding properly" check a script or a turn can gate on.
  - Migrations: show what the repository's migration directory would apply, in order, and what it
    would skip; applying is the same approval an agent gets, never a button that runs on open.
  - **DoD:** an agent creates a table, inserts a row, reads it back and asserts it through the four
    tools; a `DELETE` with no `WHERE` is refused by name, and the refusal names the rule that did it.

- [ ] **P14-4 · Frontend delivery over the daemon** (~2d)
  - The daemon already serves the SPA it embeds (P11-6). This turns that into a *distribution
    channel*: a versioned index of the frontends this daemon can hand out — the web bundle it
    carries, plus the CLI/TUI/GUI artifacts `cargo-dist` produced (P12-1) from the directory the
    operator points at — each with its `sha256` and the release it came from.
  - Why it is the daemon and not a CDN: it is the one thing a remote, WSL or SSH box already
    trusts and already holds a token for, so "open a browser at `http://host:7654/app`" beats
    "install Node, clone, build". It also removes API/client skew by construction — a client served
    by this daemon matches this daemon's document, which is the failure mode a pinned release page
    cannot fix.
  - `GET /v1/frontends` → `[{id: "web", version, sha256, size, url}, …]`, and
    `GET /v1/frontends/{id}/{version}` streams the artifact with `ETag` and range support. Both
    behind the same token as every other route: an unauthenticated scan should learn nothing, not
    even what exists.
  - **Off by default**, because serving executables over HTTP is a supply-chain surface:
    `serve.frontends = ["web"]` (or `["web", "binaries"]`), never auto-installed by a client, and
    the client verifies against the checksum it has — refusing a mismatch by name — rather than
    trusting the response.
  - The web bundle is served from the binary (the `rust-embed` from the web phase's P11-6), so the air-gapped case needs
    no release disk at all; binaries come from the release directory, because embedding five
    installers in the daemon is bytes nobody asked for.
  - **DoD:** on a host with no outbound network, a browser pointed at `http://<host>:<port>/app`
    loads the SPA that talks to that daemon, and `acpilot web install --from <url>` fetches and
    verifies the same bundle; a tampered artifact is refused by name, and an unauthenticated
    request to either route is a 401 that names no frontend.

- [ ] **P14-5 · The studio tool contract** (~2d) — the agent-facing half of the studio phase's tasks P14-1…P14-4
  - Naming: `<extension>.<action>` (`api.import`, `api.request`, `db.query`, `db.assert`,
    `regex.test`), one description format, and one JSON schema per tool generated from the
    extension's own types rather than hand-written a second time.
  - One permission subject: the broker sees `kind = extension`, the tool name, and the **target** —
    host for `api.request`, DSN name for `db.execute`, workspace path for `regex.grep` — so a rule
    can allow reads on staging and deny them on production by name.
  - One audit row per call carrying the extension id, the tool, the target and the deciding rule,
    in the same table the service phase's task P3-7 builds for everything else.
  - Tools appear in `GET /v1/tools` and in the session's capabilities, so an agent discovers them
    the way it discovers files and terminals; a tool whose extension the profile has not activated
    does not exist, which is what keeps activation meaningful.
  - The sentence the whole phase exists for: *"add a `POST /widgets` endpoint per the spec,
    implement it, and check the widgets table round-trips"* — the agent imports the spec with
    `api.import`, edits the code, calls the endpoint with `api.request`, and asserts the row with
    `db.assert`. Without this item that sentence is four features that do not know about each other.
  - **DoD:** that prompt, on a fixture workspace, with five tool calls in the audit log, the code
    edit and the row write both approved by a human, and the assertion passing because the row is
    really there.

- **Blocked on:** D18 (Tier 2 capabilities and the resource budget) and the `wasmtime` runtime ADR
  0003 defers, for the studio phase's tasks P14-1, P14-2 and P14-4. If D18 lands as "no runtime in v0.1", those ship as
  first-party panels behind a feature flag instead — a named fallback, not a shrug — and the two
  capability names above are still what the runtime has to answer for when it does arrive. The studio phase's task P14-5 is
  blocked on nothing: it is what turns the panels into something a prompt can drive.

---

## Forge phase (P15) — GitHub, GitLab and Bitbucket (post-v1, ≈18 days)

Pull requests, review comments, checks and project boards live where the code is hosted, and an
editor that cannot show them makes people leave it four times an hour — which is the one thing a
session-oriented workbench is supposed to stop. This phase puts them beside the session that is
changing the branch.

**It is a built-in crate, not an extension pack.** ADR 0003 made extensions data: a manifest, a
theme, a preset, nothing executed at install. A forge integration is network code holding a
token, so it cannot be a pack without repealing that rule for the most privileged thing in the
product. The seam is a `Forge` trait in a new `acpilot-forge` crate, compiled in; a third-party
forge is a crate that implements it, and a downloadable integration is the studio phase's (P14)
sandboxed tier once that runtime exists. Naming that now keeps "plugin" from quietly meaning two
security models.

**One surface, three clients.** Each forge normalises into the same small types — `PullRequest`,
`Review`, `Check`, `Issue`, `Pipeline` — because that is what a pane draws. What normalisation
cannot express (GitHub's merge queue, GitLab's merge trains, Bitbucket's task lists) is not
dropped: a raw view shows the forge's own JSON, and the parity below says which column a feature
is in rather than pretending all three are the same API.

| Surface | GitHub | GitLab | Bitbucket Cloud |
| --- | --- | --- | --- |
| Pull/merge requests, diff, comments | yes | yes | yes |
| Checks / pipelines and job logs | Checks API | Pipelines | Pipelines |
| Issues | yes | yes | yes |
| Project boards | Projects v2 (GraphQL only) | Boards | no |
| Submit a review | yes | yes | yes |
| Merge queue / merge trains | yes | yes | no (raw only) |
| Code search | yes | yes | limited |

**Hosts, not just `api.github.com`.** A host is `{ kind, base_url, profile }`, so GitHub
Enterprise, self-managed GitLab and Bitbucket Data Center are the same three clients pointed
somewhere else. The first enterprise user would otherwise find the feature hard-coded to a URL.

**Auth is the profile's.** The token lives in the keyring under the profile's namespace, the same
store the models phase's P4-5 uses for provider keys — never in `config.toml`, never in the
workspace, never in a log line. Read scopes by default, write scopes opt-in per host, because a
pane that can comment is a pane that can comment on the wrong repository. OAuth device flow is a
later addition (D5's pattern for LAN tokens): a personal access token is what a self-hosted
instance can actually issue today, and it is the same storage either way.

**Rate limits shape the polling, not a fixed timer.** GitHub allows 5,000 requests an hour and
charges GraphQL by points, GitLab 2,000 a minute, Bitbucket 1,000 an hour. So: a per-host token
bucket, conditional requests (`If-None-Match` against the `etag` the forge returns — the same
culture as the service phase's P3-5), a poll interval computed from the remaining budget instead
of a guessed 30 s, and GraphQL on GitHub for pull lists because one query costs a point where
thirty PRs cost thirty requests. When the budget is spent the pane says so; a feature that fails
silently when throttled teaches people it is broken.

**No webhooks.** A local daemon has no address to receive one, and hosting a relay is a service —
which is exactly what P12-5's "no telemetry by default" and this repository's "no accounts" line
exist to avoid. Refresh is on window focus, on `git fetch` of the session's branch, and on a timer
that backs off when the pane is hidden.

**Cached, never authoritative.** Pull requests, checks and comments land in the SQLite database
(P3-7) keyed by host, repository and number with the forge's `etag` and `fetched_at`, so a pane
opens instantly, works on a train, and is replaced wholesale by the next successful fetch.

**Untrusted input, explicitly.** A forge's markdown is sanitised and remote images are not loaded
by default; links open in the browser rather than being fetched by the daemon; a diff is text and
never a hook to run. The token is attached only to the configured `base_url`, and every redirect
or `next` link is re-checked against it — a bearer token sent to the wrong host is the entire
failure mode of this phase. Write actions (comment, approve, request changes, merge, close) are
`kind = forge` permission requests with the target in the detail, so they go through the broker
and the audit table like every other irreversible thing, and the confirmation names the forge and
the repository, not just "merge".

- [ ] **P15-1** `acpilot-forge`: the `Forge` trait, the normalised types, one pagination loop, and
  the mapping from 401/403/404/429 to the API's error envelope (a throttled forge is `502`
  `host_unavailable`, not a `500`).
- [ ] **P15-2** Hosts and secrets: `[[forge]]` in `config.toml` (`kind`, `base_url`, `profile`),
  token in the keyring, and `acpilot forge test <host>` reporting the scopes the token actually has
  — asked, not assumed.
- [ ] **P15-3** GitHub: pull requests, checks, reviews, issues, Projects v2 over GraphQL, code
  search, releases.
- [ ] **P15-4** GitLab: merge requests, pipelines with job logs, approvals, issues, boards.
- [ ] **P15-5** Bitbucket: pull requests, pipelines, issues, workspace projects.
- [ ] **P15-6** Cache, rate limiter and conditional requests, with the throttled and offline states
  as first-class UI states rather than an error toast.
- [ ] **P15-7** Daemon routes: `GET /v1/forges`, `GET /v1/forges/{host}/repos/{repo}/pulls`,
  `.../pulls/{number}`, `.../pulls/{number}/checks`, `.../issues`; writes as `POST` behind the
  broker. The API is the only path, so the CLI gets `acpilot pr ls` for free and no UI grows its
  own HTTP client.
- [ ] **P15-8** UI: a pull-request pane for the session (branch → open PR), checks, diff and
  comments; an issue/board view; a pipeline log view capped and streamed, not downloaded.
- [ ] **P15-9** Write actions with confirmation and an audit row: comment, approve, request
  changes, merge, close, create issue.
- [ ] **P15-10** Fixtures: recorded JSON per forge under `tests/fixtures/forge/`, a mock server for
  the routes we call, no network in CI, and a live tier behind `ACPILOT_FORGE_LIVE=1` like the
  design phase's probes — the same shape, because the risk is the same.
- [ ] **P15-11** Docs: one page per forge naming the token scopes it needs and what is not
  supported, plus the parity table above kept honest as the APIs move.
  - **DoD:** from a session on a branch with an open pull request, the pane shows the PR, its
    checks and the review comments without a browser; approving from the pane appears on the
    forge; the failing pipeline's log is readable in place; with the token deleted the pane says
    "no token for this host" instead of showing an empty list; and with the network down the
    cached PR still opens.

- **Blocked on:** the service phase's SQLite (P3-7) for the cache, the profiles phase's secrets
  (P4-5) for the token, and the version-control phase (P6.5) for turning a session's branch into a
  pull request without asking the user which repository they meant.

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
- [ ] Web: Playwright in **the web phase (P11)**, against `acpilot serve` with a temp profile — no mocked routes.
- [ ] TUI: headless snapshots via `ratatui`'s `TestBackend` in **the editor phase (P6)**, including the stats bar
  at known dimensions and a 200×60 frame budget.
- [ ] GUI: a gpui test harness in **the desktop phase (P10)** (frame + text layout assertions), added with the crate.
- [ ] SSE: a live-server suite that opens the stream, drops it, reconnects with `Last-Event-ID`,
  and asserts ordered replay with no gaps (**the service phase (P3)**).

### Dependencies & upgrades

- [ ] **XT-1** Continuous updates: automation (Dependabot or Renovate) opens a PR the day a release lands — patch/minor auto-merge on green CI, majors get a human. `cargo update` weekly even when no manifest changes, because the lockfile is what ships. Every upgrade runs the design phase's P0-3 harness, so a performance regression fails the PR instead of surfacing months later. gpui is the one exact pin (D1) and still moves within a week of a release; when the GUI-builder project starts, both repos move to one `rev` and share one patch series.
- [ ] **XT-2** Reproducible GUI builds: `cargo vendor` (or a registry mirror) exercised in CI from a clean checkout with no `~/.cargo/registry` or `~/.cargo/git` cache. There is no git dependency at the pin (ADR 0009 §5), so the check is an offline `cargo build --offline` after vendoring rather than a hunt for git revs — the point is that a build never needs the network, not that it never needs GitHub.
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

The first two rows were measured by the design phase's task P0-3 on this machine (2026-10-22, Windows 11,
RTX 5060, warm caches): keystroke→paint is met by both `gpui` and `iced` at
4.1–4.6 ms p95; **100k-line first paint is not met by either** — gpui 277–326 ms,
iced 851–896 ms — and the cost is window/device creation, not the view, which is
~0 ms per frame. Until that phase has a profile, read 150 ms as a target for how
the app *starts* (pre-warmed process, restored window), not a number a framework
reaches from a cold process. Numbers and method: `docs/notes/gui-bench.md`; the
decision they settle is `docs/adr/0009-gui-framework.md`.

### Security

- [ ] Every filesystem/terminal/permission path goes through the broker; no direct agent access to the OS outside policy.
- [ ] Language servers are programs, not libraries: trust-pinned on first use, no auto-download (P7-12), restricted to the workspace root.
- [ ] Profiles are isolation boundaries: separate keyring namespace, separate stores, `409` on cross-profile access, audited `--steal`.
- [ ] Default policy: read anywhere in workspace, ask on write, deny outside workspace, deny network-spawning tools (configurable).
- [ ] Audit log is append-only and included in `session.jsonl`.
- [ ] Secrets: keyring only; redact secret-looking strings from logs and transcripts (`sk-`, `ghp_`, bearer tokens).
- [ ] Sandbox probe (later): `landlock` (Linux), `sandbox-exec` (macOS), AppContainer (Windows) per-session; [decide] whether v0.2.

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
gpui                       = "=0.2.2"          # exact pin; see D1 + ADR 0009 + XT-1. Escalate to { git = "…/zed", rev = "<sha>" } only with an ADR note.
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
async-lsp                  = "0.2"           # probe candidate only; see P7-2
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
| GUI framework churn (gpui/iced) | Losing months | Design phase's P0-3 probe with hard numbers, UI-agnostic `acpilot-text`, bounded fallback path |
| gpui has no semver contract | A routine upgrade eats a week; a moved rev breaks the build | Exact pin, vendored builds in CI (XT-2), release-watch automation (XT-1), ADR records pin + escape hatch |
| gpui ships no accessibility bridge | Screen-reader users locked out; retrofit arrives too late | Desktop phase's task P10-7 is an implementation task with an early viability probe, keyboard-complete TUI parity, gap documented if unresolved |
| ACP v2 draft lands with breaking changes | Rework in the engine | Isolate all protocol types in `acpilot-agent`; never leak ACP types into `acpilot-proto` |
| Agents are non-deterministic; tests flake | Slow CI, mistrust | Mock agent for tests, real agents nightly only, transcript snapshots |
| Remote file watching over SSH is polling | Stale UI, battery drain | 500 ms coalesced poll, ETag revalidation, explicit "remote" indicator, inotify-over-ssh later |
| Program-as-agent parsers are brittle | "Run anything" promise breaks | Ship 2 generic parsers, make profiles data-driven and user-editable, keep ACP-native path primary |
| Permission broker bugs = data loss | Trust destroyed | Default-deny writes, shadow-write guard, audit log, property tests for path canonicalisation |
| Scope creep across four UIs | Never ships | API-first rule; v0.1 is CLI+TUI only; GUI/web gated on daemon stability |
| LSP servers hang, crash, or index forever | Editor feels broken; RAM probes | Timeouts + `$/cancelRequest` everywhere, per-server stderr ring, idle shutdown, crash-loop cap, nothing on the keystroke path |
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

- [x] D1 · GUI framework: **gpui 0.2.2** (decided 2026-10-20 — a sibling GUI-builder project will use gpui, so framework knowledge and pins are shared; the text stack and the reference ACP client come for free). The design phase's task P0-3 did not veto it: `iced` 0.14 was measured at the same frame pacing but ~3× slower to first paint (851–896 ms against 277–326 ms), so the fallback stays a fallback (`docs/notes/gui-bench.md`). Recorded 2026-10-24 in `docs/adr/0009-gui-framework.md`, where the pin was made exact and the veto restated as a comparison.
- [ ] D2 · Multi-client concurrency model: last-write-wins + ETag conflict UI (v0.1) vs CRDT (`loro`/`automerge`) later.
- [ ] D3 · Web frontend: Svelte 5 + CodeMirror (recommended) vs Leptos/WASM.
- [x] D4 · Local IPC: **HTTP over a unix socket, one protocol everywhere** (decided 2026-10-24, ADR 0004). A second JSON-RPC transport would be a second error model, a second auth story and a second client for no gain. The socket also carries the auth story: a `0700` directory and a `0600` socket make the filesystem the access boundary, which is what lets the documented "loopback needs no token" rule stay true. TCP loopback becomes an explicit `--addr` opt-in and takes a bearer token, because a TCP listener cannot inherit the filesystem's answer about who is allowed to connect. The same ADR settles the lifecycle around it — one daemon per user, spawned on demand by any client under a lock, with a 2 s readiness wait, a version refusal instead of an automatic kill, and idle exit after 30 minutes with no stream, no pending request and no turn in flight.
- [ ] D5 · Daemon auth for LAN exposure: static token (v0.1) vs OIDC device flow.
- [x] D6 · Licence, URL and versioning of the agent manifest schema: **Apache-2.0, `https://rudeboy.dev/acpilot/schema/agents-v1.json`, major version in the filename, frozen once shipped** (decided 2026-10-22). *Licence:* Apache-2.0 rather than the repository's `MIT OR Apache-2.0`, because this artifact's whole job is to be vendored into another editor's tooling or a pack's CI, and the patent grant is the half MIT does not have; a dual offer just invites the weaker half. It is a subset of what the repo already offers, so nothing in-repo changes — `LICENSE-APACHE` is the text, and the statement sits in the schema's `$comment` and on the manifest page, where someone copying the file will see it. *URL:* the docs site already owns that domain and its Pages deployment, and the file lives at `site/public/schema/agents-v1.json`, so publishing the schema is the same commit as publishing the docs — no second service, no redirect chain to rot, and `skamansam.github.io/acpilot/schema/agents-v1.json` still resolves the same bytes. The `$id` **is** that URL, so the document's identity is the address people paste. *Versioning:* the major is in the filename and matches `schema_version`; within v1 the document may only gain optional keys or enum members, anything a `schema_version = 1` loader would read differently ships as `agents-v2.json` with `schema_version = 2`, and v1 is never edited or deleted again. No `latest.json` and no redirect: a moving URL is how an editor's cached copy falls out of step with the loader silently. Enforced rather than promised — the file is generated (`cargo xtask schema`) and a drift test fails when the committed copy differs, while another asserts the `$id`'s file name matches the committed path.
- [ ] D7 · Whether `acpilot` ships as one binary with subcommands (recommended) or separate `acpilot`/`acpilotd` binaries everywhere.
- [ ] D8 · LSP wire types: `gen-lsp-types` 0.11 with the `url` feature (recommended) vs `ls-types` 0.0.6 (3.18 proposals) vs `lsp-types` 0.97 (unmaintained, `fluent-uri`). Isolated behind `acpilot-lsp::wire` either way.
- [x] D9 · Position encoding: **advertise `utf-8` + `utf-16`, use the server's answer, default to UTF-16 when it says nothing** (decided 2026-10-21 on the numbers from the design phase's P0-7). UTF-8 saves 10 ns per conversion and is what rust-analyzer picks; always-UTF-16 would be wrong for it, and assuming UTF-8 breaks the two servers that advertise nothing. Index rebuild, not conversion, is the cost that matters. See `docs/notes/lsp-servers.md`.
- [ ] D10 · Profile isolation: one daemon with N stores (recommended — profiles must run concurrently) vs one daemon per profile (stronger blast-radius, worse UX).
- [ ] D11 · Profile storage: database per profile (recommended) vs single database with a `profile` column.
- [ ] D12 · Language-server installation: detect-only in v0.1 (recommended) vs managed downloads with checksums.
- [ ] D13 · Profile inheritance: `extends` merge vs copy-on-create only.
- [x] D14 · Model adapter seam: **one `ChatBackend` trait, `LmStudio` native plus `OpenAiCompatible`, new variants only with their adapter** (decided 2026-10-20, ADR 0002). Anthropic/Gemini are not OpenAI-shaped and are explicitly out of v0.1.
- [x] D15 · Remote driver trust: **ACP bridge with scopes, `answer_permissions` off by default, permission prompts route to a human** (decided 2026-10-20, ADR 0002). TLS termination is the operator's tunnel in v0.1.
- [ ] D16 · Token counting: bytes/4 heuristic with a documented error band (recommended for v0.1) vs bundling a tokenizer per model family. The lab's `estimated` flag exists so this can change without rewriting the UI.
- [x] D17 · Extension registry trust: **signed static index, mandatory SHA-256, reviewed pull requests, no auto-update** (decided 2026-10-20, ADR 0003). A private registry is `--index` plus a trusted key.
- [ ] D18 · Extension code execution: **Tier 2 (wasmtime, ABI 1, declared capabilities) proposed, not v0.1** (ADR 0003). The open question is the capability set and the resource budget, not whether to have one. The studio phase (P14) supplies the first two customers and so the first two answers it owes: `net.http` (host allowlist from the spec being imported, audited with the extension id, time/size caps) for the API workbench, and `compute` (no capability, a CPU and memory budget) for the regex lab.
- [x] D19 · Platform order: **unix and macOS first, Windows last, and every OS branch under `src/platform/`** (decided 2026-10-21, ConPTY landed 2026-10-22). The Windows half is the one that cannot be developed on the machines the project is written on, so writing it first means writing it blind. `acpilot-host` has the seam: `Host::spawn` is portable (`std::process`) with one implementation; the local PTY is one implementation too, because `portable-pty` already branches (`platform/unix.rs` forkpty, `platform/windows.rs` ConPTY) and the platform files hold what differs *above* that — echo, `\r\n`, the terminal queries a ConPTY program blocks on, and the fact that killing a ConPTY child does not hang up its tree. `crates/acpilot-host/tests/local_host_windows.rs` asserts the property that matters (a ConPTY child sees a console where a piped child sees a redirection) and runs on `windows-latest` in CI. The rule is enforced, not remembered: a boundary test fails if `cfg(windows)`/`cfg(unix)`/`cfg(target_os…)` appears outside `src/platform/` in any crate, and `cargo xtask check` type-checks the crate for the windows-msvc and both apple targets from any host.
- [x] D20 · Naming: **`probes/` and "probe", not `spikes/` and "spike"** (decided 2026-10-22). "Spike" is Extreme Programming jargon for a time-boxed investigation; it says nothing about measuring, which is what every one of these does, and it implies throwaway code, which `probes/wsl-helper` is not. A probe is one question, answered with numbers in `docs/notes/`, in a standalone crate with its own lockfile; it outlives its question only while something still runs it. The rule for what belongs there is in `CONTRIBUTING.md` §Probes.
- [ ] D21 · Chat storage: a separate `chat.sqlite` per profile (recommended) vs. chat tables in the profile database. Message retention and write volume differ from session history, and a corrupt session store must not take the chat with it.
- [ ] D22 · Chat server: self-hosted process + SQLite (recommended) vs. a hosted service. The self-hosted form keeps the trust story identical to the daemon's; a hosted form needs an operator, a retention policy, and a statement of who can read the database.
- [x] D23 · Config format: **TOML for the files a person maintains, JSON only on the wire** (decided 2026-10-22). `[[agents]]` renders one agent as one block, so adding one is a clean hunk in review; the same data in JSON is an array element that puts a comma on the previous agent's last line in every diff of the file whose whole purpose is "add a line, never a rebuild". TOML has no `null`, so "absent means the default" has one spelling — the idiom `idle_timeout_ms` depends on, and the reason `config.toml` and the API are deliberately not interchangeable: compaction patches need absent-vs-cleared. Comments stay in the file the user owns, so the reason `submit_delay_ms` exists is a comment rather than a `description` field the parser has to carry; and a tool that edits these files must use `toml_edit` so the human's comments and ordering survive a rewrite, which a JSON writer would discard. `toml` also reports spans, so "unknown key" can name a line, which the config page already promises. JSONC was the real contender — it is what IDE settings use, and it has comments — but it has no readable array-of-tables, the `toml`/`serde_json` path we already have does not parse it, and its comments are a patch for a format that cannot express them rather than a structure we need. Depth is the one cost: `[a.b.c]` is about as deep as a file should go before the shape is asking for its own file. Publishing a JSON Schema for the TOML does not move the format — TOML self-identifies with a quoted `"$schema"` key or a `#:schema` line, both of which Taplo and Even Better TOML read — so D6 is the decision that governs how that artifact is licensed.
- [x] D24 · `extension.toml` format: **stays TOML** (decided 2026-10-22), by D23's rule but on its own facts. It is a human-maintained file in the extension author's repository, reviewed by a human in a pull request against `marketplace/index.toml`, and written once. Its one JSON-shaped corner is `[[contributes]]`, which TOML renders as repeated inline tables where JSON would have an array of objects — the readability argument that decided D23 inverted — but a pack contributes one to five things, edited rarely, so the per-diff cost is small. Against flipping: a second syntax in the loader, a second error style in `ext add`, a second published schema, and a manifest page that has to agree with both. The rule to keep: if the manifest starts carrying *generated* fields — the signed index's per-version checksums (P4.6-4) are the likely ones — do not convert it and do not let a tool reformat it. Split the artifact instead: `extension.toml` hand-written, the generated store record (versions, checksums, signature) JSON. That is D23's "JSON for the wire and for generated state" applied to the store rather than to the manifest.
- [x] D25 · The extension manifest's published schema: **`https://rudeboy.dev/acpilot/schema/extension-v1.json`, Apache-2.0, frozen once shipped, generated by the same command as the agent schema** (decided 2026-10-22). D6's argument applies unchanged — the artifact exists to be vendored into an extension author's editor and CI, the docs site already serves that directory, and `$id` is the address people paste — so this records only what differs. It is generated from `Manifest` by `schemars` and rewritten by `cargo xtask schema` alongside `agents-v1.json`: one command, one commit, no way to publish one document and forget the other, with a drift test in each crate regenerating its own file. `ExtensionKind` is published from `acpilot-proto`, where the loader's enum lives, rather than restated as a list beside the generator. What JSON Schema cannot say stays the loader's alone and is named in the document: two contributions with the same kind and path, and the `.`/`..` half of the path rule. This does not reopen D24 — the file stays TOML, and the schema describes it to an editor rather than replacing it.
- [ ] D26 · Extension capabilities as agent tools: **every studio extension action is a brokered tool named `<extension>.<action>`, with its target in the permission subject and one audit row per call** (recommended, studio task P14-5) vs each panel owning a private surface and an agent asking a human to click. The second is cheaper this week and makes the studio unusable from a prompt; the first is why the extensions exist at all. Open only because the tool description format and the subject shape are the studio phase's P14-5 to settle.

## Definition of done for v0.1.0

- [ ] fresh install → `acpilot tui` on macOS, Linux, Windows, with no setup step before it; open a workspace, edit, save.
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
