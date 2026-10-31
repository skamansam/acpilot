//! What one frame costs at a given size, measured rather than assumed.
//!
//! `cargo run --release -p acpilot-tui --example frame -- 200 60 500`
//!
//! The design pass's world at the size P6-12 sets a budget for: the tree, the
//! editor with its own buffer and parse, the session list, the transcript, the
//! composer and both footer rows. It draws into a `TestBackend` rather than a
//! terminal, so the number is the drawing — the app, the viewports, every styled
//! span — and not the tty's own write.
//!
//! Two things make the number honest. The app is handed each frame's viewports
//! back, exactly as the event loop hands them back, because the editor measures
//! itself against the cells it was given and would otherwise be drawing into a
//! stale window. And warm-up frames are not counted: the first one parses the
//! seeded file and allocates every row's cells, which is a launch cost rather than
//! a frame cost.
//!
//! `--budget <ms>` is the CI gate: the numbers are printed either way, and then the
//! run exits `1` if the mean or the 95th percentile missed the budget. A worst frame
//! is printed but not gated — a shared runner produces one occasionally, and one
//! scheduler hiccup is not a regression, while a sustained doubling is.

use std::time::{Duration, Instant};

use acpilot_tui::app::App;
use acpilot_tui::ui;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// How many frames are drawn before the clock starts.
const WARM_UP: usize = 10;

/// One frame at 60 fps, in milliseconds.
const BUDGET: f64 = 1000.0 / 60.0;

fn main() {
    let Arguments {
        cols,
        rows,
        frames,
        budget,
    } = Arguments::parse();

    let demo = acpilot_demo::demo();
    let mut app = App::demo(&demo);
    let mut terminal = Terminal::new(TestBackend::new(cols, rows)).expect("a test terminal");

    for _ in 0..WARM_UP {
        draw(&mut terminal, &mut app);
    }

    let mut times: Vec<Duration> = Vec::with_capacity(frames);
    for _ in 0..frames {
        let start = Instant::now();
        draw(&mut terminal, &mut app);
        times.push(start.elapsed());
    }
    times.sort_unstable();

    let mean = times.iter().sum::<Duration>() / u32::try_from(frames).unwrap_or(u32::MAX);
    let p95 = times[(frames * 95 / 100).min(frames - 1)];
    let worst = *times.last().expect("at least one frame");

    let mean = milliseconds(mean);
    let p95 = milliseconds(p95);
    let worst = milliseconds(worst);
    println!("{cols}x{rows} · {frames} frames · demo world");
    println!("  mean   {mean:>7.3} ms");
    println!("  p95    {p95:>7.3} ms");
    println!("  worst  {worst:>7.3} ms");
    println!(
        "  budget {BUDGET:>7.1} ms/frame · the worst frame is {:.0}% of it",
        worst / BUDGET * 100.0
    );

    // The gate, when one was asked for: a frame that does not fit is the bug the
    // budget exists to name, so it is a red run rather than a number nobody reads.
    if let Some(budget) = budget {
        if mean > budget || p95 > budget {
            eprintln!(
                "frame budget missed: mean {mean:.3} ms, p95 {p95:.3} ms against {budget:.1} ms"
            );
            std::process::exit(1);
        }
        println!("  within {budget:.1} ms");
    }
}

/// What the command line asked for.
struct Arguments {
    /// How wide the terminal is.
    cols: u16,
    /// How tall it is.
    rows: u16,
    /// How many frames to time, after the warm-up ones.
    frames: usize,
    /// The budget to hold the run to, in milliseconds, when one was given.
    budget: Option<f64>,
}

impl Arguments {
    /// Three optional sizes and an optional `--budget <ms>`, in any order.
    ///
    /// Anything that is neither is ignored, so the bare `cargo run ... -- 200 60`
    /// stays the whole of what most runs need.
    fn parse() -> Self {
        let mut sizes: Vec<u16> = Vec::new();
        let mut budget = None;
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            if let Some(value) = argument.strip_prefix("--budget=") {
                budget = value.parse().ok();
                continue;
            }
            if argument == "--budget" {
                budget = arguments.next().and_then(|value| value.parse().ok());
                continue;
            }
            if let Ok(value) = argument.parse::<u16>() {
                sizes.push(value);
            }
        }
        Self {
            cols: sizes.first().copied().unwrap_or(200).clamp(20, 500),
            rows: sizes.get(1).copied().unwrap_or(60).clamp(5, 200),
            frames: sizes
                .get(2)
                .map_or(200, |frames| usize::from((*frames).max(10))),
            budget,
        }
    }
}

/// Draw one frame and hand the viewports back, as the event loop does.
fn draw(terminal: &mut Terminal<TestBackend>, app: &mut App) {
    let mut viewports = None;
    terminal
        .draw(|frame| viewports = Some(ui::draw(frame, app)))
        .expect("a frame");
    if let Some(viewports) = viewports {
        app.set_viewports(viewports);
    }
}

/// A duration in milliseconds, for reading.
fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
