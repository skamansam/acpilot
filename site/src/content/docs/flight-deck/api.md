---
title: HTTP API
description: One control plane for every client, with an error envelope you can switch on.
---

<span class="pill pill--flying">flying</span> &nbsp; health + OpenAPI &nbsp;·&nbsp; <span class="pill pill--hangar">hangar</span> &nbsp; everything else

Every client — CLI, TUI, desktop, browser — uses this API. There is no privileged
back channel, which is why the API is designed before the UIs rather than
extracted from them afterwards.

## Today

```bash
GET /v1/health          # {"ok":true,"service":"acpilotd","version":"0.1.0","api":"/v1"}
GET /v1/openapi.json    # the contract, generated from the types
GET /docs               # Swagger UI, only with --features docs
```

## Planned

| Area | Endpoints |
| --- | --- |
| Hosts | `GET/POST /v1/hosts`, `POST /v1/hosts/{id}/test` |
| Workspaces | `GET/POST /v1/workspaces` |
| Agents | `GET /v1/agents`, `POST /v1/agents/{id}/trust` |
| Sessions | `POST /v1/sessions`, `POST /v1/sessions/{id}/prompt`, `POST /v1/sessions/{id}/cancel` |
| Events | `GET /v1/sessions/{id}/events` (SSE, `Last-Event-ID` replay) |
| Files | `GET/PUT /v1/files` with ETag and `If-Match` |
| Terminals | `WS /v1/terminals/{id}/attach`, resize, replay-on-attach |
| Permissions | `GET /v1/permissions`, `POST /v1/permissions/{id}/decide` |
| Profiles | `GET/POST /v1/profiles`, `PATCH /v1/profiles/{id}/preferences` |
| LSP | `GET /v1/lsp`, `WS /v1/lsp/{id}/attach` |

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
| `not_found` | 404 | The thing is not there |
| `conflict` | 409 | Stale ETag, duplicate turn, closed session |
| `permission_denied` | 403 | Policy or a human said no |
| `host_unavailable` | 502 | SSH/WSL host did not answer |
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
Agent output is a stream of events with sequence numbers. SSE gives replay via
`Last-Event-ID`, survives proxies, and reconnects for free. WebSockets are used
where they earn it — terminal attach, LSP bridge — not as a default.
</Aside>
