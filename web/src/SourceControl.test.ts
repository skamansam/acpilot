// The source-control pane against a mocked daemon: the read, the file's section of
// the patch, a generated draft, and the confirmation that stands between the
// composer and the one git write.

import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import SourceControl from './SourceControl.svelte';
import type { SessionSummary } from './api';
import { canCheckout, canDiscard, patchSection, stagedCount } from './git';
import { plainFetch, type PlainFetch } from './test-fetch';

const STATUS = {
  branch: 'main',
  upstream: 'origin/main',
  ahead: 1,
  behind: 0,
  head: 'abc',
  files: [
    {
      path: 'src/main.rs',
      index: 'modified' as const,
      worktree: 'unmodified' as const,
    },
    {
      path: 'new.rs',
      index: 'unmodified' as const,
      worktree: 'untracked' as const,
    },
  ],
};

const PATCH = [
  'diff --git a/src/main.rs b/src/main.rs',
  '--- a/src/main.rs',
  '+++ b/src/main.rs',
  '@@ -1 +1 @@',
  '-old',
  '+new',
  'diff --git a/new.rs b/new.rs',
  '--- /dev/null',
  '+++ b/new.rs',
  '@@ -0,0 +1 @@',
  '+new file',
  '',
].join('\n');

const SESSION: SessionSummary = {
  id: 'session-1',
  workspace_id: 'acpilot',
  agent_id: 'mock',
  title: 'one',
  status: 'idle',
  created_at: '2026-11-07T12:00:00Z',
  updated_at: '2026-11-07T12:00:00Z',
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

let target: HTMLDivElement;
let cleanups: (() => void)[];
let commits: string[];
let generated: string[];
let staged: string[][];
let unstaged: string[][];
let discarded: string[][];
let checkedOut: string[][];

beforeEach(() => {
  document.body.innerHTML = '';
  target = document.createElement('div');
  document.body.append(target);
  cleanups = [];
  commits = [];
  generated = [];
  staged = [];
  unstaged = [];
  discarded = [];
  checkedOut = [];
  vi.stubGlobal(
    'fetch',
    plainFetch(
      (async (input: string, init?: RequestInit) => {
        const route = String(input);
        if (route === '/v1/git/status') {
          return json(STATUS);
        }
        if (route === '/v1/git/diff?scope=all') {
          return json({ patch: PATCH, files: [] });
        }
        if (route === '/v1/git/commit-message' && init?.method === 'POST') {
          const body = JSON.parse(String(init.body ?? '{}')) as { session_id: string };
          generated.push(body.session_id);
          return json({
            message: 'feat: ✨ drafted subject\n\nA body.',
            subject: 'feat: ✨ drafted subject',
            body: 'A body.',
            kind: 'feat',
            scope: null,
            breaking: false,
            emoji: '✨',
            generator: 'session:session-1',
          });
        }
        if (route === '/v1/git/stage' && init?.method === 'POST') {
          const body = JSON.parse(String(init.body ?? '{}')) as { paths: string[] };
          staged.push(body.paths);
          return json(STATUS);
        }
        if (route === '/v1/git/unstage' && init?.method === 'POST') {
          const body = JSON.parse(String(init.body ?? '{}')) as { paths: string[] };
          unstaged.push(body.paths);
          return json(STATUS);
        }
        if (route === '/v1/git/discard' && init?.method === 'POST') {
          const body = JSON.parse(String(init.body ?? '{}')) as { paths: string[] };
          discarded.push(body.paths);
          return json(STATUS);
        }
        if (route === '/v1/git/checkout' && init?.method === 'POST') {
          const body = JSON.parse(String(init.body ?? '{}')) as { paths: string[] };
          checkedOut.push(body.paths);
          return json(STATUS);
        }
        if (route === '/v1/git/commit' && init?.method === 'POST') {
          const body = JSON.parse(String(init.body ?? '{}')) as { message: string };
          commits.push(body.message);
          return json({
            id: '0123456789abcdef',
            parents: [],
            author: 'Test',
            email: 'test@example.com',
            authored_at: '2026-11-07T12:00:00Z',
            subject: 'feat: ✨ drafted subject',
          });
        }
        return new Response('not found', { status: 404 });
      }) as PlainFetch,
    ),
  );
});

afterEach(() => {
  for (const cleanup of cleanups) cleanup();
  vi.unstubAllGlobals();
});

function render(): void {
  const instance = mount(SourceControl, {
    target,
    props: { token: 'stored', sessions: [SESSION], openSession: 'session-1' },
  });
  cleanups.push(() => unmount(instance));
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

async function click(label: string): Promise<void> {
  (await findButton(label)).click();
}

describe('the source-control pane', () => {
  it('reads status and the whole diff when it opens, and draws one file', async () => {
    render();
    await click('Open source control');

    await vi.waitFor(() => expect(target.textContent).toContain('src/main.rs'));
    expect(target.textContent).toContain('main');
    expect(target.textContent).toContain('1 staged');
    expect(target.textContent).toContain('+new');
    expect(target.textContent).not.toContain('+new file');

    (await findButton('M· src/main.rs')).click();
    const section = target.querySelector('.source-diff')?.textContent ?? '';
    expect(section).toContain('+new');
    expect(section).not.toContain('new file');

    (await findButton('·? new.rs')).click();
    await vi.waitFor(() =>
      expect(target.querySelector('.source-diff')?.textContent).toContain('+new file'),
    );
  });

  it('stages and unstages the selected path instead of sending the person to git', async () => {
    render();
    await click('Open source control');
    await vi.waitFor(() => expect(target.textContent).toContain('Compose commit'));

    await click('·? new.rs');
    await click('Stage');
    await vi.waitFor(() => expect(staged).toEqual([['new.rs']]));
    await vi.waitFor(() => expect(target.textContent).toContain('Staged new.rs.'));

    await click('M· src/main.rs');
    await click('Unstage');
    await vi.waitFor(() => expect(unstaged).toEqual([['src/main.rs']]));
    await vi.waitFor(() => expect(target.textContent).toContain('Unstaged src/main.rs.'));
  });

  it('asks before discarding an unstaged path or checking one out from HEAD', async () => {
    render();
    await click('Open source control');
    await vi.waitFor(() => expect(target.textContent).toContain('Compose commit'));

    await click('·? new.rs');
    await click('Discard');
    expect(discarded, 'arming is not writing').toEqual([]);
    expect(target.textContent).toContain('Discard the unstaged changes to new.rs?');
    await click('Discard path');
    await vi.waitFor(() => expect(discarded).toEqual([['new.rs']]));
    await vi.waitFor(() => expect(target.textContent).toContain('Discarded new.rs.'));

    await click('M· src/main.rs');
    await click('Checkout');
    expect(checkedOut, 'arming is not writing').toEqual([]);
    expect(target.textContent).toContain('Check out src/main.rs from HEAD?');
    await click('Checkout path');
    await vi.waitFor(() => expect(checkedOut).toEqual([['src/main.rs']]));
    await vi.waitFor(() =>
      expect(target.textContent).toContain('Checked out src/main.rs from HEAD.'),
    );
  });

  it('a generated message lands in an editable composer and commit asks first', async () => {
    render();
    await click('Open source control');
    await vi.waitFor(() => expect(target.textContent).toContain('Compose commit'));

    await click('Generate message');
    await vi.waitFor(() =>
      expect(target.querySelector<HTMLTextAreaElement>('#commit-message')?.value).toContain(
        'feat: ✨ drafted subject',
      ),
    );
    expect(generated).toEqual(['session-1']);
    const box = target.querySelector<HTMLTextAreaElement>('#commit-message');
    expect(box?.value).toContain('A body.');

    await click('Review commit');
    expect(target.textContent).toContain('Commit 1 staged path with this message?');
    expect(commits, 'reviewing is not writing').toEqual([]);

    await click('Commit staged index');
    await vi.waitFor(() => expect(commits).toEqual(['feat: ✨ drafted subject\n\nA body.']));
    await vi.waitFor(() => expect(target.textContent).toContain('Committed 0123456'));
  });

  it('refuses an empty message before the daemon sees it', async () => {
    render();
    await click('Open source control');
    await vi.waitFor(() => expect(target.textContent).toContain('Compose commit'));
    await click('Compose commit');
    await click('Review commit');
    expect(target.textContent).toContain('a commit needs a message');
    expect(commits).toEqual([]);
  });
});

describe('the patch section', () => {
  it('stops at the next file and ignores a path that is not there', () => {
    const one = patchSection({ patch: PATCH, files: [] }, 'src/main.rs');
    expect(one[0]).toBe('diff --git a/src/main.rs b/src/main.rs');
    expect(one.join('\n')).not.toContain('new.rs');
    expect(patchSection({ patch: PATCH, files: [] }, 'missing.rs')).toEqual([]);
  });

  it('counts only the index, not an untracked working-tree row', () => {
    expect(stagedCount(STATUS.files)).toBe(1);
  });

  it('offers discard for an unstaged path and checkout only with a HEAD version', () => {
    expect(canDiscard(STATUS.files[1]!)).toBe(true);
    expect(canDiscard(STATUS.files[0]!)).toBe(false);
    expect(canCheckout(STATUS.files[0]!, true)).toBe(true);
    expect(canCheckout(STATUS.files[1]!, true)).toBe(false);
    expect(canCheckout(STATUS.files[0]!, false)).toBe(false);
  });
});
