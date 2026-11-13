# The v1 API, frozen

The design phase's task P0-6 wrote this document before the service phase (P3)
built the service. The endpoint list is the part of the design that is expensive
to change later. It is the contract the CLI, the TUI, the desktop app and the
browser all speak. There is no privileged back channel, so anything one of them
needs must be here.

The OpenAPI document is the machine contract: `GET /v1/openapi.json`, generated
from `acpilot-proto`. Its committed snapshot is in
`crates/acpilot-proto/tests/snapshots/`. This file explains and fixes the list.
The snapshot is what a generator consumes. When the two disagree, the snapshot is
right and this file is a bug.

## Conventions

- **JSON only**, UTF-8, `Content-Type: application/json`. Timestamps are RFC 3339
  in UTC. Ids are opaque strings. A client must never parse or construct an id.
- **`X-Acpilot-Profile` is required on a mutating request when the daemon serves
  more than one profile** (`POST`, `PUT`, `PATCH`, `DELETE`). A missing header is
  `400 invalid_request`, not "use the active one". To guess an identity is how
  work goes out with the wrong key. A daemon with one profile still answers
  clients that predate the header, because it has no second identity to confuse.
  On reads and streams the header is optional and selects the profile explicitly.
  Without it, the request is about the active profile.
- **SSE and WS connections bind their profile at connect.** Switching profile is
  a reconnect, never a frame on the existing stream.
- **Auth**: the default endpoint is the daemon's unix socket. The filesystem is
  the access control: the runtime directory is `0700` and the socket is `0600`.
  Another local user cannot open it. A request over the socket needs no token. A
  TCP bind (`--addr`) inherits neither property, so it requires
  `Authorization: Bearer <token>` on every request, streams included. The token is
  a `0600` file in the runtime directory beside the socket (`.../acpilot/token`).
  The daemon generates it on first use and reuses it after that. `acpilot tui
  --token` or `ACPILOT_TOKEN` names one instead. The one route that answers
  without a token is the Swagger page at `/docs`. A browser pastes a token there.
  The page then sends it on the contract fetch and on every call it makes. The
  document declares the scheme (`components.securitySchemes.bearer`) and requires
  it document-wide. A generated client therefore sends the header, and Swagger's
  Authorize button exists. The socket exemption is in the scheme's description
  rather than in no scheme at all. A document that says nothing produces a client
  that cannot work. `acpilot token rotate` rewrites the file. The
  listener reads it per request, so the new token applies on the next call. The
  old token is refused from then on, with nothing restarted. A missing or wrong
  token is `401 unauthorized`, in the usual envelope. The scheme name is
  case-insensitive and the token is not.

  A **WebSocket a page opens** is the one request a browser cannot give that
  header to: `new WebSocket(url, protocols)` sets no headers at all. Attach
  routes therefore also accept the token as a `Sec-WebSocket-Protocol` name. The
  call is `new WebSocket(url, ["acpilot.v1", token])`, and the daemon answers
  with `acpilot.v1`, never with the token echoed back. That is still a header, not
  a query string. The token stays out of log lines, browser history and any
  `Referer`, which is where the URL alternative would put it.
- **Errors are always the envelope below**, including on SSE handshakes and from
  the WS upgrade when the request itself is bad.

## The document is also a build input

`web/src/api/openapi.json` is the committed document the daemon serves.
`web/src/api/schema.d.ts` is the TypeScript generated from it. `cargo xtask
web-schema` writes the first. `npm run schema` writes the second. The web half of
the gate regenerates both and fails when the committed copies are behind. A wire
change therefore cannot land without the page's types moving with it.

## Endpoints

Frozen for v1. `WS` routes are upgraded, not JSON responses.

```
GET    /v1/health
GET    /v1/openapi.json
GET    /                                 (HTML, no token: the web app)
GET    /app                              (HTML, no token: the same page, the old spelling)
GET    /app/stats                        (JSON: the page's measured bar, token required)
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
DELETE /v1/files                          POST   /v1/files/directory
POST   /v1/files/rename
GET    /v1/files/list
GET    /v1/git/status?workspace=          GET    /v1/git/diff?workspace=&scope=
GET    /v1/git/tree?workspace=            GET    /v1/git/log?workspace=&limit=
GET    /v1/git/commit/prompt              POST   /v1/git/commit-message
POST   /v1/git/stage                     POST   /v1/git/stage-hunk
POST   /v1/git/unstage                   POST   /v1/git/discard
POST   /v1/git/checkout

GET    /v1/terminals?session=             POST   /v1/terminals
DELETE /v1/terminals/{id}                 POST   /v1/terminals/{id}/resize
WS     /v1/terminals/{id}/attach

GET    /v1/profiles                       POST   /v1/profiles
GET    /v1/profiles/{id}                  PATCH  /v1/profiles/{id}
DELETE /v1/profiles/{id}
POST   /v1/profiles/{id}/activate
GET    /v1/profiles/{id}/preferences      PATCH  /v1/profiles/{id}/preferences
POST   /v1/profiles/{id}/export

GET    /v1/providers                      POST   /v1/providers

GET    /v1/lsp/servers                    POST   /v1/lsp/servers/{id}/restart
GET    /v1/lsp/servers/{id}/logs
GET    /v1/lsp/diagnostics                POST   /v1/lsp/request

GET    /v1/stats
```

`GET /v1/health` is the one route a client calls before it trusts anything else.
It answers `{ok, service, version, api, workspace?, profile?, install?}`.
`profile` is the name of the profile directory the daemon was started with, and is
omitted when it has none. The web page writes it into `/p/:profile/...` rather
than inventing a name for a daemon that cannot see its own flags. `install` is how
the running binary was installed, when that installation left a marker (ADR 0010).
It is `{method, version?}`. `method` decides who may reconcile a version
difference: a package manager that owns the files, or the program itself for a
tarball or a `cargo install`. `install` is absent when no marker was found, which
is treated as nobody's to replace. `workspace` is the id the daemon was started
with, and is omitted when it has none. A client uses it to check that the daemon
already listening serves the directory it was asked about. That check avoids
restarting a daemon that may be mid-turn (ADR 0004 §2).

## Profiles

A daemon serves every profile directory under `<config>/profiles` plus the one
`--profile` named. Each is opened for its own `agents.toml`, policy, sessions and
language servers. Its `profile.sqlite` is opened on the first request that needs
history, through a shared cache of at most four databases (least recently used
evicted). A directory without a `config.toml` is not a profile. One that fails to
load costs its own row rather than the daemon. `GET /v1/profiles` lists them as
`ProfileSummary`. `GET /v1/profiles/{id}` answers one. `POST
/v1/profiles/{id}/activate` makes a profile the one a request without
`X-Acpilot-Profile` is about, for the life of the process. Persisting that choice
is the settings cascade's `active_profile`, not a runtime request's.

`POST /v1/profiles` creates the five roots, `config.toml` and the registry row.
It opens the profile's state without a restart, and becomes active when the daemon
had no identity. A reserved or malformed name is `400`. An existing directory is
`409`. `PATCH` changes only `label` and `extends`. `clear_extends: true` detaches
the profile. Setting and clearing in one request is `400`. `DELETE` stops the
profile's language servers, removes it from the registry and the daemon, and moves
its directories to the OS trash. `?purge=true` removes them permanently. The last
profile is `409`, because a daemon with no identity could not be fixed through its
own API.

`POST /v1/profiles/{id}/export` answers the allow-listed configuration files as
one `ProfileExport` document. Keyring references are exported exactly as they were
written. Credentials are never resolved into an HTTP answer. The CLI's
`--with-secrets` path is the only place a secret is read.

A request is dispatched to the profile its header names. An unknown name is
`404`, and the message lists the profiles this daemon serves. A mutating request
without the header is `400` when there is a choice: a daemon with two or more
profiles refuses to guess. Reads and streams without a header use the active
profile. A stream binds at connect, so a switch is a reconnect rather than a frame
on an open stream.

`GET /v1/profiles/{id}/preferences` answers the effective `Preferences`: the same
merged object the `prefs` frame carries, with every layer applied (defaults, user
file, profile files, project). `PATCH` takes a `PreferencesPatch` with any subset
of the editable keys, and writes only those into the profile's `profile.toml`.
Every other key and table is left alone, because a preference change is not a
rewrite of a profile's policy. The merged preferences are read back after the
write. A patch that would make them unreadable is rolled back and answered `400`.
The successful answer is the new effective object. It is also announced as a
`prefs` frame, so an open client applies it without waiting for the file watcher.

**Added after v1 was frozen**: the provider, model, recipe and run routes below. They
follow the same conventions and change no route above. The ACP bridge and remote drivers
(P4.5-10), promotion (P4.5-9) and extensions are still deliberately later.

## Providers

A provider is a model server a profile declares: a URL and a dialect. The
declaration lives in the profile's `providers.toml` as a `[[providers]]` table.
Nothing scans the local network and nothing probes at startup. A probe is the one
thing that contacts a server, and it happens because somebody asked.

`GET /v1/providers` lists the declarations in file order, each with the last answer
this profile remembers. The answer is kept in `providers.state.toml`, beside the
declaration and separate from it, so a restart says `unreachable at 12:04` rather
than forgetting the one fact somebody needed. A declaration whose URL, dialect or
credential reference changed is `not_probed` again: the old server's answer is not
the new server's. `unreachable` means a probe ran and failed, so that state and
`not_probed` cannot be confused. A disabled provider reads `disabled` and is never
contacted.

`POST /v1/providers` writes the declaration to that file and probes it once. A
failed probe still answers `201`: the row carries the reason in `detail`, and a
`502` would leave nobody with a row to correct. A refused credential reads
`unauthorized`, because the fix differs from an unreachable host. One id is one
provider, and a second declaration of that id is `409`. Replacing a row would move
every recipe that names it to another server without saying so.

`credential_ref` names a keyring entry in the profile's namespace. The daemon reads
it at probe time and sends it as a bearer token. A reference that cannot be read
answers `unauthorized` with the account to add, and nothing is sent, because an
anonymous request would let the server take the blame. The reference is returned as
written and the token is never part of any answer. `acpilot models probe [id]` asks
the same question from a terminal, with no daemon, using the same keyring and the
same memory file, and exits `1` when a provider answers with a failure.

## Stats

`GET /v1/stats` returns `IntegrationStats[]`: one row per integration, including
the quiet ones. A bar that reflowed when a provider stopped answering would be a
bar nobody could watch. A row is `{ id, kind, label, context?, credits?,
updated_at }`. `kind` is `provider` or `agent`. `label` is the name a bar draws, so
it needs no second lookup.

`context` is `{ used_tokens, window_tokens?, source }`. `credits` is `{ used?,
remaining?, limit?, currency, source }`. `source` is `server`, `configured`,
`estimated` or `unknown`, recorded **per figure**. A server may report a window and
no balance. A plan may name a ceiling the server never mentions.

The rules a client is held to:

- **No default window.** An absent `window_tokens` renders as an indeterminate
  track, never a full or an empty one. To substitute 32k is how a prompt is
  truncated without anyone noticing.
- **A missing figure is absent, not zero.** A local server has no credit bar. A
  provider with no balance endpoint draws no credits rather than `0.00`.
- **An estimate is labelled.** `estimated` is a figure this daemon counted. A UI
  that styles it as a measurement claims something nobody reported.
- **The age is on the wire.** `updated_at` is the last observation, so a frozen
  bar is visible as frozen.

The roster fills it today. For an agent with a conversation behind it, it also
holds an estimated context reading. There is one `agent` row per entry in the
profile's `agents.toml`. When a session with that agent has said anything, the
sessions engine has counted the characters it carries, four to a token. The row is
then `source: estimated` with no `window_tokens`. A UI draws `18.4k / ?` against an
indeterminate track rather than a fraction of a window nobody stated. An agent's
own usage update replaces the count when it sends one (`source: server`, and the
window it names). A session that has done nothing yet reports no `context` at all.
A row with a reading carries that session's last event as its `updated_at`, so a
conversation that stopped goes old on the bar. Without that, it would look fresh
for as long as the roster can be read. Provider rows, configured windows and the
balance reads are the models phase's task (P4.5-12).

`GET /app/stats` is the page's copy: the same rows with `fill`, `text` and
`source` worked out. A browser cannot link `acpilot-stats`, and a second
implementation of the arithmetic in JavaScript is one that drifts. Same rows, same
order, measured by `acpilot-stats`, the crate the TUI and the desktop app also
draw. It is under `/app` because it is a rendering of the contract rather than
part of it. It needs the token: an agent roster is not the page's own business.

The page itself is served at `/` and `/app` with
`Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline';
style-src 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src
'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`, plus
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` and
`X-Frame-Options: DENY`. The bundle is one inline file, so its script and style
are the only inline exceptions. Everything it may reach is its own origin. The
page is unauthenticated because a browser cannot attach a header to a typed
address. That is exactly why it may not be framed: a framed page is where a
clickjacked token field comes from.

Three names are frozen here because two documents disagreed. The wire is harder
to rename than prose:

| Route | Why this spelling |
| --- | --- |
| `POST /v1/permissions/{id}/resolve` | "Resolve" is what the broker does. `decide` appeared on the site page only. |
| `GET /v1/lsp/servers` | Keeps `diagnostics` and `request` under one prefix, and reads better than `/v1/lsp`. |
| `GET /v1/terminals?session=` | Flat, filterable, and the same shape the TUI's session switcher needs. `/v1/sessions/{id}/terminals` is not a second route. |

`AgentStatus` has an `untrusted` state: it ran once, but the binary changed
since. `POST /v1/agents/{id}/trust` exists in v1 so a client can leave that state.
It records the SHA-256 of the program the agent's command runs, and it is the only
way that state is cleared. `GET /v1/agents` compares that pin with the file on
disk, so a replaced binary reads as `untrusted` until somebody says the new one is
the one they meant. `missing` wins over `untrusted`, because there is nothing to
approve. The pin is the stored record (`agent_pins`), so a restart does not reopen
the dialog. A daemon with no store answers `409` rather than `200` for a promise it
cannot keep. An agent whose program this host cannot resolve is `400`. An id the
profile does not declare is `404`. Without the route the state would be terminal,
which is the kind of gap this review exists to catch.

## Hosts

`GET /v1/hosts` returns `HostSummary[]`, the daemon's own host first:
`{ id, label, kind, connected, capabilities? }`. `kind` is `local`, `ssh` or `wsl`.
`capabilities` is `{ filesystem, pty, spawn, lsp }` and is absent until the host
has answered once. The local host is always `connected` with every capability
true. The daemon runs on it, so a false field would mean a platform this build
cannot run on at all. A false field is how a client learns not to offer an agent
on a host that can edit files but cannot start a process.

`POST /v1/hosts` takes `CreateHostRequest { kind, address? }` and answers `201`
with the summary, `connected: false` and no capabilities. `address` is
`user@machine` for `ssh` and a distribution name for `wsl`. Its absence is `400`.
`local` is `409`, because the daemon's own host is already listed. The id joins the
transport and the address with a colon (`ssh:me@box`). Adding the same host twice
is then `409` rather than two rows a client cannot tell apart. It is not
`ssh://me@box`: the id is a path segment on `/v1/hosts/{id}/test`, and `//` would
split it into two. An added host is recorded, so the next daemon lists it again.

Hosts are global: adding `ssh:box` once makes it available to every profile. A
profile may override the identity it uses for that host, never the hostname, with
a table in its `config.toml`:

```toml
[hosts."ssh:box"]
user = "deploy"
identity_file = "/home/me/.ssh/work"
proxy_jump = "bastion"
```

The daemon applies those fields when it builds the transport (`ssh -l`, `-i`,
`-J`). The host list and the machine `ssh:box` names are unchanged. A bare
address (`[hosts.box]`) is accepted as the key too. WSL has no SSH identity, so
the fields are ignored for a WSL host.

`POST /v1/hosts/{id}/test` asks the host whether it answers. The local host
answers with this build's capabilities. A remote host is reached by putting
`acpilotd-helper` on it and starting it over that host's transport. The daemon
then asks the helper to shake hands. A build that carries the helper writes it to
`~/.local/bin/acpilotd-helper` first. That is one `sh -s` with the static binary
base64-encoded on stdin, so a host that has never seen acpilot needs neither `scp`
nor a package manager. The copy that answers is the version this daemon speaks
rather than whatever was installed months ago. `ACPILOT_HELPER_DIR` moves the
directory, which is what a test uses instead of a real home. A host that refuses
the write, because its home is read-only or it has no `sh`, still has a helper
that is already on its `PATH`. A failure reports both attempts. The handshake's
answer names protocol 1 and the methods the helper serves. The capabilities
reported are those methods: `fs.*` is files, `exec` is programs, and `pty.*` or
`lsp.*` once a helper has them. A host is offered a pane only when the helper on it
says it can open one. A helper old enough to answer without a list is protocol 1's
files and programs. One that lists nothing is reported as a host that can do
nothing, because that is what it said. A host that cannot be reached is `502
host_unavailable` with `transport` in `details`, and the failure is reported rather
than stored. `connected` only becomes true when a helper answered in this process.
An unknown id is `404`. `ACPILOT_SSH` and `ACPILOT_WSL` move the transport program
for a host where it is not on `PATH`.

## Sessions and turns

`POST /v1/sessions` takes `CreateSessionRequest { workspace_id, agent_id, title? }`
and returns `SessionSummary`. `workspace_id` may be empty, which means this
daemon's own workspace. An id it does not serve is `404`, checked before the policy
so that answer does not depend on whether one loaded.

`SessionSummary` and `WorkspaceSummary` both carry `profile`, the identity that
owns the row. `POST /v1/workspaces` records the first profile to open a host and
root pair in the global index. A second profile asking for the same pair is `409
profile_mismatch` with the owner in `details` (`field: "owner"`), rather than a
second workspace that happens to share a checkout. `steal: true` transfers the
index row to the requesting profile and logs the old and new owner. It is the API
behind `acpilot workspace open --profile <name> --steal <path>`. A workspace id in
a profile daemon is namespaced (`work:local:/src`), so carrying one profile's id
into another's request is visible rather than accidental.

`POST /v1/sessions/{id}/prompt` takes `PromptRequest { text, idempotency_key? }`
and is **asynchronous**: it returns `202` with `PromptAccepted { session_id,
turn_id, first_seq }`. The turn itself streams over SSE. A `200` here would mean
"the agent finished", which is what no client wants to wait for.

- `idempotency_key` is client-generated. The same key inside the same session
  returns the same `PromptAccepted`, not a second turn: a dropped connection must
  not cost an agent run. It is answered before the busy check, because the request
  that started the running turn is exactly the one a retry repeats. The retry is
  the one case where "a turn is in flight" and "this turn is mine" are the same
  thing. A *different* key while a turn runs is still `409`.
- A second prompt while a turn is in flight is `409 conflict`, with the running
  `turn_id` in the message. Queuing turns would hide the state a UI has to draw.
- `POST /v1/sessions/{id}/cancel` takes an optional `{ turn_id }` and returns
  `202` with `{ "turn_id": ..., "cancelled": true | false }`. Cancelling an idle
  session is `202` with `cancelled: false` rather than an error: the caller's
  intent ("stop") is already true.
- `POST /v1/sessions/{id}/mode` takes `SetModeRequest { mode }` and answers
  `ModeState { mode, available[] }`, where each mode is `{ id, name, description? }`.
  An id the agent did not advertise is `400 invalid_request` with the alternatives
  in `details`, so a picker can be fixed without a second request. An agent that
  offers no modes, a session that has ended, and a turn in flight are all `409`.
  There is either nothing to switch or nowhere to switch it yet, and a `200` for a
  mode nobody entered would be worse than saying so. The agent's `mode_changed`
  event on the session stream reports the switch. That is why this is a request
  rather than a notification: ACP makes the agent the authority on its modes.
- `SessionStatus` is `idle`, `busy`, `awaiting_permission`, `closed`, `failed`.

`WS /v1/sessions/{id}/stream?after=N` is the same feed over a socket. It sends one
text frame per `SessionEvent`, and no `id` alongside because the event carries its
`seq`. A stored conversation is replayed and the socket closed, like its SSE stream
ending. An unknown id is `404` with the usual envelope *before* the upgrade, so a
client is not left reading a close code. A dropped frame sequence is announced as
`{"type":"resync","latest_seq":N}` rather than silently continued. The client's
answer to that is to reconnect with `after=N`. The connection is kept alive with
WebSocket pings, which a browser answers without the page seeing anything.

`GET /v1/sessions` lists every conversation this profile has, not only the ones
this process started. A stored session is reported with `status: "closed"`, because
nothing is running it. A prompt to it is `409`, because resuming a stopped
conversation is not in v1. The rows live in one SQLite database per profile:
`<data>/profiles/<id>/profile.sqlite`. That is what makes yesterday's conversation
reachable today. Events older than 30 days are compacted out of it. Permission
decisions are kept, because they are the audit rather than the transcript.
Transcripts are per profile too, under
`<data>/profiles/<id>/sessions/<session>/session.jsonl`. `<data>/index.sqlite`
only lists profiles and their workspaces, and can be rebuilt.

## Event streams

Two streams, one shape.

`GET /v1/sessions/{id}/events` is the session's stream. `GET /v1/events` is the
profile-wide stream: `profile.*`, `daemon.*`, `prefs`, `prefs_error`,
`permission.requested`, and `diagnostics`. The rest are there because what they
pair with is profile-wide. That is the pending list, the settings every surface
draws with, and the problems of a file any of them may have open. One connection
therefore covers every session a UI is watching. It also covers the theme it is
drawing with and a gutter that a server filled in on its own. It carries no `session` events: a
browser watching ten sessions would otherwise pay for all of them.

Every frame that carries data has an `id`. The ids of a session stream are the
session's monotonic `seq`, starting at 1. The stream is the source of truth for
what an agent did. The transcript behind it is what makes replay possible.

Both streams need `Authorization`, so a browser page cannot take them with
`EventSource`, which sends no headers. The web shell reads the same frames from a
`fetch` body and reconnects with `?after=<last sequence>`. The format does not
change, and the token stays a header rather than a query string.

```
id: 41
event: session
data: {"seq":41,"session_id":"s_01","kind":"message_chunk","at":"…","payload":{"role":"assistant","text":"Adding"}}

id: 42
event: permission
data: {"type":"requested","request":{"id":"p_07","session_id":"s_01","kind":"file_write","title":"Write src/http.rs","detail":"…","options":["allow_once","allow_always","deny"],"created_at":"…"}}

id: 43
event: prefs
data: {"preferences":{"schema_version":1,"theme":"acpilot.night-flight","theme_web":"acpilot.paper",…}}

event: heartbeat
data: {"seq":43}
```

| `event` | `data` | Replayed? |
| --- | --- | --- |
| `session` | `SessionEvent` (`kind` is one of the eight below) | yes, from the session's ring, and on disk in its transcript |
| `permission` | `{"type":"requested","request":PermissionRequest}` or `{"type":"resolved","id":…,"decision":PermissionDecision}` | `resolved` yes. `requested` is live-only, on both streams |
| `prefs` | `{"preferences": Preferences}` | the current ones, as the first frame of a connection. Changes are live-only |
| `prefs_error` | `{"message": "…"}` | no. A save that did not parse, with the last good preferences still in force |
| `heartbeat` | `{"seq": n}` every 15 s | no |
| `resync` | `{"latest_seq": n}` | no |

`SessionEventKind` is closed: `message_chunk`, `thought`, `tool_call`, `plan`,
`mode_changed`, `notice`, `compaction`, `commands_changed`. Adapter detail lives
in `payload`. A new kind is a wire change and appears in the OpenAPI snapshot.

**Reconnect.** The client sends `Last-Event-ID: <seq>` and receives everything
after it, in order, with no gaps. Replay is served from the live session's ring.
Every one of those events is appended to the session's transcript as it is
recorded. The conversation therefore outlives the process. A session this daemon did
not start is answered from that database. `GET /v1/sessions` lists it,
`GET /v1/sessions/{id}` describes it with status `closed`, and its stream replays
from the sequence asked for and then ends. There is no live feed behind it to wait
on. The id may be unknown, because it is older than the ring or belongs to a
session this process did not start. The server then answers `event: resync` with
the latest sequence. The client then refetches session state with
`GET /v1/sessions/{id}` instead of pretending it can catch up. The first frame is
always `retry: 2000`.

**Heartbeat** exists so a proxy cannot silently hold a dead stream open. A client
that has seen no frame for 45 s (three heartbeats) reconnects.

**Pending permissions are state, not history.** On connect, a client fetches
`GET /v1/permissions?status=pending`. `GET /v1/events` tells it when a *new*
request appears while connected, so a client never has to poll. A
`permission.requested` frame is therefore not replayed. A request that arrived
while the client was away is recovered by the state fetch. To replay old prompts
would ask the user to answer something already resolved.
`permission.resolved` is an event because it must be applied in order. It is the
same `event: permission` name with `{"type":"resolved","request_id":…,"option":…}`,
and it is what closes a dialog another client opened. The broker announces it after
the answer reaches the agent's wait, so a client never stops drawing a question
that is still pending.

**Preferences are state too, and they are the stream's first frame.** A client
that connects is told what the daemon would apply instead of learning it only when
somebody edits a file. A window or a browser that cannot see this machine's
`config.toml` still opens with the right theme. A save arrives as another `prefs`
frame, which is the same whole object, so applying one is replacing what you had
rather than patching it. The daemon watches the user's and the project's settings,
debounced. A file that does not parse is announced as a `prefs_error` frame
carrying the parser's sentence. The last good preferences stay in force, and the
next save is read normally. `theme_tui` is in the frame as well, because the frame
describes the cascade and not one surface's slice of it. A client picks the theme
for the surface it is.

**Diagnostics are events, not state.** A `diagnostics` frame carries
`DiagnosticsNotice { path, version?, problems }`, the same body
`POST /v1/files/diagnostics` answers with, minus the request. It says that a
language server published problems about a file the client did not ask about. That
happens on the daemon's own clock: a server with an index publishes again when a
background check finishes. A pane that only heard about the problems it asked for
would keep a stale gutter until the next save. `version` is the document version
the server named, when it named one, so a client can tell a late publish from a
current one. Most servers leave it out. The daemon also keeps the newest problems
per file, written by a publish and by an answer to `POST /v1/files/diagnostics`
alike. It sends them as unnumbered `diagnostics` frames when a client connects, so
a reconnect starts from what is already known. Individual frames are not replayed.
The snapshot is the state, and a file it does not mention is one nobody has asked
about since the daemon started.

**Cross-origin.** The daemon answers CORS only when `[daemon] cors = true` in a
settings file. Off is the default and the normal case: the SPA is served by this
daemon, so its requests are same origin. The Vite dev server proxies `/v1` rather
than calling it cross-origin. A permissive layer on a token-guarded TCP listener
would let any page that learned the token read every stream. That is a decision a
file has to make deliberately.

`/v1/events` has no `Last-Event-ID` replay for the same reason. Its frames are
numbered with an `id` that counts what that connection has been told. A client that
falls far enough behind to lose frames receives `event: resync` with the latest
number. The recovery is to refetch the pending list, exactly as a reconnecting
client does, rather than to expect the stream to reconstruct what it missed.
Preferences are the one exception to "no replay". They are the current answer, so
a client that falls behind on them is sent the current preferences rather than a
`resync`. Heartbeats run on the same 15 s clock as the session stream.

`WS /v1/sessions/{id}/stream` is the same session stream for a client that needs
to send while receiving. It carries the same event names and payloads, and no
`Last-Event-ID`: the client sends `{"since": seq}` instead. It exists for
interactive terminals. Nothing in v1 requires it to be used for agent events.

## Permissions

`GET /v1/permissions?status=pending[&session=]` returns `PermissionRequest[]`:
`{ id, session_id, kind, title, detail, options, created_at }`. `kind` is
`file_write`, `file_read`, `terminal`, `tool_call` or `other`. `options` is the
subset of `allow_once`, `allow_always`, `deny` the broker will accept.

`POST /v1/permissions/{id}/resolve` takes `{ "option": "allow_once" }` and returns
`PermissionDecision { option, decided_by, at }`.

- An option that is not in `options` is `400 invalid_request` with the allowed set
  in `details`.
- Repeating the same decision is idempotent (`200`, same body), so a retried
  request is safe. A *different* option after a decision is `409 conflict`: the
  agent has already acted on the first answer.
- A request whose session has closed is `409 conflict`. An unknown id is `404`.
- v1 has no timeout. A request stays pending until resolved or the session closes,
  and the broker denies on close. `PermissionDecision.decided_by` records `user`,
  `policy:<rule-id>` or `close`.

This is the scripted path: list pending, pick an option from the same object, post
it, switch on `code` on failure. No polling loop and no interactive prompt are
required.

## Files

`GET /v1/files?workspace=&path=` returns `FileContent { path, text, etag, marks? }`
for a file. A directory is `409 conflict` with the message pointing at
`/v1/files/list`. `marks` is what the gutter draws: `{ line, change }`, with
`change` one of `added`, `modified`, `deleted` and `line` counted from one in the
text as it is now. It is absent when nothing changed, when the workspace is not a
repository, and when the host has no `git`. It travels with the text rather than
behind its own route. A second question about the same file could otherwise answer
about a different version of it. `PUT /v1/files` answers with the same shape, its
marks computed from what is now on disk.

`GET /v1/files/list?workspace=&path=` returns `FileEntry[]`
(`{ path, is_dir, size, modified?, ignored?, status? }`). Paths are relative to the
workspace root with `/` as the separator on every host. `include_ignored=true`
returns the rows a `.gitignore` hides, each with `ignored: true`. The flag is absent
from an ordinary row, so a client can tell an exception from a file.

`status` is what git says changed about that path: `modified`, `added`,
`untracked`, `deleted` or `conflicted`. It is absent when nothing changed, when
the workspace is not a repository, and when the host has no `git`. A directory
carries the most urgent change beneath it, so a closed row still says something
inside it changed. The order is `conflicted` > `deleted` > `added` > `modified` >
`untracked`, because an edit to a tracked file is what a commit is about. The letter
a tree draws (`M`, `A`, `?`, `D`, `!`) is a client's rendering of the state, not a
second field. One path a listing cannot mark is one that is gone. A listing is the
filesystem, so a path that no longer exists has no row. The source-control routes
(P6.5-2) are where a file that is gone appears as `deleted`.

A workspace opened with `POST /v1/workspaces` on a host other than `local` is read
through that host's helper, not through this machine's disk. `workspace` is the
row's `id`, `path` is still relative to the root, and the daemon asks `fs.list` or
`fs.read` over the transport. The reply is translated once, here. Rows of a remote
listing carry `is_dir` and `size` and no `modified`, `ignored` or `status`. Ignore
rules and git letters are this machine's reading of files it cannot see. A remote
read carries an `etag` the daemon hashes from the text it was handed. That token is
the one a write sends back. `PUT` and `DELETE /v1/files` and
`POST /v1/files/directory` take the same `workspace`, with `PUT` and the directory
body in the request body and `DELETE` in the query. The daemon asks the host to
`fs.write`, `fs.remove` or `fs.mkdir`. A remote `if_match` is checked by reading
the file again over the same transport, so a create is a read that failed. A stale
token, and a write with no token over a file that exists, are the same `409`s they
are locally. The far side answers what it did rather than refusing. A directory
answers `removed: false`, because a tree is a decision and not a removal. A folder
that was already there answers `created: false`. "Not a file" and "not there" are
different answers to a person, and only the host can tell them apart. Every
route that *walks* the workspace (`search`, `grep`, `references`, `diagnostics`,
the `/v1/git/*` family) answers `409 conflict` naming the host when asked about a
remote workspace. Each one reads directories here, and a plausible answer about a
different machine is worse than a refusal.

`GET /v1/files/search?workspace=&q=` returns `FileEntry[]` of the files whose
paths answer `q`, ignoring case, best match first, at most 200 of them. A query is
a substring (`tui` finds `crates/acpilot-tui/src/app.rs`) or initials matched in
order (`aptui` finds the same path). A find that only accepted a substring would
not find the file you can half-remember. The walk follows the same ignore rules
the listing does: `.gitignore` files are read when `tree_respect_gitignore` is on,
so a find cannot return what the tree hides. Hidden files that no rule names
(`.env`) are found. `.git` is never walked. An empty `q` matches nothing rather
than walking the repository.

`GET /v1/files/grep?workspace=&q=` returns `GrepMatch[]` (`{ path, line, text }`).
Each row is a line of the workspace's text files that contains `q`, ignoring case.
`line` is counted from 1, rows are sorted by path and line, and at most 200 come
back. It uses the same walk and the same ignore rules as the path search. A word
therefore cannot be found in a file the tree hides. Binary files are skipped rather than decoded, by the same
NUL test a read uses. So are files over the read cap, because a search is not a way
around a route's limits. The line's text travels with the match so a jump list can
be read before it is chosen from. An empty `q` matches nothing.

`GET /v1/files/references?workspace=&q=` returns `Reference[]`
(`{ path, line, column, text }`): where one identifier is used in **code**, sorted
by path, line and column, at most 200 of them. The search is word-bounded and then
filtered by the parse tree: a row is a leaf the grammar calls a name. A comment
that mentions it and a string that spells it are not uses. A file this build ships
no grammar for (a `.md`, a `.sh`) is skipped rather than searched as prose. That
filter is what makes the list readable, and it is why the parse happens here: the
text is the daemon's. `column` is a character column counted from 1, not a byte
offset. `text` is the line, so a row can be read before it is chosen. It uses the
same walk and ignore rules as the other two searches, and the same cap on file
size. A `q` that is empty or not one word matches nothing. It is a search with a
parser's help rather than `textDocument/references`. Scope is unresolved, so two
different functions called `new` are one name until a language server answers.

With `path` and `offset`, the file the caret is in and the caret's offset in it,
counted in **characters**, the daemon asks a language server first. The server is
the one installed for that language (`rust-analyzer`, `pyright-langserver`,
`typescript-language-server`), detected on `PATH`, started once per language and
kept. The file is sent with `didOpen` (and `didChange` after that), so the answer is
about the text that is there. `textDocument/references` is asked with
`includeDeclaration`. That answer is **scoped** rather than spelled: two functions
called `new` are two functions, and a field named `id` is not every `id` in the
workspace. Rows come back in the same shape, with the server's line and its
negotiated encoding converted to a character column. Locations outside the
workspace are dropped, because the tree cannot open them.

A language with no server falls back to the parse-tree search above. So does one
that is not installed, one that will not start, and one that does not answer. That
fallback is the `parse-references` feature of `acpilotd`, on by default. A build with
`--no-default-features` links no grammar at all and answers references only from a
server.

A workspace on a host has no files here, so the fallback is not available there.
The server runs **on that workspace's host**, through its helper, and the documents
it opens are the host's paths. The host must have been tested and its helper must
offer `lsp.start` (`409` otherwise, naming the host). The server is the same
detected program as on this machine. It starts in the workspace root as the host
spells it, and its `file://` URIs index that machine's files. A references request
on a remote workspace with no server answer is `409` rather than an empty list. An
empty list would say "no uses" about a workspace nobody searched.
`POST /v1/files/diagnostics` takes the same route. The server starts on the
workspace's host, and absent `text` means the file is read through the helper. The
rows' lines come back from the machine that has the file. A server on a host is one
per workspace root and language, so two workspaces on one host are two indexes. It
is stopped by the same idle sweep as a local one.

`POST /v1/files/format` takes `FileFormatRequest { path, text, workspace? }` and
asks that server to format the buffer. It answers `FileFormat { path, text,
formatted, server?, reason? }`. `formatted` is true only when a server answered, and
`text` is what the caller should write. A language with no configured server, a
missing server or one that refuses is not an error. The text comes back unchanged
with `reason`, because format-on-save must not turn a missing formatter into a save
that loses an edit. The edit positions are converted from the server's negotiated
encoding in the daemon and applied from the end of the document backwards. An edit
that cannot be placed refuses the whole format rather than applying half of it.

`POST /v1/files/references` is the same question with the **open buffer** attached:
`ReferencesRequest { q, path, offset, text?, workspace? }`. `text` is the file as the
editor holds it, for the file `path` names, and it is what the search runs over. For
the server, which is handed that text as the document it opens, the caret's offset
resolves against what the pane draws. For the parse-tree fallback it replaces the
disk read of that one file. Nothing is written. It is a read that names its own
version. That is what makes the list right about an edit that has not been saved.
Every other file is still read where it is, because a client for a remote daemon
does not have them. A use in a *second* edited buffer is listed as its last save. An
absent `text` makes this the disk question the `GET` form asks.

`PUT /v1/files` takes `FileWriteRequest { path, text, if_match? }`. `if_match` is
the `etag` last read. A mismatch is `409 conflict`, never a silent overwrite.
Omitting it creates a file and is `409` if the file already exists. Binary content
is refused (`invalid_request`), not mangled.

The `etag` is the modification time, the size and a hash of the bytes. The first
two make a change visible without reading the file. The hash catches an edit that
preserved both, which is what a fast tool does.

A path is relative to the workspace root, and `..` is refused rather than
resolved. The answer therefore does not depend on whether the directory it climbs
out of happens to exist. The resolved path is then checked against the resolved
root, because a symlink inside the workspace can point outside it. That is `403`,
and a string prefix check would have called it contained. A parent directory that
is not there is `404`: this route saves files, it does not build trees.

None of this is on the keystroke path: the editor owns its buffer and saves
through this route.

`DELETE /v1/files?workspace=&path=` removes a **file** and answers `204` with no
body. A directory is `409`, not a recursive removal. Removing a tree is a decision
about what happens to everything inside it, and a route that guesses has already
discarded the answer. An editor's folder removal is a confirmation the client
draws, and what it calls afterwards is one removal per entry. The same path rules as
the write apply: `..` is refused, and a symlink out of the workspace is `403`.

`POST /v1/files/rename` takes `{ path, to }` and answers with the moved file's
`FileContent { path, text, etag, marks? }`. It renames the entry rather than copying
the bytes, so the text and its history stay attached to the file. `to` must be a new
path: an existing destination is `409`, never a replace. The destination's directory
must exist (`404`), because this route does not build trees. The source must be a
file (`409` for a directory), and the same path twice is `400`. A **remote**
workspace moves the entry on its host through the helper's `fs.rename`, so the text
travels between hosts in neither direction on the way. An older helper that does not
answer that method is a `409` naming the stale upload. The destination rules are the
same there.

`POST /v1/files/directory` takes `{ "path": string }` and answers `201`. Parents
are created, because a new-folder dialog names the folder and not every level
between it and the root. The deepest part of the path that already exists is the
one resolved and checked. A symlink in the middle is how a recursive create lands
outside the workspace. Something already at that path is `409`. A
zero-length name is `400`.

## Git

The source-control pane's read and index half is served from the host's own `git`
through `acpilot-git`. What a pane says is staged therefore agrees with the command
line.
The repository is discovered from the workspace root. A workspace opened inside a
monorepo is still about the repository that contains it.

`GET /v1/git/status?workspace=` returns `GitStatus { branch?, upstream?, ahead,
behind, head?, files[] }`. Each `GitStatusEntry` is `{ path, old_path?, index,
worktree }`, and both `index` and `worktree` are one of `unmodified`, `modified`,
`added`, `deleted`, `renamed`, `copied`, `type_changed`, `unmerged`, `untracked` or
`ignored`. Both halves travel on one row, because which side of the edit is staged
is the question the pane exists to answer. `branch` is absent on a detached `HEAD`.
`head` is absent before the first commit. `ahead` and `behind` are always present,
because zero is an answer.

`GET /v1/git/diff?workspace=&scope=` returns `GitDiff { patch, files[] }`. `scope`
is `unstaged` (the default: working tree against the index), `staged` (index
against `HEAD`), or `all` (working tree against `HEAD`). Anything else is `400`
with the accepted spellings in `details`. The patch is unified, colour-free and
external-diff-free: the text a pane draws rather than the text a terminal would
re-render. `files` is the list a commit composer iterates without parsing headers.

`GET /v1/git/tree?workspace=` returns `GitTreeEntry[]`, every path in the working
tree, sorted, each with whether the index knows it. This is the listing a
source-control tree folds into directories. `GET /v1/files/list` is still the one
that lists a filesystem directory and marks hidden rows.

`GET /v1/git/log?workspace=&limit=` returns `GitCommit[]`, newest first in
topological order, each `{ id, parents[], author, email, authored_at, subject }`.
`limit` defaults to 50 and is capped at 500 rather than refused. `limit=0` is an
empty history, as it is for a repository with no commits.

The index and working-tree writes are four routes. `POST /v1/git/stage`,
`POST /v1/git/unstage`, `POST /v1/git/discard` and `POST /v1/git/checkout` take
`GitIndexRequest { workspace?, paths[] }` and answer the new `GitStatus`. Paths are
relative to the repository root, as they are in every other git answer. An empty
list is `400` with `details.field: "paths"`. So is a path with a `..` or an
absolute component: the daemon checks before git sees it, so a pane cannot write
outside the repository.

Stage runs `git add -A -- <paths>`. Unstage runs `git reset -q HEAD -- <paths>`, or
`git rm --cached` before the first commit, where there is no `HEAD` to reset to. A
staged rename carries its old path too, so both halves move together.

`POST /v1/git/stage-hunk` takes `GitHunkRequest { workspace?, path, hunk }` and
answers the new `GitStatus`. `hunk` is zero-based inside that path's **current
unstaged** diff: the patch from the index to the working tree, not the combined
`HEAD`-to-worktree patch. The daemon cuts the selected hunk out of git's own output
and applies it with `git apply --cached --whitespace=nowarn -`. A client never sends
patch text. A path with no unstaged hunk, or a number past the end, is `409`,
because the patch moved under the request. An invalid path is still `400`. Staging
one hunk leaves the rest of the file in the working tree, and the next status draws
the staged and unstaged halves separately.

Discard puts the working tree back to the index. Tracked paths go through
`git restore --worktree -q -- <paths>`, while an untracked path has no index
version and is removed with `git clean -f -q --`. A staged edit stays staged,
which is what makes this the lighter action. Checkout puts **both** halves back to
`HEAD` with `git restore --source=HEAD --staged --worktree -q -- <paths>`. A path
that only exists in the index is removed with `git rm -f`, because `HEAD` does not
have it. That covers a staged addition and the new half of a rename. A staged
rename restores its old name and removes the new one from a row that names only the
new path.

Both are refused with `409` when no named path has anything to restore. Checkout
is also `409` before the first commit, because there is no `HEAD`. It is `409` for
an untracked path too, because there is no version to check out. Discard is the
action that removes it. Git's own refusal, such as a conflict or a locked index, is `409`
with its words, not a daemon fault. The routes never ask again themselves. Each
client arms the action and writes only after a second, explicit answer.

**Paths in every answer are relative to the repository root.** That is what git
reports, and it is the one spelling a status row and a patch header can share.
When the workspace is the repository's root, the ordinary case, they are the same
paths `GET /v1/files` takes. A client drawing a workspace inside a monorepo asks
`/v1/files/list` for the rows it may open.

A directory that is not inside a repository is `404 not_found`, not an empty
status. There is no repository to describe, which is the same answer a route that
read one file would give. A host without `git` is `502 host_unavailable` with
`details.field: "git"`, because installing it is the fix and a `500` would read as a
bug in the daemon. A workspace on another host is `409`, like every other route
that walks this machine's filesystem. Its tree still draws letters through
`/v1/files/list`, which asks the host, but a diff over there needs a helper method
that does not exist yet.

The generation half is two more routes. `GET /v1/git/commit/prompt` returns
`CommitPrompt { template, source }`, where `source` is `default` or `profile`. It is
the effective template before its placeholders are filled, so an editor can show
what it is about to send. A profile's template lives in
`<profile>/commit-prompt.txt` and is read per request. Without that file the
template is `acpilot_core::commit::DEFAULT_PROMPT`. An empty file is a `400` naming
its path rather than a silent fall back to the default.

`POST /v1/git/commit-message` takes `CommitMessageRequest { workspace?,
session_id, prompt? }` and returns a `CommitDraft { message, subject, body, kind,
scope?, breaking, emoji?, generator }`. It renders the **staged** diff, its files
and the recent subjects into the prompt. It sends that prompt to the named active
session and checks the answer with `acpilot_core::commit`. The staged diff is
the only change it sees. Nothing is committed, and the draft is not stored, so the
editor can change it before the commit action of P6.5-4. `prompt` replaces the
template for this request only. `generator` is `session:<id>`. The model-lab recipe
half of the ticket uses the recipe routes of P4.5-7, which serve a recipe from disk.

The format is enforced rather than fixed. An unknown type, a missing
description, a subject over 72 characters, or a body without a blank line is a
`400 invalid_request` with the rule in `details.field: "message"`. Nothing staged
is a `409` before the agent is asked. A busy or unknown session is `409` or `404`.
A session that fails or does not answer inside two minutes is `502 agent_failed`.

`POST /v1/git/commit` takes `GitCommitRequest { workspace?, message }` and commits
the staged index with the whole message, returning the `GitCommit` git made. It is
one of the git routes that writes, and it commits only after a person approves the
message. It never stages: an unstaged edit is not in the commit, and the pane's
stage/unstage controls are how an edit reaches it. An empty message is `400` with
`details.field: "message"`. An empty index is `409` before git runs. Git's own
refusal, such as a failing hook, an unresolved merge or a missing identity, is also
`409`, with git's words in the message and in `details.field: "message"`. That is
something the person can fix rather than a daemon fault. A missing `git` is still
`502`.

Config changes remain a separate, unbuilt action. All three surfaces read these
routes and prepare or restore the index. The TUI opens with `F6` and shows separate
**Unstaged** and **Staged** sections. It uses `h`/`l` to choose an unstaged hunk and
`Shift-S` to stage it. It uses `s`/`u` for the whole path and `d`/`x` for
discard/checkout. `y` approves and `n` cancels the destructive pair. The desktop
overlay uses the same `F6`, sections and keys. Both draft and edit in a composer,
and call `/commit` only after `Ctrl-S` followed by `y`. The served page's **Source
control** panel reads only when opened, and shows both halves with one mark per
hunk. It stages a hunk with **Stage hunk N of M** or the whole path with **Stage
path**/**Unstage**. It arms **Discard** or **Checkout** behind an inline
confirmation, and writes a commit only after **Review commit** and then **Commit
staged index**.

## Project map

`GET /v1/project/map?workspace=` returns `ProjectMap { path, written, updated?,
areas[] }`, where each `MapArea` is `{ name, path, summary, depends_on? }`. It says
what a directory is for, and which parts of the workspace it leans on.

The map is a document the agent writes, not state the daemon derives. It lives at
`.acpilot/project-map.toml` in the workspace, versioned with the code, so a wrong
statement about the architecture is a diff somebody argues with. It reads as:

```toml
updated = "2026-11-01"

[[areas]]
name = "TUI shell"
path = "crates/acpilot-tui"
summary = "The terminal frontend: app state, keys, drawing."
depends_on = ["acpilot-proto", "acpilot-client"]
```

`path` is a workspace-relative directory. An empty one (or `.`) is the whole
workspace. `summary` is one paragraph. `depends_on` is optional and names things
rather than resolving them, because a map is a document. Areas come back in the
order the file lists them: a map that sorted itself would be one whose order nobody
chose.

A map that is not there is **not** an error. The answer is `written: false` with no
areas. That is what a repository nobody has mapped yet looks like, and a panel says
so instead of a `404` standing in for a sentence. A file that is not UTF-8,
or not TOML, or missing a `name` or `summary`, is `400 invalid_request` naming
`.acpilot/project-map.toml`. A file that exists and cannot be read is a fact the
person editing it needs. Parsing happens in the daemon rather than in each client.
The file is where the workspace is, which may not be where the client is.

The panel's "which area am I in" question, the open file's neighbourhood, is
answered by the deepest `path` that is a prefix of the file. The comparison goes
segment by segment, so `crates/tui` is not a parent of `crates/tui-tools`, and the
earlier area wins a tie.

## Terminals

`POST /v1/terminals` takes `CreateTerminalRequest { session_id?, workspace?,
command?, args?, cwd?, size }` and returns a `TerminalSummary` with the process
already running. A terminal belongs to a session because that is what owns the
work it does. A shell with no session on the daemon's own host would be a second,
sessionless way to run commands there. So `session_id` is required for the
workspace this daemon serves, and may be **absent** for a workspace on another
host. Sessions do not run on a host yet (P8-5). A client with no session, such as a
browser tab on a remote workspace's shell, still needs a pane there. A sessionless
terminal is that client's. `TerminalSummary.session_id` is absent, and
`GET /v1/terminals?session=` does not list it for any session. It ends when its
program ends or when the daemon does. `command` defaults to the host's own shell.
`cwd` is relative to the workspace root, and an absolute path is accepted and
checked. A directory outside the root is `403`, not a silent start somewhere else.

`workspace` names a workspace from `GET /v1/workspaces`. Absent is the one this
daemon serves. A workspace on an ssh or WSL host starts the shell **there**, through
its helper. The pane is bytes in both directions plus a size, so it is the same
attach socket and the same screen. The daemon never pretends the process is local.
The host must have been tested and its helper must offer a PTY (`409` otherwise,
naming the host). `cwd` is checked by string, with no `..` out of the root and no
absolute path outside it, since there is no `canonicalize` across the transport.
The helper starts `$SHELL` or `/bin/sh` with `TERM=xterm-256color`, because a daemon
under a service manager has no terminal to copy a `TERM` from. A pane a session
opened is that session's: sessions run on this daemon, and P8-5 starts them on a
host. A pane an agent opened for a session the daemon could not map is
**sessionless** instead of filed under an id nothing resolves. A client that lists
every pane can still offer it.

The summary carries `workspace` and `host` when the pane is not in the daemon's own
workspace or on its own machine. `workspace` is the id the request named, and `host`
is the id `GET /v1/hosts` lists. A picker needs both. A shell on another host looks like a
local one otherwise, and a person typing into it deserves to know which tree it is
in.

`GET /v1/terminals?session=` lists them, same shape, filterable. With no `session`
it lists every pane, the sessionless ones included. A pane an agent opened for a
session the daemon could not map has no id to be filtered by. The unfiltered
listing is the only one that names it.

`WS /v1/terminals/{id}/attach` is the pane. A browser opens it with
`new WebSocket(url, ["acpilot.v1", token])`, with the token as a subprotocol,
because a socket constructor sets no headers (see **Auth**). The handshake answers
`acpilot.v1`. A client that can set `Authorization` sends the bearer header and
needs no protocol. The stream is deliberately split by frame type, because terminal
output can contain anything a shell can print:

| Direction | Frame | Meaning |
| --- | --- | --- |
| client → daemon | binary | Keystrokes, byte for byte. No newline is added. |
| client → daemon | text | `{"type":"resize","cols":100,"rows":30}` or `{"type":"detach"}` |
| daemon → client | text | `{"type":"attached","terminal":{…},"replay_bytes":n}` first |
| daemon → client | binary | Exactly `replay_bytes` of screen, then live output |
| daemon → client | text | `{"type":"exit","code":0}` last, then the socket closes |

`attached` is always the first frame. The binary frame after it is exactly
`replay_bytes` long: the terminal's current screen drawn with the escape sequences
that reproduce it. That is why attaching late is the normal case rather than a
race. A pane that opens on a command that finished two minutes ago shows what it
printed. Live output follows as binary frames, and anything the program writes
after the attach arrives once, in order.

Closing the socket, or sending `detach`, leaves the process running. A pane is a
view of a terminal, not its owner. The process ends when it ends. A pane that stops
reading is closed with an error code rather than queued forever, and reattaching
costs it one screen.

A terminal an agent asked its client for (`terminal/create` in ACP) is listed and
attached the same way, under an `agent-` prefixed id. It is the same PTY, not a
copy. The daemon draws a screen from the bytes the agent's reader already holds. It
writes keystrokes to the same descriptor, so a shell an agent started can be watched
and driven without a second process printing to nobody. Such a pane ends when the
agent's program ends. A pane opened after the fact still replays what the command
printed. Killing the pane is not offered, because the process belongs to the session
that started it. `DELETE /v1/terminals/{id}` on an `agent-` id is `409` for that
reason, and `terminal/kill` over ACP is the owner's way to end it.

`POST /v1/terminals/{id}/resize` takes `TerminalSize { cols, rows }` and returns the
updated `TerminalSummary`. It is the same instruction the attach socket's `resize`
frame carries, for the panes that have no socket. A layout restored at startup knows
each pane's size before anything attaches. A zero dimension is `400`, because a
program told it is zero columns wide draws nothing.

`DELETE /v1/terminals/{id}` ends the program and forgets the pane, and answers
`204`. That is the other half of `terminal/release` being optional in ACP. A pane
nobody releases is a process nobody can find, so the removal of a session closes
its terminals too. Attaching to a pane that is gone is `404`. A pane is a view, and
the thing it was a view of is gone.

## Errors

Every failure is this shape, whether it came from a route, an agent, or a host that
stopped answering:

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

`code` is stable and clients switch on it. `message` is for humans and may be
reworded. `details` is present only when the failure belongs to specific fields,
and `field` is a dotted path (`preferences.wrap`).

502 versus 500 tells a client whether to offer "retry" or "file a bug". A host that
is asleep is not a bug in the daemon.

## What the review changed

The design phase's task P0-6 read the list against four clients and fixed the gaps
rather than noting them:

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

Nothing else was missing for v1. The provider routes landed later with the models
phase (P4.5-1) and are described above. Recipes, the model lab and the ACP bridge
stay out on purpose. They are planned for the same phase and for P8. Adding them
now would freeze transports that have not been measured.

## Recipes and runs

| Route | Answers |
| --- | --- |
| `GET /v1/models` | Every enabled provider asked once. `models` holds what answered, `problems` names the providers that did not |
| `PATCH /v1/providers/{id}` | Edit the label, base URL, token or `enabled`; `404` for an unknown id |
| `DELETE /v1/providers/{id}` | `204`; `409` while a recipe names the provider |
| `POST /v1/providers/{id}/probe` | Ask one provider now and keep `last_probed_at` and the model count |
| `GET /v1/recipes` | Recipe summaries, in id order |
| `POST /v1/recipes` | `201` with the stored recipe; `400` when validation fails |
| `GET /v1/recipes/{id}` | The recipe as stored; `404` for an unknown id |
| `PATCH /v1/recipes/{id}` | Replace only the named fields |
| `DELETE /v1/recipes/{id}` | `204`; runs already recorded are left alone |
| `POST /v1/recipes/{id}/runs` | Streams the run as SSE; `?root=` is required when `corpus` is not empty |
| `GET /v1/recipes/{id}/runs` | Newest first, `?cursor=` and `?limit=` |
| `GET /v1/recipes/{id}/runs/{run}` | One record: the summary plus the prompt, selectors, resolved files with sizes, and the budget it was assembled from; `404` for a run the log does not hold, including another recipe's |

A run is one HTTP response, so its status is `200` and not `202`. The frames are:

```
event: model.run.started    data: {"run_id":"…","recipe_id":"demo","model_id":"…"}
event: model.run.delta      data: {"run_id":"…","text":"…"}
event: model.run.finished   data: {"run_id":"…","status":"succeeded","usage":{…}}
```

The record is appended to `runs.jsonl` before `finished` is sent. A failed run ends with
`status` `failed` and a `message`; the stream still closes with `200` because the request
was accepted. Closing the response abandons the run, and no record is written.

`POST /v1/recipes/{id}/promote` is not served yet; it is P4.5-9.
