# The language-server catalog

`<id>.toml` is the reviewed source. `marketplace/lsp/index-v1.json` is generated
from these files and signed by the catalog key. A person edits the TOML in a pull
request, a reviewer checks the upstream release, licence and checksum, and
`cargo xtask lsp-index` writes the signed JSON. Nothing here is fetched at
startup, and an install is always an explicit command.

## Adding or updating an entry

1. Check that the project's licence permits redistributing the binary.
2. Download the release asset for each target and record its exact size and
   SHA-256. The catalog's `sha256` is the archive's digest, not the executable's.
   the installer records the executable digest separately.
3. Add a `[[versions]]` block under `<id>.toml`. Targets are Rust triples. A
   target absent from an entry means "not available here", never "try the
   nearest one".
4. Run:

   ```bash
   ACPILOT_LSP_SIGNING_KEY=<64-hex seed> cargo xtask lsp-index
   ```

5. Commit the TOML, the generated `index-v1.json` and its `.sig`. Changing any
   byte of the JSON without re-signing makes every client refuse the catalog.

The public half is compiled into `acpilot` as `MAINTAINER_PUBLIC_KEY` in
`crates/acpilot-cli/src/lsp.rs`. `ACPILOT_LSP_INDEX_PUBKEY` points a build at a
mirror's published copy. The address is transport, the key is trust.

The private seed is **not** committed. The release job keeps it as
`ACPILOT_LSP_SIGNING_KEY`. The 2026-11-21 catalog was signed by the key whose
public half is compiled in. Losing that seed means rotating the key in a release,
not editing this file.

`ACPILOT_LSP_INDEX` points `acpilot lsp` at another published copy. The address is
a transport choice. The signature is the trust decision.
