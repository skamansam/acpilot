---
title: Secrets
description: Keys live in the OS keyring, scoped to a profile, and never cross the API.
---

<span class="pill pill--hangar">hangar</span> &nbsp; Profiles phase (P4)

Agent credentials are the most dangerous data this product touches. Three rules
govern them.

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
`acpilot` and the account `work/claude-code`. A copied config directory does not
copy credentials. The reference names only the entry. The profile is the
directory the file lives in, which is why the same line means two different
secrets in `profiles/work` and `profiles/personal`.

## 2. The API never returns a secret

`GET /v1/agents` reports *whether* a credential is present, never its value.
No route reads a secret back. Exporting a profile redacts it to a reference. If a
client needs the value, it is not a client.

<Aside type="caution" title="The exception is the point">
The daemon injects a secret into a spawned agent's environment, because that is
the only way to authenticate. That path is write-only: keyring → child process,
never keyring → HTTP response.
</Aside>

## 3. A profile is a namespace

Each profile namespaces its keyring entries. Deleting a profile deletes its
directory and state. Deleting one profile does not disturb another. A test
asserts that the survivors keep the same directory hash. `profile rm` does not
delete a profile's keyring entries yet. The keyring API cannot enumerate
them, and a trashed profile would otherwise lose credentials before it could be
restored.

## What is deliberately not supported

- **Plaintext secrets in config files.** No `api_key = "sk-..."` mode.
  Convenience here is how keys end up in a commit.
- **Environment variable fallbacks for profiles.** `ACPILOT_PROFILE` selects a
  profile. It does not carry a secret.
- **Secrets in logs.** `tracing` fields go through a redacting filter for known
  key names. The audit log records decisions, not payloads.

## Status

The backend exists. `acpilot-agent::keyring` resolves `keyring:<name>` and
`keyring:<profile>/<name>` through the OS store: Keychain on macOS, Credential
Manager on Windows, Secret Service on unix. It uses the `acpilot` service and the
account `<profile>/<name>`. A missing entry fails the manifest load and names the
account to add, rather than failing mid-turn. The suite tests the namespace and
every error path against an injected store, so it needs no logged-in desktop.

A model provider's `credential_ref` uses the same namespace. The daemon reads the
token at probe time and sends it as a bearer header. A missing entry makes the
provider row `unauthorized` and names the account to add, and nothing is sent. The
probe answer, which never contains the token, is remembered in
`providers.state.toml`.

`acpilot secret set|list|rm` writes and deletes through the same namespace. The
profile follows the launch order: `--profile`, `ACPILOT_PROFILE`, the project
marker, `active_profile`, then `default`. A credential therefore follows the
identity the command is actually running under. `set` reads the value from
stdin. There is no `--value`, because an argument is visible in shell history and
in `ps`. The command never prints the value back. `list` parses the profile's
`agents.toml` and reports `present` or `missing` per account. It shows what the
manifest expects without reading any value from the keyring.

An entry written before profiles existed, under the bare account `<name>`, is
adopted into `default/<name>` the first time `default` reads it. No other profile
ever looks at the old account. A cross-profile reference needs the manifest's
`allow_cross_profile = true` and logs a warning on every resolution.

`acpilot profile export <id>` keeps every secret as its reference. With
`--with-secrets --i-understand-the-risk --output FILE`, the export adds a separate
`secrets` map and writes a new `0600` file. It refuses a cross-profile reference
rather than copying another profile's credential.
