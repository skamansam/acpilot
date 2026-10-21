---
title: The manifest
description: extension.toml, every field, and the checks that run before anything is loaded.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P4.6-1

An extension is a directory with an `extension.toml` at its root. The manifest is
the entire interface: there is no install script, no build step at install time,
and nothing to execute.

```toml
# extensions/acpilot.rust-pack/extension.toml
schema_version = 1
id = "acpilot.rust-pack"            # publisher.id — never a bare name
name = "Rust pack"
version = "1.2.0"
publisher = "acpilot"
description = "rust-analyzer preset, a night-flight syntax theme, and Rust snippets."
homepage = "https://github.com/acpilot/extensions/tree/main/rust-pack"
min_acpilot = "0.1.0"               # refuse rather than misbehave

[[contributes]]
kind = "language_servers"
path = "lsp.toml"

[[contributes]]
kind = "theme"
path = "themes/night-flight-syntax.toml"

[[contributes]]
kind = "snippets"
path = "snippets/rust.json"

[[contributes]]
kind = "keymap"
path = "keymap.toml"

[[contributes]]
kind = "grammars"
path = "grammars/rust"              # manifest.toml + .wasm + queries there
```

## Fields

| Field | Type | Notes |
| --- | --- | --- |
| `schema_version` | integer | `1` today; a newer version is refused with the version named |
| `id` | string | `publisher.id`, lowercase, dot-separated; also the store key |
| `name` | string | Display name |
| `version` | string | Semver; the store keeps versions side by side |
| `publisher` | string | Who to blame, matched against the index entry |
| `description` | string | One sentence, shown in `ext list` |
| `homepage` | string? | Reviewed by a human before the index accepts it |
| `min_acpilot` | string? | Below this, the extension is refused, not best-efforted |
| `contributes` | array | `kind` + `path`, one entry per contribution |

## Kinds

| `kind` | Path points at | What the loader does |
| --- | --- | --- |
| `theme` | A token TOML file | Adds it to `GET /v1/themes` |
| `language_servers` | An `lsp.toml` fragment | Merges presets into the LSP registry |
| `agents` | An `agents.toml` fragment | Merges presets into the agent registry |
| `grammars` | A directory | Lazy-loads the grammar on first matching file |
| `snippets` | A JSON file | Adds per-language snippets |
| `keymap` | A `keymap.toml` layer | Applied above built-ins, below your own file |

Extension-provided presets land in the same typed registries as everything else,
which is why an extension cannot invent a new transport or a new permission: the
fragment is validated by the code that already validates your own files.

## Validation, before anything loads

- **Unknown keys are errors.** Same rule as configuration, manifests, and recipes.
  An extension that spills a typo should fail loudly on the developer's machine,
  not quietly on a user's.
- **Ids are namespaced and collision-free.** An id is `publisher.name`, and a
  theme's id must sit under its extension's publisher. `acpilot.night-flight`
  already exists, so a variant ships `acpilot.night-flight-syntax` — a free name
  because ids collide only when they are literally equal. A collision is an
  error, never an override.
- **Paths may not escape the extension directory.** `../` in a contribution path
  is rejected before the file is opened, and symlinks are resolved and checked the
  same way. This is the one check that must not have a "probably fine" branch.
- **Contribution paths must exist and parse.** A manifest that points at a missing
  grammar is a `Blocked` extension, not a partially working one.
- **`min_acpilot` is enforced.** An extension built for a newer schema is refused
  with both versions in the message, because half-understood data is worse than
  none.

<Aside type="note" title="Data, all the way down">
Nothing in this file can run code. The riskiest thing a manifest can do is name a
command for a language server or an agent — and that command is trust-pinned on
first use, with the extension's name in the prompt, so the question is "trust
rust-analyzer from acpilot.rust-pack?" rather than a bare path in a dialog.
</Aside>

## What is implemented

`crates/acpilot-extensions` parses and validates this schema, and the four
built-in extensions under `extensions/` are its first users. The checks above are
tests, not intentions: `crates/acpilot-extensions/tests/loader.rs` covers unknown
keys, schema versions, publisher mismatches, path traversal, symlink escapes, and
the collision and `extends` cycles. Installing from a registry and activating in
a profile are P4.6-3 to P4.6-5.
