# Design phase's task P0-7 — language servers: what three real servers do

Measured 2026-10-21 on Linux under WSL2 (kernel 6.18.33.2-microsoft-standard-WSL2,
x86_64), Rust 1.97.0. Fixtures were opened from the distro's own filesystem
(`/home`), not from `/mnt/c`, so the index timings are not Windows-drive timings.
Servers:

| server | version | invocation |
| --- | --- | --- |
| `rust-analyzer` | 1.97.0 (2d8144b 2026-07-07) | `rust-analyzer` |
| `typescript-language-server` | 6.0.1, TypeScript 6.0.3 | `typescript-language-server --stdio` |
| `pyright-langserver` | 1.1.414 | `pyright-langserver --stdio` |

The instrument is `crates/acpilot-lsp/examples/lsp_probe.rs`. It frames stdio itself
([`wire`](../../crates/acpilot-lsp/src/wire.rs)), and it negotiates
`general.positionEncodings = ["utf-8", "utf-16"]`. It opens one file and waits for
diagnostics. Then it asks for a definition and converts every returned range back
to a byte offset. With `--expect`, the definition must land on that exact substring or
the probe exits non-zero. Fixtures are in `probes/fixtures/lsp/{rust,typescript,python}`.

## Verdict

1. **Assume nothing, negotiate, and keep the conversion.** `rust-analyzer` chose
   `utf-8`. Both `typescript-language-server` and `pyright-langserver` advertise no
   `positionEncoding` at all, which means UTF-16. A client that assumes UTF-8 gets
   the wrong column from two of the three as soon as a line contains an emoji. A
   client that assumes UTF-16 leaves rust-analyzer's offer unused. Both behaviours
   were observed, and both gave the right column.
2. **The conversion is not the cost.** On a 100k-line, 6.7 MB file, one offset→position conversion takes 17.6 ns in
   UTF-8 and 28.0 ns in UTF-16. The reverse takes 10.0 ns and 16.1 ns. A keystroke will never notice. Rebuilding the
   line index costs 2.26 ms, which *is* relevant: the language-intelligence phase (P7) must make the index
   incremental, not re-run it per edit.
3. **Publish diagnostics are universal. Pull diagnostics are not.**
   `rust-analyzer` advertises `diagnosticProvider` and answers
   `textDocument/diagnostic`, but its first answer was `-32802` (RequestCancelled)
   and the second succeeded. The other two only publish. A client needs the
   publish path regardless, and a pull path that retries cancellation.
4. **`$/progress` is not optional UX.** During the first index,
   `rust-analyzer` sent 133 `$/progress` notifications (14 begin, 105 report, 14
   end). When the first diagnostics arrived it had begun 14 and ended 12, so an
   unmatched begin is normal while work is still running. After a 500 ms drain the
   counts balanced. A UI that ignores these shows a spinner
   with no reason.
5. **URIs were well formed.** All returned `file://` URIs parsed, had no host, and
   preserved percent-encoding for a directory containing a space and `ü`
   (`file:///tmp/lsp-probe%20%C3%BCn%C3%AFcode/index.ts`). Windows drive letters
   are untested here (the design phase's task P0-5 owns that).
6. **`async-lsp` 0.2 is transport-agnostic, so non-stdio costs no extra protocol
   work** — its `MainLoop::run` takes any `AsyncBufRead`/`AsyncWrite`. What it
   costs is adapters and ownership, below. The design phase (P0) keeps the hand-rolled
   loop. The language-intelligence phase's task P7-2 can choose with data instead of a
   guess.

## Capabilities as advertised

| | rust-analyzer | typescript-language-server | pyright-langserver |
| --- | --- | --- | --- |
| `positionEncoding` | `"utf-8"` | absent → UTF-16 | absent → UTF-16 |
| `definitionProvider` | `true` | `true` | `{workDoneProgress: true}` |
| `diagnosticProvider` | `{identifier, interFileDependencies, workspaceDiagnostics: false}` | absent | absent |
| `textDocumentSync` | `{openClose: true, change: 2, save: {}}` | `2` (incremental) | `2` (incremental) |
| `completionProvider` | yes | yes | yes |
| `hoverProvider` | `true` | `true` | `{workDoneProgress: true}` |
| client requests seen | `window/workDoneProgress/create` ×14, `workspace/diagnostic/refresh` ×2 | `workspace/configuration`, `window/workDoneProgress/create` | `workspace/configuration` ×2 |

The `workspace/diagnostic/refresh` request is a signal to re-pull diagnostics.The probe answers it with `MethodNotFound`. A real client must implement it, or
treat diagnostics as stale.

## Definition at a non-ASCII column

Every fixture puts the call site after `"😀 日本語 e\u{301}";` on the same line, so
the requested column differs between UTF-8 and UTF-16 by six bytes. The
declaration name `日本語の関数` makes the returned end column differ too.

| server | requested position (encoding) | returned range | start byte | end byte | expected start |
| --- | --- | --- | --- | --- | --- |
| rust-analyzer | line 9, char 34 (UTF-8) | line 4, char 7→25 | 111 | 129 | 111 ✅ |
| typescript-language-server | line 5, char 37 (UTF-16) | line 0, char 16→22 | 16 | 34 | 16 ✅ |
| pyright-langserver | line 5, char 33 (UTF-16) | line 0, char 4→10 | 4 | 22 | 4 ✅ |
| typescript-language-server, path with space + `ü` | line 5, char 37 (UTF-16) | line 0, char 16→22 | 16 | 34 | 16 ✅ |

The end byte is the check that catches a client that decodes the *start* with one
width and the *end* with another. 22 UTF-16 units becomes 34 bytes, because the six
CJK characters take three bytes each. rust-analyzer's 25 UTF-8 bytes becomes 129.
`LineIndex::offset` accepted both, and the declaration byte ranges match the source
exactly.

## Diagnostics

| server | path | count | example |
| --- | --- | --- | --- |
| rust-analyzer | publish | 1 | unresolved import, line 2 char 4→30 → bytes 75→101 |
| rust-analyzer | pull (attempt 2) | 1 | `expected u32, found &'static str`, line 13 char 17→31 → byte 292 |
| typescript-language-server | publish | 1 | argument type error at line 5, char 44→55 → byte 183 |
| pyright-langserver | publish | 1 | `Literal['not a number']` not assignable to `int`, line 9 char 17→31 → byte 196 |

First publish latency after `didOpen`: 500 ms (rust-analyzer), 548 ms
(typescript-language-server), 267 ms (pyright). Initialize: 24 ms, 48 ms, 124 ms.
These are a tiny fixture on an already-warm cargo/npm cache. They are a floor, not
a budget.

`rust-analyzer`'s pull request was cancelled once with `-32802` and succeeded after
750 ms. That is a race with project loading, not a capability lie. The probe retries
only that code.

## `$/progress` during the first index

| server | total | begin | report | end | first progress after `didOpen` |
| --- | --- | --- | --- | --- | --- |
| rust-analyzer | 133 | 14 | 105 | 14 | 3 ms (12 ends at first publish) |
| typescript-language-server | 2 | 1 | 0 | 1 | 121 ms |
| pyright-langserver | 0 | — | — | — | — |

`rust-analyzer` begins more work than it ends before the first diagnostics arrive,
so the client must not treat "no matching end yet" as a bug. pyright reports work
through `window/logMessage`/`showMessage` instead.

## Conversion cost

`cargo run --release -p acpilot-lsp --example position_cost` on a 100k-line,
6,657,781-byte file (every tenth line contains emoji, CJK, and a combining mark):

| operation | UTF-8 | UTF-16 |
| --- | --- | --- |
| offset → position | 17.6 ns | 28.0 ns |
| position → offset | 10.0 ns | 16.1 ns |
| build `LineIndex` (whole file) | 2.26 ms | 2.26 ms |

2,000,020 conversions per encoding, 20 rounds. The per-conversion numbers are far
below a frame. The index build is the one that must not happen on the keystroke
path. D9 should rest on interop, not on this cost: negotiate both, take the
server's answer, and default to UTF-16.

## `async-lsp` 0.2

Checked against 0.2.4 from crates.io, in a scratch crate outside this repository:

- `MainLoop::run(input: impl AsyncBufRead, output: impl AsyncWrite)` and
  `run_buffered` are generic over the transport. Running over a PTY, socket, or
  in-memory duplex is the same call as stdio, with no extra protocol code. That is
  the thing task P0-7 wanted to know, and the answer is favourable.
- It is `futures`-io based, not tokio-io based. Wrapping `tokio::process::ChildStdout`
  needs `tokio_util::compat`, i.e. a dependency and two adapters that the
  hand-rolled loop does not need.
- It brings typed request/notification methods for the whole LSP surface
  (`lsp-types` 0.97), plus lifecycle, concurrency, panic, and tracing layers. That
  is real value: our probe client is ~400 lines and implements four methods.
- Cost measured: 97 packages in its normal closure versus 66 for `acpilot-lsp`
  (both with tokio's `full` feature), so about 25 extra crates. A cold debug build
  of an otherwise empty binary took 9.1 s. That is real weight for a client that
  needs five requests. It is not disqualifying once the language-intelligence
  phase (P7) needs the whole surface.
- The trade-off is ownership: `MainLoop` owns the read loop. Raw notifications —
  exactly what the probe needed to count — are only visible by implementing
  `LanguageClient` callbacks. A request that needs to race a notification needs
  the library's router rather than `select!`.

Recommendation for the language-intelligence phase's task P7-2: adopt `async-lsp` only if the language-intelligence phase (P7) client needs more than
a dozen typed requests. Otherwise keep `wire` + a loop shaped like `client.rs`,
which is directly testable against a mock server and already uses tokio. Do not
adopt it *because* of transport: both designs are transport-agnostic.

## Reproduction

```sh
cargo install --locked rust-analyzer   # or: rustup component add rust-analyzer
cargo run -p acpilot-lsp --example lsp_probe -- \
  --server rust-analyzer --root probes/fixtures/lsp/rust \
  --file src/lib.rs --needle '日本語の関数(41)' --expect '日本語の関数' \
  -- rust-analyzer

cargo run -p acpilot-lsp --example lsp_probe -- \
  --server typescript-language-server --root probes/fixtures/lsp/typescript \
  --file index.ts --needle '日本語の関数(41 + banner)' --expect '日本語の関数' \
  --init-options '{"tsserver":{"path":"<prefix>/node_modules/typescript/lib/tsserver.js"}}' \
  -- typescript-language-server --stdio

cargo run -p acpilot-lsp --example lsp_probe -- \
  --server pyright-langserver --root probes/fixtures/lsp/python \
  --file main.py --needle '日本語の関数(41)' --expect '日本語の関数' \
  -- pyright-langserver --stdio
```

`typescript-language-server` 6.0.1 fails `initialize` with "Could not find a valid
TypeScript installation" when the workspace has no `node_modules/typescript`. It
needs either a local install or `initializationOptions.tsserver.path`. The run
above used an existing TypeScript 6.0.3 installation by absolute path. A real
Acpilot install must either vendor/require a TypeScript version or surface that
error as a readable diagnostic instead of a failed start.

## Limitations

- One file per server, one definition, one diagnostic. This proves framing,
  negotiation, and column conversion. It does not measure a real project index.
- Linux only. Windows drive letters, UNC paths, and `file://` with a host are
  untested: that is the design phase's P0-5 territory. The URI parser here (`url`)
  is the one the language-intelligence phase (P7) should keep, not string-built
  paths.
- Timings are single runs on a warm machine, not a benchmark harness.
- `didChange` was not exercised: the probe only opens a document. Incremental sync
  (`change: 2` on all three) is the language-intelligence phase's (P7) problem.

## What a kept server costs (measured 2026-11-02)

Asked before the daemon spawned servers on first use, so the numbers are the ones
the startup path actually has.

**Launching spawns nothing.** The registry is empty until a request names a language. Measured to the first
successful connect, ten samples each:

| workspace | min | median | max | children at startup |
| --- | ---: | ---: | ---: | ---: |
| this repository | 5.63 ms | 6.21 ms | 8.88 ms | 0 |
| an empty fixture | 4.29 ms | 5.30 ms | 10.32 ms | 0 |

The earlier baseline was 4.6 ms, and both medians sit inside the spread of both
samples: linking `acpilot-lsp` into the daemon costs the startup path nothing.

**The first question pays for the index.** A fresh daemon, a five-line crate and
`rust-analyzer` on `PATH`. The first references request took 2,260 ms, the second
2.2 ms, the third 2.1 ms. That 2.26 s is spawn plus index, paid once per language per daemon. It is why
the session is kept at all. It is also why `IDLE_TIMEOUT` is five minutes rather
than seconds. Throwing the index away after a short pause would pay it again.

**A kept server outlives the daemon unless something stops it.** Measured after the
same run: one `rust-analyzer` still running once `acpilotd` had exited, holding its
index. The registry is an `Arc`. The terminal mirror and the settings watch each keep it
alive in a task that outlives `serve`, so dropping the router never reaches zero. `Servers::shutdown` is the fix and `pgrep -P <daemon>` is the check: empty after
`SIGTERM`, where before it named the server. Nothing about that is LSP-specific — it
is what "a value that owns a process" has to say on the way out.

## What the daemon does with this now (2026-11-02)

The publish path above is what `POST /v1/files/diagnostics` answers from.
`acpilot_lsp::Session::diagnostics` polls the transport for
`textDocument/publishDiagnostics` for the document it just opened. It returns what
arrived within ten seconds. A `didOpen`/`didChange` drops the previous version's
problems first. So the wait is for the answer about the text that was sent, not for
the answer already in hand. The pull path (`textDocument/diagnostic`) is not wired.
Finding 3 above is why it is a retry loop and not a call. One path is enough until a
server answers diagnostics only when pulled.

The answer carries the server's name or nothing at all, because "clean" and "nobody
checked" are different claims. The TUI draws problems as gutter letters (`E`, `W`,
`I`, `H`, worst on the line) in the cell the git mark uses. It also counts errors
and warnings in the status line, with `checking` beside them while a question is in
flight. The first question waits until a server and its index are ready, which
takes seconds. It asks when a file opens, when one is saved, and once the buffer
has been still for 480 ms. That pause is measured on the event loop's clock, so a
sentence of typing is one request, and every edit restarts the pause. The key that types never
sends anything, which is the rule the phase opens with. A question that nothing
answers ends the same way. The status line carries what went wrong, and the problems
already in hand stay, because an older answer is still the newest thing anybody
knows.

**The server also speaks when nobody asks.** From 2026-11-02 the daemon drains each
session's publish queue every 250 ms (`DIAGNOSTICS_POLL`). It announces what it finds
as a `diagnostics` frame on `GET /v1/events`. The TUI applies one to the open buffer
when the path matches. That is the case the ask-per-pause could not cover. A server
finishes a background check and publishes afterwards, while the pane waits for
nothing in particular. A gutter that only moved on the next save would be stale
exactly as long as the person reads instead of types. The drain is cheap because the
transport already reads, whether or not anybody is asking. Its pump fills a new
channel per notification whatever the request in flight. So a publish nobody asked
for is a message in a queue, not a lost event. A publish about a file outside
the workspace is dropped: there is no path a client could open.

**An answer is state as well as an event.** Both halves write the same store. One is
the publish the timer drained. The other is the answer that
`POST /v1/files/diagnostics` served.
So the newest problems per file survive the client that asked for them.
`GET /v1/events` sends that store as `diagnostics` frames on connect. So a reconnect
starts from what the server already said, not from an empty gutter. A reconnect is
the laptop that woke, or the second window opened a minute later. The store is capped at
`DIAGNOSTICS_SNAPSHOT` files (64), and it drops the file whose problems moved longest
ago. It is a map rather than a file on disk: problems are recomputed, so persisting
them would be persisting somebody's obsolete opinion. A file missing from it is not a
clean file. It is a file nobody has asked about since this daemon came up, and
opening it asks.
