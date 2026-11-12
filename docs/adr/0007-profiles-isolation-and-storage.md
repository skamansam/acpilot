# 7. One database per profile, and a small global index

**Status:** accepted, 2026-11-09

**Resolves:** D11 in `TODO.md`

The profiles phase had to choose between one SQLite database with a `profile`
column and one database per profile. The decision also fixes where transcripts
and the small cross-profile index live, because those paths are what make the
choice useful rather than theoretical.

## Decision

### D11 — one database per profile

A profile's conversations, events, grants, hosts, agent pins and terminals live
in `<data>/profiles/<id>/profile.sqlite`. The database is opened by
`acpilot-store`, runs the same migration set as the service phase's schema, and is
never joined with another profile's rows. A profile is an isolation boundary, and
a shared table would make every read responsible for remembering the boundary.

The file was called `acpilot.db` before this decision. Opening a profile adopts
that sibling with a rename (including `-wal` and `-shm`) when `profile.sqlite`
does not exist, so an upgrade keeps yesterday's history instead of presenting an
empty profile. The old name is not written again.

### Transcripts are per profile, under `sessions/`

Each profile keeps its JSONL transcripts under
`<data>/profiles/<id>/sessions/<session>/session.jsonl`. A shared transcript
directory would let two profiles reuse a session id and silently read each
other's conversation, which is the same failure the database split prevents.

### The index is global and disposable

`<data>/index.sqlite` holds two small tables:

- `profiles(id, first_seen)`
- `workspaces(profile, path, first_seen)`

It answers "which profiles exist" and "where has this profile worked" without
opening any profile database. It is not a second conversation store and holds no
transcript, event or credential. Losing it costs a rebuild from profile
directories, not a conversation.

### The daemon opens at most four stores

A daemon serving every profile on the machine opens a store when a request first
needs it. The shared cache keeps the four most recently used open; the fifth
evicts the least recently used, and the next request for that profile opens it
again. Each database keeps its own `PRAGMA user_version`, so migration state is
per profile rather than a daemon-wide flag. A store that fails to open costs that
profile's history and one warning, not the other profiles.

## Consequences

- **Cross-profile queries are explicit.** `acpilot sessions ls --all` must
  iterate profile stores; it cannot become a join. The ticket's budget is 50 ms
  for ten profiles, and the index exists so the iteration does not first have to
  discover directories.
- **Deleting a profile deletes its file, not a filter.** `profile rm` moves the
  profile directory and its state roots to the OS trash; `--purge` removes them
  outright. The index row and its workspaces go with it.
- **A profile cannot borrow another's history by naming it.** The file path is
  derived from the validated profile id; there is no `profile` column a caller
  could set to the wrong value.
- **Backup is per profile.** Copying one profile directory copies its history,
  and a profile with a secret-bearing `agents.toml` does not drag another
  profile's conversations along.

## Alternatives considered

- **One database with a `profile` column.** Rejected: every query becomes an
  isolation check, a missing `WHERE` reads another identity's rows, and deleting a
  profile is a cascade through a file that still contains the rest of them. It is
  also a single write lock for profiles that never interact.
- **One database per profile without the global index.** Rejected for the
  profile list and `--all` query: discovering directories on every request is
  cheap once, not per launch, and the index is where `first_seen` belongs.
- **Keep `acpilot.db` and document the name.** Rejected because the ticket and
  the on-disk layout should agree; the adoption rename is small and keeps the
  upgrade honest.
