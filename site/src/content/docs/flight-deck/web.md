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

A shell, and honest about it. `acpilotd` embeds one page (`acpilot-web/assets/index.html`)
and serves it at `/app`: the name, the build answering, an **About** dialog and a
**Quit** that closes the tab. It is served without a token like `/docs`, because a
browser cannot attach a header to a typed address and the page holds no data.

```bash
cargo run -p acpilotd -- --addr 127.0.0.1:7777
# then open http://127.0.0.1:7777/app
```

Between the `cargo run` and the page there is one step people miss: this daemon is
the same one the CLI and the TUI talk to, so `/app` shows the build that is running
right now rather than a version baked into a bundle weeks ago.

The editor, transcript and terminal are the web phase (P11): the `web/` Svelte app,
the generated API client, and the routes above arrive then. What exists now is the
shape they hang on — embedded, self-serving, no asset directory to lose.
