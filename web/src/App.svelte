<script lang="ts">
  import { onMount } from 'svelte';
  import {
    ApiError,
    health,
    readToken,
    sessions as listSessions,
    writeToken,
    type Health,
    type SessionSummary,
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
  let aboutOpen = $state(false);
  let quitNote = $state(false);
  let dialog = $state<HTMLDialogElement | null>(null);

  const version = $derived(healthInfo?.version ?? buildVersion);
  const connected = $derived(healthInfo !== null);

  let controller: AbortController | null = null;

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
      const [status, sessions] = await Promise.all([
        health(token, current.signal),
        listSessions(token, current.signal),
      ]);
      healthInfo = status;
      rows = sessions;
    } catch (cause) {
      if (current.signal.aborted) {
        return;
      }
      healthInfo = null;
      rows = [];
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

  function when(iso: string): string {
    const at = new Date(iso);
    return Number.isNaN(at.valueOf()) ? iso : at.toLocaleString();
  }

  onMount(() => {
    if (token) {
      void load();
    }
    return () => controller?.abort();
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
