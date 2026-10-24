//! The window.
//!
//! P10-2's first slice: a themed shell that lists the profiles it was launched
//! with and shows which one is selected. The menu bar, the command palette, the
//! keymap and the panes come next; they are deliberately not sketched here,
//! because a stub that does nothing is harder to review than an absent feature.
//!
//! What this slice does prove is the shape the rest of the GUI builds on: tokens
//! come from the shared `theme.toml` ([`crate::theme`]), the profile data comes
//! from [`acpilot_profile`] rather than from the window, and the view only draws.

use gpui::{
    Bounds, Context, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    Size as GpuiSize, StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions,
    div, point, px, rgb,
};

use acpilot_extensions::UiToken;

use crate::profiles::ShellProfile;
use crate::theme::GuiTheme;

/// The shell: what is on screen and which profile is current.
pub struct Shell {
    profiles: Vec<ShellProfile>,
    selected: usize,
    theme: GuiTheme,
}

impl Shell {
    /// Build the view from what `main` resolved.
    pub fn new(profiles: Vec<ShellProfile>, theme: GuiTheme, _cx: &mut Context<Self>) -> Self {
        Self {
            profiles,
            selected: 0,
            theme,
        }
    }

    /// The profile currently shown, if any.
    #[must_use]
    pub fn current(&self) -> Option<&ShellProfile> {
        self.profiles.get(self.selected)
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Read what the frame needs before building elements: `self` cannot be
        // borrowed by a closure and by the element tree at the same time.
        let theme = self.theme.clone();
        let heading = SharedString::from(format!("Acpilot · {}", theme.name()));
        let rows: Vec<(String, String, bool)> = self
            .profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                (
                    profile.name().to_owned(),
                    profile.summary(),
                    index == self.selected,
                )
            })
            .collect();
        let footer = self.current().map_or_else(
            || "No profile was passed on the command line.".to_owned(),
            |profile| format!("{} — {}", profile.root().display(), profile.summary()),
        );

        let header = div()
            .h(px(40.0))
            .p(px(12.0))
            .bg(rgb(theme.ui(UiToken::BgPanel)))
            .border_b(px(1.0))
            .border_color(rgb(theme.ui(UiToken::Border)))
            .text_color(rgb(theme.ui(UiToken::TextMuted)))
            .child(heading);

        let list = div()
            .children(rows.into_iter().enumerate().map(|(index, (name, summary, selected))| {
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
                        cx.notify();
                    }))
            }));

        div()
            .size_full()
            .bg(rgb(theme.ui(UiToken::Bg)))
            .text_color(rgb(theme.ui(UiToken::Text)))
            .text_size(px(13.0))
            .child(header)
            .child(list)
            .child(
                div()
                    .p(px(12.0))
                    .text_color(rgb(theme.ui(UiToken::TextMuted)))
                    .text_size(px(11.0))
                    .child(SharedString::from(footer)),
            )
    }
}
