---
title: Profiles
description: "One identity per context: agents, keys, policy, preferences, history."
---

<span class="pill pill--hangar">hangar</span> &nbsp; P4

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

```text
--profile <name>   →   ACPILOT_PROFILE   →   .acpilot/profile marker
                   →   active_profile in config.toml   →   "default"
```

A name that does not resolve is an **error**, never a fallback. Falling back to
`default` means "your client's code just went out under your personal key", and
that is not a helpful default.

<Aside type="caution" title="The header rule">
Every mutating API request must name its profile with `X-Acpilot-Profile`.
Missing header → `400`. A client cannot "accidentally" act as the active profile;
it has to say which identity it means. Generated clients take the profile in
their constructor, so forgetting is a compile error rather than a runtime
surprise.
</Aside>

## Isolation is structural

- **Secrets** live in a keyring namespace per profile.
- **State** is one SQLite database per profile, so a query cannot cross profiles
  by accident; it would have to open another file.
- **Sessions and history** are per profile. `work` sessions are invisible from
  `personal` — a `404`, not an empty list, because "empty" implies "you could
  have them".
- **Cross-profile references** require an explicit opt-in and leave an audit
  line.

## Configuration cascade

```text
defaults.toml  <  config.toml  <  profiles/<name>/config.toml  <  workspace .acpilot/config.toml  <  env  <  flags
```

Merged per key, not per file. `acpilot profile diff work personal` prints the
effective configuration with the winning file for every key, which is the only
practical answer to "why is it doing that?".

## Status

Not started. P4 lands the resolution function, the cascade, and the isolation
tests before any UI exposes a profile switcher.
