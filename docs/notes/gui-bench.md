# P0-3 — the gpui benchmark, on a GPU

Status: **the frame budget is met with margin; the first-paint budget is missed,
and the number that misses it is not yet attributable to gpui.** P0-3 stays open
on one sub-item: the `iced` baseline that turns the first-paint number into a
comparison instead of a verdict.

Measured 2026-10-22 on native **Windows 11**, with `gpui` 0.2.2 and Rust 1.96.0
(the Windows toolchain; the Linux side of this box has 1.97.0).

| side | thing | version |
| --- | --- | --- |
| OS | Windows 11 | native, not WSL |
| GPU | NVIDIA GeForce RTX 5060 Laptop | driver 616.56 (`32.0.16.1656`) |
| display | laptop panel | 240 Hz |
| framework | `gpui` | 0.2.2, default features, release |
| toolchain | rustc | 1.96.0 stable (msvc) |

## Why the numbers are from Windows and not from inside WSL2

The previous run of this note stopped because WSL2 reports `Accelerated: no` and
Mesa's llvmpipe — a CPU rasteriser — which would have answered for llvmpipe rather
than for gpui.

The hardware was not the problem. `nvidia-smi` answers inside WSL2, `/dev/dxg`
exists, and the RTX 5060 is passed through; what WSL2 does **not** offer is a
Vulkan ICD for it. The installed ICDs are Intel, RADV, nouveau, lavapipe, asahi,
gfxstream and virtio — no `nvidia_icd.json`, and Mesa's `dzn` (Vulkan over D3D12)
is not shipped by Ubuntu 24.04's `mesa-vulkan-drivers` 25.2.8. gpui's Linux
renderer is Blade, which is Vulkan-only, so every frame there would have gone
through lavapipe.

The machine underneath is a Windows laptop with the same GPU, a Windows Rust
toolchain and Visual Studio, and WSL can run Windows programs. So the benchmark
was built and run natively: `probes/gpui-bench` copied to
`C:\Users\<user>\acpilot-gpui-bench`, `cargo build --release` there (10m47s cold,
33 s incremental), and the executable started through interop. The window appears
on the Windows desktop for ~2 s and the process prints one JSON object and exits.

That is a Windows number for a Windows GPU, which is what the budget wants. It
says nothing about macOS or Linux; P10-3's macOS work has no paint numbers yet.

## The instrument

`probes/gpui-bench` (standalone crate, own lockfile):

- a `uniform_list` of **100 000 rows**, each row one text element at 13 px in
  Consolas, inside a 1400 × 900 window — 50 rows visible, which is what an editor
  of that window size actually draws;
- rows are **generated on demand** from their index, not materialised. The first
  version of the harness built all 100k `SharedString`s up front and added 724 ms
  to first paint that had nothing to do with the framework; that number would have
  been reported as gpui's;
- three phases in one run, all after 30 warm-up frames: **steady** 120 frames
  (repaint, no input), **scroll** 60 frames (viewport moves three rows per frame
  through `scroll_to_item`), **type** 40 frames (one character edited into the
  row nearest the viewport centre, timed from the edit to the next frame);
- a separate **shaping probe**: 3 000 lines through `TextSystem::shape_line` with
  the text already built, cold (no cache), so shaping cost is not confused with
  paint cost;
- `first paint` is measured in three parts, because 150 ms of budget divided
  between framework start-up and a view is not one number:
  `app_ms` (process start → the app loop starts), `window_ms` (→ the view is
  constructed, i.e. the window and device exist), `first_frame_ms` (→ the first
  frame after that has been submitted).

Frame timings are wall-clock at the frame boundary: `build` is the view's
`render()`; `paint` is the end of that render to the start of the next frame,
which includes layout, shaping, paint, submit **and the compositor's vsync wait**;
`interval` is start-to-start. At 240 Hz a vsync-bound frame is 4.17 ms, so a
`paint` of 4.15 ms means "waiting", not "working".

## Numbers

Three consecutive runs with the window visible, nothing else on that GPU:

| what | run 1 | run 2 | run 3 |
| --- | --- | --- | --- |
| start-up → app loop (`app_ms`) | 770.6 | 389.3 | 654.1 |
| start-up → window exists (`window_ms`) | 948.0 | 572.1 | 726.2 |
| start-up → first frame (`first_frame_ms`) | 967.6 | 580.1 | 733.6 |
| steady `interval` p50 / p95 | 4.16 / 4.46 | 4.16 / 4.41 | 4.16 / 4.46 |
| steady `paint` p95 | 4.45 | 4.40 | 4.46 |
| scroll `interval` p50 / p95 | 4.16 / 4.40 | 4.15 / 4.41 | 4.15 / 4.47 |
| edit → next frame p50 / p95 | 4.13 / 4.37 | 4.17 / 4.49 | 4.14 / 4.36 |
| dropped frame intervals | 0 | 0 | 0 |
| shape 3 000 lines, cold | 52.2 ms | 52.5 ms | 51.3 ms |

`build` is 0.00 ms in every phase by design: `uniform_list` calls its row closure
during layout, not during `render()`, so the row work is in `paint`, where it
belongs. Rows rendered per frame: 50.

### Verdict against the two budgets

- **Keystroke→paint, p95 16 ms: met, with about 3.5× margin.** p95 frame interval
  is 4.4–4.5 ms on a 240 Hz panel, and the edit→next-frame p95 is 4.4–4.5 ms, with
  zero missed vsync periods in 660 measured intervals. The honest bound is that
  the measurement is vsync-bound: gpui is keeping up at 240 Hz, and this harness
  cannot say how much headroom above that exists. It can say the budget is not
  close.
- **First paint, 150 ms: missed by 3.9–6.5×.** 580–968 ms, of which 389–771 ms is
  spent before the app loop runs at all and a further ~180 ms in window and device
  creation. None of it is this harness's view, which costs ~0 ms.

## Does the first-paint miss veto gpui?

`TODO.md` §P0-3 says to fall back to `iced` if gpui "misses by >2×". On the number
alone it does. Two reasons not to apply that as written yet:

1. **The criterion was written expecting an iced measurement to compare against** —
   "also measure a minimal iced build of the same screen, so the fallback is
   priced rather than assumed". That half is not done, so a 2× miss has nothing to
   be 2× *of*. Process start-up, DLL loading and font enumeration are costs any
   native UI pays; if iced/Tauri pay 600 ms too, the framework is not the problem,
   and switching buys nothing.
2. **The number is not attributable.** 389–771 ms elapses before the app loop
   begins, and this harness cannot see inside it. The plausible components are
   DirectX device creation and the system font database; which one dominates is a
   10-minute profile, not a guess worth writing down as fact.

So P0-3 is **not** decided by this run. What would decide it, in order of cost:

1. an `iced` 0.14 window with the same 100k-row screen, measuring the same three
   start-up numbers (a day, and the remaining half of the exit criteria);
2. a profile of the app-loop phase to see whether first paint can be cut by
   lazily creating anything (font enumeration is the usual suspect);
3. a macOS run on hardware, which is where a first-paint budget is most likely to
   be met or missed differently.

The rest of the benchmark's spec — tree-sitter highlight cost for a 3k-line
window — is unmeasured. The shaping number above is the ceiling that highlight
cost has to fit inside: 3 000 lines cost ~52 ms cold, i.e. ~17 µs per line, which
is why shaped lines are cached across frames rather than re-shaped. A 3k-line
window re-shaped every frame would be 52 ms of work per frame against a 4.2 ms
budget, so the cache is not an optimisation, it is the design.

## What the build still costs (from the same pin)

Unchanged from the previous run, and both still true on Windows:

- `libc` must be held at **≤ 0.2.189** while gpui 0.2.2 is in the graph
  (`zed-async-tar` → `xattr` 0.2.3 uses `libc::ENOATTR`, dropped in 0.2.190).
  Irrelevant on Windows, load-bearing on Linux.
- The Linux link needs `libxkbcommon-x11`'s dev symlink even with X11 off.

The build itself is fine: `gpui` 0.2.2 from crates.io alone, no git dependency, no
`zed/` checkout, no nightly. On Windows the dependency graph takes 10m47s from
cold and 33 s incremental.

## Limitations

- One machine, one GPU, one refresh rate; three runs. A 240 Hz panel hides
  headroom: the same harness on a 60 Hz panel would be pinned at 16.7 ms and would
  measure nothing but vsync.
- vsync was not disabled, so no "how much slack is there" number exists.
- 50 visible rows, not 3 000. The 3k figure in the spec is about a highlight cache,
  which is unmeasured.
- Two harness bugs were found and fixed during this work (eager 100k-row
  construction inflating first paint; the view-build timer measuring a `Vec` clone
  instead of `render()`). The numbers above are from after both fixes; the earlier
  numbers are not in this note because they measured the harness.
- No macOS or Linux paint numbers, and Linux cannot have any on a WSL2 host
  without a Vulkan ICD for the physical GPU.
