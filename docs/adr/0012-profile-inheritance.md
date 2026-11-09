# 12. Profile inheritance layers settings, not credentials

**Status:** accepted, 2026-11-09

**Resolves:** D13 in `TODO.md`

A profile can name another in the registry with `extends`. The open question was
whether that means "merge every file from the parent beneath the child", which is
the obvious reading and the one the plan recommended for `agents`/`policy`/`lsp`,
or whether it only means "copy the parent when the child is created". The two
answers differ in what a child can do without naming it.

## Decision

### D1 — `extends` is a layer for non-secret settings

The config cascade reads the parent's `config.toml` and `profile.toml` beneath the
child's. A child therefore inherits preferences, including a theme or tab width it
did not repeat, and overrides any key it does name. The chain is walked parent
first, at most four profiles deep, and a cycle is an error naming the chain. A
missing parent is an error, not a fallback: a launch that runs on the child alone
when the parent was meant to be there is the wrong-identity failure this feature
exists to prevent.

Arrays replace, except `disabled` and `deny`, which union. A workspace adding one
disabled agent must not silently re-enable the ones the profile disabled.

### D2 — security-sensitive files are copied, not inherited

`agents.toml`, `policy.toml`, `lsp.toml`, `keymap.toml` and `theme.toml` stay
owned by the readers that parse them. `extends` does not merge them. `profile new`
(P4-9) copies them from the parent when a child is created, and the child is then
free to edit its own copy.

The reason is blast radius. An inherited `agents.toml` can carry a `keyring:`
reference, and an inherited `policy.toml` can carry a rule that allows a path or a
tool. A child that never named either would run with the parent's credential and
the parent's permission — exactly the kind of silent identity mixing profiles are
supposed to make impossible. Preferences are not secrets and a surprise there is
visible on screen, so layering them is worth the convenience.

### D3 — the registry is the one place a parent is named

`[profiles.<id>] extends = "<parent>"` in the global `config.toml` is the
declaration. It is not repeated in `profile.toml`; two spellings of one edge is a
disagreement waiting to happen. A row that is absent from the registry means the
profile has no parent, not that the parent is inferred from a directory name.

## Consequences

- **A preference change in the parent reaches every child on the next read.** That
  is the point, and it is also why the parent is a deliberate choice rather than a
  copy made once.
- **A permission change does not reach children.** A parent that tightens
  `policy.toml` must ask each child to copy it; the audit trail says which profile
  actually changed, which is what an incident review needs.
- **`profile new` is not just `mkdir`.** P4-9 owns the copy, including refusing to
  copy a secret-bearing file into a profile the person did not intend.
- **Domain files do not become cascade layers later without a new decision.** Adding
  them would make the inheritance graph reach a credential, which is what this ADR
  refuses.

## Alternatives considered

- **Inherit everything (the plan's recommendation).** Rejected: it makes a child run
  with credentials and rules it never named. The convenience is real, but the
  failure mode is a work prompt under a personal key, which is the one failure the
  profiles design is built to prevent.
- **Copy everything, including preferences.** Rejected: a theme or tab width copied
  once goes stale, and the person who changed the parent's preference reasonably
  expects the child to follow. Copying is for the files where inheritance is
  dangerous, not for all of them.
- **A per-file `inherit = true` switch.** Rejected for v1: it makes the dangerous
  answer one line away and gives an author a way to opt into exactly the leak this
  decision closes. Revisit only with an audit story for inherited credentials.
