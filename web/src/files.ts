// The editor's arithmetic, kept out of the component so a test can ask it
// without a DOM: which grammar a path earns, what a listing looks like, and what
// a refused save should say to the person who made it.

import type { FileContent, FileEntry, LineChange } from './api';

export type EditorLanguage = 'javascript' | 'json' | 'markdown' | 'rust' | 'python' | 'plain';

/** Workspace-relative paths use `/` everywhere, whatever the host is. */
export function normalisePath(path: string): string {
  let next = path.trim().replace(/\\/g, '/');
  while (next.startsWith('./')) {
    next = next.slice(2);
  }
  next = next.replace(/^\/+/, '');
  while (next.endsWith('/') && next.length > 1) {
    next = next.slice(0, -1);
  }
  return next;
}

/** The last segment of a path: what a tab would call the file. */
export function basename(path: string): string {
  const next = normalisePath(path);
  return next.slice(next.lastIndexOf('/') + 1);
}

/**
 * The grammar a path earns, by its extension.
 *
 * A name with no extension (`Makefile`, `LICENSE`) is deliberately plain: a
 * grammar guessed from a filename would be a grammar the file was not written in,
 * and a wrong highlight is worse than none.
 */
export function languageFor(path: string): EditorLanguage {
  const name = basename(path).toLowerCase();
  const dot = name.lastIndexOf('.');
  if (dot <= 0) {
    return 'plain';
  }
  switch (name.slice(dot + 1)) {
    case 'js':
    case 'jsx':
    case 'mjs':
    case 'cjs':
    case 'ts':
    case 'tsx':
    case 'mts':
    case 'cts':
      return 'javascript';
    case 'json':
    case 'jsonc':
      return 'json';
    case 'md':
    case 'markdown':
    case 'mdx':
      return 'markdown';
    case 'rs':
      return 'rust';
    case 'py':
    case 'pyi':
      return 'python';
    default:
      return 'plain';
  }
}

/** One path below another; the root is the empty string, not `/`. */
export function joinPath(dir: string, name: string): string {
  const base = normalisePath(dir);
  return base ? `${base}/${normalisePath(name)}` : normalisePath(name);
}

/** The directory above a path; the root is the empty string. */
export function parentOf(path: string): string {
  const next = normalisePath(path);
  const cut = next.lastIndexOf('/');
  return cut <= 0 ? '' : next.slice(0, cut);
}

/**
 * Directories first, then files, each by path.
 *
 * This is a tree's order rather than the host's: a listing whose rows reordered
 * between machines would make a person re-find the file they were about to click.
 */
export function listingRows(entries: FileEntry[]): FileEntry[] {
  return [...entries].sort((a, b) => {
    if (a.is_dir !== b.is_dir) {
      return a.is_dir ? -1 : 1;
    }
    return a.path.localeCompare(b.path);
  });
}

/** A buffer is dirty when it differs from the version it was read at. */
export function isDirty(file: FileContent | null, draft: string): boolean {
  return file !== null && draft !== file.text;
}

/** One open buffer: the file it was read from, and the text on screen. */
export interface EditorTab {
  /** Workspace-relative path, as the routes spell it. */
  path: string;
  /** The version last read or written; `null` until the first save creates it. */
  file: FileContent | null;
  /** What the editor is drawing. */
  draft: string;
}

/**
 * A tab is dirty when its buffer differs from the file, and a tab with no file is
 * dirty on purpose: it is a path whose first save is the create.
 */
export function tabDirty(tab: EditorTab): boolean {
  return tab.file === null || tab.draft !== tab.file.text;
}

/** An unsaved buffer as the page keeps it: the text, and the version it was typed over. */
export interface StoredDraft {
  text: string;
  /** The `etag` of the text the draft was typed over; `null` when the file did not exist. */
  etag: string | null;
}

/** The tabs a previous visit left open: their paths, which was in front, and drafts. */
export interface StoredTabs {
  paths: string[];
  active: string | null;
  drafts: Record<string, StoredDraft>;
}

/**
 * What the page stores for the next visit.
 *
 * A tab is stored by path. Its buffer travels only while it differs from the file
 * it was read at: a clean tab has nothing to recover, and copying every open file's
 * whole text into the store would duplicate what is already on disk. The etag is
 * kept beside the text so a restored draft is saved against the version it was
 * written over — if the file changed while the page was closed, the save is refused
 * rather than replacing a version the person never saw.
 */
export function tabStorage(tabs: EditorTab[], active: string | null): StoredTabs {
  const paths: string[] = [];
  const drafts: Record<string, StoredDraft> = {};
  for (const tab of tabs) {
    paths.push(tab.path);
    if (!tabDirty(tab)) {
      continue;
    }
    // A tab somebody opened as a new file and never typed in has nothing to recover.
    if (tab.file === null && tab.draft === '') {
      continue;
    }
    drafts[tab.path] = { text: tab.draft, etag: tab.file?.etag ?? null };
  }
  return { paths, active, drafts };
}

/**
 * Parse what the page stored about its tabs.
 *
 * Storage is not a contract: it can hold an older shape, a hand edit, or nothing
 * at all, and none of those is a reason the editor refuses to open. A malformed
 * value is `null`, paths are normalised the way the routes spell them, duplicates
 * collapse, and an active path that is not one of the stored ones falls back to
 * the first tab rather than selecting nothing.
 */
export function storedTabs(raw: string | null): StoredTabs | null {
  if (!raw) {
    return null;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof parsed !== 'object' || parsed === null) {
    return null;
  }
  const record = parsed as { paths?: unknown; active?: unknown; drafts?: unknown };
  if (!Array.isArray(record.paths)) {
    return null;
  }
  const paths: string[] = [];
  for (const value of record.paths) {
    if (typeof value !== 'string') {
      continue;
    }
    const path = normalisePath(value);
    if (path && !paths.includes(path)) {
      paths.push(path);
    }
  }
  if (paths.length === 0) {
    return null;
  }
  // A draft is only useful for a tab that is coming back; one for a path that is not
  // in `paths` has no tab to land in, and is dropped rather than restored alone.
  const drafts: Record<string, StoredDraft> = {};
  if (typeof record.drafts === 'object' && record.drafts !== null) {
    for (const [rawPath, value] of Object.entries(record.drafts)) {
      if (typeof value !== 'object' || value === null) {
        continue;
      }
      const draft = value as { text?: unknown; etag?: unknown };
      const path = normalisePath(rawPath);
      if (!path || !paths.includes(path) || path in drafts || typeof draft.text !== 'string') {
        continue;
      }
      drafts[path] = {
        text: draft.text,
        etag: typeof draft.etag === 'string' ? draft.etag : null,
      };
    }
  }
  const wanted = typeof record.active === 'string' ? normalisePath(record.active) : '';
  return {
    paths,
    active: wanted && paths.includes(wanted) ? wanted : paths[0] ?? null,
    drafts,
  };
}

/** The one character a gutter draws for a change. */
export function markLabel(change: LineChange): string {
  switch (change) {
    case 'added':
      return '+';
    case 'modified':
      return '~';
    case 'deleted':
      return '-';
  }
}

/** What a gutter mark means, in the sentence an element's title carries. */
export function markTitle(change: LineChange): string {
  switch (change) {
    case 'added':
      return 'Added since the last commit';
    case 'modified':
      return 'Changed since the last commit';
    case 'deleted':
      return 'Lines removed above this line since the last commit';
  }
}

/**
 * What a refused save says.
 *
 * A `409` is the one answer the editor must not smooth over: somebody else's text
 * is on disk now, and retrying would write this buffer over it. The message says
 * that in the terms of what the person can do next — reload — rather than
 * repeating the daemon's status code.
 */
export function saveFailure(status: number, message: string): string {
  const said = message.trim();
  if (status === 409) {
    return `The file changed on disk since it was read, so this save was refused. Nothing was overwritten: reload to see that version. (${said})`;
  }
  if (status === 403) {
    return `The daemon refused to write this path: ${said}`;
  }
  if (status === 400) {
    return `That buffer cannot be written: ${said}`;
  }
  return said || `The daemon refused the save (${status}).`;
}
