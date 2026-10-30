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

## Building the shell

gpui sits behind a `gui` feature that is off by default, so `cargo build` and the
workspace test gate still compile no graphics stack. The tasks carry the flag a
plain `cargo build` forgets:

```bash
cargo xtask build gui      # the binary, with gpui
cargo xtask run gui        # build and launch it
cargo xtask test gui       # `gui` plus `gui-tests`, gpui's headless harness
cargo xtask gui-check      # lint, link and tests: the whole `gui` job
```

`test gui` adds the harness because it is a second feature: it compiles gpui's
`test-support` machinery, which is what lets the About panel be drawn in a real
`Context` with no display. `gui-check` is that run plus the clippy with both
features and the link, which is all CI's `gui` job does; the job runs this task
rather than three raw cargo lines, so a green local run and a green job mean the
same thing.

On Linux the link needs the development packages gpui asks for:

```bash
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev \
  libwayland-dev libfreetype-dev libfontconfig1-dev pkg-config clang
```

`libxkbcommon-x11-dev` is neither a mistake nor optional, and it is worth knowing
why. gpui 0.2.2 declares its `xkbcommon` dependency with
`features = ["wayland", "x11"]` in its own manifest, and Cargo features only ever
add: nothing a consumer writes can turn one of them off. So even a Wayland-only
run links `libxkbcommon-x11`, and without the dev package's `.so` symlink the link
step fails with a missing `-lxkbcommon-x11` while the runtime library is sitting
right there — which reads like a missing X server and is not. XWayland does not
remove the requirement either: it runs X11 clients under a compositor, it does not
replace the shared library a binary already linked against. The remaining options
are a fork of gpui or of `xkbcommon`, which trades one `apt install` for a vendored
framework; ADR 0009 §5 records that and keeps the package.

`libc` must also stay at exactly `0.2.189` on Linux: gpui 0.2.2 pulls `xattr`
0.2.3, which uses `libc::ENOATTR`, and `0.2.190` removed it. `cargo update` breaks
that silently, and the error a developer sees is `xattr`'s. The fix is
`cargo update -p libc --precise 0.2.189`; CI asserts the pin before it builds.

## Status

The crate drives a real window: the shared theme tokens, a menu bar, the profile
list it was launched with, the command palette on `secondary-shift-p`, and
**About** (the Acpilot menu, or `escape` to close it) naming the version, the
licence, the field manual and the tools this was built with. Quit is
`secondary-q` or the menu item. The window's theme is a setting — `theme_gui`
over `theme`, from the user's config or the project's — and `--theme` is only the
override for looking at another one without editing either.

The theme is also the window's first daemon connection, and it is a connection
rather than a file read on purpose: `acpilot-gui` subscribes to `GET /v1/events`
and redraws when a `prefs` frame names another theme. The frame that opens the
stream carries the current preferences, so a window on a machine that cannot see
the daemon's `config.toml` — a remote host, a container — still opens with the
right colours, and a save reaches every open window without a relaunch. A daemon
that is not running costs one request and leaves the launch theme in place; no
daemon is spawned, because a window that starts one has taken on the sessions it
serves. Panes and the editor are P10-3, and owning sessions over that connection
is P10-4. The `gui` feature is off by default, so `cargo test` still compiles no
graphics stack.

A launch names a path rather than a profile: `acpilot-gui` opens the project you
are in, `acpilot-gui src/main.rs` the project that file belongs to, and the profile
comes from those project settings. `--profile <dir>` remains for the case a path
cannot express — two profiles of the same tree — and it wins when it is given. A
directory that is not a project names no profile, and says so rather than opening a
window with an empty sidebar.
