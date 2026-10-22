//! P0-3's latency half: what a 100k-line document costs to draw, on a real GPU.
//!
//! ```bash
//! cargo run --release
//! ```
//!
//! The screen is the one an editor actually draws — a `uniform_list` of 100k
//! rows, each row a single shaped text element, one scroll handle — not one giant
//! element holding the whole file. Three phases, in one run:
//!
//! * **steady**: no input. Every frame rebuilds the visible rows unchanged, which
//!   is the floor an editor has to pay for cursor blink and repaints.
//! * **scroll**: the viewport moves three rows per frame, so the row window and
//!   the scroll offset both change every frame.
//! * **type**: one character is edited per frame, and the time from the edit to
//!   the next frame is recorded. That is the render half of keystroke→paint; the
//!   OS input queue is not in it, and the report says so.
//!
//! The budget this is judged against is §Perf's **16 ms p95 keystroke→paint** and
//! **150 ms first paint**. Frame *interval* answers the budget, because a frame
//! that misses the compositor shows up as a gap rather than as a slower frame.
//!
//! This binary is deliberately standalone (its own `[workspace]`): it uses gpui's
//! default features, which is the shipping configuration, and it prints JSON so
//! the numbers can be pasted into `docs/notes/gui-bench.md` without retyping.
//!
//! Exit: prints one JSON object and exits 0. Nothing is written to disk.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Instant;

/// Process start, so "first paint" includes startup and not only the window's
/// first frame.
static STARTED: OnceLock<Instant> = OnceLock::new();
/// When the app loop began, set by `main` and read by the view.
static APP_STARTED: OnceLock<Option<u64>> = OnceLock::new();

use gpui::{
    App, AppContext, Application, Bounds, Context, IntoElement, ParentElement, Render,
    ScrollStrategy, SharedString, Size as GpuiSize, Styled, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions, div, font, hsla, point, px, uniform_list,
};

/// The document. 100k lines is §Perf's "large file"; the row count is what a
/// `uniform_list` is for, and rendering it as one element is the mistake the
/// benchmark exists to rule out.
const TOTAL_LINES: usize = 100_000;
/// Lines shaped in the one-off shaping measurement. §Perf says "a 3k-line
/// visible window", which is more than a screenful: 3k lines at 18 px is 54k px,
/// so this is the scroll-back cache an editor keeps shaped, not the viewport.
const SHAPE_LINES: usize = 3_000;

const WARMUP_FRAMES: usize = 30;
const STEADY_FRAMES: usize = 120;
const SCROLL_FRAMES: usize = 60;
const TYPE_FRAMES: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Warmup,
    Steady,
    Scroll,
    Type,
    Done,
}

#[derive(Default)]
struct Frame {
    build_us: u64,
    /// From the end of this frame's view build to the start of the next frame:
    /// layout, paint, submit and the compositor's wait.
    paint_us: Option<u64>,
    /// Start-to-start. A vsync miss is a long interval, not a slow frame.
    interval_us: Option<u64>,
}

struct Run {
    /// Process start, so "first paint" includes application startup rather than
    /// only the window's first frame.
    t0: Instant,
    frames: Vec<Frame>,
    phase: Phase,
    phase_frames: usize,
    last_render_end: Option<Instant>,
    last_next_frame: Option<Instant>,
    first_frame_us: Option<u64>,
    /// Process start to gpui's window and device existing, and to the view being
    /// constructed inside it. The split matters for the 150 ms first-paint
    /// budget: the first number is the framework's startup, the second is ours,
    /// and only the second is something this harness controls.
    /// Process start to the app loop starting, and to the window existing.
    /// Splitting them says whether the fixed cost is platform start-up or
    /// window/device creation.
    app_us: Option<u64>,
    window_us: Option<u64>,
    view_us: Option<u64>,
    first_render_us: Option<u64>,
    scroll_top: usize,
    edit_at: Option<Instant>,
    edit_us: Vec<u64>,
    rows_rendered: usize,
    shape_us: Option<u64>,
    shaping_font: Option<String>,
}

impl Run {
    fn new() -> Self {
        Self {
            t0: STARTED.get().copied().unwrap_or_else(Instant::now),
            frames: Vec::with_capacity(WARMUP_FRAMES + STEADY_FRAMES + SCROLL_FRAMES + TYPE_FRAMES),
            phase: Phase::Warmup,
            phase_frames: 0,
            last_render_end: None,
            last_next_frame: None,
            first_frame_us: None,
            app_us: None,
            window_us: None,
            view_us: None,
            first_render_us: None,
            scroll_top: 0,
            edit_at: None,
            edit_us: Vec::with_capacity(TYPE_FRAMES),
            rows_rendered: 0,
            shape_us: None,
            shaping_font: None,
        }
    }

    fn begin_frame(&mut self) -> Instant {
        if self.frames.is_empty() {
            self.first_render_us = Some(micros(self.t0.elapsed()));
        }
        self.frames.push(Frame::default());
        Instant::now()
    }

    fn end_frame(&mut self, start: Instant) {
        if let Some(frame) = self.frames.last_mut() {
            frame.build_us = micros(start.elapsed());
        }
        self.last_render_end = Some(Instant::now());
    }

    /// The phase's per-frame action, decided before the elements are built.
    fn action(&mut self) -> Action {
        match self.phase {
            Phase::Scroll => {
                self.scroll_top = (self.scroll_top + 3) % (TOTAL_LINES - 1_000);
                Action::ScrollTo(self.scroll_top)
            }
            Phase::Type => {
                self.edit_at = Some(Instant::now());
                Action::Edit
            }
            _ => Action::Still,
        }
    }

    /// Runs at the start of each frame, i.e. after the previous frame was
    /// submitted. This is where paint and interval are attributed, and where the
    /// phase advances.
    fn next_frame(&mut self, now: Instant) {
        if let (Some(frame), Some(end)) = (self.frames.last_mut(), self.last_render_end) {
            frame.paint_us = Some(micros(now.saturating_duration_since(end)));
            if let Some(previous) = self.last_next_frame {
                frame.interval_us = Some(micros(now.saturating_duration_since(previous)));
            }
        }
        self.last_next_frame = Some(now);

        if self.first_frame_us.is_none() {
            self.first_frame_us = Some(micros(now.saturating_duration_since(self.t0)));
        }
        if let Some(edit_at) = self.edit_at.take() {
            self.edit_us
                .push(micros(now.saturating_duration_since(edit_at)));
        }

        self.phase_frames += 1;
        let budget = match self.phase {
            Phase::Warmup => WARMUP_FRAMES,
            Phase::Steady => STEADY_FRAMES,
            Phase::Scroll => SCROLL_FRAMES,
            Phase::Type => TYPE_FRAMES,
            Phase::Done => usize::MAX,
        };
        if self.phase_frames < budget {
            return;
        }
        self.phase_frames = 0;
        self.phase = match self.phase {
            Phase::Warmup => Phase::Steady,
            Phase::Steady => Phase::Scroll,
            Phase::Scroll => Phase::Type,
            Phase::Type | Phase::Done => Phase::Done,
        };
    }
}

enum Action {
    Still,
    ScrollTo(usize),
    Edit,
}

struct Bench {
    run: Rc<RefCell<Run>>,
    scroll: UniformListScrollHandle,
    /// The row the typing phase replaces, so an edit changes real text and only
    /// one row's shaped line has to be rebuilt.
    edited: Option<(usize, SharedString)>,
    /// What that line says.
    typed: String,
}

impl Bench {
    fn new(cx: &mut Context<Self>) -> Self {
        let run = Rc::new(RefCell::new(Run::new()));
        // Rows are generated on demand rather than materialised: a real editor
        // reads a rope, and building 100k strings eagerly is a startup cost of the
        // harness that would otherwise be reported as gpui's first paint.
        let entered = micros(run.borrow().t0.elapsed());
        let app = APP_STARTED.get().copied().flatten().unwrap_or(entered);
        let bench = Self {
            run: Rc::clone(&run),
            scroll: UniformListScrollHandle::new(),
            edited: None,
            typed: "fn main() {".to_string(),
        };
        let built = micros(run.borrow().t0.elapsed());
        {
            let mut run = run.borrow_mut();
            run.app_us = Some(app);
            run.window_us = Some(entered);
            run.view_us = Some(built);
        }
        let _ = cx;
        bench
    }
}

impl Render for Bench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let start = self.run.borrow_mut().begin_frame();
        let action = self.run.borrow_mut().action();
        let (scroll_to, edit) = match action {
            Action::Still => (None, false),
            Action::ScrollTo(index) => (Some(index), false),
            Action::Edit => (None, true),
        };
        if let Some(index) = scroll_to {
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
        }
        if edit {
            // One character per frame, in the line nearest the viewport centre:
            // the edit forces that row's shaped line to be rebuilt.
            let at = (self.run.borrow().scroll_top + 20).min(TOTAL_LINES - 1);
            self.typed
                .push(if self.typed.len() % 2 == 0 { 'x' } else { 'y' });
            self.edited = Some((
                at,
                SharedString::from(format!("{}{}", self.typed, " ".repeat(20))),
            ));
        }

        let rows = Rc::clone(&self.run);
        let edited = self.edited.clone();
        let list = uniform_list("rows", TOTAL_LINES, move |range, _window, _cx| {
            rows.borrow_mut().rows_rendered = range.len();
            range
                .map(|index| {
                    let text = match &edited {
                        Some((at, text)) if *at == index => text.clone(),
                        _ => line_text(index),
                    };
                    div().h(px(18.0)).text_size(px(13.0)).child(text)
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(self.scroll.clone())
        .size_full();

        let total_frames = self.run.borrow().frames.len();
        let finish_at = WARMUP_FRAMES + STEADY_FRAMES + SCROLL_FRAMES + TYPE_FRAMES;
        // One extra frame after the last measurement, so its paint and interval
        // are attributed before the report is printed.
        let finishing = total_frames > finish_at;
        let run = Rc::clone(&self.run);
        window.on_next_frame(move |window, _cx| {
            let mut state = run.borrow_mut();
            // This is the only place a frame's paint and interval can be
            // attributed: it runs at the start of the next frame, after the
            // previous one was submitted.
            state.next_frame(Instant::now());
            if finishing {
                let shape = shape_probe(window, SHAPE_LINES);
                state.shape_us = Some(shape.0);
                state.shaping_font = Some(shape.1);
                let started = state.t0;
                report(&state, started);
                let _ = std::io::stdout().flush();
                std::process::exit(0);
            }
        });
        // Frame pacing only advances when the app asks for another frame, and
        // this must be called while a view is rendering: `request_animation_frame`
        // resolves `current_view()` internally.
        window.request_animation_frame();

        let _ = self.run.borrow_mut().end_frame(start);
        let _ = cx;
        div()
            .size_full()
            .font_family("Consolas")
            .child(list)
            .into_any_element()
    }
}

/// Shaping cost, measured without a render in the way: this is the per-line
/// `text_system` work the frame cost already contains, isolated so a regression
/// can be attributed to shaping or to paint.
fn shape_probe(window: &mut Window, count: usize) -> (u64, String) {
    let text_system = window.text_system().clone();
    let font = font("Consolas");
    let run = gpui::TextRun {
        len: 0,
        font: font.clone(),
        color: hsla(0.0, 0.0, 0.1, 1.0),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    // The text is built before the clock starts: this measures shaping, not
    // `format!`. Cold on purpose — an editor's cache is the other half, and a
    // warm loop would measure the cache.
    let lines = make_lines(count);
    let start = Instant::now();
    let mut shaped = 0usize;
    for line in lines {
        let mut run = run.clone();
        run.len = line.len();
        let _ = text_system.shape_line(line, px(13.0), &[run], None);
        shaped += 1;
    }
    debug_assert_eq!(shaped, count);
    (micros(start.elapsed()), font.family.to_string())
}

/// One row of the document, generated rather than stored. Ninety-odd characters
/// of representative Rust: identifiers, generics, punctuation, and a comment.
fn line_text(index: usize) -> SharedString {
    SharedString::from(format!(
        "    let item_{index}: Option<Arc<HashMap<Key, Value>>> = \
         cache.get(&key).map(|entry| entry.clone()); // line {index}"
    ))
}

fn make_lines(count: usize) -> Vec<SharedString> {
    (0..count).map(line_text).collect()
}

fn micros(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn percentile(values: &[u64], percent: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = ((sorted.len() - 1) as f64 * percent).round() as usize;
    sorted[rank] as f64 / 1000.0
}

fn ms(values: &[u64]) -> String {
    format!(
        "{{\"p50\": {:.2}, \"p95\": {:.2}, \"max\": {:.2}}}",
        percentile(values, 0.50),
        percentile(values, 0.95),
        percentile(values, 1.0)
    )
}

fn report(run: &Run, started: Instant) {
    let frames = &run.frames[WARMUP_FRAMES..];
    let steady_end = STEADY_FRAMES;
    let scroll_end = steady_end + SCROLL_FRAMES;
    let steady = &frames[..steady_end.min(frames.len())];
    let scroll = &frames[steady_end.min(frames.len())..scroll_end.min(frames.len())];

    let builds = |slice: &[Frame]| slice.iter().map(|frame| frame.build_us).collect::<Vec<_>>();
    let paints = |slice: &[Frame]| {
        slice
            .iter()
            .filter_map(|frame| frame.paint_us)
            .collect::<Vec<_>>()
    };
    let intervals = |slice: &[Frame]| {
        slice
            .iter()
            .filter_map(|frame| frame.interval_us)
            .collect::<Vec<_>>()
    };

    let all_intervals = intervals(frames);
    let mut sorted = all_intervals.clone();
    sorted.sort_unstable();
    // A dropped frame is one that missed a vsync period, so the reference is the
    // median period rather than the fastest one: a jittery-but-keeping-up stream
    // has intervals below the nominal period, and using the minimum would call
    // normal 240 Hz pacing a drop.
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(1).max(1);
    let dropped = all_intervals
        .iter()
        .filter(|&&i| i > median * 3 / 2)
        .count();

    println!(
        concat!(
            "{{\"gpui\": \"0.2.2\", \"lines\": {}, \"shape_lines\": {}, \"rows_rendered\": {}, ",
            "\"app_ms\": {:.2}, \"window_ms\": {:.2}, \"view_ms\": {:.2}, \"first_render_ms\": {:.2}, \"first_frame_ms\": {:.2}, ",
            "\"steady\": {{\"n\": {}, \"build\": {}, \"paint\": {}, \"interval\": {}}}, ",
            "\"scroll\": {{\"n\": {}, \"build\": {}, \"paint\": {}, \"interval\": {}}}, ",
            "\"edit_to_frame\": {{\"n\": {}, \"ms\": {}}}, ",
            "\"duration_ms\": {:.0}, \"median_interval_ms\": {:.2}, \"dropped_intervals\": {}, ",
            "\"shape_{}_ms\": {:.2}, \"font\": \"{}\"}}"
        ),
        TOTAL_LINES,
        SHAPE_LINES,
        run.rows_rendered,
        run.app_us.unwrap_or(0) as f64 / 1000.0,
        run.window_us.unwrap_or(0) as f64 / 1000.0,
        run.view_us.unwrap_or(0) as f64 / 1000.0,
        run.first_render_us.unwrap_or(0) as f64 / 1000.0,
        run.first_frame_us.unwrap_or(0) as f64 / 1000.0,
        steady.len(),
        ms(&builds(steady)),
        ms(&paints(steady)),
        ms(&intervals(steady)),
        scroll.len(),
        ms(&builds(scroll)),
        ms(&paints(scroll)),
        ms(&intervals(scroll)),
        run.edit_us.len(),
        ms(&run.edit_us),
        started.elapsed().as_millis(),
        median as f64 / 1000.0,
        dropped,
        SHAPE_LINES,
        run.shape_us.unwrap_or(0) as f64 / 1000.0,
        run.shaping_font.clone().unwrap_or_default(),
    );
}

fn main() {
    let started = *STARTED.get_or_init(Instant::now);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(40.0), px(40.0)),
            size: GpuiSize {
                width: px(1_400.0),
                height: px(900.0),
            },
        })),
        focus: true,
        show: true,
        ..Default::default()
    };
    let _ = started;

    Application::new().run(move |cx: &mut App| {
        let app_us = micros(
            STARTED
                .get()
                .copied()
                .unwrap_or_else(Instant::now)
                .elapsed(),
        );
        APP_STARTED.set(Some(app_us)).ok();
        cx.open_window(options, |_window, cx| cx.new(Bench::new))
            .expect("open the benchmark window");
        // The first frame is drawn by opening the window; `render` schedules the
        // ones after it, where a view is current.
    });
}
