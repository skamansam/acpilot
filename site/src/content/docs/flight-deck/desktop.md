---
title: Desktop
description: The native app, on gpui, with an escape hatch already documented.
---

<span class="pill pill--hangar">hangar</span> &nbsp; Desktop phase (P10)

The desktop app is the second cockpit, not the only one. It talks to the same
daemon as the TUI, so a session started in the terminal opens here, mid-turn,
with the same permissions pending.

## The framework question

The GUI is built on **gpui**, Zed's framework, pinned exactly. That is a
deliberate trade: no semver contract, in exchange for a shaped text stack and a
display-map design that would otherwise be months of work.

| Candidate | Why not |
| --- | --- |
| `iced` | Stable and pleasant, but `text_editor` is textarea-grade: the editor surface is entirely yours to build |
| `egui` | Fastest to a panel; immediate-mode text layout and weak IME make it a debug tool, not an editor |
| Tauri | Reuses the web UI, but then the "native" app is a browser with an IPC hop per edit |
| **gpui** | Chosen: built for exactly this surface, Apache-2.0, and a sibling project uses it |

The decision is recorded as **ADR 0009**, with the exact pin, the benchmark
numbers, and the five triggers that would supersede it — the escape hatch is a
written rule, not a sentiment.

<Aside type="caution" title="The escape hatch is real">
The design phase's P0-3 benchmark tests gpui against the 16 ms keystroke→paint and
150 ms first-paint budgets. If a measured alternative is more than 2× better on a budget gpui
misses, or gpui needs a nightly toolchain or cannot build from a vendored tree,
the fallback is iced, then Tauri — and nothing outside `acpilot-gui` has to
change, because the text model is UI-agnostic.
</Aside>

## What it will do that the TUI cannot

- Image and PDF previews inside the transcript.
- Native drag-and-drop onto a session.
- Menus, deep links (`acpilot://session/<id>`), and notifications that survive a
  closed window.
- Real IME and screen-reader support. **Not inherited:** gpui at our pin ships no
  accessibility bridge, so that is implementation work, timeboxed early in the desktop phase (P10).

## Starting up

Launching `acpilot-gui` when it is already running does not start a second app:
the new process hands the request to the one that is running, and its window
opens there. On Linux and macOS the hand-over goes through a per-user socket in
`$XDG_RUNTIME_DIR` (`$TMPDIR` on macOS), so it costs nothing to look for.

That is a first-paint decision, not a single-instance rule for its own sake.
Creating the graphics device happens before the first frame and dominates a cold
start — 320–411 ms of a 616–652 ms launch on the machine where that benchmark ran, all of it
inside device creation. A repeat launch never reaches that phase. Where the
repeat-launch numbers were taken (Linux, X11, debug build) a cold launch put its
window on screen in 69–70 ms and a repeat launch in 7–8 ms; method, per-phase
numbers and their limits are in `docs/notes/gui-bench.md`.

Two honest limits: on Windows there is no channel yet, so every launch is its own
process until packaging; and on Linux the device is created per window at our
pin, so the repeat launch saves the boot rather than the device.

## Status

The crate drives a real window: the shared theme tokens, a menu bar, the profile
list it was launched with, and the command palette on `secondary-shift-p`. Panes,
the editor and the daemon connection belong to tasks P10-3 and P10-4 of the desktop phase. The `gui` feature is
off by default, so `cargo test` still compiles no graphics stack.
