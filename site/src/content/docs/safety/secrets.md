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

The backend exists. `acpilot-agent::keyring` resolves `keyring:<name>` and
`keyring:<profile>/<name>` through the OS store — Keychain on macOS, Credential
Manager on Windows, Secret Service on unix — under the `acpilot` service and the
account `<profile>/<name>`. A missing entry fails the manifest load and names the
account to add, rather than failing mid-turn. The namespace and every error path
are tested against an injected store, so the suite needs no logged-in desktop.

`acpilot secret set|list|rm` writes and removes through the same namespace. `set`
reads the value from stdin — there is no `--value`, because an argument is
visible in shell history and in `ps` — and never prints it back. `list` parses
the profile's `agents.toml` and reports `present` or `missing` per account, so it
shows what the manifest expects without reading any value out of the keyring.

Still to come in P4-5: profile resolution supplies the profile name, legacy
unscoped entries are adopted into `default`, each cross-profile resolution logs a
warning, and `profile export` redacts a secret back to its reference.
