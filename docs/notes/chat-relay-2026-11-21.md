# D22 — the chat relay, and the trust model presence inherits

Status: design plus the first server slice. `crates/acpilot-relay` implements
group creation, signed invites and joins, roster/leave revocation, and ephemeral
presence. Message append/history, cross-relay federation, E2EE and a client UI are
not built. The envelope and storage shape below are what those routes will use.

D21 asked where a group's messages live. D22 asks who carries them and what the
carrier sees. The answer matters twice. Chat is the obvious user. Presence
between two people's daemons (D2) has no carrier without it. Building presence on
a peer-to-peer mesh first would create a second trust model. It would also create
a second NAT story and a second way for one person to appear as another.

The recommendation is **a self-hosted relay that is trusted with message bodies in
v1**. Group membership is asserted by signed invites, and every member's device
keys are recorded. End-to-end encryption is a later mode over the same envelope,
not a different product. The reasoning is in §2. The cost is in §7.

## 1. What the relay is

One process, one SQLite database, one HTTPS listener, and one WebSocket endpoint.
It does not run agents, hold profiles, or know a daemon's workspace. A group is a
UUID. A message belongs to exactly one group. A member is a device key plus a
display name, not a profile.

```
POST /v1/groups                        create, returns invite blob
POST /v1/groups/{group}/join           accept an invite, returns membership
GET  /v1/groups/{group}/messages       history, sequence-ordered
POST /v1/groups/{group}/messages       append one message
GET  /v1/groups/{group}/stream         SSE/WS: messages and presence frames
GET  /v1/groups/{group}/members        roster and device keys
POST /v1/groups/{group}/leave          remove this device
```

Every route except `join` requires a device-signed request. The group id in the
path is not a capability: membership is checked against the database before the
group id is used for anything else.

## 2. The trust decision

Two models are defensible.

**Server-trusted.** The relay stores plaintext (or encrypted at rest with a key the
relay holds). It can search, retain, and display history to a member who joins later.
A compromised relay reads everything. This is the model of a self-hosted Slack or
Mattermost. It is honest about the operator: the person running the relay can
read the group.

**End-to-end encrypted.** Each device has a long-term key. A group has a symmetric
key distributed in the invite. The relay stores ciphertext and opaque metadata. A
compromised relay learns who talks to whom and when, not what. Search and
server-side retention become the client's job, joining a group needs a key
delivery path, and a lost device is a key-rotation problem.

v1 chooses server-trusted, for three reasons.

1. **The self-hosted story is the trust story.** A person who runs the relay on
   their own machine is the operator. E2EE Would protect them from themselves
   while making the common case (one team, one relay) harder to use.
2. **Presence needs a roster the relay can authenticate.** A cross-daemon cursor
   frame has to be attributable to a member. A signed device key in the roster
   gives that. E2EE Does not change it, but it does add a second key distribution
   problem before the first roster exists.
3. **Search is a product requirement, not a later optimisation.** The answers to
   D21 describe groups that retain history for years. Client-side search over
   ciphertext is a different feature with a different index.

The envelope is designed so E2EE can be added without moving messages: a body is
`{ "encoding": "utf8" | "age-v1", "content": ... }`. A relay that does not
understand `age-v1` stores and forwards it. A group's mode is recorded at
creation and cannot be downgraded.

## 3. Identity and membership

A person's daemon has an Ed25519 device key, generated locally, never uploaded
with its private half. The public half is the member's identity on the relay. It
is not the D5 account key: a person may run two daemons (laptop, workstation) and
both are the same person. The roster carries a person id plus one key per
device. The person id is a random UUID, not an email or a profile name.

An invite is a blob, not a row the relay must pre-create:

```
{
  "group": "<uuid>",
  "relay": "https://relay.example",
  "name": "weekend project",
  "person": "<uuid>",
  "device": "<ed25519 public key, 64 hex>",
  "issued_at": "<RFC 3339>",
  "expires_at": "<RFC 3339>",
  "signature": "<ed25519 over the canonical JSON above>"
}
```

The inviter signs it with a device key already in the group, so a forwarded blob
cannot be edited to point at another group. The first invite is self-signed and
creates the group. `join` verifies the signature against a member key (or accepts a
self-signed first invite only when the group does not exist), then records the
joining device. The relay never sees a private key.

Transitive membership follows from the same rule: any member may invite, and the
relay records the inviter. "Any user connected to any user" is therefore a graph
the relay can answer, with the inviter as the edge. Removing a member adds their
keys to a revocation list. A revoked key can no longer post, read, or publish
presence. History stays, because deleting a person's messages from a group that
retained them is a separate policy question.

## 4. Ordering and failover

Each group has exactly one authoritative relay at a time. The relay assigns a
monotonic `seq` per group inside the write transaction. Messages are immutable and
`seq` is the only order a client trusts. A client that reconnects sends the last
`seq` it has and receives everything after it.

This is deliberately not gossip. Multi-writer gossip needs a CRDT or a vector
clock. The chat transcript is not a document that merges: two messages with
the same logical time are still two messages. One writer per group is the smallest
model that gives a total order and a resumable cursor.

Failover is a restore, not a consensus protocol. A group's database is a file. A
new relay is started against a copy, and it refuses to serve until it has replayed
to the last `seq` it can prove. Clients reconnect to the new address. The invite's
`relay` field is updated by a signed membership event. A split brain is two
relays accepting writes for one group. The group row names one relay, and clients
refuse a `seq` that goes backwards. Operators do not have to remember not to
start two copies.

## 5. Presence over the relay

Presence is a message class, not a separate protocol:

```
{ "kind": "presence", "seq": 41, "from": "<device>", "at": "...",
  "body": { "workspace": "...", "path": "src/lib.rs", "line": 12,
            "character": 4, "etag": "..." } }
```

The relay fans it out and does not persist it: presence rows are dropped on ack or
after 45 seconds. History reads skip them. The same rules as the same-daemon
design apply unchanged: full roster on connect, opt-in cursor sharing, follow
requires the target's acceptance. A follower never writes. The relay adds the
one thing a local daemon cannot: a roster signed by device keys. "Who is this
cursor from" has an answer the receiver can check.

Cross-daemon follow is a control message (`follow.request`, `follow.accept`,
`follow.stop`) carried the same way, with the target's device key as the
authority. A relay cannot make a follower's daemon accept a target it did not
approve. The acceptance is a local state change, and the relay only carries the
frames.

## 6. Storage

One database per relay, one `messages` table per group, is tempting. It is wrong
for retention and corruption. A group that runs for years should be a file that
can be backed up, moved, or deleted without touching another group. The relay
keeps a directory:

```
<data>/groups/<uuid>/group.sqlite
<data>/groups/<uuid>/members.json
<data>/relay.sqlite          # group index, relay identity, schema version
```

`group.sqlite` holds `messages(seq INTEGER PRIMARY KEY, device, at, kind, body)`,
`seen(device, seq)`, and `invites(...)`. `members.json` is the roster and
revocation list, rewritten atomically. It is small and human-inspectable. The
relay's own `relay.sqlite` holds only the group id → directory map and the relay's
signing key fingerprint. A group directory can be restored without the index.

Retention is per group: `retain_days = 0` means forever, and compaction is a
deletion on `seq`, never a rewrite. A client that wants a different retention
policy is asking for a different group.

## 7. What server-trusted gives up

- The operator can read message bodies and presence. The UI must say so before a
  person joins a group. A group created on a relay someone else runs is not a
  private group.
- A relay compromise is a full-content compromise. TLS, disk encryption, and a
  minimal attack surface reduce the window, not the class.
- Search is server-side and therefore visible to the operator in principle. Query
  logging is off by default and the relay has no analytics endpoint.

E2EE is the upgrade path, not a promise in v1. The body encoding, the device-key
roster, and the immutable envelope are the three seams that make it additive.

## 8. What v1 builds, and what it does not

Build:

- group creation and signed invites.
- device-key membership, revocation, and a signed roster.
- append-only messages with per-group `seq`, history, and a resumable stream.
- ephemeral presence with the D2 rules.
- one relay per group, restore-based failover, per-group databases.

Do not build yet:

- E2EE, key rotation, or multi-device key agreement.
- cross-relay federation or gossip.
- server-side full-text search (a later index over the same rows).
- attachments (a blob store is its own trust and quota story).
- a hosted service. The relay is self-hosted. A hosted form needs an operator,
  a retention policy, and a statement of who can read the database before it is a
  product.

## 9. Open questions this design leaves

1. **Who may invite?** Any member is the simple answer. A group may want an admin
   role. The invite signature makes either enforceable. The policy is not decided.
2. **What does leaving do to a member's presence?** The roster revocation is
   immediate. Whether their past messages remain is per-group retention.
3. **Does a person id ever merge?** Two devices are two keys under one person. A
   person who loses every device needs a recovery signature. That is the same
   problem as account recovery in ADR 0013 and should use the same answer.
4. **Where does the relay's TLS identity come from?** Self-signed plus a pinned
   fingerprint in the invite is the air-gapped answer. A public CA is the hosted
   one. The invite carries the fingerprint either way.
