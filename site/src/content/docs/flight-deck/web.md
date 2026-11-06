---
title: Web
description: The same daemon in a browser tab, for the machine you are not sitting at.
---

<span class="pill pill--hangar">hangar</span> &nbsp; Web phase (P11)

The web UI exists for one specific scene: the daemon is running on your office
workstation, and you are somewhere else. It is not a hosted service and there is
no account to create — the daemon serves its own frontend.

## Shape

`acpilotd` embeds the built SPA, so "install the web UI" means "run the daemon".
The page is themed by the same extension the terminal and the desktop app read:
the daemon resolves `theme_web` (over `theme`) from the workspace's settings each
time the page is served and sends those tokens as its CSS variables, so `--bg` and
`--accent` are the theme's, not values someone typed into a stylesheet. Reading it
per request is what makes a settings file only worth saving once: nothing has to be
restarted for the next load to be the new colours.

Routes are profile- and workspace-aware:

```text
/p/:profile/w/:workspace
/p/:profile/s/:session
```

## Pieces

| Need | Choice | Why |
| --- | --- | --- |
| Editor | CodeMirror 6 | Small, embeddable, and what a code editor wants — Monaco is a VS Code engine with a VS Code bundle |
| Diff | `@codemirror/merge` | Same document model as the editor |
| Terminal | xterm.js | The boring correct answer |
| Framework | Svelte 5 + Vite | Runes, small runtime, no virtual DOM tax |
| API client | Generated from OpenAPI | A hand-written client drifts; a generated one fails to compile |

<Aside type="note" title="Language servers do not move into the browser">
The browser never spawns a language server. It speaks to the daemon over a
WebSocket bridge, so the completions you get in a tab are the same ones the TUI
shows — including for a server running on the SSH host.
</Aside>

## Auth

The unix socket needs no token: its access control is the filesystem. Anything on
TCP — loopback included — needs a bearer token: a random value in a `0600` file
beside the socket, compared in constant time. There is no user database, so there
is no password hash to store; the file is the only place the secret lives. TLS
termination is the deployment's job; the daemon does not pretend to be a web
server fleet.

The page itself is served with a strict policy: `default-src 'none'`, inline
script and style only (the bundle is one file), `connect-src 'self'` for the API
and the streams, `frame-ancestors 'none'` so the token field cannot be clickjacked,
and `Referrer-Policy: no-referrer` so a link out cannot name the daemon.

## Status

The Svelte app exists and is what ships, with the editor still to come.
`web/` is Svelte 5 + Vite + TypeScript; `npm run build` emits **one HTML file**
into `crates/acpilot-web/assets/`, with its script and stylesheet inlined, and
`acpilotd` serves that at `/` — and at `/app`, the spelling existing links and the
page's own subpaths use. The built file is committed, so building the
daemon needs no Node; CI rebuilds it and fails if the committed page drifted from
its source.

The page asks for the daemon's token, then shows what that token can see: the
health answer (service, API version, workspace), the stats bar — a track per
integration, with **Show sources** naming where each figure came from, which is the
same question the terminal's `F4` asks — and the session list with each row's
agent, status and last event. **About** names the running build and the
licence; **Quit** tries to close the tab and says so when the browser refuses.

**Terminals** is the one interactive pane so far. The section lists every pane
`GET /v1/terminals` returns — the open session's, and the ones no session owns, a
shell in another workspace on another host among them, each saying where it runs —
and **Attach** draws the chosen one with xterm.js over
`WS /v1/terminals/{id}/attach`, with the fit addon sizing the cells and the WebGL
renderer falling back to xterm's own when a context is refused. A pane is a view,
not an owner: detaching or closing the tab leaves the program running, a dropped
socket reattaches with backoff and replays the screen, and an exit notice is the
only close that ends the view. Copy and paste are `Ctrl-Shift-C` and
`Ctrl-Shift-V`, so `Ctrl-C` stays the interrupt a shell expects.

**Sessions** is the view a turn happens in. **Open** on a row streams that
session's transcript from `GET /v1/sessions/{id}/events` and draws messages, tool
calls and notices, with the agent's markdown rendered — headings, fences, lists,
inline code and `http(s)` links, all as text nodes, so an agent cannot write markup
into the page. A permission request appears on the profile-wide `GET /v1/events`
stream, or is fetched from the pending list when the page opens after the request,
and its card is answered **optimistically**: it goes on the press and comes back if
the daemon refuses, because the pending list — not the page — is the record of what
is still a question. **Cancel turn** asks the daemon to stop the turn in flight and
says whether there was one to stop.

Neither stream can use `EventSource`: both need `Authorization`, and `EventSource`
sets no headers. The page reads the same frames from a `fetch` body and reconnects
with `after=<last sequence>`, so the token stays a header rather than a query
string. A clean end is not retried — that is a stored conversation finishing its
replay — while a dropped connection is, with a backoff. A `resync`, which means the
ring no longer holds what was missed, drops the transcript and replays it from
sequence 0: the store is the record, and a full replay is the only way to recover
frames the ring let go.

The page starts sessions too. **Start a session** picks from `GET /v1/agents` and
posts `workspace_id: ""`, which the daemon reads as its own workspace, and the
prompt box posts a turn with a per-press idempotency key so a retried request
cannot start a second run. A write's permission card draws the daemon's `detail`
as a unified diff, hunks included, and a table in the agent's markdown is drawn as
a table; an image is its alt text, linked, and never fetched, because a fetched
picture is a read receipt. The transcript keeps the newest 200 turns in the DOM
and offers the rest behind **Show earlier turns**. The status badge beside a
session's title is only as fresh as the list load — the stream updates the
transcript, not that row.

The socket is the one request in the product a browser cannot give an
`Authorization` header to, so the token travels as the `acpilot.v1` subprotocol
(`new WebSocket(url, ["acpilot.v1", token])`) and the daemon answers with the
protocol name, never the token. That is still a header rather than a query
string: the secret stays out of the daemon's log, the browser's history and any
`Referer`.
The shell is served without a token like `/docs`, because a browser cannot attach
a header to a typed address and the page holds no data; every API call behind it
is still guarded.

The page's colours come from the same theme extension the terminal and the window
read: `/` (and `/app`) inlines the web theme's palette into its `:root`, and the page then
listens on `GET /app/palette/events` so saving a theme anywhere — the TUI, the
desktop app, another tab — repaints an open page instead of waiting for a reload.
That stream is one of the unauthenticated routes, alongside `/`, `/app` and `/docs`.

The bar's rows are measured by the daemon, not by the page: `GET /app/stats`
answers the same rows `acpilot-stats` builds for the terminal and the desktop app,
already reduced to a fill and a sentence. A browser cannot link a Rust crate, and a
second implementation of the denominator rules in TypeScript is one that drifts the
first time a rule changes. That route is under `/app` because it is a rendering of
the contract rather than part of it, and it needs the token — an agent roster is not
the page's own business. A
browser cannot put an `Authorization` header on an `EventSource`, and the obvious
alternative — the token in the stream's query string — is a secret in the daemon's
log, the browser's history and any `Referer`. It is safe to leave open because the
frame carries exactly the variables the page was served with: no session, no file,
nothing a token would have protected. If that ever stops being true, the route
needs a token, not a wider exception.

Tests hold that page to account. `npm test` mounts the shell in a headless DOM
against a fake stream, which is the page's own logic, and pins the terminal's
rules in `web/src/terminals.test.ts`: which panes are listed, what a sessionless
row says, that the token is a protocol and never a URL, and that a dropped socket
reattaches while a close or an exit does not. `npm run test:live` builds a real
daemon, opens the page against it, saves `theme_web` in a temporary settings
file, and asserts the already-open page repainted — a socket, a watcher and a
stream, with the browser as the only stand-in. On the daemon side,
`crates/acpilotd/tests/terminal_attach.rs` performs the browser's exact
handshake against a real listener and the real auth layer. CI runs all of it in
the `web` and `check` jobs, so the repaint and the attach are gates rather than
demonstrations.

```bash
acpilot serve --http                  # loopback, http://127.0.0.1:7777/
acpilot serve --bind 0.0.0.0:7777    # the same over the LAN; every request needs the token
# the token is printed on the run that creates it, and lives after that in
# $XDG_RUNTIME_DIR/acpilot/token; `acpilot serve --token` prints it again
```

Between the command and the page there is one step people miss: this daemon is the
same one the CLI and the TUI talk to, so `/` shows the build that is running right
now rather than a version baked into a bundle weeks ago.

Still open from the web phase: the routes above, the generated API client, the
CodeMirror editor, and creating a pane from the page (it attaches to panes, and with
a session open it now holds the id `POST /v1/terminals` wants, but it makes no such
request). What exists now is the shape they hang on — embedded, self-serving, one
file, no asset directory to lose.
