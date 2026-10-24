# 0003 — Extensions are data first

- **Status:** proposed
- **Date:** 2026-10-20
- **Deciders:** project maintainers
- **Resolves:** D17 (registry trust), D18 (extension code execution)
- **Relates to:** ADR 0002 (local models and the model lab)

## Context

Everything Acpilot ships is currently built in: two themes, ten language-server
presets, four agent presets, no grammars, no snippets. That list is exactly the
kind of thing a community should be able to extend without waiting for us, and
exactly the kind of thing that turns into a package manager with a supply chain
if it is designed carelessly.

Zed solved this with WASM extensions: `extension.toml`, a registry, grammars
compiled at install time, and dev extensions loaded from a path. It works, and it
costs a WASM runtime, an extension ABI, and a permanent sandboxing project.

The question is not "can we copy that". It is: which of the things people will
actually want to install need *code*, and what does the other 80% cost?

## Decision

### 1. Two tiers, and the second one is not in v0.1

**Declarative extensions** (Tier 1, ships): a directory with `extension.toml` and
assets. No executable code, no sandbox, nothing to audit beyond a manifest and
the files it points at. A declarative extension can contribute:

| Kind | What it adds | Shape |
| --- | --- | --- |
| `theme` | A colour scheme for TUI, GUI and web | TOML token set |
| `language_servers` | Presets for `lsp.toml` | command, args, filetypes, root markers |
| `agents` | Presets for `agents.toml` | transport, command, args, docs |
| `grammars` | Tree-sitter grammar + highlight/indent queries | prebuilt `.wasm` grammar + `.scm` queries |
| `snippets` | Per-language snippets | JSON, same format the editor already reads |
| `keymap` | A keymap layer, applied per profile | TOML |

A grammar is data at the edge: the parser is compiled by the grammar's own
publisher, shipped as wasm, and executed by tree-sitter's runtime — which the
editor already embeds. That is not a new sandbox, it is the existing one.

**Code extensions** (Tier 2, proposed, not v0.1): transcript parsers for
program-as-agent, tokenizers, and anything that needs a real runtime. These need
`wasmtime`, an ABI, capability declarations, and a decision about how much memory
and time a hostile module may consume. That is a project, not a feature, and
shipping it half-designed would put a runtime in the editor with no story for
what it may touch. D18 records the proposed shape so the door stays open: a
`kind = "wasm"` extension declares `abi = 1` and a capability list, and the host
grants nothing it cannot revoke.

### 2. Install globally, activate per profile

An extension is installed once into a content-addressed store
(`~/.local/share/acpilot/extensions/<id>/<version>/<sha256>`), then **activated**
by a profile. The same theme should not exist three times because you have three
profiles; the same client's agent preset should not appear in your personal
cockpit because you installed it once.

Precedence, lowest to highest: built-ins < installed extensions activated in the
profile < the profile's own `lsp.toml` / `agents.toml` / `theme` setting. A
preset is a *suggestion* that lands in the same typed registries as everything
else, which means `acpilot profile diff` can explain where a value came from and
`acpilot ext disable` can take it away.

Activation never rewrites your configuration files. It changes what the loader
sees, and `acpilot ext why <preset-id>` prints the chain.

### 3. The registry is a static, signed index

Not an API, not an account, not a database: an HTTPS-hosted JSON document with
entries `{id, version, kind, checksum, size, publisher, min_acpilot, download}`,
mirrored on GitHub Pages, plus a detached signature over the index.

- Checksums are mandatory and verified on install; a mismatch is a hard failure
  and a `Blocked` status in the UI, not a warning.
- The index is signed by a maintainer key that ships in the binary; a second,
  user-configurable key can be trusted for a private registry.
- Publishing is a pull request against an index repository, so every extension
  version is reviewed by a human with a diff. That is the only supply-chain
  defence that has ever worked at this scale.
- No auto-update and no background fetch. `acpilot ext update` is a command a
  person types; a new version is a new checksum and a new review.

### 4. What an extension cannot do

- It cannot grant itself permissions. A preset that names `terminal` tools still
  meets the broker, and a command it introduces is trust-pinned on first run
  exactly like a hand-written one (P7-12).
- It cannot run at install time. Installing is unpacking a directory and hashing
  it.
- It cannot reach the network at runtime. Tier 1 has no runtime.
- It cannot shadow a built-in by id. Ids are namespaced `publisher.id`, and a
  collision is an error rather than a silent override.

## Consequences

- Roughly 80% of the demand — themes, presets, snippets, grammars — arrives
  without a sandbox, a runtime, or an ABI. That is the whole point of the split.
- Themes and presets become data that four UIs read, so a theme written once
  works in the TUI, the desktop app, and the browser. The built-ins move into the
  same format, which is how the schema gets tested.
- The registry is a maintainer bottleneck. Accepted: a reviewed index is slower
  than a self-publish button and vastly cheaper than an incident.
- Tier 2 is deferred, so transcript parsers stay data-driven (the engine phase's task P2-7's
  `parser = "script:<path>"` escape hatch remains the answer until D18 is
  settled). Some extension requests will wait.
- A grammar `.wasm` is still executable code inside tree-sitter's runtime. It is
  a narrower surface than a general ABI, not zero. The trust story is the
  reviewed index and the pinned checksum, and the docs must say that plainly
  rather than implying that "declarative" means "inert".

## Alternatives considered

- **WASM everything, like Zed.** Rejected for v0.1: correct for grammars and
  parsers, disproportionate for a theme. It would also make the first release of
  the extension system a sandboxing project.
- **A live registry API with accounts and publish tokens.** Rejected: accounts,
  rate limits, revocation, and a database, bought in exchange for removing the
  one review that keeps the index honest.
- **Extensions as git repositories the user clones.** Rejected: no checksum
  story, no version resolution, and "works on my branch" is not a distribution
  model.
- **Per-profile installs only.** Rejected: duplicated bytes and three copies of
  the same theme to patch.
- **Auto-update on start.** Rejected: a network call at launch, a new checksum
  nobody reviewed, and a bad version becomes an outage nobody chose.
