---
title: Configuration
description: One cascade, per-key merging, and a command that tells you which file won.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P4 &nbsp;·&nbsp; this shape may still change

Configuration is layered, and every layer can override a single key rather than a
whole file. The last word wins:

```text
defaults.toml
  <  config.toml                       (global)
  <  profiles/<name>/config.toml       (per profile)
  <  <workspace>/.acpilot/config.toml  (per workspace)
  <  ACPILOT_* environment variables
  <  command-line flags
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
theme = "night-flight"
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

Secrets are never read from the environment for a profile — see
[Secrets](../../safety/secrets/).
