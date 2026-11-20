# Every program's CPU and memory

Cross-cutting task **XT-4**. Written by `cargo xtask resources`, which starts a
binary the archives carry against a temporary profile, drives one scripted load,
samples the whole process tree every 100 ms, and reports peak RSS, mean and peak
CPU and the thread count. Each row is measured three times and the median is
reported here; the window each figure was taken in is in the table, because a
memory number without one measures the absence of a load.

Measured on this machine: **Linux 24.04 Ubuntu**, kernel `6.18.58.1-microsoft-standard-WSL2`, `x86_64` target, 24 logical
CPU(s) — Intel(R) Core(TM) Ultra 9 275HX — and 31809.0 MiB of memory. The build is the **release** profile.

CPU is a fraction of **one core**, summed over the tree: a percentage of the
machine would hide the second core a busy loop costs. A `—` means the row ended
before a second sample could take a delta, so memory is the only figure it has.

| program | window | peak RSS | mean CPU | peak CPU | threads |
| --- | --- | --- | --- | --- | --- |
| `acpilotd` | idle: one page request every 100 ms (drawing and waiting) | 26.5 MiB | 1.15 % of one core | 4.57 % of one core | 30 |
| `acpilot doctor` | one-shot: reads PATH, the profile and the workspace; starts no daemon | 11.1 MiB | — | — | 1 |

## Budgets

The table in `TODO.md` names two memory budgets today: an idle daemon at 60 MB and
an idle session at 15 MB. The daemon row above is the first measurement of the first
one against a real process; the session row needs a session, and the CPU rows are
still unset. Budgets are a median plus a margin rather than one sample, so they want
the reference runner's variance before they are written down here as limits.

`cargo xtask resources` is not a CI job yet. The plan the task records: the first run
sets the numbers, a job on the reference runner repeats the script, and a figure past
its margin fails. A hosted runner is noisier than this machine, so the job is a
regression gate and this note keeps the measured number and the hardware — neither is
a claim about a user's machine.

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

- The daemon row asks for the page, not a subscriber on `/v1/events`; an events
subscriber is the load the task names and is the next row to add.
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
