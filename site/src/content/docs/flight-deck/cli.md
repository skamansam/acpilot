---
title: CLI
description: A typed client for the daemon, designed so scripts can fly too.
---

<span class="pill pill--boarding">boarding</span> &nbsp; only `acpilot version` exists today

The CLI is not a second implementation of the product. It is a client of the
daemon API with a nicer surface, which means anything the TUI can do, a script
can do — and neither of them can invent behaviour the daemon does not have.

## Design rules

1. **No domain logic.** `acpilot` deserialises into `acpilot-proto` types and
   calls the API. If you find an `if` here that decides policy, it is in the wrong
   crate.
2. **Scriptable by default.** Every read command has `--json`. Exit codes are
   fixed and documented, because `if acpilot` is a legitimate way to use this.
3. **`--profile` everywhere.** Accepted before or after the subcommand; there is
   no "global mode" where it silently does not apply.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | It worked |
| `1` | It failed, no better classification |
| `2` | Bad invocation — unknown flag, malformed argument (from the parser) |
| `3` | Not found: workspace, session, agent, file |
| `4` | Conflict: stale ETag, duplicate turn, closed session |
| `5` | Permission denied — by policy or by a human |

Codes `3`–`5` exist so a script can tell "wrong path" from "not allowed" without
parsing English. This table is a promise; changing it is a breaking change.

## Planned surface

```text
acpilot host add|ls|test|rm
acpilot workspace open|ls|rm
acpilot agents list|refresh|show
acpilot session new|ls|show|rm|fork
acpilot prompt <text> [--session <id|@alias>] [--file <path>] [--follow]
acpilot watch <session>
acpilot permissions ls|allow|deny|pending
acpilot files ls|cat|write
acpilot term attach
acpilot profile new|ls|show|use|rm|export|import|diff
acpilot serve [--http]
acpilot doctor
acpilot setup
```

<Aside type="note" title="`doctor` earns its place">
`acpilot doctor` prints the resolved profile, its roots, which config file won
each key, and every agent whose command is missing. It is the first thing to run
when something is wrong, and the first command worth building well.
</Aside>

## What it looks like today

```bash
$ acpilot version
acpilot 0.1.0
api      /v1

$ acpilot
Edit and run ACP agents over one daemon

Usage: acpilot [COMMAND]

$ echo $?
2
```
