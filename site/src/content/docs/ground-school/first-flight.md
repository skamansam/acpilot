---
title: First flight
description: The five-minute version, and the honest footnote about what is missing.
---

<span class="pill pill--boarding">boarding</span>

A first flight usually means "start a session, ask an agent to change a file,
approve the diff". That flight is not available yet — the engine is the engine phase (P2). What you
can do today is pre-flight: start the daemon, read its contract, and confirm the
instrument panel lights up.

## Pre-flight (works now)

```bash
cargo run -p acpilotd
```

```bash
curl -s localhost:7777/v1/health | jq
curl -s localhost:7777/v1/openapi.json | jq '.info'
```

That is the whole cockpit at the moment: a healthy daemon and a contract with
nothing behind it yet. Short flight. Good landing.

## The flight you will actually take

When the engine phase (P2) lands, the sequence below is the target. It is written down now because
the API and the permission flow are designed backwards from it — if this feels
like too many steps, the design is wrong, not the documentation.

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
Because "allow" is not a mood, it is a decision with a scope. The default is one
request; making it permanent is a separate, auditable act.
</Aside>

## Where to look while you wait

- [Agents and ACP](../../systems/agents/) — what the daemon actually speaks.
- [Permissions](../../safety/permissions/) — the part that stops a bad turn from
  becoming a bad day.
- [Flight status](../../status/) — the current altitude, without euphemism.

<Aside type="tip" title="A note on patience">
The autopilot in the films flies the plane while everyone argues in the cabin.
Here the autopilot is the agent and the arguing is code review. The plane still
needs a pilot; that is you, and you are reading the manual. Good start.
</Aside>
