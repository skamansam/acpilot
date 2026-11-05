// The daemon's terminals, and the socket a pane is drawn from.
//
// Kept out of the components for one reason: a WebSocket and a terminal emulator
// are both things a unit test cannot usefully fake *through a component*, and the
// three rules that matter here — which panes are listed, where a pane runs, and
// what happens when the socket closes — are the page's own logic. `Terminal.svelte`
// is the drawing; this module is the contract with the daemon.

import type { TerminalSummary } from './api';

/**
 * The protocol the daemon agrees to on an attach socket.
 *
 * The token travels as the *second* protocol a browser offers, because a browser
 * sets no headers on a WebSocket. The daemon answers with this name and never
 * echoes the token (see `acpilotd::auth`).
 */
export const PROTOCOL = 'acpilot.v1';

/** `WS /v1/terminals/{id}/attach`, same-origin with the page. */
export function attachPath(id: string): string {
  return `/v1/terminals/${encodeURIComponent(id)}/attach`;
}

/**
 * The panes a picker lists: `openSession`'s, plus every pane no session owns.
 *
 * A pane with no `session_id` is not another session's leftover — it is a shell
 * in a remote workspace, or an agent's shell whose session the daemon could not
 * map — so filtering strictly by session hides a process nobody can then find.
 * With no session open (this page has no session view yet, P11-4) every pane is
 * listed, which is the same rule with nothing to exclude.
 */
export function panesFor(
  panes: TerminalSummary[],
  openSession: string | null,
): TerminalSummary[] {
  if (!openSession) {
    return panes;
  }
  return panes.filter((pane) => !pane.session_id || pane.session_id === openSession);
}

/**
 * Where a pane runs, in the words the terminal's own picker prints.
 *
 * A pane on another host looks exactly like a local one otherwise, and the pane a
 * person is about to type into is the one that must not be mistaken for this
 * machine's.
 */
export function paneWhere(pane: TerminalSummary): string {
  const origin = `${pane.workspace ?? 'this workspace'} on ${pane.host ?? 'this machine'}`;
  if (!pane.session_id) {
    return `no session · ${origin}`;
  }
  // A session's pane runs where the session does, unless the daemon said otherwise
  // (a remote workspace's shell is the session's while the workspace is not).
  return pane.workspace || pane.host ? origin : `session ${pane.session_id}`;
}

/** What an attaching pane hears from the daemon. */
export type AttachNotice =
  | { type: 'attached'; terminal: TerminalSummary; replay_bytes: number }
  | { type: 'exit'; code?: number };

/** What a scripted client can do to an attached pane. */
export interface PaneSocket {
  /** Tell the program the pane changed shape, when it is safe to send. */
  resize(cols: number, rows: number): void;
  /** Leave; the program keeps running, which is what attaching rather than owning means. */
  close(): void;
}

export interface AttachOptions {
  id: string;
  token: string;
  /** Live terminal bytes, exactly as they arrived. */
  write(bytes: Uint8Array): void;
  /** The screen replay follows this notice, so a caller resets before writing. */
  attached?(terminal: TerminalSummary): void;
  exit?(code: number | undefined): void;
  status?(status: 'reconnecting' | 'detached'): void;
  /** Tests hand in a socket; the page uses the browser's. */
  socketFactory?(url: string, protocols: string[]): WebSocket;
}

const RETRY_MS = 500;
const RETRY_MAX_MS = 5000;

/**
 * Attach to `id`, and keep attaching while the socket drops.
 *
 * A close is not the program ending: a daemon restart, an idle proxy and a pane
 * that fell behind all look the same from here. The daemon replays the current
 * screen on every attach, so reconnecting repairs the view instead of leaving a
 * screen with a hole in the middle of it. An `exit` notice is the one close that
 * is final.
 */
export function attachPane(options: AttachOptions): PaneSocket {
  const factory =
    options.socketFactory ??
    ((url: string, protocols: string[]) => new WebSocket(url, protocols));
  let socket: WebSocket | null = null;
  let closed = false;
  let finished = false;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let delay = RETRY_MS;

  function connect(): void {
    const current = factory(attachPath(options.id), [PROTOCOL, options.token]);
    current.binaryType = 'arraybuffer';
    socket = current;
    current.onopen = () => {
      delay = RETRY_MS;
    };
    current.onmessage = (event: MessageEvent) => {
      const { data } = event;
      if (typeof data === 'string') {
        let notice: AttachNotice;
        try {
          notice = JSON.parse(data) as AttachNotice;
        } catch {
          // A text frame on this socket is a control frame; one we cannot read is
          // not worth ending a view over.
          return;
        }
        if (notice.type === 'attached') {
          options.attached?.(notice.terminal);
        } else if (notice.type === 'exit') {
          finished = true;
          options.exit?.(notice.code);
        }
        return;
      }
      // `binaryType` is set above, so this is an `ArrayBuffer` and not a `Blob`.
      options.write(new Uint8Array(data as ArrayBuffer));
    };
    current.onclose = () => {
      if (closed || finished) {
        return;
      }
      options.status?.('reconnecting');
      retry = setTimeout(connect, delay);
      delay = Math.min(delay * 2, RETRY_MAX_MS);
    };
  }

  connect();

  return {
    resize(cols, rows) {
      // Only an open socket can be told: a control frame sent before the upgrade
      // is dropped, and the replay's own size is the daemon's until then.
      if (socket?.readyState === 1) {
        socket.send(JSON.stringify({ type: 'resize', cols, rows }));
      }
    },
    close() {
      closed = true;
      if (retry !== null) {
        clearTimeout(retry);
        retry = null;
      }
      options.status?.('detached');
      socket?.close();
      socket = null;
    },
  };
}
