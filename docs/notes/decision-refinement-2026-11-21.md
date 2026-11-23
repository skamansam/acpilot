# Decision refinement — 2026-11-21

The open-decisions epic was answered in the GitHub project on 2026-11-21. This
note is the reconciliation: which answers are decisions, which are directions
that still need a design, what each one invalidates. The order to build them
so nothing gets rewritten twice.

It is not an ADR. Each settled item below still needs one before code, and the
public pages stay as they are until that ADR is accepted. A page that promises
managed downloads or JWT login before either exists is the bug the docs rules
warn about.

## The answers

| ID | Issue | Answer | State |
| --- | --- | --- | --- |
| D2 | #228 | Presence, cursors and follow-along. The host UI is the only writer. No CRDT | direction, design needed |
| D3 | #229 | Unsure. Wants a feature-parity discussion, possibly embedding the desktop UI | deferred |
| D5 | #231 | Both local user/password and OIDC, each returning a JWT | direction, ADR needed |
| D7 | #233 | One command with subcommands | direction, ADR needed |
| D8 | #234 | Compare the three at P7-1 | deferred |
| D10 | #236 | One daemon with N stores | already built. Box was stale |
| D11 | #237 | One database per profile | decided in ADR 0007. Box was stale |
| D12 | #238 | Managed downloads with checksums, per-language configuration, ~30-language research | direction, ADR needed |
| D13 | #239 | "Talk when implementing" | already decided in ADR 0012. GitHub item is stale |
| D16 | #242 | Start at bytes/4, then calibrate per model from observed counts | direction |
| D18 | #244 | Talk later | deferred |
| D21 | #247 | Possibly one database per chat group per profile | open, depends on D22 |
| D22 | #248 | Self-hosted group relay, group UUID, transitive membership, failover | open design, not a scope |
| D26 | #252 | Extension actions as tools, but small-context models need a queryable registry | open design |

## Already true, so only the issue lane is stale

**D10 and D11.** The daemon serves every profile and opens one SQLite file per
profile (`StoreSlot::Lazy`, the `Stores` cache, ADR 0007). There is no
single-database-with-a-column variant in the tree. Marking the TODO boxes done
is a correction, not a new decision.

**D13.** ADR 0012 already settled inheritance on 2026-11-09: `extends` layers
non-secret settings, security-sensitive files are copied at creation. The GitHub
comment asks to discuss it when implementing. That is evidence the issue lane did
not follow the ADR, not evidence the decision is open.

The roadmap sync should close those three items rather than reopen them.

## D5 — authentication: JWT, from two credential sources

The answer is "both", so the decision is not local-users *or* OIDC. It is one
session credential — a JWT — with two ways to get it.

Proposed shape, for the ADR:

- **The unix socket is unchanged.** Filesystem permissions remain the auth story
  for the person at the machine. Login exists for TCP, the browser and the
  bridge, not for the local client.
- **Local accounts.** Username plus Argon2id password, stored in a daemon-level
  `users.sqlite`. First account created only over the unix socket, because a
  remote first-user race is a takeover.
- **OIDC.** Authorization code with PKCE for the browser, device code for the
  CLI. Verify `iss`, `aud`, `exp` and JWKS. Map to a local user by
  `(issuer, subject)`, never by email, with an explicit link command. Email is a
  mutable attribute, not an identity.
- **Tokens.** Short-lived access token, rotating refresh token, `jti` revocation
  list, signing key generated at first use and stored `0600`. Browser session in
  an `HttpOnly`, `SameSite=Strict` cookie with a CSRF token for mutations. CLI
  And scripts use a bearer header. A cookie also removes the WebSocket
  token-in-subprotocol workaround.
- **Authorization.** A token carries the profiles its subject may use. The
  profile header is still required on a mutating request, and the token's claim
  is the outer bound. A token cannot widen its own profile list.
- **Driver tokens stay separate.** Scopes and pairing are a different principal
  kind with a different lifecycle. Unifying them would give a driver a user
  session by accident.
- **Audit.** Login, refresh, revocation and failed attempts are audit rows.
  request logs carry the token id, never the token.

**Resolved by [ADR 0013](adr/0013-authentication-one-daemon-per-person.md):** the
daemon stays one per person. Login exists for the listeners that do not inherit
the socket's filesystem boundary (TCP and the browser). The socket stays
credential-free. Accounts, profile grants and refresh tokens live in a
daemon-level `users.sqlite` so a future shared daemon is a policy change rather
than a migration. V0.1 refuses a second user. First-account creation and
password reset are local-only. The static TCP token survives only until the
first account exists.

## D7 — one command, one binary

The answer is one command with subcommands. The concrete consequences:

- `acpilot daemon` runs the daemon. `acpilot agent-lm` runs the model shim.
  `acpilot gui` starts the desktop shell. The existing verbs stay.
- The spawn path re-executes `current_exe()` with `daemon` and the bind
  arguments, then detaches as it does today. `ACPILOT_DAEMON` still names a
  different binary. The sibling-`acpilotd` lookup goes away.
- The version comparison becomes mostly self-evident, but stays: `ACPILOT_DAEMON` and
  an older install answering a newer client are still mismatches worth refusing.
- Release artifacts (ADR 0011) ship one binary per platform instead of two. The
  `acpilotd` cargo target can remain for tests and development, or become a thin
  alias that executes the subcommand. The release must not require it.
- Service units become `ExecStart=/usr/bin/acpilot daemon`, which is why this is
  a P12 packaging change and not only a CLI rename.

Open for the ADR: whether `acpilotd` remains a shipped compatibility name for one
release. Removing it is cleaner. Keeping it costs one symlink and avoids breaking
the scripts people have already written.

## D12 — language servers are installed, explicitly

This reverses the "detect-only, no downloads" non-goal. The security property
that survives is **no silent install**:

- A signed static catalog, like the extension index (D17), with one row per
  language and target: version, URL, SHA-256, size, licence, archive kind.
- Installation is an explicit act (`acpilot lsp install <id>`, or a prompt that
  names the program and the source). Never during a keystroke, never in the
  background.
- Downloaded into `<data>/lsp/<id>/<version>/`, tested before extraction,
  archive-traversal tested with the extension loader's rules, executable bit
  set, then trust-pinned per P7-12. An update is a new explicit install. There
  is no auto-update channel.
- `lsp.toml` keeps per-language configuration and can pin a version or a path.
  Detection stays: an existing server on `PATH` wins unless the profile pins the
  managed one.
- Defaults are Rust-first where a Rust server exists. A research task covers ~30
  popular languages: server, licence, redistribution permission, targets, archive
  format and size. Licence review is part of the catalog, not an afterthought:
  some servers cannot be redistributed.
- Remote hosts: first cut refuses to download on the host and prints the install
  command. A helper-driven download is a later, separately audited step.

**Resolved by [ADR 0014](adr/0014-language-server-catalog.md):** v0.2. The
catalog is generated from reviewed per-server TOML, signed with its own ed25519
key, and only entries whose licence permits redistribution carry a download URL.
the rest stay known and detection-only. An install is explicit, tested before
extraction, and records the executable hash, so the install itself answers P7-12's
pin rather than adding a second prompt. Resolution is explicit command > pinned
managed version > newest managed install > `PATH`. A yanked or untrusted
managed binary starts nothing rather than falling through to another program.

## D16 — token counts start as a guess and become a measurement

Start at bytes/4 with `estimated: true`. Every provider response that reports
usage (LM Studio stats, OpenAI-compatible `usage`) records observed prompt and
completion tokens against the model id and the byte count. Once a model has
enough samples, the pre-flight estimate uses the local median ratio for that
model instead of 4. The sample count and the spread appear beside the
`estimated` flag. Local only. No telemetry, no shared corpus.

The interface already carries `estimated`, so this changes the estimator and the
run record, not the UI. The threshold and the statistic (median, not mean) are
the only numbers to settle when it is implemented.

## D2 — presence and follow, not distributed writes

This is the largest change to the session model, and it is a model, not a
feature flag. The useful reading of the answer:

- Presence is scoped to a workspace and authorized by the identity model from
  D5. A user list without authentication is a list of strangers.
- Events are ephemeral: join/leave, open file, cursor head and anchor,
  selections, viewport. Coalesced (a cursor is not a durable session event) and
  never replayed as history.
- **Request cursor** asks the peer's client to publish its workspace and open
  files. Only paths under that workspace root are listed. An open file outside
  it is omitted, exactly as the answer says.
- **Share tab** sends a reference (workspace id, relative path, revision) into a
  chat room, not file contents. The receiver's daemon resolves it locally.
- **Follow** subscribes a client to one user's cursor and viewport, scrolling
  and highlighting locally. Read-only. A follower who types either leaves
  follow or is refused. The answer's "only one would matter" is the property to
  keep.
- The host's daemon remains the only writer, so ETag last-write-wins still
  covers the host's own windows and no CRDT is needed. Presence is not
  co-editing, and the v0.1 non-goal stays true.
- Transport: one workspace-scoped stream on the daemon
  (`/v1/workspaces/{id}/presence` plus `/stream`), not a peer-to-peer mesh. A
direct connection is a later optimisation. The event shapes must not assume it.

**Designed in [`presence-follow-2026-11-21.md`](presence-follow-2026-11-21.md).**
ADR 0013's one-daemon-per-person answer splits D2 in two. Presence between one
person's own clients (TUI, GUI, browser) needs no new trust boundary. The first
version ships that half. Presence across daemons needs the D22 relay, a
server-asserted or signed roster. Group membership. The frames are
transport-neutral so the second half is a carrier, not a rewrite. A follow is
read-only: coordinates and viewports, never text or keystrokes.

## D21 and D22 — chat is a separate, federated subsystem

The answers describe a system, not a task list. It has a group UUID, an invite
blob carrying connection information, transitive membership, propagation between
servers, and failover when a server drops. Group-scoped databases that may run
for years.

Before P13-2 is code, three questions need answers:

1. **Trust model.** Does a relay see message bodies, or is the group
   end-to-end encrypted with per-device keys? Self-hosting keeps the operator
   honest but does not make the relay blind. This decides storage, search,
   retention and the meaning of "who can read the database".
2. **Identity.** Are members D5 accounts on each other's relays, or group key
   pairs independent of any daemon account? Transitive membership needs a
   verifiable identity, and "any user connected to any user" is a graph, not a
   list.
3. **Ordering.** Multiple relays propagating messages need a canonical order.
   The simplest defensible model is one authoritative relay per group at a time,
   with failover resuming from the last acknowledged sequence. Gossip without a
   leader needs a CRDT or a vector clock, which is a much larger promise.

D21 follows: one database per group per profile is the natural unit if groups
are long-lived and independently retainable. A single `chat.sqlite` per profile
is simpler but makes retention and corruption blast radius global. Do not build
either until the trust model decides what a row means.

## D26 — tools for small models need a registry, not a wall

The answer accepts extension actions as brokered tools and immediately names the
real constraint: a low-context model cannot hold every schema. The design that
fits both:

- Always expose two tools: `tools.search` (query, returns compact id,
  description, argument summary) and `tools.call` (id, arguments). The dispatcher
  routes to the extension action and puts `<extension>.<action>` plus its target
  in the permission subject, one audit row per call.
- Optionally inject the top-N matching schemas into the prompt when the model's
  window allows it, so a capable model skips the search round trip.
- The registry is per profile plus activated extensions, keyword/BM25 first.
  embeddings are an optimisation, not a requirement.
- This is also the honest answer for MCP: a dispatcher with `list_changed` is
  cheaper than a hundred tools in every request.

Open: who owns discovery (the daemon), how a tool description is written, and
whether the model or the client chooses the top-N. Settle at P14-5, before the
studio actions exist.

## D3, D8, D18 — deferred on purpose

- **D3.** Keep Svelte. The web UI is built, tested and shipped as one HTML file.
gpui cannot render into the DOM. "embed the desktop UI" means a canvas/input
  bridge or a WASM rewrite, which is the expensive direction. The parity lever
  is the API and the design tokens, not shared view code. Revisit only with a
  measured parity gap.
- **D8.** Compare at P7-1. The `acpilot-lsp::wire` boundary already makes the
  choice cheap to reverse.
- **D18.** Revisit at P14 with the capability set and the budget, per ADR 0003.

## Order of work

1. **D5 ADR** — done, [ADR 0013](adr/0013-authentication-one-daemon-per-person.md).
   The auth schema and the web login can be built. The static token is a
   bootstrap mode, not a second auth story.
2. **D12 ADR** — done, [ADR 0014](adr/0014-language-server-catalog.md). The
   catalog review and ~30-language licence research are the v0.2 task.
3. **D7 ADR** — still open. Lands with P12 packaging and the spawn-path
   change.
4. **D2 presence design** — done,
   [`presence-follow-2026-11-21.md`](presence-follow-2026-11-21.md). The
   same-daemon version can follow ADR 0013. The cross-daemon one waits for D22.
5. **D22 chat design** — after D5 and the trust-model answer. Do not start P13
   before it.
6. **D16 calibration** — when run records next change.
7. **D26 registry** — at P14-5, before studio extension actions.

## Assumptions this invalidates, once accepted

- `README.md` §5.7 and §8: "detect-only / downloads no servers" (D12).
- `README.md` §1 binaries, `site/.../ground-school/installation.md`, ADR 0004
  §5 and ADR 0011: two binaries (D7).
- `site/.../systems/architecture.md` trust table, `site/.../flight-deck/web.md`
  auth, `site/.../safety/secrets.md` "no user database", ADR 0004: static token
  only (D5).
- `site/.../systems/sessions.md` "No collaborative editing" needs a sentence
  distinguishing presence from co-editing (D2) — the non-goal itself stays.
- P13-1/P13-2 and D21/D22: one `chat.sqlite` per profile and a single
  self-hosted server (D21/D22).
- The security checklist's "no auto-download" (D12), which becomes "no silent
  download".

## Still open

- D7: whether `acpilotd` remains a shipped compatibility name, and when the
  subcommand packaging lands.
- D22: server-trusted or end-to-end encrypted, and who is a member? This also
  decides the cross-daemon half of presence.
- D21: one chat database per group per profile, once D22 says what a message
  is.
- D2: whether the first UI shows same-user clients as presence or only in a
  windows menu. The protocol does not decide the presentation.
