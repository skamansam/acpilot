# 0015 — Language servers stay on the workspace host

- **Status:** accepted
- **Date:** 2026-11-22
- **Deciders:** project maintainers
- **Relates to:** ADR 0004 (one daemon per user), ADR 0014 (language server
  catalog), P4.6-8 (wasm grammars), P7 (language intelligence)

## Context

The UI must feel responsive. A client can download a grammar or a language
server. That fact does not decide where the program runs.

The TUI, GUI and web UI are thin clients of one daemon. A workspace can live on
the local machine, an SSH host or a WSL distro. A language server indexes the
workspace. It reads build files, dependencies and source files. It answers with
paths on the host that owns those files.

A grammar is different. The editor parses the open buffer on every edit. That
work must stay beside the buffer. A network hop on the keystroke path would add
latency and couple typing to the daemon.

## Decision

1. **Language servers run on the workspace host.** The daemon starts and owns
   them through the `Host` trait. The daemon is the LSP client. Every UI reads
   diagnostics, references and formatting through the API.
2. **Grammars and highlighting run in the UI.** The UI loads a prebuilt
   tree-sitter wasm module and its queries. A signed extension or catalog can
   supply those assets. The UI caches them. The daemon does not parse the buffer
   for the editor.
3. **Artifact delivery is separate from execution.** A client may fetch a
   grammar from the daemon or from a URL the daemon names. A client may cache a
   server binary, but the binary runs where the files are. A remote host installs
   and runs its own server. A client does not ship an executable to a remote
   host.
4. **A local sidecar needs a new decision.** A future local-only mode may run a
   server beside a UI that shares the workspace filesystem. It must keep the same
   trust pins and pass a measured latency test. It is not the default and it is
   not in v0.1.

## Why this is the responsive choice

- **Startup and indexing dominate.** A warm `rust-analyzer` answers a references
  request in about 2 ms. The first request took about 2.3 s because it paid for
  spawn and index. One extra local HTTP hop is small beside that cost.
- **One server serves every UI.** A second window, the GUI and a browser tab
  share the same index. A client-side server would duplicate the process and the
  memory for each UI.
- **Remote workspaces keep working.** A language server on a remote client
  cannot read the remote files. Sending every file over the API would be slower
  and would widen the trust boundary.
- **Trust stays in one place.** ADR 0014 verifies a signed catalog and pins the
  executable. The daemon enforces that pin. A client-side copy would need the
  same review and the same pin store on every device.
- **Typing stays local.** The buffer, the rope and the tree-sitter parse never
  cross the API on a keystroke. This is the part that must meet the frame budget.

## Consequences

- The daemon pays for one warm server per language per workspace. It keeps the
  server alive between requests.
- Diagnostics can arrive after a short delay. The UI must show progress and keep
  the last answer. It must not block typing.
- The API carries semantic requests and answers. It does not carry the keystroke
  stream.
- Grammar assets need a version, a signature and a cache on each client. A stale
  grammar is a UI bug, not a daemon error.
- A local client still has one HTTP hop for semantic requests. Measure that hop
  before adding a sidecar. Optimize request shape, cancellation and caching
  first.
- A future local sidecar would add a second process and a second index. It needs
  a clear owner and a shutdown path.

## Alternatives considered

- **Run every language server in the UI.** Rejected. A remote UI has no local
  files. A local UI duplicates the index for each window. The trust pin would
  move to every client.
- **Keep grammars in the daemon.** Rejected. Parsing on every edit would cross
  the API and miss the frame budget.
- **Let a client run a server against files read through the daemon.** Rejected.
  Every file read would cross the transport. Paths, permissions and build tools
  would not match the host.
- **Install servers on first open.** Rejected by ADR 0014. An install is
  explicit, verified and reviewed.
- **Use tree-sitter only and drop LSP.** Rejected. A parse tree cannot answer
  scope-aware references, diagnostics or formatting.
