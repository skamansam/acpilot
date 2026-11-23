# 13. One daemon per person, and a JWT for every listener the person is not sitting at

**Status:** accepted, 2026-11-21

**Resolves:** D5 in `TODO.md`

**Amends:** ADR 0004 §3 — the static token on the TCP listener is replaced by a
session credential once an account exists. The socket, the one-daemon-per-user
rule and the discovery layout are unchanged.

## Context

ADR 0004 decided that the filesystem is the daemon's access control: one daemon
per OS user, a `0700` runtime directory, a `0600` socket. The TCP listener was
the exception, and it got the smallest thing that could be true — a random token
in a `0600` file beside the socket, required as `Authorization: Bearer`, read per
request so `acpilot token rotate` needs no restart. The embedded daemon generates
a token in its own process, and a remote driver proves itself with a pairing
token that is stored only as an Argon2id hash. None of those is an identity.

D5 asked for local accounts or OIDC. The answer was both, which is only coherent
if the thing they produce is the same: one session credential with two ways to
obtain it. The question that had to be answered before a schema was written is
whether that credential belongs to a person using their own daemon, or to a user
of a shared team daemon. The two produce different tables, different profile
ownership and different meanings for "session".

## Decision

### 1. The daemon stays one per person

ADR 0004's rule stands: profiles and workspaces live inside one process owned by
one OS user, and a second user gets a second daemon and a second socket. A
"team daemon" is not built in v0.1, and this ADR does not leave a half-built one
behind.

The reason is not that sharing is undesirable. It is that every invariant the
daemon has assumes a single owner: the socket has no login, every profile is
readable by whoever holds the process, the permission broker routes to whichever
client is attached, and the keyring namespace is the OS user's. Multi-tenancy
would turn each of those into an authorization check on a path that currently
does not have one, and a missed check is one person's code in another person's
transcript. Profiles are isolation between *contexts*, not between *people*.

Collaboration between people therefore goes where the trust boundary already is:
through the remote-driver bridge for a specific session, and through the D22
relay for chat and presence. Neither makes one daemon serve two identities.

The users table and per-profile grants still exist, because the schema is the
expensive thing to change later and because one person may legitimately have two
ways to log in. v0.1 enforces one active user; a second is refused until a
separate ADR decides what a shared daemon means.

### 2. Three transports, three answers

| Listener | Who may connect | How |
| --- | --- | --- |
| Unix socket / named pipe | the OS user who owns the runtime directory | filesystem permissions, no credential |
| TCP (`--addr`) | an authenticated account | JWT access token, or the session cookie in a browser |
| Embedded (P6-11) | the process that embedded the daemon | in-memory token, unchanged |
| ACP bridge | a paired remote driver | driver token with scopes, unchanged |

The socket stays credential-free. Adding a login there would be a second lock on
a door the mode bits already shut, and it would break every local client and
script for no security gain. Login exists for the listeners that do not inherit
the filesystem boundary.

Driver tokens remain a different principal kind with a different lifecycle. A
driver is a program answering one session's prompts under scopes a person
granted; a user session is a person. Unifying them would give a driver a user
session by accident, which is the one thing the pairing design exists to prevent.

### 3. Accounts are daemon-level, and the first one is local

`<data>/users.sqlite` holds:

- `users(id, username, display_name, password_hash, created_at, disabled_at)`
- `identities(user_id, issuer, subject, email_at_link, linked_at)` — one row per
  OIDC identity, unique on `(issuer, subject)`
- `profile_grants(user_id, profile, role)` — `owner`, `editor`, `viewer`
- `refresh_tokens(id, user_id, family, hash, issued_at, expires_at, revoked_at)`
- `revoked_jti(jti, expires_at)` — access tokens revoked before they expire
- `auth_events(id, at, user_id, kind, detail)` — login, refresh, link, revoke,
  failure; never a password, token or cookie

A profile's database and files stay where ADR 0007 put them. The grant table is
in the global auth database because it answers "may this subject open this
profile", which is asked before a profile store is opened.

**The first account can only be created from the machine.** On unix that means
the socket; on Windows, whose default endpoint is still loopback HTTP until
P10-5, it means a request carrying the `0600` token file. Either way, possession
of the OS user's local state is the claim, and there is no remote first-user
race to win. `acpilot auth init` is the command; it refuses an endpoint that is
not local.

Recovery follows the same rule. `acpilot auth reset` runs over the local
endpoint and can set a new password for the owner; it does not exist as a remote
route. A lost OIDC provider is recoverable because password login is local.

### 4. Local accounts

A username and an Argon2id password, hashed with the same parameters the driver
ledger uses. Password login is enabled by default for the owner and can be
disabled once OIDC is linked, but not before — a daemon whose only identity
provider is down must still be openable by the person at the keyboard.

Login failures are rate-limited per username and per source address, with a
constant-ish response so an unknown username and a wrong password are not
distinguishable beyond timing noise. The audit row records the username
attempted and the source, not the password.

### 5. OIDC

Authorization code with PKCE (S256) for a browser, device code for a CLI that
cannot open one. Configuration is daemon-level `auth.toml`, not per profile:

```toml
[[oidc]]
id = "work"
issuer = "https://id.example.com"
client_id = "acpilot"
scopes = ["openid", "profile", "email"]
# redirect_uri = "https://acpilot.example.com/v1/auth/oidc/callback"
```

Verification is explicit: the issuer must string-match the configured value,
the audience must contain the client id, `exp`/`nbf` are checked with a small
clock skew, the signing algorithm must be one the provider's JWKS advertises
(never `none`, never an HMAC verified with a public key), and the ID token's
`nonce` must match the one this daemon issued. JWKS is cached with its `kid` and
refetched on an unknown key, not on every request.

An identity maps to a local user by `(issuer, subject)`. Email is stored for
display and is never the key: it is mutable at the provider and reassignable in
ways the daemon cannot see. Linking a second identity to an existing user is an
explicit `acpilot auth link` over the local endpoint, not an automatic match on
email.

### 6. The credential

An access token is a JWT signed Ed25519 with a key generated at first use in
`<data>/auth/signing.key` (`0600`) and selected by `kid`:

```json
{
  "iss": "acpilot",
  "sub": "usr_...",
  "aud": "acpilot",
  "iat": 1763683200,
  "exp": 1763684100,
  "jti": "...",
  "profiles": ["work", "personal"]
}
```

Fifteen minutes is the access lifetime. A refresh token is 32 random bytes,
valid 30 days, stored only as an Argon2id hash, rotated on every use, and grouped
into a family: presenting a refresh token that was already rotated revokes the
whole family and audits the reuse. Logout revokes the family; "log out
everywhere" revokes every family for the user.

Access tokens are checked for signature, `iss`, `aud`, `exp` and `jti` against
`revoked_jti`. That is one indexed read per request, which is the price of being
able to end a session before its fifteen minutes are up; a purely stateless
token would make "revoke" a promise about the future.

The `profiles` claim is the outer bound, not the grant itself: a mutating request
still names its profile in the existing header, the profile must be in the
claim, and the grant must still exist in `profile_grants`. A token cannot widen
its own list, and revoking a grant takes effect at the next refresh even if the
access token is still valid. In v0.1 the owner's claim is every profile, and
`profile new` grants the owner the new one.

Signing keys are rotated with `acpilot auth rotate`: a new key becomes current,
the old one stays published in JWKS for seven days so live access tokens finish
their fifteen minutes, then is removed. Refresh tokens are unaffected because
they are opaque.

### 7. The browser gets a cookie, not a token field

The web shell currently asks for the daemon token in a password field and keeps
it in memory. That was honest for a static token; it is the wrong shape for a
session. The browser now receives:

- `acpilot_session`: `HttpOnly`, `SameSite=Strict`, `Path=/`, `Secure` whenever
  the request arrived over TLS. It carries the refresh token, not the access
  token, so a stolen cookie is revocable and rotating.
- `acpilot_csrf`: readable by the page, compared against `X-CSRF-Token` on every
  non-GET request, and paired with an `Origin` check. `SameSite=Strict` is the
  first defence; this is the second.
- The access token stays in memory in the page and is refreshed on a `401`. It is
  never written to `localStorage`, where any script on the origin can read it.

A cookie also removes the WebSocket workaround: a same-origin `WebSocket` sends
cookies, so the page no longer has to pass a token as a subprotocol. The
subprotocol path stays for bearer-token clients that have no cookie jar, and the
handshake still answers only `acpilot.v1`.

`/`, `/app`, `/docs` and `/app/palette/events` stay open as they are, but the
login page joins them: a browser that has no session can reach the form, and
nothing behind it answers without one. The page still holds no data of its own.

### 8. Migration is a mode, not a flag day

A daemon with no rows in `users` keeps today's behaviour: the TCP listener
accepts the `0600` token file. `GET /v1/health` reports
`"auth": "bootstrap"`, and `acpilot auth init` is the documented next step.

Creating the owner flips the listener to JWT and cookie authentication and
refuses the static token with `401` naming `acpilot auth login`. The token file
is not deleted — `acpilot token rotate` still owns it for pre-bootstrap daemons,
and an operator may need to prove which file was in use — but it no longer opens
the API. That is a deliberate breaking change, taken once, with a message that
names the command rather than a version note nobody reads.

`GET /v1/auth/status` answers whether bootstrap is needed, which providers are
configured, and who the caller is when authenticated. It is open, like health,
and contains no secret.

## Consequences

- **The static token stops being the product's auth story.** Documentation,
  the web shell, the CLI token prompt and the support path all change with it;
  the socket client does not.
- **One user in v0.1 is a restriction, not a schema limit.** The tables and
  grants exist so that a future team daemon is a policy change rather than a
  migration, but `auth user add` refuses until that ADR exists.
- **Profile access is now checked in two places.** The token claim and the grant
  row must agree. A missing check is a cross-profile read, so the route layer's
  profile extraction and the grant lookup are tested together, not separately.
- **Refresh tokens are a new storage of secrets.** They are hashes, the database
  is inside the daemon's data directory, and reuse revokes the family. A stolen
  database still cannot mint a session.
- **OIDC adds a network dependency to login.** Password login remains available
  for the owner precisely so that an outage at the provider is not an outage at
  the daemon.
- **The web page's token field goes away.** A browser session survives a reload
  through the cookie, which is what makes the web client usable rather than a
  demo.
- **The JWT is not a capability.** Scopes and pairing still belong to drivers;
  a user token cannot answer a driver route, and a driver token cannot open a
  user route.
- **Windows bootstrap is the token file, not the socket.** Until P10-5 gives it a
  named pipe, that is the strongest local proof the platform has, and the ADR
  says so rather than pretending the unix path exists there.

## Alternatives considered

- **A shared team daemon with per-user profiles.** Rejected for v0.1: every
  existing invariant assumes one owner, and the changes would touch the socket,
  the broker, the keyring and every profile read at once. A team daemon is a new
  product surface with its own ADR, not a flag on this one.
- **Keep the static token and add OIDC beside it.** Rejected: two credentials
  with different lifetimes and revocation stories means every route has two
  auth paths, and the weaker one is the one scripts keep using.
- **OIDC only.** Rejected: a daemon that cannot be opened when the identity
  provider is unreachable, or on a machine with no account at the provider, is
  worse than one with a local password.
- **Local passwords only.** Rejected by the answer: both were asked for, and
  OIDC is how a team's offboarding actually revokes access.
- **Per-profile users and passwords.** Rejected: identity precedes profile
  selection, and a password per profile multiplies secrets without adding a
  boundary the OS user does not already have.
- **Long-lived JWTs with no refresh token.** Rejected: a stolen token would be
  valid for its whole life with no rotation and no reuse signal.
- **Stateless access tokens with no revocation list.** Rejected: "log out" and
  "revoke this device" would be true only after fifteen minutes, which is not
  what either phrase means to a person.
- **Unify user and driver tokens.** Rejected: a driver is a program acting under
  scopes for one session, and giving it a user session would make the pairing
  code a login.
