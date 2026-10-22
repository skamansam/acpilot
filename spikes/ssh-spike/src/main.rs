//! P0-4 — a remote host, driven the way the product will drive it.
//!
//! The question is not whether SSH works; it is whether the four things an agent
//! needs over it behave: **line-framed JSON**, **stderr kept apart**, **exit
//! codes**, and **PTY resize**. Those are the assumptions `acpilot-host::ssh`
//! (P8-1) would otherwise be written on top of.
//!
//! ```bash
//! # one command on the far side
//! cargo run --release -- exec --command 'printf out; printf err >&2; exit 3'
//!
//! # the ACP transport check: the same binary runs as the agent over there
//! cargo run --release -- acp
//!
//! # PTY, plus a resize while a program is watching the window size
//! cargo run --release -- pty
//!
//! # everything, which is what the note's tables come from
//! cargo run --release -- bench
//! ```
//!
//! The mock agent is this binary with `--mock-agent`: it answers real ACP
//! methods (`initialize`, `session/new`, `session/prompt`) one JSON object per
//! line, and emits 100 `session/update` notifications for a prompt. That is
//! enough to answer the transport question without dragging an agent's install
//! into a measurement of SSH.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, load_secret_key};
use serde_json::{Value, json};
use tokio::time::timeout;

/// Everything that is not the default must be said explicitly: a spike that
/// silently tests the wrong host is worse than one that refuses to start.
struct Settings {
    host: String,
    port: u16,
    user: String,
    key: PathBuf,
    command: Option<String>,
    cols: u32,
    rows: u32,
}

enum Mode {
    Bench,
    Exec,
    Pty,
    Acp,
    MockAgent,
}

const USAGE: &str = "\
usage: ssh-spike [bench|exec|pty|acp|--mock-agent] [options]

  bench        exec + pty + acp, the table the note is written from (default)
  exec         one command, checking stdout/stderr/exit codes
  pty          open a PTY, resize it, and watch a program see the new size
  acp          speak ACP to this binary running as the agent over there
  --mock-agent  (far side) answer ACP methods one JSON line at a time

  --host <h>      default 127.0.0.1
  --port <n>      default 2222
  --user <u>      default $USER
  --key <path>    private key for publickey auth (required)
  --command <c>   exec: command line to run (default: a stdout/stderr/exit probe)
  --cols <n>      pty: width (default 100)
  --rows <n>      pty: height (default 40)";

fn main() {
    let (mode, settings) = match parse(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("ssh-spike: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    if matches!(mode, Mode::MockAgent) {
        if let Err(error) = mock_agent() {
            eprintln!("ssh-spike: mock agent: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ssh-spike: cannot start tokio: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = runtime.block_on(run(mode, &settings)) {
        eprintln!("ssh-spike: {error:#}");
        std::process::exit(1);
    }
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<(Mode, Settings), String> {
    let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
    let mut settings = Settings {
        host: "127.0.0.1".to_string(),
        port: 2222,
        user,
        key: PathBuf::new(),
        command: None,
        cols: 100,
        rows: 40,
    };
    let mut mode = Mode::Bench;

    while let Some(arg) = args.next() {
        let mut value = |name: &str| -> Result<String, String> {
            args.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "bench" => mode = Mode::Bench,
            "exec" => mode = Mode::Exec,
            "pty" => mode = Mode::Pty,
            "acp" => mode = Mode::Acp,
            "--mock-agent" => mode = Mode::MockAgent,
            "--host" => settings.host = value("--host")?,
            "--port" => {
                settings.port = value("--port")?
                    .parse()
                    .map_err(|_| "--port must be a number".to_string())?;
            }
            "--user" => settings.user = value("--user")?,
            "--key" => settings.key = PathBuf::from(value("--key")?),
            "--command" => settings.command = Some(value("--command")?),
            "--cols" => {
                settings.cols = value("--cols")?
                    .parse()
                    .map_err(|_| "--cols must be a number".to_string())?;
            }
            "--rows" => {
                settings.rows = value("--rows")?
                    .parse()
                    .map_err(|_| "--rows must be a number".to_string())?;
            }
            "-h" | "--help" => return Err("help".to_string()),
            other => return Err(format!("unknown argument {other}")),
        }
    }

    if !matches!(mode, Mode::MockAgent) && settings.key.as_os_str().is_empty() {
        // The fixture `sshd.sh` generates, so the common case is one command
        // after `./sshd.sh start`. It is a throwaway key for a loopback server.
        settings.key =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/sshd-fixture/clientkey");
        if !settings.key.is_file() {
            return Err(format!(
                "--key is required ({} does not exist; run ./sshd.sh start)",
                settings.key.display()
            ));
        }
    }
    Ok((mode, settings))
}

async fn run(mode: Mode, settings: &Settings) -> Result<()> {
    let started = Instant::now();
    let mut session = connect(settings).await?;
    println!(
        "connected   {}@{}:{} in {:.1} ms",
        settings.user,
        settings.host,
        settings.port,
        millis(started.elapsed())
    );

    match mode {
        Mode::Exec => {
            let command = settings
                .command
                .clone()
                .unwrap_or_else(|| default_probe().to_string());
            let outcome = exec(&mut session, &command).await?;
            print_exec(&command, &outcome);
        }
        Mode::Pty => {
            let outcome = pty(&mut session, settings.cols, settings.rows).await?;
            print_pty(&outcome);
        }
        Mode::Acp => {
            let outcome = acp(&mut session).await?;
            print_acp(&outcome);
        }
        Mode::Bench => {
            let command = settings
                .command
                .clone()
                .unwrap_or_else(|| default_probe().to_string());
            let outcome = exec(&mut session, &command).await?;
            print_exec(&command, &outcome);

            let outcome = pty(&mut session, settings.cols, settings.rows).await?;
            print_pty(&outcome);

            let outcome = acp(&mut session).await?;
            print_acp(&outcome);
        }
        Mode::MockAgent => unreachable!("handled before the runtime starts"),
    }

    session
        .disconnect(russh::Disconnect::ByApplication, "", "English")
        .await
        .context("disconnect")?;
    Ok(())
}

/// A command whose every channel is observable at once: stdout, stderr, and a
/// non-zero exit code that the caller has to be able to see.
fn default_probe() -> &'static str {
    "printf 'out\\n'; printf 'err\\n' >&2; exit 3"
}

// ---------------------------------------------------------------------------
// connection
// ---------------------------------------------------------------------------

struct Client;

impl client::Handler for Client {
    type Error = russh::Error;

    /// The host key is the caller's problem, not this spike's: the point is the
    /// transport, and a TOFU prompt would block a measurement. P8-1 decides the
    /// real policy.
    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

async fn connect(settings: &Settings) -> Result<client::Handle<Client>> {
    let key = load_secret_key(&settings.key, None)
        .with_context(|| format!("load {}", settings.key.display()))?;
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, (settings.host.as_str(), settings.port), Client)
        .await
        .with_context(|| format!("connect {}:{}", settings.host, settings.port))?;

    let hash = session.best_supported_rsa_hash().await?.flatten();
    let auth = session
        .authenticate_publickey(
            &settings.user,
            PrivateKeyWithHashAlg::new(Arc::new(key), hash),
        )
        .await
        .context("authenticate_publickey")?;
    if !auth.success() {
        bail!(
            "publickey authentication as {} failed (the key must be in the far side's \
             authorized_keys)",
            settings.user
        );
    }
    Ok(session)
}

// ---------------------------------------------------------------------------
// exec channel
// ---------------------------------------------------------------------------

struct ExecOutcome {
    stdout: String,
    stderr: String,
    exit: Option<u32>,
    /// How many SSH `DATA` events carried stdout. More than one for a command
    /// that wrote once means the protocol does not preserve write boundaries.
    data_events: usize,
    largest_event: usize,
    elapsed: Duration,
}

async fn exec(session: &mut client::Handle<Client>, command: &str) -> Result<ExecOutcome> {
    let started = Instant::now();
    let mut channel = session
        .channel_open_session()
        .await
        .context("open session")?;
    channel.exec(true, command).await.context("exec")?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit = None;
    let mut data_events = 0;
    let mut largest_event = 0;

    while let Some(message) = channel.wait().await {
        match message {
            russh::ChannelMsg::Data { data } => {
                data_events += 1;
                largest_event = largest_event.max(data.len());
                stdout.extend_from_slice(&data);
            }
            russh::ChannelMsg::ExtendedData { data, ext } => {
                // ext 1 is stderr in the SSH protocol; anything else is a
                // stream we do not know how to read, so it must not be folded
                // into either buffer.
                if ext == 1 {
                    stderr.extend_from_slice(&data);
                } else {
                    bail!("unexpected extended data on stream {ext}");
                }
            }
            russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => {}
            _ => {}
        }
    }

    Ok(ExecOutcome {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit,
        data_events,
        largest_event,
        elapsed: started.elapsed(),
    })
}

// ---------------------------------------------------------------------------
// PTY
// ---------------------------------------------------------------------------

struct PtyOutcome {
    /// What `stty size` said before the resize, as `rows cols`.
    before: String,
    /// What it said after, which is the actual answer to "does resize work".
    after: String,
    is_tty: bool,
    elapsed: Duration,
}

async fn pty(session: &mut client::Handle<Client>, cols: u32, rows: u32) -> Result<PtyOutcome> {
    let started = Instant::now();
    let mut channel = session
        .channel_open_session()
        .await
        .context("open session")?;
    channel
        .request_pty(true, "xterm-256color", cols, rows, 0, 0, &[])
        .await
        .context("request_pty")?;

    // A loop rather than one `stty size`: a resize only proves anything if a
    // program is watching while it happens.
    channel
        .exec(true, "while :; do stty size; sleep 0.05; done")
        .await
        .context("exec pty probe")?;

    let mut seen = String::new();
    let mut before = None;
    let mut after = None;

    // 10 s is generous by two orders of magnitude; it exists so a broken resize
    // fails the run instead of hanging it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Ok(Some(message)) = timeout(Duration::from_millis(500), channel.wait()).await else {
            continue;
        };
        if let russh::ChannelMsg::Data { data } = message {
            seen.push_str(&String::from_utf8_lossy(&data));
            while let Some(newline) = seen.find('\n') {
                let line = seen[..newline].trim().to_string();
                seen.drain(..=newline);
                if line.is_empty() {
                    continue;
                }
                if before.is_none() {
                    before = Some(line.clone());
                    channel
                        .window_change(cols + 20, rows + 10, 0, 0)
                        .await
                        .context("window_change")?;
                } else if line != before.clone().unwrap_or_default() {
                    after = Some(line);
                    break;
                }
            }
        }
        if after.is_some() {
            break;
        }
    }

    // `tty` is the difference between a PTY and a pipe, in one command.
    let mut tty_channel = session
        .channel_open_session()
        .await
        .context("open session")?;
    tty_channel
        .request_pty(true, "xterm-256color", cols, rows, 0, 0, &[])
        .await
        .context("request_pty")?;
    tty_channel.exec(true, "tty").await.context("exec tty")?;
    let mut tty_out = String::new();
    while let Some(message) = timeout(Duration::from_secs(5), tty_channel.wait())
        .await
        .context("tty timed out")?
    {
        if let russh::ChannelMsg::Data { data } = message {
            tty_out.push_str(&String::from_utf8_lossy(&data));
        }
    }
    let _ = tty_channel.close().await;

    Ok(PtyOutcome {
        before: before.unwrap_or_else(|| "<no size observed>".to_string()),
        after: after.unwrap_or_else(|| "<resize never observed>".to_string()),
        is_tty: tty_out.trim().starts_with("/dev/"),
        elapsed: started.elapsed(),
    })
}

// ---------------------------------------------------------------------------
// ACP over the exec channel
// ---------------------------------------------------------------------------

struct Frame {
    bytes: usize,
    /// When this frame finished arriving, relative to the start of the exchange.
    at: Duration,
}

struct AcpOutcome {
    frames: Vec<Frame>,
    notifications: usize,
    stop_reason: String,
    partial_events: usize,
    largest_event: usize,
    split_frames: usize,
    /// SSH `DATA` events that carried at least one frame. The far side wrote 103
    /// lines one at a time; the difference is the coalescing the client has to
    /// undo.
    data_events: usize,
    most_frames_in_one_event: usize,
    first_frame: Duration,
    exit: Option<u32>,
    elapsed: Duration,
}

async fn acp(session: &mut client::Handle<Client>) -> Result<AcpOutcome> {
    let exe = std::env::current_exe().context("current_exe")?;
    let command = format!("{} --mock-agent", shell_quote(&exe.display().to_string()));

    let started = Instant::now();
    let mut channel = session
        .channel_open_session()
        .await
        .context("open session")?;
    channel
        .exec(true, command)
        .await
        .context("exec mock agent")?;

    // The client half of the exchange, in the order ACP defines it.
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true},"terminal":true}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/","mcpServers":[]}}),
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"s_1","prompt":[{"type":"text","text":"say something in 100 chunks"}]}}),
    ] {
        channel
            .data_bytes(format!("{request}\n"))
            .await
            .context("write request")?;
    }

    let mut buffer = Vec::new();
    let mut frames = Vec::new();
    let mut notifications = 0;
    let mut stop_reason = String::new();
    let mut partial_events = 0;
    let mut largest_event = 0;
    let mut exit = None;
    let mut split_frames = 0;
    let mut data_events = 0;
    let mut most_frames_in_one_event = 0;
    let mut first_frame = Duration::ZERO;

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let Ok(Some(message)) = timeout(Duration::from_secs(5), channel.wait()).await else {
            break;
        };
        match message {
            russh::ChannelMsg::Data { data } => {
                largest_event = largest_event.max(data.len());
                // A line that was already partly here is carried into this
                // event: that is the proof that framing is the client's job.
                let mut carried = !buffer.is_empty();
                let mut frames_here = 0;
                buffer.extend_from_slice(&data);
                // Every complete line is one JSON-RPC frame. A trailing fragment
                // is not an error — it is the evidence that framing is ours.
                while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=newline).collect();
                    let line = String::from_utf8_lossy(&line);
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let frame: Value = serde_json::from_str(line).with_context(|| {
                        format!("far side wrote a line that is not JSON: {line}")
                    })?;
                    if frame.get("method").and_then(Value::as_str) == Some("session/update") {
                        notifications += 1;
                    }
                    if let Some(reason) = frame
                        .get("result")
                        .and_then(|result| result.get("stopReason"))
                        .and_then(Value::as_str)
                    {
                        stop_reason = reason.to_string();
                    }
                    frames.push(Frame {
                        bytes: line.len(),
                        at: started.elapsed(),
                    });
                    if carried {
                        split_frames += 1;
                        carried = false;
                    }
                    frames_here += 1;
                }
                if frames_here > 0 {
                    data_events += 1;
                    most_frames_in_one_event = most_frames_in_one_event.max(frames_here);
                    if first_frame.is_zero() {
                        first_frame = started.elapsed();
                    }
                }
                if !buffer.is_empty() {
                    partial_events += 1;
                }
            }
            russh::ChannelMsg::ExtendedData { data, ext } => {
                bail!(
                    "the agent wrote {} bytes on stream {ext}; stderr must not be in the frame channel",
                    data.len()
                );
            }
            russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => {}
            _ => {}
        }
        if !stop_reason.is_empty() {
            // The prompt's result is the last frame; the agent exits on EOF.
            let _ = channel.eof().await;
            break;
        }
    }

    while let Some(message) = timeout(Duration::from_secs(5), channel.wait())
        .await
        .unwrap_or(None)
    {
        if let russh::ChannelMsg::ExitStatus { exit_status } = message {
            exit = Some(exit_status);
        }
    }

    Ok(AcpOutcome {
        frames,
        notifications,
        stop_reason,
        partial_events,
        largest_event,
        split_frames,
        data_events,
        most_frames_in_one_event,
        first_frame,
        exit,
        elapsed: started.elapsed(),
    })
}

fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

// ---------------------------------------------------------------------------
// the far-side agent
// ---------------------------------------------------------------------------

/// A real ACP agent in the only sense this measurement needs: it speaks the
/// methods, one JSON object per line, and streams notifications for a prompt.
fn mock_agent() -> Result<()> {
    use std::io::{BufRead, Write};

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.context("read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = serde_json::from_str(&line).context("request is not JSON")?;
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");

        match method {
            "initialize" => reply(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":1,
                    "agentCapabilities":{"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false}}
                }}),
            )?,
            "session/new" => reply(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"s_1"}}),
            )?,
            "session/prompt" => {
                // One write and one flush per notification: if these arrive
                // coalesced, the client has to do the framing.
                for index in 0..100 {
                    let update = json!({
                        "jsonrpc":"2.0",
                        "method":"session/update",
                        "params":{
                            "sessionId":"s_1",
                            "update":{
                                "sessionUpdate":"agent_message_chunk",
                                "content":{"type":"text","text":format!("chunk {index}")}
                            }
                        }
                    });
                    writeln!(stdout, "{update}")?;
                    stdout.flush()?;
                }
                reply(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}),
                )?;
            }
            other => reply(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("no method {other}")}}),
            )?,
        }
    }
    Ok(())
}

fn reply(stdout: &mut std::io::Stdout, value: Value) -> Result<()> {
    use std::io::Write;
    writeln!(stdout, "{value}")?;
    stdout.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// reporting
// ---------------------------------------------------------------------------

fn millis(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1_000.0
}

fn print_exec(command: &str, outcome: &ExecOutcome) {
    println!("\nexec        {command}");
    println!(
        "            exit {:?} in {:.1} ms, {} stdout event(s), largest {} B",
        outcome.exit,
        millis(outcome.elapsed),
        outcome.data_events,
        outcome.largest_event
    );
    println!(
        "            stdout {:?}",
        outcome.stdout.trim_end_matches('\n')
    );
    println!(
        "            stderr {:?}",
        outcome.stderr.trim_end_matches('\n')
    );
    println!(
        "            verdict: {}",
        if outcome.exit == Some(3)
            && outcome.stdout.contains("out")
            && outcome.stderr.contains("err")
        {
            "stdout, stderr and the exit code all arrived separately"
        } else {
            "ONE OF STDOUT/STDERR/EXIT DID NOT SURVIVE THE CHANNEL"
        }
    );
}

fn print_pty(outcome: &PtyOutcome) {
    println!("\npty");
    println!("            tty           {}", outcome.is_tty);
    println!(
        "            stty size     {} then {}",
        outcome.before, outcome.after
    );
    println!(
        "            elapsed       {:.1} ms",
        millis(outcome.elapsed)
    );
    println!(
        "            verdict: {}",
        if outcome.is_tty && outcome.before != outcome.after {
            "a PTY, and a resize reaches the program"
        } else {
            "RESIZE OR TTY DID NOT REACH THE PROGRAM"
        }
    );
}

fn print_acp(outcome: &AcpOutcome) {
    let mut gaps: Vec<f64> = outcome
        .frames
        .windows(2)
        .map(|pair| millis(pair[1].at.saturating_sub(pair[0].at)))
        .collect();
    gaps.sort_by(|a, b| a.total_cmp(b));
    let p50 = gaps.get(gaps.len() / 2).copied().unwrap_or(0.0);
    let p90 = gaps
        .get((gaps.len() * 9 / 10).min(gaps.len().saturating_sub(1)))
        .copied()
        .unwrap_or(0.0);
    let mean_bytes = outcome
        .frames
        .iter()
        .map(|frame| frame.bytes)
        .sum::<usize>()
        / outcome.frames.len().max(1);

    println!("\nacp over the exec channel");
    println!(
        "            {} frame(s) ({} notification), stop reason {:?} in {:.1} ms",
        outcome.frames.len(),
        outcome.notifications,
        outcome.stop_reason,
        millis(outcome.elapsed)
    );
    println!(
        "            first frame after {:.2} ms; gap p50 {p50:.2} ms, p90 {p90:.2} ms; mean frame {mean_bytes} B",
        millis(outcome.first_frame),
    );
    println!(
        "            {} data event(s) carried them, largest {} B, up to {} frames in one event",
        outcome.data_events, outcome.largest_event, outcome.most_frames_in_one_event
    );
    println!(
        "            {} event(s) ended mid-frame, {} frame(s) needed buffering",
        outcome.partial_events, outcome.split_frames
    );
    println!("            agent exit {:?}", outcome.exit);
    println!(
        "            verdict: {}",
        if outcome.notifications == 100 && outcome.stop_reason == "end_turn" {
            "ACP survived the channel: every line parsed as exactly one frame"
        } else {
            "THE ACP EXCHANGE DID NOT COMPLETE"
        }
    );
}
