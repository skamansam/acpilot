// The editor's arithmetic, kept out of the component so a test can ask it
// without a DOM: which grammar a path earns, what a listing looks like, and what
// a refused save should say to the person who made it.

import type { FileContent, FileEntry } from './api';

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
