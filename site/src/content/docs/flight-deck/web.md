---
title: Web
description: The same daemon in a browser tab, for the machine you are not sitting at.
---

<span class="pill pill--hangar">hangar</span> &nbsp; Web phase (P11)

The web UI exists for one scene. The daemon runs on your office workstation, and
you are somewhere else. It is not a hosted service, and there is no account to
create. The daemon serves its own frontend.

## Shape

`acpilotd` embeds the built SPA, so "install the web UI" means "run the daemon".
The page takes its theme from the same extension the terminal and the desktop app
read. The daemon resolves `theme_web` (over `theme`) from the workspace's settings
each time it serves the page, and it sends those tokens as CSS variables. `--bg`
and `--accent` therefore come from the theme, not from values someone typed into
a stylesheet. Reading the settings per request is what makes a settings file
worth saving once. Nothing has to restart for the next load to use the new
colours.

Routes are profile- and workspace-aware:

```text
/p/:profile/w/:workspace
/p/:profile/s/:session
```

The page writes those addresses itself. Opening a session from the list pushes
`/p/<profile>/s/<id>`, and the workspace picker pushes `/p/<profile>/w/<name>`.
The daemon serves the page at both, so a refresh or a link sent to a colleague
lands on the same view. The profile segment comes from `GET /v1/health`, which
names the directory the daemon started with. Every request carries that profile as
`X-Acpilot-Profile`, including the streams, because an SSE subscription binds its
profile at connect. The picker is filled from `GET /v1/profiles`. Choosing a
profile calls `POST /v1/profiles/{id}/activate`, moves the address, re-reads the
page for the new identity and reconnects the palette stream. That stream binds at
connect and cannot carry the header. A daemon that serves one profile disables
the picker rather than offering a choice it cannot honour.

## Editing

The editor pane edits the daemon's files, not a browser filesystem. It lists a
directory with `GET /v1/files/list`, opens a file with `GET /v1/files`, and writes
it back with `PUT /v1/files` carrying the `etag` it read as `if_match`. A file
that changed on disk while the buffer was open is therefore a `409`. The page
shows it as "nothing was overwritten", never as a silent save. The diff view is
`@codemirror/merge` over the version on disk and the buffer. The git marks the
read route returns are drawn in a gutter beside the line numbers: `+` added, `~`
changed, `-` deleted. The marks are positions, not line numbers. Typing above one
moves it down with its line, and a save asks git again for the new text.

Open files are tabs. Each keeps its own buffer, so switching back to one does not
re-read the disk and lose an edit. A dot marks the tabs with unsaved text. The
open paths and their unsaved buffers survive a reload. A restored draft is dirty.
It is saved against the version it was typed over. A file changed while the page
was closed therefore refuses the save instead of being replaced unseen. A tab
whose file is gone keeps its draft as a file to create. A draft too large for the
browser's store is dropped with the paths still remembered. **New file** takes a
path and opens an empty buffer. Its first **Save** sends `PUT /v1/files` without
`if_match`. The daemon reads that as a create and refuses with `409` if something
already exists there. Closing a tab with unsaved text asks first.

**Rename** and **Delete** are offered on the open file and on every file row in
the listing. A row action does not open the file. Rename moves the entry through
`POST /v1/files/rename`. An open tab follows the answer, and a dirty buffer stays
dirty because the move did not change its text. Delete deletes the file through
`DELETE /v1/files` and drops its tab, asking first when the buffer has unsaved
text. A rename is refused when the destination exists or its directory is
missing. A remote workspace moves the entry through the host helper's `fs.rename`.
An older helper is a `409` asking to be updated, never a write-then-delete that
could leave a copy.

Highlighting is `@codemirror/lang-*`, the fallback the plan names. The page is one
inlined HTML file, and a tree-sitter wasm grammar would be a second asset the
daemon does not serve. The TUI draws tree-sitter's spans. The web pane draws
Lezer's, for JavaScript/TypeScript, JSON, Markdown, Rust and Python, and plain
text for anything else. Language servers are still the daemon's job. The browser
has no bridge to attach `@codemirror/lsp-client` to yet (P7).

## Pieces

| Need | Choice | Why |
| --- | --- | --- |
| Editor | CodeMirror 6 | Small, embeddable, and what a code editor wants. Monaco is a VS Code engine with a VS Code bundle |
| Diff | `@codemirror/merge` | Same document model as the editor |
| Terminal | xterm.js | The boring correct answer |
| Framework | Svelte 5 + Vite | Runes, small runtime, no virtual DOM tax |
| API client | Generated from OpenAPI | A hand-written client drifts. A generated one fails to compile |

<Aside type="note" title="Language servers do not move into the browser">
The browser never spawns a language server. It speaks to the daemon over a
WebSocket bridge. The completions in a tab therefore match the TUI's, including
for a server running on the SSH host.
</Aside>

## Auth

The unix socket needs no token. Its access control is the filesystem. Anything on
TCP, loopback included, needs a bearer token: a random value in a `0600` file
beside the socket, compared in constant time. There is no user database, so there
is no password hash to store. The file is the only place the secret lives. TLS
termination is the deployment's job. The daemon does not pretend to be a web
server fleet.

The page itself is served with a strict policy. `default-src 'none'` applies.
Inline script and style only, because the bundle is one file. `connect-src
'self'` covers the API and the streams. `frame-ancestors 'none'` stops a
clickjack against the token field. `Referrer-Policy: no-referrer` stops a link
out from naming the daemon.

## Status

The Svelte app exists and is what ships, editor included. It reads and writes a
file through the daemon's files routes and diffs the buffer against the version
on disk. `web/` is Svelte 5 + Vite + TypeScript. `npm run build` emits **one HTML
file** into `crates/acpilot-web/assets/`, with its script and stylesheet inlined.
`acpilotd` serves that at `/` and at `/app`, the spelling existing links and the
page's own subpaths use. The built file is committed, so building the daemon
needs no Node. CI rebuilds it and fails if the committed page drifted from its
source.

The page is checked at three depths. `npm test` in happy-dom covers the token flow
and the transcript's folding. `npm run test:live` runs against a spawned daemon
and covers the palette stream. `npm run test:e2e` uses Playwright and Chromium for
what only a layout engine can settle. That covers a keystroke reaching CodeMirror,
the diff, and a save carrying `if_match`. It also covers a permission card whose
Allow click writes the file. `cargo xtask web-check` runs all three, so CI's web
job does too.

The page asks for the daemon's token, then shows what that token can see. It shows
the health answer (service, API version, workspace), the stats bar, and the
session list with each row's agent, status and last event. The stats bar has a
track per integration, and **Show sources** names where each figure came from.
That is the same question the terminal's `F4` asks. **About** names the running
build and the licence. **Quit** tries to close the tab and says so when the
browser refuses.

**Terminals** is the one interactive pane so far. The section lists every pane
`GET /v1/terminals` returns: the open session's, and the ones no session owns,
including a shell in another workspace on another host. Each says where it runs.
**Attach** draws the chosen one with xterm.js over
`WS /v1/terminals/{id}/attach`. The fit addon sizes the cells, and the WebGL
renderer falls back to xterm's own when a context is refused. A pane is a view,
not an owner. Detaching or closing the tab leaves the program running. A dropped
socket reattaches with backoff and replays the screen. An exit notice is the only
close that ends the view. Copy and paste are `Ctrl-Shift-C` and `Ctrl-Shift-V`,
so `Ctrl-C` stays the interrupt a shell expects.

**Sessions** is the view a turn happens in. **Open** on a row streams that
session's transcript from `GET /v1/sessions/{id}/events` and draws messages, tool
calls and notices. The agent's markdown is rendered: headings, fences, lists,
inline code and `http(s)` links, all as text nodes. An agent therefore cannot
write markup into the page. A permission request appears on the profile-wide
`GET /v1/events` stream, or comes from the pending list when the page opens after
the request. Its card gets an **optimistic** answer. The answer goes on the press
and comes back if the daemon refuses. The pending list, not the page, is the
record of what is still a question. **Cancel turn** asks the daemon
to stop the turn in flight and says whether there was one to stop.

Neither stream can use `EventSource`. Both need `Authorization`, and
`EventSource` sets no headers. The page reads the same frames from a `fetch` body
and reconnects with `after=<last sequence>`, so the token stays a header rather
than a query string. A clean end is not retried, because that is a stored
conversation finishing its replay. A dropped connection is retried with a
backoff. A `resync` means the ring no longer holds what was missed. The page then
drops the transcript and replays it from sequence 0. The store is the record, and
a full replay is the only way to recover frames the ring let go.

The page starts sessions too. **Start a session** picks from `GET /v1/agents` and
posts `workspace_id: ""`, which the daemon reads as its own workspace. The prompt
box posts a turn with a per-press idempotency key, so a retried request cannot
start a second run. A write's permission card draws the daemon's `detail` as a
unified diff, hunks included. A table in the agent's markdown is drawn as a table.
An image is its alt text, linked, and never downloaded, because a downloaded
picture is a read receipt. The transcript keeps the newest 200 turns in the DOM and offers
the rest behind **Show earlier turns**. The status badge beside a session's title
is only as fresh as the list load. The stream updates the transcript, not that
row.

The socket is the one request in the product a browser cannot give an
`Authorization` header to. The token therefore travels as the `acpilot.v1`
subprotocol: `new WebSocket(url, ["acpilot.v1", token])`. The daemon answers with
the protocol name, never the token. That is still a header rather than a query
string. The secret stays out of the daemon's log, the browser's history and any
`Referer`. The shell is served without a token like `/docs`, because a browser
cannot attach a header to a typed address and the page holds no data. Every API
call behind it is still guarded.

The page's colours come from the same theme extension the terminal and the window
read. `/` (and `/app`) inlines the web theme's palette into its `:root`. The page
then listens on `GET /app/palette/events`. Saving a theme anywhere (the TUI, the
desktop app, another tab) therefore repaints an open page instead of waiting for
a reload. That stream is one of the unauthenticated routes, alongside `/`, `/app`
and `/docs`.

The bar's rows are measured by the daemon, not by the page. `GET /app/stats`
answers the same rows `acpilot-stats` builds for the terminal and the desktop app,
already reduced to a fill and a sentence. A browser cannot link a Rust crate. A
second implementation of the denominator rules in TypeScript would drift the first
time a rule changes. That route is under `/app` because it is a rendering of the
contract rather than part of it, and it needs the token. An agent roster is not
the page's own business. A browser cannot put an `Authorization` header on an
`EventSource`. The obvious alternative is the token in the stream's query
string. That puts a secret in the daemon's log, the browser's history and any
`Referer`. The route is safe to leave open because the frame carries exactly the
variables the page had. It carries no session, no file, and nothing a token would
protect. If that
ever stops being true, the route needs a token, not a wider exception.

Tests hold that page to account. `npm test` mounts the shell in a headless DOM
against a fake stream, which is the page's own logic. It also pins the terminal's
rules in `web/src/terminals.test.ts`. Those rules cover which panes are listed
and what a sessionless row says. They also require the token to be a protocol and
never a URL. A dropped socket must reattach, while a close or an exit must not. `npm run test:live` builds a real
daemon, opens the page against it, saves `theme_web` in a temporary settings file,
and asserts the already-open page repainted. That is a socket, a watcher and a
stream, with the browser as the only stand-in. On the daemon side,
`crates/acpilotd/tests/terminal_attach.rs` performs the browser's exact handshake
against a real listener and the real auth layer. CI runs all of it in the `web`
and `check` jobs, so the repaint and the attach are gates rather than
demonstrations.

```bash
acpilot serve                         # loopback, a free port printed with the token
acpilot serve --bind 0.0.0.0:7777    # the same over the LAN; every request needs the token
# the token is printed with the URL, and lives in $XDG_RUNTIME_DIR/acpilot/token
```

Between the command and the page there is one step people miss. This daemon is
the same one the CLI and the TUI talk to. `/` therefore shows the build that is
running now, not a version baked into a bundle weeks ago.

Still open from the web phase: attaching a language server (the daemon's WS
bridge is P7), and creating a pane from the page. The page attaches to panes, and
with a session open it now holds the id `POST /v1/terminals` wants, but it makes
no such request. What exists now is the shape they hang on: embedded,
self-serving, one file, no asset directory to lose.
