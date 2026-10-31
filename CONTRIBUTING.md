# Contributing

Acpilot is a Rust workspace with one daemon and several clients. The rules below
exist because this codebase has four UIs and two transports, and the fastest way
to make it unmaintainable is to let one of them grow its own copy of the logic.

## Before you start

```bash
# Linux and macOS
cargo install --locked cargo-nextest prek   # once; CI installs nextest
prek install                                # writes .git/hooks/pre-commit
cargo xtask check                           # fmt + clippy -D warnings + nextest: what CI runs
curl -s --unix-socket "$XDG_RUNTIME_DIR/acpilot/acpilot.sock" http://localhost/v1/health
```

```powershell
# Windows, PowerShell 5.1 or 7. Same, except the health check: `curl` there is
# an alias for Invoke-WebRequest that rejects -s, so name the binary.
cargo install --locked cargo-nextest prek   # once; CI installs nextest
prek install                                # writes .git/hooks/pre-commit
cargo xtask check                           # fmt + clippy -D warnings + nextest: what CI runs
curl.exe -s localhost:7777/v1/health        # after `cargo run -p acpilotd -- --addr 127.0.0.1:7777`
```

The daemon answers `GET /v1/health` on a unix socket once `cargo run -p acpilotd`
is up — or once any client has started it, since `cargo xtask run tui` spawns
`target/debug/acpilotd` (the sibling of the binary, not a `PATH` entry). The
first launch is `cargo xtask run tui -- .`: the task builds the client *and* the
daemon (a TUI whose sibling is missing only finds out at launch) and passes
anything after `--` to `acpilot`.
The socket is `$XDG_RUNTIME_DIR/acpilot/acpilot.sock` (`$TMPDIR` on macOS), which
is why the health check passes `--unix-socket`; on Windows the daemon listens on
`127.0.0.1:7777` until the named pipe exists, so pass `--addr` there.

`--addr 127.0.0.1:7777` is also what a browser needs, for `/docs` under
`--features docs`. It is an opt-in listener, and every request on it needs
`Authorization: Bearer <token>` with the token the daemon writes to
`$XDG_RUNTIME_DIR/acpilot/token` and logs at startup. The docs page is the one
exception, so that there is somewhere to paste the token: open it, paste, and it
fetches the contract and sends the header on every call.

`acpilot tui` needs no daemon at all: when `GET /v1/health` goes unanswered it runs
the daemon's own core in the same process (P6-11), on a loopback port the operating
system picks, with a token generated for that process — nothing is spawned, nothing is
written, and the sessions end with the window. A daemon that *is* running is always
attached to first, which is how sessions outlive the client. Pass `--no-spawn` when you
are running the daemon yourself, under a debugger or with `RUST_LOG` set: the flag now
means "attach or fail", so your `acpilotd` is the one that serves the window and its
output is wherever you started it.

Every command in both blocks is a program rather than a shell builtin, so
the only difference between them is the HTTP client's name — nothing here
depends on the bash an editor ships.

`cargo xtask check` runs `cargo nextest run --workspace --no-fail-fast`, the
same command as CI. `cargo test --workspace` still works for a single crate or a
`--test` filter, but it is not the gate: nextest isolates tests in separate
processes and skips doctests, and the local check should fail for the same
reasons CI does.

It also builds the desktop shell and runs the web UI's half. The shell first:
`acpilot-gui`'s gpui dependency sits behind a `gui` feature that is off by
default, so neither `cargo clippy --workspace` nor the workspace tests compile it,
and a refactor that breaks the shell would otherwise be found by CI's `gui` job
and nowhere else. It is a build rather than a `check` because linking is where
gpui's missing Linux symlink shows up, and it is the same call `cargo xtask build
gui` makes. The shell's *tests* stay out of the gate: they need `gui-tests` on top
of `gui`, since the headless harness is a second feature and another gpui build —
`cargo xtask gui-check` is that run, and it is the whole of CI's `gui` job: the
lint with both features, the link, and `cargo xtask test gui`. `cargo xtask test
all` is the workspace run plus it. That harness is gpui's own: without it the
About panel and the palette that opens it have no `Context` to live in, so the
tests that draw the window would only be testing a copy of it. `cargo xtask build all` compiles both clients
and the daemon in one go; `cargo xtask run gui` is the shell; `cargo xtask run
daemon` is the backend on its own, for a debugger or a second client. On Linux the
build and those tests need the dev packages gpui links against:

```bash
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev \
  libwayland-dev libfreetype-dev libfontconfig1-dev pkg-config clang
```

and `libc` held at exactly 0.2.189; the crate manifest names both, and `README.md`
§5.4 explains why the pin exists. The `xkbcommon-x11` half of that list is not
avoidable from this workspace — gpui 0.2.2 asks its `xkbcommon` dependency for
`x11` in its own manifest, and Cargo features only ever add (ADR 0009 §5).

The web UI's half is a task of its own, `cargo xtask web-check`: `npm ci` when
`web/node_modules` is missing, `npm run check`, `npm test`, a `cargo build -p
acpilotd` for the live test, `npm run test:live`, then `npm run build` and a
comparison of the result with the committed page. `cargo xtask check` calls it and
CI's `web` job calls it, so that list exists once rather than in a workflow and a
local habit that agree until somebody edits one. A `.svelte` edit that was never
rebuilt, or a type error only `svelte-check` sees, therefore fails locally instead
of on a runner — and when the page drifted, the rebuilt file is already in your
tree to commit. It needs Node 22 and npm; the failure message says so. `cargo
xtask check --no-web` is the Rust-only loop: everything above, without Node.

The web UI is built, not hand-written, and the build output is committed so
`cargo build` never needs Node:

```bash
cd web
npm ci
npm run check   # svelte-check: types and accessibility lint
npm test        # vitest in happy-dom: the token flow, storage and headers
npm run build   # one HTML file, written into crates/acpilot-web/assets/
```

Run `npm run build` whenever you change anything under `web/`, and commit the
regenerated `crates/acpilot-web/assets/index.html` in the same commit. CI's `web`
job rebuilds it — `cargo xtask web-check` is that job — and fails if you forget.

From the repository root, `cargo xtask web-build` is that build plus the two things
around it: it runs `npm ci` when `web/node_modules` is missing, and it ends by
saying whether the committed page changed — the diff CI fails on. `cargo xtask
check` runs the same build as its last web step, but treats a changed page as a
failure, which is the gate's job rather than the build task's. `cargo xtask dev`
is the other half of the loop: the daemon on `127.0.0.1:7777` with the Vite dev
server over it, so a `.svelte` edit is a browser reload rather than a rebuild and a
commit. The page in that mode is the address Vite prints (`http://localhost:5173`),
`/v1` is proxied to the daemon, and the token to paste is the one the daemon logs.
Stop it with Ctrl-C: the task stops both processes when either one stops.

The page's theme repaint is tested against a real daemon as well as a mock:
`npm test` mounts the shell with a fake stream, and `npm run test:live` spawns
`acpilotd`, saves a theme, and asserts the open page repainted. It needs a
daemon built from this checkout — `cargo build -p acpilotd` first, or point
`ACPILOTD_BIN` at one — and is not part of `npm test`, because a unit run must
not need a Rust build.

The interface itself can be looked at without a daemon:

```bash
cargo run -p acpilot-cli -- tui --demo
```

That draws the whole shell — files, editor, agent sidebar, palette, stats bar — on
the seeded world in `acpilot-demo`, with every command that is not built tagged with
the task that would build it. It reads nothing from disk, so it is the same screen on
any machine, and it is what a layout change is reviewed on before its backend
exists. The layout and the open questions are `docs/notes/tui-shell.md`.

The site shows those same screens, and it gets them the same way:

```bash
cargo xtask showcase     # capture the frames, the shell's tables, and the wasm solver
```

`crates/acpilot-showcase` drives the demo exactly as the paragraphs above describe
and paints each state through `ui::draw`, recording which theme token every cell
resolved to; `site/src/components/Showcase.astro` paints those frames and applies a
palette at draw time. The JSON it writes is committed, and a test in that crate
compares it with a fresh capture, so touching a pane, a key or a token without
running the task fails `cargo test` in the same way a stale schema does. The task
also builds `crates/acpilot-palette-wasm` — the product's own palette solver,
compiled for the browser — into `site/public/showcase/`, which is a build output
and stays out of git. It needs the target once:
`rustup target add wasm32-unknown-unknown`.

`cargo xtask check` also runs the design phase's P0-5 probe, which is not a workspace member and
so invisible to `cargo fmt --all` and `cargo clippy --workspace`: one command
covers the repository. `cargo xtask probe` is the same steps without the
workspace half, and it is what CI's `p0-5 probe` job runs. All of it spawns
cargo, rustfmt, rustc and python directly instead of going through a shell,
deliberately: a bash bundled with an editor is not part of the toolchain, Windows
may flag it as a security risk, and several agents may run the same checks.
`probes/wsl-helper/check.ps1`/`check.cmd` are wrappers around that same command
for PowerShell and cmd.exe callers. Use PowerShell, cmd, or the distro's own
shell for the distro half — not `bash.exe` out of an application's install
directory.

`.pre-commit-config.yaml` is the commit-time version of that gate, run by
[prek](https://prek.j178.dev). It runs `cargo fmt`, clippy with `-D warnings`,
the nextest workspace, the `docs`-feature test, and `cargo deny`. The OpenAPI
snapshot and WCAG contrast checks are inside the nextest run, so they are not
duplicated as separate hooks.

`prek run --all-files` runs the same hooks without committing. The hooks are a
convenience, not an authority: CI is what protects `main`, so if a hook ever
disagrees with CI, fix the hook. Do not use `git commit --no-verify` to get past
a failure — the same command will fail in CI, and later.

The toolchain is pinned in `rust-toolchain.toml`; the MSRV floor is 1.95 and is
chosen by our dependencies, not by taste. If an upgrade raises it, that is a
reviewed decision (README §5.9), so say so in the PR.

## Where code goes

| Change | Lives in |
| --- | --- |
| Wire shape, new field, enum variant | `acpilot-proto` (and the OpenAPI snapshot will fail until you update it) |
| Domain rule, error class | `acpilot-core` |
| Agent behaviour, ACP, PTY adapters | `acpilot-agent` |
| Filesystem/process access, SSH, WSL | `acpilot-host` |
| Routes, SSE, persistence | `acpilotd` |
| Anything a user sees | `acpilot-cli`, `acpilot-tui`, `acpilot-gui`, `web/` |

Two things that do not belong anywhere: domain logic in a UI crate, and UI
concerns in the daemon. If the TUI cannot do something, the GUI should not be able
to either.

## Probes

A **probe** is a time-boxed measurement that answers one question a decision
depends on. The question comes first; the code is only the instrument. `P0-4`
asked "does an SSH exec channel preserve ACP framing?" and `probes/ssh` exists to
answer it, not to become the SSH client.

The directory was called `spikes/` until 2026-10-22. "Spike" is Extreme
Programming jargon for the same idea — a spike solution buys knowledge before you
commit — but the word says nothing about measuring, which is what every one of
these does, and it suggests throwaway code, which some of them are not. The rule
below is what the name is meant to carry.

What a probe must be:

- **Standalone.** Its own `[workspace]` and committed lockfile, so nothing in it
  can leak into a product crate, and the versions measured are the versions
  recorded. `probes/fixtures/` holds inputs they share.
- **One question, with numbers.** The result goes in `docs/notes/<topic>.md`:
  what was measured, on what machine and versions, and what the measurement does
  *not* say. A probe that reports "it worked" has not answered anything.
- **Bounded and cited.** The task in `TODO.md` states the question and exit
  criteria, its result line states the answer, and any decision that followed
  cites it (an ADR for the ones that bind us).
- **Deletable.** A probe outlives its question only while something still runs
  it: `probes/wsl-helper` is a CI fixture and a driver we ship against, and the
  the design phase's P0-3 benchmark runs again on every framework upgrade. The rest are deleted once
  the note is written, because a directory of half-used instruments is worse than
  the note alone.

If a probe has to run on another operating system, say so in its note and in CI
rather than claiming the result transfers. The design phase's task P0-5 is the standing example: its
WSL1-on-a-hosted-runner half is the version that would have been published as
WSL2's if the note had not insisted otherwise.

## Platforms

Unix and macOS are built first; the Windows implementation is written last,
because it is the half that cannot be developed or tested on the machines the
project is written on. The rule that keeps that order from rotting is structural:
**a `cfg` naming an operating system may appear only under a crate's
`src/platform/`**, one file per platform, same signature. `acpilot-host` has the
first one; `crates/acpilot-host/tests/platform_boundary.rs` fails the build if a
branch appears anywhere else in any crate, and fails if a platform file goes
missing. Do not add `cfg(windows)` to a shared module — add the platform file.

`cargo xtask check` also type-checks `acpilot-host` for `x86_64-pc-windows-msvc`,
`x86_64-apple-darwin` and `aarch64-apple-darwin` from whatever machine you are on
(`cargo check` needs no linker), skipping targets you have not installed. That
catches a broken Windows module before a CI runner does; CI's matrix still builds
each platform natively, which is the authority.

The cross-check runs `--no-default-features`, because optional transports are not
part of it. `russh` (the `ssh` feature) compiles C for its target, which needs
that target's toolchain, and the SSH module contains no `cfg` — cross-checking it
would teach nothing that CI's native Windows and macOS builds do not. Keep it that
way: platform-specific code goes in `src/platform/`, where this check will see it;
transports stay platform-independent and are checked where they run.

## Tests

Unit tests sit in `#[cfg(test)]` modules next to the code; anything that crosses a
process, a file, or a wire boundary gets an integration suite in
`crates/<crate>/tests/`. `cargo nextest run --workspace` runs both, locally and in
CI; `cargo xtask check` invokes exactly that command.

Two suites are contracts rather than coverage, and should not be weakened:

- `acpilot-proto/tests/openapi_snapshot.rs` — regenerate deliberately and
  review the diff: `INSTA_UPDATE=always cargo test -p acpilot-proto` in bash or
  zsh, `$env:INSTA_UPDATE='always'; cargo test -p acpilot-proto` in PowerShell.
- `acpilot-proto/tests/wire_contract.rs` — every schema, parsed and re-serialised.
  A failure here means the wire moved, not that the test is stale.

## Commits

One idea per commit, imperative subject, scoped by area:

```
proto: add terminal resize event
acpilotd: reject stale ETags on file writes
tui: keep cursor visible on fold
docs: record the GUI framework decision
```

Body explains *why*, not *what* — the diff already says what. Reference the task
ID from `TODO.md` (`P3-5`, `XT-1`) when a commit closes one.

## Pull requests

- `cargo xtask check` green locally; CI runs the same commands on Linux, macOS
  and Windows, so a platform-specific shortcut will be caught.
- One check is required to merge: **`required checks`**, the job at the bottom of
  `.github/workflows/ci.yml`. It needs `check`, `web`, `gui`, `probes`, `deny` and
  `docs`, and passes only when all of them pass, so the ruleset below points at
  one name instead of a list that has to be edited whenever a job is added or
  renamed.

The rule itself is repository settings, not a file. It is a ruleset called
`protect main`, not legacy branch protection: rulesets are what GitHub maintains
now, they cover deletion and force pushes as rules rather than as flags, and the
old `branches/main/protection` endpoint is read-only-compatible rather than the
place to add things. An administrator creates the ruleset once with:

```bash
gh api -X POST "repos/:owner/:repo/rulesets" --input - <<'JSON'
{
  "name": "protect main",
  "target": "branch",
  "enforcement": "active",
  "conditions": {
    "ref_name": { "include": ["refs/heads/main"], "exclude": [] }
  },
  "rules": [
    { "type": "deletion" },
    { "type": "non_fast_forward" },
    {
      "type": "required_status_checks",
      "parameters": {
        "strict_required_status_checks_policy": true,
        "do_not_enforce_on_create": true,
        "required_status_checks": [{ "context": "required checks" }]
      }
    },
    {
      "type": "pull_request",
      "parameters": {
        "required_approving_review_count": 0,
        "dismiss_stale_reviews_on_push": false,
        "require_code_owner_review": false,
        "require_last_push_approval": false,
        "required_review_thread_resolution": false,
        "allowed_merge_methods": ["merge", "squash", "rebase"]
      }
    }
  ]
}
JSON
```

Each rule earns its place:

- `strict_required_status_checks_policy` is the "require branches to be up to date
  before merging" half: a green run from before the base branch moved is not
  evidence about the tree that will merge. `context` is the `required checks` job.
- `pull_request` is what makes `main` take pull requests only, and the count is `0`
  on purpose: the hold-up is the CI run, not a second person on a repository that
  has one maintainer. Leaving the rule out leaves direct pushes to `main` allowed,
  which is the whole thing this exists to stop.
- `deletion` and `non_fast_forward` are branch deletion and force pushes. Without
  them a branch can still have its history rewritten or its ref removed by whoever
  may write to it.
- `bypass_actors` is absent, which means nobody: a rule the people who wrote it can
  skip is a suggestion. That replaces legacy `enforce_admins`, and a ruleset cannot
  quietly leave administrators exempt the way an unchecked box can.
- `do_not_enforce_on_create` keeps branch *creation* out of it, so a new branch
  that happens to be named from the include pattern is still creatable; it says
  nothing about updates to an existing `main`.

Reading it back matters more than writing it — a ruleset that is inactive, or
whose conditions miss `refs/heads/main`, enforces nothing and looks configured:

```bash
gh api repos/:owner/:repo/rulesets --jq '.[] | {name, target, enforcement}'
gh api repos/:owner/:repo/rules/branches/main   # what actually applies
```
- New dependency → a sentence on why the existing ones do not do the job, plus a
  `cargo deny` clean run. The version policy is "latest stable" (README §5.9).
- Wire change → the OpenAPI snapshot diff is part of the review, by design.
- Performance-sensitive change → numbers, not adjectives, against the budgets in
  `TODO.md` §Performance budgets.

## The roadmap

The **ACPilot Roadmap** project (Projects v2) follows the repository in two halves.
The issue half is GitHub's: built-in project workflows set a card to *Todo* when
its issue is added or reopened and *Done* when it closes, with no Action involved.
Keep *Item added*, *Item closed* and *Item reopened* switched on in the project's
Workflows menu — the two *Done* ones are on by default, the others are one toggle.

The pull-request half is `.github/workflows/roadmap.yml`, because no built-in can
say that a draft is *In progress*, a PR ready for review is *In review*, a merged
PR is *Done*, and a PR closed without merging is not finished work. The mapping
and the GraphQL calls live in `.github/scripts/roadmap.mjs`.

That job needs `secrets.PROJECT_TOKEN`, a classic personal access token with the
`project` scope: the workflow's own `GITHUB_TOKEN` cannot see a project owned by a
user account. Until the secret exists the job skips itself and says so in the run
summary, so the PR lane stays inert rather than claiming to work.

Seeding the board, or filling in what an event missed, is the same script:

```bash
GH_TOKEN=$(gh auth token) GITHUB_REPOSITORY=$(gh repo view --json nameWithOwner -q .nameWithOwner) \
  node .github/scripts/roadmap.mjs backfill
node .github/scripts/roadmap.mjs pr 42     # one pull request
node .github/scripts/roadmap.mjs issue 17  # one issue
```

The TODO.md import that created the issues is `.github/scripts/todo_issues.py`;
it is idempotent and re-running it after editing `TODO.md` adds only what is new.

`TODO.md` is a tree, and the board is too: one parent issue per phase and per
cross-cutting section, with the work attached as sub-issues, so a card answers "how
much of this phase is left" instead of "how many issues are open".
`.github/scripts/issue_hierarchy.py` builds and repairs that tree from the labels
(`phase:P2`, `section:testing`) rather than from a hand-kept list, closes a parent
when its last child closes, and is a no-op when there is nothing to do.

## Releases

Merging into `main` publishes a release, and the labels on the pull request decide
the version (`.github/scripts/next_version.py`):

| Labels | Bump | Example |
| --- | --- | --- |
| `breaking`, `major` | major | `0.3.1` → `1.0.0` |
| `feature`, `enhancement` | minor | `0.3.1` → `0.4.0` |
| `bug`, `fix`, `docs`, `chore`, `refactor`, `test`, `ci`, `build`, `dependencies`, `performance`, `security` | patch | `0.3.1` → `0.3.2` |
| `skip-release` | none | — |

The strongest label wins (a PR that is both a fix and a feature is a feature), an
unlabelled PR is a patch, and the first release is `0.1.0` whatever the labels say.
The mapping lives at the top of the script as data, so a new label is a line.

A merge builds `acpilot` and `acpilotd` on Linux x86_64, macOS Apple silicon and
Windows x86_64, publishes a GitHub release with checksums, and rebuilds the docs
site: the download page reads the releases API at build time, so nothing is
committed back and `main` stays a tree only pull requests change. The `gui` feature
ships in the same tarball as the CLI and the daemon, built with `--features
acpilot-gui/gui`; the Linux runner installs the graphics stack it links against,
and the Windows and macOS runners use their own system frameworks.

`cargo xtask dist` is the packaging half of that without a tag: it drives
`cargo-dist` from `dist-workspace.toml` and writes the archives, checksums, the
shell and PowerShell installers, the Homebrew formula and an SBOM per program into
`target/distrib/`. Only the host's target is built unless you pass `--target`, and
the task refuses a `cargo-dist` other than the one the config pins, so a local build
and a released one are the same tool's output. It needs two things on `PATH` —
`cargo-dist` at that pinned version and `cargo-cyclonedx` — and prints the install
command for whichever is missing.

The workflow above and `dist` do not package the same shape yet: the workflow puts
all three programs in one tarball per platform, while `cargo-dist` archives one
package at a time (installed side by side in the same bin directory). Choosing one
is P12-2's, and the first release is the deadline.

## Decisions

Architecture decisions are recorded as ADRs in `docs/adr/`, one file per decision,
numbered, immutable once accepted (supersede rather than edit). The open questions
are listed as `D1`–`D13` in `TODO.md`; an ADR resolves one by number.

If a change is hard to reverse — a process model, a protocol shape, a storage
layout — write the ADR first. Code is cheap to change; a promise to users is not.

## Style

`rustfmt` is authoritative. Import grouping (std, then external, then crate) is a
review convention rather than a `rustfmt.toml` setting, because those options are
nightly-only and this project builds on stable (see the note in `rustfmt.toml`).

Comments explain intent. A comment that restates the next line is noise; a comment
that explains why the obvious approach was wrong is the most valuable thing in the
file.
