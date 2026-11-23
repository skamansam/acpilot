# 11. A release is cargo-dist's artifacts, signed

**Status:** accepted, 2026-11-17

ADR 0010 says an installation says how it arrived, and that a copy nobody owns may
replace itself from a release whose checksum matches. Both halves assumed a release
whose shape the repository chose: `release.yml` copied three binaries into a directory,
wrote `install.json` into it, and tarred the result. In parallel, `dist-workspace.toml`
described a second, unrelated release — one archive per program, produced by
`cargo xtask dist`, with installers and a Homebrew formula that had never been
published. Two shapes, one of which was documented and one of which shipped. P12-2 was
to choose. This is the choice.

## Decisions

### D1 — the workflow packages nothing

`cargo xtask dist` is the only thing that makes a release artifact. The workflow runs
it twice: `--artifacts local` on a runner per target, because an archive is built by a
machine that can build for it, and `--artifacts global` once, because the shell and
PowerShell installers, the Homebrew formula and the per-program SBOMs are the same for
every target.

Nothing edits an archive after dist made it. The installers dist generates carry the
checksum of the archive they were generated from; adding a file would leave an
installer that refuses the archive beside it. Anything that must be inside an archive
is named in `dist-workspace.toml` before the build.

### D2 — one archive per program, named after the package and the target

```
acpilot-cli-x86_64-unknown-linux-gnu.tar.gz
acpilotd-x86_64-unknown-linux-gnu.tar.gz
acpilot-gui-aarch64-apple-darwin.tar.gz
acpilot-cli-x86_64-pc-windows-msvc.zip
```

The version is not in the name: it is the tag the artifacts hang under, so
`releases/download/<tag>/<asset>` is a stable address across releases. `tar.gz` for
unix, `zip` for Windows, both set in `dist-workspace.toml`; `acpilot update` unpacks by
the bytes' magic, so the format is the archive's fact and not the reader's.

An update therefore fetches three files, verifies each, and installs the union. A
release that published two good archives and one substituted one installs nothing.

### D3 — the archive still says `tarball`, without a version

`install.json` is a static file at the repository root, included in every archive by
`include = ["install.json"]`. It says `{"method": "tarball"}` and nothing else, because
one file serves every release and the tag is the version; `acpilot update` records the
version it installs into the marker it finds. A hand-extracted archive is therefore
self-updatable, as it was before, and a copy with no marker at all is still `unknown`
(ADR 0010 D2) and still nobody's to replace.

### D4 — a checksum is not a signature

Every archive ships `<asset>.sha256` and `<asset>.sha256.sig`: a detached ed25519
signature over the checksum file's bytes, made by a key that exists only as a
repository secret. `acpilot update` verifies the signature before it parses the digest,
because the two files travel over the same connection — an attacker who can replace an
archive can replace the checksum beside it, and a digest nobody signed only proves the
download arrived intact.

The public half is compiled in at build time from `ACPILOT_RELEASE_PUBKEY`. A build
without it has no way to answer "did the release publish this", so it refuses to
replace itself rather than falling back to the checksum; a release without
`ACPILOT_SIGNING_KEY` fails in the signing step rather than publishing unsigned. Key
generation, rotation and the two Git configuration entries are in
`docs/release-key.md`.

`--from` is unchanged: a person who carries a binary to an air-gapped machine is the
one making the trust decision, and there is no release to verify.

## Consequences

- **Three downloads, not one.** A few megabytes each; the alternative is a shape no
  installer, formula or SBOM generator in the ecosystem agrees with.
- **`acpilot update` on a musl machine fetches the `gnu` archive.** The updater maps
  the platform to one triple, and that triple is the glibc build. A musl host that
  installs from the shell installer is still the installer's copy and still updates
  through it; teaching the updater to prefer musl is a table entry, not a redesign.
- **The Homebrew tap is not pushed yet.** The formula is generated, marked and
  uploaded as a release asset; `publish-jobs = ["homebrew"]` names the tap, and the
  repository that receives it does not exist. `brew install` therefore has nothing to
  install from until it does.
- **A key is a single point of failure.** Losing `ACPILOT_SIGNING_KEY` means the next
  release cannot be signed and `acpilot update` will refuse it; the rotation path in
  `docs/release-key.md` exists so that this is a planned change rather than an
  incident.
