---
title: Installation
description: Build Acpilot from source today. Packages arrive with v0.1.
---

<span class="pill pill--boarding">boarding</span>

There are no installers yet. Everything below builds from source. That is the
honest instruction while the aircraft is still in the hangar.

<Aside type="caution" title="No packages, no apologies">
`brew install acpilot` does not exist. Neither does the MSI, the `.deb`, or the
AppImage. They arrive in the release phase (P12), after there is something worth
packaging.
</Aside>

## Requirements

| Requirement | Why |
| --- | --- |
| Rust **1.95+** | Floor set by `rusqlite_migration`. The repo pins 1.97 in `rust-toolchain.toml` |
| Git | Workspace crates, and one dependency is a rev-pinned fork |
| An ACP agent (optional) | Nothing to fly yet. The engine phase (P2) brings the first real session |
| Node 20+ (docs and web UI) | Only if you want to build this site or `web/` |

Platform support is Linux, macOS, and Windows. CI exercises macOS and Linux on
every push. It exercises Windows too, which is how the line-ending bug got caught
before a user found it.

## Build it

```bash
git clone https://github.com/skamansam/acpilot.git
cd acpilot
cargo build            # the daemon, the CLI, and the workspace crates
cargo xtask check      # what CI runs: fmt, clippy, tests, the shell, the web UI
```

`rustup` reads `rust-toolchain.toml`, so the pinned compiler and components
install themselves on the first `cargo` command.

That build does **not** include the desktop app. Its `gui` feature is off by
default, because the default gate should not compile a graphics stack. Ask for it
when you want the window:

```bash
cargo build -p acpilot-gui --features gui
```

On Linux that links gpui's X11/Wayland backends and needs their dev packages. The
list is in `CONTRIBUTING.md`. `cargo xtask check` builds that feature too, so a
shell that stops compiling is caught before CI. It also runs the web UI's checks,
so it wants Node 22 and npm. `cargo xtask check --no-web` is the Rust-only loop.

Release packaging is `cargo-dist`'s job (`dist-workspace.toml`, P12-1). It makes
one archive per program per platform: `acpilot`, `acpilotd`, and `acpilot-gui`.
The installers put them in the same directory. That matters for the first two.
The CLI looks for the daemon beside its own executable rather than on `PATH`, so
installing `acpilot` without `acpilotd` gives you a client with nothing to talk
to.

## Run what exists today

The terminal UI needs no daemon. When nothing answers `GET /v1/health`, it runs
the daemon's own core inside its own process (P6-11). This is one command rather
than a terminal and a flag. That mode's sessions end when you quit. Run
`acpilotd` yourself when they should outlive the window. The `cargo build` above
built it next to `acpilot`. The client always attaches to a daemon that is
running.

```bash
cargo run -p acpilot-cli -- tui
```

The daemon listens on a unix socket in `$XDG_RUNTIME_DIR/acpilot` (`$TMPDIR` on
macOS). Its contract is also readable with `curl`. The `--unix-socket` flag is
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

The socket's directory is `0700` and the socket is `0600`. That is what lets a
local request go without a token. A TCP bind inherits neither, so it requires
`Authorization: Bearer <token>` on every request. The token is a `0600` file
beside the socket. The daemon generates it on first use and logs its path at
startup. `acpilot tui --token <token>` names one explicitly (or `ACPILOT_TOKEN`).
Without either, the client reads that file, which is what makes a local port work
with no typing. A CI job or a container talking to a daemon with no runtime
directory is the case the flag exists for.

If someone may have seen that token, `acpilot token rotate` replaces it. The
 daemon reads the file per request, so the new one applies on the next call.
Nothing restarts, and the daemon refuses every client holding the old one.

The docs page at `/docs` is the one route that answers without a token. A browser
cannot attach a header to a typed address, and that page is where you paste the
token. It asks for one in a field, loads the contract with it, and sends it on
every "Try it out" call. The daemon logs the token's path when it binds a port.

`--no-spawn` means "attach to a running daemon or fail". A test, CI job, or
service manager wants that. Without it, an unanswered probe starts a daemon in
the client's process rather than returning an error.

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

Running `acpilot` with no arguments prints usage and exits `2`. That is what "you
did not tell me what to do" should sound like to a script.

## Uninstall

```bash
rm -rf acpilot          # there is no installer, so there is nothing to uninstall
```

If that felt too easy, it is because nothing wrote outside its own directory.
When the daemon starts keeping state, this section gets real
instructions and a `--purge` flag.
