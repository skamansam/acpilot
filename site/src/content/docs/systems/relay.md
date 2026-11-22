---
title: The relay
description: Groups, device membership, and presence for people whose daemons are not the same process.
---

<span class="pill pill--hangar">hangar</span> &nbsp; Chat relay (D22) &nbsp;·&nbsp; first slice

Acpilot's daemon is one per person. Two people working together therefore need a
carrier between their daemons, and the relay is that carrier. It does not run
agents, read profiles, or know a workspace. It holds groups, device keys, and
ephemeral cursors.

`crates/acpilot-relay` is a separate binary, `acpilot-relay serve`, with its own
data directory and one SQLite file per group. It is **server-trusted** in v1: the
operator can read what the relay stores. That is the same trust a self-hosted team
chat has, and the UI is expected to say it before a person joins a group. The
envelope is shaped so end-to-end encryption can be added later without moving a
message, but E2EE is not built.

## Routes

| Route | What it does |
| --- | --- |
| `POST /v1/groups` | Create a group from a self-signed first invite |
| `POST /v1/groups/{group}/join` | Add the device an inviter signed for |
| `GET /v1/groups/{group}/members` | The roster, including revoked keys |
| `POST /v1/groups/{group}/leave` | Revoke this device and drop its presence |
| `GET /v1/groups/{group}/presence` | Active cursors, stale after 45 seconds |
| `POST /v1/groups/{group}/presence` | Publish one cursor frame |

Every route except create and join requires a device-signed request:
`X-Acpilot-Device`, `X-Acpilot-Timestamp`, and `X-Acpilot-Signature`. The
signature covers the method, the path, the timestamp, and the SHA-256 of the
body. A timestamp outside five minutes is refused, and the device must be on the
group's active roster. The group id in the path is not a capability. It is
checked against the roster before anything else.

An invite is signed by a member's device key. The first invite is self-signed and
creates the group. Later invites name the inviting device, so a forwarded blob
cannot be edited to point at another group. Joining records the inviter, which is
the edge transitive membership follows later. Leaving is a revocation, not a
delete: a revoked key can no longer post, read or publish presence. History is a
separate retention question and is not built yet.

## Presence

A presence frame is a cursor, not a message: workspace, path, line, character and
the `etag` it was drawn over. It is kept in memory and dropped after 45 seconds,
or as soon as the device leaves. The same rules as same-daemon presence apply:
the roster is opt-in, a follower never writes, and cross-daemon follow needs the
target's local acceptance.

## What is not here

Message append/history, the resumable message stream, cross-relay federation and
failover, attachments, search, E2EE, and a client UI. The design note
[`docs/notes/chat-relay-2026-11-21.md`](https://github.com/skamansam/acpilot/blob/main/docs/notes/chat-relay-2026-11-21.md)
records the full shape and the open questions. Until those routes exist, presence
is the only thing a second daemon can see through the relay.
