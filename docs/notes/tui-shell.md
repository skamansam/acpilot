# The TUI shell: a design pass

**Status:** designed and drawn, not wired. `acpilot tui --demo` is the screen this
note describes; everything in it that has a backend works, and everything else says
which task would give it one.

## Why a design pass at all

The interface was being decided one pane at a time, in the order the backend
happened to land. That produces a screen nobody chose. So the shell was drawn
first, end to end, on seeded data: the panes that exist, the panes that do not, one
command palette listing all of them, and a notice when a row has no implementation
behind it. The point is not the mock — it is that the layout can be argued about
with something that answers the keyboard.

The same world is rendered by the desktop app when it gets its shell (P10), from
the same crate, so the two designs cannot drift apart in the first week.

## Run it

```bash
cargo run -p acpilot-cli -- tui --demo     # the design pass, no daemon
cargo run -p acpilot-cli -- tui            # the real thing
```

Nothing is read from disk and nothing is spawned. `Tab` moves between panes, `?`
lists the focused pane's keys, `Ctrl-P` opens the palette, `Ctrl-T` attaches the
terminal pane.

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

A menu bar, three panels — **files, text, agents** — and two footer rows. The panels
are the same three in the TUI, the desktop app and the browser, which is the point of
the split.- **The menu bar** is one row of chrome, not a modal: the titles are always drawn, so
  the commands are discoverable without a key list, and `F9` is only how the keyboard
  gets into a bar whose rows are the same code paths the chords and the palette run.
  Every row prints the chord that would have run it, right-aligned, so the bar is
  where you learn the keys as well as the list; a row whose action only exists in a
  pane prints nothing, because a chord in the bar promises it works from anywhere.
  The underlined letter in a title is `Alt`'s: `Alt-F`, `Alt-V`, `Alt-S`, `Alt-H`
  open a menu with its list showing, and the pointer answers the same targets —
  click a title to open, close or switch, slide along the bar to another, click a
  row to run it. It is the one place the mouse is allowed to *choose* something,
  because a bar of labels is what a pointer is better at than a key. Help holds About, which names the
  build in the same four rows the desktop's About prints.

- **Files** (26 columns) is the workspace tree, flattened: indentation is data, not
  a walk. It folds away below 100 columns rather than squeezing the editor.
- **Text** is a tab strip and the buffer, with a gutter for line numbers and git
  markers, the current line highlighted, and the cursor at the position the status
  line reports. The buffer is a real document (P6-4): the rows are built from the
  viewport, so only the lines that fit and only the cells the horizontal scroll
  left are drawn, and their colours are the highlighter's tokens resolved through
  the theme.
- **Agents** is the session list, the transcript and the composer, in that order:
  what you can open, what it said, what you want to say.
- The **status line** is connection, profile, focused pane, cursor, last message,
  and the daemon address when there is room for it.
- The **stats bar** is one row per integration: provider, model, tokens both ways,
  money, latency. A provider that stopped answering is drawn hollow, not hidden.

Overlays, back to front: the terminal picker, the key list, the palette, the
permission card, the toast.

## What is real, and what is not

| Pane | Today | Task |
| --- | --- | --- |
| Menu bar, About | real, and every row is an existing path | — |
| Sessions, transcript, composer | real, over the daemon; the transcript is a pane (`Tab`, `F3`) with scroll, find and copy | P6-7 refines the composer |
| Permission card | real answer path, seeded diff preview | P6-8, P6-9 |
| Terminal pane | real attach, scrollback, find, copy mode, picker | P6-10 (done in the TUI) |
| Command palette, which-key, toast | real | — |
| Theme | real: every colour is a token from the theme the settings name (`theme_tui` over `theme`), resolved for the terminal, the desktop app and the page the daemon serves; a running client watches the settings files and recolours on save, and `theme_follow_terminal` solves the terminal's palette from the terminal's own `OSC 11`/`10` pair instead of naming one | `prefs.changed` over the daemon's event stream (P4-7) |
| File tree | seeded rows; movement and collapsing work | P6-6 |
| Editor | real: a `Document` drawn through `acpilot-text::pane` — virtualised rows, horizontal scroll, gutter with numbers and git markers parsed from the diff, current line, bracket matching, `/` find with smart case, `Ctrl-G` go-to-line, a click that lands a caret and a wheel that scrolls the view. The buffer is the seeded one, because reading a file is P6-6, and it is read-only until P6-5 | P6-5 (insert mode), P6-6 (opening a file), P7-10 (saving) |
| Stats bar | seeded rows | P6-13 |

## Keys

| Where | Keys |
| --- | --- |
| anywhere | `Tab` next pane · `?` keys · `Ctrl-P` palette · `F9` menus · `Alt`+underlined letter a menu · `Ctrl-T` terminal · `F3` read the transcript · `Ctrl-←`/`Ctrl-→` narrower/wider · drag a divider · `Ctrl-R` refresh · `Ctrl-C` quit |
| menu bar | `←`/`→` titles · `↓`/`Enter` open · `↑`/`↓` rows · `Enter` run · `Esc` back one level · `Alt`+letter another menu · click and hover work too |
| transcript | the copy keys below, because the keyboard being there is what reading it means |
| files | `j`/`k` move · `h`/`l` collapse/expand · `Enter` open · `/` fuzzy open |
| editor | `h`/`j`/`k`/`l` cursor · `0`/`$` start/end of line · `PgUp`/`PgDn` a page · `/` or `Ctrl-F` find · `n`/`N` next/previous match · `Ctrl-G` line[:column] · `Ctrl-S` save · `i` insert · `F12` definition · click puts the caret · wheel scrolls the view |
| sessions | `j`/`k` choose · `Enter` open · `i` compose · `n` new · `d` end · `c` cancel |
| prompt | `Enter` send · `Esc` back to the list |
| terminal | any key is a byte · `F2` copy mode · `Ctrl-T` leave |
| copy mode | `j`/`k`/`h`/`l` move · `Ctrl-U`/`Ctrl-D` page · `g`/`G` top/live · `/` find (smart-case) · `n`/`N` next match · `v` select · `y` copy · `Esc` leaves |

`Tab` never lands in the terminal: a shell wants the byte. The pane is entered and
left with `Ctrl-T`, and the copy mode's own keys take over while it is up. The
transcript answers to the same copy keys — `F3` opens it, and it is the same state,
the same corners and the same yank, over the agent's lines instead of a program's
cells. Copy mode is entered with a function key on purpose: every printable key and
every `Ctrl` chord is a byte a program can want.

A find is case-insensitive until the query grows a capital, which is the behaviour
`less` and `vim` taught and the only one that does not need a second key to toggle.
The transcript's copy mode answers to the same rule, so one `/` means one thing in
both panes.

Mouse reporting is on. The mouse does three things: it drags a divider, because the
borders between files, text and agents are where a person reaches for one; it uses
the menu bar, because a row of labels is a target a pointer can see and hit; and in
the editor it puts the caret where a click landed and scrolls the buffer under the
wheel, because a click is the fastest way to a caret and a wheel is a motion no key
has. A click on the numbers is furniture, not text: it falls through to the divider
drag it looks like. The wheel is pointed rather than aimed — it scrolls the pane
under the pointer without taking the keyboard, and without moving the caret, which
is what makes reading a file with a trackpad the same file when you let go. Every
one of these has a keyboard path that is not the fallback but the first one — a
trackpad is not available over SSH, and the key is what gets pressed on a bad
connection.

The gutter's markers are git's answer, not the buffer's: `acpilot-git` parses the
same patch the permission card shows, so the `~` beside a line and the hunk in the
review cannot disagree about what the agent did.

## When a feature is not built

A palette row is tagged with its task (`Review diff · P6-9`). Running it raises a
toast naming the task and the intent — "planned · P6-9 · Review diff", "not
implemented in this build" — and the same sentence goes on the status line. The
notice belongs to the keystroke that raised it: the next key dismisses it.

That is deliberate over a modal: the interface is being reviewed, not used, and a
dialog that has to be dismissed would turn every press into two.

## The seeded world

`crates/acpilot-demo` is data only, in protocol types: sessions (with a transcript
per session), agents (one of them missing), the tree, a buffer, a diff, a pending
permission, two terminals, two integrations, and the command catalogue. No
behaviour, no renderer, no key handling — which is what lets the desktop app import
it without importing the TUI.

In the TUI the demo is a *worker* (`backend::spawn_demo`), not a special case in
the app: it speaks the same `Update` channel the daemon's worker does, so every
pane is the real pane and the design cannot claim something the product will not
do. A prompt gets a scripted reply; a session open replays that session's stored
events at 45 ms a chunk.

## Questions this pass is meant to settle

1. **The tree at 80 columns.** Folding it away is the current answer; a toggle
   pane or a full-width tree are the alternatives.
2. **The terminal's shape.** It takes the editor and the sidebar, keeping the tree.
   A tab beside the buffer, or a split under it, are both plausible.
3. **The diff.** It lives inside the permission card, because that is where the
   decision is made. P6-9 may want a review pane of its own.
4. **The transcript's scrollback.** Answered: `F3` gives it the terminal's copy
   mode, including a yank over OSC 52. What is still open is whether a transcript
   that can be read is a pane with a focus of its own, rather than a mode entered
   from any pane with one key.
5. **The stats bar's density.** Two rows of numbers in one line is already tight at
   110 columns.
6. **Palette, menus and which-key.** All three are here and all three run the same
   paths: the palette is the fast way, the bar is the discoverable one, `?` is the
   one that teaches. Whether three survive contact with users, or the palette is
   enough once it lists keys, is still open.
7. **The dividers' keys.** Answered: `Ctrl-←`/`Ctrl-→` follow the focused pane for
   the keyboard, and a mouse drag on the divider moves the same width, so neither
   gesture is the only way. What is still open is whether the drag should also
   persist — the widths reset on launch today.
