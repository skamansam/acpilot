# Stale branch audit: what `main` holds

`main` holds every byte from the refs below, with one exception that this note
names. The commits check in `CONTRIBUTING.md` still reports work for most of
them, because a squash merge writes the same patch under a new hash. This note
records the content comparison, and the outcome at the end.

All counts use `origin/main` at `8d963e0` and the tips as of 2026-11-23.

## Why the commit count misleads

`CONTRIBUTING.md` says to drop a branch when `git rev-list --count main..<branch>`
prints zero. A squash merge keeps the branch's commits outside `main`, so the
count stays high even when every byte landed. `git cherry` has the same weakness,
because it compares patch ids.

The reliable test compares trees. Two tree ids match only when the trees match:

```bash
git rev-parse <branch-tip>^{tree}
git rev-parse <landing-commit>^{tree}
```

The landing commit is in `main`. Equal ids therefore prove that the branch holds
nothing `main` lacks.

## Result

| Ref | Ahead | Behind | Landed as | Evidence | Verdict at audit |
| --- | --- | --- | --- | --- | --- |
| `roadmap-tooling` | 2 | 138 | `40cec08`, PR #267 | tree `633d91c4` on both | yes |
| `p2-5-terminals` | 1 | 137 | `782b973`, PR #294 | tree `76f34c1b` on both | yes |
| `web-ui-and-desktop-release` | 11 | 140 | `5ec20ee`, PR #2 | tree `612a95f4` on both | yes |
| `tui-shell` | 0 | 128 | ancestor, PR #302 | `git merge-base --is-ancestor` | yes |
| `ci-required-name-test` | 0 | 126 | ancestor of `main` | no commit above `main` | yes |
| `docs/readme-template` | 0 | 75 | ancestor of `main` | no commit above `main` | yes |
| `p4.5-1-provider-registration` | 0 | 126 | ancestor of `main` | no commit above `main` | yes |
| `p4.5-2-probe-memory` | 0 | 122 | ancestor of `main` | no commit above `main` | yes |
| `p4.5-2-provider-probe` | 0 | 124 | ancestor of `main` | no commit above `main` | yes |
| `phase/p4.5-models` | 0 | 1 | ancestor, PR #338 | no commit above `main` | keep the phase ref |
| `dependabot/.../tokio-1.54.1` | 1 | 139 | `5609fe7`, PR #3 | lock in `main` has 1.54.1 | yes |
| `ci/ste-prose-gate` | 2 | 0 | ancestor of `phase/p4.6-mod-shop` | merge commit `2da9377` | after the phase merge |
| `docs/branch-deletion-rule` | 1 | 0 | ancestor of `phase/p4.6-mod-shop` | merge commit `5e1a929` | after the phase merge |
| `docs/retire-tui-shell-menus-themes` | 1 | 0 | ancestor of `phase/p4.6-mod-shop` | merge commit `8d963e0` | after the phase merge |
| `tui-shell-menus-themes` | 156 | 134 | not merged | 261 of 263 paths match `tui-shell` | no, see #344 |
| `backup/main-9eae926` | 1 | 135 | draft of #296 | see below | delete the local ref |

## The two deletions

### `roadmap-tooling`

Two commits sit above `main`: `50cc7bc` adds the roadmap scripts, and `d34e22a`
adds the issue hierarchy tool. PR #267 squashed both into `40cec08`. Both trees
carry the id `633d91c4`:

```bash
git rev-parse d34e22a^{tree}    # 633d91c43b7907823e0df1926bf3a5b978b1c138
git rev-parse 40cec08^{tree}    # 633d91c43b7907823e0df1926bf3a5b978b1c138
```

`git cherry main roadmap-tooling` marks both commits with `+`. The two-to-one
squash changed each patch id, so the mark reports identity and not content.

### `p2-5-terminals`

One commit, `b535606`, holds the bounded terminal ring. PR #294 landed it as
`782b973`, and both trees carry the id `76f34c1b`. The trees show no difference:

```bash
git diff --stat b535606 782b973    # prints nothing
```

`git cherry` marks this commit with `-`, because the squash kept the same patch.

### `web-ui-and-desktop-release`

Eleven commits sit above `main`, and PR #2 squashed the tip into `5ec20ee`. The
tip `3580861` and `5ec20ee` share the tree id `612a95f4`, so the whole branch
landed.

## The ref that needed a decision

`tui-shell-menus-themes` has 156 commits above `main`, and PR #301 closed without
a merge. #344 records the file-by-file comparison. The branch changes 263 paths
after `0c57cd6`, and 261 are identical to `tui-shell`, which `main` contains. Two
differ, and `main` holds the newer text in both:

- `.github/workflows/ci.yml` — `tui-shell` limits `on: push` to `main`.
- `CONTRIBUTING.md` — `tui-shell` adds the sentence that names the published check.

`main` lacks one path from the branch's change set:
`crates/acpilot-extensions/src/contrast.rs`. The landing commit `0c57cd6` deleted
it, and the branch drops it too, so nothing unique sits there.

A person made that call later. The Outcome section records it.

## `backup/main-9eae926`

This ref exists on this clone only. Its commit `9eae926` is the first checkpoint
of `tui-shell-menus-themes`, 156 commits before that tip. The commit holds the
same work that PR #296 landed as `0c57cd6` on 2026-10-31.

The trees differ, so the draft is not the landed version. Every path it touched
exists in `main`. Twelve of its thirteen added files are there in a later form,
and the thirteenth matches byte for byte. For example, `main` adds 329 lines to
`docs/notes/tui-shell.md` and drops 102 lines from the draft. The files it
changed follow the same pattern.

Nothing waits to be ported. Each file has the landed version, and the later 156
commits carry the refined work, which matches `tui-shell`, a branch `main`
contains. The local ref is gone.

## What this change does

- Deletes `roadmap-tooling` and `p2-5-terminals`, locally and on `origin`.
- Deletes the local `backup/main-9eae926` ref.
- Leaves the other refs in place. Their rows here are the evidence for a later
  cleanup.

## Outcome

On 2026-11-23 the cleanup finished, after `phase/p4.6-mod-shop` merged into
`main` as `7003a47`:

- Deleted `roadmap-tooling`, `p2-5-terminals`, and the local `backup/main-9eae926` ref.
- Deleted `web-ui-and-desktop-release`, `tui-shell`, and the five other zero-ahead refs.
- Deleted `phase/p4.5-models` and `phase/p4.6-mod-shop`, plus the three refs that
  merged through the phase.
- Deleted `tui-shell-menus-themes` once the evidence showed no unique content.

`main` and the dependabot ref remain.

## Commands

```bash
# compare a squashed branch with the commit that landed it
git rev-parse <branch-tip>^{tree} <landing-commit>^{tree}
git merge-base --is-ancestor <landing-commit> main

# confirm the landed draft stays in main
git merge-base --is-ancestor 0c57cd6 main

# confirm the deletions
git ls-remote --heads origin
```
