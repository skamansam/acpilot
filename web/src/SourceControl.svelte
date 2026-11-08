<script lang="ts">
  // The browser source-control pane.
  //
  // It reads `GET /v1/git/status` and both halves of the diff, draws the selected
  // file's unstaged and staged hunks, stages one hunk or the whole path, discards
  // or checks a path out after a confirmation, and opens a composer whose commit
  // is a second, explicit click.

  import { ApiError, checkoutPaths, commitMessage, discardPaths, gitDiff, gitStatus, commitStaged, stageHunk, stagePaths, unstagePaths, type CommitDraft, type GitDiff, type GitStatus, type SessionSummary } from './api';
import { canCheckout, canDiscard, canStage, canUnstage, hunksOf, marks, shortId, stagedCount } from './git';

let { token, sessions, openSession }: {
  token: string;
  sessions: SessionSummary[];
  openSession: string | null;
} = $props();

let open = $state(false);
let loading = $state(false);
let status = $state<GitStatus | null>(null);
let unstagedDiff = $state<GitDiff | null>(null);
let stagedDiff = $state<GitDiff | null>(null);
let error = $state<string | null>(null);
let note = $state<string | null>(null);
let selected = $state(0);

let composing = $state(false);
let message = $state('');
let generating = $state(false);
let committing = $state(false);
let confirming = $state(false);
let composerError = $state<string | null>(null);
let indexing = $state(false);
let restoring = $state<'discard' | 'checkout' | null>(null);
let confirmingRestore = $state<{ kind: 'discard' | 'checkout'; path: string } | null>(null);

const files = $derived(status?.files ?? []);
const stagedFiles = $derived(stagedCount(files));
const selectedEntry = $derived(files[selected] ?? null);
const unstagedHunks = $derived(
  unstagedDiff && selectedEntry ? hunksOf(unstagedDiff, selectedEntry.path) : [],
);
const stagedHunks = $derived(
  stagedDiff && selectedEntry ? hunksOf(stagedDiff, selectedEntry.path) : [],
);
const stageable = $derived(canStage(selectedEntry));
const unstageable = $derived(canUnstage(selectedEntry));
const discardable = $derived(canDiscard(selectedEntry));
const checkoutable = $derived(canCheckout(selectedEntry, status?.head !== null));
const sessionId = $derived(openSession ?? sessions[0]?.id ?? '');

function describe(failure: unknown): string {
  return failure instanceof ApiError || failure instanceof Error
    ? failure.message
    : String(failure);
}

async function load(): Promise<void> {
  loading = true;
  error = null;
  confirmingRestore = null;
  try {
    const [nextStatus, nextUnstaged, nextStaged] = await Promise.all([
      gitStatus(token),
      gitDiff('unstaged', token),
      gitDiff('staged', token),
    ]);
    status = nextStatus;
    unstagedDiff = nextUnstaged;
    stagedDiff = nextStaged;
    selected = Math.min(selected, Math.max(0, nextStatus.files.length - 1));
  } catch (failure) {
    error = describe(failure);
  } finally {
    loading = false;
  }
}

async function toggle(): Promise<void> {
  open = !open;
  if (open && status === null) {
    await load();
  }
}

function choose(index: number): void {
  selected = index;
}

async function stageOneHunk(path: string, hunk: number): Promise<void> {
  if (indexing) {
    return;
  }
  indexing = true;
  error = null;
  try {
    await stageHunk(path, hunk, token);
    note = `Staged hunk ${hunk + 1} of ${path}.`;
    await load();
  } catch (failure) {
    error = describe(failure);
  } finally {
    indexing = false;
  }
}

async function index(staged: boolean): Promise<void> {
  if (indexing || selectedEntry === null) {
    return;
  }
  const path = selectedEntry.path;
  indexing = true;
  error = null;
  try {
    if (staged) {
      await stagePaths([path], token);
      note = `Staged ${path}.`;
    } else {
      await unstagePaths([path], token);
      note = `Unstaged ${path}.`;
    }
    await load();
  } catch (failure) {
    error = describe(failure);
  } finally {
    indexing = false;
  }
}

function armRestore(kind: 'discard' | 'checkout'): void {
  if (selectedEntry === null || restoring !== null) {
    return;
  }
  if (kind === 'discard' && !discardable) {
    error = `${selectedEntry.path} has no unstaged change to discard`;
    return;
  }
  if (kind === 'checkout' && !checkoutable) {
    error = `${selectedEntry.path} has no HEAD version to check out`;
    return;
  }
  confirmingRestore = { kind, path: selectedEntry.path };
  error = null;
}

async function restore(): Promise<void> {
  if (confirmingRestore === null || restoring !== null) {
    return;
  }
  const { kind, path } = confirmingRestore;
  confirmingRestore = null;
  restoring = kind;
  error = null;
  try {
    if (kind === 'discard') {
      await discardPaths([path], token);
      note = `Discarded ${path}.`;
    } else {
      await checkoutPaths([path], token);
      note = `Checked out ${path} from HEAD.`;
    }
    await load();
  } catch (failure) {
    error = describe(failure);
  } finally {
    restoring = null;
  }
}

function compose(generate: boolean): void {
  if (stagedFiles === 0) {
    error = 'nothing is staged · stage a path first';
    return;
  }
  composing = true;
  confirming = false;
  composerError = null;
  if (generate) {
    void generateDraft();
  }
}

async function generateDraft(): Promise<void> {
  if (!sessionId) {
    composerError = 'no session is open · start one to generate a message';
    return;
  }
  generating = true;
  composerError = null;
  try {
    const draft: CommitDraft = await commitMessage(sessionId, token);
    message = draft.message;
    note = `Drafted a ${draft.kind} message from ${draft.generator}; edit it before committing.`;
  } catch (failure) {
    composerError = describe(failure);
  } finally {
    generating = false;
  }
}

function review(): void {
  if (!message.trim()) {
    composerError = 'a commit needs a message';
    return;
  }
  confirming = true;
  composerError = null;
}

async function commit(): Promise<void> {
  if (committing || !message.trim()) {
    return;
  }
  committing = true;
  confirming = false;
  composerError = null;
  try {
    const commit = await commitStaged(message, token);
    note = `Committed ${shortId(commit.id)} · ${commit.subject}`;
    composing = false;
    message = '';
    await load();
  } catch (failure) {
    composerError = describe(failure);
  } finally {
    committing = false;
  }
}
</script>

<section class="source" aria-labelledby="source-title">
  <div class="stats-head">
    <h3 id="source-title">Source control</h3>
    <button type="button" class="link" aria-expanded={open} onclick={() => void toggle()}>
      {open ? 'Close' : 'Open source control'}
    </button>
  </div>

  {#if open}
    {#if loading}
      <p class="note">Reading the index…</p>
    {:else if error && status === null}
      <p class="error" role="alert">{error}</p>
    {:else if status}
      <p class="source-branch">
        <strong>{status.branch ?? '(detached)'}</strong>
        {#if status.upstream}· {status.upstream}{/if}
        {#if status.ahead}· {status.ahead} ahead{/if}
        {#if status.behind}· {status.behind} behind{/if}
        · {files.length} changed · {stagedFiles} staged
        <button type="button" class="link" onclick={() => void load()}>Refresh</button>
      </p>
      {#if error}<p class="error" role="alert">{error}</p>{/if}

      <div class="source-body">
        <ul class="source-files" aria-label="Changed files">
          {#each files as entry, index (entry.path)}
            <li>
              <button
                type="button"
                class="link"
                aria-current={index === selected ? 'true' : undefined}
                onclick={() => choose(index)}
              >
                <code class="source-marks">{marks(entry)}</code>
                {entry.path}
              </button>
            </li>
          {:else}
            <li class="note">No changed files.</li>
          {/each}
        </ul>
        {#if selectedEntry}
          <div class="source-sections">
            <section class="source-half" aria-labelledby="unstaged-title">
              <h4 id="unstaged-title">
                Unstaged {#if unstagedHunks.length}· {unstagedHunks.length} {unstagedHunks.length === 1 ? 'hunk' : 'hunks'}{/if}
              </h4>
              {#each unstagedHunks as hunk (hunk.index)}
                <div class="source-hunk">
                  <div class="source-hunk-head">
                    <span>Hunk {hunk.index + 1} of {unstagedHunks.length}</span>
                    <button
                      type="button"
                      aria-label={`Stage hunk ${hunk.index + 1} of ${unstagedHunks.length} in ${selectedEntry.path}`}
                      onclick={() => void stageOneHunk(selectedEntry!.path, hunk.index)}
                      disabled={indexing}
                    >
                      Stage hunk {hunk.index + 1} of {unstagedHunks.length}
                    </button>
                  </div>
                  <pre class="source-diff" aria-label={`Unstaged hunk ${hunk.index + 1} of ${selectedEntry.path}`}>{#each hunk.lines as line, index (index)}<span class="diff-line" data-kind={line.startsWith('+')
                        ? 'add'
                        : line.startsWith('-')
                          ? 'del'
                          : line.startsWith('@')
                            ? 'hunk'
                            : line.startsWith('diff ')
                              ? 'meta'
                              : 'text'}>{line}</span>{/each}</pre>
                </div>
              {:else}
                <p class="note">No unstaged hunk for this path.</p>
              {/each}
            </section>

            <section class="source-half" aria-labelledby="staged-title">
              <h4 id="staged-title">
                Staged {#if stagedHunks.length}· {stagedHunks.length} {stagedHunks.length === 1 ? 'hunk' : 'hunks'}{/if}
              </h4>
              {#each stagedHunks as hunk (hunk.index)}
                <pre class="source-diff" aria-label={`Staged hunk ${hunk.index + 1} of ${selectedEntry.path}`}>{#each hunk.lines as line, index (index)}<span class="diff-line" data-kind={line.startsWith('+')
                      ? 'add'
                      : line.startsWith('-')
                        ? 'del'
                        : line.startsWith('@')
                          ? 'hunk'
                          : line.startsWith('diff ')
                            ? 'meta'
                            : 'text'}>{line}</span>{/each}</pre>
              {:else}
                <p class="note">No staged hunk for this path.</p>
              {/each}
            </section>
          </div>
        {/if}
      </div>

      <div class="source-actions">
        <button
          type="button"
          onclick={() => void index(true)}
          disabled={!stageable || indexing}
        >
          {indexing ? 'Updating index…' : 'Stage path'}
        </button>
        <button
          type="button"
          onclick={() => void index(false)}
          disabled={!unstageable || indexing}
        >
          Unstage
        </button>
        <button
          type="button"
          onclick={() => armRestore('discard')}
          disabled={!discardable || indexing || restoring !== null}
        >
          {restoring === 'discard' ? 'Discarding…' : 'Discard'}
        </button>
        <button
          type="button"
          onclick={() => armRestore('checkout')}
          disabled={!checkoutable || indexing || restoring !== null}
        >
          {restoring === 'checkout' ? 'Checking out…' : 'Checkout'}
        </button>
        <button type="button" onclick={() => compose(false)} disabled={stagedFiles === 0}>
          Compose commit
        </button>
        <button
          type="button"
          onclick={() => compose(true)}
          disabled={stagedFiles === 0 || !sessionId || generating}
        >
          {generating ? 'Drafting…' : 'Generate message'}
        </button>
        {#if !sessionId}
          <span class="note">Generation needs an open session.</span>
        {/if}
      </div>

      {#if confirmingRestore}
        <div class="source-confirm" role="alert">
          <p>
            {confirmingRestore.kind === 'discard'
              ? `Discard the unstaged changes to ${confirmingRestore.path}? Staged changes stay.`
              : `Check out ${confirmingRestore.path} from HEAD? Staged and unstaged changes will be discarded.`}
          </p>
          <button type="button" class="primary" onclick={() => void restore()}>
            {confirmingRestore.kind === 'discard' ? 'Discard path' : 'Checkout path'}
          </button>
          <button type="button" onclick={() => (confirmingRestore = null)}>Cancel</button>
        </div>
      {/if}

      {#if composing}
        <div class="source-composer">
          <label for="commit-message">Commit message</label>
          <textarea
            id="commit-message"
            bind:value={message}
            rows="5"
            spellcheck="false"
            disabled={generating || committing}
          ></textarea>
          {#if composerError}
            <p class="error" role="alert">{composerError}</p>
          {/if}
          {#if confirming}
            <div class="source-confirm" role="alert">
              <p>Commit {stagedFiles} staged {stagedFiles === 1 ? 'path' : 'paths'} with this message?</p>
              <button type="button" class="primary" onclick={() => void commit()} disabled={committing}>
                {committing ? 'Committing…' : 'Commit staged index'}
              </button>
              <button type="button" onclick={() => (confirming = false)} disabled={committing}>
                Cancel
              </button>
            </div>
          {:else}
            <div class="source-actions">
              <button type="button" class="primary" onclick={review} disabled={generating || committing}>
                Review commit
              </button>
              <button type="button" onclick={() => void generateDraft()} disabled={generating || !sessionId}>
                {generating ? 'Drafting…' : 'Generate'}
              </button>
              <button type="button" onclick={() => (composing = false)} disabled={committing}>
                Close
              </button>
            </div>
          {/if}
        </div>
      {/if}
    {/if}
  {/if}

  {#if note}<p class="note" role="status">{note}</p>{/if}
</section>
