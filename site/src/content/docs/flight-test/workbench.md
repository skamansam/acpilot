---
title: The workbench
description: Three panes, honest gauges, and the exact JSON that was sent.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P4.5-8

The lab is one screen with three panes and a strip of instruments. It is a client
of the daemon API — the same recipes, the same runs, the same transcript as the
CLI and the browser. Close it mid-run and the run keeps going, because the
daemon is doing the work and the pane was only ever a window.

```text
┌ recipe ────────────────────────┬ corpus ────────────────────────┐
│ system prompt   ⌨              │ docs/**/*.md        41 files  │
│                                │ src/**.rs            8 files  │
│ temperature   0.20 ────────●   │ ───────────────────────────   │
│ top_p         0.95 ──────●     │ budget 6,120 / 32,768 tokens  │
│ max_tokens    (server)         │ window   server (32,768)      │
│ seed          42               │ strategy truncate oldest      │
│                                │                               │
│ tools  ☑ read  ☐ write         │                               │
│        ☐ run   ☑ search        │                               │
└────────────────────────────────┴───────────────────────────────┘
┌ output ──────────────────────────────────────────────────────────┐
│ ▍ first token 118 ms   ·   41.2 tok/s   ·   stop: eosFound       │
│                                                                  │
│   The parser rejects `--strict` on this branch because …         │
└──────────────────────────────────────────────────────────────────┘
```

## The recipe pane

The **master prompt** is a real editor, not a textarea: the same buffer, undo
grouping, and search as the main editor, because the prompt is the artefact you
will edit most and copying it into a web form would be a downgrade.

Every knob shows its value, its source, and its effect. `max_tokens: (server)`
means the recipe does not set it, not that it is zero. `window: server (32,768)`
means the number came from the provider — if the provider did not report one, the
field is empty and says **unknown**, and the recipe must supply a value before a
run is accepted.

<Aside type="note" title="A knob that does nothing is worse than no knob">
Some servers accept `top_k` and ignore it, some reject it outright, some honour it
only on certain models. Acpilot cannot see inside the sampler, so it does not
pretend to: when a server rejects an optional parameter, the request is retried
without it and the run record names it under **refused parameters**. That list is
more useful than another slider, and it is honest about what it can know.
</Aside>

## The corpus pane

Corpus is how the model sees your project: globs, not pasted text.

```toml
[[corpus]]
glob = "docs/**/*.md"
max_files = 200
max_bytes = 65536
```

The pane resolves the globs against the **workspace root** — on the host the
workspace lives on, so a remote workspace reads remote files — and shows the exact
file list with sizes and a running budget meter. Ignore rules apply, binary files
are skipped, and the order is stable so two runs of the same recipe on the same
commit are the same request.

Resolution happens *before* the run, which is the point: "it worked yesterday" is
almost always a corpus that grew by a changelog file.

## The output pane

Streaming text, and a strip of instruments that only shows measured values:

| Gauge | Shown when | Notes |
| --- | --- | --- |
| Time to first token | The provider reports it | The number that tells you the model is actually loaded |
| Tokens / second | The provider reports it | LM Studio's `stats`; absent on plain OpenAI-shaped servers |
| Tokens used | Always | Marked **estimated** until the server's `usage` arrives |
| Stop reason | Always | `eosFound`, `maxPredictedTokensReached`, or our own step cap |
| Warnings | When non-empty | Refused parameters, truncated corpus, repaired tool call |

<Aside type="caution" title="Estimated is a different colour on purpose">
Pre-flight counts are a bytes/4 heuristic with a documented error band. When the
answer comes back with `usage`, the run record is reconciled and the flag flips.
The UI does not quietly swap one for the other: a number you cannot trust is
styled like a number you cannot trust, which is the entire etiquette of an
instrument panel.
</Aside>

## The effective-request inspector

A debug drawer showing the exact JSON body that was sent — messages, sampling
parameters, tool schemas, and the model id — plus anything the adapter added or
dropped on the way.

Most local-model debugging is answering "did the server get what I think I sent?"
with evidence rather than opinion. This drawer is that evidence, and it is one
keystroke from the transcript, so it can be pasted into a bug report that gets
solved.

## Compare mode

Two runs, side by side: token delta, latency delta, stop reason, and a diff of the
answers. Runs are immutable records, so comparison never re-rolls the dice — if
you want a new sample, that is a new run, clearly labelled.

Promotion lives here, one keystroke away, and it warns first:

```text
promote recipe "corpus-scraper" as agent "corpus-scraper"?
  ⚠ tool calls parsed in 7 of 10 runs (70%)
  ⚠ this model has never passed the tools probe
  ✓ window 32,768, reported by the server
```

## Keyboard first

Every pane is reachable without a mouse, the prompt editor keeps the keymap you
set globally, and no gauge is the only way to learn something: `acpilot runs show
<id> --json` returns the same record the pane is drawing. A workbench that can
only be operated by pointing at it is a kiosk.
