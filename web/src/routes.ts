// The page's addresses.
//
// A browser tab is a place, and a place should survive a refresh: these are the
// two shapes the field manual publishes, parsed and built in one module so the URL
// a link carries and the URL the page writes cannot disagree.
//
// The profile is in the address because it is part of what a request means —
// `X-Acpilot-Profile` is how every mutation names it — not because this page can
// switch one. A daemon serves one profile today; the segment is what makes the
// address complete, and the switcher appears when `GET /v1/profiles` does.

import type { SessionSummary, WorkspaceSummary } from './api';

/** One page address: which profile, and the workspace or session it names. */
export interface Route {
  profile: string;
  workspace?: string;
  session?: string;
}

/**
 * The profile a daemon that was started without `--profile` is addressed by.
 *
 * `GET /v1/health` answers with the profile's directory name when there is one,
 * and the page keeps this name only for that case. It is a label rather than an
 * identity: no directory called `default` has to exist.
 */
export const DEFAULT_PROFILE = 'default';

function decode(part: string): string {
  try {
    return decodeURIComponent(part);
  } catch {
    // A percent sign that is not an escape is still a name; refusing it would
    // turn a pasted address into a blank page.
    return part;
  }
}

/**
 * The route a pathname names, or `null` for anything that is not one.
 *
 * `/app/p/...` is accepted as well as `/p/...` because the page has been served
 * at `/app` since before it had routes, and an address copied from that era
 * should land somewhere rather than 404.
 */
export function parseRoute(pathname: string): Route | null {
  const path = pathname.startsWith('/app/') ? pathname.slice('/app'.length) : pathname;
  const parts = path.split('/').filter(Boolean);
  if (parts.length !== 4 || parts[0] !== 'p') {
    return null;
  }
  const [, profile, kind, value] = parts;
  if (!profile || !value) {
    return null;
  }
  const route: Route = { profile: decode(profile) };
  if (kind === 'w') {
    return { ...route, workspace: decode(value) };
  }
  if (kind === 's') {
    return { ...route, session: decode(value) };
  }
  return null;
}

/** The pathname a route is written as. A session address does not carry its workspace. */
export function routePath(route: Route): string {
  const profile = encodeURIComponent(route.profile);
  if (route.session) {
    return `/p/${profile}/s/${encodeURIComponent(route.session)}`;
  }
  if (route.workspace) {
    return `/p/${profile}/w/${encodeURIComponent(route.workspace)}`;
  }
  return `/p/${profile}`;
}

/**
 * The sessions a workspace address is about.
 *
 * The daemon serves one workspace, but a session names the workspace it was
 * started in, and an address that says `build` while the table shows `acpilot` is
 * an address that lied. A workspace may be named by its id or by its label in the
 * URL — both appear in the wild — so both match.
 */
export function scopeSessions(
  sessions: SessionSummary[],
  workspace: string | undefined,
  workspaces: WorkspaceSummary[],
): SessionSummary[] {
  if (!workspace) {
    return sessions;
  }
  const known = workspaces.find((row) => row.id === workspace || row.name === workspace);
  const names = new Set(
    [workspace, known?.id, known?.name].filter((name): name is string => Boolean(name)),
  );
  return sessions.filter((session) => names.has(session.workspace_id));
}
