//! `cargo xtask resources`: CPU and memory for the programs a release ships.
//!
//! Cross-cutting task XT-4. A figure without its window and its machine is a figure
//! about the machine, so every row names the load it measured and the note records
//! the hardware. The harness lives here rather than in a shipping crate: sampling a
//! process tree is a syscall surface, and a workflow is the only caller.
//!
//! Idle is a load, not the absence of one. The daemon's row keeps a client asking
//! `/v1/health` once per sample, because a poller that wakes sixty times a second
//! shows up as CPU and nowhere else. CPU is a fraction of *one core*, summed over the
//! tree, because a percentage of the machine hides the second core a busy loop costs.
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
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

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

    let measured: Result<Vec<Figure>, String> = (|| {
        let daemon = repeat(repeats, || {
            daemon_idle(&target.join(exe("acpilotd")), &profile)
        })?;
        let doctor = repeat(repeats, || doctor(&target.join(exe("acpilot")), &workspace))?;
        Ok(vec![daemon, doctor])
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

/// The budgets: the median a reference run measured, rounded up to a round number.
///
/// The budget is not the product's ceiling — the table in `TODO.md` names 60 MB for an
/// idle daemon — but the figure this machine measured, so the gate reports a regression
/// while it is still a regression rather than when it reaches a number somebody picked
/// before there was anything to compare it with. A row that stops being measured is also
/// a failure: a budget nobody checks is a promise nobody keeps.
const BUDGETS: &[Budget] = &[
    Budget {
        row: "acpilotd",
        rss_peak_mib: 32.0,
        cpu_mean: Some(2.0),
        threads: 32,
    },
    Budget {
        row: "acpilot doctor",
        rss_peak_mib: 14.0,
        // A one-shot row ends before `sysinfo` can take a delta, so it reports no CPU
        // and none is budgeted for it. The row has a budget for what it can measure.
        cpu_mean: None,
        threads: 2,
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
    threads: u64,
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
fn compare(figures: &[Figure], budgets: &[Budget]) -> Vec<Verdict> {
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
        verdicts.push(verdict(
            budget.row,
            "threads",
            figure.threads as f64,
            budget.threads as f64,
            THREAD_MARGIN,
            Unit::Count,
        ));
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
    verdicts
}

/// Print every comparison and say whether the run passed.
fn report(figures: &[Figure], release: bool, repeats: u32) -> bool {
    println!(
        "==> resources gate ({} profile, median of {}): budgets in this file",
        if release { "release" } else { "debug" },
        repeats.max(1)
    );
    let verdicts = compare(figures, BUDGETS);
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
        "acpilot-tui",
        "needs a terminal it is allowed to put into raw mode (a window is P6's frame clock)",
    ),
    (
        "acpilot-gui",
        "needs a display and the shell's `gui` feature (P10)",
    ),
    (
        "acpilot serve",
        "needs a page connected, which is P11's row",
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

/// `acpilotd` with a temporary profile, one health request per sample.
fn daemon_idle(binary: &Path, profile: &Path) -> Result<Figure, String> {
    let port = free_port()?;
    let mut child = spawn(
        Command::new(binary)
            .arg("--profile")
            .arg(profile)
            .arg("--addr")
            .arg(format!("127.0.0.1:{port}")),
        binary,
    )?;
    let measured = wait_until_healthy(port, &mut child).and_then(|()| {
        measure(
            &mut child,
            "acpilotd",
            "idle: one page request every 100 ms (drawing and waiting)",
            IDLE_WINDOW,
            Some(port),
            false,
        )
    });
    stop(&mut child);
    measured
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
        None,
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
    port: Option<u16>,
    one_shot: bool,
) -> Result<Figure, String> {
    let root = Pid::from_u32(child.id());
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
    sleep(interval);

    let started = Instant::now();
    let mut samples = 0u32;
    let mut rss_peak = 0u64;
    let mut cpu_peak = 0.0f32;
    let mut cpu_sum = 0.0f32;
    let mut threads = 0usize;
    let mut exited = None;
    while started.elapsed() < duration {
        if let Some(port) = port {
            let _ = health(port);
        }
        sleep(interval);
        let (rss, cpu, count) = sample_tree(&mut system, root, one_shot);
        if count > 0 {
            samples += 1;
            rss_peak = rss_peak.max(rss);
            cpu_peak = cpu_peak.max(cpu);
            cpu_sum += cpu;
            threads = threads.max(count);
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("{row}: {error}"))?
        {
            exited = Some(status);
            break;
        }
    }

    if let Some(status) = exited
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
    let (cpu_mean, cpu_peak) = if !one_shot && samples >= 2 {
        (Some(cpu_sum / samples as f32), Some(cpu_peak))
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
fn sample_tree(system: &mut System, root: Pid, one_shot: bool) -> (u64, f32, usize) {
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
        if !descends_from(system, *pid, root) {
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
) {
    note.push_str(&format!(
        "| `{}` | {metric} | {} | {:.0} % | {} |\n",
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

    note.push_str("## Budgets\n\n");
    note.push_str(
        "The first run set the budgets below and CI holds them: `cargo xtask resources --gate`\n\
         repeats each row three times and fails when a median is past its budget plus the\n\
         margin. A figure is written as a budget and a percentage rather than as one sample,\n\
         because the same program on another machine is not the same number, and a gate that\n\
         trips on a scheduling hitch is a gate somebody turns off. A row measured without a\n\
         budget fails too: a row outside the list is one the gate quietly stopped checking.\n\n",
    );
    note.push_str("| program | metric | budget | margin | fails above |\n");
    note.push_str("| --- | --- | --- | --- | --- |\n");
    for budget in BUDGETS {
        budget_row(
            &mut note,
            budget,
            "peak RSS",
            budget.rss_peak_mib,
            RSS_MARGIN,
            Unit::Mib,
        );
        if let Some(cpu_mean) = budget.cpu_mean {
            budget_row(
                &mut note,
                budget,
                "mean CPU",
                cpu_mean,
                CPU_MARGIN,
                Unit::Percent,
            );
        }
        budget_row(
            &mut note,
            budget,
            "threads",
            budget.threads as f64,
            THREAD_MARGIN,
            Unit::Count,
        );
    }
    note.push('\n');
    note.push_str(
        "The `acpilotd` budget is measured with the load named in its row, not at rest; the\n\
         session row and the CPU budget of a one-shot CLI are still open, because a session\n\
         is a row this harness cannot start yet and a 170 ms process gives `sysinfo` too few\n\
         intervals for a percentage. The budgets are this machine's figures, so the gate is\n\
         a regression check: this note keeps the measured number and the hardware, and\n\
         neither is a claim about a user's machine.\n\n",
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
        "- The budgets came from one machine, not a reference runner; a hosted runner that\n\
         measures a different figure needs its own budget, not a wider margin around this\n\
         one's.\n",
    );
    note.push_str(
        "- The daemon row asks for the page, not a subscriber on `/v1/events`; an events\n\
         subscriber is the load the task names and is the next row to add.\n\
         - The daemon's window is three seconds, so a sweep that runs every thirty seconds is\n\
         not in it. A regression in a periodic task needs a window that contains one.\n\
         - The free port is bound and released before the daemon binds it; a port taken in\n\
         between makes the daemon exit, which the harness reports as a dead process rather\n\
         than as a figure.\n\
         - A one-shot row reports memory and threads only: `sysinfo` derives CPU from the\n\
         delta between two refreshes, and the CLI finishes in 170 ms. A percentage from that\n\
         many intervals is a number about the sampler, not about the program.\n\
         - A language server the daemon starts is sampled as part of the daemon's tree, but no\n\
         row starts one until P7's mock server is the fixture.\n",
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
            threads,
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
        );
        assert!(verdicts.iter().all(|verdict| !verdict.over), "{verdicts:?}");
    }

    #[test]
    fn a_median_past_its_margin_is_a_breach() {
        // 20 MiB against a 16 MiB budget is 25 %, inside the 50 % RSS margin; 40 is not.
        let inside = compare(
            &[figure(20 * MIB, Some(1.0))],
            &[budget("test", 16.0, Some(1.0), 4)],
        );
        assert!(!inside[0].over, "{inside:?}");
        let past = compare(
            &[figure(40 * MIB, Some(1.0))],
            &[budget("test", 16.0, Some(1.0), 4)],
        );
        assert!(past[0].over, "{past:?}");
    }

    #[test]
    fn a_budgeted_cpu_with_no_reading_is_a_breach() {
        // A one-shot row that grows a CPU budget would pass on memory alone, and the
        // missing percentage would read as a figure inside its limit.
        let verdicts = compare(&[figure(10, None)], &[budget("test", 16.0, Some(2.0), 4)]);
        assert!(
            verdicts
                .iter()
                .any(|verdict| verdict.over && verdict.metric == "mean CPU"),
            "{verdicts:?}"
        );
    }

    #[test]
    fn an_ungated_row_and_a_rowless_budget_are_both_breaches() {
        let rowless = compare(&[], &[budget("absent", 16.0, None, 4)]);
        assert_eq!(rowless.len(), 1, "{rowless:?}");
        assert!(rowless[0].over, "{rowless:?}");

        let ungated = compare(&[named("stray", 10 * MIB)], &[]);
        assert_eq!(ungated.len(), 1, "{ungated:?}");
        assert!(ungated[0].over, "{ungated:?}");
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
}
