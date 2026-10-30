<script lang="ts">
  import { onMount } from 'svelte';
  import {
    ApiError,
    health,
    readToken,
    sessions as listSessions,
    stats as readStats,
    writeToken,
    type Health,
    type SessionSummary,
    type StatRow,
    type Track,
  } from './api';

  // The build the daemon injected into the page before anyone had a token. Its
  // health answer is preferred once there is one, because that is the process
  // that will actually serve the requests.
  const buildVersion =
    document.querySelector<HTMLMetaElement>('meta[name="acpilot-version"]')?.content ?? 'unknown';

  let token = $state(readToken());
  let draft = $state('');
  let loading = $state(false);
  let error = $state<string | null>(null);
  let healthInfo = $state<Health | null>(null);
  let rows = $state<SessionSummary[]>([]);
  let integrations = $state<StatRow[]>([]);
  // Why the bar is empty when the daemon has nothing to say about it: a bar that
  // could not be read is not a roster of none, and an older daemon that never
  // heard of `/app/stats` must not take the whole page down with it.
  let statsNote = $state<string | null>(null);
  // Provenance is one click away, the same key (F4) the terminal uses: a bar has no
  // room for where a number came from, and a bar that never says is a claim.
  let sources = $state(false);
  let aboutOpen = $state(false);
  let quitNote = $state(false);
  let dialog = $state<HTMLDialogElement | null>(null);

  const version = $derived(healthInfo?.version ?? buildVersion);
  const connected = $derived(healthInfo !== null);

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
    controller?.abort();
    const current = new AbortController();
    controller = current;
    loading = true;
    error = null;
    try {
      const [status, sessions, bar] = await Promise.all([
        health(token, current.signal),
        listSessions(token, current.signal),
        // The bar is the one part of the page a daemon older than this build may
        // not serve, so its absence is reported beside the bar rather than as a
        // failure of the page.
        readStats(token, current.signal).catch(() => null),
      ]);
      healthInfo = status;
      rows = sessions;
      integrations = bar ?? [];
      statsNote = bar
        ? null
        : 'This daemon did not report its integrations; the bar needs a build that serves /app/stats.';
    } catch (cause) {
      if (current.signal.aborted) {
        return;
      }
      healthInfo = null;
      rows = [];
      integrations = [];
      statsNote = null;
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
    rows = [];
    integrations = [];
    statsNote = null;
    error = null;
    loading = false;
    writeToken('');
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

  onMount(() => {
    if (token) {
      void load();
    }
    paletteSource = new EventSource('/app/palette/events');
    paletteSource.addEventListener('palette', (event) => {
      applyPalette(event as MessageEvent<string>);
    });
    return () => {
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
                <td>{row.title || row.id}</td>
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

      <p class="note">
        The editor, the transcript and the terminal arrive with the web phase
        (P11) — until then the TUI and the desktop app are the cockpits.
      </p>
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
