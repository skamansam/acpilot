//! P0-5 — the Windows half of the WSL helper.
//!
//! `docs/notes/pty-agents.md` and `docs/notes/lsp-servers.md` both stop at the
//! Windows boundary: WSL2's Linux PTY is a real PTY, and calling `wsl.exe` from
//! inside WSL is interop, not the path the product ships. The helper actually
//! runs the other way round — a Windows `acpilotd` spawns `wsl.exe`, which starts
//! a Linux `acpilotd-helper`, and the two speak one JSON line at a time. This
//! probe measures that direction and answers the questions in `TODO.md` §P0-5:
//!
//! * what does one call cost, and what does one *message* cost once the helper is
//!   already running? Those are different numbers by two orders of magnitude.
//! * how do `C:\x` and `/mnt/c/x` translate, and what happens to a drive that is
//!   not mounted / a distro that is not the one running?
//! * which interop quirks bite: output encoding, shell-vs-exec argument handling,
//!   exit codes, stderr, stdin framing, and ConPTY.
//!
//! ```bash
//! # the whole measurement, report as JSON for the note
//! cargo run --release -- bench --json /tmp/wsl-helper.json
//!
//! # the same, but refuse to run anywhere a WSL2 kernel VM cannot start
//! cargo run --release -- bench --require-wsl2 --oracle
//!
//! # the user story: code in WSL, app on Windows — open that path and run in it
//! cargo run --release -- workspace --workspace '\\wsl.localhost\Ubuntu-24.04\home\sam\proj' --read Cargo.toml
//!
//! # just the translation table, with wslpath as the oracle
//! cargo run --release -- paths --oracle
//!
//! # install the helper inside the distro (rustc, no network)
//! cargo run --release -- install
//! ```
//!
//! The path-translation code lives in `paths.rs` and is pure Rust with tests; it
//! is meant to be lifted into `acpilot-host` unchanged.

#![cfg_attr(not(windows), allow(dead_code))]

mod discovery;
mod listing;
mod paths;
#[cfg(windows)]
mod wsl;

use std::path::PathBuf;
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
use paths::WslPathMap;
#[cfg(windows)]
use serde::Serialize;

const DEFAULT_HELPER: &str = "~/.local/bin/acpilotd-helper";

/// Cases chosen because they are the ones that break a naive one-prefix
/// implementation: root drive, unmounted drive, both UNC spellings, a wrong
/// distro, spaces, and non-ASCII.
#[cfg(windows)]
const WINDOWS_CASES: &[&str] = &[
    r"C:\Users\SamTyler",
    r"C:/Users/SamTyler",
    r"C:\Users\SamTyler\AppData\Local",
    r"c:\users",
    r"C:\",
    r"C:\a b\ü\文件",
    r"\\wsl.localhost\Ubuntu-24.04\home\sam",
    r"\\wsl$\Ubuntu-24.04\home\sam",
    r"\\wsl.localhost\Debian\home\sam",
    r"D:\data",
    r"relative\x",
    r"C:\Users\SamTyler\..\Public",
];

#[cfg(windows)]
const LINUX_CASES: &[&str] = &[
    "/home/sam",
    "/home/sam/workspace-other/acpilot",
    "/",
    "/mnt/c",
    "/mnt/c/Users/SamTyler",
    "/mnt/d/a b/x",
    "/tmp/a b/ü/文件",
    "relative/x",
];

fn main() {
    match Spec::parse(std::env::args().skip(1)) {
        Ok(spec) => {
            if let Err(error) = spec.run() {
                eprintln!("wsl-helper: {error:#}");
                std::process::exit(1);
            }
        }
        Err(message) => {
            eprintln!("wsl-helper: {message}\n\n{}", Spec::USAGE);
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Bench,
    Paths,
    Distros,
    Install,
    Workspace,
}

struct Spec {
    command: Command,
    /// None means "whatever distro a bare `wsl.exe` starts".
    distro: Option<String>,
    helper: String,
    iterations: usize,
    oracle: bool,
    conpty: bool,
    /// Windows path to open as a workspace; `None` means the current directory.
    workspace: Option<String>,
    /// File (relative to the workspace) to read 200 times both ways, for the
    /// `\\wsl.localhost` vs in-distro comparison.
    read: Option<String>,
    /// WSL1 and WSL2 are different products (translated syscalls vs a kernel
    /// VM), so a CI run that means to describe the shipped path must not accept
    /// a WSL1 distro. Off by default: the probe still runs on whatever distro a
    /// developer has, and the report always records which one it was.
    require_wsl2: bool,
    json: Option<PathBuf>,
}

impl Spec {
    const USAGE: &'static str = "\
usage: wsl-helper [bench|workspace|paths|distros|install] [options]

  bench               latency table + one round trip per mode (default)
  workspace           open a Windows path as a WSL workspace: map it, start a
                      helper in that directory, and prove the session is there
  paths               the translation table for this distro
  distros             distro names as decoded from `wsl.exe -l -q`
  install             compile helper/acpilotd-helper.rs inside the distro and
                      install it to ~/.local/bin/acpilotd-helper

  --distro <name>     distro to use (default: the one a bare wsl.exe starts)
  --helper <path>     helper path inside the distro
                      (default: ~/.local/bin/acpilotd-helper)
  --iterations <n>    samples per one-shot measurement (default 10)
  --workspace <path>  workspace to open (default: the current directory)
  --read <file>       with workspace: time reads of <file> over UNC and in-distro
  --oracle            also run wslpath and show it next to our answer
  --require-wsl2      fail unless the distro reports VERSION 2 in `wsl -l -v`
  --no-conpty         skip the ConPTY round trip
  --json <path>       write the full report here";

    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut spec = Self {
            command: Command::Bench,
            distro: None,
            helper: DEFAULT_HELPER.to_string(),
            iterations: 10,
            oracle: false,
            conpty: true,
            workspace: None,
            read: None,
            require_wsl2: false,
            json: None,
        };

        let mut args = args.peekable();
        let mut command_seen = false;
        while let Some(arg) = args.next() {
            let mut value = |name: &str| -> Result<String, String> {
                args.next().ok_or_else(|| format!("{name} needs a value"))
            };
            match arg.as_str() {
                "bench" | "paths" | "distros" | "install" | "workspace" if !command_seen => {
                    spec.command = match arg.as_str() {
                        "bench" => Command::Bench,
                        "paths" => Command::Paths,
                        "distros" => Command::Distros,
                        "install" => Command::Install,
                        _ => Command::Workspace,
                    };
                    command_seen = true;
                }
                "--workspace" => spec.workspace = Some(value("--workspace")?),
                "--read" => spec.read = Some(value("--read")?),
                "--distro" => spec.distro = Some(value("--distro")?),
                "--helper" => spec.helper = value("--helper")?,
                "--iterations" => {
                    spec.iterations = value("--iterations")?
                        .parse()
                        .map_err(|_| "--iterations must be a number".to_string())?;
                    if spec.iterations == 0 {
                        return Err("--iterations must be at least 1".to_string());
                    }
                }
                "--oracle" => spec.oracle = true,
                "--require-wsl2" => spec.require_wsl2 = true,
                "--no-conpty" => spec.conpty = false,
                "--json" => spec.json = Some(PathBuf::from(value("--json")?)),
                "-h" | "--help" => return Err("help".to_string()),
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(spec)
    }

    fn run(&self) -> anyhow::Result<()> {
        #[cfg(not(windows))]
        {
            anyhow::bail!("this probe drives wsl.exe and only runs on Windows");
        }
        #[cfg(windows)]
        {
            match self.command {
                Command::Bench => self.bench(),
                Command::Paths => self.paths_only(),
                Command::Distros => self.distros(),
                Command::Install => self.install(),
                Command::Workspace => self.workspace(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[cfg(windows)]
impl Spec {
    fn distro(&self) -> anyhow::Result<String> {
        match &self.distro {
            Some(distro) => Ok(distro.clone()),
            None => wsl::default_distro(),
        }
    }

    /// The product view of discovery: what a user with WSL sees, and what a
    /// user without it sees. The second case must be silence, so it exits 0.
    fn distros(&self) -> anyhow::Result<()> {
        let start = Instant::now();
        let all = discovery::names(&wsl::wsl_exe(), &["-l", "-q"]);
        let elapsed = start.elapsed();
        let workspaces = discovery::workspaces(all.clone());
        if all.is_empty() {
            println!(
                "no WSL: wsl.exe is missing, failed, or listed nothing ({:.1} ms).",
                ms(elapsed)
            );
            println!(
                "\nThis is the whole message a machine without WSL gets: no host, no\n\
                 prompt, no error. The exit code is 0, because absence is not a failure."
            );
            return Ok(());
        }
        println!(
            "wsl.exe -l -q decoded {} distro(s) in {:.1} ms; usable as workspaces: {}",
            all.len(),
            ms(elapsed),
            workspaces.len()
        );
        for distro in &workspaces {
            println!("  {distro}");
        }
        for distro in all.iter().filter(|name| !discovery::is_workspace(name)) {
            println!("  {distro}   (filtered: a container runtime, not a workspace)");
        }
        println!(
            "\nThe bytes are UTF-16LE: every second byte is NUL. A reader that treats\n\
             them as UTF-8 sees one long name with NULs inside it."
        );
        Ok(())
    }

    fn install(&self) -> anyhow::Result<()> {
        let distro = self.distro()?;
        let source = include_str!("../helper/acpilotd-helper.rs");

        // Write the source through wsl.exe's stdin. This is also the cheapest
        // proof that stdin is byte-exact: the file compiles.
        // The file name becomes the crate name, so it cannot be `.acpilotd-helper.rs`:
        // rustc rejects the leading dot and the dashes.
        let write = wsl::run_with_stdin(
            Some(&distro),
            "sh",
            &[
                "-c",
                "mkdir -p ~/.local/bin && cat > /tmp/acpilotd_helper_build.rs && \
                 echo wrote $(wc -c < /tmp/acpilotd_helper_build.rs) bytes",
            ],
            Some(source.as_bytes()),
        )?;
        write.check("write helper source")?;

        let build = wsl::run(
            Some(&distro),
            "sh",
            &[
                "-c",
                "rustc -O /tmp/acpilotd_helper_build.rs -o ~/.local/bin/acpilotd-helper \
                 && ~/.local/bin/acpilotd-helper --version",
            ],
        )?;
        build.check("rustc the helper")?;
        if !build.stderr.trim().is_empty() {
            eprintln!("{}", build.stderr.trim());
        }
        print!("{}", build.stdout_lossy());
        Ok(())
    }

    fn paths_only(&self) -> anyhow::Result<()> {
        let distro = self.distro()?;
        let mounted = wsl::mounted_drives(&distro)?;
        let map = WslPathMap::new(&distro, mounted.iter().copied());
        let rows = self.path_rows(&map, &distro)?;
        print_paths(&map, &rows, self.oracle);
        Ok(())
    }

    /// The user story this probe exists for: the code lives in WSL, the editor
    /// and the app run on Windows.
    ///
    /// The app is handed one thing — a path. Here that path is a
    /// `\\wsl.localhost\<distro>\...` folder the user opened. The command finds
    /// the host it belongs to, starts the helper **in that directory inside the
    /// distro**, and asserts the helper's cwd is the mapped path. Everything
    /// after that — agent, LSP, git, PTY — runs where the files are; only JSON
    /// lines cross the boundary.
    fn workspace(&self) -> anyhow::Result<()> {
        let windows = match &self.workspace {
            Some(path) => path.clone(),
            None => std::env::current_dir()
                .map_err(|error| anyhow::anyhow!("current directory: {error}"))?
                .display()
                .to_string(),
        };
        let distro = match &self.distro {
            Some(distro) => distro.clone(),
            // A UNC path names its own distro, which is how "open this folder"
            // becomes "acpilot host wsl://Ubuntu-24.04" with no question asked.
            None => match paths::distro_from_unc(&windows) {
                Some(distro) => distro,
                None => wsl::default_distro()?,
            },
        };

        let mounted = wsl::mounted_drives(&distro)?;
        let map = WslPathMap::new(&distro, mounted.iter().copied());
        let linux = map
            .to_linux(&windows)
            .map_err(|error| anyhow::anyhow!("{windows}: {error}"))?;

        println!("opened      {windows}");
        println!("host        wsl://{distro}");
        println!("linux       {linux}");

        // A `--cd` into a directory that does not exist fails inside wsl.exe
        // with a message about the path, not about the project; check first.
        let exists = wsl::run(Some(&distro), "test", &["-d", &linux])?;
        if !exists.status.success() {
            anyhow::bail!("{linux} is not a directory in {distro}");
        }

        let helper = wsl::resolve_helper(&distro, &self.helper)?;
        let start = Instant::now();
        let mut session =
            wsl::Session::spawn_with(Some(&distro), &helper, Some(&linux), &["--stdio"])?;
        let (line, first) = session.roundtrip(&request(1), Duration::from_secs(20))?;
        let first_total = ms(start.elapsed());
        let expected = format!("\"cwd\":\"{linux}\"");
        if !line.contains(&expected) {
            session.kill();
            anyhow::bail!("helper started somewhere else: {line}");
        }

        let mut warm_ms = Vec::new();
        for id in 1..=20u64 {
            let (_, elapsed) = session.roundtrip(&request(id), Duration::from_secs(5))?;
            warm_ms.push(ms(elapsed));
        }
        session.finish()?;
        let warm = Stats::new(&warm_ms);
        println!(
            "session     {first_total:.1} ms to start in that directory (+{:.1} ms for the first \
             message), then {:.1} ms per message (p50, n={})",
            ms(first),
            warm.p50_ms,
            warm.n
        );

        if let Some(read) = &self.read {
            self.read_cost(&distro, &windows, &linux, read)?;
        }
        Ok(())
    }

    /// The README claims "do not edit WSL files over `\\wsl.localhost` UNC paths
    /// in the hot path". This is the number behind that sentence: the same file,
    /// read `READS` times through the share Windows handed us, and from one
    /// in-distro process so no spawn is hidden in either total.
    fn read_cost(
        &self,
        distro: &str,
        windows_workspace: &str,
        linux_workspace: &str,
        read: &str,
    ) -> anyhow::Result<()> {
        const READS: usize = 200;
        let relative = read.trim_start_matches(['/', '\\']).replace('\\', "/");
        let windows_file = PathBuf::from(windows_workspace).join(&relative);
        let linux_file = format!("{}/{relative}", linux_workspace.trim_end_matches('/'));
        if linux_file.contains('\'') {
            println!("\nread cost   skipped: {linux_file} would need shell quoting");
            return Ok(());
        }

        let bytes = std::fs::metadata(&windows_file)
            .map_err(|error| anyhow::anyhow!("{}: {error}", windows_file.display()))?
            .len();
        let start = Instant::now();
        for _ in 0..READS {
            std::fs::read(&windows_file)
                .map_err(|error| anyhow::anyhow!("{}: {error}", windows_file.display()))?;
        }
        let unc = start.elapsed();

        // `$(<file)` is a bash builtin (no fork per read) and the script times
        // itself with `date`, so the number excludes the ~130 ms `wsl.exe`
        // startup that would otherwise be spread over 200 reads and flatter the
        // UNC side enormously.
        let script = format!(
            "f='{linux_file}'; i=0; s=$(date +%s%N); while [ \"$i\" -lt {READS} ]; do \
             v=$(<\"$f\"); i=$((i+1)); done; e=$(date +%s%N); echo $(( (e-s)/1000 ))"
        );
        let run = wsl::run(Some(distro), "bash", &["-c", &script])?;
        run.check("in-distro read loop")?;
        let output = run.stdout_lossy();
        let micros: f64 = output.trim().parse().map_err(|_| {
            anyhow::anyhow!("in-distro loop printed {output:?} instead of microseconds")
        })?;
        let in_distro_ms = micros / 1_000.0;

        let per_read_us = |elapsed_ms: f64| elapsed_ms * 1_000.0 / READS as f64;
        println!(
            "read cost   {READS}× {bytes} B: UNC {:.1} ms ({:.0} µs/read), in-distro {:.1} ms \
             ({:.0} µs/read), {:.0}× — why the helper runs in the distro and only JSON crosses",
            ms(unc),
            per_read_us(ms(unc)),
            in_distro_ms,
            per_read_us(in_distro_ms),
            ms(unc) / in_distro_ms.max(0.001)
        );
        Ok(())
    }

    fn bench(&self) -> anyhow::Result<()> {
        let distro = self.distro()?;
        // Which WSL this is decides whether the numbers describe the product at
        // all, so it is established before the first measurement, not after.
        let wsl_version = match wsl::distro_version(&distro)? {
            Some(version) => version,
            None => anyhow::bail!("{distro} is not listed by `wsl.exe -l -v`; pass --distro"),
        };
        if self.require_wsl2 {
            listing::check_wsl2(&distro, wsl_version).map_err(anyhow::Error::msg)?;
        }
        // `-e` takes a program path, not a shell word: `~` has to be expanded here.
        let helper = wsl::resolve_helper(&distro, &self.helper)?;
        let mounted = wsl::mounted_drives(&distro)?;
        let map = WslPathMap::new(&distro, mounted.iter().copied());
        let version = wsl::run(Some(&distro), "uname", &["-r"])?;
        version.check("uname -r")?;

        println!("distro     {distro}");
        println!("wsl        WSL{wsl_version}");
        println!(
            "mounted    {}",
            mounted
                .iter()
                .map(|d| format!("{d}:"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!("helper     {helper}");
        println!("wsl.exe    {}", wsl::wsl_exe().display());

        // 1. Enumeration: no distro is started, so this is the cheap end.
        let mut list_ms = Vec::new();
        let mut distros = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            distros = wsl::list_distros()?;
            list_ms.push(ms(start.elapsed()));
        }
        let list = Stats::new(&list_ms);

        // 2. One call that starts nothing new: the floor for spawn + relay.
        let mut startup_ms = Vec::new();
        for _ in 0..self.iterations {
            let run = wsl::run(Some(&distro), "true", &[])?;
            run.check("true")?;
            startup_ms.push(ms(run.elapsed));
        }
        let startup = Stats::new(&startup_ms);

        // 3. The real thing: spawn the helper, send one line, read one line, kill.
        let mut oneshot_ms = Vec::new();
        let mut oneshot_total_ms = Vec::new();
        let mut first_response = String::new();
        for index in 0..self.iterations {
            let start = Instant::now();
            let mut session = wsl::Session::spawn(Some(&distro), &helper, &["--stdio"])?;
            let (line, _) = session.roundtrip(&request(index as u64), Duration::from_secs(20))?;
            if !line.contains(&format!("\"id\":{index}")) {
                anyhow::bail!("one-shot response did not echo id {index}: {line}");
            }
            oneshot_ms.push(ms(start.elapsed()));
            first_response = line;
            // The other half of "one call": the helper exits on stdin EOF, so the
            // caller waits for process teardown too.
            session.finish()?;
            oneshot_total_ms.push(ms(start.elapsed()));
        }
        let oneshot = Stats::new(&oneshot_ms);
        let oneshot_total = Stats::new(&oneshot_total_ms);

        // 4. Persistent session: pay the spawn once, then measure the message.
        let start = Instant::now();
        let mut session = wsl::Session::spawn(Some(&distro), &helper, &["--stdio"])?;
        let spawn_ms = ms(session.spawned_in);
        println!("spawn      {}", session.command_line);
        let (hello, first_call) = session.roundtrip(&request(0), Duration::from_secs(20))?;
        let first_total_ms = ms(start.elapsed());
        let first_ms = ms(first_call);
        let mut warm_ms = Vec::new();
        for index in 1..=30u64 {
            let (line, elapsed) = session.roundtrip(&request(index), Duration::from_secs(5))?;
            if !line.contains(&format!("\"id\":{index}")) {
                anyhow::bail!("response did not echo id {index}: {line}");
            }
            warm_ms.push(ms(elapsed));
        }

        // 5. One large line on the *warm* session: a transcript event is not a
        // ping, and this separates transfer from spawn.
        let padding = "x".repeat(1 << 20);
        let big = format!(
            r#"{{"jsonrpc":"2.0","id":4242,"method":"ping","params":{{"padding":"{padding}"}}}}"#
        );
        let big_bytes = big.len();
        let start = Instant::now();
        let (line, _) = session.roundtrip(&big, Duration::from_secs(30))?;
        let large = LargeLine {
            bytes: big_bytes,
            elapsed_ms: ms(start.elapsed()),
            intact: line.contains("\"id\":4242")
                && line.contains(&format!("\"request_bytes\":{big_bytes}")),
        };
        session.finish()?;
        let warm = Stats::new(&warm_ms);

        // What the helper reports as cwd when `--cd ~` is *not* passed: the distro
        // inherits whatever wsl.exe translates the Windows cwd to, which depends on
        // where acpilotd itself was started. One more reason to always pass --cd.
        let mut inherited = wsl::Session::spawn_with(Some(&distro), &helper, None, &["--stdio"])?;
        let (inherited_cwd, _) = inherited.roundtrip(&request(99), Duration::from_secs(20))?;
        inherited.kill();

        // 6. ConPTY: same command, pseudo console instead of pipes.
        let conpty = if self.conpty {
            let run = wsl::conpty_roundtrip(
                Some(&distro),
                &helper,
                &request(7),
                Duration::from_secs(10),
            )?;
            Some(ConPtyReport {
                response_seen: find(&run.bytes, b"\"result\""),
                echoed_request: find(&run.bytes, request(7).as_bytes()),
                crlf: find(&run.bytes, b"\r\n"),
                elapsed_ms: ms(run.elapsed),
                bytes: run.bytes.len(),
                raw: escape(&run.bytes),
                queries_answered: run.queries_answered,
            })
        } else {
            None
        };

        // 6. Translation.
        let rows = self.path_rows(&map, &distro)?;
        print_paths(&map, &rows, self.oracle);

        println!();
        print_stats("list    ", &list);
        print_stats("one call", &startup);
        print_stats("1-shot  ", &oneshot);
        print_stats("  +exit ", &oneshot_total);
        print_stats("messages", &warm);
        println!(
            "large line {} bytes on the warm session in {:.1} ms, intact={}",
            large.bytes, large.elapsed_ms, large.intact
        );
        println!(
            "CreateProcess {spawn_ms:.1} ms; spawn → first response {first_total_ms:.1} ms \
             (of which the message {first_ms:.1} ms); then {:.1} ms per message (p50)",
            warm.p50_ms
        );
        println!("cwd without --cd: {inherited_cwd}");
        if let Some(conpty) = &conpty {
            println!(
                "ConPTY  response={} echoed_request={} crlf={} queries_answered={} \
                 in {:.1} ms, {} bytes",
                conpty.response_seen,
                conpty.echoed_request,
                conpty.crlf,
                conpty.queries_answered,
                conpty.elapsed_ms,
                conpty.bytes
            );
            println!("raw     {}", conpty.raw);
        }
        println!("first response: {first_response}");

        if let Some(path) = &self.json {
            let report = Report {
                distro: distro.clone(),
                wsl_version,
                kernel: version.stdout_lossy().trim().to_string(),
                mounted,
                wsl_exe: wsl::wsl_exe().display().to_string(),
                helper: helper.clone(),
                distros,
                list,
                startup,
                oneshot,
                oneshot_total,
                large,
                spawn_ms,
                first_total_ms,
                first_message_ms: first_ms,
                warm,
                hello,
                inherited_cwd,
                conpty,
                paths: rows,
            };
            let json = serde_json::to_string_pretty(&report)?;
            std::fs::write(path, json)
                .map_err(|error| anyhow::anyhow!("write {}: {error}", path.display()))?;
            println!("\nreport: {}", path.display());
        }
        Ok(())
    }

    /// The translation table, optionally with `wslpath` as an oracle. The oracle
    /// is what makes the mapping a measurement instead of an opinion; the
    /// differences it shows are deliberate and documented next to the rows.
    fn path_rows(&self, map: &WslPathMap, distro: &str) -> anyhow::Result<Vec<PathRow>> {
        let mut rows = Vec::new();
        for case in WINDOWS_CASES {
            let ours = map.to_linux(case);
            let oracle = if self.oracle {
                let run = wsl::run(Some(distro), "wslpath", &["-u", case])?;
                if run.status.success() {
                    Some(run.stdout_lossy().trim().to_string())
                } else {
                    Some(format!("error: {}", run.stderr.trim()))
                }
            } else {
                None
            };
            rows.push(PathRow::new(case, "windows → linux", ours, oracle));
        }
        for case in LINUX_CASES {
            let ours = map.to_windows(case);
            let oracle = if self.oracle {
                let run = wsl::run(Some(distro), "wslpath", &["-w", case])?;
                if run.status.success() {
                    Some(run.stdout_lossy().trim().to_string())
                } else {
                    Some(format!("error: {}", run.stderr.trim()))
                }
            } else {
                None
            };
            rows.push(PathRow::new(case, "linux → windows", ours, oracle));
        }
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// reporting
// ---------------------------------------------------------------------------

fn ms(elapsed: std::time::Duration) -> f64 {
    elapsed.as_secs_f64() * 1_000.0
}

#[cfg(windows)]
fn print_paths(map: &WslPathMap, rows: &[PathRow], oracle: bool) {
    println!(
        "\npaths for {} (mounted: {})",
        map.distro(),
        map.mounted_drives()
            .iter()
            .map(|d| format!("{d}:"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for row in rows {
        let ours = row
            .ours
            .clone()
            .unwrap_or_else(|| format!("<{}>", row.error.clone().unwrap_or_default()));
        if oracle {
            let verdict = match &row.oracle {
                Some(oracle) if oracle == &ours => "same",
                Some(_) => "differs",
                None => "",
            };
            let oracle = row.oracle.clone().unwrap_or_default();
            println!(
                "  {:<50} → {:<46} wslpath {:<46} {verdict}",
                row.input, ours, oracle
            );
        } else {
            println!("  {:<50} → {ours}", row.input);
        }
    }
}

#[cfg(windows)]
fn print_stats(label: &str, stats: &Stats) {
    println!(
        "{label} n={:<3} min {:>7.1}  p50 {:>7.1}  p90 {:>7.1}  max {:>7.1} ms",
        stats.n, stats.min_ms, stats.p50_ms, stats.p90_ms, stats.max_ms
    );
}

#[cfg(windows)]
fn request(id: u64) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping","params":{{"note":"P0-5"}}}}"#)
}

#[cfg(windows)]
fn find(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Printable form of raw ConPTY bytes: `\r`, `\n`, `\e` and NUL are the whole
/// point of looking, so they must not vanish in a terminal.
#[cfg(windows)]
fn escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes.iter().take(400) {
        match byte {
            b'\r' => out.push_str("\\r"),
            b'\n' => out.push_str("\\n\n"),
            b'\t' => out.push_str("\\t"),
            0x1b => out.push_str("\\e"),
            0x00 => out.push_str("\\0"),
            0x20..=0x7e => out.push(byte as char),
            other => out.push_str(&format!("\\x{other:02x}")),
        }
    }
    out
}

#[cfg(windows)]
#[derive(Serialize)]
struct Stats {
    n: usize,
    min_ms: f64,
    p50_ms: f64,
    p90_ms: f64,
    max_ms: f64,
}

#[cfg(windows)]
impl Stats {
    fn new(samples: &[f64]) -> Self {
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let pick = |q: f64| -> f64 {
            if sorted.is_empty() {
                return 0.0;
            }
            let index = ((sorted.len() - 1) as f64 * q).round() as usize;
            sorted[index]
        };
        Self {
            n: sorted.len(),
            min_ms: sorted.first().copied().unwrap_or(0.0),
            p50_ms: pick(0.5),
            p90_ms: pick(0.9),
            max_ms: sorted.last().copied().unwrap_or(0.0),
        }
    }
}

#[cfg(windows)]
#[derive(Serialize)]
struct PathRow {
    input: String,
    direction: String,
    /// `Some` is the translated path, `None` means `error` says why it was
    /// refused. The distinction is the point: an unmounted drive must fail loudly.
    ours: Option<String>,
    error: Option<String>,
    oracle: Option<String>,
}

#[cfg(windows)]
impl PathRow {
    fn new(
        input: &str,
        direction: &str,
        ours: Result<String, paths::PathError>,
        oracle: Option<String>,
    ) -> Self {
        let (ours, error) = match ours {
            Ok(path) => (Some(path), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            input: input.to_string(),
            direction: direction.to_string(),
            ours,
            error,
            oracle,
        }
    }
}

#[cfg(windows)]
#[derive(Serialize)]
struct LargeLine {
    bytes: usize,
    elapsed_ms: f64,
    intact: bool,
}

#[cfg(windows)]
#[derive(Serialize)]
struct ConPtyReport {
    response_seen: bool,
    echoed_request: bool,
    crlf: bool,
    elapsed_ms: f64,
    bytes: usize,
    raw: String,
    queries_answered: usize,
}

#[cfg(windows)]
#[derive(Serialize)]
struct Report {
    distro: String,
    /// 1 or 2, straight from `wsl.exe -l -v`; a report is only comparable with
    /// one measured on the same version.
    wsl_version: u32,
    kernel: String,
    mounted: Vec<char>,
    wsl_exe: String,
    helper: String,
    distros: Vec<String>,
    list: Stats,
    startup: Stats,
    oneshot: Stats,
    oneshot_total: Stats,
    large: LargeLine,
    spawn_ms: f64,
    first_total_ms: f64,
    first_message_ms: f64,
    warm: Stats,
    hello: String,
    inherited_cwd: String,
    conpty: Option<ConPtyReport>,
    paths: Vec<PathRow>,
}
