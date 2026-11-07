// The token flow as the page performs it: what a remembered token does on load,
// what a rejected one does, and what the form does with what you type.
//
// Svelte 5 mounts itself (`mount`) and happy-dom is a DOM, so there is no
// component-testing library here — it would mostly re-export these two things.
// What is being pinned is behaviour a browser would show and a type would not:
// the daemon is asked with the right header, and the token is forgotten the
// moment the daemon says no.

import { mount, unmount } from 'svelte';

import { plainFetch, type PlainFetch } from './test-fetch';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from './App.svelte';
import type { Health, SessionSummary, StatRow, TerminalSummary } from './api';

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

// Two panes: one the session opened, and one nobody owns — a shell in a named
// workspace on another host. The second is the row the picker exists for.
const PANE: TerminalSummary = {
  id: 't1',
  session_id: 'session-1',
  size: { cols: 80, rows: 24 },
  alive: true,
  command: '/bin/sh -c make',
};

const ORPHAN: TerminalSummary = {
  id: 't2',
  workspace: 'build',
  host: 'ssh:box',
  size: { cols: 80, rows: 24 },
  alive: true,
  command: 'tail -f',
};

// One row of each shape the bar has to draw: a measured context window, and an
// estimate with no denominator that is also old enough to say so.
const STATS: StatRow[] = [
  {
    id: 'lmstudio',
    kind: 'provider',
    label: 'lmstudio',
    context: { fill: 0.5, text: '16.4k / 32.8k', source: 'server' },
    updated_at: '2026-10-24T12:00:00Z',
    stale: false,
  },
  {
    id: 'claude-code',
    kind: 'agent',
    label: 'Claude Code',
    context: { fill: null, text: '41.0k / ?', source: 'estimated' },
    credits: {
      fill: null,
      text: '4.50 credits left',
      source: 'server',
    },
    updated_at: '2026-10-24T11:40:00Z',
    stale: true,
  },
];

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

let target: HTMLDivElement;
let fetchMock: ReturnType<typeof vi.fn>;
let cleanups: (() => void)[];

/**
 * `EventSource`, as much of it as the page uses.
 *
 * happy-dom has no `EventSource`, and a real one would open a socket in a unit
 * test. This one records the URL, lets a test push a frame, and notices `close` —
 * which is everything the page's theme listener touches.
 */
class FakeEventSource {
  static instances: FakeEventSource[] = [];

  readonly url: string;
  closed = false;
  private listeners = new Map<string, ((event: MessageEvent<string>) => void)[]>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void {
    const forType = this.listeners.get(type) ?? [];
    forType.push(listener);
    this.listeners.set(type, forType);
  }

  close(): void {
    this.closed = true;
  }

  /** Deliver a frame of `type` with `data`, the way a daemon would. */
  emit(type: string, data: string): void {
    for (const listener of this.listeners.get(type) ?? []) {
      listener(new MessageEvent(type, { data }));
    }
  }
}

function lastSource(): FakeEventSource {
  const source = FakeEventSource.instances.at(-1);
  if (!source) {
    throw new Error('the page did not open a palette stream');
  }
  return source;
}

/** Mount the page the way `main.ts` does, and remember to take it down again. */
function render(): void {
  const instance = mount(App, { target });
  cleanups.push(() => unmount(instance));
}

function sentTokens(): (string | undefined)[] {
  return fetchMock.mock.calls.map(([input, init]) => {
    // The typed API calls go out as `Request`s; streams and the off-contract
    // routes still call `fetch(url, init)`.
    if (input instanceof Request) {
      return input.headers.get('Authorization') ?? undefined;
    }
    const headers = (init as RequestInit | undefined)?.headers as Record<string, string> | undefined;
    // Header names survive a `Request` in lower case.
    return headers?.Authorization ?? headers?.authorization;
  });
}

/** The path a mocked call asked for, whichever shape it used. */
function routeOf(input: RequestInfo | URL): string {
  return input instanceof Request ? new URL(input.url).pathname : String(input);
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
  // happy-dom keeps the location between tests; a deep link in one test must not
  // decide the address the next one starts at.
  history.replaceState({}, '', '/');
  cleanups = [];
  // The repaint writes inline style onto the document element; a test that starts
  // with yesterday's colours would pass for the wrong reason.
  document.documentElement.removeAttribute('style');
  FakeEventSource.instances = [];
  vi.stubGlobal('EventSource', FakeEventSource);

  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const route = routeOf(input);
    if (route === '/v1/health') {
      return json(HEALTH);
    }
    if (route === '/v1/sessions') {
      return json([SESSION]);
    }
    if (route === '/app/stats') {
      return json(STATS);
    }
    if (route === '/v1/terminals') {
      return json([PANE, ORPHAN]);
    }
    // The editor lists the workspace root when it mounts. A test that opens a
    // file answers the read route itself; an empty root is enough here.
    if (route === '/v1/files/list') {
      return json([]);
    }
    return new Response('not found', { status: 404 });
  });
  vi.stubGlobal('fetch', plainFetch(fetchMock as unknown as PlainFetch));

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
    // The load's seven calls plus the editor's listing, which is a call the page
    // makes on its own because the pane is part of the page rather than a route.
    expect(sentTokens()).toEqual([
      'Bearer stored',
      'Bearer stored',
      'Bearer stored',
      'Bearer stored',
      'Bearer stored',
      'Bearer stored',
      'Bearer stored',
      'Bearer stored',
    ]);
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

describe('the stats bar', () => {
  it('draws a track and the figures behind it for each integration', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stored');

    render();

    const rows = await vi.waitFor(() => {
      const found = [...target.querySelectorAll('.stats-row')];
      expect(found.length).toBe(2);
      return found;
    });
    const [first] = rows;
    expect(first?.textContent).toContain('lmstudio');
    expect(first?.textContent).toContain('16.4k / 32.8k');
    // Half of the window, drawn as half a track.
    expect(first?.querySelector<HTMLElement>('.track-fill')?.style.width).toBe('50%');
  });

  it('an unreported denominator is an indeterminate track, not an empty one', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stored');

    render();

    const unknown = await vi.waitFor(() => {
      const found = target.querySelector<HTMLElement>(
        '.stats-row[data-stale="true"] .track[data-indeterminate="true"]',
      );
      expect(found).not.toBeNull();
      return found!;
    });
    // No value to announce: a bar that said 0% would be claiming a measurement.
    expect(unknown.getAttribute('aria-valuenow')).toBeNull();
    expect(unknown.getAttribute('aria-valuetext')).toBe('no denominator reported');
    expect(
      target.querySelector('.stats-row[data-stale="true"] .old')?.getAttribute('title'),
    ).toBe('2026-10-24T11:40:00Z');
  });

  it('a daemon that cannot answer for the bar still shows the page', async () => {
    // An older daemon has no `/app/stats`. The sessions are still what a person
    // came for, so the bar reports what it could not read instead of failing the
    // load — and the note says which daemon would answer.
    sessionStorage.setItem(TOKEN_KEY, 'stored');
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const route = String(input);
      if (route === '/v1/health') {
        return json(HEALTH);
      }
      if (route === '/v1/sessions') {
        return json([SESSION]);
      }
      return new Response('not found', { status: 404 });
    });

    render();

    await vi.waitFor(() => expect(target.textContent).toContain('a session that exists'));
    expect(target.textContent).toContain('/app/stats');
  });

  it('the sources are one click away', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stored');
    render();
    const button = await findButton('Show sources');

    button.click();

    await vi.waitFor(() => expect(target.textContent).toContain('context estimated · credits server'));
    expect(target.textContent).not.toContain('16.4k / 32.8k');
    expect(
      target.querySelector('button.link')?.getAttribute('aria-pressed'),
      'the control says what it did',
    ).toBe('true');
  });
});

describe('the terminals', () => {
  it('lists a pane nobody owns and says where it runs', async () => {
    // Filtering the list by the open session would hide this row, and the process
    // behind it would be one nobody could find from the page.
    sessionStorage.setItem(TOKEN_KEY, 'stored');

    render();

    const rows = await vi.waitFor(() => {
      const found = [...target.querySelectorAll('.pane-row')];
      expect(found.length).toBe(2);
      return found;
    });
    expect(rows[0]?.textContent).toContain('/bin/sh -c make');
    expect(rows[0]?.textContent).toContain('session session-1');
    expect(rows[1]?.textContent).toContain('no session');
    expect(rows[1]?.textContent).toContain('build on ssh:box');
    expect(rows[1]?.querySelector('button')?.textContent).toBe('Attach');
  });

  it('a daemon that cannot list terminals still shows the page', async () => {
    // An older daemon has no `/v1/terminals`. The sessions are still what a person
    // came for, so the section says what it could not read instead of failing.
    sessionStorage.setItem(TOKEN_KEY, 'stored');
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const route = String(input);
      if (route === '/v1/health') {
        return json(HEALTH);
      }
      if (route === '/v1/sessions') {
        return json([SESSION]);
      }
      if (route === '/app/stats') {
        return json(STATS);
      }
      return new Response('not found', { status: 404 });
    });

    render();

    await vi.waitFor(() => expect(target.textContent).toContain('a session that exists'));
    expect(target.textContent).toContain('/v1/terminals');
  });
});

describe('the session view', () => {
  it('opens a session from the table and streams its transcript', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stored');
    const stream = (body: string): Response =>
      new Response(body, {
        status: 200,
        headers: { 'Content-Type': 'text/event-stream' },
      });
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const route = String(input);
      if (route === '/v1/health') {
        return json(HEALTH);
      }
      if (route === '/v1/sessions') {
        return json([SESSION]);
      }
      if (route === '/app/stats') {
        return json(STATS);
      }
      if (route === '/v1/terminals') {
        return json([PANE, ORPHAN]);
      }
      if (route === '/v1/sessions/session-1/events?after=0') {
        return stream(
          `event: session\ndata: ${JSON.stringify({
            seq: 1,
            session_id: 'session-1',
            kind: 'message_chunk',
            at: '2026-10-24T12:06:00Z',
            payload: { role: 'assistant', content: { text: 'hello from the agent' } },
          })}\n\n`,
        );
      }
      if (route === '/v1/events') {
        return stream('');
      }
      if (route === '/v1/permissions?status=pending&session=session-1') {
        return json([]);
      }
      return new Response('not found', { status: 404 });
    });

    render();

    const open = await findButton('Open');
    open.click();

    await vi.waitFor(() => expect(target.textContent).toContain('hello from the agent'));
    expect(target.querySelector('.session')).not.toBeNull();
    // Opening a session is a navigation: the address says which one is open, so a
    // refresh and a link copied from the bar both land here.
    expect(location.pathname).toBe('/p/default/s/session-1');
    expect(sentTokens()).toContain('Bearer stored');
  });

  it('starts a session from the agent picker and opens it', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stored');
    let created: unknown = null;
    const stream = (body: string): Response =>
      new Response(body, {
        status: 200,
        headers: { 'Content-Type': 'text/event-stream' },
      });
    fetchMock.mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
      const route = String(input);
      if (route === '/v1/health') {
        return json(HEALTH);
      }
      if (route === '/v1/sessions' && (init?.method ?? 'GET') === 'GET') {
        return json([]);
      }
      if (route === '/v1/sessions' && init?.method === 'POST') {
        created = JSON.parse(String(init.body));
        return json({ ...SESSION, id: 'session-2', title: 'a new session' });
      }
      if (route === '/v1/agents') {
        return json([
          {
            id: 'mock-agent',
            name: 'Mock agent',
            transport: 'acp_stdio',
            status: 'available',
            command: 'mock-agent --stdio',
          },
        ]);
      }
      if (route === '/app/stats') {
        return json(STATS);
      }
      if (route === '/v1/terminals') {
        return json([]);
      }
      if (route === '/v1/sessions/session-2/events?after=0') {
        return stream(
          `event: session\ndata: ${JSON.stringify({
            seq: 1,
            session_id: 'session-2',
            kind: 'message_chunk',
            at: '2026-10-24T12:06:00Z',
            payload: { role: 'assistant', content: { text: 'a fresh turn' } },
          })}\n\n`,
        );
      }
      if (route === '/v1/events' || route === '/v1/permissions?status=pending&session=session-2') {
        return route === '/v1/events' ? stream('') : json([]);
      }
      return new Response('not found', { status: 404 });
    });

    render();

    await find('form.new-session');
    target.querySelector('form.new-session')?.dispatchEvent(
      new Event('submit', { bubbles: true, cancelable: true }),
    );

    // The daemon fills in its own workspace when the field is empty, and the
    // first prompt supplies the title.
    await vi.waitFor(() => expect(created).toEqual({ workspace_id: '', agent_id: 'mock-agent' }));
    await vi.waitFor(() => expect(target.textContent).toContain('a new session'));
    await vi.waitFor(() => expect(target.textContent).toContain('a fresh turn'));
    expect(location.pathname).toBe('/p/default/s/session-2');
  });

  it('opens the session a shared address names', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stored');
    history.replaceState({}, '', '/p/work/s/session-9');
    const stream = (body: string): Response =>
      new Response(body, {
        status: 200,
        headers: { 'Content-Type': 'text/event-stream' },
      });
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const route = String(input);
      if (route === '/v1/health') {
        return json(HEALTH);
      }
      if (route === '/v1/sessions') {
        return json([]);
      }
      if (route === '/v1/sessions/session-9') {
        return json({ ...SESSION, id: 'session-9', title: 'a linked session' });
      }
      if (route === '/app/stats') {
        return json(STATS);
      }
      if (route === '/v1/terminals') {
        return json([]);
      }
      if (route === '/v1/sessions/session-9/events?after=0') {
        return stream(
          `event: session\ndata: ${JSON.stringify({
            seq: 1,
            session_id: 'session-9',
            kind: 'message_chunk',
            at: '2026-10-24T12:06:00Z',
            payload: { role: 'assistant', content: { text: 'a linked turn' } },
          })}\n\n`,
        );
      }
      if (route === '/v1/events' || route === '/v1/permissions?status=pending&session=session-9') {
        return route === '/v1/events' ? stream('') : json([]);
      }
      return new Response('not found', { status: 404 });
    });

    render();

    await vi.waitFor(() => expect(target.textContent).toContain('a linked turn'));
    // The link's profile is the page's profile too: the URL is not rewritten to
    // the daemon's own name just because it has one.
    expect(location.pathname).toBe('/p/work/s/session-9');
    expect(
      fetchMock.mock.calls.some(([input, init]) => {
        // The typed client sends a `Request`; the stream and the off-contract
        // routes send `(url, init)`, which is why the adapter exists. Either way,
        // the profile the address names has to be on the request.
        const headers =
          input instanceof Request
            ? input.headers
            : ((init as RequestInit | undefined)?.headers as
                | Record<string, string>
                | undefined);
        const value =
          headers instanceof Headers
            ? headers.get('X-Acpilot-Profile')
            : (headers?.['X-Acpilot-Profile'] ?? headers?.['x-acpilot-profile']);
        return value === 'work';
      }),
    ).toBe(true);
  });

  it('the workspace picker changes the address and the sessions it shows', async () => {
    sessionStorage.setItem(TOKEN_KEY, 'stored');
    const workspaces = [
      { id: 'local:/acpilot', name: 'acpilot', host_id: 'local', root: '/acpilot', session_count: 1 },
      { id: 'local:/build', name: 'build', host_id: 'local', root: '/build', session_count: 0 },
    ];
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const route = String(input);
      if (route === '/v1/health') {
        return json(HEALTH);
      }
      if (route === '/v1/sessions') {
        return json([SESSION]);
      }
      if (route === '/v1/workspaces') {
        return json(workspaces);
      }
      if (route === '/app/stats') {
        return json([]);
      }
      return new Response('not found', { status: 404 });
    });

    render();

    const select = await find<HTMLSelectElement>('select#workspace');
    await vi.waitFor(() => expect(select.options.length).toBe(2));
    select.value = 'build';
    select.dispatchEvent(new Event('change', { bubbles: true }));

    await vi.waitFor(() => expect(location.pathname).toBe('/p/default/w/build'));
    await vi.waitFor(() => expect(target.textContent).not.toContain('a session that exists'));
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
    expect(sentTokens()).toEqual([
      'Bearer typed',
      'Bearer typed',
      'Bearer typed',
      'Bearer typed',
      'Bearer typed',
      'Bearer typed',
      'Bearer typed',
      'Bearer typed',
    ]);
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

describe('the theme stream', () => {
  it('recolours the page in place instead of waiting for a reload', async () => {
    // A theme saved in the TUI or the desktop app reaches an open tab without the
    // reload the page used to need.
    render();
    await find('input#token');
    const source = lastSource();
    // Same origin as the page, so a build served by the daemon needs no base URL
    // and `npm run dev` proxies it.
    expect(source.url).toBe('/app/palette/events');

    const root = document.documentElement;
    expect(root.style.getPropertyValue('--bg')).toBe('');

    source.emit(
      'palette',
      JSON.stringify({ color_scheme: 'light', vars: { '--bg': '#fbf7ef', '--accent': '#8a4b12' } }),
    );

    expect(root.style.getPropertyValue('--bg')).toBe('#fbf7ef');
    expect(root.style.getPropertyValue('--accent')).toBe('#8a4b12');
    expect(root.style.colorScheme).toBe('light');
  });

  it('a frame it cannot read leaves the colours alone', async () => {
    render();
    await find('input#token');
    const source = lastSource();
    const root = document.documentElement;

    source.emit('palette', '{not json');

    expect(root.style.getPropertyValue('--bg')).toBe('');
  });

  it('closes the stream when the page goes away', async () => {
    render();
    await find('input#token');
    const source = lastSource();

    await vi.waitFor(() => expect(cleanups.length).toBe(1));
    cleanups.shift()!();

    expect(source.closed).toBe(true);
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
