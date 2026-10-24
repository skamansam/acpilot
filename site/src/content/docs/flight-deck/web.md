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

Not started. The `web/` directory and the `acpilot-web` crate arrive with the web phase (P11);
the page is here so the API decisions can be read against the frontend they serve.
