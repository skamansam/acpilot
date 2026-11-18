# The marketplace

`index.json` is the marketplace: a reviewed list of extension versions, each naming the archive to download and the sha256 it must hash to, signed by the maintainer's key. It is a file, not a service: no accounts, no upload endpoint, no crawl, no auto-update. To be listed, open a pull request against this repository. `index.toml` is where a repository is proposed before it has a release archive to point at.

## Adding an entry

1. Publish the extension somewhere public, with a licence.
2. Make sure `acpilot ext add --from <checkout>` succeeds on it: a valid `extension.toml`, contributions that exist, and no install-time script.
3. Add an entry to `index.toml`:

   ```toml
   [[extensions]]
   id = "you.pack"
   repo = "https://github.com/you/acpilot-pack"
   path = "pack"
   description = "One sentence, no marketing."
   publisher = "you"
   ```

4. In the pull request, say what the extension does and what it contributes.

Review looks at the same things the loader will refuse. A namespaced id must match the publisher. No path may leave the extension directory. No code may run at install time. A theme's token pairs must be readable.

## What "install" means

The index ships inside each acpilot build, so `search` and `add` need no network
call for the listing itself:

```bash
acpilot ext search rust
acpilot ext add acpilot.rust-pack
acpilot ext activate acpilot.rust-pack --profile work
```

`ext add` checks the archive's size and sha256 against the signed listing before
unpacking, requires the manifest inside to name the same id, version and publisher,
then hashes the installed directory and records that checksum. `ext verify --all`
checks it again, and either mismatch is `Blocked` rather than a warning.

A checkout is still installable for development:

```bash
acpilot ext add --from /tmp/acpilot-pack/pack
```

That copies the directory into the store rather than linking it, so it is
checksummed like any other install; re-run it under a new version after an edit.

## The signature

`index.json` is signed with the maintainer's ed25519 key, and the detached
signature is `index.json.sig`: `{"key": "acpilot-release", "signature": "<128 hex>"}`.
The public half is compiled into acpilot, so a listing signed by any other key is
refused with that key's id named. A private registry adds its own key under
`[[registry.trusted_keys]]` in the global `config.toml`; a workspace cannot add a
trust root to itself.

Editing the index means re-signing it. The signature covers the file's exact
bytes, so a merge that changes a checksum without a new signature is refused by
every client rather than silently trusted. `acpilot ext search` on a build whose
shipped index is stale is still a safe answer: the listings are the release's, not
a background fetch's.

`--index <path|url>` points a command at another signed index:

```bash
acpilot ext search --index https://example.invalid/acpilot/index.json
```

## Why a list in a repository

A registry service needs hosting, a moderation interface, an account system, and a plan for the day an attacker compromises it. A git repository gets review, history, and a revert for free. A pull request that adds a malicious entry is also visible to everyone, not queued behind a dashboard.

## Other editors' extensions

TextMate `.tmTheme` files can already be converted:

```bash
acpilot ext import-theme --from Dracula.tmTheme --id you.dracula
```

Importing an entire Zed or VS Code extension is future work: their formats carry code (WASM, node), while this format is data. When it happens, the review and the checksum stay the same. The parser is what changes.
