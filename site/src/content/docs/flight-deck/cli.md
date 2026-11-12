---
title: CLI
description: A typed client for the daemon, designed so scripts can fly too.
---

<span class="pill pill--boarding">boarding</span> &nbsp; `acpilot version`, the filesystem `ext` verbs, and `secret` exist today

The CLI is not a second implementation of the product. It is a client of the
daemon API with a nicer surface. Anything the TUI can do, a script can do. Neither
can invent behaviour the daemon does not have.

## Design rules

1. **No domain logic.** `acpilot` deserialises into `acpilot-proto` types and
   calls the API. If you find an `if` here that decides policy, it is in the wrong
   crate.
2. **Scriptable by default.** Every read command has `--json`. Exit codes are
   fixed and documented, because `if acpilot` is a legitimate way to use this.
3. **`--profile` everywhere.** Accepted before or after the subcommand. There is
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
parsing English. This table is a promise. Changing it is a breaking change.

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
acpilot [--profile <name|dir>] <command>
acpilot ext activate|deactivate <id>
acpilot ext why <preset>
acpilot secret set|list|rm
acpilot token rotate
acpilot config explain <key> [PATH]
acpilot tui [PATH] [--api-url <url|path>] [--token <token>] [--no-spawn] [--workspace <dir>] [--no-mouse]
acpilot serve [--bind <addr>] [--token <token>] [--workspace <dir>]
acpilot doctor [PATH] [--api-url <url|path>] [--token <token>]
```

<Aside type="note" title="`doctor` earns its place">
`acpilot doctor` prints the resolved profile, its roots, which config file won
each key, and every agent whose command is missing. It also asks the first
declared local agent to `initialize`, so the report says whether it speaks ACP,
not only whether its command exists. Run it first when something is wrong. It is
the first command worth building well.
</Aside>

```bash
$ acpilot doctor
acpilot 0.1.0 · api /v1
config     /home/sam/.config/acpilot
profile    default · /home/sam/.config/acpilot/profiles/default · not created yet (not required)
workspace  /home/sam/code/thing
project    none · temporary root, nothing is written
daemon     /run/user/1000/acpilot/acpilot.sock · running · 0.1.0

Agents
  declared   none · the editor works without one
  on PATH    claude, codex
  missing    gemini, freebuff

Language servers
  rust         rust-analyzer                  installed
  python       pyright-langserver             missing
  typescript   typescript-language-server     installed
  javascript   typescript-language-server    installed
```

When the profile exists, its five roots follow the profile line. Each is marked
`present` or `missing`. A settings key this build does not know is listed under
them as `unknown`, with the file that set it.

A machine with nothing set up exits `0`. That is the first-run state, not a
fault. The only failure is a declaration that cannot run. That includes a profile
without a `config.toml`, or a manifest that does not parse. It also includes an
agent whose command is not on `PATH`, or a declared agent that never answers the
handshake. `doctor` starts no
daemon and writes no file, so it is safe on a machine that has never run Acpilot.

The daemon line is a probe, not a start. `doctor` asks `GET /v1/health` at the
endpoint a `tui` start would use: the default, `ACPILOT_API_URL`, or `--api-url`.
It uses `--token` or the token file a local daemon wrote. It starts nothing. A
daemon that is not running is reported as unreachable rather than as a fault,
because the TUI starts one on demand. A `401` is reported as a refused token.
That proves something is listening, and it points at the token instead of at a
missing daemon.

When an agent is declared, its handshake appears under **Agents**:

```
Agents
  declared   Claude · claude
  handshake  Claude · Claude Agent 0.94.0 · ACP V1
```

That is the one process `doctor` starts: ACP `initialize`, with no workspace, no
conversation and no prompt. It ends when the answer arrives or after ten seconds.
A program finds its way onto `PATH` more easily than it earns that answer. That
is why the report separates the two.

A fresh install needs no profile and no `agents.toml`. With no agents file the
daemon serves the workspace with no agents, so the editor is usable immediately.
The file appears only when you declare one. `doctor` then answers what a machine
is missing.

## What it looks like today

`acpilot tui` opens the terminal UI against the daemon and **starts it first if
nothing is listening** (ADR 0004). It shows the session list, the open session's
transcript as it streams, and a permission card that answers with `y`, `a` or
`n`. It is a client of the same API as everything else. The editor phase (P6)
therefore grows this window rather than a second engine.

```bash
$ acpilot tui                       # attach to a daemon, or host the core in this process
$ acpilot tui --no-spawn            # attach or fail: tests, CI, a service manager
$ acpilot tui --profile work --workspace ~/code/thing
$ acpilot tui --no-mouse            # the terminal keeps its own drag-to-select
```

```bash
$ acpilot serve                        # a free loopback port, printed with the token
$ acpilot serve --bind 127.0.0.1:7777  # a fixed port
$ acpilot serve --bind 0.0.0.0:7777    # the same over the LAN; the token guards every call
```

```
┌  acpilot 0.1.0 · ready in 3 ms
│
│  Local       http://127.0.0.1:54321/
│  Token       8f5b9d0c2a7e41f3b6c8d9e0f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4
│  Token file  /run/user/1000/acpilot/token
│
│  Open the page and paste the token when it asks.
└  Ctrl-C to stop
```

The OS chooses the port unless `--bind` names one. The token is generated on
first use and reused unless `--token <token>` supplies one. It is written to the
daemon's runtime directory, and `acpilot token rotate` replaces it without
restarting anything.

Mouse reporting is on. The menu bar, the dividers, a click that lands the caret,
a drag that selects and the wheel all need it. It costs the terminal its own
drag-to-select, though most terminals keep that on Shift-drag. `--no-mouse` turns
reporting off and leaves the gesture to the terminal. Nothing is lost but the
pointer: every mouse action has a key, and `F2` copy mode is the built-in answer
for reading a terminal's output. A drag held against the top or bottom of the
editor or the terminal scrolls past its edge and keeps selecting. The widths the
dividers were dragged to are remembered for the next start in `tui-layout.toml`
under the data directory. They are not in a settings file, which is the person's
to edit.

The default endpoint is the daemon's unix socket in `$XDG_RUNTIME_DIR/acpilot`
(`$TMPDIR` on macOS), or loopback HTTP on Windows until the named pipe exists.
`--api-url` takes either a URL or a socket path. `ACPILOT_API_URL` takes the same
value. One string, because to a user "where the daemon is" is one place, not two
transports. The socket's directory is `0700` and the socket `0600`. That is the
whole access-control story for a local daemon. A URL has no such story, so a
daemon on a port requires a token. `--token` (or `ACPILOT_TOKEN`) names one.
Without either, the client uses the file the daemon wrote beside its socket, so a
local port needs no typing. The client receives the token rather than looking it
up. That is why a `401` is reported as a refused token naming that file, not as
"no daemon". The second message would send you to start another daemon on the
same port. `acpilot token rotate` rewrites that file. The daemon reads it per
request, so the new token applies on the next call. Every client holding the old
one is refused, with nothing restarting. Both native clients resolve that file
themselves. `acpilot tui` and `acpilot-gui` read it when `--token` (or
`ACPILOT_TOKEN`) is absent, so a window or terminal opened against `acpilot
serve` on this machine needs only its URL.

`acpilot config explain <key>` reads the same cascade a start does. It prints the
winning value with its origin. The origins are the defaults, the global file, and
a profile's `config.toml` or `profile.toml`. They also include the project's
`.acpilot/config.toml`, an `ACPILOT_*` variable, or a flag. A bare key is a
preference, so `tab_width` means `preferences.tab_width`.

The order is probe, then spawn. A daemon that answers `GET /v1/health` is used as
it is. Only silence starts one. The spawned daemon is detached from the
terminal's process group, because closing the window that happened to start it
must not end every session. Its output goes to
`$XDG_RUNTIME_DIR/acpilot/daemon-*.log`. Two windows opening at once take a lock
rather than racing. A daemon whose version differs from the client is reported
with both versions instead of being restarted, because it may be mid-turn for
someone else. `acpilotd` is looked for next to the `acpilot` binary, never on
`PATH`. `ACPILOT_DAEMON` names it when an installer put the two apart.

`PATH` is the usual way to say which project this is. A directory is the
workspace as named. A file climbs its parents to the nearest
`.acpilot/config.toml`. A file with no project above it becomes a temporary
project in its own directory, and nothing is written until project settings are
saved. The profile comes from that project's `profile` key, so the common start
is `acpilot tui` in the tree you are working in, with no flags at all.

`--profile` and `--workspace` still exist and still win when given. They are the
daemon's flags, so they only apply when this command starts it. A daemon that is
already running keeps the ones it started with. Naming a different profile or
workspace is exit `4`. Showing one project's sessions under another project's
name is worse than an error.

Scripting the lab is a first-class case, not a workaround. A corpus experiment
that cannot run in CI is a corpus experiment nobody repeats.

```bash
acpilot recipes run corpus-scraper "list the undocumented flags" --json \
  | jq '{stop: .stop_reason, ttft: .first_token_ms, warnings}'
```

Credentials are the one thing the CLI owns without the daemon. A machine with a
locked keyring or a stopped daemon still needs a way to recover its secrets. There
is no `--value` flag, because an argument is visible in shell history and in
`ps`. The value comes from stdin.

```bash
$ printf %s "$ANTHROPIC_API_KEY" | acpilot secret set anthropic --profile work
stored work/anthropic in the acpilot keyring

$ acpilot secret list --profile work
present  work/anthropic
missing  work/openai

$ acpilot secret rm anthropic --profile work
removed work/anthropic from the acpilot keyring
```

`list` reads the profile's `agents.toml` to know which entries should exist. The
keyring has no portable way to enumerate. It reports presence, never a value.
Deleting an entry that is not there exits `3`, so a script can tell the
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
