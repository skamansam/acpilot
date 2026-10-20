---
title: The model lab
description: Point Acpilot at a model you already run, tune the prompt and the knobs, then keep the result as an agent.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P4.5

A **local model** is not an agent. It is an HTTP endpoint that continues text. It
has never heard of your repository, cannot open a file, and will cheerfully agree
to delete one.

The model lab is where you fix that. It is a workbench, not a chat window:

```text
provider ──► model ──► recipe ──► run ──► recipe, tuned ──► agent
 (a URL)     (an id)   (a file)   (a record)                (a manifest line)
```

Each step is a thing you can look at, diff, and throw away:

| Step | What it is | Where it lives |
| --- | --- | --- |
| Provider | A chat endpoint, declared and probed by you | `config.toml` / `providers.toml` |
| Model | One model id the provider reported | Discovered, never invented |
| Recipe | Prompt, sampling, context budget, tool ceiling | `profiles/<name>/recipes/<id>.toml` |
| Run | One request and one answer, recorded | `runs.jsonl`, immutable |
| Agent | A recipe promoted into `agents.toml` | Spawned like any other ACP agent |

<Aside type="note" title="The autopilot is still the autopilot">
The point of the lab is not to make the model smarter. It is to make the model's
limits *visible* before it is allowed to touch your files: which parameters the
server actually honours, how much window it really has, whether it can emit a
tool call at all, and how often that tool call parses.
</Aside>

## Why a lab instead of a settings pane

Because prompt tuning is an experiment, and an experiment without a record is a
rumour. The lab keeps the four things you need to answer "why was it better
yesterday?":

- **The exact request.** System prompt hash, sampling values, resolved corpus
  files, budget split. Replay re-sends it unchanged.
- **The exact answer.** Token usage, time to first token, stop reason, and the
  parameters the server quietly ignored.
- **The comparison.** Two runs side by side, with a delta in tokens and latency.
- **The verdict.** A promotion step that refuses to be quiet about weak
  tool-calling, and a manifest entry you can delete in one command.

## What this is not

- **Not a chat app.** There is no general-purpose assistant here. A recipe exists
  to become an agent, and the agent runs inside the permission broker.
- **Not a hosted service.** No model, no key, and no prompt leaves your machine
  unless you declare a provider that lives somewhere else.
- **Not a second brain for the editor.** The lab talks to the daemon API like
  every other surface; it holds no state of its own. There is one transcript, and
  it is the daemon's.

## What is built today

The contract is: this page and the five beside it, the OpenAPI types behind them,
and an ADR that says why. No server has been probed yet. P4.5 builds the crate,
the shim agent, and the lab in that order — with the API first, because a
workbench that invents its own state would be the sixth thing to disagree about
what happened.

<Aside type="caution" title="Altitude check">
Pages in this section describe P4.5. The [flight status](../status/) page is the
one that tells you whether it flies; this page tells you which runway was
planned.
</Aside>
