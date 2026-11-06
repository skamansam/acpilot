// The daemon API, as much of it as the shell uses.
//
// Hand-written rather than generated from the OpenAPI document: the generated
// client (P11-2) is a phase of its own, and the routes here are small enough that
// a wrong field name is caught by `svelte-check` in the component instead of by a
// browser at runtime. When the generated client lands, this module is the only
// thing that changes.

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

/** Availability, as `GET /v1/agents` spells it. */
export type AgentStatus = 'available' | 'missing' | 'unhealthy' | 'untrusted';

/** One row of `GET /v1/agents`: an agent this profile configured. */
export interface AgentSummary {
  id: string;
  name: string;
  transport: 'acp_stdio' | 'pty' | 'http_json_rpc';
  status: AgentStatus;
  /** The command that will be spawned, for display. */
  command: string;
  protocol_version?: string;
}

/** The closed set of things that can happen on a session's stream. */
export type SessionEventKind =
  | 'message_chunk'
  | 'thought'
  | 'tool_call'
  | 'plan'
  | 'mode_changed'
  | 'notice'
  | 'compaction'
  | 'commands_changed';

/**
 * One frame of `GET /v1/sessions/{id}/events`.
 *
 * `payload` is the agent's own shape and is deliberately not modelled: every
 * adapter puts different fields there, and a page that refused the ones it did
 * not know would hide a turn instead of drawing what it could.
 */
export interface SessionEvent {
  seq: number;
  session_id: string;
  kind: SessionEventKind;
  at: string;
  payload: Record<string, unknown>;
}

/** What a caller may answer a permission request with. */
export type PermissionOption = 'allow_once' | 'allow_always' | 'deny';

/** One pending request, as `GET /v1/permissions?status=pending` lists it. */
export interface PermissionRequest {
  id: string;
  session_id: string;
  kind: string;
  title: string;
  detail: string;
  options: PermissionOption[];
  created_at: string;
}

/** The decision the daemon applied, with the name of who made it. */
export interface PermissionDecision {
  option: PermissionOption;
  decided_by: string;
  at: string;
}

/** `POST /v1/sessions/{id}/cancel`'s answer. */
export interface CancelAccepted {
  turn_id: string | null;
  cancelled: boolean;
}

/** `POST /v1/sessions/{id}/prompt`'s answer: the turn was accepted, not finished. */
export interface PromptAccepted {
  session_id: string;
  turn_id: string;
  first_seq: number;
}

/** One row of `GET /v1/terminals`: a pane a client may attach to. */
export interface TerminalSummary {
  id: string;
  /** Absent for a pane no session owns: another workspace's shell, or an agent's
   * shell whose session the daemon could not map. */
  session_id?: string;
  /** The workspace the pane runs in, when it is not this daemon's own. */
  workspace?: string;
  /** The host it runs on, when it is not this daemon's machine (`ssh:me@box`). */
  host?: string;
  size: { cols: number; rows: number };
  alive: boolean;
  command: string;
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

/**
 * `GET /v1/terminals` — every pane the daemon owns.
 *
 * Unfiltered on purpose. A pane may have no session at all (a shell in a remote
 * workspace, or an agent's shell whose session could not be mapped), and asking
 * for one session's panes hides a process that is still running.
 */
export function terminals(token: string, signal?: AbortSignal): Promise<TerminalSummary[]> {
  return get<TerminalSummary[]>('/v1/terminals', token, signal);
}

/** `GET /v1/agents` — the profile's agents and whether this host can run them. */
export function agents(token: string, signal?: AbortSignal): Promise<AgentSummary[]> {
  return get<AgentSummary[]>('/v1/agents', token, signal);
}

async function send<T>(path: string, body: unknown, token: string): Promise<T> {
  const response = await fetch(path, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${token}`,
      Accept: 'application/json',
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(body),
  });
  if (response.status === 401) {
    throw new ApiError(401, 'That token was not accepted. Copy it from the daemon log or token file.');
  }
  if (!response.ok) {
    const text = await response.text().catch(() => '');
    throw new ApiError(
      response.status,
      text.trim() || `The daemon answered ${response.status} ${response.statusText}.`,
    );
  }
  return (await response.json()) as T;
}

/**
 * `GET /v1/permissions?status=pending&session=` — the questions still waiting.
 *
 * Pending requests are state, not history: the stream carries new ones while the
 * page is connected, and this is how a page that just opened learns the ones it
 * was not there for.
 */
export function pendingPermissions(
  token: string,
  session: string,
  signal?: AbortSignal,
): Promise<PermissionRequest[]> {
  return get<PermissionRequest[]>(
    `/v1/permissions?status=pending&session=${encodeURIComponent(session)}`,
    token,
    signal,
  );
}

/**
 * `POST /v1/permissions/{id}/resolve` — answer one question.
 *
 * Retry-safe by the daemon's own rule: the same option twice returns the same
 * decision, a different option after one was applied is `409` because the agent
 * has already acted. `deny` is an answer, not a dismissal.
 */
export function resolvePermission(
  id: string,
  option: PermissionOption,
  token: string,
): Promise<PermissionDecision> {
  return send<PermissionDecision>(
    `/v1/permissions/${encodeURIComponent(id)}/resolve`,
    { option },
    token,
  );
}

/**
 * `POST /v1/sessions` — start one.
 *
 * `workspace_id` is sent empty on purpose: the daemon refuses a workspace it does
 * not serve and substitutes its own when the field is empty, and the page has no
 * second root to offer. `title` is left out — the first prompt supplies one.
 */
export function createSession(
  agent: string,
  token: string,
): Promise<SessionSummary> {
  return send<SessionSummary>(
    '/v1/sessions',
    { workspace_id: '', agent_id: agent },
    token,
  );
}

/**
 * `POST /v1/sessions/{id}/prompt` — start a turn.
 *
 * The answer is an acknowledgement; the turn streams on the session's own stream.
 * The idempotency key is generated per press so a retried request is the same
 * prompt rather than a second agent run.
 */
export function promptSession(id: string, text: string, token: string): Promise<PromptAccepted> {
  return send<PromptAccepted>(
    `/v1/sessions/${encodeURIComponent(id)}/prompt`,
    { text, idempotency_key: newIdempotencyKey() },
    token,
  );
}

/** A key for one prompt press, used again if the request is retried. */
function newIdempotencyKey(): string {
  const source = globalThis.crypto;
  if (source && typeof source.randomUUID === 'function') {
    return source.randomUUID();
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

/**
 * `POST /v1/sessions/{id}/cancel` — ask the agent to stop the turn in flight.
 *
 * Cancelling an idle session is `202` with `cancelled: false` rather than an
 * error: the caller's intent is already true.
 */
export function cancelSession(id: string, token: string): Promise<CancelAccepted> {
  return send<CancelAccepted>(`/v1/sessions/${encodeURIComponent(id)}/cancel`, {}, token);
}
