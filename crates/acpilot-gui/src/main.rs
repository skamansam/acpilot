//! `acpilot-gui` — the window, opened on the profile a path implies.
//!
//! ```bash
//! cargo run -p acpilot-gui --features gui                    # this directory
//! cargo run -p acpilot-gui --features gui -- src/main.rs     # its project
//! cargo run -p acpilot-gui --features gui -- --profile ~/.config/acpilot/profiles/work
//! ```
//!
//! The path is the whole argument surface for the usual launch: a directory is the
//! project, a file climbs to the one it is in, and the profile follows from that
//! project's settings. `--profile` remains for the case a directory cannot express —
//! two profiles of the same tree — and it wins when it is given.
//!
//! A launch that finds another instance running hands it the request and exits
//! instead of starting a second process. That is what first paint costs: gpui
//! creates its device before the first frame, 320–411 ms of Direct3D on the
//! machines we measured, and nothing at the pin lets a caller defer or overlap it
//! (`docs/notes/gui-bench.md`). A reused process opens the next window in 30–55
//! ms, because the device is already there.

use std::process::ExitCode;

use acpilot_gui::app;
use acpilot_gui::platform::{self, Claim};
use acpilot_gui::profiles;
use acpilot_gui::theme::GuiTheme;
use clap::Parser;
use futures::StreamExt as _;
use gpui::{App, Application};

/// The Acpilot desktop app.
#[derive(Debug, Parser)]
#[command(name = "acpilot-gui", version, about)]
struct Cli {
    /// A directory, or a file inside one. Defaults to this directory.
    ///
    /// A directory is the workspace. A file climbs its parents until it finds
    /// `.acpilot/config.toml`, and the profile comes from that project's settings.
    #[arg(value_name = "PATH")]
    path: Option<std::path::PathBuf>,

    /// A profile directory to open. Repeat for more than one.
    ///
    /// Wins over the profile a project implies, for the case a path cannot express.
    #[arg(long, value_name = "DIR")]
    profile: Vec<std::path::PathBuf>,

    /// Theme id to draw with, from any loaded extension.
    #[arg(long, value_name = "ID", default_value = "acpilot.night-flight")]
    theme: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let profiles = match profiles::open_for(cli.path.as_deref(), &cli.profile) {
        Ok(profiles) => profiles,
        Err(error) => {
            eprintln!("acpilot-gui: {error}");
            return ExitCode::FAILURE;
        }
    };

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

    // The claim is taken before the runtime starts, so a launch that is only
    // handing over never pays for a renderer. It is also taken after the profiles
    // and theme are validated: a second instance with a typo in `--profile`
    // should fail loudly here rather than be silently forwarded to a window the
    // user cannot see.
    let requests = match platform::claim() {
        Claim::Secondary => {
            println!("acpilot-gui: asked the running instance to open a window");
            return ExitCode::SUCCESS;
        }
        Claim::Primary(instance) => Some(instance.requests()),
        Claim::Unavailable => None,
    };

    Application::new().run(move |cx: &mut App| {
        // Native menus, where the platform draws them (macOS). The palette is
        // the path that works everywhere, which is why the same actions are
        // bound to keys as well as listed here.
        cx.set_menus(app::menus());
        // Once, on the app: a second window shares this keymap, and binding it
        // again per window would append a duplicate for every launch.
        cx.bind_keys(app::key_bindings());
        app::open_window(cx, profiles.clone(), theme.clone())
            .expect("the platform accepted a window");

        let Some(mut requests) = requests else {
            return;
        };
        let trace = std::env::var_os("ACPILOT_GUI_TRACE").is_some();
        cx.spawn(async move |cx| {
            while requests.next().await.is_some() {
                let profiles = profiles.clone();
                let theme = theme.clone();
                // `activate` matters: a forwarded launch is a person asking for
                // the window *now*, and opening it behind a terminal would look
                // like the second launch did nothing.
                let opened = cx.update(|cx| {
                    let started = std::time::Instant::now();
                    let opened = app::open_window(cx, profiles, theme);
                    cx.activate(true);
                    opened.map(|()| (cx.windows().len(), started.elapsed()))
                });
                match opened {
                    // The app is shutting down; the channel's sender is dropped
                    // with the listener when this task ends.
                    Err(_) => break,
                    Ok(Err(error)) => {
                        eprintln!(
                            "acpilot-gui: the running instance could not open a window: {error:#}"
                        );
                    }
                    Ok(Ok((windows, took))) if trace => {
                        eprintln!(
                            "acpilot-gui: opened a window in {} ms, {windows} now open",
                            took.as_millis()
                        );
                    }
                    Ok(Ok(_)) => (),
                }
            }
        })
        .detach();
    });

    ExitCode::SUCCESS
}
