---
title: In case of emergency
description: Frequently asked questions, a glossary, and the brace position.
---

<span class="pill pill--flying">flying</span> &nbsp; the answers &nbsp;·&nbsp; <span class="pill pill--hangar">hangar</span> &nbsp; the aircraft

## Frequently asked questions

### Surely this is just VS Code with a chat panel.

It is not. And don't call it Shirley.

A chat panel sits beside your editor. Acpilot *is* the editor, and the agent is a
crew member with a headset and a limited licence: it can propose a change, it
cannot sign for it.

### Is the autopilot going to fly us into a mountain?

No. The autopilot proposes; you approve. Anything outside the workspace is denied
before you are even asked, and every decision — including "allowed, once" — ends
up in the audit log. The log is the black box, except it is readable and you do
not have to fish it out of the sea.

### Which agents work?

If it speaks ACP, it flies natively. If it is a terminal program with a prompt,
it flies through the PTY adapter — freebuff, for instance, is the reference case
for that path. If it only speaks to a proprietary cloud through a vendor SDK and
a browser session, we cannot help, and neither can your firewall.

### Can I run an agent on a remote box?

That is the point of [hosts](../systems/hosts/). The agent runs where the code is.
Your laptop is a cockpit, not a runway.

### Windows?

Yes — via a helper inside WSL rather than UNC paths. `\\wsl.localhost` works in
the same way that walking to the destination works: technically correct, and it
takes all day.

### Why profiles?

Because the key you use for a client is not the key you use on a weekend. Mixed
credentials are the one failure this product cannot apologise its way out of.

### Why does the documentation sound like this?

Because documentation that reads like a terms-of-service update gets skimmed, and
skimming is how people miss the part about permissions. Aviation runs on
checklists and deadpan humour for the same reason: both keep you calm while the
gauges tell you something is wrong.

### When can I use it?

P2 — agents that actually run — is next. The [flight status](../status/) page is
updated in the same pull request as each phase, so it is the only schedule worth
trusting.

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

<Aside type="note" title="The brace position">
If something goes wrong: `acpilot session cancel`, then read the audit log. If
the daemon is unreachable, the sessions keep running — that is what a daemon is
for. Nothing is lost except your patience, which was never in the overhead bin.
</Aside>

## Still stuck?

Open an issue at [skamansam/acpilot](https://github.com/skamansam/acpilot/issues).
Include the output of `acpilot doctor` when it exists, and the last twenty lines
of the session transcript. Both are more useful than a screenshot of a spinner,
no matter how handsome the spinner is.
