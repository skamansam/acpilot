// The terminal contract with the daemon, without a browser or a real socket.
//
// The rules pinned here are the ones that cost a person a process when they are
// wrong: which panes the picker lists (never dropping one nobody owns), what a
// row says about where it runs, and what a dropped socket does — attach again and
// replay the screen, or leave the program alone when the pane was closed.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { TerminalSummary } from './api';
import { attachPane, attachPath, paneWhere, panesFor } from './terminals';

const TOKEN = 'the-daemon-token';

function pane(overrides: Partial<TerminalSummary>): TerminalSummary {
  return {
    id: 't1',
    session_id: 's1',
    size: { cols: 80, rows: 24 },
    alive: true,
    command: '/bin/sh',
    ...overrides,
  };
}

/** A WebSocket as much as `attachPane` uses it. */
class FakeSocket {
  readyState = 0;
  binaryType = '';
  sent: string[] = [];
  closed = false;
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;

  constructor(
    readonly url: string,
    readonly protocols: string[],
  ) {}

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    this.closed = true;
  }

  open(): void {
    this.readyState = 1;
    this.onopen?.(new Event('open'));
  }

  say(data: string): void {
    this.onmessage?.({ data } as MessageEvent);
  }

  bytes(data: number[]): void {
    this.onmessage?.({ data: new Uint8Array(data).buffer } as MessageEvent);
  }

  drop(): void {
    this.readyState = 3;
    this.onclose?.({} as CloseEvent);
  }
}

let sockets: FakeSocket[];
let written: number[][];
let attached: TerminalSummary[];
let exits: (number | undefined)[];
let statuses: string[];

function factory(url: string, protocols: string[]): WebSocket {
  const socket = new FakeSocket(url, protocols);
  sockets.push(socket);
  return socket as unknown as WebSocket;
}

function connect(): {
  resize: (cols: number, rows: number) => void;
  close: () => void;
} {
  return attachPane({
    id: 't1',
    token: TOKEN,
    write: (bytes) => written.push([...bytes]),
    attached: (terminal) => attached.push(terminal),
    exit: (code) => exits.push(code),
    status: (status) => statuses.push(status),
    socketFactory: factory,
  });
}

beforeEach(() => {
  sockets = [];
  written = [];
  attached = [];
  exits = [];
  statuses = [];
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('the pane list', () => {
  it('keeps the open session and every pane nobody owns', () => {
    const ours = pane({ id: 't1', session_id: 's1' });
    const stranger = pane({ id: 't2', session_id: 's9' });
    const orphan = pane({ id: 't3', session_id: undefined, workspace: 'build' });

    expect(panesFor([ours, stranger, orphan], 's1').map((row) => row.id)).toEqual(['t1', 't3']);
  });

  it('lists every pane when no session is open', () => {
    // The page has no session view yet, and filtering by a session nobody chose
    // would hide every pane on the daemon.
    const rows = [pane({ id: 't1' }), pane({ id: 't2', session_id: 's9' })];
    expect(panesFor(rows, null).map((row) => row.id)).toEqual(['t1', 't2']);
  });

  it('says where a sessionless pane runs', () => {
    expect(
      paneWhere(pane({ session_id: undefined, workspace: 'build', host: 'ssh:box' })),
    ).toBe('no session · build on ssh:box');
  });

  it('a sessionless pane with no origin says so without inventing one', () => {
    expect(paneWhere(pane({ session_id: undefined }))).toBe(
      'no session · this workspace on this machine',
    );
  });

  it('a session-owned pane names its session unless the daemon said where', () => {
    expect(paneWhere(pane({ session_id: 's1' }))).toBe('session s1');
    expect(paneWhere(pane({ session_id: 's1', host: 'ssh:box' }))).toBe(
      'this workspace on ssh:box',
    );
  });
});

describe('the attach socket', () => {
  it('offers the token as a protocol, never in the URL', () => {
    connect();

    expect(sockets).toHaveLength(1);
    expect(sockets[0]?.url).toBe(attachPath('t1'));
    expect(sockets[0]?.url).not.toContain(TOKEN);
    expect(sockets[0]?.protocols).toEqual(['acpilot.v1', TOKEN]);
    expect(sockets[0]?.binaryType).toBe('arraybuffer');
  });

  it('encodes a pane id rather than pasting it into a path', () => {
    expect(attachPath('t1/../x y')).toBe('/v1/terminals/t1%2F..%2Fx%20y/attach');
  });

  it('writes bytes, announces the screen replay, and reports the exit', () => {
    connect();
    sockets[0]?.open();

    sockets[0]?.say(
      JSON.stringify({
        type: 'attached',
        terminal: pane({ id: 't1' }),
        replay_bytes: 2,
      }),
    );
    sockets[0]?.bytes([104, 105]);
    sockets[0]?.say(JSON.stringify({ type: 'exit', code: 3 }));

    expect(attached.map((terminal) => terminal.id)).toEqual(['t1']);
    expect(written).toEqual([[104, 105]]);
    expect(exits).toEqual([3]);
  });

  it('a control frame it cannot read does not end the view', () => {
    connect();
    sockets[0]?.open();
    sockets[0]?.say('{not json');
    sockets[0]?.bytes([1]);

    expect(written).toEqual([[1]]);
    expect(statuses).toEqual([]);
  });

  it('sends a resize only once the socket is open', () => {
    const socket = connect();
    socket.resize(100, 30);
    expect(sockets[0]?.sent).toEqual([]);

    sockets[0]?.open();
    socket.resize(100, 30);

    expect(sockets[0]?.sent).toEqual([JSON.stringify({ type: 'resize', cols: 100, rows: 30 })]);
  });

  it('attaches again after a drop, and replays the screen it is given', () => {
    connect();
    sockets[0]?.open();
    sockets[0]?.say(
      JSON.stringify({ type: 'attached', terminal: pane({ id: 't1' }), replay_bytes: 0 }),
    );

    sockets[0]?.drop();
    expect(statuses).toEqual(['reconnecting']);
    vi.advanceTimersByTime(500);

    expect(sockets).toHaveLength(2);
    sockets[1]?.open();
    sockets[1]?.say(
      JSON.stringify({ type: 'attached', terminal: pane({ id: 't1' }), replay_bytes: 1 }),
    );
    sockets[1]?.bytes([7]);

    expect(attached).toHaveLength(2);
    expect(written).toEqual([[7]]);
  });

  it('waits longer between attempts that keep failing', () => {
    connect();
    sockets[0]?.open();

    sockets[0]?.drop();
    vi.advanceTimersByTime(500);
    sockets[1]?.drop();
    vi.advanceTimersByTime(500);
    expect(sockets).toHaveLength(2);

    vi.advanceTimersByTime(500);
    expect(sockets).toHaveLength(3);
  });

  it('does not attach again when the pane was closed', () => {
    const socket = connect();
    sockets[0]?.open();

    socket.close();
    vi.advanceTimersByTime(60_000);

    expect(statuses).toEqual(['detached']);
    expect(sockets).toHaveLength(1);
  });

  it('does not attach again after the program exited', () => {
    connect();
    sockets[0]?.open();
    sockets[0]?.say(JSON.stringify({ type: 'exit' }));

    sockets[0]?.drop();
    vi.advanceTimersByTime(60_000);

    expect(sockets).toHaveLength(1);
    expect(exits).toEqual([undefined]);
  });
});
