import { describe, expect, it } from 'vitest';

import type { SessionSummary, WorkspaceSummary } from './api';
import { DEFAULT_PROFILE, parseRoute, routePath, scopeSessions } from './routes';

const SESSION: SessionSummary = {
  id: 'session-1',
  workspace_id: 'acpilot',
  agent_id: 'mock-agent',
  title: 'a session',
  status: 'idle',
  created_at: '2026-10-24T12:00:00Z',
  updated_at: '2026-10-24T12:05:00Z',
};

const WORKSPACES: WorkspaceSummary[] = [
  { id: 'local:/acpilot', name: 'acpilot', host_id: 'local', root: '/acpilot', session_count: 1 },
  { id: 'local:/build', name: 'build', host_id: 'local', root: '/build', session_count: 0 },
];

describe('a page address', () => {
  it('names a workspace, a session, or only a profile', () => {
    expect(parseRoute('/p/work/w/acpilot')).toEqual({ profile: 'work', workspace: 'acpilot' });
    expect(parseRoute('/p/work/s/session-1')).toEqual({ profile: 'work', session: 'session-1' });
    expect(parseRoute('/p/work')).toBeNull();
    expect(parseRoute('/')).toBeNull();
    expect(parseRoute('/v1/sessions')).toBeNull();
  });

  it('is written and read back unchanged, escapes included', () => {
    for (const path of ['/p/work/w/acpilot', '/p/work/s/session-1', '/p/a%2Fb/w/c%20d']) {
      const route = parseRoute(path);
      expect(route).not.toBeNull();
      expect(routePath(route!)).toBe(path);
    }
  });

  it('accepts the /app spelling the page was published under', () => {
    expect(parseRoute('/app/p/work/w/acpilot')).toEqual({
      profile: 'work',
      workspace: 'acpilot',
    });
    // The canonical address is still `/p/...`, so a link copied from `/app`
    // keeps working when that alias goes away.
    expect(routePath(parseRoute('/app/p/work/s/session-1')!)).toBe('/p/work/s/session-1');
  });

  it('leaves a malformed escape alone instead of refusing the address', () => {
    expect(parseRoute('/p/100%/w/acpilot')).toEqual({ profile: '100%', workspace: 'acpilot' });
  });
});

describe('a workspace address', () => {
  it('shows the sessions started in that workspace and no others', () => {
    expect(scopeSessions([SESSION], 'build', WORKSPACES)).toEqual([]);
    expect(scopeSessions([SESSION], 'acpilot', WORKSPACES)).toEqual([SESSION]);
  });

  it('matches a workspace by id as well as by label', () => {
    // A link may have been built from the id, which is the durable half of the row.
    expect(scopeSessions([SESSION], 'local:/acpilot', WORKSPACES)).toEqual([SESSION]);
  });

  it('does not filter at all when the address names no workspace', () => {
    expect(scopeSessions([SESSION], undefined, WORKSPACES)).toEqual([SESSION]);
  });

  it('has a name for a daemon that serves no profile', () => {
    expect(DEFAULT_PROFILE).toBe('default');
  });
});
