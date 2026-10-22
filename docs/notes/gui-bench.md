# P0-3 — the gpui pin: buildable, not yet measurable

Status: **the build half is answered; the latency half is not measurable on this
machine.** P0-3 stays open until the numbers exist on hardware with a real GPU.

Measured 2026-10-21 on WSL2 (Ubuntu 24.04.5, kernel 6.18.33.2-microsoft-standard-WSL2),
Rust 1.97.0, `gpui` 0.2.2.

| side | thing | version |
| --- | --- | --- |
| toolchain | rustc | 1.97.0 stable |
| framework | `gpui` | 0.2.2 (crates.io) |
| renderer | Mesa llvmpipe | 25.2.8, OpenGL 4.5 core, `Accelerated: no` |
| display | WSLg | Wayland socket + `DISPLAY=:0`, no `/dev/dri` |

## What was measured: does the pin build from crates.io alone?

Yes — with two conditions, and both are worth knowing before P10 pins it.

```bash
cd spikes/gpui-pin
cargo run --release      # links gpui and prints a colour
```

A release build from a clean target is **48.5 s** for the dependency graph, no
git dependency, no `zed/` checkout, no nightly toolchain. That answers the exit
criterion that would have vetoed the framework: *cannot be built from a clean
vendored tree* does not fire. `deny.toml`'s `github = ["zed-industries"]` source
allowance is not exercised by 0.2.2 either.

### Condition 1: `libc` must be held at ≤ 0.2.189

gpui → `gpui_http_client` → `zed-async-tar` → `xattr` 0.2.3, and `xattr` uses
`libc::ENOATTR`. Linux libc stopped exporting that in 0.2.190, so a fresh resolve
fails **before gpui compiles**:

```
error[E0425]: cannot find value `ENOATTR` in crate `libc`
  --> .../xattr-0.2.3/src/sys/mod.rs:12
```

Bisected by building only `xattr` against each pin:

| `libc` | result |
| --- | --- |
| 0.2.174 | builds |
| 0.2.175 | builds |
| 0.2.186 | builds |
| 0.2.189 | builds |
| 0.2.190 | **fails** (`ENOATTR`) |
| 0.2.191 | **fails** (`ENOATTR`) |

So the pin is `libc = "=0.2.189"` while gpui 0.2.2 is in the graph — and that is a
workspace-wide hold, because the resolution is shared with `portable-pty`, which
currently brings 0.2.191. It is not a one-line inconvenience: holding a crate as
central as `libc` back for a GUI framework is exactly the kind of hidden cost the
spike exists to price. When P10 adds gpui, either hold libc or patch `xattr`;
both belong in the ADR.

### Condition 2: the Linux link needs `libxkbcommon-x11`

Linking fails without the *dev* symlink, even with X11 turned off:

```
rust-lld: error: unable to find library -lxkbcommon-x11
```

`gpui = { default-features = false, features = ["font-kit", "wayland"] }` does not
remove it — the `xkbcommon` binding links the X11 support library regardless. On
this box only `libxkbcommon-x11.so.0` is installed (no `-dev` package, and no root
to add one), so the build ran with a local symlink on the linker path:

```bash
mkdir -p /tmp/xkblib
ln -sf /usr/lib/x86_64-linux-gnu/libxkbcommon-x11.so.0 /tmp/xkblib/libxkbcommon-x11.so
RUSTFLAGS="-L /tmp/xkblib" cargo build --release
```

A normal Linux desktop installs `libxkbcommon-x11-dev` and never sees this. It is
recorded because a CI image will see it.

## Why there are no latency numbers yet

The budgets P0-3 is judged against — **16 ms p95 keystroke→paint, 150 ms first
paint** — are properties of the renderer. This box has no `/dev/dri`, and
`glxinfo` reports Mesa llvmpipe with `Accelerated: no`: every frame would be
rasterised by the CPU. Measuring keystroke→paint here and comparing it with 16 ms
would answer for *llvmpipe*, not for gpui, and would veto the framework for a
property of the test machine. That is the same mistake as publishing WSL1 startup
numbers as WSL2's (P0-5), and this project has already had to correct that once.

The spike therefore stops at the boundary: a machine with a GPU runs the harness,
this one proves the pin builds.

## What the GPU machine must run

From `TODO.md` §P0-3, unchanged:

- a 100k-line Rust file rendered through `text_system` shaped lines and a
  `uniform_list` row window, not one giant element;
- first paint, keystroke→paint (p95), and scroll frame time;
- tree-sitter highlighting cost for a 3k-line visible window;
- a minimal `iced` 0.14 build of the same screen, so the fallback is priced.

Write the numbers into this file, then either keep gpui (and write the ADR with
the libc hold as a recorded cost) or take the veto to `iced` → Tauri.

## Limitations

- The renderer fact is the whole limitation: no GPU, no decision-grade paint
  numbers, and no reading of gpui's GPU path at all.
- The build was done with the Wayland backend only; X11 is untested here.
- One toolchain (1.97.0) and one `gpui` version. A newer patch release may drop
  `zed-async-tar`, which would retire condition 1 — check before pinning libc
  forever.
