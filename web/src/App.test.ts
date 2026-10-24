// The token flow as the page performs it: what a remembered token does on load,
// what a rejected one does, and what the form does with what you type.
//
// Svelte 5 mounts itself (`mount`) and happy-dom is a DOM, so there is no
// component-testing library here — it would mostly re-export these two things.
// What is being pinned is behaviour a browser would show and a type would not:
// the daemon is asked with the right header, and the token is forgotten the
// moment the daemon says no.

import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from './App.svelte';
import type { Health, SessionSummary } from './api';

const TOKEN_KEY = 'acpilot.token';

const HEALTH: Health = {
  ok: true,
  service: 'acpilotd',
  version: '0.1.0',
  api: 'v1',
  workspace: 'acpilot',
};

const SESSION: SessionSummary = {
  id: 'session-1',
  workspace_id: 'acpilot',
  agent_id: 'mock-agent',
  title: 'a session that exists',
  status: 'idle',
  created_at: '2026-10-24T12:00:00Z',
  updated_at: '2026-10-24T12:05:00Z',
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

let target: HTMLDivElement;
let fetchMock: ReturnType<typeof vi.fn>;
let cleanups: (() => void)[];

/** Mount the page the way `main.ts` does, and remember to take it down again. */
function render(): void {
  const instance = mount(App, { target });
  cleanups.push(() => unmount(instance));
}

function sentTokens(): (string | undefined)[] {
  return fetchMock.mock.calls.map(([, init]) => {
    const headers = (init as RequestInit | undefined)?.headers as Record<string, string> | undefined;
    return headers?.Authorization;
  });
}

async function find<T extends Element>(selector: string): Promise<T> {
  let found: T | null = null;
  await vi.waitFor(() => {
    found = target.querySelector<T>(selector);
    expect(found).not.toBeNull();
  });
  // `waitFor` polls; by here the last poll assigned it.
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

beforeEach(() => {
  // The daemon writes this into the page before it serves it, so the version is
  // known before anyone has a token.
  document.head.innerHTML = '<meta name="acpilot-version" content="0.1.0" />';
  document.body.innerHTML = '';
  target = document.createElement('div');
  document.body.append(target);
  sessionStorage.clear();
  cleanups = [];

  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const route = String(input);
    if (route === '/v1/health') {
      return json(HEALTH);
    }
    if (route === '/v1/sessions') {
      return json([SESSION]);
    }
    return new Response('not found', { status: 404 });
  });
  vi.stubGlobal('fetch', fetchMock);

  // happy-dom implements <dialog>; if a version ever does not, the About effect
  // would throw on mount and every test would fail for a reason that is not the
  // token flow.
  const prototype: HTMLDialogElement | undefined = window.HTMLDialogElement?.prototype;
  if (prototype && typeof prototype.showModal !== 'function') {
    prototype.showModal = function showModal(this: HTMLDialogElement) {
      this.open = true;
    };
    prototype.close = function close(this: HTMLDialogElement) {
      this.open = false;
    };
  }
});

afterEach(() => {
  for (const cleanup of cleanups) {
    cleanup();
  }
  vi.unstubAllGlobals();
  sessionStorage.clear();
});

describe('a remembered token', () => {
  it('connects on load, and every request carries it', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stored');

    render();

    await vi.waitFor(() => expect(target.textContent).toContain('acpilotd'));
    expect(target.textContent).toContain('a session that exists');
    expect(target.textContent).toContain('v1');
    expect(sentTokens()).toEqual(['Bearer stored', 'Bearer stored']);
    expect(target.querySelector('input#token')).toBeNull();
  });

  it('shows the session table with its status and when it moved', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stored');

    render();

    const row = await find<HTMLTableRowElement>('tbody tr');
    expect(row.textContent).toContain('a session that exists');
    expect(row.textContent).toContain('mock-agent');
    expect(row.querySelector('[data-status]')?.getAttribute('data-status')).toBe('idle');
  });
});

describe('no token', () => {
  it('is a form, and no request at all', async () => {
    render();

    await find('input#token');
    expect(target.textContent).toContain('Connect to this daemon');
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('stores what was typed and sends it', async () => {
    render();
    const input = await find<HTMLInputElement>('input#token');

    input.value = 'typed';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    target.querySelector('form')?.dispatchEvent(
      new Event('submit', { bubbles: true, cancelable: true }),
    );

    await vi.waitFor(() => expect(target.textContent).toContain('acpilotd'));
    expect(sessionStorage.getItem(TOKEN_KEY)).toBe('typed');
    expect(sentTokens()).toEqual(['Bearer typed', 'Bearer typed']);
  });

  it('an empty submission connects to nothing', async () => {
    render();
    const input = await find<HTMLInputElement>('input#token');

    input.value = '   ';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    target.querySelector('form')?.dispatchEvent(
      new Event('submit', { bubbles: true, cancelable: true }),
    );

    // Whitespace is not a token; asking the daemon would only teach it that we
    // do not trim.
    await Promise.resolve();
    expect(fetchMock).not.toHaveBeenCalled();
    expect(sessionStorage.getItem(TOKEN_KEY)).toBeNull();
  });
});

describe('a token the daemon rejects', () => {
  it('is forgotten, and the form says why', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stale');
    fetchMock.mockImplementation(async () => new Response('denied', { status: 401 }));

    render();

    await vi.waitFor(() => expect(target.textContent).toMatch(/not accepted/i));
    expect(sessionStorage.getItem(TOKEN_KEY)).toBeNull();
    expect(target.querySelector('input#token')).not.toBeNull();
    expect(target.textContent).not.toContain('acpilotd');
  });

  it('is not carried into the next load', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stale');
    fetchMock.mockImplementation(async () => new Response('denied', { status: 401 }));
    render();
    await vi.waitFor(() => expect(target.textContent).toMatch(/not accepted/i));

    // The page is remounted the way a reload would mount it.
    target.replaceChildren();
    fetchMock.mockClear();
    render();

    await find('input#token');
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

describe('disconnecting', () => {
  it('forgets the token and goes back to the form', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stored');
    render();
    const button = await findButton('Disconnect');

    button.click();

    await vi.waitFor(() => expect(target.querySelector('input#token')).not.toBeNull());
    expect(sessionStorage.getItem(TOKEN_KEY)).toBeNull();
    expect(target.textContent).not.toContain('acpilotd');
  });
});
