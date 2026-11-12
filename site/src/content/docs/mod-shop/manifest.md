---
title: The manifest
description: extension.toml, every field, and the checks that run before the loader reads anything.
---

<span class="pill pill--hangar">hangar</span> &nbsp; Mod shop phase (P4.6-1)

An extension is a directory with an `extension.toml` at its root. The manifest is
the entire interface. There is no install script, no build step at install time,
and nothing to execute.

```toml
# extensions/acpilot.rust-pack/extension.toml
"$schema" = "https://rudeboy.dev/acpilot/schema/extension-v1.json"
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
| `$schema` | string? | Where the JSON Schema lives. The loader ignores it. See [Schema](#schema) |
| `schema_version` | integer | `1` today. The loader refuses a newer version and names it |
| `id` | string | `publisher.id`, lowercase, dot-separated. Also the store key |
| `name` | string | Display name |
| `version` | string | Semver. The store keeps versions side by side |
| `publisher` | string | Who to blame, matched against the index entry |
| `description` | string | One sentence, shown in `ext list` |
| `homepage` | string? | A human reviews it before the index accepts it |
| `min_acpilot` | string? | Below this, the loader refuses the extension, not best-efforts it |
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

Extension-provided presets land in the same typed registries as everything else.
An extension therefore cannot invent a new transport or a new permission. The
code that already checks your own files checks the fragment.

An `agents` fragment is the same document a profile's `agents.toml` is. One reader
parses both. A PTY parser or an HTTP endpoint written in a pack therefore means
the same thing as one written in a profile. One rule is an extension's alone: it
may not name a credential. The loader refuses `env`, `auth`, and
`allow_cross_profile` in a pack. A pack that could name `keyring:work/token` could
have another profile's secret handed to a process it also chose. Those fields
belong in the profile's own `agents.toml`, which the same schema at
[`agents-v1.json`](../../reference/manifest/) describes.

## Schema

The file can identify its schema, so an editor completes keys and underlines
typos before `ext add` ever sees them:

```toml
"$schema" = "https://rudeboy.dev/acpilot/schema/extension-v1.json"
```

The project publishes that document at
`https://rudeboy.dev/acpilot/schema/extension-v1.json`. It is JSON Schema
2020-12, Apache-2.0, and generated from the same Rust type the loader parses. It
therefore cannot describe a key the loader would refuse. CI checks the four
built-in manifests under `extensions/` against it. A test also checks that a file
the schema rejects is a file the loader rejects. Taplo and Even Better TOML also
read a first line of `#:schema <url>` if you would rather keep the document free
of editor plumbing.

The schema checks every field's type, `additionalProperties: false`, the shape of
`id`, `version`, and `publisher`, the seven `kind` values, and at least one
contribution. It cannot check two contributions with the same kind and path. It
also cannot check the `.`/`..` half of the path rule, because JSON Schema has no
way to say it.
**The loader stays the authority.** The schema is there so its refusals arrive as
a squiggle rather than as a surprise on the next start.

The `-v1` in the name is the major version, and this URL is frozen. This is the
same rule as the agent manifest's schema (D6). Within v1 the document may only
gain optional keys or enum members. Anything a `schema_version = 1` loader would
read differently ships as `extension-v2.json`. There is no `latest.json` to go
stale in an editor's cache. `cargo xtask schema` regenerates both published
documents together.

## Validation, before anything loads

- **Unknown keys are errors.** This is the same rule as configuration, manifests,
  and recipes. An extension that spills a typo should fail loudly on the
  developer's machine, not quietly on a user's.
- **The loader enforces namespaced, collision-free ids.** An id is
  `publisher.name`, and a theme's id must sit under its extension's publisher. `acpilot.night-flight`
  already exists, so a variant ships `acpilot.night-flight-syntax`. That is a free
  name because ids collide only when they are literally equal. A collision is an
  error, never an override.
- **Paths may not escape the extension directory.** The loader rejects `../` in a
  contribution path before it opens the file. It resolves symlinks and checks
  them the same way. This is the one check that must not have a "probably fine"
  branch.
- **Contribution paths must exist and parse.** A manifest that points at a
  missing grammar is a `Blocked` extension, not a partially working one.
- **The loader enforces `min_acpilot`.** It refuses an extension built for a
  newer schema and names both versions. Half-understood data is worse than none.

<Aside type="note" title="Data, all the way down">
Nothing in this file can run code. The riskiest thing a manifest can do is name a
command for a language server or an agent. That command is trust-pinned on first
use, with the extension's name in the prompt. The question is "trust
rust-analyzer from acpilot.rust-pack?" rather than a bare path in a dialog.
</Aside>

## What exists

`crates/acpilot-extensions` parses and checks this schema, and the four
built-in extensions under `extensions/` are its first users. The checks above are
tests, not intentions. `crates/acpilot-extensions/tests/loader.rs` covers unknown
keys, schema versions, publisher mismatches, path traversal, symlink escapes, and
the collision and `extends` cycles. Installing from a registry and activating in
a profile are tasks P4.6-3 to P4.6-5.
