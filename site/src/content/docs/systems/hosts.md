---
title: "Hosts: SSH and WSL"
description: Where the code actually lives, and why Windows does not crawl through UNC paths.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P8 (SSH), P9 (WSL)

A **host** is a machine with a filesystem and processes. Local is a host. An SSH
box is a host. A WSL distribution is a host. A workspace lives on one, and
everything — files, terminals, agents, language servers — runs there.

## One trait, three implementations

```rust
trait Host {
    async fn read(&self, path: &str) -> Result<Vec<u8>>;
    async fn write(&self, path: &str, bytes: &[u8]) -> Result<()>;
    async fn spawn_pty(&self, cmd: &Command, size: Size) -> Result<Pty>;
    async fn spawn_agent(&self, spec: &AgentSpec) -> Result<Child>;
}
```

The point is not abstraction for its own sake. It is that a remote agent must not
get a second, worse implementation of "read a file" — the shadow-write guard, the
path canonicalisation, and the audit log have to apply identically.

## The helper

For SSH and WSL, the daemon uploads a small static binary (`acpilotd-helper`) and
talks JSON-RPC to it over stdio. Filesystem calls, PTYs, and spawns happen on the
far side of the connection, where the latency is zero instead of per-syscall.

```text
acpilotd ── ssh exec ──► acpilotd-helper (on the host) ──► fs / pty / spawn / lsp
```

<Aside type="caution" title="The Windows-specific reason">
`\\wsl.localhost\Ubuntu\...` works, and it is how you lose an afternoon: every
file operation pays a 9P round trip. Talking to a helper inside the distribution
is not a clever workaround, it is the only version that feels like a local
editor.
</Aside>

## What is preserved

- **Path spelling.** A path is stored as the host spells it; translation happens
  at the edges (clipboard, file dialogs, `--file` arguments), never in the middle.
- **Line endings.** CRLF files stay CRLF. A save is a write, not a conversion.
- **Exit codes and signals.** A killed command is a killed command, not a
  generic failure.

## Remote watching is polling

Until inotify-over-SSH exists, a remote workspace watches by polling (500 ms,
coalesced) with ETag revalidation. The UI shows a "remote" indicator, because a
stale tree that looks live is worse than a tree that admits it is a poll.

## Status

Not started. P0-4 proves ACP over an SSH exec channel; P0-5 proves the helper
round-trip and path translation before P8 and P9 build on them.
