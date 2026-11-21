# Every program's CPU and memory

Cross-cutting task **XT-4**. Written by `cargo xtask resources`, which starts a
binary the archives carry against a temporary profile, drives one scripted load,
samples the whole process tree every 100 ms, and reports peak RSS, mean and peak
CPU and the thread count. Each row is measured three times and the median is
reported here; the window each figure was taken in is in the table, because a
memory number without one measures the absence of a load.

Measured on this machine: **Linux 24.04 Ubuntu**, kernel `6.18.54.1-microsoft-standard-WSL2`, `x86_64` target, 24 logical
CPU(s) — Intel(R) Core(TM) Ultra 9 275HX — and 31809.0 MiB of memory. The build is the **release** profile.

CPU is a fraction of **one core**, summed over the tree: a percentage of the
machine would hide the second core a busy loop costs. A `—` means the row ended
before a second sample could take a delta, so memory is the only figure it has.

| program | window | peak RSS | mean CPU | peak CPU | threads |
| --- | --- | --- | --- | --- | --- |
| `acpilotd` | idle: one `/v1/events` subscriber open for the window (drawing and waiting) | 19.2 MiB | 0.00 % of one core | 0.00 % of one core | 28 |
| `acpilotd + 1 session` | one idle session: created over the API, its mock agent running; the daemon's own process | 22.2 MiB | 0.00 % of one core | 0.00 % of one core | 32 |
| `acpilot doctor` | one-shot: reads PATH, the profile and the workspace; starts no daemon | 11.2 MiB | — | — | 1 |

One idle session adds **3.1 MiB** to the daemon's own process — the session row's
peak less the idle row's — against the table's 15 MB budget, which has no
margin because it is a stated ceiling rather than a figure this harness
discovered.

## Budgets

The first run set the budgets below and CI holds them: `cargo xtask resources --gate`
repeats each row three times and fails when a median is past its budget plus the
margin. A figure is written as a budget and a percentage rather than as one sample,
because the same program on another machine is not the same number, and a gate that
trips on a scheduling hitch is a gate somebody turns off. A row measured without a
budget fails too: a row outside the list is one the gate quietly stopped checking.

| program | metric | budget | margin | fails above |
| --- | --- | --- | --- | --- |
| `acpilotd` | peak RSS | 20.0 MiB | 50 % | 30.0 MiB |
| `acpilotd` | mean CPU | 0.50 % | 100 % | 1.00 % |
| `acpilotd` | threads | 12 | 50 % | 18 |
| `acpilotd + 1 session` | peak RSS | 24.0 MiB | 50 % | 36.0 MiB |
| `acpilotd + 1 session` | mean CPU | 0.50 % | 100 % | 1.00 % |
| `acpilotd + 1 session` | threads | 16 | 50 % | 24 |
| `acpilot doctor` | peak RSS | 8.0 MiB | 50 % | 12.0 MiB |
| `acpilot doctor` | threads | 2 | 50 % | 3 |

The `acpilotd` rows are measured with the load named in theirs rather than at rest:
one `/v1/events` subscriber open for the window, and the same daemon with one idle
session, read on its own process because the mock agent it starts is a separate
program. The session's added memory is checked on its own, with no margin. The
one-shot CLI row is memory and threads only — a 170 ms process gives `sysinfo` too
few intervals for a percentage — so its CPU budget is `None` rather than zero. The
budgets below are the medians a run on GitHub's `ubuntu-latest` measured, not this
machine's: the table above is this machine, and the gate compares the runner with
the runner. A box with more logical CPUs reports more threads, because the
daemon's pool follows the core count, so `--gate` there trips the thread rows
while `cargo xtask resources` still writes this note.

## What is not measured here, and why

- `acpilot-tui` — needs a terminal it is allowed to put into raw mode (a window is P6's frame clock).
- `acpilot-gui` — needs a display and the shell's `gui` feature (P10).
- `acpilot serve` — needs a page connected, which is P11's row.
- `language servers` — needs one installed; measured as the daemon's children once P7's mock server is the fixture.
- `acpilotd-helper` — has no build on this host until P9.

Also out of scope, on purpose: the agent's own consumption (third-party code, shown
only so a turn's total is visible), the browser tab `acpilot serve` hands a page to,
and video memory, which belongs to the desktop phase's P10-6. A language server's
memory is reported per root when its row lands, because one server is shared by every
attached workspace.

## Limitations

- The budgets are one runner's medians, so a runner image whose kernel or CPU count
differs can move the thread rows without a regression: re-measure with
`cargo xtask resources --gate` on the runner and take its figures rather than
widening the margin.
- The session row's agent is this repository's fixture, not a real agent, and the row
reads the daemon alone: a real agent's memory is its own program's, and the harness
budgets the daemon's share of a session.
- The daemon's window is three seconds, so a sweep that runs every thirty seconds is
not in it. A regression in a periodic task needs a window that contains one.
- The free port is bound and released before the daemon binds it; a port taken in
between makes the daemon exit, which the harness reports as a dead process rather
than as a figure.
- A one-shot row reports memory and threads only: `sysinfo` derives CPU from the
delta between two refreshes, and the CLI finishes in 170 ms. A percentage from that
many intervals is a number about the sampler, not about the program.
- A language server the daemon starts is sampled as part of the daemon's tree, but no
row starts one until P7's mock server is the fixture.
