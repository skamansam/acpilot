---
title: From model to agent
description: A recipe becomes an agent by becoming a process that speaks ACP — not by teaching the daemon about prompts.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P4.5-5, P4.5-9

Promotion is one command, and the command writes one manifest entry. That is the
whole trick:

```bash
acpilot recipes promote corpus-scraper --as corpus-scraper
```

```toml
# ~/.config/acpilot/profiles/work/agents.toml
[[agents]]
id = "corpus-scraper"
name = "Corpus scraper (Qwen 7B)"
transport = "acp-stdio"
command = "acpilot-agent-lm"
args = ["--recipe", "corpus-scraper"]
restart = "on-crash"
# recipe: profiles/work/recipes/corpus-scraper.toml (mtime 2026-10-20T14:02:11Z)
```

From that moment it is not a model. It is an agent like any other: `acpilot agents
ls` lists it, the TUI's session picker shows it, the transcript records it, the
permission broker guards it, and `acpilot agents rm corpus-scraper` removes it
without a migration.

## What actually happens in a turn

`acpilot-agent-lm` is a small ACP **agent** process. It owns the conversation with
the model, and it owns nothing else:

```text
              ┌──────────────── acpilotd ────────────────┐
              │  session · permissions · files · host    │
              └───────▲──────────────────────┬───────────┘
        ACP callbacks │                      │ stdio ACP
                      │                      ▼
              ┌───────┴──────────────────────────────┐
              │        acpilot-agent-lm              │
              │  prompt assembly · tool parse · loop │
              └───────┬──────────────────────────────┘
                      │ HTTP (chat completions)
                      ▼
              LM Studio on 127.0.0.1:1234
```

The model never gets a file descriptor. It gets a tool *schema*, and the shim
turns its tool calls into ACP client calls that the daemon may refuse:

| The model asks for | The shim calls | Who decides |
| --- | --- | --- |
| Read `docs/api.md` | `fs/read_text_file` | Policy: reads inside the workspace are allowed by default |
| Write `src/routes.rs` | `fs/write_text_file` | Permission broker: diff shown, human or rule answers |
| Run `cargo test` | `terminal/create` | Policy: workspace root, command classification, audit |
| Anything else | `session/request_permission` | Strictest case by default |

So a local model inherits three things it would otherwise need its own
implementation of: the **permission broker**, the **host abstraction** (SSH and
WSL come free, because the daemon reads the file where it lives), and the
**transcript** with its replayable sequence numbers.

<Aside type="note" title="Why a separate process and not a function call">
Because the alternative is a second ACP client inside the daemon: its own tool
schema, its own permission round-trip, its own cancellation semantics. Two
implementations of the same protocol is how an editor ends up with two behaviours
for "cancel", and the one you tested is never the one the user hit.
</Aside>

## The loop, and where it stops

Each turn is: assemble prompt → call model → if the answer is a tool call, execute
it through ACP and append the result → repeat.

Limits are explicit because a local model with a broken tool-call format will
otherwise loop until the context window catches fire:

- **`max_steps`** from the recipe (1–50). At the cap the turn stops with a stop
  reason of `step_limit`, not a hang and not a silent truncation.
- **One repair attempt.** A tool call that does not parse gets one corrective
  message with the schema restated. A second failure becomes a visible notice and
  a plain-text answer, because pretending progress is worse than admitting the
  format.
- **Cancellation is immediate.** `session/cancel` aborts the HTTP stream rather
  than waiting for the next chunk; a 40-token-per-second model should not hold
  `Ctrl-C` hostage.
- **Overflow is an event.** Truncate, summarise, or refuse — the recipe says which,
  and the record carries the numbers either way.

## Promotion is honest about the odds

The command warns before it writes:

- The model never passed the tools probe (`tools: false`): it can still answer
  questions, but the schema is not offered, and the agent will say so rather than
  describe commands it cannot run.
- Tool calls parsed in fewer than 90% of the recipe's recent runs: that number is
  in the manifest comment, so the next person to read the file inherits the
  warning instead of discovering it.
- The recipe's window exceeds what the host can load: LM Studio will be the one to
  fail, and it will fail at the worst moment.

Warnings do not block. A workbench that refuses to let you try is not a workbench.
But none of them are silent, and none of them are buried in a log file.

## Reversing it

```bash
acpilot agents rm corpus-scraper      # removes the manifest entry
acpilot recipes rm corpus-scraper     # removes the recipe, if you want that too
```

Two files, two commands, no state to unwind. The model was never installed by
Acpilot and the transcript is a journal, not a dependency.
