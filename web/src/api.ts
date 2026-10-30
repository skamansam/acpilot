// The daemon API, as much of it as the shell uses.
//
// Hand-written rather than generated from the OpenAPI document: the generated
// client (P11-2) is a phase of its own, and the two routes here are small enough
// that a wrong field name is caught by `svelte-check` in the component instead
// of by a browser at runtime. When the generated client lands, this module is
// the only thing that changes.

/** What `GET /v1/health` answers. */
export interface Health {
  ok: boolean;
  service: string;
  version: string;
  api: string;
  /** The one workspace this daemon serves, when it was started against one. */
  workspace: string | null;
}

/** Lifecycle state, as `GET /v1/sessions` spells it. */
export type SessionStatus =
  | 'idle'
  | 'busy'
  | 'awaiting_permission'
  | 'closed'
  | 'failed';

/** One row of `GET /v1/sessions`. */
export interface SessionSummary {
  id: string;
  workspace_id: string;
  agent_id: string;
  title: string;
  status: SessionStatus;
  created_at: string;
  updated_at: string;
}

/** Where one figure came from, as the daemon spells it. */
export type StatSource = 'server' | 'configured' | 'estimated' | 'unknown';

/**
 * One measured quantity.
 *
 * `fill` is absent when the denominator was not reported — a track with no fill,
 * never a full or an empty one. `text` is the same sentence the TUI draws, spelled
 * by the daemon from the same crate, so the two bars cannot disagree.
 */
export interface Track {
  fill: number | null;
  text: string;
  source: StatSource;
}

/** One integration's row in the stats bar, as `GET /app/stats` measures it. */
export interface StatRow {
  id: string;
  kind: 'provider' | 'agent';
  label: string;
  context?: Track;
  credits?: Track;
  updated_at: string;
  stale: boolean;
}

/** Why a call failed, in the terms the page shows. */
export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

/** The token, in memory and in the tab's session storage — never a cookie. */
const TOKEN_KEY = 'acpilot.token';

export function readToken(): string {
  try {
    return sessionStorage.getItem(TOKEN_KEY) ?? '';
  } catch {
    // Storage can be denied by a browser setting; the page still works, it just
    // asks again on reload.
    return '';
  }
}

export function writeToken(token: string): void {
  try {
    if (token) {
      sessionStorage.setItem(TOKEN_KEY, token);
    } else {
      sessionStorage.removeItem(TOKEN_KEY);
    }
  } catch {
    // As above: a refused write is not a failed connection.
  }
}

async function get<T>(path: string, token: string, signal?: AbortSignal): Promise<T> {
  const response = await fetch(path, {
    headers: { Authorization: `Bearer ${token}`, Accept: 'application/json' },
    signal,
  });
  if (response.status === 401) {
    throw new ApiError(401, 'That token was not accepted. Copy it from the daemon log or token file.');
  }
  if (!response.ok) {
    // The daemon answers errors as JSON when it can; a proxy in front of it may
    // answer HTML instead, which is why the body is not parsed as a contract.
    const body = await response.text().catch(() => '');
    throw new ApiError(
      response.status,
      body.trim() || `The daemon answered ${response.status} ${response.statusText}.`,
    );
  }
  return (await response.json()) as T;
}

export function health(token: string, signal?: AbortSignal): Promise<Health> {
  return get<Health>('/v1/health', token, signal);
}

export function sessions(token: string, signal?: AbortSignal): Promise<SessionSummary[]> {
  return get<SessionSummary[]>('/v1/sessions', token, signal);
}

/**
 * `GET /app/stats` — the page's copy of the stats bar.
 *
 * Under `/app` rather than `/v1` because it is not the contract: a browser cannot
 * link the crate that does the arithmetic, so the daemon measures the contract's
 * rows for it. It needs the token like every other call — the roster is not the
 * page's own business — which is why it is a `fetch` and not an `EventSource`.
 */
export function stats(token: string, signal?: AbortSignal): Promise<StatRow[]> {
  return get<StatRow[]>('/app/stats', token, signal);
}
