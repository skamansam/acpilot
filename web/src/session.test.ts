// The session view's contract with the daemon, without a browser.
//
// Three things here are easy to break and invisible in a screenshot: the stream
// carries the token in a header rather than the URL, a reconnect resumes at the
// last sequence instead of starting over, and agent text is rendered as text —
// markdown's own syntax is the only markup that ever survives.

import { describe, expect, it, vi } from 'vitest';
import type { SessionEvent } from './api';
import {
  buildTranscript,
  followPermissions,
  followSession,
  inline,
  markdown,
  permissionLabel,
  sessionEventsPath,
  SseParser,
} from './session';

const TOKEN = 'the-daemon-token';

function chunk(seq: number, role: string, text: string): SessionEvent {
  return {
    seq,
    session_id: 's1',
    kind: 'message_chunk',
    at: '2026-11-05T10:00:00Z',
    payload: { role, content: { text } },
  };
}

function frame(event: string, data: unknown, id?: string): string {
  const lines = [`event: ${event}`];
  if (id !== undefined) {
    lines.push(`id: ${id}`);
  }
  lines.push(`data: ${JSON.stringify(data)}`);
  return `${lines.join('\n')}\n\n`;
}

function sse(text: string, status = 200): Response {
  return new Response(text, {
    status,
    headers: { 'Content-Type': 'text/event-stream' },
  });
}

describe('the SSE parser', () => {
  it('holds a frame until its blank line arrives', () => {
    const parser = new SseParser();

    expect(parser.push('event: session\ndata: {"seq":1}')).toEqual([]);
    expect(parser.push('\n\ndata: {"seq":2}\n\n')).toEqual([
      { event: 'session', id: undefined, data: '{"seq":1}' },
      { event: 'message', id: undefined, data: '{"seq":2}' },
    ]);
  });

  it('joins multi-line data, keeps ids, and ignores comments', () => {
    const parser = new SseParser();

    const frames = parser.push(': keep-alive\nevent: session\nid: 7\ndata: {"a":\ndata: 1}\n\n');

    expect(frames).toEqual([{ event: 'session', id: '7', data: '{"a":\n1}' }]);
  });

  it('drops carriage returns rather than splitting on them', () => {
    const parser = new SseParser();

    expect(parser.push('event: heartbeat\r\ndata: {"seq":3}\r\n\r\n')).toEqual([
      { event: 'heartbeat', id: undefined, data: '{"seq":3}' },
    ]);
  });
});

describe('the session stream', () => {
  it('asks with the token in a header, never in the URL', async () => {
    const fetchMock = vi.fn(async () =>
      sse(frame('session', chunk(1, 'assistant', 'hello'), '1')),
    );
    const events: SessionEvent[] = [];

    const stream = followSession({
      id: 's1',
      token: TOKEN,
      onEvent: (event) => events.push(event),
      fetchImpl: fetchMock as unknown as typeof fetch,
    });
    await vi.waitFor(() => expect(events).toHaveLength(1));
    stream.close();

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe('/v1/sessions/s1/events?after=0');
    expect(url).not.toContain(TOKEN);
    expect((init.headers as Record<string, string>).Authorization).toBe(`Bearer ${TOKEN}`);
    expect((init.headers as Record<string, string>).Accept).toBe('text/event-stream');
    expect(events[0]?.seq).toBe(1);
    expect(sessionEventsPath('a/b', 4)).toBe('/v1/sessions/a%2Fb/events?after=4');
  });

  it('resumes at the last sequence it saw after a drop', async () => {
    // A body that delivers one frame and then fails: the socket halfway through,
    // which is the case the replay ring exists for.
    const encoder = new TextEncoder();
    const broken = new Response(
      new ReadableStream<Uint8Array>({
        start(controller) {
          controller.enqueue(
            encoder.encode(frame('session', chunk(4, 'assistant', 'half a turn'), '4')),
          );
          // The error comes after the frame has been read, which is what a socket
          // cut in the middle of a turn looks like: one event arrives, then the
          // read fails.
          setTimeout(() => controller.error(new Error('connection reset')), 0);
        },
      }),
      { status: 200, headers: { 'Content-Type': 'text/event-stream' } },
    );
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(broken)
      .mockResolvedValueOnce(sse(''));
    const seen: number[] = [];

    const stream = followSession({
      id: 's1',
      token: TOKEN,
      retryMs: 1,
      onEvent: (event) => seen.push(event.seq),
      fetchImpl: fetchMock as unknown as typeof fetch,
    });
    // `waitFor` polls; a 1 ms backoff can retry several times before the poll
    // looks, so the assertion is that a second attempt happened, not that it was
    // the second call overall.
    await vi.waitFor(() => expect(fetchMock.mock.calls.length).toBeGreaterThanOrEqual(2));
    stream.close();

    expect(seen).toEqual([4]);
    expect(String(fetchMock.mock.calls[1]?.[0])).toBe('/v1/sessions/s1/events?after=4');
  });

  it('a resync hands the caller the newest sequence and resumes after it', async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(sse(frame('resync', { latest_seq: 41 })))
      .mockResolvedValueOnce(sse(''));
    const resyncs: number[] = [];

    const stream = followSession({
      id: 's1',
      token: TOKEN,
      onResync: (latest) => resyncs.push(latest),
      fetchImpl: fetchMock as unknown as typeof fetch,
    });
    // The frame names a sequence the stream no longer holds; the view refetches
    // state, and a reconnect starts after what the daemon still has.
    await vi.waitFor(() => expect(resyncs).toEqual([41]));
    stream.close();
  });

  it('an ended stream says so and is not retried', async () => {
    const fetchMock = vi.fn(async () => sse(frame('session', chunk(1, 'assistant', 'x'), '1')));
    const statuses: string[] = [];
    const ended = vi.fn();

    const stream = followSession({
      id: 's1',
      token: TOKEN,
      retryMs: 1,
      onStatus: (status) => statuses.push(status),
      onEnd: ended,
      onEvent: () => {},
      fetchImpl: fetchMock as unknown as typeof fetch,
    });
    await vi.waitFor(() => expect(ended).toHaveBeenCalled());
    stream.close();
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(statuses.at(-1)).toBe('ended');
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('a rejected token is reported, not retried forever', async () => {
    const fetchMock = vi.fn(async () => sse('denied', 401));
    const errors: unknown[] = [];

    const stream = followSession({
      id: 's1',
      token: 'stale',
      retryMs: 1,
      onError: (error) => errors.push(error),
      onEvent: () => {},
      fetchImpl: fetchMock as unknown as typeof fetch,
    });
    await vi.waitFor(() => expect(errors).toHaveLength(1));
    stream.close();

    expect(String(errors[0])).toMatch(/not accepted/i);
  });
});

describe('the permission stream', () => {
  it('reports requests and resolutions, and knows the documented id shape', async () => {
    const requested = {
      id: 'p1',
      session_id: 's1',
      kind: 'file_write',
      title: 'Write src/http.rs',
      detail: 'diff',
      options: ['allow_once', 'deny'],
      created_at: '2026-11-05T10:00:00Z',
    };
    const fetchMock = vi.fn(async () =>
      sse(
        frame('permission', { type: 'requested', request: requested }, '1') +
          frame('permission', { type: 'resolved', request_id: 'p1', option: 'deny' }, '2') +
          frame('permission', { type: 'resolved', id: 'p2', decision: { option: 'allow_once' } }, '3'),
      ),
    );
    const asked: string[] = [];
    const answered: [string, string | null][] = [];

    const stream = followPermissions({
      token: TOKEN,
      onRequested: (request) => asked.push(request.id),
      onResolved: (id, choice) => answered.push([id, choice]),
      fetchImpl: fetchMock as unknown as typeof fetch,
    });
    await vi.waitFor(() => expect(answered).toHaveLength(2));
    stream.close();

    expect(asked).toEqual(['p1']);
    expect(answered).toEqual([
      ['p1', 'deny'],
      ['p2', 'allow_once'],
    ]);
  });
});

describe('folding a transcript', () => {
  it('joins chunks of one role and starts a turn when the role changes', () => {
    const turns = buildTranscript([
      chunk(1, 'user', 'write '),
      chunk(2, 'assistant', 'the '),
      chunk(3, 'assistant', 'file'),
    ]);

    expect(turns).toEqual([
      { kind: 'message', seq: 1, role: 'user', text: 'write ' },
      { kind: 'message', seq: 2, role: 'assistant', text: 'the file' },
    ]);
  });

  it('patches a tool card instead of adding a second one', () => {
    const turns = buildTranscript([
      {
        seq: 1,
        session_id: 's1',
        kind: 'tool_call',
        at: '2026-11-05T10:00:00Z',
        payload: {
          tool_call_id: 't1',
          title: 'Read src/lib.rs',
          status: 'in_progress',
          kind: 'read',
        },
      },
      {
        seq: 2,
        session_id: 's1',
        kind: 'tool_call',
        at: '2026-11-05T10:00:01Z',
        payload: {
          tool_call_id: 't1',
          patch: true,
          title: 'Read src/lib.rs (done)',
          status: 'completed',
        },
      },
    ]);

    expect(turns).toHaveLength(1);
    expect(turns[0]).toMatchObject({
      kind: 'tool',
      id: 't1',
      title: 'Read src/lib.rs (done)',
      status: 'completed',
      tool: 'read',
    });
  });

  it('reads the payload shapes the adapters actually send', () => {
    const turns = buildTranscript([
      {
        seq: 1,
        session_id: 's1',
        kind: 'notice',
        at: '2026-11-05T10:00:00Z',
        payload: { level: 'warning', title: 'Careful', message: 'the sandbox is off' },
      },
      {
        seq: 2,
        session_id: 's1',
        kind: 'plan',
        at: '2026-11-05T10:00:01Z',
        payload: { entries: [{ content: 'read the ticket' }, 'write the test'] },
      },
      {
        seq: 3,
        session_id: 's1',
        kind: 'mode_changed',
        at: '2026-11-05T10:00:02Z',
        payload: { mode_id: 'code' },
      },
      {
        seq: 4,
        session_id: 's1',
        kind: 'compaction',
        at: '2026-11-05T10:00:03Z',
        payload: { content: { text: 'a summary' } },
      },
    ]);

    expect(turns[0]).toMatchObject({ kind: 'notice', level: 'warning', title: 'Careful' });
    expect(turns[1]).toEqual({
      kind: 'plan',
      seq: 2,
      entries: ['read the ticket', 'write the test'],
    });
    expect(turns[2]).toMatchObject({ kind: 'mode', mode: 'code' });
    expect(turns[3]).toMatchObject({ kind: 'other', label: 'Compaction', text: 'a summary' });
  });
});

describe('markdown', () => {
  it('draws headings, fences, lists and inline spans', () => {
    const blocks = markdown(
      ['## A heading', 'text with `code` and **bold** and *soft*', '', '```rs', 'fn main() {}', '```', '- one', '- two'].join('\n'),
    );

    expect(blocks[0]).toEqual({
      kind: 'heading',
      level: 2,
      spans: [{ text: 'A heading' }],
    });
    expect(blocks[1]).toMatchObject({
      kind: 'paragraph',
      spans: [
        { text: 'text with ' },
        { text: 'code', code: true },
        { text: ' and ' },
        { text: 'bold', strong: true },
        { text: ' and ' },
        { text: 'soft', em: true },
      ],
    });
    expect(blocks[2]).toEqual({ kind: 'code', language: 'rs', text: 'fn main() {}' });
    expect(blocks[3]).toEqual({
      kind: 'list',
      ordered: false,
      items: [[{ text: 'one' }], [{ text: 'two' }]],
    });
  });

  it('leaves HTML as text, so an agent cannot write markup into the page', () => {
    const spans = inline('<img src=x onerror=alert(1)>');

    expect(spans).toEqual([{ text: '<img src=x onerror=alert(1)>' }]);
    expect(markdown('<script>alert(1)</script>')[0]).toEqual({
      kind: 'paragraph',
      spans: [{ text: '<script>alert(1)</script>' }],
    });
  });

  it('only links schemes the page can safely follow', () => {
    expect(inline('[docs](https://example.com/a)')).toEqual([
      { text: 'docs', href: 'https://example.com/a' },
    ]);
    expect(inline('[bad](javascript:alert(1))')).toEqual([
      { text: '[bad](javascript:alert(1))' },
    ]);
  });

  it('a fence still open at the end of a chunk is code, not lost text', () => {
    expect(markdown('```\npartial')).toEqual([
      { kind: 'code', language: '', text: 'partial' },
    ]);
  });

  it('says an answer in the words the card uses', () => {
    expect(permissionLabel('allow_once')).toBe('Allow once');
    expect(permissionLabel('allow_always')).toBe('Allow always');
    expect(permissionLabel('deny')).toBe('Deny');
  });
});
