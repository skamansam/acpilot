# 0009 — GUI framework: gpui 0.2.2, pinned exactly

- **Status:** accepted
- **Date:** 2026-10-24
- **Deciders:** project maintainers
- **Resolves:** D1 (GUI framework)
- **Relates to:** ADR 0001 (the index this fills), `docs/notes/gui-bench.md`
  (the design phase's task P0-3 measurements), TODO `P0-3` (closed), `P10-1`, `XT-1`, `XT-2`

## Context

The desktop app is the one interface whose framework is expensive to swap later:
its text surface, keybindings, theme plumbing and accessibility all inherit the
framework's primitives. Getting it wrong costs months, and getting it wrong
*quietly* — a year of work built on a dependency that stops moving — costs more.

Four candidates were considered (README §5.4 has the long form). The choice
between the first two is a trade, not a winner:

- **gpui** is the only candidate built for exactly this surface: shaped line
  layout in `gpui::text_system`, `uniform_list` for virtualised rows, and a
  display-map design in Zed's `crates/editor` we can copy for folds, blocks and
  inlays. It has no semver contract, and at 0.2.2 no accessibility bridge.
- **iced** has semver, a decade-stable API and a framework we would not be
  betting on someone else's roadmap for — but `text_editor` is textarea-grade, so
  the editor surface is entirely ours to write.

D1 was taken on 2026-10-20 on the strength of the first point plus a
non-technical one: a sibling project here will be a GUI builder written in gpui,
so framework knowledge, upgrade work and upstream fixes are paid once across two
products instead of sunk into a one-off. That decision was provisional until the design phase's task P0-3
measured it. The design phase's task P0-3 closed on 2026-10-22; this ADR records the number that settles
it and the trigger that reopens it.

## Decision

### 1. gpui, at exactly 0.2.2

`gpui = "=0.2.2"` in `[workspace.dependencies]`, consumed by `acpilot-gui` when
the desktop phase's task P10-2 wires it in. The `=` matters: a bare `"0.2.2"` is a caret requirement that
matches `0.2.3` and every later `0.2.x`, which is precisely the float this
decision forbids. Floating within a 0.x line is how a year-old surface becomes a
different year-old surface with nobody able to say which.

The pin is a policy, not a preference, and it has four parts:

| Rule | Why |
| --- | --- |
| Bump deliberately, never implicitly | A new release is adopted within a week of landing (XT-1), with the design phase's P0-3 harness as the gate, so breakage is attributable to one bump |
| Escalate to a git `rev` only with a note here | We never track `branch = "main"`; a rev bump carries the reason and the upstream issue in this file |
| Patch narrowly, upstream first | A carried change lives as a small series in `patches/gpui/` with the PR linked; a drifting fork is the failure mode |
| One pin across both projects | When the GUI-builder project starts, both repos pin the same version — and, if we ever escalate, the same `rev` — with one shared patch series |

The `zed/` checkout on this machine is reference reading for those patterns, and
is not a dependency.

### 2. What the numbers justify

The design phase's task P0-3 (`docs/notes/gui-bench.md`) built the same screen twice — a 100 000-line
document in a 1400 × 900 window, 50 rows of 18 px text visible, rows generated
rather than materialised — and ran both frameworks interleaved on one machine
(RTX 5060, 240 Hz panel, native Windows 11, release builds, warm file caches):

| | gpui 0.2.2 | iced 0.14.0 |
| --- | --- | --- |
| process start → app loop | 231–273 ms | 50–66 ms |
| → window and device exist | 272–313 ms | 853–898 ms |
| → first frame | **277–326 ms** | **851–896 ms** |
| steady frame interval p95 | 4.41–4.56 ms | 4.32–4.45 ms |
| scroll frame interval p95 | 4.30–4.65 ms | 4.37–4.59 ms |
| app-side work per frame (p95) | 0.00 ms | 0.01 ms |
| dropped frame intervals | 0–1 | 0 |
| shape 3 000 lines, cold | 49.5–51.4 ms (one run 66.0) | 46.1–47.6 ms |

Three conclusions, which are the reason this is an ADR and not a footnote:

1. **The keystroke budget is not close, for both.** The 16 ms p95 target is met
   with room (4.1–4.6 ms p95, 0–1 missed vsync periods per run). This is a
   vsync-bound measurement on a 240 Hz panel, so it says "the budget is not the
   binding constraint", not "there is 10× headroom".
2. **The fallback is the more expensive option, not the safer one.** iced boots
   4–5× faster and then spends it several times over in window and device
   creation: 851–896 ms to a painted window against 272–313 ms. Switching would
   make the one budget that is missed ~3× worse.
3. **Shaped lines must be cached across frames either way.** 3 000 lines cost
   ~50 ms to shape cold (~17 µs per line) in both frameworks — 12× the 4.2 ms
   frame period at 240 Hz. That is why the `acpilot-text` design holds a shaped
   cache and invalidates it by range rather than re-shaping a window per frame.
   It is a design constraint the benchmark established, and it is framework-
   independent.

### 3. The veto is a comparison, not an absolute number

The design phase's task P0-3's exit criteria said to fall back to iced if gpui missed the 150 ms
first-paint budget "by more than 2×". Measured honestly, that clause does not
work, and the reason is recorded here because it changes how future performance
gates are written:

- On a warm machine gpui is **1.85–2.17×** over (median 1.90×) — on the line.
- The *same binary* measured **580–968 ms** (3.9–6.5×) earlier in the session,
  before the DLL and driver caches were warm. A first-paint budget that does not
  state the machine state it was measured in is measuring the OS file cache.
- iced misses the same 150 ms budget by **5.7–6.0×**, so the clause read as an
  absolute number rejects the fallback harder than the framework it protects.

The clause is therefore restated as a comparison, and this is the operative rule
from here on:

> A framework is vetoed when a measured alternative is **more than 2× better** on
> a budget that is missed — same screen, same machine state, same run protocol —
> or when it needs a nightly toolchain, or cannot be built from a vendored tree,
> or would need an unsound patch to reach the budget.

No measurement supports switching. The lever that remains is window/device
creation (iced's own split: 51 ms to boot, then 750–800 ms to a window), not the
view, which costs ~0 ms per frame in both.

The 150 ms figure stays as a product target for how **the app starts** — a
pre-warmed process, a persisted window, a splash — not as a number any native
framework measured here meets from a cold process start. `TODO.md` §Perf now says
that in those words, so 277 ms is not a regression against a budget the reference
implementation also misses.

### 4. Recorded triggers that reopen this decision

This ADR is not "gpui forever". It converts to `superseded by NNNN` when any one
of these is observed and reproduced:

| Trigger | Threshold | Where it is caught |
| --- | --- | --- |
| iced becomes more than 2× better on a budget gpui misses | iced's first frame at most half gpui's — ≲140–165 ms while gpui stays at 277–326 ms, warm machine | Design phase's task P0-3 harness re-run (XT-1) |
| gpui needs a nightly toolchain, or cannot build from a vendored tree with no network | any | `cargo xtask check`, CI |
| The accessibility probe fails | `accesskit` bridge not viable by the end of the first week of the desktop phase (P10) | Desktop phase's task P10-7 |
| An upstream release breaks us and no fix lands within one release cycle | ~1 month | XT-1 bump PR stuck open |
| gpui 0.2.2's crates.io surface is yanked, or the crate stops publishing | any | XT-1 |

The fallback order is **iced 0.14** → Tauri (reuse the web UI). Both are priced
in README §5.4; neither is a rewrite, because nothing UI-specific lives outside
`acpilot-gui` (`acpilot-text`, the API client and every panel's data are
UI-agnostic by construction).

### 5. What the pin costs to build

Verified against the design phase's P0-3 probe lockfile, which is the authority for the resolved
graph rather than anyone's recollection:

- **gpui 0.2.2 resolves entirely from crates.io.** Zero git sources in the probe
  lockfile — `zed-font-kit 0.14.1-zed` is a published crate, not a rev. The
  README's earlier claim of a git dependency was wrong and is corrected with this
  ADR. Vendoring (XT-2) is still worth doing for offline reproducibility, but it
  is not rescuing a build that needs GitHub at compile time.
- **`libc` must stay at ≤ 0.2.189 on Linux**, because `zed-async-tar` → `xattr`
  0.2.3 uses `libc::ENOATTR`, which 0.2.190 removed. Irrelevant on Windows;
  load-bearing on Linux, and XT-1 must not "just bump libc".
- **The Linux link needs `libxkbcommon-x11`'s dev symlink**, even with X11 off.
  Packaging (P10-8) inherits this.
- **No nightly, no `zed/` checkout.** Stable Rust; Zed pins 1.91.1, below the
  workspace's 1.95 floor, so the MSRV is unaffected.

### 6. Accessibility is ours, and that is the honest cost

gpui 0.2.2's dependency graph contains no `accesskit*` package — verified against
the probe lockfile, not inferred — so there is no bridge to inherit: focus order,
labels, roles and live-region events for the transcript and the permission dialog
are implementation work (P10-7), timeboxed early so a failure is discovered in
week one rather than before release. The probe's default-feature iced build
contains no `accesskit*` package either, so the fallback would not hand us an
accessible surface for free; the difference is that iced offers a documented path
to one. That is recorded rather than glossed, because "we can switch if
accessibility goes badly" is only true if the switch is cheaper than the work.

## Consequences

- **We adopt a year-old API surface deliberately.** `0.2.2` was published
  2025-10-22 and main has moved on. We wait for features instead of tracking
  main, which means some friction we could patch around is simply accepted.
- **The pin is now enforced, so upgrades are a task.** XT-1's one-week adoption
  window, the exact pin, and the design phase's P0-3 harness as the gate are the price of the
  stability we do not get from semver.
- **Upstream cadence becomes ours.** If gpui's release pace stalls, we are the
  ones waiting; if it moves fast, we are the ones running the harness every week.
  The GUI-builder sibling project halves that cost and doubles our upstream
  leverage, which is the whole argument for the pin.
- **First paint is a startup problem, not a framework problem.** 277–326 ms on a
  warm machine against a 150 ms product target means the desktop phase's tasks P10-6 and P10-8 have to
  answer it with process/window strategy. The measured lever is device and window
  creation, and profiling that phase is the next useful measurement.
- **The first answer is process reuse, not a pre-warmed device.** The pin exposes
  no async or shared device, so `acpilot-gui` claims a per-user socket before it
  starts gpui and a second launch hands its request to the running process. On
  the box where it was measured a cold launch mapped its window in 69–70 ms and a
  repeat launch in 7–8 ms; on Windows the saved phase is the 320–411 ms
  `CreateDevice`. It also fixes a product question the ADR did not ask — two
  app icons and two windows over one profile — so it is not only a performance
  trade. Windows waits for a named pipe (P10-5).
- **Shaped-line caching is structural.** Any surface that re-shapes a visible
  window per frame would blow the frame budget by 12×; the text design cannot be
  simplified later without re-measuring.
- **Folds, inlays and the minimap are still ours.** gpui gives the primitives and
  a reference design, not a finished editor. The display map (P10-3) is real work
  with a known shape, which is a different risk from unknown work.
- **Screen readers are a v0.1 limitation if the probe fails.** the desktop phase's task P10-7 says so in
  the release notes rather than claiming parity we do not have.

## Alternatives considered

- **iced 0.14 as the primary.** Rejected on the numbers: 3× slower to first
  paint, the same frame pacing, and the editor surface — the part that is hard —
  entirely ours to write. It stays the first fallback because its semver and its
  documented `accesskit` path are exactly what we would need on the day the
  triggers in §4 fire.
- **egui / eframe.** Rejected: immediate-mode rebuilds text layout every frame,
  which against a 12× shaping cost per frame is not a style question; weak native
  selection and IME make it a debug tool rather than an editor.
- **Tauri 2 and the web UI.** Rejected as the primary: the "native" app would be
  a browser with an IPC hop per edit, contradicting the Rust-native premise, and
  it would make the desktop app a second consumer of the web frontend rather than
  its own cockpit. Kept as the second fallback because it reuses CodeMirror and
  its text editing is genuinely good.
- **No decision until the desktop phase (P10) starts.** Rejected: the text stack, keybindings and
  theme plumbing are the desktop phase's tasks P10-2 and P10-3, and starting them on a framework
  we might abandon is how a phase gets thrown away.
- **Tracking gpui `main`.** Rejected outright: unattributable breakage, no
  bisectable history across a release, and a build that changes under a reviewer.
