---
title: The marketplace
description: A curated list of repositories, changed by pull request, with no service behind it.
---

<span class="pill pill--boarding">boarding</span> &nbsp; curated list landed; downloads are P4.6-4

The marketplace is a file in this repository: [`marketplace/index.toml`](https://github.com/skamansam/acpilot/blob/main/marketplace/index.toml).
It lists extension repositories that a human has reviewed. There is no account
system, no upload endpoint, no crawl, and nothing that runs on your machine just
because you opened the list.

## Adding your extension

1. Publish the extension somewhere public, with a licence.
2. Check that `acpilot ext add --from <checkout>` succeeds. That runs the same
   loader the product uses: namespaced ids, contribution paths that stay inside
   the directory, no install-time script.
3. Open a pull request adding an entry: `id`, `repo`, `path` (the directory
   containing `extension.toml`), `description`, `publisher`.

The review is the supply chain. A list that anyone can append to without a
review is a list of strangers' code with a "trusted" badge drawn on it.

## Installing from a listing

Until the signed index lands, `ext add` takes a directory, not a URL:

```bash
git clone https://github.com/you/acpilot-pack /tmp/acpilot-pack
acpilot ext add --from /tmp/acpilot-pack/pack
acpilot ext activate you.pack --profile work
```

The store still hashes it and records the checksum, so `ext verify --all` is the
same check either way.

## Other editors

- **TextMate themes** already import: `acpilot ext import-theme --from Dracula.tmTheme --id you.dracula`
  converts the plist, inherits everything it does not define from the built-in
  theme with the same appearance, and reports any pair below WCAG AA.
- **VS Code and Zed extensions are future work.** Zed's marketplace serves
  extensions and runs code extensions as WASM with a host API; VS Code's are
  node. This format is data, and the import would have to either translate the
  declarative parts or grow a runtime — which is exactly the decision ADR 0003
  recorded rather than half-building. The curated list is what makes a
  translation layer reviewable when it happens.

<Aside type="note" title="Why not a marketplace service">
A registry is hosting, moderation, accounts, and a compromise plan. A repository
is review, history, and `git revert`. For a list of a few dozen packs, the second
one is both cheaper and easier to audit — and when the signed index arrives it is
still a static document, not a service.
</Aside>
