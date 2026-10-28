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
    let mut sizes = std::env::args()
        .skip(1)
        .filter_map(|argument| argument.parse::<u16>().ok());
    let cols = sizes.next().unwrap_or(200).clamp(20, 500);
    let rows = sizes.next().unwrap_or(60).clamp(5, 200);
    let frames = sizes
        .next()
        .map_or(200, |frames| usize::from(frames.max(10)));

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

    println!("{cols}x{rows} · {frames} frames · demo world");
    println!("  mean   {:>7.3} ms", milliseconds(mean));
    println!("  p95    {:>7.3} ms", milliseconds(p95));
    println!("  worst  {:>7.3} ms", milliseconds(worst));
    println!(
        "  budget {BUDGET:>7.1} ms/frame · the worst frame is {:.0}% of it",
        milliseconds(worst) / BUDGET * 100.0
    );
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
