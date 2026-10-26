---
title: HTTP API
description: One control plane for every client, with an error envelope you can switch on.
---

<span class="pill pill--flying">flying</span> &nbsp; health + OpenAPI &nbsp;·&nbsp; <span class="pill pill--hangar">hangar</span> &nbsp; everything else

Every client — CLI, TUI, desktop, browser — uses this API. There is no privileged
back channel, which is why the API is designed before the UIs rather than
extracted from them afterwards. `docs/api.md` in the repository is the frozen v1
contract, including the event taxonomy and the reconnect rules; this page is the
short version.

## Today

```bash
GET /v1/health                  # {"ok":true,"service":"acpilotd","version":"0.1.0","api":"/v1","workspace":"app"}
GET /v1/agents                  # configured agents, with availability (available | missing)
GET /v1/permissions?status=pending
POST /v1/permissions/{id}/resolve
GET /v1/events                  # text/event-stream: preferences, permission.requested, heartbeat
GET /v1/sessions                # POST starts one; prompt, cancel, and an SSE transcript per session
GET /v1/terminals?session=      # POST starts one; WS /v1/terminals/{id}/attach is the pane
GET /v1/files?path=             # PUT writes it back with `if_match`; /v1/files/list for directories
GET /v1/openapi.json            # the contract, generated from the types
GET /docs                       # Swagger UI, only with --features docs; asks for the token
GET /app                        # the web shell: build version, About, quit; no token
```

`GET /v1/health` also reports the workspace the daemon was started with
(`workspace`, omitted when it serves none), which is how a client confirms that
the daemon it found is the one for the directory it was asked about — the check
behind `acpilot tui --workspace <dir>` refusing a mismatch instead of showing
another project's sessions.

`GET /v1/agents` answers from the profile the daemon was started with —
`acpilotd --profile <dir>` reads that directory's `agents.toml` — and reports
availability from a `PATH` lookup on the daemon's host. No process is started, so
listing agents cannot launch one. With no profile loaded the list is empty;
handshake status (`unhealthy`) and trust pinning (`untrusted`) are later phases.

`GET /v1/events` is what makes a prompt immediate instead of polled: a request
raised after the client connects arrives as a `permission` frame carrying the
same object the pending list returns, so the client can answer it without asking
again. It is profile-wide, and it does not replay — a request that arrived while
the client was away is recovered from the pending list, which is state.

The same stream carries the theme. A `prefs` frame is the whole `Preferences`
object the daemon would apply, sent as the first frame of a connection and again
whenever the user's or the project's `config.toml` settles after a save, so a
window or a browser can follow `theme` without seeing this machine's files and
without a restart. Preferences are state rather than history, which is why they
are the one thing the stream sends on connect; a file that does not parse
announces nothing, and the last good preferences stay in force. The TUI resolves
the same frames, so a client behind a socket and one sitting in the daemon's
directory end up with the same palette from two paths.

`WS /v1/terminals/{id}/attach` is the one route that upgrades instead of
answering. Binary frames are the terminal in both directions — keystrokes one
way, output the other — and text frames are control: `resize` from the client,
`attached` and `exit` from the daemon. `attached` announces how many bytes of
screen follow, and those bytes are the escape sequences that redraw what the
terminal already shows, so attaching late is the normal case rather than a race.
Closing the socket leaves the process running: a pane is a view, not an owner.
The same route serves the terminals an agent opens — they are listed under an
`agent-` id, over the PTY the agent already has, so a person can watch and type
into the shell an agent started without a second process behind it.

## Planned

| Area | Endpoints |
| --- | --- |
| Hosts | `GET/POST /v1/hosts`, `POST /v1/hosts/{id}/test` |
| Workspaces | `GET/POST /v1/workspaces` |
| Agents | `POST /v1/agents/{id}/refresh`, `POST /v1/agents/{id}/trust` |

| Permissions | `GET /v1/permissions`, `POST /v1/permissions/{id}/resolve` |
| Profiles | `GET/POST /v1/profiles`, `PATCH /v1/profiles/{id}/preferences` |
| LSP | `GET /v1/lsp/servers`, `GET /v1/lsp/diagnostics`, `POST /v1/lsp/request` |
| Models (post-v1) | `GET/POST /v1/providers`, `POST /v1/providers/{id}/probe`, `GET /v1/models` |
| Recipes (post-v1) | `GET/POST /v1/recipes`, `POST /v1/recipes/{id}/runs`, `GET /v1/recipes/{id}/runs`, `POST /v1/recipes/{id}/promote` |
| Remote drivers (post-v1) | `GET /v1/acp/bridge`, `GET /v1/acp/drivers`, `POST /v1/acp/pairings`, `POST /v1/acp/drivers/{id}/revoke`; ACP itself at `/acp` |

## The error envelope

Every failure is the same shape, whether it came from a route, an agent, or a
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

`502` versus `500` matters: it tells a client whether to offer "retry" or "file a
bug". A host that is asleep is not a bug in the daemon.

## Two rules that shape every route

**The API is a control plane, not a keystroke pipe.** Edits live in the client's
text buffer; the API handles saves, reads, and intent. HTTP never sits between a
keypress and a character appearing.

**Mutations must name their profile.** A mutating request without
`X-Acpilot-Profile` is `400`, not "whatever is active". Guessing an identity is
how code gets sent with the wrong key.

<Aside type="note" title="Why SSE and not WebSockets everywhere">
Agent output is a stream of events with sequence numbers — `session.*`, `permission.*`,
`lsp.*`, `model.run.*` — and the same replay guarantee applies to all of them. SSE gives replay via
`Last-Event-ID`, survives proxies, and reconnects for free. WebSockets are used
where they earn it — terminal attach, LSP bridge — not as a default.
</Aside>
