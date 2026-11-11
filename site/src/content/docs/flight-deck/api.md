---
title: HTTP API
description: One control plane for every client, with an error envelope you can switch on.
---

<span class="pill pill--flying">flying</span> &nbsp; health + OpenAPI &nbsp;·&nbsp; <span class="pill pill--hangar">hangar</span> &nbsp; everything else

Every client uses this API: CLI, TUI, desktop, and browser. There is no privileged
back channel. That is why we design the API before the UIs, rather than
extract it from them afterwards. `docs/api.md` in the repository is the frozen v1
contract, including the event taxonomy and the reconnect rules. This page is the
short version.

## Today

```bash
GET /v1/health                  # {"ok":true,"service":"acpilotd","version":"0.1.0","api":"/v1","workspace":"app"}
GET /v1/agents                  # configured agents, with availability (available | missing)
GET /v1/permissions?status=pending
POST /v1/permissions/{id}/resolve
GET /v1/events                  # text/event-stream: preferences, permission.requested, diagnostics, heartbeat
GET /v1/sessions                # POST starts one; prompt, cancel, and an SSE transcript per session
GET /v1/terminals?session=      # POST starts one; WS /v1/terminals/{id}/attach is the pane
GET /v1/files?path=             # PUT writes it back with `if_match`; /v1/files/list for directories
POST /v1/files/format           # the daemon's language server; unchanged text with a reason when none
GET/POST /v1/profiles           # PATCH/DELETE one; /preferences reads or changes the cascade; /export is keyring-safe
GET /v1/git/status              # status, diff, tree and log from the host's git; paths are repo-relative
GET /v1/git/commit/prompt       # the profile's commit prompt; POST /v1/git/commit-message drafts from the index
POST /v1/git/stage              # add paths to the index and answer the new status; /unstage reverses it
POST /v1/git/commit             # commit the staged index with a confirmed message; the route does not stage
GET /v1/stats                   # one row per integration: context usage, credits, and where each came from
GET /v1/openapi.json            # the contract, generated from the types
GET /docs                       # Swagger UI, only with --features docs; asks for the token
GET /                          # the web app: build version, About, quit; no token. `/app` is the same page
GET /app/stats                  # the shell's stats bar, measured by the daemon; needs the token
```

`GET /v1/health` also reports the workspace the daemon opened with. The daemon
omits the `workspace` field when it serves none. A client uses it to confirm that
the daemon it found is the one for the directory the client asked about. That is
the confirmation behind `acpilot tui --workspace <dir>` refusing a mismatch
instead of showing another project's sessions. The answer also reports `profile`,
the profile directory's name, omitted when the daemon started without one. The
web page writes that name into its `/p/:profile/...` addresses. A page that
guessed would address a profile nobody started.

`GET /v1/agents` answers from the profile the operator chose at daemon start.
`acpilotd --profile <dir>` reads that directory's `agents.toml`. The route reports
availability from a `PATH` lookup on the daemon's host. It starts no process, so
listing agents cannot start one. With no profile loaded the list is empty.
Handshake status (`unhealthy`) and trust pinning (`untrusted`) are later phases.

`GET /v1/events` makes a prompt immediate instead of polled. A request raised
after the client connects arrives as a `permission` frame. That frame carries the
same object the pending list returns, so the client can answer without asking
again. The stream is profile-wide, and it does not replay. A request that arrived
while the client was away comes from the pending list, which is state.

The same stream carries the theme. A `prefs` frame is the whole `Preferences`
object the daemon would apply. The daemon sends it as the first frame of a
connection. It sends it again whenever the user's or the project's `config.toml`
settles after a save. A window or a browser can therefore follow `theme` without seeing
this machine's files and without a restart. Preferences are state rather than
history, which is why they are the one thing the stream sends on connect. A file
that does not parse gets a `prefs_error` frame with the parser's sentence, and
the last good preferences stay in force. `PATCH
/v1/profiles/{id}/preferences` changes only the keys it names and sends the new
object immediately. A client can therefore offer a preference control without
polling. The TUI resolves the same frames. A client behind a socket and one
sitting in the daemon's directory therefore end up with the same palette from
two paths.

`WS /v1/terminals/{id}/attach` is the one route that upgrades instead of
answering. Binary frames are the terminal in both directions: keystrokes one way,
output the other. Text frames are control: `resize` from the client, `attached`
and `exit` from the daemon. `attached` announces how many bytes of screen follow.
Those bytes are the escape sequences that redraw what the terminal already shows,
so attaching late is the normal case rather than a race. Closing the socket
leaves the process running. A pane is a view, not an owner. The same route serves
the terminals an agent opens. The daemon lists them under an `agent-` id, over
the PTY the agent already has. A person can therefore watch and type into the
shell an agent started without a second process behind it.

## Planned

| Area | Endpoints |
| --- | --- |
| Hosts | `GET/POST /v1/hosts`, `POST /v1/hosts/{id}/test` |
| Workspaces | `GET/POST /v1/workspaces` |
| Agents | `POST /v1/agents/{id}/refresh`, `POST /v1/agents/{id}/trust` |

| Permissions | `GET /v1/permissions`, `POST /v1/permissions/{id}/resolve` |
| LSP | `GET /v1/lsp/servers`, `GET /v1/lsp/diagnostics`, `POST /v1/lsp/request` |
| Models (post-v1) | `GET/POST /v1/providers`, `POST /v1/providers/{id}/probe`, `GET /v1/models` |
| Recipes (post-v1) | `GET/POST /v1/recipes`, `POST /v1/recipes/{id}/runs`, `GET /v1/recipes/{id}/runs`, `POST /v1/recipes/{id}/promote` |
| Remote drivers (post-v1) | `GET /v1/acp/bridge`, `GET /v1/acp/drivers`, `POST /v1/acp/pairings`, `POST /v1/acp/drivers/{id}/revoke`. ACP itself at `/acp` |

## The error envelope

Every failure has the same shape, whether it came from a route, an agent, or a
host that stopped answering:

```json
{
  "code": "permission_denied",
  "message": "write outside workspace: ../secrets.env",
  "details": [{ "field": "path", "message": "escapes /home/me/app" }]
}
```

| `code` | HTTP | Meaning |
| --- | --- | --- |
| `invalid_request` | 400 | Malformed body, bad enum, missing profile header |
| `not_found` | 404 | The thing is not there |
| `conflict` | 409 | Stale ETag, duplicate turn, closed session, already answered |
| `permission_denied` | 403 | Policy or a human said no |
| `timeout` | 504 | An agent, LSP server or host did not answer in time |
| `host_unavailable` | 502 | SSH/WSL host did not answer |
| `model_unavailable` | 502 | The inference server is down, refused, or answered nonsense |
| `agent_failed` | 502 | The agent process died or broke protocol |
| `protocol` | 500 | Malformed frame, bad version |
| `io` | 500 | Filesystem or process I/O |

`502` versus `500` matters. It tells a client whether to offer "retry" or "file a
bug". A host that is asleep is not a bug in the daemon.

## Two rules that shape every route

**The API is a control plane, not a keystroke pipe.** Edits live in the client's
text buffer. The API handles saves, reads, and intent. HTTP never sits between a
keypress and a character appearing.

**Mutations must name their profile.** A mutating request without
`X-Acpilot-Profile` is `400`, not "whatever is active". Guessing an identity is
how code gets sent with the wrong key.

<Aside type="note" title="Why SSE and not WebSockets everywhere">
Agent output is a stream of events with sequence numbers: `session.*`,
`permission.*`, `lsp.*`, and `model.run.*`. The same replay guarantee applies to
all of them. SSE gives replay via `Last-Event-ID`, survives proxies, and
reconnects for free. WebSockets serve where they earn it, such as terminal
attach and the LSP bridge. They are not the default.
</Aside>
