// The cockpit end to end: the page the daemon serves, in Chromium, against a real
// daemon, a real profile and the real fixture agent.
//
// Everything the browser talks to is a process this file started: `acpilot serve`
// with a temporary profile and workspace, whose token is read from its own startup
// line. There are no mocked routes and no happy-dom: the point of this suite is
// the parts a unit test cannot see — a keystroke reaching CodeMirror, a save
// carrying the `etag` it read, an agent's request arriving as a card, and the
// click that answers it writing a file.
//
// `cargo xtask web-check` builds `acpilot` and the mock agent, then runs this, so
// CI exercises the same page a release ships. `CONTRIBUTING.md` says to install
// the browser once with `npx playwright install chromium`.

import { execFile, spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { expect, test, type Page } from '@playwright/test';

/** The repository root, from `web/e2e/cockpit.spec.ts`. */
const ROOT = fileURLToPath(new URL('../..', import.meta.url));
const EXE = process.platform === 'win32' ? '.exe' : '';
const SERVE = process.env.ACPILOT_E2E_CLI ?? join(ROOT, 'target', 'debug', `acpilot${EXE}`);
const MOCK_AGENT =
  process.env.ACPILOT_E2E_MOCK_AGENT ??
  join(ROOT, 'target', 'debug', `acpilot-mock-agent${EXE}`);

const NOTES = 'notes for the flight\n';

let home: string;
let profile: string;
let workspace: string;
let runtime: string;
let outside: string;
let origin: string;
let token: string;
let daemon: ChildProcessWithoutNullStreams | undefined;
let log = '';

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, milliseconds);
  });
}

/** Run git in the fixture workspace, rejecting with the command and its stderr. */
function git(...args: string[]): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile('git', args, { cwd: workspace, encoding: 'utf8' }, (error, stdout, stderr) => {
      if (error) {
        reject(new Error(`git ${args.join(' ')} failed: ${stderr || error.message}`));
        return;
      }
      resolve(stdout.trim());
    });
  });
}

/** A port nothing is listening on, so a developer's daemon and this one never meet. */
async function freePort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  const port = typeof address === 'object' && address !== null ? address.port : 0;
  await new Promise<void>((resolve) => {
    server.close(() => resolve());
  });
  return port;
}

/**
 * The token `acpilot serve` printed, once it has.
 *
 * The file is the durable copy, but reading it would mean knowing the platform's
 * runtime directory. The startup panel is the daemon's own answer.
 */
async function waitForToken(child: ChildProcessWithoutNullStreams): Promise<string> {
  const deadline = Date.now() + 30_000;
  for (;;) {
    const match = /Token\s+([0-9a-f]{16,})/.exec(log);
    if (match) {
      return match[1];
    }
    if (child.exitCode !== null) {
      throw new Error(`the daemon exited ${child.exitCode} before printing a token:\n${log}`);
    }
    if (Date.now() > deadline) {
      throw new Error(`the daemon printed no token within 30s:\n${log}`);
    }
    await sleep(50);
  }
}

/** Poll `/app` until it answers: a connection refused a moment earlier is flake. */
async function waitForPage(): Promise<void> {
  const deadline = Date.now() + 30_000;
  for (;;) {
    if (daemon?.exitCode != null) {
      throw new Error(`the daemon exited ${daemon.exitCode} before serving:\n${log}`);
    }
    try {
      const response = await fetch(`${origin}/app`);
      if (response.ok) {
        return;
      }
    } catch {
      // Nothing is listening yet.
    }
    if (Date.now() > deadline) {
      throw new Error(`the daemon did not answer ${origin}/app within 30s:\n${log}`);
    }
    await sleep(100);
  }
}

test.beforeAll(async () => {
  expect(
    existsSync(SERVE),
    `no acpilot binary at ${SERVE}: build it with \`cargo build -p acpilot-cli\`, or set ACPILOT_E2E_CLI`,
  ).toBe(true);
  expect(
    existsSync(MOCK_AGENT),
    `no mock agent at ${MOCK_AGENT}: build it with \`cargo build -p acpilot-agent\`, or set ACPILOT_E2E_MOCK_AGENT`,
  ).toBe(true);

  home = await mkdtemp(join(tmpdir(), 'acpilot-e2e-home-'));
  profile = await mkdtemp(join(tmpdir(), 'acpilot-e2e-profile-'));
  workspace = await mkdtemp(join(tmpdir(), 'acpilot-e2e-workspace-'));
  runtime = await mkdtemp(join(tmpdir(), 'acpilot-e2e-runtime-'));
  outside = await mkdtemp(join(tmpdir(), 'acpilot-e2e-outside-'));

  // A profile the daemon can open: `Profile::open` refuses a directory without a
  // config, which is the point of the refusal — no daemon running on defaults
  // nobody chose.
  await writeFile(join(profile, 'config.toml'), 'schema_version = 1\n');
  await writeFile(
    join(profile, 'agents.toml'),
    `schema_version = 1

[[agents]]
id = "mock"
name = "Mock"
description = "The fixture agent."
transport = "acp-stdio"
command = ${JSON.stringify(MOCK_AGENT)}
`,
  );
  await writeFile(join(workspace, 'notes.txt'), NOTES);

  // The source-control pane needs a repository, and a tracked edit is what it
  // can stage. Committing the baseline first leaves the appended line unstaged.
  await git('init', '-q');
  await git('add', '--', 'notes.txt');
  await git(
    '-c',
    'user.name=Acpilot',
    '-c',
    'user.email=acpilot@example.com',
    'commit',
    '-q',
    '-m',
    'baseline',
  );
  await writeFile(join(workspace, 'notes.txt'), `${NOTES}unstaged change\n`);

  const port = await freePort();
  origin = `http://127.0.0.1:${port}`;
  daemon = spawn(
    SERVE,
    [
      'serve',
      '--bind',
      `127.0.0.1:${port}`,
      '--profile',
      profile,
      '--workspace',
      workspace,
    ],
    {
      env: {
        ...process.env,
        ACPILOT_HOME: home,
        XDG_RUNTIME_DIR: runtime,
        RUST_LOG: process.env.RUST_LOG ?? 'acpilotd=info',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );
  daemon.stdout.setEncoding('utf8');
  daemon.stderr.setEncoding('utf8');
  daemon.stdout.on('data', (chunk: string) => {
    log += chunk;
  });
  daemon.stderr.on('data', (chunk: string) => {
    log += chunk;
  });

  token = await waitForToken(daemon);
  await waitForPage();
});

test.afterAll(async () => {
  if (daemon && daemon.exitCode === null) {
    const exited = new Promise<void>((resolve) => {
      daemon?.once('exit', () => resolve());
    });
    daemon.kill();
    await exited;
  }
  for (const directory of [home, profile, workspace, runtime, outside]) {
    if (directory) {
      await rm(directory, { recursive: true, force: true });
    }
  }
});

/** Open the page and connect with the token the daemon printed. */
async function connect(page: Page): Promise<void> {
  await page.goto(`${origin}/app`);
  await page.locator('input#token').fill(token);
  await page.getByRole('button', { name: 'Connect', exact: true }).click();
  // The form disappearing is the page's own signal that the token worked; the
  // editor's listing arrives after it, and each test waits for what it needs.
  await expect(page.locator('input#token')).toHaveCount(0);
}

test('edits a file, shows the diff, and saves with the version it read', async ({ page }) => {
  await connect(page);

  await page.getByRole('button', { name: 'notes.txt', exact: true }).click();
  await expect(page.locator('.editor-path')).toHaveText('notes.txt');

  // A keystroke, not a `dispatch`: this is the one claim happy-dom cannot check.
  await page.locator('.cm-content').click();
  await page.keyboard.press('Control+End');
  await page.keyboard.type('\nfrom the browser');
  // The tab strip has its own dot; the bar's is the one this pane always had.
  await expect(page.locator('.editor-bar .dirty')).toBeVisible();

  await page.getByRole('button', { name: 'Diff', exact: true }).click();
  await expect(page.locator('.diff-host .cm-mergeView')).toBeVisible();
  await expect(page.locator('.diff-host')).toContainText('from the browser');
  await page.getByRole('button', { name: 'Edit', exact: true }).click();

  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Saved notes.txt' })).toBeVisible();
  await expect(page.locator('.editor-bar .dirty')).toHaveCount(0);

  // The daemon wrote the workspace's file, not a browser-side copy.
  await expect
    .poll(async () => readFile(join(workspace, 'notes.txt'), 'utf8'))
    .toContain('from the browser');
});

test('stages and unstages a path through the source-control panel', async ({ page }) => {
  await connect(page);
  const source = page.locator('section.source');
  await source.getByRole('button', { name: 'Open source control', exact: true }).click();

  const files = source.locator('.source-files');
  await expect(files).toContainText('notes.txt');
  await files.getByRole('button').filter({ hasText: 'notes.txt' }).click();
  await expect(source.locator('.source-branch')).toContainText('1 changed');
  await expect(source.locator('.source-branch')).toContainText('0 staged');
  await expect.poll(() => git('diff', '--cached', '--name-only')).toBe('');

  await source.getByRole('button', { name: 'Stage path', exact: true }).click();
  await expect(source.getByRole('status')).toContainText('Staged notes.txt.');
  await expect(source.locator('.source-branch')).toContainText('1 staged');
  await expect.poll(() => git('diff', '--cached', '--name-only')).toBe('notes.txt');
  await expect.poll(() => git('show', ':notes.txt')).toContain('unstaged change');

  await source.getByRole('button', { name: 'Unstage', exact: true }).click();
  await expect(source.getByRole('status')).toContainText('Unstaged notes.txt.');
  await expect(source.locator('.source-branch')).toContainText('0 staged');
  await expect.poll(() => git('diff', '--cached', '--name-only')).toBe('');
  await expect.poll(() => git('diff', '--name-only')).toBe('notes.txt');
});

test('prompts the mock agent, approves its write outside the workspace, and sees it land', async ({
  page,
}) => {
  await connect(page);

  await expect(page.locator('select#agent')).toBeVisible();
  await page.getByRole('button', { name: 'Start session', exact: true }).click();
  await page.waitForURL(/\/s\/[^/]+$/);

  const target = join(outside, 'agent.txt');
  const prompt = page.locator('textarea[id^="prompt-"]');
  await prompt.fill(`write ${target}`);
  await page.getByRole('button', { name: 'Send', exact: true }).click();

  // The agent's `fs/write_text_file` is outside the workspace, so the daemon holds
  // it: the card is the ask, and the path is what the person is deciding about.
  const card = page.locator('article.permission');
  await expect(card).toBeVisible();
  await expect(card).toContainText('agent.txt');
  await expect(card.getByRole('button', { name: 'Allow once', exact: true })).toBeVisible();

  await card.getByRole('button', { name: 'Allow once', exact: true }).click();
  await expect(card).toHaveCount(0);

  // The agent reports what the client answered, and the file is the evidence the
  // answer reached the write rather than only the transcript.
  await expect(page.locator('.transcript')).toContainText('fs/write_text_file accepted');
  await expect.poll(async () => readFile(target, 'utf8')).toBe('hello\n');
});
