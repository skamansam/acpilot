# P0-2 — a program as an agent: three TUIs under one PTY driver

Measured 2026-10-21 on Linux (x86_64), Rust 1.97.0.

| program | version | invocation |
| --- | --- | --- |
| Freebuff | npm wrapper 0.0.106 (the native TUI it downloads prints no version) | `freebuff` |
| Claude Code | 2.1.306 | `claude` |
| `fake-tui.sh` | this repository | `bash spikes/fixtures/tui/fake-tui.sh` |

The instrument is `spikes/pty-agent`: a standalone crate that opens a PTY with
`portable-pty` 0.9, feeds every byte into a `vt100` 0.18 screen, sends a prompt,
and records the screen at every candidate turn boundary. It writes a JSON report
per run; the reports stay in `/tmp` and only the numbers below are committed.

```bash
# startup: what the program needs before it will take a prompt
cargo run -q -- --cmd freebuff --cwd ~/workspace-other/acpilot --ready 2500 --observe 15000

# one turn, bracketed paste, Enter in its own write
cargo run -q -- --cmd freebuff --cwd ~/workspace-other/acpilot \
  --prompt 'Reply with exactly the word OK and nothing else.' --paste --quit 'text:/exit' \
  --json /tmp/freebuff.json

cargo run -q -- --cmd claude --cwd ~/workspace-other/acpilot \
  --prompt 'Reply with exactly the word OK and nothing else.' --quit 'text:/exit' \
  --json /tmp/claude.json

bash spikes/fixtures/tui/fake-tui.sh   # the deterministic double
```

## Verdict

1. **The transport is generic; the semantics are not.** One `PtyAgent` — spawn,
   render, write, snapshot — drove all three programs unchanged. What differed
   per program was every question that matters for an adapter: which key submits,
   whether the text and the Enter may share a write, where the answer is on the
   screen, what a finished turn looks like, and which startup dialog has to be
   answered first. `transport = "pty"` must therefore carry a **profile**, not a
   parser guess.
2. **An idle timeout is a lie at every threshold below 5 s.** The program's own
   screen says when it finished (`Churned for 2s · done 2:46 PM` in Claude,
   `• 3s` in Freebuff). At 300 ms every run returned a truncated answer; at
   2,000 ms four of the five real turns did. Only 5 s never lied — and it adds
   5 s of latency to every turn, which for a chat UI is the difference between
   "instant" and "broken".
3. **The turn boundary has to come from the screen, and the screen has to be
   captured before the program exits.** Both real programs run on the alternate
   screen (`CSI ? 1049 h`): after `quit`, the captured screen is **empty** in
   every run. A driver that exits the child and then reads the terminal has
   nothing to read.
4. **A bare PTY is not a terminal.** Both programs probe the terminal before
   they settle: `CSI ? u` (kitty keyboard), `CSI > 0 q` (XTVERSION), and — Freebuff
   — `CSI 6 n` (cursor position) three times, Claude — `CSI c` (DA1). Nothing
   answers a bare PTY, and a driver that does not answer is relying on the
   program's fallback path rather than on the path a user gets.
5. **Sending `text` and `\r` in one write does not submit.** Four runs left the
   prompt sitting in the box with no turn started — and the idle detector then
   declared that non-turn complete. Splitting the write (text, ~250 ms, Enter)
   worked every time. This is the single most expensive line in the spike.
6. **The answer is not the screen.** Freebuff renders rotating ads, a spinner,
   and the model's `• Thinking` text in the same region as the answer; Claude
   prefixes the answer with `●` and puts a status bar under it. Screen text is
   the raw material, not the transcript: extracting messages needs the program's
   own markers.

## Startup: what each program demands

| | Freebuff | Claude Code |
| --- | --- | --- |
| quiet before ready | 4.5–8.2 s (ads animate) | 3.9–10.0 s ("Auto-updating…") |
| startup bytes / SGR | 31–33 KB / 855–926 | 2.5 KB / 51, plus a 1.3 KB dialog |
| alternate screen | yes, from the first frame | no for the trust dialog, yes for the TUI |
| bracketed paste | enabled | enabled |
| startup gate | none at the git root; a "you started this in a subdirectory" modal elsewhere | **"Is this a project you created or one you trust?"** on first run in a directory |
| gates answered by | `--pre` (Enter) | `--pre` (Enter) |
| submits on | `\r`, in a separate write | `\r`, in a separate write |
| leaves on | `/exit` → code 0 | `/exit` → code 0 |
| on Ctrl-D | stays alive | stays alive |

Neither program takes a prompt before its gate is answered, and Claude's gate is
drawn **outside** the alternate screen — so a driver that only captures the
alternate screen never sees the dialog that is blocking it. Both `Ctrl-D` paths
did nothing within 5 s; `/exit` exited cleanly with code 0 in both.

## The turn boundary, measured

Each run evaluates every threshold during the same turn and keeps the screen it
would have returned. "Matches" means the screen at that threshold is byte-for-byte
the screen at the 5 s truth-teller; "cut" means the answer was still arriving.

| run | first byte | largest gap | real turn (screen) | 300 | 500 | 800 | 1,200 | 2,000 | 5,000 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `fake-tui.sh` | 0 ms | 901 ms | 1.45 s | cut | cut | cut | ok | ok | ok |
| Freebuff, paste + split Enter | 1 ms | 2,380 ms | 3 s | cut | cut | cut | cut | cut | ok |
| Claude Code | 20 ms | 2,914 ms | 2 s | cut | cut | cut | cut | cut | ok |
| Claude Code (second turn) | 20 ms | 3,364 ms | 2 s | cut | cut | cut | cut | cut | ok |

The gap that kills the short thresholds is the model thinking, not the network:
2.4 s and 2.9–3.4 s of silence in the middle of a live turn, after the first
token had already arrived. A 2 s timeout is not "a bit eager" — it truncated
every real turn in this sample.

The cost is visible in the detector latency: Claude answered in 2 s and the
driver declared the turn over at **10.9 s**; Freebuff answered in 3 s and the
driver took **13.3 s**. Five seconds of that is the truth-teller, the rest is the
program's own trailing repaints. A per-program sentinel — an empty `❯` row in
Claude, an empty input box in Freebuff — is the difference between 2 s and 200 ms.

## What actually breaks

**Alternate screen.** The transcript is not in scrollback and not in the
terminal after exit: `final_screen` was the empty string in every run, for all
three programs. Capture at the sentinel, never after `quit`.

**Colours.** `contents()` throws away 855–926 SGR sequences' worth of meaning in
Freebuff's startup alone. Usually that is fine; here it is not, because the one
distinction the consumer needs — the model's *thinking* versus its *answer* — is
carried by a glyph and a colour in Freebuff (`• Thinking` followed by the plain
answer) and by `●` in Claude. Text-only scraping merges them.

**Bracketed paste.** The markers arrive *inside* the input line
(`^[[200~write a long answer^[[201~` at the reference program, which is what a
program that has not implemented paste handling sees). Freebuff strips them and
shows a clean `Chat: …` line; the reference double does not, which is exactly why
the double is useful. Paste is still required: a multi-line prompt sent as raw
keystrokes submits on the first newline.

**Same-write Enter.** The failure is silent and it looks like a completed turn:

```text
╭──────────────────────────────────────────────────────────────────────╮
│  Reply with exactly the word OK and nothing else. Do not use any tools.▍
╰──────────────────────────────────────────────────────────────────────╯
```

That screen is the 5 s truth-teller's answer for a turn that never started. The
text arrived, the Enter was consumed with it, and nothing else happened. Two
writes fixed it in every subsequent run.

**Terminal queries.** `CSI ? u`, `CSI > 0 q`, `CSI 6 n`, `CSI c` are asked in the
first second. The driver answers them (DA1, DA2, XTVERSION, `?0u` for kitty,
cursor position, text-area size). Answering is not optional in principle — a
terminal always answers — though these two programs happen to keep working
without it.

**Startup gates.** Claude's trust dialog and Freebuff's "subdirectory of a git
repo" modal are the first screen either program shows, and neither is the input
box. A PTY adapter needs a per-program "answer the gates" step before it can send
anything, and it needs to detect that a gate is showing rather than guess from
the clock: Claude's dialog appears *after* a loading spinner has already been
quiet for 1.2 s.

## What the answer looks like

Freebuff, end of turn, exactly as the screen returns it:

```text
   [02:48 PM]
   Reply with exactly the word OK and nothing else. ⎘
  • Thinking
    The user wants a reply of exactly the word OK and nothing else.
  OK
                                                     ⎘ • 3s • △▽
┌────────────────────────────────────────────────────┐
│ Archil                                    SPONSORED │
```

Claude, same turn:

```text
● OK

✻ Churned for 2s · done 2:46 PM

──────────────
❯
──────────────
  acpilot | Opus 5.5 | 5% of 1M
  ⏵⏵ auto mode on (shift+tab to cycle)
```

Both contain the answer, the timing, and the sentinel in a fixed place — and
both need the program's own grammar to separate them from ads, spinners, the
status bar, and the thought. That is a profile, not a heuristic.

## Consequences for P2-7

- `transport = "pty"` ships **profiles**, one per program: submit key + write
  split, startup gates, input row, message markers, completion sentinel, exit
  command. The two shipped parsers (`plain-transcript`, `ansi-transcript`) are the
  fallback for programs without a profile, not the primary mechanism.
- Completion = the program's sentinel, with a 5 s idle **backstop**. The backstop
  is what catches a sentinel that changed in a new version; the sentinel is what
  keeps the turn fast.
- The adapter returns the screen **at the sentinel**, and the `Turn` record keeps
  the raw screen next to the extracted messages, so a profile regression is a
  diff and not a mystery.
- The driver owns terminal-query answering, the two-write submit, and paste
  framing; no profile should have to repeat that.
- `thought` is a separate event from `message` — the screen makes the distinction
  cheap to keep and expensive to recover later.

## Limitations

- Two programs, one platform, one turn each. Windows and WSL are P0-5; no resize,
  cancellation, tool-permission prompt, or multi-turn context was exercised.
- The spike returns screen text; it does **not** extract messages. That is P2-7's
  job, and it is deliberately left undone here.
- Query replies are sent once per chunk rather than once per occurrence, so
  Freebuff's three `CSI 6 n` probes in one chunk get one answer. Harmless here,
  wrong in principle.
- The Freebuff runs used its free tier and consumed tokens. Its native binary
  does not print a version, so the wrapper's `0.0.106` is the only number to pin.
