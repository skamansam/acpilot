# P0-7 — language servers: what three real servers do

Measured 2026-10-21 on Linux under WSL2 (kernel 6.18.33.2-microsoft-standard-WSL2,
x86_64), Rust 1.97.0. Fixtures were opened from the distro's own filesystem
(`/home`), not from `/mnt/c`, so the index timings are not Windows-drive timings.
Servers:

| server | version | invocation |
| --- | --- | --- |
| `rust-analyzer` | 1.97.0 (2d8144b 2026-07-07) | `rust-analyzer` |
| `typescript-language-server` | 6.0.1, TypeScript 6.0.3 | `typescript-language-server --stdio` |
| `pyright-langserver` | 1.1.414 | `pyright-langserver --stdio` |

The instrument is `crates/acpilot-lsp/examples/lsp_probe.rs`: it frames stdio itself
([`wire`](../../crates/acpilot-lsp/src/wire.rs)), negotiates
`general.positionEncodings = ["utf-8", "utf-16"]`, opens one file, waits for
diagnostics, asks for a definition, and converts every returned range back to a
byte offset. With `--expect`, the definition must start on that exact substring or
the probe exits non-zero. Fixtures are in `probes/fixtures/lsp/{rust,typescript,python}`.

## Verdict

1. **Assume nothing, negotiate, and keep the conversion.** `rust-analyzer` chose
   `utf-8`; both `typescript-language-server` and `pyright-langserver` advertise no
   `positionEncoding` at all, which means UTF-16. A client that assumes UTF-8 gets
   the wrong column from two of the three as soon as a line contains an emoji; a
   client that assumes UTF-16 leaves rust-analyzer's offer unused. Both behaviours
   were observed, and both converted correctly.
2. **The conversion is not the cost.** On a 100k-line, 6.7 MB file, one
   offset→position conversion is 17.6 ns in UTF-8 and 28.0 ns in UTF-16; the
   reverse is 10.0 ns and 16.1 ns. A keystroke will never notice. Rebuilding the
   line index costs 2.26 ms, which *is* relevant: P7 must make the index
   incremental, not re-run it per edit.
3. **Publish diagnostics are universal; pull diagnostics are not.**
   `rust-analyzer` advertises `diagnosticProvider` and answers
   `textDocument/diagnostic`, but its first answer was `-32802` (RequestCancelled)
   and the second succeeded. The other two only publish. A client needs the
   publish path regardless, and a pull path that retries cancellation.
4. **`$/progress` is not optional UX.** During the first index,
   `rust-analyzer` sent 133 `$/progress` notifications (14 begin, 105 report, 14
   end). At the moment the first diagnostics arrived it had begun 14 and ended
   12, so an unmatched begin is normal while work is still running; after a
   500 ms drain the counts balanced. A UI that ignores these shows a spinner
   with no reason.
5. **URIs were well formed.** All returned `file://` URIs parsed, had no host, and
   preserved percent-encoding for a directory containing a space and `ü`
   (`file:///tmp/lsp-probe%20%C3%BCn%C3%AFcode/index.ts`). Windows drive letters
   are untested here (P0-5 owns that).
6. **`async-lsp` 0.2 is transport-agnostic, so non-stdio costs no extra protocol
   work** — its `MainLoop::run` takes any `AsyncBufRead`/`AsyncWrite`. What it
   costs is adapters and ownership, below. P0 keeps the hand-rolled loop; P7-2 can
   choose with data instead of a guess.

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

The `workspace/diagnostic/refresh` request is a signal to re-pull diagnostics. The
probe answers it with `MethodNotFound`; a real client must implement it or treat
diagnostics as stale.

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

The end byte is the check that catches a client that decoded the *start* correctly
and the *end* with the wrong width: 22 UTF-16 units becomes 34 bytes because the
six CJK characters take three bytes each; rust-analyzer's 25 UTF-8 bytes becomes
129. Both were accepted by `LineIndex::offset`, and the declaration byte ranges
match the source exactly.

## Diagnostics

| server | path | count | example |
| --- | --- | --- | --- |
| rust-analyzer | publish | 1 | unresolved import, line 2 char 4→30 → bytes 75→101 |
| rust-analyzer | pull (attempt 2) | 1 | `expected u32, found &'static str`, line 13 char 17→31 → byte 292 |
| typescript-language-server | publish | 1 | argument type error at line 5, char 44→55 → byte 183 |
| pyright-langserver | publish | 1 | `Literal['not a number']` not assignable to `int`, line 9 char 17→31 → byte 196 |

First publish latency after `didOpen`: 500 ms (rust-analyzer), 548 ms
(typescript-language-server), 267 ms (pyright). Initialize: 24 ms, 48 ms, 124 ms.
These are a tiny fixture on an already-warm cargo/npm cache; they are a floor, not
a budget.

`rust-analyzer`'s pull request was cancelled once with `-32802` and succeeded after
750 ms; that is a race with project loading, not a capability lie. The probe retries
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
below a frame; the index build is the one that must not happen on the keystroke
path. D9 should be decided on correctness and interop, not this cost: negotiate
both, take the server's answer, default to UTF-16.

## `async-lsp` 0.2

Checked against 0.2.4 from crates.io, in a scratch crate outside this repository:

- `MainLoop::run(input: impl AsyncBufRead, output: impl AsyncWrite)` and
  `run_buffered` are generic over the transport. Running over a PTY, socket, or
  in-memory duplex is the same call as stdio — no extra protocol code. This is the
  thing P0-7 wanted to know, and the answer is favourable.
- It is `futures`-io based, not tokio-io based. Wrapping `tokio::process::ChildStdout`
  needs `tokio_util::compat`, i.e. a dependency and two adapters that the
  hand-rolled loop does not need.
- It brings typed request/notification methods for the whole LSP surface
  (`lsp-types` 0.97), plus lifecycle, concurrency, panic, and tracing layers. That
  is real value: our probe client is ~400 lines and implements four methods.
- Cost measured: 97 packages in its normal closure versus 66 for
  `acpilot-lsp` (both with tokio's `full` feature), so about 25 extra crates,
  and 9.1 s for a cold debug build of an otherwise empty binary. That is real
  weight for a client that currently needs five requests, but it is not
  disqualifying once P7 needs the whole surface.
- The trade-off is ownership: `MainLoop` owns the read loop. Raw notifications —
  exactly what the probe needed to count — are only visible by implementing
  `LanguageClient` callbacks, and a request that needs to race a notification
  needs the library's router rather than `select!`.

Recommendation for P7-2: adopt `async-lsp` only if the P7 client needs more than
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
TypeScript installation" when the workspace has no `node_modules/typescript`; it
needs either a local install or `initializationOptions.tsserver.path`. The run
above used an existing TypeScript 6.0.3 installation by absolute path. A real
Acpilot install must either vendor/require a TypeScript version or surface that
error as a readable diagnostic instead of a failed start.

## Limitations

- One file per server, one definition, one diagnostic. This proves framing,
  negotiation, and column conversion; it does not measure a real project index.
- Linux only. Windows drive letters, UNC paths, and `file://` with a host are
  untested — that is P0-5's territory, and the URI parser here (`url`) is the one
  P7 should keep using rather than string-building paths.
- Timings are single runs on a warm machine, not a benchmark harness.
- `didChange` was not exercised; the probe only opens a document. Incremental sync
  (`change: 2` on all three) is P7's problem.
