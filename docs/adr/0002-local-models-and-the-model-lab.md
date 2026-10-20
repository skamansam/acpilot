# 0002 — Local models, the model lab, and remote ACP drivers

- **Status:** proposed
- **Date:** 2026-10-20
- **Deciders:** project maintainers
- **Resolves:** D14 (model adapter seam), D15 (remote driver trust model)
- **Supersedes:** —

## Context

Acpilot already has a way to run an agent: spawn a program that speaks ACP, or
drive a program through a PTY. Both assume the *program* is the intelligence.

A local model inverts that. LM Studio (or Ollama, llama.cpp, vLLM) is a server
with a chat endpoint. It has no notion of a session, a file, a permission, or a
tool result. Everything an "agent" does — reading a file, running a command,
asking before writing — is the caller's job.

The user-facing ask is concrete: open a workbench, point it at a local model,
edit the master prompt and the sampling knobs, feed it a corpus from the current
project, compare runs, and then keep the good one as an agent. Separately, a
remote ACP client (Devin, or any other driver) should be able to connect and
drive that agent over ACP, with authentication, without being handed the keys to
the machine.

These are expensive to get wrong: a model loop that bypasses the permission
broker would undo the one promise the product makes.

## Decision

### 1. A model is not an agent; a recipe is not an agent either

The lab produces **recipes** (data). A recipe becomes an **agent** only through
one narrow, reversible step: it is written to `agents.toml` as a normal
`acp-stdio` entry that spawns `acpilot-agent-lm --recipe <id>`.

That shim is a small ACP agent. It owns the model conversation loop and nothing
else. It reaches the outside world only through ACP client callbacks:

| The model wants to | The shim calls | The daemon decides |
| --- | --- | --- |
| read a file | `fs/read_text_file` | host routing, workspace scope, audit |
| write a file | `fs/write_text_file` | permission broker, diff, shadow-write guard |
| run a command | `terminal/create` | policy, workspace root, audit |
| do anything else | `session/request_permission` | policy or a human |

Consequence: no new trust boundary. A model-backed agent is exactly as dangerous
as `claude-code-acp`, because it is the same kind of process talking to the same
broker. It also inherits SSH/WSL host routing and the transcript for free.

Rejected alternative: run the loop inside `acpilotd`. It is fewer processes and
it would need its own copy of the tool schema, the permission round-trip, and
the cancellation semantics — a second implementation of ACP that will drift.

### 2. One adapter seam, LM Studio first

`acpilot-model` exposes `ChatBackend`: `models()`, `probe()`, `chat()`. Two
implementations ship, and they are different code paths on purpose:

- `LmStudioBackend` — LM Studio's native `/api/v0` surface. `GET /api/v0/models`
  reports `type`, `arch`, `quantization`, `state`, and `max_context_length`;
  `POST /api/v0/chat/completions` returns `stats` (tok/s, time to first token)
  and `model_info`. That metadata is the point of the lab, so it is read from the
  server rather than guessed.
- `OpenAiCompatibleBackend` — plain `/v1/chat/completions` and `/v1/models`, for
  Ollama, llama.cpp, vLLM, OpenRouter, and hosted endpoints. It reports fewer
  facts, and the UI shows fewer gauges rather than pretending.

`ProviderKind` therefore has two variants today. Anthropic and Gemini are *not*
OpenAI-shaped, and adding them means adding a variant *and* an adapter, in that
order. LM Studio's Anthropic-compatible endpoint is what the adapter will be
developed against, so the work does not need a cloud key to begin.

### 3. Recipes are files, runs are records

A recipe is TOML in the profile: `profiles/<name>/recipes/<id>.toml`, with
`schema_version`, and unknown keys are an error — the same rule as
configuration and manifests. It is reviewable in a diff and shareable by copying
a file.

A run is immutable: the exact request (provider, model, system prompt hash,
sampling parameters, corpus selectors, context budget, tool exposure) plus what
came back (latency, first-token time, token usage, stop reason, warnings). The
lab's compare view is a diff of two records, not a re-roll of the dice.

### 4. No implicit network

Nothing contacts a model server until a provider is declared and probed by
explicit action. There is no polling, no telemetry, no "check for models" on
startup, and a declared provider that is unreachable is reported as unreachable
rather than retried in the background. LM Studio authenticates with an optional
`Bearer` token; the token lives in the keyring, and the API exposes only a
`credential_ref`.

### 5. Remote drivers speak ACP, and they do not sign for the machine

`acpilotd` can host an **ACP endpoint** (`acp-stdio` to a local client, or ACP
over HTTP/JSON-RPC to a remote one) so an external driver — Devin, a CI job, a
second editor — can run sessions on this daemon. Enabling it is an explicit,
off-by-default act with a bind address, a token, and a pairing code for the
first connection.

A driver is granted scopes: `observe`, `prompt`, `cancel`, and — off by default —
`answer_permissions`. Without that last scope, a permission request raised by a
turn the driver started is routed to the human's attached clients and the driver's
call blocks until a human or a policy answers. A cloud agent may type; it may not
approve. Everything is audited with the driver's identity attached, and every
session records which driver opened it.

## Consequences

- One extra process per model-backed agent (~10–20 MB plus the model, which is
  loaded by LM Studio, not by us). Acceptable: it buys the broker, the hosts, and
  the transcript with no new code paths.
- Pre-flight token counts are **estimates**; only `usage` from the server is
  authoritative. The UI must label them differently, and context overflow is a
  reported event, not a silent truncation.
- Local tool-calling quality varies wildly. The workbench measures tool-call
  validity (parse rate, schema-valid rate, steps-to-complete) and shows it before
  you promote a recipe to an agent.
- The bridge is new attack surface. It is off by default, scoped, token-gated,
  pairing-gated, and audited; the ADR deliberately does not allow
  `answer_permissions` on a driver by default.
- Two providers to test, two shapes. The mock OpenAI-compatible server in the
  test suite is the default fixture; LM Studio is exercised nightly when present.

## Alternatives considered

- **In-daemon model loop.** Rejected: duplicates ACP's client side, bypasses the
  supervisor and its restart policy, and makes "model" a special case forever.
- **One adapter per provider.** Rejected: four nearly identical HTTP clients and
  a matrix of quirks. An OpenAI-shaped adapter plus explicit native extras is
  less code and fewer lies.
- **Recipes in SQLite.** Rejected: invisible in a diff, unshareable, unmigratable.
  Files match manifests, preferences, and language servers.
- **Let the remote driver answer permission prompts.** Rejected for v0.1: a
  driver that can approve its own writes is an agent with a credit card. Opt-in
  scope exists for a reason, and the release notes must say so.
- **A hosted "lab" service that stores prompts.** Rejected: the corpus is your
  source code.
