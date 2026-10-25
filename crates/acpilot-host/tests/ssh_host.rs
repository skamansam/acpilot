//! `SshHost` against a real sshd, not a mock.
//!
//! Compiled only with the `ssh` feature, which is on by default: the platform
//! cross-check runs `--no-default-features` and must not need this file's
//! symbols.
//!
//! Opt-in, because it needs a server: start the design phase's P0-4 fixture and point the test at
//! it. The same `sshd` the note measured, on loopback, with a throwaway host key.
//!
//! ```bash
//! probes/ssh/sshd.sh start                       # prints the key path
//! ACPILOT_SSH_TEST=1 \
//! ACPILOT_SSH_KEY="$PWD/probes/ssh/target/sshd-fixture/clientkey" \
//! ACPILOT_SSH_USER="$USER" \
//!   cargo test -p acpilot-host --test ssh_host -- --test-threads=1
//! ```
//!
//! Without `ACPILOT_SSH_TEST=1` every test returns immediately: CI has no sshd,
//! and a test that silently needs one is a test that fails on a laptop for the
//! wrong reason.
//!
//! What is asserted is what the design phase's task P0-4 measured, from the other side of the seam: exec
//! keeps stderr out of the frame stream and preserves the exit code; a PTY is a
//! terminal and takes a resize mid-run; and a hundred lines written one at a time
//! arrive as a byte stream the caller has to frame.

#![cfg(feature = "ssh")]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use acpilot_host::{ExecCommand, Host, HostKeyPolicy, Size, SshConfig, SshHost};

fn fixture() -> Option<SshConfig> {
    if std::env::var("ACPILOT_SSH_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set ACPILOT_SSH_TEST=1 with ACPILOT_SSH_KEY to run this suite");
        return None;
    }
    let key = std::env::var("ACPILOT_SSH_KEY").expect("ACPILOT_SSH_KEY names the client key");
    let user = std::env::var("ACPILOT_SSH_USER").unwrap_or_else(|_| "root".to_string());
    let port: u16 = std::env::var("ACPILOT_SSH_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(2222);
    let host = std::env::var("ACPILOT_SSH_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

    Some(
        SshConfig::new(host, user, key)
            .port(port)
            // The fixture's host key is generated per checkout and thrown away,
            // so there is nothing to compare it against.
            .host_key(HostKeyPolicy::AcceptAny),
    )
}

/// Drain a channel into a buffer that the test can watch while it runs. A PTY has
/// no message boundaries, so "read one line" would hang or cut a line in half.
fn drain(reader: Box<dyn Read + Send>) -> Arc<Mutex<String>> {
    let buffer = Arc::new(Mutex::new(String::new()));
    let target = Arc::clone(&buffer);
    thread::spawn(move || {
        let mut reader = reader;
        let mut chunk = [0u8; 4096];
        while let Ok(read) = reader.read(&mut chunk) {
            if read == 0 {
                break;
            }
            if let Ok(mut text) = target.lock() {
                text.push_str(&String::from_utf8_lossy(&chunk[..read]));
            }
        }
    });
    buffer
}

fn wait_for(buffer: &Arc<Mutex<String>>, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if buffer.lock().is_ok_and(|text| text.contains(needle)) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn exec_keeps_streams_apart_and_reports_the_exit_code() {
    let Some(config) = fixture() else {
        return;
    };
    let host = SshHost::connect(config).expect("connect");

    let mut child = host
        .spawn(&ExecCommand::new("sh").args(["-c", "printf out; printf err >&2; exit 3"]))
        .expect("spawn");

    // Both must be drained: a channel window is shared by the two streams, and
    // reading only stdout can block a program that logs.
    let mut stderr_reader = child.take_stderr().expect("stderr reader");
    let stderr_thread = thread::spawn(move || {
        let mut text = String::new();
        stderr_reader
            .read_to_string(&mut text)
            .expect("read stderr");
        text
    });
    let mut stdout = String::new();
    child
        .take_stdout()
        .expect("stdout reader")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let stderr = stderr_thread.join().expect("stderr thread");

    let exit = child.wait().expect("wait");
    assert_eq!(stdout, "out", "stdout carries only stdout");
    assert_eq!(stderr, "err", "stderr carries only stderr");
    assert_eq!(exit.code, Some(3));
    assert!(!exit.success);
    assert_eq!(
        child.id(),
        None,
        "a remote pid is not a pid this machine can use"
    );
}

#[test]
fn a_pty_is_a_terminal_and_takes_a_resize_mid_run() {
    let Some(config) = fixture() else {
        return;
    };
    let host = SshHost::connect(config).expect("connect");

    let mut pty = host
        .spawn_pty(
            &ExecCommand::new("sh").args(["-c", "tty; stty size; sleep 2; stty size"]),
            Size::new(100, 40),
        )
        .expect("spawn on a pty");
    let output = drain(pty.take_reader().expect("one reader"));

    // Wait for the first size, change the window, and let the program report it.
    assert!(
        wait_for(&output, "40 100", Duration::from_secs(15)),
        "the pty must start at the requested size, got {:?}",
        output.lock().unwrap()
    );
    pty.resize(Size::new(120, 50)).expect("resize");
    assert_eq!(pty.size(), Size::new(120, 50));

    let exit = pty.wait().expect("wait");
    assert!(exit.success, "{exit:?}");
    assert!(
        wait_for(&output, "50 120", Duration::from_secs(10)),
        "the program must observe the resize: {:?}",
        output.lock().unwrap()
    );
    let seen = output.lock().unwrap().clone();
    assert!(
        seen.contains("/dev/pts/"),
        "a pty is a real terminal, got {seen:?}"
    );
}

#[test]
fn a_hundred_lines_arrive_as_a_byte_stream_the_caller_frames() {
    let Some(config) = fixture() else {
        return;
    };
    let host = SshHost::connect(config).expect("connect");

    let mut child = host
        .spawn(&ExecCommand::new("sh").args(["-c", "cat"]))
        .expect("spawn");
    let mut stdin = child.take_stdin().expect("stdin writer");
    let started = Instant::now();
    for index in 0..100 {
        stdin
            .write_all(format!("line {index}\n").as_bytes())
            .expect("write a line");
    }
    stdin.flush().expect("flush");
    drop(stdin);

    let mut stdout = String::new();
    child
        .take_stdout()
        .expect("stdout reader")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let exit = child.wait().expect("wait");

    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 100, "every line survives the hop: {stdout:?}");
    assert_eq!(lines.first().copied(), Some("line 0"));
    assert_eq!(lines.last().copied(), Some("line 99"));
    assert!(exit.success, "{exit:?}");
    // Loopback, so this is a floor; it exists to catch a per-line round trip.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "100 lines took {:?}",
        started.elapsed()
    );
}

#[test]
fn the_default_policy_refuses_a_host_that_is_not_in_known_hosts() {
    let Some(config) = fixture() else {
        return;
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let known_hosts = std::env::temp_dir().join(format!(
        "acpilot-known-hosts-{}-{nonce}",
        std::process::id()
    ));
    let strict = SshConfig {
        host_key: HostKeyPolicy::known_hosts(&known_hosts),
        ..config
    };

    let error = SshHost::connect(strict).expect_err("an unknown host key must be refused");
    let message = error.to_string();
    assert!(
        message.contains("known_hosts") || message.contains("not in"),
        "{message}"
    );
}
