# P0-5 — the WSL helper round trip, from Windows

Measured 2026-10-21 on Windows 11 (10.0.26300), `wsl.exe` 2.7.10.0, WSL2
kernel 6.18.33.2-microsoft-standard-WSL2, distro `Ubuntu-24.04`, Rust 1.97.0 on
both sides. The Windows half had never been measured: P0-2's PTY numbers are a
real Linux PTY, and calling `wsl.exe` from *inside* WSL is interop, not the path
the product ships.

| side | thing | version |
| --- | --- | --- |
| Windows | `C:\Windows\System32\wsl.exe` | 2.7.10.0 |
| Windows | host | Windows 11 10.0.26300 |
| Linux | distro | Ubuntu-24.04, kernel 6.18.33.2 |
| Linux | `acpilotd-helper` | this repository, built with `rustc -O` |

The instrument is `spikes/wsl-helper`: a Windows binary that enumerates distros,
spawns a helper inside one with `wsl.exe -d <d> --cd ~ -e <helper> --stdio`,
writes one JSON line, reads one back, and records the timings. The Linux end is
`spikes/wsl-helper/helper/acpilotd-helper.rs`, a dependency-free 150-line
program that answers one line per line and exits on EOF.

Both halves are tested on both operating systems ("Testing both sides" below):
`paths.rs` through cargo, the helper through `rustc --test` plus a stdlib-only
protocol checker, so nothing in this note rests on a single-OS run.

```powershell
# build and install the helper inside the distro (no cargo, no network)
cargo run --release -- install

# open a path that lives in WSL and run a session in that project directory
cargo run --release -- workspace --workspace '\\wsl.localhost\Ubuntu-24.04\home\sam\proj'

# the whole measurement; the JSON report stays in %TEMP%
cargo run --release -- bench --oracle --json "$env:TEMP\wsl-helper.json"

# the same, refusing to run on a WSL1 distro (what CI uses)
cargo run --release -- bench --require-wsl2 --oracle

# just the translation table, with wslpath as the oracle
cargo run --release -- paths --oracle
```

Note for this checkout: it lives on `//wsl.localhost/...`, where both toolchains
would share one `target/`, and a Windows build that follows a Linux one then
trips over files the Linux build owns. The checks therefore run with a
Windows-local target directory; `check.ps1` sets one, and an explicit
`CARGO_TARGET_DIR` always wins (see below).

## Verdict

1. **A helper is a session, not a call.** One spawn costs ~140 ms before the
   first byte comes back and ~197 ms including the helper's exit; after that a
   message costs **0.1 ms**. Spawning per prompt is a 1,400× tax on every turn.
2. **`-e`, never `--`.** `wsl.exe -- <program>` routes the command line through
   the user's shell: `wsl -d D -- wslpath -u 'C:\Users\SamTyler'` printed
   `wslpath: C:UsersSamTyler` because bash ate the backslashes, and a missing program
   exited **127**. `-e` (`--exec`) calls `execvpe` directly, keeps the
   backslashes, and reports a missing program as a relay error with exit **1**.
3. **`-e` does not expand `~`.** `-e ~/.local/bin/acpilotd-helper` dies with
   `execvpe(~/.local/bin/acpilotd-helper) failed: No such file or directory`;
   `--cd ~` *is* expanded by `wsl.exe`. The daemon must resolve `$HOME` once and
   pass an absolute helper path.
4. **`wsl.exe -l -q` is UTF-16LE with CRLF**, NUL between every ASCII byte
   (`55 00 62 00 …`). `--version` and `-l -v` are UTF-16LE too. A UTF-8 reader
   sees one long name with NULs in it.
5. **There are two path mappings, and the drive one is conditional.**
   `C:\x` ↔ `/mnt/c/x` only for drives that are *mounted* (`ls /mnt` returned
   `c` and `e` on this box); `/home/sam` ↔ `\\wsl.localhost\Ubuntu-24.04\home\sam`
   for everything else. `wslpath -w /mnt/d/x` produced a UNC path, because
   `/mnt/d` does not exist — a translation function that assumes A–Z is wrong.
6. **Pipes are the transport; ConPTY is not.** Under ConPTY the helper never
   started: `wsl.exe` wrote `ESC [ 6 n` (cursor position) and waited for an
   answer a bare ConPTY does not give. Once the probe answered DSR, the same
   command worked — with the request echoed back and CRLF line endings. A
   JSON-line parser on ConPTY must strip escape sequences, ignore the echo, and
   tolerate `\r\n`.
7. **Pass `--cd` even though `wsl.exe` guesses.** With no `--cd`, the helper
   inherited the host working directory translated into Linux
   (`/home/sam/workspace-other/acpilot/spikes/wsl-helper` from the UNC cwd,
   `/mnt/c/Users/SamTyler` from `C:\Users\SamTyler`). Deterministic startup is
   worth one flag.

## Which WSL

Every number here is WSL2: `wsl.exe -l -v` reports `VERSION 2` and the kernel
string ends `-microsoft-standard-WSL2`. That is not a footnote. WSL2 starts a
real kernel in a Hyper-V VM; WSL1 translates Linux syscalls onto NT ones.
Startup cost, `/mnt` and PTY behaviour all differ, so a WSL1 run would produce a
second, unrelated table rather than corroborate this one.

The bench reads the `VERSION` column, records it (`wsl_version` in the JSON) and
`--require-wsl2` turns "which product is this" into a hard failure:

```
wsl-helper: distro <name> is WSL1, not WSL2: WSL1 translates syscalls instead of
running a kernel, so … run this where a kernel VM can start
```

The parsing lives in `src/listing.rs` rather than `src/wsl.rs` on purpose:
`wsl.rs` is `cfg(windows)`, so its tests only run on Windows, and a bug that
decides which product was measured must not be able to hide behind that gate.
Six tests cover it on both OSes — UTF-16LE and BOM handling, the header row, a
name with spaces, rubbish rows, and both branches of the gate.

## Working in WSL from Windows

The case this has to support is the common one: **the code lives in WSL and the
person works in Windows.** The app is handed exactly one thing — the path the
user opened, usually `\\wsl.localhost\<distro>\...` from Explorer — and the
question is whether that path is enough to work in.

`wsl-helper workspace` answers it end to end: infer the distro from the UNC
path, map the path to Linux, start the helper **in that directory inside the
distro**, and assert the helper's cwd is the mapped path before measuring.

```bash
cargo run --release -- workspace \
  --workspace '\\wsl.localhost\Ubuntu-24.04\home\sam\workspace-other\acpilot' \
  --read README.md
```

```
opened      \\wsl.localhost\Ubuntu-24.04\home\sam\workspace-other\acpilot
host        wsl://Ubuntu-24.04
linux       /home/sam/workspace-other/acpilot
session     123.6 ms to start in that directory (+115.4 ms for the first message),
            then 0.1 ms per message (p50, n=20)
read cost   200× 3434 B: UNC 537.7 ms (2689 µs/read), in-distro 3.1 ms (16 µs/read), 173×
```

So the contract is:

- **The Windows path is a door, not an identity.** The workspace is
  `(distro, Linux path)`; `acpilot host add wsl --distro Ubuntu-24.04` creates
  the same host without any folder being opened.
- **Everything runs where the files are.** The helper is started with
  `--cd <linux path>` and is verified to be there; agent, LSP, git and PTY are
  all in-distro, and only JSON lines cross the boundary (0.1 ms each).
- **Do not read the tree over UNC.** The same file, 200 reads each way: 37,220 B
  costs **13.0 ms/read** through `\\wsl.localhost` and **0.096 ms/read** inside
  the distro — **136×** (a 3,434 B file: 2.7 ms vs 0.016 ms, 173×). The Windows
  side's `std::fs::read` loop is in-process; the distro side is a bash builtin
  that times itself with `date`, so neither number hides a spawn. Two runs
  agreed within 5% (135× and 136×). UNC is fine for a read-only browse and bad
  for an editor's hot path — README §5.6 asserted that, this measures it.
- **No WSL installed is not an error.** Detection is one `wsl.exe -l -q`, and a
  missing binary, a non-zero exit and an empty listing all collapse to "no
  hosts" — no prompt, no error, exit 0 (`src/discovery.rs`). Five tests on each
  OS drive that through real processes: a binary that does not exist, `exit 3`,
  a command that succeeds silently, and decode-then-filter. `ACPILOT_WSL_EXE`
  points the spike at any other program, so the absence path can be seen on a
  machine that *has* WSL — `ACPILOT_WSL_EXE=C:\Windows\System32\cmd.exe cargo run
  -- distros` prints the silence. `docker-desktop` and `docker-desktop-*` are
  filtered out: a real distro name, never a workspace.

What is left for the product is wiring, not protocol work: P3-8's
`POST /v1/hosts` takes `wsl://<distro>`, P3-9 ships the helper, and the UI's
"open folder" passes the UNC path through this same mapping.

## Latency

Release build, warm distro (already running; see Limitations), n as shown.
Milliseconds. Three runs agreed within ~10%; the table is one saved report
(`/tmp/wsl-helper.json`).

| what | n | min | p50 | p90 | max |
| --- | --- | --- | --- | --- | --- |
| `wsl.exe -l -q`, decoded (no distro started) | 5 | 74.4 | 76.6 | 82.9 | 82.9 |
| `wsl.exe -d D -e true` (call + exit) | 10 | 183.2 | 200.0 | 211.8 | 225.4 |
| helper one-shot, spawn → response | 10 | 135.4 | 138.8 | 143.4 | 148.4 |
| helper one-shot, spawn → response → EOF exit | 10 | 188.4 | 197.2 | 210.5 | 210.7 |
| helper message on a warm session | 30 | 0.077 | 0.107 | 0.142 | 0.193 |
| 1 MiB request line on the warm session | 1 | — | 1.34 | — | — |

A second session on the same box, run from a plain `C:\Users\SamTyler` cwd
instead of the UNC checkout, reproduced the shape: list 66.3/76.2, one call
166.3/182.6, one-shot 127.8/134.3, with exit 172.6/191.3 ms (p50/p90), warm
message 0.1 ms, 1 MiB line 1.5 ms intact. Differences from the table above are
within ±15% and do not change any ordering or conclusion.

* `Command::spawn` itself is ~6 ms; the other ~133 ms is the WSL relay starting
  the distro-side process. That is the number a session pays once, not the
  helper's own startup.
* A one-shot *helper* response (139 ms) beats a one-shot `-e true` (200 ms) only
  because the latter is timed through process exit; with exit included the two
  agree at ~197–200 ms. The clock is dominated by `wsl.exe`, not by what runs.
* The 1 MiB line arrived intact in 1.3 ms on the warm session — the pipe is not
  the bottleneck, and there is no 64 KiB framing limit to design around. The
  helper's `BufRead::lines()` handled it without a custom codec.
* Truncated answers would matter more than averages: none were seen. The largest
  per-message time on a warm session was 0.19 ms.

## Path translation

`spikes/wsl-helper/src/paths.rs` is the liftable artifact: pure functions, 12
unit tests, no Windows APIs. `WslPathMap::new(distro, mounted_drives)` then
`to_linux` / `to_windows`.

Measured against `wslpath` (`wsl -d D -e wslpath -u|-w <path>`) on this box:

| input | ours | `wslpath` |
| --- | --- | --- |
| `C:\Users\SamTyler` | `/mnt/c/Users/SamTyler` | same |
| `C:/Users/SamTyler` | `/mnt/c/Users/SamTyler` | same |
| `c:\users` | `/mnt/c/users` | same |
| `C:\` | `/mnt/c` | `/mnt/c/` |
| `C:\a b\ü\文件` | `/mnt/c/a b/ü/文件` | same |
| `\\wsl.localhost\Ubuntu-24.04\home\sam` | `/home/sam` | same |
| `\\wsl$\Ubuntu-24.04\home\sam` | `/home/sam` | same |
| `\\wsl.localhost\Debian\home\sam` | refused: *path belongs to distro Debian, not Ubuntu-24.04* | `wslpath` exits non-zero |
| `D:\data` | refused: *drive d: is not mounted in WSL (mounted: c:, e:)* | `wslpath` exits non-zero |
| `relative\x` | `relative/x` | same |
| `C:\Users\SamTyler\..\Public` | `/mnt/c/Users/SamTyler/../Public` | `/mnt/c/Users/Public` |
| `/home/sam` | `\\wsl.localhost\Ubuntu-24.04\home\sam` | same |
| `/` | `\\wsl.localhost\Ubuntu-24.04\` | same |
| `/mnt/c` | `C:\` | same |
| `/mnt/d/a b/x` | `\\wsl.localhost\Ubuntu-24.04\mnt\d\a b\x` | same |
| `/tmp/a b/ü/文件` | `\\wsl.localhost\Ubuntu-24.04\tmp\a b\ü\文件` | same |
| `relative/x` | `relative\x` | same |

Three differences are deliberate:

1. `C:\` normalises to `/mnt/c` without the trailing slash; every consumer joins
   with `/` anyway, and the slash makes `Path` comparisons noisy.
2. Dot segments pass through. `wslpath` resolves `..`; a translator that silently
   rewrites paths can change meaning across a symlink, and path *policy* belongs
   to the layer that knows the workspace (P3-5). Callers that need a canonical
   path must resolve it where the policy lives.
3. An unmounted drive or a foreign distro is a **typed refusal**, not a best
   guess. `wslpath` also refuses (non-zero exit, message on stderr); a daemon
   that turned that into a UNC path would hand the agent a file it cannot read.

The mapping is exactly two rules, and both are needed:

```
C:\Users\SamTyler        ↔ /mnt/c/Users/SamTyler        (mounted drives only)
\\wsl.localhost\<distro>\home\sam ↔ /home/sam           (distro root, any path)
```

`mounted_drives` comes from `wsl -e ls -1 /mnt` (single letters only), not from a
hard-coded alphabet: `D:` exists in Windows and has no Linux path, and `E:` was
mounted here even though it is easy to assume only `C:` is.

## Testing both sides

The split is what is *exercised* where, not what compiles: `wsl.rs` is
`#[cfg(windows)]`, so the crate builds on Linux and the liftable half is tested
by the same OS the daemon will run on.

| check | Windows | inside Ubuntu-24.04 |
| --- | --- | --- |
| `cargo fmt --check` | passes | passes |
| `cargo clippy --all-targets -- -D warnings` | passes | passes |
| `cargo test` (`paths.rs`, `listing.rs`, `discovery.rs`) | 23 passed | 23 passed |
| `rustc --test helper/acpilotd-helper.rs` | 4 passed | 4 passed |
| `helper/check_protocol.py` against a native helper | passes | passes |
| `wsl-helper bench` end to end | passes, `wsl WSL2` | refuses, exit 1 |

```powershell
# on Windows, from the repo root: every local check, no Unix shell involved
pwsh -File spikes/wsl-helper/check.ps1

# both halves in one run; the distro's own shell runs its three checks
pwsh -File spikes/wsl-helper/check.ps1 -Wsl -WslDistro Ubuntu-24.04

# from cmd.exe or Explorer instead of PowerShell
spikes\wsl-helper\check.cmd -Wsl -WslDistro Ubuntu-24.04
```

```bash
# inside the distro, in the distro's shell — not an editor's bundled one
cd ~/workspace-other/acpilot/spikes/wsl-helper
cargo test && cargo clippy --all-targets -- -D warnings
rustc --test helper/acpilotd-helper.rs -o /tmp/helper-test && /tmp/helper-test
python3 helper/check_protocol.py ~/.local/bin/acpilotd-helper
```

The Windows half deliberately does not go through bash. A bash that arrives
with an editor is not part of the toolchain, Windows may block it as a security
risk, and a check that only works under one app's shell is not a check — so the
checks spawn `cargo`, `rustfmt`, `rustc` and `python` directly. They live in
`cargo xtask spike` and nowhere else: `check.ps1` and `check.cmd` are thin
wrappers that call that command (plus, with `-Wsl`, the same command inside the
distro), and CI calls it too.

CI runs that table on every push in `.github/workflows/ci.yml`, job `p0-5 spike`
on `ubuntu-latest` + `windows-latest` — by calling `cargo xtask spike`, so the
steps CI trusts are the ones a developer runs rather than a second transcription
of them. The Windows job therefore never invokes bash. The Linux-only platform
guard (the binary's own exit codes 1 and 2) stays a workflow step: it asserts
the program's behaviour, not a command line.

The end-to-end bench is a second, **opt-in** job (`wsl-bench`) instead of a
third row in that matrix, because it needs the one thing no hosted runner has.
WSL2 requires nested virtualisation, and standard `windows-latest` runners do
not get it ([actions/runner-images#5920](https://github.com/actions/runner-images/issues/5920));
they can only offer WSL1, which `--require-wsl2` refuses rather than presenting
WSL1 timings as the shipped path. Turn the job on against a Windows machine you
control, registered as a self-hosted runner with the `wsl2` label and a distro
that has `rustc` (the job installs the helper with the spike's own `install`):

```bash
gh variable set WSL_BENCH  --body true
gh variable set WSL_RUNNER --body '["self-hosted","windows","wsl2"]'
```

It runs `bench --require-wsl2 --oracle --json $RUNNER_TEMP/wsl-helper.json` and
uploads that JSON as the `wsl-helper-bench` artifact — the file this note's
tables are regenerated from, with `wsl_version` in it saying which product it
measured. Both runner commands were executed as written (under PowerShell,
against this WSL2 box): exit 0, `wsl_version: 2`, 20 `wslpath` oracle rows.
Without the variables the job is skipped before a runner is even chosen, so the
repository does not carry a permanently red check.

The Linux build of `wsl-helper` printing *this spike drives wsl.exe and only runs
on Windows* and exiting 1 is the guard working, not a failure; `--help` exits 2.
The helper's unit tests run on either OS with `rustc --test`, which needs no
cargo and no harness dependency — the same property that lets the daemon install
the helper with a bare `rustc`.

One wrinkle this checkout has: when the repository is reached over
`\\wsl.localhost\...`, both toolchains share one `target/`. Files the Linux build
leaves behind are owned by the WSL uid, and the next Windows build then fails
with `Access is denied. (os error 5)` while replacing a
`build_script_build-*.exe`. The Windows half therefore needs a Windows-local
target directory:

```powershell
$env:CARGO_TARGET_DIR='C:/Users/SamTyler/AppData/Local/Temp/wsl-helper-target'
cargo test --manifest-path '\\wsl.localhost\Ubuntu-24.04\home\sam\workspace-other\acpilot\spikes\wsl-helper\Cargo.toml'
```

That is a property of editing a WSL-hosted checkout from Windows, not of the
product, which builds from a normal Windows path.

## Interop quirks, in the order they bite

| quirk | what was observed |
| --- | --- |
| `-l -q` encoding | UTF-16LE + CRLF; NUL-interleaved. `-l -v` and `--version` too. |
| `--` vs `-e` | `--` goes through the user's shell (bash here), eats `\`, exits 127 on a missing program. `-e` execs directly, keeps `\`, exits 1 via relay error. |
| `~` in the program path | not expanded by `-e`; `--cd ~` is. |
| failed exec message | `<3>WSL (4062 - Relay) ERROR: CreateProcessCommon:818: execvpe(...) failed: No such file or directory` on stderr. |
| exit codes | `7` → 7, `3` → 3 through `-e`; a missing binary is 1, not 127. |
| stdout/stderr | separate and clean; stderr carries only the program's own output and relay errors. |
| stdin | byte-exact and streamed per line; 1 MiB intact; no extra newline. |
| cwd | translated from the host automatically (UNC → the Linux path, `C:\x` → `/mnt/c/x`). Pass `--cd` for determinism. |
| default distro | `printenv WSL_DISTRO_NAME` via a bare `wsl.exe` is the reliable answer; `-l -q` also lists `docker-desktop`, which is not a general-purpose distro. |
| x64 toolchain | `rustc` in the distro built the helper with no network: `rustc -O /tmp/acpilotd_helper_build.rs -o ~/.local/bin/acpilotd-helper`. The name matters — `.acpilotd-helper.rs` is an invalid crate name. |
| Git Bash (dev loop only) | absolute Linux arguments are mangled (`/home/sam` becomes `C:/Program Files/Git/home/sam`); `MSYS_NO_PATHCONV=1` fixes it. The product never goes through bash. |

## ConPTY

The first ConPTY run produced **4 bytes in 10 s**: `\e[6n`. `wsl.exe` asks for the
cursor position before it starts the payload, and ConPTY does not answer. With a
`\e[1;1R` reply, the same command completed in 145–165 ms and returned 246 bytes:

```
\e[6n                         ← the query, before anything else
\e[?9001h\e[?1004h\e[m       ← Win32 input mode / focus reporting / SGR reset
\e]0;C:\WINDOWS\System32\wsl.exe\x07   ← OSC window title
\e[?25h                      ← cursor show
{"jsonrpc":"2.0","id":7,"method":"ping",...}\r\n   ← the request, echoed
{"jsonrpc":"2.0","id":7,"result":{...}}\r\n          ← the response
```

So a ConPTY channel is usable only with a terminal-query responder, and it
doubles every request (echo) and changes the newline (`\r\n`). That is exactly
the P0-2 problem in a different place. For the helper's JSON channel, use pipes;
keep ConPTY for `terminal/create` (P2-5), where a responder is mandatory anyway.

## Consequences for `acpilot-host`

- One helper child per host, started lazily but **before the first prompt**, and
  multiplexed by JSON-RPC id. 140 ms at boot is fine; 140 ms per prompt is not.
- The spawn line is
  `wsl.exe -d <distro> --cd ~ -e <absolute helper path> --stdio`, with
  `CREATE_NO_WINDOW`, stdin/stdout piped, stderr captured into the session log.
- Resolve the helper path from `$HOME` once (or install it to a fixed absolute
  path and verify it with `--version` before use); never pass `~` to `-e`.
- Enumerate with `-l -q` and decode UTF-16LE; drop `docker-desktop`; name the
  distro explicitly on every call instead of relying on the default.
- Lift `WslPathMap` from `paths.rs`. Populate `mounted_drives` from `ls /mnt`
  once per connection, refuse unmounted drives, and keep `.`/`..` resolution out
  of the translator.
- Log the `wsl.exe` version (`2.7.10.0`) and the distro with the session: `--cd`
  and `-e` behaviour is version-dependent, and a mixed-version fleet will
  otherwise produce unreproducible path bugs.
- Keep a supervisor for the helper: exit code 1 with a relay error means the
  distro is gone, not that the request failed. `never|on-crash` restart is P2-2's
  policy, but the distinction has to reach it.

## Limitations

- One box, one distro, one `wsl.exe` version; the distro was already running, so
  **cold start after a reboot is not measured**. Terminating it here would have
  killed the checkout's own processes (this repository lives inside that distro);
  a CI runner or a dedicated box can measure it with `wsl --terminate`.
- The helper is a stand-in: it echoes the id and reports `pid`/`cwd`/`user`. No
  real JSON-RPC dispatch, no concurrency (one pipe, serialised), no
  cancellation mid-request, no auth.
- Only the standard drive and UNC mappings are covered: no `/etc/wsl.conf`
  custom mount roots, no symlinked drives, no `\\wsl.localhost` path for another
  Windows user, no non-UTF-8 paths.
- The ConPTY probe answers DSR and DA1 only; a program that asks something else
  would still stall, which is the argument for pipes.
- Absolute timings are one machine's; the ratio (≈140 ms once vs ≈0.1 ms per
  message) is the portable result.
