//! The Host seam, exercised against real processes.
//!
//! These are not unit tests of a wrapper: each one spawns something and checks
//! the observable difference. The PTY test in particular asserts the property
//! that makes a PTY worth having — a program *can tell* it is on a terminal —
//! because a pipe dressed up as a terminal passes every other check.

#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use acpilot_host::{ExecCommand, Host, LocalHost, Size};

/// Read a PTY until `needle` arrives, the child closes the terminal, or the
/// deadline passes. A PTY has no message boundaries, so "read one line" would
/// either hang or cut a line in half.
fn read_until(mut reader: Box<dyn Read + Send>, needle: &str, timeout: Duration) -> String {
    let (sender, receiver) = mpsc::channel();
    let needle = needle.to_string();
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    buffer.extend_from_slice(&chunk[..read]);
                    if String::from_utf8_lossy(&buffer).contains(&needle) {
                        break;
                    }
                }
                // A closed PTY reports EIO on some kernels rather than EOF.
                Err(_) => break,
            }
        }
        let _ = sender.send(String::from_utf8_lossy(&buffer).into_owned());
    });
    receiver.recv_timeout(timeout).unwrap_or_default()
}

#[test]
fn exec_keeps_streams_apart_and_reports_the_exit_code() {
    let mut child = LocalHost
        .spawn(&ExecCommand::new("sh").args(["-c", "printf out; printf err >&2; exit 3"]))
        .expect("spawn sh");

    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .take_stdout()
        .expect("stdout is piped")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    child
        .take_stderr()
        .expect("stderr is piped")
        .read_to_string(&mut stderr)
        .expect("read stderr");

    let exit = child.wait().expect("wait");
    assert_eq!(stdout, "out", "stdout carries only stdout");
    assert_eq!(stderr, "err", "stderr carries only stderr");
    assert_eq!(exit.code, Some(3));
    assert!(!exit.success, "a non-zero exit is not success");
}

#[test]
fn exec_runs_in_the_given_directory_with_the_given_environment() {
    let directory = std::env::temp_dir().join(format!("acpilot-host-{}", std::process::id()));
    fs::create_dir_all(&directory).expect("create temp dir");
    fs::write(directory.join("marker.txt"), "here").expect("write marker");

    let mut child = LocalHost
        .spawn(
            &ExecCommand::new("sh")
                .args(["-c", r#"cat marker.txt; printf "|%s" "$ACPILOT_TEST""#])
                .cwd(&directory)
                .env("ACPILOT_TEST", "set"),
        )
        .expect("spawn sh");

    let mut stdout = String::new();
    child
        .take_stdout()
        .expect("stdout is piped")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let exit = child.wait().expect("wait");
    fs::remove_dir_all(&directory).ok();

    assert_eq!(stdout, "here|set");
    assert!(exit.success, "{exit:?}");
}

#[test]
fn a_pty_is_a_terminal_and_a_pipe_is_not() {
    let script = r#"if [ -t 1 ]; then echo tty; else echo pipe; fi"#;

    let mut pty = LocalHost
        .spawn_pty(
            &ExecCommand::new("sh").args(["-c", script]),
            Size::new(100, 40),
        )
        .expect("spawn on a pty");
    let reader = pty.take_reader().expect("a pty has one reader");
    let output = read_until(reader, "tty", Duration::from_secs(10));
    let exit = pty.wait().expect("wait");

    assert!(
        output.contains("tty"),
        "a pty must be detectable as a terminal, got {output:?}"
    );
    assert!(exit.success, "{exit:?}");

    // The same command through a pipe, so the assertion above cannot pass by
    // accident on a platform where `-t 1` always reports true.
    let mut child = LocalHost
        .spawn(&ExecCommand::new("sh").args(["-c", script]))
        .expect("spawn on pipes");
    let mut stdout = String::new();
    child
        .take_stdout()
        .expect("stdout is piped")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    child.wait().expect("wait");
    assert_eq!(stdout.trim(), "pipe");
}

#[test]
fn a_pty_can_be_resized_and_killed() {
    let mut pty = LocalHost
        .spawn_pty(
            &ExecCommand::new("sh").args(["-c", "sleep 30"]),
            Size::DEFAULT,
        )
        .expect("spawn on a pty");

    assert_eq!(pty.size(), Size::DEFAULT);
    pty.resize(Size::new(120, 50)).expect("resize");
    assert_eq!(pty.size(), Size::new(120, 50));

    pty.kill().expect("kill");
    let exit = pty.wait().expect("wait");
    assert!(!exit.success, "a killed process did not succeed: {exit:?}");
}
