# 10. An install says how it was installed, and only some may replace themselves

**Status:** accepted, 2026-11-04

`GET /v1/health` reports the daemon's version, and `acpilot-client` refuses to talk
to a daemon whose version differs. That refusal is correct — a daemon may be mid-turn
— but it dead-ends: the person is told two version strings and "restart the daemon",
without being told *how* to get the two halves back into step. The answer differs by
installation method, and until now nothing recorded it. `cargo-dist` will ship tarballs,
a shell installer, a PowerShell installer and a Homebrew formula (P12-1), a distribution
may also package the binaries itself, and a developer has `cargo install` or a checkout.
A daemon that suggests `sudo apt install acpilot` to somebody who used the tarball has
made the problem worse.

## Decision

### D1 — every installation writes a marker file

An installation that is not a raw binary drop writes `install.json` describing itself:

```json
{ "method": "apt", "package": "acpilot", "version": "0.4.0" }
```

- `method` is one of `apt`, `dnf`, `pacman`, `brew`, `msi`, `script`, `tarball`,
  `cargo`, `appimage`. It is a closed set: an unknown string is treated as `unknown`,
  never as permission.
- `version` is the version the installer put there, so a marker left behind by an
  older package can be recognised as stale rather than trusted.
- `package` and any other fields are informational. Nothing branches on them.

**The marker is shipped by the package, not written by the program.** That is the
whole point: a file in the `.deb` says "this came from apt" because apt put it there.
A binary that guesses its own installation from `PATH` or from the directory it sits
in guesses wrong for exactly the installations that matter — the ones a package manager
owns.

### D2 — where the marker is looked for, in this order

1. Beside the binary: `<dir of the running executable>/install.json`, which covers a
   tarball, an AppImage and a `cargo install` (whose `~/.cargo/bin` is user-owned).
2. A shared prefix (unix): `/usr/local/share/acpilot/install.json`, then
   `/usr/share/acpilot/install.json`. A system package ships the second.
3. Per-user state: `$XDG_DATA_HOME/acpilot/install.json`, else
   `~/.local/share/acpilot/install.json`, for the shell and PowerShell installers.

First match wins. Nothing is merged: two markers is two installations disagreeing
about one binary, and picking the more permissive one is how a daemon edits a file
that a package manager will overwrite on the next upgrade.

No marker means `unknown`, and `unknown` is treated as *not yours to replace*.

### D3 — ownership decides who may replace the binary

| method | who updates |
| --- | --- |
| `apt`, `dnf`, `pacman`, `brew`, `msi` | the package manager |
| `script`, `tarball`, `appimage`, `cargo` | the program |
| `unknown` | nobody: the client reports and stops |

A package manager owns its files. Replacing `/usr/bin/acpilotd` behind dpkg leaves
the database describing a version that is not on disk, and the next `apt upgrade`
either reverts it or fails the checksum. So for those methods the product **prints the
command** — `sudo apt install --only-upgrade acpilot` — and never downloads anything.

For the self-owned methods, the update is: download the release artifact for this
platform, verify its published checksum, write it to a temporary file in the same
directory, `chmod 755`, `rename` over the old path (atomic on both platforms), then
restart the daemon if one is running.

### D4 — what the version mismatch reports

`GET /v1/health` gains `install` when a marker was found:

```json
{ "ok": true, "service": "acpilotd", "version": "0.4.0", "api": "/v1",
  "install": { "method": "apt", "version": "0.4.0", "replaces": "package-manager" } }
```

`replaces` is `package-manager`, `self` or `nobody`, so a client does not have to
carry a copy of the table in D3 to know whether it may offer an in-place upgrade.
When a client's `check_version` fails, its error message becomes actionable: it names
the method, both versions, and the one command that reconciles them.

### D5 — a mismatch is still a refusal

Nothing here makes mixed versions *work*. The daemon does not serve an older client,
and the client does not downgrade the daemon silently. What changes is that the
refusal now ends with a step the person can take, chosen by the installation that is
actually present. That keeps ADR 0004's rule — one version, one process — intact.

## Consequences

- **The marker can lie**, if a user moves a binary out of its installation. The
  consequence is bounded: a self-owned marker on a root-owned binary makes an update
  attempt fail at `rename`, which is a permission error and not a corrupted install.
- **`cargo install` needs the marker written by hand**, since cargo has no hooks. The
  `xtask`/installer path writes it; a bare `cargo install` is `unknown` and is treated
  as not replaceable, which is safe and slightly unhelpful.
- **A distribution that invents a method** gets `unknown`, and is asked to extend the
  closed set rather than to pass an arbitrary string through.
- **Two binaries, one install**: `acpilot`, `acpilotd` and `acpilot-gui` come from one
  archive per target (P12-1), so one marker beside them describes all three.
- **Self-update is a new network path**, and is deliberately limited to a downloaded
  release with a verified checksum. No `curl | sh`, no unsigned artifact, no update of
  anything the daemon did not ask for.
