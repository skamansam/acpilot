<script lang="ts">
  // The browser source-control pane.
  //
  // It reads `GET /v1/git/status` and `GET /v1/git/diff?scope=all`, draws the
  // selected file's section of the daemon's patch, stages or unstages that path,
  // and opens a composer whose commit is a second, explicit click.

  import { ApiError, commitMessage, gitDiff, gitStatus, commitStaged, stagePaths, unstagePaths, type CommitDraft, type GitDiff, type GitStatus, type SessionSummary } from './api';
import { canStage, canUnstage, marks, patchSection, shortId, stagedCount } from './git';

let { token, sessions, openSession }: {
  token: string;
  sessions: SessionSummary[];
  openSession: string | null;
} = $props();

let open = $state(false);
let loading = $state(false);
let status = $state<GitStatus | null>(null);
let diff = $state<GitDiff | null>(null);
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

const files = $derived(status?.files ?? []);
const section = $derived(
  diff && files[selected] ? patchSection(diff, files[selected]!.path) : [],
);
const stagedFiles = $derived(stagedCount(files));
const selectedEntry = $derived(files[selected] ?? null);
const stageable = $derived(canStage(selectedEntry));
const unstageable = $derived(canUnstage(selectedEntry));
const sessionId = $derived(openSession ?? sessions[0]?.id ?? '');

function describe(failure: unknown): string {
  return failure instanceof ApiError || failure instanceof Error
    ? failure.message
    : String(failure);
}

async function load(): Promise<void> {
  loading = true;
  error = null;
  try {
    const [nextStatus, nextDiff] = await Promise.all([gitStatus(token), gitDiff(token)]);
    status = nextStatus;
    diff = nextDiff;
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
        <pre class="source-diff" aria-label="Diff for the selected file">{#each section as line, index (index)}<span class="diff-line" data-kind={line.startsWith('+')
                ? 'add'
                : line.startsWith('-')
                  ? 'del'
                  : line.startsWith('@')
                    ? 'hunk'
                    : line.startsWith('diff ')
                      ? 'meta'
                      : 'text'}>{line}</span>{/each}{#if section.length === 0}<span class="note">No diff for this path yet · Stage puts it in the index.</span>{/if}</pre>
      </div>

      <div class="source-actions">
        <button
          type="button"
          onclick={() => void index(true)}
          disabled={!stageable || indexing}
        >
          {indexing ? 'Updating index…' : 'Stage'}
        </button>
        <button
          type="button"
          onclick={() => void index(false)}
          disabled={!unstageable || indexing}
        >
          Unstage
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
