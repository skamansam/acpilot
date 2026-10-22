# P0-1 — two real agents through one ACP client

Measured 2026-10-21 on Linux (WSL2, x86_64). The instrument is
[`probes/acp-handshake`](../../probes/acp-handshake/src/main.rs) — one client, one
code path, two agents — built on `agent-client-protocol` 3.4.0. It performs
`initialize` → `session/new` → `session/prompt`, records every `session/update`,
answers `session/request_permission` by auto-deny, then calls `session/load` and
records the replay. Every frame on the agent's stdio is captured too, so the wire
can be checked against the typed view.

```bash
cargo run -- --agent claude --json /tmp/claude.json
cargo run -- --command 'npx -y opencode-ai@latest acp' --json /tmp/opencode.json
```

The plan named `@zed-industries/claude-code-acp` and Gemini CLI. The first has been
renamed to `@agentclientprotocol/claude-agent-acp` (the old package still works but
prints a deprecation warning); Gemini CLI was not installed or signed in on this
machine, so OpenCode was the second agent.

The fixture is [`probes/fixtures/acp`](../../probes/fixtures/acp): two files, one
subdirectory, deliberately untracked. The prompt is `list the files here`, which
forces at least one tool call without asking the model to change anything.

## Verdict

1. **One client is enough, because every capability is optional.** Both agents
   speak protocol v1 with the same four calls. Their `initialize` answers differ in
   `sessionCapabilities` (Claude has `delete` and `additionalDirectories`, OpenCode
   has `fork`) and `session/new` differs in shape: Claude returns `modes` *and*
   `configOptions`, OpenCode returns only `configOptions`. A client that reads
   `session.modes` without a null check crashes on one of the two.
2. **The update surface is small.** Seven `sessionUpdate` kinds arrived in total
   (`agent_message_chunk`, `user_message_chunk`, `tool_call`, `tool_call_update`,
   `available_commands_update`, `usage_update`, `config_option_update`) — plus
   `session_info_update` on replay. No `plan` and no `agent_thought_chunk` appeared
   once, even though the schema and the plan capability allow them; P2-3 must
   support them but cannot be tested against these two agents.
3. **A denial is not an error.** `session/request_permission` arrived once, for a
   file write, with `allow_once` / `allow_always` / `reject_once` options. Answering
   `reject` left the turn running: the tool call became `failed` with the text
   `User refused permission to run tool`, the model explained the refusal, the file
   was never written (verified on disk), and `stopReason` was still `end_turn`.
   The broker (P2-6) records a decision, not a failure.
4. **`session/load` works on both, and is not a transcript.** Both advertise
   `loadSession: true`; both answer successfully. But the replay is *condensed*:
   Claude replayed 5 updates for a turn that had streamed 29, OpenCode 7 for 16.
   Chunks are merged into one message each. P2-8's append-only `session.jsonl`
   is therefore still the source of truth; `load` is for resuming a session the
   daemon no longer remembers, not for rebuilding a UI.
5. **The agent's version is a dependency with silent failure modes.** The installed
   OpenCode 1.15.13 answered `session/prompt` with `end_turn`, zero tokens, and no
   error, and printed nothing — its free tier requires ≥ 1.18.0. `npx
   opencode-ai@latest` (1.19.3) worked. A run record that only stores `stopReason`
   would have called that success; store the agent's `agentInfo` version and the
   token counts beside it.
6. **There is traffic outside `session/update`.** Claude sent
   `_auth/status_update` (an `_auth/`-namespaced notification carrying account
   identity — redacted here) two or three times per session, and put `steering` and `claudeCode`
   hints in `_meta`. The SDK tolerated the unknown method without failing. Clients
   must ignore unknown notifications rather than close the connection.

## Capabilities as advertised

| | `@agentclientprotocol/claude-agent-acp` 0.94.0 | OpenCode 1.19.3 |
| --- | --- | --- |
| `agentInfo` | `Claude Agent`, name `@agentclientprotocol/claude-agent-acp` | `OpenCode`, no title |
| `protocolVersion` | 1 | 1 |
| `loadSession` | `true` | `true` |
| `promptCapabilities` | image ✅, audio ❌, embeddedContext ✅ | image ✅, audio ❌, embeddedContext ✅ |
| `mcpCapabilities` | http ✅, sse ✅ | http ✅, sse ✅ |
| `sessionCapabilities` | `list`, `delete`, `additionalDirectories`, `resume`, `close` | `list`, `resume`, `close`, `fork` |
| `authMethods` | `[]` (already signed in; `auth.logout` advertised) | `opencode-login` |
| `initialize._meta` | `steering.supported`, `claudeCode.promptQueueing`, `authStatus` | absent |
| `session/new` fields | `sessionId`, `modes`, `configOptions` | `sessionId`, `configOptions` |
| modes | `default` (Manual), `acceptEdits`, `plan`, `auto`, `bypassPermissions` | none; a `mode` config option has `build`/`plan` |
| config options | `mode`, `model`, `effort`, `fast`, `agent` | `model`, `mode` |

`session/set_config_option` works on OpenCode 1.19.3 (setting the model it already
had returned the full updated option list and emitted a `config_option_update`
update). Setting a model id copied from 1.15.13's list failed with JSON-RPC
`-32602 Invalid params: model not found` — the agent advertises a value it then
refuses, so a refused config write is a recoverable warning, not a fatal error.
Claude's options (`mode`, `effort`, `fast`, `agent`) were not written by this
probe, so their accepted value shape remains untested; `mode` also has its own
`session/set_mode` path, which this probe did not call.

## One turn, in events

| run | wall clock | first update | prompt-phase updates | load replay | wire lines |
| --- | --- | --- | --- | --- | --- |
| Claude, `list the files here` | 17,829 ms | 7,660 ms (`available_commands_update`) | 36 | 7 | 54 |
| Claude, write `out.txt` (denied) | 6,890 ms | 1,583 ms | 29 | 5 | 48 |
| OpenCode, `list the files here` | 13,320 ms | 2,104 ms | 16 | 7 | 31 |
| OpenCode, `Reply with exactly: ok` | 7,440 ms | 2,485 ms | 4 | 2 | 16 |

Every run ended with `stopReason: "end_turn"`. `cancelled`, `max_tokens`,
`max_turn_requests` and `refusal` were not observed — they need a cancellation or a
cap, which this probe does not perform. `usage_update` carries `used`/`size` during
the turn; OpenCode adds `cost`, Claude adds `_meta["_claude/model"]`. Claude puts
token counts in the `session/prompt` response `_meta.quota`; OpenCode puts them in
the response's `usage` (`inputTokens`, `outputTokens`, `totalTokens`,
`cachedReadTokens`) — which the typed response **silently dropped**, because
`usage` on `PromptResponse` is behind the SDK's `unstable_end_turn_token_usage`
feature. It was visible only on the wire capture. Enable that feature in P2 or the
run record will claim zero tokens for a turn that used 14,158.

The shape of a tool call is not stable across its lifetime:

```json
{"sessionUpdate":"tool_call","toolCallId":"toolu_…","title":"Terminal","kind":"execute","status":null,"rawInput":{}}
{"sessionUpdate":"tool_call_update","toolCallId":"toolu_…","title":"ls -la …/acp","rawInput":{"command":"ls -la …"}}
{"sessionUpdate":"tool_call_update","toolCallId":"toolu_…","status":"completed"}
```

The first `tool_call` can be a placeholder with an empty `rawInput` and no status;
the title and arguments arrive in the following updates. A UI that renders the
first event as the final one shows a blank card until completion, and a client
that keys tool calls by title instead of `toolCallId` merges two `ls` calls.

`available_commands_update` is not a startup event: Claude sent it three times
during the turn, once before any text and twice later. Commands must be replaceable
mid-turn (`P2-3`'s `CommandsChanged`), not merged once at session creation.

## Permission

The request, verbatim except for path shortening:

```json
{
  "sessionId": "2d8d44ee-…",
  "toolCall": {
    "toolCallId": "toolu_01E8Jv8pzzvgj8QAzWHJx8JY",
    "name": "Write",
    "title": "Write out.txt",
    "kind": "edit",
    "status": "pending",
    "rawInput": {"file_path": "…/acp/out.txt", "content": "hello\n"},
    "content": [{"type": "diff", "path": "…/acp/out.txt", "oldText": null, "newText": "hello\n"}],
    "locations": [{"path": "…/acp/out.txt"}]
  },
  "options": [
    {"optionId": "allow-once", "name": "Yes", "kind": "allow_once"},
    {"optionId": "allow-with-updates", "name": "Yes, allow all edits during this session", "kind": "allow_always"},
    {"optionId": "reject", "name": "No", "kind": "reject_once"}
  ]
}
```

The answer is a normal JSON-RPC result, not a notification:

```json
{"jsonrpc":"2.0","id":0,"result":{"outcome":{"outcome":"selected","optionId":"reject"}}}
```

The broker is not optional. The request is a *blocking* call from the agent's point
of view: no prompt progress happens until it is answered, and the agent offers a
diff and locations before you decide. That is enough to render the write exactly as
it will land, which is what the TUI's permission card needs; it is also why P2-6
must answer from a script as well as from a keyboard.

Denying is ordinary: `tool_call_update` → `status: "failed"`, content
`User refused permission to run tool`, no file on disk, turn ends `end_turn`.
Claude never asked for the read-only `ls` calls in `list the files here`; a
permission policy that expects every tool call to prompt will be wrong about reads.

## `session/load`

| | Claude | OpenCode |
| --- | --- | --- |
| advertised / answered | ✅ / ✅ | ✅ / ✅ |
| replayed updates | 5 (denied write turn) | 7 (`list` turn) |
| first replayed update | `session_info_update` | `user_message_chunk` |
| response body | `modes`, `configOptions` | `configOptions` |

The replay interleaves with the response: replayed notifications arrive *before*
`session/load`'s result, so a client must mark the load phase in advance or it will
render history as live tokens. Chunks are consolidated (one `agent_message_chunk`
per message, not 17). The turn's timestamps are not replayed, so a replayed session
cannot be put back on a timeline without the daemon's own record.

## Transport

ACP over stdio is **one JSON object per line** — no `Content-Length` framing like
LSP. The longest line observed was 32,148 bytes: a `session/update` carrying the
full command list, with every command's description. Nothing split a JSON object
across reads, but nothing promises that either.
Frames are JSON-RPC 2.0 with string ids from the client (`initialize`, `session/*`)
and integer ids from the agent (`session/request_permission` used `id: 0`).
Unknown methods (`_auth/status_update`) were ignored without an error; `_meta` is
where both agents put vendor extensions, and it is present on requests, responses,
and updates.

The SDK's `LineDirection` debug hook made this observable without a proxy. That is
the cheapest way to keep the wire visible in P2, and it is what a bug report about
"did the agent get what I sent" should attach.

## What this forces on P2

- **P2-3** must model `CommandsChanged`, `Notice`, `Compaction`, `Plan`, and
  `Thought` even though these runs never produced them, and must treat unknown
  `SessionUpdate` variants and unknown notifications as ignorable. Sequence numbers
  are ours to assign; the protocol gives none.
- **P2-6** permission options map one-to-one, and the outcome is `selected` with an
  option id or `cancelled`. Denial must be audited as a decision and must leave the
  turn alive. The auto-deny used here is exactly the "no answer" policy tests need.
- **P2-8** is still required: `load` replays a consolidated, timestamp-free history.
- **P2-2** must record `agentInfo.version` in the session record, enable
  `unstable_end_turn_token_usage` (or read `_meta`), and surface a turn that produced
  no text and zero tokens as a warning, because one of the two agents reported
  exactly that and called it success.
