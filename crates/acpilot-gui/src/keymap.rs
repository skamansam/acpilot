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
    /// The source-control pane has the keys.
    Source,
    /// The commit composer is over it.
    Commit,
}

impl Context {
    /// The name gpui matches against `key_context`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Shell => "Shell",
            Self::Palette => "Palette",
            Self::Source => "Source",
            Self::Commit => "Commit",
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
    /// Swap the stats bar's figures for the source behind each one.
    ToggleStatsSources,
    /// Open or close the source-control pane.
    ToggleSource,
    /// Choose the next changed file.
    SourceNext,
    /// Choose the previous changed file.
    SourcePrevious,
    /// Scroll the selected file's diff down a page.
    SourcePageDown,
    /// Scroll it up a page.
    SourcePageUp,
    /// Ask the daemon for the index again.
    SourceRefresh,
    /// Put the selected path in the index.
    SourceStage,
    /// Remove the selected path from the index.
    SourceUnstage,
    /// Arm a discard of the selected path's unstaged changes.
    SourceDiscard,
    /// Arm a checkout of the selected path from HEAD.
    SourceCheckout,
    /// Confirm the armed discard or checkout.
    SourceConfirm,
    /// Cancel the armed discard or checkout.
    SourceCancel,
    /// Open the commit composer.
    SourceCompose,
    /// Open it and ask the session for a draft.
    SourceGenerate,
    /// Close the source-control pane.
    CloseSource,
    /// Choose the next session a draft may be asked of.
    ComposerNextSession,
    /// Choose the previous session a draft may be asked of.
    ComposerPreviousSession,
    /// Close the composer without writing.
    CloseComposer,
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

use Context::{Commit, Palette, Shell, Source};

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
    // A function key, as in the terminal: no pane whose keys are text gives one
    // up, and this one is not text.
    Binding {
        keys: "f4",
        action: Action::ToggleStatsSources,
        contexts: &[Shell],
        description: "Where the stats came from",
    },
    // The same function key the terminal uses. It is not a printable key, so a
    // pane that will later take text cannot want it.
    Binding {
        keys: "f6",
        action: Action::ToggleSource,
        contexts: &[Shell],
        description: "Source control",
    },
    Binding {
        keys: "escape",
        action: Action::CloseSource,
        contexts: &[Source],
        description: "Close source control",
    },
    Binding {
        keys: "down",
        action: Action::SourceNext,
        contexts: &[Source],
        description: "Next changed file",
    },
    Binding {
        keys: "j",
        action: Action::SourceNext,
        contexts: &[Source],
        description: "Next changed file",
    },
    Binding {
        keys: "up",
        action: Action::SourcePrevious,
        contexts: &[Source],
        description: "Previous changed file",
    },
    Binding {
        keys: "k",
        action: Action::SourcePrevious,
        contexts: &[Source],
        description: "Previous changed file",
    },
    Binding {
        keys: "pagedown",
        action: Action::SourcePageDown,
        contexts: &[Source],
        description: "Scroll the diff down",
    },
    Binding {
        keys: "pageup",
        action: Action::SourcePageUp,
        contexts: &[Source],
        description: "Scroll the diff up",
    },
    Binding {
        keys: "r",
        action: Action::SourceRefresh,
        contexts: &[Source],
        description: "Refresh the index",
    },
    Binding {
        keys: "s",
        action: Action::SourceStage,
        contexts: &[Source],
        description: "Stage the selected path",
    },
    Binding {
        keys: "u",
        action: Action::SourceUnstage,
        contexts: &[Source],
        description: "Unstage the selected path",
    },
    Binding {
        keys: "d",
        action: Action::SourceDiscard,
        contexts: &[Source],
        description: "Discard the selected path's unstaged changes",
    },
    Binding {
        keys: "x",
        action: Action::SourceCheckout,
        contexts: &[Source],
        description: "Check the selected path out from HEAD",
    },
    Binding {
        keys: "y",
        action: Action::SourceConfirm,
        contexts: &[Source],
        description: "Confirm the armed restore",
    },
    Binding {
        keys: "n",
        action: Action::SourceCancel,
        contexts: &[Source],
        description: "Cancel the armed restore",
    },
    Binding {
        keys: "c",
        action: Action::SourceCompose,
        contexts: &[Source],
        description: "Compose a commit message",
    },
    Binding {
        keys: "g",
        action: Action::SourceGenerate,
        contexts: &[Source],
        description: "Generate a commit message",
    },
    Binding {
        keys: "tab",
        action: Action::ComposerNextSession,
        contexts: &[Commit],
        description: "Next session for the draft",
    },
    Binding {
        keys: "shift-tab",
        action: Action::ComposerPreviousSession,
        contexts: &[Commit],
        description: "Previous session for the draft",
    },
    Binding {
        keys: "escape",
        action: Action::CloseComposer,
        contexts: &[Commit],
        description: "Close the composer",
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
            Action::ToggleStatsSources,
            Action::ToggleSource,
            Action::SourceNext,
            Action::SourcePrevious,
            Action::SourcePageDown,
            Action::SourcePageUp,
            Action::SourceRefresh,
            Action::SourceStage,
            Action::SourceUnstage,
            Action::SourceDiscard,
            Action::SourceCheckout,
            Action::SourceConfirm,
            Action::SourceCancel,
            Action::SourceCompose,
            Action::SourceGenerate,
            Action::CloseSource,
            Action::ComposerNextSession,
            Action::ComposerPreviousSession,
            Action::CloseComposer,
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
