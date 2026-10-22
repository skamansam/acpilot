//! Talking to `wsl.exe` from Windows.
//!
//! The rules this module encodes were measured, not assumed:
//!
//! * **`-- <command>` goes through the user's shell, `-e/--exec` does not.**
//!   `wsl -d D -- wslpath -u 'C:\Users'` printed `wslpath: C:Users` — bash ate
//!   the backslashes. `wsl -d D -e wslpath -u 'C:\Users'` printed the right
//!   answer. The helper must be started with `-e`, or every Windows path it is
//!   handed is silently mangled.
//! * **`-l -q` is UTF-16LE with CRLF**, NUL-interleaved; a line reader that does
//!   not decode it yields garbage (or one long name). `-l -v` is UTF-16LE too,
//!   and its `VERSION` column is the only local way to tell WSL1 from WSL2.
//! * **A helper is a long-lived process, not a call.** One spawn serves many JSON
//!   lines; the per-message cost is then milliseconds, while the spawn is hundreds.
//! * **`--cd ~`** is the working-directory flag: without it the distro inherits
//!   whatever `wsl.exe` decides the host cwd translates to.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use crate::listing::{decode_listing, parse_verbose};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Do not flash a console window for a child that is a pipe endpoint. A daemon
/// that spawns helpers on demand would otherwise blink one per session.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `%SystemRoot%\System32\wsl.exe`, not `wsl` from `PATH`: users install shims
/// (WinGet, Scoop, an old Store alias) and the version then decides the
/// `--cd`/`-e` behaviour behind our back.
pub fn wsl_exe() -> PathBuf {
    // Probe-only seam: point at another program to exercise the "no WSL" path on
    // a machine that has WSL. The product never reads this.
    if let Some(path) = std::env::var_os("ACPILOT_WSL_EXE") {
        return PathBuf::from(path);
    }
    let system_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .unwrap_or_else(|| "C:\\Windows".into());
    let candidate = PathBuf::from(system_root).join("System32").join("wsl.exe");
    if candidate.is_file() {
        candidate
    } else {
        PathBuf::from("wsl.exe")
    }
}

/// One `wsl.exe` listing, raw. `-l -q` and `-l -v` both write UTF-16LE; the
/// decoding and parsing live in `listing.rs` so they are tested on every OS.
fn listing(args: &[&str]) -> Result<Vec<u8>> {
    let mut command = command(None);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let output = command
        .output()
        .with_context(|| format!("wsl.exe {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "wsl.exe {} failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// One finished `wsl.exe` call.
pub struct Run {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub elapsed: Duration,
}

impl Run {
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn check(&self, what: &str) -> Result<&Self> {
        if self.status.success() {
            return Ok(self);
        }
        bail!("{what} failed with {}: {}", self.status, self.stderr.trim())
    }
}

fn command(distro: Option<&str>) -> Command {
    let mut command = Command::new(wsl_exe());
    if let Some(distro) = distro {
        command.args(["-d", distro]);
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// One call, no shell, with an optional stdin payload.
pub fn run_with_stdin(
    distro: Option<&str>,
    program: &str,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<Run> {
    let mut command = command(distro);
    command
        .args(["-e", program])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let start = Instant::now();
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {}", wsl_exe().display()))?;
    if let Some(payload) = stdin {
        let mut pipe = child.stdin.take().context("child stdin")?;
        pipe.write_all(payload).context("write stdin")?;
        // Dropping closes it, which is what tells `cat`/`sh` that input ended.
    }
    let output = child.wait_with_output().context("wait for wsl.exe")?;
    Ok(Run {
        status: output.status,
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        elapsed: start.elapsed(),
    })
}

pub fn run(distro: Option<&str>, program: &str, args: &[&str]) -> Result<Run> {
    run_with_stdin(distro, program, args, None)
}

pub fn list_distros() -> Result<Vec<String>> {
    // `-l -q` is the cheap enumeration: no distro is started.
    let distros = decode_listing(&listing(&["-l", "-q"])?);
    if distros.is_empty() {
        bail!("wsl.exe -l -q printed no distros");
    }
    Ok(distros)
}

/// The WSL version of one distro, from `wsl.exe -l -v`.
///
/// `Ok(None)` means the distro is not registered at all, which is a different
/// failure from "it is registered as WSL1" and deserves its own message.
/// Matching is case-insensitive because `wsl.exe` accepts a name that differs
/// only in case.
pub fn distro_version(distro: &str) -> Result<Option<u32>> {
    let rows = parse_verbose(&listing(&["-l", "-v"])?);
    Ok(rows
        .into_iter()
        .find(|row| row.name.eq_ignore_ascii_case(distro))
        .map(|row| row.version))
}

/// The Linux home directory. Needed because `-e` does **not** expand `~`: a
/// `-e ~/.local/bin/helper` is handed to `execvpe` literally and dies with
/// `execvpe(~/.local/bin/...) failed`. `--cd ~` is expanded by `wsl.exe`, the
/// program path is not.
pub fn home_dir(distro: &str) -> Result<String> {
    let run = run(Some(distro), "printenv", &["HOME"])?;
    run.check("printenv HOME")?;
    let home = run.stdout_lossy().trim().to_string();
    if home.is_empty() {
        bail!("HOME is empty in {distro}");
    }
    Ok(home)
}

/// Expand a leading `~/` the way `-e` will not.
pub fn resolve_helper(distro: &str, helper: &str) -> Result<String> {
    match helper.strip_prefix("~/") {
        Some(rest) => Ok(format!(
            "{}/{rest}",
            home_dir(distro)?.trim_end_matches('/')
        )),
        None => Ok(helper.to_string()),
    }
}

/// The distro a plain `wsl.exe` would start, straight from the running distro's
/// environment instead of guessing from the `*` in `-l -v`.
pub fn default_distro() -> Result<String> {
    let run = run(None, "printenv", &["WSL_DISTRO_NAME"]).context("query default distro")?;
    run.check("printenv WSL_DISTRO_NAME")?;
    let name = run.stdout_lossy().trim().to_string();
    if name.is_empty() {
        bail!("WSL_DISTRO_NAME is empty; pass --distro");
    }
    Ok(name)
}

/// Drives that actually exist under `/mnt`. `wslpath` translates only these;
/// everything else has no Linux path (see `paths.rs`).
pub fn mounted_drives(distro: &str) -> Result<Vec<char>> {
    let run = run(Some(distro), "ls", &["-1", "/mnt"])?;
    run.check("ls /mnt")?;
    let drives = run
        .stdout_lossy()
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.len() == 1 && line.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        })
        .filter_map(|line| line.chars().next())
        .collect::<Vec<_>>();
    Ok(drives)
}

/// A helper process that answers one JSON line per line.
pub struct Session {
    child: Child,
    /// `Option` so `finish` can close the pipe and let the helper exit on EOF;
    /// a killed process never tests whether the helper's own loop is clean.
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    pub spawned_in: Duration,
    pub command_line: String,
}

impl Session {
    /// `wsl.exe -d <d> --cd ~ -e <helper> --stdio`. `-e` because `--` would route
    /// the command line through the user's shell and eat backslashes.
    pub fn spawn(distro: Option<&str>, helper: &str, args: &[&str]) -> Result<Self> {
        Self::spawn_with(distro, helper, Some("~"), args)
    }

    /// `cd: None` is the control case: what the helper inherits when nobody
    /// passes `--cd`.
    pub fn spawn_with(
        distro: Option<&str>,
        helper: &str,
        cd: Option<&str>,
        args: &[&str],
    ) -> Result<Self> {
        let mut command = command(distro);
        if let Some(cd) = cd {
            command.args(["--cd", cd]);
        }
        command
            .args(["-e", helper])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let start = Instant::now();
        let mut child = command.spawn().context("spawn wsl.exe helper")?;
        let stdin = child.stdin.take().context("helper stdin")?;
        let stdout = child.stdout.take().context("helper stdout")?;
        let spawned_in = start.elapsed();

        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let command_line = format!(
            "{} -d {} {} -e {} {}",
            wsl_exe().display(),
            distro.unwrap_or("<default>"),
            cd.map(|cd| format!("--cd {cd} ")).unwrap_or_default(),
            helper,
            args.join(" ")
        );
        Ok(Self {
            child,
            stdin: Some(stdin),
            lines,
            spawned_in,
            command_line,
        })
    }

    /// Write one line, wait for one line. The clock starts before the write so the
    /// number includes the pipe round trip on both sides, not just our waiting.
    pub fn roundtrip(&mut self, request: &str, timeout: Duration) -> Result<(String, Duration)> {
        let start = Instant::now();
        let stdin = self.stdin.as_mut().context("session is finished")?;
        writeln!(stdin, "{request}").context("write request")?;
        stdin.flush().context("flush request")?;
        let line = self
            .lines
            .recv_timeout(timeout)
            .context("helper did not answer before the timeout")?;
        Ok((line, start.elapsed()))
    }

    /// Close stdin and wait for the helper to exit on EOF. The helper's contract
    /// says it does; this is how the probe proves it.
    pub fn finish(&mut self) -> Result<()> {
        drop(self.stdin.take());
        let status = self.child.wait().context("wait for helper exit")?;
        if !status.success() {
            bail!("helper exited with {status} on stdin EOF");
        }
        Ok(())
    }

    pub fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// ConPTY probe: the same wsl.exe, the same helper, but the child's stdio is a
/// pseudo console instead of two pipes. This is the half P0-2 could not measure.
///
/// The first run proved the point the hard way: nothing happened for 10 s, and
/// the only bytes that came back were `ESC [ 6 n`, the cursor-position request.
/// A bare ConPTY does not answer it, so `wsl.exe` waits before it starts the
/// helper — the same terminal-query problem P0-2 hit with the TUIs. The probe now
/// answers DSR and DA1 and records how many queries that took.
pub struct ConPtyRun {
    pub bytes: Vec<u8>,
    pub elapsed: Duration,
    pub queries_answered: usize,
}

pub fn conpty_roundtrip(
    distro: Option<&str>,
    helper: &str,
    request: &str,
    settle: Duration,
) -> Result<ConPtyRun> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open ConPTY")?;

    let mut command = CommandBuilder::new(wsl_exe());
    if let Some(distro) = distro {
        command.arg("-d");
        command.arg(distro);
    }
    command.args(["--cd", "~", "-e", helper, "--stdio"]);
    let mut child = pair
        .slave
        .spawn_command(command)
        .context("spawn helper under ConPTY")?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("ConPTY reader")?;
    let mut writer = pair.master.take_writer().context("ConPTY writer")?;

    let start = Instant::now();
    let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&bytes);
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().expect("buffer").extend_from_slice(&buffer[..n]),
            }
        }
    });

    // ConPTY echoes the request and adds its own cursor noise, so wait for the
    // JSON response to appear rather than for the first newline.
    let deadline = Instant::now() + settle;
    let mut request_sent = false;
    let mut answered_dsr = 0;
    let mut answered_da1 = 0;
    let mut queries_answered = 0;
    loop {
        let (dsr, da1, response) = {
            let buffer = bytes.lock().expect("buffer");
            (
                count_of(&buffer, b"\x1b[6n"),
                count_of(&buffer, b"\x1b[c"),
                buffer.windows(8).any(|window| window == b"\"result\""),
            )
        };
        if response {
            break;
        }
        if dsr > answered_dsr {
            // Cursor position report: row 1, column 1 is enough for a program
            // that only wants to know whether anyone is home.
            writer.write_all(b"\x1b[1;1R")?;
            writer.flush()?;
            answered_dsr = dsr;
            queries_answered += 1;
        } else if da1 > answered_da1 {
            writer.write_all(b"\x1b[?1;2c")?;
            writer.flush()?;
            answered_da1 = da1;
            queries_answered += 1;
        }
        if !request_sent && (queries_answered > 0 || start.elapsed() > Duration::from_millis(200)) {
            writeln!(writer, "{request}").context("write to ConPTY")?;
            writer.flush().context("flush ConPTY")?;
            request_sent = true;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let elapsed = start.elapsed();
    let _ = child.kill();
    let _ = child.wait();
    let bytes = bytes.lock().expect("buffer").clone();
    Ok(ConPtyRun {
        bytes,
        elapsed,
        queries_answered,
    })
}

fn count_of(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}
