# The marketplace

`index.toml` is the marketplace. It is a reviewed list of extension repositories, not a service: no accounts, no upload endpoint, no crawl, no auto-update. To be listed, open a pull request against this repository.

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

## What "install" means today

The signed static index with per-version checksums is the mod-shop phase's task P4.6-4. Until it lands, a listing gets you a repository you found, and installing means:

```bash
git clone https://github.com/you/acpilot-pack /tmp/acpilot-pack
acpilot ext add --from /tmp/acpilot-pack/pack
acpilot ext activate you.pack --profile work
```

`ext add` hashes the directory and records the checksum. `ext verify --all` checks it again, and a mismatch is `Blocked` rather than a warning.

## Why a list in a repository

A registry service needs hosting, a moderation interface, an account system, and a plan for the day an attacker compromises it. A git repository gets review, history, and a revert for free. A pull request that adds a malicious entry is also visible to everyone, not queued behind a dashboard.

## Other editors' extensions

TextMate `.tmTheme` files can already be converted:

```bash
acpilot ext import-theme --from Dracula.tmTheme --id you.dracula
```

Importing an entire Zed or VS Code extension is future work: their formats carry code (WASM, node), while this format is data. When it happens, the review and the checksum stay the same. The parser is what changes.
