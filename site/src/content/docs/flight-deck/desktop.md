---
title: Desktop
description: The native app, on gpui, with an escape hatch already documented.
---

<span class="pill pill--hangar">hangar</span> &nbsp; Desktop phase (P10)

The desktop app is the second cockpit, not the only one. It talks to the same
daemon as the TUI. A session started in the terminal therefore opens here,
mid-turn, with the same permissions pending.

## The framework question

The GUI uses **gpui**, Zed's framework, pinned exactly. That is a deliberate
trade. It gives up a semver contract in exchange for a shaped text stack and a
display-map design that would otherwise take months.

| Candidate | Why not |
| --- | --- |
| `iced` | Stable and pleasant, but `text_editor` is textarea-grade. You build the editor surface entirely yourself |
| `egui` | Fastest to a panel. Immediate-mode text layout and weak IME make it a debug tool, not an editor |
| Tauri | Reuses the web UI, but then the "native" app is a browser with an IPC hop per edit |
| **gpui** | Chosen: built for exactly this surface, Apache-2.0, and a sibling project uses it |

**ADR 0009** records the decision, with the exact pin, the benchmark
numbers, and the five triggers that would supersede it. The escape hatch is a
written rule, not a sentiment.

<Aside type="caution" title="The escape hatch is real">
The design phase's P0-3 benchmark tests gpui against the 16 ms keystroke→paint
and 150 ms first-paint budgets. We fall back to iced, then Tauri, if a measured
alternative is more than 2× better on a budget gpui misses. The same fallback
applies if gpui needs a nightly toolchain or cannot build from a vendored tree.
Nothing outside `acpilot-gui` has to change, because the text model is
UI-agnostic.
</Aside>

## What it will do that the TUI cannot

- Image and PDF previews inside the transcript.
- Native drag-and-drop onto a session.
- Menus, deep links (`acpilot://session/<id>`), and notifications that survive a
  closed window.
- Real IME and screen-reader support. **Not inherited:** gpui at our pin ships no
  accessibility bridge. That is implementation work, timeboxed early in the
  desktop phase (P10).

## Starting up

Opening `acpilot-gui` when it is already running does not start a second app.
The new process hands the request to the running one, and its window opens there.
On Linux and macOS the hand-over goes through a per-user socket in
`$XDG_RUNTIME_DIR` (`$TMPDIR` on macOS), so looking for it costs nothing.

That is a first-paint decision, not a single-instance rule for its own sake.
Creating the graphics device happens before the first frame and dominates a cold
start. It took 320–411 ms of a 616–652 ms start on the benchmark machine. All of
that time was inside device creation. A repeat start never reaches that phase.
The repeat-start numbers came from Linux, X11, and a debug build. A cold start
took 69–70 ms to put its window on screen. A repeat start took 7–8 ms. The
method,
per-phase numbers and their limits are in `docs/notes/gui-bench.md`.

Two honest limits. On Windows there is no channel yet, so every start is its own
process until packaging. On Linux gpui creates the device per window at our pin,
so the repeat start saves the boot rather than the device.

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

`test gui` adds the harness because it is a second feature. It compiles gpui's
`test-support` machinery, which is what lets the About panel be drawn in a real
`Context` with no display. `gui-check` is that run plus the clippy with both
features and the link. That is all CI's `gui` job does. The job runs this task
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
add. Nothing a consumer writes can turn one of them off. Even a Wayland-only run
therefore links `libxkbcommon-x11`. Without the dev package's `.so` symlink, the
link step fails with a missing `-lxkbcommon-x11` while the runtime library sits
right there. That reads like a missing X server, and it is not. XWayland does not
remove the requirement either. It runs X11 clients under a compositor. It does
not replace the shared library a binary already linked against. The remaining
options are a fork of gpui or of `xkbcommon`, which trades one `apt install` for
a vendored framework. ADR 0009 §5 records that and keeps the package.

`libc` must also stay at exactly `0.2.189` on Linux. gpui 0.2.2 pulls `xattr`
0.2.3, which uses `libc::ENOATTR`, and `0.2.190` removed it. `cargo update` breaks
that silently, and the error a developer sees is `xattr`'s. The fix is `cargo
update -p libc --precise 0.2.189`. CI asserts the pin before it builds.

## Status

The crate drives a real window. It draws the shared theme tokens, a menu bar, and
the profile list it opened with. It also draws the command palette on
`secondary-shift-p`, and **About** (the Acpilot menu, or `escape` to close it).
About names the version, the licence, the field manual and the toolchain. Quit is
`secondary-q` or the menu item. The window's theme is a setting: `theme_gui` over
`theme`, from the user's config or the project's. `--theme` is only the override
for looking at another one without editing either.

The theme is also the window's first daemon connection. It is a connection rather
than a file read on purpose. `acpilot-gui` subscribes to `GET /v1/events` and
redraws when a `prefs` frame names another theme. The frame that opens the stream
carries the current preferences. A window on a machine that cannot see the
daemon's `config.toml`, such as a remote host or a container, therefore still
opens with the right colours. A save reaches every open window without a
relaunch. A daemon that is not running costs one request and leaves the opening
theme in place. The app spawns no daemon, because a window that starts one has
taken on the sessions it serves. Panes and the editor are P10-3. Owning sessions
over that connection is P10-4. The `gui` feature is off by default, so `cargo
test` still compiles no graphics stack.

The footer carries the stats bar: one row per provider and agent, a context track
and a credit track. It uses the same `acpilot_stats` value the terminal and the
page draw. The window therefore cannot disagree with either about what `18.4k /
32.8k` means. It also cannot paint an estimate as a measurement. A figure nobody reported is
drawn as a hairline rather than a full or an empty strip. A reading that went
quiet says `old`. `F4`, or **Where the stats came from** in the palette, answers
what measured each figure instead of what it is. The window asks the daemon for
these every fifteen seconds. With no daemon behind it, the bar says so rather than
failing a start over a footer.

The source-control overlay is real too. `F6` reads the changed paths and both
halves of the selected file's diff. `h`/`l` choose an unstaged hunk, and
`Shift-S` stages just that hunk. `s`/`u` stage and unstage a whole path. `d`/`x`
arm a discard or checkout that `y` confirms, and `n` cancels. Discard keeps the
staged half. Checkout resets both halves to `HEAD` and is unavailable before the
first commit. `c` opens the commit composer. `Tab`/`Shift-Tab` there chooses which
listed session drafts the message. A window with no session pane can therefore
still ask the agent working on that part of the repository. The first session is
only the default. `Ctrl-G` asks it for a draft, and `Ctrl-S` then `y` commits the staged
index.

The command names a path rather than a profile. `acpilot-gui` opens the project you
are in, and `acpilot-gui src/main.rs` opens the project that file belongs to. The
profile comes from those project settings. `--profile <dir>` remains for the case
a path cannot express, such as two profiles of the same tree, and it wins when
given. A directory that is not a project names no profile, and the app says so
rather than opening a window with an empty sidebar.
