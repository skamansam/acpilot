---
title: First flight
description: The five-minute version, and the honest footnote about what is missing.
---

<span class="pill pill--boarding">boarding</span>

A first flight usually means "start a session, ask an agent to change a file,
approve the diff". That flight is not available yet. It lands in the engine
phase (P2). You can do pre-flight today: open the terminal UI and read the
contract. The UI hosts the daemon when none is running.

## Pre-flight (works now)

`acpilot tui` probes `GET /v1/health`. When nothing answers, it runs the daemon's
core in its own process (P6-11). The client spawns nothing and writes nothing.
This is why the block needs no second terminal:

```bash
cargo build --bins     # acpilot and acpilotd; the client starts the sibling, not one on PATH
cargo run -p acpilot-cli -- tui
```

The same daemon answers `curl`. You can read the contract without opening a
window. The TUI stays attached until you quit it. The daemon outlives it. The
daemon listens on a socket, so point `curl` at the socket:

```bash
SOCK="$XDG_RUNTIME_DIR/acpilot/acpilot.sock"     # $TMPDIR on macOS
curl -s --unix-socket "$SOCK" http://localhost/v1/health | jq
curl -s --unix-socket "$SOCK" http://localhost/v1/openapi.json | jq '.info'
```

That is the whole cockpit at the moment: a healthy daemon and a contract with
nothing behind it yet. Short flight. Good landing.

## The flight you will actually take

When the engine phase (P2) lands, the sequence below is the target. We wrote it
down now because we design the API and the permission flow backwards from it.
If this feels like too many steps, the design is wrong, not the documentation.

```bash
# 1. Point Acpilot at a directory and an agent
acpilot workspace open ~/code/thing --agent claude-code

# 2. Ask for a change
acpilot session new --prompt "add a --dry-run flag to the CLI"

# 3. Watch it work, from here or from another terminal
acpilot watch @last

# 4. Approve or refuse the write
acpilot permissions ls
acpilot permissions allow 4f2a --once
```

<Aside type="note" title="Why `--once` is explicit">
Because "allow" is not a mood. It is a decision with a scope. The default is one
request. Making it permanent is a separate, auditable act.
</Aside>

## Where to look while you wait

- [Agents and ACP](../../systems/agents/) — what the daemon actually speaks.
- [Permissions](../../safety/permissions/) — the part that stops a bad turn from
  becoming a bad day.
- [Flight status](../../status/) — the current altitude, without euphemism.

<Aside type="tip" title="A note on patience">
The autopilot in the films flies the plane while everyone argues in the cabin.
Here the autopilot is the agent and the arguing is code review. The plane still
needs a pilot. That is you, and you are reading the manual. Good start.
</Aside>
