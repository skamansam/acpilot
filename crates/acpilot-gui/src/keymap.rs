//! The keys the shell answers to, as data.
//!
//! The desktop phase's task P10-2 asks for the GUI to read the same keymap data as the TUI rather than a
//! second hand-written table. `keymap.toml` is the mod-shop phase's P4.6-9 to define — a file format
//! and its precedence rules are a decision, not a heading — so until then the
//! table lives here as a constant: plain data, no gpui in sight, and one place to
//! add a binding instead of two.
//!
//! A binding names the *contexts* it applies to rather than repeating itself per
//! context. `down` means "next profile" when the shell has focus and "next
//! palette row" when the palette does, and that is two bindings in one table
//! entry rather than two tables that drift.

/// Where a binding is live. The shell sets exactly one on the focused element,
/// so a context is both a keymap scope and a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    /// The profile list is showing.
    Shell,
    /// The command palette is open.
    Palette,
}

impl Context {
    /// The name gpui matches against `key_context`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Shell => "Shell",
            Self::Palette => "Palette",
        }
    }
}

/// What a keystroke means. These are the shell's verbs; the gpui action types are
/// generated from them in [`crate::app`], so the table and the handler cannot
/// disagree about the name of a thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Open or close the command palette.
    TogglePalette,
    /// Move the palette selection up.
    PaletteUp,
    /// Move the palette selection down.
    PaletteDown,
    /// Run the palette's selected command.
    PaletteConfirm,
    /// Close the palette without running anything.
    PaletteDismiss,
    /// Select the next profile.
    NextProfile,
    /// Select the previous profile.
    PreviousProfile,
    /// Leave the app.
    Quit,
}

/// One row of the keymap.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    /// The keystroke, in gpui's syntax (`secondary` is cmd on macOS, ctrl
    /// elsewhere, which is the only way one table can be right on both).
    pub keys: &'static str,
    /// What it does.
    pub action: Action,
    /// Where it is live.
    pub contexts: &'static [Context],
    /// What a person would call it, for a menu or a palette hint.
    pub description: &'static str,
}

use Context::{Palette, Shell};

/// The shell's keys.
pub const DEFAULT: &[Binding] = &[
    Binding {
        keys: "secondary-shift-p",
        action: Action::TogglePalette,
        contexts: &[Shell, Palette],
        description: "Command palette",
    },
    Binding {
        keys: "secondary-q",
        action: Action::Quit,
        contexts: &[Shell, Palette],
        description: "Quit Acpilot",
    },
    Binding {
        keys: "down",
        action: Action::NextProfile,
        contexts: &[Shell],
        description: "Next profile",
    },
    Binding {
        keys: "up",
        action: Action::PreviousProfile,
        contexts: &[Shell],
        description: "Previous profile",
    },
    Binding {
        keys: "down",
        action: Action::PaletteDown,
        contexts: &[Palette],
        description: "Next command",
    },
    Binding {
        keys: "up",
        action: Action::PaletteUp,
        contexts: &[Palette],
        description: "Previous command",
    },
    Binding {
        keys: "enter",
        action: Action::PaletteConfirm,
        contexts: &[Palette],
        description: "Run the selected command",
    },
    Binding {
        keys: "escape",
        action: Action::PaletteDismiss,
        contexts: &[Palette],
        description: "Close the palette",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn expanded() -> Vec<(Context, &'static str, Action)> {
        DEFAULT
            .iter()
            .flat_map(|binding| {
                binding
                    .contexts
                    .iter()
                    .map(move |context| (*context, binding.keys, binding.action))
            })
            .collect()
    }

    #[test]
    fn no_keystroke_means_two_things_in_one_context() {
        let mut seen = Vec::new();
        for (context, keys, _) in expanded() {
            assert!(
                !seen.contains(&(context, keys)),
                "{keys:?} is bound twice in {context:?}"
            );
            seen.push((context, keys));
        }
    }

    #[test]
    fn the_palette_can_be_opened_and_closed_from_anywhere() {
        // The first thing a user tries, and the one binding that has to survive
        // a mode change: the palette is how you get out of the palette.
        for context in [Shell, Palette] {
            assert!(
                expanded()
                    .iter()
                    .any(|(at, _, action)| *at == context && *action == Action::TogglePalette),
                "no toggle in {context:?}"
            );
        }
    }

    #[test]
    fn every_palette_action_is_reachable_while_the_palette_is_open() {
        for action in [
            Action::PaletteUp,
            Action::PaletteDown,
            Action::PaletteConfirm,
            Action::PaletteDismiss,
        ] {
            assert!(
                expanded()
                    .iter()
                    .any(|(context, _, bound)| *context == Palette && *bound == action),
                "{action:?} is not bound in the palette"
            );
        }
    }

    #[test]
    fn every_action_is_bound_somewhere() {
        for action in [
            Action::TogglePalette,
            Action::PaletteUp,
            Action::PaletteDown,
            Action::PaletteConfirm,
            Action::PaletteDismiss,
            Action::NextProfile,
            Action::PreviousProfile,
            Action::Quit,
        ] {
            assert!(
                DEFAULT.iter().any(|binding| binding.action == action),
                "{action:?} has no binding"
            );
        }
    }

    #[test]
    fn keystrokes_are_written_the_way_gpui_parses_them() {
        for binding in DEFAULT {
            assert!(!binding.keys.is_empty());
            assert_eq!(
                binding.keys,
                binding.keys.to_ascii_lowercase(),
                "keystrokes are lowercase, modifiers included"
            );
            assert!(
                !binding.contexts.is_empty(),
                "{:?} is bound in no context and can never fire",
                binding.action
            );
            assert!(!binding.description.is_empty());
        }
    }
}
