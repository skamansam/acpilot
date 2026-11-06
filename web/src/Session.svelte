<script lang="ts">
  import { onMount } from 'svelte';
  import {
    cancelSession,
    pendingPermissions,
    promptSession,
    resolvePermission,
    type PermissionOption,
    type PermissionRequest,
    type SessionEvent,
    type SessionSummary,
  } from './api';
  import {
    buildTranscript,
    describeError,
    diffLines,
    followPermissions,
    followSession,
    inline,
    markdown,
    permissionLabel,
    windowed,
    type Span,
    type StreamStatus,
    type Turn,
  } from './session';

  let {
    session,
    token,
    onclose,
  }: { session: SessionSummary; token: string; onclose?: () => void } = $props();

  // What the transcript is made of. The events are kept whole so a re-fold after
  // each chunk is cheap and cannot disagree with itself; a chunk is not a message,
  // and the folding rule lives in `session.ts` where it is tested without a DOM.
  let turns = $state<Turn[]>([]);
  let pending = $state<PermissionRequest[]>([]);
  let answers = $state<Record<string, { option: PermissionOption; state: 'sending' | 'sent' }>>({});
  let status = $state<StreamStatus>('connecting');
  let streamNote = $state<string | null>(null);
  let answerError = $state<string | null>(null);
  let cancelNote = $state<string | null>(null);
  let cancelling = $state(false);
  let promptText = $state('');
  let sendingPrompt = $state(false);
  let promptError = $state<string | null>(null);
  // A replayed conversation can be thousands of events; only the newest page is
  // in the DOM, and "show earlier" grows the page instead of virtualising it.
  let windowSize = $state(200);
  const seen = new Set<number>();
  const events: SessionEvent[] = [];

  const streamLabel = $derived(
    status === 'live'
      ? 'live'
      : status === 'connecting'
        ? 'connecting…'
        : status === 'reconnecting'
          ? 'reconnecting…'
          : 'ended',
  );

  const answered = $derived(Object.entries(answers));
  const paging = $derived(windowed(turns, windowSize));

  /**
   * The questions still waiting, from the daemon's own list.
   *
   * Pending requests are state, not history: this is how a page that just opened
   * learns the ones raised while it was away. It is also the reconcile half of an
   * optimistic answer — if the daemon still lists a request after the POST failed,
   * the card comes back rather than staying gone on a guess.
   */
  async function refreshPending(): Promise<void> {
    try {
      const list = await pendingPermissions(token, session.id);
      // A card whose answer is in flight stays hidden until the daemon agrees;
      // every other row is the daemon's answer, not ours.
      pending = list.filter((request) => answers[request.id]?.state !== 'sending');
    } catch (cause) {
      answerError = describeError(cause);
    }
  }

  function addPending(request: PermissionRequest): void {
    // A resolution that raced the request must win: an answered card is gone.
    if (answers[request.id] || pending.some((waiting) => waiting.id === request.id)) {
      return;
    }
    pending = [...pending, request];
  }

  /**
   * Answer a request, optimistically.
   *
   * The card goes the moment the button is pressed — a permission prompt that
   * waits for a round trip is a prompt a person presses twice. The daemon is still
   * the authority: a refusal is shown, and the pending list is refetched so a card
   * that is genuinely still waiting reappears.
   */
  async function answer(request: PermissionRequest, option: PermissionOption): Promise<void> {
    answers = { ...answers, [request.id]: { option, state: 'sending' } };
    pending = pending.filter((waiting) => waiting.id !== request.id);
    answerError = null;
    try {
      const decision = await resolvePermission(request.id, option, token);
      answers = { ...answers, [request.id]: { option: decision.option, state: 'sent' } };
      await refreshPending();
    } catch (cause) {
      const message = describeError(cause);
      // The entry is removed rather than left failed: the card is the record, and
      // a failed answer that stayed gone would be worse than the error beside it.
      const remaining = { ...answers };
      delete remaining[request.id];
      answers = remaining;
      answerError = message;
      await refreshPending();
    }
  }

  /**
   * Start a turn.
   *
   * The text is cleared only once the daemon accepted it: a prompt that failed is
   * still the sentence a person typed, and clearing it early is the one mistake
   * this form cannot undo.
   */
  async function sendPrompt(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const text = promptText.trim();
    if (!text || sendingPrompt) {
      return;
    }
    sendingPrompt = true;
    promptError = null;
    try {
      await promptSession(session.id, text, token);
      promptText = '';
    } catch (cause) {
      promptError = describeError(cause);
    } finally {
      sendingPrompt = false;
    }
  }

  async function cancel(): Promise<void> {
    cancelling = true;
    cancelNote = null;
    try {
      const accepted = await cancelSession(session.id, token);
      cancelNote = accepted.cancelled
        ? `Asked the agent to stop${accepted.turn_id ? ` ${accepted.turn_id}` : ''}.`
        : 'Nothing was in flight, so there was nothing to stop.';
    } catch (cause) {
      cancelNote = describeError(cause);
    } finally {
      cancelling = false;
    }
  }

  onMount(() => {
    void refreshPending();

    const transcript = followSession({
      id: session.id,
      token,
      onEvent(event) {
        // A reconnect can overlap the last event the ring already delivered.
        if (seen.has(event.seq)) {
          return;
        }
        seen.add(event.seq);
        events.push(event);
        turns = buildTranscript(events);
      },
      onResync() {
        // The ring could not replay what was missed, so the transcript is rebuilt
        // from the store: drop it and ask from sequence 0, which is the only way
        // to recover the frames a resync skipped. The pending list is refetched
        // for the same reason — it is state, and this is the moment to re-ask.
        seen.clear();
        events.length = 0;
        turns = [];
        windowSize = 200;
        transcript.restart(0);
        void refreshPending();
      },
      onStatus(next) {
        status = next;
        if (next === 'live') {
          streamNote = null;
        } else if (next === 'reconnecting') {
          streamNote = 'The stream dropped; reconnecting where it left off.';
        }
      },
      onEnd() {
        streamNote =
          'This stream has ended. A stored conversation ends after its replay, and a live one ends when the session does.';
      },
      onError(cause) {
        streamNote = `The transcript is not reachable right now: ${describeError(cause)} Retrying.`;
      },
    });

    // New questions arrive on the profile-wide stream rather than on the
    // transcript, because a request belongs to a session and outlives its turn.
    const permissions = followPermissions({
      token,
      onRequested(request) {
        if (request.session_id === session.id) {
          addPending(request);
        }
      },
      onResolved(id, option) {
        pending = pending.filter((waiting) => waiting.id !== id);
        if (!answers[id] && option) {
          answers = { ...answers, [id]: { option, state: 'sent' } };
        }
      },
      onResync() {
        void refreshPending();
      },
      onError(cause) {
        answerError = `Permission updates are not reachable: ${describeError(cause)}`;
      },
    });

    return () => {
      transcript.close();
      permissions.close();
    };
  });
</script>

{#snippet spans(list: Span[])}
  {#each list as span}{#if span.code}<code>{span.text}</code>{:else if span.href}<a
        href={span.href}
        target="_blank"
        rel="noreferrer noopener">{span.text}</a
      >{:else if span.strong}<strong>{span.text}</strong>{:else if span.em}<em>{span.text}</em
      >{:else}{span.text}{/if}{/each}
{/snippet}

{#snippet prose(source: string)}
  {#each markdown(source) as block, index (index)}
    {#if block.kind === 'code'}
      <pre class="md-code" data-language={block.language}><code>{block.text}</code></pre>
    {:else if block.kind === 'heading'}
      <p class="md-heading" data-level={block.level}>{@render spans(block.spans)}</p>
    {:else if block.kind === 'list' && block.ordered}
      <ol class="md-list">
        {#each block.items as item, itemIndex (itemIndex)}<li>{@render spans(item)}</li>{/each}
      </ol>
    {:else if block.kind === 'list'}
      <ul class="md-list">
        {#each block.items as item, itemIndex (itemIndex)}<li>{@render spans(item)}</li>{/each}
      </ul>
    {:else if block.kind === 'quote'}
      <blockquote>{@render spans(block.spans)}</blockquote>
    {:else if block.kind === 'table'}
      <table class="md-table">
        <thead>
          <tr>
            {#each block.head as cell, cellIndex (cellIndex)}
              <th scope="col">{@render spans(inline(cell))}</th>
            {/each}
          </tr>
        </thead>
        <tbody>
          {#each block.rows as row, rowIndex (rowIndex)}
            <tr>
              {#each row as cell, cellIndex (cellIndex)}
                <td>{@render spans(inline(cell))}</td>
              {/each}
            </tr>
          {/each}
        </tbody>
      </table>
    {:else}
      <p>{@render spans(block.spans)}</p>
    {/if}
  {/each}
{/snippet}

<section class="session" aria-labelledby="session-title">
  <div class="session-head">
    <h3 id="session-title">{session.title || session.id}</h3>
    <span class="status" data-status={session.status}>{session.status}</span>
    <span class="session-meta">{session.agent_id} · <code>{session.id}</code></span>
    <span class="session-stream" data-status={status}>{streamLabel}</span>
  </div>

  <div class="session-actions">
    <button
      type="button"
      onclick={() => void cancel()}
      disabled={cancelling || session.status === 'closed'}
    >
      {cancelling ? 'Cancelling…' : 'Cancel turn'}
    </button>
    <button type="button" class="link" onclick={() => onclose?.()}>Close</button>
  </div>

  {#if cancelNote}
    <p class="note" role="status">{cancelNote}</p>
  {/if}
  {#if streamNote}
    <p class="note" role="status">{streamNote}</p>
  {/if}
  {#if answerError}
    <p class="error" role="alert">{answerError}</p>
  {/if}

  {#if pending.length > 0}
    <div class="permissions">
      {#each pending as request (request.id)}
        <article class="permission" aria-labelledby={`permission-${request.id}`}>
          <h4 id={`permission-${request.id}`}>{request.title}</h4>
          <p class="permission-kind">{request.kind.replaceAll('_', ' ')}</p>
          {#if request.detail}
            {#if request.kind === 'file_write' && request.detail.includes('\n')}
              <pre class="diff" aria-label="proposed change">{#each diffLines(request.detail) as line, lineIndex (lineIndex)}<span class="diff-line" data-kind={line.kind}>{line.text}</span>{/each}</pre>
            {:else}
              <pre class="permission-detail">{request.detail}</pre>
            {/if}
          {/if}
          <div class="permission-options">
            {#each request.options as option (option)}
              <button
                type="button"
                class:primary={option === 'allow_once'}
                onclick={() => void answer(request, option)}
              >
                {permissionLabel(option)}
              </button>
            {/each}
          </div>
        </article>
      {/each}
    </div>
  {/if}

  {#if answered.length > 0}
    <ul class="answered">
      {#each answered as [id, entry] (id)}
        <li>{permissionLabel(entry.option)} — {entry.state === 'sending' ? 'sending…' : 'answered'}</li>
      {/each}
    </ul>
  {/if}

  <form class="prompt" onsubmit={(event) => void sendPrompt(event)}>
    <label for={`prompt-${session.id}`}>Prompt this session</label>
    <textarea
      id={`prompt-${session.id}`}
      rows="3"
      bind:value={promptText}
      disabled={sendingPrompt || session.status === 'closed'}
      placeholder="What should the agent do?"
    ></textarea>
    <div class="prompt-actions">
      <button
        type="submit"
        class="primary"
        disabled={sendingPrompt || !promptText.trim() || session.status === 'closed'}
      >
        {sendingPrompt ? 'Sending…' : 'Send'}
      </button>
      {#if promptError}
        <span class="error" role="alert">{promptError}</span>
      {/if}
    </div>
  </form>

  {#if turns.length > 0}
    {#if paging.hidden > 0}
      <button type="button" class="link show-earlier" onclick={() => (windowSize += 200)}>
        Show {Math.min(paging.hidden, 200)} earlier turns
      </button>
    {/if}
    <ol class="transcript">
      {#each paging.shown as turn (turn.seq)}
        {#if turn.kind === 'message'}
          <li class="turn" data-role={turn.role}>
            <span class="turn-role">{turn.role}</span>
            {@render prose(turn.text)}
          </li>
        {:else if turn.kind === 'thought'}
          <li class="turn thought">
            <details>
              <summary>Thinking</summary>
              {@render prose(turn.text)}
            </details>
          </li>
        {:else if turn.kind === 'tool'}
          <li class="tool" data-status={turn.status}>
            <div class="tool-head">
              <span class="tool-title">{turn.title}</span>
              <span class="tool-status">{turn.status}</span>
            </div>
            {#if turn.tool}
              <span class="tool-kind">{turn.tool}</span>
            {/if}
            {#if turn.detail}
              <details>
                <summary>Details</summary>
                <pre class="tool-detail">{turn.detail}</pre>
              </details>
            {/if}
          </li>
        {:else if turn.kind === 'notice'}
          <li class="turn notice" data-level={turn.level}>
            <strong>{turn.title}</strong>
            {#if turn.message}
              <span>{turn.message}</span>
            {/if}
          </li>
        {:else if turn.kind === 'plan'}
          <li class="turn plan">
            <span class="turn-role">plan</span>
            <ol class="md-list">
              {#each turn.entries as entry, entryIndex (entryIndex)}<li>{entry}</li>{/each}
            </ol>
          </li>
        {:else if turn.kind === 'mode'}
          <li class="turn mode">Mode: <code>{turn.mode}</code></li>
        {:else}
          <li class="turn other">{turn.label}: {turn.text}</li>
        {/if}
      {/each}
    </ol>
  {:else}
    <p class="note">
      No events on this session yet. A turn appears here as the agent streams it; a
      stored conversation replays from the daemon's record.
    </p>
  {/if}
</section>
