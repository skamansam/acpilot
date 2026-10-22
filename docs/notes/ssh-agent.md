# P0-4 — ACP over an SSH channel, and a PTY beside it

Measured 2026-10-21 on WSL2 (Ubuntu 24.04.5, kernel 6.18.33.2-microsoft-standard-WSL2),
OpenSSH_9.6p1 Ubuntu-3ubuntu13.20, `russh` 0.64.1, Rust 1.97.0. Loopback only:
client and server are the same kernel, so every number here is a *floor*, not a
prediction of a real remote link.

| side | thing | version |
| --- | --- | --- |
| server | OpenSSH `sshd` | 9.6p1, Ubuntu 3ubuntu13.20 |
| server | host | Ubuntu 24.04.5 on WSL2, kernel 6.18.33.2 |
| client | `russh` | 0.64.1 |
| transport | loopback TCP | `127.0.0.1:2222` |

The instrument is `spikes/ssh-spike`: `exec` (stdout, stderr, exit code), `pty`
(request, run, resize while a program watches), and `acp` (this binary running
as a mock agent on the far side, answering `initialize`, `session/new` and
`session/prompt` in real ACP shapes). `bench` runs all three.

```bash
cd spikes/ssh-spike
./sshd.sh start                 # unprivileged sshd on 127.0.0.1:2222, key-only
cargo run --release -- bench
./sshd.sh stop
```

## The harness, and why it is not docker compose

`TODO.md` said `docker compose up sshd`. This box has no usable container runtime
(Docker Desktop's WSL integration is off; rootless podman's socket refuses), so
`sshd.sh` starts the distro's own `sshd` instead. That is not a weaker test: it is
the same OpenSSH, the same key exchange, the same channels — only the process
boundary differs, and the process boundary is not what P0-4 asks about. It runs
unprivileged on a high port with `PasswordAuthentication no`, `UsePAM no`, a
throwaway host key and a throwaway client key under `target/` (gitignored), so
nothing privileged and no key is committed.

## Verdict

1. **The exec channel is a byte stream, not a message channel.** The far side
   wrote 103 JSON lines with one `write` and one `flush` each. They arrived in
   **18–33 SSH `DATA` events**, with **up to 37 frames in a single event**
   (largest event 6,007 B). An ACP client that treats each `DATA` as one message
   would process a fraction of the transcript; the client must buffer and split
   on `\n`, which is exactly what P0-1's SDK transport does.
2. **No partial lines were seen on loopback — and that proves nothing.** Zero of
   the 103 frames straddled an event boundary on any run. TCP on one kernel hands
   over in large pieces; a real link with a smaller MSS or a slow reader will split
   lines. The client must handle a line arriving in two events, so it is written
   to (and counted), not assumed away.
3. **stderr stays out of the frame channel.** A probe that writes to both and
   exits 3 came back with `stdout "out"`, `stderr "err"`, and exit code **3**
   intact: stderr arrives as `SSH_EXTENDED_DATA` with `ext = 1`, not as stdout
   noise. The spike fails the run if the agent writes anything on an extended
   stream, because a frame stream polluted by a warning is the failure mode that
   looks like a protocol bug.
4. **A PTY survives the hop, resize included.** `request_pty` then `window_change`
   took a watching `stty size` from `40 100` to `50 120` on every run, and `tty`
   answered a real `/dev/pts/N`. Nothing about the resize needed the session to
   be re-established.
5. **The cost is the connection, not the messages.** Connect plus publickey auth
   is 70.7–113.9 ms; after that an exec round trip is 44.7–49.8 ms and the whole
   103-frame ACP exchange is 43.1–51.4 ms. So a host must be connected once and
   kept, for the same reason the WSL helper is a session (P0-5: 140 ms to spawn,
   0.1 ms per message).

| what | n | measured |
| --- | --- | --- |
| connect + publickey auth | 5 | 70.7 / 77.0 / 78.8 / 81.7 / 113.9 ms |
| exec probe (stdout+stderr+exit 3) | 5 | 44.7 / 44.8 / 48.5 / 48.8 / 49.8 ms |
| STDOUT events for that probe | 5 | 1 (4 B) — a small write can arrive whole |
| PTY `stty size`, before → after resize | 5 | `40 100` → `50 120` every run |
| PTY session incl. resize + `tty` | 1 | 97.9 ms |
| ACP: 103 frames (100 notifications) | 5 | 43.1 / 47.2 / 47.8 / 50.5 / 51.4 ms |
| ACP: `DATA` events for 103 frames | 5 | 33 / 28 / 27 / 26 / 20 |
| ACP: most frames in one event | 5 | 13 / 12 / 22 / 16 / 24 (37 on a later run) |
| ACP: frames split across events | 5 | 0 |

## Consequences for `acpilot-host::ssh` (P8-1)

- **Exec channel for ACP, PTY for terminals.** Do not run ACP through a PTY:
  that adds echo and CRLF, which is the ConPTY problem P0-5 found, for nothing.
- **Frame on newline, with a cap.** Keep a buffer across `DATA` events and bound
  it (a 16 MiB line is a bug or an attack, not a message). The `agent-client-protocol`
  `Lines` transport already does this; the point is that the channel cannot.
- **Drain stdout and stderr concurrently.** SSH's channel window is consumed by
  both streams; an agent that logs to stderr will block on a full window if the
  daemon only reads stdout. The spike reads both in one loop, and P2-2's ring
  buffer is where stderr belongs.
- **Connect once per host and multiplex.** ~50 ms per exchange is fine; ~78 ms per
  prompt is not. The same conclusion as P0-5, from a different transport.
- **Resize is a channel message, not a reconnect** (`window_change`), so the
  terminal API's resize maps straight onto it.
- **The host key needs a policy.** The spike accepts any key (`Ok(true)`) because
  a TOFU prompt would block a measurement; `known_hosts` verification is P8-1's
  job and is not answered here.

## Limitations

- **Loopback, one kernel**: no RTT, no loss, no slow reader, no window exhaustion,
  no `ProxyJump`. Every latency here is a lower bound and the framing evidence is
  the *conservative* case (coalescing without splitting).
- One server version (OpenSSH 9.6), one client (russh 0.64), one user, key auth
  only — no agent forwarding, FIDO keys, `Match` blocks or ControlMaster. The
  README's escalation path (shell out to `ssh` when russh cannot express the
  config) is untested.
- The agent is a **mock**: it speaks real ACP methods and notification shapes, but
  it is not a third-party agent. P0-1 already ran two real agents over stdio; what
  P0-4 needed to know is whether the SSH channel preserves their framing, and that
  is what was measured.
- No multiplexed channels: one session at a time. P8-1's keepalive, reconnect and
  backoff are unmeasured.
- The sshd fixture is not a hardened server; it exists for one user on loopback.
