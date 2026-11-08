// The daemon API, as much of it as the shell uses.
//
// The calls go through `openapi-fetch` against `api/schema.d.ts`, which
// `npm run schema` generates from `api/openapi.json` — the document the daemon
// serves, committed. A renamed field or a missing route is a type error here
// rather than a browser that draws nothing.
//
// Two routes are not in that document and keep hand-written shapes, said out loud
// so they are not mistaken for generated ones: `/v1/health`, the liveness answer
// the contract never described, and `/app/stats`, which is a rendering of the
// contract rather than part of it.

import createClient from 'openapi-fetch';

import type { components, paths } from './api/schema';

/** What `GET /v1/health` answers. Not in the OpenAPI document: see above. */
export interface Health {
  ok: boolean;
  service: string;
  version: string;
  api: string;
  /** The one workspace this daemon serves, when it was started against one. */
  workspace: string | null;
  /** The profile directory's name, when the daemon was started with one. */
  profile?: string | null;
}

/** Lifecycle state, as `GET /v1/sessions` spells it. */
export type SessionStatus = components['schemas']['SessionStatus'];

/** One row of `GET /v1/sessions`. */
export type SessionSummary = components['schemas']['SessionSummary'];

/** Availability, as `GET /v1/agents` spells it. */
export type AgentStatus = components['schemas']['AgentStatus'];

/** One row of `GET /v1/agents`: an agent this profile configured. */
export type AgentSummary = components['schemas']['AgentSummary'];

/** The closed set of things that can happen on a session's stream. */
export type SessionEventKind = components['schemas']['SessionEventKind'];

/**
 * One frame of `GET /v1/sessions/{id}/events`.
 *
 * `payload` is the agent's own shape and is deliberately not modelled: every
 * adapter puts different fields there, and a page that refused the ones it did
 * not know would hide a turn instead of drawing what it could.
 */
export type SessionEvent = components['schemas']['SessionEvent'];

/** What a caller may answer a permission request with. */
export type PermissionOption = components['schemas']['PermissionOption'];

/** One pending request, as `GET /v1/permissions?status=pending` lists it. */
export type PermissionRequest = components['schemas']['PermissionRequest'];

/** The decision the daemon applied, with the name of who made it. */
export type PermissionDecision = components['schemas']['PermissionDecision'];

/** `POST /v1/sessions/{id}/cancel`'s answer. */
export type CancelAccepted = components['schemas']['CancelAccepted'];

/** `POST /v1/sessions/{id}/prompt`'s answer: the turn was accepted, not finished. */
export type PromptAccepted = components['schemas']['PromptAccepted'];

/** One row of `GET /v1/terminals`: a pane a client may attach to. */
export type TerminalSummary = components['schemas']['TerminalSummary'];

/** One file, as `GET /v1/files` and `PUT /v1/files` answer it. */
export type FileContent = components['schemas']['FileContent'];

/** One row of `GET /v1/files/list`: a file or a directory. */
export type FileEntry = components['schemas']['FileEntry'];

/** What happened to one line since the last commit, as a gutter draws it. */
export type LineChange = components['schemas']['LineChange'];

/** One marked line: 1-based line number and what changed on it. */
export type LineMark = components['schemas']['LineMark'];

/** One row of `GET /v1/workspaces`: a directory this daemon may serve. */
export type WorkspaceSummary = components['schemas']['WorkspaceSummary'];

/** What happened to a path on one side of the index. */
export type GitChangeKind = components['schemas']['GitChangeKind'];

/** One changed path, with the index's answer and the working tree's. */
export type GitStatusEntry = components['schemas']['GitStatusEntry'];

/** Branch state and every changed path. */
export type GitStatus = components['schemas']['GitStatus'];

/** A unified diff and the files it touches. */
export type GitDiff = components['schemas']['GitDiff'];

/** Which two trees a diff compares. */
export type GitDiffScope = 'all' | 'unstaged' | 'staged';

/** One commit, as `POST /v1/git/commit` answers it. */
export type GitCommit = components['schemas']['GitCommit'];

/** Paths to move across the index; each is workspace-relative. */
export type GitIndexRequest = components['schemas']['GitIndexRequest'];

/** A generated message the person can edit before committing. */
export type CommitDraft = components['schemas']['CommitDraft'];

/** One profile, as `GET /v1/profiles` will list it (P4-8). */
export type ProfileSummary = components['schemas']['ProfileSummary'];

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

/**
 * The profile the page's address names.
 *
 * Kept beside the token because it is the same kind of fact — a request needs it,
 * and no response carries it. `setProfile` is called from the page's route change,
 * so a request cannot be sent for a profile the address does not say. The daemon
 * resolves it to one of the profiles it serves; `activate` is what makes the
 * streams, which carry no header, agree with the next address.
 */
let profile = '';

export function setProfile(next: string): void {
  profile = next;
}

/** The profile header alone, for the streams that assemble their own requests. */
export function profileHeader(): Record<string, string> {
  return profile ? { 'X-Acpilot-Profile': profile } : {};
}

/** The headers every typed call carries. The token travels here, never in a URL. */
function auth(token: string): Record<string, string> {
  return { ...profileHeader(), Authorization: `Bearer ${token}`, Accept: 'application/json' };
}

/** The typed client, on paths generated from the document the daemon serves. */
const client = createClient<paths>({
  // Absolute, because the client builds a `Request` before it calls fetch and a
  // browser refuses `new Request('/v1/sessions')` — a relative base would work in
  // the test DOM and throw in the page. The page is served by the daemon it talks
  // to, so its own origin is the right one.
  baseUrl: globalThis.location?.origin ?? '',
  // Resolved per call, not captured at import: a test that stubs `globalThis.fetch`
  // after importing this module must get its stub rather than the network, and a
  // page that installs a proxy fetch later should get that one.
  fetch: (request) => globalThis.fetch(request),
});

client.use({
  async onResponse({ response }) {
    if (response.ok) {
      return response;
    }
    if (response.status === 401) {
      throw new ApiError(
        401,
        'That token was not accepted. Copy it from the daemon log or token file.',
      );
    }
    // The daemon answers errors as JSON when it can; a proxy in front of it may
    // answer HTML instead, which is why the body is not parsed as a contract.
    // Cloned because the client has already read the body it parsed.
    const body = await response.clone().text().catch(() => '');
    throw new ApiError(
      response.status,
      body.trim() || `The daemon answered ${response.status} ${response.statusText}.`,
    );
  },
});

/**
 * The body of a successful answer.
 *
 * A 2xx with no body is a daemon this page cannot use; saying so once, here, keeps
 * every call below from carrying a branch it never takes.
 */
function answered<T>(result: { data?: T; response: Response }): T {
  if (result.data === undefined) {
    throw new ApiError(result.response.status, 'The daemon answered without a body.');
  }
  return result.data;
}

/** A `GET` for the two routes the document does not describe. */
async function get<T>(path: string, token: string, signal?: AbortSignal): Promise<T> {
  const response = await fetch(path, {
    headers: auth(token),
    signal,
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
  return (await response.json()) as T;
}

export function health(token: string, signal?: AbortSignal): Promise<Health> {
  return get<Health>('/v1/health', token, signal);
}

export async function sessions(
  token: string,
  signal?: AbortSignal,
): Promise<SessionSummary[]> {
  return answered(await client.GET('/v1/sessions', { headers: auth(token), signal }));
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
export async function terminals(
  token: string,
  signal?: AbortSignal,
): Promise<TerminalSummary[]> {
  return answered(await client.GET('/v1/terminals', { headers: auth(token), signal }));
}

/**
 * `GET /v1/workspaces` — the directories this daemon may serve.
 *
 * Off the document, like `/v1/health`: the route exists but the crate that serves
 * it registers no `utoipa` path, so the generated client cannot know it. The
 * daemon always names its own workspace in health, so a failure here costs the
 * picker, not the page: the address still says which workspace it is on.
 */
export function workspaces(token: string, signal?: AbortSignal): Promise<WorkspaceSummary[]> {
  return get<WorkspaceSummary[]>('/v1/workspaces', token, signal);
}

/** `GET /v1/profiles` — every profile this daemon serves, the active one marked. */
export async function profiles(token: string, signal?: AbortSignal): Promise<ProfileSummary[]> {
  return answered(await client.GET('/v1/profiles', { headers: auth(token), signal }));
}

/**
 * `POST /v1/profiles/{id}/activate` — the profile headerless reads are about.
 *
 * The page calls this before it moves the address, because the streams bind at
 * connect and cannot carry the header: the palette is the active profile's until
 * it reconnects, so activating first is what makes the new address and the new
 * colours agree.
 */
export async function activateProfile(id: string, token: string): Promise<ProfileSummary> {
  return answered(
    await client.POST('/v1/profiles/{id}/activate', {
      params: { path: { id } },
      // The header names the profile the request is about, not the page's current
      // one. The route lives on the daemon's profiles router rather than a
      // profile's, so it reads the path; and the address moves only after the
      // daemon agrees, so a click on the first frame — before health has been
      // adopted — would otherwise carry no profile header at all.
      headers: { ...auth(token), 'X-Acpilot-Profile': id },
    }),
  );
}

/** `GET /v1/sessions/{id}` — one session, for an address that names one. */
export async function session(
  id: string,
  token: string,
  signal?: AbortSignal,
): Promise<SessionSummary> {
  return answered(
    await client.GET('/v1/sessions/{id}', {
      params: { path: { id } },
      headers: auth(token),
      signal,
    }),
  );
}

/** `GET /v1/agents` — the profile's agents and whether this host can run them. */
export async function agents(token: string, signal?: AbortSignal): Promise<AgentSummary[]> {
  return answered(await client.GET('/v1/agents', { headers: auth(token), signal }));
}

/**
 * `GET /v1/permissions?status=pending&session=` — the questions still waiting.
 *
 * Pending requests are state, not history: the stream carries new ones while the
 * page is connected, and this is how a page that just opened learns the ones it
 * was not there for.
 */
export async function pendingPermissions(
  token: string,
  session: string,
  signal?: AbortSignal,
): Promise<PermissionRequest[]> {
  return answered(
    await client.GET('/v1/permissions', {
      params: { query: { status: 'pending', session } },
      headers: auth(token),
      signal,
    }),
  );
}

/**
 * `POST /v1/permissions/{id}/resolve` — answer one question.
 *
 * Retry-safe by the daemon's own rule: the same option twice returns the same
 * decision, a different option after one was applied is `409` because the agent
 * has already acted. `deny` is an answer, not a dismissal.
 */
export async function resolvePermission(
  id: string,
  option: PermissionOption,
  token: string,
): Promise<PermissionDecision> {
  return answered(
    await client.POST('/v1/permissions/{id}/resolve', {
      params: { path: { id } },
      body: { option },
      headers: auth(token),
    }),
  );
}

/**
 * `POST /v1/sessions` — start one.
 *
 * `workspace_id` is sent empty on purpose: the daemon refuses a workspace it does
 * not serve and substitutes its own when the field is empty, and the page has no
 * second root to offer. `title` is left out — the first prompt supplies one.
 */
export async function createSession(agent: string, token: string): Promise<SessionSummary> {
  return answered(
    await client.POST('/v1/sessions', {
      body: { workspace_id: '', agent_id: agent },
      headers: auth(token),
    }),
  );
}

/**
 * `POST /v1/sessions/{id}/prompt` — start a turn.
 *
 * The answer is an acknowledgement; the turn streams on the session's own stream.
 * The idempotency key is generated per press so a retried request is the same
 * prompt rather than a second agent run.
 */
export async function promptSession(
  id: string,
  text: string,
  token: string,
): Promise<PromptAccepted> {
  return answered(
    await client.POST('/v1/sessions/{id}/prompt', {
      params: { path: { id } },
      body: { text, idempotency_key: newIdempotencyKey() },
      headers: auth(token),
    }),
  );
}

/**
 * `GET /v1/files?path=` — read one UTF-8 file.
 *
 * A directory is a `409` naming the list route rather than an empty text: the
 * page has a browser, and drawing a directory as a zero-byte file would hide
 * every name inside it.
 */
export async function readFile(
  path: string,
  token: string,
  signal?: AbortSignal,
): Promise<FileContent> {
  return answered(
    await client.GET('/v1/files', {
      params: { query: { path } },
      headers: auth(token),
      signal,
    }),
  );
}

/** `GET /v1/files/list?path=` — one directory's rows, workspace-relative. */
export async function listDirectory(
  path: string,
  token: string,
  signal?: AbortSignal,
): Promise<FileEntry[]> {
  return answered(
    await client.GET('/v1/files/list', {
      params: { query: { path } },
      headers: auth(token),
      signal,
    }),
  );
}

/**
 * `PUT /v1/files` — write a buffer back.
 *
 * `ifMatch` is the `etag` the buffer was read at, and it is the whole concurrency
 * story: the daemon answers `409` rather than overwriting text somebody else
 * wrote while this buffer was open. There is no "force" here on purpose — a
 * forced write is the silent overwrite the route exists to prevent.
 */
export async function writeFile(
  path: string,
  text: string,
  ifMatch: string,
  token: string,
): Promise<FileContent> {
  return answered(
    await client.PUT('/v1/files', {
      body: { path, text, if_match: ifMatch },
      headers: auth(token),
    }),
  );
}

/**
 * `PUT /v1/files` without `if_match` — create a file that is not there yet.
 *
 * The daemon reads the absent token as "this path is new" and answers `409` when
 * something already exists there, so a create cannot become the silent replace
 * the route refuses. The parent directory has to exist: a save into a directory
 * that is not there is reported rather than built.
 */
export async function createFile(
  path: string,
  text: string,
  token: string,
): Promise<FileContent> {
  return answered(
    await client.PUT('/v1/files', {
      body: { path, text },
      headers: auth(token),
    }),
  );
}

/**
 * `DELETE /v1/files?path=` — remove a file.
 *
 * A directory is `409` rather than a recursive delete: the daemon will not guess
 * what should happen to everything inside it. A renamed or deleted path keeps
 * nothing of the buffer, so the caller drops its tab.
 */
export async function deleteFile(path: string, token: string): Promise<void> {
  await client.DELETE('/v1/files', {
    params: { query: { path } },
    headers: auth(token),
  });
}

/**
 * `POST /v1/files/rename` — move a file and answer with its new version.
 *
 * The answer is the moved file's `FileContent`, so the tab can follow the path
 * without a second read. The daemon refuses an existing destination; a rename is
 * never a replace. On a remote workspace this is `409`: the host helper has no
 * rename operation, so write and delete work there and a move does not.
 */
export async function renameFile(
  path: string,
  to: string,
  token: string,
): Promise<FileContent> {
  return answered(
    await client.POST('/v1/files/rename', {
      body: { path, to },
      headers: auth(token),
    }),
  );
}

/** `GET /v1/git/status` — branch state and every changed path. */
export async function gitStatus(
  token: string,
  signal?: AbortSignal,
): Promise<GitStatus> {
  return answered(await client.GET('/v1/git/status', { headers: auth(token), signal }));
}

/**
 * `GET /v1/git/diff?scope=` — the patch and its file list, from one moment.
 *
 * `unstaged` is what can still be added to the index; `staged` is what the next
 * commit already holds. The browser asks for the two halves separately because a
 * hunk is numbered inside the unstaged diff, and a combined patch numbers hunks
 * differently.
 */
export async function gitDiff(
  scope: GitDiffScope,
  token: string,
  signal?: AbortSignal,
): Promise<GitDiff> {
  return answered(
    await client.GET('/v1/git/diff', {
      params: { query: { scope } },
      headers: auth(token),
      signal,
    }),
  );
}

/**
 * `POST /v1/git/commit-message` — a session drafts from the staged index.
 *
 * A draft is not a commit: the daemon stages and writes nothing, and the answer is
 * text the page puts in an editable box.
 */
export async function commitMessage(sessionId: string, token: string): Promise<CommitDraft> {
  return answered(
    await client.POST('/v1/git/commit-message', {
      body: { session_id: sessionId },
      headers: auth(token),
    }),
  );
}

/**
 * `POST /v1/git/stage` — put paths in the index and answer the new status.
 *
 * The pane stages the selected path from here, so preparing a commit no longer
 * sends the person to a terminal for `git add`. The paths that remain unstaged
 * are still in the working tree, which is why the answer is a status rather than
 * a commit.
 */
export async function stagePaths(paths: string[], token: string): Promise<GitStatus> {
  return answered(
    await client.POST('/v1/git/stage', {
      body: { paths },
      headers: auth(token),
    }),
  );
}

/**
 * `POST /v1/git/stage-hunk` — put one unstaged hunk in the index.
 *
 * `hunk` is zero-based inside the path's current unstaged diff. The daemon cuts
 * the hunk from git's own patch, so the page never sends patch text of its own.
 */
export async function stageHunk(
  path: string,
  hunk: number,
  token: string,
): Promise<GitStatus> {
  return answered(
    await client.POST('/v1/git/stage-hunk', {
      body: { path, hunk },
      headers: auth(token),
    }),
  );
}

/**
 * `POST /v1/git/unstage` — take paths out of the index and answer the new status.
 *
 * This is the opposite of stage, not a discard: the working-tree change stays.
 */
export async function unstagePaths(paths: string[], token: string): Promise<GitStatus> {
  return answered(
    await client.POST('/v1/git/unstage', {
      body: { paths },
      headers: auth(token),
    }),
  );
}

/**
 * `POST /v1/git/discard` — put the working tree back to the index.
 *
 * A staged edit stays staged. An untracked path is removed, because the index
 * has no version to restore it from.
 */
export async function discardPaths(paths: string[], token: string): Promise<GitStatus> {
  return answered(
    await client.POST('/v1/git/discard', {
      body: { paths },
      headers: auth(token),
    }),
  );
}

/**
 * `POST /v1/git/checkout` — restore both halves of a path from HEAD.
 *
 * This is the stronger reset: a staged addition is removed, and an untracked path
 * is refused because HEAD has no version to check out.
 */
export async function checkoutPaths(paths: string[], token: string): Promise<GitStatus> {
  return answered(
    await client.POST('/v1/git/checkout', {
      body: { paths },
      headers: auth(token),
    }),
  );
}

/**
 * `POST /v1/git/commit` — commit the index the pane prepared.
 *
 * The caller has confirmed the whole message. Staging happens in this pane first;
 * git's index is what the route commits.
 */
export async function commitStaged(message: string, token: string): Promise<GitCommit> {
  return answered(
    await client.POST('/v1/git/commit', {
      body: { message },
      headers: auth(token),
    }),
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
export async function cancelSession(id: string, token: string): Promise<CancelAccepted> {
  return answered(
    await client.POST('/v1/sessions/{id}/cancel', {
      params: { path: { id } },
      headers: auth(token),
    }),
  );
}
