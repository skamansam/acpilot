---
title: CLI
description: A typed client for the daemon, designed so scripts can fly too.
---

<span class="pill pill--boarding">boarding</span> &nbsp; `acpilot version`, the filesystem `ext` verbs, and `secret` exist today

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
acpilot models probe <provider> | ls
acpilot recipes ls|show|new|run|rm|promote
acpilot runs ls|show|replay|compare
acpilot acp pair|token show|token rotate|drivers|revoke
acpilot ext add --from <dir> | import-theme --from <file> --id <publisher.name>
acpilot ext verify --all | prune [--dry-run]
acpilot ext activate|deactivate <id> --profile <name>
acpilot ext why <preset> --profile <name>
acpilot secret set|list|rm [--profile <name>]
acpilot tui [--api-url <url>] [--no-spawn] [--profile <dir>] [--workspace <dir>]
acpilot serve [--http]
acpilot doctor
```

<Aside type="note" title="`doctor` earns its place">
`acpilot doctor` prints the resolved profile, its roots, which config file won
each key, and every agent whose command is missing. It is the first thing to run
when something is wrong, and the first command worth building well.
</Aside>

A fresh install launches with the shipped presets. The first use of a profile
writes that profile's `agents.toml` — starting with its `$schema` line — once,
and never over a file that already exists; `doctor` then answers what a machine is
missing.

## What it looks like today

`acpilot tui` opens the terminal UI against the daemon, **starting it first if
nothing is listening** (ADR 0004): the session list, the open session's transcript
as it streams, and a permission card that answers with `y`, `a` or `n`. It is a
client of the same API as everything else, so the editor phase (P6) grows this
window rather than a second engine.

```bash
$ acpilot tui                       # the first client starts the daemon
$ acpilot tui --no-spawn            # fail instead: tests, CI, a service manager
$ acpilot tui --profile ~/.config/acpilot/profiles/work --workspace ~/code/thing
```

The order is probe, then spawn: a daemon that answers `GET /v1/health` is used as
it is, and only silence starts one. The spawned daemon is detached from the
terminal's process group — closing the window that happened to start it must not
end every session — with its output in `$XDG_RUNTIME_DIR/acpilot/daemon-*.log`.
Two windows opening at once take a lock rather than racing, and a daemon whose
version differs from the client is reported with both versions instead of being
restarted, because it may be mid-turn for someone else. `acpilotd` is looked for
next to the `acpilot` binary, never on `PATH`; `ACPILOT_DAEMON` names it when an
installer put the two apart.

`--profile` and `--workspace` are the daemon's flags, so they only apply when
this command starts it. A daemon that is already running keeps the ones it was
started with, and naming a different profile or workspace is exit `4`: showing
one project's sessions under another project's name is worse than an error.

Scripting the lab is a first-class case, not a workaround: a corpus experiment
that cannot be run in CI is a corpus experiment nobody repeats.

```bash
acpilot recipes run corpus-scraper "list the undocumented flags" --json \
  | jq '{stop: .stop_reason, ttft: .first_token_ms, warnings}'
```

Credentials are the one thing the CLI owns without the daemon: a machine with a
locked keyring or a stopped daemon still needs a way to repair its secrets.
There is no `--value` flag, because an argument is visible in shell history and
in `ps`; the value comes from stdin.

```bash
$ printf %s "$ANTHROPIC_API_KEY" | acpilot secret set anthropic --profile work
stored work/anthropic in the acpilot keyring

$ acpilot secret list --profile work
present  work/anthropic
missing  work/openai

$ acpilot secret rm anthropic --profile work
removed work/anthropic from the acpilot keyring
```

`list` reads the profile's `agents.toml` to know which entries should exist — the
keyring has no portable way to enumerate — and reports presence, never a value.
Removing an entry that is not there exits `3`, so a script can tell the
difference.

```bash
$ acpilot ext verify --all
ok      acpilot.paper@1.0.0

$ acpilot ext why rust-analyzer --profile work
language server rust-analyzer
  command     rust-analyzer
  args        []
  languages   rust
  from        built-in acpilot.language-servers 1.0.0   (wins)

$ acpilot version
acpilot 0.1.0
api      /v1

$ acpilot
Edit and run ACP agents over one daemon

Usage: acpilot [COMMAND]

$ echo $?
2
```
