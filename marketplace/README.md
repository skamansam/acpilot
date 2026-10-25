# The marketplace

`index.toml` is the marketplace. It is a reviewed list of extension repositories,
not a service: no accounts, no upload endpoint, no crawl, no auto-update. To be
listed, open a pull request against this repository.

## Adding an entry

1. Publish the extension somewhere public, with a licence.
2. Make sure `acpilot ext add --from <checkout>` succeeds on it: valid
   `extension.toml`, contributions that exist, no install-time script.
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

Review looks at the same things the loader will refuse: a namespaced id that
matches the publisher, no path that leaves the extension directory, no code that
runs at install time, and a theme whose token pairs are not unreadable.

## What "install" means today

The signed static index with per-version checksums is the mod-shop phase's task P4.6-4. Until it lands,
listing gets a repository found; installing it means:

```bash
git clone https://github.com/you/acpilot-pack /tmp/acpilot-pack
acpilot ext add --from /tmp/acpilot-pack/pack
acpilot ext activate you.pack --profile work
```

`ext add` hashes the directory and records the checksum; `ext verify --all`
rechecks it, and a mismatch is `Blocked` rather than a warning.

## Why a list in a repository

A registry service needs hosting, a moderation interface, an account system, and
a story for what happens when it is compromised. A git repository gets review,
history, and a revert for free — and a PR that adds a malicious entry is visible
to everyone, not queued behind a dashboard.

## Other editors' extensions

TextMate `.tmTheme` files can already be converted:

```bash
acpilot ext import-theme --from Dracula.tmTheme --id you.dracula
```

Importing an entire Zed or VS Code extension is future work: their formats are
code-bearing (WASM, node), while this format is data. When it happens, the
review and the checksum are the same; the parser is what changes.
