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
    Bounds, Context, FocusHandle, InteractiveElement, IntoElement, KeyBinding, KeyDownEvent,
    Menu, MenuItem, ParentElement, Render, SharedString, Size as GpuiSize,
    StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions, actions, div, point,
    px, rgb,
};

use acpilot_extensions::UiToken;

use crate::keymap::{self, Action as KeyAction, Context as KeyContext};
use crate::palette::{self, Command, CommandId};
use crate::profiles::ShellProfile;
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
        KeyAction::Quit => KeyBinding::new(keys, Quit, Some(context)),
    }
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
    focus: FocusHandle,
    focused: bool,
}

impl Shell {
    /// Build the view from what `main` resolved.
    pub fn new(profiles: Vec<ShellProfile>, theme: GuiTheme, cx: &mut Context<Self>) -> Self {
        cx.bind_keys(key_bindings());
        Self {
            commands: palette::commands(&profiles),
            profiles,
            selected: 0,
            theme,
            palette: None,
            focus: cx.focus_handle(),
            focused: false,
        }
    }

    /// The profile currently shown, if any.
    #[must_use]
    pub fn current(&self) -> Option<&ShellProfile> {
        self.profiles.get(self.selected)
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
            CommandId::Quit => cx.quit(),
        }
    }

    fn toggle_palette(
        &mut self,
        _: &TogglePalette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    fn move_palette(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Self { palette, commands, .. } = self;
        let Some(palette) = palette.as_mut() else {
            return;
        };
        let len = palette::filter(commands, &palette.query).len();
        palette.selected = palette::move_selection(palette.selected, len, delta);
        cx.notify();
    }

    fn palette_down(
        &mut self,
        _: &PaletteDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_palette(1, cx);
    }

    fn palette_up(
        &mut self,
        _: &PaletteUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let Some(command) = matched.get(palette.selected).map(|index| self.commands[*index].id)
        else {
            return;
        };
        self.run(command, cx);
    }

    fn move_profile(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.selected = palette::move_selection(self.selected, self.profiles.len(), delta);
        cx.notify();
    }

    fn next_profile(
        &mut self,
        _: &NextProfile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                .children(palette_rows.iter().map(
                    |(row, command, label, detail, selected)| {
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
                    },
                ))
        });

        let list = div().children(profiles.into_iter().map(|(index, name, summary, selected)| {
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
        }));

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
            .on_action(cx.listener(Self::quit))
            .on_key_down(cx.listener(Self::key_down))
            .child(header)
            .children(palette_panel)
            .child(if empty_palette {
                div()
                    .p(px(12.0))
                    .text_color(rgb(theme.ui(UiToken::TextMuted)))
                    .child(SharedString::from("No command matches that."))
                    .into_any_element()
            } else {
                list.into_any_element()
            })
            .child(
                div()
                    .p(px(12.0))
                    .text_color(rgb(theme.ui(UiToken::TextMuted)))
                    .text_size(px(11.0))
                    .child(SharedString::from(footer))
                    .child(SharedString::from(
                        "secondary-shift-p — command palette · down/up — profile · secondary-q — quit",
                    )),
            )
    }
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
        assert_eq!(key_bindings().len(), expected);
    }

    #[test]
    fn the_menu_carries_the_palette_and_a_way_out() {
        let menus = menus();
        assert_eq!(menus.len(), 2);
        assert_eq!(menus[0].name.to_string(), "Acpilot");
        assert_eq!(
            menus[0].items.len(),
            3,
            "palette, separator, quit — the three things a window with one pane needs"
        );
        assert_eq!(menus[1].name.to_string(), "Profile");
    }
}
