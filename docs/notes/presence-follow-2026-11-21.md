# Presence and follow — protocol design

**Status:** design, 2026-11-21. Not an ADR: it depends on D22 for the cross-daemon
half and on the P13 schedule for a task number.

**Answers:** D2 in `TODO.md` ("presence and follow, not distributed writes").

**Depends on:** ADR 0013 (one daemon per person, D5 identity), ADR 0004
(workspaces), the session stream's WebSocket shape, P7's file etags, and a D22
decision for anything that crosses a daemon boundary.

## What D2 asked for, and what can be built first

The answer was: a person can see who else is in a workspace, request another
person's cursor, see their open file and tabs, follow their cursor, and share a
tab into a chat. Writes stay host-authoritative; no CRDT, no distributed edit.

ADR 0013 decides one daemon per person. That splits D2 into two problems that
must not be designed as one:

1. **Same daemon** — one person's TUI, desktop window, browser tab and a second
   window of the same person. Presence here is real, useful (handoff from the
   terminal to the GUI) and needs no new trust boundary. This is what the first
   implementation can ship.
2. **Across daemons** — another person's daemon, which requires the D22 relay,
   an identity that is meaningful on both ends, and a decision about whether the
   relay can read presence or only route it. That is not buildable before D22,
   and a protocol that pretends otherwise will be rewritten.

So this design defines one wire protocol with a transport seam, and states
plainly that v1 carries it only inside a daemon. The cross-daemon carrier is a
later, separately reviewed addition; the frames below are chosen so that it can
be added without changing client behaviour.

## Invariants

- **The host is the authority.** Presence reports where a person is looking; it
  never moves another person's cursor, edits a buffer, or answers a permission.
  Following is a view, not remote control. Remote control is the ACP bridge's
  job and keeps its own scopes.
- **Presence is not data access.** A follower receives coordinates and a file
  path, never file contents. The follower's client loads the file through the
  existing authenticated file route, so a profile grant is still what decides
  whether the text is readable.
- **Ephemeral.** The roster lives in memory. No table, no transcript, no audit
  of cursor positions, no history after the process exits. A daemon restart is a
  room where everyone has left.
- **Scoped to a workspace.** A workspace id is the room. Being in profile A
  says nothing about profile B, and a client attached to two workspaces gets two
  rosters.
- **Opt-in for the sensitive part.** Being listed as present is on by default
  for clients in the workspace. Sharing a cursor is off until the person turns
  it on for that client, because a cursor leaks file names and the shape of
  unannounced work. It can be turned off at any moment and the effect is
  immediate.
- **Bounded.** A roster is capped, frames are size-capped, and update rates are
  capped. Presence is a status line, not a streaming service.

## Identity

A participant is `(user, client)`:

- `user` is the D5 subject (`sub`) and display name. With one account in v0.1,
  every participant has the same user; that is expected, not a bug.
- `client` is a random id generated per client process, not persisted. It is how
  a follow targets one window rather than "wherever that person is".
- `kind` is `tui`, `gui`, `web`, `cli` or `embedded`.
- The daemon never invents a user from a connection. On the unix socket the
  owner is the account; on TCP the JWT's `sub` is the identity and the profile
  header must resolve to a workspace the token may use.

The token is the authorization for the socket, and the cookie or bearer token
is the authorization for the WebSocket. Presence has no separate secret and no
anonymous mode.

## Transport

Two routes, mirroring the session stream:

```
GET  /v1/workspaces/{workspace}/presence
WS   /v1/workspaces/{workspace}/presence/stream
```

`GET` returns a snapshot for clients that only want a status line. The WebSocket
is for clients that show or follow cursors.

The server sends a **full roster** on connect and after every change, not a
delta:

```json
{
  "type": "presence",
  "seq": 42,
  "workspace": "ws_...",
  "participants": [
    {
      "user": { "id": "usr_...", "name": "Sam" },
      "client": "c_7f3a",
      "kind": "tui",
      "since": 1763683200,
      "activity": "active",
      "sharing": "cursor",
      "cursor": {
        "path": "crates/acpilotd/src/lsp.rs",
        "line": 99,
        "character": 4,
        "encoding": "utf-16",
        "etag": "1763683100-18221-9f3c1a2b4d5e6f70"
      },
      "following": null
    }
  ]
}
```

A full roster is affordable because the cap is 16 participants; a delta protocol
would add sequence-gap recovery for a frame smaller than the snapshot it
replaces. `seq` is a per-workspace counter that starts at 1 when the daemon
starts. A client that sees a lower `seq` than before, or reconnects, treats the
next frame as a reset and drops any follow state that names a missing client.

No replay, no `after` parameter, no ring: presence has no history worth
replaying. This is the deliberate difference from `/v1/sessions/{id}/stream`.

Client frames:

```json
{ "type": "hello", "client": "c_7f3a", "kind": "tui", "share": "none" }
{ "type": "share", "mode": "cursor" }
{ "type": "cursor", "path": "...", "line": 99, "character": 4,
  "encoding": "utf-16", "etag": "..." }
{ "type": "follow", "target": "c_91bd" }
{ "type": "unfollow" }
{ "type": "viewport", "path": "...", "top": 80, "bottom": 120, "etag": "..." }
{ "type": "heartbeat" }
```

The server ignores unknown frame types rather than closing the socket, so a
newer client talking to an older daemon degrades to "no presence feature"
instead of a disconnect loop. A malformed frame (wrong types, a path that
escapes, line numbers beyond a bound) closes with `1008`.

## Coordinates

A cursor is a position in a file, not a position in someone's editor:

- `path` is workspace-relative, normalized, and must resolve inside the
  workspace root by the same check the file routes use. Absolute paths and `..`
  are refused; the refusal is a close, because a client sending one is either
  broken or probing.
- `line` is zero-based, `character` is a UTF-16 code-unit offset when
  `encoding` is `utf-16` (the LSP default), or a UTF-8 byte offset when it is
  `utf-8`. The daemon does not convert between them for presence; it validates
  bounds and passes them through, and a client that cannot read the stated
  encoding does not render the caret rather than guessing.
- `etag` is the file route's `mtime-size-hash16` token. A follower compares it
  with the file it has open; a mismatch means "the target is on a different
  revision", and the follower reloads or shows a stale banner. Presence never
  carries the text that would let it compute an offset itself.

`viewport` uses `top`/`bottom` line numbers and the same etag, for following a
person reading rather than editing. It is sent only to clients that accepted a
follow; it is not part of the public roster.

## Share, follow, end

Sharing is a mode on the target client: `none` (default) or `cursor`. Setting it
is local, immediate and not sticky across restarts. A client that has never
shared sends no `cursor` frames, and the roster shows `sharing: "none"` with no
cursor object.

Following is a request, not a subscription the follower can create alone:

1. Follower sends `follow { target: "c_91bd" }`.
2. The daemon checks that both clients are in the same workspace, that the
   target is sharing, and that the target is not already followed by this
   client. Failures are a `follow_end { reason }`, not silence.
3. The target client receives `follow_request { from: {user, client, kind} }`
   and shows who is asking. Accepting is a local action; the default is no.
   "Always allow this client" is an in-memory choice for the life of the
   connection, never a stored grant.
4. On accept, the daemon records the edge and sends `follow_started` to both.
   The target's client begins sending `viewport` at no more than 4 Hz, coalesced
   to the latest state; the daemon relays it only to followers.
5. The follower may send `unfollow`; the target may revoke by setting sharing to
   `none`, which ends every follow of that client. Closing the file, leaving the
   workspace, disconnecting or 45 seconds without a heartbeat ends it too, with
   a `follow_end` naming the reason.

A client follows at most one target at a time. A second `follow` replaces the
first and sends `follow_end` to the old target, so the UI has one state rather
than a graph. The target may be followed by up to 8 clients; the cap is enforced
at request time.

The daemon does not persist follows and does not grant anything by them. A
follower that loses the target sees a message, not a frozen cursor.

## Rates, sizes and expiry

| Thing | Limit |
| --- | --- |
| participants per workspace | 16 |
| cursor frames per client | 20/s sent, 10/s relayed (latest wins) |
| viewport frames per target | 4/s |
| frame size | 4 KiB |
| path length | 1 024 bytes |
| heartbeat | every 15 s; 45 s without one expires the participant |
| follow lifetime | 30 min, renewed by activity, then re-request |

Expiry is the daemon's, not the client's: a crashed TUI must not leave a ghost
cursor in someone else's window. Activity is derived from frames the client
already sends; there is no separate "I am idle" claim to trust.

## Privacy and logging

- Presence is memory only. Nothing is written to `profile.sqlite`, the session
  transcript or the audit log. The audit log records auth and permission
  decisions, not where somebody looked.
- The roster exposes display name and client kind. It does not expose email,
  token claims beyond `sub`, IP address, or the names of files a person has open
  unless they are sharing a cursor.
- Logs at `debug` may record join/leave with the client id; paths and positions
  are not logged. A crash report must not carry a roster.
- `[presence] enabled = false` in the profile's `config.toml` disables both
  routes for that profile, and a client that asks anyway gets `404` like any
  other absent feature. Default is on, cursor sharing off.
- A workspace with no other authenticated client is a roster of one; the UI
  hides it rather than showing a permanent "1 person here".

## Failure behaviour

- **Daemon restart.** Every socket closes, `seq` resets, clients reconnect and
  receive a roster. Follow state is gone; the UI says so instead of trying to
  resume against a new process.
- **Missed frames.** The roster is a snapshot, so there is nothing to miss; the
  next frame is correct. Cursor frames are last-write-wins and carry their own
  etag.
- **Slow client.** The server drops cursor frames for a socket whose send queue
  is behind rather than buffering without bound. A dropped frame is invisible;
  the next one catches up.
- **Target crashes.** Heartbeat expiry removes it within 45 s, and the follower
  gets `follow_end { reason: "gone" }` at that point.
- **Wrong workspace.** The route resolves the workspace from the id and the
  caller's grants before the upgrade; an unknown or ungranted workspace is the
  same `404`, not a different error that confirms it exists.

## The cross-daemon seam

The frames above are transport-neutral. A relay carrier sends the same
`presence` snapshot and the same `follow_request`/`viewport` frames, with two
additions that are deliberately out of scope until D22 is decided:

- **Authenticity.** A receiving daemon must know that a roster came from the
  daemon that owns the workspace, not from the relay. That is a signature over
  the frame by the workspace owner's key, or a relay that is trusted to assert
  it. Which one depends on whether D22 chooses a server-trusted or an
  end-to-end encrypted model.
- **Membership.** A group's roster is the intersection of "in the group" and
  "in the workspace". Group membership is D22's; this protocol does not invent a
  second list.

Until then, a daemon refuses a workspace presence request that names a remote
workspace, and the UI does not offer a follow button for a participant it cannot
reach.

## What this design does not build

- No CRDT, no operational transform, no shared buffer. A follower cannot type
  into the target's editor; the target's writes are the only writes.
- No remote control. The ACP bridge already carries scoped driver actions; a
  cursor-follow that could approve a permission prompt would be a hole in it.
- No persisted presence, "last seen", typing indicators, cursor history or
  analytics.
- No sharing a tab into chat yet. That needs a chat message whose payload names
  a workspace and path and whose recipient can resolve them; it is a P13
  feature, not a presence frame.
- No audio/video signalling. P13-4 owns calls; presence is not their signalling
  channel.

## Implementation shape

1. A `presence` module in `acpilotd` owning the per-workspace roster behind a
   `tokio::sync::broadcast` channel, plus the two routes and the WebSocket
   upgrade. No store, no migration.
2. Register the routes beside the session stream so they inherit the same auth
   middleware. The socket listener needs nothing; the TCP listener needs the D5
   session credential, which is why this follows ADR 0013 rather than preceding
   it.
3. A client-side `Presence` state machine in `acpilot-client`: connect,
   snapshot, heartbeat, share, follow, viewport, reconnect with reset. The TUI
   status line and the GUI avatars read it; neither owns the protocol.
4. Tests: two clients in one workspace see each other; a cursor frame outside
   the root closes; sharing off produces no cursor; follow requires acceptance;
   revoking sharing ends it; heartbeat expiry removes a crashed client; the
   roster caps at 16; a reconnect resets `seq` and drops a stale follow; the
   routes are absent when `presence.enabled = false`.
5. The cross-daemon carrier is a separate design after D22. The client state
   machine is written so that its transport is an enum, not a rewrite.

## Questions this design leaves open

1. **Does the first version show presence at all when there is only one
   account?** The protocol supports it; the UI may choose to show other clients
   of the same user only in a "windows" menu rather than an avatar row.
2. **Should follow be per-file or per-workspace?** This design follows the
   target's active file and viewport, because that is what "follow their cursor"
   means; a workspace-level follow that opens whatever they open is a larger
   permission and is not in v1.
3. **Does the relay carry presence when nobody is in the chat?** If presence is
   a chat-plane feature, it inherits the chat's retention and trust model. If it
   is a workspace-plane feature, it needs its own always-on connection. D22's
   answer decides which is cheaper.
