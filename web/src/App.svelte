<script lang="ts">
  import { onMount } from 'svelte';
  import {
    ApiError,
    agents as listAgents,
    createSession,
    health,
    readToken,
    sessions as listSessions,
    stats as readStats,
    terminals as listTerminals,
    writeToken,
    profiles as listProfiles,
    session as readSession,
    setProfile,
    workspaces as listWorkspaces,
    type AgentSummary,
    type Health,
    type ProfileSummary,
    type SessionSummary,
    type StatRow,
    type TerminalSummary,
    type Track,
    type WorkspaceSummary,
  } from './api';
  import { DEFAULT_PROFILE, parseRoute, routePath, scopeSessions, type Route } from './routes';
  import Editor from './Editor.svelte';
  import Session from './Session.svelte';
  import Terminal from './Terminal.svelte';
  import { paneWhere, panesFor } from './terminals';

  // The build the daemon injected into the page before anyone had a token. Its
  // health answer is preferred once there is one, because that is the process
  // that will actually serve the requests.
  const buildVersion =
    document.querySelector<HTMLMetaElement>('meta[name="acpilot-version"]')?.content ?? 'unknown';

  // The address the page was opened at, if it named one. A link with a profile in
  // it is a decision the page keeps; a bare `/` is placed on the daemon's own
  // profile and workspace after health answers.
  const initialRoute = parseRoute(location.pathname);
  let route = $state<Route>(initialRoute ?? { profile: DEFAULT_PROFILE });

  let token = $state(readToken());
  let draft = $state('');
  let loading = $state(false);
  let error = $state<string | null>(null);
  let healthInfo = $state<Health | null>(null);
  let allRows = $state<SessionSummary[]>([]);
  // The rows the address is about: a workspace address scopes the table, a
  // session address shows the workspace it belongs to (no scope) so the list it
  // came from is still there. A function rather than an inline expression because
  // `workspaceList` is declared below: the derivation runs after the component is
  // set up, and a direct reference would read like a use before declaration.
  const rows = $derived(scopeRows(allRows));

  function scopeRows(list: SessionSummary[]): SessionSummary[] {
    return scopeSessions(list, route.workspace, workspaceList);
  }
  let integrations = $state<StatRow[]>([]);
  // Why the bar is empty when the daemon has nothing to say about it: a bar that
  // could not be read is not a roster of none, and an older daemon that never
  // heard of `/app/stats` must not take the whole page down with it.
  let statsNote = $state<string | null>(null);
  // The panes this daemon owns, listed unfiltered. A pane with no session is still
  // a process: a shell in another workspace on another host, or an agent's shell
  // whose session the daemon could not map. Hiding it is how it becomes a process
  // nobody can find.
  let panes = $state<TerminalSummary[]>([]);
  let panesNote = $state<string | null>(null);
  // The pane being drawn, when one is. The process belongs to the daemon: closing
  // this view detaches, and the program keeps running.
  let openPane = $state<TerminalSummary | null>(null);
  // The session whose transcript is open is the one the address names. It also
  // narrows the terminal list: `panesFor` keeps that session's panes *plus* every
  // pane nobody owns, and never drops a pane with no `session_id`.
  const openSession = $derived(route.session ?? null);
  // The agents this profile configured, and the one the form will start. An
  // empty list is a daemon with no `agents.toml`: there is nothing to start, and
  // the form says that instead of offering an empty picker.
  let agentList = $state<AgentSummary[]>([]);
  let agentsNote = $state<string | null>(null);
  // The profiles `GET /v1/profiles` lists, empty on a daemon that serves one
  // profile and has no profiles API (P4-8). The address's profile is still
  // shown, because it is what every request names.
  let profileList = $state<ProfileSummary[]>([]);
  let workspaceList = $state<WorkspaceSummary[]>([]);
  // A session an address names that the list does not have: asked for by id, so a
  // shared link opens the conversation instead of an empty transcript.
  let directSession = $state<SessionSummary | null>(null);
  let routeError = $state<string | null>(null);
  let chosenAgent = $state('');
  let creating = $state(false);
  let createError = $state<string | null>(null);
  // Provenance is one click away, the same key (F4) the terminal uses: a bar has no
  // room for where a number came from, and a bar that never says is a claim.
  let sources = $state(false);
  let aboutOpen = $state(false);
  let quitNote = $state(false);
  let dialog = $state<HTMLDialogElement | null>(null);

  const version = $derived(healthInfo?.version ?? buildVersion);
  const connected = $derived(healthInfo !== null);
  const viewSession = $derived(
    route.session
      ? (rows.find((row) => row.id === route.session) ??
        (directSession?.id === route.session ? directSession : null))
      : null,
  );
  const activeWorkspace = $derived(
    workspaceList.find((row) => row.name === route.workspace || row.id === route.workspace) ??
      null,
  );

  let controller: AbortController | null = null;

  // The daemon pushes the web theme's colours, so saving a theme anywhere — the
  // TUI, the desktop app, another tab — repaints this page instead of waiting for
  // a reload. The stream is open because an `EventSource` cannot set a header; it
  // carries the palette the page was already served with and nothing else.
  let paletteSource: EventSource | null = null;

  interface PaletteFrame {
    color_scheme?: string;
    vars?: Record<string, string>;
  }

  function applyPalette(event: MessageEvent<string>): void {
    let frame: PaletteFrame;
    try {
      frame = JSON.parse(event.data) as PaletteFrame;
    } catch {
      // A frame we cannot read is a frame we ignore: the page keeps the colours
      // it has, which are the ones its own stylesheet names.
      return;
    }
    const root = document.documentElement;
    if (frame.color_scheme) {
      root.style.colorScheme = frame.color_scheme;
    }
    for (const [name, value] of Object.entries(frame.vars ?? {})) {
      root.style.setProperty(name, value);
    }
  }

  async function load(): Promise<void> {
    if (!token) {
      return;
    }
    setProfile(route.profile);
    controller?.abort();
    const current = new AbortController();
    controller = current;
    loading = true;
    error = null;
    try {
      const [status, sessions, bar, terminals, agents, profiles, workspaces] = await Promise.all([
        health(token, current.signal),
        listSessions(token, current.signal),
        // The bar is the one part of the page a daemon older than this build may
        // not serve, so its absence is reported beside the bar rather than as a
        // failure of the page.
        readStats(token, current.signal).catch(() => null),
        // Unfiltered, for the same reason: the picker keeps the open session's panes
        // and every pane nobody owns, so a shell in another workspace is not hidden
        // by a list that asked only for one session's.
        listTerminals(token, current.signal).catch(() => null),
        // As with the bar and the panes: an older daemon that does not answer
        // for its agents costs the page the form, not the sessions.
        listAgents(token, current.signal).catch(() => null),
        // The profiles API is P4-8 and not in the frozen contract; a daemon that
        // does not serve it answers 404, which costs the switcher its list, not
        // the page.
        listProfiles(token, current.signal).catch(() => null),
        // The workspaces the picker offers. The daemon's own is in health even
        // when this route is absent, so the address still has a workspace.
        listWorkspaces(token, current.signal).catch(() => null),
      ]);
      healthInfo = status;
      allRows = sessions;
      profileList = profiles ?? [];
      workspaceList = workspaces ?? [];
      // A bare address takes the daemon's profile; a link with one in it keeps it.
      // The URL is the state, so this is a `replaceState`: no extra history entry
      // for a page that has not navigated anywhere.
      if (!initialRoute && status.profile && route.profile !== status.profile) {
        route = { ...route, profile: status.profile };
        setProfile(route.profile);
      }
      if (!route.workspace && !route.session) {
        const served = status.workspace ?? workspaceList[0]?.name;
        if (served) {
          navigate({ profile: route.profile, workspace: served }, true);
        }
      }
      integrations = bar ?? [];
      panes = terminals ? panesFor(terminals, openSession) : [];
      statsNote = bar
        ? null
        : 'This daemon did not report its integrations; the bar needs a build that serves /app/stats.';
      panesNote = terminals
        ? null
        : 'This daemon did not list its terminals; the terminal needs a build that serves /v1/terminals.';
      agentList = agents ?? [];
      agentsNote = agents
        ? null
        : 'This daemon did not list its agents; a session cannot be started from here.';
      // Keep the picker on an agent that still exists, defaulting to the first one
      // this host can actually run.
      const runnable = agentList.filter((agent) => agent.status === 'available');
      if (!agentList.some((agent) => agent.id === chosenAgent)) {
        chosenAgent = (runnable[0] ?? agentList[0])?.id ?? '';
      }
      // A pane that ended while this view was open is a row nothing can attach to.
      if (openPane && !panes.some((pane) => pane.id === openPane?.id)) {
        openPane = null;
      }
      // An address that names a session the list does not have is one this
      // process has not listed yet; ask for it by id rather than leaving a real
      // address over an empty transcript. A session that is gone sends the page
      // back to the workspace it came from, with the reason.
      routeError = null;
      if (route.session && !rows.some((row) => row.id === route.session)) {
        try {
          directSession = await readSession(route.session, token, current.signal);
        } catch (cause) {
          if (current.signal.aborted) {
            return;
          }
          directSession = null;
          routeError = `No session ${route.session} on this daemon.`;
          const served = status.workspace ?? workspaceList[0]?.name;
          navigate({ profile: route.profile, ...(served ? { workspace: served } : {}) }, true);
        }
      } else {
        directSession = null;
      }
    } catch (cause) {
      if (current.signal.aborted) {
        return;
      }
      healthInfo = null;
      allRows = [];
      profileList = [];
      workspaceList = [];
      directSession = null;
      routeError = null;
      integrations = [];
      statsNote = null;
      panes = [];
      panesNote = null;
      agentList = [];
      agentsNote = null;
      chosenAgent = '';
      openPane = null;
      if (cause instanceof ApiError) {
        error = cause.message;
        // A rejected token is not going to start working; forget it so the form
        // asks instead of retrying a secret the daemon already said no to.
        if (cause.status === 401) {
          token = '';
          writeToken('');
        }
      } else {
        error = cause instanceof Error ? cause.message : String(cause);
      }
    } finally {
      if (controller === current) {
        loading = false;
        controller = null;
      }
    }
  }

  function connect(event: SubmitEvent): void {
    event.preventDefault();
    const candidate = draft.trim();
    if (!candidate) {
      return;
    }
    token = candidate;
    writeToken(candidate);
    draft = '';
    void load();
  }

  function disconnect(): void {
    controller?.abort();
    controller = null;
    token = '';
    draft = '';
    healthInfo = null;
    allRows = [];
    profileList = [];
    workspaceList = [];
    directSession = null;
    routeError = null;
    integrations = [];
    statsNote = null;
    panes = [];
    panesNote = null;
    agentList = [];
    agentsNote = null;
    chosenAgent = '';
    createError = null;
    openPane = null;
    error = null;
    loading = false;
    writeToken('');
  }

  /**
   * Start a session and open it.
   *
   * The daemon attaches the agent before it answers, so a failure here is the
   * agent saying no, and the message is shown where the button was pressed
   * rather than as a page-wide error.
   */
  async function startSession(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    if (!chosenAgent || creating) {
      return;
    }
    creating = true;
    createError = null;
    try {
      const created = await createSession(chosenAgent, token);
      allRows = [created, ...allRows.filter((row) => row.id !== created.id)];
      directSession = null;
      navigate({ profile: route.profile, session: created.id });
    } catch (cause) {
      createError = cause instanceof ApiError ? cause.message : String(cause);
    } finally {
      creating = false;
    }
  }

  function quit(): void {
    window.close();
    // A tab nobody scripted open cannot be closed by script, and a button that
    // silently does nothing reads as broken. Say what to do instead.
    window.setTimeout(() => {
      quitNote = true;
    }, 250);
  }

  /** The tracks a row has, with the name each bar is announced by. */
  function bands(row: StatRow): { label: string; track: Track }[] {
    const found: { label: string; track: Track }[] = [];
    if (row.context) {
      found.push({ label: `${row.label} context`, track: row.context });
    }
    if (row.credits) {
      found.push({ label: `${row.label} credits`, track: row.credits });
    }
    return found;
  }

  /**
   * A track's width: the fill as a percentage, or none at all.
   *
   * `null` is "nobody reported the denominator", and the stylesheet draws that as a
   * dashed track — never as empty, which would read as "nothing used".
   */
  function width(track: Track): string {
    return track.fill === null ? '0' : `${Math.round(track.fill * 100)}%`;
  }

  function when(iso: string): string {
    const at = new Date(iso);
    return Number.isNaN(at.valueOf()) ? iso : at.toLocaleString();
  }

  /** Go to an address, and let the page follow it. */
  function navigate(next: Route, replace = false): void {
    const url = routePath(next);
    if (replace) {
      history.replaceState({}, '', url);
    } else {
      history.pushState({}, '', url);
    }
    route = next;
    setProfile(next.profile);
  }

  /** Open a session by putting it in the address, which is where the page reads it. */
  function open(id: string): void {
    navigate({ profile: route.profile, session: id });
  }

  /** Back to the workspace, keeping the profile the address names. */
  function closeSession(): void {
    const served = activeWorkspace?.name ?? workspaceList[0]?.name ?? route.workspace;
    navigate({ profile: route.profile, ...(served ? { workspace: served } : {}) });
  }

  function switchWorkspace(event: Event): void {
    const value = (event.currentTarget as HTMLSelectElement).value;
    navigate({ profile: route.profile, ...(value ? { workspace: value } : {}) });
  }

  function switchProfile(event: Event): void {
    const value = (event.currentTarget as HTMLSelectElement).value;
    if (!value || value === route.profile) {
      return;
    }
    // The header is how every request names a profile, so the change is a reload
    // rather than a filter over rows fetched for the old one.
    navigate({ profile: value, ...(route.workspace ? { workspace: route.workspace } : {}) });
    void load();
  }

  /** The back button: the address changed, so the page re-reads it. */
  function followed(): void {
    route =
      parseRoute(location.pathname) ?? { profile: route.profile, workspace: route.workspace };
    setProfile(route.profile);
    void load();
  }

  onMount(() => {
    window.addEventListener('popstate', followed);
    if (token) {
      void load();
    }
    paletteSource = new EventSource('/app/palette/events');
    paletteSource.addEventListener('palette', (event) => {
      applyPalette(event as MessageEvent<string>);
    });
    return () => {
      window.removeEventListener('popstate', followed);
      controller?.abort();
      paletteSource?.close();
      paletteSource = null;
    };
  });

  // The native dialog gives Escape, focus trapping and a backdrop for free, so
  // the state and the element are kept in step rather than hand-rolling an
  // overlay that would get one of the three wrong.
  $effect(() => {
    if (!dialog) {
      return;
    }
    if (aboutOpen && !dialog.open) {
      dialog.showModal();
    } else if (!aboutOpen && dialog.open) {
      dialog.close();
    }
  });
</script>

<div class="shell">
  <header class="topbar">
    <h1 class="brand">Acpilot</h1>
    <div class="scope">
      <label for="profile">Profile</label>
      <select
        id="profile"
        value={route.profile}
        onchange={switchProfile}
        disabled={profileList.length < 2}
        title={profileList.length < 2
          ? 'This daemon serves one profile. Switching needs the profiles API (P4-8).'
          : 'Requests name this profile in X-Acpilot-Profile.'}
      >
        {#each profileList as entry (entry.id)}
          <option value={entry.id}>{entry.label}</option>
        {/each}
        {#if !profileList.some((entry) => entry.id === route.profile)}
          <option value={route.profile}>{route.profile}</option>
        {/if}
      </select>
      <label for="workspace">Workspace</label>
      <select id="workspace" value={route.workspace ?? ''} onchange={switchWorkspace}>
        {#if !route.workspace}
          <option value="">every workspace</option>
        {/if}
        {#each workspaceList as entry (entry.id)}
          <option value={entry.name}>{entry.name}</option>
        {/each}
        {#if route.workspace && !workspaceList.some((entry) => entry.name === route.workspace)}
          <option value={route.workspace}>{route.workspace}</option>
        {/if}
      </select>
    </div>
    <span class="version">Version {version}</span>
  </header>

  <main>
    {#if connected}
      <h2>{healthInfo?.service ?? 'daemon'}</h2>
      <p class="tagline">
        Connected to the daemon at this address. Everything it will do for this
        browser needs the token you gave it, and nothing else is stored.
      </p>

      <dl class="facts">
        <div>
          <dt>API</dt>
          <dd>{healthInfo?.api}</dd>
        </div>
        <div>
          <dt>Workspace</dt>
          <dd>{healthInfo?.workspace ?? 'none'}</dd>
        </div>
        <div>
          <dt>Sessions</dt>
          <dd>{rows.length}</dd>
        </div>
      </dl>

      <section class="stats" aria-labelledby="stats-title">
        <div class="stats-head">
          <h3 id="stats-title">Integrations</h3>
          <button
            type="button"
            class="link"
            aria-pressed={sources}
            onclick={() => (sources = !sources)}
          >
            {sources ? 'Hide sources' : 'Show sources'}
          </button>
        </div>
        {#if integrations.length}
          <ul class="stats-rows">
            {#each integrations as row (row.id)}
              <li class="stats-row" data-kind={row.kind} data-stale={row.stale}>
                <span class="stats-name">
                  {row.kind === 'agent' ? '◆' : '◇'}
                  {row.label}
                  {#if row.stale}
                    <span class="old" title={row.updated_at}>(old)</span>
                  {/if}
                </span>
                {#if sources}
                  <span class="stats-source">
                    context {row.context?.source ?? '—'} · credits {row.credits?.source ?? '—'}
                  </span>
                {:else if !row.context && !row.credits}
                  <span class="stats-source">nothing reported</span>
                {:else}
                  {#each bands(row) as band (band.label)}
                    <span
                      class="track"
                      role="progressbar"
                      aria-label={band.label}
                      aria-valuemin={0}
                      aria-valuemax={100}
                      aria-valuenow={band.track.fill === null
                        ? undefined
                        : Math.round(band.track.fill * 100)}
                      aria-valuetext={band.track.fill === null
                        ? 'no denominator reported'
                        : undefined}
                      data-source={band.track.source}
                      data-indeterminate={band.track.fill === null}
                    >
                      <span class="track-fill" style:width={width(band.track)}></span>
                    </span>
                    <span class="track-text" data-source={band.track.source}>
                      {band.track.source === 'estimated' ? '~' : ''}{band.track.text}
                    </span>
                  {/each}
                {/if}
              </li>
            {/each}
          </ul>
        {:else}
          <p class="note">
            {statsNote ??
              'This daemon has no integrations to report. Give the profile an agents.toml and its agents appear here.'}
          </p>
        {/if}
      </section>

      <form class="new-session" onsubmit={(event) => void startSession(event)}>
        <label for="agent">Start a session</label>
        <div class="new-session-row">
          <select id="agent" bind:value={chosenAgent} disabled={creating || agentList.length === 0}>
            {#each agentList as agent (agent.id)}
              <option value={agent.id}>{agent.name} · {agent.status}</option>
            {/each}
          </select>
          <button type="submit" class="primary" disabled={creating || !chosenAgent}>
            {creating ? 'Starting…' : 'Start session'}
          </button>
        </div>
        {#if agentsNote}
          <p class="note">{agentsNote}</p>
        {:else if agentList.length === 0}
          <p class="note">
            This profile has no agents. Add an <code>agents.toml</code> and refresh.
          </p>
        {/if}
        {#if createError}
          <p class="error" role="alert">{createError}</p>
        {/if}
      </form>

      <div class="panels">
        <section class="panel" aria-labelledby="sessions-title">
          <h3 id="sessions-title">Sessions</h3>
          {#if routeError}
            <p class="error" role="alert">{routeError}</p>
          {/if}
          <div class="card">
            <table>
              <caption class="visually-hidden">Sessions this daemon knows about</caption>
          <thead>
            <tr>
              <th scope="col">Session</th>
              <th scope="col">Agent</th>
              <th scope="col">Status</th>
              <th scope="col">Updated</th>
            </tr>
          </thead>
          <tbody>
            {#each rows as row (row.id)}
              <tr>
                <td>
                  <button type="button" class="link" onclick={() => open(row.id)}>
                    Open
                  </button>
                  {row.title || row.id}
                </td>
                <td class="muted">{row.agent_id}</td>
                <td><span class="status" data-status={row.status}>{row.status}</span></td>
                <td class="muted">{when(row.updated_at)}</td>
              </tr>
            {:else}
              <tr>
                <td class="empty" colspan="4">
                  No sessions yet. Start one from the TUI or the desktop app.
                </td>
              </tr>
            {/each}
          </tbody>
            </table>
          </div>
        </section>

        <section class="panel" aria-labelledby="transcript-title">
          <h3 id="transcript-title">Transcript</h3>
          {#if viewSession}
            {#key viewSession.id}
              <Session session={viewSession} {token} onclose={closeSession} />
            {/key}
          {:else}
            <p class="note">
              Pick a session on the left, or open the one in the address bar. A
              session's turn is streamed here; the address keeps which one, so a
              refresh does not lose it.
            </p>
          {/if}
        </section>
      </div>

      <section class="editor-panel" aria-labelledby="editor-title">
        <h3 id="editor-title">Editor</h3>
        <Editor {token} />
      </section>

      <section class="terminals" aria-labelledby="terminals-title">
        <div class="stats-head">
          <h3 id="terminals-title">Terminals</h3>
          {#if panes.length}
            <span class="pane-count">
              {panes.length}
              {panes.length === 1 ? 'pane' : 'panes'}
            </span>
          {/if}
        </div>
        {#if openPane}
          <Terminal pane={openPane} {token} onclose={() => (openPane = null)} />
        {:else if panes.length}
          <ul class="pane-rows">
            {#each panes as pane (pane.id)}
              <li class="pane-row" data-session={pane.session_id ?? 'none'} data-alive={pane.alive}>
                <span class="pane-name"><code>{pane.command}</code></span>
                <span class="pane-where">{paneWhere(pane)}</span>
                <span class="status" data-status={pane.alive ? 'idle' : 'closed'}>
                  {pane.alive ? 'running' : 'exited'}
                </span>
                <button type="button" onclick={() => (openPane = pane)}>Attach</button>
              </li>
            {/each}
          </ul>
          <p class="note pane-note">
            A pane with <strong>no session</strong> is not any session's — it may be a
            shell in another workspace on another host. Attaching draws the same process
            the terminal or the desktop app is using; leaving this page detaches it and
            leaves the program running.
          </p>
        {:else}
          <p class="note">
            {panesNote ??
              'No terminals on this daemon. Start one from the TUI or the desktop app, then refresh — this page attaches to panes, it does not own them.'}
          </p>
        {/if}
      </section>

      <div class="actions">
        <button type="button" class="primary" onclick={() => void load()} disabled={loading}>
          {loading ? 'Refreshing…' : 'Refresh'}
        </button>
        <button type="button" onclick={() => (aboutOpen = true)}>About</button>
        <button type="button" onclick={disconnect}>Disconnect</button>
        <button type="button" onclick={quit}>Quit</button>
      </div>
    {:else}
      <h2>Connect to this daemon</h2>
      <p class="tagline">
        A code editor that flies your agents. You keep the controls — which is why
        the permission prompts exist.
      </p>
      <p class="note">
        Paste the daemon's token. It is written to
        <code>$XDG_RUNTIME_DIR/acpilot/token</code> and logged at startup. The token
        stays in this tab's session storage and travels only in an
        <code>Authorization</code> header.
      </p>

      <form class="card" onsubmit={connect}>
        <label for="token">Daemon token</label>
        <input
          id="token"
          name="token"
          type="password"
          autocomplete="off"
          spellcheck="false"
          bind:value={draft}
          disabled={loading}
        />
        <div class="actions">
          <button type="submit" class="primary" disabled={loading || !draft.trim()}>
            {loading ? 'Connecting…' : 'Connect'}
          </button>
          <button type="button" onclick={() => (aboutOpen = true)}>About</button>
          <button type="button" onclick={quit}>Quit</button>
        </div>
        {#if error}
          <p class="error" role="alert">{error}</p>
        {/if}
      </form>

    {/if}

    {#if quitNote}
      <p class="note" role="status">
        This tab was not opened by a script, so the browser will not close it. Close
        the tab to leave; the daemon keeps running, which is the point of it.
      </p>
    {/if}
  </main>

  <dialog bind:this={dialog} aria-labelledby="about-title" onclose={() => (aboutOpen = false)}>
    <h2 id="about-title">Acpilot <span class="version">{version}</span></h2>
    <p class="tagline">A code editor that flies your agents.</p>
    <dl>
      <dt>Licence</dt>
      <dd>MIT OR Apache-2.0</dd>
      <dt>Field manual</dt>
      <dd><a href="https://rudeboy.dev/acpilot/">rudeboy.dev/acpilot</a></dd>
      <dt>Built with</dt>
      <dd>Freebuff — <a href="https://freebuff.com">freebuff.com</a></dd>
    </dl>
    <form method="dialog"><button type="submit">Close</button></form>
  </dialog>
</div>
