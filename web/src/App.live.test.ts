/// <reference types="node" />

// The shell against a real daemon, in a headless DOM.
//
// `App.test.ts` answers "does the page apply a palette frame", with the daemon
// replaced by a `fetch` mock and an `EventSource` that can be told what to say.
// That is the right test for the page's own logic and the wrong one for the
// claim the page makes: that saving a theme on this machine repaints a tab that
// is already open. Three things have to be real for that claim to mean
// anything — the daemon, its settings watcher, and the HTTP stream between them
// — so here all three are: `acpilotd` is spawned as a process, its palette
// stream is read over a socket, and the only fake left is the browser, which is
// happy-dom.
//
// The daemon is built by the test's caller, not by the test: spawning `cargo` from
// a test makes a test into a build, and the two want different failure messages. A
// missing binary fails with the command that produces it.
//
// `npm run test:live` (see `vitest.live.config.ts`). Not part of `npm test`:
// a unit run must not need a compiled daemon.

import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { request } from 'node:http';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';

import { mount, unmount } from 'svelte';
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest';

import App from './App.svelte';

/**
 * The repository root: the nearest directory above the working directory that
 * holds the daemon's crate.
 *
 * Walked rather than derived from `import.meta.url`, which is not a `file:` URL
 * once Vite has served the module, and from the working directory alone, which
 * is `web/` under `npm run test:live` and the repository root under a bare
 * `vitest --config web/vitest.live.config.ts`.
 */
function repositoryRoot(): string {
  let directory = process.cwd();
  for (;;) {
    if (existsSync(join(directory, 'crates', 'acpilotd'))) {
      return directory;
    }
    const parent = dirname(directory);
    if (parent === directory) {
      throw new Error(`no repository root above ${process.cwd()}`);
    }
    directory = parent;
  }
}

const ROOT = repositoryRoot();

/** The daemon this test drives: the one `cargo build -p acpilotd` makes. */
const DAEMON = process.env.ACPILOTD_BIN
  ? process.env.ACPILOTD_BIN
  : join(ROOT, 'target', 'debug', process.platform === 'win32' ? 'acpilotd.exe' : 'acpilotd');

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, milliseconds);
  });
}

/** A port nothing is listening on, so two runs of this file cannot meet. */
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
 * `GET url`, with `node:http` rather than `fetch`.
 *
 * The shell's own requests go through the page's `fetch`, and this module is
 * checking what that page was served rather than taking part in it. `node:http`
 * also keeps the stream below honest: the response body is read as bytes
 * arriving, which is what an `EventSource` is.
 */
function get(url: string): Promise<{ status: number; body: string }> {
  return new Promise((resolve, reject) => {
    const parsed = new URL(url);
    const call = request(
      {
        hostname: parsed.hostname,
        port: parsed.port,
        path: `${parsed.pathname}${parsed.search}`,
        method: 'GET',
      },
      (response) => {
        response.setEncoding('utf8');
        let body = '';
        response.on('data', (chunk: string) => {
          body += chunk;
        });
        response.on('end', () => resolve({ status: response.statusCode ?? 0, body }));
      },
    );
    call.on('error', reject);
    call.end();
  });
}

/**
 * Read an SSE stream, frame by frame, over a real socket.
 *
 * Enough of the format for this daemon's frames: `event:` and `data:` lines, a
 * blank line between frames, and `:` keep-alives ignored. Carriage returns are
 * dropped rather than parsed — a frame's data is JSON, which cannot carry one
 * unescaped — so a chunk boundary can never split a line ending the parser
 * cares about.
 */
function listen(url: string, onFrame: (event: string, data: string) => void): () => void {
  const parsed = new URL(url);
  const call = request({
    hostname: parsed.hostname,
    port: parsed.port,
    path: `${parsed.pathname}${parsed.search}`,
    method: 'GET',
    headers: { Accept: 'text/event-stream' },
  });
  call.on('response', (response) => {
    response.setEncoding('utf8');
    let buffer = '';
    response.on('data', (chunk: string) => {
      buffer = (buffer + chunk).replaceAll('\r', '');
      let boundary = buffer.indexOf('\n\n');
      while (boundary !== -1) {
        const block = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        let event = 'message';
        const data: string[] = [];
        for (const line of block.split('\n')) {
          if (line.startsWith('event:')) {
            event = line.slice('event:'.length).trim();
          } else if (line.startsWith('data:')) {
            data.push(line.slice('data:'.length).replace(/^ /, ''));
          }
        }
        if (data.length > 0) {
          onFrame(event, data.join('\n'));
        }
        boundary = buffer.indexOf('\n\n');
      }
    });
  });
  // A closed page ends the request, and that is not a failure to report.
  call.on('error', () => {});
  call.end();
  return () => {
    call.destroy();
  };
}

/** Where the page is being served from, once the daemon is up. */
let origin = '';

/**
 * `EventSource`, over this module's stream reader.
 *
 * happy-dom has none, and the page is written against the real interface, so
 * this has to be one: the URL comes from the page (relative, because a page
 * served by the daemon is same-origin with it) and listeners are named events.
 * `new EventSource` in the page is then a request to the daemon.
 */
class SocketEventSource {
  static opened: SocketEventSource[] = [];

  readonly url: string;
  closed = false;
  private listeners = new Map<string, ((event: MessageEvent<string>) => void)[]>();
  private stop: () => void;

  constructor(url: string) {
    this.url = new URL(url, `${origin}/`).toString();
    SocketEventSource.opened.push(this);
    this.stop = listen(this.url, (type, data) => {
      for (const listener of this.listeners.get(type) ?? []) {
        listener(new MessageEvent<string>(type, { data }));
      }
    });
  }

  addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void {
    const forType = this.listeners.get(type) ?? [];
    forType.push(listener);
    this.listeners.set(type, forType);
  }

  close(): void {
    this.closed = true;
    this.stop();
  }
}

/** Poll `condition` until it holds, or fail with what was being waited for. */
async function waitFor(condition: () => boolean, wanted: string, timeout = 20_000): Promise<void> {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (condition()) {
      return;
    }
    await sleep(25);
  }
  throw new Error(`waited ${timeout}ms for ${wanted}`);
}

/**
 * The CSS variables in a page's `:root` block.
 *
 * The daemon inlines the palette there when it serves `/app`; this is the page a
 * browser would have painted before the stream said anything, and the stream's
 * first frame has to agree with it rather than be a second opinion.
 */
function paletteOf(html: string): Map<string, string> {
  const block = /:root\{([^}]*)\}/.exec(html)?.[1];
  if (!block) {
    throw new Error('the served page has no `:root` block');
  }
  const variables = new Map<string, string>();
  for (const declaration of block.split(';')) {
    const [name, value] = declaration.split(':');
    if (name?.startsWith('--') && value) {
      variables.set(name.trim(), value.trim());
    }
  }
  return variables;
}

function painted(name: string): string {
  return document.documentElement.style.getPropertyValue(name);
}

let home: string;
let workspace: string;
let runtime: string;
let daemon: ChildProcess | undefined;
let log = '';
let target: HTMLDivElement;
let instance: Record<string, unknown> | undefined;

beforeAll(async () => {
  expect(
    existsSync(DAEMON),
    `no daemon at ${DAEMON}: build it with \`cargo build -p acpilotd\`, or set ACPILOTD_BIN`,
  ).toBe(true);

  // One directory each for the three things the daemon would otherwise take from
  // the machine running the test: its settings, the workspace it serves, and the
  // token in its runtime directory.
  home = await mkdtemp(join(tmpdir(), 'acpilot-live-home-'));
  workspace = await mkdtemp(join(tmpdir(), 'acpilot-live-workspace-'));
  runtime = await mkdtemp(join(tmpdir(), 'acpilot-live-runtime-'));

  const port = await freePort();
  origin = `http://127.0.0.1:${port}`;
  daemon = spawn(DAEMON, ['--addr', `127.0.0.1:${port}`, '--workspace', workspace], {
    env: {
      ...process.env,
      ACPILOT_HOME: home,
      XDG_RUNTIME_DIR: runtime,
      // The log is only read when something has gone wrong, and then it is the
      // only thing that can say what.
      RUST_LOG: process.env.RUST_LOG ?? 'acpilotd=info',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  daemon.stdout?.on('data', (chunk: Buffer) => {
    log += chunk.toString();
  });
  daemon.stderr?.on('data', (chunk: Buffer) => {
    log += chunk.toString();
  });

  // Ready when the shell answers: the palette stream is opened after it, and a
  // connection refused a moment earlier would be a flaky test rather than a
  // broken one.
  const deadline = Date.now() + 30_000;
  for (;;) {
    if (daemon.exitCode !== null) {
      throw new Error(`the daemon exited ${daemon.exitCode} before answering:\n${log}`);
    }
    try {
      const page = await get(`${origin}/app`);
      if (page.status === 200) {
        break;
      }
    } catch {
      // Nothing is listening yet.
    }
    if (Date.now() > deadline) {
      throw new Error(`the daemon did not answer ${origin}/app within 30s:\n${log}`);
    }
    await sleep(100);
  }

  // `App.svelte` reads the build version from the page, and `main.ts` is what puts
  // the app in a `#app` element; both are the served page's job, so the test does
  // them the same way rather than mounting into a document the daemon never made.
  document.head.innerHTML = '<meta name="acpilot-version" content="live" />';
  document.body.innerHTML = '';
  target = document.createElement('div');
  document.body.append(target);
  sessionStorage.clear();
  // A repaint writes inline style onto the document element; a test that started
  // with yesterday's colours would pass for the wrong reason.
  document.documentElement.removeAttribute('style');
  SocketEventSource.opened = [];
  vi.stubGlobal('EventSource', SocketEventSource);
});

afterAll(async () => {
  if (instance) {
    await unmount(instance);
    instance = undefined;
  }
  vi.unstubAllGlobals();
  if (daemon && daemon.exitCode === null) {
    const exited = new Promise<void>((resolve) => {
      daemon?.once('exit', () => resolve());
    });
    daemon.kill();
    await exited;
  }
  for (const directory of [home, workspace, runtime]) {
    if (directory) {
      await rm(directory, { recursive: true, force: true });
    }
  }
});

describe('the shell against a real daemon', () => {
  it('repaints when a theme is saved, without a reload', async () => {
    // Night flight is the default, so a daemon with no settings file serves it:
    // the palette on the page came from the cascade, not from a stylesheet.
    const served = paletteOf((await get(`${origin}/app`)).body);
    expect(served.get('--bg'), 'the served page is night flight').toBe('#070d18');

    instance = mount(App, { target }) as Record<string, unknown>;

    // The first frame is the palette in force, so a page that connected after a
    // save lands on the current theme rather than the one its HTML was built with.
    await waitFor(() => painted('--bg') === '#070d18', 'the first palette frame');
    expect(document.documentElement.style.colorScheme).toBe('dark');
    expect(SocketEventSource.opened.map((source) => source.url)).toEqual([
      `${origin}/app/palette/events`,
    ]);

    // Every variable, not just the background: a repaint that carried half a
    // palette would leave the rest of the page at the previous theme's colours.
    for (const [name, value] of served) {
      expect(painted(name), `the frame and the served page disagree about ${name}`).toBe(value);
    }

    // The same save the TUI's theme picker writes, from a program that is not the
    // daemon: the watcher is the only thing between this file and the open tab.
    await writeFile(join(home, 'config.toml'), '[preferences]\ntheme_web = "acpilot.paper"\n');

    await waitFor(() => painted('--bg') === '#fbf7ef', 'the page to repaint into paper');
    expect(painted('--accent'), "paper's accent, not night flight's").toBe('#8a5a10');
    expect(document.documentElement.style.colorScheme).toBe('light');

    // And a reload would show the same thing: the repaint is the theme, not a
    // frame the served page would contradict.
    expect(paletteOf((await get(`${origin}/app`)).body).get('--bg')).toBe('#fbf7ef');
  });
});
