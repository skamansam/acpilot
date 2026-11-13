# The TUI shell: a design pass

**Status:** designed, drawn and wired where the table below says so. `acpilot tui
--demo` is the screen this note describes. Everything in it that has a backend
works, and everything else says which task would give it one.

## Why a design pass at all

The interface was being decided one pane at a time, in the order the backend
happened to land. That produces a screen nobody chose. So the shell was drawn
first, end to end, on seeded data. It shows the panes that exist, the panes that
do not, and one command palette listing all of them. It also shows a notice when
a row has no implementation behind it. The point is not the mock. It is that the
layout can be argued about with something that answers the keyboard.

The desktop app renders the same world when it gets its shell (P10), from the
same crate. So the two designs cannot drift apart in the first week.

## Run it

```bash
cargo run -p acpilot-cli -- tui --demo     # the design pass, no daemon
cargo run -p acpilot-cli -- tui            # the real thing
```

Nothing is read from disk and nothing is spawned. `Tab` moves between panes, `?`
lists the focused pane's keys, and `Ctrl-P` opens the palette. `Ctrl-T` attaches
the terminal pane, or starts a shell in the session's workspace when an agent has
not opened one. On a remote workspace, that is a shell on that host.

The picker also lists panes no session owns. An agent's shell whose session the
daemon could not map says `no session`. It names the workspace it is in and the
host it runs on. `n` in the picker starts a shell of the session's own.
Attaching to such a pane leaves that address on the status line while it has the
keys. The picker is closed by the time commands go into it.

## Layout

```
 Acpilot  File   View   Session   Help              f9 menus · night-flight
┌ Files ──────────────┬ app.rs  rust ────────────────┬ Sessions ───────────────────┐
│ ▸ Cargo.toml     M  │ crates/acpilot-tui/src/app.rs│ ▸ wire the terminal  busy   │
│ ▾ crates            │   1  use acpilot_proto…      │   flaky retry test   idle   │
│   ▾ acpilot-tui     │   2                          │   review the pane    closed │
│       Cargo.toml    │   3  const SCROLLBACK…       ├─────────────────────────────┤
│     ▾ src           │   4                          │ mock · wire the terminal    │
│         app.rs   M  │  …                           │ Attach the TUI to a         │
│         ui.rs    M  │                              │ terminal an agent opened…   │
├─────────────────────┴──────────────────────────────┼─────────────────────────────┤
│                                                     │ Prompt · Enter sends       │
└─────────────────────────────────────────────────────┴─────────────────────────────┘
 ● profile · focus · 17:9 · status                                       daemon
 ● lmstudio · qwen2.5-coder-7b  21 620 tok · $0.000 │ ● anthropic … 7 060 tok
```

A menu bar, three panels (**files, text, agents**) and two footer rows. The TUI,
the desktop app and the browser show the same three panels. That sameness is the
point of the split.

- **The menu bar** is one row of chrome, not a modal. The titles are always
  drawn, so the commands are discoverable without a key list. `F9` is only how
  the keyboard gets into a bar whose rows run the same code paths as the chords
  and the palette. Every row prints the chord that would have run it,
  right-aligned, so the bar teaches the keys as well as the list. A row whose
  action exists only in a pane prints nothing, because a chord in the bar
  promises it works from anywhere. The underlined letter in a title is `Alt`'s:
  `Alt-F`, `Alt-V`, `Alt-S` and `Alt-H` open a menu with its list showing. The
  pointer answers the same targets. Click a title to open, close or switch,
  slide along the bar to another, and click a row to run it. This is the one
  place the mouse may *choose* something, because a bar of labels is what a
  pointer is better at than a key. Help holds About, which names the build in
  the same four rows the desktop's About prints. - **Files** (26 columns) is the
  workspace tree, flattened: indentation is data, not a walk. It folds away
  below 100 columns rather than squeezing the editor. - **Text** is a tab strip
  and the buffer. It has a gutter for line numbers and git markers, the current
  line highlighted, and the cursor at the position the status line reports. The
  buffer is a real document (P6-4). The rows are built from the viewport, so it
  draws only the lines that fit and only the cells the horizontal scroll left.
  Their colours are the highlighter's tokens resolved through the theme. -
  **Agents** is the session list, the transcript and the composer, in that
  order. They answer what you can open, what it said, and what you want to say.
  The transcript renders an assistant reply's markdown (headings, lists, quotes,
  fenced code, inline code, bold, italic, links). It folds every tool call to
  one row whose status moves in place. `Enter` under the copy cursor opens the
  result. While the open session is busy, a cursor sits at the end of the last
  line and the title says `streaming`. The composer is multiline, and `@` opens
  a path completion the daemon walks. - The **status line** is connection,
  profile, focused pane, cursor, last message, and the daemon address when there
  is room for it. - The **stats bar** is one row per integration: provider,
  model, tokens both ways, money, latency. A provider that stopped answering is
  drawn hollow, not hidden.

Overlays, back to front: the agent picker, the terminal picker, the key list and
the palette. Then the content search's jump list, the source-control pane, the
permission card, the diff review that answers it, the commit composer and the
toast.

The assistant's overlays and panes are absent at launch. The window opens in the
editor, and only **Session → Select Agent…** (or the palette's **Select agent…**)
brings the sessions, transcript and composer into being. Closing them leaves the
document open.

## What is real, and what is not

| Pane | Today | Task |
| --- | --- | --- |
| Menu bar, About | real, and every row is an existing path | — |
| Sessions, transcript, composer | real, over the daemon. The transcript is a pane (`Tab`, `F3`) with scroll, find, copy, markdown, foldable tool calls and a streaming cursor. The composer is multiline with `@path` completion from the daemon's walk | — |
| Permission card | real: the request's own title, kind and session, a preview of the write when it is one, and one row per option the request actually sent (`y` once, `a` always for the session, `n` deny). `Tab`/`Shift-Tab` move between questions that are waiting, and `Esc` answers later. The card is drawn over the transcript while it streams, because a turn in flight is exactly when an agent asks | — |
| Diff review | real: `Ctrl-D` opens it over the card, unified or two columns, a decision per hunk, answered through the same permission path. `Esc` closes it and leaves the question pending | — |
| Source control | real: `F6` opens a modal pane over the shell, fed by `GET /v1/git/status` and both halves of `GET /v1/git/diff`. The changed paths are on the left. The selected file's **Unstaged** and **Staged** sections are on the right, one hunk at a time, with the chosen unstaged hunk marked. `j`/`k` choose a path and `h`/`l` choose an unstaged hunk. `Shift-S` stages that one hunk through `POST /v1/git/stage-hunk`, `PgUp`/`PgDn` scroll the diff, and `s`/`u` move the selected path in or out of the index through `POST /v1/git/stage` and `/unstage`. `r` asks the daemon again. `d` arms `POST /v1/git/discard` (working tree back to the index, staged changes kept) and `x` arms `POST /v1/git/checkout` (both halves back to `HEAD`). Both wait for `y`, and `n` or `Esc` cancels. Checkout is refused before the first commit and for an untracked path, where discard is the action that removes it. `--demo` uses its one seeded changed file and refuses the writes | — |
| Commit composer | real: `c` opens it over the source pane, and `Ctrl-G` asks the open session for a draft through `POST /v1/git/commit-message`. The message is editable with arrows, `Home`/`End`, `Backspace`/`Delete` and `Enter`. `Ctrl-S` asks, `y` sends the whole message to `POST /v1/git/commit`, and `Esc` closes without writing. The route refuses an empty message and an empty index before git runs, and maps a hook's refusal to `409` | — |
| Terminal pane | real attach, scrollback, find, copy mode, picker | P6-10 (done in the TUI) |
| Command palette, which-key, toast | real | — |
| Theme | real: every colour is a token from the theme the settings name (`theme_tui` over `theme`), resolved for the terminal, the desktop app and the page the daemon serves. A running client watches the settings files and recolours on save. `theme_follow_terminal` solves the terminal's palette from the terminal's own `OSC 11`/`10` pair instead of naming one | `prefs.changed` over the daemon's event stream (P4-7) |
| File tree | real: the daemon's `GET /v1/files/list`, one directory per row opened and spliced into the flat list with a depth. `Enter` reads a file through `GET /v1/files` and opens it in the editor. `H` asks for the rows an ignore rule hides (`include_ignored`), which arrive marked and are drawn greyed. `/` searches the workspace by path through `GET /v1/files/search`, which ranks a substring or initials and walks with the same ignore rules, so a find cannot see what the tree cannot. `Ctrl-Shift-F` (or the palette's **Search file contents**) opens a jump list fed by `GET /v1/files/grep`, and each row carries the line with the words that matched marked (folded the way the daemon folded them). `Enter` opens the file at that line. A row's letter is the daemon's, not the client's: the listing marks each path with what git says changed (`M`, `A`, `?`, `D`, `!`), and gives a directory the most urgent change beneath it. A workspace that is not a repository, or a host without `git`, shows none rather than claiming everything is committed | P6.5-4 opens that pane with `F6`, drawing the changed paths and the selected file's diff |
| Editor | real: a `Document` drawn through `acpilot-text::pane` — virtualised rows, horizontal scroll, gutter with numbers and git markers parsed from the diff, current line, bracket matching, the daemon's language-server problems as gutter letters refreshed once the buffer has been still for 480 ms, `/` find with smart case, `Ctrl-G` go-to-line, a click that lands a caret, a drag that selects and a `y` that copies, and a wheel that scrolls the view. It is editable: `i`/`a`/`I`/`A`/`o`/`O` enter insert mode, `u` and `Ctrl-Z` undo, `Ctrl-Y` redoes, and the title names the mode the keys are in. `v` enters visual mode, where motions extend the run and `y` copies it, `d`/`x` delete it, and `Esc` leaves. `:` opens a command line with `:w`, `:q`/`:q!`, `:N` and `:e path`. A run of Up/Down keeps the column it started from across a short line, and Shift-click extends a selection. The buffer is whatever the tree opened, or the seeded one in `--demo`, and a `+` on the tab says the text is not the file's. Its normal and visual keys are the keymap's, so `keymap.toml` can move them | P7-10 (saving) |
| Stats bar | real: one row per integration from `GET /v1/stats`, drawn by `acpilot-stats` (the crate the desktop app and the page also use). A context and a credit track per row, an indeterminate track when nobody reported a denominator, the fill coloured by source rather than by level, and `F4` for where each figure came from. The daemon's rows are a roster, plus an estimated context for an agent with a conversation behind it and no window unless the agent names one. So a live bar before any session is rows that say so, and the seeded rows in `--demo` exercise all four sources | P4.5-12 (provider rows and balances) |
| Outline panel | real: `acpilot_text::outline` collects the open buffer's definitions from the parse tree. `Show outline` in the Panels menu puts it up, `j`/`k` choose and `Enter` puts the caret on the definition's line. It re-reads when the text has changed since the last read, because `App::tick` compares `Document::revision`. A burst of typing therefore costs one parse a frame rather than one per keystroke | — |
| Project map panel | real: the agent writes `.acpilot/project-map.toml` — `updated`, then one `[[areas]]` with a `name`, the `path` it owns, a `summary` and the `depends_on` it leans on — and `GET /v1/project/map` parses it where the workspace is. The panel lists the areas in the order the agent wrote them, marks the area the open file belongs to (deepest path wins, compared by segments so `crates/tui` is not the parent of `crates/tui-tools`) and opens its summary and edges. `j`/`k` choose, `Enter` puts a summary on the status line, and `r` re-reads a map the agent has just rewritten. A workspace with no map says so rather than showing an empty list | a regeneration prompt over the tree and the README. The map is a document a person can correct, so the panel reads it rather than writing it |
| Wiki panel | planned, not built: linked pages per subsystem in the workspace, read in a pane with a page list and a jump. A link to an unwritten page is the prompt to write it | panel work in `TODO.md` |
| References panel | real: `Shift-F12` (or View → **Find References**) reads the identifier under the caret and asks `GET /v1/files/references`, which walks the workspace like the content search and keeps only the leaves `acpilot_text::references` parses as names. A comment, a string and a `.md` are not uses. Rows carry `path:line` and the line with the name marked. `Enter` opens the file at the use, the panel stays up, and `Esc` gives the keys back. The list follows the caret: while the buffer holds the keys, `App::tick` reads the word under it and asks about a name that is still there on the next frame. So moving through a file re-lists without a second chord, and typing an identifier costs no walks. A changed `Document::revision` is a new question too, so a use typed while the caret rests on the name appears without moving off it. Between two words the list clears, because a word the caret is beside is not a word it is on. It does not follow while the keys are in another pane, so opening a use does not re-ask about whatever the caret lands on. The list is also asked of a language server now: with a caret (`path` and `offset`) the daemon sends `textDocument/references` to `rust-analyzer`, `pyright-langserver` or `typescript-language-server` when one is installed, so the list is scoped rather than spelled. The grammar search is the fallback, and `--no-default-features` drops it and the 2.2 MB of parser tables with it | — |

## Keys

| Where | Keys |
| --- | --- |
| anywhere | `Tab` next pane · `?` keys · `Ctrl-P` palette · `F9` menus · `Alt`+underlined letter a menu · `F6` source control · `Ctrl-T` terminal, or a new shell when the session has none · `F3` read the transcript · `Ctrl-←`/`Ctrl-→` narrower/wider · drag a divider · `Ctrl-R` refresh · `Ctrl-C` quit |
| terminal picker | `j`/`k` choose · `Enter` attach · `n` start a shell in the session's workspace · `Esc`/`q` close |
| agent picker | `j`/`k` choose · `Enter` chooses the agent and reveals the assistant · `Esc`/`q` close |
| menu bar | `←`/`→` titles · `↓`/`Enter` open · `↑`/`↓` rows · `Enter` run · `Esc` back one level · `Alt`+letter another menu · click and hover work too |
| transcript | the copy keys below, because the keyboard being there is what reading it means · `Enter` folds or opens the tool call under the cursor |
| files | `j`/`k` move · `h`/`l` collapse/expand · `Enter` open · `/` find a file across the workspace — the daemon's walk, so the ignore rules apply; `↑`/`↓` choose, `Enter` opens, `Esc` puts the tree back · `H` shows the paths an ignore rule hides, greyed and italic, asking every open directory again so the tree keeps its shape · a click focuses the tree on that row · `Ctrl-R` re-lists, which is how a change to `tree_respect_gitignore` arrives · the tree is the daemon's listing, so it hides what the workspace's `.gitignore` files hide unless that preference or `H` says otherwise |
| editor | `h`/`j`/`k`/`l` cursor in a column that Up/Down remembers · `0`/`$` start/end of line · `PgUp`/`PgDn` a page · `/` or `Ctrl-F` find · `n`/`N` next/previous match · `Ctrl-G` line[:column] · `v` visual selection · `y` copy the selection · `Esc` clear it · `Ctrl-S` or `:w` save · `:` command line (`:q` refuses unsaved text, `:q!` discards) · `i`/`a` insert at the caret or after it · `I`/`A` at the line's start or end · `o`/`O` a line below or above · `u`/`Ctrl-Z` undo · `Ctrl-Y` redo · `x` delete a character · `F12` definition · click puts the caret · Shift-click extends the selection · drag selects · wheel scrolls the view |
| visual mode | motions (`h`/`j`/`k`/`l`, arrows, `0`/`$`, `PgUp`/`PgDn`) extend the run · `y` copy · `d`/`x` delete · `:` a command · `v`/`Esc`/click leave |
| insert mode | any key is text · `Enter` a line · `Backspace`/`Delete` a character · `Tab`/`Shift-Tab` indent/outdent · arrows and `Home`/`End` move · `Ctrl-Z` undo · `Esc` normal mode |
| sessions | `j`/`k` or the arrows choose · `Enter` open · `i` compose · `n` new · `d` end · `c` cancel · a click focuses the list on that session and opens it |
| prompt | `Enter` send · `Alt-Enter`/`Shift-Enter`/`Ctrl-Enter` a line · `@` complete a path · arrows and `Home`/`End` move the caret · `Backspace`/`Delete` edit beside it · `Esc` back to the list |
| terminal | any key is a byte · `F2` copy mode · drag selects · `Ctrl-T` leave |
| copy mode | `j`/`k`/`h`/`l` move · `Ctrl-U`/`Ctrl-D` page · `g`/`G` top/live · `/` find (smart-case) · `n`/`N` next match · `v` select · `V` whole lines · `y` copy · `Esc` leaves |
| source control | `j`/`k` changed file · `h`/`l` unstaged hunk · `Shift-S` stage hunk · `PgUp`/`PgDn` diff · `s`/`u` stage/unstage path · `d`/`x` discard/checkout · `c` compose · `g` generate and compose · `r` re-read · `Esc`/`q` close |
| commit composer | printable keys and `Enter` edit · arrows/`Home`/`End` move · `Backspace`/`Delete` edit · `Ctrl-G` generate from the open session · `Ctrl-S` ask to commit · `y`/`Enter` confirm · `n`/`Esc` cancel · `Esc` close when not committing |
| review | `j`/`k` hunk · `a` accept · `x` reject · `A`/`X` all · `s` split/inline · `y` answer · `n` decline · `Esc` or `Ctrl-D` close |
| permission card | `y` allow once · `a` allow always for this session · `n` deny, only for the options the request sent · `Tab`/`Shift-Tab` the next/previous question · `Esc` answer later, and **Session → Answer Permission…** brings the card back |

Every key in the `anywhere`, files, editor, visual, sessions, prompt, copy-mode
and permission rows comes from `crates/acpilot-tui/src/keymap.rs`, not from a
`match` arm. That is what makes the file below possible. The table has a context
for each of those panes. So `expand = ["e"]` moves the tree's expand verb
without touching the editor's `l`. And `permission-deny = ["x"]` moves the
answer without touching the composer's `x`.

`keymap.toml` beside `config.toml` chooses a preset and rebinds any of those
verbs by name. `preset = "vim"` is the table above. `preset = "vscode"` moves
with the arrows and `Home`/`End` instead of `h`/`j`/`k`/`l` and `0`/`$`. It puts
the palette on `Ctrl-Shift-P` as well as `Ctrl-P`. It also turns Vim mode off:
the editor opens in insert, and `Esc` clears a selection rather than entering
normal mode. Under `[keys]`, `palette = ["ctrl-p", "f1"]` replaces the keys for
that verb. An empty list unbinds it. And `move-down = ["down", "j", "ctrl-n"]`
is how a motion gains a third spelling.

A client refuses a name that is not a verb. It refuses a key that is not a key,
and one key bound to two verbs in one context. It names the word that was wrong.
The table is checked after the whole file is applied, so swapping two keys is
legal. A file that does not load leaves the client on the preset. It says so on
stderr before taking the screen, because no keys at all is worse than the keys
the preset describes. `?` lists the bindings in force, client keys first, so the
panel and the keys cannot disagree about what `Ctrl-P` does.

Two things are deliberately outside the table. Insert mode is a text field,
where every printable key is a character and `?` and `Tab` are text rather than
the app's keys. The composer follows the same rule: its printable keys are text.
Only its commands (`submit`, `newline`, `complete`, the motions and `cancel`)
are in the table. Also `F12`/`Shift-F12` are not bound. One of them raises a
task marker rather than doing something. A marker is not a verb a person should
be able to move onto a key that works. The menu bar, terminal picker,
source-control pane, commit composer and diff review still keep their own keys.
They are the next piece of this work rather than a second table.

`Tab` never lands in the terminal: a shell wants the byte. `Ctrl-T` enters and
leaves the pane, and the copy mode's own keys take over while it is up. The
transcript answers to the same copy keys. `F3` opens it, and it is the same
state, the same corners and the same yank, over the agent's lines instead of a
program's cells. Copy mode uses a function key on purpose: every printable key
and every `Ctrl` chord is a byte a program can want.

A find is case-insensitive until the query grows a capital. That is the behaviour
`less` and `vim` taught, and the only one that does not need a second key to toggle.
The transcript's copy mode answers to the same rule, so one `/` means one thing in
both panes.

Mouse reporting is on. The mouse does three things. It drags a divider, because
the borders between files, text and agents are where a person reaches for one.
It uses the menu bar, because a row of labels is a target a pointer can see and
hit. And in the editor it puts the caret where a click landed, and scrolls the
buffer under the wheel. A click is the fastest way to a caret, and a wheel is a
motion no key has. The terminal answers the same wheel by reading back through
its scrollback. Output that arrives meanwhile joins the history behind the
reader rather than pushing the line they were on down. So a build's error stays
under the cursor while the build is still printing, and a keystroke returns the
pane to the newest line. A command typed into a pane showing the old output
would print its answer out of sight. A title that says how far back the view is
keeps a scrolled pane from looking like a program that stopped printing. A click
on the numbers is furniture, not text: it falls through to the divider drag it
looks like. The wheel is pointed rather than aimed. It scrolls the pane under
the pointer without taking the keyboard and without moving the caret. That is
what makes reading a file with a trackpad the same file when you let go.

A drag across the terminal's output selects it. The press enters copy mode with
the anchor on the cell it landed on. A pointer has no key to press first, and a
person dragging across a shell's output means the output. Letting go keeps the
selection, and the status line names the two keys that finish it (`y` copies,
`Esc` leaves). A press that selected one cell or none ends the copy mode it
began. Copy mode is the one state where a keystroke goes nowhere near the
program. A person who clicked a shell and went back to typing must not be typing
at a pane that quietly stopped listening. A drag that leaves the pane selects to
the edge it left by rather than dropping the gesture. A query being typed owns
the keyboard, so a click does not move the selection out from under it.

The editor answers a drag the same way, with one difference that matters: it
yanks text, not cells. The selection is the `Document`'s own, an anchor and a
head in character offsets. So what `y` copies is the buffer's slice of that
range, tabs and wide glyphs included exactly once. An edit or a motion collapses
it. A selection that survived an edit would be a range pointing at text that
moved. A Shift-click extends the selection from its start rather than anchoring
where the last click landed. Visual mode (`v`) grows the same range with the
motion keys, so a run can be marked without a pointer. It is painted reversed,
the mark the terminal's copy mode already uses, so no theme can hide it behind
an accent colour. A click in the tree or the session list is also a focus move
and a choice. The tree selects the row under the pointer, and the session list
opens the session under it. A list is a column of targets, not a pane you click
to get into.

A drag does not stop at the pane's edge. A pointer cannot leave the screen. So a
drag held against the top or bottom row of the pane it began in scrolls the view
a line. It keeps the selection growing with it. That uses the loop's clock
rather than an event's, because a pointer held still sends nothing at all. It
uses a faster clock only while there is more view in the direction it is
pulling. The copy view keeps its cursor and both selection ends in lines of the
history rather than rows of the screen. So the drag can keep reading to the
oldest line a pane holds. The yank takes every line between the two ends even
when the screen shows only one of them. What was already marked stays marked.
The editor's selection is a buffer offset, and the terminal's anchor is a
history line. So a drag across a shell's output cannot quietly hand the cells it
covered to the lines above.

The widths the dividers were dragged to are remembered once, on the way out, in
`<data>/tui-layout.toml`, not in `config.toml`. A width is the client's memory
of itself rather than a setting anyone edits. A client that rewrote a
hand-edited settings file to remember a column would be editing something it
does not own. A launch that changed nothing writes nothing. A file that does not
parse gives the defaults, and a width no pane could use is clamped on the way
in.

Mouse reporting costs the terminal's own click-drag selection, so `--no-mouse`
turns it off and leaves the terminal's selection alone. Every mouse action here
has a key, and `Ctrl-←`/`Ctrl-→`, `F2` copy mode and a click's caret are
reachable without a pointer. What the flag does not do is pretend. With no
reporting there are no mouse events to ignore, so the panes never hear about a
pointer.

Every one of these has a keyboard path that is not the fallback but the first one. A
trackpad is not available over SSH, and the key is what gets pressed on a bad
connection.

## The frame budget

`crates/acpilot-tui/examples/frame.rs` draws the demo world into a `TestBackend`
at a size given on the command line. The world is the tree, the editor with its
own parse, the session list, the transcript, the composer and both footer rows.
It hands each frame's viewports back the way the event loop does. It prints the
mean, p95 and worst of the frames after ten warm-up ones:

```bash
cargo run --release -p acpilot-tui --example frame -- 200 60 500
```

At 200×60, which is the size P6-12 budgets for: **mean 0.48 ms, p95 0.73–0.76
ms, worst 2.7–3.5 ms** across runs. That is under a fifth of the 16.7 ms a 60
fps frame has, and it is before a tty is written to. A debug build of the same
run is 9.2 ms mean and 14.9 ms worst. That is worth knowing twice: unoptimised
code is nineteen times slower, and it is still inside the budget. What this
measures is the drawing (the layout, the cells, every styled span), and not the
daemon, the event poll or the OSC 52 write. Those are the loop's, and the loop's
cost at rest is the 120 ms tick.

`--budget <ms>` turns the run into the gate CI uses:

```bash
cargo run --release -p acpilot-tui --example frame -- 200 60 300 --budget 16.7
exit 0    # mean 0.45–0.63 ms, p95 0.61–1.01 ms, worst 2.3–2.5 ms
```

The numbers are printed either way, and the run exits 1 when the mean or the
95th percentile misses the budget. The worst frame is printed and not gated: a
shared runner produces one occasionally, and one scheduler hiccup is not a
regression. A sustained doubling is another matter, and that is what the mean
and the p95 see. It is exactly the bug the budget exists to name. `frame-budget`
in `.github/workflows/ci.yml` runs it in release and is one of `required`'s
jobs.

The gutter's markers are git's answer, not the buffer's. `acpilot-git` parses
the same patch the permission card shows. So the `~` beside a line and the hunk
in the review cannot disagree about what the agent did.

## When a feature is not built

A palette row is tagged with its task (`Open file · P6-6`). Running it raises a
toast naming the task and the intent ("planned · P6-6 · Open file", "not
implemented in this build"). The same sentence goes on the status line. The
notice belongs to the keystroke that raised it: the next key dismisses it.

That is deliberate over a modal. The interface is being reviewed, not used, and a
dialog that has to be dismissed would turn every press into two.

## The seeded world

`crates/acpilot-demo` is data only, in protocol types. It holds sessions (with a
transcript per session), agents (one of them missing), the tree, a buffer, a
diff and a pending permission. It also holds two terminals, two integrations and
the command catalogue. It has no behaviour, no renderer and no key handling,
which is what lets the desktop app import it without importing the TUI.

In the TUI the demo is a *worker* (`backend::spawn_demo`), not a special case in
the app. It speaks the same `Update` channel the daemon's worker does. So every
pane is the real pane, and the design cannot claim something the product will
not do. A prompt gets a scripted reply, and a session open replays that
session's stored events at 45 ms a chunk.

## Questions this pass is meant to settle

1. **The tree at 80 columns.** Folding it away is the current answer. A toggle
   pane or a full-width tree are the alternatives. 2. **The terminal's shape.**
   It takes the editor and the sidebar, keeping the tree. A tab beside the
   buffer, or a split under it, are both plausible. 3. **The diff.** Answered
   for a card-sized diff: the card draws it, and `Ctrl-D` opens the review over
   that card. That is still the place the decision is made, with room for the
   hunks and a decision on each. Whether a long review wants a pane of its own
   is open, and so is how much diff a card should hold before it scrolls. 4.
   **The transcript's scrollback.** Answered: `F3` gives it the terminal's copy
   mode, including a yank over OSC 52. It is a pane with a focus of its own, not
   a mode entered from any pane with one key. The rows are rendered rather than
   the source text (markdown, a folded row per tool call). So a yank copies what
   is on screen, and a find searches it. What is still open is the cost of that.
   Every frame builds the rows for the whole transcript, which is free at a
   session's length. It is the thing to virtualise if a conversation ever grows
   to a file's size. 5. **The stats bar's density.** Two rows of numbers in one
   line is already tight at 110 columns. 6. **The keymap file.** Answered for
   the client's keys and the editor's. The table is data, `keymap.toml` chooses
   a Vim or VS Code preset and rebinds any verb, and `?` lists what is in force.
   What is still open is the rest of the panes. The tree, the session list, the
   composer, the permission card and copy mode keep their own arms. A keymap
   layer shipped by an extension (P4.6-9) is also open. It stacks above the
   built-ins and below the person's own file. 7. **Palette, menus and
   which-key.** All three are here and all three run the same paths. The palette
   is the fast way, the bar is the discoverable one, and `?` is the one that
   teaches. Whether three survive contact with users, or the palette is enough
   once it lists keys, is still open. 8. **The dividers' keys.** Answered:
   `Ctrl-←`/`Ctrl-→` follow the focused pane for the keyboard. A mouse drag on
   the divider moves the same width, so neither gesture is the only way. What is
   still open is whether the drag should also persist: the widths reset on
   launch today.
## The model lab

`Session → Build an Agent in the Model Lab…` opens a page over the panes
(P4.5-8), not a fourth column: a recipe is about the model, and a settings screen
in a column would be a fourth panel in a shell whose point is three.

Three panes answer three questions. **Recipe** is the draft that will be sent:
label, master prompt, sampling, budget, tool ceiling. **Corpus** is what the
workspace contributes — selectors, the files the last run resolved them to, and a
byte meter against the recipe's ceiling. **Output** is the answer as it streams,
the gauges the provider reported, and the run history.

The rules are the ones a lab needs to be worth reading:

- A figure the provider did not report is absent or says `not reported`. Estimated
  tokens carry a sentence saying the server reported no usage, and the gauges are
  coloured as an estimate.
- The inspector (`v`) shows the exact request a run sent: prompt, selectors, the
  files with sizes and tokens, the budget, and the parameters. The files come from
  the run's record, which is why `GET /v1/recipes/{id}/runs/{run}` exists; without
  a record the inspector says what the plan is and leaves the file list empty
  rather than guessing.
- A parameter the server refused is named in the run's warnings, beside the
  request, because a knob that silently does nothing is worse than no knob.
- Compare (`c`) marks a run as the base; the selected run's gauges then carry the
  token and latency difference. A run that did not report a duration is compared
  in tokens only and says so.
- Editing is local until `^S`. `Esc` backs out one step at a time — the prompt,
  the inspector, the draft, the page — so the key that means "not that" cannot
  throw away a half-typed prompt.

Promotion is one keystroke away in the ticket, not here: `POST
/v1/recipes/{id}/promote` is P4.5-9's route, so the lab saves and runs but does
not write `agents.toml` yet.
