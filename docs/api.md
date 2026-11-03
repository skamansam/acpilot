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

GET    /v1/stats
```

`GET /v1/health` is the one route a client calls before it trusts anything else:
`{ok, service, version, api, workspace?}`. `workspace` is the id the daemon was
started with and is omitted when it has none; a client uses it to check that the
daemon already listening is serving the directory it was asked about, rather than
restarting a daemon that may be mid-turn (ADR 0004 §2).

**Deliberately not v1**: providers and models, the model lab and recipes, the ACP
bridge and remote drivers, and extensions. They follow the same conventions when
they land; none of them changes a route above.

## Stats

`GET /v1/stats` returns `IntegrationStats[]`: one row per integration, including the
quiet ones, because a bar that reflowed when a provider stopped answering would be a
bar nobody could watch. A row is `{ id, kind, label, context?, credits?, updated_at
}`; `kind` is `provider` or `agent`, and `label` is the display name so a bar can be
drawn without a second lookup.

`context` is `{ used_tokens, window_tokens?, source }` and `credits` is `{ used?,
remaining?, limit?, currency, source }`. `source` is `server`, `configured`,
`estimated` or `unknown`, recorded **per figure** — a server may report a window and
no balance, and a plan may name a ceiling the server never mentions.

The rules a client is held to:

- **No default window.** An absent `window_tokens` renders as an indeterminate
track, never a full or an empty one. Substituting 32k is how a prompt gets
truncated without anyone noticing.
- **A missing figure is absent, not zero.** A local server has no credit bar; a
provider with no balance endpoint shows no credits rather than `0.00`.
- **An estimate is labelled.** `estimated` is a figure this daemon counted, and a
UI that styles it as a measurement is claiming something nobody reported.
- **The age is on the wire.** `updated_at` is the last observation, so a frozen bar
is visible as frozen.

What fills it today is the roster, and — for an agent with a conversation behind it
— an estimated context reading. One `agent` row per entry in the profile's
`agents.toml`; when a session with that agent has said anything, the sessions engine
has counted the characters it carries (four to a token), so the row is
`source: estimated` with no `window_tokens`, and a UI draws `18.4k / ?` against an
indeterminate track rather than a fraction of a window nobody stated. An agent's own
usage update replaces the count when it sends one (`source: server`, and the window
it names). A session that has done nothing yet reports no `context` at all, and a
row with a reading carries that session's last event as its `updated_at`, so a
conversation that stopped goes old on the bar instead of looking fresh for as long
as the roster can be read. Provider rows, configured windows and the balance reads
are the models phase's task (P4.5-12).

`GET /app/stats` is the page's copy: the same rows with `fill`, `text` and `source`
worked out, because a browser cannot link `acpilot-stats` and a second
implementation of the arithmetic in JavaScript is one that drifts. Same rows, same
order, measured by `acpilot-stats` — the crate the TUI and the desktop app also
draw. It is under `/app` because it is a rendering of the contract rather than part
of it, and it needs the token: an agent roster is not the page's own business.

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

`GET /v1/sessions` lists every conversation this profile has, not only the ones
this process started. A stored session is reported with `status: "closed"` —
nothing is running it — and a prompt to it is `409`, because resuming a stopped
conversation is not in v1. The rows live in one SQLite database per profile (the
service phase's task P3-7), which is what makes yesterday's conversation reachable
today; events older than 30 days are compacted out of it, while permission
decisions are kept because they are the audit rather than the transcript.

## Event streams

Two streams, one shape.

`GET /v1/sessions/{id}/events` is the session's stream. `GET /v1/events` is the
profile-wide stream: `profile.*`, `daemon.*`, `prefs`, `permission.requested`, and
`diagnostics` — the rest because what they pair with is profile-wide: the pending
list, the settings every surface draws with, and the problems of a file any of them
may have open. One connection therefore covers every session a UI is watching, the
theme it is drawing with, and a gutter that a server filled in on its own. It
carries no `session` events: a browser watching ten sessions would otherwise pay
for all of them.

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

id: 43
event: prefs
data: {"preferences":{"schema_version":1,"theme":"acpilot.night-flight","theme_web":"acpilot.paper",…}}

event: heartbeat
data: {"seq":43}
```

| `event` | `data` | Replayed? |
| --- | --- | --- |
| `session` | `SessionEvent` (`kind` is one of the eight below) | yes, from the session's ring, and on disk in its transcript |
| `permission` | `{"type":"requested","request":PermissionRequest}` or `{"type":"resolved","id":…,"decision":PermissionDecision}` | `resolved` yes; `requested` live-only, on both streams |
| `prefs` | `{"preferences": Preferences}` | the current ones, as the first frame of a connection; changes are live-only |
| `heartbeat` | `{"seq": n}` every 15 s | no |
| `resync` | `{"latest_seq": n}` | no |

`SessionEventKind` is closed: `message_chunk`, `thought`, `tool_call`, `plan`,
`mode_changed`, `notice`, `compaction`, `commands_changed`. Adapter detail lives
in `payload`; a new kind is a wire change and shows up in the OpenAPI snapshot.

**Reconnect.** The client sends `Last-Event-ID: <seq>` and receives everything
after it, in order, with no gaps. Replay is served from the live session's ring,
and every one of those events is appended to the session's transcript as it is
recorded, so the conversation outlives the process. A session this daemon did not
start is answered from that database: `GET /v1/sessions` lists it,
`GET /v1/sessions/{id}` describes it with status `closed`, and its stream replays
from the sequence asked for and then ends, because there is no live feed behind it
to wait on. If the id is unknown
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

**Preferences are state too, and they are the stream's first frame.** A client
that connects is told what the daemon would apply instead of learning it only when
somebody edits a file, so a window or a browser that cannot see this machine's
`config.toml` still opens with the right theme. A save — the daemon watches the
user's and the project's settings, debounced — arrives as another `prefs` frame,
which is the same whole object, so applying one is replacing what you had rather
than patching it. A file that does not parse announces nothing: the last good
preferences stay in force, and the next save is read normally. `theme_tui` is in
the frame as well, because the frame describes the cascade and not one surface's
slice of it — a client picks the theme for the surface it is.

**Diagnostics are events, not state.** A `diagnostics` frame carries
`DiagnosticsNotice { path, version?, problems }` — the same body
`POST /v1/files/diagnostics` answers with, minus the request — and says that a
language server published problems about a file the client did not ask about. That
happens on the daemon's own clock: a server with an index publishes again when a
background check finishes, and a pane that only heard about the problems it asked
for would keep a stale gutter until the next save. `version` is the document version
the server named, when it named one, so a client can tell a late publish from a
current one; most servers leave it out. The daemon also keeps the newest problems per
file — written by a publish and by an answer to `POST /v1/files/diagnostics` alike —
and sends them as unnumbered `diagnostics` frames when a client connects, so a
reconnect starts from what is already known. Individual frames are not replayed: the
snapshot is the state, and a file it does not mention is one nobody has asked about
since the daemon started.

`/v1/events` has no `Last-Event-ID` replay for the same reason. Its frames are
numbered with an `id` that counts what that connection has been told, and a
client that falls far enough behind to lose frames receives `event: resync` with
the latest number: the recovery is to refetch the pending list, exactly as a
reconnecting client does, rather than to expect the stream to reconstruct what it
missed. Preferences are the one exception to "no replay" — they are the current
answer, so a client that falls behind on them is sent the current preferences
rather than a `resync`. Heartbeats run on the same 15 s clock as the session
stream.

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

`GET /v1/files?workspace=&path=` returns `FileContent { path, text, etag, marks? }`
for a file; a directory is `409 conflict` with the message pointing at
`/v1/files/list`. `marks` is what the gutter draws — `{ line, change }` with
`change` one of `added`, `modified`, `deleted` and `line` counted from one in the
text as it is now — and it is absent when nothing changed, when the workspace is not
a repository, and when the host has no `git`. It travels with the text rather than
behind its own route because a second question about the same file could answer about
a different version of it. `PUT /v1/files` answers with the same shape, its marks
computed from what is now on disk.

`GET /v1/files/list?workspace=&path=` returns `FileEntry[]`
(`{ path, is_dir, size, modified?, ignored?, status? }`), paths relative to the
workspace root with `/` as the separator on every host. `include_ignored=true`
returns the rows a `.gitignore` hides, each with `ignored: true`; the flag is absent
from an ordinary row, so a client can tell an exception from a file.

`status` is what git says changed about that path — `modified`, `added`,
`untracked`, `deleted` or `conflicted` — and it is absent when nothing changed, when
the workspace is not a repository, and when the host has no `git`. A directory
carries the most urgent change beneath it, so a closed row still says something
inside it changed; the order is `conflicted` > `deleted` > `added` > `modified` >
`untracked`, because an edit to a tracked file is what a commit is about. The letter
a tree draws (`M`, `A`, `?`, `D`, `!`) is a client's rendering of the state, not a
second field. One path a listing cannot mark is a deleted one: a listing is the
filesystem, so a file that is gone has no row, and the source-control routes
(P6.5-2) are where a deleted file is listed as deleted.

`GET /v1/files/search?workspace=&q=` returns `FileEntry[]` of the files whose
paths answer `q`, ignoring case, best match first, at most 200 of them. A query is
a substring (`tui` finds `crates/acpilot-tui/src/app.rs`) or initials matched in
order (`aptui` finds the same path), because a find that only accepted a substring
would not find the file you can half-remember. The walk follows the same ignore
rules the listing does — `.gitignore` files are read when `tree_respect_gitignore`
is on — so a find cannot return what the tree hides. Hidden files that no rule
names (`.env`) are found, `.git` is never walked, and an empty `q` matches nothing
rather than walking the repository.

`GET /v1/files/grep?workspace=&q=` returns `GrepMatch[]` (`{ path, line, text }`):
the lines of the workspace's text files that contain `q`, ignoring case, `line`
counted from 1, sorted by path and line, at most 200 of them. Same walk and the
same ignore rules as the path search, so a word cannot be found in a file the tree
hides. Binary files are skipped rather than decoded — the NUL test a read uses —
and so are files over the read cap, because a search is not a way around a route's
limits. The line's text travels with the match so a jump list can be read before it
is chosen from; an empty `q` matches nothing.

`GET /v1/files/references?workspace=&q=` returns `Reference[]`
(`{ path, line, column, text }`): where one identifier is used in **code**, sorted by
path, line and column, at most 200 of them. The search is word-bounded and then
filtered by the parse tree — a row is a leaf the grammar calls a name — so a comment
that mentions it and a string that spells it are not uses, and a file this build
ships no grammar for (a `.md`, a `.sh`) is skipped rather than searched as prose.
That filter is what makes the list readable, and it is why the parse happens here:
the text is the daemon's. `column` is a character column counted from 1, not a byte
offset; `text` is the line, so a row can be read before it is chosen. Same walk and
ignore rules as the other two searches, the same cap on file size, and a `q` that is
empty or not one word matches nothing. It is a search with a parser's help rather than
`textDocument/references`: scope is unresolved, so two different functions called
`new` are one name until a language server answers.

With `path` and `offset` — the file the caret is in and the caret's offset in it,
counted in **characters** — the daemon asks a language server first. The server is
the one installed for that language (`rust-analyzer`, `pyright-langserver`,
`typescript-language-server`), detected on `PATH`, started once per language and
kept; the file is sent with `didOpen` (and `didChange` after that) so the answer is
about the text that is there, and `textDocument/references` is asked with
`includeDeclaration`. That answer is **scoped** rather than spelled: two functions
called `new` are two functions, and a field named `id` is not every `id` in the
workspace. Rows come back in the same shape, with the server's line and its
negotiated encoding converted to a character column, and locations outside the
workspace are dropped because the tree cannot open them.

A language with no server, no server installed, one that will not start, or one that
does not answer falls back to the parse-tree search above. That fallback is the
`parse-references` feature of `acpilotd`, on by default: a build with
`--no-default-features` links no grammar at all and answers references only from a
server.

`POST /v1/files/references` is the same question with the **open buffer** attached:
`ReferencesRequest { q, path, offset, text?, workspace? }`. `text` is the file as the
editor holds it, for the file `path` names, and it is what the search runs over — for
the server, which is handed that text as the document it opens, so the caret's offset
resolves against what the pane shows, and for the parse-tree fallback in place of the
disk read of that one file. Nothing is written: it is a read that names its own
version, and it is what makes the list right about an edit that has not been saved.
Every other file is still read where it is, because a client for a remote daemon does
not have them, so a use in a *second* edited buffer is listed as its last save. An
absent `text` makes this the disk question the `GET` form asks.

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

## Project map

`GET /v1/project/map?workspace=` returns `ProjectMap { path, written, updated?, areas[] }`,
where each `MapArea` is `{ name, path, summary, depends_on? }`: what a directory is for,
and which parts of the workspace it leans on.

The map is a document the agent writes, not state the daemon derives. It lives at
`.acpilot/project-map.toml` in the workspace — versioned with the code, so a wrong
statement about the architecture is a diff somebody argues with — and reads as:

```toml
updated = "2026-11-01"

[[areas]]
name = "TUI shell"
path = "crates/acpilot-tui"
summary = "The terminal frontend: app state, keys, drawing."
depends_on = ["acpilot-proto", "acpilot-client"]
```

`path` is a workspace-relative directory and an empty one (or `.`) is the whole
workspace; `summary` is one paragraph; `depends_on` is optional and names things
rather than resolving them, because a map is a document. Areas come back in the order
the file lists them — a map that sorted itself would be one whose order nobody chose.

A map that is not there is **not** an error: the answer is `written: false` with no
areas, which is what a repository nobody has mapped yet looks like, and a panel says
so instead of a `404` standing in for a sentence. A file that is not UTF-8, or not
TOML, or missing a `name` or `summary`, is `400 invalid_request` naming
`.acpilot/project-map.toml`: a file that exists and cannot be read is a fact the
person editing it needs. Parsing happens in the daemon rather than in each client
because the file is where the workspace is, which may not be where the client is.

The panel's "which area am I in" question — the open file's neighbourhood — is
answered by the deepest `path` that is a prefix of the file, compared segment by
segment so `crates/tui` is not a parent of `crates/tui-tools`, with the earlier area
winning a tie.

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
