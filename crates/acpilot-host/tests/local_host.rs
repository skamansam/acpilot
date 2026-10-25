//! The Host seam, exercised against real processes.
//!
//! These are not unit tests of a wrapper: each one spawns something and checks
//! the observable difference. The PTY test in particular asserts the property
//! that makes a PTY worth having — a program *can tell* it is on a terminal —
//! because a pipe dressed up as a terminal passes every other check.

#![cfg(unix)]

use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use acpilot_host::{ExecCommand, Host, HostError, LocalHost, Size};

/// A directory this test owns. Named for the process so two tests running at
/// once do not collide, and removed by the test that created it.
fn scratch(name: &str) -> std::path::PathBuf {
    let directory =
        std::env::temp_dir().join(format!("acpilot-host-{name}-{}", std::process::id()));
    fs::create_dir_all(&directory).expect("create temp dir");
    directory
}

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
    let directory = scratch("exec");
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

#[test]
fn files_are_read_and_written_as_bytes_and_missing_ones_are_errors() {
    let directory = scratch("files");
    let text = directory.join("notes.txt");
    let binary = directory.join("bytes.bin");

    LocalHost
        .write_file(&text, b"one\ntwo\n")
        .expect("write text");
    assert_eq!(
        LocalHost.read_file(&text).expect("read text"),
        b"one\ntwo\n"
    );

    // Bytes, not text: the caller decides what to refuse, and a lossy conversion
    // here would hand back something that is not the file.
    LocalHost
        .write_file(&binary, &[0xff, 0x00, 0xfe])
        .expect("write bytes");
    assert_eq!(
        LocalHost.read_file(&binary).expect("read bytes"),
        [0xff, 0x00, 0xfe]
    );

    let missing = directory.join("absent.txt");
    match LocalHost.read_file(&missing) {
        Err(HostError::ReadFile { path, source }) => {
            assert_eq!(path, missing);
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        other => panic!("a missing file must name itself: {other:?}"),
    }

    // A missing parent is an error, not a directory tree to build.
    let orphan = directory.join("no/such/dir/file.txt");
    assert!(
        LocalHost.write_file(&orphan, b"x").is_err(),
        "writing under a missing parent must fail"
    );
    assert!(!directory.join("no").exists());

    fs::remove_dir_all(&directory).ok();
}

#[test]
fn canonicalize_resolves_a_symlink_out_of_its_directory() {
    let directory = scratch("canonicalize");
    let outside = scratch("canonicalize-outside");
    let target = outside.join("secret.txt");
    fs::write(&target, "not for the agent").expect("write target");

    let link = directory.join("link.txt");
    std::os::unix::fs::symlink(&target, &link).expect("create symlink");

    let resolved = LocalHost.canonicalize(&link).expect("resolve the link");
    assert_eq!(
        resolved,
        fs::canonicalize(&target).expect("resolve the target"),
        "a containment check must see where the link goes, not where it sits"
    );
    assert!(!resolved.starts_with(&directory));

    // A path that does not exist cannot be resolved, which is why a write
    // resolves its parent instead.
    assert!(LocalHost.canonicalize(&directory.join("new.txt")).is_err());
    assert_eq!(
        LocalHost
            .canonicalize(Path::new(&directory))
            .expect("resolve the directory"),
        fs::canonicalize(&directory).expect("resolve locally")
    );

    fs::remove_dir_all(&directory).ok();
    fs::remove_dir_all(&outside).ok();
}
