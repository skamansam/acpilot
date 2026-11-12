---
title: Profiles
description: "One identity per context: agents, keys, policy, preferences, history."
---

<span class="pill pill--hangar">hangar</span> &nbsp; Profiles phase (P4)

A **profile** is a named identity: `work`, `personal`, `client-acme`. It owns
agents, credentials, language-server configuration, permission policy, editor
preferences, sessions, and workspaces.

Profiles exist because those things are context-scoped, not machine-scoped. The
key you use for a client is not the key you use on a weekend. Silently mixing
them is the worst failure this product can have. Unlike a crash, it does not
leave a stack trace.

## What belongs to a profile

| Per profile | Global |
| --- | --- |
| Agents and their credentials | Daemon settings (port, log level) |
| Language-server configuration | Host definitions (SSH targets, WSL distros) |
| Permission policy and trust pins | The list of profiles itself |
| Editor, theme, keymap preferences | |
| Sessions, history, workspaces | |

## Resolution is explicit and loud

The daemon resolves the active profile in this order:

```mermaid
flowchart LR
    flag["--profile name"] --> env["ACPILOT_PROFILE"]
    env --> marker["profile = \"name\"<br/>in .acpilot/config.toml"]
    marker --> active["active_profile<br/>in config.toml"]
    active --> fallback["default<br/>created on first use"]
```

A name that does not resolve is an **error** with exit code `3` and the command
that fixes it, never a fallback. Falling back to `default` means "your client's
code just went out under your personal key", and that is not a helpful default.
Only the final fallback creates and opens `default`. That fallback applies when
there is no flag, no environment variable, no marker, and no `active_profile`.

<Aside type="caution" title="The header rule">
Every mutating API request must name its profile with `X-Acpilot-Profile`. A
missing header is `400`. A client cannot "accidentally" act as the active
profile. It has to say which identity it means. Generated clients take the
profile in their constructor, so forgetting is a compile error rather than a
runtime surprise.
</Aside>

## Isolation is structural

- **Secrets** live in a keyring namespace per profile. `secret set|list|rm`
  resolve the profile in the launch order. `profile export` keeps a credential as
  its reference unless the values are explicitly requested.
- **State** is one SQLite database per profile, so a query cannot cross profiles
  by accident. It would have to open another file. A profile's database is
  `<data>/profiles/<id>/profile.sqlite`. The daemon opens it on first use and
  keeps at most four open, least recently used first. `<data>/index.sqlite` is a
  disposable global list of profiles and the workspaces each profile saw.
- **Sessions and history** are per profile. `work` sessions are invisible from
  `personal`: a `404`, not an empty list, because "empty" implies "you could have
  them". Transcripts live under
  `<data>/profiles/<id>/sessions/<session>/session.jsonl`.
- **Workspaces have an owner.** A workspace summary names its profile, and the
  daemon namespaces its id (`work:local:/src`). The global index records the first
  profile to open a host and root pair. A second profile gets `409
  profile_mismatch` with `details.owner` instead of a second workspace over the
  same checkout. `acpilot workspace open --profile <id> --steal <path>` transfers
  the row deliberately, and the daemon logs both owners.
- **Profiles share hosts. Identities stay separate.** A profile may set `[hosts.<id>]` in
  its `config.toml` with `user`, `identity_file`, and `proxy_jump`. Those change
  how the profile reaches the machine, never which machine the host id names.
  The hostname is not a field.
- **Removal is real.** `acpilot profile rm <id>` moves the config directory and
  the separate data, cache, state, and runtime roots to the OS trash. It prints
  each path and forgets the index row. `--purge` deletes them outright. A profile
  whose `config.toml` is broken can still be deleted. `profile rm` does not
  delete keyring entries yet. They are a separate store, and the keyring API
  cannot enumerate them.
- **Cross-profile references** require an explicit opt-in and log a warning on
  every resolution.

## Configuration cascade

The cascade is the same one described in [configuration](../../reference/config/):
built-ins, then the global file, then the profile, then the workspace, then the
environment, then flags. The layers merge per key, and the daemon records the
winning file so `profile diff` can name it.

The merge is per key, not per file. `acpilot profile diff work personal` prints
the effective configuration with the winning file for every key. That is the only
practical answer to "why is it doing that?".

## Status

Partial. A daemon serves every profile directory under `<config>/profiles` and
dispatches each request by `X-Acpilot-Profile`. Each profile has its own agents,
policy, database, sessions, and language servers. The profile API serves list and
read, create, patch label/parent, activate, delete (trash by default,
`?purge=true` to purge), preferences, and export. The export carries keyring
references, never resolved credentials. The web cockpit's profile picker uses
the list and activation routes.

The daemon serves the launch order (`--profile` → environment → marker →
`active_profile` → created `default`). It isolates profile state by database and
transcript directory. The daemon serves `acpilot profile rm` (P4-4),
`acpilot profile export` (P4-5), and P4-6's workspace ownership. Workspace and
session summaries name their profile. A session row persists it. A root owned by another
profile is a `409 profile_mismatch`, and `--steal` transfers it deliberately.
The daemon reads per-host `user`/`identity_file`/`proxy_jump` overrides from the
profile's `config.toml`. Renaming and importing a profile, and the rest of P4-9's
CLI verbs, are still open. The isolation matrix is P4-10.
