<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { basicSetup } from 'codemirror';
  import { EditorState, type Extension } from '@codemirror/state';
  import { EditorView } from '@codemirror/view';
  import { MergeView } from '@codemirror/merge';
  import { javascript } from '@codemirror/lang-javascript';
  import { json } from '@codemirror/lang-json';
  import { markdown } from '@codemirror/lang-markdown';
  import { rust } from '@codemirror/lang-rust';
  import { python } from '@codemirror/lang-python';
  import {
    ApiError,
    listDirectory,
    readFile,
    writeFile,
    type FileContent,
    type FileEntry,
  } from './api';
  import {
    basename,
    isDirty,
    languageFor,
    listingRows,
    normalisePath,
    parentOf,
    saveFailure,
    type EditorLanguage,
  } from './files';

  let { token }: { token: string } = $props();

  // The browser's state: which directory is listed, and the rows in it.
  let dir = $state('');
  let entries = $state<FileEntry[]>([]);
  let listingNote = $state<string | null>(null);
  const listing = $derived(listingRows(entries));

  // The open file and the buffer. `file` is the version last read from or written
  // to disk — its `etag` is what a save sends — and `draft` is what is on screen.
  let file = $state<FileContent | null>(null);
  let draft = $state('');
  // Bumped on every load, so the editor is rebuilt for a new document rather
  // than having its changes dispatched into the previous one.
  let generation = $state(0);
  let mode = $state<'edit' | 'diff'>('edit');
  let hostEl = $state<HTMLDivElement | null>(null);
  let diffEl = $state<HTMLDivElement | null>(null);

  let opening = $state(false);
  let saving = $state(false);
  let note = $state<string | null>(null);
  let error = $state<string | null>(null);
  let pathInput = $state('');
  let browserOpen = $state(true);

  const dirty = $derived(isDirty(file, draft));

  // The palette, not a second theme file: the daemon's colours arrive as custom
  // properties, and an editor that named its own would be the one pane that did
  // not follow a live repaint.
  const theme = EditorView.theme({
    '&': {
      backgroundColor: 'var(--bg)',
      color: 'var(--text)',
      fontSize: '0.9rem',
      height: '100%',
    },
    '.cm-content': {
      fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
      padding: '0.5rem 0',
    },
    '.cm-gutters': {
      backgroundColor: 'var(--bg-panel)',
      color: 'var(--text-muted)',
      border: 'none',
    },
    '.cm-activeLine': { backgroundColor: 'rgb(255 255 255 / 0.04)' },
    '.cm-activeLineGutter': { backgroundColor: 'transparent', color: 'var(--text)' },
    '&.cm-focused': { outline: 'none' },
    '.cm-selectionBackground, ::selection': { backgroundColor: 'rgb(90 169 255 / 0.25)' },
  });

  function extensionFor(language: EditorLanguage, path: string): Extension {
    switch (language) {
      case 'javascript':
        // The one grammar for both languages: a `.ts` parsed without the TS
        // dialect draws identifiers as errors, and `javascript()` reads the
        // dialect from the options rather than the extension.
        return javascript({
          typescript: /\.[cm]?tsx?$/.test(path),
          jsx: /\.[jt]sx$/.test(path),
        });
      case 'json':
        return json();
      case 'markdown':
        return markdown();
      case 'rust':
        return rust();
      case 'python':
        return python();
      default:
        return [];
    }
  }

  function describe(failure: unknown): string {
    if (failure instanceof ApiError || failure instanceof Error) {
      return failure.message;
    }
    return String(failure);
  }

  async function browse(path: string): Promise<void> {
    const next = normalisePath(path);
    listingNote = null;
    try {
      entries = await listDirectory(next, token);
      dir = next;
    } catch (failure) {
      entries = [];
      listingNote = describe(failure);
    }
  }

  async function open(path: string): Promise<void> {
    const next = normalisePath(path);
    if (!next) {
      return;
    }
    opening = true;
    error = null;
    note = null;
    try {
      const read = await readFile(next, token);
      file = read;
      draft = read.text;
      pathInput = read.path;
      generation += 1;
      mode = 'edit';
      // A file opened by path should leave the browser looking at its directory,
      // or the next click would be measured from a listing nobody can see.
      const parent = parentOf(read.path);
      if (parent !== dir) {
        entries = await listDirectory(parent, token).catch(() => entries);
        dir = parent;
      }
    } catch (failure) {
      error = describe(failure);
    } finally {
      opening = false;
    }
  }

  async function save(): Promise<void> {
    if (!file || !dirty || saving) {
      return;
    }
    saving = true;
    error = null;
    note = null;
    try {
      const written = await writeFile(file.path, draft, file.etag, token);
      file = written;
      draft = written.text;
      note = `Saved ${written.path}`;
      // The row's git status changed with the write; the listing is a second
      // question about the same file, so it is asked again rather than guessed.
      void browse(dir);
    } catch (failure) {
      error = saveFailure(failure instanceof ApiError ? failure.status : 0, describe(failure));
    } finally {
      saving = false;
    }
  }

  async function reload(): Promise<void> {
    if (!file) {
      return;
    }
    const path = file.path;
    await open(path);
    note = 'Reloaded from disk. Unsaved edits were discarded.';
  }

  // The editor is built once per loaded document. Reading `draft` and `file`
  // outside the effect's dependencies is deliberate: a keystroke must not rebuild
  // the view, and a save must not move the caret.
  $effect(() => {
    const target = hostEl;
    const loaded = generation;
    const editing = mode === 'edit';
    if (!target || !loaded || !editing) {
      return;
    }
    const doc = untrack(() => draft);
    const where = untrack(() => file?.path ?? '');
    const view = new EditorView({
      state: EditorState.create({
        doc,
        extensions: [
          basicSetup,
          theme,
          extensionFor(languageFor(where), where),
          EditorView.updateListener.of((update) => {
            if (update.docChanged) {
              draft = update.state.doc.toString();
            }
          }),
        ],
      }),
      parent: target,
    });
    view.focus();
    return () => view.destroy();
  });

  // The diff is `@codemirror/merge` over the two documents the page already has:
  // the version on disk and the buffer. Both sides are read-only — the buffer is
  // edited in the other mode, and an edit made in a side-by-side view would be an
  // edit to a document that is not the one being saved.
  $effect(() => {
    const target = diffEl;
    const comparing = mode === 'diff';
    if (!target || !comparing) {
      return;
    }
    const before = untrack(() => file?.text ?? '');
    const after = untrack(() => draft);
    const merge = new MergeView({
      a: {
        doc: before,
        extensions: [EditorView.editable.of(false), EditorState.readOnly.of(true)],
      },
      b: {
        doc: after,
        extensions: [EditorView.editable.of(false), EditorState.readOnly.of(true)],
      },
      parent: target,
      collapseUnchanged: { margin: 3, minSize: 4 },
    });
    return () => merge.destroy();
  });

  onMount(() => {
    void browse('');
  });
</script>

<section class="editor" aria-label="Editor">
  <div class="editor-bar">
    <button
      type="button"
      class="link"
      aria-expanded={browserOpen}
      onclick={() => (browserOpen = !browserOpen)}
    >
      Files
    </button>
    <code class="editor-path">{file?.path ?? 'no file open'}</code>
    {#if dirty}
      <span class="dirty" title="Unsaved changes" aria-label="Unsaved changes">●</span>
    {/if}
    <span class="editor-spacer"></span>
    {#if file}
      <button
        type="button"
        aria-pressed={mode === 'diff'}
        onclick={() => (mode = mode === 'edit' ? 'diff' : 'edit')}
      >
        {mode === 'diff' ? 'Edit' : 'Diff'}
      </button>
      <button type="button" onclick={() => void reload()} disabled={opening || saving}>
        Reload
      </button>
      <button type="button" class="primary" onclick={() => void save()} disabled={!dirty || saving}>
        {saving ? 'Saving…' : 'Save'}
      </button>
    {/if}
  </div>

  {#if browserOpen}
    <div class="file-browser">
      <form
        class="file-open"
        onsubmit={(event) => {
          event.preventDefault();
          void open(pathInput);
        }}
      >
        <label for="file-path" class="visually-hidden">Path in the workspace</label>
        <input
          id="file-path"
          bind:value={pathInput}
          placeholder="path/to/file"
          spellcheck="false"
          autocomplete="off"
        />
        <button type="submit" disabled={opening}>Open</button>
      </form>
      <p class="file-where">
        <button type="button" class="link" onclick={() => void browse('')}>root</button>
        {#if dir}<span>/ {dir}</span>{/if}
        {#if dir}
          <button type="button" class="link" onclick={() => void browse(parentOf(dir))}>
            up
          </button>
        {/if}
      </p>
      {#if listing.length}
        <ul class="file-rows">
          {#each listing as row (row.path)}
            <li>
              <button
                type="button"
                class="link"
                data-dir={row.is_dir}
                onclick={() => (row.is_dir ? void browse(row.path) : void open(row.path))}
              >
                {row.is_dir ? '▸ ' : ''}{basename(row.path)}
              </button>
              {#if row.status}
                <span class="file-status" data-change={row.status}>{row.status}</span>
              {/if}
            </li>
          {/each}
        </ul>
      {:else}
        <p class="note">{listingNote ?? 'This directory is empty.'}</p>
      {/if}
    </div>
  {/if}

  {#if error}<p class="error" role="alert">{error}</p>{/if}
  {#if note}<p class="note" role="status">{note}</p>{/if}

  {#if file}
    {#if mode === 'diff'}
      <div class="diff-host" bind:this={diffEl} aria-label="Diff against the version on disk"></div>
      <p class="note">
        Left is the version on disk, right is the buffer. Both are read-only here;
        switch to Edit to change the buffer.
      </p>
    {:else}
      <div class="editor-host" bind:this={hostEl}></div>
    {/if}
    {#if file.marks?.length}
      <p class="note">
        {file.marks.length}
        {file.marks.length === 1 ? 'line differs' : 'lines differ'} from the last commit.
      </p>
    {/if}
  {:else}
    <p class="note">
      Open a file from the list or type its path. A save carries the version the
      file was read at (<code>if_match</code>), so a file somebody else changed is
      refused rather than overwritten.
    </p>
  {/if}
</section>
