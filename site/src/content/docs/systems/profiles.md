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
them is the worst failure this product can have, and unlike a crash it does not
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

The active profile is resolved in this order:

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
Only the final fallback — no flag, no environment variable, no marker, no
`active_profile` — creates and opens `default`.

<Aside type="caution" title="The header rule">
Every mutating API request must name its profile with `X-Acpilot-Profile`.
Missing header → `400`. A client cannot "accidentally" act as the active profile;
it has to say which identity it means. Generated clients take the profile in
their constructor, so forgetting is a compile error rather than a runtime
surprise.
</Aside>

## Isolation is structural

- **Secrets** live in a keyring namespace per profile. `secret set|list|rm`
  resolve the profile in the launch order, and `profile export` keeps a
  credential as its reference unless the values are explicitly requested.
- **State** is one SQLite database per profile, so a query cannot cross profiles
  by accident; it would have to open another file. A profile's database is
  `<data>/profiles/<id>/profile.sqlite`; the daemon opens it on first use and
  keeps at most four open, least recently used first. `<data>/index.sqlite` is a
  disposable global list of profiles and the workspaces each has seen.
- **Sessions and history** are per profile. `work` sessions are invisible from
  `personal` — a `404`, not an empty list, because "empty" implies "you could
  have them". Transcripts live under
  `<data>/profiles/<id>/sessions/<session>/session.jsonl`.
- **Removal is real.** `acpilot profile rm <id>` moves the config directory and
  the separate data, cache, state and runtime roots to the OS trash, prints each
  path, and forgets the index row. `--purge` deletes them outright. A profile
  whose `config.toml` is broken can still be removed. Keyring entries are not
  deleted by `profile rm` yet: they are a separate store, and the keyring API
  cannot enumerate them.
- **Cross-profile references** require an explicit opt-in and log a warning on
  every resolution.

## Configuration cascade

The cascade is the same one described in [configuration](../../reference/config/):
built-ins, then the global file, then the profile, then the workspace, then the
environment, then flags — merged per key, with the winning file recorded so
`profile diff` can name it.

Merged per key, not per file. `acpilot profile diff work personal` prints the
effective configuration with the winning file for every key, which is the only
practical answer to "why is it doing that?".

## Status

Partial. A daemon serves every profile directory under `<config>/profiles` and
dispatches each request by `X-Acpilot-Profile`: each profile has its own agents,
policy, database, sessions and language servers. `GET /v1/profiles`, `GET
/v1/profiles/{id}` and `POST /v1/profiles/{id}/activate` are served, and the web
cockpit's profile picker uses them. The launch order (`--profile` → environment →
marker → `active_profile` → created `default`) is served, and profile state is
isolated by database and transcript directory. `acpilot profile rm` (P4-4) and
`acpilot profile export` (P4-5) are served. Creating, renaming and importing a
profile, and the per-profile preferences routes, are still open (P4-8/P4-9), as
is the isolation matrix (P4-10).
