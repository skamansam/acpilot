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

/// Measure every row this build can reach, three times each, and write the note.
pub(crate) fn run(debug: bool, repeats: u32) -> bool {
    match measure_all(debug, repeats) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("==> resources: {error}");
            false
        }
    }
}

fn measure_all(debug: bool, repeats: u32) -> Result<(), String> {
    let release = !debug;
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
    Ok(())
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
        "The table in `TODO.md` names two memory budgets today: an idle daemon at 60 MB and\n\
         an idle session at 15 MB. The daemon row above is the first measurement of the first\n\
         one against a real process; the session row needs a session, and the CPU rows are\n\
         still unset. Budgets are a median plus a margin rather than one sample, so they want\n\
         the reference runner's variance before they are written down here as limits.\n\n",
    );
    note.push_str(
        "`cargo xtask resources` is not a CI job yet. The plan the task records: the first run\n\
         sets the numbers, a job on the reference runner repeats the script, and a figure past\n\
         its margin fails. A hosted runner is noisier than this machine, so the job is a\n\
         regression gate and this note keeps the measured number and the hardware — neither is\n\
         a claim about a user's machine.\n\n",
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
