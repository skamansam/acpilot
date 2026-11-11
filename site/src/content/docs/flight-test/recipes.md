---
title: Recipes
description: One model, one prompt, one budget — as a file you can diff, review, and share.
---

<span class="pill pill--hangar">hangar</span> &nbsp; Models phase (P4.5-3)

A **recipe** is a tuned configuration for one model of one provider. It is TOML in
the profile, and you can review it in a diff. Sharing one means sending a file.
That is the same argument that made agents a manifest and language servers a
table.

```toml
# ~/.config/acpilot/profiles/work/recipes/corpus-scraper.toml
schema_version = 1
id = "corpus-scraper"
label = "Corpus scraper (Qwen 7B)"
provider_id = "lmstudio"
model_id = "qwen2-7b-instruct"

system_prompt = """
You extract structured facts from a documentation corpus.

Rules:
- Answer only from the files provided. If a fact is absent, say absent.
- Cite the file path and line range for every fact.
- Never invent an API that is not in the corpus.
"""

[sampling]
temperature = 0.2
top_p = 0.95
max_tokens = 1200
seed = 42
stop = []

[context]
window = 32768
reserve_output = 1500
strategy = "truncate_oldest"
max_corpus_bytes = 262144

[tools]
read_files = true
write_files = false
run_commands = false
search = true
max_steps = 8

notes = "Good at extraction, bad at arithmetic. Do not promote without a probe."
```

## Fields

| Field | Type | Notes |
| --- | --- | --- |
| `schema_version` | integer | `1` today. Migrations are explicit. |
| `id` | string | Also the file name. It must match. |
| `label` | string | Display name in the lab and the agent list |
| `provider_id` | string | Must exist in `providers.toml` and be enabled |
| `model_id` | string | Must appear in that provider's model list |
| `system_prompt` | string | The master prompt. Multi-line strings are the norm |
| `sampling.temperature` | float | `0.0`–`2.0` |
| `sampling.top_p` | float | `0.0`–`1.0` |
| `sampling.max_tokens` | integer? | Absent means the server's default |
| `sampling.top_k` | integer? | Sent when set. A server that refuses it triggers one retry without it, reported on the run |
| `sampling.repeat_penalty` | float? | As above |
| `sampling.seed` | integer? | Makes a run reproducible *on the same server and model* |
| `sampling.stop` | string[] | Sequences that end generation |
| `context.window` | integer | Must not exceed the model's reported window |
| `context.reserve_output` | integer | Held back so the answer cannot be truncated by its own budget |
| `context.strategy` | enum | `truncate_oldest`, `summarize`, `refuse` |
| `context.max_corpus_bytes` | integer | Ceiling before tokenisation |
| `tools.*` | bool | A **ceiling**, not a grant. The broker still answers each call |
| `tools.max_steps` | integer | `1`–`50`. The loop stops here and says so |
| `notes` | string? | What it is good at, and what it is bad at |

## Validation, at load

The loader checks recipes when it reads them, not when a run is half-finished:

- **Unknown keys are errors.** This is the same rule as configuration and
  manifests. A knob that silently does nothing is a bug report waiting to happen.
- **The loader checks the window against the provider.** If the server reports 32,768
  and the recipe says 65,536, the recipe is wrong and the loader rejects it. If
  the server reports nothing, the recipe must state a number. The run record
  marks that number `context_source: recipe`, so nobody later mistakes it for a
  fact.
- **The loader refuses tools the model cannot call**, with an explanation. It
  does not quietly drop them from the schema. A prompt that says "you may run
  commands" over a model with no tool support teaches the model to hallucinate
  commands.
- **The budget must fit.** `reserve_output` plus the corpus estimate must leave
  room for the system prompt and tool schemas, or the recipe does not load.
- **The loader names a missing provider or model.** The API returns `404
  not_found`, and the CLI exits `3`, when the profile loads, not mid-turn.

<Aside type="note" title="Hot reload, last-good wins">
The daemon watches the recipe directory with a 200 ms debounce. A saved edit
applies to the next run. A syntax error keeps the last good version and raises a
notice in every attached UI. It does **not** fail a turn that is already
streaming. An editor that loses your work because a file was half-written while
you saved it is a bad editor.
</Aside>

## Why files and not a database

Because a prompt change is a code change. It deserves a diff, a review, and a
`git revert`. A workspace can check in recipes under `.acpilot/recipes/`, and
they travel with the project. That is how a team ends up with one corpus scraper
instead of five, each slightly worse.

## What a recipe is not

- **Not an agent.** It has no process, no session, and no permissions. Promotion
  is the step that makes it one. See [from model to agent](../model-to-agent/).
- **Not portable across models for free.** `provider_id` and `model_id` are
  fields, not hints. Swapping models is a new recipe. The prompt that suits a 7B
  model is not the prompt that suits a 70B one. Pretending otherwise produces
  confident nonsense with a provenance trail.
