---
title: Providers
description: LM Studio first, the OpenAI-compatible rail for everything else, and no scanning of the local network without being asked.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P4.5-1, P4.5-2

A **provider** is a URL and a dialect. That is the whole abstraction. Acpilot does
not download models, manage GPU memory, or restart your inference server; LM
Studio, Ollama, and llama.cpp already do that well, and duplicating it would be
the wrong kind of ambition.

## LM Studio, first

LM Studio is the reference provider because it answers the questions a lab needs
to ask. Its native REST API reports what the model *is*, and what the request
*cost*:

```toml
# ~/.config/acpilot/profiles/work/providers.toml
schema_version = 1

[[providers]]
id = "lmstudio"
kind = "lm_studio"
label = "LM Studio (this machine)"
base_url = "http://127.0.0.1:1234"
enabled = true
# Only for servers started with a token (lms server start --token):
# credential_ref = "keyring:lmstudio/work"
```

```bash
curl -s http://127.0.0.1:1234/api/v0/models | jq '.data[0]'
```

```json
{
  "id": "qwen2-vl-7b-instruct",
  "object": "model",
  "type": "vlm",
  "publisher": "mlx-community",
  "arch": "qwen2_vl",
  "compatibility_type": "mlx",
  "quantization": "4bit",
  "state": "not-loaded",
  "max_context_length": 32768
}
```

That response fills most of the lab's instrument panel:

| Field | Becomes | Why it matters |
| --- | --- | --- |
| `type` | `chat` / `vision` / `embedding` | An embedding model is not an agent, and the picker says so |
| `arch` | Display string | "Why is this one weird at code?" |
| `quantization` | Display string | `Q4_K_M` explains more than temperature does |
| `state` | `loaded` | A not-loaded model costs seconds on the first run; the UI warns |
| `max_context_length` | `context_window` (`context_source: server`) | The recipe is validated against it, not against a guess |

And the chat endpoint returns the numbers a tuning session lives on — `usage`,
plus `stats.tokens_per_second`, `stats.time_to_first_token`, and
`stats.stop_reason`, with `model_info` and `runtime` naming the engine that
actually answered:

```bash
curl -s http://127.0.0.1:1234/api/v0/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen2-7b-instruct","messages":[{"role":"user","content":"hi"}]}' \
  | jq '{usage, stats}'
```

## Everything else: the OpenAI-compatible rail

Most local servers speak OpenAI's chat-completions shape. One adapter covers them:

```toml
[[providers]]
id = "ollama"
kind = "open_ai_compatible"
base_url = "http://127.0.0.1:11434/v1"
enabled = true
```

| Server | Base URL | Notes |
| --- | --- | --- |
| Ollama | `http://127.0.0.1:11434/v1` | Its native `/api` is richer; the compatible path is enough to start |
| llama.cpp / `llama-server` | `http://127.0.0.1:8080/v1` | |
| vLLM | `http://127.0.0.1:8000/v1` | |
| LM Studio (compatible mode) | `http://127.0.0.1:1234/v1` | Fewer facts than `lm_studio`; the gauges go quiet |
| OpenRouter / hosted OpenAI-shaped | `https://…/v1` | Set `credential_ref`; the key stays in the keyring |

The difference between the two kinds is not marketing. `open_ai_compatible`
reports a model list and an answer. It does not report quantization, load state,
time to first token, or a trustworthy context window, so the lab shows fewer
gauges — **and says which ones are missing rather than filling them with a
plausible number.** A 4,096-token guess is how a prompt gets truncated without
anybody noticing.

<Aside type="note" title="Not OpenAI-shaped is not a synonym for unsupported">
Anthropic's Messages API and Gemini's API are different shapes, not different
dialects. Each needs its own variant *and* its own adapter, in that order, which
is why neither is in v0.1 (D14). LM Studio also exposes an
Anthropic-compatible endpoint, so the adapter can be developed and tested
offline before it borrows a cloud key.
</Aside>

## No implicit network

Nothing contacts a provider until you declare it *and* enable it. There is no
startup scan of `127.0.0.1`, no polling, and no background retry: an unreachable
provider is reported as unreachable and stays that way until something asks
again.

```bash
acpilot models probe lmstudio     # the only thing that talks to the network
```

If a provider is down, the error is `model_unavailable` with a `502`: the daemon
is healthy, the thing behind it is not, and the UI may offer "retry" without
implying that Acpilot broke.

<Aside type="tip" title="Credentials are references, not values">
`credential_ref` names a keyring entry, exactly like a profile secret. The API
returns the reference; it never returns the token, and `acpilot provider show`
prints `keyring:lmstudio/work` rather than something you should not paste into an
issue.
</Aside>
