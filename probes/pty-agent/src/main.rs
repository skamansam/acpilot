//! P0-2 — one PTY driver, two real TUI programs.
//!
//! The probe answers the questions in `TODO.md` §P0-2 and nothing else:
//!
//! * can a TUI program be driven as an agent by writing a prompt and reading the
//!   final screen, or does every program need its own adapter profile?
//! * what breaks with alternate-screen TUIs, colours, and bracketed paste?
//! * what does "turn complete" cost when it is detected from an idle timeout,
//!   and how often would that timeout fire in the middle of an answer?
//!
//! The last question is the one worth measuring rather than arguing about, so the
//! runner evaluates every candidate quiet threshold during the same turn and
//! records the screen each one would have returned. The note in
//! `docs/notes/pty-agents.md` is written from those records.
//!
//! ```bash
//! # what does the program look like before anyone types?
//! cargo run -- --cmd freebuff --observe 4000 --json /tmp/freebuff-probe.json
//!
//! # one turn, sent as a bracketed paste, ended by the 5 s truth-teller
//! cargo run -- --cmd freebuff --prompt 'reply with the single word OK' \
//!     --paste --json /tmp/freebuff-turn.json
//! ```

use std::{
    io::{Read, Write},
    num::NonZeroU16,
    path::PathBuf,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use serde::Serialize;

/// Quiet thresholds evaluated in parallel during every turn. 5,000 ms is also the
/// stand-in for "the turn really is over"; the others are the guesses a real
/// client would make, and the report says which guesses were wrong.
const THRESHOLDS_MS: [u64; 6] = [300, 500, 800, 1_200, 2_000, 5_000];

/// How long "the turn really is over" waits for silence. A streaming answer has
/// gaps of tens of milliseconds; a model that thinks for longer than this makes
/// even the truth-teller wrong, which is exactly why `max_gap_ms` is recorded.
const TRUE_QUIET_MS: u64 = 5_000;

/// `\r`, `\n`, `\t`, `\e` and `\\`, because every useful dialog needs Enter or
/// Escape and a shell quoting layer is already complicated enough.
fn decode_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\u{1b}'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn main() {
    let spec = match Spec::parse(std::env::args().skip(1)) {
        Ok(spec) => spec,
        Err(message) => {
            eprintln!("pty-agent: {message}\n\n{}", Spec::USAGE);
            std::process::exit(2);
        }
    };

    if let Err(error) = spec.run() {
        eprintln!("pty-agent: {error:#}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Submit {
    Cr,
    Lf,
}

impl Submit {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Cr => b"\r",
            Self::Lf => b"\n",
        }
    }
}

#[derive(Debug, Clone)]
enum Quit {
    CtrlD,
    Text(String),
    Kill,
}

impl Quit {
    fn label(&self) -> String {
        match self {
            Self::CtrlD => "ctrl-d".to_string(),
            Self::Text(text) => format!("text:{text}"),
            Self::Kill => "kill".to_string(),
        }
    }
}

struct Spec {
    name: String,
    argv: Vec<String>,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    prompt: Option<String>,
    /// Bytes typed before the prompt, one entry per `--pre`. Both real TUIs gate
    /// the first prompt behind a dialog (trust prompt, "wrong directory" modal),
    /// and a driver that cannot answer it never reaches the input box.
    pre: Vec<String>,
    pre_wait_ms: u64,
    /// A real terminal answers these probes. A bare PTY does not, and the program
    /// on the other end can then sit waiting for a reply that never comes.
    answer_queries: bool,
    submit: Submit,
    /// Gap between the text and the Enter key. Sending both in one write is
    /// treated as a paste by at least one TUI, which swallows the Enter.
    submit_delay_ms: u64,
    paste: bool,
    observe_ms: u64,
    ready_ms: u64,
    settle_ms: u64,
    deadline_ms: u64,
    quit: Quit,
    json: Option<PathBuf>,
    show_screen: bool,
}

impl Spec {
    const USAGE: &'static str = "\
usage: pty-agent --cmd '<program> [args...]' [options]

  --cmd <line>        program to run; split on whitespace
  --name <label>      name used in the report (default: first argv word)
  --cwd <dir>         working directory (default: current directory)
  --cols <n>          terminal width (default: 120)
  --rows <n>          terminal height (default: 40)
  --pre <keys>        send keys before the prompt; \\r \\n \\t \\e are decoded,
                      repeatable (one per startup dialog)
  --pre-wait <ms>     quiet window after each --pre before the next step (default: 900)
  --no-answers        do not answer terminal queries (DA1/DA2, DSR, XTVERSION,
                      kitty keyboard); useful for reproducing a dumb PTY
  --prompt <text>     send this prompt after the program goes quiet
  --paste             wrap the prompt in bracketed-paste markers (\\e[200~ … \\e[201~)
  --submit <cr|lf>    submit key after the prompt (default: cr)
  --submit-delay <ms> wait this long between the text and the Enter (default: 250)
  --observe <ms>      probe: how long to watch before giving up (default: 3000)
  --ready <ms>        quiet period that counts as 'startup finished' (default: 1200)
  --settle <ms>       after the turn, keep reading this long to catch a late answer
                      (default: 3000; 0 disables)
  --deadline <ms>     hard cap on the turn (default: 120000)
  --quit <how>        ctrl-d | kill | text:<literal>   (default: ctrl-d)
  --json <path>       write the full run report here
  --no-screen         do not print the captured screen";

    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut spec = Self {
            name: String::new(),
            argv: Vec::new(),
            cwd: std::env::current_dir().context("current directory")?,
            cols: 120,
            rows: 40,
            prompt: None,
            pre: Vec::new(),
            pre_wait_ms: 900,
            answer_queries: true,
            submit: Submit::Cr,
            submit_delay_ms: 250,
            paste: false,
            observe_ms: 3_000,
            ready_ms: 1_200,
            settle_ms: 3_000,
            deadline_ms: 120_000,
            quit: Quit::CtrlD,
            json: None,
            show_screen: true,
        };

        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let mut value = |name: &str| -> Result<String> {
                args.next().with_context(|| format!("{name} needs a value"))
            };
            match arg.as_str() {
                "--cmd" => {
                    let line = value("--cmd")?;
                    spec.argv = line.split_whitespace().map(str::to_string).collect();
                }
                "--name" => spec.name = value("--name")?,
                "--cwd" => spec.cwd = PathBuf::from(value("--cwd")?),
                "--cols" => spec.cols = value("--cols")?.parse()?,
                "--rows" => spec.rows = value("--rows")?.parse()?,
                "--prompt" => spec.prompt = Some(value("--prompt")?),
                "--pre" => spec.pre.push(decode_escapes(&value("--pre")?)),
                "--pre-wait" => spec.pre_wait_ms = value("--pre-wait")?.parse()?,
                "--no-answers" => spec.answer_queries = false,
                "--paste" => spec.paste = true,
                "--submit-delay" => spec.submit_delay_ms = value("--submit-delay")?.parse()?,
                "--submit" => {
                    spec.submit = match value("--submit")?.as_str() {
                        "cr" => Submit::Cr,
                        "lf" => Submit::Lf,
                        other => bail!("--submit must be cr or lf, not {other}"),
                    }
                }
                "--observe" => spec.observe_ms = value("--observe")?.parse()?,
                "--ready" => spec.ready_ms = value("--ready")?.parse()?,
                "--settle" => spec.settle_ms = value("--settle")?.parse()?,
                "--deadline" => spec.deadline_ms = value("--deadline")?.parse()?,
                "--quit" => {
                    let how = value("--quit")?;
                    spec.quit = match how.as_str() {
                        "ctrl-d" => Quit::CtrlD,
                        "kill" => Quit::Kill,
                        other => match other.strip_prefix("text:") {
                            Some(literal) if !literal.is_empty() => Quit::Text(literal.to_string()),
                            _ => bail!("--quit must be ctrl-d, kill or text:<literal>"),
                        },
                    }
                }
                "--json" => spec.json = Some(PathBuf::from(value("--json")?)),
                "--no-screen" => spec.show_screen = false,
                "-h" | "--help" => {
                    println!("{}", Self::USAGE);
                    std::process::exit(0);
                }
                other => bail!("unknown argument {other}"),
            }
        }

        if spec.argv.is_empty() {
            bail!("--cmd is required");
        }
        if spec.cols == 0 || spec.rows == 0 {
            bail!("--cols and --rows must be at least 1");
        }
        if spec.name.is_empty() {
            spec.name = spec
                .argv
                .first()
                .map(|word| {
                    PathBuf::from(word)
                        .file_name()
                        .map_or_else(|| word.clone(), |name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "program".to_string());
        }
        Ok(spec)
    }

    fn run(self) -> Result<()> {
        let mut agent = PtyAgent::spawn(&self)?;
        let mut report = Report {
            name: self.name.clone(),
            argv: self.argv.clone(),
            cwd: self.cwd.display().to_string(),
            size: (self.cols, self.rows),
            ..Report::default()
        };

        // 1. Wait for the program's first screen to stop changing. This is the
        //    closest thing to a generic "ready" signal: no TUI needs to cooperate.
        let ready_started = agent.elapsed();
        agent.pump_until_quiet(
            Duration::from_millis(self.ready_ms),
            Duration::from_millis(self.observe_ms.max(1)),
        )?;
        report.ready_ms = agent.elapsed().as_millis();
        report.ready_reason = if agent.saw_eof {
            "exit"
        } else if agent.last_data_at >= ready_started {
            "quiet"
        } else {
            "observe-cap"
        };
        report.ready = agent.snapshot();

        // Startup gates: answer each dialog, then wait for the screen to settle
        // again before calling the program ready.
        for keys in &self.pre {
            agent.send(keys.as_bytes())?;
            agent.pump_until_quiet(
                Duration::from_millis(self.pre_wait_ms),
                Duration::from_millis(self.pre_wait_ms.max(1) * 4),
            )?;
        }
        if !self.pre.is_empty() {
            report.gated_ms = agent.elapsed().as_millis();
            report.ready = agent.snapshot();
        }
        report.pre_prompt_stream = agent.stream.clone();

        // 2. Optionally drive one turn.
        if let Some(prompt) = self.prompt.as_deref() {
            let framing = self.framing(prompt);
            report.encoding = Some(if self.paste {
                format!("bracketed-paste + {}", self.submit_name())
            } else {
                self.submit_name().to_string()
            });

            agent.send(framing.as_bytes())?;
            // The Enter goes in its own write, after a beat. A single write of
            // "text\r" is one paste as far as freebuff is concerned, and the
            // carriage return is consumed with the text: the box keeps the words
            // and never submits. Two writes, one keystroke each.
            if self.submit_delay_ms > 0 {
                agent.pump_for(Duration::from_millis(self.submit_delay_ms))?;
            }
            let submitted_at = agent.elapsed();
            agent.send(self.submit.bytes())?;
            let turn = agent.run_turn(
                submitted_at,
                Duration::from_millis(self.deadline_ms),
                self.settle_ms,
            )?;
            report.submit_delay_ms = self.submit_delay_ms;
            report.turn = Some(turn);
        }

        // 3. Leave on our terms, and record whether the program agreed.
        report.quit = agent.quit(&self.quit);

        report.final_screen = agent.screen_text();
        report.stream = agent.stream.clone();
        report.wall_ms = agent.elapsed().as_millis();

        report.print(self.show_screen);

        if let Some(path) = &self.json {
            let body = serde_json::to_string_pretty(&report)?;
            std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
            println!("\nreport: {}", path.display());
        }
        Ok(())
    }

    fn submit_name(&self) -> &'static str {
        match self.submit {
            Submit::Cr => "cr",
            Submit::Lf => "lf",
        }
    }

    fn framing(&self, prompt: &str) -> String {
        let mut framed = String::new();
        if self.paste {
            framed.push_str("\u{1b}[200~");
            framed.push_str(prompt);
            framed.push_str("\u{1b}[201~");
        } else {
            framed.push_str(prompt);
        }
        framed
    }
}

// ---------------------------------------------------------------------------
// the report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
struct Snapshot {
    at_ms: u128,
    text: String,
    cursor_row: u16,
    cursor_col: u16,
    alternate_screen: bool,
    /// What the emulator thinks the input mode is — the two flags that break
    /// naive paste-then-Enter drivers.
    bracketed_paste: bool,
    cursor_hidden: bool,
}

/// Escape-sequence traffic, counted because the number is the answer to "what do
/// colours cost a screen scraper": `contents()` drops all of it.
#[derive(Debug, Clone, Default, Serialize)]
struct StreamStats {
    bytes: usize,
    sgr: usize,
    alt_enter: usize,
    alt_leave: usize,
    paste_on: usize,
    paste_off: usize,
    hide_cursor: usize,
    show_cursor: usize,
    other_csi: usize,
    bare_esc: usize,
    /// Queries the program asked the terminal, and the ones we answered.
    queries: usize,
    queries_answered: usize,
}

impl StreamStats {
    fn absorb(&mut self, bytes: &[u8]) {
        self.bytes += bytes.len();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != 0x1b {
                i += 1;
                continue;
            }
            let rest = &bytes[i + 1..];
            if rest.first() == Some(&b'[') {
                let mut j = 1;
                while j < rest.len() && !(0x40..=0x7e).contains(&rest[j]) {
                    j += 1;
                }
                if j < rest.len() {
                    let params = &rest[1..j];
                    match (rest[j], params) {
                        (b'm', _) => self.sgr += 1,
                        (b'h', b"?1049") => self.alt_enter += 1,
                        (b'l', b"?1049") => self.alt_leave += 1,
                        (b'h', b"?2004") => self.paste_on += 1,
                        (b'l', b"?2004") => self.paste_off += 1,
                        (b'l', b"?25") => self.hide_cursor += 1,
                        (b'h', b"?25") => self.show_cursor += 1,
                        _ => self.other_csi += 1,
                    }
                    i += j + 2;
                    continue;
                }
            }
            self.bare_esc += 1;
            i += 1;
        }
    }

    fn describe(&self) -> String {
        format!(
            "{} B, {} SGR, alt-screen {}/{} (enter/leave), bracketed paste {}/{}, cursor hidden {}, queries answered {}/{}",
            self.bytes,
            self.sgr,
            self.alt_enter,
            self.alt_leave,
            self.paste_on,
            self.paste_off,
            self.hide_cursor,
            self.queries_answered,
            self.queries
        )
    }
}

/// One quiet threshold's verdict. `screen_matches_end` is the check that matters:
/// a candidate that fires before the last byte of the answer has a different screen.
#[derive(Debug, Clone, Serialize)]
struct Candidate {
    threshold_ms: u64,
    fired_at_ms: Option<u128>,
    screen_matches_end: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct Turn {
    submitted_at_ms: u128,
    /// Everything below is relative to the submit, not to process start: the
    /// number a user feels is "how long after I pressed Enter".
    first_byte_ms: Option<u128>,
    end_ms: u128,
    max_gap_ms: u128,
    gaps_ms: Vec<u128>,
    candidates: Vec<Candidate>,
    settle_bytes: usize,
    screen_changed_after_end: bool,
    end_screen: Snapshot,
    saw_eof: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
struct QuitReport {
    sent: String,
    exited: bool,
    exit_code: Option<i32>,
    exit_ms: Option<u128>,
    killed: bool,
}

#[derive(Debug, Default, Serialize)]
struct Report {
    name: String,
    /// When the program only became ready after a startup dialog.
    gated_ms: u128,
    argv: Vec<String>,
    cwd: String,
    size: (u16, u16),
    ready_ms: u128,
    ready_reason: &'static str,
    ready: Snapshot,
    encoding: Option<String>,
    submit_delay_ms: u64,
    pre_prompt_stream: StreamStats,
    turn: Option<Turn>,
    quit: QuitReport,
    final_screen: String,
    stream: StreamStats,
    wall_ms: u128,
}

impl Report {
    fn print(&self, show_screen: bool) {
        println!("== {} ==", self.name);
        println!("  command      {}", self.argv.join(" "));
        println!("  cwd          {}", self.cwd);
        println!("  size         {}x{}", self.size.0, self.size.1);
        println!(
            "  ready        {} ms ({}), alternate screen: {}, bracketed paste: {}",
            self.ready_ms,
            self.ready_reason,
            self.ready.alternate_screen,
            self.ready.bracketed_paste
        );
        if self.gated_ms > 0 {
            println!(
                "  gated        {} ms (startup dialog answered)",
                self.gated_ms
            );
        }
        println!("  startup      {}", self.pre_prompt_stream.describe());
        if let Some(encoding) = &self.encoding {
            println!(
                "  prompt sent  as {encoding}, Enter {} ms later in its own write",
                self.submit_delay_ms
            );
        }
        if let Some(turn) = &self.turn {
            println!(
                "  first byte   {}",
                turn.first_byte_ms
                    .map(|ms| format!("{ms} ms after submit"))
                    .unwrap_or_else(|| "never".to_string())
            );
            println!(
                "  turn end     {} ms after submit, largest gap {} ms{}",
                turn.end_ms,
                turn.max_gap_ms,
                if turn.saw_eof {
                    " (program exited)"
                } else {
                    ""
                }
            );
            println!("  threshold    fired      screen == final");
            for candidate in &turn.candidates {
                let fired = candidate
                    .fired_at_ms
                    .map(|ms| format!("{ms:>7} ms"))
                    .unwrap_or_else(|| "     —".to_string());
                let verdict = match candidate.screen_matches_end {
                    Some(true) => "yes",
                    Some(false) => "NO — cut mid-answer",
                    None => "—",
                };
                println!("    {:>5} ms   {fired}   {verdict}", candidate.threshold_ms);
            }
            println!(
                "  after end    {} B in the settle window, screen changed: {}",
                turn.settle_bytes, turn.screen_changed_after_end
            );
        }
        println!(
            "  quit         sent {}, exited: {}{}{}",
            self.quit.sent,
            self.quit.exited,
            self.quit
                .exit_code
                .map(|code| format!(" (code {code})"))
                .unwrap_or_default(),
            if self.quit.killed { " [killed]" } else { "" }
        );
        if show_screen {
            let at_end = self.turn.as_ref().map_or(self.ready.text.as_str(), |turn| {
                turn.end_screen.text.as_str()
            });
            println!("\n--- screen when the turn ended ---\n{at_end}\n--- end ---");
            if !self.turn.is_some() {
                return;
            }
            println!(
                "\n--- screen after quit ---\n{}\n--- end ---",
                self.final_screen
            );
        }
    }
}

// ---------------------------------------------------------------------------
// the PTY
// ---------------------------------------------------------------------------

enum Chunk {
    Data { at: Duration, bytes: Vec<u8> },
    Eof,
}

#[derive(Clone, Copy)]
enum QueryKind {
    DeviceAttributes,
    SecondaryDeviceAttributes,
    Version,
    KittyKeyboard,
    StatusReport,
    CursorPosition,
    TextAreaSize,
}

struct Query {
    sequence: &'static [u8],
    kind: QueryKind,
}

/// The probe sequences seen in the wild during this probe, and the answer a
/// real xterm-compatible terminal gives. `?u` and `>0q` are the two that only
/// modern TUIs send.
const QUERIES: [Query; 7] = [
    Query {
        sequence: b"\x1b[?u",
        kind: QueryKind::KittyKeyboard,
    },
    Query {
        sequence: b"\x1b[>0q",
        kind: QueryKind::Version,
    },
    Query {
        sequence: b"\x1b[>c",
        kind: QueryKind::SecondaryDeviceAttributes,
    },
    Query {
        sequence: b"\x1b[c",
        kind: QueryKind::DeviceAttributes,
    },
    Query {
        sequence: b"\x1b[6n",
        kind: QueryKind::CursorPosition,
    },
    Query {
        sequence: b"\x1b[5n",
        kind: QueryKind::StatusReport,
    },
    Query {
        sequence: b"\x1b[18t",
        kind: QueryKind::TextAreaSize,
    },
];

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

struct PtyAgent {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: Receiver<Chunk>,
    parser: vt100::Parser,
    stream: StreamStats,
    started: Instant,
    last_data_at: Duration,
    saw_eof: bool,
    saw_output: bool,
    answer_queries: bool,
}

impl PtyAgent {
    fn spawn(spec: &Spec) -> Result<Self> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let (program, rest) = spec
            .argv
            .split_first()
            .context("the command line is empty")?;
        let mut command = CommandBuilder::new(program);
        command.args(rest);
        command.cwd(&spec.cwd);
        // A TUI that thinks it is on a dumb terminal draws nothing. This is the
        // whole reason the transport is a PTY rather than a pipe.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("spawning {program}"))?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
        let writer = pair.master.take_writer().context("pty writer")?;

        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Chunk::Eof);
                        break;
                    }
                    Ok(count) => {
                        let chunk = Chunk::Data {
                            at: started.elapsed(),
                            bytes: buffer[..count].to_vec(),
                        };
                        if tx.send(chunk).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            child,
            writer,
            rx,
            parser: vt100::Parser::new(
                NonZeroU16::new(spec.rows).context("rows")?,
                NonZeroU16::new(spec.cols).context("cols")?,
                0,
            ),
            stream: StreamStats::default(),
            started,
            last_data_at: Duration::ZERO,
            saw_eof: false,
            saw_output: false,
            answer_queries: spec.answer_queries,
        })
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Read whatever is available within `timeout`. Returns `true` when bytes
    /// arrived, so callers can distinguish "quiet" from "slow".
    fn pump(&mut self, timeout: Duration) -> Result<bool> {
        match self.rx.recv_timeout(timeout) {
            Ok(Chunk::Data { at, bytes }) => {
                self.stream.absorb(&bytes);
                self.parser.process(&bytes);
                self.last_data_at = at;
                self.saw_output = true;
                self.handle_queries(&bytes);
                Ok(true)
            }
            Ok(Chunk::Eof) | Err(RecvTimeoutError::Disconnected) => {
                self.saw_eof = true;
                Ok(false)
            }
            Err(RecvTimeoutError::Timeout) => Ok(false),
        }
    }

    /// Reply to the terminal probes a real terminal would answer. Without these
    /// the program can block on capability detection, which looks exactly like a
    /// hang from the outside — that is why the count is reported.
    fn handle_queries(&mut self, bytes: &[u8]) {
        let mut replies: Vec<String> = Vec::new();
        for query in QUERIES {
            if contains(bytes, query.sequence) {
                let reply = match query.kind {
                    QueryKind::DeviceAttributes => "\u{1b}[?1;2c".to_string(),
                    QueryKind::SecondaryDeviceAttributes => "\u{1b}[>0;370;0c".to_string(),
                    QueryKind::Version => "\u{1b}P>|xterm(370)\u{1b}\\".to_string(),
                    QueryKind::KittyKeyboard => "\u{1b}[?0u".to_string(),
                    QueryKind::StatusReport => "\u{1b}[0n".to_string(),
                    QueryKind::CursorPosition => {
                        let (row, col) = self.parser.screen().cursor_position();
                        format!("\u{1b}[{};{}R", row + 1, col + 1)
                    }
                    QueryKind::TextAreaSize => {
                        let (rows, cols) = self.parser.screen().size();
                        format!("\u{1b}[8;{};{}t", rows, cols)
                    }
                };
                self.stream.queries += 1;
                replies.push(reply);
            }
        }
        if !self.answer_queries {
            return;
        }
        for reply in replies {
            if self.send(reply.as_bytes()).is_ok() {
                self.stream.queries_answered += 1;
            }
        }
    }

    /// Read for a fixed wall-clock window without treating silence as anything.
    fn pump_for(&mut self, window: Duration) -> Result<()> {
        let begin = self.elapsed();
        while self.elapsed() - begin < window {
            self.pump(Duration::from_millis(10))?;
        }
        Ok(())
    }

    /// Pump until the stream has been silent for `quiet`, or `cap` passes. Silence
    /// is measured from the first byte, so a program that never speaks costs the
    /// full cap rather than returning instantly.
    fn pump_until_quiet(&mut self, quiet: Duration, cap: Duration) -> Result<()> {
        let begin = self.elapsed();
        loop {
            if self.saw_eof {
                return Ok(());
            }
            let now = self.elapsed();
            if now - begin >= cap {
                return Ok(());
            }
            if self.saw_output && now - self.last_data_at >= quiet {
                return Ok(());
            }
            self.pump(Duration::from_millis(20))?;
        }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes).context("writing to the pty")?;
        self.writer.flush().context("flushing the pty")?;
        Ok(())
    }

    fn screen_text(&self) -> String {
        let screen = self.parser.screen();
        let cols = screen.size().1.get();
        let mut lines: Vec<String> = screen
            .rows(0, cols)
            .map(|row| row.trim_end().to_string())
            .collect();
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    fn snapshot(&self) -> Snapshot {
        let screen = self.parser.screen();
        let (cursor_row, cursor_col) = screen.cursor_position();
        Snapshot {
            at_ms: self.elapsed().as_millis(),
            text: self.screen_text(),
            cursor_row,
            cursor_col,
            alternate_screen: screen.alternate_screen(),
            bracketed_paste: screen.bracketed_paste(),
            cursor_hidden: screen.hide_cursor(),
        }
    }

    fn run_turn(
        &mut self,
        submitted_at: Duration,
        deadline: Duration,
        settle_ms: u64,
    ) -> Result<Turn> {
        let mut candidates: Vec<Candidate> = THRESHOLDS_MS
            .iter()
            .map(|threshold| Candidate {
                threshold_ms: *threshold,
                fired_at_ms: None,
                screen_matches_end: None,
            })
            .collect();
        let mut fired_screens: Vec<Option<String>> = vec![None; THRESHOLDS_MS.len()];

        let mut since = submitted_at;
        let mut first_byte = None;
        let mut gaps = Vec::new();
        let mut max_gap = 0;
        let mut end = self.elapsed();

        loop {
            if self.elapsed() - submitted_at >= deadline {
                break;
            }
            self.pump(Duration::from_millis(20))?;
            let now = self.elapsed();

            if self.saw_output && self.last_data_at != since {
                let gap = now.saturating_sub(since).as_millis();
                if first_byte.is_some() {
                    max_gap = max_gap.max(gap);
                    if gaps.len() < 1024 {
                        gaps.push(gap);
                    }
                }
                since = self.last_data_at;
                if first_byte.is_none() {
                    first_byte = Some(now.saturating_sub(submitted_at).as_millis());
                }
            }

            if first_byte.is_some() {
                let quiet = now.saturating_sub(since);
                for (index, threshold) in THRESHOLDS_MS.iter().enumerate() {
                    if candidates[index].fired_at_ms.is_none()
                        && quiet >= Duration::from_millis(*threshold)
                    {
                        candidates[index].fired_at_ms =
                            Some(now.saturating_sub(submitted_at).as_millis());
                        fired_screens[index] = Some(self.screen_text());
                    }
                }
            }

            let quiet = now.saturating_sub(since);
            if self.saw_eof
                || (first_byte.is_some() && quiet >= Duration::from_millis(TRUE_QUIET_MS))
            {
                end = now;
                break;
            }
        }

        // The truth-teller is the 5 s threshold; if the turn never went quiet we
        // fall back to the deadline screen.
        let end_screen = self.screen_text();
        for (index, candidate) in candidates.iter_mut().enumerate() {
            candidate.screen_matches_end = fired_screens[index]
                .as_ref()
                .map(|screen| *screen == end_screen);
        }

        // Keep reading after the declared end: if anything arrives, the boundary
        // we picked was early, and that is a fact worth recording rather than a
        // detail to hide behind a "usually fine".
        let settle_bytes = self.stream.bytes;
        self.pump_until_quiet(
            Duration::from_millis(settle_ms.max(1)),
            Duration::from_millis(settle_ms.max(1)),
        )?;
        let settle_bytes = self.stream.bytes - settle_bytes;
        let screen_changed_after_end = self.screen_text() != end_screen;

        Ok(Turn {
            submitted_at_ms: submitted_at.as_millis(),
            first_byte_ms: first_byte,
            end_ms: end.saturating_sub(submitted_at).as_millis(),
            max_gap_ms: max_gap,
            gaps_ms: gaps,
            candidates,
            settle_bytes,
            screen_changed_after_end,
            end_screen: Snapshot {
                at_ms: end.saturating_sub(submitted_at).as_millis(),
                text: end_screen,
                cursor_row: 0,
                cursor_col: 0,
                alternate_screen: self.parser.screen().alternate_screen(),
                bracketed_paste: self.parser.screen().bracketed_paste(),
                cursor_hidden: self.parser.screen().hide_cursor(),
            },
            saw_eof: self.saw_eof,
        })
    }

    fn quit(&mut self, how: &Quit) -> QuitReport {
        let mut report = QuitReport {
            sent: how.label(),
            ..QuitReport::default()
        };
        let started = self.elapsed();

        match how {
            Quit::Kill => {
                let _ = self.child.kill();
                report.killed = true;
            }
            Quit::CtrlD => {
                if self.send(b"\x04").is_err() {
                    report.killed = true;
                }
            }
            Quit::Text(text) => {
                // Same two-write rule as a prompt: text, a beat, then Enter.
                let _ = self.send(text.as_bytes());
                let _ = self.pump_for(Duration::from_millis(250));
                let _ = self.send(b"\r");
            }
        }

        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    report.exited = true;
                    report.exit_code = Some(i32::try_from(status.exit_code()).unwrap_or(-1));
                    report.exit_ms = Some(self.elapsed().as_millis());
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            if self.elapsed() - started >= Duration::from_secs(5) {
                let _ = self.child.kill();
                report.killed = true;
                // One last look so an instant kill is not reported as a hang.
                if let Ok(Some(status)) = self.child.try_wait() {
                    report.exited = true;
                    report.exit_code = Some(i32::try_from(status.exit_code()).unwrap_or(-1));
                    report.exit_ms = Some(self.elapsed().as_millis());
                }
                break;
            }
            let _ = self.pump(Duration::from_millis(25));
        }

        // Drain anything the program wrote on the way out (leave alt screen, reset
        // colours) so the final screen is the one the terminal would show.
        while let Ok(true) = self.pump(Duration::from_millis(50)) {}
        report
    }
}
