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
"$schema" = "https://rudeboy.dev/acpilot/schema/agents-v1.json"
schema_version = 1

[[agents]]
id = "claude-code"
name = "Claude Code"
transport = "acp-stdio"
command = "npx"
args = ["-y", "@zed-industries/claude-code-acp"]
env = { ANTHROPIC_API_KEY = "keyring:claude-code" }
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

These are the spellings to write. The snake_case form the API returns
(`acp_stdio`) is accepted too, so a value copied out of `GET /v1/agents` can be
pasted in unchanged — but a typo is still an error that lists the valid values.

The bridge in [remote control](../../flight-test/remote-control/) is the mirror
image: there the daemon is the ACP **server**, and a remote driver is the client.
It is configured in `config.toml` rather than in this file, because it is a
property of the daemon, not of an agent.

## Fields

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | Stable identifier; lowercase, used by `--agent` and in the API |
| `name` | string | Display name; defaults to the id |
| `description` | string | One sentence on what the agent is for |
| `transport` | enum | One of the three above |
| `command` | string | `acp-stdio` and `pty`: executable, resolved on the **host** the workspace lives on |
| `args` | string[] | `acp-stdio` and `pty`: passed verbatim |
| `env` | table | `acp-stdio` and `pty`: literal values (`NO_COLOR = "1"`), or `keyring:<name>` references |
| `url` | string | `http-jsonrpc` only: where the agent answers (http or https) |
| `auth` | string | `http-jsonrpc` only: a `keyring:` reference. A plaintext token is refused |
| `restart` | enum | `never` (the default) or `on-crash` (max 3 attempts) |
| `allow_cross_profile` | bool | Required before a `keyring:<profile>/<name>` reference is accepted |
| `parser` | string | PTY only: `plain-transcript`, `ansi-transcript`, or `script:<path>` |
| `prompt_row` | string | PTY only: text that identifies the input row, so chrome can be cut away |
| `sentinel` | string | PTY only: the input row back at rest — the primary turn-complete signal |
| `exit_command` | string | PTY only: what to type to leave cleanly (e.g. `/exit`); `Ctrl-D` is not universal |
| `submit_delay_ms` | integer | PTY only: gap between the text and the Enter key; one write of `text\r` is a paste, not a submit, in at least one TUI |
| `idle_timeout_ms` | integer | PTY only: backstop when the sentinel does not fire. 5,000 avoided every truncated turn measured; smaller values did not |

## Schema

The file identifies its schema, so an editor completes keys and underlines typos
before the loader ever sees them:

```toml
"$schema" = "https://rudeboy.dev/acpilot/schema/agents-v1.json"
```

That document is published at
`https://rudeboy.dev/acpilot/schema/agents-v1.json` — JSON Schema 2020-12,
Apache-2.0 — and generated from the same Rust types the loader parses, so it
cannot describe a key the loader would refuse. A test validates the presets we
ship against it and checks that a file the schema rejects is a file the loader
rejects. If you would rather keep the document free of editor plumbing, Taplo and
Even Better TOML also read a first line of `#:schema <url>`.

What the schema checks: the root keys, `schema_version`, and for each transport
the fields it may carry and the ones it cannot load without. What it cannot
check, because JSON Schema has no way to say it: a duplicate `id`, and a
`keyring:` reference that does not resolve. **The loader stays the authority**;
the schema is there so its refusals arrive as a squiggle rather than as a
surprise on the next start.

The `-v1` in the name is the major version, and this URL is frozen. Within v1 the
document may only gain optional keys or enum members; any change a
`schema_version = 1` loader would read differently ships as `agents-v2.json`
alongside `schema_version = 2`, and the v1 file is never edited again. There is
deliberately no `latest.json`: a moving URL is how an editor's cached schema
falls out of step with the loader without anyone noticing.

## Validation

A manifest is checked before use, not at spawn time:

- Unknown keys are errors (same rule as configuration). The one exception is
  `"$schema"`, which is an editor's and is ignored here.
- `transport` must be one of the three, and its required fields must be present.
- A field that belongs to another transport is an error too. `sentinel` on an
  `acp-stdio` agent is a misunderstanding worth reporting, not a typo worth
  ignoring — and a key that silently does nothing is how a file stops matching
  the behaviour somebody believes it has.
- `id` must be unique within the profile.
- `keyring:` references are resolved eagerly, so a missing credential is reported
  when the profile loads rather than mid-turn. `keyring:<name>` means this
  profile; `keyring:<profile>/<name>` means another one and additionally needs
  `allow_cross_profile = true`. The entry lives in the OS keyring under the
  `acpilot` service, with the account `<profile>/<name>` in both forms — so the
  same line means two different secrets in `profiles/work` and `profiles/personal`.

A file that fails any check does not load at all, and a profile whose file
fails to reload keeps the agents it already had: the old snapshot is only
replaced by a complete, valid file.

<Aside type="note" title="Why a manifest and not a registry service">
Because the file is reviewable, diffable, and portable. A registry would need
accounts, network calls, and a trust model that nobody asked for. If your team
wants to share agents, share a file.
</Aside>
