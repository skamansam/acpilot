---
title: Updating
description: How this copy was installed, who may replace it, and the one command that does.
---

<span class="pill pill--flying">flying</span> &nbsp; `acpilot update`, `--check`, `--from`

A client refuses to talk to a daemon of a different version (ADR 0004), which is
correct and, on its own, useless: you are told two version strings and nothing
about what to do. What to do depends on how acpilot arrived on the machine. A
copy `apt` unpacked is `apt`'s to upgrade. A tarball somebody unzipped is theirs.
Nothing in a binary can tell those apart, so the installation says so itself.

## The marker

Every installation except a bare binary drop leaves one small file called
`install.json`:

```json
{ "method": "tarball", "version": "0.2.6" }
```

`method` is the closed set `apt`, `dnf`, `pacman`, `brew`, `msi`, `script`,
`tarball`, `appimage`, `cargo`. `version` is what the installer put there, so a
marker left by an older package is recognisable rather than trusted. Any other
field is informational; nothing branches on it.

The marker is written by the package, not by the program. That is the whole
point: a file in the `.deb` says "this came from apt" because apt put it there,
and a binary that guessed from where it sits would guess wrong for exactly the
installations that matter.

`acpilot update` looks in this order, and the first answer wins:

| Where | Which installation |
| --- | --- |
| `<directory of the running executable>/install.json` | tarball, AppImage, `cargo install`, the Homebrew keg |
| `/usr/local/share/acpilot/install.json` | a package installed under `/usr/local` |
| `/usr/share/acpilot/install.json` | a system package |
| `$XDG_DATA_HOME/acpilot/install.json` (else `~/.local/share/acpilot`) | the shell and PowerShell installers |

Two markers is two installations disagreeing about one binary, so they are never
merged. Picking the more permissive one is how a program edits a file that a
package manager overwrites on its next upgrade.

## Who may replace it

```bash
$ acpilot update --check
acpilot 0.2.6 at /usr/local/bin/acpilot
installed by tarball, 0.2.6
this copy is yours to replace, because tarball is not a package manager:
  acpilot update
  acpilot update --from <a new acpilot binary>, for a machine that cannot download
```

| `method` | Who upgrades it |
| --- | --- |
| `apt`, `dnf`, `pacman`, `brew`, `msi` | the package manager, and only it |
| `script`, `tarball`, `appimage`, `cargo` | `acpilot update` |
| no marker, or a method this build does not know | nobody: it reports and stops |

A package-managed copy is never written by this command, even with `--from`. The
package manager would put its own file back on the next upgrade, at best, or fail
its checksum, at worst. It prints the command that does own the files instead:

```bash
$ acpilot update
acpilot 0.2.6 at /opt/homebrew/bin/acpilot
installed by brew, 0.2.6
brew owns these files, so it is the one to upgrade them:
  brew upgrade acpilot
acpilot update: brew owns this copy, so nothing was replaced; upgrading it here would be undone by its next upgrade
$ echo $?
1
```

The Homebrew formula writes `{"method":"brew"}` in its own install step, so a keg
copy reports `brew` and not the `tarball` its archive says.

## Updating a copy you own

```bash
$ acpilot update
acpilot 0.2.5 at /usr/local/bin/acpilot
installed by tarball, 0.2.5
this copy is yours to replace, because tarball is not a package manager:
  acpilot update
  acpilot update --from <a new acpilot binary>, for a machine that cannot download
downloading https://github.com/skamansam/acpilot/releases/download/v0.2.6/acpilot-0.2.6-linux-x86_64.tar.gz
installed acpilot 0.2.6 from v0.2.6: replaced acpilotd, acpilot at /usr/local/bin/acpilot
a daemon that is already running keeps the older binary until it is restarted
```

That is four steps, and each one can stop the update:

1. **The asset for this platform** is named from the release (`linux-x86_64`,
   `macos-aarch64`, `windows-x86_64`, …). A platform the release does not package
   says so instead of downloading a 404 page.
2. **The bytes are checked** against the `.sha256` the release published beside
   the archive. A digest that differs ends the update and nothing is written.
   There is no `curl | sh` and no unsigned artifact here.
3. **The archive is unpacked** into a temporary directory, and only the program
   names are read from it — never an entry's path, so an archive that contains
   `../../etc/passwd` has nothing this command wants.
4. **The programs are replaced** one at a time, each written beside its
   destination, made executable, and renamed over it. A failure halfway therefore
   leaves whole programs, not a half-written one. `acpilotd` and `acpilot-gui` are
   replaced only where they already exist: an update does not change which
   programs arrived, and `acpilot` itself is written last so a failure leaves you
   a client you can still run.

The marker is then rewritten with the version that is now on disk, keeping the
method. The bytes changed; the way they arrived did not.

| Flag | What it does |
| --- | --- |
| `--check` | Print the provenance and download nothing |
| `--version <TAG>` | Install a named release (`v0.2.6` or `0.2.6`) instead of the latest |
| `--from <PATH>` | Replace this `acpilot` with a binary you brought yourself |

Exit codes are the CLI's usual ones: `0` when the copy is already the requested
version or was replaced, `1` when it was not (a package manager owns it, no marker
says it may be replaced, or a download or write failed), `2` for a bad invocation.

## What it does not do

- **It does not restart a daemon.** A running `acpilotd` keeps the binary it
  started from until it is restarted; the command says so rather than killing a
  daemon that may be mid-turn.
- **It does not verify a signature**, only the published sha256. Both travel over
  the same TLS connection, so the checksum catches a truncated or substituted
  download, not a compromised release.
- **It does not update a copy with no marker.** Move the binary out of its
  installation and it becomes `unknown`, which is treated as not yours to replace.
  Reinstall with an installer, or write the marker yourself:

  ```bash
  printf '{"method":"tarball","version":"0.2.6"}' > /usr/local/bin/install.json
  ```

## The short version

```bash
acpilot update --check            # who installed this, and who may replace it
acpilot update                    # latest release, checksum verified
acpilot update --version v0.2.4   # a named release
acpilot update --from ./acpilot   # a binary you brought
```
