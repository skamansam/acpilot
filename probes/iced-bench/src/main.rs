//! P0-3's other half: the same screen, in `iced`, so the first-paint number has
//! something to be compared against instead of being a verdict.
//!
//! ```bash
//! cargo run --release          # the benchmark
//! cargo run --release -- font  # one timed paragraph, no window
//! ```
//!
//! The instrument is deliberately the same as `probes/gpui-bench`: a 100k-line
//! document in a 1400 × 900 window, 50 rows of 18 px visible, 13 px monospace,
//! three phases (steady, scroll, type) after 30 warm-up frames, one cold shaping
//! probe of 3 000 lines, and the same three parts of "first paint". Anything that
//! differs is called out in the report and in `docs/notes/gui-bench.md`; the
//! point of a baseline is that the numbers are comparable, not that the code is.
//!
//! Two differences are structural and are not the harness's choice:
//!
//! * iced has no `uniform_list`. The rows are virtualised by hand — a spacer of
//!   the scrolled-past height, the ~50 visible rows, a spacer of the rest — and
//!   driven with `scrollable::scroll_to`. Drawing 100k widgets was the mistake
//!   the gpui harness exists to rule out, so it is not repeated here.
//! * iced's frame clock is `window::frames()`, a subscription that yields the
//!   `Instant` stamped when `RedrawRequested` handling *begins* — before iced
//!   builds that frame. `view()` is where the build finishes and the next tick
//!   closes the interval, but there is no paint boundary to report: iced has no
//!   public "frame presented" callback, so `edit_to_view` (the app-side half of
//!   keystroke→paint) is reported instead of `edit_to_frame`. The note says what
//!   that costs the comparison.
//!
//! Prints one JSON object and exits 0. Nothing is written to disk.

use std::cell::RefCell;
use std::hint::black_box;
use std::io::Write;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Instant;

use iced::advanced::graphics::text::Paragraph;
use iced::advanced::text::{Alignment as TextAlignment, LineHeight, Shaping, Text, Wrapping};
use iced::alignment;
use iced::widget::operation::{AbsoluteOffset, scroll_to};
use iced::widget::{Column, Id, container, scrollable, space, text};
use iced::{Element, Font, Length, Pixels, Point, Size, Subscription, Task, Theme, window};

/// Process start, taken before iced's runtime is constructed so "first paint"
/// includes start-up rather than only the first frame.
static STARTED: OnceLock<Instant> = OnceLock::new();

const TOTAL_LINES: usize = 100_000;
/// §Perf's "3k-line visible window": the scroll-back cache, not the viewport.
const SHAPE_LINES: usize = 3_000;

/// gpui's row is `div().h(px(18.0))`; the same height here, so 900 px of window
/// is the same 50 rows.
const ROW_HEIGHT: f32 = 18.0;
const ROW_FONT_SIZE: f32 = 13.0;
const VISIBLE_ROWS: usize = 50;
const WINDOW_SIZE: (f32, f32) = (1_400.0, 900.0);

const WARMUP_FRAMES: usize = 30;
const STEADY_FRAMES: usize = 120;
const SCROLL_FRAMES: usize = 60;
const TYPE_FRAMES: usize = 40;
const MEASURED_FRAMES: usize = WARMUP_FRAMES + STEADY_FRAMES + SCROLL_FRAMES + TYPE_FRAMES;

/// Consolas, like the gpui bench: both harnesses ask the system for the same
/// face, so glyph metrics are not a difference between the two numbers.
const FONT_NAME: &str = "Consolas";

fn row_font() -> Font {
    Font {
        family: iced::font::Family::Name(FONT_NAME),
        ..Font::DEFAULT
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Warmup,
    Steady,
    Scroll,
    Type,
}

#[derive(Default)]
struct Frame {
    /// Time spent building this frame's widget tree, i.e. the `view()` body.
    build_us: u64,
    /// Tick to tick. A vsync miss is a long interval, not a slow frame.
    interval_us: Option<u64>,
}

struct Run {
    t0: Instant,
    frames: Vec<Frame>,
    phase: Phase,
    phase_frames: usize,
    last_tick: Option<Instant>,
    /// Frame boundaries seen. One `view()` is expected per tick; this guards
    /// against a runtime that builds the view more than once per frame, which
    /// would otherwise make the phase counters and the frame vector disagree.
    ticks: usize,
    first_frame_us: Option<u64>,
    /// Process start to iced's boot callback, to the window existing, and to the
    /// first `view()`. The same three parts gpui reports, so a difference can be
    /// attributed to a part rather than to "start-up".
    app_us: Option<u64>,
    window_us: Option<u64>,
    view_us: Option<u64>,
    first_render_us: Option<u64>,
    scroll_top: usize,
    edit_at: Option<Instant>,
    edit_us: Vec<u64>,
    rows_rendered: usize,
    shape_us: Option<u64>,
    finished: bool,
}

impl Run {
    fn new(t0: Instant) -> Self {
        Self {
            t0,
            frames: Vec::with_capacity(MEASURED_FRAMES + 8),
            phase: Phase::Warmup,
            phase_frames: 0,
            last_tick: None,
            ticks: 0,
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
            finished: false,
        }
    }

    /// A frame boundary. `window::frames()` yields the `Instant` of a
    /// `RedrawRequested`, so this runs after the previous frame was submitted
    /// and before this one is built.
    fn tick(&mut self, at: Instant) {
        if let Some(frame) = self.frames.last_mut() {
            // iced's tick is stamped at the *start* of handling
            // `RedrawRequested`, before the frame is built, and there is no
            // public "frame presented" callback — so this reports pacing, not
            // paint. `docs/notes/gui-bench.md` says what that costs.
            if let Some(previous) = self.last_tick {
                frame.interval_us = Some(micros(at.saturating_duration_since(previous)));
            }
        }
        self.last_tick = Some(at);
        self.ticks += 1;
        if self.first_frame_us.is_none() {
            self.first_frame_us = Some(micros(at.saturating_duration_since(self.t0)));
        }
    }

    fn advance(&mut self) {
        self.phase_frames += 1;
        let budget = match self.phase {
            Phase::Warmup => WARMUP_FRAMES,
            Phase::Steady => STEADY_FRAMES,
            Phase::Scroll => SCROLL_FRAMES,
            Phase::Type => TYPE_FRAMES,
        };
        if self.phase_frames < budget {
            return;
        }
        self.phase_frames = 0;
        self.phase = match self.phase {
            Phase::Warmup => Phase::Steady,
            Phase::Steady => Phase::Scroll,
            Phase::Scroll => Phase::Type,
            Phase::Type => Phase::Type,
        };
    }
}

#[derive(Debug, Clone)]
enum Message {
    /// One redraw, with the instant the windowing system requested it.
    Tick(Instant),
    Window(window::Event),
}

struct Bench {
    run: Rc<RefCell<Run>>,
    scroll: Id,
    /// One character per frame is edited into this row, so exactly one row's
    /// shaped line has to be rebuilt.
    edited: Option<(usize, String)>,
    typed: String,
}

impl Bench {
    /// Rows are generated from their index, not materialised: 100k eager strings
    /// are a start-up cost of the harness, and in the gpui run that mistake
    /// added 724 ms to a number that would have been read as the framework's.
    fn row(&self, index: usize) -> Element<'_, Message> {
        let content = match &self.edited {
            Some((at, line)) if *at == index => line.clone(),
            _ => line_text(index),
        };
        container(
            text(content)
                .size(ROW_FONT_SIZE)
                .font(row_font())
                .line_height(LineHeight::Absolute(Pixels::from(ROW_HEIGHT))),
        )
        .height(Length::Fixed(ROW_HEIGHT))
        .width(Length::Fill)
        .into()
    }
}

fn view(state: &Bench) -> Element<'_, Message> {
    let started = Instant::now();
    let scroll_top = state.run.borrow().scroll_top;

    let first = scroll_top.min(TOTAL_LINES - VISIBLE_ROWS);
    let last = first + VISIBLE_ROWS;
    let top = first as f32 * ROW_HEIGHT;
    let bottom = (TOTAL_LINES - last) as f32 * ROW_HEIGHT;

    let mut children = Vec::with_capacity(VISIBLE_ROWS + 2);
    children.push(space::vertical().height(Length::Fixed(top)).into());
    children.extend((first..last).map(|index| state.row(index)));
    children.push(space::vertical().height(Length::Fixed(bottom)).into());

    let list = scrollable(Column::with_children(children))
        .id(state.scroll.clone())
        .height(Length::Fill)
        .width(Length::Fill);

    // `black_box` keeps the layout from being optimised away: it is the work the
    // budget is about, and a release build must not decide it is dead.
    let built = Instant::now();
    {
        let mut run = state.run.borrow_mut();
        if run.first_render_us.is_none() {
            run.first_render_us = Some(micros(built.saturating_duration_since(run.t0)));
        }
        if run.view_us.is_none() {
            run.view_us = Some(micros(built.saturating_duration_since(run.t0)));
        }
        run.rows_rendered = last - first;
        if let Some(edit_at) = run.edit_at.take() {
            run.edit_us
                .push(micros(built.saturating_duration_since(edit_at)));
        }
        if run.frames.len() <= run.ticks {
            run.frames.push(Frame {
                build_us: micros(built.saturating_duration_since(started)),
                ..Frame::default()
            });
        }
    }
    container(black_box(list))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn update(state: &mut Bench, message: Message) -> Task<Message> {
    match message {
        Message::Window(event) => {
            if matches!(event, window::Event::Opened { .. }) {
                let mut run = state.run.borrow_mut();
                if run.window_us.is_none() {
                    run.window_us = Some(micros(Instant::now().saturating_duration_since(run.t0)));
                }
            }
            Task::none()
        }
        Message::Tick(at) => {
            let mut run = state.run.borrow_mut();
            run.tick(at);
            if run.frames.len() > MEASURED_FRAMES {
                if !run.finished {
                    run.finished = true;
                    // After the last measured frame, so the probe's cold shaping
                    // is not competing with a frame for the same CPU.
                    run.shape_us = Some(shape_probe(SHAPE_LINES));
                    finish(&run);
                }
                return Task::none();
            }
            run.advance();
            match run.phase {
                Phase::Scroll => {
                    run.scroll_top = (run.scroll_top + 3) % (TOTAL_LINES - 1_000);
                    scroll_to(
                        state.scroll.clone(),
                        AbsoluteOffset {
                            x: 0.0,
                            y: run.scroll_top as f32 * ROW_HEIGHT,
                        },
                    )
                }
                Phase::Type => {
                    run.edit_at = Some(Instant::now());
                    state.typed.push(if state.typed.len().is_multiple_of(2) {
                        'x'
                    } else {
                        'y'
                    });
                    let at = (run.scroll_top + 20).min(TOTAL_LINES - 1);
                    state.edited = Some((at, format!("{}{}", state.typed, " ".repeat(20))));
                    Task::none()
                }
                Phase::Warmup | Phase::Steady => Task::none(),
            }
        }
    }
}

/// Shaping, measured without a render in the way: the per-line paragraph work
/// the frame cost already contains, isolated so a regression can be attributed
/// to text or to paint.
///
/// The gpui probe timed `TextSystem::shape_line`; this times
/// `Paragraph::with_text`, which is shaping **and layout** for one line. The
/// number is therefore an upper bound on the comparison rather than a like-for-
/// like, and the note says so.
fn shape_probe(count: usize) -> u64 {
    let lines = make_lines(count);
    // Warm the global font system first: cosmic-text's first paragraph loads the
    // system font database, and that is start-up cost, not shaping cost. The
    // `font` mode measures the load on its own.
    let _ = black_box(shape_one("warm"));
    let start = Instant::now();
    for line in &lines {
        let paragraph = black_box(shape_one(line));
        debug_assert!(!paragraph.buffer().lines.is_empty());
    }
    micros(start.elapsed())
}

fn shape_one(content: &str) -> Paragraph {
    <Paragraph as iced::advanced::text::Paragraph>::with_text(Text {
        content,
        bounds: Size::new(WINDOW_SIZE.0, ROW_HEIGHT),
        size: Pixels::from(ROW_FONT_SIZE),
        line_height: LineHeight::Absolute(Pixels::from(ROW_HEIGHT)),
        font: row_font(),
        align_x: TextAlignment::Default,
        align_y: alignment::Vertical::Top,
        shaping: Shaping::Advanced,
        wrapping: Wrapping::None,
    })
}

/// One row of the document, generated rather than stored — the same line the
/// gpui harness draws, so both are shaping the same characters.
fn line_text(index: usize) -> String {
    format!(
        "    let item_{index}: Option<Arc<HashMap<Key, Value>>> = \
         cache.get(&key).map(|entry| entry.clone()); // line {index}"
    )
}

fn make_lines(count: usize) -> Vec<String> {
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

fn finish(run: &Run) {
    let frames = &run.frames[WARMUP_FRAMES.min(run.frames.len())..];
    let steady_end = STEADY_FRAMES.min(frames.len());
    let scroll_end = (steady_end + SCROLL_FRAMES).min(frames.len());
    let steady = &frames[..steady_end];
    let scroll = &frames[steady_end..scroll_end];

    let builds = |slice: &[Frame]| slice.iter().map(|frame| frame.build_us).collect::<Vec<_>>();
    let intervals = |slice: &[Frame]| {
        slice
            .iter()
            .filter_map(|frame| frame.interval_us)
            .collect::<Vec<_>>()
    };

    let all_intervals = intervals(frames);
    let mut sorted = all_intervals.clone();
    sorted.sort_unstable();
    // Relative to the median period, not the fastest: a jittery stream has
    // intervals below the nominal period, and the minimum would call normal
    // 240 Hz pacing a drop.
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(1).max(1);
    let dropped = all_intervals
        .iter()
        .filter(|&&i| i > median * 3 / 2)
        .count();

    println!(
        concat!(
            "{{\"iced\": \"0.14\", \"lines\": {}, \"shape_lines\": {}, \"rows_rendered\": {}, ",
            "\"app_ms\": {:.2}, \"window_ms\": {:.2}, \"view_ms\": {:.2}, \"first_render_ms\": {:.2}, \"first_frame_ms\": {:.2}, ",
            "\"steady\": {{\"n\": {}, \"build\": {}, \"interval\": {}}}, ",
            "\"scroll\": {{\"n\": {}, \"build\": {}, \"interval\": {}}}, ",
            "\"edit_to_view\": {{\"n\": {}, \"ms\": {}}}, ",
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
        ms(&intervals(steady)),
        scroll.len(),
        ms(&builds(scroll)),
        ms(&intervals(scroll)),
        run.edit_us.len(),
        ms(&run.edit_us),
        run.t0.elapsed().as_millis(),
        median as f64 / 1000.0,
        dropped,
        SHAPE_LINES,
        run.shape_us.unwrap_or(0) as f64 / 1000.0,
        FONT_NAME,
    );
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

fn dark(_state: &Bench) -> Theme {
    Theme::Dark
}

fn main() {
    let started = *STARTED.get_or_init(Instant::now);

    // The font database is shared by every paragraph in the process, and it is
    // loaded on the first one. Measuring it here — in a process with no window —
    // is the only way to say how much of a first paint is font enumeration
    // without moving that cost into the benchmark's own start-up.
    if std::env::args().any(|argument| argument == "font") {
        let start = Instant::now();
        let paragraph = shape_one(
            "fn dark(_state: &Bench) -> Theme {
    Theme::Dark
}

fn main() {}",
        );
        let elapsed = micros(start.elapsed());
        println!(
            "{{\"font_load_and_shape_ms\": {:.2}, \"lines\": {}, \"font\": \"{}\"}}",
            elapsed as f64 / 1000.0,
            black_box(paragraph.buffer().lines.len()),
            FONT_NAME,
        );
        let _ = std::io::stdout().flush();
        return;
    }

    let run = Rc::new(RefCell::new(Run::new(started)));

    iced::application(
        move || {
            let mut state = run.borrow_mut();
            state.app_us = Some(micros(Instant::now().saturating_duration_since(started)));
            drop(state);
            let bench = Bench {
                run: Rc::clone(&run),
                scroll: Id::unique(),
                edited: None,
                typed: "fn dark(_state: &Bench) -> Theme {
    Theme::Dark
}

fn main() {"
                    .to_string(),
            };
            (bench, Task::none())
        },
        update,
        view,
    )
    .window(window::Settings {
        size: Size::new(WINDOW_SIZE.0, WINDOW_SIZE.1),
        position: window::Position::Specific(Point::new(40.0, 40.0)),
        ..window::Settings::default()
    })
    .subscription(|_state| {
        Subscription::batch([
            window::frames().map(Message::Tick),
            window::events().map(|(_id, event)| Message::Window(event)),
        ])
    })
    .theme(dark)
    .run()
    .expect("run the iced benchmark");
}
