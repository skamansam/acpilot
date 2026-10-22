---
title: Secrets
description: Keys live in the OS keyring, scoped to a profile, and never cross the API.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P4

Agent credentials are the most dangerous data this product touches. They are
handled with three rules, in order.

## 1. The keyring holds them, not the config

```toml
# agents.toml — what is on disk
[[agents]]
id = "claude-code"
transport = "acp-stdio"
command = "npx"
args = ["-y", "@zed-industries/claude-code-acp"]
env = { ANTHROPIC_API_KEY = "keyring:claude-code" }
```

The value is a reference. The secret lives in the OS keyring under the service
`acpilot` and the account `work/claude-code`, so a copied config directory does
not copy credentials. The reference names only the entry; the profile is the
directory the file lives in, which is why the same line means two different
secrets in `profiles/work` and `profiles/personal`.

## 2. The API never returns a secret

`GET /v1/agents` reports *whether* a credential is present, never its value.
There is no route that reads a secret back; exporting a profile redacts it to a
reference. If a client needs the value — it does not — it is not a client.

<Aside type="caution" title="The exception is the point">
The daemon injects a secret into a spawned agent's environment, because that is
the only way to authenticate. That is a write-only path: keyring → child process,
never keyring → HTTP response.
</Aside>

## 3. A profile is a namespace

Keyring entries are namespaced per profile. Deleting a profile removes its
entries; deleting one profile does not disturb another (a test asserts the
directory hash of the survivors is unchanged).

## What is deliberately not supported

- **Plaintext secrets in config files.** No `api_key = "sk-..."` mode. Convenience
  here is how keys end up in a commit.
- **Environment variable fallbacks for profiles.** `ACPILOT_PROFILE` selects a
  profile; it does not carry a secret.
- **Secrets in logs.** `tracing` fields go through a redacting filter for known
  key names; the audit log records decisions, not payloads.

## Status

Not started. P4-4 builds the keyring namespace and the export-redaction tests;
until then there is nothing to leak, which is the one advantage of a skeleton.
