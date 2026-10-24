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
| Node 20+ (docs only) | Only if you want to build this site |

Platform support is Linux, macOS, and Windows. macOS and Linux are exercised on
every push; Windows is too, which is how the line-ending bug got caught before a
user found it.

## Build it

```bash
git clone https://github.com/skamansam/acpilot.git
cd acpilot
cargo build            # the daemon, the CLI, and the workspace crates
cargo xtask check      # what CI runs: fmt, clippy, tests
```

`rustup` reads `rust-toolchain.toml`, so the pinned compiler and components
install themselves on the first `cargo` command.

## Run what exists today

The terminal UI starts the daemon itself, so this is one command rather than a
terminal and a flag. `cargo build` above built `acpilotd` next to `acpilot`, which
is where the client looks for it.

```bash
cargo run -p acpilot-cli -- tui
```

The daemon answers on `127.0.0.1:7777` whether or not the TUI is open, so its
contract is also readable with `curl`:

```bash
curl -s localhost:7777/v1/health
# {"ok":true,"service":"acpilotd","version":"0.1.0","api":"/v1","workspace":"acpilot"}

cargo run -p acpilotd --features docs   # by hand, then open http://127.0.0.1:7777/docs
```

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
