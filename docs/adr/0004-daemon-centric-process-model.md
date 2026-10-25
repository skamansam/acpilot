# 0004 — One daemon per user, spawned on demand; projects are workspaces

- **Status:** accepted
- **Date:** 2026-10-24
- **Deciders:** project maintainers
- **Resolves:** D4 (local IPC)
- **Relates to:** D7 (one binary or two, left open — see §5), D10 (profile
  isolation, same conclusion reached from a different direction), ADR 0001 (the
  index this fills), TODO `P3-1`, `P3-2`, `P5-1`

## Context

Every surface is a client of one API (ADR 0001's D4 question, `docs/api.md`), and
the first one — `acpilot tui` — now exists. That makes the unglamorous questions
load-bearing, because they are what a person meets before any feature:

1. **Nothing starts the daemon.** `acpilot tui` assumes something is listening on
   `127.0.0.1:7777`; if not, it says "cannot reach …" and sits there. A terminal
   app that requires a second terminal and a remembered flag is a terminal app
   nobody opens twice.
2. **A project is a process.** The daemon takes `--workspace` once at startup, so
   editing another project means killing it and starting another. That is the
   behaviour that invites "one daemon per project".
3. **Localhost is not a trust boundary.** The documented rule is "a loopback bind
   needs no token". On a shared machine, that means any local user can open a
   session in your workspace and answer your permission prompts.
4. **The permission broker needs a listener.** It refuses immediately with
   `DecidedBy::Unattended` when no client is subscribed to its announcements
   (`permission.rs`), which is the correct policy and also the reason the pending
   list cannot be split across processes without making agents silently refuse.

Per-project daemons look like the simple answer to (2): switching project is
switching port. The cost is that (4) is now unsatisfiable across projects — an
agent in project A asks for a file write while the window showing project B is
the only one open, and A's broker sees no subscriber and refuses. The same split
multiplies the audit ledger, the keyring namespace and the agent registry, and
then needs a way to answer "is anything waiting anywhere", which is either an
orchestrator or N connections per client. That is real machinery to reintroduce a
property one process has for free.

## Decision

### 1. One daemon per user per machine

Profiles and workspaces are *inside* that process, not instances of it. This is
D10's recommendation ("one daemon with N stores") reached from the lifecycle side:
the reason not to run one daemon per profile is the same reason not to run one per
project — the pending list, the audit log and the keyring are singular facts.

Multiple daemons remain possible and supported, but **explicit**: a second one is
started with its own socket and name, for a remote machine, a different OS user,
hard isolation, or a test run. Nothing discovers it by accident.

### 2. Any client starts it on demand

The order, for `acpilot tui`, `acpilot gui`, and eventually every command:

1. Resolve the endpoint: `--api-url` / `ACPILOT_API_URL`, else the default
   discovery entry (see §3).
2. Probe `GET /v1/health` with a short timeout (300 ms — a daemon on loopback
   either answers instantly or is not there).
3. If absent and spawning is allowed: take the spawn lock, start `acpilotd`
   detached with the flags the client was given, poll health every 50 ms for up
   to 2 s, release the lock.
4. If the lock is held, another client is already spawning: wait for health
   rather than starting a second daemon.
5. Compare `health.version` with the client's. A mismatch is a refusal that names
   both versions and the endpoint — never an automatic kill, because a daemon
   may be mid-turn for a session someone else is watching.

`--no-spawn` disables step 3 for tests, CI and service managers. Spawning must not
depend on `PATH`: the client looks for `acpilotd` next to its own executable and
reports the exact path it tried when it cannot find it.

### 3. Unix socket by default; TCP loopback is opt-in and authenticated

This settles D4: **HTTP over a unix socket, one protocol everywhere**. A second
JSON-RPC transport would be a second error model, a second auth story and a second
client implementation for no gain.

| Platform | Default endpoint |
| --- | --- |
| Linux/BSD | `$XDG_RUNTIME_DIR/acpilot/acpilot.sock` (directory `0700`, socket `0600`) |
| macOS | the same, under `$TMPDIR` when `XDG_RUNTIME_DIR` is unset |
| Windows | `\\.\pipe\acpilot-<user>` (named pipe, ACL to the owning user) — not built yet, so loopback HTTP until P10-5 |
| Opt-in | `--addr 127.0.0.1:7777`, which **requires** `Authorization: Bearer` from a `0600` token file beside the socket |

The filesystem is the access control. That is the only reason "loopback needs no
token" is safe to promise: on the socket, local users other than the owner cannot
open the endpoint at all, and a TCP listener does not inherit that property, so it
does not inherit the exemption either. `docs/api.md` gains one sentence for this.

Discovery metadata lives beside the socket:
`$XDG_RUNTIME_DIR/acpilot/daemons/<name>.json` — `{name, socket, addr, pid, version}`.
It is written by the daemon at startup and removed at exit, and it is what lets
`acpilot --daemon <name>` attach to a named or remote instance. It is a directory
listing, not a registry service and not an orchestrator: no daemon talks to
another, because each one is already a server and a client that can name an
address does not need a middleman.

### 4. Idle exit, and a turn keeps it alive

A daemon exits after 30 minutes with no attached event stream, no pending
permission request and no turn in flight. The next client spawns it again. Without
this, every login accumulates one idle daemon; with it, closing the last window
ends the process, which is what a person expects from a helper they never started.

`--idle-timeout <seconds>` (`0` disables) exists for a daemon that is being
supervised deliberately. A turn in flight, a pending question, or an attached
stream all reset the clock — an agent mid-edit must not be killed because nobody
is watching the transcript.

### 5. Project switching is a workspace, not a process

`POST /v1/workspaces` already exists in the frozen route list with no handler;
this ADR is why it is the next one that matters. A workspace is a root on a host,
sessions carry `workspace_id`, and the TUI grows a picker rather than a restart.
`--workspace` at startup stays as "the first workspace this daemon was given", not
as "the only one it will ever serve".

D7 (one binary with subcommands vs separate `acpilot`/`acpilotd`) is deliberately
**left open**. What this ADR fixes is the constraint either answer must satisfy:
spawning must locate the daemon executable deterministically — a sibling file, a
hidden `acpilot daemon` subcommand, or an explicit `ACPILOT_DAEMON` — and never
rely on a `PATH` entry that an installer may not have written.

## Consequences

- **A crash takes every project's sessions with it.** Until transcript persistence
  (P2-8) lands, an auto-restarted daemon cannot resume a turn: the session is
  gone, and the person sees an empty list where their conversation was. Named
  daemons are the escape hatch for work that cannot afford that, and persistence
  is the fix.
- **One version, one process.** Upgrading `acpilot` while an older daemon is
  running produces a refusal until the daemon restarts (idle exit makes that
  happen on its own). This is better than a half-upgraded pair and worse than an
  atomic one; the refusal names the endpoint so the fix is obvious.
- **Spawn races need a lock, not a `try-bind`.** Two clients launching at once is
  a normal case (a desktop session restoring two windows), so the lock is load
  bearing rather than defensive.
- **The socket path is an interface.** Moving it breaks every client and any
  service unit a user wrote; it is versioned by the directory name if it ever has
  to move.
- **Server-side multi-user is not a goal.** Two users on one machine get two
  daemons and two sockets, not one shared daemon with tenants. That keeps the auth
  story to "who owns the socket".
- **The TUI must show which daemon it is on.** With named instances possible, a
  status line that only says "connected" is how someone answers a permission in
  the wrong project's daemon. The endpoint is already in the status line for this
  reason.

## Alternatives considered

- **One daemon per project.** Rejected in Context: the pending list, the audit
  ledger and the keyring are singular, and the broker refuses when nobody is
  listening. It converts a solved problem into a distributed one.
- **An orchestrator process that starts and routes to daemons.** Rejected: it is a
  second thing to install, supervise, upgrade and diagnose, and it solves nothing
  a discovery file plus `--daemon <name>` does not. It also becomes a single point
  of failure for every project at once, which is the failure mode it was meant to
  avoid.
- **A user service (systemd/launchd) as the default.** Rejected as the default,
  supported as an option: it writes to the user's system at install time, needs
  per-OS units kept in sync with flags, and makes "start the app" depend on a
  service manager that may be broken. Discovery does not care who started the
  daemon, so a supervised daemon and an auto-spawned one are the same client-side
  code.
- **Always connect, never spawn.** Rejected: it is today's behaviour, and it makes
  every surface require a daemon that nothing starts.
- **Per-project ports with a fixed range.** Rejected: collisions on a shared
  machine, a port count as a product limit, and no way to say which daemon is
  healthy without probing all of them.
- **Token on loopback as well.** Not rejected for policy reasons but deferred: on
  a socket the filesystem already provides the boundary, and requiring a token on
  the one endpoint that needs no listener configuration would make `curl` against
  a local daemon a two-step ritual. TCP takes the token.

## Implementation order

Recorded here so the pieces land in an order where each is usable:

1. **Discovery and auto-spawn** over the existing TCP endpoint: endpoint
   resolution, probe, lock, detached spawn, version refusal, `--no-spawn`.
   (Done — `acpilot-client::daemon` implements all five and `acpilot tui` uses it;
   `health.workspace` names the root so a client can refuse a daemon serving the
   wrong directory.)
2. **Unix socket transport** in the daemon and an HTTP-over-unix client, making
   the socket the default and TCP the authenticated opt-in. Requires a client
   transport that speaks HTTP over a unix socket; `reqwest` does not, so this is
   where that dependency decision is made. (Done 2026-10-24, with one correction
   to the sentence above: `reqwest` 0.13 does speak it — `ClientBuilder::unix_socket`
   — so the decision was to stay on the client the workspace already had rather
   than add a second HTTP stack for one transport. The daemon binds
   `$XDG_RUNTIME_DIR/acpilot/acpilot.sock` (`0700` directory, `0600` socket),
   removes it on Ctrl-C or `SIGTERM`, and refuses a path another daemon still
   answers on; `--socket <path>` moves it and `--addr <host:port>` is the opt-in
   TCP listener. The token on `--addr` landed with it rather than after it
   (2026-10-24): 32 random bytes in a `0600` file beside the socket, required on
   every request including streams, so the port never existed unauthenticated and
   no client had to be written twice. The Windows named pipe is P10-5's, so that
   platform's default endpoint is loopback HTTP for now — with the token, because
   a port there is the only listener it has.)
3. **Workspaces**: `GET/POST /v1/workspaces`, sessions in a workspace, TUI picker.
4. **Idle exit** with the turn/pending/stream conditions above, plus
   `--idle-timeout`.
5. **Named instances**: the `daemons/<name>.json` directory and `--daemon <name>`.
