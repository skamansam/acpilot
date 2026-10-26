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

Loopback is no-token by default. Anything that is not loopback needs a bearer
token, stored as an argon2 hash. TLS termination is the deployment's job; the
daemon does not pretend to be a web server fleet.

## Status

The Svelte app exists and is what ships, with the editor still to come.
`web/` is Svelte 5 + Vite + TypeScript; `npm run build` emits **one HTML file**
into `crates/acpilot-web/assets/`, with its script and stylesheet inlined, and
`acpilotd` serves that at `/app`. The built file is committed, so building the
daemon needs no Node; CI rebuilds it and fails if the committed page drifted from
its source.

The page asks for the daemon's token, then shows what that token can see: the
health answer (service, API version, workspace), and the session list with each
row's agent, status and last event. **About** names the running build and the
licence; **Quit** tries to close the tab and says so when the browser refuses.
The shell is served without a token like `/docs`, because a browser cannot attach
a header to a typed address and the page holds no data; every API call behind it
is still guarded.

The page's colours come from the same theme extension the terminal and the window
read: `/app` inlines the web theme's palette into its `:root`, and the page then
listens on `GET /app/palette/events` so saving a theme anywhere — the TUI, the
desktop app, another tab — repaints an open page instead of waiting for a reload.
That stream is one of the unauthenticated routes, alongside `/app` and `/docs`. A
browser cannot put an `Authorization` header on an `EventSource`, and the obvious
alternative — the token in the stream's query string — is a secret in the daemon's
log, the browser's history and any `Referer`. It is safe to leave open because the
frame carries exactly the variables the page was served with: no session, no file,
nothing a token would have protected. If that ever stops being true, the route
needs a token, not a wider exception.

```bash
cargo run -p acpilotd -- --addr 127.0.0.1:7777
# then open http://127.0.0.1:7777/app
# the token is in $XDG_RUNTIME_DIR/acpilot/token and in the daemon's log
```

Between the `cargo run` and the page there is one step people miss: this daemon is
the same one the CLI and the TUI talk to, so `/app` shows the build that is running
right now rather than a version baked into a bundle weeks ago.

Still open from the web phase: the routes above, the generated API client, the
CodeMirror editor, the SSE transcript, the xterm terminal, and the daemon-side
CSP. What exists now is the shape they hang on — embedded, self-serving, one file,
no asset directory to lose.
