---
title: Agents and ACP
description: What the daemon speaks, what it expects, and what it refuses to guess.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P2

An **agent** is a program that can read your files, run commands, and write
changes, driven over a protocol instead of a chat window. Acpilot's native
protocol is the [Agent Client Protocol](https://agentclientprotocol.com) — the
same one Zed uses — so an agent written for ACP works here without an adapter.

## What the daemon does with an agent

```mermaid
sequenceDiagram
    participant D as acpilotd
    participant A as agent process
    D->>A: initialize
    A-->>D: capabilities, protocol version
    D->>A: session/new
    A-->>D: session id
    D->>A: session/prompt
    A-->>D: session/update — chunks, tool calls, plans
    A->>D: session/request_permission
    D-->>A: allow, ask a human, or deny
    A-->>D: stopReason
    D->>A: session/cancel
```

Three things make this more than a pipe:

1. **The permission broker sits in the middle.** `session/request_permission`
   does not go to a UI as a toast; it becomes a pending record the daemon can
   audit, replay, and answer from policy.
2. **The filesystem callbacks go through the host.** When an agent asks to read
   a file, the daemon reads it from the host the workspace lives on — local, SSH,
   or WSL — not from whatever machine the daemon happens to be on.
3. **Every update becomes a sequenced event.** UIs render from the event stream,
   so a reconnecting client can replay and arrive at the same screen.

## Capabilities, not assumptions

Agents differ. The daemon records what each advertised during `initialize` and
the UIs adapt: an agent with no plan support gets no plan panel, not an empty
one. The same goes for `session/load` — if an agent cannot resume, the daemon
says so rather than pretending a restart is a resume.

<Aside type="caution" title="Protocol versions">
ACP v1 is the stable target. The v2 draft is tracked in an isolated module and
not adopted: an editor that chases a draft protocol twice a year is a science
project, not a tool.
</Aside>

## Model-backed agents

A local model is not an agent, but it can become one: a recipe tuned in the
[model lab](../../flight-test/) is promoted into an `acp-stdio` entry that runs
`acpilot-agent-lm`. The shim owns the conversation loop and reaches files,
terminals, and permissions only through the ACP callbacks above.

Which means the diagram on this page does not change. A model-backed agent is an
ordinary agent with an unusual prompt — same supervisor, same broker, same
transcript, same audit. That was the entire reason to build it that way.

## Trust

An agent is an arbitrary program with your repository and your network. The first
time a command is run it is hashed and pinned; if the binary changes, the agent
shows as **untrusted** until you say otherwise. This is not paranoia; it is the
same reason your shell asks before running a downloaded script.

## What is built today

The manifest and the process come first, because every later layer is built
against them. `agents.toml` is parsed and validated, secrets are resolved when
the profile loads, and the registry keeps one validated snapshot that a bad save
cannot half-replace. The supervisor starts an `acp-stdio` agent, keeps its stderr
in a bounded ring that is readable while it runs, restarts it on a crash when the
manifest says `on-crash`, and ends the process tree rather than only the wrapper
when a session closes.

`GET /v1/agents` lists what the daemon was given, in declaration order, with
`available` or `missing` from a `PATH` lookup on the daemon's host. That status
costs no process: listing agents cannot start one. A handshake-dependent
`unhealthy` or a trust-pinned `untrusted` arrives with sessions and trust storing,
not before.

A session is the client side of the protocol. It takes the supervisor's two
pipes and speaks ACP over them, so the same code serves a child process, an SSH
host, or a test. `initialize` completes before the handle is returned — no caller
holds a session whose capabilities it has not seen — and every `session/update`
becomes one numbered event before it is broadcast, so a slow subscriber drops
events but cannot renumber them. One turn runs at a time: a second prompt is
refused here rather than left to whatever an agent does with two. `session/cancel`
is a notification, so it reaches an agent mid-turn.

Two refusals matter while the rest is unbuilt. A permission request is answered
with the agent's own reject option and recorded as a notice, because no grant
should be implied by a session that has no policy yet. A request for a method
this build does not implement — `fs/*`, `terminal/create` — is answered with
`MethodNotFound` naming the method: not advertising a capability does not stop an
agent asking, and a request with no answer is a hang, not a refusal. All of it is
exercised against a mock agent that runs as a real child process, so the pipes,
the handshake and the tree kill are run in CI rather than only type-checked.

The permission broker (P2-6), the filesystem callbacks (P2-4) and terminals
(P2-5) are not built yet. `pty` and `http-jsonrpc` agents are accepted by the
schema but refused by the supervisor until their phases land, so the diagram
above is still the target, not a full description of today's daemon.
Model-backed agents arrive in P4.5, on top of the same layer, and inherit
whatever P2 gets right — or wrong — without a second implementation to blame.
