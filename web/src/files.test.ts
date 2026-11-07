import { describe, expect, it } from 'vitest';

import type { FileContent, FileEntry } from './api';
import {
  basename,
  isDirty,
  joinPath,
  languageFor,
  listingRows,
  markLabel,
  markTitle,
  normalisePath,
  parentOf,
  saveFailure,
  storedTabs,
  tabDirty,
} from './files';

function entry(path: string, isDir = false): FileEntry {
  return { path, is_dir: isDir, size: 0 };
}

function content(text: string, path = 'src/main.rs'): FileContent {
  return { path, text, etag: 'v1' };
}

describe('languageFor', () => {
  it('names the grammar a known extension earns', () => {
    expect(languageFor('src/app.ts')).toBe('javascript');
    expect(languageFor('src/app.tsx')).toBe('javascript');
    expect(languageFor('package.json')).toBe('json');
    expect(languageFor('README.md')).toBe('markdown');
    expect(languageFor('src/main.rs')).toBe('rust');
    expect(languageFor('tools/run.py')).toBe('python');
  });

  it('draws a name with no extension, or an unknown one, as plain text', () => {
    expect(languageFor('Makefile')).toBe('plain');
    expect(languageFor('LICENSE')).toBe('plain');
    expect(languageFor('.env')).toBe('plain');
    expect(languageFor('notes.txt')).toBe('plain');
  });

  it('does not guess from a directory called something else', () => {
    // The basename is what earns the grammar; `rs` as a parent directory is not
    // an extension, and a file named `README` inside it is still plain.
    expect(languageFor('rs/README')).toBe('plain');
  });
});

describe('paths', () => {
  it('normalises separators and the root spelling the routes use', () => {
    expect(normalisePath('')).toBe('');
    expect(normalisePath('./src/main.rs')).toBe('src/main.rs');
    expect(normalisePath('/src/main.rs')).toBe('src/main.rs');
    expect(normalisePath('src\\main.rs')).toBe('src/main.rs');
    expect(normalisePath('src/')).toBe('src');
    // The root is the empty string everywhere the page spells a path: a listing
    // asks for `''`, not `/`.
    expect(normalisePath('/')).toBe('');
  });

  it('joins below a directory, with the root as the empty string', () => {
    expect(joinPath('', 'src')).toBe('src');
    expect(joinPath('src', 'main.rs')).toBe('src/main.rs');
    expect(joinPath('src/', '/main.rs')).toBe('src/main.rs');
  });

  it('walks up to the root and stops there', () => {
    expect(parentOf('src/main.rs')).toBe('src');
    expect(parentOf('src')).toBe('');
    expect(parentOf('')).toBe('');
  });

  it('names the last segment', () => {
    expect(basename('src/main.rs')).toBe('main.rs');
    expect(basename('main.rs')).toBe('main.rs');
    expect(basename('')).toBe('');
  });
});

describe('listingRows', () => {
  it('puts directories before files, each by path', () => {
    const rows = listingRows([
      entry('src/z.rs'),
      entry('b.txt'),
      entry('src', true),
      entry('a.txt'),
      entry('crates', true),
    ]);
    expect(rows.map((row) => row.path)).toEqual(['crates', 'src', 'a.txt', 'b.txt', 'src/z.rs']);
  });

  it('does not mutate the daemon’s order', () => {
    const rows = [entry('b'), entry('a')];
    listingRows(rows);
    expect(rows.map((row) => row.path)).toEqual(['b', 'a']);
  });
});

describe('isDirty', () => {
  it('is false until something changes, and false again when it changes back', () => {
    expect(isDirty(null, 'anything')).toBe(false);
    expect(isDirty(content('one\ntwo\n'), 'one\ntwo\n')).toBe(false);
    expect(isDirty(content('one\ntwo\n'), 'one\ntwo')).toBe(true);
  });
});

describe('tabDirty', () => {
  it('is dirty when the buffer differs from disk, and when there is no file yet', () => {
    expect(tabDirty({ path: 'new.txt', file: null, draft: '' })).toBe(true);
    expect(tabDirty({ path: 'src/main.rs', file: content('one'), draft: 'one' })).toBe(false);
    expect(tabDirty({ path: 'src/main.rs', file: content('one'), draft: 'two' })).toBe(true);
  });
});

describe('storedTabs', () => {
  it('reads back the paths a visit left, normalised and deduplicated', () => {
    expect(
      storedTabs(
        JSON.stringify({ paths: ['./src/main.rs', 'src/main.rs', ' README.md '], active: 'README.md' }),
      ),
    ).toEqual({ paths: ['src/main.rs', 'README.md'], active: 'README.md' });
  });

  it('falls back to the first tab when the active one is gone or absent', () => {
    expect(storedTabs(JSON.stringify({ paths: ['a.txt'], active: 'gone.txt' }))?.active).toBe('a.txt');
    expect(storedTabs(JSON.stringify({ paths: ['a.txt'] }))?.active).toBe('a.txt');
  });

  it('ignores anything that is not a stored tab list', () => {
    expect(storedTabs(null)).toBeNull();
    expect(storedTabs('not json')).toBeNull();
    expect(storedTabs(JSON.stringify({ paths: 'src' }))).toBeNull();
    // Nothing usable is the same answer as nothing stored: the page opens with
    // no tabs rather than a tab whose path cannot be named.
    expect(storedTabs(JSON.stringify({ paths: [] }))).toBeNull();
    expect(storedTabs(JSON.stringify({ paths: ['  ', 7] }))).toBeNull();
  });
});

describe('gutter labels', () => {
  it('names each change in one character, with a sentence for its title', () => {
    expect(markLabel('added')).toBe('+');
    expect(markLabel('modified')).toBe('~');
    expect(markLabel('deleted')).toBe('-');
    expect(markTitle('added')).toContain('Added');
    expect(markTitle('modified')).toContain('Changed');
    expect(markTitle('deleted')).toContain('removed');
  });
});

describe('saveFailure', () => {
  it('says a conflict was not written over, and what to do instead', () => {
    const message = saveFailure(409, 'the file changed');
    expect(message).toContain('changed on disk');
    expect(message).toContain('Nothing was overwritten');
    expect(message).toContain('reload');
  });

  it('names the refusal a path and a buffer get', () => {
    expect(saveFailure(403, 'outside the workspace')).toContain('refused to write this path');
    expect(saveFailure(400, 'not UTF-8')).toContain('cannot be written');
  });

  it('falls back to the daemon’s sentence, and never to an empty error', () => {
    expect(saveFailure(500, 'the disk is full')).toBe('the disk is full');
    expect(saveFailure(0, '  ')).toContain('refused the save');
  });
});
