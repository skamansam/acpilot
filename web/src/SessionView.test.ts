// The session view, as a person uses it.
//
// The pure rules live in `session.test.ts`; this file mounts the component and
// checks the two things only a page can get wrong: an answer leaves the card the
// moment it is pressed and comes back if the daemon did not take it, and cancel
// says what the daemon actually did.

import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import Session from './Session.svelte';
import type { PermissionRequest, SessionSummary } from './api';

const TOKEN = 'the-daemon-token';

const SESSION: SessionSummary = {
  id: 'session-1',
  workspace_id: 'acpilot',
  agent_id: 'mock-agent',
  title: 'a session that exists',
  status: 'idle',
  created_at: '2026-11-05T10:00:00Z',
  updated_at: '2026-11-05T10:05:00Z',
};

const REQUEST: PermissionRequest = {
  id: 'p1',
  session_id: 'session-1',
  kind: 'file_write',
  title: 'Write src/http.rs',
  detail: '--- a/http.rs\n+++ b/http.rs',
  options: ['allow_once', 'deny'],
  created_at: '2026-11-05T10:00:01Z',
};

function frame(event: string, data: unknown): string {
  return `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
}

const TRANSCRIPT =
  frame('session', {
    seq: 1,
    session_id: 'session-1',
    kind: 'message_chunk',
    at: '2026-11-05T10:00:00Z',
    payload: { role: 'assistant', content: { text: 'hello **there**' } },
  }) +
  frame('session', {
    seq: 2,
    session_id: 'session-1',
    kind: 'tool_call',
    at: '2026-11-05T10:00:01Z',
    payload: {
      tool_call_id: 't1',
      title: 'Read src/lib.rs',
      status: 'completed',
      kind: 'read',
    },
  }) +
  frame('session', {
    seq: 3,
    session_id: 'session-1',
    kind: 'notice',
    at: '2026-11-05T10:00:02Z',
    payload: { level: 'warning', title: 'Careful', message: 'the sandbox is off' },
  });

function sse(body: string): Response {
  return new Response(body, {
    status: 200,
    headers: { 'Content-Type': 'text/event-stream' },
  });
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

let target: HTMLDivElement;
let cleanups: (() => void)[];
let fetchMock: ReturnType<typeof vi.fn>;
let pending: PermissionRequest[];
let answeredByDaemon: boolean;
let failure: string | null;
let resolveBody: unknown;
let resolveToken: string | undefined;

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

function render(): void {
  const instance = mount(Session, {
    target,
    props: { session: SESSION, token: TOKEN },
  });
  cleanups.push(() => unmount(instance));
}

beforeEach(() => {
  document.body.innerHTML = '';
  target = document.createElement('div');
  document.body.append(target);
  cleanups = [];
  pending = [REQUEST];
  answeredByDaemon = false;
  failure = null;
  resolveBody = null;
  resolveToken = undefined;

  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const route = String(input);
    const method = init?.method ?? 'GET';
    if (route === '/v1/sessions/session-1/events?after=0') {
      return sse(TRANSCRIPT);
    }
    if (route === '/v1/events') {
      return sse('');
    }
    if (route === '/v1/permissions?status=pending&session=session-1') {
      return json(answeredByDaemon ? [] : pending);
    }
    if (route === '/v1/permissions/p1/resolve' && method === 'POST') {
      resolveBody = JSON.parse(String(init?.body));
      resolveToken = (init?.headers as Record<string, string> | undefined)?.Authorization;
      if (failure !== null) {
        return new Response(failure, { status: 500 });
      }
      answeredByDaemon = true;
      return json({
        option: (resolveBody as { option: string }).option,
        decided_by: 'user',
        at: '2026-11-05T10:00:02Z',
      });
    }
    if (route === '/v1/sessions/session-1/cancel' && method === 'POST') {
      return json({ turn_id: 't1', cancelled: true });
    }
    return new Response('not found', { status: 404 });
  });
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  for (const cleanup of cleanups) {
    cleanup();
  }
  vi.unstubAllGlobals();
});

describe('the transcript', () => {
  it('draws messages, tool cards and notices from the stream', async () => {
    render();

    await vi.waitFor(() => expect(target.textContent).toContain('hello'));
    expect(target.querySelector('.turn strong')?.textContent).toBe('there');
    expect(target.textContent).toContain('Read src/lib.rs');
    expect(target.querySelector('.tool')?.getAttribute('data-status')).toBe('completed');
    expect(target.textContent).toContain('the sandbox is off');
    expect(target.querySelector('.turn.notice')?.getAttribute('data-level')).toBe('warning');
  });
});

describe('answering a permission', () => {
  it('takes the card away at once and confirms with the daemon', async () => {
    render();
    const allow = await findButton('Allow once');

    allow.click();

    // The optimistic half: gone before the request has answered.
    await vi.waitFor(() => expect(target.querySelector('.permission')).toBeNull());
    await vi.waitFor(() => expect(resolveBody).toEqual({ option: 'allow_once' }));
    await vi.waitFor(() => expect(target.textContent).toContain('Allow once — answered'));
    expect(resolveToken).toBe(`Bearer ${TOKEN}`);
    expect(target.querySelector('.permission')).toBeNull();
    expect(answeredByDaemon).toBe(true);
  });

  it('puts the card back and says why when the daemon refuses the answer', async () => {
    failure = 'the daemon said no';
    render();
    const deny = await findButton('Deny');

    deny.click();

    await vi.waitFor(() => expect(target.textContent).toContain('the daemon said no'));
    // The reconcile half: the pending list still lists it, so it is still a
    // question — a card that stayed gone would be an answer nobody gave.
    await vi.waitFor(() => expect(target.querySelector('.permission')).not.toBeNull());
    expect(target.textContent).toContain('Write src/http.rs');
  });
});

describe('cancelling', () => {
  it('asks the daemon to stop the turn and says what it did', async () => {
    render();
    const cancel = await findButton('Cancel turn');

    cancel.click();

    await vi.waitFor(() => expect(target.textContent).toContain('Asked the agent to stop t1.'));
    const call = fetchMock.mock.calls.find(
      ([input]) => String(input) === '/v1/sessions/session-1/cancel',
    ) as unknown as [string, RequestInit] | undefined;
    expect(call).toBeDefined();
    expect(call?.[1].method ?? 'GET').toBe('POST');
    expect((call?.[1].headers as Record<string, string>).Authorization).toBe(`Bearer ${TOKEN}`);
  });
});
