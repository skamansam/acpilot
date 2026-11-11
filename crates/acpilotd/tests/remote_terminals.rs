#![cfg(unix)]

//! A terminal on another host, driven through a stand-in helper.
//!
//! The shell script below plays both halves of the transport: the `ssh` stand-in runs
//! the helper command the daemon built, and the helper answers `hello` and the `pty.*`
//! calls as a real one would — including a notification, because that is the shape the
//! daemon's dispatcher exists for. Nothing here is a PTY on this machine: the point is
//! that the daemon asks the far side, and that the bytes and the exit status come back.
//!
//! `POST /v1/terminals` is not the entry point, for the reason `terminal_routes.rs`
//! gives: the route refuses a session id it does not know, and a session needs a
//! profile's agent registry. What the route adds on top of this is the workspace lookup
//! and the session check, which the suite beside this one covers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use acpilot_host::ExecCommand;
use acpilot_proto::host::HostKind;
use acpilot_proto::terminal::TerminalSize;
use acpilotd::helper::{Connections, Transport};
use acpilotd::{Output, Terminal};

/// A helper that answers the PTY calls and echoes what it is typed.
///
/// `pty.open` insists on the workspace root as the working directory: a daemon that
/// forgot to send one, or sent the daemon's own path, gets an error instead of a
/// terminal, which is the assertion a log file would otherwise make.
const STAND_IN: &str = r#"#!/bin/sh
while read -r line; do
  # The *first* id: a `pty.*` request also carries the terminal's own id inside its
  # parameters, and a greedy match would answer with that number instead. This is the
  # same mistake `Connection::request` refuses to make.
  id=${line#*'"id":'}
  id=${id%%,*}
  data=$(printf '%s' "$line" | sed -n 's/.*"data":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"method":"hello"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocol":1,"methods":["hello","fs.read","exec","pty.open","pty.write","pty.resize","pty.kill","pty.close"]}}\n' "$id"
      ;;
    *'"method":"pty.open"'*)
      case "$line" in
        *'"cwd":"/home/box/project"'*)
          printf '{"jsonrpc":"2.0","id":%s,"result":{"id":0,"pid":4242,"program":"/bin/sh"}}\n' "$id"
          ;;
        *)
          printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"the cwd was not the workspace root"}}\n' "$id"
          ;;
      esac
      ;;
    *'"method":"pty.write"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"id":0,"bytes":1}}\n' "$id"
      printf '{"jsonrpc":"2.0","method":"pty.data","params":{"id":0,"data":"%s"}}\n' "$data"
      ;;
    *'"method":"pty.resize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"id":0,"cols":100,"rows":30}}\n' "$id"
      ;;
    *'"method":"pty.kill"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"id":0,"signalled":true}}\n' "$id"
      printf '{"jsonrpc":"2.0","method":"pty.exit","params":{"id":0,"code":0}}\n'
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no such method"}}\n' "$id"
      ;;
  esac
done
"#;

/// A helper from before `pty.open`: protocol 1, files and programs only.
const PTYLESS: &str = r#"#!/bin/sh
while read -r line; do
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"hello"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocol":1,"methods":["hello","fs.read","fs.list","fs.write","exec"]}}\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no such method"}}\n' "$id"
      ;;
  esac
done
"#;

/// The `ssh` the daemon runs, which is a shell that runs the command it was given.
///
/// The three arguments before the command are `-o BatchMode=yes <address>`; the helper
/// itself is one argument (`sh -s`, or `sh -c 'exec …'`), which is why joining with
/// `$*` is right where `exec "$@"` would run `-o` as a program.
const SSH: &str = r#"#!/bin/sh
shift 3
exec sh -c "$*"
"#;

fn write(path: &std::path::Path, script: &str) {
    std::fs::write(path, script).expect("write a stand-in");
    let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(path, permissions).expect("chmod");
}

/// A daemon-side host that reaches `helper` through the stand-in transport.
fn host(directory: &std::path::Path, helper: &str) -> (Arc<Connections>, Transport) {
    let ssh = directory.join("ssh");
    write(&ssh, SSH);
    let connections = Arc::new(Connections::new());
    // The stand-in is installed the way a release helper is, which also exercises the
    // install step: the daemon writes it to the directory it was told to use, then starts
    // that copy rather than anything on `PATH`.
    connections.prefer_bundle(Some(helper.as_bytes().to_vec()));
    connections.prefer_install_dir(Some(directory.join("bin").to_string_lossy().into_owned()));
    let transport = Transport::new(HostKind::Ssh, "box", ssh.to_string_lossy().into_owned());
    (connections, transport)
}

#[tokio::test]
async fn a_remote_terminal_paints_writes_resizes_and_ends() {
    let directory = tempfile::tempdir().expect("directory");
    let (connections, transport) = host(directory.path(), STAND_IN);
    let terminal = Terminal::spawn_remote(
        "t1".to_owned(),
        Some("s1".to_owned()),
        transport,
        Arc::clone(&connections),
        Some(ExecCommand::new("/bin/sh")),
        Some(PathBuf::from("/home/box/project")),
        TerminalSize { cols: 80, rows: 24 },
    )
    .expect("the host starts a terminal");
    assert_eq!(
        terminal.summary().command,
        "/bin/sh",
        "the command line is what the request named"
    );
    assert_eq!(terminal.summary().size, TerminalSize { cols: 80, rows: 24 });

    let (_replay, mut output) = terminal.attach();

    // The stand-in echoes what it is typed, so this is the input direction and the output
    // direction in one assertion: the bytes went out, and they came back as a frame.
    terminal.write(b"remote!\r").expect("the host takes input");
    let typed = tokio::time::timeout(Duration::from_secs(20), output.recv())
        .await
        .expect("a frame within the budget")
        .expect("a frame");
    let Output::Data(bytes) = typed else {
        panic!("the frame was {typed:?}, not output");
    };
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        "remote!\r",
        "the pane sees exactly the bytes the host echoed"
    );

    terminal
        .resize(TerminalSize {
            cols: 100,
            rows: 30,
        })
        .expect("the host takes a new size");
    assert_eq!(
        terminal.summary().size,
        TerminalSize {
            cols: 100,
            rows: 30
        },
        "and the pane reports it"
    );

    terminal.kill().expect("the host ends the program");
    let code = loop {
        let frame = tokio::time::timeout(Duration::from_secs(20), output.recv())
            .await
            .expect("a frame within the budget")
            .expect("a frame");
        match frame {
            // The host may paint on its way out; the exit is what this loop is for.
            Output::Data(_) => continue,
            Output::Exit { code } => break code,
        }
    };
    assert_eq!(code, Some(0), "the code the host reported");
    assert!(
        !terminal.summary().alive,
        "a pane whose program ended is not alive"
    );
}

#[test]
fn a_helper_without_a_pty_is_refused_by_name() {
    let directory = tempfile::tempdir().expect("directory");
    let (connections, transport) = host(directory.path(), PTYLESS);
    let error = Terminal::spawn_remote(
        "t1".to_owned(),
        Some("s1".to_owned()),
        transport,
        connections,
        None,
        Some(PathBuf::from("/home/box/project")),
        TerminalSize { cols: 80, rows: 24 },
    )
    .expect_err("a helper from before pty.open cannot start a pane");
    let message = error.error.to_string();
    assert!(
        message.contains("does not start terminals"),
        "the message says what the host cannot do: {message}"
    );
}
