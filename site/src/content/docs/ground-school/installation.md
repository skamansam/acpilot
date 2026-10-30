---
title: Installation
description: Build Acpilot from source today; packages arrive with v0.1.
---

<span class="pill pill--boarding">boarding</span>

There are no installers yet. Everything below builds from source, which is the
honest instruction while the aircraft is still in the hangar.

<Aside type="caution" title="No packages, no apologies">
`brew install acpilot` does not exist. Neither does the MSI, the `.deb`, or the
AppImage. They arrive in the release phase (P12), after there is something worth packaging.
</Aside>

## Requirements

| Requirement | Why |
| --- | --- |
| Rust **1.95+** | Floor set by `rusqlite_migration`; the repo pins 1.97 in `rust-toolchain.toml` |
| Git | Workspace crates, and one dependency is a rev-pinned fork |
| An ACP agent (optional) | Nothing to fly yet; the engine phase (P2) brings the first real session |
| Node 20+ (docs and web UI) | Only if you want to build this site or `web/` |

Platform support is Linux, macOS, and Windows. macOS and Linux are exercised on
every push; Windows is too, which is how the line-ending bug got caught before a
user found it.

## Build it

```bash
git clone https://github.com/skamansam/acpilot.git
cd acpilot
cargo build            # the daemon, the CLI, and the workspace crates
cargo xtask check      # what CI runs: fmt, clippy, tests, the shell, the web UI
```

`rustup` reads `rust-toolchain.toml`, so the pinned compiler and components
install themselves on the first `cargo` command.

That build does **not** include the desktop app: its `gui` feature is off by
default, because the default gate should not compile a graphics stack. Ask for it
when you want the window:

```bash
cargo build -p acpilot-gui --features gui
```

On Linux that links gpui's X11/Wayland backends and needs their dev packages; the
list is in `CONTRIBUTING.md`. `cargo xtask check` builds that feature too — a
shell that stops compiling is otherwise found by CI and nowhere else — and it runs
the web UI's checks, so it also wants Node 22 and npm. `cargo xtask check --no-web`
is the Rust-only loop.

Release packaging is `cargo-dist`'s (`dist-workspace.toml`, P12-1), which makes one
archive per program per platform: `acpilot`, `acpilotd` and `acpilot-gui`. The
installers put them in the same directory, which matters for the first two — the
CLI looks for the daemon beside its own executable rather than on `PATH`, so
installing `acpilot` without `acpilotd` gives you a client with nothing to talk to.

## Run what exists today

The terminal UI starts the daemon itself, so this is one command rather than a
terminal and a flag. `cargo build` above built `acpilotd` next to `acpilot`, which
is where the client looks for it.

```bash
cargo run -p acpilot-cli -- tui
```

The daemon listens on a unix socket in `$XDG_RUNTIME_DIR/acpilot` (`$TMPDIR` on
macOS), so its contract is also readable with `curl` — the `--unix-socket` flag is
the whole difference from a URL:

```bash
curl -s --unix-socket "$XDG_RUNTIME_DIR/acpilot/acpilot.sock" http://localhost/v1/health
# {"ok":true,"service":"acpilotd","version":"0.1.0","api":"/v1","workspace":"acpilot"}

# A port instead, for a client that is not on this machine's filesystem:
cargo run -p acpilotd --features docs -- --addr 127.0.0.1:7777
# every request needs the token the daemon wrote beside the socket:
curl -s http://127.0.0.1:7777/v1/health \
  -H "Authorization: Bearer $(cat "$XDG_RUNTIME_DIR/acpilot/token")"
```

The socket's directory is `0700` and the socket `0600`, which is what lets a local
request go without a token. A TCP bind inherits neither, so it requires
`Authorization: Bearer <token>` on every request: a `0600` file beside the socket,
generated on first use and logged by path at startup. `acpilot tui --token <token>`
names one explicitly (or `ACPILOT_TOKEN`); without either it reads that file, which
is what makes a local port work with no typing. A CI job or a container talking to
a daemon it has no runtime directory for is the case the flag exists for.

If that token may have been seen, `acpilot token rotate` replaces it: the daemon
reads the file per request, so the new one applies on the next call, nothing
restarts, and every client holding the old one is refused.

The docs page at `/docs` is the one route that answers without a token — a browser
cannot attach a header to a typed address, and that page is where the token gets
pasted. It asks for one in a field, fetches the contract with it, and sends it on
every "Try it out" call. The daemon logs the token's path when it binds a port.

A daemon started by the client is detached from the terminal, and `--no-spawn`
turns the behaviour off when something else should own the process.

<Aside type="note" title="Why the docs page is behind a feature">
Swagger UI pulls its assets from a CDN. That is fine on a developer machine and
rude to assume in a release build, so the route only exists when you ask for it.
</Aside>

## The CLI, such as it is

```bash
cargo run -p acpilot-cli -- version
# acpilot 0.1.0
# api      /v1
```

Running `acpilot` with no arguments prints usage and exits `2`, because that is
what "you did not tell me what to do" should sound like to a script.

## Uninstall

```bash
rm -rf acpilot          # there is no installer, so there is nothing to uninstall
```

If that felt too easy, it is because nothing has written outside its own
directory. When the daemon starts keeping state, this section gets real
instructions and a `--purge` flag.
