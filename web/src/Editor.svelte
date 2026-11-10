<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { basicSetup } from 'codemirror';
  import { Compartment, EditorState, type Extension } from '@codemirror/state';
  import { EditorView, gutter } from '@codemirror/view';
  import { MergeView } from '@codemirror/merge';
  import { javascript } from '@codemirror/lang-javascript';
  import { json } from '@codemirror/lang-json';
  import { markdown } from '@codemirror/lang-markdown';
  import { rust } from '@codemirror/lang-rust';
  import { python } from '@codemirror/lang-python';
  import {
    ApiError,
    createFile,
    deleteFile,
    formatFile,
    listDirectory,
    preferencesFor,
    readFile,
    renameFile,
    writeFile,
    type FileEntry,
    type LineMark,
    type Preferences,
  } from './api';
  import {
    basename,
    languageFor,
    listingRows,
    normalisePath,
    parentOf,
    saveFailure,
    storedTabs,
    tabDirty,
    tabStorage,
    type EditorLanguage,
    type EditorTab,
  } from './files';
  import { gitMarks, gitMarksFor, setGitMarks } from './git-gutter';

  let { token, profile = '' }: { token: string; profile?: string } = $props();

  // The daemon's answer, not this component's own idea of it: wrap and rulers
  // are the same preferences the TUI and desktop app apply. An absent profile —
  // the component mounted on its own in a test — leaves the editor modeless
  // rather than inventing defaults that disagree with the server.
  let preferences = $state<Preferences | null>(null);
  const wrap = new Compartment();
  const wrapExtension = (on: boolean): Extension =>
    on ? EditorView.lineWrapping : [];

  // A tab is a buffer plus the version of the file it was read at. A tab with no
  // `file` is a path whose first save is a create, which is why it is dirty from
  // the moment it exists.
  interface Tab extends EditorTab {
    /** `edit` or the read-only diff against the version on disk. */
    mode: 'edit' | 'diff';
    /** Bumped when the document is replaced, so the view is rebuilt for it. */
    generation: number;
  }

  let tabs = $state<Tab[]>([]);
  let active = $state(0);
  const tab = $derived(tabs[active] ?? null);
  const dirty = $derived(tab !== null && tabDirty(tab));

  // The browser's state: which directory is listed, and the rows in it.
  let dir = $state('');
  let entries = $state<FileEntry[]>([]);
  let listingNote = $state<string | null>(null);
  const listing = $derived(listingRows(entries));

  // The create flow: a path and the form that asks for it. The tab exists before
  // the file does — saving it is the request that creates it.
  let creating = $state(false);
  let newPath = $state('');

  // Renaming and deleting the open file ask for a path or a confirmation; `moving`
  // is one flag for both, because the pane must not start either while the other
  // is in flight.
  let renaming = $state(false);
  let renameFrom = $state<string | null>(null);
  let renamePath = $state('');
  let moving = $state(false);

  let hostEl = $state<HTMLDivElement | null>(null);
  let diffEl = $state<HTMLDivElement | null>(null);

  let opening = $state(false);
  let saving = $state(false);
  let note = $state<string | null>(null);
  let storageNote = $state<string | null>(null);
  let error = $state<string | null>(null);
  let pathInput = $state('');
  let browserOpen = $state(true);

  // The live view, when the pane is in edit mode. A save asks git again and hands
  // the new marks to this view; a tab switch builds a new one with its own marks.
  let view: EditorView | null = null;

  /** Where the open tabs and their unsaved buffers survive a reload. */
  const TABS_KEY = 'acpilot.editor.tabs';

  // A draft is written a moment after the last keystroke, not on every one: a large
  // buffer serialised per key is a tax on typing, and a browser may drop an unload
  // handler, which is why the timer also carries the last change.
  let persistTimer: ReturnType<typeof setTimeout> | null = null;
  let pendingStore: ReturnType<typeof tabStorage> | null = null;

  // True until the stored list has been read: the writing effect must not save an
  // empty list over the tabs it is about to restore.
  let restoring = $state(true);

  // One character beside the line number. A deletion names the line above the gap,
  // which is where the daemon counts it and where the TUI draws it. The field holds
  // positions, so an edit moves a mark with its line instead of leaving it behind.
  const gitGutter = gutter({
    class: 'cm-git-gutter',
    markers: (view) => view.state.field(gitMarks),
  });

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
      backgroundImage: 'var(--acpilot-ruler-image, none)',
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
    '.cm-git-gutter': { width: '0.9rem' },
    '.cm-git-mark': {
      fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
      fontSize: '0.75rem',
      padding: '0 0.15rem',
    },
    '.cm-git-added': { color: 'var(--success)' },
    '.cm-git-modified': { color: 'var(--warning)' },
    '.cm-git-deleted': { color: 'var(--danger)' },
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

  /**
   * Open a path, or bring its tab forward.
   *
   * A tab already holds a buffer that may be dirty, so opening a path again is a
   * switch rather than a re-read: a second read would silently replace edits the
   * person can see.
   */
  async function open(path: string): Promise<void> {
    const next = normalisePath(path);
    if (!next) {
      return;
    }
    const existing = tabs.findIndex((candidate) => candidate.path === next);
    if (existing >= 0) {
      active = existing;
      pathInput = next;
      return;
    }
    opening = true;
    error = null;
    note = null;
    try {
      const read = await readFile(next, token);
      tabs.push({ path: read.path, file: read, draft: read.text, mode: 'edit', generation: 1 });
      active = tabs.length - 1;
      pathInput = read.path;
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

  /** Start a file that does not exist yet: an empty buffer and a path to create. */
  function addDraft(event: Event): void {
    event.preventDefault();
    const path = normalisePath(newPath);
    if (!path) {
      return;
    }
    const existing = tabs.findIndex((candidate) => candidate.path === path);
    if (existing >= 0) {
      active = existing;
    } else {
      tabs.push({ path, file: null, draft: '', mode: 'edit', generation: 1 });
      active = tabs.length - 1;
    }
    creating = false;
    newPath = '';
    error = null;
    note = `New ${path} — its first save creates it.`;
    // The parent may not be listed yet; the save reports a missing directory in
    // the daemon's own words, so a browse that fails here is not an error.
    void browse(parentOf(path));
  }

  function beginCreate(): void {
    creating = true;
    newPath = dir ? `${dir}/` : '';
  }

  /** Remove a tab row; the caller has already asked, if a question was needed. */
  function dropTab(index: number): void {
    tabs.splice(index, 1);
    if (index < active) {
      active -= 1;
    } else if (active > tabs.length - 1) {
      active = Math.max(0, tabs.length - 1);
    }
    renaming = false;
  }

  function close(index: number): void {
    const target = tabs[index];
    if (!target) {
      return;
    }
    if (tabDirty(target)) {
      const ask = globalThis.confirm;
      if (typeof ask === 'function' && !ask(`Close ${target.path} and discard unsaved changes?`)) {
        return;
      }
    }
    dropTab(index);
  }

  /** Ask for a new path, for the open file or a file in the listing. */
  function beginRename(path = tab?.path): void {
    if (!path) {
      return;
    }
    renaming = true;
    renameFrom = path;
    renamePath = path;
  }

  async function confirmRename(event: Event): Promise<void> {
    event.preventDefault();
    const from = renameFrom;
    const to = normalisePath(renamePath);
    renaming = false;
    renameFrom = null;
    if (!from || !to || to === from) {
      return;
    }
    moving = true;
    error = null;
    note = null;
    try {
      // The daemon moves the file and answers with its new path and token. A dirty
      // buffer stays dirty: the move did not change the text on screen.
      const moved = await renameFile(from, to, token);
      const followed = tabs.find((open) => open.path === from);
      if (followed) {
        followed.path = moved.path;
        followed.file = moved;
        // Only the visible view can take new gutter marks; another tab's marks are
        // read when its own view is built.
        if (followed === tab) {
          showMarks(moved.marks ?? []);
          pathInput = moved.path;
        }
      }
      note = `Renamed ${from} to ${moved.path}`;
      void browse(parentOf(moved.path));
    } catch (failure) {
      error = describe(failure);
    } finally {
      moving = false;
    }
  }

  /** Delete a file, after asking, and drop the tab that pointed at it. */
  async function remove(path = tab?.path): Promise<void> {
    if (!path) {
      return;
    }
    const index = tabs.findIndex((candidate) => candidate.path === path);
    const open = index >= 0 ? tabs[index] : null;
    const ask = globalThis.confirm;
    const question =
      open && tabDirty(open)
        ? `Delete ${path} and discard its unsaved changes?`
        : `Delete ${path}?`;
    if (typeof ask === 'function' && !ask(question)) {
      return;
    }
    moving = true;
    error = null;
    note = null;
    try {
      await deleteFile(path, token);
      const current = tabs.findIndex((candidate) => candidate.path === path);
      if (current >= 0) {
        dropTab(current);
      }
      note = `Deleted ${path}`;
      void browse(dir);
    } catch (failure) {
      error = describe(failure);
    } finally {
      moving = false;
    }
  }

  /** Hand the gutter a new set of marks without replacing the document. */
  function showMarks(marks: LineMark[]): void {
    view?.dispatch({
      effects: setGitMarks.of(marks.map((mark) => ({ line: mark.line, change: mark.change }))),
    });
  }

  async function save(): Promise<void> {
    const open = tab;
    if (!open || saving) {
      return;
    }
    if (open.file !== null && open.draft === open.file.text) {
      return;
    }
    saving = true;
    error = null;
    note = null;
    try {
      // A tab with no file is a create: the daemon reads an absent `if_match` as
      // "this path is new" and refuses a file that is already there.
      let text = open.draft;
      let formatter: string | null = null;
      if (preferences?.format_on_save) {
        try {
          const answer = await formatFile(open.path, text, token);
          if (answer.formatted) {
            text = answer.text;
            formatter = answer.server ?? 'the language server';
          }
        } catch {
          // A formatter that cannot be reached is not a failed save: write the
          // buffer the person typed rather than losing the edit to an enhancement.
        }
      }
      const written =
        open.file === null
          ? await createFile(open.path, text, token)
          : await writeFile(open.path, text, open.file.etag, token);
      if (view && view.state.doc.toString() !== written.text) {
        // The formatter rewrote the buffer: draw what was written rather than
        // leaving the old text looking unsaved beside a file that already has it.
        view.dispatch({
          changes: { from: 0, to: view.state.doc.length, insert: written.text },
        });
      }
      open.file = written;
      open.draft = written.text;
      open.path = written.path;
      // The write answered with the marks this text earns; draw them now rather
      // than rebuilding the view and moving the caret to apply them.
      showMarks(written.marks ?? []);
      note = formatter ? `Saved ${written.path} · formatted by ${formatter}` : `Saved ${written.path}`;
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
    const open = tab;
    if (!open || open.file === null) {
      return;
    }
    opening = true;
    error = null;
    note = null;
    try {
      const read = await readFile(open.path, token);
      open.file = read;
      open.draft = read.text;
      open.mode = 'edit';
      open.generation += 1;
      note = 'Reloaded from disk. Unsaved edits were discarded.';
    } catch (failure) {
      error = describe(failure);
    } finally {
      opening = false;
    }
  }

  /**
   * Reopen the tabs the last visit left open, with the buffers it kept for them.
   *
   * A path that has since gone is still worth a tab when it has a draft: the draft
   * is the only copy of text that was never saved, so it comes back as a file to
   * create. Without one, a path pointing at nothing would be a click that can never
   * work.
   */
  async function restoreTabs(): Promise<void> {
    let saved: ReturnType<typeof storedTabs> = null;
    try {
      saved = storedTabs(localStorage.getItem(TABS_KEY));
    } catch {
      // Storage can be denied; the page then starts with no tabs, as it did before.
    }
    let restored = 0;
    if (saved) {
      for (const path of saved.paths) {
        const kept = saved.drafts[path];
        // No etag is a tab whose first save was never made: it comes back as the
        // create it was, so a file that appeared there meanwhile is a `409` rather
        // than a save over somebody's text.
        if (kept && kept.etag === null) {
          tabs.push({ path, file: null, draft: kept.text, mode: 'edit', generation: 1 });
          restored += 1;
          continue;
        }
        try {
          const read = await readFile(path, token);
          // The stored token, not the fresh one: if the file changed while the page
          // was closed, the save must be refused so the newer text is not replaced
          // unseen.
          const file = kept?.etag ? { ...read, etag: kept.etag } : read;
          const draft = kept ? kept.text : read.text;
          tabs.push({ path: read.path, file, draft, mode: 'edit', generation: 1 });
          if (kept && draft !== read.text) {
            restored += 1;
          }
        } catch {
          // Gone or refused with no draft: the rest of the tabs are still worth it.
        }
      }
      const index = tabs.findIndex((open) => open.path === saved.active);
      if (index >= 0) {
        active = index;
        pathInput = tabs[index]?.path ?? '';
      }
    }
    restoring = false;
    if (restored > 0) {
      note = `${restored} unsaved ${restored === 1 ? 'draft was' : 'drafts were'} restored; nothing was saved.`;
    }
    void browse('');
  }

  function storeTabs(value: ReturnType<typeof tabStorage>): void {
    try {
      localStorage.setItem(TABS_KEY, JSON.stringify(value));
      storageNote = null;
    } catch {
      // A full or denied store costs the drafts, not the tabs: paths are a fraction
      // of the size, and losing the list because one buffer was large would be worse.
      try {
        localStorage.setItem(
          TABS_KEY,
          JSON.stringify({ paths: value.paths, active: value.active }),
        );
        storageNote =
          'Drafts are too large to keep for the next visit; the open paths are still remembered.';
      } catch {
        storageNote = 'The browser refused to remember the open tabs.';
      }
    }
  }

  // A write per keystroke would serialise the whole buffer on the typing path; one
  // shortly after the last change is enough for a reload to find the text.
  function scheduleStore(value: ReturnType<typeof tabStorage>): void {
    pendingStore = value;
    if (persistTimer !== null) {
      clearTimeout(persistTimer);
    }
    persistTimer = setTimeout(() => {
      persistTimer = null;
      const next = pendingStore;
      pendingStore = null;
      if (next) {
        storeTabs(next);
      }
    }, 250);
  }

  /** Write the last change now: the page is going away and the timer with it. */
  function flushStore(): void {
    if (persistTimer !== null) {
      clearTimeout(persistTimer);
      persistTimer = null;
    }
    const next = pendingStore;
    pendingStore = null;
    if (next) {
      storeTabs(next);
    }
  }

  $effect(() => {
    const value = tabStorage(tabs, tab?.path ?? null);
    if (restoring) {
      return;
    }
    scheduleStore(value);
  });

  // The editor is built once per loaded document. Reading `draft` and `file`
  // outside the effect's dependencies is deliberate: a keystroke must not rebuild
  // the view, and a save must not move the caret.
  $effect(() => {
    const target = hostEl;
    const open = tab;
    const editing = open?.mode === 'edit';
    const generation = open?.generation ?? 0;
    if (!target || !open || !editing) {
      return;
    }
    void generation;
    const doc = untrack(() => open.draft);
    const path = untrack(() => open.path);
    const marks = untrack(() => open.file?.marks ?? []);
    const created = new EditorView({
      state: EditorState.create({
        doc,
        extensions: [
          basicSetup,
          theme,
          extensionFor(languageFor(path), path),
          gitMarksFor(marks),
          gitGutter,
          wrap.of(wrapExtension(untrack(() => preferences?.wrap ?? false))),
          EditorView.updateListener.of((update) => {
            if (update.docChanged) {
              open.draft = update.state.doc.toString();
            }
          }),
        ],
      }),
      parent: target,
    });
    view = created;
    created.focus();
    return () => {
      if (view === created) {
        view = null;
      }
      created.destroy();
    };
  });

  // The diff is `@codemirror/merge` over the two documents the page already has:
  // the version on disk and the buffer. Both sides are read-only — the buffer is
  // edited in the other mode, and an edit made in a side-by-side view would be an
  // edit to a document that is not the one being saved.
  $effect(() => {
    const target = diffEl;
    const open = tab;
    const comparing = open?.mode === 'diff' && open.file !== null;
    if (!target || !open || !comparing) {
      return;
    }
    const before = untrack(() => open.file?.text ?? '');
    const after = untrack(() => open.draft);
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

  // A saved preference reaches an open tab without a reload. Wrap is a
  // compartment because CodeMirror extensions are otherwise fixed at creation;
  // rulers are a background layer measured in `ch`, so they follow the font.
  $effect(() => {
    const on = preferences?.wrap ?? false;
    view?.dispatch({ effects: wrap.reconfigure(wrapExtension(on)) });
    const rulers = preferences?.rulers ?? [];
    if (!hostEl) {
      return;
    }
    const image = rulers
      .map(
        (column) =>
          `linear-gradient(to right, transparent calc(${column}ch - 1px), var(--border) calc(${column}ch - 1px), var(--border) ${column}ch, transparent ${column}ch)`,
      )
      .join(', ');
    hostEl.style.setProperty('--acpilot-ruler-image', image || 'none');
  });

  onMount(() => {
    void restoreTabs();
    if (profile) {
      void preferencesFor(profile, token)
        .then((loaded) => {
          preferences = loaded;
        })
        // Preferences are an enhancement: an older daemon or a failed read leaves
        // the editor usable with its defaults rather than blocking the files.
        .catch(() => {});
    }
    // The debounce is short, but a person can reload inside it; unmounting flushes.
    return () => flushStore();
  });
</script>

<section class="editor" aria-label="Editor">
  {#if tabs.length}
    <div class="editor-tabs" role="tablist" aria-label="Open files">
      {#each tabs as open, index (open.path)}
        <div class="editor-tab" data-active={index === active}>
          <button
            type="button"
            class="tab-name"
            role="tab"
            aria-selected={index === active}
            title={open.path}
            onclick={() => (active = index)}
          >
            {basename(open.path) || open.path}
          </button>
          {#if tabDirty(open)}
            <span class="dirty" title="Unsaved changes" aria-label="Unsaved changes">●</span>
          {/if}
          <button
            type="button"
            class="tab-close"
            aria-label={`Close ${open.path}`}
            title={`Close ${open.path}`}
            onclick={() => close(index)}
          >
            ×
          </button>
        </div>
      {/each}
    </div>
  {/if}

  <div class="editor-bar">
    <button
      type="button"
      class="link"
      aria-expanded={browserOpen}
      onclick={() => (browserOpen = !browserOpen)}
    >
      Files
    </button>
    <code class="editor-path">{tab?.path ?? 'no file open'}</code>
    {#if dirty}
      <span class="dirty" title="Unsaved changes" aria-label="Unsaved changes">●</span>
    {/if}
    <span class="editor-spacer"></span>
    {#if tab}
      {#if tab.file}
        <button
          type="button"
          aria-pressed={tab.mode === 'diff'}
          onclick={() => {
            if (tab) {
              tab.mode = tab.mode === 'edit' ? 'diff' : 'edit';
            }
          }}
        >
          {tab.mode === 'diff' ? 'Edit' : 'Diff'}
        </button>
      {/if}
      <button
        type="button"
        onclick={() => void reload()}
        disabled={opening || saving || tab.file === null}
      >
        Reload
      </button>
      {#if tab.file}
        <button type="button" onclick={() => beginRename()} disabled={moving || saving}>Rename</button>
        <button type="button" onclick={() => void remove()} disabled={moving || saving}>
          Delete
        </button>
      {/if}
      <button
        type="button"
        class="primary"
        onclick={() => void save()}
        disabled={saving || !dirty}
      >
        {saving ? 'Saving…' : 'Save'}
      </button>
    {/if}
  </div>

  {#if renaming}
    <form class="file-rename" onsubmit={confirmRename}>
      <label for="rename-path" class="visually-hidden">New path</label>
      <input
        id="rename-path"
        bind:value={renamePath}
        spellcheck="false"
        autocomplete="off"
      />
      <button type="submit" disabled={moving}>Rename</button>
      <button type="button" onclick={() => (renaming = false)}>Cancel</button>
    </form>
  {/if}

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
        <button type="button" onclick={beginCreate}>New file</button>
      </form>
      {#if creating}
        <form class="file-new-form" onsubmit={addDraft}>
          <label for="new-file-path" class="visually-hidden">Path for the new file</label>
          <input
            id="new-file-path"
            bind:value={newPath}
            placeholder="path/to/new-file"
            spellcheck="false"
            autocomplete="off"
          />
          <button type="submit">Create</button>
          <button type="button" onclick={() => (creating = false)}>Cancel</button>
        </form>
      {/if}
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
              {#if !row.is_dir}
                <span class="file-row-actions">
                  <button
                    type="button"
                    aria-label={`Rename ${row.path}`}
                    title={`Rename ${row.path}`}
                    onclick={() => beginRename(row.path)}
                    disabled={moving || saving}
                  >
                    Rename
                  </button>
                  <button
                    type="button"
                    aria-label={`Delete ${row.path}`}
                    title={`Delete ${row.path}`}
                    onclick={() => void remove(row.path)}
                    disabled={moving || saving}
                  >
                    Delete
                  </button>
                </span>
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
  {#if storageNote}<p class="note" role="status">{storageNote}</p>{/if}

  {#if tab}
    {#if tab.mode === 'diff' && tab.file}
      <div class="diff-host" bind:this={diffEl} aria-label="Diff against the version on disk"></div>
      <p class="note">
        Left is the version on disk, right is the buffer. Both are read-only here;
        switch to Edit to change the buffer.
      </p>
    {:else}
      <div class="editor-host" bind:this={hostEl}></div>
    {/if}
    {#if tab.file?.marks?.length}
      <p class="note">
        {tab.file.marks.length}
        {tab.file.marks.length === 1 ? 'line differs' : 'lines differ'} from the last commit.
        The gutter marks them: <span class="file-status" data-change="added">+</span> added,
        <span class="file-status" data-change="modified">~</span> changed,
        <span class="file-status" data-change="deleted">-</span> removed.
      </p>
    {/if}
  {:else}
    <p class="note">
      Open a file from the list, type its path, or start a new one with <strong>New file</strong>.
      A save carries the version the file was read at (<code>if_match</code>), so a file
      somebody else changed is refused rather than overwritten; a new file's first save
      sends no token and the daemon creates it.
    </p>
  {/if}
</section>
