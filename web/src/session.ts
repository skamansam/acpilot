// The daemon's session stream, read through `fetch` and folded for the page.
//
// `EventSource` is the obvious thing for server-sent events and the wrong thing
// here: every frame needs `Authorization`, and `EventSource` cannot send a header.
// The alternative it leaves is a token in the query string, which is the one place
// `acpilotd::auth` refuses to look. So this module reads the same frames from a
// `fetch` body and parses them itself — the format is a handful of lines, and the
// only thing lost is the browser's own reconnect, which is replaced below by one
// that carries the last sequence it saw.
//
// The transcript folding and the markdown pass are here rather than in the
// component because they are rules, not drawing: what merges into one message,
// which payload key holds the text, and that agent output can never become markup.

import { ApiError, type PermissionOption, type PermissionRequest, type SessionEvent } from './api';

/** One parsed server-sent event frame. */
export interface SseFrame {
  event: string;
  id?: string;
  data: string;
}

/**
 * A streaming parser for the SSE wire format.
 *
 * Chunk boundaries fall anywhere — inside a line, between `\r` and `\n`, inside a
 * JSON value — so a frame is only returned once its terminating blank line has
 * arrived. Carriage returns are dropped rather than parsed: this daemon's `data:`
 * lines are JSON, which cannot carry a raw one, and dropping them means a chunk
 * split between the two halves of a CRLF is harmless.
 */
export class SseParser {
  private buffer = '';

  push(chunk: string): SseFrame[] {
    this.buffer = (this.buffer + chunk).replaceAll('\r', '');
    const frames: SseFrame[] = [];
    let boundary = this.buffer.indexOf('\n\n');
    while (boundary !== -1) {
      const block = this.buffer.slice(0, boundary);
      this.buffer = this.buffer.slice(boundary + 2);
      const frame = this.block(block);
      if (frame) {
        frames.push(frame);
      }
      boundary = this.buffer.indexOf('\n\n');
    }
    return frames;
  }

  private block(block: string): SseFrame | null {
    let event = 'message';
    let id: string | undefined;
    const data: string[] = [];
    for (const line of block.split('\n')) {
      // A comment is the daemon's keep-alive, and an empty line here is the
      // trailing one a chunk boundary left behind.
      if (line === '' || line.startsWith(':')) {
        continue;
      }
      const colon = line.indexOf(':');
      const field = colon === -1 ? line : line.slice(0, colon);
      let value = colon === -1 ? '' : line.slice(colon + 1);
      if (value.startsWith(' ')) {
        value = value.slice(1);
      }
      if (field === 'event') {
        event = value;
      } else if (field === 'id') {
        id = value;
      } else if (field === 'data') {
        data.push(value);
      }
    }
    // A frame with no data says nothing a page can draw: `retry:` and a comment
    // are the two the daemon sends, and neither names an event of its own.
    if (data.length === 0) {
      return null;
    }
    return { event, id, data: data.join('\n') };
  }
}

/** How a stream is doing, in the words its header prints. */
export type StreamStatus = 'connecting' | 'live' | 'reconnecting' | 'ended';

interface StreamOptions {
  token: string;
  onFrame(frame: SseFrame): void;
  onStatus?(status: StreamStatus): void;
  onEnd?(): void;
  onError?(error: unknown): void;
  fetchImpl?: typeof fetch;
  /** The first wait after a drop; doubles to `retryMaxMs`. Tests pass 0. */
  retryMs?: number;
  retryMaxMs?: number;
}

/** A stream that can be left. */
export interface StreamHandle {
  close(): void;
}

/**
 * Read `url()` as SSE, reconnecting after an error.
 *
 * A *clean* end is not retried: it is how a stored conversation's replay ends, and
 * a page that reconnected forever to a history would be a page that never stopped
 * asking. A network failure is retried with a backoff, and `url()` is called again
 * so the next attempt can carry the sequence the last one reached.
 */
function openStream(url: () => string, options: StreamOptions): StreamHandle {
  const doFetch = options.fetchImpl ?? fetch;
  const first = options.retryMs ?? 500;
  const cap = options.retryMaxMs ?? 5000;
  let closed = false;
  let finished = false;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let delay = first;
  let controller: AbortController | null = null;

  const schedule = (): void => {
    if (closed) {
      return;
    }
    options.onStatus?.('reconnecting');
    retry = setTimeout(() => {
      retry = null;
      void run();
    }, delay);
    delay = Math.min(delay * 2, cap);
  };

  async function run(): Promise<void> {
    if (closed) {
      return;
    }
    controller = new AbortController();
    try {
      const response = await doFetch(url(), {
        headers: {
          Authorization: `Bearer ${options.token}`,
          Accept: 'text/event-stream',
        },
        signal: controller.signal,
      });
      if (response.status === 401) {
        throw new ApiError(
          401,
          'That token was not accepted. Copy it from the daemon log or token file.',
        );
      }
      if (!response.ok) {
        const body = await response.text().catch(() => '');
        throw new ApiError(
          response.status,
          body.trim() || `The daemon answered ${response.status} ${response.statusText}.`,
        );
      }
      if (!response.body) {
        throw new Error('the daemon answered a stream with no body');
      }
      options.onStatus?.('live');
      delay = first;
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      const parser = new SseParser();
      for (;;) {
        const { value, done } = await reader.read();
        if (done) {
          break;
        }
        for (const frame of parser.push(decoder.decode(value, { stream: true }))) {
          options.onFrame(frame);
        }
      }
      for (const frame of parser.push(decoder.decode())) {
        options.onFrame(frame);
      }
      if (closed) {
        return;
      }
      finished = true;
      options.onStatus?.('ended');
      options.onEnd?.();
    } catch (error) {
      if (closed || controller?.signal.aborted) {
        return;
      }
      options.onError?.(error);
      // A refusal is not a drop: a rejected token or a session this daemon does
      // not know will answer the same way every time, and retrying it forever is
      // a page that never stops asking. Only `429` is worth another attempt.
      if (error instanceof ApiError && error.status >= 400 && error.status < 500 && error.status !== 429) {
        options.onStatus?.('ended');
        return;
      }
      schedule();
    }
  }

  options.onStatus?.('connecting');
  void run();

  return {
    close() {
      if (closed) {
        return;
      }
      closed = true;
      controller?.abort();
      if (retry !== null) {
        clearTimeout(retry);
        retry = null;
      }
      // A stream that already ended said so; saying it again from the teardown
      // would make leaving the view look like a dropped connection.
      if (!finished) {
        options.onStatus?.('ended');
      }
    },
  };
}

/** `GET /v1/sessions/{id}/events?after=`, same-origin with the page. */
export function sessionEventsPath(id: string, after: number): string {
  return `/v1/sessions/${encodeURIComponent(id)}/events?after=${after}`;
}

export interface SessionStreamOptions {
  id: string;
  token: string;
  /** The first sequence to ask for; a fresh view asks for everything. */
  after?: number;
  onEvent?(event: SessionEvent): void;
  /** The daemon could not replay a gap; the caller refetches state. */
  onResync?(latest: number): void;
  onStatus?(status: StreamStatus): void;
  onEnd?(): void;
  onError?(error: unknown): void;
  fetchImpl?: typeof fetch;
  retryMs?: number;
}

/**
 * Follow one session's transcript.
 *
 * The reconnect point is the last sequence delivered, so a dropped connection
 * replays exactly the turn the page missed — the ring and the database are what
 * make that possible on the daemon's side. `resync` carries the newest sequence
 * instead of the events that were skipped, which is the daemon saying its replay
 * buffer ran out: the page refetches state and the next attempt starts after it.
 */
export function followSession(options: SessionStreamOptions): StreamHandle {
  let after = options.after ?? 0;
  return openStream(() => sessionEventsPath(options.id, after), {
    token: options.token,
    fetchImpl: options.fetchImpl,
    retryMs: options.retryMs,
    onStatus: (status) => options.onStatus?.(status),
    onEnd: () => options.onEnd?.(),
    onError: (error) => options.onError?.(error),
    onFrame(frame) {
      if (frame.event === 'session') {
        let event: SessionEvent;
        try {
          event = JSON.parse(frame.data) as SessionEvent;
        } catch {
          // A frame that is not the event it claims to be is not worth ending a
          // transcript over; the next sequence still advances the cursor.
          return;
        }
        if (typeof event.seq === 'number' && event.seq > after) {
          after = event.seq;
        }
        options.onEvent?.(event);
      } else if (frame.event === 'resync') {
        const latest = latestSeq(frame.data);
        if (latest > after) {
          after = latest;
        }
        options.onResync?.(latest);
      }
    },
  });
}

export interface PermissionStreamOptions {
  token: string;
  onRequested(request: PermissionRequest): void;
  onResolved(id: string, option: PermissionOption | null): void;
  onResync?(): void;
  onError?(error: unknown): void;
  fetchImpl?: typeof fetch;
  retryMs?: number;
}

/**
 * Follow `GET /v1/events` for permission frames.
 *
 * The session stream carries a conversation; this one carries the questions a
 * conversation is blocked on. Both are needed for the same view: a request raised
 * while the page was closed is recovered from the pending list, and one raised
 * while it is open arrives here instead of on a poll.
 */
export function followPermissions(options: PermissionStreamOptions): StreamHandle {
  return openStream(() => '/v1/events', {
    token: options.token,
    fetchImpl: options.fetchImpl,
    retryMs: options.retryMs,
    onError: (error) => options.onError?.(error),
    onFrame(frame) {
      if (frame.event === 'resync') {
        options.onResync?.();
        return;
      }
      if (frame.event !== 'permission') {
        return;
      }
      let payload: Record<string, unknown>;
      try {
        payload = asRecord(JSON.parse(frame.data));
      } catch {
        return;
      }
      const type = text(payload.type);
      const request = asRecord(payload.request);
      if (type === 'requested' && text(request.id) && text(request.session_id)) {
        options.onRequested(request as unknown as PermissionRequest);
        return;
      }
      if (type === 'resolved') {
        const id = text(payload.request_id) ?? text(payload.id);
        if (id) {
          const decision = asRecord(payload.decision);
          options.onResolved(id, option(decision.option) ?? option(payload.option));
        }
      }
    },
  });
}

function latestSeq(data: string): number {
  try {
    const value = asRecord(JSON.parse(data)).latest_seq;
    return typeof value === 'number' && Number.isFinite(value) ? value : 0;
  } catch {
    return 0;
  }
}

/** One turn, folded from the events that built it. */
export type Turn =
  | { kind: 'message'; seq: number; role: string; text: string }
  | { kind: 'thought'; seq: number; text: string }
  | {
      kind: 'tool';
      seq: number;
      id: string;
      title: string;
      status: string;
      tool: string | null;
      detail: string | null;
    }
  | { kind: 'notice'; seq: number; level: string; title: string; message: string | null }
  | { kind: 'plan'; seq: number; entries: string[] }
  | { kind: 'mode'; seq: number; mode: string }
  | { kind: 'other'; seq: number; label: string; text: string };

/**
 * Fold a session's events into the transcript a person reads.
 *
 * Streaming means the unit on the wire is a chunk, not a message: consecutive
 * chunks of one role are one paragraph, and a tool call's later updates patch the
 * card it belongs to instead of appending a second one. Everything is appended in
 * sequence order, which is the order the daemon recorded and replayed it.
 */
export function buildTranscript(events: SessionEvent[]): Turn[] {
  const turns: Turn[] = [];
  for (const event of events) {
    const payload = event.payload ?? {};
    if (event.kind === 'message_chunk') {
      const role = text(payload.role) ?? 'assistant';
      const body = contentText(payload);
      const last = turns.at(-1);
      if (last?.kind === 'message' && last.role === role) {
        last.text += body;
      } else {
        turns.push({ kind: 'message', seq: event.seq, role, text: body });
      }
      continue;
    }
    if (event.kind === 'thought') {
      const body = contentText(payload);
      const last = turns.at(-1);
      if (last?.kind === 'thought') {
        last.text += body;
      } else {
        turns.push({ kind: 'thought', seq: event.seq, text: body });
      }
      continue;
    }
    if (event.kind === 'tool_call') {
      const id = text(payload.tool_call_id) ?? `seq-${event.seq}`;
      const title = text(payload.title);
      const status = text(payload.status);
      const tool = text(payload.kind) ?? text(payload.name);
      const detail = toolDetail(payload);
      let index = -1;
      for (let at = turns.length - 1; at >= 0; at -= 1) {
        const turn = turns[at];
        if (turn?.kind === 'tool' && turn.id === id) {
          index = at;
          break;
        }
      }
      if (index !== -1) {
        const existing = turns[index] as Extract<Turn, { kind: 'tool' }>;
        turns[index] = {
          ...existing,
          title: title ?? existing.title,
          status: status ?? existing.status,
          tool: tool ?? existing.tool,
          detail: detail ?? existing.detail,
        };
      } else {
        turns.push({
          kind: 'tool',
          seq: event.seq,
          id,
          title: title ?? id,
          status: status ?? 'unknown',
          tool,
          detail,
        });
      }
      continue;
    }
    if (event.kind === 'notice') {
      turns.push({
        kind: 'notice',
        seq: event.seq,
        level: text(payload.level) ?? 'info',
        title: text(payload.title) ?? 'Notice',
        message: text(payload.message),
      });
      continue;
    }
    if (event.kind === 'plan') {
      const entries = Array.isArray(payload.entries)
        ? payload.entries.map((entry) => text(asRecord(entry).content) ?? String(entry))
        : [];
      turns.push({ kind: 'plan', seq: event.seq, entries });
      continue;
    }
    if (event.kind === 'mode_changed') {
      turns.push({
        kind: 'mode',
        seq: event.seq,
        mode: text(payload.mode_id) ?? text(payload.mode) ?? 'unknown',
      });
      continue;
    }
    if (event.kind === 'compaction') {
      const summary = text(payload.summary) ?? contentText(payload);
      turns.push({
        kind: 'other',
        seq: event.seq,
        label: 'Compaction',
        text: summary || text(payload.status) || 'the conversation was compacted',
      });
      continue;
    }
    if (event.kind === 'commands_changed') {
      const commands = Array.isArray(payload.commands) ? payload.commands.length : 0;
      turns.push({
        kind: 'other',
        seq: event.seq,
        label: 'Commands',
        text: `${commands} ${commands === 1 ? 'command' : 'commands'} available`,
      });
      continue;
    }
    turns.push({
      kind: 'other',
      seq: event.seq,
      label: String(event.kind),
      text: JSON.stringify(payload),
    });
  }
  return turns;
}

/**
 * The text a chunk carries.
 *
 * `SessionEvent` payloads are the adapter's own shape: ACP content chunks put the
 * text under `content.text`, an older shape put it under `text`, and a test or a
 * fixture may put a bare string under `content`. All three are read here so the
 * page cannot lose a turn to a field name.
 */
function contentText(payload: Record<string, unknown>): string {
  if (typeof payload.content === 'string') {
    return payload.content;
  }
  const block = asRecord(payload.content);
  if (typeof block.text === 'string') {
    return block.text;
  }
  if (typeof payload.text === 'string') {
    return payload.text;
  }
  if (Array.isArray(payload.content)) {
    return payload.content.map((item) => text(asRecord(item).text) ?? '').join('');
  }
  return '';
}

function toolDetail(payload: Record<string, unknown>): string | null {
  for (const key of ['raw_input', 'raw_output']) {
    if (payload[key] !== undefined) {
      return JSON.stringify(payload[key], null, 2);
    }
  }
  const body = contentText(payload);
  return body || null;
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null ? (value as Record<string, unknown>) : {};
}

function text(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function option(value: unknown): PermissionOption | null {
  return value === 'allow_once' || value === 'allow_always' || value === 'deny' ? value : null;
}

/** The words the permission card puts on an answer. */
export function permissionLabel(choice: PermissionOption): string {
  if (choice === 'allow_once') {
    return 'Allow once';
  }
  if (choice === 'allow_always') {
    return 'Allow always';
  }
  return 'Deny';
}

/** A run of inline text: the leaf of the markdown tree. */
export interface Span {
  text: string;
  code?: boolean;
  strong?: boolean;
  em?: boolean;
  href?: string;
}

/** One block of rendered markdown. */
export type MarkdownBlock =
  | { kind: 'paragraph'; spans: Span[] }
  | { kind: 'heading'; level: number; spans: Span[] }
  | { kind: 'code'; language: string; text: string }
  | { kind: 'list'; ordered: boolean; items: Span[][] }
  | { kind: 'quote'; spans: Span[] };

const FENCE = /^\s*```\s*([\w+-]*)\s*$/;
const HEADING = /^(#{1,6})\s+(.*)$/;
const BULLET = /^\s*[-*+]\s+(.*)$/;
const ORDERED = /^\s*\d+[.)]\s+(.*)$/;

/**
 * The markdown subset an agent's messages actually use.
 *
 * Returns data, never HTML: the component renders spans as text and the only
 * attribute that survives is an `http(s)` link. That is the security property, not
 * a limitation — an agent that writes `<img onerror=…>` into its transcript gets
 * four characters of angle brackets printed at the reader, exactly as it should.
 */
export function markdown(source: string): MarkdownBlock[] {
  const blocks: MarkdownBlock[] = [];
  const lines = source.replaceAll('\r', '').split('\n');
  let paragraph: string[] = [];
  const flush = (): void => {
    if (paragraph.length > 0) {
      blocks.push({ kind: 'paragraph', spans: inline(paragraph.join(' ')) });
      paragraph = [];
    }
  };

  let index = 0;
  while (index < lines.length) {
    const line = lines[index] ?? '';
    const fence = FENCE.exec(line);
    if (fence) {
      flush();
      const body: string[] = [];
      index += 1;
      while (index < lines.length && !/^\s*```\s*$/.test(lines[index] ?? '')) {
        body.push(lines[index] ?? '');
        index += 1;
      }
      // A fence still open is a stream mid-block: everything so far is the code.
      index += 1;
      blocks.push({ kind: 'code', language: fence[1] ?? '', text: body.join('\n') });
      continue;
    }
    if (!line.trim()) {
      flush();
      index += 1;
      continue;
    }
    const heading = HEADING.exec(line);
    if (heading) {
      flush();
      blocks.push({
        kind: 'heading',
        level: (heading[1] ?? '#').length,
        spans: inline(heading[2] ?? ''),
      });
      index += 1;
      continue;
    }
    if (/^\s*>/.test(line)) {
      flush();
      const quoted: string[] = [];
      while (index < lines.length && /^\s*>/.test(lines[index] ?? '')) {
        quoted.push((lines[index] ?? '').replace(/^\s*>\s?/, ''));
        index += 1;
      }
      blocks.push({ kind: 'quote', spans: inline(quoted.join(' ')) });
      continue;
    }
    const bullet = BULLET.exec(line);
    const numbered = ORDERED.exec(line);
    if (bullet || numbered) {
      flush();
      const ordered = numbered !== null;
      const items: Span[][] = [];
      for (;;) {
        const current = ordered ? ORDERED.exec(lines[index] ?? '') : BULLET.exec(lines[index] ?? '');
        if (!current) {
          break;
        }
        items.push(inline(current[1] ?? ''));
        index += 1;
      }
      blocks.push({ kind: 'list', ordered, items });
      continue;
    }
    paragraph.push(line.trim());
    index += 1;
  }
  flush();
  return blocks;
}

/** Inline markdown: code, bold, emphasis and links, in that precedence. */
export function inline(source: string): Span[] {
  const spans: Span[] = [];
  let literal = '';
  const flush = (): void => {
    if (literal) {
      spans.push({ text: literal });
      literal = '';
    }
  };
  let index = 0;
  while (index < source.length) {
    if (source.startsWith('`', index)) {
      const end = source.indexOf('`', index + 1);
      if (end !== -1) {
        flush();
        spans.push({ text: source.slice(index + 1, end), code: true });
        index = end + 1;
        continue;
      }
    }
    if (source.startsWith('**', index)) {
      const end = source.indexOf('**', index + 2);
      if (end !== -1) {
        flush();
        spans.push({ text: source.slice(index + 2, end), strong: true });
        index = end + 2;
        continue;
      }
    }
    if (source[index] === '[') {
      const label = source.indexOf('](', index);
      if (label !== -1) {
        const end = source.indexOf(')', label + 2);
        if (end !== -1) {
          const href = source.slice(label + 2, end);
          if (/^https?:\/\//i.test(href)) {
            flush();
            spans.push({ text: source.slice(index + 1, label), href });
            index = end + 1;
            continue;
          }
        }
      }
    }
    const marker = source[index];
    // An underscore inside a word is a name (`snake_case`), not emphasis; an
    // asterisk is emphasis wherever it has a partner.
    const wordBefore = /[\p{L}\p{N}]/u.test(source[index - 1] ?? '');
    if ((marker === '*' || (marker === '_' && !wordBefore)) && source[index + 1] !== marker) {
      const end = source.indexOf(marker ?? '', index + 1);
      if (end > index + 1) {
        flush();
        spans.push({ text: source.slice(index + 1, end), em: true });
        index = end + 1;
        continue;
      }
    }
    literal += source[index];
    index += 1;
  }
  flush();
  return spans;
}

/** A failure as the one sentence the page shows. */
export function describeError(cause: unknown): string {
  if (cause instanceof ApiError) {
    return cause.message;
  }
  return cause instanceof Error ? cause.message : String(cause);
}
