---
title: In case of emergency
description: Frequently asked questions, a glossary, and the brace position.
---

<span class="pill pill--flying">flying</span> &nbsp; the answers &nbsp;·&nbsp; <span class="pill pill--hangar">hangar</span> &nbsp; the aircraft

## Frequently asked questions

### Surely this is just VS Code with a chat panel.

It is not. And do not call it Shirley.

A chat panel sits beside your editor. Acpilot *is* the editor. The agent is a
crew member with a headset and a limited licence. It can propose a change. It
cannot sign for it.

### Is the autopilot going to fly us into a mountain?

No. The autopilot proposes and you approve. The policy denies anything outside
the workspace before it asks you. Every decision, including "allowed, once",
goes into the audit log. The log is the black box, but it is readable and you do
not have to fish it out of the sea.

### Which agents work?

If it speaks ACP, it flies natively. If it is a terminal program with a prompt,
it flies through the PTY adapter. Freebuff is the reference case for that path.
If it only speaks to a proprietary cloud through a vendor SDK and a browser
session, we cannot help. Neither can your firewall.

### Can I run an agent on a remote box?

That is the point of [hosts](../systems/hosts/). The agent runs where the code
is. Your laptop is a cockpit, not a runway.

### Windows?

Yes. We use a helper inside WSL instead of UNC paths. `\\wsl.localhost` works in
the same way that walking to the destination works: it is technically correct,
and it takes all day.

### Why profiles?

Because the key you use for a client is not the key you use on a weekend. Mixed
credentials are the one failure this product cannot apologise its way out of.

### Can the model lab run with no internet at all?

Yes. A local model on `127.0.0.1`, a corpus read from disk, and a recipe in the
profile keep everything on the machine. The daemon makes no calls you did not
ask for. Declare a provider, probe it, and the rest is a socket to your own GPU.

Looks like I picked the wrong week to stop checking egress logs. Fortunately,
this one is short enough to read standing up.

### Do I need a GPU the size of a wardrobe?

No. You need a model small enough for the memory you have. The lab is honest
about which one that is: the gauges show the reported context window, the
quantization, and whether the server loaded the model. A 70B model that meets
8 GB of unified memory says "the plane! the plane!" and then exits.

Start small. Measure tool-call validity. Promote only what parses.

### Can something else drive my agent remotely?

Yes. A remote driver connects over ACP with a token and a pairing code. It can
open a session and prompt. It cannot approve its own writes unless you grant
that scope, per driver, on purpose. See
[remote control](../flight-test/remote-control/).

We have clearance, Clarence. The tower clears. The passenger does not.

### Why does the documentation sound like this?

Because documentation that reads like a terms-of-service update gets skimmed.
Skimming is how people miss the part about permissions. Aviation runs on
checklists and deadpan humour for the same reason: both keep you calm while the
gauges tell you something is wrong. Every so often it also sounds like a 1980
film about an airplane, because the alternative is sounding like a release note.

### When can I use it?

The engine phase (P2) is next. We update the [flight status](../status/) page in
the same pull request as each phase. It is the only schedule worth trusting.

### Was the sequel better?

No. Sequels rarely are. The next version will be called v0.2, and it will contain
a working agent loop instead of a joke about one.

## Glossary, as read by the cabin crew

| Term | Meaning |
| --- | --- |
| **Autopilot** | The agent. Very capable. Not in command. |
| **Vector** | Where a change is going: file, range, intent. |
| **Clearance** | A permission decision, with a scope and an author. |
| **Squawk** | A profile identifier. Two aircraft, two squawks, no confusion. |
| **Black box** | The append-only transcript: `session.jsonl`, replayable. |
| **Cabin pressure** | Context window. When it drops, the agent starts repeating itself. |
| **Turbulence** | Non-determinism. Normal. Fasten your seatbelt and re-read the diff. |
| **Holding pattern** | A pending permission request, waiting for a human. |
| **Mayday** | `Ctrl-C`. Cancels the turn, not the session. |
| **Runway** | The workspace root. Nothing takes off from outside it. |
| **Flight plan** | The diff you approved before the agent started. |
| **Model lab** | The workbench: a provider, a prompt, a budget, and the record of what happened. |
| **Recipe** | One model plus one prompt plus one budget, as a file. Not an agent until promoted. |
| **Autopilot (local)** | A model you already run. Very confident. Reads nothing it is not handed. |
| **Tower** | A remote driver over the ACP bridge. Clears, routes, does not sign. |
| **Vector, Victor** | The routing question: which provider, which model, which corpus. |

<Aside type="note" title="The brace position">
If something goes wrong, run `acpilot session cancel` and read the audit log. If
the daemon is unreachable, the sessions keep running. That is what a daemon is
for. Nothing is lost except your patience, which was never in the overhead bin.
</Aside>

## Still stuck?

Open an issue at [skamansam/acpilot](https://github.com/skamansam/acpilot/issues).
Include the output of `acpilot doctor` when it exists, and the last twenty lines
of the session transcript. Both are more useful than a screenshot of a spinner,
no matter how handsome the spinner is.
