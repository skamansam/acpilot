// The source-control pane's arithmetic, away from the component.
//
// The patch is the daemon's text. This module only chooses one file's section out
// of it, with the same header-to-next-header rule the TUI and the desktop use, so
// the browser cannot draw a diff nobody else would.

import type { GitChangeKind, GitDiff, GitStatusEntry } from './api';

/** Whether the index holds a change for this kind. */
export function staged(kind: GitChangeKind): boolean {
  return kind !== 'unmodified' && kind !== 'untracked';
}

/** How many paths the next commit would contain. */
export function stagedCount(files: GitStatusEntry[]): number {
  return files.filter((entry) => staged(entry.index)).length;
}

/** The two marks a changed row draws: the index's and the working tree's. */
export function marks(entry: GitStatusEntry): string {
  return `${letter(entry.index)}${letter(entry.worktree)}`;
}

/** One letter for a side of a change. */
export function letter(kind: GitChangeKind): string {
  switch (kind) {
    case 'modified':
      return 'M';
    case 'added':
      return 'A';
    case 'deleted':
      return 'D';
    case 'renamed':
      return 'R';
    case 'copied':
      return 'C';
    case 'type_changed':
      return 'T';
    case 'unmerged':
      return 'U';
    case 'untracked':
      return '?';
    default:
      return '·';
  }
}

/**
 * One file's section of a unified patch, its `diff --git` header included.
 *
 * Matching the header matters: two files can contain the same path at the start of
 * a `---` line, and only the `diff --git` line says where a section begins.
 */
export function patchSection(diff: GitDiff, path: string): string[] {
  const suffix = ` b/${path}`;
  const lines: string[] = [];
  let inside = false;
  for (const line of diff.patch.split('\n')) {
    if (line.startsWith('diff --git ')) {
      inside = line.endsWith(suffix);
      if (inside) {
        lines.push(line);
      }
    } else if (inside) {
      lines.push(line);
    }
  }
  // `split` gives a trailing empty line for the usual final newline; it is not a
  // line of the diff and should not draw as one blank row.
  if (lines.at(-1) === '') {
    lines.pop();
  }
  return lines;
}

/**
 * Whether the selected path has a working-tree change to add.
 *
 * An untracked file is stageable: `git add` is how it enters the index at all.
 * A path whose only change is already in the index has nothing left to add.
 */
export function canStage(entry: GitStatusEntry | null): boolean {
  return entry !== null && entry.worktree !== 'unmodified' && entry.worktree !== 'ignored';
}

/** Whether the selected path is in the next commit and can be removed. */
export function canUnstage(entry: GitStatusEntry | null): boolean {
  return entry !== null && staged(entry.index);
}

/**
 * Whether the selected path has an unstaged change, or is untracked.
 *
 * Discard restores from the index; an untracked file has no index version, so the
 * daemon removes it instead.
 */
export function canDiscard(entry: GitStatusEntry | null): boolean {
  return entry !== null && entry.worktree !== 'unmodified' && entry.worktree !== 'ignored';
}

/**
 * Whether the selected path has a version in HEAD to check out.
 *
 * An untracked path has no HEAD version; discard is the action that can remove it.
 * A staged path can be checked out even when the working tree matches the index.
 */
export function canCheckout(entry: GitStatusEntry | null, hasHead: boolean): boolean {
  if (entry === null || !hasHead) {
    return false;
  }
  return (
    staged(entry.index) ||
    (entry.worktree !== 'unmodified' &&
      entry.worktree !== 'ignored' &&
      entry.worktree !== 'untracked')
  );
}

/** The first seven characters of a commit id, or the whole id when shorter. */
export function shortId(id: string): string {
  return [...id].slice(0, 7).join('');
}
