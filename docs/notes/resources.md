# Every program's CPU and memory

Cross-cutting task **XT-4**. `cargo xtask resources` writes this note. It starts a
binary the archives carry against a temporary profile. It drives one scripted load.
It samples the whole process tree every 100 ms. It reports peak RSS, mean and peak
CPU and the thread count. Each row is measured three times and the median is
reported here. The window each figure was taken in is in the table, because a
memory number without one measures the absence of a load.

Measured on this machine: **Linux 24.04 Ubuntu**, kernel `6.18.54.1-microsoft-standard-WSL2`, `x86_64` target, 24 logical
CPU(s) — Intel(R) Core(TM) Ultra 9 275HX — and 31809.0 MiB of memory. The build is the **release** profile.

CPU is a fraction of **one core**, summed over the tree: a percentage of the
machine would hide the second core a busy loop costs. A `—` means the row ended
before a second sample could take a delta, so memory is the only figure it has.

| program | window | peak RSS | mean CPU | peak CPU | threads |
| --- | --- | --- | --- | --- | --- |
| `acpilotd` | idle: one `/v1/events` subscriber open for the window (drawing and waiting) | 18.6 MiB | 0.00 % of one core | 0.00 % of one core | 28 |
| `acpilotd + 1 session` | one idle session: created over the API, its mock agent running. The daemon's own process | 22.0 MiB | 0.00 % of one core | 0.00 % of one core | 32 |
| `acpilotd + 1 turn` | one prompt turn: the prompt sent over the API, the fixture agent's events recorded and streamed to one subscriber. The daemon's own process, with the rest of the window idle | 22.3 MiB | 0.00 % of one core | 0.00 % of one core | 32 |
| `acpilot serve` | one page connected: the page fetched, one `/v1/events` subscriber open for the window | 19.3 MiB | 0.00 % of one core | 0.00 % of one core | 28 |
| `acpilot tui` | one design pass: `acpilot tui --demo` at its frame clock under a 120x40 pseudo-terminal. No daemon and no session traffic | 14.6 MiB | 0.19 % of one core | 4.72 % of one core | 1 |
| `acpilot doctor` | one-shot: reads PATH, the profile and the workspace. Starts no daemon | 10.8 MiB | — | — | 1 |

One idle session adds **3.4 MiB** to the daemon's own process. The session row's
peak less the idle row's. Against the table's 15 MB budget, which has no
margin because it is a stated ceiling rather than a figure this harness
discovered.

## Budgets

The first run set the budgets below, and CI holds them. `cargo xtask resources
--gate` repeats each row three times and fails when a median is past its budget
plus the margin. A figure is written as a budget and a percentage, not as one
sample. The same program on another machine is not the same number. A gate that
trips on a scheduling hitch is a gate somebody turns off. A row measured without a
budget fails too: a row outside the list is one the gate quietly stopped checking.
The last column says where each budget came from. A median the runner took, or a
ceiling the product states. Because those are different kinds of promise, and a
regression check is only meaningful against the first.

| program | metric | budget | margin | fails above | source |
| --- | --- | --- | --- | --- | --- |
| `acpilotd` | peak RSS | 20.0 MiB | 50 % | 30.0 MiB | the runner's median |
| `acpilotd` | mean CPU | 0.50 % | 100 % | 1.00 % | the runner's median |
| `acpilotd` | threads | 28 (4 + 1/core) | 50 % | 42 | the runner's count, one per CPU |
| `acpilotd + 1 session` | peak RSS | 24.0 MiB | 50 % | 36.0 MiB | the runner's median |
| `acpilotd + 1 session` | mean CPU | 0.50 % | 100 % | 1.00 % | the runner's median |
| `acpilotd + 1 session` | threads | 32 (8 + 1/core) | 50 % | 48 | the runner's count, one per CPU |
| `acpilotd + 1 turn` | peak RSS | 23.0 MiB | 50 % | 34.5 MiB | the runner's median |
| `acpilotd + 1 turn` | mean CPU | 0.50 % | 100 % | 1.00 % | the runner's median |
| `acpilotd + 1 turn` | threads | 32 (8 + 1/core) | 50 % | 48 | the runner's count, one per CPU |
| `acpilot serve` | peak RSS | 22.0 MiB | 50 % | 33.0 MiB | the runner's median |
| `acpilot serve` | mean CPU | 0.50 % | 100 % | 1.00 % | the runner's median |
| `acpilot serve` | threads | 28 (4 + 1/core) | 50 % | 42 | the runner's count, one per CPU |
| `acpilot tui` | peak RSS | 16.0 MiB | 50 % | 24.0 MiB | the runner's median |
| `acpilot tui` | mean CPU | 0.50 % | 100 % | 1.00 % | the runner's median |
| `acpilot tui` | threads | 1 (1) | 50 % | 2 | the runner's count |
| `acpilot doctor` | peak RSS | 8.0 MiB | 50 % | 12.0 MiB | the runner's median |
| `acpilot doctor` | threads | 2 (2) | 50 % | 3 | the runner's count |
| `acpilotd + 1 session` | added RSS | 15.0 MiB | 0 % | 15.0 MiB | the table's 15 MB ceiling |

The `acpilotd` rows are measured with the load named in theirs, not at rest. One
`/v1/events` subscriber is open for the window. The same daemon runs one idle
session and is read on its own process, because the mock agent it starts is a
separate program. The session's added memory is checked on its own, with no margin.
The TUI row is the client drawing its design pass under a 120x40 pseudo-terminal.
That client needs no daemon: the figure is the frame clock's cost, not a session's.
The one-shot CLI row is memory and threads only. A 170 ms process gives `sysinfo`
too few intervals for a percentage. So its CPU budget is `None` rather than zero.
The budgets below are the medians a run on GitHub's `ubuntu-latest` measured, not
this machine's. The table above is this machine. The gate compares the runner with
the runner. A thread budget is a fixed cost plus one per logical CPU, not a
single number. The runtime's pool follows the core count. A bare figure would be
a budget for the machine it was measured on.

## What is not measured here, and why

- `acpilot-gui` — needs a display and the shell's `gui` feature (P10).
- `language servers` — needs one installed. Measured as the daemon's children once P7's mock server is the fixture.
- `acpilotd-helper` — has no build on this host until P9.

Three things are also out of scope on purpose. The agent's own consumption is
third-party code, and it is shown only so a turn's total is visible. The browser
tab `acpilot serve` hands a page to is out of scope. Video memory belongs to the
desktop phase's P10-6. A language server's memory is reported per root when its
row lands, because one server is shared by every attached workspace.

## Limitations

- The memory and CPU budgets are one runner's medians. A runner image whose kernel
differs can move a row without a regression. Re-measure with `cargo xtask resources
--gate` on the runner and take its figures rather than widening the margin. The
thread budgets instead follow the logical CPU count, so the same constants describe
the runner and a larger machine.
- The session row's agent is this repository's fixture, not a real agent. The row
reads the daemon alone. A real agent's memory is its own program's, and the harness
budgets the daemon's share of a session.
- The daemon's window is three seconds. A sweep that runs every thirty seconds is
not in it. A regression in a periodic task needs a window that contains one.
- The free port is bound and released before the daemon binds it. A port taken in
between makes the daemon exit, which the harness reports as a dead process rather
than as a figure.
- A one-shot row reports memory and threads only: `sysinfo` derives CPU from the
delta between two refreshes. The CLI finishes in 170 ms. A percentage from that
many intervals is a number about the sampler, not about the program.
- A language server the daemon starts is sampled as part of the daemon's tree. No
row starts one until P7's mock server is the fixture.
- The TUI row draws the design pass under a just-opened 120x40 pseudo-terminal: no
daemon, no agent and no keystrokes. A real session's traffic and another terminal's
size move the figure, and its budget is a median of exactly this screen.
