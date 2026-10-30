//! The window.
//!
//! A themed shell that lists the profiles it was launched with, a menu bar, and a
//! command palette driven from the keymap table in [`crate::keymap`]. Panes and
//! the editor come next; what is here is the frame they will hang on.
//!
//! The shell owns no domain logic. Tokens come from the shared `theme.toml`
//! ([`crate::theme`]), profiles from [`acpilot_profile`] ([`crate::profiles`]),
//! and the commands the palette offers are built from those two, not invented
//! here.

use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, KeyBinding, KeyDownEvent, Menu, MenuItem, ParentElement, Render, SharedString,
    Size as GpuiSize, StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions,
    actions, div, point, px, rgb,
};

use acpilot_extensions::UiToken;
use acpilot_stats::{Row, Track};

use crate::keymap::{self, Action as KeyAction, Context as KeyContext};
use crate::palette::{self, Command, CommandId};
use crate::profiles::ShellProfile;
use crate::stats;
use crate::theme::GuiTheme;

actions!(
    shell,
    [
        TogglePalette,
        PaletteUp,
        PaletteDown,
        PaletteConfirm,
        PaletteDismiss,
        NextProfile,
        PreviousProfile,
        ShowAbout,
        DismissAbout,
        ToggleStatsSources,
        Quit,
    ]
);

/// The keymap as gpui bindings, one table entry per binding it names.
///
/// The match is the only place a [`KeyAction`] becomes a gpui action, so a new
/// verb is a compile error here until it is handled — which is the property the
/// `actions!` list and the table would otherwise lose track of.
#[must_use]
pub fn key_bindings() -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    for binding in keymap::DEFAULT {
        for context in binding.contexts {
            bindings.push(to_binding(binding.keys, binding.action, context.name()));
        }
    }
    // Escape closes About in the shell context. It is not in the keymap table
    // because it is not a command: the table is the list a user can read, and
    // "close the thing that is open" belongs with the thing, not with the Verbs.
    bindings.push(KeyBinding::new(
        "escape",
        DismissAbout,
        Some(KeyContext::Shell.name()),
    ));
    bindings
}

fn to_binding(keys: &str, action: KeyAction, context: &'static str) -> KeyBinding {
    match action {
        KeyAction::TogglePalette => KeyBinding::new(keys, TogglePalette, Some(context)),
        KeyAction::PaletteUp => KeyBinding::new(keys, PaletteUp, Some(context)),
        KeyAction::PaletteDown => KeyBinding::new(keys, PaletteDown, Some(context)),
        KeyAction::PaletteConfirm => KeyBinding::new(keys, PaletteConfirm, Some(context)),
        KeyAction::PaletteDismiss => KeyBinding::new(keys, PaletteDismiss, Some(context)),
        KeyAction::NextProfile => KeyBinding::new(keys, NextProfile, Some(context)),
        KeyAction::PreviousProfile => KeyBinding::new(keys, PreviousProfile, Some(context)),
        KeyAction::ToggleStatsSources => KeyBinding::new(keys, ToggleStatsSources, Some(context)),
        KeyAction::Quit => KeyBinding::new(keys, Quit, Some(context)),
    }
}

/// The version this window reports in About.
///
/// The same constant the CLI and the daemon report, rather than a string written
/// here: a window that says 0.1.0 while the tarball says 0.4.2 is a bug report.
pub const VERSION: &str = acpilot_core::VERSION;

/// Everything the About view says, in one place a test can read.
#[must_use]
pub fn about_lines() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Version", VERSION),
        ("License", "MIT OR Apache-2.0"),
        ("Field manual", "rudeboy.dev/acpilot"),
        ("Built with", "Freebuff — freebuff.com"),
    ]
}

/// The application menu.
///
/// Native menus are labels the OS draws, so they are not a second keymap: the
/// keystroke a user sees beside an item comes from the binding, and the item only
/// names the action. macOS is the platform that draws these; on Linux and Windows
/// gpui 0.2.2 has no native menu bar, which is why the palette is the path that
/// works everywhere.
#[must_use]
pub fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "Acpilot".into(),
            items: vec![
                MenuItem::action("About Acpilot", ShowAbout),
                MenuItem::separator(),
                MenuItem::action("Command Palette…", TogglePalette),
                MenuItem::separator(),
                MenuItem::action("Quit Acpilot", Quit),
            ],
        },
        Menu {
            name: "Profile".into(),
            items: vec![
                MenuItem::action("Next Profile", NextProfile),
                MenuItem::action("Previous Profile", PreviousProfile),
            ],
        },
    ]
}

/// The palette's transient state. `None` means it is closed.
struct PaletteState {
    query: String,
    selected: usize,
}

/// The shell: what is on screen, which profile is current, and whether the
/// palette is open.
pub struct Shell {
    profiles: Vec<ShellProfile>,
    selected: usize,
    theme: GuiTheme,
    commands: Vec<Command>,
    palette: Option<PaletteState>,
    /// Whether the About view is showing.
    about: bool,
    /// The stats bar's rows, as the shared crate measured them.
    integrations: Vec<Row>,
    /// Whether the bar shows each figure's source instead of its numbers.
    stats_sources: bool,
    focus: FocusHandle,
    focused: bool,
}

impl Shell {
    /// Build the view from what `main` resolved.
    pub fn new(profiles: Vec<ShellProfile>, theme: GuiTheme, cx: &mut Context<Self>) -> Self {
        Self {
            commands: palette::commands(&profiles),
            profiles,
            selected: 0,
            theme,
            palette: None,
            about: false,
            integrations: Vec::new(),
            stats_sources: false,
            focus: cx.focus_handle(),
            focused: false,
        }
    }

    /// The profile currently shown, if any.
    #[must_use]
    pub fn current(&self) -> Option<&ShellProfile> {
        self.profiles.get(self.selected)
    }

    /// The theme the window is drawing with.
    #[must_use]
    pub fn theme(&self) -> &GuiTheme {
        &self.theme
    }

    /// Recolour the window.
    ///
    /// Called when the daemon announces the settings changed. A theme that is the
    /// one already on screen is not a repaint: the frame that opens a connection
    /// carries the same preferences the window launched with, and every window
    /// would flicker once at startup for a change that did not happen.
    pub fn set_theme(&mut self, theme: GuiTheme, cx: &mut Context<Self>) {
        if self.theme == theme {
            return;
        }
        self.theme = theme;
        cx.notify();
    }

    /// Replace the stats bar's rows.
    ///
    /// The rows arrive already measured by `acpilot_stats`, so there is no
    /// arithmetic here. A snapshot equal to the one on screen is not a repaint: the
    /// poll asks every fifteen seconds and most answers carry the same figures, so
    /// comparing first keeps a window that is sitting still from drawing.
    pub fn set_stats(&mut self, rows: Vec<Row>, cx: &mut Context<Self>) {
        if self.integrations == rows {
            return;
        }
        self.integrations = rows;
        cx.notify();
    }

    /// The rows the bar is drawing.
    #[must_use]
    pub fn integrations(&self) -> &[Row] {
        &self.integrations
    }

    /// Whether the bar is showing provenance instead of figures.
    #[must_use]
    pub fn showing_stats_sources(&self) -> bool {
        self.stats_sources
    }

    /// Run a palette command.
    fn run(&mut self, command: CommandId, cx: &mut Context<Self>) {
        match command {
            CommandId::SelectProfile(index) if index < self.profiles.len() => {
                self.selected = index;
                self.palette = None;
                cx.notify();
            }
            // A profile that vanished between building the list and choosing it.
            // Impossible while the list is fixed for the window's life, and not
            // worth a panic when the list becomes live.
            CommandId::SelectProfile(_) => {}
            // The same pair of facts `show_about` sets: About showing, palette
            // closed, so one Escape does not have to dismiss both.
            CommandId::About => {
                self.palette = None;
                self.about = true;
                cx.notify();
            }
            // A toggle that acts on the bar behind the palette, so the palette
            // closes: every command in this list leaves the palette closed.
            CommandId::StatsSources => {
                self.palette = None;
                self.stats_sources = !self.stats_sources;
                cx.notify();
            }
            CommandId::Quit => cx.quit(),
        }
    }

    fn toggle_palette(&mut self, _: &TogglePalette, _window: &mut Window, cx: &mut Context<Self>) {
        self.palette = match self.palette {
            Some(_) => None,
            None => Some(PaletteState {
                query: String::new(),
                selected: 0,
            }),
        };
        cx.notify();
    }

    fn dismiss_palette(
        &mut self,
        _: &PaletteDismiss,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.take().is_some() {
            cx.notify();
        }
    }

    /// Open About, and close the palette: two overlays competing for the same
    /// escape key is how a window ends up needing a second escape.
    fn show_about(&mut self, _: &ShowAbout, _window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.about = true;
        cx.notify();
    }

    fn dismiss_about(&mut self, _: &DismissAbout, _window: &mut Window, cx: &mut Context<Self>) {
        if self.about {
            self.about = false;
            cx.notify();
        }
    }

    /// `F4`: swap every figure for the source behind it, and back.
    fn toggle_stats_sources(
        &mut self,
        _: &ToggleStatsSources,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stats_sources = !self.stats_sources;
        cx.notify();
    }

    /// Whether About is showing, for a caller that needs to know before drawing.
    #[must_use]
    pub fn showing_about(&self) -> bool {
        self.about
    }

    /// Whether the palette is open, for the same reason.
    #[must_use]
    pub fn showing_palette(&self) -> bool {
        self.palette.is_some()
    }

    fn move_palette(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Self {
            palette, commands, ..
        } = self;
        let Some(palette) = palette.as_mut() else {
            return;
        };
        let len = palette::filter(commands, &palette.query).len();
        palette.selected = palette::move_selection(palette.selected, len, delta);
        cx.notify();
    }

    fn palette_down(&mut self, _: &PaletteDown, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_palette(1, cx);
    }

    fn palette_up(&mut self, _: &PaletteUp, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_palette(-1, cx);
    }

    fn confirm_palette(
        &mut self,
        _: &PaletteConfirm,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(palette) = self.palette.as_ref() else {
            return;
        };
        let matched = palette::filter(&self.commands, &palette.query);
        let Some(command) = matched
            .get(palette.selected)
            .map(|index| self.commands[*index].id)
        else {
            return;
        };
        self.run(command, cx);
    }

    fn move_profile(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.selected = palette::move_selection(self.selected, self.profiles.len(), delta);
        cx.notify();
    }

    fn next_profile(&mut self, _: &NextProfile, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_profile(1, cx);
    }

    fn previous_profile(
        &mut self,
        _: &PreviousProfile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_profile(-1, cx);
    }

    fn quit(&mut self, _: &Quit, _window: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    /// Text entry for the palette, which gpui 0.2.2 does not give us: the
    /// framework has no text field, so the focused element reads its own
    /// keystrokes. Only characters are appended here — `down`, `enter` and
    /// `escape` arrive as bound actions, and handling them twice would move the
    /// selection twice per key.
    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_none() {
            return;
        }
        let keystroke = &event.keystroke;
        if keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform {
            return;
        }

        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        let changed = if keystroke.key == "backspace" {
            palette.query.pop().is_some()
        } else if keystroke.key == "space" {
            palette.query.push(' ');
            true
        } else if let Some(text) = keystroke.key_char.as_deref() {
            if text.is_empty() {
                false
            } else {
                palette.query.push_str(text);
                true
            }
        } else {
            false
        };

        if changed {
            // A new query means a new best row: keeping the old index would run
            // whatever happens to sit third in a list the user just narrowed.
            palette.selected = 0;
            cx.notify();
        }
    }
}

/// Open a shell window on these profiles and this theme.
///
/// Every window is its own view with its own command palette; the keymap and the
/// theme are already on the app, so a second window costs a view and nothing
/// else. That is what makes the activation path cheap — see [`crate::platform`].
///
/// The view is returned because the theme is now live: the daemon announces a
/// save, and every open window has to be recoloured without being reopened.
pub fn open_window(
    cx: &mut App,
    profiles: Vec<ShellProfile>,
    theme: GuiTheme,
) -> anyhow::Result<Entity<Shell>> {
    let handle = cx.open_window(window_options(), move |_window, cx| {
        cx.new(|cx| Shell::new(profiles, theme, cx))
    })?;
    handle.entity(cx)
}

/// The window the app opens with.
///
/// One window, restored later; the size here is the size a person asked for the
/// first time they ran a code editor, not a preference system.
#[must_use]
pub fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(80.0), px(60.0)),
            size: GpuiSize {
                width: px(1_100.0),
                height: px(720.0),
            },
        })),
        focus: true,
        show: true,
        ..Default::default()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // gpui dispatches key events to the focused node. The shell is the only
        // focusable thing in the window, so it takes focus once rather than
        // every frame (which would fight the palette's own clicks).
        if !self.focused {
            window.focus(&self.focus);
            self.focused = true;
        }

        // Read what the frame needs before building elements: element trees are
        // 'static, so a closure cannot borrow `self`.
        let theme = self.theme.clone();
        let heading = SharedString::from(format!("Acpilot · {}", theme.name()));
        let palette_open = self.palette.is_some();
        let context = if palette_open {
            KeyContext::Palette.name()
        } else {
            KeyContext::Shell.name()
        };

        let profiles: Vec<(usize, String, String, bool)> = self
            .profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                (
                    index,
                    profile.name().to_owned(),
                    profile.summary(),
                    index == self.selected,
                )
            })
            .collect();

        let segments: Vec<Segment> = self
            .integrations
            .iter()
            .map(|row| Segment::of(row, self.stats_sources))
            .collect();

        let palette_rows: Vec<(usize, CommandId, String, String, bool)> = self
            .palette
            .as_ref()
            .map(|palette_state| {
                let matched = palette::filter(&self.commands, &palette_state.query);
                matched
                    .iter()
                    .enumerate()
                    .map(|(row, index)| {
                        let command = &self.commands[*index];
                        (
                            row,
                            command.id,
                            command.label.clone(),
                            command.detail.clone(),
                            row == palette_state.selected,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        let footer = self.current().map_or_else(
            || "No profile was passed on the command line.".to_owned(),
            |profile| format!("{} — {}", profile.root().display(), profile.summary()),
        );
        let empty_palette = palette_open && palette_rows.is_empty();
        let about_open = self.about;

        let header = div()
            .h(px(40.0))
            .p(px(12.0))
            .bg(rgb(theme.ui(UiToken::BgPanel)))
            .border_b(px(1.0))
            .border_color(rgb(theme.ui(UiToken::Border)))
            .text_color(rgb(theme.ui(UiToken::TextMuted)))
            .child(heading);

        let palette_panel = self.palette.as_ref().map(|palette_state| {
            let query = if palette_state.query.is_empty() {
                SharedString::from("Type to filter commands")
            } else {
                SharedString::from(format!("> {}", palette_state.query))
            };
            let query_color = if palette_state.query.is_empty() {
                theme.ui(UiToken::TextMuted)
            } else {
                theme.ui(UiToken::AccentHigh)
            };

            div()
                .p(px(12.0))
                .bg(rgb(theme.ui(UiToken::Bg)))
                .border_b(px(1.0))
                .border_color(rgb(theme.ui(UiToken::Accent)))
                .text_color(rgb(query_color))
                .child(div().text_size(px(15.0)).child(query))
                .children(
                    palette_rows
                        .iter()
                        .map(|(row, command, label, detail, selected)| {
                            let (background, label_color) = if *selected {
                                (theme.ui(UiToken::Accent), theme.ui(UiToken::Bg))
                            } else {
                                (theme.ui(UiToken::Bg), theme.ui(UiToken::Text))
                            };
                            let detail_color = if *selected {
                                theme.ui(UiToken::Bg)
                            } else {
                                theme.ui(UiToken::TextMuted)
                            };
                            let command = *command;
                            let row = *row;
                            div()
                                .id(("palette-row", row))
                                .p(px(8.0))
                                .bg(rgb(background))
                                .text_color(rgb(label_color))
                                .child(SharedString::from(label.clone()))
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(rgb(detail_color))
                                        .child(SharedString::from(detail.clone())),
                                )
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    this.run(command, cx);
                                }))
                        }),
                )
        });

        let list = div().children(
            profiles
                .into_iter()
                .map(|(index, name, summary, selected)| {
                    let background = if selected {
                        theme.ui(UiToken::Accent)
                    } else {
                        theme.ui(UiToken::BgPanel)
                    };
                    let (name_color, summary_color) = if selected {
                        (theme.ui(UiToken::Bg), theme.ui(UiToken::Bg))
                    } else {
                        (theme.ui(UiToken::Text), theme.ui(UiToken::TextMuted))
                    };

                    div()
                        .id(("profile", index))
                        .h(px(48.0))
                        .p(px(12.0))
                        .bg(rgb(background))
                        .border_b(px(1.0))
                        .border_color(rgb(theme.ui(UiToken::Border)))
                        .text_color(rgb(name_color))
                        .child(SharedString::from(name))
                        .child(
                            div()
                                .text_color(rgb(summary_color))
                                .text_size(px(11.0))
                                .child(SharedString::from(summary)),
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.selected = index;
                            this.palette = None;
                            cx.notify();
                        }))
                }),
        );

        // An empty bar says why it is empty rather than drawing nothing: a footer
        // that vanishes and a footer with no integrations are the same picture
        // otherwise.
        let stats_children: Vec<AnyElement> = if segments.is_empty() {
            vec![
                div()
                    .text_color(rgb(theme.ui(UiToken::TextMuted)))
                    .child(SharedString::from("stats · no integrations"))
                    .into_any_element(),
            ]
        } else {
            segments
                .iter()
                .map(|segment| segment_element(&theme, segment))
                .collect()
        };

        let stats_bar = div()
            .id("stats")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.0))
            .h(px(28.0))
            .px(px(12.0))
            .overflow_hidden()
            .bg(rgb(theme.ui(UiToken::BgPanel)))
            .border_t(px(1.0))
            .border_color(rgb(theme.ui(UiToken::Border)))
            .text_size(px(11.0))
            .children(stats_children);

        let about_panel = about_open.then(|| {
            let lines: Vec<(usize, String, String)> = about_lines()
                .into_iter()
                .enumerate()
                .map(|(row, (label, value))| (row, label.to_owned(), value.to_owned()))
                .collect();
            div()
                .id("about")
                .p(px(20.0))
                .bg(rgb(theme.ui(UiToken::BgPanel)))
                .border(px(1.0))
                .border_color(rgb(theme.ui(UiToken::Accent)))
                .text_color(rgb(theme.ui(UiToken::Text)))
                .child(
                    div()
                        .text_size(px(20.0))
                        .child(SharedString::from("Acpilot")),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme.ui(UiToken::TextMuted)))
                        .child(SharedString::from(
                            "A code editor that flies your agents. You keep the controls.",
                        )),
                )
                .children(lines.into_iter().map(|(row, label, value)| {
                    div()
                        .id(("about-row", row))
                        .pt(px(6.0))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(theme.ui(UiToken::TextMuted)))
                                .child(SharedString::from(label)),
                        )
                        .child(div().text_size(px(13.0)).child(SharedString::from(value)))
                }))
                .child(
                    div()
                        .id("about-close")
                        .mt(px(16.0))
                        .p(px(8.0))
                        .bg(rgb(theme.ui(UiToken::Accent)))
                        .text_color(rgb(theme.ui(UiToken::Bg)))
                        .child(SharedString::from("Close (escape)"))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.about = false;
                            cx.notify();
                        })),
                )
                .into_any_element()
        });

        div()
            .id("shell")
            .size_full()
            .track_focus(&self.focus)
            .key_context(context)
            .bg(rgb(theme.ui(UiToken::Bg)))
            .text_color(rgb(theme.ui(UiToken::Text)))
            .text_size(px(13.0))
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::palette_up))
            .on_action(cx.listener(Self::palette_down))
            .on_action(cx.listener(Self::confirm_palette))
            .on_action(cx.listener(Self::dismiss_palette))
            .on_action(cx.listener(Self::next_profile))
            .on_action(cx.listener(Self::previous_profile))
            .on_action(cx.listener(Self::show_about))
            .on_action(cx.listener(Self::dismiss_about))
            .on_action(cx.listener(Self::toggle_stats_sources))
            .on_action(cx.listener(Self::quit))
            .on_key_down(cx.listener(Self::key_down))
            .child(header)
            .children(palette_panel)
            .child(if let Some(about) = about_panel {
                about
            } else if empty_palette {
                div()
                    .p(px(12.0))
                    .text_color(rgb(theme.ui(UiToken::TextMuted)))
                    .child(SharedString::from("No command matches that."))
                    .into_any_element()
            } else {
                list.into_any_element()
            })
            .child(stats_bar)
            .child(
                div()
                    .p(px(12.0))
                    .text_color(rgb(theme.ui(UiToken::TextMuted)))
                    .text_size(px(11.0))
                    .child(SharedString::from(footer))
                    .child(SharedString::from(
                        "secondary-shift-p — command palette · down/up — profile · f4 — where the stats came from · escape — close about · secondary-q — quit",
                    )),
            )
    }
}

/// One integration, as the bar draws it.
///
/// A projection of [`acpilot_stats::Row`], not a second measurement: the words are
/// the crate's, and what is added here is what only a window can answer — the
/// glyph, the theme tokens and the pixel lengths.
struct Segment {
    marker: &'static str,
    label: String,
    stale: bool,
    quiet: bool,
    /// The provenance line, when the bar is showing sources instead of figures.
    sources: Option<String>,
    context: Option<TrackView>,
    credits: Option<TrackView>,
}

/// One measured quantity, ready to draw.
struct TrackView {
    /// The filled length in pixels, or `None` for a figure with no denominator.
    width: Option<f32>,
    colour: UiToken,
    figure: UiToken,
    text: String,
}

impl Segment {
    fn of(row: &Row, sources: bool) -> Self {
        Self {
            marker: stats::marker(row.kind),
            label: row.label.clone(),
            stale: row.stale,
            quiet: row.is_quiet(),
            sources: sources.then(|| row.sources()),
            context: row.context.as_ref().map(TrackView::of),
            credits: row.credits.as_ref().map(TrackView::of),
        }
    }
}

impl TrackView {
    fn of(track: &Track) -> Self {
        Self {
            width: stats::fill_width(track),
            colour: stats::source_token(track.source),
            figure: stats::figure_token(track.source),
            text: track.text.clone(),
        }
    }
}

/// One integration's place on the bar: a glyph, a name, and what it costs.
///
/// A row with nothing reported says so instead of drawing two empty strips, which
/// would be read as two zeros.
fn segment_element(theme: &GuiTheme, segment: &Segment) -> AnyElement {
    let label = if segment.stale {
        theme.ui(UiToken::TextMuted)
    } else {
        theme.ui(UiToken::Text)
    };
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .text_color(rgb(theme.ui(UiToken::TextMuted)))
                .child(SharedString::from(segment.marker)),
        )
        .child(
            div()
                .text_color(rgb(label))
                .child(SharedString::from(segment.label.clone())),
        );
    // A stale reading keeps its label and loses its emphasis: the number is still
    // the last one somebody reported, and when is enough to read it right.
    if segment.stale {
        row = row.child(muted(theme, "old"));
    }
    if let Some(sources) = &segment.sources {
        row = row.child(muted(theme, sources.clone()));
    } else if segment.quiet {
        row = row.child(muted(theme, "nothing reported"));
    } else {
        if let Some(context) = &segment.context {
            row = row.child(measured(theme, context));
        }
        if let Some(credits) = &segment.credits {
            row = row.child(muted(theme, "cr"));
            row = row.child(measured(theme, credits));
        }
    }
    row.into_any_element()
}

/// A track and the figures beside it, kept together so a length and its number
/// cannot drift apart when the bar is reflowed.
fn measured(theme: &GuiTheme, view: &TrackView) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .child(track_strip(theme, view))
        .child(
            div()
                .text_color(rgb(theme.ui(view.figure)))
                .child(SharedString::from(view.text.clone())),
        )
        .into_any_element()
}

/// The strip itself: a filled length over a track-coloured bed.
///
/// A figure with no denominator is a hairline instead of a full or an empty strip,
/// so "nobody reported a window" cannot be read as "the window is nearly empty" or
/// as "there is no conversation".
fn track_strip(theme: &GuiTheme, view: &TrackView) -> AnyElement {
    let Some(width) = view.width else {
        return div()
            .w(px(stats::TRACK_WIDTH))
            .h(px(stats::INDETERMINATE_HEIGHT))
            .bg(rgb(theme.ui(UiToken::TextMuted)))
            .into_any_element();
    };
    div()
        .w(px(stats::TRACK_WIDTH))
        .h(px(stats::TRACK_HEIGHT))
        .bg(rgb(theme.ui(UiToken::Border)))
        .child(
            div()
                .w(px(width))
                .h(px(stats::TRACK_HEIGHT))
                .bg(rgb(theme.ui(view.colour))),
        )
        .into_any_element()
}

/// Secondary text on the bar.
fn muted(theme: &GuiTheme, text: impl Into<SharedString>) -> AnyElement {
    let text: SharedString = text.into();
    div()
        .text_color(rgb(theme.ui(UiToken::TextMuted)))
        .child(text)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_keystroke_in_the_keymap_parses() {
        // `KeyBinding::new` panics on a malformed keystroke, so building the whole
        // table is the test: a typo in "secondary-shift-p" fails here rather than
        // at somebody's first keypress.
        let expected: usize = keymap::DEFAULT
            .iter()
            .map(|binding| binding.contexts.len())
            .sum();
        // Plus the one binding that is deliberately not in the table: Escape
        // closes About. `key_bindings` appends it, so counting only the table
        // is off by one.
        assert_eq!(key_bindings().len(), expected + 1);
    }

    #[test]
    fn about_says_which_build_this_is() {
        // The panel draws these rows in this order, so the four facts a bug report
        // asks for are pinned here rather than in a screenshot: which build, under
        // what licence, where the manual is, what built it.
        let lines = about_lines();
        assert_eq!(lines.len(), 4, "four rows, no more and no fewer");
        let value = |label: &str| {
            lines
                .iter()
                .find(|(name, _)| *name == label)
                .map(|(_, value)| *value)
        };
        // The same constant `acpilot --version` and the daemon report: a window
        // that says 0.1.0 while its tarball says 0.4.2 is a bug report about a
        // build nobody has.
        assert_eq!(value("Version"), Some(acpilot_core::VERSION));
        assert_eq!(value("License"), Some("MIT OR Apache-2.0"));
        assert_eq!(value("Field manual"), Some("rudeboy.dev/acpilot"));
        assert_eq!(value("Built with"), Some("Freebuff — freebuff.com"));
    }

    #[test]
    fn the_menu_carries_the_palette_and_a_way_out() {
        let menus = menus();
        assert_eq!(menus.len(), 2);
        assert_eq!(menus[0].name.to_string(), "Acpilot");
        assert_eq!(
            menus[0].items.len(),
            5,
            "about, palette and quit, each separated: the three things a window with one pane needs"
        );
        assert_eq!(menus[1].name.to_string(), "Profile");
    }
}

/// The shell in a window, driven the way a person drives it.
///
/// `gui-tests` is gpui's own headless harness — a platform with no display and a
/// real `Context` — because the things worth testing here are inside the view:
/// which overlay is on top, and what a command does to it. A copy of that logic in
/// a test would test the copy. It is a feature of its own so that the plain
/// workspace gate still compiles neither gpui nor a graphics stack.
#[cfg(all(test, feature = "gui-tests"))]
mod drawn {
    use super::*;

    fn theme() -> GuiTheme {
        let extensions = acpilot_extensions::builtin::load_all().expect("the built-ins load");
        GuiTheme::from_extensions(&extensions, "acpilot.night-flight")
            .expect("night-flight resolves")
    }

    /// A row as the daemon would report it: half a window the server measured, and
    /// no credit bar, because a local server owes nothing.
    fn rows() -> Vec<Row> {
        use acpilot_proto::stats::{IntegrationKind, StatSource};

        vec![Row {
            id: "lmstudio".to_owned(),
            kind: IntegrationKind::Provider,
            label: "lmstudio".to_owned(),
            context: Some(Track {
                fill: Some(0.5),
                text: "16.4k / 32.8k".to_owned(),
                source: StatSource::Server,
            }),
            credits: None,
            updated_at: "2026-10-30T12:00:00Z".to_owned(),
            stale: false,
        }]
    }

    /// The path that works on Linux and Windows, where gpui 0.2.2 draws no menu
    /// bar: the palette row is the only way in.
    #[gpui::test]
    async fn the_palette_row_opens_about_and_escape_closes_it(cx: &mut gpui::TestAppContext) {
        let visual = cx.add_empty_window();
        let shell = visual.update(|_window, cx| cx.new(|cx| Shell::new(Vec::new(), theme(), cx)));

        assert!(
            !visual.update(|_window, cx| shell.read(cx).showing_about()),
            "a window does not start with About on top of it"
        );

        visual.update(|_window, cx| {
            shell.update(cx, |shell, cx| {
                shell.run(CommandId::About, cx);
            });
        });
        assert!(
            visual.update(|_window, cx| shell.read(cx).showing_about()),
            "the palette row shows About where there is no menu bar"
        );

        // Escape, as the keymap binds it: the same action the binding names.
        visual.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.dismiss_about(&DismissAbout, window, cx);
            });
        });
        assert!(
            !visual.update(|_window, cx| shell.read(cx).showing_about()),
            "escape closes it again"
        );
    }

    /// The bar draws what the daemon measured: a row arrives already measured, and
    /// the window only lays it out. `F4` swaps every figure for its source, and the
    /// same key puts them back.
    #[gpui::test]
    async fn the_stats_bar_takes_the_daemons_rows_and_f4_shows_their_sources(
        cx: &mut gpui::TestAppContext,
    ) {
        let visual = cx.add_empty_window();
        let shell = visual.update(|_window, cx| cx.new(|cx| Shell::new(Vec::new(), theme(), cx)));
        assert!(
            visual.update(|_window, cx| shell.read(cx).integrations().is_empty()),
            "a window with no daemon behind it starts with an empty bar"
        );

        let rows = rows();
        visual.update(|_window, cx| {
            shell.update(cx, |shell, cx| shell.set_stats(rows.clone(), cx));
        });
        assert_eq!(
            visual.update(|_window, cx| shell.read(cx).integrations().len()),
            1,
            "the row the daemon reported is on the bar"
        );
        assert!(
            !visual.update(|_window, cx| shell.read(cx).showing_stats_sources()),
            "and it is drawn as figures to begin with"
        );

        let segment = Segment::of(&rows[0], false);
        let context = segment.context.expect("a context track");
        assert_eq!(
            context.width,
            Some(stats::TRACK_WIDTH / 2.0),
            "half a window is half a track, in pixels"
        );
        assert_eq!(
            context.text, "16.4k / 32.8k",
            "the sentence is the shared crate's, not the window's"
        );
        assert!(
            segment.credits.is_none() && !segment.quiet,
            "a local server owes nothing, which is not the same as reporting nothing"
        );

        visual.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.toggle_stats_sources(&ToggleStatsSources, window, cx);
            });
        });
        let shown = visual.update(|_window, cx| {
            let shell = shell.read(cx);
            (
                shell.showing_stats_sources(),
                Segment::of(&rows[0], true).sources,
            )
        });
        assert!(shown.0, "F4 puts the provenance on the bar");
        assert_eq!(
            shown.1.as_deref(),
            Some("context server · credits —"),
            "which is the row's own words, including the credits nobody reported"
        );

        visual.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.toggle_stats_sources(&ToggleStatsSources, window, cx);
            });
        });
        assert!(
            !visual.update(|_window, cx| shell.read(cx).showing_stats_sources()),
            "and the same key puts the numbers back"
        );
    }

    /// The bar is painted, not just stored: the shell is the window's root view
    /// here, so drawing it lays the profile list, the bar and the footer out. A
    /// track that cannot be laid out or a colour token the theme does not have
    /// fails here rather than at somebody's first launch.
    #[gpui::test]
    async fn the_window_draws_with_the_stats_bar(cx: &mut gpui::TestAppContext) {
        let (shell, visual) = cx.add_window_view(|_window, cx| Shell::new(Vec::new(), theme(), cx));

        // Once with nothing reported, once with a measured row, and once in the
        // provenance view: the three shapes the bar has.
        let _ = visual.update(|window, cx| window.draw(cx));
        visual.update(|_window, cx| {
            shell.update(cx, |shell, cx| shell.set_stats(rows(), cx));
        });
        let _ = visual.update(|window, cx| window.draw(cx));
        visual.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.toggle_stats_sources(&ToggleStatsSources, window, cx);
            });
        });
        let _ = visual.update(|window, cx| window.draw(cx));
    }

    /// About and the palette share one escape key, so opening About has to close
    /// the palette — otherwise one key would have to dismiss two overlays.
    #[gpui::test]
    async fn opening_about_closes_the_palette(cx: &mut gpui::TestAppContext) {
        let visual = cx.add_empty_window();
        let shell = visual.update(|_window, cx| cx.new(|cx| Shell::new(Vec::new(), theme(), cx)));

        let opened = visual.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.toggle_palette(&TogglePalette, window, cx);
                shell.showing_palette()
            })
        });
        assert!(opened, "the palette opens to begin with");

        visual.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.show_about(&ShowAbout, window, cx);
            });
        });

        let state = visual.update(|_window, cx| {
            shell.update(cx, |shell, _cx| {
                (shell.showing_about(), shell.showing_palette())
            })
        });
        assert_eq!(state, (true, false), "About on top, palette closed");
    }
}
