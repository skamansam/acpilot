# The v1 API, frozen

The design phase's task P0-6 wrote this down before the service phase (P3) built it, because the endpoint list is the part of
the design that is expensive to change later. It is the contract the CLI, the TUI,
the desktop app and the browser all speak — there is no privileged back channel,
so anything one of them needs has to be here.

The machine contract is the OpenAPI document (`GET /v1/openapi.json`, generated
from `acpilot-proto`), with a committed snapshot in
`crates/acpilot-proto/tests/snapshots/`. This file explains and fixes the list;
the snapshot is what a generator consumes. When the two disagree, the snapshot is
right and this file is a bug.

## Conventions

- **JSON only**, UTF-8, `Content-Type: application/json`. Timestamps are RFC 3339
  in UTC; ids are opaque strings and must never be parsed or constructed by a
  client.
- **`X-Acpilot-Profile` is required on every mutating request** (`POST`, `PUT`,
  `PATCH`, `DELETE`). Missing is `400 invalid_request`, not "use the active one":
  guessing an identity is how work gets sent with the wrong key. On reads it is
  optional and selects the profile explicitly; omitting it means the active
  profile.
- **SSE and WS connections bind their profile at connect.** Switching profile is
  a reconnect, never a frame on the existing stream.
- **Auth**: the default endpoint is the daemon's unix socket, and the filesystem
  is the access control — the runtime directory is `0700` and the socket `0600`, so
  another local user cannot open it at all, and a request over it needs no token. A
  TCP bind (`--addr`) inherits neither property, so it requires
  `Authorization: Bearer <token>` on every request, including streams. The token is
  a `0600` file in the runtime directory beside the socket (`.../acpilot/token`),
  generated on first use and reused afterwards; `acpilot tui --token` or
  `ACPILOT_TOKEN` names one instead. The one route that answers without it is the
  Swagger page at `/docs`, which is where a browser pastes one — it then sends it on
  the contract fetch and on every call the page makes. The document declares the
  scheme (`components.securitySchemes.bearer`) and requires it document-wide, so a
  client generated from it sends the header and Swagger's Authorize button exists;
  the socket exemption is in the scheme's description rather than in no scheme at
  all, because a document that says nothing produces a client that cannot work.`acpilot token rotate` rewrites the file; the listener
  reads it per request, so the new token applies on the next call and the old one
  is refused from then on, with nothing restarting. A missing or wrong token is
  `401 unauthorized`, in the usual envelope, and the scheme is case-insensitive
  while the token is not.
- **Errors are always the envelope below**, including on SSE handshakes and from
  the WS upgrade when the request itself is bad.

## Endpoints

Frozen for v1. `WS` routes are upgraded, not JSON responses.

```
GET    /v1/health
GET    /v1/openapi.json
GET    /app                              (HTML, no token: the web shell)
GET    /docs                             (HTML, no token: Swagger UI, --features docs)

GET    /v1/hosts                          POST   /v1/hosts
                                          POST   /v1/hosts/{id}/test

GET    /v1/workspaces                     POST   /v1/workspaces

GET    /v1/agents                         POST   /v1/agents/{id}/refresh
                                          POST   /v1/agents/{id}/trust

GET    /v1/sessions                       POST   /v1/sessions
GET    /v1/sessions/{id}                  DELETE /v1/sessions/{id}
POST   /v1/sessions/{id}/prompt           POST   /v1/sessions/{id}/cancel
POST   /v1/sessions/{id}/mode             GET    /v1/sessions/{id}/events  (SSE)
WS     /v1/sessions/{id}/stream
GET    /v1/events                                                     (SSE)

GET    /v1/permissions?status=pending     POST   /v1/permissions/{id}/resolve

GET    /v1/files                          PUT    /v1/files
GET    /v1/files/list

GET    /v1/terminals?session=             POST   /v1/terminals
WS     /v1/terminals/{id}/attach

GET    /v1/profiles                       POST   /v1/profiles
GET    /v1/profiles/{id}                  PATCH  /v1/profiles/{id}
DELETE /v1/profiles/{id}
POST   /v1/profiles/{id}/activate
GET    /v1/profiles/{id}/preferences      PATCH  /v1/profiles/{id}/preferences
POST   /v1/profiles/{id}/export

GET    /v1/lsp/servers                    POST   /v1/lsp/servers/{id}/restart
GET    /v1/lsp/servers/{id}/logs
GET    /v1/lsp/diagnostics                POST   /v1/lsp/request
```

`GET /v1/health` is the one route a client calls before it trusts anything else:
`{ok, service, version, api, workspace?}`. `workspace` is the id the daemon was
started with and is omitted when it has none; a client uses it to check that the
daemon already listening is serving the directory it was asked about, rather than
restarting a daemon that may be mid-turn (ADR 0004 §2).

**Deliberately not v1**: providers and models, the model lab and recipes, the ACP
bridge and remote drivers, extensions and stats. They follow the same conventions
when they land; none of them changes a route above.

Three names are frozen here because two documents disagreed, and the wire is
harder to rename than prose:

| Route | Why this spelling |
| --- | --- |
| `POST /v1/permissions/{id}/resolve` | "Resolve" is what the broker does; `decide` appeared on the site page only. |
| `GET /v1/lsp/servers` | Keeps `diagnostics` and `request` under one prefix, and reads better than `/v1/lsp`. |
| `GET /v1/terminals?session=` | Flat, filterable, and the same shape the TUI's session switcher needs; `/v1/sessions/{id}/terminals` is not a second route. |

`AgentStatus` has an `untrusted` state — "ran once, but the binary changed
since" — so `POST /v1/agents/{id}/trust` exists in v1 to leave it: it records the
hash the human approved and is the only way that state is cleared. Without the
route the state would be terminal, which is the kind of gap this review exists to
catch.

## Sessions and turns

`POST /v1/sessions` takes `CreateSessionRequest { workspace_id, agent_id, title? }`
and returns `SessionSummary`.

`POST /v1/sessions/{id}/prompt` takes `PromptRequest { text, idempotency_key? }`
and is **asynchronous**: it returns `202` with `PromptAccepted { session_id,
turn_id, first_seq }`. The turn itself streams over SSE; `200` here would mean
"the agent finished", which is what no client wants to wait for.

- `idempotency_key` is client-generated. The same key inside the same session
  returns the same `PromptAccepted`, not a second turn: a dropped connection must
  not cost an agent run.
- A second prompt while a turn is in flight is `409 conflict`, with the running
  `turn_id` in the message. Queuing turns would hide the state a UI has to show.
- `POST /v1/sessions/{id}/cancel` takes an optional `{ turn_id }` and returns
  `202` with `{ "turn_id": ..., "cancelled": true | false }`. Cancelling an idle
  session is `202` with `cancelled: false` rather than an error: the caller's
  intent ("stop") is already true.
- `POST /v1/sessions/{id}/mode` takes `{ "mode": string }`; unknown modes are
  `400 invalid_request` with the agent's advertised modes in `details`.
- `SessionStatus` is `idle`, `busy`, `awaiting_permission`, `closed`, `failed`.

## Event streams

Two streams, one shape.

`GET /v1/sessions/{id}/events` is the session's stream. `GET /v1/events` is the
profile-wide stream: `profile.*`, `daemon.*`, and `permission.requested` — the
last because the pending list it pairs with is profile-wide, so one connection
covers every session a UI is watching. It carries no `session` events: a browser
watching ten sessions would otherwise pay for all of them.

Every frame that carries data has an `id`, and the ids of a session stream are
the session's monotonic `seq` (starting at 1). The stream is the source of truth
for what an agent did, and the transcript behind it is what makes replay
possible.

```
id: 41
event: session
data: {"seq":41,"session_id":"s_01","kind":"message_chunk","at":"…","payload":{"role":"assistant","text":"Adding"}}

id: 42
event: permission
data: {"type":"requested","request":{"id":"p_07","session_id":"s_01","kind":"file_write","title":"Write src/http.rs","detail":"…","options":["allow_once","allow_always","deny"],"created_at":"…"}}

event: heartbeat
data: {"seq":42}
```

| `event` | `data` | Replayed? |
| --- | --- | --- |
| `session` | `SessionEvent` (`kind` is one of the eight below) | yes, from the session's ring, and on disk in its transcript |
| `permission` | `{"type":"requested","request":PermissionRequest}` or `{"type":"resolved","id":…,"decision":PermissionDecision}` | `resolved` yes; `requested` live-only, on both streams |
| `heartbeat` | `{"seq": n}` every 15 s | no |
| `resync` | `{"latest_seq": n}` | no |

`SessionEventKind` is closed: `message_chunk`, `thought`, `tool_call`, `plan`,
`mode_changed`, `notice`, `compaction`, `commands_changed`. Adapter detail lives
in `payload`; a new kind is a wire change and shows up in the OpenAPI snapshot.

**Reconnect.** The client sends `Last-Event-ID: <seq>` and receives everything
after it, in order, with no gaps. Replay is served from the live session's ring,
and every one of those events is appended to the session's transcript as it is
recorded, so the file outlives the process even though no route reads it back yet:
enumerating stored sessions is P3-7's, with the SQLite table. If the id is unknown
— older than the ring, or a session this process did not start — the server answers
`event: resync` with the latest sequence and the client refetches session state
(`GET /v1/sessions/{id}`) instead of pretending it can catch up. The first frame is
always `retry: 2000`.

**Heartbeat** exists so a proxy cannot silently hold a dead stream open; a client
that has seen no frame for 45 s (three heartbeats) reconnects.

**Pending permissions are state, not history.** On connect, a client fetches
`GET /v1/permissions?status=pending`; `GET /v1/events` tells it when a *new*
request appears while connected, so a client never has to poll. A
`permission.requested` frame is therefore not replayed — a request that arrived
while the client was away is recovered by the state fetch, and replaying old
prompts would ask the user to answer something already resolved.
`permission.resolved` is an event because it must be applied in order.

`/v1/events` has no `Last-Event-ID` replay for the same reason. Its frames are
numbered with an `id` that counts what that connection has been told, and a
client that falls far enough behind to lose frames receives `event: resync` with
the latest number: the recovery is to refetch the pending list, exactly as a
reconnecting client does, rather than to expect the stream to reconstruct what it
missed. Heartbeats run on the same 15 s clock as the session stream.

`WS /v1/sessions/{id}/stream` is the same session stream for a client that needs
to send while receiving. It carries the same event names and payloads, no
`Last-Event-ID` (the client sends `{"since": seq}` instead), and exists for
interactive terminals; nothing in v1 requires it to be used for agent events.

## Permissions

`GET /v1/permissions?status=pending[&session=]` returns `PermissionRequest[]`:
`{ id, session_id, kind, title, detail, options, created_at }`, where `kind` is
`file_write`, `file_read`, `terminal`, `tool_call` or `other`, and `options` is the
subset of `allow_once`, `allow_always`, `deny` the broker will accept.

`POST /v1/permissions/{id}/resolve` takes `{ "option": "allow_once" }` and returns
`PermissionDecision { option, decided_by, at }`.

- An option that is not in `options` is `400 invalid_request` with the allowed set
  in `details`.
- Repeating the same decision is idempotent (`200`, same body) so a retried
  request is safe. A *different* option after a decision is `409 conflict`: the
  agent has already acted on the first answer.
- A request whose session has closed is `409 conflict`; an unknown id is `404`.
- v1 has no timeout. A request stays pending until resolved or the session closes,
  and the broker denies on close. `PermissionDecision.decided_by` records `user`,
  `policy:<rule-id>` or `close`.

This is the scripted path: list pending, pick an option from the same object, post
it, switch on `code` on failure. No polling loop and no interactive prompt are
required.

## Files

`GET /v1/files?workspace=&path=` returns `FileContent { path, text, etag }` for a
file; a directory is `409 conflict` with the message pointing at `/v1/files/list`.

`GET /v1/files/list?workspace=&path=` returns `FileEntry[]`
(`{ path, is_dir, size, modified? }`), paths relative to the workspace root with
`/` as the separator on every host.

`PUT /v1/files` takes `FileWriteRequest { path, text, if_match? }`. `if_match` is
the `etag` last read; a mismatch is `409 conflict`, never a silent overwrite.
Omitting it creates a file and is `409` if the file already exists. Binary content
is refused (`invalid_request`), not mangled.

The `etag` is the modification time, the size and a hash of the bytes. The first
two make a change visible without reading the file; the hash catches an edit that
preserved both, which is what a fast tool does.

A path is relative to the workspace root and `..` is refused rather than
resolved, so the answer does not depend on whether the directory it climbs out of
happens to exist. The resolved path is then checked against the resolved root,
because a symlink inside the workspace can point outside it — that is `403`, and
a string prefix check would have called it contained. A parent directory that is
not there is `404`: this route saves files, it does not build trees.

None of this is on the keystroke path: the editor owns its buffer and saves
through this route.

## Terminals

`POST /v1/terminals` takes `CreateTerminalRequest { session_id, command?, args?,
cwd?, size }` and returns a `TerminalSummary` with the process already running.
A terminal belongs to a session because that is what owns the work it does; a
shell with no session would be a second, sessionless way to run commands on the
daemon's host. `command` defaults to the host's own shell, and `cwd` is relative
to the workspace root (an absolute path is accepted and checked) — a directory
outside it is `403`, not a silent start somewhere else.

`GET /v1/terminals?session=` lists them, same shape, filterable.

`WS /v1/terminals/{id}/attach` is the pane. The stream is deliberately split by
frame type, because terminal output can contain anything a shell can print:

| Direction | Frame | Meaning |
| --- | --- | --- |
| client → daemon | binary | Keystrokes, byte for byte. No newline is added. |
| client → daemon | text | `{"type":"resize","cols":100,"rows":30}` or `{"type":"detach"}` |
| daemon → client | text | `{"type":"attached","terminal":{…},"replay_bytes":n}` first |
| daemon → client | binary | Exactly `replay_bytes` of screen, then live output |
| daemon → client | text | `{"type":"exit","code":0}` last, then the socket closes |

`attached` is always the first frame, and the binary frame after it is exactly
`replay_bytes` long: the terminal's current screen drawn with the escape
sequences that reproduce it. That is why attaching late is the normal case rather
than a race — a pane that opens on a command that finished two minutes ago shows
what it printed. Live output follows as binary frames, and anything the program
writes after the attach arrives once, in order.

Closing the socket — or sending `detach` — leaves the process running. A pane is
a view of a terminal, not its owner; the process ends when it ends. A pane that
stops reading is closed with an error code rather than queued forever, and
reattaching costs it one screen.

A terminal an agent asked its client for (`terminal/create` in ACP) is listed and
attached the same way, under an `agent-` prefixed id. It is the same PTY, not a
copy: the daemon draws a screen from the bytes the agent's reader already got and
writes keystrokes to the same descriptor, so a shell an agent started can be
watched and driven without a second process printing to nobody. Such a pane ends
when the agent's program ends — a pane opened after the fact still replays what
the command printed — and killing the pane is not offered, because the process
belongs to the session that started it.

## Errors

Every failure is this shape, whether it came from a route, an agent, or a host
that stopped answering:

```json
{
  "code": "permission_denied",
  "message": "write outside workspace: ../secrets.env",
  "details": [{ "field": "path", "message": "escapes /home/me/app" }]
}
```

| `code` | HTTP | Meaning |
| --- | --- | --- |
| `invalid_request` | 400 | Malformed body, bad enum value, missing profile header |
| `unauthorized` | 401 | A TCP listener without `Authorization: Bearer <token>` |
| `not_found` | 404 | The thing is not there |
| `conflict` | 409 | Stale ETag, duplicate turn, closed session, already answered |
| `permission_denied` | 403 | Policy or a human said no |
| `timeout` | 504 | An agent, LSP server or host did not answer in time |
| `host_unavailable` | 502 | SSH/WSL host did not answer |
| `model_unavailable` | 502 | The inference server is down, refused, or answered nonsense (P4.5) |
| `agent_failed` | 502 | The agent process died or broke protocol |
| `protocol` | 500 | Malformed frame, bad version |
| `io` | 500 | Filesystem or process I/O |

`code` is stable and clients switch on it; `message` is for humans and may be
reworded. `details` is present only when the failure belongs to specific fields,
and `field` is a dotted path (`preferences.wrap`).

502 versus 500 tells a client whether to offer "retry" or "file a bug". A host
that is asleep is not a bug in the daemon.

## What the review changed

The design phase's task P0-6 read the list against four clients and fixed the gaps rather than noting
them:

| Client need | Served by | Was it in README? |
| --- | --- | --- |
| CLI: list/show sessions, one-shot prompt, follow, cancel | sessions routes, `202` + `turn_id`, SSE | yes, except the `202`/turn contract — added |
| CLI: `acpilot host add wsl`, connection test | `POST /v1/hosts`, `POST /v1/hosts/{id}/test` | the test route was missing — added |
| TUI: permission prompt with no poll latency | session SSE `permission` event + pending fetch on connect | the taxonomy was scattered — fixed here |
| TUI: mode switch, terminal panes, diagnostics | `/mode`, `/v1/terminals*`, `/v1/lsp/*` | yes |
| Script: answer a permission non-interactively | deterministic `resolve` semantics: retry-safe, `409` only for a different answer | the route existed, the semantics did not — added |
| Web: reconnect without losing events | `Last-Event-ID`, heartbeat, `resync` | the mechanism was in the service phase's task P3-3, not in the README — added |
| Any client: switch profile without restart | `X-Acpilot-Profile`, `POST /v1/profiles/{id}/activate` | the activate route was missing — added |
| TUI/CLI: clear an agent whose binary changed | `POST /v1/agents/{id}/trust` | missing, and the proto already had `status: untrusted` with no way out — added |
| Any client: know whether to retry | error envelope with HTTP mapping | missing from README — added |

Nothing else was missing for v1. Providers, models, recipes and the ACP bridge
stay out on purpose: they are the models phase (P4.5)/Language intelligence phase (P7)/Remote hosts phase (P8), and adding them now would freeze
transports that have not been measured.
