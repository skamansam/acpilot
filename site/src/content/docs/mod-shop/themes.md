---
title: Themes
description: One token set, four cockpits, and a contrast check that fails the build.
---

<span class="pill pill--hangar">hangar</span> &nbsp; P4.6-6

A theme is the most-installed extension and the easiest to get wrong. It is also
the one thing every surface must agree on: the TUI, the desktop app, the browser,
and the docs should not each invent what "muted text" means.

```toml
# themes/night-flight-syntax.toml
schema_version = 1
id = "acpilot.night-flight-syntax"
name = "Night flight (syntax)"
appearance = "dark"

[ui]
bg          = "#070d18"
bg_panel    = "#0c1424"
text        = "#e8edf6"
text_muted  = "#8b99ad"
accent      = "#f7c65c"
accent_high = "#ffe3a8"
border      = "#24334a"
success     = "#59c98b"
warning     = "#f7c65c"
danger      = "#ff7a7a"

[syntax]
comment   = "#5a6879"
keyword   = "#f7c65c"
string    = "#8ad6a0"
number    = "#e0a3ff"
type      = "#8fc7ff"
function  = "#ffd9a0"
variable  = "#e8edf6"
constant  = "#ffb3c1"
operator  = "#b9c4d4"
punctuation = "#8b99ad"
```

## Tokens

| Group | Tokens | Used by |
| --- | --- | --- |
| Surface | `bg`, `bg_panel`, `border` | Panes, cards, sidebar, dialogs |
| Text | `text`, `text_muted` | Body, hints, line numbers |
| Emphasis | `accent`, `accent_high` | Selection, focus, the current page |
| Status | `success`, `warning`, `danger` | Diagnostics, permission prompts, failed turns |
| Syntax | `comment`, `keyword`, `string`, `number`, `type`, `function`, `variable`, `constant`, `operator`, `punctuation` | Highlighting, code blocks, diffs |

A theme that omits a token inherits the built-in value for that token rather than
rendering an unset colour, so a syntax-only theme is a normal thing to publish.

## Contrast is enforced, not reviewed

Every built-in theme and every installed theme is checked in CI: each
`text`-on-`bg`, `text_muted`-on-`bg_panel`, and `accent`-on-`bg` pair must clear
WCAG AA (4.5:1). A theme below the line fails the build with the offending pair
and both ratios printed.

That rule exists because of a real bug in this very manual: the sidebar's selected
item once painted near-black text on a dark panel, and it was unreadable until
somebody measured it. "It looked fine on my monitor" is how a theme ships broken.

## Switching, without a restart

The theme is a preference, so changing it emits `prefs.changed` and every attached
client re-reads the tokens:

```bash
acpilot theme set acpilot.night-flight-syntax
acpilot theme ls --json | jq '.[] | select(.appearance=="light")'
```

<Aside type="note" title="Pairing, not guessing">
`appearance` tells the UI which half of a pair it is. Starlight's light/dark
toggle, the TUI's `:theme` command, and the desktop app's switcher all use it to
pick the right theme per mode instead of recolouring one in the dark.
</Aside>

## Where the built-ins go

In P4.6 the compiled-in themes move into this same format and load through the
same loader, marked `built_in: true`. If the schema is incomplete, the first build
fails on our own two themes — which is the cheapest possible time to find out.
