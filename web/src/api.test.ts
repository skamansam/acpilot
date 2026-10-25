// Where the token is kept, and how it travels.
//
// These are the two facts the page's security rests on — the token lives in this
// tab's session storage and leaves in an `Authorization` header — and neither is
// visible in a type or a screenshot, which is why they are pinned here rather
// than checked by hand with devtools open.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiError, health, readToken, sessions, writeToken } from './api';

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

const HEALTH = {
  ok: true,
  service: 'acpilotd',
  version: '0.1.0',
  api: 'v1',
  workspace: 'acpilot',
};

const SESSION = {
  id: 'session-1',
  workspace_id: 'acpilot',
  agent_id: 'mock-agent',
  title: 'a session that exists',
  status: 'idle' as const,
  created_at: '2026-10-24T12:00:00Z',
  updated_at: '2026-10-24T12:05:00Z',
};

let fetchMock: ReturnType<typeof vi.fn>;

function authorization(call: number): string | undefined {
  const init = fetchMock.mock.calls[call]?.[1] as RequestInit | undefined;
  return (init?.headers as Record<string, string> | undefined)?.Authorization;
}

function path(call: number): string {
  return String(fetchMock.mock.calls[call]?.[0]);
}

beforeEach(() => {
  sessionStorage.clear();
  fetchMock = vi.fn(async () => json(HEALTH));
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
  sessionStorage.clear();
});

describe('the token in storage', () => {
  it('is kept in this tab and never in a cookie', () => {
    expect(readToken()).toBe('');

    writeToken('s3cret');

    expect(readToken()).toBe('s3cret');
    expect(document.cookie).not.toContain('s3cret');
  });

  it('is removed when the page forgets it', () => {
    writeToken('s3cret');
    writeToken('');

    expect(readToken()).toBe('');
    expect(sessionStorage.getItem('acpilot.token')).toBeNull();
  });

  it('survives a browser that refuses storage', () => {
    // A privacy setting is not a failed connection: the page should still load
    // and ask again, not throw on the way in.
    vi.stubGlobal('sessionStorage', {
      getItem() {
        throw new Error('storage is denied');
      },
      setItem() {
        throw new Error('storage is denied');
      },
      removeItem() {
        throw new Error('storage is denied');
      },
    });

    expect(readToken()).toBe('');
    expect(() => writeToken('s3cret')).not.toThrow();
    expect(() => writeToken('')).not.toThrow();
  });
});

describe('the token on the wire', () => {
  it('is an Authorization header on the health call, never part of the URL', async () => {
    const answer = await health('s3cret');

    expect(answer.service).toBe('acpilotd');
    expect(path(0)).toBe('/v1/health');
    expect(path(0)).not.toContain('s3cret');
    expect(authorization(0)).toBe('Bearer s3cret');
  });

  it('is an Authorization header on the session list too', async () => {
    fetchMock.mockResolvedValueOnce(json([SESSION]));

    const rows = await sessions('s3cret');

    expect(rows.map((row) => row.id)).toEqual(['session-1']);
    expect(path(0)).toBe('/v1/sessions');
    expect(authorization(0)).toBe('Bearer s3cret');
  });

  it('a refusal is an error the page can act on, not a JSON parse failure', async () => {
    // The daemon answers 401 with no useful body; a proxy may answer HTML. The
    // status is the part that must survive.
    fetchMock.mockResolvedValueOnce(new Response('denied', { status: 401 }));

    const cause = await sessions('stale').catch((error: unknown) => error);

    expect(cause).toBeInstanceOf(ApiError);
    expect((cause as ApiError).status).toBe(401);
    expect((cause as ApiError).message).toMatch(/not accepted/i);
  });

  it('an error the daemon explains is shown as it wrote it', async () => {
    fetchMock.mockResolvedValueOnce(
      new Response('  this daemon serves one workspace\n', { status: 409 }),
    );

    const cause = await health('s3cret').catch((error: unknown) => error);

    expect(cause).toBeInstanceOf(ApiError);
    expect((cause as ApiError).status).toBe(409);
    expect((cause as ApiError).message).toBe('this daemon serves one workspace');
  });
});
