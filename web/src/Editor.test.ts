// The editor pane against a mocked daemon: the root listing it asks for on
// mount, a directory it walks into, and a file it opens and draws. What is being
// pinned is the route and the header; the buffer arithmetic has its own test in
// `files.test.ts`.

import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import Editor from './Editor.svelte';
import type { FileContent, FileEntry, Preferences } from './api';
import { plainFetch, type PlainFetch } from './test-fetch';

const ROOT: FileEntry[] = [
  { path: 'src', is_dir: true, size: 0 },
  { path: 'README.md', is_dir: false, size: 12 },
];

const SRC: FileEntry[] = [{ path: 'src/main.rs', is_dir: false, size: 34 }];

const README: FileContent = { path: 'README.md', text: '# Acpilot\n', etag: 'v1' };

const PREFS: Preferences = {
  schema_version: 1,
  tab_width: 4,
  insert_spaces: true,
  format_on_save: false,
  trim_trailing_whitespace: false,
  wrap: true,
  rulers: [80],
  theme: 'acpilot.night-flight',
  theme_follow_terminal: false,
  tree_respect_gitignore: true,
  diagnostics_min_severity: 'warning',
};

const MAIN: FileContent = {
  path: 'src/main.rs',
  text: 'fn main() {\n    println!("hello");\n}\n',
  etag: 'v2',
  marks: [
    { line: 1, change: 'added' },
    { line: 2, change: 'deleted' },
  ],
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

let target: HTMLDivElement;
let cleanups: (() => void)[];
let calls: { route: string; authorization: string | undefined }[];
let writes: { path: string; text: string; if_match?: string }[];
let deletes: string[];
let renames: { path: string; to: string }[];

beforeEach(() => {
  document.body.innerHTML = '';
  target = document.createElement('div');
  document.body.append(target);
  cleanups = [];
  calls = [];
  writes = [];
  deletes = [];
  renames = [];
  localStorage.clear();
  vi.stubGlobal('confirm', () => true);
  vi.stubGlobal(
    'fetch',
    plainFetch(
      (async (input: string, init?: RequestInit) => {
        const route = String(input);
        const headers = (init?.headers ?? {}) as Record<string, string>;
        calls.push({ route, authorization: headers.Authorization ?? headers.authorization });
        if (route === '/v1/profiles/demo/preferences') {
          return json(PREFS);
        }
        if (route.startsWith('/v1/files/list?path=src')) {
          return json(SRC);
        }
        if (route.startsWith('/v1/files/list')) {
          return json(ROOT);
        }
        if (route.startsWith('/v1/files?path=src%2Fmain.rs')) {
          return json(MAIN);
        }
        if (route.startsWith('/v1/files?') && init?.method === 'DELETE') {
          const path = new URL(route, 'http://daemon').searchParams.get('path') ?? '';
          deletes.push(path);
          return new Response(null, { status: 204 });
        }
        if (route === '/v1/files/rename' && init?.method === 'POST') {
          const body = JSON.parse(String(init.body ?? '{}')) as { path: string; to: string };
          renames.push(body);
          return json({ path: body.to, text: '', etag: 'v10', marks: [] });
        }
        if (route.startsWith('/v1/files?path=README.md')) {
          return json(README);
        }
        if (route === '/v1/files' && init?.method === 'PUT') {
          const body = JSON.parse(String(init.body ?? '{}')) as {
            path: string;
            text: string;
            if_match?: string;
          };
          writes.push(body);
          return json({ path: body.path, text: body.text, etag: 'v9', marks: [] });
        }
        return new Response('not found', { status: 404 });
      }) as PlainFetch,
    ),
  );
});

afterEach(() => {
  for (const cleanup of cleanups) {
    cleanup();
  }
  vi.unstubAllGlobals();
});

function render(props: { token: string; profile?: string } = { token: 'stored' }): void {
  const instance = mount(Editor, { target, props });
  cleanups.push(() => unmount(instance));
}

async function find(selector: string): Promise<Element> {
  let found: Element | null = null;
  await vi.waitFor(() => {
    found = target.querySelector(selector);
    expect(found).not.toBeNull();
  });
  return found!;
}

async function findButton(label: string): Promise<HTMLButtonElement> {
  let found: HTMLButtonElement | null = null;
  await vi.waitFor(() => {
    found =
      [...target.querySelectorAll('button')].find(
        (candidate) => candidate.textContent?.trim() === label,
      ) ?? null;
    expect(found).not.toBeNull();
  });
  return found!;
}

describe('the file browser', () => {
  it('lists the workspace root on mount, with the token', async () => {
    render();

    await find('ul.file-rows');
    expect(target.textContent).toContain('README.md');
    expect(target.textContent).toContain('src');
    expect(calls[0]?.route).toBe('/v1/files/list?path=');
    expect(calls[0]?.authorization).toBe('Bearer stored');
  });

  it('walks into a directory and back out again', async () => {
    render();

    await findButton('▸ src');
    (await findButton('▸ src')).click();

    await vi.waitFor(() => expect(target.textContent).toContain('main.rs'));
    expect(calls.some((call) => call.route.startsWith('/v1/files/list?path=src'))).toBe(true);

    (await findButton('up')).click();
    await vi.waitFor(() => expect(target.textContent).toContain('README.md'));
  });

  it('says why a directory could not be listed instead of looking empty', async () => {
    vi.stubGlobal(
      'fetch',
      plainFetch((async () => new Response('no workspace here', { status: 409 })) as PlainFetch),
    );
    render();

    await vi.waitFor(() => expect(target.textContent).toContain('no workspace here'));
  });
});

describe('a file', () => {
  it('opens from a listing and draws its text in the editor', async () => {
    render();

    const row = await findButton('README.md');
    row.click();

    await vi.waitFor(() => expect(target.querySelector('.editor-path')?.textContent).toBe('README.md'));
    // The CodeMirror surface is built only once a document exists: a test that
    // never opens a file does not need a DOM the editor can measure.
    await find('.cm-editor');
    expect(target.textContent).toContain('# Acpilot');
  });

  it('applies wrap and rulers from the profile preferences', async () => {
    render({ token: 'stored', profile: 'demo' });
    await (await findButton('README.md')).click();
    await find('.cm-editor');

    await vi.waitFor(() => {
      const host = target.querySelector('.editor-host') as HTMLElement;
      expect(host.style.getPropertyValue('--acpilot-ruler-image')).toContain('80ch');
    });
    expect(target.querySelector('.cm-content')?.classList.contains('cm-lineWrapping')).toBe(true);
    expect(calls.some((call) => call.route === '/v1/profiles/demo/preferences')).toBe(true);
  });

  it('can be opened by typing its path', async () => {
    render();
    await find('ul.file-rows');

    const input = (await find('input#file-path')) as HTMLInputElement;
    input.value = 'src/main.rs';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    target
      .querySelector('form.file-open')
      ?.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));

    await vi.waitFor(() => expect(target.querySelector('.editor-path')?.textContent).toBe('src/main.rs'));
    expect(calls.some((call) => call.route.startsWith('/v1/files?path=src%2Fmain.rs'))).toBe(true);
  });

  it('draws the git marks in a gutter beside the line numbers', async () => {
    render();
    await find('ul.file-rows');

    const input = (await find('input#file-path')) as HTMLInputElement;
    input.value = 'src/main.rs';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    target
      .querySelector('form.file-open')
      ?.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));

    const added = await find('.cm-git-mark[data-change="added"]');
    expect(added.textContent).toBe('+');
    const deleted = await find('.cm-git-mark[data-change="deleted"]');
    expect(deleted.textContent).toBe('-');
    // The sentence under the pane still counts them; the gutter says which lines.
    await vi.waitFor(() =>
      expect(target.textContent?.replace(/\s+/g, ' ')).toContain(
        '2 lines differ from the last commit.',
      ),
    );
  });

  it('keeps a tab per file and switching one brings its buffer back', async () => {
    render();

    await (await findButton('README.md')).click();
    await vi.waitFor(() => expect(target.querySelectorAll('[role="tab"]').length).toBe(1));

    const input = (await find('input#file-path')) as HTMLInputElement;
    input.value = 'src/main.rs';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    target
      .querySelector('form.file-open')
      ?.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));

    await vi.waitFor(() => expect(target.querySelectorAll('[role="tab"]').length).toBe(2));
    const readme = [...target.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find(
      (candidate) => candidate.textContent?.trim() === 'README.md',
    );
    expect(readme, 'README kept its tab').toBeDefined();
    readme!.click();

    await vi.waitFor(() =>
      expect(target.querySelector('.editor-path')?.textContent).toBe('README.md'),
    );
    expect(target.textContent).toContain('# Acpilot');
  });

  it('closes a tab and leaves no file open when it was the last', async () => {
    render();

    await (await findButton('README.md')).click();
    await vi.waitFor(() => expect(target.querySelectorAll('[role="tab"]').length).toBe(1));
    (await find('[aria-label="Close README.md"]') as HTMLButtonElement).click();

    await vi.waitFor(() => expect(target.querySelectorAll('[role="tab"]').length).toBe(0));
    expect(target.querySelector('.editor-path')?.textContent).toBe('no file open');
  });

  it('creates a new file with no if_match on its first save', async () => {
    render();
    await find('ul.file-rows');

    (await findButton('New file')).click();
    const input = (await find('input#new-file-path')) as HTMLInputElement;
    input.value = 'notes.txt';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    target
      .querySelector('form.file-new-form')
      ?.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));

    await vi.waitFor(() =>
      expect(target.querySelector('.editor-path')?.textContent).toBe('notes.txt'),
    );

    (await findButton('Save')).click();
    await vi.waitFor(() => expect(writes.length).toBe(1));
    // The absent token is the create: the daemon refuses a path that exists.
    expect(writes[0]).toEqual({ path: 'notes.txt', text: '' });
  });

  it('reopens the tabs a reload left behind', async () => {
    localStorage.setItem(
      'acpilot.editor.tabs',
      JSON.stringify({ paths: ['README.md'], active: 'README.md' }),
    );
    render();

    await vi.waitFor(() => expect(target.querySelectorAll('[role="tab"]').length).toBe(1));
    expect(target.querySelector('.editor-path')?.textContent).toBe('README.md');
  });

  it('remembers the open tabs as paths for the next visit', async () => {
    render();
    await (await findButton('README.md')).click();

    await vi.waitFor(() => {
      const stored = JSON.parse(localStorage.getItem('acpilot.editor.tabs') ?? '{}');
      expect(stored.paths).toEqual(['README.md']);
      expect(stored.active).toBe('README.md');
    });
  });

  it('restores an unsaved draft and saves it against the version it was typed over', async () => {
    localStorage.setItem(
      'acpilot.editor.tabs',
      JSON.stringify({
        paths: ['README.md'],
        active: 'README.md',
        drafts: { 'README.md': { text: '# mine\n', etag: 'v1' } },
      }),
    );
    render();

    await vi.waitFor(() =>
      expect(target.querySelector('.editor-path')?.textContent).toBe('README.md'),
    );
    expect(target.querySelector('.editor-bar .dirty'), 'the draft came back dirty').not.toBeNull();

    // The effect keeps a still-dirty restored tab for the next visit too, with the
    // token it was typed over rather than the one just read.
    await vi.waitFor(() => {
      const stored = JSON.parse(localStorage.getItem('acpilot.editor.tabs') ?? '{}');
      expect(stored.drafts?.['README.md']).toEqual({ text: '# mine\n', etag: 'v1' });
    });

    (await findButton('Save')).click();
    await vi.waitFor(() => expect(writes.length).toBe(1));
    expect(writes[0]).toEqual({ path: 'README.md', text: '# mine\n', if_match: 'v1' });
  });

  it('renames the open file and follows it to the new path', async () => {
    render();
    await (await findButton('README.md')).click();
    await vi.waitFor(() => expect(target.querySelectorAll('[role="tab"]').length).toBe(1));

    (await findButton('Rename')).click();
    const input = (await find('input#rename-path')) as HTMLInputElement;
    input.value = 'docs/README.md';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    target
      .querySelector('form.file-rename')
      ?.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));

    await vi.waitFor(() =>
      expect(target.querySelector('.editor-path')?.textContent).toBe('docs/README.md'),
    );
    expect(renames).toEqual([{ path: 'README.md', to: 'docs/README.md' }]);
  });

  it('renames a file from its row without opening it', async () => {
    render();
    await find('ul.file-rows');

    (await find('[aria-label="Rename README.md"]') as HTMLButtonElement).click();
    const input = (await find('input#rename-path')) as HTMLInputElement;
    expect(input.value).toBe('README.md');
    input.value = 'docs/README.md';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    target
      .querySelector('form.file-rename')
      ?.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));

    await vi.waitFor(() => expect(renames).toEqual([{ path: 'README.md', to: 'docs/README.md' }]));
    expect(target.querySelectorAll('[role="tab"]').length).toBe(0);
    await vi.waitFor(() =>
      expect(target.textContent).toContain('Renamed README.md to docs/README.md'),
    );
  });

  it('deletes a file from its row without opening it', async () => {
    render();
    await find('ul.file-rows');

    (await find('[aria-label="Delete README.md"]') as HTMLButtonElement).click();

    await vi.waitFor(() => expect(deletes).toEqual(['README.md']));
    expect(target.querySelectorAll('[role="tab"]').length).toBe(0);
    await vi.waitFor(() => expect(target.textContent).toContain('Deleted README.md'));
  });

  it('deletes the open file after asking and drops its tab', async () => {
    render();
    await (await findButton('README.md')).click();
    await vi.waitFor(() => expect(target.querySelectorAll('[role="tab"]').length).toBe(1));

    (await findButton('Delete')).click();

    await vi.waitFor(() => expect(deletes).toEqual(['README.md']));
    await vi.waitFor(() => expect(target.querySelectorAll('[role="tab"]').length).toBe(0));
    expect(target.textContent).toContain('Deleted README.md');
  });
});
