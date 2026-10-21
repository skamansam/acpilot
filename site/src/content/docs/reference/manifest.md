---
title: Agent manifest
description: The file that tells Acpilot what an agent is and how to start it.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P2 &nbsp;·&nbsp; provisional until P2 ships

Agents are declared in `agents.toml`, per profile. The manifest is data, not code:
adding an agent should never require a rebuild.

## Example

```toml
# ~/.config/acpilot/profiles/work/agents.toml
schema_version = 1

[[agents]]
id = "claude-code"
name = "Claude Code"
transport = "acp-stdio"
command = "npx"
args = ["-y", "@zed-industries/claude-code-acp"]
env = { ANTHROPIC_API_KEY = "keyring:claude-code/work" }
restart = "on-crash"

[[agents]]
id = "freebuff"
name = "Freebuff"
transport = "pty"
command = "freebuff"
parser = "ansi-transcript"
prompt_row = "▍Enter a coding task"
sentinel = "▍Enter a coding task"
exit_command = "/exit"
idle_timeout_ms = 5000

[[agents]]
id = "local-gemini"
transport = "acp-stdio"
command = "gemini"
args = ["--experimental-acp"]

# Written by `acpilot recipes promote`. A model-backed agent is an ordinary
# ACP agent; the recipe is its configuration, not a second transport.
[[agents]]
id = "corpus-scraper"
name = "Corpus scraper (Qwen 7B)"
transport = "acp-stdio"
command = "acpilot-agent-lm"
args = ["--recipe", "corpus-scraper"]
restart = "on-crash"
# recipe: profiles/work/recipes/corpus-scraper.toml (mtime 2026-10-20T14:02:11Z)
```

## Transports

| Transport | What it means | Requirements |
| --- | --- | --- |
| `acp-stdio` | Native ACP over stdio | `command`; agent implements ACP |
| `pty` | Program driven through a PTY | `command`, `parser`, and a `sentinel`; `idle_timeout_ms` is the backstop |
| `http-jsonrpc` | ACP over HTTP | `url`; used for remote agents |

The bridge in [remote control](../../flight-test/remote-control/) is the mirror
image: there the daemon is the ACP **server**, and a remote driver is the client.
It is configured in `config.toml` rather than in this file, because it is a
property of the daemon, not of an agent.

## Fields

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | Stable identifier; used by `--agent` and in the API |
| `name` | string | Display name |
| `transport` | enum | One of the three above |
| `command` | string | Executable, resolved on the **host** the workspace lives on |
| `args` | string[] | Passed verbatim |
| `env` | table | Values may be `keyring:<agent>/<profile>` references |
| `restart` | enum | `never` or `on-crash` (max 3 attempts) |
| `parser` | string | PTY only: `plain-transcript`, `ansi-transcript`, or `script:<path>` |
| `prompt_row` | string | PTY only: text that identifies the input row, so chrome can be cut away |
| `sentinel` | string | PTY only: the input row back at rest — the primary turn-complete signal |
| `exit_command` | string | PTY only: what to type to leave cleanly (e.g. `/exit`); `Ctrl-D` is not universal |
| `submit_delay_ms` | integer | PTY only: gap between the text and the Enter key; one write of `text\r` is a paste, not a submit, in at least one TUI |
| `idle_timeout_ms` | integer | PTY only: backstop when the sentinel does not fire. 5,000 avoided every truncated turn measured; smaller values did not |

## Validation

A manifest is checked before use, not at spawn time:

- Unknown keys are errors (same rule as configuration).
- `transport` must be one of the three, and its required fields must be present.
- `id` must be unique within the profile.
- `keyring:` references are resolved eagerly, so a missing credential is reported
  when the profile loads rather than mid-turn.

<Aside type="note" title="Why a manifest and not a registry service">
Because the file is reviewable, diffable, and portable. A registry would need
accounts, network calls, and a trust model that nobody asked for. If your team
wants to share agents, share a file.
</Aside>
