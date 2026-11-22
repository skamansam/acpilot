---
title: Design principles
description: Six constraints the code has to satisfy, and where each one is visible.
---

<span class="pill pill--boarding">boarding</span> &nbsp; The principles are the design. The pages they point at carry their own status

These are not slogans. Each one is a constraint that rules something out. Each
one has a place in the manual where you can see the consequence rather than the
adjective. Where a constraint is not yet satisfied, the page it points at
says which part is missing.

## 1. One behaviour, one implementation

If the TUI can do it, it goes through the API. The TUI, the desktop app, the
browser and the CLI are thin clients of one daemon. None of them reimplements a
session, a permission decision or a file write. A bug fixed in the daemon is
fixed in every surface. A surface that disagrees with another is a client
bug, not a second opinion.

The cost is real: the API has to be good enough for the most demanding client,
and a keystroke cannot take a round trip. The escape is that each UI owns its
text buffer and applies edits optimistically, so typing stays local while truth
stays in the daemon.

Where it shows: [Architecture](../../systems/architecture/) — the map and the
attach sequence.

## 2. The agent is a process, not a plugin

An agent is a program that speaks ACP, wrapped in a manifest. Acpilot spawns it,
supervises it, and can run it on another machine. Crashing it is allowed.
restarting it is a normal operation. Replacing it is an edit to `agents.toml`.
Nothing in the editor links against an agent, and no agent gets to be special
because it shipped first.

This is also why a program with no ACP support can still be an agent: run it in
a PTY, detect its turns. Expose it through the same session and turn model.

Where it shows: [Architecture](../../systems/architecture/) — the supervisor and
the two agent shapes, and [Agents and ACP](../../systems/agents/).

## 3. Permissions are durable objects

Every "allow" has an id, a scope, a decision and a log line. A permission
request is state in the daemon, not a dialog that exists only in the window that
happened to be focused. That is what lets a script answer one, lets a browser
tab opened later see it still pending. Lets the audit say who decided,
including `timeout` and `unattended` when nobody did.

The rule that follows from this: a decision is delivered once. The first resolve
wins, the same answer again is a no-op, and a different one is a conflict.

Where it shows: [Sessions](../../systems/sessions/) — the pending list and the
decision table, and [Permissions](../../safety/permissions/) — the policy itself.

## 4. Local-first, remote-capable

Editing a file requires no network. The daemon listens on a unix socket by
default, binds a TCP port only when told to. The bridge that lets a remote
driver in is off until someone enables it with an address. Reaching your office
machine is an opt-in with a token, not the default shape of the product.

Remote capability is a transport, not a second product. The same session model
runs over SSH, WSL or the bridge, with the same permission broker in the middle.

Where it shows: [Architecture](../../systems/architecture/) — the trust boundaries,
and [Hosts: SSH and WSL](../../systems/hosts/).

## 5. Boring dependencies, pinned

Prefer the crate everyone uses over the crate with the prettiest README. Where
the choice is contested, it gets an ADR with the numbers and the triggers that
would reverse it. Where a dependency has no stability contract, it is pinned
exactly and the pin is treated as a task rather than a footnote.

This is a principle about maintenance, not taste. The point is to minimise the
number of places where this project is the first serious user of a library.

Where it shows: the [status page](../../status/) for what is actually shipping,
and the dependency table in the repository README for the pins and the reasons.

## 6. Profiles are isolation boundaries, not themes

If two things must not share a credential or a history, they must not share a
profile. Agents, secrets, language servers, policy, preferences and sessions are
scoped to one profile. The code must not be able to guess which one it is
in. That is why the profile is a required argument on a mutating request rather
than ambient state. Why an unresolved name is an error instead of a silent
fallback to `default`.

The failure this prevents is the one with the worst consequences: running the
right agent with the wrong key, and finding out later.

Where it shows: [Profiles](../../systems/profiles/).

## What is deliberately absent

No hosted service, no synced profiles, no bundled model and no cloud
dependency. A profile is a local identity, not an account. The daemon does not
phone home, and the only listener it opens is the one you asked for. Those are
non-goals rather than principles, but they are the reason several of the
principles above can stay simple.
