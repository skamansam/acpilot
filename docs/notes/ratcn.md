# ratcn: theming, and what adopting it would take

The question was whether the TUI should use [ratcn](https://ratcn.com) to handle
theming. It should not. The reason matters, as do the two pieces we took and the
condition under which we would revisit the components.

Version read: ratcn **0.0.6** (2026-10-06, first release 2026-08-07), `ratatui
^0.30.2`, `ratatui-textarea ^0.9.3` (not optional), MSRV 1.88. Ours is ratatui
0.30 and MSRV 1.95. Nothing here fails to build, so this is a design question,
not a compatibility one.

## What `ratcn::Theme` is

It has seventeen public `ratatui::style::Color` fields. The colour fields are
`foreground`, `muted_foreground`, `background`, `surface`, `field`, `primary`,
`primary_foreground`, `secondary`, `secondary_foreground`, `accent`,
`destructive`, `destructive_foreground`, `warning`, `border`, `ring` and
`cursor`. The last field is `name: &'static str`. It has `const` presets:
`default_dark`, `terminal`, catppuccin, gruvbox, nord, tokyo_night and
solarized. It also has `Theme::adaptive(background, foreground, palette16)`.

It is a palette type for the library's own widgets. There is no file format, no
inheritance, no user data, no syntax role, and nothing outside the terminal. The
type is `#[non_exhaustive]`, the name is `&'static str`, and every preset is a
`const fn`. That is exactly right for a crate whose themes are its own source. It
is exactly wrong for ours, where a theme is a TOML file a stranger can install.

Ours is a token package with 10 UI tokens and 10 syntax tokens. It has `extends`
inheritance, an all-tokens rule for a base, contrast measured on load, TextMate
import, a marketplace, and one token file feeding three surfaces. Those surfaces
are ratatui here, gpui in the desktop app, and CSS in the page `acpilotd` serves.

## Why a projection loses information both ways

| ours (10 UI + 10 syntax) | ratcn (17) |
| --- | --- |
| `bg` | `background` |
| `bg_panel` | `surface` and `field` (one value, two roles) |
| `text`, `text_muted` | `foreground`, `muted_foreground` |
| `accent`, `accent_high` | `accent`. `primary` is a second, unrelated accent |
| `border` | `border` |
| `warning`, `danger` | `warning`, `destructive` |
| `success`, `accent_high` | **nothing** |
| all 10 syntax tokens | **nothing** |
| — | `primary`, `primary_foreground`, `secondary`, `secondary_foreground`, `destructive_foreground`, `ring`, `cursor` |

So half the fields map one-to-one. Two of ours have no home, and ten have no home
at all. Seven of theirs would have to be derived. `ring` and `cursor` come from `accent`
with a blend. `primary`/`secondary` would be invented, since a theme file cannot
express them. That is not a simplification. It is a second palette with a
translation table that has to be tested against the first.

The one thing ratcn does better here is the *ladder*. `field` is one rung of a
four-level well (`field` < focus < hover < cursor row). The lift amounts are
constants, and the direction is read from the background (`away_from`). A light
theme therefore gets the ladder upside down for free. Their presets are held to
`field` between 1.10:1 and 1.35:1 of the background and to WCAG text floors by
their own tests. That reasoning is good, and we took it. See "What we took"
below.

## What rendering the panes with ratcn would mean

This is not a theming change, and not a reskin. The interactive components need
ratcn's runtime. That means `Ratcn<State, Msg>` with `render` and `handle_event`,
a `FocusState`, its own layer stack, its own mouse handling, and a message type
per app. Paint-only `*Widget`s are usable alone. `Dialog` and `ScrollArea` are
composites with no widget of their own.

Our TUI is ~1,770 lines of drawing in `ui.rs` and ~4,250 lines of state and key
handling in `app.rs`. The parts ratcn has no answer for are the parts that make
this UI. They include draggable dividers with mouse hit-testing read back from the
drawing code, and a menu bar with `Alt` mnemonics. They also include a `vt100`
grid pane that types and resizes, a command palette, which-key hints, and copy
mode. A permission card containing a diff is another. ratcn's first-class pieces are buttons, inputs, lists,
selects, tabs, dialogs, toasts, tooltips, checkboxes and progress. That is the
settings-and-dialog layer, not the cockpit.

Costs if we did it: its event loop replaces ours. Its runtime owns focus and
layers, so the keyboard routing moves with it. Components are *copied* into
`src/components/` by `cargo ratcn add` rather than depended on when they need
changing. Even the paint-only widgets bring `ratatui-textarea`,
`unicode-segmentation` and `unicode-width`. It is a 0.0.x crate with seven
releases in two months, which is fine to vendor and early to build a shell on.

## The bridge, if we ever want the widgets

Cheap, and worth writing down, because "we cannot use ratcn" is not true. We
cannot use it *as the theme system*. A projection from our palette to theirs is
about twenty lines, built from a preset because `#[non_exhaustive]` rules out a
literal:

```rust
// Not shipped: the shape of the bridge if a pane ever needs a ratcn component.
fn projection(palette: &acpilot_extensions::Palette) -> ratcn::Theme {
    let mut theme = ratcn::Theme::default_dark();
    let colour = |token: UiToken| {
        let (r, g, b) = palette.ui(token).expect("a complete theme").channels();
        ratatui::style::Color::Rgb(r, g, b)
    };
    theme.name = "Acpilot";
    theme.background = colour(UiToken::Bg);
    theme.surface = colour(UiToken::BgPanel);
    theme.field = colour(UiToken::BgPanel);
    theme.foreground = colour(UiToken::Text);
    theme.muted_foreground = colour(UiToken::TextMuted);
    theme.accent = colour(UiToken::Accent);
    theme.primary = colour(UiToken::Accent);
    theme.border = colour(UiToken::Border);
    theme.warning = colour(UiToken::Warning);
    theme.destructive = colour(UiToken::Danger);
    theme.ring = colour(UiToken::AccentHigh);
    theme.cursor = colour(UiToken::AccentHigh);
    // No source: primary_foreground, secondary, secondary_foreground,
    // destructive_foreground. Solved or borrowed from the preset; the syntax tokens
    // are dropped because no ratcn component draws code.
    theme
}
```

The cost of that bridge is two palette types for one surface. Take it only with a
component, not as a general policy.

## What we took

1. **The panel window.** `bg_panel` must sit between **1.10:1 and 1.35:1** of `bg`
   (`contrast::PANEL_MIN`/`PANEL_MAX`). Both built-ins were outside it: night
   flight 1.06:1, paper 1.08:1. Both now sit at 1.17:1. It is a warning at load,
   not a refusal. CI fails if a built-in drifts out.
2. **`adaptive`, in our token space.** `theme_follow_terminal` solves the whole
   token set from the terminal's own `OSC 11`/`10` pair
   (`acpilot_extensions::follow`). It validates the result with the same audit
   and returns [`None`] rather than showing a palette that fails. ratcn's version
   returns a terminal-only `Color` palette, which the desktop app and the page
   could not draw.
3. **Deepening a mid-ramp background.** White ink on a panel lifted off `#808080`
   tops out around 3.3:1. The solver therefore moves the background towards its
   own end until the ladder is solvable. That is the same rule, in the same
   place: the second paragraph of `Theme::adaptive`'s docs.

## When to revisit

The trigger is a surface that needs real widgets with focus, hover and layers:
the settings dialog, a session picker with search, a modal form. Then vendor the
specific component (`cargo ratcn add dialog`), project the palette with the
function above, and keep our tokens authoritative. Do not revisit on a new
colour, a new UI surface, or a new theme. Nothing in ratcn's theme type helps
with any of those, and the projection only exists to feed its widgets.
