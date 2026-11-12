# Design phase's task P0-3 — gpui against iced, on a GPU

Status: **closed, and the framework decision stands.** Both frameworks meet the
frame budget. Both miss the first-paint budget. The fallback is priced and it is
~3× slower, so we do not take it. Two parts of the task's spec are still
unmeasured: tree-sitter highlight cost for a 3k-line window, and any macOS
number. They are listed under Limitations rather than hidden.

Measured 2026-10-22 on native **Windows 11**, not in WSL, with `gpui` 0.2.2,
`iced` 0.14.0 and Rust 1.96.0. That is the Windows toolchain. The Linux side of
this box has 1.97.0.

| side | thing | version |
| --- | --- | --- |
| OS | Windows 11 | native, not WSL |
| GPU | NVIDIA GeForce RTX 5060 Laptop | driver 616.56 (`32.0.16.1656`) |
| display | laptop panel | 240 Hz |
| frameworks | `gpui` / `iced` | 0.2.2 / 0.14.0, default features, release |
| toolchain | rustc | 1.96.0 stable (msvc) |

## Why the numbers are from Windows and not from inside WSL2

WSL2 reports `Accelerated: no` and Mesa's llvmpipe, a CPU rasteriser. It would
answer for llvmpipe rather than for either framework.

The hardware is not the problem. `nvidia-smi` answers inside WSL2, `/dev/dxg`
exists, and the RTX 5060 is passed through. WSL2 does **not** offer a Vulkan ICD
for it. The installed ICDs are Intel, RADV, nouveau, lavapipe, asahi, gfxstream
and virtio. There is no `nvidia_icd.json`, and Ubuntu 24.04's
`mesa-vulkan-drivers` 25.2.8 does not ship Mesa's `dzn` (Vulkan over D3D12).
gpui's Linux renderer is Blade, which is Vulkan-only, so every frame there would
go through lavapipe.

The machine underneath is a Windows laptop with the same GPU, a Windows Rust
toolchain and Visual Studio. WSL can run Windows programs. Both benchmarks were
therefore built and run natively. The crate was copied to
`C:\Users\<user>\acpilot-gpui-bench` and `…\acpilot-iced-bench`.
`cargo build --release` ran there: gpui 10m47s cold / 33 s incremental, iced
4m02s cold / 13 s incremental. The executables started through interop. Each
window appears on the Windows desktop for ~2 s, and the process prints one JSON
object and exits.

That is a Windows number for a Windows GPU, which is what the budget wants. It
says nothing about macOS or Linux. The desktop phase's P10-3 macOS work has no
paint numbers yet.

## The two instruments

`probes/gpui-bench` and `probes/iced-bench` are separate standalone crates with
their own lockfiles. They draw the same screen: a **100 000-line** document in a
1400 × 900 window, with **50 rows** of 18 px visible in 13 px Consolas. Each row
is one named line of generated Rust. Rows are generated from their index,
**not materialised**, in both.

- `probes/gpui-bench`: a `uniform_list` of 100k rows, one text element per row,
  one scroll handle.
- `probes/iced-bench`: iced has no `uniform_list`, so the same window is
  virtualised by hand. A spacer holds the scrolled-past height, then the 50
  visible rows, then a spacer for the rest, driven with `scrollable::scroll_to`.
  Drawing 100k widgets was the mistake the gpui harness exists to rule out, so it
  is not repeated here.

Both run the same three phases after 30 warm-up frames. **Steady** runs 120
frames (repaint, no input). **Scroll** runs 60 frames (viewport moves three rows
per frame). **Type** runs 40 frames (one character edited into the row nearest the
viewport centre). They also run a cold **shaping probe** of 3 000 lines. First
paint has the same three parts. The first is process start → the app loop starts
(`app_ms`), the second is the window and device (`window_ms`), and the third is
the first frame (`first_frame_ms`).

What differs is only where the clock is read. It is worth being exact about that,
because it bounds what the comparison can claim:

-  **gpui's frame boundary is a callback that runs at frame start.** `build` is
  the view's `render()`. `paint` is the end of that render to the start of the
  next frame. It includes layout, shaping, paint, submit **and the compositor's
  vsync wait**. `interval` is start-to-start. At 240 Hz a vsync-bound
  frame is 4.17 ms, so a `paint` of 4.15 ms means "waiting", not "working".
- **iced's frame boundary is `window::frames()`**, which yields the `Instant`
  stamped at the start of handling `RedrawRequested`, *before* iced builds that
  frame's view. `interval` is still start-to-start pacing, and `build` is still
  the view body. But **there is no "paint" number**. A `tick - view end` delta
  would measure the gap to a request issued immediately after the previous
  present, i.e. 0.00 ms, and about nothing. iced's public API has no "frame
  presented" callback, and a screenshot fence would perturb the frame it
  measured. `edit_to_view` is reported instead: the edit to the view that
  responds to it, which is the app-side half of keystroke→paint.
- Because of that, **gpui's `edit_to_frame` (≈4.2 ms, a full vsync period) and
  iced's `edit_to_view` (≈0.01 ms) are not the same quantity**. Do not read one
  framework as 400× more responsive. The comparable statement is the one both
  report: frame pacing.

## Numbers

Interleaved runs (gpui, iced, gpui, iced, …) on the same machine, with the
binaries and their DLLs already in the OS cache. Neither framework gets a colder
system than the other: six gpui, five warm iced.

| what | gpui 0.2.2 | iced 0.14.0 |
| --- | --- | --- |
| start-up → app loop (`app_ms`) | 231–273 | 50–66 |
| start-up → window exists (`window_ms`) | 272–313 | 853–898 |
| start-up → first frame (`first_frame_ms`) | **277–326** | **851–896** |
| steady `interval` p50 / p95 | 4.14–4.17 / 4.41–4.56 | 4.15–4.18 / 4.32–4.45 |
| scroll `interval` p50 / p95 | 4.14–4.17 / 4.30–4.65 | 4.14–4.17 / 4.37–4.59 |
| view build per frame p95 | 0.00 | 0.01 |
| dropped frame intervals | 0–1 | 0 |
| shape 3 000 lines, cold | 49.5–51.4 ms (one run 66.0) | 46.1–47.6 ms |
| rows rendered per frame | 50 | 50 |

Beyond the table:

-  iced's **first run after a rebuild** is not the steady-state number. It took
  2 766 ms to first frame, with 11 dropped intervals while wgpu compiled its
  pipelines. Every run after took 851–896 ms. It is reported here because a
  user's first start after an upgrade pays it.
- iced's **font system**, measured by `probes/iced-bench` in `font` mode (one
  timed paragraph, no window), takes 25.5–26.0 ms warm. It takes 2 244 ms on the
  very first invocation on this machine. That is cosmic-text reading the system
  font database, and it is the reason a 300 ms start-up can be 2.5 s once.
- Neither harness measured a *paint* time it can attribute to the framework.
  Both are vsync-bound, and gpui's `paint` (4.15 ms) is the wait for the
  compositor, not work.

### Verdict against the two budgets

- **Keystroke→paint, p95 16 ms: met, by both, with room.** Both pace at
  4.1–4.6 ms p95 on a 240 Hz panel, with 0–1 missed vsync periods per run across
  ~1 300 measured frame intervals. App-side work is 0.00–0.01 ms per frame. The
  honest bound is that this is a vsync-bound measurement. The harness can say the
  budget is not close, not how much headroom above 240 Hz exists.
- **First paint, 150 ms: missed by both.** gpui is 1.85–2.17× over (277–326 ms).
  iced is 5.7–6.0× over (851–896 ms).
- **The fallback is priced, and it is the more expensive one.** iced buys a
  4.5–5.4× faster *boot* (50 ms against 231–273 ms) and spends it several times
  over in window and device creation. That is 853–898 ms to a painted window
  against 272–313 ms. Total first paint is ~3× worse. It has the same
  steady-state pacing and ~2× the build time. Nothing in these numbers argues
  for switching.

## Where the first-paint time actually goes (2026-10-24)

The 277–326 ms above is one number for three phases. A number that cannot be
attributed cannot be fixed. `ACPILOT_PROBE_TRACE=1` now prints phase marks taken
from outside gpui, together with gpui's own `log` records on the same µs clock.
The report gains a `trace` object. Same release binary, same machine, four runs
through the WSL interop:

| phase (cumulative, ms since process start) | run 1 | run 4 |
| --- | --- | --- |
| process start → DXGI has chosen the GPU | 94.0 | 191.8 |
| → `Created device with Direct3D 11.1 feature level` | 505.1 (**+411.1**) | 511.8 (**+320.0**) |
| → GUI font system ready (`Use Segoe UI`) | 573.6 (+68.5) | 569.5 (+57.6) |
| → `Application::new()` returns | 590.1 (+16.5) | 576.3 (+6.8) |
| → the view is built (window creation + `render` wiring) | 636.1 (+46.0) | 610.1 (+33.8) |
| → first frame presented | 652.4 (+16.2) | 616.6 (+6.5) |

What it says:

1. **Device creation is the whole story.** 320–411 ms of a 616–652 ms start is
   `CreateDevice`, and it happens inside `Application::new()`, before a window
   exists. Window creation is 34–46 ms, the view costs ~0, and the first frame
   takes 7–16 ms.
2. **gpui 0.2.2's Windows backend creates a Direct3D 11.1 device.** That is
   gpui's own log line, not an inference. Nothing at this version exposes an
   async or pre-warmed device. The lever is therefore where in the process
   lifetime the device is created, not how fast the window or the view is.
3. **The 277–326 ms warm figure did not reproduce.** These runs are 2.0–2.4× it,
   with the same binary shape and the same GPU. Frame pacing is unchanged
   (interval p95 4.6–4.7 ms, 1 dropped interval). The difference is entirely in
   the pre-window phase, which is the machine-state caveat below, observed rather
   than argued.
4. Adapter enumeration alone varied 94→192 ms between runs. The phase split is
   stable in shape and not in magnitude. A first-paint claim needs the run
   protocol, not just the total.

Limits of this instrument: it attributes between gpui's log lines, so anything
inside `CreateDevice` is one opaque number. The marks are taken on the main
thread and cost a mutex lock each. Four runs, one machine, one driver.

## Does the first-paint miss veto gpui?

`TODO.md` §the design phase's task P0-3 says to fall back to `iced` if gpui
"misses by >2×". The answer depends on the machine state, and that is the
finding, not a hedge:

- On a machine whose DLLs and driver caches are warm, gpui sits **on** the line:
  1.85–2.17×, median 1.90×. The veto does not fire. It is within run-to-run noise
  of firing, which is not a usable acceptance test.
- The same binary, run earlier in this session before those caches were warm,
  measured **580–968 ms**, 3.9–6.5×, and would have fired it. A first-paint
  budget that does not name the machine state it is measured in is measuring the
  OS file cache.
- iced misses the same budget by 5.7–6.0×. The clause reads as a number to clear
  rather than a comparison to win. Read that way, it rejects the **fallback**
  harder than the framework it was written to protect against. It cannot be the
  decision rule.

What the numbers do support:

1. **D1 stands: gpui.** The veto does not fire on the warm number, and the
   fallback is strictly worse on the only budget that is missed.
2. **A first-paint budget on a desktop framework needs a stated machine state.**
   It also needs a target that is not 150 ms. No native framework measured here
   paints in that time from a process start. gpui's own 277 ms is already 1.9×. If
   150 ms is a product requirement, it is a requirement about *how the app
   starts*: pre-warmed process, persisted window, splash. It is not about the
   framework, and it should be written in `TODO.md` §Perf that way.
3. **The remaining lever is window/device creation, not the view.** The 100k-row
   list costs ~0 ms per frame in both. iced's own split reports where the time
   goes: 51 ms to boot, then 750–800 ms to a window. A profile of that phase
   (DirectX/wgpu device creation and pipeline compilation) is the next useful
   measurement. It is the one thing that could move the number at all.

The rest of the benchmark's spec is still open. **Tree-sitter highlight cost** for
a 3k-line window is unmeasured, and it has to fit inside the shaping number
above. 3 000 lines cost ~50 ms cold in both frameworks (~17 µs per line). That
is why shaped lines are cached across frames rather than re-shaped. A 3k-line
window re-shaped every frame would be 50 ms of work per frame against a 4.2 ms
budget. The cache is therefore not an optimisation. It is the design.

## What the app does about it (2026-10-24)

`acpilot-gui` now claims a per-user socket before it starts gpui. It forwards a
second start to the process that owns the socket
(`crates/acpilot-gui/src/platform/`). A repeat start therefore never reaches the
phase above: no process boot, no font system, no `Application::new`. The forwarded
process prints one line, exits 0 in 4 ms, and the running instance opens the
window.

Measured on this box: Linux, X11 through Xwayland on Weston, **debug build**
(`cargo build`, not `--release`), 1100 × 720 window. `xdotool`/`xwininfo` polled
the X tree every 20 ms.

| what | 4 runs |
| --- | --- |
| cold: `exec` → X window mapped | 69–70 ms |
| repeat: `exec` of the forwarding start → its exit | 4 ms |
| repeat: `exec` of the forwarding start → the new window mapped | 7–8 ms |
| repeat: the window call inside the running instance, first frame included | 88–90 ms |

What those numbers do and do not say:

- **They are map times, not paint times.** A mapped window is what a person sees
  appear. The frame that fills it is inside the 88–90 ms figure above. The
  first-paint instrument from the rest of this note needs the probe crate, and no
  Linux first-paint number exists yet. The 616–652 ms and the 320–411 ms inside
  `CreateDevice` are Windows, and are not reproduced here.
- **On Linux the device is not saved, only the boot.** gpui 0.2.2 creates the
  blade renderer per window, so the 88–90 ms stays inside the running process
  even when the process is warm. The win is the 60 ms of process start, font
  system and `Application::new` that a second start does not pay. That is why
  the map time drops from ~70 ms to ~7 ms.
- **On Windows the device is saved.** The trace above puts `CreateDevice` inside
  `Application::new`, so a forwarded start skips it entirely. *Inferred, not
  measured on Windows:* the remaining cost should be the window path alone,
  34–46 ms window creation plus 7–16 ms to the first frame. That inference is why
  single-instance is worth doing on Windows at all. It is also the number to
  confirm the first time the shell runs on the Windows box.

## What the build still costs (from the same gpui pin)

Unchanged, and both still true on Windows:

- `libc` must be held at **≤ 0.2.189** while gpui 0.2.2 is in the graph.
  `zed-async-tar` → `xattr` 0.2.3 uses `libc::ENOATTR`, which 0.2.190 dropped.
  Irrelevant on Windows, load-bearing on Linux.
- The Linux link needs `libxkbcommon-x11`'s dev symlink even with X11 off. This
  is not fixable from this workspace. `gpui 0.2.2` asks its `xkbcommon`
  dependency for `x11` in its own manifest, and features are additive, so a
  downstream `default-features = false` does not remove it. Tried and recorded
  in ADR 0009 §5 rather than left as a to-do.

The build itself is fine: `gpui` 0.2.2 from crates.io alone, no git dependency, no
`zed/` checkout, no nightly. `iced` 0.14 builds clean from crates.io on both
platforms, in a third of the time.

## Limitations

- One machine, one GPU, one refresh rate. Six runs each. A 240 Hz panel hides
  headroom. The same harness on a 60 Hz panel would be pinned at 16.7 ms and
  would measure nothing but vsync.
- vsync was not disabled, so no "how much slack is there" number exists.
- **Warm-machine numbers.** Both frameworks were measured with their binaries and
  DLLs in the OS cache. The cold-first-run cost is real and, for iced, ~3× the
  warm one. It is reported above rather than averaged in.
- iced has no public frame-presented signal, so its paint and true
  keystroke→paint latency are not in this comparison. Only pacing and app-side
  work are.
- iced's shaping probe times `Paragraph::with_text`, which is shaping **and**
  layout. gpui's times `TextSystem::shape_line`. The iced figure is therefore an
  upper bound on a like-for-like comparison, not a like-for-like one.
- 50 visible rows, not 3 000. The 3k figure in the spec is about a highlight
  cache, which is unmeasured.
- Two gpui harness bugs were found and fixed earlier in this work. One was eager
  100k-row construction inflating first paint. The other was the view-build timer
  measuring a `Vec` clone instead of `render()`. The numbers here are from after
  both fixes.
- First paint was only attributed down to `CreateDevice` on Windows. The split
  inside it (pipeline compilation, adapter features, WDDM) is not measured.
- No macOS paint numbers for either framework. iced *does* paint on this WSL2
  box: 275 ms to first frame, on a 60 Hz software path. gpui's Vulkan-only
  renderer has no ICD for the physical GPU. That is a portability data point, not
  a comparable measurement.
