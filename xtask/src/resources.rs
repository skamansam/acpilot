//! `cargo xtask resources`: CPU and memory for the programs a release ships.
//!
//! Cross-cutting task XT-4. A figure without its window and its machine is a figure
//! about the machine, so every row names the load it measured and the note records
//! the hardware. The harness lives here rather than in a shipping crate: sampling a
//! process tree is a syscall surface, and a workflow is the only caller.
//!
//! Idle is a load, not the absence of one. The daemon's idle row holds one
//! `/v1/events` subscriber open for the whole window, because the profile-wide stream
//! is what a connected client costs when nobody is typing: a pump that wakes sixty
//! times a second shows up as CPU and nowhere else. The session row is the same
//! daemon with one session created over the API and its mock agent running, read on
//! the daemon's own process — the agent is a separate program, and its memory is not
//! the session's to budget. CPU is a fraction of *one core*, summed over the tree,
//! because a percentage of the machine hides the second core a busy loop costs. The
//! TUI row runs the design pass under a real pseudo-terminal, because a client that
//! cannot put its terminal into raw mode refuses to launch; the demo reads no project
//! and contacts no daemon, so the figure is the frame clock rather than a session's
//! traffic.
//!
//! What is not measured yet, and why, is printed at the end of a run and written into
//! the note: the desktop shell needs a window (P10), the page needs a browser (P11), a
//! language server needs one installed (P7), and `acpilotd-helper` has no build on
//! this host until P9. The rows arrive with their phases; the harness is
//! cross-cutting so the first regression is reported while there is still time to fix
//! it.
//!
//! `--gate` is the version CI runs. It measures the same rows, prints every figure
//! beside its budget and the limit the margin gives it, and exits non-zero when one is
//! past that limit — or when a row is measured without a budget, because a row outside
//! the list is a row the gate silently stopped checking. The budgets are release
//! figures, so a debug build is refused rather than compared with them.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle, sleep};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::{App, build, path_in_repo};

/// How often the process tree is sampled.
const SAMPLE: Duration = Duration::from_millis(100);
/// The daemon's window: long enough for a first sweep and several health polls.
const IDLE_WINDOW: Duration = Duration::from_secs(3);
/// How long the harness waits for the daemon's first health answer.
const STARTUP: Duration = Duration::from_secs(15);
/// The longest a one-shot row may run before the harness gives up on it.
const ONE_SHOT_WINDOW: Duration = Duration::from_secs(30);
/// Where the numbers and the machine they came from are written.
const NOTE: &str = "docs/notes/resources.md";
/// The daemon alone, with one subscriber: the row a session is measured against.
const IDLE_ROW: &str = "acpilotd";
/// The daemon plus one idle session, as the session row is named.
///
/// Not `acpilotd` again: the gate keys a figure by its row, and two rows under one
/// name would let one of them stand in for the other.
const SESSION_ROW: &str = "acpilotd + 1 session";
/// The daemon with one prompt turn in it: the path a client's keystroke takes.
///
/// Not a CLI row: the `acpilot` binary has no `prompt` verb, and the clients that do
/// start a turn are the TUI and anything speaking the API. What the turn costs the
/// process that serves it is this row's figure.
const TURN_ROW: &str = "acpilotd + 1 turn";
/// The terminal UI drawing its seeded world: the client's frame clock.
const TUI_ROW: &str = "acpilot tui";
/// The TUI's window: long enough for several hundred frames at its clock.
const TUI_WINDOW: Duration = Duration::from_secs(5);
/// `acpilot serve` with one page attached: the CLI's long-lived row.
const SERVE_ROW: &str = "acpilot serve";
/// The table's budget for one idle session: what a session may add to the daemon.
///
/// A hard limit rather than a measured median, because it is a number the product
/// states rather than one this harness discovered — so it has no margin. The row's
/// own memory budget is measured like every other row's.
const SESSION_MIB: f64 = 15.0;

/// One measured row, after the repeats have been reduced to a median.
#[derive(Debug, Clone)]
struct Figure {
    row: &'static str,
    window: &'static str,
    rss_peak: u64,
    cpu_mean: Option<f32>,
    cpu_peak: Option<f32>,
    threads: usize,
}

/// Measure every row this build can reach, three times each, and write the note — or,
/// with `gate`, hold the medians to the budgets instead of recording them.
pub(crate) fn run(debug: bool, repeats: u32, gate: bool) -> bool {
    match measure_all(debug, repeats, gate) {
        Ok(passed) => passed,
        Err(error) => {
            eprintln!("==> resources: {error}");
            false
        }
    }
}

fn measure_all(debug: bool, repeats: u32, gate: bool) -> Result<bool, String> {
    let release = !debug;
    if gate && debug {
        return Err(
            "the budgets are release figures; `--gate` refuses a debug build rather than \
             comparing a slower program with them"
                .to_owned(),
        );
    }
    if gate && repeats < 3 {
        return Err(format!(
            "the gate compares a median of three runs, and {repeats} repeat(s) is not one"
        ));
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    if !build(App::Tui, release) {
        return Err("the binaries this row measures could not be built".to_owned());
    }
    if !build_mock_agent(release) {
        return Err("the fixture agent a session row starts could not be built".to_owned());
    }

    let target = target_dir(&root, release);
    let profile = std::env::temp_dir().join(format!("acpilot-resources-{}", std::process::id()));
    let workspace = profile.join("workspace");
    fs::create_dir_all(&workspace).map_err(|error| format!("{}: {error}", workspace.display()))?;
    fs::write(workspace.join("README.md"), "# resources\n")
        .map_err(|error| format!("{}: {error}", workspace.display()))?;
    // A profile is a directory with a `config.toml`; the daemon refuses to load one
    // without it, and an empty file is a profile with every default.
    fs::write(profile.join("config.toml"), "")
        .map_err(|error| format!("{}: {error}", profile.join("config.toml").display()))?;
    // One agent, so the session row has something to start. The command is the
    // fixture binary `cargo build -p acpilot-agent` writes beside the daemon, which
    // is why the row puts that directory first on the daemon's `PATH`.
    fs::write(
        profile.join("agents.toml"),
        r#"schema_version = 1

[[agents]]
id = "mock"
name = "Mock"
description = "The fixture agent a session row starts."
transport = "acp-stdio"
command = "acpilot-mock-agent"
"#,
    )
    .map_err(|error| format!("{}: {error}", profile.join("agents.toml").display()))?;

    let measured: Result<Vec<Figure>, String> = (|| {
        let daemon = repeat(repeats, || {
            daemon_idle(&target.join(exe("acpilotd")), &profile, &workspace)
        })?;
        let session = repeat(repeats, || {
            daemon_with_session(&target.join(exe("acpilotd")), &profile, &workspace)
        })?;
        let turn = repeat(repeats, || {
            daemon_with_turn(&target.join(exe("acpilotd")), &profile, &workspace)
        })?;
        let serve = repeat(repeats, || {
            serve_row(&target.join(exe("acpilot")), &profile, &workspace)
        })?;
        let tui = repeat(repeats, || tui_row(&target.join(exe("acpilot")), &profile))?;
        let doctor = repeat(repeats, || doctor(&target.join(exe("acpilot")), &workspace))?;
        Ok(vec![daemon, session, turn, serve, tui, doctor])
    })();
    let _ = fs::remove_dir_all(&profile);
    let figures = measured?;

    if gate {
        return Ok(report(&figures, release, repeats));
    }

    let note = render(&figures, release);
    let path = path_in_repo(NOTE);
    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return Err(format!("{}: {error}", parent.display()));
    }
    fs::write(&path, note).map_err(|error| format!("{}: {error}", path.display()))?;
    println!("==> wrote {NOTE}");

    for figure in &figures {
        println!(
            "    {:<16} {:>9} {:>18} {:>16} {:>8}",
            figure.row,
            mib(figure.rss_peak),
            cpu(figure.cpu_mean),
            cpu(figure.cpu_peak),
            figure.threads
        );
    }
    println!("    not measured on this host (the row arrives with its phase):");
    for (row, reason) in SKIPPED {
        println!("      {row}: {reason}");
    }
    Ok(true)
}

/// The budgets: the median a run on GitHub's `ubuntu-latest` measured, rounded up.
///
/// The budget is not the product's ceiling — the table in `TODO.md` names 60 MB for an
/// idle daemon — but the figure the runner the gate runs on measured, so the gate
/// compares CI with CI rather than a developer's box with a hosted machine. A row that
/// stops being measured is also a failure: a budget nobody checks is a promise nobody
/// keeps. Thread budgets are a fixed cost plus one per logical CPU rather than a single
/// number, because the runtime's pool is sized from that count: the same constants then
/// describe the runner, a container and a developer's box.
const BUDGETS: &[Budget] = &[
    Budget {
        row: IDLE_ROW,
        rss_peak_mib: 20.0,
        cpu_mean: Some(0.5),
        // The runner measured 8 threads on 4 logical CPUs; this machine 28 on 24.
        threads: Threads {
            base: 4,
            per_core: 1,
        },
    },
    Budget {
        row: SESSION_ROW,
        rss_peak_mib: 24.0,
        cpu_mean: Some(0.5),
        threads: Threads {
            base: 8,
            per_core: 1,
        },
    },
    Budget {
        row: TURN_ROW,
        // The runner measured 22.7 MiB and 12 threads on 4 logical CPUs: the session
        // row's shape, which is what a turn costs on top of the session it runs in.
        rss_peak_mib: 23.0,
        cpu_mean: Some(0.5),
        threads: Threads {
            base: 8,
            per_core: 1,
        },
    },
    Budget {
        row: SERVE_ROW,
        // The runner measured 20.8 MiB and 8 threads on 4 logical CPUs; this machine
        // 28 threads on 24, so the pool is four plus the core count here too.
        rss_peak_mib: 22.0,
        cpu_mean: Some(0.5),
        threads: Threads {
            base: 4,
            per_core: 1,
        },
    },
    Budget {
        row: TUI_ROW,
        // The runner measured 15.9 MiB, 0.30 % mean CPU and one thread on 4 logical
        // CPUs, and this machine the same one thread on 24: the client draws on one
        // thread whatever the host has, so there is no per-CPU term.
        rss_peak_mib: 16.0,
        cpu_mean: Some(0.5),
        threads: Threads {
            base: 1,
            per_core: 0,
        },
    },
    Budget {
        row: "acpilot doctor",
        rss_peak_mib: 8.0,
        // A one-shot row ends before `sysinfo` can take a delta, so it reports no CPU
        // and none is budgeted for it. The row has a budget for what it can measure.
        cpu_mean: None,
        threads: Threads {
            base: 2,
            per_core: 0,
        },
    },
];

/// The fraction of a peak-RSS budget a median may exceed.
///
/// Half again, because a hosted runner allocates differently from this machine and the
/// gate has to catch a doubled figure, not a different libc.
const RSS_MARGIN: f64 = 0.5;
/// Wider than memory: CPU time on a shared runner is the scheduler as much as the work.
const CPU_MARGIN: f64 = 1.0;
/// Threads are whole and small; half again is the leak this catches, one per request.
const THREAD_MARGIN: f64 = 0.5;

/// One row's budgets, each a median and each with the margin above.
#[derive(Debug, Clone, Copy)]
struct Budget {
    row: &'static str,
    rss_peak_mib: f64,
    cpu_mean: Option<f64>,
    threads: Threads,
}

/// A thread budget: a fixed cost plus a worker for each logical CPU.
///
/// The runtime builds its pool from the machine's logical CPU count, so a thread budget
/// written as one number is a budget for the machine it was measured on. `base` is what
/// the row costs whatever the host — the reactor, a socket per client, the blocking
/// pool; `per_core` is the worker per CPU. The constants come from two runs: the
/// runner's 8 threads on 4 CPUs and this machine's 28 on 24.
#[derive(Debug, Clone, Copy)]
struct Threads {
    base: u64,
    per_core: u64,
}

impl Threads {
    /// The budget on a machine with `cores` logical CPUs.
    fn on(self, cores: u64) -> u64 {
        self.base + self.per_core * cores
    }

    /// The budget's shape, for a log line or a table: `4 + 1/core`.
    fn text(self) -> String {
        if self.per_core == 0 {
            self.base.to_string()
        } else {
            format!("{} + {}/core", self.base, self.per_core)
        }
    }
}

/// The logical CPUs the runtime sizes its pool from, so the number a budget must follow:
/// `available_parallelism` honors a cgroup quota and an affinity mask, which is why a
/// container on a large host gets the workers — and threads — it will actually use.
fn logical_cpus() -> u64 {
    std::thread::available_parallelism().map_or(1, |count| count.get() as u64)
}

/// How one figure and its limit are written.
#[derive(Debug, Clone, Copy)]
enum Unit {
    Mib,
    Percent,
    Count,
}

impl Unit {
    fn write(self, value: f64) -> String {
        match self {
            Unit::Mib => format!("{value:.1} MiB"),
            Unit::Percent => format!("{value:.2} %"),
            Unit::Count => format!("{value:.0}"),
        }
    }
}

/// One figure against its budget: printed whether it passed or not, because a log that
/// shows only the failure cannot tell a figure that moved from one never taken.
#[derive(Debug)]
struct Verdict {
    row: &'static str,
    metric: &'static str,
    measured: String,
    limit: String,
    over: bool,
}

fn verdict(
    row: &'static str,
    metric: &'static str,
    value: f64,
    budget: f64,
    margin: f64,
    unit: Unit,
) -> Verdict {
    let limit = budget * (1.0 + margin);
    Verdict {
        over: value > limit,
        measured: unit.write(value),
        limit: format!(
            "{} (budget {} + {:.0} %)",
            unit.write(limit),
            unit.write(budget),
            margin * 100.0
        ),
        row,
        metric,
    }
}

/// Every measured figure against its budget, in budget order, then every figure a
/// budget does not name.
fn compare(figures: &[Figure], budgets: &[Budget], cores: u64) -> Vec<Verdict> {
    let mut verdicts = Vec::new();
    for budget in budgets {
        let Some(figure) = figures.iter().find(|figure| figure.row == budget.row) else {
            verdicts.push(Verdict {
                row: budget.row,
                metric: "row",
                measured: "not measured".to_owned(),
                limit: "a budget with no row is not gated".to_owned(),
                over: true,
            });
            continue;
        };
        verdicts.push(verdict(
            budget.row,
            "peak RSS",
            figure.rss_peak as f64 / (1024.0 * 1024.0),
            budget.rss_peak_mib,
            RSS_MARGIN,
            Unit::Mib,
        ));
        if let Some(cpu_budget) = budget.cpu_mean {
            match figure.cpu_mean {
                Some(cpu_mean) => verdicts.push(verdict(
                    budget.row,
                    "mean CPU",
                    f64::from(cpu_mean),
                    cpu_budget,
                    CPU_MARGIN,
                    Unit::Percent,
                )),
                None => verdicts.push(Verdict {
                    row: budget.row,
                    metric: "mean CPU",
                    measured: "no reading".to_owned(),
                    limit: format!("{} is budgeted", Unit::Percent.write(cpu_budget)),
                    over: true,
                }),
            }
        }
        let allowed = budget.threads.on(cores) as f64;
        let limit = allowed * (1.0 + THREAD_MARGIN);
        verdicts.push(Verdict {
            row: budget.row,
            metric: "threads",
            measured: Unit::Count.write(figure.threads as f64),
            limit: format!(
                "{} (budget {} = {} + {:.0} %)",
                Unit::Count.write(limit),
                Unit::Count.write(allowed),
                budget.threads.text(),
                THREAD_MARGIN * 100.0
            ),
            over: figure.threads as f64 > limit,
        });
    }
    for figure in figures {
        if !budgets.iter().any(|budget| budget.row == figure.row) {
            verdicts.push(Verdict {
                row: figure.row,
                metric: "row",
                measured: "measured".to_owned(),
                limit: "no budget, so the row is not gated".to_owned(),
                over: true,
            });
        }
    }
    // The session row's own budget is the daemon with a session in it; the table's
    // budget is what the session *added*, which is the figure a leak moves. Measured
    // as one peak minus the other, so a daemon that grew for an unrelated reason is
    // charged to its own row and not to the session.
    if budgets.iter().any(|budget| budget.row == SESSION_ROW) {
        let idle = figures.iter().find(|figure| figure.row == IDLE_ROW);
        let session = figures.iter().find(|figure| figure.row == SESSION_ROW);
        if let (Some(idle), Some(session)) = (idle, session) {
            let added = session.rss_peak.saturating_sub(idle.rss_peak);
            verdicts.push(verdict(
                SESSION_ROW,
                "added RSS",
                added as f64 / (1024.0 * 1024.0),
                SESSION_MIB,
                0.0,
                Unit::Mib,
            ));
        }
    }
    verdicts
}

/// Print every comparison and say whether the run passed.
fn report(figures: &[Figure], release: bool, repeats: u32) -> bool {
    println!(
        "==> resources gate ({} profile, median of {}): budgets in this file",
        if release { "release" } else { "debug" },
        repeats.max(1)
    );
    let verdicts = compare(figures, BUDGETS, logical_cpus());
    for verdict in &verdicts {
        println!(
            "    {:>4}  {:<18} {:<9} {:>12}  limit {:>34}",
            if verdict.over { "OVER" } else { "ok" },
            verdict.row,
            verdict.metric,
            verdict.measured,
            verdict.limit
        );
    }
    let breaches = verdicts.iter().filter(|verdict| verdict.over).count();
    if breaches == 0 {
        println!("==> every figure is inside its budget");
        true
    } else {
        eprintln!("==> {breaches} figure(s) past budget: a regression is a bug, not a budget");
        false
    }
}

/// The rows a phase has not given this harness yet.
const SKIPPED: &[(&str, &str)] = &[
    (
        "acpilot-gui",
        "needs a display and the shell's `gui` feature (P10)",
    ),
    (
        "language servers",
        "needs one installed; measured as the daemon's children once P7's mock server is the fixture",
    ),
    ("acpilotd-helper", "has no build on this host until P9"),
];

/// Run one row `repeats` times and report the median of each figure.
fn repeat<F>(repeats: u32, mut run: F) -> Result<Figure, String>
where
    F: FnMut() -> Result<Figure, String>,
{
    let mut figures = Vec::new();
    for _ in 0..repeats.max(1) {
        figures.push(run()?);
    }
    Ok(median(figures))
}

/// The median figure, field by field.
///
/// A median rather than a mean because one scheduling hitch should not move a budget,
/// and rather than a minimum because the slow run is the one a person feels.
fn median(mut figures: Vec<Figure>) -> Figure {
    let first = figures.remove(0);
    figures.push(first.clone());
    let rss_peak = median_u64(figures.iter().map(|figure| figure.rss_peak).collect());
    let threads = median_u64(figures.iter().map(|figure| figure.threads as u64).collect()) as usize;
    let cpu_mean = median_cpu(&figures, |figure| figure.cpu_mean);
    let cpu_peak = median_cpu(&figures, |figure| figure.cpu_peak);
    Figure {
        row: first.row,
        window: first.window,
        rss_peak,
        cpu_mean,
        cpu_peak,
        threads,
    }
}

fn median_u64(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

/// A median only when every run could report one: mixing a sample with a hole would
/// make the figure look like a CPU number that was never taken.
fn median_cpu(figures: &[Figure], field: impl Fn(&Figure) -> Option<f32>) -> Option<f32> {
    let mut values = Vec::with_capacity(figures.len());
    for figure in figures {
        values.push(field(figure)?);
    }
    values.sort_by(f32::total_cmp);
    Some(values[values.len() / 2])
}

/// `acpilotd` with a temporary profile and one `/v1/events` subscriber.
fn daemon_idle(binary: &Path, profile: &Path, workspace: &Path) -> Result<Figure, String> {
    daemon(binary, profile, workspace, Load::Idle)
}

/// The same daemon with one idle session in it.
fn daemon_with_session(binary: &Path, profile: &Path, workspace: &Path) -> Result<Figure, String> {
    daemon(binary, profile, workspace, Load::Session)
}

/// The same daemon with one prompt turn: a client's keystroke, end to end.
fn daemon_with_turn(binary: &Path, profile: &Path, workspace: &Path) -> Result<Figure, String> {
    daemon(binary, profile, workspace, Load::Turn)
}

/// What the daemon has running inside its window.
#[derive(Debug, Clone, Copy)]
enum Load {
    /// A subscriber and nothing else.
    Idle,
    /// One session, created over the API and left idle.
    Session,
    /// One session with one prompt turn sent through the API.
    Turn,
}

/// A daemon, its subscriber, and the load the row names.
///
/// The subscriber opens before the session, so the window measures a daemon that is
/// already serving the stream: a connection arriving in the middle of a measurement
/// is the setup's cost, not the load's.
fn daemon(binary: &Path, profile: &Path, workspace: &Path, load: Load) -> Result<Figure, String> {
    let port = free_port()?;
    let mut child = spawn(
        Command::new(binary)
            .arg("--profile")
            .arg(profile)
            .arg("--workspace")
            .arg(workspace)
            .arg("--addr")
            .arg(format!("127.0.0.1:{port}"))
            // The token, the database and the discovered profiles are state, and a
            // row that wrote the machine's real ones would measure somebody's
            // history rather than this window. The daemon writes its own under the
            // temporary profile instead.
            .env("XDG_RUNTIME_DIR", profile)
            .env("ACPILOT_HOME", profile)
            // And the config root, so the daemon discovers no profiles but this one:
            // a row that loaded a developer's real profiles would count threads and
            // memory that belong to somebody's settings, and the runner's empty list
            // would be a different daemon.
            .env("XDG_CONFIG_HOME", profile)
            .env("PATH", search_path(binary)),
        binary,
    )?;
    let measured = wait_until_healthy(port, &mut child).and_then(|()| {
        let token = wait_for_token(profile)?;
        let subscriber = Subscriber::open(port, &token)?;
        let figure = match load {
            Load::Idle => measure(
                &mut child,
                IDLE_ROW,
                "idle: one `/v1/events` subscriber open for the window (drawing and waiting)",
                IDLE_WINDOW,
                false,
                true,
            ),
            Load::Session => create_session(port, &token, "mock").and_then(|_session| {
                measure(
                    &mut child,
                    SESSION_ROW,
                    "one idle session: created over the API, its mock agent running; the \
                     daemon's own process",
                    IDLE_WINDOW,
                    false,
                    false,
                )
            }),
            Load::Turn => create_session(port, &token, "mock").and_then(|session| {
                prompt_session(port, &token, &session).and_then(|()| {
                    measure(
                        &mut child,
                        TURN_ROW,
                        "one prompt turn: the prompt sent over the API, the fixture agent's \
                         events recorded and streamed to one subscriber; the daemon's own \
                         process, with the rest of the window idle",
                        IDLE_WINDOW,
                        false,
                        false,
                    )
                })
            }),
        };
        subscriber.finish();
        figure
    });
    stop(&mut child);
    measured
}

/// `acpilot tui --demo` under a pseudo-terminal: the client's frame clock.
///
/// A terminal UI needs a terminal: without one it cannot put stdin into raw mode, and
/// a row that measured the refusal would measure the launcher. The design pass is the
/// one launch that contacts no daemon and reads no project, so the figure is the
/// client's own — the frame clock, the editor and the panes over the seeded world —
/// with none of a session's traffic in it. The pty is 120x40 for every run: a taller
/// terminal paints more cells, and a budget for one screen is not a budget for another.
fn tui_row(binary: &Path, profile: &Path) -> Result<Figure, String> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("the TUI row has no pseudo-terminal: {error}"))?;
    let mut command = CommandBuilder::new(binary);
    command.arg("tui");
    command.arg("--demo");
    // A terminal that names itself, or the client draws its dumb fallback and the row
    // measures a program nobody runs.
    command.env("TERM", "xterm-256color");
    // The design pass reads no project, but it does read the settings cascade: the
    // machine's real theme is not this row's to load.
    command.env("XDG_CONFIG_HOME", profile);
    command.env("ACPILOT_HOME", profile);
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("cannot start acpilot tui: {error}"))?;
    // The parent's copy of the slave, or the master never sees the child exit.
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("cannot read the TUI's terminal: {error}"))?;
    // A full-screen frame is several kilobytes and nobody else reads the master: a pty
    // whose output is not drained blocks the program mid-draw, which would turn this
    // row into a measurement of the harness. The first byte is also how the row knows
    // it is sampling a program that drew rather than one that sat in raw mode.
    let (painted, first_frame) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let running = Arc::clone(&stop);
    let draining = thread::spawn(move || {
        let mut announced = false;
        let mut chunk = [0u8; 8192];
        while !running.load(Ordering::Relaxed) {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(_) => {
                    if !announced {
                        announced = true;
                        let _ = painted.send(());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    let measured = (|| -> Result<Figure, String> {
        let pid = child
            .process_id()
            .ok_or_else(|| "the TUI row has no process to sample".to_owned())?;
        let deadline = Instant::now() + STARTUP;
        loop {
            match first_frame.recv_timeout(Duration::from_millis(50)) {
                Ok(()) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(status) = child
                        .try_wait()
                        .map_err(|error| format!("{TUI_ROW}: {error}"))?
                    {
                        return Err(format!("acpilot tui exited before it drew ({status})"));
                    }
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "acpilot tui drew no frame inside {}s",
                            STARTUP.as_secs()
                        ));
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("acpilot tui stopped before it drew a frame".to_owned());
                }
            }
        }
        sample(
            Pid::from_u32(pid),
            TUI_ROW,
            "one design pass: `acpilot tui --demo` at its frame clock under a 120x40 \
             pseudo-terminal; no daemon and no session traffic",
            TUI_WINDOW,
            false,
            true,
            || {
                child
                    .try_wait()
                    .map(|status| status.map(|status| status.to_string()))
                    .map_err(|error| format!("{TUI_ROW}: {error}"))
            },
        )
    })();
    stop.store(true, Ordering::Relaxed);
    let _ = child.kill();
    let _ = child.wait();
    drop(pair.master);
    let _ = draining.join();
    measured
}

/// `acpilot serve` with one page connected: the CLI's long-lived row.
///
/// The load is a page that is really open, not a request per sample: the page is fetched
/// (the one route that answers without a token) and then holds one `/v1/events`
/// subscriber for the window, which is what a browser costs the server while somebody
/// reads. The browser tab's own memory is not this row's — it is not the process under
/// test, and the page renders in a sandbox the harness does not have.
fn serve_row(binary: &Path, profile: &Path, workspace: &Path) -> Result<Figure, String> {
    let port = free_port()?;
    // Fixed rather than generated, so the harness knows the secret without parsing the
    // panel, and written under the temporary runtime directory, so it is nobody's token.
    let token = "acpilot-resources-serve";
    let mut child = spawn(
        Command::new(binary)
            .arg("--profile")
            .arg(profile)
            .arg("serve")
            .arg("--bind")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--token")
            .arg(token)
            .arg("--workspace")
            .arg(workspace)
            // The token, the database and the discovered profiles are state: the row
            // writes its own under the temporary profile rather than reading the
            // machine's, the same way the daemon rows do it.
            .env("XDG_RUNTIME_DIR", profile)
            .env("ACPILOT_HOME", profile)
            .env("XDG_CONFIG_HOME", profile)
            .env("PATH", search_path(binary)),
        binary,
    )?;
    let measured = wait_for_page(port, &mut child).and_then(|()| {
        let subscriber = Subscriber::open(port, token)?;
        let figure = measure(
            &mut child,
            SERVE_ROW,
            "one page connected: the page fetched, one `/v1/events` subscriber open for \
             the window",
            IDLE_WINDOW,
            false,
            true,
        );
        subscriber.finish();
        figure
    });
    stop(&mut child);
    measured
}

/// Wait until `acpilot serve` answers its page, or say what it answered instead.
fn wait_for_page(port: u16, child: &mut Child) -> Result<(), String> {
    let deadline = Instant::now() + STARTUP;
    let mut last = "no answer".to_owned();
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("acpilot serve exited before its page ({status})"));
        }
        let request = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        match request_once(port, request, Duration::from_secs(5)) {
            Ok(response) if response.starts_with("HTTP/1.1 200") => return Ok(()),
            Ok(response) => last = first_line(&response),
            Err(error) => last = error,
        }
        sleep(Duration::from_millis(50));
    }
    Err(format!(
        "acpilot serve answered no page inside {}s: {last}",
        STARTUP.as_secs()
    ))
}

/// The fixture agent the session row starts: `cargo build -p acpilot-agent` writes it
/// beside the daemon, because no release archive carries it.
fn build_mock_agent(release: bool) -> bool {
    let mut command = Command::new(env!("CARGO"));
    command.args(["build", "-p", "acpilot-agent"]);
    if release {
        command.arg("--release");
    }
    crate::run_step("build (mock agent)", command)
}

/// `PATH` with the daemon's own directory first, so the fixture agent it spawns is
/// the one this build wrote rather than one a developer happens to have installed.
fn search_path(binary: &Path) -> String {
    let mut paths = vec![binary.parent().unwrap_or(Path::new(".")).to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths)
        .map(|paths| paths.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The token the daemon wrote under the profile, once it is there.
fn wait_for_token(profile: &Path) -> Result<String, String> {
    let path = profile.join("acpilot").join("token");
    let deadline = Instant::now() + STARTUP;
    while Instant::now() < deadline {
        match fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => return Ok(text.trim().to_owned()),
            _ => sleep(Duration::from_millis(20)),
        }
    }
    Err(format!(
        "the daemon wrote no token at {} inside {}s",
        path.display(),
        STARTUP.as_secs()
    ))
}

/// `POST /v1/sessions` with the fixture agent, and the id the daemon gave it.
///
/// The id is read from the answer rather than generated here: the daemon owns the
/// conversation's identity, and a second request needs what it called the session.
fn create_session(port: u16, token: &str, agent: &str) -> Result<String, String> {
    let body = format!(r#"{{"workspace_id":"","agent_id":"{agent}"}}"#);
    let request = format!(
        "POST /v1/sessions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer \
         {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: \
         close\r\n\r\n{body}",
        body.len()
    );
    let response = request_once(port, &request, Duration::from_secs(10))?;
    if !(response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.1 201")) {
        return Err(format!(
            "the session was refused: {}",
            first_line(&response)
        ));
    }
    let body = response.split("\r\n\r\n").nth(1).unwrap_or_default();
    body.split("\"id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("the session answer named no id: {}", first_line(&response)))
}

/// `POST /v1/sessions/{id}/prompt`: one turn, which the fixture agent answers at once.
fn prompt_session(port: u16, token: &str, session: &str) -> Result<(), String> {
    let body = r#"{"text":"stream 6"}"#;
    let request = format!(
        "POST /v1/sessions/{session}/prompt HTTP/1.1\r\nHost: localhost\r\nAuthorization: \
         Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let response = request_once(port, &request, Duration::from_secs(10))?;
    if response.starts_with("HTTP/1.1 202") || response.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        Err(format!("the prompt was refused: {}", first_line(&response)))
    }
}

/// One request, its whole answer, and a cap: an error page is not a transcript.
fn request_once(port: u16, request: &str, timeout: Duration) -> Result<String, String> {
    let mut stream = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), timeout)
        .map_err(|error| format!("cannot reach the daemon: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("cannot set a read timeout: {error}"))?;
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("cannot send the request: {error}"))?;
    let mut response = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                response.extend_from_slice(&chunk[..read]);
                if response.len() >= 8 * 1024 {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(format!("cannot read the answer: {error}")),
        }
    }
    Ok(String::from_utf8_lossy(&response).into_owned())
}

fn first_line(response: &str) -> String {
    response.lines().next().unwrap_or("no answer").to_owned()
}

/// One subscriber on `/v1/events`, drained by its own thread for the window.
///
/// The daemon writes a frame only when there is news, so the thread is mostly
/// blocked in `read`: what the row measures is the connection being there, which is
/// what a client costs while a person is reading rather than typing.
struct Subscriber {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Subscriber {
    fn open(port: u16, token: &str) -> Result<Self, String> {
        let mut stream = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(5),
        )
        .map_err(|error| format!("cannot open the events stream: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| format!("cannot set a read timeout: {error}"))?;
        // A stream is a request like any other, so it carries the token: the page is
        // the only route that answers without one.
        let request = format!(
            "GET /v1/events HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer \
             {token}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("cannot send the events request: {error}"))?;
        let mut head = [0u8; 64];
        let read = stream
            .read(&mut head)
            .map_err(|error| format!("cannot read the events answer: {error}"))?;
        if read == 0 || !head[..read].starts_with(b"HTTP/1.1 200") {
            return Err(format!(
                "the events stream answered {}",
                String::from_utf8_lossy(&head[..read]).trim()
            ));
        }
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|error| format!("cannot set a read timeout: {error}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut chunk = [0u8; 1024];
                while !stop.load(Ordering::Relaxed) {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => break,
                    }
                }
                let _ = stream.shutdown(Shutdown::Both);
            })
        };
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `acpilot doctor` against the temporary workspace: the CLI's startup cost.
///
/// The profile is a name rather than the daemon's directory: the client validates a
/// name, and a profile that does not exist is the first-run state `doctor` is built to
/// report — it starts no daemon and writes nothing, so the real configuration is not
/// touched.
fn doctor(binary: &Path, workspace: &Path) -> Result<Figure, String> {
    let mut child = spawn(
        Command::new(binary)
            .arg("--profile")
            .arg("acpilot-resources")
            .arg("doctor")
            .arg(workspace),
        binary,
    )?;
    let measured = measure(
        &mut child,
        "acpilot doctor",
        "one-shot: reads PATH, the profile and the workspace; starts no daemon",
        ONE_SHOT_WINDOW,
        true,
        true,
    );
    if measured.is_err() {
        stop(&mut child);
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("acpilot doctor exited {status}"));
    }
    measured
}

/// Sample `child`'s process tree for `duration`.
///
/// The whole tree, not the root: an agent, a helper or a language server is part of
/// the figure its row reports. A one-shot row is sampled every
/// [`SAMPLE`] too but is allowed to end inside the window; a long-lived row that ends
/// is an error, because a daemon that died is not an idle daemon.
fn measure(
    child: &mut Child,
    row: &'static str,
    window: &'static str,
    duration: Duration,
    one_shot: bool,
    whole_tree: bool,
) -> Result<Figure, String> {
    let root = Pid::from_u32(child.id());
    sample(root, row, window, duration, one_shot, whole_tree, || {
        child
            .try_wait()
            .map(|status| status.map(|status| status.to_string()))
            .map_err(|error| format!("{row}: {error}"))
    })
}

/// The sampling loop, for a row whose program is not a [`Child`] of this process: a
/// program started on a pseudo-terminal is the terminal's child, and only its pid and
/// a way to ask whether it is still running are available here.
fn sample(
    root: Pid,
    row: &'static str,
    window: &'static str,
    duration: Duration,
    one_shot: bool,
    whole_tree: bool,
    mut exited: impl FnMut() -> Result<Option<String>, String>,
) -> Result<Figure, String> {
    // A one-shot row is sampled from the moment it exists, at 2 ms, because a CLI
    // that finishes in fifty milliseconds has no 100 ms window to stand in. Its
    // tree is one process, so refreshing only that pid is also the cheaper read.
    let interval = if one_shot {
        Duration::from_millis(20)
    } else {
        SAMPLE
    };
    let mut system = System::new();
    // sysinfo's CPU figure is a delta between two refreshes; this is the base.
    refresh_for(&mut system, root, one_shot);

    let started = Instant::now();
    let mut samples = 0u32;
    let mut cpu_samples = 0u32;
    let mut rss_peak = 0u64;
    let mut cpu_peak = 0.0f32;
    let mut cpu_sum = 0.0f32;
    let mut threads = 0usize;
    let mut stopped = None;
    loop {
        // Sampled before the sleep, because a one-shot row can be over by the time an
        // interval elapses: a `doctor` run on a hosted runner exited inside 40 ms and
        // reported 0.0 MiB, a corpse's memory reading as a figure inside every budget.
        // A zero reading counts for the same reason — a process that is gone is not a
        // process that used no memory.
        let (rss, cpu, count) = sample_tree(&mut system, root, one_shot, whole_tree);
        if count > 0 && rss > 0 {
            samples += 1;
            rss_peak = rss_peak.max(rss);
            threads = threads.max(count);
            // A one-shot row's first delta is the few milliseconds between the base
            // refresh and this sample rather than a window, so it counts for memory
            // and threads and not for CPU.
            if !one_shot || samples > 1 {
                cpu_samples += 1;
                cpu_peak = cpu_peak.max(cpu);
                cpu_sum += cpu;
            }
        }
        if let Some(status) = exited()? {
            stopped = Some(status);
            break;
        }
        if started.elapsed() >= duration {
            break;
        }
        sleep(interval);
    }

    if let Some(status) = stopped
        && !one_shot
    {
        return Err(format!("{row}: exited during its window ({status})"));
    }
    if samples == 0 {
        return Err(format!(
            "{row}: the process tree was never seen, so nothing was measured"
        ));
    }
    // A one-shot row is too short for sysinfo's clock: it derives a percentage from
    // the delta between two refreshes, and a 170 ms CLI does not give it enough
    // intervals for the number to mean anything. Memory and threads still do, and a
    // `0.00 %` would be a claim this harness cannot make.
    let (cpu_mean, cpu_peak) = if !one_shot && cpu_samples >= 2 {
        (Some(cpu_sum / cpu_samples as f32), Some(cpu_peak))
    } else {
        (None, None)
    };
    Ok(Figure {
        row,
        window,
        rss_peak,
        cpu_mean,
        cpu_peak,
        threads,
    })
}

/// Memory, CPU and thread count summed over the tree rooted at `root`.
fn sample_tree(
    system: &mut System,
    root: Pid,
    one_shot: bool,
    whole_tree: bool,
) -> (u64, f32, usize) {
    refresh_for(system, root, one_shot);
    let mut rss = 0u64;
    let mut cpu = 0.0f32;
    let mut threads = 0usize;
    for (pid, process) in system.processes() {
        // On Linux a thread is also a task in the process table, carrying its
        // process's memory: counting one as a process multiplies RSS by the thread
        // count. The thread count comes from `tasks` below instead.
        if process.thread_kind().is_some() {
            continue;
        }
        // A session row reads the daemon alone: the agent it starts is a separate
        // program, and its memory is not the session's to budget.
        if !whole_tree {
            if *pid != root {
                continue;
            }
        } else if !descends_from(system, *pid, root) {
            continue;
        }
        rss += process.memory();
        cpu += process.cpu_usage();
        // A process is at least one thread; sysinfo's task list is Linux-only and is
        // empty when the refresh did not ask for it, which is not zero threads.
        threads += process.tasks().map_or(1, |tasks| tasks.len().max(1));
    }
    (rss, cpu, threads)
}

fn refresh_for(system: &mut System, root: Pid, one_shot: bool) {
    let kind = ProcessRefreshKind::new().with_memory().with_cpu();
    if one_shot {
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[root]), kind);
    } else {
        system.refresh_processes_specifics(ProcessesToUpdate::All, kind);
    }
}

fn descends_from(system: &System, pid: Pid, root: Pid) -> bool {
    if pid == root {
        return true;
    }
    let mut current = pid;
    for _ in 0..64 {
        let Some(process) = system.process(current) else {
            return false;
        };
        let Some(parent) = process.parent() else {
            return false;
        };
        if parent == root {
            return true;
        }
        current = parent;
    }
    false
}

/// The daemon's answer, whatever it is: a status code is still a request, and the
/// point of the load is that a client keeps waking the process.
///
/// The root is asked rather than `/v1/health` because a TCP listener wants a bearer
/// token on every route but the page itself, and a request the daemon rejects is not
/// the work an idle client costs it. The page is the one route served without one.
fn health(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(200),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    if stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buffer = [0u8; 16];
    stream.read(&mut buffer).is_ok_and(|read| read > 0)
}

fn wait_until_healthy(port: u16, child: &mut Child) -> Result<(), String> {
    let deadline = Instant::now() + STARTUP;
    while Instant::now() < deadline {
        if health(port) {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!(
                "acpilotd exited before it answered health ({status})"
            ));
        }
        sleep(Duration::from_millis(50));
    }
    Err(format!(
        "acpilotd did not answer a page request inside {}s",
        STARTUP.as_secs()
    ))
}

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("no loopback port to bind: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("no loopback address: {error}"))
}

fn spawn(command: &mut Command, binary: &Path) -> Result<Child, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", binary.display()))
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn target_dir(root: &Path, release: bool) -> PathBuf {
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    target.join(if release { "release" } else { "debug" })
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn cpu(value: Option<f32>) -> String {
    value.map_or_else(
        || "—".to_owned(),
        |value| format!("{value:.2} % of one core"),
    )
}

/// One budget as the note's table writes it.
fn budget_row(
    note: &mut String,
    budget: &Budget,
    metric: &str,
    value: f64,
    margin: f64,
    unit: Unit,
    source: &str,
) {
    note.push_str(&format!(
        "| `{}` | {metric} | {} | {:.0} % | {} | {source} |\n",
        budget.row,
        unit.write(value),
        margin * 100.0,
        unit.write(value * (1.0 + margin))
    ));
}

/// The note: the figures beside the window and the machine that produced them.
fn render(figures: &[Figure], release: bool) -> String {
    let system = System::new_all();
    let cores = system.cpus().len();
    let brand = system
        .cpus()
        .first()
        .map_or("unknown", |cpu| cpu.brand())
        .to_owned();
    let memory = system.total_memory();
    let os = System::long_os_version().unwrap_or_else(|| std::env::consts::OS.to_owned());
    let kernel = System::kernel_version().unwrap_or_default();

    let mut note = String::new();
    note.push_str("# Every program's CPU and memory\n\n");
    note.push_str(
        "Cross-cutting task **XT-4**. Written by `cargo xtask resources`, which starts a\n\
         binary the archives carry against a temporary profile, drives one scripted load,\n\
         samples the whole process tree every 100 ms, and reports peak RSS, mean and peak\n\
         CPU and the thread count. Each row is measured three times and the median is\n\
         reported here; the window each figure was taken in is in the table, because a\n\
         memory number without one measures the absence of a load.\n\n",
    );
    note.push_str(&format!(
        "Measured on this machine: **{os}**, kernel `{kernel}`, `{}` target, {cores} logical\n\
         CPU(s) — {brand} — and {} of memory. The build is the **{}** profile.\n\n",
        std::env::consts::ARCH,
        mib(memory),
        if release { "release" } else { "debug" }
    ));
    note.push_str(
        "CPU is a fraction of **one core**, summed over the tree: a percentage of the\n\
         machine would hide the second core a busy loop costs. A `—` means the row ended\n\
         before a second sample could take a delta, so memory is the only figure it has.\n\n",
    );

    note.push_str("| program | window | peak RSS | mean CPU | peak CPU | threads |\n");
    note.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for figure in figures {
        note.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            figure.row,
            figure.window,
            mib(figure.rss_peak),
            cpu(figure.cpu_mean),
            cpu(figure.cpu_peak),
            figure.threads
        ));
    }
    note.push('\n');
    if let (Some(idle), Some(session)) = (
        figures.iter().find(|figure| figure.row == IDLE_ROW),
        figures.iter().find(|figure| figure.row == SESSION_ROW),
    ) {
        note.push_str(&format!(
            "One idle session adds **{}** to the daemon's own process — the session row's\n\
             peak less the idle row's — against the table's {:.0} MB budget, which has no\n\
             margin because it is a stated ceiling rather than a figure this harness\n\
             discovered.\n\n",
            mib(session.rss_peak.saturating_sub(idle.rss_peak)),
            SESSION_MIB
        ));
    }

    let cpus = logical_cpus();
    note.push_str("## Budgets\n\n");
    note.push_str(
        "The first run set the budgets below and CI holds them: `cargo xtask resources --gate`\n\
         repeats each row three times and fails when a median is past its budget plus the\n\
         margin. A figure is written as a budget and a percentage rather than as one sample,\n\
         because the same program on another machine is not the same number, and a gate that\n\
         trips on a scheduling hitch is a gate somebody turns off. A row measured without a\n\
         budget fails too: a row outside the list is one the gate quietly stopped checking.\n\
         The last column says where each budget came from — a median the runner took, or a\n\
         ceiling the product states — because those are different kinds of promise, and a\n\
         regression check is only meaningful against the first.\n\n",
    );
    note.push_str("| program | metric | budget | margin | fails above | source |\n");
    note.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for budget in BUDGETS {
        budget_row(
            &mut note,
            budget,
            "peak RSS",
            budget.rss_peak_mib,
            RSS_MARGIN,
            Unit::Mib,
            "the runner's median",
        );
        if let Some(cpu_mean) = budget.cpu_mean {
            budget_row(
                &mut note,
                budget,
                "mean CPU",
                cpu_mean,
                CPU_MARGIN,
                Unit::Percent,
                "the runner's median",
            );
        }
        let source = if budget.threads.per_core == 0 {
            "the runner's count"
        } else {
            "the runner's count, one per CPU"
        };
        note.push_str(&format!(
            "| `{}` | threads | {} ({}) | {:.0} % | {} | {source} |\n",
            budget.row,
            budget.threads.on(cpus),
            budget.threads.text(),
            THREAD_MARGIN * 100.0,
            (budget.threads.on(cpus) as f64 * (1.0 + THREAD_MARGIN)).ceil() as u64
        ));
    }
    // The session delta is not a measured median but the table's own promise, so it is
    // in the same table with its source named rather than left to the prose.
    note.push_str(&format!(
        "| `{}` | added RSS | {} | 0 % | {} | the table's {:.0} MB ceiling |\n",
        SESSION_ROW,
        Unit::Mib.write(SESSION_MIB),
        Unit::Mib.write(SESSION_MIB),
        SESSION_MIB
    ));
    note.push('\n');
    note.push_str(
        "The `acpilotd` rows are measured with the load named in theirs rather than at rest:\n\
         one `/v1/events` subscriber open for the window, and the same daemon with one idle\n\
         session, read on its own process because the mock agent it starts is a separate\n\
         program. The session's added memory is checked on its own, with no margin. The TUI\n\
         row is the client drawing its design pass under a 120x40 pseudo-terminal, the one\n\
         launch that needs no daemon: the frame clock's cost, not a session's. The one-shot\n\
         CLI row is memory and threads only — a 170 ms process gives `sysinfo` too few\n\
         intervals for a percentage — so its CPU budget is `None` rather than zero. The\n\
         budgets below are the medians a run on GitHub's `ubuntu-latest` measured, not this\n\
         machine's: the table above is this machine, and the gate compares the runner with\n\
         the runner. A thread budget is a fixed cost plus one per logical CPU rather than\n\
         a single number, because the runtime's pool follows the core count: a bare\n\
         figure would be a budget for the machine it was measured on.\n\n",
    );

    note.push_str("## What is not measured here, and why\n\n");
    for (row, reason) in SKIPPED {
        note.push_str(&format!("- `{row}` — {reason}.\n"));
    }
    note.push_str(
        "\nAlso out of scope, on purpose: the agent's own consumption (third-party code, shown\n\
         only so a turn's total is visible), the browser tab `acpilot serve` hands a page to,\n\
         and video memory, which belongs to the desktop phase's P10-6. A language server's\n\
         memory is reported per root when its row lands, because one server is shared by every\n\
         attached workspace.\n\n",
    );

    note.push_str("## Limitations\n\n");
    note.push_str(
        "- The memory and CPU budgets are one runner's medians, so a runner image whose\n\
         kernel differs can move a row without a regression: re-measure with\n\
         `cargo xtask resources --gate` on the runner and take its figures rather than\n\
         widening the margin. The thread budgets instead follow the logical CPU count, so\n\
         the same constants describe the runner and a larger machine.\n",
    );
    note.push_str(
        "- The session row's agent is this repository's fixture, not a real agent, and the row\n\
         reads the daemon alone: a real agent's memory is its own program's, and the harness\n\
         budgets the daemon's share of a session.\n\
         - The daemon's window is three seconds, so a sweep that runs every thirty seconds is\n\
         not in it. A regression in a periodic task needs a window that contains one.\n\
         - The free port is bound and released before the daemon binds it; a port taken in\n\
         between makes the daemon exit, which the harness reports as a dead process rather\n\
         than as a figure.\n\
         - A one-shot row reports memory and threads only: `sysinfo` derives CPU from the\n\
         delta between two refreshes, and the CLI finishes in 170 ms. A percentage from that\n\
         many intervals is a number about the sampler, not about the program.\n\
         - A language server the daemon starts is sampled as part of the daemon's tree, but no\n\
         row starts one until P7's mock server is the fixture.\n\
         - The TUI row draws the design pass under a just-opened 120x40 pseudo-terminal: no\n\
         daemon, no agent and no keystrokes. A real session's traffic and another terminal's\n\
         size move the figure, and its budget is a median of exactly this screen.\n",
    );
    note
}

#[cfg(test)]
mod tests {
    use super::*;

    fn figure(rss: u64, cpu: Option<f32>) -> Figure {
        Figure {
            row: "test",
            window: "test",
            rss_peak: rss,
            cpu_mean: cpu,
            cpu_peak: cpu,
            threads: 4,
        }
    }

    #[test]
    fn the_median_is_a_figure_from_the_middle() {
        let median = median(vec![
            figure(10, Some(1.0)),
            figure(30, Some(3.0)),
            figure(20, Some(2.0)),
        ]);
        assert_eq!(median.rss_peak, 20);
        assert_eq!(median.cpu_mean, Some(2.0));
    }

    #[test]
    fn a_missing_cpu_reading_is_not_averaged_around() {
        let median = median(vec![figure(10, Some(1.0)), figure(20, None)]);
        assert_eq!(median.cpu_mean, None, "one hole is not zero percent");
    }

    /// `Figure::rss_peak` is bytes; a budget is MiB.
    const MIB: u64 = 1024 * 1024;

    fn budget(row: &'static str, rss: f64, cpu_mean: Option<f64>, threads: u64) -> Budget {
        Budget {
            row,
            rss_peak_mib: rss,
            cpu_mean,
            threads: Threads {
                base: threads,
                per_core: 0,
            },
        }
    }

    fn named(row: &'static str, rss: u64) -> Figure {
        Figure {
            row,
            ..figure(rss, None)
        }
    }

    #[test]
    fn a_median_inside_its_margin_passes() {
        let verdicts = compare(
            &[figure(20 * MIB, Some(1.0))],
            &[budget("test", 16.0, Some(1.0), 4)],
            4,
        );
        assert!(verdicts.iter().all(|verdict| !verdict.over), "{verdicts:?}");
    }

    #[test]
    fn a_median_past_its_margin_is_a_breach() {
        // 20 MiB against a 16 MiB budget is 25 %, inside the 50 % RSS margin; 40 is not.
        let inside = compare(
            &[figure(20 * MIB, Some(1.0))],
            &[budget("test", 16.0, Some(1.0), 4)],
            4,
        );
        assert!(!inside[0].over, "{inside:?}");
        let past = compare(
            &[figure(40 * MIB, Some(1.0))],
            &[budget("test", 16.0, Some(1.0), 4)],
            4,
        );
        assert!(past[0].over, "{past:?}");
    }

    #[test]
    fn a_budgeted_cpu_with_no_reading_is_a_breach() {
        // A one-shot row that grows a CPU budget would pass on memory alone, and the
        // missing percentage would read as a figure inside its limit.
        let verdicts = compare(
            &[figure(10, None)],
            &[budget("test", 16.0, Some(2.0), 4)],
            4,
        );
        assert!(
            verdicts
                .iter()
                .any(|verdict| verdict.over && verdict.metric == "mean CPU"),
            "{verdicts:?}"
        );
    }

    #[test]
    fn an_ungated_row_and_a_rowless_budget_are_both_breaches() {
        let rowless = compare(&[], &[budget("absent", 16.0, None, 4)], 4);
        assert_eq!(rowless.len(), 1, "{rowless:?}");
        assert!(rowless[0].over, "{rowless:?}");

        let ungated = compare(&[named("stray", 10 * MIB)], &[], 4);
        assert_eq!(ungated.len(), 1, "{ungated:?}");
        assert!(ungated[0].over, "{ungated:?}");
    }

    #[test]
    fn a_session_that_adds_more_than_the_table_allows_is_a_breach() {
        let budgets = [
            budget(IDLE_ROW, 32.0, None, 32),
            budget(SESSION_ROW, 32.0, None, 32),
        ];
        let figures = vec![named(IDLE_ROW, 25 * MIB), named(SESSION_ROW, 30 * MIB)];
        let inside = compare(&figures, &budgets, 4);
        assert!(
            inside
                .iter()
                .any(|verdict| verdict.metric == "added RSS" && !verdict.over),
            "{inside:?}"
        );
        let leaked = vec![named(IDLE_ROW, 25 * MIB), named(SESSION_ROW, 45 * MIB)];
        let past = compare(&leaked, &budgets, 4);
        assert!(
            past.iter()
                .any(|verdict| verdict.metric == "added RSS" && verdict.over),
            "{past:?}"
        );
    }

    #[test]
    fn a_child_of_the_row_is_part_of_its_tree() {
        // The sampling test itself is the row: the test binary's own pid is the root,
        // and every process it is looking for is inside this tree.
        let mut system = System::new_all();
        refresh_for(&mut system, Pid::from_u32(std::process::id()), false);
        let me = Pid::from_u32(std::process::id());
        assert!(descends_from(&system, me, me));
        assert!(!descends_from(&system, me, Pid::from_u32(u32::MAX)));
    }

    #[test]
    fn a_thread_budget_follows_the_logical_cpu_count() {
        // One worker per CPU and a fixed cost beside it: 12 threads fits 5 CPUs
        // (9 budget, 13 limit) and does not fit 2 (6 budget, 9 limit).
        let budgets = [Budget {
            row: "test",
            rss_peak_mib: 16.0,
            cpu_mean: None,
            threads: Threads {
                base: 4,
                per_core: 1,
            },
        }];
        let figure = Figure {
            threads: 12,
            ..figure(10, None)
        };
        let small = compare(std::slice::from_ref(&figure), &budgets, 2);
        assert!(
            small
                .iter()
                .any(|verdict| verdict.metric == "threads" && verdict.over),
            "{small:?}"
        );
        let large = compare(&[figure], &budgets, 5);
        assert!(
            large
                .iter()
                .any(|verdict| verdict.metric == "threads" && !verdict.over),
            "{large:?}"
        );
    }
}
