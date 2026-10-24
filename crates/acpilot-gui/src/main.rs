//! `acpilot-gui` — the window, opened on the profiles it is given.
//!
//! ```bash
//! cargo run -p acpilot-gui --features gui -- --profile ~/.config/acpilot/profiles/work
//! ```
//!
//! Nothing is discovered: a profile is a directory, and which directory is the
//! caller's decision. The daemon takes `--profile` for the same reason, and a
//! second resolution rule here is how the GUI would end up showing a profile the
//! daemon is not serving.

use std::process::ExitCode;

use acpilot_gui::Shell;
use acpilot_gui::app;
use acpilot_gui::profiles::ShellProfile;
use acpilot_gui::theme::GuiTheme;
use clap::Parser;
use gpui::{App, AppContext as _, Application};

/// The Acpilot desktop app.
#[derive(Debug, Parser)]
#[command(name = "acpilot-gui", version, about)]
struct Cli {
    /// A profile directory to open. Repeat for more than one.
    #[arg(long, value_name = "DIR")]
    profile: Vec<std::path::PathBuf>,

    /// Theme id to draw with, from any loaded extension.
    #[arg(long, value_name = "ID", default_value = "acpilot.night-flight")]
    theme: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.profile.is_empty() {
        eprintln!(
            "acpilot-gui: pass at least one --profile <dir>. Nothing is discovered yet, so a \
             window with no profile would be a window with no cockpit."
        );
        return ExitCode::from(2);
    }

    let mut profiles = Vec::with_capacity(cli.profile.len());
    for directory in &cli.profile {
        match ShellProfile::open(directory) {
            Ok(profile) => profiles.push(profile),
            Err(error) => {
                eprintln!("acpilot-gui: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    let extensions = match acpilot_extensions::builtin::load_all() {
        Ok(extensions) => extensions,
        Err(errors) => {
            // Our own files failing is a build-time bug that `include_str!`
            // cannot catch (a schema mistake, not a missing file), so say all of
            // them rather than the first.
            for error in errors {
                eprintln!("acpilot-gui: built-in extension: {error}");
            }
            return ExitCode::FAILURE;
        }
    };

    let theme = match GuiTheme::from_extensions(&extensions, &cli.theme) {
        Ok(theme) => theme,
        Err(error) => {
            eprintln!("acpilot-gui: theme: {error}");
            return ExitCode::FAILURE;
        }
    };

    Application::new().run(move |cx: &mut App| {
        // Native menus, where the platform draws them (macOS). The palette is
        // the path that works everywhere, which is why the same actions are
        // bound to keys as well as listed here.
        cx.set_menus(app::menus());
        let options = app::window_options();
        let profiles = profiles.clone();
        let theme = theme.clone();
        cx.open_window(options, |_window, cx| {
            cx.new(|cx| Shell::new(profiles, theme, cx))
        })
        .expect("the platform accepted a window");
    });

    ExitCode::SUCCESS
}
