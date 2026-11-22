---
title: Updating
description: How this copy was installed, who may replace it, and the one command that does.
---

<span class="pill pill--flying">flying</span> &nbsp; `acpilot update`, `--check`, `--from`

A client refuses to talk to a daemon of a different version (ADR 0004). That is
correct and, on its own, useless. You are told two version strings and nothing
about what to do. What to do depends on how acpilot arrived on the machine. A
copy `apt` unpacked is `apt`'s to upgrade. A tarball somebody unzipped is theirs.
Nothing in a binary can tell those apart. The installation says so itself.

## The marker

Every installation except a bare binary drop leaves one small file called
`install.json`:

```json
{ "method": "tarball", "version": "0.2.6" }
```

`method` is the closed set `apt`, `dnf`, `pacman`, `brew`, `msi`, `script`,
`tarball`, `appimage`, `cargo`. `version` is what the installer put there, so a
marker left by an older package is recognisable rather than trusted. Any other
field is informational. Nothing branches on it.

The marker is written by the package, not by the program. That is the whole
point. A file in the `.deb` says "this came from apt" because apt put it there.
A binary that guessed from where it sits would guess wrong for exactly the
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
downloading https://github.com/skamansam/acpilot/releases/download/v0.2.6/acpilotd-x86_64-unknown-linux-gnu.tar.gz
downloading https://github.com/skamansam/acpilot/releases/download/v0.2.6/acpilot-gui-x86_64-unknown-linux-gnu.tar.gz
downloading https://github.com/skamansam/acpilot/releases/download/v0.2.6/acpilot-cli-x86_64-unknown-linux-gnu.tar.gz
installed acpilot 0.2.6 from v0.2.6: replaced acpilotd, acpilot at /usr/local/bin/acpilot
a daemon that is already running keeps the older binary until it is restarted
```

That is five steps, and each one can stop the update:

1. **The assets for this platform** are named from the release. A release is one
   archive per program — `acpilot-cli`, `acpilotd`, `acpilot-gui`. The archive is
   named after the package and the target triple, with the version in the tag rather than in the
   file name. A platform the release does not build says so instead of fetching a
   404 page.
2. **The signature is checked before the digest.** Every archive publishes
   `<asset>.sha256` and `<asset>.sha256.sig`: a detached ed25519 signature over the
   checksum file's bytes, made by the release key. A checksum with no valid
   signature ends the update. The digest on its own proves the download arrived
   intact. It says nothing about who published it.
3. **The bytes are checked** against that signed digest. A digest that differs ends
   the update and nothing is written. There is no `curl | sh` here.
4. **The archives are unpacked** into a temporary directory, and only the program
   names are read from it. Never an entry's path. An archive that contains
   `../../etc/passwd` has nothing this command wants.
5. **The programs are replaced** one at a time, each written beside its
   destination, made executable, and renamed over it. A failure halfway therefore
   leaves whole programs, not a half-written one. `acpilotd` and `acpilot-gui` are
   replaced only where they already exist: an update does not change which
   programs arrived. `acpilot` itself is written last so a failure leaves you
   a client you can still run.

The marker is then rewritten with the version that is now on disk, keeping the
method. The bytes changed. The way they arrived did not.

The release public key is compiled in from `ACPILOT_RELEASE_PUBKEY`. A binary built
without it cannot tell a release from a substitution. It refuses to replace itself
rather than falling back to the digest alone. The release workflow signs every
checksum with `ACPILOT_SIGNING_KEY` and fails when that secret is missing. The
repository's `docs/release-key.md` covers generating and rotating the pair.

| Flag | What it does |
| --- | --- |
| `--check` | Print the provenance and download nothing |
| `--version <TAG>` | Install a named release (`v0.2.6` or `0.2.6`) instead of the latest |
| `--from <PATH>` | Replace this `acpilot` with a binary you brought yourself |

Exit codes are the CLI's usual ones. `0` means the copy is already the requested
version or was replaced. `1` means it was not: a package manager owns it, no marker
says it may be replaced, or a download or write failed. `2` is a bad invocation.

## What it does not do

- **It does not restart a daemon.** A running `acpilotd` keeps the binary it
  started from until it is restarted. The command says so rather than killing a
  daemon that may be mid-turn.
- **It does not check a signature for `--from`.** A downloaded release is verified:
signature first, then the digest it covers. A binary carried to an air-gapped
machine is the trust decision of the person carrying it, and there is no release
to check.
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
