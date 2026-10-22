---
title: Configuration
description: One cascade, per-key merging, and a command that tells you which file won.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P4 &nbsp;·&nbsp; this shape may still change

Configuration is layered, and every layer can override a single key rather than a
whole file. The last word wins:

```mermaid
flowchart TD
    defaults["defaults.toml<br/>shipped with the binary"] --> global["config.toml<br/>global"]
    global --> profile["profiles/name/config.toml<br/>per profile"]
    profile --> workspace["workspace/.acpilot/config.toml"]
    workspace --> env["ACPILOT_* environment"]
    env --> flags["command-line flags<br/>highest precedence"]
```

## Where things live

| Platform | Config | Data | Runtime |
| --- | --- | --- | --- |
| Linux | `~/.config/acpilot` | `~/.local/share/acpilot` | `$XDG_RUNTIME_DIR/acpilot` |
| macOS | `~/Library/Application Support/acpilot` | same | Unix socket in the config dir |
| Windows | `%APPDATA%\acpilot` | `%LOCALAPPDATA%\acpilot` | `\\.\pipe\acpilot-$USER` |

Only `acpilot-profile` is allowed to call `directories`. Every other crate
resolves mutable paths through it, so "where does this file go" has exactly one
answer.

## Global config

```toml
# ~/.config/acpilot/config.toml
schema_version = 1
active_profile = "work"

[daemon]
addr = "127.0.0.1:7777"
log = "info"

[editor]
tab_width = 2
insert_spaces = true
format_on_save = true
wrap = false
theme = "acpilot.night-flight"
```

## Profile config

```toml
# ~/.config/acpilot/profiles/work/config.toml
schema_version = 1

[policy]
default = "ask"                       # allow | ask | deny
outside_workspace = "deny"

[lsp.rust]
command = "rust-analyzer"
args = []
```

## Providers and recipes

A model server is declared per profile, next to the agents that may use it. It is
inert until `enabled = true` *and* something asks it a question — there is no
startup scan.

```toml
# ~/.config/acpilot/profiles/work/providers.toml
schema_version = 1

[[providers]]
id = "lmstudio"
kind = "lm_studio"                 # or "open_ai_compatible"
label = "LM Studio (this machine)"
base_url = "http://127.0.0.1:1234"
enabled = true
# credential_ref = "keyring:work/lmstudio"
```

Recipes live one directory over, one file per model-plus-prompt:

```text
profiles/work/
  config.toml
  providers.toml
  agents.toml
  extensions.toml                  # activated extensions (install is global)
  recipes/corpus-scraper.toml      # see Flight test → Recipes
```

## The ACP bridge

Global, because it is a listener rather than an identity. Off unless enabled, and
`profiles` is the fence around what a paired remote driver can reach.

```toml
# ~/.config/acpilot/config.toml
[acp.bridge]
enabled = false                    # the default, and the recommendation
bind = "127.0.0.1:7788"
require_token = true
profiles = ["work"]
rate_limit_per_minute = 120
```

<Aside type="note" title="One socket, one identity list">
Binding a second port per profile would be the obvious alternative and the wrong
one: it multiplies listeners, certificates, and firewall rules. One endpoint,
an explicit profile allowlist, and a `404` for anything outside it — the same
answer a local client gets.
</Aside>

<Aside type="note" title="Unknown keys are an error, not a shrug">
A typo in a config file should be loud. `acpilot doctor` lists every unknown key
with the file and line that produced it, and the daemon refuses to start with a
config it cannot fully understand — a setting that silently does nothing is worse
than no setting.
</Aside>

## Precedence, not guessing

```bash
acpilot profile diff work personal
```

Prints the effective configuration with the winning file for each key. When the
answer to "why is it doing that?" is a config file three layers deep, this is the
command that ends the argument.

## Environment variables

| Variable | Effect |
| --- | --- |
| `ACPILOT_PROFILE` | Selects the profile (after `--profile`, before the marker file) |
| `ACPILOT_ADDR` | Daemon bind address (used by `acpilotd` today) |
| `ACPILOT_LOG` | `tracing` filter override |
| `RUST_LOG` | Fallback for `ACPILOT_LOG` |
| `ACPILOT_BRIDGE_TOKEN` | Read once at startup for the ACP bridge when `require_token` is set and the keyring is unavailable (containers). Never echoed by `doctor`. |

Secrets are never read from the environment for a profile — see
[Secrets](../../safety/secrets/).
