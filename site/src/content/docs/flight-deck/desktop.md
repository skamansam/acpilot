---
title: Desktop
description: The native app, on gpui, with an escape hatch already documented.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P10

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

<Aside type="caution" title="The escape hatch is real">
P0-3 benchmarks gpui against the 16 ms keystroke→paint and 150 ms first-paint
budgets. If it misses by more than 2×, needs a nightly toolchain, or cannot build
from a vendored tree, the fallback is iced, then Tauri — and nothing outside
`acpilot-gui` has to change, because the text model is UI-agnostic.
</Aside>

## What it will do that the TUI cannot

- Image and PDF previews inside the transcript.
- Native drag-and-drop onto a session.
- Menus, deep links (`acpilot://session/<id>`), and notifications that survive a
  closed window.
- Real IME and screen-reader support. **Not inherited:** gpui at our pin ships no
  accessibility bridge, so that is implementation work, timeboxed early in P10.

## Status

The crate is a placeholder with the dependency deliberately unwired, so `cargo
test` does not compile a graphics stack before there is anything to draw.
