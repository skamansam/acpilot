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
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use acpilot_gui::app;
use acpilot_gui::platform::{self, Claim};
use acpilot_gui::profiles;
use acpilot_gui::theme::GuiTheme;
use acpilot_gui::{Shell, daemon};
use acpilot_stats::Row;
use clap::Parser;
use futures::StreamExt as _;
use gpui::{App, Application, Entity};

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

    /// Theme id to draw with, overriding the one the settings name.
    ///
    /// The theme is a setting: `[preferences] theme_gui` in the user's config, or
    /// in the project's `.acpilot/config.toml`, and this flag is only for looking
    /// at another one without editing either.
    #[arg(long, value_name = "ID")]
    theme: Option<String>,
}

/// Recolour every open window when the daemon announces new settings.
///
/// A poll on the app's own timer rather than an `await` on the channel: the
/// receiver is a blocking one filled by a plain thread, and the window's executor
/// has to stay free to draw. 120 ms is under the threshold where a recolour reads
/// as a missed frame and far above a busy loop. Only the newest theme is applied —
/// two saves inside one tick are one repaint, not two flickers.
fn recolour_on_settings(
    cx: &mut App,
    shells: Arc<Mutex<Vec<Entity<Shell>>>>,
    changed: std::sync::mpsc::Receiver<GuiTheme>,
) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let mut latest: Option<GuiTheme> = None;
            while let Ok(theme) = changed.try_recv() {
                latest = Some(theme);
            }
            let Some(theme) = latest else {
                continue;
            };
            let windows = shells
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            for shell in windows {
                let theme = theme.clone();
                // The app is shutting down when this fails, which is the one
                // reason to stop asking.
                if cx
                    .update(|cx| shell.update(cx, |shell, cx| shell.set_theme(theme, cx)))
                    .is_err()
                {
                    return;
                }
            }
        }
    })
    .detach();
}

/// Push the daemon's stats at every open window when a new answer arrives.
///
/// The same shape as `recolour_on_settings`: a poll on the app's own timer, because
/// the receiver is a blocking channel filled by a plain thread. The newest answer
/// wins inside one tick — the rows are a snapshot, so an older one is not news.
fn measure_on_stats(
    cx: &mut App,
    shells: Arc<Mutex<Vec<Entity<Shell>>>>,
    changed: std::sync::mpsc::Receiver<Vec<Row>>,
) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let mut latest: Option<Vec<Row>> = None;
            while let Ok(rows) = changed.try_recv() {
                latest = Some(rows);
            }
            let Some(rows) = latest else {
                continue;
            };
            let windows = shells
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            for shell in windows {
                let rows = rows.clone();
                // The app is shutting down when this fails, which is the one reason
                // to stop asking.
                if cx
                    .update(|cx| shell.update(cx, |shell, cx| shell.set_stats(rows, cx)))
                    .is_err()
                {
                    return;
                }
            }
        }
    })
    .detach();
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

    // The settings are the ground truth: the theme is a preference, and this flag
    // is the override. A launch whose settings do not parse should say so rather
    // than draw in a theme nobody chose.
    let theme_id = match cli.theme.clone() {
        Some(id) => id,
        None => match profiles::theme_id(cli.path.as_deref()) {
            Ok(id) => id,
            Err(error) => {
                eprintln!("acpilot-gui: {error}");
                return ExitCode::FAILURE;
            }
        },
    };
    let theme = match GuiTheme::from_extensions(&extensions, &theme_id) {
        Ok(theme) => theme,
        Err(error) => {
            eprintln!("acpilot-gui: theme: {error}");
            return ExitCode::FAILURE;
        }
    };

    // The daemon's answer is the live one: the launch theme above is what the
    // window opens with, and a `prefs` frame replaces it. A daemon that is not
    // running costs this thread one failed request, not a window.
    let (settings, changed) = std::sync::mpsc::channel::<GuiTheme>();
    daemon::follow(daemon::endpoint(), extensions.clone(), move |theme| {
        let _ = settings.send(theme);
    });

    // The stats bar is a second, slower question to the same daemon: the roster and
    // what each integration costs. A window with no daemon behind it keeps an empty
    // bar that says so, rather than failing a launch over a footer.
    let (stats, stats_changed) = std::sync::mpsc::channel::<Vec<Row>>();
    daemon::follow_stats(daemon::endpoint(), move |rows| {
        let _ = stats.send(rows);
    });

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
        // Every window, so a settings change reaches all of them. A forwarded
        // launch opens one more while the app is running; it joins the list.
        let shells: Arc<Mutex<Vec<Entity<Shell>>>> = Arc::default();
        let opened = app::open_window(cx, profiles.clone(), theme.clone())
            .expect("the platform accepted a window");
        shells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(opened);
        recolour_on_settings(cx, Arc::clone(&shells), changed);
        measure_on_stats(cx, Arc::clone(&shells), stats_changed);

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
                    if let Ok(shell) = &opened {
                        shells
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(shell.clone());
                    }
                    opened.map(|_| (cx.windows().len(), started.elapsed()))
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
