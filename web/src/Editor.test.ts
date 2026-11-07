// The editor pane against a mocked daemon: the root listing it asks for on
// mount, a directory it walks into, and a file it opens and draws. What is being
// pinned is the route and the header; the buffer arithmetic has its own test in
// `files.test.ts`.

import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import Editor from './Editor.svelte';
import type { FileContent, FileEntry } from './api';
import { plainFetch, type PlainFetch } from './test-fetch';

const ROOT: FileEntry[] = [
  { path: 'src', is_dir: true, size: 0 },
  { path: 'README.md', is_dir: false, size: 12 },
];

const SRC: FileEntry[] = [{ path: 'src/main.rs', is_dir: false, size: 34 }];

const README: FileContent = { path: 'README.md', text: '# Acpilot\n', etag: 'v1' };

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

beforeEach(() => {
  document.body.innerHTML = '';
  target = document.createElement('div');
  document.body.append(target);
  cleanups = [];
  calls = [];
  vi.stubGlobal(
    'fetch',
    plainFetch(
      (async (input: string, init?: RequestInit) => {
        const route = String(input);
        const headers = (init?.headers ?? {}) as Record<string, string>;
        calls.push({ route, authorization: headers.Authorization ?? headers.authorization });
        if (route.startsWith('/v1/files/list?path=src')) {
          return json(SRC);
        }
        if (route.startsWith('/v1/files/list')) {
          return json(ROOT);
        }
        if (route.startsWith('/v1/files?path=src%2Fmain.rs')) {
          return json(MAIN);
        }
        if (route.startsWith('/v1/files?path=README.md')) {
          return json(README);
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

function render(): void {
  const instance = mount(Editor, { target, props: { token: 'stored' } });
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

  it('reports the git marks it was given without pretending to draw a gutter', async () => {
    render();
    await (await findButton('README.md')).click();
    await find('.cm-editor');

    // README has no marks; main.rs does, so open it the same way the browser does.
    // The browser stays open: its form is where a path is typed.
    const input = (await find('input#file-path')) as HTMLInputElement;
    input.value = 'src/main.rs';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    target
      .querySelector('form.file-open')
      ?.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));

    // The sentence is wrapped across lines in the markup; compare it as one line.
    await vi.waitFor(() =>
      expect(target.textContent?.replace(/\s+/g, ' ')).toContain(
        '2 lines differ from the last commit.',
      ),
    );
  });
});
